use std::collections::HashSet;
use std::fs;
use std::num::NonZeroUsize;
use std::path::Path;
use std::time::Instant;

use anyhow::Result;
use codex_code_index::DEFAULT_QUERY_CACHE_CAPACITY;
use codex_code_index::DEFAULT_SEARCH_RESULT_CACHE_CAPACITY;
use codex_code_index::DEFAULT_SEARCH_TOKEN_BUDGET;
use codex_code_index::IndexInitOptions;
use codex_code_index::QueryIntent;
use codex_code_index::SearchEngine;
use codex_code_index::SearchEngineOptions;
use codex_code_index::SearchRequest;
use codex_code_index::analyze_query;
use codex_code_index::discover_indexable_files;
use codex_code_index::init_index;
use codex_file_search::FileSearchOptions;
use tempfile::tempdir;

const DEFAULT_FILE_COUNT: usize = 20_000;
const DEFAULT_SAMPLES: usize = 60;
const SEARCH_MAX_RESULTS: usize = 20;

#[tokio::main]
async fn main() -> Result<()> {
    let file_count = std::env::args()
        .nth(1)
        .map(|value| value.parse::<usize>())
        .transpose()?
        .unwrap_or(DEFAULT_FILE_COUNT);
    anyhow::ensure!(file_count > 0, "file count must be greater than zero");
    let search_samples = std::env::args()
        .nth(2)
        .map(|value| value.parse::<usize>())
        .transpose()?
        .unwrap_or(DEFAULT_SAMPLES)
        .max(1);

    let temp = tempdir()?;
    let root = temp.path().join("benchmark_repo");
    fs::create_dir_all(root.join(".git"))?;
    for index in 0..file_count {
        let directory = root.join(format!("src/module_{:03}", index / 20));
        fs::create_dir_all(&directory)?;
        fs::write(
            directory.join(format!("file_{index:05}.rs")),
            synthetic_source(index, "original"),
        )?;
    }

    let db_path = temp.path().join("index.sqlite");
    init_index(IndexInitOptions {
        root: root.clone(),
        db_path: Some(db_path.clone()),
        ..IndexInitOptions::new(&root)
    })
    .await?;
    let engine = SearchEngine::open(SearchEngineOptions {
        root: root.clone(),
        db_path: Some(db_path),
        query_cache_capacity: DEFAULT_QUERY_CACHE_CAPACITY,
        result_cache_capacity: DEFAULT_SEARCH_RESULT_CACHE_CAPACITY,
    })
    .await?;

    // Warm both paths before measuring unique queries.
    engine
        .search(SearchRequest {
            query: "warmup_function_benchmark".to_string(),
            token_budget: DEFAULT_SEARCH_TOKEN_BUDGET,
            max_results: SEARCH_MAX_RESULTS,
        })
        .await?;
    no_index_fallback_search(&root, "warmup_file_benchmark.rs", SEARCH_MAX_RESULTS).await?;

    let mut indexed_latencies_ms = Vec::with_capacity(search_samples);
    let mut no_index_latencies_ms = Vec::with_capacity(search_samples);
    let mut content_scan_latencies_ms = Vec::with_capacity(search_samples);
    let mut indexed_by_kind = [Vec::new(), Vec::new(), Vec::new()];
    let mut no_index_by_kind = [Vec::new(), Vec::new(), Vec::new()];
    let mut content_scan_by_kind = [Vec::new(), Vec::new(), Vec::new()];
    let mut no_index_empty_results = 0_usize;
    let mut content_scan_empty_results = 0_usize;

    for sample in 0..search_samples {
        let kind = sample % 3;
        let query = benchmark_query(sample, file_count);

        let started = Instant::now();
        let response = engine
            .search(SearchRequest {
                query: query.clone(),
                token_budget: DEFAULT_SEARCH_TOKEN_BUDGET,
                max_results: SEARCH_MAX_RESULTS,
            })
            .await?;
        let indexed_ms = elapsed_ms(started);
        indexed_latencies_ms.push(indexed_ms);
        indexed_by_kind[kind].push(indexed_ms);

        let started = Instant::now();
        let no_index_results = no_index_fallback_search(&root, &query, SEARCH_MAX_RESULTS).await?;
        let no_index_ms = elapsed_ms(started);
        no_index_latencies_ms.push(no_index_ms);
        no_index_by_kind[kind].push(no_index_ms);
        if no_index_results == 0 {
            no_index_empty_results += 1;
        }

        let started = Instant::now();
        let content_scan_results = no_index_content_search(&root, &query).await?;
        let content_scan_ms = elapsed_ms(started);
        content_scan_latencies_ms.push(content_scan_ms);
        content_scan_by_kind[kind].push(content_scan_ms);
        if content_scan_results == 0 {
            content_scan_empty_results += 1;
        }

        anyhow::ensure!(
            !response.results.is_empty() || no_index_results == 0,
            "indexed search unexpectedly returned no results for {query:?}"
        );
    }
    engine.close().await;

    println!("files={file_count} samples={search_samples}");
    println!(
        "indexed_p50_ms={:.3} indexed_p95_ms={:.3} indexed_avg_ms={:.3}",
        percentile(&indexed_latencies_ms, 50.0),
        percentile(&indexed_latencies_ms, 95.0),
        average(&indexed_latencies_ms),
    );
    println!(
        "no_index_p50_ms={:.3} no_index_p95_ms={:.3} no_index_avg_ms={:.3} no_index_empty_results={no_index_empty_results}",
        percentile(&no_index_latencies_ms, 50.0),
        percentile(&no_index_latencies_ms, 95.0),
        average(&no_index_latencies_ms),
    );
    println!(
        "content_scan_p50_ms={:.3} content_scan_p95_ms={:.3} content_scan_avg_ms={:.3} content_scan_empty_results={content_scan_empty_results}",
        percentile(&content_scan_latencies_ms, 50.0),
        percentile(&content_scan_latencies_ms, 95.0),
        average(&content_scan_latencies_ms),
    );
    print_kind(
        "symbol",
        &indexed_by_kind[0],
        &no_index_by_kind[0],
        &content_scan_by_kind[0],
    );
    print_kind(
        "path",
        &indexed_by_kind[1],
        &no_index_by_kind[1],
        &content_scan_by_kind[1],
    );
    print_kind(
        "natural",
        &indexed_by_kind[2],
        &no_index_by_kind[2],
        &content_scan_by_kind[2],
    );
    println!(
        "fallback_p50_speedup={:.2}x fallback_p95_speedup={:.2}x",
        percentile(&no_index_latencies_ms, 50.0)
            / percentile(&indexed_latencies_ms, 50.0).max(f64::EPSILON),
        percentile(&no_index_latencies_ms, 95.0)
            / percentile(&indexed_latencies_ms, 95.0).max(f64::EPSILON),
    );
    println!(
        "content_scan_p50_speedup={:.2}x content_scan_p95_speedup={:.2}x",
        percentile(&content_scan_latencies_ms, 50.0)
            / percentile(&indexed_latencies_ms, 50.0).max(f64::EPSILON),
        percentile(&content_scan_latencies_ms, 95.0)
            / percentile(&indexed_latencies_ms, 95.0).max(f64::EPSILON),
    );
    println!(
        "fallback_total_time_speedup={:.2}x indexed_total_ms={:.3} no_index_total_ms={:.3} content_scan_total_ms={:.3}",
        no_index_latencies_ms.iter().sum::<f64>()
            / indexed_latencies_ms.iter().sum::<f64>().max(f64::EPSILON),
        indexed_latencies_ms.iter().sum::<f64>(),
        no_index_latencies_ms.iter().sum::<f64>(),
        content_scan_latencies_ms.iter().sum::<f64>(),
    );
    Ok(())
}

