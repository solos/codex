use std::sync::Arc;

use codex_core::ThreadManager;
use codex_core_plugins::EffectivePluginsChange;

/// Invalidates local plugin consumers after a marketplace or installed-plugin change.
pub(crate) fn effective_plugins_changed_callback(
    _auth_manager: Arc<codex_login::AuthManager>,
    thread_manager: Arc<ThreadManager>,
    _config_manager: crate::config_manager::ConfigManager,
    _config_processor: crate::request_processors::ConfigRequestProcessor,
    _request_serialization_queues: crate::request_serialization::RequestSerializationQueues,
) -> Arc<dyn Fn(EffectivePluginsChange) + Send + Sync> {
    Arc::new(move |_change| {
        thread_manager.plugins_manager().clear_cache();
        thread_manager.skills_service().clear_cache();
        let refresh_thread_manager = Arc::clone(&thread_manager);
        tokio::spawn(async move {
            refresh_thread_manager.invalidate_mcp_runtimes().await;
            refresh_thread_manager.refresh_hook_runtimes().await;
        });
    })
}
