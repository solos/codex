use codex_http_client::HttpClientFactory;

use crate::EnvironmentManager;
use crate::ExecServerError;
use crate::ExecServerRuntimeOptions;
use crate::environment_provider::EnvironmentProviderSnapshot;

#[derive(Debug)]
pub(crate) enum PreparedEnvironmentSource {
    Snapshot(EnvironmentProviderSnapshot),
}

/// Holds discovered execution environments before their HTTP policy is resolved.
///
/// Preparing environments does not start any connection. The default environment
/// is always process-local.
#[derive(Debug)]
pub struct PreparedEnvironmentManager {
    pub(crate) source: PreparedEnvironmentSource,
}

impl PreparedEnvironmentManager {
    /// Local-only builds always return `false`.
    pub fn default_environment_is_remote(&self) -> bool {
        false
    }

    /// Builds the manager and starts remote connections using the supplied policy.
    pub fn build(
        self,
        local_runtime_paths: Option<ExecServerRuntimeOptions>,
        http_client_factory: HttpClientFactory,
    ) -> Result<EnvironmentManager, ExecServerError> {
        match self.source {
            PreparedEnvironmentSource::Snapshot(snapshot) => EnvironmentManager::from_snapshot(
                snapshot,
                local_runtime_paths,
                http_client_factory,
            ),
        }
    }
}
