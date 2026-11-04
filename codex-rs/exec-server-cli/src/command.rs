//! Command-line startup shared by `codex exec-server` and `codex-exec-server`.
//!
//! Transport, configuration, and shutdown are kept together.

use clap::Parser;
use codex_arg0::Arg0DispatchPaths;
use codex_core::config::Config;
use codex_exec_server::ExecServerRuntimeOptions;
use codex_http_client::HttpClientFactory;
use codex_http_client::OutboundProxyPolicy;
use codex_utils_cli::CliConfigOverrides;
use codex_websocket_auth::WebsocketAuthArgs;

use crate::config::load_exec_server_config;
use crate::exec_server_telemetry;

/// [EXPERIMENTAL] Run the standalone exec-server service.
#[derive(Debug, Parser)]
pub struct ExecServerCommand {
    /// Error out when config.toml contains fields that are not recognized by this version of Codex.
    #[arg(
        id = "exec_server_strict_config",
        long = "strict-config",
        default_value_t = false,
        global = true
    )]
    pub strict_config: bool,

    /// Linux PID namespace: isolate (default) or inherit. Inherit allows signals to
    /// other same-UID processes; enable only when provisioning a dedicated environment.
    #[arg(long, value_name = "MODE", default_value = "isolate", global = true)]
    linux_sandbox_pid_namespace: codex_sandboxing::LinuxSandboxPidNamespace,

    /// Allow permitted private IP destinations to use the configured upstream proxy.
    /// If no valid upstream proxy applies to the request protocol, connect directly.
    /// Loopback stays local. This flag does not require upstream routing.
    #[arg(
        long,
        env = "CODEX_EXEC_SERVER_PROXY_PRIVATE_IPS_VIA_UPSTREAM",
        global = true
    )]
    proxy_private_ips_via_upstream: bool,

    /// Maximum number of requests to process concurrently on each connection.
    #[arg(
        long = "concurrent-requests",
        value_name = "COUNT",
        default_value = "1"
    )]
    request_dispatch_mode: codex_exec_server::RequestDispatchMode,

    /// Transport endpoint URL. Supported values: `ws://IP:PORT` (default), `stdio`, `stdio://`.
    #[arg(long = "listen", value_name = "URL")]
    listen: Option<String>,

    #[command(flatten)]
    websocket_auth: WebsocketAuthArgs,
}

impl ExecServerCommand {
    /// Starts the executor with the given runtime paths and configuration overrides.
    pub async fn run(
        self,
        arg0_paths: &Arg0DispatchPaths,
        root_config_overrides: &CliConfigOverrides,
    ) -> anyhow::Result<()> {
        let strict_config = self.strict_config;
        let websocket_auth = self.websocket_auth.try_into_settings()?;
        let codex_self_exe = arg0_paths
            .codex_self_exe
            .clone()
            .ok_or_else(|| anyhow::anyhow!("Codex executable path is not configured"))?;
        let mut runtime_paths = ExecServerRuntimeOptions::new(
            codex_self_exe,
            arg0_paths.codex_linux_sandbox_exe.clone(),
        )?
        .with_linux_sandbox_pid_namespace(self.linux_sandbox_pid_namespace)
        .with_proxy_private_ips_via_upstream(self.proxy_private_ips_via_upstream);
        // Config loading below owns error handling (non-strict stdio can proceed
        // without config). Never retain other startup flags in the executor.
        if let Ok(overrides) = root_config_overrides.parse_overrides() {
            let cli_flags = codex_config::build_cli_overrides_layer(&overrides);
            runtime_paths.prefer_mxc = cli_flags
                .get("features")
                .and_then(|features| features.get("prefer_mxc"))
                .and_then(toml::Value::as_bool);
        }
        let config_result = load_exec_server_config(root_config_overrides, strict_config).await;
        let config = if strict_config {
            Some(config_result?)
        } else {
            config_result.ok()
        };
        let (_otel, telemetry) = exec_server_telemetry::init(config.as_ref());
        #[cfg(target_os = "macos")]
        let runtime_paths =
            runtime_paths.with_allowed_symlinked_codex_home(config.as_ref().and_then(|config| {
                codex_config::allowed_symlinked_codex_home(
                    &config.config_layer_stack,
                    &config.codex_home,
                )
            }));
        let http_client_factory = config
            .as_ref()
            .map(Config::http_client_factory)
            .unwrap_or_else(|| HttpClientFactory::new(OutboundProxyPolicy::ReqwestDefault));
        let listen_url = self
            .listen
            .unwrap_or_else(|| codex_exec_server::DEFAULT_LISTEN_URL.to_string());
        exec_server_telemetry::run_until_shutdown(codex_exec_server::run_main_with_telemetry(
            &listen_url,
            runtime_paths,
            telemetry,
            http_client_factory,
            self.request_dispatch_mode,
            websocket_auth,
        ))
        .await
        .map_err(anyhow::Error::from_boxed)
    }
}

#[cfg(test)]
#[path = "exec_server_args_tests.rs"]
mod tests;
