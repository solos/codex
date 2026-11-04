use std::sync::Arc;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use std::time::Duration;

use codex_config::LoaderOverrides;
use codex_core::config::ConfigBuilder;
use codex_core::config::ConfigOverrides;
use codex_feedback::CodexFeedback;
use codex_http_client::HttpClientFactory;
use codex_models_manager::manager::ModelsEndpointClient;
use codex_models_manager::manager::ModelsEndpointFuture;
use codex_models_manager::manager::ModelsEndpointResponse;
use codex_models_manager::manager::OpenAiModelsManager;
use codex_models_manager::manager::SharedModelsManager;
use codex_protocol::error::CodexErr;
use codex_protocol::error::Result as CoreResult;
use codex_utils_absolute_path::AbsolutePathBuf;
use pretty_assertions::assert_eq;
use tempfile::tempdir;
use tokio::sync::Notify;
use tokio::sync::mpsc;
use tokio::sync::oneshot;
use tracing_subscriber::layer::SubscriberExt;

use super::*;

#[derive(Debug)]
struct TestModelsEndpoint {
    fetch_count: AtomicUsize,
    fetched: Notify,
    release_second_fetch: Notify,
}

impl TestModelsEndpoint {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            fetch_count: AtomicUsize::new(0),
            fetched: Notify::new(),
            release_second_fetch: Notify::new(),
        })
    }

    async fn wait_for_fetch_count(&self, expected: usize) {
        tokio::time::timeout(Duration::from_secs(1), async {
            while self.fetch_count.load(Ordering::SeqCst) < expected {
                self.fetched.notified().await;
            }
        })
        .await
        .unwrap_or_else(|_| panic!("expected {expected} model fetches"));
    }
}

impl ModelsEndpointClient for TestModelsEndpoint {
    fn identity(&self) -> Option<String> {
        Some("test-provider".to_string())
    }

    fn has_command_auth(&self) -> bool {
        true
    }

    fn uses_codex_backend(&self) -> ModelsEndpointFuture<'_, bool> {
        Box::pin(async { false })
    }

    fn list_models<'a>(
        &'a self,
        _client_version: &'a str,
        _http_client_factory: HttpClientFactory,
    ) -> ModelsEndpointFuture<'a, CoreResult<ModelsEndpointResponse>> {
        Box::pin(async move {
            let fetch_index = self.fetch_count.fetch_add(1, Ordering::SeqCst);
            self.fetched.notify_one();
            if fetch_index == 0 {
                return Err(CodexErr::Io(std::io::Error::other("test failure")));
            }
            if fetch_index == 1 {
                self.release_second_fetch.notified().await;
            }
            Ok(ModelsEndpointResponse {
                models: Vec::new(),
                etag: None,
                identity: self.identity().expect("test endpoint identity"),
            })
        })
    }
}

