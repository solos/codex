mod cell_actor;
mod runtime;
mod service;
mod session_runtime;
mod ui;

pub(crate) type TaskFailureHandler = std::sync::Arc<dyn Fn(String) + Send + Sync>;

pub use codex_code_mode_protocol::*;
pub use service::InProcessCodeModeSession;
pub use ui::run as run_ui_sandbox;
