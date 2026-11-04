//! Read-only installed-version selection and persisted installation identity.

use codex_utils_absolute_path::AbsolutePathBuf;
use semver::Version;
use std::cmp::Ordering;
use std::fs;

pub const DEFAULT_PLUGIN_VERSION: &str = "local";
pub const PLUGINS_CACHE_DIR: &str = "plugins/cache";
pub fn validate_plugin_version_segment(plugin_version: &str) -> Result<(), String> {
    if plugin_version.is_empty() {
        return Err("invalid plugin version: must not be empty".to_string());
    }
    if matches!(plugin_version, "." | "..") {
        return Err("invalid plugin version: path traversal is not allowed".to_string());
    }
    if !plugin_version
        .chars()
        .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.' | '+'))
    {
        return Err(
            "invalid plugin version: only ASCII letters, digits, `.`, `+`, `_`, and `-` are allowed"
                .to_string(),
        );
    }
    Ok(())
}

pub fn compare_plugin_versions(left: &str, right: &str) -> Ordering {
    match (Version::parse(left), Version::parse(right)) {
        (Ok(left), Ok(right)) => left.cmp(&right),
        _ => left.cmp(right),
    }
}

pub fn active_plugin_version(plugin_base_root: &AbsolutePathBuf) -> Option<String> {
    let mut discovered_versions = fs::read_dir(plugin_base_root.as_path())
        .ok()?
        .filter_map(Result::ok)
        .filter_map(|entry| {
            entry.file_type().ok().filter(std::fs::FileType::is_dir)?;
            entry.file_name().into_string().ok()
        })
        .filter(|version| validate_plugin_version_segment(version).is_ok())
        .collect::<Vec<_>>();
    discovered_versions.sort_unstable_by(|left, right| compare_plugin_versions(left, right));
    if discovered_versions.is_empty() {
        None
    } else if discovered_versions
        .iter()
        .any(|version| version == DEFAULT_PLUGIN_VERSION)
    {
        Some(DEFAULT_PLUGIN_VERSION.to_string())
    } else {
        discovered_versions.pop()
    }
}
