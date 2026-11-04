//! Skill invocation events and privacy rules for local skill identifiers.

use crate::facts::AnalyticsFact;
use crate::facts::CustomAnalyticsFact;
use crate::facts::InvocationType;
use crate::facts::SkillInvocation;
use crate::facts::SkillInvocationLocation;
use crate::facts::SkillInvokedInput;
use crate::reducer::AnalyticsReducer;
use crate::reducer::normalize_path_for_skill_id;
use crate::reducer::skill_id_for_local_skill;
use crate::tests::support::TEST_PRODUCT_CLIENT_ID;
use crate::tests::support::test_tracking_context;
use codex_utils_path_uri::PathUri;
use pretty_assertions::assert_eq;
use serde_json::json;
use std::path::PathBuf;

fn expected_absolute_path(path: &PathBuf) -> String {
    std::fs::canonicalize(path)
        .unwrap_or_else(|_| path.to_path_buf())
        .to_string_lossy()
        .replace('\\', "/")
}

#[test]
fn normalize_path_for_skill_id_repo_scoped_uses_relative_path() {
    let repo_root = std::env::temp_dir().join("repo/root");
    let skill_path = repo_root.join(".codex/skills/doc/SKILL.md");

    let path = normalize_path_for_skill_id(
        Some("https://example.com/repo.git"),
        Some(repo_root.as_path()),
        &PathUri::from_host_native_path(&skill_path).expect("native skill path"),
    );

    assert_eq!(path, ".codex/skills/doc/SKILL.md");
}

#[test]
fn normalize_path_for_skill_id_user_scoped_uses_absolute_path() {
    let skill_path = std::env::temp_dir().join("Users/abc/.codex/skills/doc/SKILL.md");

    let path = normalize_path_for_skill_id(
        /*repo_url*/ None,
        /*repo_root*/ None,
        &PathUri::from_host_native_path(&skill_path).expect("native skill path"),
    );
    let expected = expected_absolute_path(&skill_path);

    assert_eq!(path, expected);
}

#[test]
fn normalize_path_for_skill_id_admin_scoped_uses_absolute_path() {
    let skill_path = std::env::temp_dir().join("etc/codex/skills/doc/SKILL.md");

    let path = normalize_path_for_skill_id(
        /*repo_url*/ None,
        /*repo_root*/ None,
        &PathUri::from_host_native_path(&skill_path).expect("native skill path"),
    );
    let expected = expected_absolute_path(&skill_path);

    assert_eq!(path, expected);
}

#[test]
fn normalize_path_for_skill_id_repo_root_not_in_skill_path_uses_absolute_path() {
    let repo_root = std::env::temp_dir().join("repo/root");
    let skill_path = std::env::temp_dir().join("other/path/.codex/skills/doc/SKILL.md");

    let path = normalize_path_for_skill_id(
        Some("https://example.com/repo.git"),
        Some(repo_root.as_path()),
        &PathUri::from_host_native_path(&skill_path).expect("native skill path"),
    );
    let expected = expected_absolute_path(&skill_path);

    assert_eq!(path, expected);
}

/// Foreign paths keep native telemetry spelling without depending on host filesystem lookup.
#[test]
fn normalize_path_for_skill_id_foreign_path_uses_native_spelling() {
    let (uri, expected) = if cfg!(windows) {
        (
            "file:///Users/abc/.codex/skills/doc%20%23/SKILL.md",
            "/Users/abc/.codex/skills/doc #/SKILL.md",
        )
    } else {
        (
            "file:///C:/Users/abc/.codex/skills/doc%20%23/SKILL.md",
            "C:/Users/abc/.codex/skills/doc #/SKILL.md",
        )
    };
    let path = normalize_path_for_skill_id(
        /*repo_url*/ None,
        /*repo_root*/ None,
        &PathUri::parse(uri).expect("foreign skill URI"),
    );

    assert_eq!(path, expected);
}