/// Mirrors the extension's no-index fallback: discover allowed files, perform
/// an ignore-aware filename search, then apply the index discovery allowlist.
async fn no_index_fallback_search(root: &Path, query: &str, max_results: usize) -> Result<usize> {
    let analysis = analyze_query(query)?;
    let pattern = analysis
        .path_terms
        .first()
        .or_else(|| analysis.symbol_terms.first())
        .or_else(|| analysis.terms.first())
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("query produced no searchable terms"))?;
    let root = root.canonicalize()?;
    let limit = NonZeroUsize::new(max_results.saturating_mul(8).min(200))
        .ok_or_else(|| anyhow::anyhow!("search result limit must be non-zero"))?;

    tokio::task::spawn_blocking(move || {
        let allowed = discover_indexable_files(&root)?
            .into_iter()
            .collect::<HashSet<_>>();
        let matches = codex_file_search::run(
            &pattern,
            vec![root],
            FileSearchOptions {
                limit,
                respect_gitignore: true,
                ..FileSearchOptions::default()
            },
            /*cancel_flag*/ None,
        )?;
        Ok(matches
            .matches
            .into_iter()
            .filter(|file_match| allowed.contains(&file_match.full_path()))
            .count())
    })
    .await?
}

/// Approximates a no-index content search such as `rg`: discover allowed files,
/// then scan each path or file body and count matching files.
async fn no_index_content_search(root: &Path, query: &str) -> Result<usize> {
    let analysis = analyze_query(query)?;
    let terms = if analysis.intent == QueryIntent::Path {
        analysis.path_terms
    } else {
        analysis.terms
    };
    if terms.is_empty() {
        return Ok(0);
    }
    let root = root.canonicalize()?;

    tokio::task::spawn_blocking(move || {
        let files = discover_indexable_files(&root)?;
        let mut matching_files = 0_usize;
        for file in files {
            let path_matches = if analysis.intent == QueryIntent::Path {
                let path = file.to_string_lossy();
                terms.iter().any(|term| path.contains(term))
            } else {
                false
            };
            let content_matches = if analysis.intent == QueryIntent::Path {
                false
            } else {
                let Ok(content) = fs::read_to_string(&file) else {
                    continue;
                };
                terms.iter().any(|term| content.contains(term))
            };
            if path_matches || content_matches {
                matching_files += 1;
            }
        }
        Ok(matching_files)
    })
    .await?
}

