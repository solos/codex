//! Source-neutral plugin catalogs, reconciliation, and source-aware enablement policy.

use codex_config::types::PluginConfig;
use codex_core_plugin_common::PluginIdentity;
use codex_utils_path_uri::PathUri;
use serde_json::Value;
use std::collections::BTreeMap;
use std::collections::HashMap;

/// One source's discovery snapshot; only a complete empty catalog establishes absence.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PluginCatalog {
    pub entries: Vec<PluginCatalogEntry>,
    pub warnings: Vec<String>,
    /// Listing omitted entries.
    pub is_partial: bool,
}

/// Manifest metadata and every location that can supply the same logical plugin.
#[derive(Clone, PartialEq, Eq)]
pub struct PluginCatalogEntry {
    pub id: PluginIdentity,
    pub enabled: bool,
    pub display_name: String,
    pub version: String,
    /// Raw MCP server declarations keyed by their authored names.
    pub mcp_servers: BTreeMap<String, Value>,
    pub connector_ids: Vec<String>,
    pub locations: Vec<PluginSourceLocation>,
}

impl PluginCatalogEntry {
    /// Disabled entries never project. Executor entries require every installation's local
    /// plugin ID in the supplied config, where omitted enablement retains the default.
    pub fn is_enabled(&self, configured_plugins: &HashMap<String, PluginConfig>) -> bool {
        if !self.enabled {
            return false;
        }
        self.locations.iter().all(|location| match location {
            PluginSourceLocation::Executor { plugin_id, .. } => configured_plugins
                .get(plugin_id)
                .is_some_and(|plugin| plugin.enabled.unwrap_or(true)),
        })
    }
}

impl std::fmt::Debug for PluginCatalogEntry {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("PluginCatalogEntry")
            .field("id", &self.id)
            .field("enabled", &self.enabled)
            .field("display_name", &self.display_name)
            .field("version", &self.version)
            // Declaration values can contain secrets, so only log their keys.
            .field(
                "mcp_server_names",
                &self.mcp_servers.keys().collect::<Vec<_>>(),
            )
            .field("connector_ids", &self.connector_ids)
            .field("locations", &self.locations)
            .finish()
    }
}

/// A source that can supply a plugin.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum PluginSourceLocation {
    Executor {
        /// Executor environment that owns `root`.
        environment_id: String,
        /// Local `name@marketplace` key used for this installation in configuration.
        plugin_id: String,
        root: PathUri,
    },
}

/// Merges executor catalog snapshots without creating a second authority.
pub fn merge_catalogs(executor: Option<&PluginCatalog>) -> PluginCatalog {
    executor.cloned().unwrap_or_default()
}
