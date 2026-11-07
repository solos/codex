use std::fs;
use std::path::Path;
use std::path::PathBuf;
use std::time::Instant;

use anyhow::Result;
use codex_code_index::DEFAULT_QUERY_CACHE_CAPACITY;
use codex_code_index::DEFAULT_SEARCH_RESULT_CACHE_CAPACITY;
use codex_code_index::DEFAULT_SEARCH_TOKEN_BUDGET;
use codex_code_index::IndexInitOptions;
use codex_code_index::IndexUpdateOptions;
use codex_code_index::SearchEngine;
use codex_code_index::SearchEngineOptions;
use codex_code_index::SearchRequest;
use codex_code_index::init_index;
use codex_code_index::update_paths;
use tempfile::tempdir;

const DEFAULT_SEARCH_SAMPLES: usize = 50;
const DEFAULT_INCREMENTAL_SAMPLES: usize = 5;
const SEARCH_MAX_RESULTS: usize = 20;

#[tokio::main]
async fn main() -> Result<()> {
    let file_count = std::env::args()
        .nth(1)
        .map(|value| value.parse::<usize>())
        .transpose()?
        .unwrap_or(1_000);
    anyhow::ensure!(file_count > 0, "file count must be greater than zero");
    let search_samples = std::env::args()
        .nth(2)
        .map(|value| value.parse::<usize>())
        .transpose()?
        .unwrap_or(DEFAULT_SEARCH_SAMPLES)
        .max(1);
    let incremental_samples = std::env::args()
        .nth(3)
        .map(|value| value.parse::<usize>())
        .transpose()?
        .unwrap_or(DEFAULT_INCREMENTAL_SAMPLES)
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
    let before_index = read_process_memory();

    let started = Instant::now();
    let full = init_index(IndexInitOptions {
        root: root.clone(),
        db_path: Some(db_path.clone()),
        ..IndexInitOptions::new(&root)
    })
    .await?;
    let measured_full = started.elapsed();
    let after_index = read_process_memory();
    let index_size_bytes = index_size_bytes(&db_path);

    let engine = SearchEngine::open(SearchEngineOptions {
        root: root.clone(),
        db_path: Some(db_path.clone()),
        query_cache_capacity: DEFAULT_QUERY_CACHE_CAPACITY,
        result_cache_capacity: DEFAULT_SEARCH_RESULT_CACHE_CAPACITY,
    })
    .await?;
    // Warm SQLite pages and result-cache registration with a query outside the sample set.
    engine
        .search(SearchRequest {
            query: "warmup_function_benchmark".to_string(),
            token_budget: DEFAULT_SEARCH_TOKEN_BUDGET,
            max_results: SEARCH_MAX_RESULTS,
        })
        .await?;

    let mut search_latencies_ms = Vec::with_capacity(search_samples);
    let mut search_latencies_by_kind = [Vec::new(), Vec::new(), Vec::new()];
    let mut search_results_total = 0_usize;
    for sample in 0..search_samples {
        let query_kind = sample % 3;
        let started = Instant::now();
        let response = engine
            .search(SearchRequest {
                query: benchmark_query(sample, file_count),
                token_budget: DEFAULT_SEARCH_TOKEN_BUDGET,
                max_results: SEARCH_MAX_RESULTS,
            })
            .await?;
        let latency = elapsed_ms(started);
        search_latencies_ms.push(latency);
        search_latencies_by_kind[query_kind].push(latency);
        search_results_total = search_results_total.saturating_add(response.results.len());
    }
    let after_search = read_process_memory();
    engine.close().await;

    let mut incremental_latencies_ms = Vec::with_capacity(incremental_samples);
    let mut first_incremental = None;
    for sample in 0..incremental_samples {
        let file_index = sample % file_count;
        let path = source_path(&root, file_index);
        fs::write(
            &path,
            synthetic_source(file_index, &format!("update_{sample}")),
        )?;
        let started = Instant::now();
        let report = update_paths(
            IndexUpdateOptions {
                root: root.clone(),
                db_path: Some(db_path.clone()),
                ..IndexUpdateOptions::new(&root)
            },
            &[path],
        )
        .await?;
        let measured = started.elapsed();
        anyhow::ensure!(
            report.files_indexed == 1 && report.errors.is_empty(),
            "incremental update failed: {report:?}"
        );
        let latency = duration_ms(measured);
        incremental_latencies_ms.push(latency);
        if first_incremental.is_none() {
            first_incremental = Some((measured, report));
        }
    }

    let first_update_ms = incremental_latencies_ms[0];
    let (first_elapsed, first_report) =
        first_incremental.ok_or_else(|| anyhow::anyhow!("missing incremental sample"))?;
    let search_p95_ms = percentile(&search_latencies_ms, 95.0);
    let incremental_p95_ms = percentile(&incremental_latencies_ms, 95.0);
    anyhow::ensure!(
        search_p95_ms < 200.0,
        "query P95 gate failed: {search_p95_ms:.3} ms"
    );
    anyhow::ensure!(
        incremental_p95_ms < 2_000.0,
        "incremental-update P95 gate failed: {incremental_p95_ms:.3} ms"
    );
    println!("files={file_count}");
    println!("full_index_ms={}", measured_full.as_millis());
    println!(
        "full_index_reported_ms={} chunks={}",
        full.elapsed_ms, full.chunks_written
    );
    println!(
        "search_samples={} search_results_total={} search_p50_ms={:.3} search_p95_ms={:.3} search_max_ms={:.3} search_avg_ms={:.3}",
        search_latencies_ms.len(),
        search_results_total,
        percentile(&search_latencies_ms, 50.0),
        search_p95_ms,
        max_latency(&search_latencies_ms),
        average_latency(&search_latencies_ms),
    );
    print_search_kind_latencies("symbol", &search_latencies_by_kind[0]);
    print_search_kind_latencies("path", &search_latencies_by_kind[1]);
    print_search_kind_latencies("natural", &search_latencies_by_kind[2]);
    println!(
        "incremental_samples={} incremental_p50_ms={:.3} incremental_p95_ms={:.3} incremental_max_ms={:.3} incremental_avg_ms={:.3}",
        incremental_latencies_ms.len(),
        percentile(&incremental_latencies_ms, 50.0),
        incremental_p95_ms,
        max_latency(&incremental_latencies_ms),
        average_latency(&incremental_latencies_ms),
    );
    println!("single_file_update_ms={}", first_elapsed.as_millis());
    println!(
        "single_file_update_reported_ms={} files_indexed={} chunks={}",
        first_report.elapsed_ms, first_report.files_indexed, first_report.chunks_written
    );
    println!(
        "speedup={:.2}x",
        measured_full.as_secs_f64() / (first_update_ms / 1_000.0).max(f64::EPSILON)
    );
    println!("index_size_mib={:.3}", bytes_to_mib(index_size_bytes));
    print_memory("before_index", before_index);
    print_memory("after_index", after_index);
    print_memory("after_search", after_search);
    Ok(())
}

