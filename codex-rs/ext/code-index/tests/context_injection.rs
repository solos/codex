use std::fs;
use std::path::Path;
use std::path::PathBuf;

use codex_code_index::IndexInitOptions;
use codex_code_index::init_index;
use codex_code_index_extension::CodeIndexExtensionConfig;
use codex_code_index_extension::install;
use codex_core::config::Config;
use codex_exec_server::LOCAL_FS;
use codex_extension_api::ContextualUserFragment;
use codex_extension_api::ExtensionData;
use codex_extension_api::ExtensionRegistry;
use codex_extension_api::ExtensionRegistryBuilder;
use codex_extension_api::TurnInputContext;
use codex_extension_api::TurnInputEnvironment;
use codex_file_system::FileSystemEnvironmentAccessor;
use codex_protocol::models::ContentItem;
use codex_protocol::models::ResponseItem;
use codex_protocol::user_input::UserInput;
use codex_utils_path_uri::PathUri;
use codex_utils_string::approx_token_count;
use tempfile::TempDir;

#[tokio::test]
async fn enabled_extension_injects_indexed_code_for_the_user_task() -> anyhow::Result<()> {
    let fixture = IndexedFixture::new().await?;
    let registry = registry_with_config(fixture.config(1_000)).await;
    let thread_store = ExtensionData::new("thread");
    thread_store.insert(fixture.config(1_000));

    let fragments = contribute(
        &registry,
        &fixture.root,
        "Counter increment_counter",
        &thread_store,
    )
    .await?;
    let [text] = fragments.as_slice() else {
        panic!("expected one code-index context fragment");
    };

    assert!(text.contains(r#"<codex_internal_context source="code_index">"#));
    assert!(text.contains("src/counter.rs"));
    assert!(text.contains("increment_counter"));
    Ok(())
}

#[tokio::test]
async fn disabled_extension_is_a_noop() -> anyhow::Result<()> {
    let temp = TempDir::new()?;
    let root = temp.path().join("repo");
    fs::create_dir_all(&root)?;
    let registry = registry_with_config(CodeIndexExtensionConfig::default()).await;
    let thread_store = ExtensionData::new("thread");
    thread_store.insert(CodeIndexExtensionConfig::default());

    let fragments = contribute(&registry, &root, "find anything", &thread_store).await?;

    assert!(fragments.is_empty());
    Ok(())
}

#[tokio::test]
async fn missing_index_falls_back_to_file_search() -> anyhow::Result<()> {
    let temp = TempDir::new()?;
    let root = temp.path().join("repo");
    fs::create_dir_all(root.join("src"))?;
    fs::write(
        root.join("src/counter.rs"),
        "pub fn increment_counter() {}\n",
    )?;
    let config = CodeIndexExtensionConfig {
        enabled: true,
        index_path: Some(temp.path().join("missing.sqlite")),
        ..CodeIndexExtensionConfig::default()
    };
    let registry = registry_with_config(config.clone()).await;
    let thread_store = ExtensionData::new("thread");
    thread_store.insert(config);

    let fragments = contribute(&registry, &root, "counter", &thread_store).await?;

    let [text] = fragments.as_slice() else {
        panic!("expected one fallback file-search context fragment");
    };
    assert!(text.contains("fallback file search"));
    assert!(text.contains("src/counter.rs"));
    Ok(())
}

#[tokio::test]
async fn fallback_search_does_not_expose_ignored_or_sensitive_files() -> anyhow::Result<()> {
    let temp = TempDir::new()?;
    let root = temp.path().join("repo");
    fs::create_dir_all(root.join("src"))?;
    fs::write(root.join(".env"), "SECRET_TOKEN=do-not-index\n")?;
    fs::write(root.join(".codexignore"), "ignored_secret.rs\n")?;
    fs::write(
        root.join("ignored_secret.rs"),
        "pub fn ignored_secret() {}\n",
    )?;
    fs::write(root.join("src/counter.rs"), "pub fn counter() {}\n")?;
    let config = CodeIndexExtensionConfig {
        enabled: true,
        index_path: Some(temp.path().join("missing.sqlite")),
        ..CodeIndexExtensionConfig::default()
    };
    let registry = registry_with_config(config.clone()).await;
    let thread_store = ExtensionData::new("thread");
    thread_store.insert(config);

    for query in ["SECRET_TOKEN", "ignored_secret", ".env"] {
        let fragments = contribute(&registry, &root, query, &thread_store).await?;
        assert!(
            fragments
                .iter()
                .all(|text| !text.contains("ignored_secret.rs") && !text.contains(".env")),
            "fallback search exposed a private path for query {query:?}: {fragments:?}"
        );
    }
    Ok(())
}

#[tokio::test]
async fn injected_context_respects_the_configured_token_budget() -> anyhow::Result<()> {
    let fixture = IndexedFixture::new().await?;
    let config = fixture.config(80);
    let registry = registry_with_config(config.clone()).await;
    let thread_store = ExtensionData::new("thread");
    thread_store.insert(config);

    let fragments = contribute(
        &registry,
        &fixture.root,
        "Counter increment_counter implementation",
        &thread_store,
    )
    .await?;
    let [text] = fragments.as_slice() else {
        panic!("expected one code-index context fragment");
    };

    assert!(
        approx_token_count(text) <= 80,
        "injected context used {} tokens: {text}",
        approx_token_count(text)
    );
    Ok(())
}

struct IndexedFixture {
    _temp: TempDir,
    root: PathBuf,
    db_path: PathBuf,
}

impl IndexedFixture {
    async fn new() -> anyhow::Result<Self> {
        let temp = TempDir::new()?;
        let root = temp.path().join("repo");
        fs::create_dir_all(root.join(".git"))?;
        fs::create_dir_all(root.join("src"))?;
        fs::write(
            root.join("src/counter.rs"),
            "pub struct Counter { value: i64 }\n\
             impl Counter {\n\
                 pub fn increment_counter(&mut self) -> i64 {\n\
                     self.value += 1;\n\
                     self.value\n\
                 }\n\
             }\n",
        )?;
        let db_path = temp.path().join("index.sqlite");
        init_index(IndexInitOptions {
            root: root.clone(),
            db_path: Some(db_path.clone()),
            ..IndexInitOptions::new(".")
        })
        .await?;
        Ok(Self {
            _temp: temp,
            root,
            db_path,
        })
    }

    fn config(&self, token_budget: usize) -> CodeIndexExtensionConfig {
        CodeIndexExtensionConfig {
            enabled: true,
            token_budget,
            max_results: 8,
            index_path: Some(self.db_path.clone()),
            embedding: false,
            debug: false,
        }
    }
}

async fn registry_with_config(config: CodeIndexExtensionConfig) -> ExtensionRegistry<Config> {
    let mut builder = ExtensionRegistryBuilder::<Config>::new();
    install(&mut builder, move |_| config.clone());
    builder.build()
}

async fn contribute(
    registry: &ExtensionRegistry<Config>,
    root: &Path,
    query: &str,
    thread_store: &ExtensionData,
) -> anyhow::Result<Vec<String>> {
    let session_store = ExtensionData::new("session");
    let turn_store = ExtensionData::new("turn");
    let step_store = ExtensionData::new("step");
    let cwd = PathUri::from_host_native_path(root)?;
    let fragments = registry.turn_input_contributors()[0]
        .contribute(
            TurnInputContext {
                step_store: &step_store,
                turn_id: "turn-1".to_string(),
                user_input: vec![UserInput::Text {
                    text: query.to_string(),
                    text_elements: Vec::new(),
                }],
                environments: vec![TurnInputEnvironment {
                    environment_id: "local".to_string(),
                    cwd,
                    is_primary: true,
                    fs: &FileSystemEnvironmentAccessor::unrestricted(&LOCAL_FS),
                }],
            },
            /*extension_metrics*/ None,
            &session_store,
            thread_store,
            &turn_store,
        )
        .await;
    Ok(fragments.into_iter().map(fragment_text).collect::<Vec<_>>())
}

fn fragment_text(fragment: Box<dyn ContextualUserFragment + Send>) -> String {
    let item = fragment.into_boxed_response_item();
    let ResponseItem::Message { content, .. } = item else {
        panic!("expected message content");
    };
    content
        .into_iter()
        .filter_map(|item| match item {
            ContentItem::InputText { text } => Some(text),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}
