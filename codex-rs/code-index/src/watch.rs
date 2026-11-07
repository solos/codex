use std::collections::BTreeSet;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use codex_file_watcher::DebouncedWatchReceiver;
use codex_file_watcher::FileWatcher;
use codex_file_watcher::FileWatcherSubscriber;
use codex_file_watcher::WatchPath;
use codex_file_watcher::WatchRegistration;

use crate::chunker::DEFAULT_CHUNK_MAX_TOKENS;
use crate::chunker::DEFAULT_CHUNK_OVERLAP_LINES;
use crate::discovery::DEFAULT_MAX_FILE_BYTES;
use crate::indexer::DEFAULT_PARSE_CONCURRENCY;
use crate::indexer::IndexInitOptions;
use crate::indexer::IndexReport;
use crate::indexer::IndexUpdateOptions;
use crate::indexer::index_artifact_paths;
use crate::indexer::init_index;
use crate::indexer::resolve_index_paths;
use crate::indexer::update_paths;

pub const DEFAULT_WATCH_DEBOUNCE: Duration = Duration::from_millis(200);
const RETRY_DELAY: Duration = Duration::from_secs(1);

#[derive(Debug, Clone)]
pub struct IndexWatchOptions {
    pub root: PathBuf,
    pub db_path: Option<PathBuf>,
    pub debounce: Duration,
    pub max_file_bytes: u64,
    pub chunk_max_tokens: usize,
    pub chunk_overlap_lines: usize,
    pub parse_concurrency: usize,
}

impl IndexWatchOptions {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            db_path: None,
            debounce: DEFAULT_WATCH_DEBOUNCE,
            max_file_bytes: DEFAULT_MAX_FILE_BYTES,
            chunk_max_tokens: DEFAULT_CHUNK_MAX_TOKENS,
            chunk_overlap_lines: DEFAULT_CHUNK_OVERLAP_LINES,
            parse_concurrency: DEFAULT_PARSE_CONCURRENCY,
        }
    }

    fn init_options(&self) -> IndexInitOptions {
        IndexInitOptions {
            root: self.root.clone(),
            db_path: self.db_path.clone(),
            max_file_bytes: self.max_file_bytes,
            chunk_max_tokens: self.chunk_max_tokens,
            chunk_overlap_lines: self.chunk_overlap_lines,
            parse_concurrency: self.parse_concurrency,
        }
    }

    fn update_options(&self) -> IndexUpdateOptions {
        IndexUpdateOptions {
            root: self.root.clone(),
            db_path: self.db_path.clone(),
            max_file_bytes: self.max_file_bytes,
            chunk_max_tokens: self.chunk_max_tokens,
            chunk_overlap_lines: self.chunk_overlap_lines,
            parse_concurrency: self.parse_concurrency,
        }
    }
}

/// A debounced, single-process code-index watcher.
///
/// The watcher is registered before the initial full refresh, so changes made
/// while indexing begins are retained in the event queue. Updates run
/// sequentially and failed path batches are retried by the next call to
/// [`Self::next_update`].
pub struct IndexWatcher {
    options: IndexWatchOptions,
    receiver: DebouncedWatchReceiver,
    pending_paths: BTreeSet<PathBuf>,
    artifact_paths: BTreeSet<PathBuf>,
    _subscriber: FileWatcherSubscriber,
    _registration: WatchRegistration,
}

impl IndexWatcher {
    pub async fn start(
        options: IndexWatchOptions,
        file_watcher: Arc<FileWatcher>,
    ) -> Result<(Self, IndexReport)> {
        Self::start_inner(options, file_watcher, /*require_registration*/ true).await
    }

    async fn start_inner(
        options: IndexWatchOptions,
        file_watcher: Arc<FileWatcher>,
        require_registration: bool,
    ) -> Result<(Self, IndexReport)> {
        let paths = resolve_index_paths(&options.root, options.db_path.as_deref())?;
        let artifact_paths = index_artifact_paths(&paths.db_path).into_iter().collect();
        let (subscriber, receiver) = file_watcher.add_subscriber();
        let registration = subscriber.register_paths(vec![WatchPath {
            path: paths.root,
            recursive: true,
        }]);
        if require_registration && !registration.registration_succeeded() {
            anyhow::bail!("failed to register the code index filesystem watch");
        }
        let initial = init_index(options.init_options()).await?;
        Ok((
            Self {
                receiver: DebouncedWatchReceiver::new(receiver, options.debounce),
                options,
                pending_paths: BTreeSet::new(),
                artifact_paths,
                _subscriber: subscriber,
                _registration: registration,
            },
            initial,
        ))
    }

