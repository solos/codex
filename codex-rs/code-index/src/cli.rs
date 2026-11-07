use std::future::Future;
use std::path::Path;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use anyhow::Result;
use clap::Args;
use clap::Subcommand;
use codex_file_watcher::FileWatcher;
use codex_utils_process::background_command;

use crate::indexer::IndexControl;
use crate::indexer::IndexInitOptions;
use crate::indexer::IndexReport;
use crate::indexer::IndexStatus;
use crate::indexer::IndexStatusOptions;
use crate::indexer::IndexUpdateOptions;
use crate::indexer::index_status;
use crate::indexer::init_index_with_control;
use crate::indexer::resolve_index_paths;
use crate::indexer::update_index_with_control;
use crate::search::DEFAULT_SEARCH_RESULT_LIMIT;
use crate::search::DEFAULT_SEARCH_TOKEN_BUDGET;
use crate::search::IndexSearchOptions;
use crate::search::SearchResponse;
use crate::search::search_index;
use crate::watch::IndexWatchOptions;
use crate::watch::IndexWatcher;

/// Manage the local code index.
#[derive(Debug, Args)]
pub struct IndexCommand {
    #[command(subcommand)]
    pub subcommand: IndexSubcommand,
}

#[derive(Debug, Subcommand)]
pub enum IndexSubcommand {
    /// Build or incrementally refresh an index.
    Init(IndexInitArgs),
    /// Reconcile the index with the current filesystem.
    Update(IndexUpdateArgs),
    /// Watch the repository and incrementally update changed files.
    Watch(IndexWatchArgs),
    /// Show index metadata and counts.
    Status(IndexStatusArgs),
    /// Search indexed code with hybrid lexical, symbol, and optional vector ranking.
    Search(IndexSearchArgs),
}

#[derive(Debug, Args)]
pub struct IndexInitArgs {
    /// Repository or directory to index.
    #[arg(value_name = "PATH", default_value = ".")]
    pub path: PathBuf,

    /// Override the SQLite database path.
    #[arg(long, value_name = "PATH")]
    pub db: Option<PathBuf>,

    /// Print a machine-readable report.
    #[arg(long, default_value_t = false)]
    pub json: bool,

    /// Return immediately and finish indexing in a detached child process.
    #[arg(long, default_value_t = false)]
    pub background: bool,

    /// Maximum concurrent parser workers. Zero selects a bounded automatic value.
    #[arg(long, value_name = "N", default_value_t = crate::indexer::DEFAULT_PARSE_CONCURRENCY)]
    pub jobs: usize,
}

#[derive(Debug, Args)]
pub struct IndexStatusArgs {
    /// Repository or directory whose index should be inspected.
    #[arg(value_name = "PATH", default_value = ".")]
    pub path: PathBuf,

    /// Override the SQLite database path.
    #[arg(long, value_name = "PATH")]
    pub db: Option<PathBuf>,

    /// Print machine-readable status.
    #[arg(long, default_value_t = false)]
    pub json: bool,
}

#[derive(Debug, Args)]
pub struct IndexUpdateArgs {
    /// Repository or directory to update.
    #[arg(value_name = "PATH", default_value = ".")]
    pub path: PathBuf,

    /// Override the SQLite database path.
    #[arg(long, value_name = "PATH")]
    pub db: Option<PathBuf>,

    /// Print a machine-readable report.
    #[arg(long, default_value_t = false)]
    pub json: bool,

    /// Maximum concurrent parser workers. Zero selects a bounded automatic value.
    #[arg(long, value_name = "N", default_value_t = crate::indexer::DEFAULT_PARSE_CONCURRENCY)]
    pub jobs: usize,
}

#[derive(Debug, Args)]
pub struct IndexWatchArgs {
    /// Repository or directory to watch.
    #[arg(value_name = "PATH", default_value = ".")]
    pub path: PathBuf,

    /// Override the SQLite database path.
    #[arg(long, value_name = "PATH")]
    pub db: Option<PathBuf>,

    /// Debounce window for filesystem events, in milliseconds.
    #[arg(long, default_value_t = 200)]
    pub debounce_ms: u64,

    /// Print one machine-readable report per line.
    #[arg(long, default_value_t = false)]
    pub json: bool,

    /// Maximum concurrent parser workers. Zero selects a bounded automatic value.
    #[arg(long, value_name = "N", default_value_t = crate::indexer::DEFAULT_PARSE_CONCURRENCY)]
    pub jobs: usize,
}

