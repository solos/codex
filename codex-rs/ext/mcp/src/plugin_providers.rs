//! Host-assembled plugin sources; discovery results belong to the active thread, not this registry.

use codex_core_plugins::ExecutorPluginProvider;
use std::sync::Arc;

pub struct PluginProviders {
    // Keep concrete access to resolve_bound so loaders retain the exact executor filesystem.
    pub(crate) executor: Arc<ExecutorPluginProvider>,
}

impl PluginProviders {
    pub fn new(executor: Arc<ExecutorPluginProvider>) -> Self {
        Self { executor }
    }
}
