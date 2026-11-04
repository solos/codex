use anyhow::Context;
use codex_config::skill_config_rules_from_stack;
use codex_login::CodexAuth;
use codex_plugin::PluginId;
use std::collections::HashSet;
use tracing::warn;

use crate::OPENAI_API_CURATED_MARKETPLACE_NAME;
use crate::OPENAI_CURATED_MARKETPLACE_NAME;
use crate::PluginsConfigInput;
use crate::PluginsManager;
use crate::marketplace::MarketplacePluginInstallPolicy;

const TOOL_SUGGEST_DISCOVERABLE_PLUGIN_ALLOWLIST: &[&str] = &[
    "github@openai-curated",
    "notion@openai-curated",
    "slack@openai-curated",
    "gmail@openai-curated",
    "google-calendar@openai-curated",
    "google-drive@openai-curated",
    "openai-developers@openai-curated",
    "canva@openai-curated",
    "teams@openai-curated",
    "sharepoint@openai-curated",
    "outlook-email@openai-curated",
    "outlook-calendar@openai-curated",
    "linear@openai-curated",
    "figma@openai-curated",
    "chrome@openai-bundled",
    "computer-use@openai-bundled",
];

#[derive(Debug, Clone)]
pub struct ToolSuggestPluginDiscoveryInput {
    pub plugins: PluginsConfigInput,
    pub configured_plugin_ids: HashSet<String>,
    pub disabled_plugin_ids: HashSet<String>,
    pub loaded_plugin_app_connector_ids: HashSet<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ToolSuggestDiscoverablePlugin {
    pub id: String,
    pub name: String,
    pub description: Option<String>,
    pub has_skills: bool,
    pub mcp_server_names: Vec<String>,
    pub app_connector_ids: Vec<String>,
}

impl PluginsManager {
    pub async fn list_tool_suggest_discoverable_plugins(
        &self,
        input: &ToolSuggestPluginDiscoveryInput,
        _auth: Option<&CodexAuth>,
    ) -> anyhow::Result<Vec<ToolSuggestDiscoverablePlugin>> {
        if !input.plugins.plugins_enabled {
            return Ok(Vec::new());
        }

        let marketplaces = self
            .list_marketplaces_for_config(&input.plugins, &[], /*include_openai_curated*/ true)
            .context("failed to list plugin marketplaces for tool suggestions")?
            .marketplaces;
        let skill_config_rules = skill_config_rules_from_stack(&input.plugins.config_layer_stack);

        let mut discoverable_plugins = Vec::<ToolSuggestDiscoverablePlugin>::new();
        for marketplace in marketplaces {
            let marketplace_name = marketplace.name;

            for plugin in marketplace.plugins {
                let is_configured_plugin = input.configured_plugin_ids.contains(plugin.id.as_str());
                let is_fallback_plugin = is_tool_suggest_fallback_plugin(&plugin.id);
                if plugin.installed
                    || plugin.policy.installation == MarketplacePluginInstallPolicy::NotAvailable
                    || input.disabled_plugin_ids.contains(plugin.id.as_str())
                    || (!is_configured_plugin && !is_fallback_plugin)
                {
                    continue;
                }

                let plugin_id = plugin.id.clone();
                match self
                    .tool_suggest_metadata_for_marketplace_plugin(
                        &marketplace_name,
                        &plugin,
                        &skill_config_rules,
                    )
                    .await
                {
                    Ok(plugin) => {
                        discoverable_plugins.push(ToolSuggestDiscoverablePlugin {
                            id: plugin.config_name,
                            name: plugin.display_name,
                            description: plugin.description,
                            has_skills: plugin.has_skills,
                            mcp_server_names: plugin.mcp_server_names,
                            app_connector_ids: plugin
                                .app_connector_ids
                                .into_iter()
                                .map(|connector_id| connector_id.0)
                                .collect(),
                        });
                    }
                    Err(err) => {
                        warn!("failed to load discoverable plugin suggestion {plugin_id}: {err:#}")
                    }
                }
            }
        }
        discoverable_plugins.sort_by(|left, right| {
            left.name
                .cmp(&right.name)
                .then_with(|| left.id.cmp(&right.id))
        });
        Ok(discoverable_plugins)
    }
}

fn is_tool_suggest_fallback_plugin(plugin_id: &str) -> bool {
    if TOOL_SUGGEST_DISCOVERABLE_PLUGIN_ALLOWLIST.contains(&plugin_id) {
        return true;
    }

    let Ok(plugin_id) = PluginId::parse(plugin_id) else {
        return false;
    };
    if plugin_id.marketplace_name != OPENAI_API_CURATED_MARKETPLACE_NAME {
        return false;
    }

    let default_curated_plugin_id = format!(
        "{}@{}",
        plugin_id.plugin_name, OPENAI_CURATED_MARKETPLACE_NAME
    );
    TOOL_SUGGEST_DISCOVERABLE_PLUGIN_ALLOWLIST.contains(&default_curated_plugin_id.as_str())
}
