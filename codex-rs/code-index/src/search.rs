use std::collections::BTreeMap;
use std::collections::HashMap;
use std::num::NonZeroUsize;
use std::path::Path;
use std::path::PathBuf;
use std::time::Instant;

use anyhow::Context;
use anyhow::Result;
use codex_utils_string::approx_bytes_for_tokens;
use codex_utils_string::approx_token_count;
use codex_utils_string::take_bytes_at_char_boundary;
use lru::LruCache;
use serde::Serialize;
use tokio::sync::Mutex;

use crate::embedding::EmbeddingProvider;
use crate::embedding::QueryEmbeddingCache;
use crate::indexer::resolve_index_paths;
use crate::store::ChunkRecord;
use crate::store::Store;

pub const DEFAULT_SEARCH_TOKEN_BUDGET: usize = 4_000;
pub const DEFAULT_SEARCH_RESULT_LIMIT: usize = 20;
pub const DEFAULT_QUERY_CACHE_CAPACITY: usize = 128;
pub const DEFAULT_SEARCH_RESULT_CACHE_CAPACITY: usize = 32;

const RRF_K: f64 = 60.0;
// Weighted reciprocal-rank fusion favors exact symbols and semantic matches
// without letting any one backend dominate a chunk that all backends agree on.
const FTS_WEIGHT: f64 = 1.0;
const TRIGRAM_WEIGHT: f64 = 0.8;
const SYMBOL_WEIGHT: f64 = 1.3;
const VECTOR_WEIGHT: f64 = 1.3;
const CONTEXT_SCORE_FACTOR: f64 = 0.2;
const MAX_CONTEXT_ANCHORS: usize = 8;
const CONTEXT_LINE_WINDOW: u64 = 3;
const CONTEXT_CHUNKS_PER_ANCHOR: usize = 2;
const MAX_QUERY_TERMS: usize = 64;
const MAX_SYMBOL_TERMS: usize = 48;
const MAX_PATH_TERMS: usize = 32;
const MAX_TERM_BYTES: usize = 256;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum QueryIntent {
    Symbol,
    Path,
    NaturalLanguage,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueryAnalysis {
    pub original: String,
    pub normalized: String,
    pub terms: Vec<String>,
    pub symbol_terms: Vec<String>,
    pub path_terms: Vec<String>,
    pub intent: QueryIntent,
}

#[derive(Debug, Clone)]
pub struct IndexSearchOptions {
    pub root: PathBuf,
    pub db_path: Option<PathBuf>,
    pub query: String,
    pub token_budget: usize,
    pub max_results: usize,
    pub query_cache_capacity: usize,
}

impl IndexSearchOptions {
    pub fn new(root: impl Into<PathBuf>, query: impl Into<String>) -> Self {
        Self {
            root: root.into(),
            db_path: None,
            query: query.into(),
            token_budget: DEFAULT_SEARCH_TOKEN_BUDGET,
            max_results: DEFAULT_SEARCH_RESULT_LIMIT,
            query_cache_capacity: DEFAULT_QUERY_CACHE_CAPACITY,
        }
    }
}

#[derive(Debug, Clone)]
pub struct SearchEngineOptions {
    pub root: PathBuf,
    pub db_path: Option<PathBuf>,
    pub query_cache_capacity: usize,
    pub result_cache_capacity: usize,
}

impl SearchEngineOptions {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            db_path: None,
            query_cache_capacity: DEFAULT_QUERY_CACHE_CAPACITY,
            result_cache_capacity: DEFAULT_SEARCH_RESULT_CACHE_CAPACITY,
        }
    }
}

#[derive(Debug, Clone)]
pub struct SearchRequest {
    pub query: String,
    pub token_budget: usize,
    pub max_results: usize,
}

