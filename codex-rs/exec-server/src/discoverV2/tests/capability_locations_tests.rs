//! Exercises installed-package and fixed skill location prewarming on real filesystem fixtures.

use pretty_assertions::assert_eq;
use std::path::Path;

use codex_utils_path_uri::PathUri;

use super::CapabilityLocation;
use super::CapabilityLocationRequest;
use super::InstalledPlugin;
use super::prewarm_locations;
use crate::LocalFileSystem;

pub(crate) const MANIFEST: &str = r#"{"name":"demo","version":"1.10.0"}"#;

pub(crate) fn request(root: &Path) -> anyhow::Result<CapabilityLocationRequest> {
    for path in ["codex", "home", "project", "etc/codex"] {
        std::fs::create_dir_all(root.join(path))?;
    }
    Ok(CapabilityLocationRequest {
        codex_home: PathUri::from_host_native_path(root.join("codex"))?,
        user_home: Some(PathUri::from_host_native_path(root.join("home"))?),
        system_skills_root: Some(PathUri::from_host_native_path(
            root.join("etc/codex/skills"),
        )?),
    })
}

pub(crate) fn write(root: &Path, relative: &str, contents: &str) -> anyhow::Result<()> {
    let path = root.join(relative);
    let parent = path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("test fixture path has no parent"))?;
    std::fs::create_dir_all(parent)?;
    std::fs::write(path, contents)?;
    Ok(())
}
