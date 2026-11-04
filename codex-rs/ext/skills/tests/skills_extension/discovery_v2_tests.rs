//! Verifies metadata-only V2 skill projection and refresh without legacy discovery.

use super::*;
use codex_exec_server::DiscoverV2CapabilitiesResponse;
use codex_exec_server::DiscoveredPluginFiles;
use codex_exec_server::EnvironmentManager;
use codex_exec_server::ExecutorCapabilityDiscoveryV2Snapshot;
use codex_exec_server::ExecutorPlugin;
use codex_exec_server::ExecutorSkill;
use codex_skills::SkillDependencies;
use codex_skills::SkillPolicy;
use codex_skills::SkillToolDependency;
use codex_skills_extension::ExecutorSkillProvider;
use codex_skills_extension::merge_catalogs;
use pretty_assertions::assert_eq;

#[tokio::test]
async fn v2_empty_failed_and_partial_results_never_load_legacy_roots() -> TestResult {
    let provider = ExecutorSkillProvider::new_with_restriction_product(
        Arc::new(EnvironmentManager::default_for_tests()),
        /*restriction_product*/ None,
    );
    let successful = installation("executor-a")?;
    let failed = ExecutorCapabilityDiscoveryV2Snapshot {
        environment_id: "executor-b".to_string(),
        result: Err("discovery unavailable".to_string()),
    };
    let query = |snapshots| SkillListQuery {
        turn_id: "turn-1".to_string(),
        // A legacy lookup would report an unavailable environment. V2 must ignore this root.
        executor_roots: vec![SelectedCapabilityRoot {
            id: "legacy".to_string(),
            location: CapabilityRootLocation::Environment {
                environment_id: "unavailable-legacy-executor".to_string(),
                path: PathUri::parse("file:///legacy").unwrap(),
            },
        }],
        resolved_executor_roots: Vec::new(),
        host_snapshot: None,
        include_host_skills: false,
        include_bundled_skills: false,
        mcp_resources: None,
        executor_capability_discovery: None,
        executor_capability_discovery_v2: Some(snapshots),
    };
    let successful_catalog = provider.list(query(vec![successful.clone()])).await?;
    let failure_warning =
        "Environment `executor-b` discovery failed: discovery unavailable".to_string();
    for (snapshots, expected) in [
        (Vec::new(), SkillCatalog::default()),
        (
            vec![failed.clone()],
            SkillCatalog {
                is_partial: true,
                entries: Vec::new(),
                warnings: vec![failure_warning.clone()],
            },
        ),
        (
            vec![successful, failed],
            SkillCatalog {
                is_partial: true,
                warnings: vec![failure_warning],
                ..successful_catalog
            },
        ),
    ] {
        assert_eq!(provider.list(query(snapshots)).await?, expected);
    }
    Ok(())
}
