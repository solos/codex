use std::future::Future;
use std::num::NonZeroUsize;
use std::path::Path;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Instant;

use anyhow::Context;
use anyhow::Result;
use lru::LruCache;
use tokio::sync::Mutex;
use tokio::task::JoinSet;

use crate::indexer::resolve_index_paths;
use crate::store::Store;

pub const DEFAULT_EMBEDDING_CONCURRENCY: usize = 4;
const MAX_EMBEDDING_CONCURRENCY: usize = 16;
const EMBEDDING_WRITE_BATCH_SIZE: usize = 32;

/// Identifies one embedding namespace for a chunk's content.
///
/// The content hash makes cache entries reusable across file moves and
/// unchanged files. Provider and model are part of the key so switching either
/// cannot return an incompatible vector.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct EmbeddingCacheKey {
    pub content_hash: String,
    pub provider: String,
    pub model: String,
}

impl EmbeddingCacheKey {
    pub fn new(
        content_hash: impl Into<String>,
        provider: impl Into<String>,
        model: impl Into<String>,
    ) -> Self {
        Self {
            content_hash: content_hash.into(),
            provider: provider.into(),
            model: model.into(),
        }
    }
}

pub type EmbeddingFuture<'a> = Pin<Box<dyn Future<Output = Result<Vec<f32>>> + Send + 'a>>;

/// Optional embedding backend used by hybrid search.
///
/// The index itself remains fully functional without a provider. When a
/// provider is supplied, search uses the in-memory query-embedding cache and
/// compares the query vector with chunk vectors already stored in the
/// persistent embedding cache.
pub trait EmbeddingProvider: Send + Sync {
    fn provider_name(&self) -> &str;

    fn model_name(&self) -> &str;

    fn embed<'a>(&'a self, text: &'a str) -> EmbeddingFuture<'a>;
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct QueryEmbeddingCacheKey {
    query: String,
    provider: String,
    model: String,
}

pub(crate) struct QueryEmbeddingCache {
    inner: Mutex<Option<LruCache<QueryEmbeddingCacheKey, Arc<Vec<f32>>>>>,
}

impl QueryEmbeddingCache {
    pub(crate) fn new(capacity: usize) -> Self {
        Self {
            inner: Mutex::new(NonZeroUsize::new(capacity).map(LruCache::new)),
        }
    }

    pub(crate) async fn get_or_embed(
        &self,
        provider: &dyn EmbeddingProvider,
        query: &str,
    ) -> Result<Arc<Vec<f32>>> {
        let provider_name = provider.provider_name();
        let model_name = provider.model_name();
        validate_namespace(provider_name, model_name)?;
        anyhow::ensure!(!query.trim().is_empty(), "query must not be empty");

        let key = QueryEmbeddingCacheKey {
            query: query.to_string(),
            provider: provider_name.to_string(),
            model: model_name.to_string(),
        };
        if let Some(cached) = self.cached(&key).await {
            return Ok(cached);
        }

        let embedding = provider.embed(query).await?;
        validate_embedding(&embedding)?;
        let embedding = Arc::new(embedding);
        let mut guard = self.inner.lock().await;
        if let Some(cache) = guard.as_mut() {
            cache.put(key, Arc::clone(&embedding));
        }
        Ok(embedding)
    }

    async fn cached(&self, key: &QueryEmbeddingCacheKey) -> Option<Arc<Vec<f32>>> {
        let mut guard = self.inner.lock().await;
        guard.as_mut().and_then(|cache| cache.get(key).cloned())
    }
}

/// Options for materializing chunk embeddings in the persistent cache.
#[derive(Debug, Clone)]
pub struct EmbeddingBackfillOptions {
    pub root: PathBuf,
    pub db_path: Option<PathBuf>,
    pub concurrency: usize,
}

impl EmbeddingBackfillOptions {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            db_path: None,
            concurrency: DEFAULT_EMBEDDING_CONCURRENCY,
        }
    }
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct EmbeddingBackfillReport {
    pub root_path: String,
    pub db_path: String,
    pub provider: String,
    pub model: String,
    pub chunks_missing: u64,
    pub embeddings_created: u64,
    pub dimensions: Option<usize>,
    pub elapsed_ms: u64,
}

