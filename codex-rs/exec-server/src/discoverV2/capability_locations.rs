//! Prewarms installed cache packages and fixed standalone skill locations before discovery.

mod candidates;
use candidates::CandidateCapabilityLocations;

use std::collections::BTreeMap;
use std::io;

use crate::LocalFileSystem;
use codex_file_system::ExecutorFileSystem;
use codex_file_system::WalkEntryKind;
use codex_file_system::WalkOptions;
use codex_file_watcher::WatchPath;
use codex_utils_path_uri::PathUri;
use serde::Deserialize;
use serde::Serialize;

use codex_core_plugin_common::installed::PLUGINS_CACHE_DIR;
use codex_core_plugin_common::installed::active_plugin_version;
use codex_core_plugin_common::plugin_id::PluginId;

/// Fixed paths are supplied by the executor, not read from the sandbox helper's sanitized env.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct CapabilityLocationRequest {
    pub codex_home: PathUri,
    pub user_home: Option<PathUri>,
    pub system_skills_root: Option<PathUri>,
}

const MAX_LOCATIONS: usize = 512;
const AGENTS_SKILLS_SUFFIX: &str = ".agents/skills";

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct CapabilityLocation {
    pub root: PathUri,
    pub standalone_root_id: Option<String>,
    pub plugin: Option<InstalledPlugin>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct InstalledPlugin {
    pub id: String,
    pub version: String,
}

/// Selected locations; membership does not authorize capability reads.
#[derive(Clone, Default)]
pub(crate) struct CapabilityLocations {
    pub locations: Vec<CapabilityLocation>,
    pub watches: Vec<WatchPath>,
    pub warnings: Vec<String>,
}

/// Resolves fixed home/cache roots on the executor without reading configuration or using a cwd.
pub(crate) async fn prewarm_locations(
    fs: &LocalFileSystem,
    request: &CapabilityLocationRequest,
) -> io::Result<CapabilityLocations> {
    let codex_home = request.codex_home.to_abs_path()?;
    let user_home = request
        .user_home
        .as_ref()
        .map(PathUri::to_abs_path)
        .transpose()?;
    // Fixed standalone roots extend Flora's V1 root; project skill discovery is deferred.
    let mut skill_roots = vec![("codex-home-skills", codex_home.join("skills"))];
    if let Some(home) = user_home {
        skill_roots.push(("user-agent-skills", home.join(AGENTS_SKILLS_SUFFIX)));
    }
    if let Some(system_root) = &request.system_skills_root {
        skill_roots.push(("system-skills", system_root.to_abs_path()?));
    }
    CandidateCapabilityLocations::resolve(
        fs,
        Some(PathUri::from_abs_path(&codex_home.join(PLUGINS_CACHE_DIR))),
        skill_roots
            .into_iter()
            .map(|(id, root)| crate::SkillDiscoveryRoot {
                id: id.to_string(),
                path: PathUri::from_abs_path(&root),
            })
            .collect(),
    )
    .await
}

async fn discover_cached_plugins(
    fs: &LocalFileSystem,
    cache: &PathUri,
) -> io::Result<CapabilityLocations> {
    let mut locations = CapabilityLocations::default();
    // Enumerate packages; the shared selector scans their version directories.
    let walk = match fs
        .walk(
            cache,
            WalkOptions {
                max_depth: 1,
                max_directories: 2_000,
                max_entries: 20_000,
                follow_directory_symlinks: true,
                prune_hidden_directories: false,
            },
            /*sandbox*/ None,
        )
        .await
    {
        Ok(walk) => walk,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(locations),
        Err(error) => return Err(error),
    };
    // Reported walk errors must not become a cached partial inventory.
    if let Some(error) = walk.errors.into_iter().next() {
        return Err(io::Error::other(format!(
            "failed to scan plugin cache path {}: {}",
            error.path, error.message
        )));
    }
    if walk.truncated {
        locations
            .warnings
            .push("plugin cache traversal limit reached".to_string());
    }
    let mut packages = BTreeMap::new();
    for entry in walk.entries {
        if entry.kind != WalkEntryKind::Directory {
            continue;
        }
        let Some(relative) = entry.path.relative_path_from(cache) else {
            continue;
        };
        let parts = relative.split(['/', '\\']).collect::<Vec<_>>();
        let [marketplace, name] = parts.as_slice() else {
            continue;
        };
        let Ok(id) = PluginId::new((*name).to_string(), (*marketplace).to_string()) else {
            continue;
        };
        packages.insert(id.as_key(), entry.path.to_abs_path()?);
    }
    let installed = tokio::task::spawn_blocking(move || {
        packages
            .into_iter()
            .filter_map(|(id, package_root)| {
                let version = active_plugin_version(&package_root)?;
                Some((id, package_root, version))
            })
            .collect::<Vec<_>>()
    })
    .await
    .map_err(io::Error::other)?;
    for (id, package_root, version) in installed {
        if locations.locations.len() == MAX_LOCATIONS {
            locations.warnings.push(format!(
                "plugin cache reached its {MAX_LOCATIONS}-location limit"
            ));
            break;
        }
        locations.locations.push(CapabilityLocation {
            root: PathUri::from_abs_path(&package_root.join(&version)),
            standalone_root_id: None,
            plugin: Some(InstalledPlugin { id, version }),
        });
    }
    Ok(locations)
}

#[cfg(test)]
#[path = "tests/capability_locations_tests.rs"]
pub(crate) mod tests;
