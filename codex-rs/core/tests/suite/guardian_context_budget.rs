//! Review requests preserve required evidence while fitting the aggregate budget.
//! Parent token-budget mode must not replace Guardian's summary compaction.

use anyhow::Result;
use codex_core::TurnInputRequest;
use codex_core::config::Constrained;
use codex_core::config::CurrentTimeReminderConfig;
use codex_core::config::RolloutBudgetConfig;
use codex_core::context::GuardianContextMode;
use codex_extension_api::ExtensionRegistryBuilder;
use codex_features::Feature;
use codex_history::RolloutItem;
use codex_protocol::config_types::ApprovalsReviewer;
use codex_protocol::models::ContentItem;
use codex_protocol::models::ResponseItem;
use codex_protocol::openai_models::AutoReviewMessages;
use codex_protocol::protocol::AskForApproval;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::Op;
use codex_protocol::protocol::ReviewDecision;
use codex_protocol::user_input::UserInput;
use core_test_support::ThreadIdle;
use core_test_support::responses;
use core_test_support::responses::ev_assistant_message;
use core_test_support::responses::ev_completed;
use core_test_support::responses::ev_custom_tool_call;
use core_test_support::responses::ev_function_call;
use core_test_support::responses::ev_response_created;
use core_test_support::responses::sse;
use core_test_support::skip_if_no_network;
use core_test_support::skip_if_wine_exec;
use core_test_support::test_codex::test_codex;
use core_test_support::wait_for_event;
use pretty_assertions::assert_eq;
use serde_json::json;
use std::sync::Arc;
use test_case::test_case;

