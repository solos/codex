# Local Code Index

This crate implements the local code index used by the Codex code-index
extension. The feature is enabled by default and does not make network calls.

See [`IMPROVEMENTS.zh-CN.md`](IMPROVEMENTS.zh-CN.md) for the Chinese capability
and performance report, or [`PERFORMANCE.md`](PERFORMANCE.md) for the English
benchmark report.

## ADR-0001: SQLite-backed Hybrid Code Retrieval

### Context

Codex needs repository-scale code retrieval without introducing a resident
service or blocking the interactive CLI. The implementation must respect
repository ignore rules, avoid sensitive files, keep source local, support
incremental updates, and remain useful when an embedding provider is absent.

The repository is Rust and already has a SQLite abstraction in `codex-state`,
an ignore-aware file discovery stack, and extension hooks for turn-input
context. The index therefore follows those existing abstractions instead of
adding a separate database or daemon.

### Alternatives

| Option | Advantages | Costs | Decision |
| --- | --- | --- | --- |
| SQLite FTS5/trigram plus SQLite metadata and embedding BLOBs | No new service, transactional WAL reads, simple deployment, portable across platforms | Exact vector scan is not suitable for very large vector sets | Selected |
| SQLite metadata plus sqlite-vec or LanceDB | Better approximate vector search at scale | Extra native dependency and platform packaging risk; vector retrieval is optional | Deferred until vector scale requires it |
| External search service | Strong distributed indexing and vector features | Resident service, operational complexity, privacy boundary, CLI startup risk | Rejected |

### Decision

- Store file metadata, chunks, FTS5, trigram data, and embedding-cache rows in
  one local SQLite database.
- Use `tree-sitter` for Rust, TypeScript, TSX, JavaScript, JSX, and Python.
  Unknown text formats use bounded, overlapping line chunks.
- Use SHA-256 file and chunk hashes for incremental updates. A full ignore-aware
  walk detects additions and deletions; unchanged file hashes skip parsing and
  rewriting.
- Use weighted reciprocal-rank fusion (RRF) over BM25, trigram, exact-symbol,
  and optional vector rankings.
- Run each backend on its own single-connection read-only pool so SQLite does
  not serialize backend work inside one `try_join!`.
- Keep vector support behind an injected `EmbeddingProvider`. No provider or
  remote embedding call is installed by default. `backfill_embeddings`
  materializes distinct chunk hashes with bounded provider concurrency
  (default 4, maximum 16).

### Consequences

- Queries see a consistent WAL snapshot while an index writer updates another
  connection.
- The default path remains lexical and symbol based. Vector retrieval requires
  an explicit host provider and embedding-cache population.
- Exact cosine scan is intentional for the current optional vector path. Move
  to an approximate index only when measured vector datasets exceed the
  interactive latency budget.

## Architecture

### Discovery

`Discovery` uses the `ignore` crate with `.gitignore`, `.ignore`, and
`.codexignore` support. It skips hidden files, default generated directories,
symlinks, sensitive names/extensions, files over 1 MiB, binary files, and
non-UTF-8 input. All paths stored in the index are repository-relative.

### Chunking

Tree-sitter nodes are selected at function, type, module, class, method, and
similar semantic boundaries. Each chunk records path, language, symbol, parent
symbol, node kind, line range, content, and content hash. Oversized semantic
chunks are split by token budget with bounded line overlap.

### Storage

The SQLite database contains:

- `files`: path, size, mtime, content hash, language, indexing timestamp.
- `chunks`: semantic chunk metadata and content.
- `chunks_fts`: BM25 full-text index.
- `chunks_trigram`: substring/trigram index.
- `embedding_cache`: content-hash/provider/model keyed float32 embeddings.
- `index_meta`: root and schema metadata.

Batch writes use a single immediate transaction and WAL. Read-only pools use a
short busy timeout. Schema version 4 adds content-hash lookup and performance
indexes. A separate writer lock serializes index update processes. Stale
embedding-cache rows are pruned after chunk and file deletion.

### Incremental Updates

`index update` walks the tree and reparses only files whose content hash
changed. Inside a changed file, chunk rows whose line span and content hash are
unchanged are retained; only stale chunks are deleted and only new content
hashes are inserted. `update_paths` scopes work to changed files and directory
subtrees. Deletion and rename events remove stale rows. Ignore-file changes
trigger a full reconciliation while retaining hash-based skip behavior.

`index watch` registers the filesystem watch before the initial refresh,
debounces events, filters database sidecars, updates path batches sequentially,
and retains failed batches for retry.

### Retrieval

Query analysis distinguishes symbol, path, and natural-language intent. For
natural-language queries, the planner samples up to eight terms, estimates
FTS document frequency with a data-version-scoped cache, and routes only the
most selective terms into FTS. Single-token terms use the FTS5 vocabulary
table as a fast frequency lookup; multi-token terms retain exact MATCH counts.
Trigram retrieval is enabled only when at least one selective term was found;
broad natural-language terms would otherwise rank nearly every chunk before
RRF. Symbol and path queries retain the full prefix-FTS plus trigram path.

