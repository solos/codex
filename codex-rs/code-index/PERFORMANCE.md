# Local Code Index Performance Report

## Summary

This report measures the opt-in local code index against two no-index
baselines on the same synthetic repository:

- **Indexed search:** the SQLite FTS5/trigram/symbol hybrid search engine.
- **No-index fallback:** the ignore-aware filename search used by the extension
  when the index is unavailable.
- **Full content scan:** an ignore-aware file walk that reads every allowed
  file and performs substring matching, approximating an `rg`-style fallback.

The benchmark consists of 20,000 source files, 160,000 AST chunks, and 60
mixed queries (20 symbol, 20 path, and 20 natural-language queries). Query
latency includes query analysis, backend retrieval, RRF fusion, adjacent-chunk
merging, nearby-context lookup, and token-budget clipping.

## Results

The indexed-vs-no-index table is the median of three stable runs. Times are in
milliseconds.

| Metric | Indexed | No-index fallback | Full content scan |
| --- | ---: | ---: | ---: |
| Overall P50 | **5.011** | 59.192 | 75.353 |
| Overall P95 | **12.016** | 69.328 | 77.347 |
| 60-query total | **273.230** | 3,732.366 | 3,756.836 |
| Empty results | **0 / 60** | 40 / 60 | 0 / 60 |

| Comparison | P50 speedup | P95 speedup | Total-time speedup |
| --- | ---: | ---: | ---: |
| Indexed vs no-index fallback | **11.81x** | **5.77x** | **13.66x** |
| Indexed vs full content scan | **15.04x** | **6.44x** | **13.75x** |

The fallback often looks cheap because it performs filename matching rather
than repository-wide retrieval, but it returns no usable result for 40 of the
60 queries. Its latency is therefore not a like-for-like quality comparison.
The indexed search returned a non-empty result for every query.

### Breakdown by query type

| Query type | Indexed P50 / P95 | Fallback P50 / P95 | Full scan P50 / P95 |
| --- | ---: | ---: | ---: |
| Symbol | **5.307 / 12.016** | 67.754 / 70.834 | 76.205 / 79.014 |
| Path | **4.978 / 5.811** | 59.140 / 68.475 | 35.808 / 37.291 |
| Natural language | **0.980 / 18.858** | 58.087 / 68.476 | 75.514 / 76.779 |

Natural-language P50 is lower than symbol and path P50 because the benchmark
queries share a highly common term that can be selected without a broad
multi-backend search. Its P95 is higher because the first query for a term
pays for document-frequency statistics before the cache is populated.

## Optimization

Profiling the pre-optimization natural-language path changed the diagnosis.
For the problematic query, the approximate backend costs were:

| Stage | Approximate latency |
| --- | ---: |
| FTS/BM25 | 104-107 ms |
| Trigram | 152 ms |
| Exact-symbol lookup | 0.4-0.6 ms |
| RRF fusion | 0.09-0.16 ms |
| Deduplication, context, clipping | < 0.3 ms |

RRF was not the bottleneck. The dominant cost was ranking broad FTS and
trigram queries whose OR terms matched nearly all 160,000 chunks. A global
`LIMIT` cannot avoid that ranking work when every term has a large posting
list.

The natural-language planner now:

1. Samples at most eight query terms.
2. Counts each term's FTS document frequency, caching the result until SQLite's
   data version changes. Single-token terms use the FTS5 vocabulary table as a
   fast lookup; multi-token terms use exact MATCH counts.
3. Treats a term as selective when it matches at most
   `max(total_chunks / 20, 64)` chunks.
4. Sends at most three selective terms to exact-phrase FTS.
5. Disables trigram retrieval unless at least one genuinely selective term was
   selected.
6. Falls back to the least common observed term when every term is common.

Symbol and path queries keep the original prefix-FTS plus trigram behavior.
The backend candidate limit is still applied before fusion, so the fusion
input remains bounded.

The FTS5 vocabulary fast path reduced natural-language P95 from 22.350 ms to
18.858 ms in the same 20,000-file benchmark. Multi-token terms still fall back
to exact counts because a vocabulary document frequency measures individual
tokens, not phrase co-occurrence.

This optimization is deliberately about **query routing before retrieval**,
not replacing FTS5 with another inverted index. SQLite FTS5 is already an
inverted index, and the measured backend ranking cost was the limiting factor.

## Index Cost

The latest reference run produced:

| Metric | Value |
| --- | ---: |
| Full index time | 10,089 ms |
| Indexed files | 20,000 |
| Indexed chunks | 160,000 |
| First single-file incremental update | 167 ms |
| Single-file incremental update P50 | 164.804 ms |
| Single-file incremental update P95 | 167.041 ms |
| Full index / first incremental update | 60.40x |
| Index database size | 186.766 MiB |
| RSS after indexing | 29.145 MiB |
| RSS after searches | 31.199 MiB |
| Peak RSS | 32.547 MiB |

The full-index ratio describes update cost, not query speed. A larger ratio
means incremental updates are cheaper relative to rebuilding the index.

## Interpretation

- The index is materially faster for typical symbol and path queries at both
  P50 and P95.
- Natural-language search also wins at P50 and P95 for this workload, but its
  P95 is the dominant remaining query-latency cost.
- The no-index fallback should not be presented as equally capable: it returns
  no result for two thirds of the benchmark queries.
- The index's primary user-facing benefit is not only latency. It also makes
  repository-scale retrieval possible without repeatedly walking and parsing
  the whole repository.

The next optimization target should be the remaining exact-count path for
multi-token terms, for example token-aware vocabulary aggregation or a
persistent term-frequency summary. A lightweight semantic retrieval path
remains useful for recall, but it should be evaluated separately with an
embedding provider rather than assumed to replace lexical retrieval.

## Reproduction

```bash
cd codex-rs

cargo build -p codex-code-index --release \
  --example incremental_benchmark --locked
target/release/examples/incremental_benchmark 20000 60 3

cargo build -p codex-code-index-extension --release \
  --example index_vs_no_index_benchmark --locked
target/release/examples/index_vs_no_index_benchmark 20000 60
```

The incremental benchmark exits unsuccessfully if query P95 is at least
200 ms or incremental-update P95 is at least 2 seconds.

These measurements use a synthetic repository. Real-world results depend on
language mix, file sizes, filesystem cache state, query vocabulary, and whether
vector retrieval is enabled.
