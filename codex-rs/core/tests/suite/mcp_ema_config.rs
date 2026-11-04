//! Exercises EMA activation and registration ownership through real configuration layers.

use std::sync::Arc;

use anyhow::Result;
use codex_config::LoaderOverrides;
use codex_config::McpServerAuth;
use codex_core::ConfigRefreshOutcome;
use codex_core::config::ConfigBuilder;
use codex_core::config::set_project_trust_level;
use codex_login::CodexAuth;
use codex_protocol::config_types::TrustLevel;
use codex_protocol::protocol::EventMsg;
use core_test_support::responses;
use core_test_support::skip_if_no_network;
use core_test_support::test_codex::test_codex;
use core_test_support::wait_for_event_match;
use pretty_assertions::assert_eq;
use tempfile::tempdir;
use test_case::test_case;
use wiremock::MockServer;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn refresh_ema_denial_clears_authority_for_configured_servers() -> Result<()> {
    skip_if_no_network!(Ok(()));
    let endpoint = MockServer::start().await;
    let responses_server = responses::start_mock_server().await;
    let home = Arc::new(tempdir()?);
    let url = format!("{}/mcp", endpoint.uri());
    let policy = |revision| {
        format!(
            "[features]\nuse_xaa = true\napps = false\nsecret_auth_storage = false\n\
             [mcp_enterprise_managed_auth.idp]\nissuer = '{}/idp-{revision}'\nclient_id = 'client-{revision}'\n\
             [mcp_servers.enterprise]\nurl = '{url}'\nauth = 'ema_auth'\noauth_resource = 'resource-{revision}'\n",
            endpoint.uri(),
        )
    };
    let fixture = test_codex()
        .with_home(Arc::clone(&home))
        .with_auth(CodexAuth::from_api_key("test-api-key"))
        .with_pre_build_hook(move |home| {
            std::fs::write(home.join("config.toml"), policy(1))
                .expect("write enterprise MCP policy");
        })
        .build_with_auto_env(&responses_server)
        .await?;
    let (before, _) = fixture.codex.current_mcp_config_and_runtime_context().await;
    let current_config = fixture.codex.config().await;
    let requirements_path = home.path().join("requirements.toml");
    std::fs::write(&requirements_path, "[features]\nuse_xaa = false\n")?;
    let mut loader_overrides = LoaderOverrides::without_managed_config_for_tests();
    loader_overrides.system_requirements_path = Some(requirements_path);
    let refreshed = ConfigBuilder::default()
        .loader_overrides(loader_overrides)
        .codex_home(home.path().to_path_buf())
        .fallback_cwd(Some(fixture.config.cwd.to_path_buf()))
        .build()
        .await?;
    assert_eq!(
        fixture
            .codex
            .refresh_mcp_config(current_config, refreshed)
            .await,
        ConfigRefreshOutcome::Published
    );
    let (after, _) = fixture.codex.current_mcp_config_and_runtime_context().await;
    let old_servers = codex_mcp::configured_mcp_servers(&before);
    let new_servers = codex_mcp::configured_mcp_servers(&after);
    let old = old_servers["enterprise"]
        .ema_registration()
        .expect("original registration");
    assert!(old_servers["enterprise"].enabled);
    assert!(
        !new_servers["enterprise"].enabled,
        "managed XAA opt-out must apply"
    );
    assert_eq!(old.resource(), Some("resource-1"));
    assert!(new_servers["enterprise"].ema_registration().is_none());
    fixture.codex.shutdown_and_wait().await?;
    assert!(endpoint.received_requests().await.unwrap().is_empty());
    Ok(())
}

#[derive(Clone, Copy)]
enum ActivationScenario {
    FeatureDisabled,
    MissingIdp,
    OperatorEnabled,
}