fn benchmark_query(sample: usize, file_count: usize) -> String {
    let file_index = sample % file_count;
    match sample % 3 {
        0 => format!("function_{file_index}_0_original"),
        1 => format!("src/module_{:03}/file_{file_index:05}.rs", file_index / 20),
        _ => format!("function value index {file_index}"),
    }
}

fn synthetic_source(index: usize, suffix: &str) -> String {
    let mut source = String::new();
    for function in 0..8 {
        source.push_str(&format!(
            "pub fn function_{index}_{function}_{suffix}() -> usize {{\n    let value = {function};\n    value + {index}\n}}\n\n"
        ));
    }
    source
}

fn elapsed_ms(started: Instant) -> f64 {
    started.elapsed().as_secs_f64() * 1_000.0
}

fn percentile(samples_ms: &[f64], percentile: f64) -> f64 {
    let mut sorted = samples_ms.to_vec();
    sorted.sort_by(f64::total_cmp);
    let rank = ((sorted.len().saturating_sub(1)) as f64 * percentile / 100.0).ceil() as usize;
    sorted[rank.min(sorted.len() - 1)]
}

fn average(samples_ms: &[f64]) -> f64 {
    samples_ms.iter().sum::<f64>() / samples_ms.len() as f64
}

fn print_kind(kind: &str, indexed: &[f64], no_index: &[f64], content_scan: &[f64]) {
    if indexed.is_empty() || no_index.is_empty() || content_scan.is_empty() {
        return;
    }
    let indexed_p50 = percentile(indexed, 50.0);
    let indexed_p95 = percentile(indexed, 95.0);
    let no_index_p50 = percentile(no_index, 50.0);
    let no_index_p95 = percentile(no_index, 95.0);
    let content_scan_p50 = percentile(content_scan, 50.0);
    let content_scan_p95 = percentile(content_scan, 95.0);
    println!(
        "{kind}_indexed_p50_ms={indexed_p50:.3} {kind}_indexed_p95_ms={indexed_p95:.3} \
         {kind}_no_index_p50_ms={no_index_p50:.3} {kind}_no_index_p95_ms={no_index_p95:.3} \
         {kind}_content_scan_p50_ms={content_scan_p50:.3} {kind}_content_scan_p95_ms={content_scan_p95:.3} \
         {kind}_fallback_p50_speedup={:.2}x {kind}_fallback_p95_speedup={:.2}x \
         {kind}_content_scan_p50_speedup={:.2}x {kind}_content_scan_p95_speedup={:.2}x",
        no_index_p50 / indexed_p50.max(f64::EPSILON),
        no_index_p95 / indexed_p95.max(f64::EPSILON),
        content_scan_p50 / indexed_p50.max(f64::EPSILON),
        content_scan_p95 / indexed_p95.max(f64::EPSILON),
    );
}
