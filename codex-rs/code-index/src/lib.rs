mod chunker;
mod cli;
mod discovery;
mod embedding;
mod indexer;
mod language;
mod search;
mod store;
mod watch;

pub use cli::IndexCommand;
pub use cli::IndexInitArgs;
pub use cli::IndexSearchArgs;
pub use cli::IndexStatusArgs;
pub use cli::IndexSubcommand;
pub use cli::IndexUpdateArgs;
pub use cli::IndexWatchArgs;
pub use cli::run;
pub use cli::run_with_codex_exe;
pub use embedding::DEFAULT_EMBEDDING_CONCURRENCY;
pub use embedding::EmbeddingBackfillOptions;
pub use embedding::EmbeddingBackfillReport;
pub use embedding::EmbeddingCache;
pub use embedding::EmbeddingCacheKey;
pub use embedding::EmbeddingCacheOptions;
pub use embedding::EmbeddingFuture;
pub use embedding::EmbeddingProvider;
pub use embedding::backfill_embeddings;
pub use indexer::DEFAULT_PARSE_CONCURRENCY;
pub use indexer::IndexControl;
pub use indexer::IndexInitOptions;
pub use indexer::IndexMode;
pub use indexer::IndexProgress;
pub use indexer::IndexReport;
pub use indexer::IndexStatus;
pub use indexer::IndexStatusOptions;
pub use indexer::IndexUpdateOptions;
pub use indexer::index_status;
pub use indexer::init_index;
pub use indexer::init_index_with_control;
pub use indexer::update_index;
pub use indexer::update_index_with_control;
pub use indexer::update_paths;
pub use indexer::update_paths_with_control;
pub use search::DEFAULT_QUERY_CACHE_CAPACITY;
pub use search::DEFAULT_SEARCH_RESULT_CACHE_CAPACITY;
pub use search::DEFAULT_SEARCH_RESULT_LIMIT;
pub use search::DEFAULT_SEARCH_TOKEN_BUDGET;
pub use search::IndexSearchOptions;
pub use search::QueryAnalysis;
pub use search::QueryIntent;
pub use search::SearchEngine;
pub use search::SearchEngineOptions;
pub use search::SearchRequest;
pub use search::SearchResponse;
pub use search::SearchResult;
pub use search::analyze_query;
pub use search::search_index;
pub use search::search_index_with_embedding_provider;
pub use watch::DEFAULT_WATCH_DEBOUNCE;
pub use watch::IndexWatchOptions;
pub use watch::IndexWatcher;

/// Discover files that are eligible for indexing under `root`.
///
/// This applies the same ignore, hidden-file, sensitive-file, binary-size, and
/// path rules used by the indexer. It is intended for privacy-preserving
/// fallback search when an index is not available yet.
pub fn discover_indexable_files(root: &std::path::Path) -> anyhow::Result<Vec<std::path::PathBuf>> {
    let root = root.canonicalize().map_err(|error| {
        anyhow::anyhow!(
            "failed to resolve fallback search root {}: {error}",
            root.display()
        )
    })?;
    let discovery =
        discovery::discover_files_excluding(&root, discovery::DEFAULT_MAX_FILE_BYTES, &[])?;
    Ok(discovery
        .files
        .into_iter()
        .map(|file| file.absolute_path)
        .collect())
}