The search engine runs FTS, trigram, exact-symbol, and optional vector
backends on independent read connections, fuses rankings with weighted RRF,
merges adjacent chunks, adds nearby context, and clips output to the requested
token budget.
Query embeddings and complete search responses use bounded LRU caches.
Query and symbol term extraction is capped at 64 and 48 terms respectively so
long task descriptions cannot produce unbounded SQL and FTS expressions.
`backfill_embeddings` writes missing chunk vectors to the persistent
content-hash/provider/model cache in bounded SQLite batches.

### CLI and Agent Integration

The CLI exposes:

```text
codex index init [PATH] [--db PATH] [--background] [--jobs N]
codex index update [PATH] [--db PATH] [--jobs N]
codex index watch [PATH] [--db PATH] [--debounce-ms N]
codex index status [PATH] [--db PATH]
codex index search "QUERY" [--path PATH] [--budget N] [--limit N]
```

The extension is installed through the existing extension registry and is
enabled by default. Set `features.code_index.enabled = false` to opt out. Before
a turn it searches using the user task text, injects a bounded internal context
fragment, and falls back to ignore-aware file search if the index is missing or
unreadable. Fallback results are filtered through the same discovery policy as
the index, so ignored, hidden, generated, and sensitive files are not exposed.

A host that enables vector retrieval supplies an `EmbeddingProvider` and calls
`backfill_embeddings` after indexing. Rerunning the backfill only processes
content hashes missing for that provider/model namespace.

Example configuration:

```toml
[features.code_index]
# Enabled by default; set to false to opt out.
enabled = true
token_budget = 1200
max_results = 8
index_path = ".codex/index.sqlite"
embedding = false
debug = false
```

## Non-goals

- No resident indexing daemon.
- No network embedding calls unless a host explicitly injects a provider.
- No proprietary Cursor code or service dependency.
- No index of hidden, ignored, generated, binary, oversized, or sensitive
  files.
- No approximate vector index in the current SQLite-only implementation.

## Verification

Run:

```text
cargo test -p codex-code-index --locked
cargo test -p codex-code-index-extension --locked
cargo clippy -p codex-code-index --all-targets --no-deps --locked -- -D warnings
cargo clippy -p codex-code-index-extension --all-targets --no-deps --locked -- -D warnings
cargo check -p codex-app-server --locked
cargo check -p codex-cli --locked
cargo fmt --all -- --check
```

Run the synthetic benchmark in release mode:

```text
cargo build -p codex-code-index --release --example incremental_benchmark --locked
target/release/examples/incremental_benchmark 20000 60 3

cargo build -p codex-code-index-extension --release \
  --example index_vs_no_index_benchmark --locked
target/release/examples/index_vs_no_index_benchmark 20000 60
```

The example exits unsuccessfully if query P95 is at least 200 ms or
incremental-update P95 is at least 2 seconds.

The 20k-file reference run on the development machine produced:

```text
files=20000
full_index_ms=10089
full_index_reported_ms=10088 chunks=160000
search_samples=60 search_results_total=384 search_p50_ms=4.208 search_p95_ms=12.159 search_max_ms=18.641
search_symbol_p50_ms=4.266 search_symbol_p95_ms=12.159
search_path_p50_ms=4.214 search_path_p95_ms=4.476
search_natural_p50_ms=0.452 search_natural_p95_ms=18.641
incremental_samples=3 incremental_p50_ms=164.804 incremental_p95_ms=167.041
single_file_update_ms=167 files_indexed=1 chunks=8
speedup=60.40x index_size_mib=186.766
memory_after_index_rss_mib=29.145 peak_rss_mib=32.547
```

## File Map

- `src/discovery.rs`: ignore-aware discovery and sensitive-file filtering.
- `src/language.rs`: language detection and tree-sitter node policy.
- `src/chunker.rs`: semantic and fallback chunking.
- `src/store.rs`: schema, writes, FTS/trigram/symbol/vector reads.
- `src/indexer.rs`: full/incremental indexing, batching, cancellation, locking.
- `src/watch.rs`: debounced watcher and retry behavior.
- `src/embedding.rs`: embedding provider and content-hash cache.
- `src/search.rs`: query analysis, parallel backends, RRF, context, budgets.
- `src/cli.rs`: `index init/update/watch/status/search`.
- `examples/incremental_benchmark.rs`: latency, update, size, and RSS report.
- `../ext/code-index/examples/index_vs_no_index_benchmark.rs`: indexed versus
  no-index fallback and full-content scan comparison.
- `../ext/code-index`: turn-input context injection and fallback search.
