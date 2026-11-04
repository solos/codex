//! Reusable provider catalog caches.
//! Provider catalogs remain unfiltered until step preparation applies policy.

use std::sync::Arc;

use codex_exec_server::ExecutorCapabilityDiscoverySnapshot;
use codex_exec_server::ResolvedSelectedCapabilityRoot;
use codex_protocol::capabilities::SelectedCapabilityRoot;

use crate::catalog::SkillCatalog;

/// Reusable provider catalogs, before step-specific plugin policy and merging.
#[derive(Clone, Debug, Default)]
pub(crate) struct SkillCatalogState {
    // Roots-only discovery, reused within a turn.
    pub(crate) executor_cache: Option<CachedExecutorCatalog>,
    // DiscoveryV1.
    pub(crate) executor_discovery_cache: Option<CachedExecutorDiscoveryCatalog>,
    // DiscoveryV2.
    pub(crate) executor_v2_cache: Option<CachedExecutorV2Catalog>,
}

/// Reuses the projection of unchanged successful V2 inventories, before plugin policy.
#[derive(Clone, Debug)]
pub(crate) struct CachedExecutorV2Catalog {
    pub(crate) snapshots: Vec<codex_exec_server::ExecutorCapabilityDiscoveryV2Snapshot>,
    pub(crate) catalog: SkillCatalog,
}

impl CachedExecutorV2Catalog {
    pub(crate) fn same_discovery(
        &self,
        snapshots: &[codex_exec_server::ExecutorCapabilityDiscoveryV2Snapshot],
    ) -> bool {
        self.snapshots.len() == snapshots.len()
            && self
                .snapshots
                .iter()
                .zip(snapshots)
                .all(|(previous, current)| previous.same_discovery(current))
    }
}

#[derive(Clone, Debug)]
pub(crate) struct ExecutorCatalogCacheKey {
    pub(crate) turn_id: String,
    pub(crate) resolved_roots: Vec<ResolvedSelectedCapabilityRoot>,
    pub(crate) include_bundled_skills: bool,
}

impl ExecutorCatalogCacheKey {
    pub(crate) fn matches(&self, query: &crate::provider::SkillListQuery) -> bool {
        self.turn_id == query.turn_id
            && self
                .resolved_roots
                .iter()
                .map(ResolvedSelectedCapabilityRoot::selected_root)
                .eq(query.executor_roots.iter())
            && self.include_bundled_skills == query.include_bundled_skills
            // Unbound callers cannot establish identity for a cached filesystem catalog.
            && query.resolved_executor_roots.len() == query.executor_roots.len()
            && self.resolved_roots.iter().zip(&query.resolved_executor_roots).all(|(previous, current)| {
                previous.selected_root() == current.selected_root()
                    && Arc::ptr_eq(previous.environment(), current.environment())
            })
    }
}

#[derive(Clone, Debug)]
pub(crate) struct CachedExecutorCatalog {
    pub(crate) key: ExecutorCatalogCacheKey,
    pub(crate) catalog: SkillCatalog,
}

/// Keeps the latest result; only successful results for unchanged inputs can be reused.
#[derive(Clone, Debug)]
pub(crate) struct CachedExecutorDiscoveryCatalog {
    pub(crate) roots: Vec<SelectedCapabilityRoot>,
    pub(crate) discovery: ExecutorCapabilityDiscoverySnapshot,
    pub(crate) include_bundled_skills: bool,
    pub(crate) catalog: SkillCatalog,
}

impl CachedExecutorDiscoveryCatalog {
    pub(crate) fn matches(
        &self,
        roots: &[SelectedCapabilityRoot],
        discovery: Option<&ExecutorCapabilityDiscoverySnapshot>,
        include_bundled_skills: bool,
    ) -> bool {
        discovery.is_some_and(|discovery| {
            self.roots == roots
                && self.include_bundled_skills == include_bundled_skills
                && self.discovery.sandbox_contexts() == discovery.sandbox_contexts()
                && self.discovery.roots().len() == discovery.roots().len()
                && self.discovery.roots().iter().zip(discovery.roots()).all(
                    |(previous, current)| {
                        previous.selected_root == current.selected_root
                            && match (&previous.result, &current.result) {
                                (Ok(previous), Ok(current)) => Arc::ptr_eq(previous, current),
                                (Err(previous), Err(current)) => previous == current,
                                (Ok(_), Err(_)) | (Err(_), Ok(_)) => false,
                            }
                    },
                )
        })
    }
}
