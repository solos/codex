#![allow(clippy::unwrap_used, unused_imports)]

use anyhow::Context;
use assert_cmd::prelude::*;
use codex_apply_patch::CODEX_CORE_APPLY_PATCH_ARG1;
use core_test_support::responses::ev_apply_patch_custom_tool_call;
use core_test_support::responses::ev_assistant_message;
use core_test_support::responses::ev_completed;
use core_test_support::responses::ev_exec_command_call_with_args;
use core_test_support::responses::mount_sse_sequence;
use core_test_support::responses::sse;
use core_test_support::responses::start_mock_server;
use pretty_assertions::assert_eq;
use std::fs;
use std::process::Command;
use tempfile::tempdir;

/// While we may add an `apply-patch` subcommand to the `codex` CLI multitool
/// at some point, we must ensure that the smaller `codex-exec` CLI can still
/// emulate the `apply_patch` CLI.
#[test]
fn test_standalone_exec_cli_can_use_apply_patch() -> anyhow::Result<()> {
    let tmp = tempdir()?;
    let relative_path = "source.txt";
    let absolute_path = tmp.path().join(relative_path);
    fs::write(&absolute_path, "original content\n")?;

    Command::new(codex_utils_cargo_bin::cargo_bin("codex-exec")?)
        .arg(CODEX_CORE_APPLY_PATCH_ARG1)
        .arg(
            r#"*** Begin Patch
*** Update File: source.txt
@@
-original content
+modified by apply_patch
*** End Patch"#,
        )
        .current_dir(tmp.path())
        .assert()
        .success()
        .stdout("Success. Updated the following files:\nM source.txt\n")
        .stderr(predicates::str::is_empty());
    assert_eq!(
        fs::read_to_string(absolute_path)?,
        "modified by apply_patch\n"
    );
    Ok(())
}

#[cfg(not(target_os = "windows"))]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_apply_patch_tool() -> anyhow::Result<()> {
    use core_test_support::skip_if_no_network;
    use core_test_support::test_codex_exec::test_codex_exec;

    skip_if_no_network!(Ok(()));

    let test = test_codex_exec();
    let tmp_path = test.cwd_path().to_path_buf();
    let add_patch = r#"*** Begin Patch
*** Add File: test.md
+Hello world
*** End Patch"#;
    let update_patch = r#"*** Begin Patch
