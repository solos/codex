//! Tests source-aware plugin telemetry selection and complete-inventory bounds.

use crate::session::tests::make_session_and_context;
use codex_extension_api::SelectedPluginIdentity;
use codex_extension_api::SelectedPluginSnapshot;
use codex_utils_plugins::PluginIdentity;
use pretty_assertions::assert_eq;
