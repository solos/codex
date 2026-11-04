use crate::config::Config;
use codex_otel::OtelProvider;
use std::error::Error;

/// Telemetry export has been removed from this build.
///
/// Keep the function as a compatibility hook for callers that support an
/// optional local provider. It always returns `None`, so no OTLP or built-in
/// analytics exporter is created.
pub fn build_provider(
    _config: &Config,
    _service_version: &str,
    _service_name_override: Option<&str>,
    _default_analytics_enabled: bool,
) -> Result<Option<OtelProvider>, Box<dyn Error>> {
    Ok(None)
}

pub fn record_process_start(_otel: Option<&OtelProvider>, _originator: &str) {}

pub fn install_sqlite_telemetry(_otel: Option<&OtelProvider>, _originator: &str) {}