impl SearchRequest {
    pub fn new(query: impl Into<String>) -> Self {
        Self {
            query: query.into(),
            token_budget: DEFAULT_SEARCH_TOKEN_BUDGET,
            max_results: DEFAULT_SEARCH_RESULT_LIMIT,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct SearchResult {
    pub path: String,
    pub line: u64,
    pub end_line: u64,
    pub symbol: Option<String>,
    pub parent_symbol: Option<String>,
    pub kind: String,
    pub language: String,
    pub score: f64,
    pub content: String,
    pub truncated: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct SearchResponse {
    pub query: String,
    pub intent: QueryIntent,
    pub results: Vec<SearchResult>,
    pub total_tokens: usize,
    pub truncated: bool,
    pub elapsed_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct QueryResultCacheKey {
    data_version: i64,
    query: String,
    token_budget: usize,
    max_results: usize,
    embedding_namespace: Option<(String, String)>,
}

struct QueryResultCache {
    inner: Mutex<Option<LruCache<QueryResultCacheKey, SearchResponse>>>,
}

impl QueryResultCache {
    fn new(capacity: usize) -> Self {
        Self {
            inner: Mutex::new(NonZeroUsize::new(capacity).map(LruCache::new)),
        }
    }

    async fn get(&self, key: &QueryResultCacheKey) -> Option<SearchResponse> {
        let mut guard = self.inner.lock().await;
        guard.as_mut().and_then(|cache| cache.get(key).cloned())
    }

    async fn put(&self, key: QueryResultCacheKey, response: &SearchResponse) {
        let mut guard = self.inner.lock().await;
        if let Some(cache) = guard.as_mut() {
            cache.put(key, response.clone());
        }
    }
}

pub struct SearchEngine {
    // The shared read-only helper opens a single-connection pool. Each
    // lexical and vector backend gets its own connection so `try_join!`
    // executes the backend queries concurrently instead of queueing them.
    fts_store: Store,
    trigram_store: Store,
    symbol_store: Store,
    vector_store: Store,
    query_embeddings: QueryEmbeddingCache,
    query_results: QueryResultCache,
}

impl SearchEngine {
    pub async fn open(options: SearchEngineOptions) -> Result<Self> {
        let paths = resolve_index_paths(&options.root, options.db_path.as_deref())?;
        anyhow::ensure!(
            paths.db_path.exists(),
            "code index does not exist at {}; run `codex index init` first",
            paths.db_path.display()
        );
        let fts_store = open_read_store(&paths.db_path, "FTS").await?;
        let trigram_store = open_read_store(&paths.db_path, "trigram").await?;
        let symbol_store = open_read_store(&paths.db_path, "symbol").await?;
        let vector_store = open_read_store(&paths.db_path, "vector").await?;
        Ok(Self {
            fts_store,
            trigram_store,
            symbol_store,
            vector_store,
            query_embeddings: QueryEmbeddingCache::new(options.query_cache_capacity),
            query_results: QueryResultCache::new(options.result_cache_capacity),
        })
    }

    pub async fn search(&self, request: SearchRequest) -> Result<SearchResponse> {
        self.search_inner(request, None).await
    }

    pub async fn search_with_embedding_provider(
        &self,
        request: SearchRequest,
        provider: &dyn EmbeddingProvider,
    ) -> Result<SearchResponse> {
        self.search_inner(request, Some(provider)).await
    }

    pub async fn close(self) {
        self.fts_store.close().await;
        self.trigram_store.close().await;
        self.symbol_store.close().await;
        self.vector_store.close().await;
    }

    async fn search_inner(
        &self,
        request: SearchRequest,
        provider: Option<&dyn EmbeddingProvider>,
    ) -> Result<SearchResponse> {
        let started = Instant::now();
        let analysis = analyze_query(&request.query)?;
        let candidate_limit = request.max_results.saturating_mul(8).clamp(50, 500).max(1);
        if request.token_budget == 0 || request.max_results == 0 {
            return Ok(SearchResponse {
                query: analysis.original,
                intent: analysis.intent,
                results: Vec::new(),
                total_tokens: 0,
                truncated: !request.query.is_empty(),
                elapsed_ms: elapsed_ms(started),
            });
        }
        let embedding_namespace = provider.map(|provider| {
            (
                provider.provider_name().to_string(),
                provider.model_name().to_string(),
            )
        });
        let cache_key = QueryResultCacheKey {
            data_version: self.fts_store.data_version().await?,
            query: analysis.normalized.clone(),
            token_budget: request.token_budget,
            max_results: request.max_results,
            embedding_namespace,
        };
        if let Some(mut response) = self.query_results.get(&cache_key).await {
            response.elapsed_ms = elapsed_ms(started);
            return Ok(response);
        }

        let fts_query = build_fts_query(&analysis);
        let trigram_query = build_trigram_query(&analysis);
        let symbol_terms = analysis.symbol_terms.clone();
        let vector_query = analysis.normalized.clone();
        let fts = async {
            match fts_query {
                Some(query) => self.fts_store.fts_search(&query, candidate_limit).await,
                None => Ok(Vec::new()),
            }
        };
        let trigram = async {
            match trigram_query {
                Some(query) => {
                    self.trigram_store
                        .trigram_search(&query, candidate_limit)
                        .await
                }
                None => Ok(Vec::new()),
            }
        };
        let symbols = async {
            self.symbol_store
                .exact_symbol_search(&symbol_terms, candidate_limit)
                .await
        };
        let vectors = async {
            let Some(provider) = provider else {
                return Ok(Vec::new());
            };
            let provider_name = provider.provider_name().to_string();
            let model_name = provider.model_name().to_string();
            let embedding = self
                .query_embeddings
                .get_or_embed(provider, &vector_query)
                .await?;
            self.vector_store
                .vector_search(&embedding, &provider_name, &model_name, candidate_limit)
                .await
        };

        let (fts, trigram, symbols, vectors) = tokio::try_join!(fts, trigram, symbols, vectors)?;
        let mut chunks = fuse_rankings(&[
            (fts.as_slice(), FTS_WEIGHT),
            (trigram.as_slice(), TRIGRAM_WEIGHT),
            (symbols.as_slice(), SYMBOL_WEIGHT),
            (vectors.as_slice(), VECTOR_WEIGHT),
        ]);
        chunks = deduplicate_adjacent(chunks);
        self.add_nearby_context(&mut chunks, request.max_results)
            .await?;
        chunks = deduplicate_adjacent(chunks);

        let (results, total_tokens, truncated) =
            clip_to_budget(chunks, request.token_budget, request.max_results);
        let response = SearchResponse {
            query: analysis.original,
            intent: analysis.intent,
            results,
            total_tokens,
            truncated,
            elapsed_ms: elapsed_ms(started),
        };
        self.query_results.put(cache_key, &response).await;
        Ok(response)
    }

    async fn add_nearby_context(
        &self,
        chunks: &mut Vec<FusedChunk>,
        max_context: usize,
    ) -> Result<()> {
        if chunks.is_empty() || chunks.len() >= max_context {
            return Ok(());
        }
        let top_score = chunks[0].score;
        let mut known_ids = chunks
            .iter()
            .map(|chunk| chunk.chunk.id)
            .collect::<std::collections::HashSet<_>>();
        let anchors = chunks
            .iter()
            .take(MAX_CONTEXT_ANCHORS)
            .filter(|chunk| chunk.score >= top_score * CONTEXT_SCORE_FACTOR)
            .cloned()
            .collect::<Vec<_>>();
        let wanted = max_context
            .saturating_sub(chunks.len())
            .min(MAX_CONTEXT_ANCHORS * CONTEXT_CHUNKS_PER_ANCHOR);
        let mut added = 0;
        for anchor in anchors {
            let neighbors = self
                .fts_store
                .nearby_chunks(
                    &anchor.chunk,
                    CONTEXT_LINE_WINDOW,
                    CONTEXT_CHUNKS_PER_ANCHOR,
                )
                .await?;
            for neighbor in neighbors {
                if known_ids.insert(neighbor.id) {
                    chunks.push(FusedChunk {
                        chunk: neighbor,
                        score: anchor.score * CONTEXT_SCORE_FACTOR,
                    });
                    added += 1;
                }
                if added >= wanted {
                    break;
                }
            }
            if added >= wanted {
                break;
            }
        }
        Ok(())
    }
}

async fn open_read_store(db_path: &Path, backend: &str) -> Result<Store> {
    Store::open_read_only(db_path).await.with_context(|| {
        format!(
            "failed to open code index {backend} search connection {}",
            db_path.display()
        )
    })
}

pub async fn search_index(options: IndexSearchOptions) -> Result<SearchResponse> {
    search_index_with_optional_provider(options, None).await
}

pub async fn search_index_with_embedding_provider(
    options: IndexSearchOptions,
    provider: &dyn EmbeddingProvider,
) -> Result<SearchResponse> {
    search_index_with_optional_provider(options, Some(provider)).await
}

async fn search_index_with_optional_provider(
    options: IndexSearchOptions,
    provider: Option<&dyn EmbeddingProvider>,
) -> Result<SearchResponse> {
    let engine = SearchEngine::open(SearchEngineOptions {
        root: options.root,
        db_path: options.db_path,
        query_cache_capacity: options.query_cache_capacity,
        result_cache_capacity: DEFAULT_SEARCH_RESULT_CACHE_CAPACITY,
    })
    .await?;
    let response = match provider {
        Some(provider) => {
            engine
                .search_with_embedding_provider(
                    SearchRequest {
                        query: options.query,
                        token_budget: options.token_budget,
                        max_results: options.max_results,
                    },
                    provider,
                )
                .await
        }
        None => {
            engine
                .search(SearchRequest {
                    query: options.query,
                    token_budget: options.token_budget,
                    max_results: options.max_results,
                })
                .await
        }
    };
    engine.close().await;
    response
}

pub fn analyze_query(query: &str) -> Result<QueryAnalysis> {
    let original = query.trim().to_string();
    let normalized = query.split_whitespace().collect::<Vec<_>>().join(" ");
    anyhow::ensure!(!normalized.is_empty(), "search query must not be empty");

    let raw_tokens = query_tokens(&normalized);
    let mut terms = Vec::new();
    let mut symbol_terms = Vec::new();
    let mut path_terms = Vec::new();
    let mut has_symbol_shape = false;

    for token in raw_tokens {
        let token = trim_query_token(&token);
        if token.is_empty() {
            continue;
        }
        let token = take_bytes_at_char_boundary(token, MAX_TERM_BYTES);
        if looks_like_path(token) {
            push_unique_case_insensitive(&mut path_terms, token.to_string());
            push_unique_case_insensitive(&mut terms, token.to_string());
            continue;
        }

        let symbol_shaped = is_symbol_shaped(token);
        has_symbol_shape |= symbol_shaped;
        for part in symbol_parts(token) {
            if !is_stop_word(&part) {
                push_unique_case_insensitive(&mut terms, part.clone());
            }
            if part.chars().any(char::is_alphanumeric) {
                push_unique_case_insensitive(&mut symbol_terms, part);
            }
        }
        if symbol_shaped {
            push_unique_case_insensitive(&mut symbol_terms, token.to_string());
        }
    }

    if terms.is_empty() {
        terms.push(normalized.to_lowercase());
    }
    terms.truncate(MAX_QUERY_TERMS);
    symbol_terms.truncate(MAX_SYMBOL_TERMS);
    path_terms.truncate(MAX_PATH_TERMS);
    let meaningful_word_count = terms.len();
    let intent = if !path_terms.is_empty() {
        QueryIntent::Path
    } else if has_symbol_shape || meaningful_word_count == 1 {
        QueryIntent::Symbol
    } else {
        QueryIntent::NaturalLanguage
    };

    Ok(QueryAnalysis {
        original,
        normalized,
        terms,
        symbol_terms,
        path_terms,
        intent,
    })
}

fn query_tokens(query: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    for character in query.chars() {
        if character.is_alphanumeric()
            || matches!(character, '_' | '-' | '/' | '\\' | '.' | ':' | '@')
        {
            current.push(character);
        } else if !current.is_empty() {
            tokens.push(std::mem::take(&mut current));
        }
    }
    if !current.is_empty() {
        tokens.push(current);
    }
    tokens
}

fn trim_query_token(token: &str) -> &str {
    token.trim_matches(|character: char| matches!(character, '.' | ':' | '/' | '\\' | '-' | '_'))
}

fn looks_like_path(token: &str) -> bool {
    if token.contains('/') || token.contains('\\') {
        return true;
    }
    let Some(extension) = Path::new(token)
        .extension()
        .and_then(|extension| extension.to_str())
    else {
        return false;
    };
    matches!(
        extension.to_ascii_lowercase().as_str(),
        "rs" | "ts"
            | "tsx"
            | "js"
            | "jsx"
            | "mjs"
            | "cjs"
            | "py"
            | "pyi"
            | "md"
            | "toml"
            | "json"
            | "yaml"
            | "yml"
            | "sql"
            | "sh"
            | "go"
            | "java"
            | "kt"
            | "c"
            | "h"
            | "cc"
            | "cpp"
            | "hpp"
            | "cs"
            | "rb"
            | "php"
            | "swift"
    )
}

fn is_symbol_shaped(token: &str) -> bool {
    token.contains("::")
        || token.contains('_')
        || token.chars().any(char::is_uppercase)
        || token.matches('.').count() >= 1
}

fn symbol_parts(token: &str) -> Vec<String> {
    let mut parts = vec![token.to_string()];
    for part in token.split("::").flat_map(|part| part.split('.')) {
        if !part.is_empty() {
            parts.push(part.to_string());
        }
    }
    parts.sort();
    parts.dedup();
    parts
}

fn is_stop_word(word: &str) -> bool {
    matches!(
        word.to_ascii_lowercase().as_str(),
        "a" | "an"
            | "and"
            | "are"
            | "as"
            | "at"
            | "be"
            | "by"
            | "code"
            | "do"
            | "does"
            | "find"
            | "for"
            | "from"
            | "how"
            | "in"
            | "is"
            | "it"
            | "of"
            | "on"
            | "or"
            | "please"
            | "search"
            | "the"
            | "this"
            | "to"
            | "using"
            | "we"
            | "what"
            | "where"
            | "with"
    )
}

fn push_unique_case_insensitive(values: &mut Vec<String>, value: String) {
    if !values
        .iter()
        .any(|existing| existing.eq_ignore_ascii_case(&value))
    {
        values.push(value);
    }
}

fn build_fts_query(analysis: &QueryAnalysis) -> Option<String> {
    let mut terms = analysis.terms.clone();
    for path in &analysis.path_terms {
        push_unique_case_insensitive(&mut terms, path.clone());
    }
    terms.retain(|term| term.chars().any(char::is_alphanumeric));
    if terms.is_empty() {
        return None;
    }
    Some(
        terms
            .into_iter()
            .map(|term| {
                let escaped = term.replace('"', "\"\"");
                format!("\"{escaped}\"*")
            })
            .collect::<Vec<_>>()
            .join(" OR "),
    )
}

fn build_trigram_query(analysis: &QueryAnalysis) -> Option<String> {
    let mut terms = analysis.terms.clone();
    for path in &analysis.path_terms {
        push_unique_case_insensitive(&mut terms, path.clone());
    }
    terms.retain(|term| term.chars().count() >= 3);
    if terms.is_empty() {
        return None;
    }
    Some(
        terms
            .into_iter()
            .map(|term| {
                let escaped = term.replace('"', "\"\"");
                format!("\"{escaped}\"")
            })
            .collect::<Vec<_>>()
            .join(" OR "),
    )
}

#[derive(Debug, Clone)]
struct FusedChunk {
    chunk: ChunkRecord,
    score: f64,
}

fn fuse_rankings(sources: &[(&[ChunkRecord], f64)]) -> Vec<FusedChunk> {
    let mut fused = HashMap::<i64, FusedChunk>::new();
    for (ranking, weight) in sources {
        for (index, chunk) in ranking.iter().enumerate() {
            let contribution = weight / (RRF_K + index as f64 + 1.0);
            if let Some(existing) = fused.get_mut(&chunk.id) {
                existing.score += contribution;
            } else {
                fused.insert(
                    chunk.id,
                    FusedChunk {
                        chunk: chunk.clone(),
                        score: contribution,
                    },
                );
            }
        }
    }
    let mut fused = fused.into_values().collect::<Vec<_>>();
    sort_fused_chunks(&mut fused);
    fused
}

fn sort_fused_chunks(chunks: &mut [FusedChunk]) {
    chunks.sort_by(|left, right| {
        right
            .score
            .total_cmp(&left.score)
            .then_with(|| left.chunk.path.cmp(&right.chunk.path))
            .then_with(|| left.chunk.start_line.cmp(&right.chunk.start_line))
            .then_with(|| left.chunk.id.cmp(&right.chunk.id))
    });
}

fn deduplicate_adjacent(chunks: Vec<FusedChunk>) -> Vec<FusedChunk> {
    let mut merged = Vec::<FusedChunk>::new();
    'candidate: for candidate in chunks {
        for existing in &mut merged {
            if should_merge(&existing.chunk, &candidate.chunk) {
                merge_chunk(&mut existing.chunk, &candidate.chunk);
                existing.score = existing.score.max(candidate.score);
                continue 'candidate;
            }
        }
        merged.push(candidate);
    }
    sort_fused_chunks(&mut merged);
    merged
}

fn should_merge(left: &ChunkRecord, right: &ChunkRecord) -> bool {
    if left.path != right.path
        || left.symbol != right.symbol
        || left.parent_symbol != right.parent_symbol
    {
        return false;
    }
    let overlap = right.start_line <= left.end_line && left.start_line <= right.end_line;
    let adjacent = right.start_line <= left.end_line.saturating_add(1)
        && left.start_line <= right.end_line.saturating_add(1);
    if left.symbol.is_none() {
        overlap
    } else {
        adjacent
    }
}

fn merge_chunk(target: &mut ChunkRecord, source: &ChunkRecord) {
    let target_start = target.start_line;
    target.content = merge_content(
        &target.content,
        target_start,
        &source.content,
        source.start_line,
    );
    target.start_line = target.start_line.min(source.start_line);
    target.end_line = target.end_line.max(source.end_line);
}

fn merge_content(left: &str, left_start: u64, right: &str, right_start: u64) -> String {
    let mut lines = BTreeMap::<u64, String>::new();
    add_content_lines(&mut lines, left, left_start);
    add_content_lines(&mut lines, right, right_start);
    lines.into_values().collect()
}

fn add_content_lines(lines: &mut BTreeMap<u64, String>, content: &str, start_line: u64) {
    for (offset, line) in content.split_inclusive('\n').enumerate() {
        let line_number = start_line.saturating_add(offset as u64);
        if let Some(existing) = lines.get_mut(&line_number) {
            if existing.trim_end() != line.trim_end() {
                existing.push_str(line);
            }
        } else {
            lines.insert(line_number, line.to_string());
        }
    }
}

fn clip_to_budget(
    chunks: Vec<FusedChunk>,
    budget: usize,
    max_results: usize,
) -> (Vec<SearchResult>, usize, bool) {
    let mut results = Vec::new();
    let mut total_tokens = 0_usize;
    let mut truncated = false;
    for chunk in chunks {
        if results.len() >= max_results {
            truncated = true;
            break;
        }
        let remaining = budget.saturating_sub(total_tokens);
        if remaining == 0 {
            truncated = true;
            break;
        }
        let original_tokens = approx_token_count(&chunk.chunk.content);
        let (content, content_truncated) = if original_tokens > remaining {
            (truncate_to_tokens(&chunk.chunk.content, remaining), true)
        } else {
            (chunk.chunk.content, false)
        };
        if content.trim().is_empty() {
            truncated = true;
            break;
        }
        let content_tokens = approx_token_count(&content);
        total_tokens = total_tokens.saturating_add(content_tokens);
        truncated |= content_truncated;
        results.push(SearchResult {
            path: chunk.chunk.path,
            line: chunk.chunk.start_line,
            end_line: chunk.chunk.end_line,
            symbol: chunk.chunk.symbol,
            parent_symbol: chunk.chunk.parent_symbol,
            kind: chunk.chunk.kind,
            language: chunk.chunk.language,
            score: chunk.score,
            content,
            truncated: content_truncated,
        });
        if content_truncated {
            break;
        }
    }
    (results, total_tokens, truncated)
}

fn truncate_to_tokens(content: &str, budget: usize) -> String {
    take_bytes_at_char_boundary(content, approx_bytes_for_tokens(budget)).to_string()
}

fn elapsed_ms(started: Instant) -> u64 {
    u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::sync::atomic::AtomicUsize;
    use std::sync::atomic::Ordering;

    use pretty_assertions::assert_eq;
    use tempfile::tempdir;

    use super::*;
    use crate::embedding::EmbeddingCacheKey;
    use crate::embedding::EmbeddingFuture;
    use crate::indexer::IndexInitOptions;
    use crate::indexer::init_index;

    fn chunk(id: i64, path: &str, start: u64, end: u64, symbol: &str) -> ChunkRecord {
        ChunkRecord {
            id,
            path: path.to_string(),
            language: "rust".to_string(),
            symbol: Some(symbol.to_string()),
            parent_symbol: None,
            kind: "function_item".to_string(),
            start_line: start,
            end_line: end,
            content: format!("line {start}\nline {end}\n"),
            content_hash: format!("hash-{id}"),
        }
    }

    #[test]
    fn query_preprocessing_distinguishes_symbols_paths_and_intent() {
        let symbol = analyze_query("Counter::increment").expect("symbol query");
        assert_eq!(symbol.intent, QueryIntent::Symbol);
        assert!(symbol.symbol_terms.iter().any(|term| term == "increment"));

        let path = analyze_query("src/widget.ts").expect("path query");
        assert_eq!(path.intent, QueryIntent::Path);
        assert_eq!(path.path_terms, vec!["src/widget.ts"]);

        let natural =
            analyze_query("where do we increment the counter value").expect("natural query");
        assert_eq!(natural.intent, QueryIntent::NaturalLanguage);
        assert_eq!(natural.terms, vec!["increment", "counter", "value"]);
    }

    #[test]
    fn fts_query_quotes_untrusted_operators() {
        let analysis = analyze_query("foo OR bar\"*").expect("analysis");
        let query = build_fts_query(&analysis).expect("fts query");
        assert!(query.contains("\"foo\"*"));
        assert!(query.contains("\"bar\"*"));
        assert!(!query.contains("OR\"*"));
    }

    #[test]
    fn query_analysis_bounds_expression_size_for_long_tasks() {
        let query = (0..2_000_usize)
            .map(|index| {
                if index.is_multiple_of(2) {
                    format!("src/generated/{index}/identifier.rs")
                } else {
                    format!("identifier_{index}")
                }
            })
            .collect::<Vec<_>>()
            .join(" ");
        let analysis = analyze_query(&query).expect("long analysis");

        assert_eq!(analysis.terms.len(), MAX_QUERY_TERMS);
        assert_eq!(analysis.symbol_terms.len(), MAX_SYMBOL_TERMS);
        assert_eq!(analysis.path_terms.len(), MAX_PATH_TERMS);
        let fts = build_fts_query(&analysis).expect("bounded fts query");
        assert!(fts.matches(" OR ").count() < MAX_QUERY_TERMS + MAX_PATH_TERMS);
    }

    #[test]
    fn query_analysis_bounds_individual_term_bytes() {
        let query = format!("prefix_{}", "x".repeat(MAX_TERM_BYTES * 2));
        let analysis = analyze_query(&query).expect("long token analysis");

        assert!(
            analysis
                .terms
                .iter()
                .all(|term| term.len() <= MAX_TERM_BYTES)
        );
        assert!(
            analysis
                .symbol_terms
                .iter()
                .all(|term| term.len() <= MAX_TERM_BYTES)
        );
    }

    #[test]
    fn rrf_fuses_multiple_rankings_and_merges_adjacent_chunks() {
        let first = chunk(1, "src/lib.rs", 10, 20, "increment");
        let second = chunk(2, "src/lib.rs", 21, 30, "increment");
        let third = chunk(3, "src/widget.ts", 5, 10, "render");
        let first_ranking = [first.clone(), third.clone()];
        let second_ranking = [third, first.clone()];
        let third_ranking = [second];
        let fused = fuse_rankings(&[
            (&first_ranking, 1.0),
            (&second_ranking, 1.0),
            (&third_ranking, 1.0),
        ]);
        assert_eq!(fused[0].chunk.id, first.id);

        let merged = deduplicate_adjacent(fused);
        assert!(merged.iter().any(|entry| entry.chunk.path == "src/lib.rs"
            && entry.chunk.start_line == 10
            && entry.chunk.end_line == 30));
    }

    #[test]
    fn budget_clipping_prefers_high_scores_and_truncates_the_tail() {
        let chunks = vec![
            FusedChunk {
                chunk: chunk(1, "src/lib.rs", 1, 2, "first"),
                score: 2.0,
            },
            FusedChunk {
                chunk: chunk(2, "src/lib.rs", 10, 12, "second"),
                score: 1.0,
            },
        ];
        let (results, tokens, truncated) = clip_to_budget(chunks, 5, 10);
        assert_eq!(results.len(), 2);
        assert!(tokens <= 5);
        assert!(truncated);
        assert!(results[0].content.starts_with("line 1"));
    }

    #[tokio::test]
    async fn result_cache_evicts_least_recently_used_entries() {
        let cache = QueryResultCache::new(1);
        let response = SearchResponse {
            query: "query".to_string(),
            intent: QueryIntent::NaturalLanguage,
            results: Vec::new(),
            total_tokens: 0,
            truncated: false,
            elapsed_ms: 0,
        };
        let key = |query: &str| QueryResultCacheKey {
            data_version: 1,
            query: query.to_string(),
            token_budget: 100,
            max_results: 10,
            embedding_namespace: None,
        };
        let first = key("first");
        let second = key("second");

        cache.put(first.clone(), &response).await;
        assert!(cache.get(&first).await.is_some());
        cache.put(second.clone(), &response).await;
        assert!(cache.get(&first).await.is_none());
        assert!(cache.get(&second).await.is_some());
    }

    struct TestEmbeddingProvider {
        calls: AtomicUsize,
    }

    impl EmbeddingProvider for TestEmbeddingProvider {
        fn provider_name(&self) -> &str {
            "test-provider"
        }

        fn model_name(&self) -> &str {
            "test-model"
        }

        fn embed<'a>(&'a self, _text: &'a str) -> EmbeddingFuture<'a> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Box::pin(async { Ok(vec![1.0, 0.0]) })
        }
    }

    #[tokio::test]
    async fn vector_search_uses_query_embedding_cache_across_result_cache_misses() {
        let temp = tempdir().expect("tempdir");
        let root = temp.path().join("repo");
        fs::create_dir_all(root.join(".git")).expect("git dir");
        fs::write(
            root.join("lib.rs"),
            "pub fn vector_needle() -> usize { 42 }\n",
        )
        .expect("source");
        let db_path = temp.path().join("index.sqlite");
        init_index(IndexInitOptions {
            root: root.clone(),
            db_path: Some(db_path.clone()),
            ..IndexInitOptions::new(&root)
        })
        .await
        .expect("index");

        let store = Store::open_writable(&db_path).await.expect("store");
        let needle = store
            .fts_search("\"vector_needle\"*", 1)
            .await
            .expect("find chunk")
            .into_iter()
            .next()
            .expect("vector chunk");
        store
            .put_embedding(
                &EmbeddingCacheKey::new(needle.content_hash, "test-provider", "test-model"),
                &[1.0, 0.0],
            )
            .await
            .expect("cache chunk embedding");
        store.close().await;

        let engine = SearchEngine::open(SearchEngineOptions {
            root,
            db_path: Some(db_path),
            query_cache_capacity: 8,
            result_cache_capacity: 8,
        })
        .await
        .expect("search engine");
        let provider = TestEmbeddingProvider {
            calls: AtomicUsize::new(0),
        };
        let request = SearchRequest {
            query: "semantic concept unrelated to source".to_string(),
            token_budget: DEFAULT_SEARCH_TOKEN_BUDGET,
            max_results: 8,
        };
        let first = engine
            .search_with_embedding_provider(request.clone(), &provider)
            .await
            .expect("first vector search");
        assert_eq!(
            first
                .results
                .first()
                .and_then(|result| result.symbol.as_deref()),
            Some("vector_needle")
        );
        let second = engine
            .search_with_embedding_provider(
                SearchRequest {
                    max_results: 7,
                    ..request.clone()
                },
                &provider,
            )
            .await
            .expect("second vector search");
        assert_eq!(
            second
                .results
                .first()
                .and_then(|result| result.symbol.as_deref()),
            Some("vector_needle")
        );
        assert_eq!(provider.calls.load(Ordering::SeqCst), 1);
        let cached_result = engine
            .search_with_embedding_provider(request, &provider)
            .await
            .expect("cached result search");
        assert_eq!(
            cached_result
                .results
                .first()
                .and_then(|result| result.symbol.as_deref()),
            Some("vector_needle")
        );
        assert_eq!(provider.calls.load(Ordering::SeqCst), 1);
        engine.close().await;
    }
}