    /// Wait for the next debounced batch and update only affected paths.
    ///
    /// A failed update returns an error and retains the batch. The next call
    /// waits briefly and retries it before consuming newer filesystem events.
    pub async fn next_update(&mut self) -> Option<Result<IndexReport>> {
        let changed_paths = if self.pending_paths.is_empty() {
            loop {
                let paths = self
                    .receiver
                    .recv()
                    .await?
                    .paths
                    .into_iter()
                    .filter(|path| !self.artifact_paths.contains(path))
                    .collect::<Vec<_>>();
                if !paths.is_empty() {
                    break paths;
                }
            }
        } else {
            tokio::time::sleep(RETRY_DELAY).await;
            std::mem::take(&mut self.pending_paths)
                .into_iter()
                .collect()
        };

        match update_paths(self.options.update_options(), &changed_paths).await {
            Ok(report) => Some(Ok(report)),
            Err(error) => {
                self.pending_paths.extend(changed_paths);
                Some(Err(error))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use pretty_assertions::assert_eq;
    use tempfile::tempdir;
    use tokio::time::timeout;

    use super::*;
    use crate::indexer::IndexStatusOptions;
    use crate::indexer::index_status;

    fn watch_options(root: &std::path::Path, db_path: PathBuf) -> IndexWatchOptions {
        IndexWatchOptions {
            root: root.to_path_buf(),
            db_path: Some(db_path),
            debounce: Duration::from_millis(10),
            ..IndexWatchOptions::new(root)
        }
    }

    #[tokio::test]
    async fn synthetic_watch_events_update_create_rename_and_delete() {
        let temp = tempdir().expect("tempdir");
        let root = temp.path().join("repo");
        fs::create_dir_all(root.join(".git")).expect("git dir");
        fs::create_dir_all(root.join("src")).expect("src dir");
        fs::write(root.join("src/keep.rs"), "pub fn keep() {}\n").expect("keep");
        fs::write(root.join("src/remove.rs"), "pub fn remove() {}\n").expect("remove");
        let db_path = temp.path().join("index.sqlite");
        let file_watcher = Arc::new(FileWatcher::noop());
        let (mut watcher, initial) = IndexWatcher::start_inner(
            watch_options(&root, db_path.clone()),
            file_watcher.clone(),
            /*require_registration*/ false,
        )
        .await
        .expect("start watcher");
        assert_eq!(initial.files_indexed, 2);

        let created = root.join("src/created.rs");
        fs::write(&created, "pub fn created() {}\n").expect("create");
        file_watcher
            .send_paths_for_test(vec![created.clone()])
            .await;
        let report = timeout(Duration::from_secs(1), watcher.next_update())
            .await
            .expect("create timeout")
            .expect("receiver open")
            .expect("create update");
        assert_eq!(report.mode, crate::indexer::IndexMode::Incremental);
        assert_eq!(report.files_indexed, 1);

        fs::remove_file(root.join("src/remove.rs")).expect("remove");
        file_watcher
            .send_paths_for_test(vec![root.join("src/remove.rs")])
            .await;
        let report = timeout(Duration::from_secs(1), watcher.next_update())
            .await
            .expect("delete timeout")
            .expect("receiver open")
            .expect("delete update");
        assert_eq!(report.files_deleted, 1);

        let renamed = root.join("src/renamed.rs");
        fs::rename(&created, &renamed).expect("rename");
        file_watcher
            .send_paths_for_test(vec![created, renamed])
            .await;
        let report = timeout(Duration::from_secs(1), watcher.next_update())
            .await
            .expect("rename timeout")
            .expect("receiver open")
            .expect("rename update");
        assert_eq!(report.files_indexed, 1);
        assert_eq!(report.files_deleted, 1);

        let status = index_status(IndexStatusOptions {
            root,
            db_path: Some(db_path),
        })
        .await
        .expect("status");
        assert_eq!(status.file_count, 2);
    }

    #[tokio::test]
    async fn failed_watch_update_is_retained_and_retried() {
        let temp = tempdir().expect("tempdir");
        let root = temp.path().join("repo");
        fs::create_dir_all(root.join(".git")).expect("git dir");
        let source = root.join("source.rs");
        fs::write(&source, "pub fn first() {}\n").expect("initial source");
        let db_path = temp.path().join("index.sqlite");
        let file_watcher = Arc::new(FileWatcher::noop());
        let (mut watcher, initial) = IndexWatcher::start_inner(
            watch_options(&root, db_path.clone()),
            file_watcher.clone(),
            /*require_registration*/ false,
        )
        .await
        .expect("start watcher");
        assert_eq!(initial.files_indexed, 1);

        let backup_path = temp.path().join("index.backup.sqlite");
        fs::rename(&db_path, &backup_path).expect("move database");
        fs::create_dir(&db_path).expect("replace database with directory");
        fs::write(&source, "pub fn retried() {}\n").expect("modify source");
        file_watcher.send_paths_for_test(vec![source]).await;
        let failure = timeout(Duration::from_secs(1), watcher.next_update())
            .await
            .expect("failure timeout")
            .expect("receiver open")
            .expect_err("update should fail while the database path is a directory");
        assert!(failure.to_string().contains("failed to open code index"));

        fs::remove_dir(&db_path).expect("remove database directory");
        fs::rename(&backup_path, &db_path).expect("restore database");
        let report = timeout(Duration::from_secs(3), watcher.next_update())
            .await
            .expect("retry timeout")
            .expect("receiver open")
            .expect("retried update");
        assert_eq!(report.files_indexed, 1);
        assert_eq!(report.chunks_written, 1);
    }

    #[tokio::test]
    async fn live_watch_observes_a_source_change() {
        let temp = tempdir().expect("tempdir");
        let root = temp.path().join("repo");
        fs::create_dir_all(root.join(".git")).expect("git dir");
        let source = root.join("live.rs");
        fs::write(&source, "pub fn first() {}\n").expect("initial source");
        let db_path = temp.path().join("index.sqlite");
        let file_watcher = Arc::new(FileWatcher::new().expect("live watcher"));
        let (mut watcher, initial) =
            IndexWatcher::start(watch_options(&root, db_path), file_watcher)
                .await
                .expect("start watcher");
        assert_eq!(initial.files_indexed, 1);

        fs::write(&source, "pub fn second() {}\n").expect("modify source");
        let report = timeout(Duration::from_secs(10), watcher.next_update())
            .await
            .expect("live watch timeout")
            .expect("receiver open")
            .expect("live update");
        assert_eq!(report.files_indexed, 1);
    }
}
