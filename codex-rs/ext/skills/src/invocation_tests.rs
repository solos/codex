use pretty_assertions::assert_eq;

use super::*;
use crate::catalog::SkillAuthority;
use crate::catalog::SkillCatalog;
use crate::catalog::SkillCatalogEntry;
use crate::catalog::SkillPackageId;
use crate::catalog::SkillResourceId;

#[test]
fn detects_executor_skill_document_reads() {
    let invocation = detect_executor_skill("cat demo/SKILL.md", &["executor-a"])
        .expect("document read should identify the executor skill");

    assert_eq!(invocation.skill_name, "executor-a-skill");
    assert!(matches!(
        invocation.location,
        SkillInvocationLocation::Resource { id, .. }
            if id == "skill://executor-a/demo/SKILL.md"
    ));
}

#[test]
fn detects_executor_skill_script_runs() {
    let invocation = detect_executor_skill("python demo/scripts/run.py", &["executor-a"])
        .expect("script execution should identify the executor skill");

    assert_eq!(invocation.skill_name, "executor-a-skill");
}

#[test]
fn ignores_matching_paths_from_other_environments() {
    let invocation = detect_executor_skill("cat demo/SKILL.md", &["executor-b", "executor-a"])
        .expect("matching path should resolve to the execution environment");

    assert_eq!(invocation.skill_name, "executor-a-skill");
    assert!(detect_executor_skill("cat demo/SKILL.md", &["executor-b"]).is_none());
}

#[test]
fn ignores_commands_without_skill_access() {
    assert!(detect_executor_skill("echo hello", &["executor-a"]).is_none());
}