/// Embed every distinct chunk content hash missing from the persistent cache.
///
/// The query runs once up front, so repeated content and reruns reuse existing
/// cache entries. Provider calls are bounded by `concurrency`, and completed
/// embeddings are committed in small SQLite batches.
pub async fn backfill_embeddings(
    options: EmbeddingBackfillOptions,
    provider: Arc<dyn EmbeddingProvider>,
) -> Result<EmbeddingBackfillReport> {
    let started = Instant::now();
    let provider_name = provider.provider_name().to_string();
    let model_name = provider.model_name().to_string();
    validate_namespace(&provider_name, &model_name)?;

    let paths = resolve_index_paths(&options.root, options.db_path.as_deref())?;
    anyhow::ensure!(
        paths.db_path.exists(),
        "code index does not exist at {}; run `codex index init` first",
        paths.db_path.display()
    );
    let store = Store::open_writable(&paths.db_path).await?;
    let missing = store
        .chunks_missing_embedding(&provider_name, &model_name)
        .await?;
    let chunks_missing = u64::try_from(missing.len()).unwrap_or(u64::MAX);
    let concurrency = effective_embedding_concurrency(options.concurrency);
    let mut pending = missing.into_iter();
    let mut workers = JoinSet::new();
    let mut write_batch = Vec::with_capacity(EMBEDDING_WRITE_BATCH_SIZE);
    let mut embeddings_created = 0_u64;
    let mut dimensions = None;

    loop {
        while workers.len() < concurrency {
            let Some(chunk) = pending.next() else {
                break;
            };
            let provider = Arc::clone(&provider);
            let provider_name = provider_name.clone();
            let model_name = model_name.clone();
            workers.spawn(async move {
                let embedding = provider.embed(&chunk.content).await?;
                validate_embedding(&embedding)?;
                Ok::<_, anyhow::Error>((
                    EmbeddingCacheKey::new(chunk.content_hash, provider_name, model_name),
                    embedding,
                ))
            });
        }

        let Some(joined) = workers.join_next().await else {
            break;
        };
        let (key, embedding) =
            joined.context("embedding worker failed before returning a result")??;
        if let Some(expected) = dimensions {
            anyhow::ensure!(
                embedding.len() == expected,
                "embedding provider returned inconsistent dimensions: expected {expected}, got {}",
                embedding.len()
            );
        } else {
            dimensions = Some(embedding.len());
        }
        embeddings_created = embeddings_created.saturating_add(1);
        write_batch.push((key, embedding));
        if write_batch.len() >= EMBEDDING_WRITE_BATCH_SIZE {
            store.put_embeddings_batch(&write_batch).await?;
            write_batch.clear();
        }
    }
    store.put_embeddings_batch(&write_batch).await?;
    store.close().await;

    Ok(EmbeddingBackfillReport {
        root_path: paths.root.to_string_lossy().into_owned(),
        db_path: paths.db_path.to_string_lossy().into_owned(),
        provider: provider_name,
        model: model_name,
        chunks_missing,
        embeddings_created,
        dimensions,
        elapsed_ms: u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
    })
}

fn effective_embedding_concurrency(requested: usize) -> usize {
    let requested = if requested == 0 {
        std::thread::available_parallelism().map_or(DEFAULT_EMBEDDING_CONCURRENCY, |value| {
            value.get().min(DEFAULT_EMBEDDING_CONCURRENCY)
        })
    } else {
        requested
    };
    requested.clamp(1, MAX_EMBEDDING_CONCURRENCY)
}

fn validate_embedding(embedding: &[f32]) -> Result<()> {
    anyhow::ensure!(!embedding.is_empty(), "embedding must not be empty");
    anyhow::ensure!(
        embedding.iter().all(|value| value.is_finite()),
        "embedding contains a non-finite value"
    );
    anyhow::ensure!(
        embedding.iter().any(|value| *value != 0.0),
        "embedding must not be an all-zero vector"
    );
    Ok(())
}

fn validate_namespace(provider: &str, model: &str) -> Result<()> {
    anyhow::ensure!(
        !provider.trim().is_empty(),
        "embedding provider name must not be empty"
    );
    anyhow::ensure!(
        !model.trim().is_empty(),
        "embedding model name must not be empty"
    );
    Ok(())
}