#[tokio::test]
async fn refreshes_immediately_periodically_and_stops_when_dropped() {
    let codex_home = tempdir().expect("temp dir");
    let endpoint = TestModelsEndpoint::new();
    let requirements_path = codex_home.path().join("requirements.toml");
    std::fs::write(&requirements_path, "model_provider = 'ollama'").unwrap();
    let config_manager = crate::config_manager::ConfigManager::new_for_tests(
        codex_home.path().to_path_buf(),
        vec![("model_provider".to_string(), "ollama".into())],
        LoaderOverrides::with_managed_config_path_for_tests(
            codex_home.path().join("managed_config.toml"),
        ),
    );
    // The TUI forwards its typed --oss selection through the raw CLI overrides too.
    let config = Arc::new(
        ConfigBuilder::default()
            .codex_home(codex_home.path().to_path_buf())
            .loader_overrides(LoaderOverrides::without_managed_config_for_tests())
            .harness_overrides(ConfigOverrides {
                model_provider: Some("ollama".to_string()),
                ..Default::default()
            })
            .build()
            .await
            .unwrap(),
    );
    let models_manager: SharedModelsManager = Arc::new(OpenAiModelsManager::new(
        codex_home.path().to_path_buf(),
        endpoint.clone(),
        /*auth_manager*/ None,
    ));
    let auth_manager = codex_login::AuthManager::shared_from_config(
        config.as_ref(),
        /*enable_codex_api_key_env*/ false,
    )
    .await
    .expect("test auth manager");
    let catalog = Arc::new(ModelCatalog::new(
        config_manager,
        Some(config.cwd.to_path_buf()),
        config,
        Arc::clone(&models_manager),
        auth_manager,
    ));
    let worker = spawn_with_interval(&catalog, Duration::from_millis(/*millis*/ 10));

    endpoint.wait_for_fetch_count(/*expected*/ 2).await;
    drop(worker);
    endpoint.release_second_fetch.notify_one();
    tokio::time::sleep(Duration::from_millis(30)).await;

    assert_eq!(endpoint.fetch_count.load(Ordering::SeqCst), 2);

    // Reloading after login preserves an embedded caller's typed --oss override.
    catalog.invalidate().await;
    assert_eq!(
        catalog.current().await.unwrap().0.model_provider_id,
        "ollama"
    );

    assert!(Arc::ptr_eq(
        &catalog.current().await.unwrap().1,
        &models_manager
    ));

    // The catalog follows a new managed route without reusing the old endpoint.
    std::fs::write(&requirements_path, "model_provider = 'openai'").unwrap();
    catalog.invalidate().await;
    catalog.list_models(RefreshStrategy::Offline).await.unwrap();
    assert_eq!(
        catalog.current().await.unwrap().0.model_provider_id,
        "openai"
    );
    std::fs::remove_file(&requirements_path).unwrap();
    catalog.invalidate().await;
    assert_eq!(
        catalog.current().await.unwrap().0.model_provider_id,
        "ollama"
    );
    assert_eq!(endpoint.fetch_count.load(Ordering::SeqCst), 2);
}
#[tokio::test]
async fn catalog_reload_recovers_original_project_after_startup_fallback() -> anyhow::Result<()> {
    let home = tempdir()?;
    let project = tempdir()?;
    let project_path = AbsolutePathBuf::from_absolute_path(project.path())?
        .canonicalize()?
        .to_path_buf();
    std::fs::create_dir(project_path.join(".codex"))?;
    std::fs::write(
        home.path().join("config.toml"),
        toml::to_string(&serde_json::json!({
            "projects": {project_path.to_string_lossy(): {"trust_level": "trusted"}}
        }))?,
    )?;
    let project_config = project_path.join(".codex/config.toml");
    std::fs::write(&project_config, "model_catalog_json = [")?;
    let manager = crate::config_manager::ConfigManager::without_managed_config_for_tests(
        home.path().to_path_buf(),
    );
    let startup = manager
        .load_startup_config(Some(project_path.clone()))
        .await?;
    assert!(startup.used_defaults);
    let config = Arc::new(startup.config);
    let auth = codex_login::AuthManager::shared_from_config(
        config.as_ref(),
        /*enable_codex_api_key_env*/ false,
    )
    .await?;
    let models = codex_core::build_models_manager(&config, Arc::clone(&auth));
    let catalog = ModelCatalog::new(manager, Some(project_path.clone()), config, models, auth);
    let mut model = codex_models_manager::bundled_models_response()?
        .models
        .remove(0);
    model.slug = "project-model".into();
    std::fs::write(
        project_path.join(".codex/catalog.json"),
        serde_json::to_vec(&serde_json::json!({"models": [model]}))?,
    )?;
    std::fs::write(&project_config, "model_catalog_json = 'catalog.json'")?;
    catalog.invalidate().await;
    let (config, _) = catalog.current().await?;
    assert_eq!(config.cwd.as_path(), project_path);
    let models = catalog.list_models(RefreshStrategy::Offline).await?;
    assert_eq!(
        models
            .iter()
            .map(|model| model.id.as_str())
            .collect::<Vec<_>>(),
        vec!["project-model"]
    );
    Ok(())
}
