use std::fs;
use std::path::Path;

use codex_code_index::IndexInitOptions;
use codex_code_index::IndexSearchOptions;
use codex_code_index::QueryIntent;
use codex_code_index::SearchResponse;
use codex_code_index::init_index;
use codex_code_index::search_index;
use pretty_assertions::assert_eq;
use tempfile::tempdir;

#[tokio::test]
async fn symbol_natural_language_and_path_queries_return_relevant_chunks() -> anyhow::Result<()> {
    let temp = tempdir().expect("tempdir");
    let root = temp.path().join("repo");
    fs::create_dir_all(root.join(".git")).expect("git dir");
    fs::create_dir_all(root.join("src")).expect("src dir");
    fs::write(
        root.join("src/lib.rs"),
        r#"
pub struct Counter {
    value: usize,
}

impl Counter {
    pub fn increment_counter(&mut self) -> usize {
        self.value += 1;
        self.value
    }
}

pub fn calculate_retry_delay(attempt: usize) -> usize {
    let strategy = "exponential backoff retry";
    attempt * strategy.len()
}
"#,
    )
    .expect("rust source");
    fs::write(
        root.join("src/widget.ts"),
        r#"
export class Widget {
    render(label: string) {
        return `<span>${label}</span>`;
    }
}
"#,
    )
    .expect("typescript source");

    let db_path = temp.path().join("index.sqlite");
    init_index(IndexInitOptions {
        root: root.clone(),
        db_path: Some(db_path.clone()),
        ..IndexInitOptions::new(&root)
    })
    .await
    .expect("index fixture");

    let symbol_query = search(&root, &db_path, "increment_counter").await?;
    assert_eq!(symbol_query.intent, QueryIntent::Symbol);
    assert!(
        symbol_query.results.iter().take(3).any(|result| {
            result.path == "src/lib.rs"
                && result.symbol.as_deref() == Some("increment_counter")
                && result.line > 0
        }),
        "symbol query should find the exact function"
    );

    let second_symbol_query = search(&root, &db_path, "calculate_retry_delay").await?;
    assert_eq!(second_symbol_query.intent, QueryIntent::Symbol);
    assert!(
        second_symbol_query
            .results
            .iter()
            .take(3)
            .any(|result| result.symbol.as_deref() == Some("calculate_retry_delay")),
        "second symbol query should find the exact function"
    );

    let natural_query = search(
        &root,
        &db_path,
        "where is exponential backoff retry implemented",
    )
    .await?;
    assert_eq!(natural_query.intent, QueryIntent::NaturalLanguage);
    assert!(
        natural_query.results.iter().take(5).any(|result| {
            result.path == "src/lib.rs" && result.symbol.as_deref() == Some("calculate_retry_delay")
        }),
        "natural-language query should find the retry implementation"
    );

    let second_natural_query = search(&root, &db_path, "increment the counter value").await?;
    assert_eq!(second_natural_query.intent, QueryIntent::NaturalLanguage);
    assert!(
        second_natural_query
            .results
            .iter()
            .take(5)
            .any(|result| result.symbol.as_deref() == Some("increment_counter")),
        "second natural-language query should find the counter behavior"
    );

    let path_query = search(&root, &db_path, "src/widget.ts render").await?;
    assert_eq!(path_query.intent, QueryIntent::Path);
    assert!(
        path_query
            .results
            .first()
            .is_some_and(|result| result.path == "src/widget.ts"),
        "path query should rank the named file first"
    );

    let second_path_query = search(&root, &db_path, "src/lib.rs calculate_retry_delay").await?;
    assert_eq!(second_path_query.intent, QueryIntent::Path);
    assert!(
        second_path_query
            .results
            .first()
            .is_some_and(|result| result.path == "src/lib.rs"),
        "second path query should rank the named file first"
    );

    let budget_query = search_with_budget(&root, &db_path, "retry", 12).await?;
    assert!(budget_query.total_tokens <= 12);
    for pair in budget_query.results.windows(2) {
        if pair[0].path == pair[1].path && pair[0].symbol == pair[1].symbol {
            assert!(
                pair[0].end_line.saturating_add(1) < pair[1].line,
                "adjacent chunks should be merged before returning results"
            );
        }
    }
    Ok(())
}

async fn search(root: &Path, db_path: &Path, query: &str) -> anyhow::Result<SearchResponse> {
    search_with_budget(
        root,
        db_path,
        query,
        codex_code_index::DEFAULT_SEARCH_TOKEN_BUDGET,
    )
    .await
}

async fn search_with_budget(
    root: &Path,
    db_path: &Path,
    query: &str,
    token_budget: usize,
) -> anyhow::Result<SearchResponse> {
    let mut options = IndexSearchOptions::new(root, query);
    options.db_path = Some(db_path.to_path_buf());
    options.token_budget = token_budget;
    search_index(options).await
}
