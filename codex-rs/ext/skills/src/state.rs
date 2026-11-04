//! Shared skill lifecycle state, including session resources and thread-owned caches.
//! Keeps discovery caches and live authorization checks separate from step preparation.

use std::collections::HashSet;
use std::sync::Arc;
use std::sync::Mutex;

use codex_exec_server::EnvironmentAccessKey;
use codex_extension_api::ExtensionData;
use codex_extension_api::ExtensionMetrics;
use codex_mcp::McpResourceClient;

use crate::SkillsExtensionConfig;
use crate::aliases::SkillAliasState;
use crate::catalog::SkillAuthority;
use crate::catalog::SkillCatalog;
use crate::catalog::SkillPackageId;
use crate::catalog::SkillReadResult;
use crate::catalog::SkillSourceKind;
use crate::catalog_observability::record_catalog;
use crate::catalog_state::CachedExecutorCatalog;
use crate::catalog_state::CachedExecutorDiscoveryCatalog;
use crate::catalog_state::CachedExecutorV2Catalog;
use crate::catalog_state::ExecutorCatalogCacheKey;
use crate::catalog_state::SkillCatalogState;
use crate::provider::SkillListQuery;
use crate::shadow_selection_experiment::RecentSkillInvocations;
use crate::shadow_selection_experiment::ShadowSelectionTurnState;
use crate::shadow_selection_experiment::ShadowTaskContext;
use crate::sources::SkillProviders;
use crate::step_state::SkillStepCatalog;

pub(crate) struct SkillsSessionState {
    pub(crate) mcp_resources: Option<Arc<McpResourceClient>>,
    pub(crate) extension_metrics: Option<Arc<dyn ExtensionMetrics>>,
}

/// Thread-owned skill configuration and caches; consumers can only read catalog snapshots.
pub struct SkillsThreadState {
    config: Mutex<SkillsExtensionConfig>,
    catalog_state: Mutex<SkillCatalogState>,
    extension_metrics: Option<Arc<dyn ExtensionMetrics>>,
    aliases: SkillAliasState,
    shadow_selection_turn: Mutex<Option<ShadowSelectionTurn>>,
    pub(crate) executor_read_snapshot: Mutex<Option<ExecutorReadSnapshot>>,
    pub(crate) recent_skill_invocations: Arc<RecentSkillInvocations>,
    pub(crate) shadow_task_context: Arc<ShadowTaskContext>,
}

impl SkillsThreadState {
    pub(crate) fn new(
        config: SkillsExtensionConfig,
        extension_metrics: Option<Arc<dyn ExtensionMetrics>>,
    ) -> Self {
        Self {
            config: Mutex::new(config),
            catalog_state: Mutex::new(SkillCatalogState::default()),
            extension_metrics,
            aliases: SkillAliasState::default(),
            shadow_selection_turn: Mutex::new(None),
            executor_read_snapshot: Mutex::new(None),
            recent_skill_invocations: Arc::new(RecentSkillInvocations::default()),
            shadow_task_context: Arc::new(ShadowTaskContext::default()),
        }
    }

