//! Checks saved-directory fallback and the user-facing retained-checkout notice.
use super::*;
use clap::Parser;
use pretty_assertions::assert_eq;

#[test]
fn recovery_message_names_checkout_and_safe_manual_action() {
    let recovery = StartupRecovery {
        root: PathBuf::from("checkout with spaces"),
        // This test inspects rendering without printing or changing terminal state on drop.
        finished: AtomicBool::new(true),
    };
    insta::assert_snapshot!(recovery.message(), @r#"
    Startup did not finish binding a thread to this worktree: "checkout with spaces"
    The checkout was kept. Inspect it and confirm no session is using it.
    To remove it, run `git worktree remove <checkout-path>` from the source repository,
    replacing <checkout-path> with the path above. Do not use --force.
    "#);
}
#[tokio::test]
async fn latest_directory_uses_turn_context_and_preserves_fallback() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let fallback = dir.path().join("old");
    let latest = dir.path().join("latest");
    let path = dir.path().join("rollout.jsonl");
    let turn = serde_json::json!({
        "timestamp": "2026-09-04T00:00:00Z", "type": "turn_context", "payload": {
            "cwd": latest, "approval_policy": "never", "sandbox_policy": {"type":"read-only"},
            "model": "test-model", "summary": "auto"
        }
    });
    std::fs::write(&path, format!("{turn}\n"))?;
    assert_eq!(
        latest_thread_cwd(Some(path.clone()), fallback.clone()).await,
        latest
    );
    std::fs::write(&path, "")?;
    assert_eq!(
        latest_thread_cwd(Some(path), fallback.clone()).await,
        fallback
    );
    assert_eq!(
        latest_thread_cwd(/*path*/ None, fallback.clone()).await,
        fallback
    );
    Ok(())
}
