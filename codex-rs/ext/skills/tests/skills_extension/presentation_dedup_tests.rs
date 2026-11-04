//! Pre-budget deduplication preserves source packages and aliases across executor readiness changes.

use super::*;
use codex_exec_server::EnvironmentManager;
use codex_extension_api::ToolEnvironment;
use pretty_assertions::assert_eq;
