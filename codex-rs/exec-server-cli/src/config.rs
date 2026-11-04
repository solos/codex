//! Loads executor startup configuration without changing CLI policy.

use codex_app_server::in_process::EmbeddedNetworkPolicy;
use codex_core::config::ConfigBuilder;
use codex_utils_cli::CliConfigOverrides;

pub(super) async fn load_exec_server_config(
    root_config_overrides: &CliConfigOverrides,
    strict_config: bool,
) -> anyhow::Result<codex_core::config::Config> {
    let cli_kv_overrides = root_config_overrides
        .parse_overrides()
        .map_err(anyhow::Error::msg)?;
    let mut config = ConfigBuilder::default()
        .cli_overrides(cli_kv_overrides)
        .strict_config(strict_config)
        .build()
        .await?;
    EmbeddedNetworkPolicy::default().activate(&mut config);
    Ok(config)
}
