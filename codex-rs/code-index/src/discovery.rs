use std::ffi::OsStr;
use std::path::Path;
use std::path::PathBuf;
use std::time::UNIX_EPOCH;

use anyhow::Context;
use anyhow::Result;
use ignore::WalkBuilder;

pub(crate) const DEFAULT_MAX_FILE_BYTES: u64 = 1024 * 1024;

#[derive(Debug, Clone)]
pub(crate) struct DiscoveredFile {
    pub absolute_path: PathBuf,
    pub relative_path: String,
    pub size_bytes: u64,
    pub mtime_ns: i64,
}

#[derive(Debug, Default)]
pub(crate) struct Discovery {
    pub files: Vec<DiscoveredFile>,
    pub skipped_large: usize,
    pub skipped_sensitive: usize,
    pub walk_errors: usize,
}

pub(crate) fn resolve_index_root(path: &Path) -> Result<PathBuf> {
    let canonical = path
        .canonicalize()
        .with_context(|| format!("failed to resolve index root {}", path.display()))?;
    anyhow::ensure!(
        canonical.is_dir(),
        "index root is not a directory: {}",
        canonical.display()
    );

    let mut current = canonical.clone();
    loop {
        if current.join(".git").exists() {
            return Ok(current);
        }
        if !current.pop() {
            return Ok(canonical);
        }
    }
}

#[cfg(test)]
pub(crate) fn discover_files(root: &Path, max_file_bytes: u64) -> Result<Discovery> {
    discover_files_excluding(root, max_file_bytes, &[])
}

pub(crate) fn discover_files_excluding(
    root: &Path,
    max_file_bytes: u64,
    excluded_paths: &[PathBuf],
) -> Result<Discovery> {
    let mut discovery = Discovery::default();
    let mut builder = WalkBuilder::new(root);
    builder
        // Unlike the interactive @-mention search, the index is private by default.
        .hidden(true)
        .follow_links(false)
        .require_git(false)
        .add_custom_ignore_filename(".codexignore")
        .filter_entry(|entry| {
            !entry
                .file_type()
                .is_some_and(|file_type| file_type.is_dir())
                || !is_default_excluded_dir(entry.file_name())
        });

    for entry in builder.build() {
        let entry = match entry {
            Ok(entry) => entry,
            Err(_) => {
                discovery.walk_errors += 1;
                continue;
            }
        };
        let Some(file_type) = entry.file_type() else {
            continue;
        };
        if !file_type.is_file() || file_type.is_symlink() {
            continue;
        }
        if excluded_paths.iter().any(|path| entry.path() == path) {
            continue;
        }
        if is_sensitive_file(entry.file_name()) {
            discovery.skipped_sensitive += 1;
            continue;
        }
        let metadata = match entry.metadata() {
            Ok(metadata) => metadata,
            Err(_) => {
                discovery.walk_errors += 1;
                continue;
            }
        };
        if metadata.len() > max_file_bytes {
            discovery.skipped_large += 1;
            continue;
        }

        let Ok(relative_path) = entry.path().strip_prefix(root) else {
            continue;
        };
        let relative_path = relative_path.to_string_lossy().replace('\\', "/");
        let mtime_ns = metadata
            .modified()
            .ok()
            .and_then(|modified| modified.duration_since(UNIX_EPOCH).ok())
            .and_then(|duration| i64::try_from(duration.as_nanos()).ok())
            .unwrap_or_default();
        discovery.files.push(DiscoveredFile {
            absolute_path: entry.into_path(),
            relative_path,
            size_bytes: metadata.len(),
            mtime_ns,
        });
    }

    discovery
        .files
        .sort_by(|left, right| left.relative_path.cmp(&right.relative_path));
    Ok(discovery)
}

fn is_default_excluded_dir(name: &OsStr) -> bool {
    matches!(
        name.to_str(),
        Some(
            ".git"
                | ".hg"
                | ".svn"
                | ".venv"
                | "venv"
                | "__pycache__"
                | "node_modules"
                | "target"
                | "dist"
                | "build"
                | "coverage"
                | "vendor"
                | ".next"
                | ".turbo"
        )
    )
}

fn is_sensitive_file(name: &OsStr) -> bool {
    let Some(name) = name.to_str() else {
        return false;
    };
    let lower = name.to_ascii_lowercase();
    if lower == ".env" || lower.starts_with(".env.") {
        return true;
    }
    if matches!(
        lower.as_str(),
        "id_rsa"
            | "id_dsa"
            | "id_ecdsa"
            | "id_ed25519"
            | "credentials"
            | "credentials.json"
            | "secrets.json"
            | ".npmrc"
            | ".pypirc"
            | ".netrc"
            | ".git-credentials"
    ) {
        return true;
    }
    if lower.starts_with("credentials.")
        || lower.starts_with("secret.")
        || lower.starts_with("secrets.")
    {
        return true;
    }

    Path::new(&lower)
        .extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| {
            matches!(
                extension,
                "pem"
                    | "key"
                    | "p12"
                    | "pfx"
                    | "jks"
                    | "keystore"
                    | "kdbx"
                    | "age"
                    | "gpg"
                    | "secret"
                    | "secrets"
            )
        })
}

#[cfg(test)]
mod tests {
    use std::fs;

    use pretty_assertions::assert_eq;
    use tempfile::tempdir;

    use super::*;

    #[test]
    fn discovery_skips_private_and_generated_files() {
        let temp = tempdir().expect("tempdir");
        let root = temp.path();
        fs::create_dir(root.join(".git")).expect("git dir");
        fs::create_dir(root.join("src")).expect("src dir");
        fs::create_dir(root.join("node_modules")).expect("node_modules");
        fs::create_dir(root.join("ignored-dir")).expect("ignored dir");
        fs::write(root.join(".gitignore"), "ignored.rs\nignored-dir/\n").expect("gitignore");
        fs::write(root.join(".codexignore"), "private.rs\n").expect("codexignore");
        fs::write(root.join("src/lib.rs"), "fn visible() {}").expect("visible");
        fs::write(root.join(".env"), "SECRET=1").expect("env");
        fs::write(root.join(".hidden.rs"), "fn hidden() {}").expect("hidden");
        fs::write(root.join("node_modules/pkg.js"), "ignored").expect("generated");
        fs::write(root.join("ignored.rs"), "fn ignored() {}").expect("gitignored");
        fs::write(root.join("ignored-dir/pkg.rs"), "fn ignored() {}").expect("ignored dir file");
        fs::write(root.join("private.rs"), "fn private() {}").expect("codexignored");
        fs::write(root.join("src/key.pem"), "secret").expect("pem");

        let discovery = discover_files(root, DEFAULT_MAX_FILE_BYTES).expect("discover");
        let paths = discovery
            .files
            .iter()
            .map(|file| file.relative_path.as_str())
            .collect::<Vec<_>>();

        assert_eq!(paths, vec!["src/lib.rs"]);
        assert_eq!(discovery.skipped_sensitive, 1);
    }

    #[test]
    fn discovery_skips_files_over_size_limit() {
        let temp = tempdir().expect("tempdir");
        let root = temp.path();
        fs::create_dir(root.join(".git")).expect("git dir");
        fs::write(root.join("small.rs"), "abc").expect("small");
        fs::write(root.join("large.rs"), "abcdef").expect("large");

        let discovery = discover_files(root, 4).expect("discover");
        let paths = discovery
            .files
            .iter()
            .map(|file| file.relative_path.as_str())
            .collect::<Vec<_>>();

        assert_eq!(paths, vec!["small.rs"]);
        assert_eq!(discovery.skipped_large, 1);
    }
}
