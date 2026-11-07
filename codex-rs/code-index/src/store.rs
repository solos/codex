use std::collections::BTreeMap;
use std::collections::HashMap;
use std::mem::size_of;
use std::path::Path;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::Context;
use anyhow::Result;
use codex_state::SqliteConfig;
use codex_state::SqlitePoolWriteExt;
use codex_utils_absolute_path::AbsolutePathBuf;
use sqlx::QueryBuilder;
use sqlx::Row;
use sqlx::Sqlite;
use sqlx::SqlitePool;

use crate::chunker::CodeChunk;
use crate::embedding::EmbeddingCacheKey;
use crate::language::CodeLanguage;

const INITIAL_SCHEMA: &str = include_str!("../migrations/0001_initial.sql");
const EMBEDDING_CACHE_SCHEMA: &str = include_str!("../migrations/0002_embedding_cache.sql");
const PERFORMANCE_SCHEMA: &str = include_str!("../migrations/0003_performance.sql");
const CONTENT_HASH_SCHEMA: &str = include_str!("../migrations/0004_content_hash_index.sql");
const SCHEMA_VERSION: &str = "4";

#[derive(Debug, Clone)]
pub(crate) struct ExistingFile {
    pub content_hash: String,
}

#[derive(Debug, Clone)]
pub(crate) struct PreparedFile {
    pub path: String,
    pub size_bytes: u64,
    pub mtime_ns: i64,
    pub content_hash: String,
    pub language: CodeLanguage,
    pub chunks: Vec<CodeChunk>,
    pub indexed_at_unix_ms: i64,
}

#[derive(Debug, Clone)]
pub(crate) struct ChunkRecord {
    pub id: i64,
    pub path: String,
    pub language: String,
    pub symbol: Option<String>,
    pub parent_symbol: Option<String>,
    pub kind: String,
    pub start_line: u64,
    pub end_line: u64,
    pub content: String,
    #[cfg(test)]
    pub content_hash: String,
}

#[derive(Debug, Clone)]
pub(crate) struct StoreStatus {
    pub file_count: u64,
    pub chunk_count: u64,
    pub last_indexed_unix_ms: Option<i64>,
    pub languages: BTreeMap<String, u64>,
}

#[derive(Debug, Clone)]
pub(crate) struct MissingEmbedding {
    pub content_hash: String,
    pub content: String,
}

pub(crate) struct Store {
    pool: SqlitePool,
    path: PathBuf,
    fts_vocab_enabled: bool,
}

