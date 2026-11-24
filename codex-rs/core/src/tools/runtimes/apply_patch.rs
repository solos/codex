//! Apply Patch runtime: executes verified patches under the orchestrator.
//!
//! Assumes `apply_patch` verification/approval happened upstream. Reuses the
//! selected turn environment filesystem for both local and remote turns, with
//! sandboxing enforced by the explicit filesystem sandbox context.
use crate::exec::is_likely_sandbox_denied;
use crate::session::turn_context::TurnEnvironment;
use crate::tools::sandboxing::Approvable;
use crate::tools::sandboxing::ApprovalAction;
use crate::tools::sandboxing::ExecApprovalRequirement;
use crate::tools::sandboxing::SandboxAttempt;
use crate::tools::sandboxing::Sandboxable;
use crate::tools::sandboxing::ToolCtx;
use crate::tools::sandboxing::ToolError;
use crate::tools::sandboxing::ToolRuntime;
use crate::tools::sandboxing::executor_windows_sandbox_selection;
use codex_apply_patch::AppliedPatchDelta;
use codex_apply_patch::AppliedPatchFileChange;
use codex_apply_patch::ApplyPatchAction;
use codex_apply_patch::ApplyPatchOptions;
use codex_apply_patch::content_hash;
use codex_exec_server::ExecutorFileSystem;
use codex_exec_server::FileSystemSandboxContext;
use codex_exec_server::ReadFileOptions;
use codex_protocol::error::CodexErr;
use codex_protocol::error::SandboxErr;
use codex_protocol::exec_output::ExecToolCallOutput;
use codex_protocol::exec_output::StreamOutput;
use codex_protocol::models::AdditionalPermissionProfile;
use codex_protocol::protocol::AskForApproval;
use codex_protocol::protocol::FileChange;
use codex_sandboxing::SandboxType;
use codex_sandboxing::SandboxablePreference;
use codex_sandboxing::is_likely_executor_managed_sandbox_denied;
use codex_sandboxing::policy_transforms::effective_permission_profile;
use codex_sandboxing::record_filesystem_sandbox_violation;
use codex_utils_path_uri::PathUri;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Instant;

#[derive(Clone, Debug, Eq, PartialEq, Hash, serde::Serialize)]
pub(crate) struct ApplyPatchApprovalKey {
    pub(crate) environment_id: String,
    pub(crate) path: PathUri,
}

#[derive(Debug)]
pub struct ApplyPatchRequest {
    pub turn_environment: TurnEnvironment,
    pub action: ApplyPatchAction,
    pub file_paths: Vec<PathUri>,
    pub changes: Arc<std::collections::HashMap<PathBuf, FileChange>>,
    pub exec_approval_requirement: ExecApprovalRequirement,
    pub additional_permissions: Option<AdditionalPermissionProfile>,
    pub permissions_preapproved: bool,
}

impl ApplyPatchRequest {
    pub(crate) fn approval_action(&self, call_id: &str) -> ApprovalAction {
        ApprovalAction::ApplyPatch {
            id: call_id.to_string(),
            environment_id: self.turn_environment.selection.environment_id.clone(),
            cwd: self.action.cwd.clone(),
            files: self.file_paths.clone(),
            patch: self.action.patch.clone(),
            changes: Arc::clone(&self.changes),
            permissions_preapproved: self.permissions_preapproved,
        }
    }
}

#[derive(Default)]
pub struct ApplyPatchRuntime {
    committed_delta: AppliedPatchDelta,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct FileFingerprint {
    content_hash: u64,
}

#[derive(Default)]
struct ApplyPatchFileLedger {
    files: Mutex<HashMap<PathUri, FileFingerprint>>,
}

#[derive(Debug)]
pub struct ApplyPatchRuntimeOutput {
    pub exec_output: ExecToolCallOutput,
    pub delta: AppliedPatchDelta,
}

impl ApplyPatchRuntime {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn committed_delta(&self) -> &AppliedPatchDelta {
        &self.committed_delta
    }

    fn file_system_sandbox_context_for_attempt(
        req: &ApplyPatchRequest,
        attempt: &SandboxAttempt<'_>,
    ) -> Option<FileSystemSandboxContext> {
        if !attempt.sandbox_requested {
            return None;
        }

        let permissions = effective_permission_profile(
            attempt.exec_server_permissions,
            req.additional_permissions.as_ref(),
        );
        Some(FileSystemSandboxContext {
            sandbox_override: attempt.sandbox_override,
            permissions,
            cwd: attempt.sandbox_cwd.clone(),
            workspace_roots: attempt.workspace_roots.to_vec(),
            user_home_dir: req.turn_environment.user_home_dir.clone(),
            temporary_directories: None,
            windows_sandbox_selection: executor_windows_sandbox_selection(
                attempt.windows_sandbox_type,
                attempt.windows_sandbox_level,
                attempt.sandbox_cwd,
            ),
            windows_sandbox_proxy_settings_mode: None,
            use_legacy_landlock: attempt.use_legacy_landlock,
        })
    }
}

impl Sandboxable for ApplyPatchRuntime {
    fn sandbox_preference(&self) -> SandboxablePreference {
        SandboxablePreference::Auto
    }
    fn escalate_on_failure(&self) -> bool {
        true
    }
}

impl Approvable<ApplyPatchRequest> for ApplyPatchRuntime {
    fn approval_action(
        &self,
        req: &ApplyPatchRequest,
        call_id: &str,
    ) -> std::io::Result<ApprovalAction> {
        Ok(req.approval_action(call_id))
    }