use super::image_rollout::RecordingFileAttachmentStore;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn independent_review_preserves_oversized_instruction_order() -> Result<()> {
    skip_if_no_network!(Ok(()));
    skip_if_wine_exec!(
        Ok(()),
        "Guardian approval actions require host-native paths"
    );
    let server = responses::start_mock_server().await;
    let mut extensions = ExtensionRegistryBuilder::new();
    extensions.thread_lifecycle_contributor(Arc::new(ThreadIdle));
    let mut test = test_codex()
        .with_extensions(Arc::new(extensions.build()))
        .with_model_info_override("gpt-5.6-luna", |model| model.context_window = Some(28_000))
        .with_model_info_override("gpt-5.5", |model| {
            model.auto_review_model_override = Some("gpt-5.6-luna".to_owned());
        })
        .with_config(|config| {
            config
                .features
                .disable(Feature::GuardianReuseParentCompaction)
                .expect("review the legacy transcript rather than the opaque checkpoint");
            config.permissions.approval_policy = Constrained::allow_any(AskForApproval::OnRequest);
            config.approvals_reviewer = ApprovalsReviewer::AutoReview;
            config.model_auto_compact_token_limit = Some(1_000_000);
        })
        .build_with_auto_env(&server)
        .await?;
    let old_history: Vec<RolloutItem> = serde_json::from_value(json!([{
        "type": "compacted",
        "payload": {
            "message": "Historical checkpoint without producer metadata.",
            "replacement_history": [{
                "type": "compaction",
                "id": "old-checkpoint",
                "encrypted_content": "legacy opaque context"
            }]
        }
    }]))?;
    test.codex.ensure_rollout_materialized().await;
    test.codex.append_rollout_items(&old_history).await?;
    test.codex =
        super::guardian_checkpoint_migration::resume(&test, &test.codex, old_history).await?;
    assert_eq!(
        GuardianContextMode::from_history(
            test.codex.conversation_history_snapshot().await.as_ref()
        ),
        GuardianContextMode::Independent,
    );
    let command = json!({
        "cmd": "echo complete-instructions",
        "sandbox_permissions": "require_escalated",
        "justification": "Run the requested command."
    })
    .to_string();
    let events = (0..2)
        .flat_map(|turn| {
            let mut events = vec![
                sse(vec![
                    ev_function_call(&format!("exec-{turn}"), "exec_command", &command),
                    ev_completed(&format!("parent-{turn}")),
                ]),
                sse(vec![
                    ev_assistant_message(
                        "decision",
                        r#"{"risk_level":"low","user_authorization":"high","outcome":"allow"}"#,
                    ),
                    ev_completed(&format!("review-{turn}")),
                ]),
                sse(vec![
                    ev_assistant_message("done", "done"),
                    ev_completed(&format!("done-{turn}")),
                ]),
            ];
            if turn == 1 {
                events.insert(
                    /*index*/ 1,
                    sse(vec![
                        json!({
                            "type": "response.output_item.done",
                            "item": {
                                "type": "compaction",
                                "id": "review-summary",
                                "encrypted_content": "Earlier reviewer context."
                            }
                        }),
                        ev_completed("review-compaction"),
                    ]),
                );
            }
            events
        })
        .collect();
    let responses = responses::mount_sse_sequence(&server, events).await;
    let padding = "é🙂 ".repeat(/*n*/ 3_500);
    let initial = format!(
        "{padding}Run the requested echo command. You may edit scratch files only.{padding}"
    );
    test.submit_text_turn(&initial).await?;
    ThreadIdle::wait(&test.codex).await;
    let followup = format!("{padding}{padding}Keep all files private.{padding}{padding}");
    let approval = format!(
        "{}\nApproved action: {command}",
        codex_guardian_context::MANUAL_APPROVAL_DEVELOPER_PREFIX
    );
    let restriction = "Revoke permission to edit files. Only run the echo command.";
    test.codex
        .inject_response_items(vec![
            responses::user_message_item(&followup),
            ResponseItem::Message {
                status: None,
                encrypted_content: None,
                id: None,
                role: "developer".to_owned(),
                content: vec![ContentItem::InputText {
                    text: approval.clone(),
                }],
                phase: None,
                internal_chat_message_metadata_passthrough: None,
            },
        ])
        .await?;
    test.submit_text_turn(restriction).await?;
    ThreadIdle::wait(&test.codex).await;
    let (compactions, requests): (Vec<_>, Vec<_>) = responses
        .requests()
        .into_iter()
        .partition(|request| !request.inputs_of_type("compaction_trigger").is_empty());
    let reviews = requests
        .iter()
        .filter(|request| request.body_json()["client_metadata"]["x-openai-subagent"] == "guardian")
        .collect::<Vec<_>>();
    assert_eq!((compactions.len(), reviews.len()), (1, 2));
    assert!(
        reviews[0]
            .message_input_text_groups("user")
            .last()
            .expect("first review")
            .concat()
            .contains(&initial)
    );
    let groups = reviews[1].message_input_text_groups("user");
    let delta = groups.last().expect("follow-up review").concat();
    assert!(delta.contains("<truncated omitted_approx_tokens="));
    assert!(
        delta.contains("User instructions and prior approvals may be incomplete where marked.")
    );
    let transcript = delta
        .split_once(">>> TRANSCRIPT DELTA START\n")
        .expect("legacy transcript delta")
        .1
        .split_once(">>> TRANSCRIPT DELTA END\n")
        .expect("legacy transcript delta end")
        .0;
    let approval_start = transcript.find(&approval).expect("complete prior approval");
    let restriction_start = transcript.find(restriction).expect("later restriction");
    assert!(approval_start + approval.len() < restriction_start);
    assert!(!delta.contains(&initial), "old entries are not resent");
    assert_eq!(
        reviews[0].body_json()["client_metadata"]["thread_id"],
        reviews[1].body_json()["client_metadata"]["thread_id"]
    );
    test.codex.shutdown_and_wait().await?;
    Ok(())
}

#[derive(Clone, Copy)]
enum ReviewerResponse {
    Decision,
    ToolContinuation,
    CachedCompactionContinuation,
    FileImageContinuation,
    UncompactableContinuation,
    CompactionError,
    NextReview,
}