impl Store {
    pub(crate) async fn open_writable(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent).await.with_context(|| {
                format!("failed to create index directory {}", parent.display())
            })?;
        }
        let sqlite = sqlite_config_for(path)?;
        let pool = sqlite
            .open_read_write_pool(path)
            .await
            .with_context(|| format!("failed to open code index {}", path.display()))?;
        let store = Self {
            pool,
            path: path.to_path_buf(),
            fts_vocab_enabled: false,
        };
        store.initialize_schema().await?;
        Ok(store)
    }

    pub(crate) async fn open_read_only(path: &Path) -> Result<Self> {
        let sqlite = sqlite_config_for(path)?;
        let pool = sqlite
            .open_read_only_pool(path, Some(Duration::from_secs(5)))
            .await
            .with_context(|| format!("failed to open code index {}", path.display()))?;
        Ok(Self {
            pool,
            path: path.to_path_buf(),
            fts_vocab_enabled: false,
        })
    }

    async fn initialize_schema(&self) -> Result<()> {
        sqlx::raw_sql(INITIAL_SCHEMA)
            .execute(&self.pool)
            .await
            .context("failed to initialize code index schema")?;
        sqlx::raw_sql(EMBEDDING_CACHE_SCHEMA)
            .execute(&self.pool)
            .await
            .context("failed to initialize code index schema")?;
        sqlx::raw_sql(PERFORMANCE_SCHEMA)
            .execute(&self.pool)
            .await
            .context("failed to initialize code index performance indexes")?;
        sqlx::raw_sql(CONTENT_HASH_SCHEMA)
            .execute(&self.pool)
            .await
            .context("failed to initialize code index content-hash index")?;
        sqlx::query(
            "INSERT INTO index_meta (key, value) VALUES ('schema_version', ?)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value
             WHERE index_meta.value <> excluded.value",
        )
        .bind(SCHEMA_VERSION)
        .execute(&self.pool)
        .await?;

        // Fail during init rather than discovering missing FTS capability during a query.
        sqlx::query_scalar::<_, f64>(
            "SELECT bm25(chunks_fts) FROM chunks_fts
             WHERE chunks_fts MATCH 'codex' LIMIT 1",
        )
        .fetch_optional(&self.pool)
        .await
        .context("SQLite FTS5/BM25 is unavailable")?;
        sqlx::query_scalar::<_, f64>(
            "SELECT bm25(chunks_trigram) FROM chunks_trigram
             WHERE chunks_trigram MATCH 'codex' LIMIT 1",
        )
        .fetch_optional(&self.pool)
        .await
        .context("SQLite FTS5 trigram tokenizer is unavailable")?;
        Ok(())
    }

    pub(crate) async fn existing_files(&self) -> Result<HashMap<String, ExistingFile>> {
        let rows = sqlx::query("SELECT path, content_hash FROM files ORDER BY path")
            .fetch_all(&self.pool)
            .await?;
        let mut files = HashMap::with_capacity(rows.len());
        for row in rows {
            let path: String = row.try_get("path")?;
            files.insert(
                path,
                ExistingFile {
                    content_hash: row.try_get("content_hash")?,
                },
            );
        }
        Ok(files)
    }

    pub(crate) async fn apply_batch(&self, files: &[PreparedFile]) -> Result<u64> {
        if files.is_empty() {
            return Ok(0);
        }

        // BEGIN IMMEDIATE serializes writers across processes and lets the
        // underlying pool retry only SQLite BUSY admission. Each batch remains
        // an atomic commit, while WAL readers keep a consistent snapshot.
        let mut transaction = self.pool.begin_immediate().await?;
        let mut chunks_written = 0_u64;
        for file in files {
            let existing_file_id =
                sqlx::query_scalar::<_, i64>("SELECT id FROM files WHERE path = ?")
                    .bind(&file.path)
                    .fetch_optional(&mut *transaction)
                    .await?;

            let file_id = sqlx::query_scalar::<_, i64>(
                "INSERT INTO files
                    (path, size_bytes, mtime_ns, content_hash, language, indexed_at_unix_ms)
                 VALUES (?, ?, ?, ?, ?, ?)
                 ON CONFLICT(path) DO UPDATE SET
                    size_bytes = excluded.size_bytes,
                    mtime_ns = excluded.mtime_ns,
                    content_hash = excluded.content_hash,
                    language = excluded.language,
                    indexed_at_unix_ms = excluded.indexed_at_unix_ms
                 RETURNING id",
            )
            .bind(&file.path)
            .bind(i64::try_from(file.size_bytes).unwrap_or(i64::MAX))
            .bind(file.mtime_ns)
            .bind(&file.content_hash)
            .bind(file.language.as_str())
            .bind(file.indexed_at_unix_ms)
            .fetch_one(&mut *transaction)
            .await?;

            let existing_chunks = if existing_file_id.is_some() {
                sqlx::query(
                    "SELECT id, start_line, end_line, content_hash
                     FROM chunks WHERE file_id = ?",
                )
                .bind(file_id)
                .fetch_all(&mut *transaction)
                .await?
                .into_iter()
                .map(|row| {
                    Ok((
                        row.try_get::<i64, _>("id")?,
                        ChunkIdentity {
                            start_line: row.try_get("start_line")?,
                            end_line: row.try_get("end_line")?,
                            content_hash: row.try_get("content_hash")?,
                        },
                    ))
                })
                .collect::<Result<Vec<_>>>()?
            } else {
                Vec::new()
            };
            let next_chunks = file
                .chunks
                .iter()
                .map(ChunkIdentity::from)
                .collect::<std::collections::HashSet<_>>();
            let stale_chunk_ids = existing_chunks
                .into_iter()
                .filter_map(|(id, identity)| (!next_chunks.contains(&identity)).then_some(id))
                .collect::<Vec<_>>();
            if !stale_chunk_ids.is_empty() {
                let mut delete = QueryBuilder::<Sqlite>::new("DELETE FROM chunks WHERE id IN (");
                let mut separated = delete.separated(", ");
                for id in stale_chunk_ids {
                    separated.push_bind(id);
                }
                delete.push(")");
                delete.build().execute(&mut *transaction).await?;
            }

            for chunk in &file.chunks {
                let result = sqlx::query(
                    "INSERT INTO chunks
                        (file_id, path, language, symbol, parent_symbol, kind,
                         start_line, end_line, content, content_hash)
                     VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
                     ON CONFLICT(file_id, start_line, end_line, content_hash) DO NOTHING",
                )
                .bind(file_id)
                .bind(&file.path)
                .bind(file.language.as_str())
                .bind(&chunk.symbol)
                .bind(&chunk.parent_symbol)
                .bind(&chunk.kind)
                .bind(i64::try_from(chunk.start_line).unwrap_or(i64::MAX))
                .bind(i64::try_from(chunk.end_line).unwrap_or(i64::MAX))
                .bind(&chunk.content)
                .bind(&chunk.content_hash)
                .execute(&mut *transaction)
                .await?;
                chunks_written = chunks_written.saturating_add(result.rows_affected());
            }
        }
        self.prune_orphaned_embeddings_in(&mut transaction).await?;
        transaction.commit().await?;
        Ok(chunks_written)
    }

    pub(crate) async fn delete_paths(&self, paths: &[String]) -> Result<()> {
        if paths.is_empty() {
            return Ok(());
        }
        let mut transaction = self.pool.begin_immediate().await?;
        for path in paths {
            sqlx::query(
                "DELETE FROM chunks
                 WHERE file_id IN (SELECT id FROM files WHERE path = ?)",
            )
            .bind(path)
            .execute(&mut *transaction)
            .await?;
            sqlx::query("DELETE FROM files WHERE path = ?")
                .bind(path)
                .execute(&mut *transaction)
                .await?;
        }
        self.prune_orphaned_embeddings_in(&mut transaction).await?;
        transaction.commit().await?;
        Ok(())
    }

    async fn prune_orphaned_embeddings_in(
        &self,
        transaction: &mut sqlx::Transaction<'_, Sqlite>,
    ) -> Result<()> {
        sqlx::query(
            "DELETE FROM embedding_cache
             WHERE NOT EXISTS (
                 SELECT 1 FROM chunks
                 WHERE chunks.content_hash = embedding_cache.content_hash
             )",
        )
        .execute(&mut **transaction)
        .await?;
        Ok(())
    }

    pub(crate) async fn set_meta(&self, key: &str, value: &str) -> Result<()> {
        sqlx::query(
            "INSERT INTO index_meta (key, value) VALUES (?, ?)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        )
        .bind(key)
        .bind(value)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub(crate) async fn get_embedding(&self, key: &EmbeddingCacheKey) -> Result<Option<Vec<f32>>> {
        let value = sqlx::query_scalar::<_, Vec<u8>>(
            "SELECT embedding FROM embedding_cache
             WHERE content_hash = ? AND provider = ? AND model = ?",
        )
        .bind(&key.content_hash)
        .bind(&key.provider)
        .bind(&key.model)
        .fetch_optional(&self.pool)
        .await?;
        value.map(|bytes| decode_embedding(&bytes)).transpose()
    }

    pub(crate) async fn put_embedding(
        &self,
        key: &EmbeddingCacheKey,
        embedding: &[f32],
    ) -> Result<()> {
        let now = crate::indexer::now_unix_ms();
        let bytes = encode_embedding(embedding);
        sqlx::query(
            "INSERT INTO embedding_cache
                (content_hash, provider, model, dimensions, embedding,
                 created_at_unix_ms, updated_at_unix_ms)
             VALUES (?, ?, ?, ?, ?, ?, ?)
             ON CONFLICT(content_hash, provider, model) DO UPDATE SET
                dimensions = excluded.dimensions,
                embedding = excluded.embedding,
                updated_at_unix_ms = excluded.updated_at_unix_ms",
        )
        .bind(&key.content_hash)
        .bind(&key.provider)
        .bind(&key.model)
        .bind(i64::try_from(embedding.len()).unwrap_or(i64::MAX))
        .bind(bytes)
        .bind(now)
        .bind(now)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub(crate) async fn put_embeddings_batch(
        &self,
        embeddings: &[(EmbeddingCacheKey, Vec<f32>)],
    ) -> Result<()> {
        if embeddings.is_empty() {
            return Ok(());
        }
        let now = crate::indexer::now_unix_ms();
        let mut transaction = self.pool.begin_immediate().await?;
        for (key, embedding) in embeddings {
            sqlx::query(
                "INSERT INTO embedding_cache
                    (content_hash, provider, model, dimensions, embedding,
                     created_at_unix_ms, updated_at_unix_ms)
                 VALUES (?, ?, ?, ?, ?, ?, ?)
                 ON CONFLICT(content_hash, provider, model) DO UPDATE SET
                    dimensions = excluded.dimensions,
                    embedding = excluded.embedding,
                    updated_at_unix_ms = excluded.updated_at_unix_ms",
            )
            .bind(&key.content_hash)
            .bind(&key.provider)
            .bind(&key.model)
            .bind(i64::try_from(embedding.len()).unwrap_or(i64::MAX))
            .bind(encode_embedding(embedding))
            .bind(now)
            .bind(now)
            .execute(&mut *transaction)
            .await?;
        }
        transaction.commit().await?;
        Ok(())
    }

    pub(crate) async fn chunks_missing_embedding(
        &self,
        provider: &str,
        model: &str,
    ) -> Result<Vec<MissingEmbedding>> {
        let rows = sqlx::query(
            "SELECT DISTINCT chunks.content_hash, chunks.content
             FROM chunks
             WHERE NOT EXISTS (
                 SELECT 1
                 FROM embedding_cache
                 WHERE embedding_cache.content_hash = chunks.content_hash
                   AND embedding_cache.provider = ?
                   AND embedding_cache.model = ?
             )
             ORDER BY chunks.content_hash",
        )
        .bind(provider)
        .bind(model)
        .fetch_all(&self.pool)
        .await?;
        rows.iter()
            .map(|row| {
                Ok(MissingEmbedding {
                    content_hash: row.try_get("content_hash")?,
                    content: row.try_get("content")?,
                })
            })
            .collect()
    }

    pub(crate) async fn fts_search(
        &self,
        match_query: &str,
        limit: usize,
    ) -> Result<Vec<ChunkRecord>> {
        let rows = sqlx::query(
            "SELECT chunks.id, chunks.path, chunks.language, chunks.symbol,
                    chunks.parent_symbol, chunks.kind, chunks.start_line,
                    chunks.end_line, chunks.content, chunks.content_hash
             FROM chunks_fts
             JOIN chunks ON chunks.id = chunks_fts.rowid
             WHERE chunks_fts MATCH ?
             ORDER BY bm25(chunks_fts, 4.0, 8.0, 2.0, 1.0) ASC,
                      chunks.path ASC, chunks.start_line ASC, chunks.id ASC
             LIMIT ?",
        )
        .bind(match_query)
        .bind(i64::try_from(limit).unwrap_or(i64::MAX))
        .fetch_all(&self.pool)
        .await?;
        rows.iter().map(chunk_from_row).collect()
    }

    pub(crate) async fn count_fts_matches(&self, match_query: &str) -> Result<u64> {
        let count = sqlx::query_scalar::<_, i64>(
            "SELECT count(*) FROM chunks_fts WHERE chunks_fts MATCH ?",
        )
        .bind(match_query)
        .fetch_one(&self.pool)
        .await?;
        Ok(u64::try_from(count).unwrap_or_default())
    }

    pub(crate) async fn enable_fts_vocab(&mut self) -> Result<()> {
        // This is a temp table on the single read connection. Creating it does
        // not write the index database, so read-only searches can still use it.
        sqlx::raw_sql(
            "CREATE VIRTUAL TABLE IF NOT EXISTS temp.chunks_fts_vocab
             USING fts5vocab('main', 'chunks_fts', 'row')",
        )
        .execute(&self.pool)
        .await?;
        self.fts_vocab_enabled = true;
        Ok(())
    }

    pub(crate) async fn count_fts_vocab_token(&self, term: &str) -> Result<Option<u64>> {
        if !self.fts_vocab_enabled {
            return Ok(None);
        }
        let count =
            sqlx::query_scalar::<_, i64>("SELECT doc FROM temp.chunks_fts_vocab WHERE term = ?")
                .bind(term)
                .fetch_optional(&self.pool)
                .await?;
        Ok(Some(
            u64::try_from(count.unwrap_or_default()).unwrap_or_default(),
        ))
    }

    pub(crate) async fn chunk_count(&self) -> Result<u64> {
        let count = sqlx::query_scalar::<_, i64>("SELECT count(*) FROM chunks")
            .fetch_one(&self.pool)
            .await?;
        Ok(u64::try_from(count).unwrap_or_default())
    }

    pub(crate) async fn trigram_search(
        &self,
        match_query: &str,
        limit: usize,
    ) -> Result<Vec<ChunkRecord>> {
        let rows = sqlx::query(
            "SELECT chunks.id, chunks.path, chunks.language, chunks.symbol,
                    chunks.parent_symbol, chunks.kind, chunks.start_line,
                    chunks.end_line, chunks.content, chunks.content_hash
             FROM chunks_trigram
             JOIN chunks ON chunks.id = chunks_trigram.rowid
             WHERE chunks_trigram MATCH ?
             ORDER BY bm25(chunks_trigram, 4.0, 8.0, 2.0, 1.0) ASC,
                      chunks.path ASC, chunks.start_line ASC, chunks.id ASC
             LIMIT ?",
        )
        .bind(match_query)
        .bind(i64::try_from(limit).unwrap_or(i64::MAX))
        .fetch_all(&self.pool)
        .await?;
        rows.iter().map(chunk_from_row).collect()
    }

    pub(crate) async fn exact_symbol_search(
        &self,
        terms: &[String],
        limit: usize,
    ) -> Result<Vec<ChunkRecord>> {
        let terms = normalized_terms(terms);
        if terms.is_empty() {
            return Ok(Vec::new());
        }
        let mut query = QueryBuilder::<Sqlite>::new(
            "SELECT chunks.id, chunks.path, chunks.language, chunks.symbol,
                    chunks.parent_symbol, chunks.kind, chunks.start_line,
                    chunks.end_line, chunks.content, chunks.content_hash
             FROM chunks
             WHERE lower(chunks.symbol) IN (",
        );
        push_bound_terms(&mut query, &terms);
        query.push(") OR lower(chunks.parent_symbol) IN (");
        push_bound_terms(&mut query, &terms);
        query.push(
            ") ORDER BY CASE
                          WHEN lower(chunks.symbol) IN (",
        );
        push_bound_terms(&mut query, &terms);
        query.push(
            ") THEN 0
                          WHEN lower(chunks.parent_symbol) IN (",
        );
        push_bound_terms(&mut query, &terms);
        query.push(
            ") THEN 1 ELSE 2 END ASC,
                      chunks.path ASC, chunks.start_line ASC, chunks.id ASC
                  LIMIT ",
        );
        let rows = query
            .push_bind(i64::try_from(limit).unwrap_or(i64::MAX))
            .build()
            .fetch_all(&self.pool)
            .await?;
        rows.iter().map(chunk_from_row).collect()
    }

    pub(crate) async fn vector_search(
        &self,
        query_embedding: &[f32],
        provider: &str,
        model: &str,
        limit: usize,
    ) -> Result<Vec<ChunkRecord>> {
        if query_embedding.is_empty() || limit == 0 {
            return Ok(Vec::new());
        }
        let rows = sqlx::query(
            "SELECT chunks.id, chunks.path, chunks.language, chunks.symbol,
                    chunks.parent_symbol, chunks.kind, chunks.start_line,
                    chunks.end_line, chunks.content, chunks.content_hash,
                    embedding_cache.embedding
             FROM chunks
             JOIN embedding_cache
               ON embedding_cache.content_hash = chunks.content_hash
              AND embedding_cache.provider = ?
              AND embedding_cache.model = ?",
        )
        .bind(provider)
        .bind(model)
        .fetch_all(&self.pool)
        .await?;

        let mut matches = Vec::new();
        for row in &rows {
            let bytes: Vec<u8> = row.try_get("embedding")?;
            let embedding = decode_embedding(&bytes)?;
            let Some(similarity) = cosine_similarity(query_embedding, &embedding) else {
                continue;
            };
            if similarity <= 0.0 {
                continue;
            }
            matches.push((similarity, chunk_from_row(row)?));
        }
        matches.sort_by(|(left_score, left), (right_score, right)| {
            right_score
                .total_cmp(left_score)
                .then_with(|| left.path.cmp(&right.path))
                .then_with(|| left.start_line.cmp(&right.start_line))
                .then_with(|| left.id.cmp(&right.id))
        });
        matches.truncate(limit);
        Ok(matches.into_iter().map(|(_, chunk)| chunk).collect())
    }

    pub(crate) async fn nearby_chunks(
        &self,
        anchor: &ChunkRecord,
        line_window: u64,
        limit: usize,
    ) -> Result<Vec<ChunkRecord>> {
        if limit == 0 {
            return Ok(Vec::new());
        }
        let lower = anchor.start_line.saturating_sub(line_window);
        let upper = anchor.end_line.saturating_add(line_window);
        let rows = sqlx::query(
            "SELECT chunks.id, chunks.path, chunks.language, chunks.symbol,
                    chunks.parent_symbol, chunks.kind, chunks.start_line,
                    chunks.end_line, chunks.content, chunks.content_hash
             FROM chunks
             WHERE chunks.path = ?
               AND chunks.id <> ?
               AND chunks.end_line >= ?
               AND chunks.start_line <= ?
             ORDER BY abs(chunks.start_line - ?) ASC, chunks.start_line ASC, chunks.id ASC
             LIMIT ?",
        )
        .bind(&anchor.path)
        .bind(anchor.id)
        .bind(i64::try_from(lower).unwrap_or(i64::MAX))
        .bind(i64::try_from(upper).unwrap_or(i64::MAX))
        .bind(i64::try_from(anchor.start_line).unwrap_or(i64::MAX))
        .bind(i64::try_from(limit).unwrap_or(i64::MAX))
        .fetch_all(&self.pool)
        .await?;
        rows.iter().map(chunk_from_row).collect()
    }

    pub(crate) async fn status(&self) -> Result<StoreStatus> {
        let file_count = sqlx::query_scalar::<_, i64>("SELECT count(*) FROM files")
            .fetch_one(&self.pool)
            .await?;
        let chunk_count = sqlx::query_scalar::<_, i64>("SELECT count(*) FROM chunks")
            .fetch_one(&self.pool)
            .await?;
        let last_indexed_unix_ms = sqlx::query_scalar::<_, String>(
            "SELECT value FROM index_meta WHERE key = 'last_indexed_unix_ms'",
        )
        .fetch_optional(&self.pool)
        .await?
        .and_then(|value| value.parse::<i64>().ok());

        let rows = sqlx::query(
            "SELECT language, count(*) AS count FROM files GROUP BY language ORDER BY language",
        )
        .fetch_all(&self.pool)
        .await?;
        let mut languages = BTreeMap::new();
        for row in rows {
            let language: String = row.try_get("language")?;
            let count: i64 = row.try_get("count")?;
            languages.insert(language, u64::try_from(count).unwrap_or(0));
        }

        Ok(StoreStatus {
            file_count: u64::try_from(file_count).unwrap_or(0),
            chunk_count: u64::try_from(chunk_count).unwrap_or(0),
            last_indexed_unix_ms,
            languages,
        })
    }

    pub(crate) async fn data_version(&self) -> Result<i64> {
        Ok(sqlx::query_scalar::<_, i64>("PRAGMA data_version")
            .fetch_one(&self.pool)
            .await?)
    }

    pub(crate) async fn close(self) {
        self.pool.close().await;
    }

    pub(crate) fn path(&self) -> &Path {
        &self.path
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct ChunkIdentity {
    start_line: i64,
    end_line: i64,
    content_hash: String,
}

impl From<&CodeChunk> for ChunkIdentity {
    fn from(chunk: &CodeChunk) -> Self {
        Self {
            start_line: i64::try_from(chunk.start_line).unwrap_or(i64::MAX),
            end_line: i64::try_from(chunk.end_line).unwrap_or(i64::MAX),
            content_hash: chunk.content_hash.clone(),
        }
    }
}

fn chunk_from_row(row: &sqlx::sqlite::SqliteRow) -> Result<ChunkRecord> {
    Ok(ChunkRecord {
        id: row.try_get("id")?,
        path: row.try_get("path")?,
        language: row.try_get("language")?,
        symbol: row.try_get("symbol")?,
        parent_symbol: row.try_get("parent_symbol")?,
        kind: row.try_get("kind")?,
        start_line: positive_line(row.try_get("start_line")?),
        end_line: positive_line(row.try_get("end_line")?),
        content: row.try_get("content")?,
        #[cfg(test)]
        content_hash: row.try_get("content_hash")?,
    })
}

fn positive_line(line: i64) -> u64 {
    u64::try_from(line).unwrap_or(1).max(1)
}

fn normalized_terms(terms: &[String]) -> Vec<String> {
    let mut normalized = terms
        .iter()
        .map(|term| term.trim().to_lowercase())
        .filter(|term| !term.is_empty())
        .collect::<Vec<_>>();
    normalized.sort();
    normalized.dedup();
    normalized
}

fn push_bound_terms(query: &mut QueryBuilder<Sqlite>, terms: &[String]) {
    let mut separated = query.separated(", ");
    for term in terms {
        separated.push_bind(term);
    }
}

fn cosine_similarity(left: &[f32], right: &[f32]) -> Option<f64> {
    if left.len() != right.len() || left.is_empty() {
        return None;
    }
    let mut dot = 0.0_f64;
    let mut left_norm = 0.0_f64;
    let mut right_norm = 0.0_f64;
    for (left, right) in left.iter().zip(right) {
        let left = f64::from(*left);
        let right = f64::from(*right);
        dot += left * right;
        left_norm += left * left;
        right_norm += right * right;
    }
    if left_norm == 0.0 || right_norm == 0.0 {
        return None;
    }
    Some(dot / (left_norm.sqrt() * right_norm.sqrt()))
}

fn encode_embedding(embedding: &[f32]) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(embedding.len().saturating_mul(size_of::<f32>()));
    for value in embedding {
        bytes.extend_from_slice(&value.to_le_bytes());
    }
    bytes
}

fn decode_embedding(bytes: &[u8]) -> Result<Vec<f32>> {
    anyhow::ensure!(
        bytes.len().is_multiple_of(size_of::<f32>()),
        "cached embedding has an invalid byte length"
    );
    Ok(bytes
        .chunks_exact(size_of::<f32>())
        .map(|chunk| f32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]))
        .collect())
}

fn sqlite_config_for(path: &Path) -> Result<SqliteConfig> {
    let sqlite_home = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    Ok(SqliteConfig::from_sqlite_home_with_nfs_enforcement(
        AbsolutePathBuf::from_absolute_path(sqlite_home)?,
        /*enforce_delete_on_nfs*/ false,
    ))
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;
    use tempfile::tempdir;

    use super::*;

    #[tokio::test]
    async fn store_uses_wal_and_performance_indexes() {
        let temp = tempdir().expect("tempdir");
        let db_path = temp.path().join("index.sqlite");
        let store = Store::open_writable(&db_path).await.expect("open store");

        let journal_mode = sqlx::query_scalar::<_, String>("PRAGMA journal_mode")
            .fetch_one(&store.pool)
            .await
            .expect("journal mode");
        assert_eq!(journal_mode, "wal");

        let indexes = sqlx::query_scalar::<_, String>(
            "SELECT name FROM sqlite_master
             WHERE type = 'index'
               AND name IN (
                    'idx_chunks_path_lines',
                    'idx_chunks_symbol_lower',
                    'idx_chunks_parent_symbol_lower',
                    'idx_files_content_hash'
               )
             ORDER BY name",
        )
        .fetch_all(&store.pool)
        .await
        .expect("performance indexes");
        assert_eq!(
            indexes,
            vec![
                "idx_chunks_parent_symbol_lower",
                "idx_chunks_path_lines",
                "idx_chunks_symbol_lower",
                "idx_files_content_hash",
            ]
        );
        store.close().await;
    }

    #[tokio::test]
    async fn store_reopens_after_fts_rows_are_present() {
        let temp = tempdir().expect("tempdir");
        let db_path = temp.path().join("index.sqlite");
        let store = Store::open_writable(&db_path).await.expect("open store");
        store
            .apply_batch(&[PreparedFile {
                path: "src/lib.rs".to_string(),
                size_bytes: 8,
                mtime_ns: 0,
                content_hash: "file-hash".to_string(),
                language: CodeLanguage::Rust,
                chunks: vec![CodeChunk {
                    symbol: Some("codex_probe".to_string()),
                    parent_symbol: None,
                    kind: "function_item".to_string(),
                    start_line: 1,
                    end_line: 1,
                    content: "fn codex_probe() {}".to_string(),
                    content_hash: "chunk-hash".to_string(),
                }],
                indexed_at_unix_ms: 0,
            }])
            .await
            .expect("write chunk");
        store.close().await;

        let reopened = Store::open_writable(&db_path)
            .await
            .expect("reopen store with populated FTS tables");
        let status = reopened.status().await.expect("status");
        assert_eq!(status.chunk_count, 1);
        reopened.close().await;
    }
}
