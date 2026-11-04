//! Accepted-line aggregation, finalization, and event payload tests.

use crate::events::CodexAcceptedLineFingerprintsEventParams;
use crate::events::CodexAcceptedLineFingerprintsEventRequest;
use crate::events::TrackEventRequest;
use crate::facts::AnalyticsFact;
use crate::reducer::AnalyticsReducer;
use crate::tests::support::ingest_turn_prerequisites;
use crate::tests::support::sample_turn_completed_notification;
use codex_app_server_protocol::ServerNotification;
use codex_app_server_protocol::TurnDiffUpdatedNotification;
use codex_app_server_protocol::TurnStatus as AppServerTurnStatus;
use pretty_assertions::assert_eq;
use serde_json::json;

#[test]
fn accepted_line_fingerprints_event_serializes_expected_shape() {
    let event = TrackEventRequest::AcceptedLineFingerprints(Box::new(
        CodexAcceptedLineFingerprintsEventRequest {
            event_type: "codex_accepted_line_fingerprints",
            event_params: CodexAcceptedLineFingerprintsEventParams {
                event_type: "codex.accepted_line_fingerprints",
                turn_id: "turn-1".to_string(),
                thread_id: "thread-1".to_string(),
                product_surface: Some("codex".to_string()),
                model_slug: Some("gpt-5.1-codex".to_string()),
                completed_at: 1710000000,
                repo_hash: Some("repo-hash-1".to_string()),
                accepted_added_lines: 42,
                accepted_deleted_lines: 40,
                line_fingerprints: [],
            },
        },
    ));

    let payload = serde_json::to_value(&event).expect("serialize accepted line fingerprints event");

    assert_eq!(
        payload,
        json!({
            "event_type": "codex_accepted_line_fingerprints",
            "event_params": {
                "event_type": "codex.accepted_line_fingerprints",
                "turn_id": "turn-1",
                "thread_id": "thread-1",
                "product_surface": "codex",
                "model_slug": "gpt-5.1-codex",
                "completed_at": 1710000000,
                "repo_hash": "repo-hash-1",
                "accepted_added_lines": 42,
                "accepted_deleted_lines": 40,
                "line_fingerprints": []
            }
        })
    );
}

#[tokio::test]
async fn reducer_emits_large_accepted_line_aggregates_without_fingerprints() {
    let mut reducer = AnalyticsReducer::default();
    let mut events = Vec::new();

    ingest_turn_prerequisites(
        &mut reducer,
        &mut events,
        /*include_initialize*/ true,
        /*include_resolved_config*/ true,
        /*include_started*/ true,
        /*include_token_usage*/ true,
    )
    .await;
    events.clear();

    let mut diff = "\
diff --git a/src/lib.rs b/src/lib.rs
index 1111111..2222222
--- a/src/lib.rs
+++ b/src/lib.rs
@@ -0,0 +1,20000 @@
"
    .to_string();
    for index in 0..20_000 {
        diff.push_str(&format!("+let value_{index} = {index};\n"));
    }

    reducer
        .ingest(
            AnalyticsFact::Notification(Box::new(ServerNotification::TurnDiffUpdated(
                TurnDiffUpdatedNotification {
                    thread_id: "thread-2".to_string(),
                    turn_id: "turn-2".to_string(),
                    diff,
                },
            ))),
            &mut events,
        )
        .await;
    assert!(events.is_empty());

    reducer
        .ingest(
            AnalyticsFact::Notification(Box::new(sample_turn_completed_notification(
                "thread-2",
                "turn-2",
                AppServerTurnStatus::Completed,
                /*codex_error_info*/ None,
            ))),
            &mut events,
        )
        .await;

    let accepted_line_events = events
        .iter()
        .filter_map(|event| match event {
            TrackEventRequest::AcceptedLineFingerprints(event) => Some(event),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(accepted_line_events.len(), 1);
    let event = accepted_line_events[0];
    assert_eq!(event.event_params.turn_id, "turn-2");
    assert_eq!(event.event_params.thread_id, "thread-2");
    assert_eq!(event.event_params.accepted_added_lines, 20_000);
    assert_eq!(event.event_params.accepted_deleted_lines, 0);
    assert!(event.event_params.line_fingerprints.is_empty());
    assert!(serde_json::to_vec(event).expect("serialize event").len() < 2_100_000);
}

#[tokio::test]
async fn reducer_emits_accepted_line_fingerprints_once_from_latest_turn_diff_on_completion() {
    let mut reducer = AnalyticsReducer::default();
    let mut events = Vec::new();

    ingest_turn_prerequisites(
        &mut reducer,
        &mut events,
        /*include_initialize*/ true,
        /*include_resolved_config*/ true,
        /*include_started*/ true,
        /*include_token_usage*/ true,
    )
    .await;
    events.clear();

    for line in ["let old_value = 1;", "let latest_value = 2;"] {
        let diff = format!(
            "\
diff --git a/src/lib.rs b/src/lib.rs
index 1111111..2222222
--- a/src/lib.rs
+++ b/src/lib.rs
@@ -0,0 +1 @@
+{line}
"
        );
        reducer
            .ingest(
                AnalyticsFact::Notification(Box::new(ServerNotification::TurnDiffUpdated(
                    TurnDiffUpdatedNotification {
                        thread_id: "thread-2".to_string(),
                        turn_id: "turn-2".to_string(),
                        diff,
                    },
                ))),
                &mut events,
            )
            .await;
    }
    assert!(events.is_empty());

    reducer
        .ingest(
            AnalyticsFact::Notification(Box::new(sample_turn_completed_notification(
                "thread-2",
                "turn-2",
                AppServerTurnStatus::Completed,
                /*codex_error_info*/ None,
            ))),
            &mut events,
        )
        .await;

    let accepted_line_events = events
        .iter()
        .filter_map(|event| match event {
            TrackEventRequest::AcceptedLineFingerprints(event) => Some(event),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(accepted_line_events.len(), 1);
    let event = accepted_line_events[0];
    assert_eq!(event.event_params.accepted_added_lines, 1);
    assert!(event.event_params.line_fingerprints.is_empty());
}
