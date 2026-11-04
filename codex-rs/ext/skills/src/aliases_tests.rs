use pretty_assertions::assert_eq;

use super::AliasPlan;
use super::SkillLocator;
use crate::catalog::SkillSourceKind;

#[test]
fn assigns_aliases_in_first_seen_order() {
    let roots = [
        "/skills/beta",
        "/skills/alpha",
        "/skills/beta",
        "/skills/alpha",
    ];
    let plan = AliasPlan::build(&SkillSourceKind::Host, &roots).expect("alias plan should build");

    assert_eq!(
        vec!["- `r0` = `/skills/beta`", "- `r1` = `/skills/alpha`"],
        plan.root_lines()
    );
}

#[test]
fn reuses_aliases_for_duplicate_roots() {
    let root = "/skills/shared";
    let plan = AliasPlan::build(&SkillSourceKind::Host, &[root, root, root])
        .expect("alias plan should build");

    assert_eq!(vec!["- `r0` = `/skills/shared`"], plan.root_lines());
    assert_eq!(
        Some("r0/alpha/SKILL.md".to_string()),
        plan.shorten("/skills/shared/alpha/SKILL.md")
    );
    assert_eq!(
        Some("r0/beta/SKILL.md".to_string()),
        plan.shorten("/skills/shared/beta/SKILL.md")
    );
}

#[test]
fn rejects_unknown_roots_and_locators_outside_the_root() {
    let root = "/skills/shared";
    let plan =
        AliasPlan::build(&SkillSourceKind::Host, &[root, root]).expect("alias plan should build");

    assert_eq!(None, plan.shorten("/skills/other/alpha"));
    assert_eq!(None, plan.shorten("/skills/shared-other/alpha"));
    assert_eq!(None, plan.shorten(root));
}

#[test]
fn returns_none_when_no_roots_are_provided() {
    assert!(AliasPlan::build(&SkillSourceKind::Host, &[]).is_none());
}

#[test]
fn aliases_roots_used_by_only_one_skill() {
    let root = "/skills/singleton";
    let plan = AliasPlan::build(&SkillSourceKind::Host, &[root])
        .expect("singleton root should produce an alias");

    assert_eq!(vec!["- `r0` = `/skills/singleton`"], plan.root_lines());
    assert_eq!(
        Some("r0/alpha/SKILL.md".to_string()),
        plan.shorten("/skills/singleton/alpha/SKILL.md")
    );
}

#[test]
fn appended_aliases_preserve_captured_steps_and_existing_locators() {
    let mut aliases = std::sync::Arc::new(AliasPlan::default());
    let mut budget = super::AliasBudget::default();
    aliases.extend_alias_plan("e", ["skill://root"], &mut budget);
    let previous = std::sync::Arc::clone(&aliases);
    let locator = "skill://root/nested/demo";
    let original = aliases.shorten(locator).unwrap();

    aliases.extend_alias_plan("e", ["skill://root/nested", "skill://root"], &mut budget);
    assert_eq!(previous.root_lines(), vec!["- `e0` = `skill://root`"]);
    assert_eq!(previous.shorten(locator), Some(original.clone()));
    assert_eq!(aliases.shorten(locator), Some("e1/demo".into()));
    assert_eq!(aliases.expand(&original).as_deref(), Some(locator));
    assert_eq!(aliases.expand("e1/demo").as_deref(), Some(locator));
    assert_eq!(
        aliases.root_lines_for(&std::collections::HashSet::from(["e1".to_string()])),
        vec!["- `e1` = `skill://root/nested`"]
    );
}

#[test]
fn alias_number_and_byte_budgets_are_shared_without_reassigning_existing_roots() {
    let mut first = std::sync::Arc::new(AliasPlan::default());
    let mut executor = std::sync::Arc::new(AliasPlan::default());
    let mut budget = super::AliasBudget {
        roots: super::MAX_ALIAS_ROOTS - 2,
        ..Default::default()
    };
    first.extend_alias_plan("a", ["skill://first-root"], &mut budget);
    executor.extend_alias_plan("e", ["skill://first", "skill://second"], &mut budget);
    assert_eq!(budget.roots, super::MAX_ALIAS_ROOTS);
    assert_eq!(executor.root_lines(), vec!["- `e0` = `skill://first`"]);
    assert_eq!(executor.shorten("skill://second/demo"), None);
    assert_eq!(
        first.shorten("skill://first-root/demo"),
        Some("a0/demo".into())
    );

    let mut budget = super::AliasBudget::default();
    let mut aliases = std::sync::Arc::new(AliasPlan::default());
    let oversized = "x".repeat(super::MAX_ALIAS_BYTES);
    aliases.extend_alias_plan("e", ["skill://first", &oversized], &mut budget);
    aliases.extend_alias_plan("e", ["skill://second"], &mut budget);
    assert_eq!(
        aliases.root_lines(),
        vec!["- `e0` = `skill://first`", "- `e1` = `skill://second`"]
    );
    assert_eq!(aliases.shorten(&format!("{oversized}/demo")), None);
}
