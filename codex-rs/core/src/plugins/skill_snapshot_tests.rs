use codex_core_plugins::PluginsManager;
use codex_core_plugins::store::PluginStore;
use codex_login::AuthManager;
use codex_login::CodexAuth;
use codex_plugin::PluginId;
use codex_protocol::protocol::Product;
use codex_skills_extension::HostSkillsLoadInput;
use codex_skills_extension::HostSkillsService;
use codex_utils_absolute_path::AbsolutePathBuf;
use pretty_assertions::assert_eq;
use std::sync::Arc;

use super::test_support::load_plugins_config;
use super::test_support::write_file;

const PLUGIN_CONFIG_NAME: &str = "sample@openai-curated-remote";
const REMOTE_PLUGIN_ID: &str = "plugins~Plugin_sample";