    fn wants_no_sandbox_approval(&self, policy: AskForApproval) -> bool {
        match policy {
            AskForApproval::Never => false,
            AskForApproval::Granular(granular_config) => granular_config.allows_sandbox_approval(),
            AskForApproval::OnRequest => true,
            AskForApproval::UnlessTrusted => true,
        }
    }

    // apply_patch approvals are decided upstream by assess_patch_safety.
    //
    // This override ensures the orchestrator runs the patch approval flow when required instead
    // of falling back to the global exec approval policy.
    fn exec_approval_requirement(
        &self,
        req: &ApplyPatchRequest,
    ) -> Option<ExecApprovalRequirement> {
        Some(req.exec_approval_requirement.clone())
    }
}

impl ToolRuntime<ApplyPatchRequest, ApplyPatchRuntimeOutput> for ApplyPatchRuntime {
    fn turn_environment<'a>(&self, req: &'a ApplyPatchRequest) -> &'a TurnEnvironment {
        &req.turn_environment
    }

    fn uses_executor_managed_process_sandbox(&self, req: &ApplyPatchRequest) -> bool {
        req.turn_environment.environment.is_remote()
    }

    fn sandbox_cwd<'a>(&self, req: &'a ApplyPatchRequest) -> Option<&'a PathUri> {
        Some(&req.action.cwd)
    }

    async fn run(
        &mut self,
        req: &ApplyPatchRequest,
        attempt: &SandboxAttempt<'_>,
        ctx: &ToolCtx,
    ) -> Result<ApplyPatchRuntimeOutput, ToolError> {
        let started_at = Instant::now();
        let fs = req.turn_environment.environment.get_filesystem();
        let sandbox = Self::file_system_sandbox_context_for_attempt(req, attempt);
        let follow_symlinks = attempt.sandbox_requested
            || !attempt.manager.should_sandbox(
                attempt.permissions,
                self.sandbox_preference(),
                attempt.enforce_managed_network,
            );
        let ledger = ctx
            .session
            .services
            .thread_extension_data
            .get_or_init(ApplyPatchFileLedger::default);
        let expected_hashes = verify_ledger_before_patch(
            &ledger,
            req.action.changes().keys(),
            fs.as_ref(),
            follow_symlinks,
            sandbox.as_ref(),
        )
        .await
        .map_err(ToolError::Rejected)?;
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let result = codex_apply_patch::apply_patch_with_options_and_freshness(
            &req.action.patch,
            ApplyPatchOptions {
                // Only reject links when an otherwise-required sandbox was bypassed.
                // Executor-managed sandboxes can have SandboxType::None.
                follow_symlinks,
            },
            &req.action.cwd,
            Some(&expected_hashes),
            &mut stdout,
            &mut stderr,
            fs.as_ref(),
            sandbox.as_ref(),
        )
        .await;
        let stdout = String::from_utf8_lossy(&stdout).into_owned();
        let stderr = String::from_utf8_lossy(&stderr).into_owned();
        let failed = result.is_err();
        let exit_code = if failed { 1 } else { 0 };
        let delta = match result {
            Ok(delta) => delta,
            Err(failure) => failure.into_parts().1,
        };
        // Count committed paths, including changes made before a later patch failure.
        for change in delta.changes() {
            let move_path = match &change.change {
                AppliedPatchFileChange::Update { move_path, .. } => move_path.as_ref(),
                AppliedPatchFileChange::Add { .. } | AppliedPatchFileChange::Delete { .. } => None,
            };
            for path in
                std::iter::once(&change.path).chain(move_path.filter(|path| *path != &change.path))
            {
                if let Some(filename @ ("agents.md" | "agents.override.md")) = path
                    .basename()
                    .map(|name| name.to_ascii_lowercase())
                    .as_deref()
                {
                    ctx.step_context.session_telemetry.counter(
                        "codex.agents_md.edit",
                        /*inc*/ 1,
                        &[("filename", filename)],
                    );
                }
            }
        }
        record_committed_patch_files(&ledger, delta.changes());
        self.committed_delta.append(delta);
        let output = ExecToolCallOutput {
            exit_code,
            stdout: StreamOutput::new(stdout.clone()),
            stderr: StreamOutput::new(stderr.clone()),
            aggregated_output: StreamOutput::new(format!("{stdout}{stderr}")),
            duration: started_at.elapsed(),
            timed_out: false,
        };
        let sandbox_denied = failed
            && if attempt.sandbox == SandboxType::None {
                attempt.sandbox_requested && is_likely_executor_managed_sandbox_denied(&output)
            } else {
                is_likely_sandbox_denied(attempt.sandbox, &output)
            };
        if sandbox_denied {
            // TODO(iceweasel): Report executor filesystem sandbox backends like process/start so
            // executor-managed apply_patch denials can emit backend-specific violation telemetry.
            if attempt.sandbox != SandboxType::None {
                record_filesystem_sandbox_violation(attempt.sandbox, &output);
            }
            return Err(ToolError::Codex(CodexErr::Sandbox(SandboxErr::Denied {
                output: Box::new(output),
                network_policy_decision: None,
            })));
        }
        Ok(ApplyPatchRuntimeOutput {
            exec_output: output,
            delta: self.committed_delta.clone(),
        })
    }
}

