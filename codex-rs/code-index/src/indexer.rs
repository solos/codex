use std::collections::BTreeSet;
use std::fs::File;
use std::fs::OpenOptions;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::time::Duration;
use std::time::Instant;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

use anyhow::Context;
use anyhow::Result;
use codex_utils_home_dir::find_codex_home;
use sha2::Digest;
use sha2::Sha256;

use crate::chunker::DEFAULT_CHUNK_MAX_TOKENS;
use crate::chunker::DEFAULT_CHUNK_OVERLAP_LINES;
use crate::chunker::chunk_file;
use crate::discovery::DEFAULT_MAX_FILE_BYTES;
use crate::discovery::DiscoveredFile;
use crate::discovery::discover_files_excluding;
use crate::discovery::resolve_index_root;
use crate::language::CodeLanguage;
use crate::store::PreparedFile;
use crate::store::Store;

const BATCH_SIZE: usize = 64;
const MAX_REPORTED_ERRORS: usize = 20;
const WRITER_LOCK_TIMEOUT: Duration = Duration::from_secs(30);
const WRITER_LOCK_RETRY_MIN: Duration = Duration::from_millis(25);
const WRITER_LOCK_RETRY_MAX: Duration = Duration::from_millis(250);
pub const DEFAULT_PARSE_CONCURRENCY: usize = 0;
const MAX_PARSE_CONCURRENCY: usize = 32;
const PROGRESS_REPORT_INTERVAL: u64 = 128;

#[derive(Debug, Clone)]
pub struct IndexInitOptions {
    pub root: PathBuf,
    pub db_path: Option<PathBuf>,
    pub max_file_bytes: u64,
    pub chunk_max_tokens: usize,
    pub chunk_overlap_lines: usize,
    /// Maximum concurrent parser workers. Zero selects a bounded automatic value.
    pub parse_concurrency: usize,
}

impl IndexInitOptions {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            db_path: None,
            max_file_bytes: DEFAULT_MAX_FILE_BYTES,
            chunk_max_tokens: DEFAULT_CHUNK_MAX_TOKENS,
            chunk_overlap_lines: DEFAULT_CHUNK_OVERLAP_LINES,
            parse_concurrency: DEFAULT_PARSE_CONCURRENCY,
        }
    }
}

#[derive(Debug, Clone)]
pub struct IndexUpdateOptions {
    pub root: PathBuf,
    pub db_path: Option<PathBuf>,
    pub max_file_bytes: u64,
    pub chunk_max_tokens: usize,
    pub chunk_overlap_lines: usize,
    /// Maximum concurrent parser workers. Zero selects a bounded automatic value.
    pub parse_concurrency: usize,
}

impl IndexUpdateOptions {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            db_path: None,
            max_file_bytes: DEFAULT_MAX_FILE_BYTES,
            chunk_max_tokens: DEFAULT_CHUNK_MAX_TOKENS,
            chunk_overlap_lines: DEFAULT_CHUNK_OVERLAP_LINES,
            parse_concurrency: DEFAULT_PARSE_CONCURRENCY,
        }
    }
}

impl From<IndexInitOptions> for IndexUpdateOptions {
    fn from(options: IndexInitOptions) -> Self {
        Self {
            root: options.root,
            db_path: options.db_path,
            max_file_bytes: options.max_file_bytes,
            chunk_max_tokens: options.chunk_max_tokens,
            chunk_overlap_lines: options.chunk_overlap_lines,
            parse_concurrency: options.parse_concurrency,
        }
    }
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct IndexProgress {
    pub files_processed: u64,
    pub files_total: u64,
    pub files_indexed: u64,
    pub files_unchanged: u64,
    pub skipped_binary: u64,
    pub skipped_non_utf8: u64,
    pub read_errors: u64,
    pub elapsed_ms: u64,
    pub cancelled: bool,
}

#[derive(Clone, Default)]
pub struct IndexControl {
    cancel: Arc<AtomicBool>,
    progress: Option<Arc<dyn Fn(IndexProgress) + Send + Sync>>,
}

impl IndexControl {
    pub fn with_progress(
        mut self,
        progress: impl Fn(IndexProgress) + Send + Sync + 'static,
    ) -> Self {
        self.progress = Some(Arc::new(progress));
        self
    }

    pub fn cancel(&self) {
        self.cancel.store(true, Ordering::SeqCst);
    }

    fn is_cancelled(&self) -> bool {
        self.cancel.load(Ordering::SeqCst)
    }

    fn cancel_flag(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.cancel)
    }

    fn report(&self, progress: IndexProgress) {
        if let Some(callback) = &self.progress {
            callback(progress);
        }
    }
}

#[derive(Debug, Clone)]
pub struct IndexStatusOptions {
    pub root: PathBuf,
    pub db_path: Option<PathBuf>,
}

