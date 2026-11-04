//! Managed policies and explicit user goals reach Guardian through their distinct trusted sources.

use anyhow::Context;
use codex_core::config::Constrained;
use codex_core::context::UserGoalUpdate;
use codex_prompts::ResolvedModelMessages;
use codex_protocol::config_types::ApprovalsReviewer;
use codex_protocol::models::PermissionProfile;
use codex_protocol::protocol::AskForApproval;
use core_test_support::context_snapshot;
use core_test_support::context_snapshot::ContextSnapshotOptions;
use core_test_support::responses;
use core_test_support::skip_if_no_network;
use core_test_support::test_codex::test_codex;
use pretty_assertions::assert_eq;
use serde_json::json;
