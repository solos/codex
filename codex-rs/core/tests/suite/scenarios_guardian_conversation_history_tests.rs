//! Parent-backed history retrieval and live authorization on a reused reviewer.

use std::sync::Arc;
use std::sync::Mutex;

use codex_core::config::Constrained;
use codex_features::Feature;
use codex_protocol::config_types::ApprovalsReviewer;
use codex_protocol::models::PermissionProfile;
use codex_protocol::protocol::AskForApproval;
use core_test_support::apps_test_server::AppsTestServer;
use core_test_support::apps_test_server::apps_enabled_builder;
use core_test_support::apps_test_server::recorded_apps_tool_calls;
use core_test_support::responses;
use core_test_support::skip_if_no_network;
use pretty_assertions::assert_eq;
use serde_json::Value;
use serde_json::json;
use test_case::test_case;
use wiremock::Mock;
use wiremock::MockServer;
use wiremock::Request;
use wiremock::ResponseTemplate;
use wiremock::matchers::body_partial_json;
use wiremock::matchers::method;

const CUSTOM_HISTORY_PROMPT: &str =
    "Eval history policy: retrieve the owner's earlier instructions before approving.";

#[derive(Clone, Copy)]
enum HistoryScenario {
    BoundedHistory,
    SmallerReviewerBudget,
    PermissionChanges,
}
