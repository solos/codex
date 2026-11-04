//! Embedded startup preserves caller configuration and bootstrap access across authentication.

use super::configure;
use crate::config_manager::ConfigManager;
use codex_arg0::Arg0DispatchPaths;
use codex_config::LoaderOverrides;
use codex_config::NoopThreadConfigLoader;
use codex_core::config::ConfigBuilder;
use codex_core::config::ConfigOverrides;
use codex_core::config::set_project_trust_level;
use codex_login::CodexAuth;
use codex_model_provider::DEFAULT_CHATGPT_BASE_URL;
use codex_protocol::config_types::TrustLevel;
use pretty_assertions::assert_eq;
use std::sync::Arc;

#[tokio::test]
async fn startup_reloads_the_callers_selected_project() -> anyhow::Result<()> {
    let temp = tempfile::tempdir()?;
    let root = temp.path().canonicalize()?;
    let home = root.join("home");
    let selected = root.join("selected");
    std::fs::create_dir(&home)?;
    std::fs::create_dir_all(selected.join(".codex"))?;
    std::fs::create_dir(selected.join(".git"))?;
    set_project_trust_level(&home, &selected, TrustLevel::Trusted)?;
    let selected_url = "https://selected.example/backend-api/";
    let user_config = home.join("config.toml");
    let trust = std::fs::read_to_string(&user_config)?;
    std::fs::write(
        &user_config,
        format!("chatgpt_base_url = '{selected_url}'\n{trust}"),
    )?;
    let project_config = selected.join(".codex/config.toml");
    std::fs::write(&project_config, "model = 'selected-model'")?;
    let loader_overrides = LoaderOverrides::without_managed_config_for_tests();
    let mut config = Arc::new(
        ConfigBuilder::default()
            .codex_home(home.clone())
            .harness_overrides(ConfigOverrides {
                cwd: Some(selected.clone()),
                ..Default::default()
            })
            .loader_overrides(loader_overrides.clone())
            .build()
            .await?,
    );
    assert_eq!(config.chatgpt_base_url, selected_url);
    assert_eq!(config.model.as_deref(), Some("selected-model"));
    let manager = ConfigManager::new(
        home,
        Vec::new(),
        loader_overrides,
        /*strict_config*/ true,
        Arg0DispatchPaths::default(),
        Arc::new(NoopThreadConfigLoader),
    );
    let _auth = configure(
        &manager,
        &mut config,
        /*enable_codex_api_key_env*/ false,
    )
    .await?;
    assert_eq!(
        config.cwd.as_path().canonicalize()?,
        selected.canonicalize()?
    );
    assert_eq!(config.chatgpt_base_url, selected_url);

    let marked = CodexAuth::from_api_key("sk-proj-fedramp-test-key");
    for (configured_url, expected_url) in [
        (None, "https://gov.chatgpt.com/backend-api/"),
        (Some(DEFAULT_CHATGPT_BASE_URL), DEFAULT_CHATGPT_BASE_URL),
    ] {
        let setting = configured_url
            .map(|url| format!("chatgpt_base_url = '{url}'\n"))
            .unwrap_or_default();
        std::fs::write(&user_config, format!("{setting}{trust}"))?;
        let _auth = configure(
            &manager,
            &mut config,
            /*enable_codex_api_key_env*/ false,
        )
        .await?;
        assert_eq!(config.chatgpt_backend_url().resolve(&marked), expected_url);
    }

    std::fs::write(&project_config, "[malformed")?;
    let error = configure(
        &manager,
        &mut config,
        /*enable_codex_api_key_env*/ false,
    )
    .await
    .err()
    .ok_or_else(|| anyhow::anyhow!("embedded startup ignored the selected project"))?;
    assert!(
        error
            .to_string()
            .contains("Error parsing project config file")
    );
    Ok(())
}
