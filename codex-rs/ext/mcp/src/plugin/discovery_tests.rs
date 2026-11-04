//! Verifies merged MCPs use the first retained installation while preserving all package identities.

use super::*;
use codex_config::McpServerTransportConfig;
use codex_config::types::PluginsConfigToml;
use codex_core::config::ConfigBuilder;
use codex_core_plugins::PluginIdentity;
use codex_extension_api::SelectedPluginContribution;
use codex_utils_path_uri::PathUri;
use pretty_assertions::assert_eq;