#[derive(Debug, Clone)]
pub struct EmbeddingCacheOptions {
    pub root: PathBuf,
    pub db_path: Option<PathBuf>,
}

impl EmbeddingCacheOptions {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            db_path: None,
        }
    }
}

/// Persistent embedding cache backed by the code-index SQLite database.
///
/// Search calls an explicitly configured [`EmbeddingProvider`] at query time,
/// while chunk vectors are populated through this cache by callers that
/// backfill embeddings.
pub struct EmbeddingCache {
    store: Store,
}

impl EmbeddingCache {
    pub async fn open(options: EmbeddingCacheOptions) -> Result<Self> {
        let paths = resolve_index_paths(&options.root, options.db_path.as_deref())?;
        let store = Store::open_writable(&paths.db_path).await?;
        Ok(Self { store })
    }

    pub async fn open_read_only(options: EmbeddingCacheOptions) -> Result<Self> {
        let paths = resolve_index_paths(&options.root, options.db_path.as_deref())?;
        let store = Store::open_read_only(&paths.db_path).await?;
        Ok(Self { store })
    }

    pub async fn get(&self, key: &EmbeddingCacheKey) -> Result<Option<Vec<f32>>> {
        validate_key(key)?;
        self.store.get_embedding(key).await
    }

    pub async fn put(&self, key: &EmbeddingCacheKey, embedding: &[f32]) -> Result<()> {
        validate_key(key)?;
        validate_embedding(embedding)?;
        self.store.put_embedding(key, embedding).await
    }

    pub async fn close(self) {
        self.store.close().await;
    }

    pub fn db_path(&self) -> &Path {
        self.store.path()
    }
}