#[derive(Debug, Args)]
pub struct IndexSearchArgs {
    /// Code search query.
    #[arg(value_name = "QUERY")]
    pub query: String,

    /// Repository or directory whose index should be searched.
    #[arg(long, value_name = "PATH", default_value = ".")]
    pub path: PathBuf,

    /// Override the SQLite database path.
    #[arg(long, value_name = "PATH")]
    pub db: Option<PathBuf>,

    /// Maximum approximate tokens of code context to return.
    #[arg(long, value_name = "N", default_value_t = DEFAULT_SEARCH_TOKEN_BUDGET)]
    pub budget: usize,

    /// Maximum number of result chunks after fusion.
    #[arg(long, value_name = "N", default_value_t = DEFAULT_SEARCH_RESULT_LIMIT)]
    pub limit: usize,

    /// Print machine-readable results.
    #[arg(long, default_value_t = false)]
    pub json: bool,
}

pub async fn run(command: IndexCommand) -> Result<()> {
    run_with_codex_exe(command, None).await
}

pub async fn run_with_codex_exe(command: IndexCommand, codex_exe: Option<&Path>) -> Result<()> {
    match command.subcommand {
        IndexSubcommand::Init(args) => {
            if args.background {
                let report = spawn_background_init(&args, codex_exe)?;
                if args.json {
                    println!("{}", serde_json::to_string_pretty(&report)?);
                } else {
                    print_background_report(&report);
                }
                return Ok(());
            }
            let options = IndexInitOptions {
                root: args.path,
                db_path: args.db,
                parse_concurrency: args.jobs,
                ..IndexInitOptions::new(".")
            };
            let control = progress_control(args.json);
            let report =
                run_interruptibly(init_index_with_control(options, control.clone()), control)
                    .await?;
            if args.json {
                println!("{}", serde_json::to_string_pretty(&report)?);
            } else {
                print_init_report(&report);
            }
        }
        IndexSubcommand::Status(args) => {
            let status = index_status(IndexStatusOptions {
                root: args.path,
                db_path: args.db,
            })
            .await?;
            if args.json {
                println!("{}", serde_json::to_string_pretty(&status)?);
            } else {
                print_status(&status);
            }
        }
        IndexSubcommand::Update(args) => {
            let options = IndexUpdateOptions {
                root: args.path,
                db_path: args.db,
                parse_concurrency: args.jobs,
                ..IndexUpdateOptions::new(".")
            };
            let control = progress_control(args.json);
            let report =
                run_interruptibly(update_index_with_control(options, control.clone()), control)
                    .await?;
            if args.json {
                println!("{}", serde_json::to_string_pretty(&report)?);
            } else {
                print_init_report(&report);
            }
        }
        IndexSubcommand::Watch(args) => {
            let file_watcher = Arc::new(
                FileWatcher::new().context("failed to initialize the code index watcher")?,
            );
            let options = IndexWatchOptions {
                root: args.path,
                db_path: args.db,
                debounce: Duration::from_millis(args.debounce_ms),
                parse_concurrency: args.jobs,
                ..IndexWatchOptions::new(".")
            };
            let (mut watcher, initial) = IndexWatcher::start(options, file_watcher).await?;
            if args.json {
                println!("{}", serde_json::to_string(&initial)?);
            } else {
                print_init_report(&initial);
                println!("Watching for changes (press Ctrl-C to stop)");
            }

            loop {
                let update = tokio::select! {
                    signal = tokio::signal::ctrl_c() => {
                        signal?;
                        break;
                    }
                    update = watcher.next_update() => update,
                };
                let Some(update) = update else {
                    break;
                };
                match update {
                    Ok(report) if args.json => {
                        println!("{}", serde_json::to_string(&report)?);
                    }
                    Ok(report) => print_update_report(&report),
                    Err(error) => eprintln!("Index update failed; will retry: {error:#}"),
                }
            }
        }
        IndexSubcommand::Search(args) => {
            let mut options = IndexSearchOptions::new(args.path, args.query);
            options.db_path = args.db;
            options.token_budget = args.budget;
            options.max_results = args.limit;
            let response = search_index(options).await?;
            if args.json {
                println!("{}", serde_json::to_string_pretty(&response)?);
            } else {
                print_search_response(&response);
            }
        }
    }
    Ok(())
}