    pub(crate) fn config(&self) -> SkillsExtensionConfig {
        self.config
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    pub(crate) fn set_config(&self, config: SkillsExtensionConfig) {
        *self
            .config
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = config;
    }

    pub(crate) fn prepare_step_catalog(
        &self,
        executor: SkillCatalog,
        discovery_v2: bool,
    ) -> SkillStepCatalog {
        let mut step = SkillStepCatalog::new(executor, discovery_v2, Default::default());
        step.aliases = self.aliases.capture(
            step.merged
                .entries
                .iter()
                .flat_map(super::catalog::SkillCatalogEntry::source_locations)
                .filter(|source| source.enabled && source.prompt_visible)
                .filter_map(|source| source.alias_root.map(|root| (source.authority.kind, root))),
        );
        step
    }

    /// Returns the executor catalog, reusing matching discovery caches.
    #[tracing::instrument(
        name = "skills.executor.refresh_executor_catalog",
        level = "info",
        skip_all,
        fields(root_count = query.executor_roots.len())
    )]
    pub(crate) async fn refresh_executor_catalog(
        &self,
        providers: &SkillProviders,
        query: SkillListQuery,
    ) -> SkillCatalog {
        if let Some(snapshots) = &query.executor_capability_discovery_v2 {
            let snapshots = snapshots.clone();
            let (cached, already_observed) = {
                let state = self
                    .catalog_state
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                let cached = state.executor_v2_cache.as_ref();
                let already_observed =
                    cached.is_some_and(|cached| cached.same_discovery(&snapshots));
                (
                    cached
                        .filter(|_| {
                            already_observed
                                && snapshots.iter().all(|snapshot| snapshot.result.is_ok())
                        })
                        .map(|cached| cached.catalog.clone()),
                    already_observed,
                )
            };
            match cached {
                Some(catalog) => catalog,
                None => {
                    let catalog = providers.list_executor_for_turn(query).await;
                    // Failed snapshots are rebuilt for callers, but each is still one observation.
                    if !already_observed {
                        record_catalog(self.extension_metrics.as_deref(), "executor", &catalog);
                    }
                    self.catalog_state
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .executor_v2_cache = Some(CachedExecutorV2Catalog {
                        snapshots,
                        catalog: catalog.clone(),
                    });
                    catalog
                }
            }
        } else if query.executor_capability_discovery.is_some() {
            self.executor_discovery_catalog_snapshot(providers, query)
                .await
        } else {
            if let Some(cached) = self
                .catalog_state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .executor_cache
                .as_ref()
                .filter(|cached| cached.key.matches(&query))
            {
                return cached.catalog.clone();
            }

            let key = ExecutorCatalogCacheKey {
                turn_id: query.turn_id.clone(),
                resolved_roots: query.resolved_executor_roots.clone(),
                include_bundled_skills: query.include_bundled_skills,
            };
            let catalog = if query.executor_roots.is_empty() {
                SkillCatalog::default()
            } else {
                providers.list_executor_for_turn(query).await
            };
            record_catalog(self.extension_metrics.as_deref(), "executor", &catalog);
            self.catalog_state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .executor_cache = Some(CachedExecutorCatalog {
                key,
                catalog: catalog.clone(),
            });
            catalog
        }
    }

    /// Reuses matching successful discovery; otherwise returns the result of this request.
    async fn executor_discovery_catalog_snapshot(
        &self,
        providers: &SkillProviders,
        query: SkillListQuery,
    ) -> SkillCatalog {
        let discovery = query.executor_capability_discovery.clone();
        let discovery_failed = discovery.as_ref().is_some_and(|discovery| {
            discovery.roots().iter().any(|root| {
                root.result
                    .as_ref()
                    .ok()
                    .is_none_or(|discovered| discovered.error.is_some())
            })
        });
        let already_observed = {
            let state = self
                .catalog_state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let cached = state.executor_discovery_cache.as_ref().filter(|cached| {
                cached.matches(
                    &query.executor_roots,
                    discovery.as_ref(),
                    query.include_bundled_skills,
                )
            });
            if !discovery_failed && let Some(cached) = cached {
                return cached.catalog.clone();
            }
            cached.is_some()
        };

        let roots = query.executor_roots.clone();
        let include_bundled_skills = query.include_bundled_skills;
        let discovered = providers.list_executor_for_turn(query).await;
        if !already_observed {
            record_catalog(self.extension_metrics.as_deref(), "executor", &discovered);
        }
        let mut state = self
            .catalog_state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        // Failed discovery is returned to this caller but is never reused by another request.
        state.executor_discovery_cache =
            discovery.map(|discovery| CachedExecutorDiscoveryCatalog {
                roots,
                discovery,
                include_bundled_skills,
                catalog: discovered.clone(),
            });
        discovered
    }

    pub(crate) fn replace_shadow_selection_turn(
        &self,
        turn_id: String,
        state: Option<ShadowSelectionTurnState>,
    ) {
        *self
            .shadow_selection_turn
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) =
            state.map(|state| ShadowSelectionTurn {
                turn_id,
                state: Arc::new(state),
            });
    }

    pub(crate) fn shadow_selection_turn(
        &self,
        turn_id: &str,
    ) -> Option<Arc<ShadowSelectionTurnState>> {
        self.shadow_selection_turn
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_ref()
            .filter(|turn| turn.turn_id == turn_id)
            .map(|turn| Arc::clone(&turn.state))
    }
}

/// One bounded executor resource, retained for continuations until replacement or thread drop.
/// Interleaved resources may evict it; misses reread and validate the content-bound cursor.
pub(crate) struct ExecutorReadSnapshot {
    pub(crate) authority: SkillAuthority,
    pub(crate) package: SkillPackageId,
    // The key binds cached contents to their source filesystem and callback permissions.
    pub(crate) access: EnvironmentAccessKey,
    pub(crate) result: Arc<SkillReadResult>,
}

struct ShadowSelectionTurn {
    turn_id: String,
    state: Arc<ShadowSelectionTurnState>,
}

#[derive(Default)]
pub(crate) struct EmittedCatalogBudgetWarnings(Mutex<HashSet<String>>);

impl EmittedCatalogBudgetWarnings {
    pub(crate) fn insert(&self, warning: &str) -> bool {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(warning.to_string())
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct HostSkillsCatalogInWorldState;
