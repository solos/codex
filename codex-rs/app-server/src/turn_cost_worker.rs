//! Compatibility surface for the removed turn-cost analytics uploader.

use codex_core::config::Config;
use codex_login::AuthManager;
use codex_otel::SessionTelemetry;
use codex_protocol::ThreadId;
use codex_protocol::protocol::Event;
use std::sync::Arc;

pub(crate) struct TurnCostWorker {
    handle: TurnCostWorkerHandle,
}

#[derive(Clone)]
pub(crate) struct TurnCostWorkerHandle;

impl TurnCostWorker {
    pub(crate) fn spawn(config: Arc<Config>, auth_manager: Arc<AuthManager>) -> Option<Self> {
        let _ = (config, auth_manager);
        None
    }

    pub(crate) fn handle(&self) -> TurnCostWorkerHandle {
        self.handle.clone()
    }

    pub(crate) fn shutdown(&self) {}
}

impl TurnCostWorkerHandle {
    pub(crate) fn observe_event(
        &self,
        _thread_id: ThreadId,
        _thread_config: &Config,
        _event: &Event,
        _session_telemetry: impl FnOnce() -> SessionTelemetry,
    ) {
    }
}