fn validate_key(key: &EmbeddingCacheKey) -> Result<()> {
    anyhow::ensure!(
        !key.content_hash.trim().is_empty(),
        "embedding cache content hash must not be empty"
    );
    validate_namespace(&key.provider, &key.model)
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::sync::atomic::AtomicUsize;
    use std::sync::atomic::Ordering;

    use pretty_assertions::assert_eq;
    use tempfile::tempdir;

    use super::*;
    use crate::indexer::IndexInitOptions;
    use crate::indexer::init_index;

    struct CountingProvider {
        calls: AtomicUsize,
    }

    impl EmbeddingProvider for CountingProvider {
        fn provider_name(&self) -> &str {
            "counting"
        }

        fn model_name(&self) -> &str {
            "test-model"
        }

        fn embed<'a>(&'a self, text: &'a str) -> EmbeddingFuture<'a> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Box::pin(async move { Ok(vec![1.0, text.len() as f32]) })
        }
    }

    struct ConcurrencyTrackingProvider {
        calls: AtomicUsize,
        active: AtomicUsize,
        max_active: AtomicUsize,
    }

    impl EmbeddingProvider for ConcurrencyTrackingProvider {
        fn provider_name(&self) -> &str {
            "concurrency"
        }

        fn model_name(&self) -> &str {
            "test-model"
        }

        fn embed<'a>(&'a self, _text: &'a str) -> EmbeddingFuture<'a> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            let active = &self.active;
            let max_active = &self.max_active;
            Box::pin(async move {
                let current = active.fetch_add(1, Ordering::SeqCst) + 1;
                max_active.fetch_max(current, Ordering::SeqCst);
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                active.fetch_sub(1, Ordering::SeqCst);
                Ok(vec![1.0, 0.0])
            })
        }
    }

    #[tokio::test]
    async fn embedding_cache_is_keyed_by_content_provider_and_model() {
        let temp = tempdir().expect("tempdir");
        let root = temp.path().join("repo");
        fs::create_dir_all(root.join(".git")).expect("git dir");
        let db_path = temp.path().join("index.sqlite");
        let options = EmbeddingCacheOptions {
            root: root.clone(),
            db_path: Some(db_path.clone()),
        };
        let cache = EmbeddingCache::open(options.clone()).await.expect("cache");
        let key = EmbeddingCacheKey::new("chunk-hash", "local", "model-a");
        assert_eq!(cache.get(&key).await.expect("empty cache"), None);

        cache
            .put(&key, &[0.25, -1.5, 3.0])
            .await
            .expect("put embedding");
        assert_eq!(
            cache.get(&key).await.expect("cached embedding"),
            Some(vec![0.25, -1.5, 3.0])
        );
        assert_eq!(
            cache
                .get(&EmbeddingCacheKey::new("chunk-hash", "local", "model-b"))
                .await
                .expect("different model"),
            None
        );
        cache.close().await;

        let cache = EmbeddingCache::open_read_only(options)
            .await
            .expect("reopen cache");
        assert_eq!(
            cache.get(&key).await.expect("persisted embedding"),
            Some(vec![0.25, -1.5, 3.0])
        );
        cache.close().await;
    }

    #[tokio::test]
    async fn query_embedding_cache_evicts_least_recently_used_entries() {
        let provider = CountingProvider {
            calls: AtomicUsize::new(0),
        };
        let cache = QueryEmbeddingCache::new(1);
        cache
            .get_or_embed(&provider, "first")
            .await
            .expect("first embedding");
        cache
            .get_or_embed(&provider, "second")
            .await
            .expect("second embedding");
        cache
            .get_or_embed(&provider, "first")
            .await
            .expect("evicted first embedding");
        assert_eq!(provider.calls.load(Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn embedding_backfill_reuses_content_hash_cache_across_duplicates_and_reruns() {
        let temp = tempdir().expect("tempdir");
        let root = temp.path().join("repo");
        fs::create_dir_all(root.join(".git")).expect("git dir");
        let source = "pub fn duplicate_chunk() -> usize { 1 }\n";
        fs::write(root.join("one.rs"), source).expect("one");
        fs::write(root.join("two.rs"), source).expect("two");
        let db_path = temp.path().join("index.sqlite");
        init_index(IndexInitOptions {
            root: root.clone(),
            db_path: Some(db_path.clone()),
            ..IndexInitOptions::new(&root)
        })
        .await
        .expect("index");

        let provider = Arc::new(CountingProvider {
            calls: AtomicUsize::new(0),
        });
        let options = EmbeddingBackfillOptions {
            root: root.clone(),
            db_path: Some(db_path.clone()),
            concurrency: 2,
        };
        let first = backfill_embeddings(options.clone(), provider.clone())
            .await
            .expect("first backfill");
        assert_eq!(first.chunks_missing, 1);
        assert_eq!(first.embeddings_created, 1);
        assert_eq!(first.dimensions, Some(2));
        assert_eq!(provider.calls.load(Ordering::SeqCst), 1);

        let second = backfill_embeddings(options, provider.clone())
            .await
            .expect("second backfill");
        assert_eq!(second.chunks_missing, 0);
        assert_eq!(second.embeddings_created, 0);
        assert_eq!(provider.calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn embedding_backfill_bounds_provider_concurrency() {
        let temp = tempdir().expect("tempdir");
        let root = temp.path().join("repo");
        fs::create_dir_all(root.join(".git")).expect("git dir");
        for index in 0..9 {
            fs::write(
                root.join(format!("source_{index}.rs")),
                format!("pub fn embedding_{index}() -> usize {{ {index} }}\n"),
            )
            .expect("source");
        }
        let db_path = temp.path().join("index.sqlite");
        init_index(IndexInitOptions {
            root: root.clone(),
            db_path: Some(db_path.clone()),
            ..IndexInitOptions::new(&root)
        })
        .await
        .expect("index");

        let provider = Arc::new(ConcurrencyTrackingProvider {
            calls: AtomicUsize::new(0),
            active: AtomicUsize::new(0),
            max_active: AtomicUsize::new(0),
        });
        let report = backfill_embeddings(
            EmbeddingBackfillOptions {
                root,
                db_path: Some(db_path),
                concurrency: 3,
            },
            provider.clone(),
        )
        .await
        .expect("backfill");

        assert_eq!(report.chunks_missing, 9);
        assert_eq!(report.embeddings_created, 9);
        assert_eq!(provider.calls.load(Ordering::SeqCst), 9);
        assert!(provider.max_active.load(Ordering::SeqCst) <= 3);
        assert!(
            provider.max_active.load(Ordering::SeqCst) > 1,
            "backfill should execute provider calls concurrently"
        );
    }
}
