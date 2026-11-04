//! Coordinates executor MCP contributions with thread-owned catalogs.
//!
//! One contributor is registered for both lifecycles. Provider instances are shared, while
//! catalogs and executor metadata live together in thread extension data.

use crate::PluginProviders;
use codex_core::config::Config;
use codex_core_plugins::ExecutorPluginProvider;
use codex_exec_server::EnvironmentManager;
use codex_extension_api::ExtensionRegistryBuilder;
use std::sync::Arc;

pub(crate) struct PluginContributor {
    pub(crate) providers: PluginProviders,
}

/// Installs one coordinator for executor MCP callbacks.
pub fn install_plugin_providers(
    builder: &mut ExtensionRegistryBuilder<Config>,
    providers: PluginProviders,
) {
    let contributor = Arc::new(PluginContributor { providers });
    builder.thread_lifecycle_contributor(contributor.clone());
    builder.config_contributor(contributor.clone());
    builder.turn_lifecycle_contributor(contributor.clone());
    builder.mcp_server_contributor(contributor);
}

/// Installs plugin support backed by selected environment roots.
pub fn install_plugins(
    builder: &mut ExtensionRegistryBuilder<Config>,
    environment_manager: Arc<EnvironmentManager>,
) {
    install_plugin_providers(
        builder,
        PluginProviders::new(Arc::new(ExecutorPluginProvider::new(environment_manager))),
    );
}
