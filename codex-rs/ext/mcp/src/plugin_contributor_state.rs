//! Thread-owned catalog and stable selected-root metadata, guarded by a short-lived mutex.

use crate::catalog_observability::record_catalog;
use codex_core_plugins::PluginCatalog;
use codex_core_plugins::PluginRootOwnership;
use codex_core_plugins::catalog_from_snapshots;
use codex_exec_server::ExecutorCapabilityDiscoveryV2Snapshot;
use codex_exec_server::FileSystemSandboxContext;
use codex_extension_api::ExtensionMetrics;
use codex_extension_api::SelectedPluginContribution;
use codex_protocol::capabilities::SelectedCapabilityRoot;
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::MutexGuard;

/// Plugin configuration and caches owned by one thread; mutable state stays extension-private.
#[derive(Default)]
pub struct PluginsThreadState {
    state: Mutex<PluginContributorState>,
}

impl PluginsThreadState {
    pub(crate) fn contributor_state(&self) -> MutexGuard<'_, PluginContributorState> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

/// `None` is unpublished; an empty catalog is a successful discovery result.
#[derive(Default)]
pub(crate) struct PluginContributorState {
    pub(crate) extension_metrics: Option<Arc<dyn ExtensionMetrics>>,
    pub(crate) plugins_enabled: bool,
    pub(crate) executor_catalog: Option<PluginCatalog>,
    executor_snapshots: Vec<ExecutorCapabilityDiscoveryV2Snapshot>,
    pub(crate) executor_cache: Vec<CachedSelectedRoot>,
}

impl PluginContributorState {
    /// Reuses unchanged discovery and preserves surviving installations' order on refresh.
    pub(crate) fn update_executor_catalog(
        &mut self,
        snapshots: &[ExecutorCapabilityDiscoveryV2Snapshot],
    ) {
        if self.executor_catalog.is_some() && self.executor_snapshots == snapshots {
            return;
        }
        let mut catalog = catalog_from_snapshots(snapshots);
        if let Some(previous) = &self.executor_catalog {
            // Preserve installation order to keep MCPs on the same executor while it remains available.
            let mut previous_ranks = HashMap::with_capacity(previous.entries.len());
            for (rank, entry) in previous.entries.iter().enumerate() {
                previous_ranks
                    .entry((&entry.id, entry.locations.as_slice()))
                    .or_insert(rank);
            }
            catalog.entries.sort_by_key(|entry| {
                previous_ranks
                    .get(&(&entry.id, entry.locations.as_slice()))
                    .copied()
                    .unwrap_or(usize::MAX)
            });
        }
        record_catalog(self.extension_metrics.as_deref(), "executor", &catalog);
        self.executor_catalog = Some(catalog);
        self.executor_snapshots = snapshots.to_vec();
    }

    pub(crate) fn clear_executor_catalog(&mut self) {
        self.executor_catalog = None;
        self.executor_snapshots.clear();
    }
}

pub(crate) struct CachedSelectedRoot {
    pub(crate) root: SelectedCapabilityRoot,
    pub(crate) ownership: Option<PluginRootOwnership>,
    pub(crate) ownership_sandbox: Option<Box<FileSystemSandboxContext>>,
    pub(crate) metadata: CachedPluginMetadata,
}

/// Denied roots retain ownership without loading their capabilities.
pub(crate) enum CachedPluginMetadata {
    Unloaded,
    Loaded(Option<SelectedPluginContribution>),
}

/// Bounded declarations projected from merged plugin metadata.
#[derive(Clone, Default)]
pub(crate) struct SelectedPluginMetadata {
    pub(crate) plugin_display_name: String,
    pub(crate) servers: Vec<(String, codex_config::McpServerConfig)>,
    pub(crate) connector_ids: Vec<String>,
}

#[cfg(test)]
#[path = "plugin_contributor_state_tests.rs"]
mod tests;