fn benchmark_query(sample: usize, file_count: usize) -> String {
    let file_index = sample % file_count;
    match sample % 3 {
        0 => format!("function_{file_index}_0_original"),
        1 => format!("src/module_{:03}/file_{file_index:05}.rs", file_index / 20),
        _ => format!("function value index {file_index}"),
    }
}

fn source_path(root: &Path, index: usize) -> PathBuf {
    root.join(format!("src/module_{:03}", index / 20))
        .join(format!("file_{index:05}.rs"))
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
    duration_ms(started.elapsed())
}

fn duration_ms(duration: std::time::Duration) -> f64 {
    duration.as_secs_f64() * 1_000.0
}

fn percentile(samples_ms: &[f64], percentile: f64) -> f64 {
    let mut sorted = samples_ms.to_vec();
    sorted.sort_by(f64::total_cmp);
    let rank = ((sorted.len().saturating_sub(1)) as f64 * percentile / 100.0).ceil() as usize;
    sorted[rank.min(sorted.len() - 1)]
}

fn max_latency(samples_ms: &[f64]) -> f64 {
    samples_ms
        .iter()
        .copied()
        .max_by(f64::total_cmp)
        .unwrap_or_default()
}

fn average_latency(samples_ms: &[f64]) -> f64 {
    samples_ms.iter().sum::<f64>() / samples_ms.len() as f64
}

fn print_search_kind_latencies(kind: &str, samples_ms: &[f64]) {
    if samples_ms.is_empty() {
        return;
    }
    println!(
        "search_{kind}_samples={} search_{kind}_p50_ms={:.3} search_{kind}_p95_ms={:.3} search_{kind}_max_ms={:.3}",
        samples_ms.len(),
        percentile(samples_ms, 50.0),
        percentile(samples_ms, 95.0),
        max_latency(samples_ms),
    );
}

fn index_size_bytes(db_path: &Path) -> u64 {
    ["", "-wal", "-shm"]
        .into_iter()
        .filter_map(|suffix| {
            let mut path = db_path.as_os_str().to_owned();
            path.push(suffix);
            fs::metadata(PathBuf::from(path)).ok()
        })
        .map(|metadata| metadata.len())
        .sum()
}

fn bytes_to_mib(bytes: u64) -> f64 {
    bytes as f64 / (1024.0 * 1024.0)
}

#[derive(Clone, Copy)]
struct ProcessMemory {
    rss_mib: f64,
    peak_rss_mib: f64,
}

fn print_memory(stage: &str, memory: Option<ProcessMemory>) {
    let (rss, peak) = memory.map_or((f64::NAN, f64::NAN), |memory| {
        (memory.rss_mib, memory.peak_rss_mib)
    });
    println!("memory_{stage}_rss_mib={rss:.3} peak_rss_mib={peak:.3}");
}

fn read_process_memory() -> Option<ProcessMemory> {
    #[cfg(target_os = "linux")]
    {
        let status = fs::read_to_string("/proc/self/status").ok()?;
        let rss_mib = read_status_kib(&status, "VmRSS:")? / 1024.0;
        let peak_rss_mib = read_status_kib(&status, "VmHWM:").unwrap_or(rss_mib * 1024.0) / 1024.0;
        Some(ProcessMemory {
            rss_mib,
            peak_rss_mib,
        })
    }
    #[cfg(not(target_os = "linux"))]
    {
        None
    }
}

#[cfg(target_os = "linux")]
fn read_status_kib(status: &str, field: &str) -> Option<f64> {
    status
        .lines()
        .find_map(|line| line.strip_prefix(field))
        .and_then(|value| value.split_whitespace().next())
        .and_then(|value| value.parse::<f64>().ok())
}
