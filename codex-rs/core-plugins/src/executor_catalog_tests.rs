//! Tests conversion of DiscoveryV2 payloads into inert plugin catalogs.

use super::catalog_from_snapshots;
use crate::PluginCatalog;
use crate::PluginCatalogEntry;
use crate::PluginIdentity;
use crate::PluginSourceLocation;
use codex_exec_server::CapabilityTextFile;
use codex_exec_server::DiscoverV2CapabilitiesResponse;
use codex_exec_server::DiscoveredPluginFiles;
use codex_exec_server::ExecutorCapabilityDiscoveryV2Snapshot;
use codex_exec_server::ExecutorPlugin;
use codex_utils_path_uri::PathUri;
use pretty_assertions::assert_eq;
use std::collections::BTreeMap;
use std::sync::Arc;
