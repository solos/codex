#[derive(Debug, Clone)]
pub struct PluginsConfigInput {
    pub config_layer_stack: ConfigLayerStack,
    pub plugins: PluginsConfigToml,
    pub model_provider_id: String,
    pub plugins_enabled: bool,
}

impl PluginsConfigInput {
    // Require validated policy at construction; a default followed by a setter could fail open.
    pub fn new(
        config_layer_stack: ConfigLayerStack,
        plugins: PluginsConfigToml,
        model_provider_id: String,
        plugins_enabled: bool,
    ) -> Self {
        Self {
            config_layer_stack,
            plugins,
            model_provider_id,
            plugins_enabled,
        }
    }
}

#[path = "manager_errors.rs"]
mod errors;
pub use errors::PluginInstallError;
pub use errors::PluginUninstallError;
use errors::marketplace_error_sub_error_type;
use errors::marketplace_error_type;
use errors::plugin_install_error_type;

#[path = "marketplace_context.rs"]
mod marketplace_context;
pub use marketplace_context::PluginMarketplaceContext;
pub use marketplace_context::PluginMarketplaceScope;

use super::LoadedPlugin;
use super::PluginLoadOutcome;
use crate::PluginGitMode;
use crate::app_mcp_routing::apply_app_mcp_routing_policy;
use crate::app_mcp_routing::plugin_supports_apps;
use crate::installed_marketplaces::installed_marketplace_roots_from_layer_stack;
use crate::is_openai_curated_marketplace_name;
use crate::loaded_cache_metrics;
use crate::loaded_cache_metrics::RequestOutcome;
use crate::loader::PluginHookLoadOutcome;
use crate::loader::TargetCuratedMarketplace;
use crate::loader::curated_plugin_cache_version;
use crate::loader::load_plugin_apps_from_manifest;
use crate::loader::load_plugin_hooks;
use crate::loader::load_plugin_hooks_from_layer_stack;
use crate::loader::load_plugin_mcp_servers_from_manifest_with_format;
use crate::loader::load_plugin_skill_inventory;
use crate::loader::load_plugins_from_layer_stack;
use crate::loader::log_plugin_load_errors;
use crate::loader::materialize_marketplace_plugin_source;
use crate::loader::plugin_capability_summary_from_root;
use crate::loader::plugin_is_eligible_for_target_marketplace;
use crate::loader::refresh_non_curated_plugin_cache_detailed;
use crate::loader::refresh_non_curated_plugin_cache_force_reinstall_detailed;
use crate::manifest::PluginManifestFormat;
use crate::manifest::PluginManifestInterface;
use crate::marketplace::MarketplaceError;
use crate::marketplace::MarketplaceInterface;
use crate::marketplace::MarketplaceListError;
use crate::marketplace::MarketplaceListOutcome;
use crate::marketplace::MarketplacePluginAuthPolicy;
use crate::marketplace::MarketplacePluginManifestFallback;
use crate::marketplace::MarketplacePluginPolicy;
use crate::marketplace::MarketplacePluginSource;
use crate::marketplace::ResolvedMarketplacePlugin;
use crate::marketplace::find_installable_marketplace_plugin;
use crate::marketplace::find_marketplace_plugin;
use crate::marketplace::home_dir;
use crate::marketplace::list_marketplaces_with_cache;
use crate::marketplace::plugin_interface_with_marketplace_category;
use crate::marketplace_policy::MarketplacePolicy;
use crate::marketplace_policy::configured_plugins_from_stack;
use crate::marketplace_upgrade::ConfigLayerReload;
use crate::marketplace_upgrade::ConfiguredMarketplaceUpgradeError;
use crate::marketplace_upgrade::ConfiguredMarketplaceUpgradeOutcome;
use crate::marketplace_upgrade::upgrade_configured_git_marketplaces_with_mode;
use crate::skill_snapshots::new_plugin_skill_snapshots;
use crate::startup_sync::curated_plugins_api_marketplace_path;
use crate::startup_sync::curated_plugins_repo_path;
use crate::startup_sync::read_curated_plugins_sha;
use crate::store::PluginInstallResult as StorePluginInstallResult;
use crate::store::PluginStore;
use crate::store::PluginStoreError;
use crate::tool_suggest_metadata::ToolSuggestMetadataCache;
use codex_analytics::AnalyticsEventsClient;
use codex_analytics::PluginInstallSource;
use codex_config::ConfigLayerStack;
use codex_config::SkillConfigRules;
use codex_config::clear_user_plugin;
use codex_config::set_user_plugin_enabled;
use codex_config::skill_config_rules_from_stack;
use codex_config::types::PluginConfig;
use codex_config::types::PluginsConfigToml;
use codex_config::types::ToolSuggestDisabledTool;
use codex_config::types::ToolSuggestDiscoverableType;
use codex_connectors::ConnectorSnapshot;
use codex_connectors::PluginConnectorSource;
use codex_hooks::plugin_hook_declarations;
use codex_login::AuthManager;
use codex_login::CodexAuth;
use codex_plugin::AppConnectorId;
use codex_plugin::PluginCapabilitySummary;
use codex_plugin::PluginId;
use codex_plugin::PluginIdError;
use codex_plugin::PluginTelemetryMetadata;
use codex_plugin::app_connector_ids_from_declarations;
use codex_plugin::prompt_safe_plugin_description;
use codex_protocol::auth::AuthMode;
use codex_protocol::protocol::HookEventName;
use codex_protocol::protocol::Product;
use codex_skills::SkillMetadata;
use codex_skills::SkillRootLoader;
use codex_skills::SkillRootSnapshots;
use codex_tools::DiscoverablePluginInfo;
use codex_tools::DiscoverableTool;
use codex_tools::filter_request_plugin_install_discoverable_tools_for_client;
use codex_utils_absolute_path::AbsolutePathBuf;
use codex_utils_path_uri::PathUri;
use codex_utils_plugins::PluginIdentity;
use codex_utils_plugins::PluginSkillRoot;
use std::collections::HashMap;
use std::collections::HashSet;
use std::collections::VecDeque;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::RwLock;
use std::time::Duration;
use std::time::Instant;
use tokio::sync::OnceCell;
use tokio::sync::OwnedSemaphorePermit;
use tokio::sync::Semaphore;
use tokio::sync::mpsc;
use tokio::sync::watch;
use tracing::instrument;
use tracing::warn;

const LOADED_PLUGINS_CACHE_CAPACITY: usize = 8;

type EffectivePluginsChangedCallback = Arc<dyn Fn(EffectivePluginsChange) + Send + Sync + 'static>;

/// Effective-plugin changes that downstream composition layers may act on.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct EffectivePluginsChange {}

/// Inputs used to select endpoint-backed plugin install candidates.
pub struct RecommendedPluginCandidatesInput<'a> {
    pub plugins_config: &'a PluginsConfigInput,
    pub loaded_plugins: &'a PluginLoadOutcome,
    pub auth: Option<&'a CodexAuth>,
    pub disabled_tools: &'a [ToolSuggestDisabledTool],
    pub app_server_client_name: Option<&'a str>,
}

#[derive(Clone, PartialEq, Eq)]
struct NonCuratedCacheRefreshRequest {
    roots: Vec<AbsolutePathBuf>,
    configured_plugin_keys: Vec<String>,
    configured_plugin_sources: Vec<NonCuratedPluginSource>,
    mode: NonCuratedCacheRefreshMode,
    git_mode: PluginGitMode,
}