*** Update File: test.md
@@
-Hello world
+Final text
*** End Patch"#;
    let response_streams = vec![
        sse(vec![
            ev_apply_patch_custom_tool_call("request_0", add_patch),
            ev_completed("request_0"),
        ]),
        sse(vec![
            ev_apply_patch_custom_tool_call("request_1", update_patch),
            ev_completed("request_1"),
        ]),
        sse(vec![ev_completed("request_2")]),
    ];
    let server = start_mock_server().await;
    mount_sse_sequence(&server, response_streams).await;

    test.cmd_with_server(&server)
        .arg("--skip-git-repo-check")
        .arg("-s")
        .arg("danger-full-access")
        .arg("foo")
        .assert()
        .success();

    let final_path = tmp_path.join("test.md");
    let contents = std::fs::read_to_string(&final_path).expect("final file should be readable");
    assert_eq!(contents, "Final text\n");
    Ok(())
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn human_output_prints_unchanged_turn_diff_once() -> anyhow::Result<()> {
    use core_test_support::skip_if_no_network;
    use core_test_support::test_codex_exec::test_codex_exec;

    skip_if_no_network!(Ok(()));

    let test = test_codex_exec();
    let run_dir = test.cwd_path();
    Command::new("git")
        .args(["init", "--quiet"])
        .current_dir(run_dir)
        .assert()
        .success();
    fs::write(run_dir.join("probe.txt"), "value=1\n")?;
    Command::new("git")
        .args(["add", "probe.txt"])
        .current_dir(run_dir)
        .assert()
        .success();
    Command::new("git")
        .args([
            "-c",
            "user.name=Reproducer",
            "-c",
            "user.email=reproducer@example.invalid",
            "commit",
            "--quiet",
            "-m",
            "baseline",
        ])
        .current_dir(run_dir)
        .assert()
        .success();

    // Replay the reported single edit, unrelated command, and final response.
    let server = start_mock_server().await;
    let mock = mount_sse_sequence(
        &server,
        vec![
            sse(vec![
                ev_apply_patch_custom_tool_call(
                    "patch",
                    "*** Begin Patch\n*** Update File: probe.txt\n@@\n-value=1\n+value=2\n*** End Patch",
                ),
                ev_completed("response-1"),
            ]),
            sse(vec![
                ev_exec_command_call_with_args(
                    "command",
                    &serde_json::json!({
                        "cmd": "printf 'DIAG_CMD_OK\\n'",
                        "shell": "/bin/sh",
                        "login": false,
                        "yield_time_ms": 10_000,
                    }),
                ),
                ev_completed("response-2"),
            ]),
            sse(vec![
                ev_assistant_message("message", "DIAG_DONE"),
                ev_completed("response-3"),
            ]),
        ],
    )
    .await;
    let result = test
        .cmd_with_server(&server)
        .args([
            "--ephemeral",
            "--color",
            "never",
            "-s",
            "danger-full-access",
            "-c",
            "features.hooks=false",
            "-c",
            "features.shell_snapshot=false",
            "-c",
            "features.shell_snapshot_v2=false",
            "Replace value=1 with value=2 once, print DIAG_CMD_OK, then finish with DIAG_DONE.",
        ])
        .assert()
        .success()
        .stdout("DIAG_DONE\n");
    let stderr = String::from_utf8_lossy(&result.get_output().stderr);
    assert_eq!(fs::read_to_string(run_dir.join("probe.txt"))?, "value=2\n");
    assert_eq!(mock.requests().len(), 3);
    assert_eq!(stderr.matches("patch: completed").count(), 1, "{stderr}");
    assert_eq!(
        stderr.lines().filter(|line| *line == "DIAG_CMD_OK").count(),
        1,
        "{stderr}"
    );
    assert_eq!(
        stderr
            .lines()
            .filter(|line| *line == "diff --git a/probe.txt b/probe.txt")
            .count(),
        1,
        "{stderr}"
    );
    Ok(())
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn nested_apply_patch_alias_works_with_restricted_reads() -> anyhow::Result<()> {
    use core_test_support::skip_if_no_network;
    use core_test_support::skip_if_sandbox;
    use core_test_support::test_codex_exec::test_codex_exec;

    skip_if_no_network!(Ok(()));
    skip_if_sandbox!(Ok(()));
    #[cfg(target_os = "linux")]
    if super::sandbox::linux_sandbox_test_env().await.is_none() {
        return Ok(());
    }
    let test = test_codex_exec();
    let binary = codex_utils_cargo_bin::cargo_bin("codex-exec")?.canonicalize()?;
    let binary_dir = serde_json::to_string(binary.parent().expect("binary directory"))?;
    let codex_home = serde_json::to_string(test.home_path())?;
    fs::write(
        test.home_path().join("config.toml"),
        format!(
            r#"default_permissions = "aliases"
approval_policy = "never"
[features]
shell_snapshot = false
shell_snapshot_v2 = false
[permissions.aliases.filesystem]
":minimal" = "read"
":project_roots" = "write"
{binary_dir} = "read"
{codex_home} = "none"
"#
        ),
    )?;
    fs::write(
        test.home_path().join("outside-workspace.txt"),
        "must remain unreadable to the script",
    )?;
    fs::write(
        test.cwd_path().join("patch.sh"),
        "if cat \"$HOME/outside-workspace.txt\" >/dev/null 2>&1; then exit 99; fi\napply_patch <<'PATCH'\n*** Begin Patch\n*** Add File: patched.txt\n+through the nested alias\n*** End Patch\nPATCH\n",
    )?;
    let server = start_mock_server().await;
    let mock = mount_sse_sequence(
        &server,
        vec![
            sse(vec![
                ev_exec_command_call_with_args(
                    "nested-patch",
                    &serde_json::json!({
                        "cmd": "/bin/sh patch.sh",
                        "shell": "/bin/sh",
                        "login": false,
                        "yield_time_ms": 10_000,
                    }),
                ),
                ev_completed("response-1"),
            ]),
            sse(vec![ev_completed("response-2")]),
        ],
    )
    .await;
    let run = test.cmd_with_server(&server)
        .env("HOME", test.home_path())
        .env_remove("CODEX_API_KEY")
        .env_remove("OPENAI_API_KEY")
        .arg("--skip-git-repo-check")
        .arg("-c")
        .arg("default_permissions=\"aliases\"")
        .arg("-c")
        .arg("approvals_reviewer=\"user\"")
        .arg("-c")
        .arg("model_provider=\"test\"")
        .arg("-c")
        .arg(format!(
            "model_providers.test={{name=\"test\",base_url={:?},wire_api=\"responses\",requires_openai_auth=false,supports_websockets=false}}",
            format!("{}/v1", server.uri())
        ))
        .arg("run the patch script")
        .assert()
        .success();
    let output = mock
        .function_call_output_text("nested-patch")
        .expect("tool output");
    assert!(
        output.contains("Process exited with code 0"),
        "{output}\n{}",
        String::from_utf8_lossy(&run.get_output().stderr)
    );
    assert_eq!(
        fs::read_to_string(test.cwd_path().join("patched.txt"))?,
        "through the nested alias\n"
    );
    Ok(())
}

#[cfg(not(target_os = "windows"))]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_apply_patch_freeform_tool() -> anyhow::Result<()> {
    use core_test_support::skip_if_no_network;
    use core_test_support::test_codex_exec::test_codex_exec;

    skip_if_no_network!(Ok(()));

    let test = test_codex_exec();
    let freeform_add_patch = r#"*** Begin Patch
*** Add File: app.py
+class BaseClass:
+  def method():
+    return False
*** End Patch"#;
    let freeform_update_patch = r#"*** Begin Patch
*** Update File: app.py
@@  def method():
-    return False
+
+    return True
*** End Patch"#;
    let response_streams = vec![
        sse(vec![
            ev_apply_patch_custom_tool_call("request_0", freeform_add_patch),
            ev_completed("request_0"),
        ]),
        sse(vec![
            ev_apply_patch_custom_tool_call("request_1", freeform_update_patch),
            ev_completed("request_1"),
        ]),
        sse(vec![ev_completed("request_2")]),
    ];
    let server = start_mock_server().await;
    mount_sse_sequence(&server, response_streams).await;

    test.cmd_with_server(&server)
        .arg("--skip-git-repo-check")
        .arg("-s")
        .arg("danger-full-access")
        .arg("foo")
        .assert()
        .success();

    // Verify final file contents
    let final_path = test.cwd_path().join("app.py");
    let contents = std::fs::read_to_string(&final_path).expect("final file should be readable");
    assert_eq!(
        contents,
        include_str!("../fixtures/apply_patch_freeform_final.txt")
    );
    Ok(())
}
