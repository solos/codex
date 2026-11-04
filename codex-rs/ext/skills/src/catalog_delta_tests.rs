use super::*;
use pretty_assertions::assert_eq;

#[test]
fn update_spends_budget_only_on_accepted_rows_and_their_new_roots() {
    let mut draft = UpdateDraft {
        message: CatalogMessage::Update {
            heading: "\n## Executor skills update\n".to_string(),
        },
        skill_lines: Vec::new(),
        root_lines: Vec::new(),
    };
    let skill = |line: &str, alias: &str, root: &str| CatalogSkill {
        snapshot: SkillEntrySnapshot {
            fingerprint: String::new(),
            shown: false,
        },
        line: line.to_string(),
        root: Some(AliasRoot {
            name: alias.to_string(),
            value: root.into(),
        }),
    };
    let notes = skill(
        "- release-notes: (executor package: e0/notes)",
        "c0",
        "skill://writing",
    );
    let docs = skill(
        "- docs: (executor package: e0/docs)",
        "e0",
        "skill://writing",
    );
    let expected = "\n## Executor skills update\n### Skill roots\nThese mappings replace earlier mappings for the same aliases.\n- `e0` = `skill://writing`\n### Added or updated skills:\n- release-notes: (executor package: e0/notes)\n- docs: (executor package: e0/docs)\n";
    let mut remaining = SkillMetadataBudget::Characters(expected.chars().count());
    let mut snapshot = CatalogSnapshot::default();
    let large = skill(&"a".repeat(remaining.limit()), "c1", "skill://design");

    assert!(!draft.try_append_skill(&large, &mut snapshot, &mut remaining));
    assert!(draft.try_append_skill(&notes, &mut snapshot, &mut remaining));
    assert!(draft.try_append_skill(&docs, &mut snapshot, &mut remaining));

    assert_eq!(draft.into_body("Executor").as_deref(), Some(expected));
    assert_eq!(
        snapshot.roots,
        BTreeMap::from([("c0".into(), "skill://writing".into())])
    );
    assert_eq!(snapshot.usage.characters, expected.chars().count());
    assert_eq!(remaining, SkillMetadataBudget::Characters(0));
}