impl IndexStatusOptions {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            db_path: None,
        }
    }
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct IndexReport {
    pub mode: IndexMode,
    pub root_path: String,
    pub db_path: String,
    pub changed_paths: u64,
    pub files_seen: u64,
    pub files_indexed: u64,
    pub files_unchanged: u64,
    pub files_deleted: u64,
    pub chunks_written: u64,
    pub skipped_large: u64,
    pub skipped_sensitive: u64,
    pub skipped_binary: u64,
    pub skipped_non_utf8: u64,
    pub read_errors: u64,
    pub walk_errors: u64,
    pub errors: Vec<String>,
    pub elapsed_ms: u64,
    pub cancelled: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum IndexMode {
    Full,
    Incremental,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct IndexStatus {
    pub root_path: String,
    pub db_path: String,
    pub initialized: bool,
    pub file_count: u64,
    pub chunk_count: u64,
    pub last_indexed_unix_ms: Option<i64>,
    pub languages: std::collections::BTreeMap<String, u64>,
}

#[derive(Debug)]
pub(crate) struct ResolvedIndexPaths {
    pub root: PathBuf,
    pub db_path: PathBuf,
}

pub(crate) fn resolve_index_paths(
    root: &Path,
    configured_db_path: Option<&Path>,
) -> Result<ResolvedIndexPaths> {
    let root = resolve_index_root(root)?;
    let db_path = match configured_db_path {
        Some(path) if path.is_absolute() => path.to_path_buf(),
        Some(path) => std::env::current_dir()
            .map(|current_dir| current_dir.join(path))
            .map_err(anyhow::Error::from)?,
        None => default_index_db_path(&root)?,
    };
    Ok(ResolvedIndexPaths { root, db_path })
}

pub async fn init_index(options: IndexInitOptions) -> Result<IndexReport> {
    init_index_with_control(options, IndexControl::default()).await
}

pub async fn init_index_with_control(
    options: IndexInitOptions,
    control: IndexControl,
) -> Result<IndexReport> {
    run_index(options.into(), None, control).await
}

/// Reconcile the full indexed tree with the current filesystem.
///
/// This is still hash-incremental: unchanged files are not parsed or rewritten.
pub async fn update_index(options: IndexUpdateOptions) -> Result<IndexReport> {
    update_index_with_control(options, IndexControl::default()).await
}

pub async fn update_index_with_control(
    options: IndexUpdateOptions,
    control: IndexControl,
) -> Result<IndexReport> {
    run_index(options, None, control).await
}

/// Update only the indexed files affected by `changed_paths`.
///
/// Changed paths may be absolute or relative to the index root. Directory
/// paths affect their whole subtree. Deleted and renamed paths are removed from
/// the index, and ignore-file changes trigger a full reconciliation while
/// retaining content-hash skip behavior.
pub async fn update_paths(
    options: IndexUpdateOptions,
    changed_paths: &[PathBuf],
) -> Result<IndexReport> {
    update_paths_with_control(options, changed_paths, IndexControl::default()).await
}

pub async fn update_paths_with_control(
    options: IndexUpdateOptions,
    changed_paths: &[PathBuf],
    control: IndexControl,
) -> Result<IndexReport> {
    run_index(options, Some(changed_paths), control).await
}

async fn run_index(
    options: IndexUpdateOptions,
    changed_paths: Option<&[PathBuf]>,
    control: IndexControl,
) -> Result<IndexReport> {
    let started = Instant::now();
    let ResolvedIndexPaths { root, db_path } =
        resolve_index_paths(&options.root, options.db_path.as_deref())?;
    let _writer_lock = IndexWriterLock::acquire(&db_path).await?;
    let store = Store::open_writable(&db_path).await?;
    let artifact_paths = index_artifact_paths(&db_path);
    let artifact_relative_paths = artifact_paths
        .iter()
        .filter_map(|path| normalize_changed_path(&root, path))
        .collect::<Vec<_>>();
    let discovery = discover_files_excluding(&root, options.max_file_bytes, &artifact_paths)?;
    let existing = store.existing_files().await?;

    let mut seen = BTreeSet::new();
    let mut batch = Vec::with_capacity(BATCH_SIZE);
    let mut report = IndexReport {
        mode: if changed_paths.is_some() {
            IndexMode::Incremental
        } else {
            IndexMode::Full
        },
        root_path: root.to_string_lossy().into_owned(),
        db_path: db_path.to_string_lossy().into_owned(),
        changed_paths: u64::try_from(changed_paths.map_or(0, <[PathBuf]>::len)).unwrap_or(u64::MAX),
        files_seen: u64::try_from(discovery.files.len()).unwrap_or(u64::MAX),
        files_indexed: 0,
        files_unchanged: 0,
        files_deleted: 0,
        chunks_written: 0,
        skipped_large: u64::try_from(discovery.skipped_large).unwrap_or(u64::MAX),
        skipped_sensitive: u64::try_from(discovery.skipped_sensitive).unwrap_or(u64::MAX),
        skipped_binary: 0,
        skipped_non_utf8: 0,
        read_errors: 0,
        walk_errors: u64::try_from(discovery.walk_errors).unwrap_or(u64::MAX),
        errors: Vec::new(),
        elapsed_ms: 0,
        cancelled: false,
    };
    let mut scope = match changed_paths {
        Some(paths) => ChangeScope::from_paths(&root, paths, &artifact_relative_paths, &mut report),
        None => ChangeScope::All,
    };
    if scope.contains_ignore_control() {
        // A changed ignore file can make any path enter or leave the index.
        // Reconcile the tree, but hash skipping still avoids reparsing unchanged files.
        scope = ChangeScope::All;
    }
    if matches!(&scope, ChangeScope::Paths(paths) if paths.is_empty()) {
        store.close().await;
        report.elapsed_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
        return Ok(report);
    }

    let selected_files = discovery
        .files
        .into_iter()
        .filter(|file| scope.matches(&file.relative_path))
        .collect::<Vec<_>>();
    let files_total = u64::try_from(selected_files.len()).unwrap_or(u64::MAX);
    let mut files_processed = 0_u64;
    let parse_concurrency = effective_parse_concurrency(options.parse_concurrency);
    let chunk_max_tokens = options.chunk_max_tokens;
    let chunk_overlap_lines = options.chunk_overlap_lines;
    let worker_cancel = control.cancel_flag();
    control.report(progress_snapshot(
        &report,
        files_processed,
        files_total,
        started,
    ));

    let mut files = selected_files.into_iter();
    let mut workers = tokio::task::JoinSet::new();
    let mut active_workers = 0_usize;

    loop {
        while active_workers < parse_concurrency && !control.is_cancelled() {
            let Some(file) = files.next() else {
                break;
            };
            let root = root.clone();
            let existing_hash = existing
                .get(&file.relative_path)
                .map(|existing| existing.content_hash.clone());
            let cancel = Arc::clone(&worker_cancel);
            workers.spawn_blocking(move || {
                if cancel.load(Ordering::SeqCst) {
                    return (file, Err(anyhow::anyhow!("code index interrupted")));
                }
                let outcome = prepare_file(
                    &root,
                    &file,
                    existing_hash.as_deref(),
                    chunk_max_tokens,
                    chunk_overlap_lines,
                );
                (file, outcome)
            });
            active_workers += 1;
        }

        if active_workers == 0 {
            if control.is_cancelled() {
                report.cancelled = true;
            }
            break;
        }

        let Some(joined) = workers.join_next().await else {
            break;
        };
        active_workers -= 1;
        if control.is_cancelled() {
            report.cancelled = true;
            // Stop scheduling new files, but wait for already-running parser
            // workers before closing the store or returning to the CLI.
            continue;
        }

        let (file, outcome) = match joined {
            Ok(result) => result,
            Err(error) => {
                report.read_errors += 1;
                if report.errors.len() < MAX_REPORTED_ERRORS {
                    report
                        .errors
                        .push(format!("code index parser worker failed: {error}"));
                }
                continue;
            }
        };
        files_processed = files_processed.saturating_add(1);
        match outcome {
            Ok(PrepareOutcome::Indexed(prepared)) => {
                seen.insert(file.relative_path.clone());
                report.files_indexed += 1;
                batch.push(prepared);
                if batch.len() >= BATCH_SIZE {
                    report.chunks_written = report
                        .chunks_written
                        .saturating_add(store.apply_batch(&batch).await?);
                    batch.clear();
                }
            }
            Ok(PrepareOutcome::Unchanged) => {
                seen.insert(file.relative_path);
                report.files_unchanged += 1;
            }
            Ok(PrepareOutcome::SkippedBinary) => {
                report.skipped_binary += 1;
            }
            Ok(PrepareOutcome::SkippedNonUtf8) => {
                report.skipped_non_utf8 += 1;
            }
            Err(error) => {
                // Preserve an existing entry when a transient read or parse error occurs.
                if file.absolute_path.exists() {
                    seen.insert(file.relative_path.clone());
                }
                report.read_errors += 1;
                if report.errors.len() < MAX_REPORTED_ERRORS {
                    report
                        .errors
                        .push(format!("{}: {error}", file.relative_path));
                }
            }
        }
        if files_processed.is_multiple_of(PROGRESS_REPORT_INTERVAL)
            || files_processed >= files_total
        {
            control.report(progress_snapshot(
                &report,
                files_processed,
                files_total,
                started,
            ));
        }
    }
    report.chunks_written = report
        .chunks_written
        .saturating_add(store.apply_batch(&batch).await?);

    if !report.cancelled {
        let deleted_paths = existing
            .keys()
            .filter(|path| !seen.contains(*path) && scope.matches(path))
            .cloned()
            .collect::<Vec<_>>();
        report.files_deleted = u64::try_from(deleted_paths.len()).unwrap_or(u64::MAX);
        store.delete_paths(&deleted_paths).await?;
    }
    if !report.cancelled
        && (changed_paths.is_none() || report.files_indexed > 0 || report.files_deleted > 0)
    {
        store.set_meta("root_path", &report.root_path).await?;
        store
            .set_meta("last_indexed_unix_ms", &now_unix_ms().to_string())
            .await?;
    }
    store.close().await;

    report.errors.sort();
    report.elapsed_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
    control.report(progress_snapshot(
        &report,
        files_processed,
        files_total,
        started,
    ));
    Ok(report)
}

fn effective_parse_concurrency(requested: usize) -> usize {
    let requested = if requested == 0 {
        std::thread::available_parallelism().map_or(4, std::num::NonZeroUsize::get)
    } else {
        requested
    };
    requested.clamp(1, MAX_PARSE_CONCURRENCY)
}

fn progress_snapshot(
    report: &IndexReport,
    files_processed: u64,
    files_total: u64,
    started: Instant,
) -> IndexProgress {
    IndexProgress {
        files_processed,
        files_total,
        files_indexed: report.files_indexed,
        files_unchanged: report.files_unchanged,
        skipped_binary: report.skipped_binary,
        skipped_non_utf8: report.skipped_non_utf8,
        read_errors: report.read_errors,
        elapsed_ms: u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
        cancelled: report.cancelled,
    }
}

enum ChangeScope {
    All,
    Paths(Vec<String>),
}

impl ChangeScope {
    fn from_paths(
        root: &Path,
        paths: &[PathBuf],
        artifact_relative_paths: &[String],
        report: &mut IndexReport,
    ) -> Self {
        let mut changed = Vec::new();
        for path in paths {
            match normalize_changed_path(root, path) {
                Some(path) if !artifact_relative_paths.contains(&path) => changed.push(path),
                Some(_) => {}
                None => {
                    if report.errors.len() < MAX_REPORTED_ERRORS {
                        report.errors.push(format!(
                            "ignored changed path outside index root: {}",
                            path.display()
                        ));
                    }
                }
            }
        }
        changed.sort();
        changed.dedup();
        if changed.iter().any(String::is_empty) {
            Self::All
        } else {
            Self::Paths(changed)
        }
    }

    fn matches(&self, relative_path: &str) -> bool {
        match self {
            Self::All => true,
            Self::Paths(paths) => paths.iter().any(|changed| {
                relative_path == changed
                    || relative_path
                        .strip_prefix(changed)
                        .is_some_and(|suffix| suffix.starts_with('/'))
            }),
        }
    }

    fn contains_ignore_control(&self) -> bool {
        match self {
            Self::All => false,
            Self::Paths(paths) => paths.iter().any(|path| is_ignore_control(path)),
        }
    }
}

fn normalize_changed_path(root: &Path, path: &Path) -> Option<String> {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        root.join(path)
    };
    let relative = absolute.strip_prefix(root).ok()?;
    let mut components = Vec::new();
    for component in relative.components() {
        match component {
            std::path::Component::CurDir => {}
            std::path::Component::Normal(component) => {
                components.push(component.to_string_lossy().into_owned());
            }
            std::path::Component::ParentDir => {
                components.pop()?;
            }
            std::path::Component::RootDir | std::path::Component::Prefix(_) => return None,
        }
    }
    Some(components.join("/"))
}

fn is_ignore_control(path: &str) -> bool {
    matches!(
        path.rsplit('/').next(),
        Some(".gitignore" | ".codexignore" | ".ignore")
    ) || path.ends_with(".git/info/exclude")
}

pub async fn index_status(options: IndexStatusOptions) -> Result<IndexStatus> {
    let ResolvedIndexPaths { root, db_path } =
        resolve_index_paths(&options.root, options.db_path.as_deref())?;
    if !db_path.exists() {
        return Ok(IndexStatus {
            root_path: root.to_string_lossy().into_owned(),
            db_path: db_path.to_string_lossy().into_owned(),
            initialized: false,
            file_count: 0,
            chunk_count: 0,
            last_indexed_unix_ms: None,
            languages: std::collections::BTreeMap::new(),
        });
    }

    let store = Store::open_read_only(&db_path).await?;
    let status = store.status().await?;
    store.close().await;
    Ok(IndexStatus {
        root_path: root.to_string_lossy().into_owned(),
        db_path: db_path.to_string_lossy().into_owned(),
        initialized: true,
        file_count: status.file_count,
        chunk_count: status.chunk_count,
        last_indexed_unix_ms: status.last_indexed_unix_ms,
        languages: status.languages,
    })
}

fn prepare_file(
    root: &Path,
    file: &DiscoveredFile,
    existing_content_hash: Option<&str>,
    chunk_max_tokens: usize,
    chunk_overlap_lines: usize,
) -> Result<PrepareOutcome> {
    let bytes = std::fs::read(&file.absolute_path)?;
    if is_binary(&bytes) {
        return Ok(PrepareOutcome::SkippedBinary);
    }
    let Ok(source) = String::from_utf8(bytes) else {
        return Ok(PrepareOutcome::SkippedNonUtf8);
    };
    let content_hash = format!("{:x}", Sha256::digest(source.as_bytes()));
    if existing_content_hash.is_some_and(|existing| existing == content_hash) {
        return Ok(PrepareOutcome::Unchanged);
    }

    let language = CodeLanguage::from_path(&file.absolute_path);
    let chunks =
        chunk_file(language, &source, chunk_max_tokens, chunk_overlap_lines).map_err(|error| {
            anyhow::anyhow!(
                "failed to parse {} under {}: {error}",
                file.relative_path,
                root.display()
            )
        })?;
    Ok(PrepareOutcome::Indexed(PreparedFile {
        path: file.relative_path.clone(),
        size_bytes: file.size_bytes,
        mtime_ns: file.mtime_ns,
        content_hash,
        language,
        chunks,
        indexed_at_unix_ms: now_unix_ms(),
    }))
}

fn is_binary(bytes: &[u8]) -> bool {
    bytes.iter().take(8192).any(|byte| *byte == 0)
}

fn default_index_db_path(root: &Path) -> Result<PathBuf> {
    let home = find_codex_home()?;
    let digest = Sha256::digest(root.to_string_lossy().as_bytes());
    let repo_key = format!("{digest:x}").chars().take(20).collect::<String>();
    Ok(home
        .as_ref()
        .join("code-index")
        .join(repo_key)
        .join("index.sqlite"))
}

pub(crate) fn now_unix_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|duration| i64::try_from(duration.as_millis()).ok())
        .unwrap_or_default()
}

