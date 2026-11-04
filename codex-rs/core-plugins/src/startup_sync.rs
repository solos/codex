use std::path::Path;
use std::path::PathBuf;

const CURATED_PLUGINS_RELATIVE_DIR: &str = ".tmp/plugins";
const CURATED_PLUGINS_SHA_FILE: &str = ".tmp/plugins.sha";

pub fn curated_plugins_repo_path(codex_home: &Path) -> PathBuf {
    codex_home.join(CURATED_PLUGINS_RELATIVE_DIR)
}

pub fn curated_plugins_api_marketplace_path(codex_home: &Path) -> PathBuf {
    curated_plugins_repo_path(codex_home).join(".agents/plugins/api_marketplace.json")
}

pub fn read_curated_plugins_sha(codex_home: &Path) -> Option<String> {
    std::fs::read_to_string(codex_home.join(CURATED_PLUGINS_SHA_FILE))
        .ok()
        .map(|sha| sha.trim().to_string())
        .filter(|sha| !sha.is_empty())
}
