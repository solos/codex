//! Tests thread-owned discovery caching, installation ordering, and contribution projection.

use super::*;
use crate::PluginProviders;
use crate::plugin_contributor::PluginContributor;
use codex_core::config::ConfigBuilder;
use codex_core_plugins::ExecutorPluginProvider;
use codex_core_plugins::PluginCatalogEntry;
use codex_core_plugins::PluginIdentity;
use codex_core_plugins::PluginProvider;
use codex_core_plugins::PluginSourceLocation;
use codex_exec_server::CapabilityTextFile;
use codex_exec_server::DiscoverV2CapabilitiesResponse;
use codex_exec_server::DiscoveredPluginFiles;
use codex_exec_server::EnvironmentManager;
use codex_exec_server::ExecutorCapabilityDiscoveryCache;
use codex_exec_server::ExecutorPlugin;
use codex_exec_server::LOCAL_ENVIRONMENT_ID;
use codex_extension_api::ExtensionData;
use codex_extension_api::ExtensionDataInit;
use codex_extension_api::McpServerContribution;
use codex_extension_api::McpServerContributionContext;
use codex_extension_api::McpServerContributor;
use codex_features::Feature;
use codex_protocol::capabilities::CapabilityRootLocation;
use codex_utils_path_uri::PathUri;
use pretty_assertions::assert_eq;
use std::collections::BTreeMap;
use std::sync::Arc;

async fn contribution_labels(
    contributor: &PluginContributor,
    context: McpServerContributionContext<'_, codex_core::config::Config>,
) -> Vec<String> {
    let mut labels = Vec::new();
    let policy = codex_config::types::PluginsConfigToml::default();
    for plugin in contributor.selected_plugins(context, &policy).await {
        if !plugin.enabled {
            labels.push(format!("disabled:{}", plugin.plugin_id));
            continue;
        }
        let contribution = plugin.mcp.await;
        labels.extend(
            contribution
                .servers
                .into_iter()
                .map(|(name, _)| format!("mcp:{name}")),
        );
        labels.push(plugin.selected_root_id);
    }
    for contribution in contributor.contribute(context).await {
        if let McpServerContribution::HostedPluginConnectors { plugin_id, .. } = contribution {
            labels.push(format!("cloud:{plugin_id}"));
        } else {
            panic!("unexpected contribution: {contribution:?}");
        }
    }
    labels
}

#[test]
fn executor_refresh_preserves_order_and_removes_stale_entries() -> anyhow::Result<()> {
    let mut state = PluginContributorState::default();
    // Reuse equal snapshots, including equal responses allocated by a later discovery.
    let b = executor_snapshot("executor-b")?;
    let c = executor_snapshot("executor-c")?;
    state.update_executor_catalog(&[b.clone(), c.clone()]);
    state.update_executor_catalog(&[executor_snapshot("executor-b")?, c.clone()]);
    assert!(Arc::ptr_eq(
        state.executor_snapshots[0].result.as_ref().unwrap(),
        b.result.as_ref().unwrap(),
    ));

    // Keep B ahead of C even when discovery reverses them and adds an earlier-named A.
    let a = executor_snapshot("executor-a")?;
    let mut updated_b = b;
    Arc::make_mut(updated_b.result.as_mut().unwrap()).plugins[0].display_name =
        "Updated".to_string();
    state.update_executor_catalog(&[a.clone(), c.clone(), updated_b.clone()]);
    assert_eq!(
        state.executor_catalog.as_ref().unwrap(),
        &catalog_from_snapshots(&[updated_b.clone(), c.clone(), a.clone()]),
    );
    // Removing B promotes C; B returning later appends it rather than displacing C.
    state.update_executor_catalog(&[a.clone(), c.clone()]);
    state.update_executor_catalog(&[updated_b.clone(), a.clone(), c.clone()]);
    assert_eq!(
        state.executor_catalog.as_ref().unwrap(),
        &catalog_from_snapshots(&[c.clone(), a.clone(), updated_b]),
    );
    // A failed scan removes that environment's entries, and recovery rebuilds them.
    let failed = ExecutorCapabilityDiscoveryV2Snapshot {
        environment_id: c.environment_id.clone(),
        result: Err("discovery failed".to_string()),
    };
    state.update_executor_catalog(&[failed.clone(), a.clone()]);
    assert_eq!(
        state.executor_catalog.as_ref().unwrap(),
        &catalog_from_snapshots(&[failed, a.clone()]),
    );
    state.update_executor_catalog(&[c.clone(), a.clone()]);
    assert_eq!(
        state.executor_catalog.as_ref().unwrap(),
        &catalog_from_snapshots(&[a, c]),
    );
    Ok(())
}