struct IndexWriterLock {
    _file: File,
}

impl IndexWriterLock {
    async fn acquire(db_path: &Path) -> Result<Self> {
        if let Some(parent) = db_path.parent() {
            tokio::fs::create_dir_all(parent).await.with_context(|| {
                format!("failed to create index directory {}", parent.display())
            })?;
        }
        let lock_path = writer_lock_path(db_path);
        let file = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(&lock_path)
            .with_context(|| format!("failed to open index writer lock {}", lock_path.display()))?;
        let started = Instant::now();
        let mut delay = WRITER_LOCK_RETRY_MIN;
        loop {
            match file.try_lock() {
                Ok(()) => return Ok(Self { _file: file }),
                Err(std::fs::TryLockError::WouldBlock) => {
                    if started.elapsed() >= WRITER_LOCK_TIMEOUT {
                        anyhow::bail!(
                            "timed out waiting for the code index writer lock {}",
                            lock_path.display()
                        );
                    }
                    tokio::time::sleep(delay).await;
                    delay = (delay * 2).min(WRITER_LOCK_RETRY_MAX);
                }
                Err(std::fs::TryLockError::Error(error)) => {
                    return Err(error).with_context(|| {
                        format!(
                            "failed to acquire index writer lock {}",
                            lock_path.display()
                        )
                    });
                }
            }
        }
    }
}