#[derive(Clone, PartialEq, Eq)]
struct NonCuratedPluginSource {
    marketplace_path: AbsolutePathBuf,
    plugin_key: String,
    source: MarketplacePluginSource,
    local_version: Option<String>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum NonCuratedCacheRefreshMode {
    IfVersionChanged,
    ForceReinstall,
}

#[derive(Default)]
struct NonCuratedCacheRefreshState {
    requested: Option<NonCuratedCacheRefreshRequest>,
    last_refreshed: Option<NonCuratedCacheRefreshRequest>,
    in_flight: bool,
}

#[derive(Clone, Copy, Default)]
struct NonCuratedCacheRefreshCompletion {
    sequence: u64,
    changed_sequence: u64,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PluginListBackgroundTaskOptions {
    pub local_marketplaces: Vec<ConfiguredMarketplace>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginInstallRequest {
    pub plugin_name: String,
    pub marketplace_path: AbsolutePathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginReadRequest {
    pub plugin_name: String,
    pub marketplace_path: AbsolutePathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginInstallOutcome {
    pub plugin_id: PluginId,
    pub plugin_version: String,
    pub installed_path: AbsolutePathBuf,
    pub auth_policy: MarketplacePluginAuthPolicy,
}

#[derive(Debug, Clone, PartialEq)]
pub struct PluginReadOutcome {
    pub marketplace_name: String,
    pub marketplace_path: Option<AbsolutePathBuf>,
    pub plugin: PluginDetail,
}

#[derive(Debug, Clone, PartialEq)]
pub struct PluginDetail {
    pub id: String,
    pub name: String,
    pub local_version: Option<String>,
    pub description: Option<String>,
    pub source: MarketplacePluginSource,
    pub policy: MarketplacePluginPolicy,
    pub interface: Option<PluginManifestInterface>,
    pub keywords: Vec<String>,
    pub installed: bool,
    pub enabled: bool,
    pub skills: Vec<SkillMetadata>,
    pub disabled_skill_paths: HashSet<PathUri>,
    /// Packaged onboarding path; callers apply visibility and enablement.
    pub onboarding_skill: Option<AbsolutePathBuf>,
    pub hooks: Vec<PluginHookSummary>,
    pub apps: Vec<AppConnectorId>,
    pub app_category_by_id: HashMap<String, String>,
    pub mcp_server_names: Vec<String>,
    pub details_unavailable_reason: Option<PluginDetailsUnavailableReason>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct PluginHookSummary {
    pub key: String,
    pub event_name: HookEventName,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PluginDetailsUnavailableReason {
    InstallRequiredForSource,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfiguredMarketplace {
    pub name: String,
    pub path: AbsolutePathBuf,
    pub interface: Option<MarketplaceInterface>,
    pub plugins: Vec<ConfiguredMarketplacePlugin>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfiguredMarketplacePlugin {
    pub id: String,
    pub name: String,
    pub local_version: Option<String>,
    pub installed_version: Option<String>,
    pub source: MarketplacePluginSource,
    pub policy: MarketplacePluginPolicy,
    pub interface: Option<PluginManifestInterface>,
    pub keywords: Vec<String>,
    pub manifest_fallback: Option<MarketplacePluginManifestFallback>,
    pub installed: bool,
    pub enabled: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ConfiguredMarketplaceListOutcome {
    pub marketplaces: Vec<ConfiguredMarketplace>,
    pub errors: Vec<MarketplaceListError>,
}

#[derive(Default)]
struct ConfiguredPluginStates {
    installed: HashSet<String>,
    enabled: HashSet<String>,
}

impl From<PluginDetail> for PluginCapabilitySummary {
    fn from(value: PluginDetail) -> Self {
        let has_skills = value.skills.iter().any(|skill| {
            !value
                .disabled_skill_paths
                .contains(&skill.path_to_skills_md)
        });
        Self {
            config_name: value.id,
            display_name: value.name,
            plugin_namespace: None,
            description: prompt_safe_plugin_description(value.description.as_deref()),
            has_skills,
            mcp_server_names: value.mcp_server_names,
            app_connector_ids: value.apps,
        }
    }
}

pub struct PluginsManager {
    codex_home: PathBuf,
    store: PluginStore,
    non_curated_cache_refresh_lock: Semaphore,
    non_curated_cache_refresh_state: RwLock<NonCuratedCacheRefreshState>,
    non_curated_cache_refresh_completion: watch::Sender<NonCuratedCacheRefreshCompletion>,
    // Loaded capabilities vary by effective configuration and account.
    loaded_plugins_cache: Mutex<LoadedPluginsCache>,
    loaded_plugins_load_semaphore: Semaphore,
    skill_root_loader: Arc<dyn SkillRootLoader<PluginSkillRoot>>,
    tool_suggest_metadata_cache: ToolSuggestMetadataCache,
    restriction_product: Option<Product>,
    auth_manager: Arc<AuthManager>,
    analytics_events_client: RwLock<Option<AnalyticsEventsClient>>,
    plugin_install_source: PluginInstallSource,
}

#[derive(Clone)]
struct LoadedPluginsCacheEntry {
    key: PluginLoadCacheKey,
    plugins: Vec<LoadedPlugin>,
    plugin_skill_snapshots: SkillRootSnapshots<PluginSkillRoot>,
}

#[derive(Default)]
struct LoadedPluginsCache {
    generation: u64,
    // Most recently used first.
    entries: VecDeque<Arc<LoadedPluginsCacheEntry>>,
}

impl LoadedPluginsCache {
    fn get(
        cache: &Mutex<Self>,
        key: &PluginLoadCacheKey,
        store: &PluginStore,
    ) -> Option<Arc<LoadedPluginsCacheEntry>> {
        let (generation, entry) = {
            let cache = cache
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let entry = cache.entries.iter().find(|entry| &entry.key == key)?;
            (cache.generation, Arc::clone(entry))
        };
        // Another process can replace installed versions without invalidating this manager.
        // Keep the parsed skills paired with the installation whose paths they advertise.
        // Validate outside the cache lock so filesystem work does not block other cache keys.
        let stale = entry.plugins.iter().any(|plugin| {
            let Ok(plugin_id) = PluginId::parse(&plugin.config_name) else {
                return false;
            };
            let installed_root = store
                .active_plugin_root(&plugin_id)
                .unwrap_or_else(|| store.plugin_base_root(&plugin_id));
            installed_root != plugin.root
        });
        let mut cache = cache
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if cache.generation != generation {
            return None;
        }
        // A load can replace this key without changing the cache generation.
        let index = cache
            .entries
            .iter()
            .position(|current| Arc::ptr_eq(current, &entry))?;
        let current = cache.entries.remove(index)?;
        if stale {
            return None;
        }
        cache.entries.push_front(current);
        Some(entry)
    }
}

#[derive(Clone, PartialEq, Eq)]
struct PluginLoadCacheKey {
    configured_plugins: HashMap<String, PluginConfig>,
    plugin_policy: PluginsConfigToml,
    skill_config_rules: SkillConfigRules,
}

impl PluginLoadCacheKey {
    fn from_config(config: &PluginsConfigInput, codex_home: &Path) -> Self {
        Self {
            configured_plugins: configured_plugins_from_stack(
                &config.config_layer_stack,
                codex_home,
            ),
            plugin_policy: config.plugins.clone(),
            skill_config_rules: skill_config_rules_from_stack(&config.config_layer_stack),
        }
    }
}

fn target_curated_marketplace(auth_mode: Option<AuthMode>) -> TargetCuratedMarketplace {
    if auth_mode.is_some_and(AuthMode::uses_codex_backend) {
        TargetCuratedMarketplace::OpenAi
    } else {
        TargetCuratedMarketplace::OpenAiApi
    }
}

impl PluginsManager {
    pub fn new(
        codex_home: PathBuf,
        auth_manager: Arc<AuthManager>,
        skill_root_loader: Arc<dyn SkillRootLoader<PluginSkillRoot>>,
    ) -> Self {
        Self::new_with_options(
            codex_home,
            Some(Product::Codex),
            auth_manager,
            skill_root_loader,
        )
    }

    pub fn new_with_options(
        codex_home: PathBuf,
        restriction_product: Option<Product>,
        auth_manager: Arc<AuthManager>,
        skill_root_loader: Arc<dyn SkillRootLoader<PluginSkillRoot>>,
    ) -> Self {
        // Product restrictions are enforced at marketplace admission time for a given CODEX_HOME:
        // listing, install, and curated refresh all consult this restriction context before new
        // plugins enter local config or cache. After admission, runtime plugin loading trusts the
        // contents of that CODEX_HOME and does not re-filter configured plugins by product, so
        // already-admitted plugins may continue exposing MCP servers/tools from shared local state.
        //
        // This assumes a single CODEX_HOME is only used by one product.
        let store = PluginStore::new(codex_home.clone());
        let tool_suggest_metadata_cache =
            ToolSuggestMetadataCache::new(Arc::clone(&store.manifest_cache));
        Self {
            codex_home,
            store,
            non_curated_cache_refresh_lock: Semaphore::new(/*permits*/ 1),
            non_curated_cache_refresh_state: RwLock::new(NonCuratedCacheRefreshState::default()),
            non_curated_cache_refresh_completion: watch::channel(
                NonCuratedCacheRefreshCompletion::default(),
            )
            .0,
            loaded_plugins_cache: Mutex::new(LoadedPluginsCache::default()),
            loaded_plugins_load_semaphore: Semaphore::new(/*permits*/ 1),
            skill_root_loader,
            tool_suggest_metadata_cache,
            restriction_product,
            auth_manager,
            analytics_events_client: RwLock::new(None),
            plugin_install_source: PluginInstallSource::Manual,
        }
    }

    pub fn with_plugin_install_source(mut self, source: PluginInstallSource) -> Self {
        self.plugin_install_source = source;
        self
    }

    pub fn auth_mode(&self) -> Option<AuthMode> {
        self.auth_manager.get_api_auth_mode()
    }

    pub fn set_analytics_events_client(&self, analytics_events_client: AnalyticsEventsClient) {
        let mut stored_client = match self.analytics_events_client.write() {
            Ok(client_guard) => client_guard,
            Err(err) => err.into_inner(),
        };
        *stored_client = Some(analytics_events_client);
    }

    fn restriction_product_matches(&self, products: Option<&[Product]>) -> bool {
        match products {
            None => true,
            Some([]) => false,
            Some(products) => self
                .restriction_product
                .is_some_and(|product| product.matches_product_restriction(products)),
        }
    }

    /// Returns skill snapshots parsed while loading the matching plugin cache entry.
    pub fn plugin_skill_snapshots_for_config(
        &self,
        config: &PluginsConfigInput,
    ) -> Option<SkillRootSnapshots<PluginSkillRoot>> {
        if !config.plugins_enabled {
            return None;
        }
        let key = PluginLoadCacheKey::from_config(config, self.codex_home.as_path());
        LoadedPluginsCache::get(&self.loaded_plugins_cache, &key, &self.store)
            .map(|cached| cached.plugin_skill_snapshots.clone())
    }

    #[instrument(
        name = "plugins_for_config",
        level = "info",
        skip_all,
        fields(
            otel.name = "plugins_for_config",
            plugins_enabled = config.plugins_enabled
        )
    )]
    pub async fn plugins_for_config(&self, config: &PluginsConfigInput) -> PluginLoadOutcome {
        if !config.plugins_enabled {
            return PluginLoadOutcome::default();
        }

        // Both the semaphore wait and skill loading can outlive the auth state captured at the
        // start of this load. Account switches require a restart, so discard those passes;
        // same-account revisions also cover ordinary token refreshes, where returning no plugins
        // is wrong, so retry those instead.
        let auth_change_receiver = self.auth_manager.auth_change_receiver();
        let mut cache_outcome = RequestOutcome::Hit;

        loop {
            let auth_revision = *auth_change_receiver.borrow();
            let auth = self.auth_manager.auth_cached();
            let auth_mode = auth.as_ref().map(CodexAuth::api_auth_mode);
            let cache_key = PluginLoadCacheKey::from_config(config, self.codex_home.as_path());
            if let Some(plugins) = self.cached_loaded_plugins(&cache_key) {
                let outcome = self.resolve_loaded_plugins_for_auth(plugins, auth_mode);
                if *auth_change_receiver.borrow() == auth_revision {
                    cache_outcome.record();
                    return outcome;
                }
                continue;
            }

            if matches!(cache_outcome, RequestOutcome::Hit) {
                cache_outcome = RequestOutcome::HitAfterWait;
            }
            let wait_started = Instant::now();
            let load_permit = self.loaded_plugins_load_semaphore.acquire().await;
            loaded_cache_metrics::record_duration(
                loaded_cache_metrics::WAIT_DURATION,
                wait_started.elapsed(),
            );
            let Ok(_load_permit) = load_permit else {
                warn!("plugin load semaphore closed");
                return PluginLoadOutcome::default();
            };
            if *auth_change_receiver.borrow() != auth_revision {
                continue;
            }
            if let Some(plugins) = self.cached_loaded_plugins(&cache_key) {
                let outcome = self.resolve_loaded_plugins_for_auth(plugins, auth_mode);
                if *auth_change_receiver.borrow() == auth_revision {
                    cache_outcome.record();
                    return outcome;
                }
                continue;
            }
            let cache_generation = self.loaded_plugins_cache_generation();
            let plugin_skill_snapshots = new_plugin_skill_snapshots();
            cache_outcome = RequestOutcome::Load;
            let load_started = Instant::now();
            let plugins = load_plugins_from_layer_stack(
                &config.config_layer_stack,
                &config.plugins,
                &self.store,
                Some(&plugin_skill_snapshots),
                self.restriction_product,
                self.skill_root_loader.as_ref(),
            )
            .await;
            loaded_cache_metrics::record_duration(
                loaded_cache_metrics::LOAD_DURATION,
                load_started.elapsed(),
            );
            if *auth_change_receiver.borrow() != auth_revision {
                continue;
            }
            log_plugin_load_errors(&plugins);
            self.cache_loaded_plugins_if_current(
                cache_generation,
                cache_key,
                plugins.clone(),
                plugin_skill_snapshots,
            );
            let outcome = self.resolve_loaded_plugins_for_auth(plugins, auth_mode);
            if *auth_change_receiver.borrow() == auth_revision {
                cache_outcome.record();
                return outcome;
            }
        }
    }

    fn resolve_loaded_plugins_for_auth(
        &self,
        mut plugins: Vec<LoadedPlugin>,
        auth_mode: Option<AuthMode>,
    ) -> PluginLoadOutcome {
        let target_curated_marketplace = target_curated_marketplace(auth_mode);
        plugins.retain(|plugin| {
            plugin_is_eligible_for_target_marketplace(
                &plugin.config_name,
                target_curated_marketplace,
            )
        });
        for plugin in &mut plugins {
            let plugin_active = plugin.is_active();
            apply_app_mcp_routing_policy(
                &mut plugin.apps,
                &mut plugin.mcp_servers,
                auth_mode,
                plugin_active,
            );
        }
        PluginLoadOutcome::from_plugins(plugins)
    }

    pub fn clear_cache(&self) {
        self.clear_loaded_plugins_cache();
    }

    fn clear_loaded_plugins_cache(&self) {
        self.tool_suggest_metadata_cache.clear();
        let mut cache = match self.loaded_plugins_cache.lock() {
            Ok(cache) => cache,
            Err(err) => err.into_inner(),
        };
        cache.generation = cache.generation.wrapping_add(1);
        cache.entries.clear();
        drop(cache);
        loaded_cache_metrics::record_event("clear");
    }

    fn clear_caches_after_marketplace_source_refresh(
        &self,
        installed_plugin_cache_refreshed: bool,
        on_effective_plugins_changed: Option<&EffectivePluginsChangedCallback>,
    ) {
        if installed_plugin_cache_refreshed {
            self.clear_cache();
            if let Some(on_effective_plugins_changed) = on_effective_plugins_changed {
                on_effective_plugins_changed(EffectivePluginsChange::default());
            }
        } else {
            self.tool_suggest_metadata_cache.clear();
        }
    }

    /// Resolve plugin hooks for a config layer stack without loading other plugin capabilities.
    pub async fn plugin_hooks_for_layer_stack(
        &self,
        config_layer_stack: &ConfigLayerStack,
        config: &PluginsConfigInput,
    ) -> PluginHookLoadOutcome {
        if !config.plugins_enabled {
            return PluginHookLoadOutcome::default();
        }
        let target_curated_marketplace = target_curated_marketplace(self.auth_mode());
        load_plugin_hooks_from_layer_stack(
            config_layer_stack,
            &config.plugins,
            &self.store,
            target_curated_marketplace,
        )
        .await
    }

    fn cached_loaded_plugins(&self, key: &PluginLoadCacheKey) -> Option<Vec<LoadedPlugin>> {
        LoadedPluginsCache::get(&self.loaded_plugins_cache, key, &self.store)
            .map(|cached| cached.plugins.clone())
    }

    fn loaded_plugins_cache_generation(&self) -> u64 {
        match self.loaded_plugins_cache.lock() {
            Ok(cache) => cache.generation,
            Err(err) => err.into_inner().generation,
        }
    }

    fn cache_loaded_plugins_if_current(
        &self,
        generation: u64,
        key: PluginLoadCacheKey,
        plugins: Vec<LoadedPlugin>,
        plugin_skill_snapshots: SkillRootSnapshots<PluginSkillRoot>,
    ) {
        let mut cache = match self.loaded_plugins_cache.lock() {
            Ok(cache) => cache,
            Err(err) => err.into_inner(),
        };
        if cache.generation != generation {
            return;
        }
        cache.entries.retain(|entry| entry.key != key);
        cache.entries.push_front(Arc::new(LoadedPluginsCacheEntry {
            key,
            plugins,
            plugin_skill_snapshots,
        }));
        let evicted = cache.entries.len() > LOADED_PLUGINS_CACHE_CAPACITY;
        cache.entries.truncate(LOADED_PLUGINS_CACHE_CAPACITY);
        drop(cache);
        if evicted {
            loaded_cache_metrics::record_event("capacity_eviction");
        }
    }

    pub fn connector_snapshot(
        &self,
        sources: impl IntoIterator<Item = PluginConnectorSource>,
        disabled_plugin_ids: &[String],
    ) -> ConnectorSnapshot {
        ConnectorSnapshot::from_plugin_sources(sources, disabled_plugin_ids, HashSet::new())
    }

    pub async fn telemetry_metadata_for_installed_plugin(
        &self,
        plugin_id: &PluginId,
    ) -> PluginTelemetryMetadata {
        let mut metadata = self.telemetry_metadata_for_plugin_id(plugin_id);
        metadata.capability_summary = match self.store.active_plugin_root(plugin_id) {
            Some(plugin_root) => {
                plugin_capability_summary_from_root(
                    plugin_id,
                    &plugin_root,
                    self.skill_root_loader.as_ref(),
                    &self.store.manifest_cache,
                )
                .await
            }
            None => None,
        };
        metadata
    }

    pub async fn install_plugin(
        &self,
        config_layer_stack: &ConfigLayerStack,
        request: PluginInstallRequest,
    ) -> Result<PluginInstallOutcome, PluginInstallError> {
        let resolved = self.resolve_installable_plugin(config_layer_stack, &request)?;
        let plugin_id = resolved.plugin_id.clone();
        match self.install_resolved_plugin(resolved).await {
            Ok(outcome) => Ok(outcome),
            Err(err) => {
                self.track_plugin_install_failed(
                    &plugin_id,
                    plugin_install_error_type(&err),
                    err.sub_error_type(),
                    err.to_string(),
                );
                Err(err)
            }
        }
    }

    fn resolve_installable_plugin(
        &self,
        config_layer_stack: &ConfigLayerStack,
        request: &PluginInstallRequest,
    ) -> Result<ResolvedMarketplacePlugin, PluginInstallError> {
        let resolved = match find_installable_marketplace_plugin(
            &request.marketplace_path,
            &request.plugin_name,
            self.restriction_product,
        ) {
            Ok(resolved) => resolved,
            Err(err) => {
                self.track_plugin_install_resolution_failed(&err);
                return Err(err.into());
            }
        };
        if let Err(message) =
            MarketplacePolicy::from_requirements(config_layer_stack.requirements())
                .validate_install(
                    config_layer_stack,
                    self.codex_home.as_path(),
                    &request.marketplace_path,
                    &resolved.plugin_id.marketplace_name,
                )
        {
            let err = MarketplaceError::InvalidMarketplaceFile {
                path: request.marketplace_path.to_path_buf(),
                message,
            };
            self.track_plugin_install_resolution_failed(&err);
            return Err(err.into());
        }
        Ok(resolved)
    }

    fn track_plugin_install_resolution_failed(&self, err: &MarketplaceError) {
        let sub_error_type = marketplace_error_sub_error_type(err);
        let plugin_id = match err {
            MarketplaceError::PluginNotFound {
                plugin_name,
                marketplace_name,
            }
            | MarketplaceError::PluginNotAvailable {
                plugin_name,
                marketplace_name,
            } => PluginId::new(plugin_name.clone(), marketplace_name.clone()).ok(),
            MarketplaceError::Io { .. }
            | MarketplaceError::MarketplaceNotFound { .. }
            | MarketplaceError::InvalidMarketplaceFile { .. }
            | MarketplaceError::PluginsDisabled
            | MarketplaceError::InvalidPlugin(_) => None,
        };
        if let Some(plugin_id) = plugin_id {
            self.track_plugin_install_failed(
                &plugin_id,
                marketplace_error_type(err),
                sub_error_type,
                err.to_string(),
            );
        } else {
            tracing::warn!(
                error_type = %marketplace_error_type(err),
                sub_error_type = sub_error_type.as_deref(),
                error = %err,
                "plugin install failed while resolving marketplace plugin"
            );
            self.emit_plugin_install_failed(
                PluginTelemetryMetadata {
                    plugin_id: None,
                    capability_summary: None,
                },
                marketplace_error_type(err),
                sub_error_type,
            );
        }
    }

    fn track_plugin_install_failed(
        &self,
        plugin_id: &PluginId,
        error_type: &'static str,
        sub_error_type: Option<String>,
        error_message: String,
    ) {
        tracing::warn!(
            plugin_id = %plugin_id.as_key(),
            error_type = %error_type,
            sub_error_type = sub_error_type.as_deref(),
            error = %error_message,
            "plugin install failed"
        );
        self.emit_plugin_install_failed(
            self.telemetry_metadata_for_plugin_id(plugin_id),
            error_type,
            sub_error_type,
        );
    }

    fn emit_plugin_install_failed(
        &self,
        plugin: PluginTelemetryMetadata,
        error_type: &'static str,
        sub_error_type: Option<String>,
    ) {
        let analytics_events_client = match self.analytics_events_client.read() {
            Ok(client) => client.clone(),
            Err(err) => err.into_inner().clone(),
        };
        if let Some(analytics_events_client) = analytics_events_client {
            analytics_events_client.track_plugin_install_failed(
                plugin,
                self.plugin_install_source,
                error_type.to_string(),
                sub_error_type,
            );
        }
    }

    async fn install_resolved_plugin(
        &self,
        resolved: ResolvedMarketplacePlugin,
    ) -> Result<PluginInstallOutcome, PluginInstallError> {
        let auth_policy = resolved.policy.authentication;
        let plugin_version =
            if is_openai_curated_marketplace_name(&resolved.plugin_id.marketplace_name) {
                let curated_plugin_version = read_curated_plugins_sha(self.codex_home.as_path())
                    .ok_or_else(|| {
                        PluginStoreError::Invalid(
                            "local curated marketplace sha is not available".to_string(),
                        )
                    })?;
                Some(curated_plugin_cache_version(&curated_plugin_version))
            } else {
                None
            };
        let store = self.store.clone();
        let codex_home = self.codex_home.clone();
        let manifest_fallback_contents = resolved
            .manifest_fallback
            .contents_if_has_metadata()
            .map(str::to_string);
        let result: StorePluginInstallResult = tokio::task::spawn_blocking(move || {
            let materialized =
                materialize_marketplace_plugin_source(codex_home.as_path(), &resolved.source)
                    .map_err(PluginStoreError::Invalid)?;
            let source_path = materialized.path;
            match (plugin_version, manifest_fallback_contents.as_deref()) {
                (Some(plugin_version), Some(manifest_contents)) => store
                    .install_with_version_and_fallback_manifest(
                        source_path,
                        resolved.plugin_id,
                        plugin_version,
                        manifest_contents,
                    ),
                (Some(plugin_version), None) => {
                    store.install_with_version(source_path, resolved.plugin_id, plugin_version)
                }
                (None, Some(manifest_contents)) => store.install_with_fallback_manifest(
                    source_path,
                    resolved.plugin_id,
                    manifest_contents,
                ),
                (None, None) => store.install(source_path, resolved.plugin_id),
            }
        })
        .await
        .map_err(PluginInstallError::join)??;

        set_user_plugin_enabled(
            &self.codex_home,
            result.plugin_id.as_key(),
            /*enabled*/ true,
        )
        .await
        .map_err(anyhow::Error::from)?;

        let analytics_events_client = match self.analytics_events_client.read() {
            Ok(client) => client.clone(),
            Err(err) => err.into_inner().clone(),
        };
        if let Some(analytics_events_client) = analytics_events_client {
            analytics_events_client.track_plugin_installed(
                self.telemetry_metadata_for_installed_plugin(&result.plugin_id)
                    .await,
            );
        }

        Ok(PluginInstallOutcome {
            plugin_id: result.plugin_id,
            plugin_version: result.plugin_version,
            installed_path: result.installed_path,
            auth_policy,
        })
    }

    pub async fn uninstall_plugin(&self, plugin_id: String) -> Result<(), PluginUninstallError> {
        let plugin_id = PluginId::parse(&plugin_id)?;
        self.uninstall_plugin_id(plugin_id).await
    }

    async fn uninstall_plugin_id(&self, plugin_id: PluginId) -> Result<(), PluginUninstallError> {
        let plugin_telemetry = if self.store.active_plugin_root(&plugin_id).is_some() {
            Some(
                self.telemetry_metadata_for_installed_plugin(&plugin_id)
                    .await,
            )
        } else {
            None
        };
        let store = self.store.clone();
        let plugin_id_for_store = plugin_id.clone();
        tokio::task::spawn_blocking(move || store.uninstall(&plugin_id_for_store))
            .await
            .map_err(PluginUninstallError::join)??;

        clear_user_plugin(&self.codex_home, plugin_id.as_key())
            .await
            .map_err(anyhow::Error::from)?;

        let analytics_events_client = match self.analytics_events_client.read() {
            Ok(client) => client.clone(),
            Err(err) => err.into_inner().clone(),
        };
        if let Some(plugin_telemetry) = plugin_telemetry
            && let Some(analytics_events_client) = analytics_events_client
        {
            analytics_events_client.track_plugin_uninstalled(plugin_telemetry);
        }

        Ok(())
    }

    pub fn list_marketplaces_for_context(
        &self,
        context: &PluginMarketplaceContext,
        include_openai_curated: bool,
    ) -> Result<ConfiguredMarketplaceListOutcome, MarketplaceError> {
        context.list_marketplaces(self, include_openai_curated)
    }

    pub fn list_marketplaces_for_config(
        &self,
        config: &PluginsConfigInput,
        additional_roots: &[AbsolutePathBuf],
        include_openai_curated: bool,
    ) -> Result<ConfiguredMarketplaceListOutcome, MarketplaceError> {
        if !config.plugins_enabled {
            return Ok(ConfiguredMarketplaceListOutcome::default());
        }

        let plugin_states = self.configured_plugin_states(config);
        self.list_marketplaces_for_config_with_states(
            config,
            additional_roots,
            include_openai_curated,
            &plugin_states,
        )
    }

    fn list_marketplaces_for_config_with_states(
        &self,
        config: &PluginsConfigInput,
        additional_roots: &[AbsolutePathBuf],
        include_openai_curated: bool,
        plugin_states: &ConfiguredPluginStates,
    ) -> Result<ConfiguredMarketplaceListOutcome, MarketplaceError> {
        let marketplace_roots =
            self.marketplace_roots(config, additional_roots, include_openai_curated);
        let marketplace_outcome = self.list_marketplaces_with_policy(config, &marketplace_roots)?;
        let mut seen_plugin_keys = HashSet::new();
        let marketplaces = marketplace_outcome
            .marketplaces
            .into_iter()
            .filter_map(|marketplace| {
                let marketplace_name = marketplace.name.clone();
                let plugins = marketplace
                    .plugins
                    .into_iter()
                    .filter_map(|plugin| {
                        let plugin_key = format!("{}@{marketplace_name}", plugin.name);
                        if !seen_plugin_keys.insert(plugin_key.clone()) {
                            return None;
                        }
                        if !self.restriction_product_matches(plugin.policy.products.as_deref()) {
                            return None;
                        }
                        let plugin_id =
                            PluginId::new(plugin.name.clone(), marketplace_name.clone()).ok();
                        let installed = plugin_states.installed.contains(&plugin_key);
                        let installed_version = installed.then_some(()).and_then(|_| {
                            plugin_id
                                .as_ref()
                                .and_then(|plugin_id| self.store.active_plugin_version(plugin_id))
                        });
                        let enabled = plugin_states.enabled.contains(&plugin_key);
                        let mut interface = plugin.interface;
                        let mut local_version = plugin.local_version;
                        let manifest_fallback = plugin.manifest_fallback.clone();
                        if installed
                            && plugin.source.is_install_materialized()
                            && let Some(plugin_id) = plugin_id.as_ref()
                            && let Some(plugin_root) = self.store.active_plugin_root(plugin_id)
                            && let Some(manifest) = self
                                .store
                                .manifest_cache
                                .load(plugin_root.as_path())
                                .map(|loaded| loaded.manifest)
                        {
                            local_version = manifest.version.clone();
                            let marketplace_category = interface
                                .as_ref()
                                .and_then(|interface| interface.category.clone());
                            interface = plugin_interface_with_marketplace_category(
                                manifest.interface,
                                marketplace_category,
                            );
                        }

                        Some(ConfiguredMarketplacePlugin {
                            // Enabled state is keyed by `<plugin>@<marketplace>`, so duplicate
                            // plugin entries from duplicate marketplace files intentionally
                            // resolve to the first discovered source.
                            id: plugin_key,
                            installed_version,
                            installed,
                            enabled,
                            name: plugin.name,
                            local_version,
                            source: plugin.source,
                            policy: plugin.policy,
                            keywords: plugin.keywords,
                            interface,
                            manifest_fallback,
                        })
                    })
                    .collect::<Vec<_>>();

                (!plugins.is_empty()).then_some(ConfiguredMarketplace {
                    name: marketplace.name,
                    path: marketplace.path,
                    interface: marketplace.interface,
                    plugins,
                })
            })
            .collect();

        Ok(ConfiguredMarketplaceListOutcome {
            marketplaces,
            errors: marketplace_outcome.errors,
        })
    }

    pub fn discover_marketplaces_for_config(
        &self,
        config: &PluginsConfigInput,
        additional_roots: &[AbsolutePathBuf],
    ) -> Result<MarketplaceListOutcome, MarketplaceError> {
        if !config.plugins_enabled {
            return Ok(MarketplaceListOutcome::default());
        }

        let marketplace_roots = self.marketplace_roots(
            config,
            additional_roots,
            /*include_openai_curated*/ true,
        );
        self.list_marketplaces_with_policy(config, &marketplace_roots)
    }

    pub(crate) async fn tool_suggest_metadata_for_marketplace_plugin(
        &self,
        marketplace_name: &str,
        plugin: &ConfiguredMarketplacePlugin,
        skill_config_rules: &SkillConfigRules,
    ) -> Result<PluginCapabilitySummary, MarketplaceError> {
        let fragment = self
            .tool_suggest_metadata_cache
            .metadata_for_plugin(
                marketplace_name,
                plugin,
                self.restriction_product,
                self.skill_root_loader.as_ref(),
            )
            .await?;
        Ok(fragment.project(skill_config_rules, self.auth_mode()))
    }

    pub async fn read_plugin_for_config(
        &self,
        config: &PluginsConfigInput,
        request: &PluginReadRequest,
    ) -> Result<PluginReadOutcome, MarketplaceError> {
        if !config.plugins_enabled {
            return Err(MarketplaceError::PluginsDisabled);
        }

        let plugin = find_marketplace_plugin(&request.marketplace_path, &request.plugin_name)?;
        MarketplacePolicy::from_requirements(config.config_layer_stack.requirements())
            .validate_install(
                &config.config_layer_stack,
                self.codex_home.as_path(),
                &request.marketplace_path,
                &plugin.plugin_id.marketplace_name,
            )
            .map_err(|message| MarketplaceError::InvalidMarketplaceFile {
                path: request.marketplace_path.to_path_buf(),
                message,
            })?;
        if !self.restriction_product_matches(plugin.policy.products.as_deref()) {
            return Err(MarketplaceError::PluginNotFound {
                plugin_name: plugin.plugin_id.plugin_name,
                marketplace_name: plugin.plugin_id.marketplace_name,
            });
        }

        let marketplace_name = plugin.plugin_id.marketplace_name.clone();
        let plugin_key = plugin.plugin_id.as_key();
        let manifest_fallback = plugin
            .manifest_fallback
            .contents_if_has_metadata()
            .map(|_| plugin.manifest_fallback.clone());
        let plugin_states = self.configured_plugin_states(config);
        let installed = plugin_states.installed.contains(&plugin_key);
        let installed_version = if installed {
            self.store.active_plugin_version(&plugin.plugin_id)
        } else {
            None
        };
        let plugin = self
            .read_plugin_detail_for_marketplace_plugin(
                config,
                &marketplace_name,
                ConfiguredMarketplacePlugin {
                    id: plugin_key.clone(),
                    name: plugin.plugin_id.plugin_name,
                    local_version: plugin
                        .manifest
                        .as_ref()
                        .and_then(|manifest| manifest.version.clone()),
                    installed_version,
                    source: plugin.source,
                    policy: plugin.policy,
                    interface: plugin.interface,
                    keywords: plugin
                        .manifest
                        .as_ref()
                        .map(|manifest| manifest.keywords.clone())
                        .unwrap_or_default(),
                    manifest_fallback,
                    installed,
                    enabled: plugin_states.enabled.contains(&plugin_key),
                },
            )
            .await?;

        Ok(PluginReadOutcome {
            marketplace_name,
            marketplace_path: Some(request.marketplace_path.clone()),
            plugin,
        })
    }

    #[instrument(level = "trace", skip_all)]
    pub async fn read_plugin_detail_for_marketplace_plugin(
        &self,
        config: &PluginsConfigInput,
        marketplace_name: &str,
        plugin: ConfiguredMarketplacePlugin,
    ) -> Result<PluginDetail, MarketplaceError> {
        if !self.restriction_product_matches(plugin.policy.products.as_deref()) {
            return Err(MarketplaceError::PluginNotFound {
                plugin_name: plugin.name,
                marketplace_name: marketplace_name.to_string(),
            });
        }

        let plugin_id =
            PluginId::new(plugin.name.clone(), marketplace_name.to_string()).map_err(|err| {
                match err {
                    PluginIdError::Invalid(message) => MarketplaceError::InvalidPlugin(message),
                }
            })?;
        let plugin_key = plugin_id.as_key();
        if plugin.source.is_install_materialized() && !plugin.installed {
            let description = plugin_install_required_description(&plugin.source);
            return Ok(PluginDetail {
                id: plugin_key,
                name: plugin.name,
                local_version: None,
                description: Some(description),
                source: plugin.source,
                policy: plugin.policy,
                interface: plugin.interface,
                keywords: plugin.keywords,
                installed: plugin.installed,
                enabled: plugin.enabled,
                skills: Vec::new(),
                disabled_skill_paths: HashSet::new(),
                onboarding_skill: None,
                hooks: Vec::new(),
                apps: Vec::new(),
                app_category_by_id: HashMap::new(),
                mcp_server_names: Vec::new(),
                details_unavailable_reason: Some(
                    PluginDetailsUnavailableReason::InstallRequiredForSource,
                ),
            });
        }

        let source_path = if plugin.source.is_install_materialized() && plugin.installed {
            self.store.active_plugin_root(&plugin_id).ok_or_else(|| {
                MarketplaceError::InvalidPlugin(format!(
                    "installed plugin cache entry is missing for {plugin_key}"
                ))
            })?
        } else {
            let codex_home = self.codex_home.clone();
            let source = plugin.source.clone();
            let materialized = tokio::task::spawn_blocking(move || {
                materialize_marketplace_plugin_source(codex_home.as_path(), &source)
            })
            .await
            .map_err(|err| {
                MarketplaceError::InvalidPlugin(format!(
                    "failed to materialize plugin source: {err}"
                ))
            })?
            .map_err(MarketplaceError::InvalidPlugin)?;
            materialized.path.clone()
        };
        if !source_path.as_path().is_dir() {
            return Err(MarketplaceError::InvalidPlugin(
                "path does not exist or is not a directory".to_string(),
            ));
        }
        let loaded_manifest =
            if codex_utils_plugins::find_plugin_manifest_path(source_path.as_path()).is_some() {
                self.store.manifest_cache.load(source_path.as_path())
            } else {
                plugin
                    .manifest_fallback
                    .as_ref()
                    .and_then(|fallback| fallback.parse_for_plugin_root(source_path.as_path()))
                    .map(|manifest| crate::manifest::LoadedPluginManifest {
                        manifest,
                        format: PluginManifestFormat::Legacy,
                    })
            }
            .ok_or_else(|| {
                MarketplaceError::InvalidPlugin("missing or invalid plugin.json".to_string())
            })?;
        let manifest_format = loaded_manifest.format;
        let manifest = loaded_manifest.manifest;
        let description = manifest.description.clone();
        let marketplace_category = plugin
            .interface
            .as_ref()
            .and_then(|interface| interface.category.clone());
        let interface = plugin_interface_with_marketplace_category(
            manifest.interface.clone(),
            marketplace_category,
        );
        let plugin_identity = PluginIdentity {
            plugin_id: plugin_id.as_key(),
        };
        let skill_config_rules = skill_config_rules_from_stack(&config.config_layer_stack);
        let resolved_skills = load_plugin_skill_inventory(
            &source_path,
            &plugin_identity,
            &manifest,
            manifest_format,
            self.restriction_product,
            /*plugin_skill_snapshots*/ None,
            self.skill_root_loader.as_ref(),
        )
        .await
        .resolve(&skill_config_rules);
        let onboarding_skill = manifest.paths.onboarding_skill.as_ref().and_then(|path| {
            let plugin_root = source_path.canonicalize().ok()?;
            let path = path.canonicalize().ok()?;
            if !path.as_path().starts_with(plugin_root.as_path()) {
                return None;
            }
            let path_uri = PathUri::from_abs_path(&path);
            resolved_skills
                .skills
                .iter()
                .any(|skill| skill.path_to_skills_md == path_uri)
                .then_some(path)
        });
        let plugin_data_root = self.store.plugin_data_root(&plugin_id);
        let (hook_sources, _hook_load_warnings) = if manifest_format == PluginManifestFormat::Legacy
        {
            load_plugin_hooks(&source_path, &plugin_id, &plugin_data_root, &manifest.paths)
        } else {
            (Vec::new(), Vec::new())
        };
        let hooks = plugin_hook_declarations(&hook_sources)
            .into_iter()
            .map(|hook| PluginHookSummary {
                key: hook.key,
                event_name: hook.event_name,
            })
            .collect();
        let auth_mode = self.auth_mode();
        let mut app_declarations = if plugin_supports_apps(manifest_format, &plugin_id) {
            load_plugin_apps_from_manifest(source_path.as_path(), &manifest.paths).await
        } else {
            Vec::new()
        };
        let mcp_data_root = (manifest_format == PluginManifestFormat::AgentPlugin)
            .then(|| self.store.mcp_data_root(&plugin_id, manifest_format));
        let mut mcp_servers = load_plugin_mcp_servers_from_manifest_with_format(
            source_path.as_path(),
            &manifest.paths,
            /*plugin_policy*/ None,
            mcp_data_root.as_deref(),
            manifest_format,
        )
        .await;
        apply_app_mcp_routing_policy(
            &mut app_declarations,
            &mut mcp_servers,
            auth_mode,
            /*plugin_active*/ true,
        );
        let apps = app_connector_ids_from_declarations(&app_declarations);
        let mut seen_app_connector_ids = HashSet::new();
        let mut app_category_by_id = HashMap::new();
        for app in &app_declarations {
            if seen_app_connector_ids.insert(app.connector_id.0.as_str())
                && let Some(category) = &app.category
            {
                app_category_by_id.insert(app.connector_id.0.clone(), category.clone());
            }
        }
        let mut mcp_server_names = mcp_servers.into_keys().collect::<Vec<_>>();
        mcp_server_names.sort_unstable();
        mcp_server_names.dedup();

        Ok(PluginDetail {
            id: plugin.id,
            name: plugin.name,
            local_version: manifest.version.clone(),
            description,
            source: plugin.source,
            policy: plugin.policy,
            interface,
            keywords: manifest.keywords,
            installed: plugin.installed,
            enabled: plugin.enabled,
            skills: resolved_skills.skills,
            disabled_skill_paths: resolved_skills.disabled_skill_paths,
            onboarding_skill,
            hooks,
            apps,
            app_category_by_id,
            mcp_server_names,
            details_unavailable_reason: None,
        })
    }

    /// Starts background tasks and sends marketplace upgrade warnings through the supplied channel.
    pub fn maybe_start_plugin_startup_tasks_for_config(
        self: &Arc<Self>,
        _config: &PluginsConfigInput,
        _prefetch_discovery: bool,
        _reload_config: ConfigLayerReload,
        _on_effective_plugins_changed: Option<EffectivePluginsChangedCallback>,
        _upgrade_warnings: mpsc::UnboundedSender<String>,
    ) {
        // Marketplace updates are local-only; no startup network sync is performed.
    }

    pub fn upgrade_configured_marketplaces_for_config(
        &self,
        _config: &PluginsConfigInput,
        _marketplace_name: Option<&str>,
        _reload_config: &ConfigLayerReload,
    ) -> Result<ConfiguredMarketplaceUpgradeOutcome, String> {
        Err("remote marketplace upgrades are not supported".to_string())
    }

    /// Carries the initiating Git trust policy through marketplace and plugin refreshes.
    fn upgrade_configured_marketplaces_for_config_with_mode(
        &self,
        config: &PluginsConfigInput,
        marketplace_name: Option<&str>,
        mode: PluginGitMode,
        reload_config: &ConfigLayerReload,
    ) -> Result<ConfiguredMarketplaceUpgradeOutcome, String> {
        let mut outcome = upgrade_configured_git_marketplaces_with_mode(
            self.codex_home.as_path(),
            &config.config_layer_stack,
            marketplace_name,
            mode,
            reload_config,
        );
        if let Some(marketplace_name) = marketplace_name
            && outcome.selected_marketplaces.is_empty()
        {
            return Err(format!(
                "marketplace `{marketplace_name}` is not configured as a Git marketplace"
            ));
        }
        if !outcome.upgraded_roots.is_empty() {
            let mut configured_plugin_keys = configured_plugins_from_stack(
                &config.config_layer_stack,
                self.codex_home.as_path(),
            )
            .into_keys()
            .collect::<Vec<_>>();
            configured_plugin_keys.sort_unstable();
            match refresh_non_curated_plugin_cache_force_reinstall_detailed(
                self.codex_home.as_path(),
                &outcome.upgraded_roots,
                &configured_plugin_keys,
                mode,
            ) {
                Ok(refresh_outcome) => {
                    self.clear_caches_after_marketplace_source_refresh(
                        refresh_outcome.cache_refreshed,
                        /*on_effective_plugins_changed*/ None,
                    );
                    outcome
                        .errors
                        .extend(refresh_outcome.errors.into_iter().map(|error| {
                            ConfiguredMarketplaceUpgradeError {
                                marketplace_name: error.marketplace_name,
                                message: error.message,
                            }
                        }));
                }
                Err(err) => {
                    self.clear_cache();
                    outcome.errors.push(ConfiguredMarketplaceUpgradeError {
                        marketplace_name: marketplace_name
                            .unwrap_or("all configured marketplaces")
                            .to_string(),
                        message: format!(
                            "failed to refresh installed plugin cache after marketplace upgrade: {err}"
                        ),
                    });
                }
            }
        }
        Ok(outcome)
    }

    pub fn maybe_start_non_curated_plugin_cache_refresh(
        self: &Arc<Self>,
        context: &PluginMarketplaceContext,
        marketplaces: &[ConfiguredMarketplace],
    ) {
        if let Some(request) = context.non_curated_cache_refresh_request(
            self,
            marketplaces,
            NonCuratedCacheRefreshMode::IfVersionChanged,
            PluginGitMode::Automatic,
        ) {
            self.schedule_non_curated_plugin_cache_refresh(request);
        }
    }

    /// Runs an explicitly requested refresh using the caller's normal Git configuration.
    pub async fn refresh_non_curated_plugin_cache_for_context(
        self: &Arc<Self>,
        context: &PluginMarketplaceContext,
    ) -> bool {
        let Ok(_refresh_permit) = self.non_curated_cache_refresh_lock.acquire().await else {
            return false;
        };
        let mut completion = self.non_curated_cache_refresh_completion.subscribe();
        let changed_sequence = completion.borrow_and_update().changed_sequence;
        let marketplaces =
            match self.list_marketplaces_for_context(context, /*include_openai_curated*/ false) {
                Ok(outcome) => outcome.marketplaces,
                Err(err) => {
                    warn!("failed to prepare non-curated plugin cache refresh: {err}");
                    Vec::new()
                }
            };
        if let Some(request) = context.non_curated_cache_refresh_request(
            self,
            &marketplaces,
            NonCuratedCacheRefreshMode::IfVersionChanged,
            PluginGitMode::Manual,
        ) {
            self.schedule_non_curated_plugin_cache_refresh(request);
        }

        loop {
            let in_flight = match self.non_curated_cache_refresh_state.read() {
                Ok(state) => state.in_flight,
                Err(err) => err.into_inner().in_flight,
            };
            if !in_flight {
                return completion.borrow().changed_sequence != changed_sequence;
            }
            if completion.changed().await.is_err() {
                return false;
            }
        }
    }

    fn schedule_non_curated_plugin_cache_refresh(
        self: &Arc<Self>,
        mut request: NonCuratedCacheRefreshRequest,
    ) {
        let should_spawn = {
            let mut state = match self.non_curated_cache_refresh_state.write() {
                Ok(state) => state,
                Err(err) => err.into_inner(),
            };
            if request.mode == NonCuratedCacheRefreshMode::IfVersionChanged
                && state.last_refreshed.as_ref().is_some_and(|last_refreshed| {
                    request.configured_plugin_sources.iter().any(|source| {
                        last_refreshed
                            .configured_plugin_sources
                            .iter()
                            .any(|previous_source| {
                                previous_source.plugin_key == source.plugin_key
                                    && previous_source.local_version == source.local_version
                                    && (previous_source.marketplace_path != source.marketplace_path
                                        || previous_source.source != source.source)
                            })
                    })
                })
            {
                request.mode = NonCuratedCacheRefreshMode::ForceReinstall;
            }
            if request.mode == NonCuratedCacheRefreshMode::IfVersionChanged
                && state.requested.as_ref().is_some_and(|requested| {
                    requested.mode == NonCuratedCacheRefreshMode::ForceReinstall
                        && requested.roots == request.roots
                })
            {
                request.mode = NonCuratedCacheRefreshMode::ForceReinstall;
            }
            // Reconcile each canonical plugin generation once before publishing its resource.
            if state.requested.as_ref().is_some_and(|requested| {
                requested == &request
                    || (requested.git_mode == PluginGitMode::Manual
                        && request.git_mode == PluginGitMode::Automatic
                        && requested.roots == request.roots
                        && requested.configured_plugin_keys == request.configured_plugin_keys
                        && requested.configured_plugin_sources == request.configured_plugin_sources
                        && requested.mode == request.mode)
            }) || (request.mode == NonCuratedCacheRefreshMode::IfVersionChanged
                && !state.in_flight
                && state.last_refreshed.as_ref().is_some_and(|last_refreshed| {
                    last_refreshed.roots == request.roots
                        && last_refreshed.configured_plugin_keys == request.configured_plugin_keys
                        && last_refreshed.configured_plugin_sources
                            == request.configured_plugin_sources
                        && (last_refreshed.git_mode == request.git_mode
                            || last_refreshed.git_mode == PluginGitMode::Manual)
                }))
            {
                return;
            }
            state.requested = Some(request);
            if state.in_flight {
                false
            } else {
                state.in_flight = true;
                true
            }
        };
        if !should_spawn {
            return;
        }

        let manager = Arc::clone(self);
        if let Err(err) = std::thread::Builder::new()
            .name("plugins-non-curated-cache-refresh".to_string())
            .spawn(move || manager.run_non_curated_plugin_cache_refresh_loop())
        {
            let mut state = match self.non_curated_cache_refresh_state.write() {
                Ok(state) => state,
                Err(err) => err.into_inner(),
            };
            state.in_flight = false;
            state.requested = None;
            self.non_curated_cache_refresh_completion
                .send_modify(|completion| {
                    completion.sequence = completion.sequence.wrapping_add(1);
                });
            warn!("failed to start non-curated plugin cache refresh task: {err}");
        }
    }

    fn run_non_curated_plugin_cache_refresh_loop(self: Arc<Self>) {
        loop {
            let request = {
                let state = match self.non_curated_cache_refresh_state.read() {
                    Ok(state) => state,
                    Err(err) => err.into_inner(),
                };
                state.requested.clone()
            };

            let Some(request) = request else {
                let mut state = match self.non_curated_cache_refresh_state.write() {
                    Ok(state) => state,
                    Err(err) => err.into_inner(),
                };
                state.in_flight = false;
                self.non_curated_cache_refresh_completion
                    .send_modify(|completion| {
                        completion.sequence = completion.sequence.wrapping_add(1);
                    });
                return;
            };

            let refresh_result = match request.mode {
                NonCuratedCacheRefreshMode::IfVersionChanged => {
                    refresh_non_curated_plugin_cache_detailed(
                        self.codex_home.as_path(),
                        &request.roots,
                        &request.configured_plugin_keys,
                        request.git_mode,
                    )
                }
                NonCuratedCacheRefreshMode::ForceReinstall => {
                    refresh_non_curated_plugin_cache_force_reinstall_detailed(
                        self.codex_home.as_path(),
                        &request.roots,
                        &request.configured_plugin_keys,
                        request.git_mode,
                    )
                }
            };
            let (refreshed, cache_changed) = match refresh_result {
                Ok(refresh_outcome) => {
                    if refresh_outcome.cache_refreshed {
                        self.clear_cache();
                    }
                    for error in &refresh_outcome.errors {
                        warn!(
                            marketplace = error.marketplace_name,
                            error = %error.message,
                            "failed to refresh configured plugin cache"
                        );
                    }
                    (
                        refresh_outcome.errors.is_empty(),
                        refresh_outcome.cache_refreshed,
                    )
                }
                Err(err) => {
                    self.clear_cache();
                    warn!("failed to refresh non-curated plugin cache: {err}");
                    (false, false)
                }
            };

            let mut state = match self.non_curated_cache_refresh_state.write() {
                Ok(state) => state,
                Err(err) => err.into_inner(),
            };
            if refreshed {
                state.last_refreshed = Some(request.clone());
            }
            let complete = state.requested.as_ref() == Some(&request);
            if complete {
                state.requested = None;
                state.in_flight = false;
            }
            self.non_curated_cache_refresh_completion
                .send_modify(|completion| {
                    completion.sequence = completion.sequence.wrapping_add(1);
                    if cache_changed {
                        completion.changed_sequence = completion.changed_sequence.wrapping_add(1);
                    }
                });
            if complete {
                return;
            }
        }
    }

    fn configured_plugin_states(&self, config: &PluginsConfigInput) -> ConfiguredPluginStates {
        let configured_plugins =
            configured_plugins_from_stack(&config.config_layer_stack, self.codex_home.as_path());
        let installed = configured_plugins
            .keys()
            .filter(|plugin_key| {
                PluginId::parse(plugin_key)
                    .ok()
                    .is_some_and(|plugin_id| self.store.is_installed(&plugin_id))
            })
            .cloned()
            .collect::<HashSet<_>>();
        let enabled = configured_plugins
            .into_iter()
            .filter_map(|(plugin_key, plugin)| {
                (plugin.enabled.unwrap_or(true) && config.plugins.allows_plugin(&plugin_key))
                    .then_some(plugin_key)
            })
            .collect::<HashSet<_>>();
        ConfiguredPluginStates { installed, enabled }
    }

    fn marketplace_roots(
        &self,
        config: &PluginsConfigInput,
        additional_roots: &[AbsolutePathBuf],
        include_openai_curated: bool,
    ) -> Vec<AbsolutePathBuf> {
        // Treat the curated catalog as an extra marketplace root so plugin listing can surface it
        // without requiring every caller to know where it is stored.
        let mut roots = additional_roots.to_vec();
        roots.extend(installed_marketplace_roots_from_layer_stack(
            &config.config_layer_stack,
            self.codex_home.as_path(),
        ));
        let curated_marketplace_path = if include_openai_curated {
            match target_curated_marketplace(self.auth_mode()) {
                TargetCuratedMarketplace::OpenAi => {
                    let curated_repo_root = curated_plugins_repo_path(self.codex_home.as_path());
                    curated_repo_root.is_dir().then_some(curated_repo_root)
                }
                TargetCuratedMarketplace::OpenAiApi => {
                    let api_marketplace_path =
                        curated_plugins_api_marketplace_path(self.codex_home.as_path());
                    api_marketplace_path
                        .is_file()
                        .then_some(api_marketplace_path)
                }
            }
        } else {
            None
        };
        if let Some(curated_marketplace_path) = curated_marketplace_path
            && let Ok(curated_marketplace_path) =
                AbsolutePathBuf::try_from(curated_marketplace_path)
        {
            roots.push(curated_marketplace_path);
        }
        roots.sort_unstable();
        roots.dedup();
        roots
    }

    fn list_marketplaces_with_policy(
        &self,
        config: &PluginsConfigInput,
        roots: &[AbsolutePathBuf],
    ) -> Result<MarketplaceListOutcome, MarketplaceError> {
        let mut outcome =
            list_marketplaces_with_cache(roots, home_dir().as_deref(), &self.store.manifest_cache)?;
        let policy = MarketplacePolicy::from_requirements(config.config_layer_stack.requirements());
        outcome.marketplaces.retain(|marketplace| {
            policy
                .validate_install(
                    &config.config_layer_stack,
                    self.codex_home.as_path(),
                    &marketplace.path,
                    &marketplace.name,
                )
                .is_ok()
        });
        Ok(outcome)
    }
}

pub(crate) fn plugin_install_required_description(source: &MarketplacePluginSource) -> String {
    let source_description = match source {
        MarketplacePluginSource::Git {
            url,
            path,
            ref_name,
            sha,
        } => {
            let mut parts = vec![url.clone()];
            if let Some(path) = path {
                parts.push(format!("path `{path}`"));
            }
            if let Some(ref_name) = ref_name {
                parts.push(format!("ref `{ref_name}`"));
            }
            if let Some(sha) = sha {
                parts.push(format!("sha `{sha}`"));
            }
            parts.join(", ")
        }
        MarketplacePluginSource::Local { path } => path.as_path().display().to_string(),
        MarketplacePluginSource::Npm {
            package,
            version,
            registry,
        } => {
            let mut parts = vec![package.clone()];
            if let Some(version) = version {
                parts.push(format!("version `{version}`"));
            }
            if let Some(registry) = registry {
                parts.push(format!("registry `{registry}`"));
            }
            parts.join(", ")
        }
    };

    let source_kind = if matches!(source, MarketplacePluginSource::Npm { .. }) {
        "an npm plugin"
    } else {
        "a cross-repo plugin"
    };
    format!(
        "This is {source_kind}. Install it to view more detailed information. The source of the plugin is {source_description}."
    )
}
