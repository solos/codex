//! Multi-source budgeting and feature rollback through the skills extension.

use super::*;
use pretty_assertions::assert_eq;

#[tokio::test]
async fn skill_sources_spend_the_same_remaining_budget() -> TestResult {
    let mut previous = serde_json::Map::new();
    let enabled = TestConfig {
        catalog_deltas_enabled: true,
        ..default_config()
    };
    render_catalogs(&[], &enabled, &mut previous).await?;

    // One release-notes row fits the 100-token budget. The larger entry sorts first.
    let large = "Detailed architectural guidance. ".repeat(40);
    let small = "Draft release notes. ".repeat(9);
    let entries = [
        ("architecture-review", large.as_str()),
        ("release-notes", small.as_str()),
    ];
    let emitted = render_catalogs(&entries, &enabled, &mut previous).await?;
    assert_eq!(
        emitted
            .lines()
            .filter(|line| line.starts_with("- ") && line.contains("release-notes"))
            .count(),
        1
    );
    assert!(emitted.contains("- executor-release-notes:"), "{emitted}");
    assert!(!emitted.contains("architecture-review"), "{emitted}");
    let used = previous
        .values()
        .filter_map(|s| s["usage"]["tokens"].as_u64())
        .sum::<u64>();
    assert!((50..=100).contains(&used), "{used}");

    let edited = [
        ("architecture-review", "Review architecture."),
        ("release-notes", small.as_str()),
    ];
    assert_eq!(render_catalogs(&edited, &enabled, &mut previous).await?, "");
    let disabled = default_config();
    let empty = render_catalogs(&[], &disabled, &mut previous).await?;
    assert!(empty.contains("skills are currently available"), "{empty}");
    assert_eq!(render_catalogs(&[], &disabled, &mut previous).await?, "");
    let omitted = TestConfig {
        omit_descriptions: true,
        ..enabled
    };
    previous.clear();
    let emitted =
        render_catalogs(&[("notes", "Hidden description.")], &enabled, &mut previous).await?;
    assert!(emitted.contains("description shown above"), "{emitted}");
    assert!(
        emitted.contains("- host-notes: Hidden description."),
        "{emitted}"
    );
    // Resume a snapshot written before the description policy was persisted.
    for snapshot in previous.values_mut() {
        snapshot.as_object_mut().unwrap().remove("omitDescriptions");
    }
    let emitted =
        render_catalogs(&[("notes", "Hidden description.")], &omitted, &mut previous).await?;
    assert!(
        emitted.contains("OR the task clearly matches a skill's name shown above, you must use that skill for that turn."),
        "{emitted}"
    );
    assert!(emitted.contains("- executor-notes: "), "{emitted}");
    assert!(!emitted.contains("Hidden description."), "{emitted}");
    assert_eq!(
        render_catalogs(&[("notes", "Edited description.")], &omitted, &mut previous).await?,
        ""
    );
    // A policy change still refreshes instructions when no more rows fit the budget.
    let emitted =
        render_catalogs(&[("notes", "Edited description.")], &enabled, &mut previous).await?;
    assert!(emitted.contains("description shown above"), "{emitted}");
    assert!(!emitted.contains("- executor-notes:"), "{emitted}");
    assert!(!emitted.contains("- cloud-notes:"), "{emitted}");
    assert!(!emitted.contains("- host-notes:"), "{emitted}");
    assert_eq!(
        render_catalogs(&[("notes", "Edited description.")], &enabled, &mut previous).await?,
        ""
    );
    Ok(())
}