fn writer_lock_path(db_path: &Path) -> PathBuf {
    let mut path = db_path.as_os_str().to_os_string();
    path.push(".writer.lock");
    PathBuf::from(path)
}

pub(crate) fn index_artifact_paths(db_path: &Path) -> Vec<PathBuf> {
    let mut paths = vec![db_path.to_path_buf(), writer_lock_path(db_path)];
    for suffix in ["-wal", "-shm", "-journal"] {
        let mut sidecar = db_path.as_os_str().to_os_string();
        sidecar.push(suffix);
        paths.push(PathBuf::from(sidecar));
    }
    paths
}

enum PrepareOutcome {
    Indexed(PreparedFile),
    Unchanged,
    SkippedBinary,
    SkippedNonUtf8,
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::time::Duration;

    use codex_state::SqliteConfig;
    use codex_utils_absolute_path::AbsolutePathBuf;
    use pretty_assertions::assert_eq;
    use tempfile::tempdir;

    use super::*;

    #[tokio::test]
    async fn init_index_is_incremental_and_cleans_deleted_files() {
        let temp = tempdir().expect("tempdir");
        let root = temp.path().join("repo");
        fs::create_dir_all(root.join(".git")).expect("git dir");
        fs::create_dir_all(root.join("src")).expect("src dir");
        fs::write(
            root.join("src/lib.rs"),
            "pub fn unique_needle() -> usize { 1 }\n",
        )
        .expect("source");
        fs::write(root.join("README.md"), "documentation\n").expect("readme");
        fs::write(root.join(".env"), "SECRET=1\n").expect("env");
        fs::write(root.join("private.pem"), "secret\n").expect("pem");
        fs::write(root.join(".hidden.rs"), "fn hidden() {}\n").expect("hidden");
        fs::write(root.join(".gitignore"), "gitignored.rs\n").expect("gitignore");
        fs::write(root.join(".codexignore"), "codexignored.rs\n").expect("codexignore");
        fs::write(root.join("gitignored.rs"), "fn ignored() {}\n").expect("gitignored");
        fs::write(root.join("codexignored.rs"), "fn ignored() {}\n").expect("codexignored");
        fs::write(root.join("image.bin"), [0_u8, 1, 2, 3]).expect("binary");
        fs::write(root.join("invalid-utf8.txt"), [0xff_u8, 0xfe, 0xfd]).expect("non utf8");

        let db_path = temp.path().join("index.sqlite");
        let report = init_index(IndexInitOptions {
            root: root.clone(),
            db_path: Some(db_path.clone()),
            ..IndexInitOptions::new(&root)
        })
        .await
        .expect("initial index");
        assert_eq!(report.files_indexed, 2);
        assert_eq!(report.skipped_sensitive, 1);
        assert_eq!(report.skipped_binary, 1);
        assert_eq!(report.skipped_non_utf8, 1);

        let sqlite = SqliteConfig::from_sqlite_home_with_nfs_enforcement(
            AbsolutePathBuf::from_absolute_path(temp.path()).expect("absolute temp path"),
            /*enforce_delete_on_nfs*/ false,
        );
        let pool = sqlite
            .open_read_only_pool(&db_path, Some(Duration::from_secs(5)))
            .await
            .expect("open db");
        let hidden_count: i64 =
            sqlx::query_scalar("SELECT count(*) FROM files WHERE path = '.hidden.rs'")
                .fetch_one(&pool)
                .await
                .expect("hidden query");
        assert_eq!(hidden_count, 0);
        let secret_count: i64 =
            sqlx::query_scalar("SELECT count(*) FROM files WHERE path = '.env'")
                .fetch_one(&pool)
                .await
                .expect("secret query");
        assert_eq!(secret_count, 0);
        let ignored_count: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM files
             WHERE path IN ('gitignored.rs', 'codexignored.rs', 'invalid-utf8.txt')",
        )
        .fetch_one(&pool)
        .await
        .expect("ignored query");
        assert_eq!(ignored_count, 0);
        let fts_count: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM chunks_fts WHERE chunks_fts MATCH 'unique_needle'",
        )
        .fetch_one(&pool)
        .await
        .expect("fts query");
        assert_eq!(fts_count, 1);
        let trigram_count: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM chunks_trigram WHERE chunks_trigram MATCH 'unique_needle'",
        )
        .fetch_one(&pool)
        .await
        .expect("trigram query");
        assert_eq!(trigram_count, 1);

        let second = init_index(IndexInitOptions {
            root: root.clone(),
            db_path: Some(db_path.clone()),
            ..IndexInitOptions::new(&root)
        })
        .await
        .expect("second index");
        assert_eq!(second.files_indexed, 0);
        assert_eq!(second.files_unchanged, 2);

        fs::write(
            root.join("src/lib.rs"),
            "pub fn replacement_needle() -> usize { 2 }\n",
        )
        .expect("modify source");
        fs::remove_file(root.join("README.md")).expect("delete readme");
        let third = init_index(IndexInitOptions {
            root: root.clone(),
            db_path: Some(db_path.clone()),
            ..IndexInitOptions::new(&root)
        })
        .await
        .expect("third index");
        assert_eq!(third.files_indexed, 1);
        assert_eq!(third.files_deleted, 1);

        let old_fts_count: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM chunks_fts WHERE chunks_fts MATCH 'unique_needle'",
        )
        .fetch_one(&pool)
        .await
        .expect("old fts query");
        assert_eq!(old_fts_count, 0);
        let new_fts_count: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM chunks_fts WHERE chunks_fts MATCH 'replacement_needle'",
        )
        .fetch_one(&pool)
        .await
        .expect("new fts query");
        assert_eq!(new_fts_count, 1);
    }

    #[tokio::test]
    async fn init_index_skips_files_over_configured_limit() {
        let temp = tempdir().expect("tempdir");
        let root = temp.path().join("repo");
        fs::create_dir_all(root.join(".git")).expect("git dir");
        fs::write(root.join("small.rs"), "fn a() {}\n").expect("small");
        fs::write(root.join("large.rs"), "fn large() { let value = 1; }\n").expect("large");

        let db_path = temp.path().join("index.sqlite");
        let report = init_index(IndexInitOptions {
            root: root.clone(),
            db_path: Some(db_path),
            max_file_bytes: 12,
            ..IndexInitOptions::new(&root)
        })
        .await
        .expect("index");

        assert_eq!(report.files_indexed, 1);
        assert_eq!(report.skipped_large, 1);
    }

    #[tokio::test]
    async fn cancelled_init_does_not_schedule_or_commit_files() {
        let temp = tempdir().expect("tempdir");
        let root = temp.path().join("repo");
        fs::create_dir_all(root.join(".git")).expect("git dir");
        fs::write(root.join("one.rs"), "pub fn cancelled() {}\n").expect("one");
        fs::write(root.join("two.rs"), "pub fn skipped() {}\n").expect("two");

        let db_path = temp.path().join("index.sqlite");
        let control = IndexControl::default();
        control.cancel();
        let report = init_index_with_control(
            IndexInitOptions {
                root: root.clone(),
                db_path: Some(db_path),
                ..IndexInitOptions::new(&root)
            },
            control,
        )
        .await
        .expect("cancelled index");

        assert!(report.cancelled);
        assert_eq!(report.files_indexed, 0);
        assert_eq!(report.chunks_written, 0);
        assert_eq!(report.files_deleted, 0);
    }

    #[tokio::test]
    async fn cancellation_stops_scheduling_after_in_flight_work() {
        let temp = tempdir().expect("tempdir");
        let root = temp.path().join("repo");
        fs::create_dir_all(root.join(".git")).expect("git dir");
        for index in 0..160 {
            fs::write(
                root.join(format!("file_{index:03}.rs")),
                format!("pub fn function_{index}() -> usize {{ {index} }}\n"),
            )
            .expect("source");
        }

        let db_path = temp.path().join("index.sqlite");
        let control = IndexControl::default();
        let cancel_control = control.clone();
        let control = control.with_progress(move |progress| {
            if progress.files_processed >= 1 && !progress.cancelled {
                cancel_control.cancel();
            }
        });
        let report = init_index_with_control(
            IndexInitOptions {
                root: root.clone(),
                db_path: Some(db_path),
                parse_concurrency: 2,
                ..IndexInitOptions::new(&root)
            },
            control,
        )
        .await
        .expect("cancelled index");

        assert!(report.cancelled);
        assert!(report.files_indexed > 0);
        assert!(report.files_indexed < 160);
        assert_eq!(report.files_deleted, 0);
    }

    #[tokio::test]
    async fn update_paths_reindexes_only_changed_files_and_removes_deleted_paths() {
        let temp = tempdir().expect("tempdir");
        let root = temp.path().join("repo");
        fs::create_dir_all(root.join(".git")).expect("git dir");
        fs::write(root.join("one.rs"), "pub fn old_needle() {}\n").expect("one");
        fs::write(root.join("two.rs"), "pub fn untouched() {}\n").expect("two");
        let db_path = temp.path().join("index.sqlite");
        init_index(IndexInitOptions {
            root: root.clone(),
            db_path: Some(db_path.clone()),
            ..IndexInitOptions::new(&root)
        })
        .await
        .expect("initial index");

        fs::write(root.join("one.rs"), "pub fn new_needle() {}\n").expect("modify one");
        let report = update_paths(
            IndexUpdateOptions {
                root: root.clone(),
                db_path: Some(db_path.clone()),
                ..IndexUpdateOptions::new(&root)
            },
            &[root.join("one.rs")],
        )
        .await
        .expect("update one file");
        assert_eq!(report.mode, IndexMode::Incremental);
        assert_eq!(report.files_indexed, 1);
        assert_eq!(report.files_unchanged, 0);
        assert_eq!(report.files_deleted, 0);

        let sqlite = SqliteConfig::from_sqlite_home_with_nfs_enforcement(
            AbsolutePathBuf::from_absolute_path(temp.path()).expect("absolute temp path"),
            /*enforce_delete_on_nfs*/ false,
        );
        let pool = sqlite
            .open_read_only_pool(&db_path, Some(Duration::from_secs(5)))
            .await
            .expect("open db");
        let old_count: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM chunks_fts WHERE chunks_fts MATCH 'old_needle'",
        )
        .fetch_one(&pool)
        .await
        .expect("old fts query");
        let new_count: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM chunks_fts WHERE chunks_fts MATCH 'new_needle'",
        )
        .fetch_one(&pool)
        .await
        .expect("new fts query");
        assert_eq!((old_count, new_count), (0, 1));

        fs::remove_file(root.join("two.rs")).expect("delete two");
        let report = update_paths(
            IndexUpdateOptions {
                root: root.clone(),
                db_path: Some(db_path),
                ..IndexUpdateOptions::new(&root)
            },
            &[root.join("two.rs")],
        )
        .await
        .expect("delete one file");
        assert_eq!(report.files_deleted, 1);
    }

    #[tokio::test]
    async fn update_paths_preserves_unchanged_chunks_inside_a_changed_file() {
        let temp = tempdir().expect("tempdir");
        let root = temp.path().join("repo");
        fs::create_dir_all(root.join(".git")).expect("git dir");
        let source_path = root.join("source.rs");
        fs::write(
            &source_path,
            "pub fn first() {\n    let alpha = 1;\n}\n\n\
             pub fn second() {\n    let beta = 2;\n}\n",
        )
        .expect("source");
        let db_path = temp.path().join("index.sqlite");
        init_index(IndexInitOptions {
            root: root.clone(),
            db_path: Some(db_path.clone()),
            ..IndexInitOptions::new(&root)
        })
        .await
        .expect("initial index");

        let sqlite = SqliteConfig::from_sqlite_home_with_nfs_enforcement(
            AbsolutePathBuf::from_absolute_path(temp.path()).expect("absolute temp path"),
            /*enforce_delete_on_nfs*/ false,
        );
        let pool = sqlite
            .open_read_only_pool(&db_path, Some(Duration::from_secs(5)))
            .await
            .expect("open db");
        let first_id: i64 = sqlx::query_scalar(
            "SELECT id FROM chunks WHERE path = 'source.rs' AND symbol = 'first'",
        )
        .fetch_one(&pool)
        .await
        .expect("first chunk id");
        let second_id: i64 = sqlx::query_scalar(
            "SELECT id FROM chunks WHERE path = 'source.rs' AND symbol = 'second'",
        )
        .fetch_one(&pool)
        .await
        .expect("second chunk id");

        fs::write(
            &source_path,
            "pub fn first() {\n    let omega = 2;\n}\n\n\
             pub fn second() {\n    let beta = 2;\n}\n",
        )
        .expect("modify first function");
        let report = update_paths(
            IndexUpdateOptions {
                root,
                db_path: Some(db_path),
                ..IndexUpdateOptions::new(".")
            },
            &[source_path],
        )
        .await
        .expect("update changed file");

        assert_eq!(report.files_indexed, 1);
        assert_eq!(report.chunks_written, 1);
        let unchanged_id: i64 = sqlx::query_scalar(
            "SELECT id FROM chunks WHERE path = 'source.rs' AND symbol = 'second'",
        )
        .fetch_one(&pool)
        .await
        .expect("unchanged chunk id");
        let old_changed_count: i64 = sqlx::query_scalar("SELECT count(*) FROM chunks WHERE id = ?")
            .bind(first_id)
            .fetch_one(&pool)
            .await
            .expect("old changed chunk");
        assert_eq!(unchanged_id, second_id);
        assert_eq!(old_changed_count, 0);
    }

    #[tokio::test]
    async fn changing_an_ignore_file_reconciles_previously_indexed_files() {
        let temp = tempdir().expect("tempdir");
        let root = temp.path().join("repo");
        fs::create_dir_all(root.join(".git")).expect("git dir");
        fs::write(root.join(".gitignore"), "").expect("gitignore");
        fs::write(root.join("tracked.rs"), "pub fn tracked() {}\n").expect("tracked");
        fs::write(root.join("ignored.rs"), "pub fn ignored() {}\n").expect("ignored");
        let db_path = temp.path().join("index.sqlite");
        init_index(IndexInitOptions {
            root: root.clone(),
            db_path: Some(db_path.clone()),
            ..IndexInitOptions::new(&root)
        })
        .await
        .expect("initial index");

        fs::write(root.join(".gitignore"), "ignored.rs\n").expect("change ignore");
        let report = update_paths(
            IndexUpdateOptions {
                root: root.clone(),
                db_path: Some(db_path.clone()),
                ..IndexUpdateOptions::new(&root)
            },
            &[root.join(".gitignore")],
        )
        .await
        .expect("reconcile ignore");
        assert_eq!(report.files_deleted, 1);

        let status = index_status(IndexStatusOptions {
            root,
            db_path: Some(db_path),
        })
        .await
        .expect("status");
        assert_eq!(status.file_count, 1);
    }

    #[tokio::test]
    async fn writer_lock_serializes_updates_without_blocking_read_only_status() {
        let temp = tempdir().expect("tempdir");
        let root = temp.path().join("repo");
        fs::create_dir_all(root.join(".git")).expect("git dir");
        fs::write(root.join("source.rs"), "pub fn first() {}\n").expect("source");
        let db_path = temp.path().join("index.sqlite");
        init_index(IndexInitOptions {
            root: root.clone(),
            db_path: Some(db_path.clone()),
            ..IndexInitOptions::new(&root)
        })
        .await
        .expect("initial index");

        let lock = IndexWriterLock::acquire(&db_path)
            .await
            .expect("writer lock");
        let update_root = root.clone();
        let update_db_path = db_path.clone();
        let update = tokio::spawn(async move {
            update_index(IndexUpdateOptions {
                root: update_root,
                db_path: Some(update_db_path),
                ..IndexUpdateOptions::new(".")
            })
            .await
        });
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(
            !update.is_finished(),
            "second writer must wait for the lock"
        );
        let status = index_status(IndexStatusOptions {
            root: root.clone(),
            db_path: Some(db_path.clone()),
        })
        .await
        .expect("read while writer lock is held");
        assert_eq!(status.file_count, 1);

        drop(lock);
        tokio::time::timeout(Duration::from_secs(5), update)
            .await
            .expect("wait for queued writer")
            .expect("writer task")
            .expect("queued update");
    }

    #[test]
    fn chunk_hash_is_stable() {
        let chunk = crate::chunker::CodeChunk {
            symbol: None,
            parent_symbol: None,
            kind: "text".to_string(),
            start_line: 1,
            end_line: 1,
            content: "hello".to_string(),
            content_hash: crate::chunker::hash_text("hello"),
        };
        assert_eq!(
            chunk.content_hash,
            crate::chunker::hash_text("hello"),
            "hash helper must be deterministic"
        );
    }
}