async fn verify_ledger_before_patch<'a>(
    ledger: &ApplyPatchFileLedger,
    paths: impl Iterator<Item = &'a PathUri>,
    fs: &dyn ExecutorFileSystem,
    follow_symlinks: bool,
    sandbox: Option<&FileSystemSandboxContext>,
) -> Result<HashMap<PathUri, u64>, String> {
    let requested = paths.cloned().collect::<std::collections::HashSet<_>>();
    let expected = {
        let files = ledger.files.lock().map_err(|_| {
            "Could not verify file state because the apply_patch ledger lock was poisoned."
                .to_string()
        })?;
        files
            .iter()
            .map(|(path, fingerprint)| (path.clone(), fingerprint.content_hash))
            .collect::<HashMap<_, _>>()
    };

    for (path, expected) in &expected {
        if !requested.contains(path) {
            continue;
        }
        let current = match fs
            .read_file_text(path, ReadFileOptions { follow_symlinks }, sandbox)
            .await
        {
            Ok(current) => current,
            Err(err) => {
                // Reject once, then clear the stale entry so a following create/retry is not
                // permanently blocked after the file was moved or deleted outside apply_patch.
                update_ledger_fingerprint(
                    ledger,
                    path,
                    FileFingerprint {
                        content_hash: *expected,
                    },
                    None,
                );
                return Err(format!(
                    "File changed or disappeared since this session last edited it: {} could not be re-read ({err}). Re-read the file, then rebuild the patch.",
                    path.inferred_native_path_string()
                ));
            }
        };
        let current_fingerprint = FileFingerprint {
            content_hash: codex_apply_patch::content_hash(&current),
        };
        if current_fingerprint.content_hash != *expected {
            // The rejection is the freshness warning. Refresh only this entry so the model can
            // apply a rebuilt patch after re-reading instead of staying blocked for the session.
            update_ledger_fingerprint(
                ledger,
                path,
                FileFingerprint {
                    content_hash: *expected,
                },
                Some(current_fingerprint),
            );
            return Err(format!(
                "File changed since this session last edited it: {}. This patch was rejected. Re-read the file and rebuild the patch from its current contents.",
                path.inferred_native_path_string()
            ));
        }
    }
    Ok(expected)
}

fn update_ledger_fingerprint(
    ledger: &ApplyPatchFileLedger,
    path: &PathUri,
    expected: FileFingerprint,
    replacement: Option<FileFingerprint>,
) {
    if let Ok(mut files) = ledger.files.lock()
        && files.get(path).is_some_and(|current| *current == expected)
    {
        match replacement {
            Some(fingerprint) => {
                files.insert(path.clone(), fingerprint);
            }
            None => {
                files.remove(path);
            }
        }
    }
}

fn record_committed_patch_files(
    ledger: &ApplyPatchFileLedger,
    changes: &[codex_apply_patch::AppliedPatchChange],
) {
    let mut updates: Vec<(PathUri, Option<FileFingerprint>)> = Vec::new();
    for change in changes {
        match &change.change {
            AppliedPatchFileChange::Add { content, .. } => {
                updates.push((
                    change.path.clone(),
                    Some(FileFingerprint {
                        content_hash: content_hash(content),
                    }),
                ));
            }
            AppliedPatchFileChange::Update {
                new_content: content,
                move_path,
                ..
            } => {
                let destination = move_path.as_ref().unwrap_or(&change.path);
                if destination != &change.path {
                    updates.push((change.path.clone(), None));
                }
                updates.push((
                    destination.clone(),
                    Some(FileFingerprint {
                        content_hash: content_hash(content),
                    }),
                ));
            }
            AppliedPatchFileChange::Delete { .. } => {
                updates.push((change.path.clone(), None));
            }
        }
    }
    if let Ok(mut files) = ledger.files.lock() {
        for (path, fingerprint) in updates {
            match fingerprint {
                Some(fingerprint) => {
                    files.insert(path, fingerprint);
                }
                None => {
                    files.remove(&path);
                }
            }
        }
    }
}

#[cfg(test)]
#[path = "apply_patch_tests.rs"]
mod tests;