fn progress_control(json: bool) -> IndexControl {
    IndexControl::default().with_progress(move |progress| {
        if json {
            return;
        }
        eprintln!(
            "Indexing {}/{} files: {} indexed, {} unchanged, {} errors, {} ms{}",
            progress.files_processed,
            progress.files_total,
            progress.files_indexed,
            progress.files_unchanged,
            progress.read_errors,
            progress.elapsed_ms,
            if progress.cancelled {
                " (cancelled)"
            } else {
                ""
            }
        );
    })
}

async fn run_interruptibly<F, T>(future: F, control: IndexControl) -> Result<T>
where
    F: Future<Output = Result<T>>,
{
    tokio::pin!(future);
    let mut interrupted = false;
    loop {
        tokio::select! {
            result = &mut future => return result,
            signal = tokio::signal::ctrl_c(), if !interrupted => {
                signal?;
                interrupted = true;
                control.cancel();
                eprintln!("Interrupt received; waiting for in-flight parser workers to stop...");
            }
        }
    }
}

#[derive(Debug, Clone, serde::Serialize)]
struct BackgroundIndexReport {
    pid: u32,
    root_path: String,
    db_path: String,
    log_path: String,
}

fn spawn_background_init(
    args: &IndexInitArgs,
    configured_codex_exe: Option<&Path>,
) -> Result<BackgroundIndexReport> {
    let codex_exe = match configured_codex_exe {
        Some(path) => path.to_path_buf(),
        None => std::env::current_exe()
            .context("failed to locate the Codex executable for background indexing")?,
    };
    let paths = resolve_index_paths(&args.path, args.db.as_deref())?;
    if let Some(parent) = paths.db_path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("failed to create index directory {}", parent.display()))?;
    }
    let log_path = paths.db_path.with_file_name(format!(
        "{}.init.log",
        paths
            .db_path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("index.sqlite")
    ));
    let log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
        .with_context(|| format!("failed to open index log {}", log_path.display()))?;

    let child = background_command(&codex_exe)
        .arg("index")
        .arg("init")
        .arg(&paths.root)
        .arg("--db")
        .arg(&paths.db_path)
        .arg("--json")
        .arg("--jobs")
        .arg(args.jobs.to_string())
        .stdin(Stdio::null())
        .stdout(Stdio::from(log.try_clone()?))
        .stderr(Stdio::from(log))
        .spawn()
        .with_context(|| {
            format!(
                "failed to start background index with {}",
                codex_exe.display()
            )
        })?;

    Ok(BackgroundIndexReport {
        pid: child.id(),
        root_path: paths.root.to_string_lossy().into_owned(),
        db_path: paths.db_path.to_string_lossy().into_owned(),
        log_path: log_path.to_string_lossy().into_owned(),
    })
}

fn print_init_report(report: &IndexReport) {
    println!("Indexed {}", report.root_path);
    println!("Database: {}", report.db_path);
    println!(
        "Files: {} indexed, {} unchanged, {} deleted ({} discovered)",
        report.files_indexed, report.files_unchanged, report.files_deleted, report.files_seen
    );
    println!("Chunks written: {}", report.chunks_written);
    println!(
        "Skipped: {} sensitive, {} large, {} binary, {} non-UTF-8",
        report.skipped_sensitive,
        report.skipped_large,
        report.skipped_binary,
        report.skipped_non_utf8
    );
    if report.read_errors > 0 || report.walk_errors > 0 {
        println!(
            "Warnings: {} file errors, {} walk errors",
            report.read_errors, report.walk_errors
        );
        for error in &report.errors {
            println!("  {error}");
        }
    }
    println!("Elapsed: {} ms", report.elapsed_ms);
    if report.cancelled {
        println!("Interrupted: completed batches were committed; deletions were skipped");
    }
}

fn print_update_report(report: &IndexReport) {
    println!(
        "Updated {} paths changed: {} files indexed, {} unchanged, {} deleted, {} chunks written in {} ms",
        report.changed_paths,
        report.files_indexed,
        report.files_unchanged,
        report.files_deleted,
        report.chunks_written,
        report.elapsed_ms
    );
    if !report.errors.is_empty() {
        for error in &report.errors {
            println!("  {error}");
        }
    }
    if report.cancelled {
        println!("Interrupted: completed path updates were committed");
    }
}

fn print_background_report(report: &BackgroundIndexReport) {
    println!("Background index started (pid {})", report.pid);
    println!("Index root: {}", report.root_path);
    println!("Database: {}", report.db_path);
    println!("Log: {}", report.log_path);
}

