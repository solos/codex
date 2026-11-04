use std::path::Path;

use anyhow::Result;
use tempfile::TempDir;

const THREAD_ID: &str = "123e4567-e89b-12d3-a456-426614174000";

#[test]
fn queue_rejects_empty_message() -> Result<()> {
    let output = std::process::Command::new(codex_utils_cargo_bin::cargo_bin("codex")?)
        .args(["queue", "--thread", THREAD_ID, "--message", ""])
        .output()?;
    assert!(!output.status.success());
    Ok(())
}

#[test]
fn queue_rejects_unsupported_options() -> Result<()> {
    let codex_home = TempDir::new()?;
    for (args, expected) in [
        ("queue --image screenshot.png", "image attachments"),
        ("queue --model other-model", "`--model`"),
        ("--oss queue", "`--oss`"),
        ("queue --local-provider ollama", "`--local-provider`"),
        (
            "queue --dangerously-bypass-hook-trust",
            "`--dangerously-bypass-hook-trust`",
        ),
        (
            "-c model=other queue -c analytics.enabled=false",
            "`model` config override",
        ),
        ("queue -s read-only", "`--sandbox`"),
        ("--approve-for-me queue", "`--approve-for-me`"),
        ("prompt queue", "positional prompt; use `--message`"),
    ] {
        let output = std::process::Command::new(codex_utils_cargo_bin::cargo_bin("codex")?)
            .env("CODEX_HOME", codex_home.path())
            .args(args.split_whitespace())
            .args(["--thread", THREAD_ID, "--message", "do the thing"])
            .output()?;
        assert!(!output.status.success());
        let stderr = String::from_utf8(output.stderr)?;
        assert!(
            stderr.contains("`codex queue`") && stderr.contains(expected),
            "{args:?}: {stderr}"
        );
    }
    Ok(())
}

#[test]
fn session_commands_validate_config() -> Result<()> {
    let cases: &[(&[&str], &str, &str)] = &[
        (
            &["--strict-config"],
            "config.toml",
            "unknown_queue_option = true\n",
        ),
        (&["--profile", "queue"], "queue.config.toml", "model = 42\n"),
        (
            &[],
            "config.toml",
            "experimental_thread_store_endpoint = \"https://example.com\"\n",
        ),
    ];
    let commands: &[&[&str]] = &[
        &["queue", "--thread", THREAD_ID, "--message", "do the thing"],
        &["archive", THREAD_ID],
        &["unarchive", THREAD_ID],
        &["delete", "--force", THREAD_ID],
    ];
    for &command in commands {
        for &(args, config_file, config) in cases {
            let codex_home = TempDir::new()?;
            std::fs::write(codex_home.path().join(config_file), config)?;
            let output = std::process::Command::new(codex_utils_cargo_bin::cargo_bin("codex")?)
                .env("CODEX_HOME", codex_home.path())
                .current_dir(codex_home.path())
                .args(args)
                .args(command)
                .output()?;
            let stderr = String::from_utf8(output.stderr)?;
            assert!(!output.status.success());
            assert!(
                stderr.contains("failed to load config.toml"),
                "{command:?}, {config:?}: {stderr}"
            );
        }
    }
    Ok(())
}
