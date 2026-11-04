use super::*;
use crate::legacy_core::config::ConfigBuilder;
use app_test_support::create_fake_rollout;
use app_test_support::rollout_path;
use codex_config::LoaderOverrides;
use codex_protocol::ThreadId;
use pretty_assertions::assert_eq;
use std::cell::Cell;
use tempfile::TempDir;

async fn start_session(
    app_server: &mut AppServerSession,
    config: &Config,
    target: &SessionTarget,
    action: SessionStartAction,
    confirm: impl AsyncFnOnce() -> Result<UnarchiveChoice>,
) -> Result<SessionStartOutcome> {
    let local_settings = LocalSettings::from(config);
    let initial_result = action
        .start(app_server, config, &local_settings, target)
        .await;
    complete_session_start(
        app_server,
        SessionStartConfig {
            config,
            local_settings: &local_settings,
        },
        &crate::AppServerTarget::Embedded,
        target,
        action,
        initial_result,
        confirm,
    )
    .await
}
#[test]
fn session_start_error_surfaces_archived_guidance_without_rollout_path() {
    let thread_id =
        ThreadId::from_string("019e72f4-e09a-70f2-b2c2-a153a57b8cc0").expect("thread id");
    let target_session = SessionTarget {
        backend_id: crate::backend_id::BackendId::new(),
        path: Some(std::path::PathBuf::from(
            "/Users/me/.codex/archived_sessions/rollout.jsonl",
        )),
        thread_id,
        cwd: None,
        history_mode: None,
    };
    let expected = format!(
        "session {thread_id} is archived. Run `codex unarchive {thread_id}` to unarchive it first."
    );

    for action in ["resume", "fork"] {
        let err = color_eyre::eyre::eyre!(
            "thread/{action} failed during TUI bootstrap: thread/{action} failed: {expected} (code -32600)"
        );

        assert_eq!(
            session_start_error(action, &target_session, err).to_string(),
            expected
        );
    }
}