fn print_status(status: &IndexStatus) {
    println!("Index root: {}", status.root_path);
    println!("Database: {}", status.db_path);
    if !status.initialized {
        println!("Status: not initialized");
        return;
    }
    println!(
        "Files: {} | Chunks: {}",
        status.file_count, status.chunk_count
    );
    if let Some(last_indexed) = status.last_indexed_unix_ms {
        println!("Last indexed (unix ms): {last_indexed}");
    }
    if !status.languages.is_empty() {
        let languages = status
            .languages
            .iter()
            .map(|(language, count)| format!("{language}={count}"))
            .collect::<Vec<_>>()
            .join(", ");
        println!("Languages: {languages}");
    }
}

fn print_search_response(response: &SearchResponse) {
    if response.results.is_empty() {
        println!("No results for {:?}", response.query);
        return;
    }
    for (index, result) in response.results.iter().enumerate() {
        let symbol = result
            .symbol
            .as_deref()
            .or(result.parent_symbol.as_deref())
            .unwrap_or(result.kind.as_str());
        println!(
            "{}. {}:{}-{} {} score={:.6}{}",
            index + 1,
            result.path,
            result.line,
            result.end_line,
            symbol,
            result.score,
            if result.truncated { " (truncated)" } else { "" }
        );
        print!("{}", result.content);
        if !result.content.ends_with('\n') {
            println!();
        }
        println!();
    }
    println!(
        "{} results, {} approx. tokens{}",
        response.results.len(),
        response.total_tokens,
        if response.truncated {
            " (budget limit reached)"
        } else {
            ""
        }
    );
}

#[cfg(test)]
mod tests {
    use clap::Parser;
    use pretty_assertions::assert_eq;

    use super::*;

    #[derive(Debug, Parser)]
    struct TestCli {
        #[command(flatten)]
        index: IndexCommand,
    }

    #[test]
    fn index_init_and_status_parse() {
        let cli = TestCli::try_parse_from([
            "test",
            "init",
            "/tmp/repo",
            "--db",
            "/tmp/index.sqlite",
            "--json",
            "--background",
        ])
        .expect("parse index init");
        match cli.index.subcommand {
            IndexSubcommand::Init(args) => {
                assert_eq!(args.path, PathBuf::from("/tmp/repo"));
                assert_eq!(args.db, Some(PathBuf::from("/tmp/index.sqlite")));
                assert!(args.json);
                assert!(args.background);
            }
            other => panic!("unexpected subcommand: {other:?}"),
        }

        let cli =
            TestCli::try_parse_from(["test", "status", ".", "--json"]).expect("parse index status");
        match cli.index.subcommand {
            IndexSubcommand::Status(args) => {
                assert_eq!(args.path, PathBuf::from("."));
                assert_eq!(args.db, None);
                assert!(args.json);
            }
            other => panic!("unexpected subcommand: {other:?}"),
        }

        let cli = TestCli::try_parse_from(["test", "update", "/tmp/repo", "--json"])
            .expect("parse index update");
        match cli.index.subcommand {
            IndexSubcommand::Update(args) => {
                assert_eq!(args.path, PathBuf::from("/tmp/repo"));
                assert_eq!(args.db, None);
                assert!(args.json);
            }
            other => panic!("unexpected subcommand: {other:?}"),
        }

        let cli = TestCli::try_parse_from([
            "test",
            "watch",
            "/tmp/repo",
            "--db",
            "/tmp/index.sqlite",
            "--debounce-ms",
            "75",
            "--json",
        ])
        .expect("parse index watch");
        match cli.index.subcommand {
            IndexSubcommand::Watch(args) => {
                assert_eq!(args.path, PathBuf::from("/tmp/repo"));
                assert_eq!(args.db, Some(PathBuf::from("/tmp/index.sqlite")));
                assert_eq!(args.debounce_ms, 75);
                assert!(args.json);
            }
            other => panic!("unexpected subcommand: {other:?}"),
        }

        let cli = TestCli::try_parse_from([
            "test",
            "search",
            "increment counter",
            "--path",
            "/tmp/repo",
            "--budget",
            "1200",
            "--limit",
            "5",
            "--json",
        ])
        .expect("parse index search");
        match cli.index.subcommand {
            IndexSubcommand::Search(args) => {
                assert_eq!(args.query, "increment counter");
                assert_eq!(args.path, PathBuf::from("/tmp/repo"));
                assert_eq!(args.budget, 1200);
                assert_eq!(args.limit, 5);
                assert!(args.json);
            }
            other => panic!("unexpected subcommand: {other:?}"),
        }
    }
}
