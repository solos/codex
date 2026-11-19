use std::path::Path;

use anyhow::Result;
use tempfile::TempDir;

fn codex_command(codex_home: &Path) -> Result<assert_cmd::Command> {
    let mut cmd = assert_cmd::Command::new(codex_utils_cargo_bin::cargo_bin("codex")?);
    cmd.env("CODEX_HOME", codex_home);
    Ok(cmd)
}

#[test]
fn model_list_bundled_prints_json() -> Result<()> {
    let codex_home = TempDir::new()?;
    let mut cmd = codex_command(codex_home.path())?;
    let output = cmd
        .args(["model", "list", "--bundled", "--json"])
        .output()?;

    assert!(output.status.success());
    let value: serde_json::Value = serde_json::from_slice(&output.stdout)?;
    assert!(!value.as_array().unwrap_or(&Vec::new()).is_empty());

    Ok(())
}

#[test]
fn model_set_preserves_reasoning_effort_and_comments() -> Result<()> {
    let codex_home = TempDir::new()?;
    let config_path = codex_home.path().join("config.toml");
    std::fs::write(
        &config_path,
        "# model selection\nmodel = \"old-model\"\nmodel_reasoning_effort = \"high\"\n",
    )?;

    let mut cmd = codex_command(codex_home.path())?;
    let output = cmd.args(["model", "set", "gpt-5.4"]).output()?;

    assert!(output.status.success());
    let contents = std::fs::read_to_string(config_path)?;
    let expected = r#"# model selection
model = "gpt-5.4"
model_reasoning_effort = "high"
"#;
    assert_eq!(contents, expected);

    Ok(())
}
