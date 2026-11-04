use anyhow::Result;
use codex_core::CodexThread;
use codex_core::TurnInputRequest;
use codex_core::TurnInputSubmission;
use codex_core::config::Constrained;
use codex_features::Feature;
use codex_models_manager::model_info::model_info_from_slug;
use codex_protocol::config_types::ApprovalsReviewer;
use codex_protocol::error::CodexErrorDetails;
use codex_protocol::models::PermissionProfile;
use codex_protocol::openai_models::ApprovalMessages;
use codex_protocol::openai_models::ModelsResponse;
use codex_protocol::openai_models::PermissionMessages;
use codex_protocol::protocol::AskForApproval;
use codex_protocol::protocol::CodexErrorInfo;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::Op;
use codex_protocol::protocol::ThreadSettingsOverrides;
use codex_protocol::protocol::ThreadSettingsSnapshot;
use codex_protocol::user_input::UserInput;
use core_test_support::responses;
use core_test_support::skip_if_no_network;
use core_test_support::test_codex::test_codex;
use core_test_support::wait_for_event;
use pretty_assertions::assert_eq;
use std::time::Duration;
use test_case::test_case;
use tokio::time::timeout;

const INITIAL_MODEL: &str = "gpt-5.4";
const PROTECTED_MODEL: &str = "gpt-5.2";
const REVIEW_INSTRUCTIONS: &str = "Protected-model automatic review instructions.";
const PERMISSION_INSTRUCTIONS: &str = "Protected-model read-only instructions.";

#[derive(Clone, Copy)]
enum SettingsOperation {
    Standalone,
    StandaloneWithReply,
    TurnStart,
}

impl SettingsOperation {
    async fn submit(
        self,
        codex: &CodexThread,
        thread_settings: ThreadSettingsOverrides,
    ) -> Result<ThreadSettingsSnapshot> {
        let id = match self {
            Self::Standalone => {
                codex
                    .submit(Op::ThreadSettings {
                        thread_settings,
                        reply: None,
                    })
                    .await?
            }
            Self::StandaloneWithReply => {
                if let Err(error) = codex.update_thread_settings(thread_settings).await {
                    assert!(matches!(
                        error.details(),
                        CodexErrorDetails::InvalidRequest(_)
                    ));
                    return Err(error.into());
                }
                // A successful update still notifies subscribers. A prior direct rejection must not.
                return match wait_for_event(codex, |event| {
                    matches!(
                        event,
                        EventMsg::ThreadSettingsApplied(_) | EventMsg::Error(_)
                    )
                })
                .await
                {
                    EventMsg::ThreadSettingsApplied(applied) => Ok(applied.thread_settings),
                    event => anyhow::bail!("unexpected settings event: {event:?}"),
                };
            }
            Self::TurnStart => {
                let result = codex
                    .start_or_steer_turn(
                        TurnInputRequest::user_input(vec![UserInput::Text {
                            text: "use the protected model".to_string(),
                            text_elements: Vec::new(),
                        }])
                        .with_thread_settings(thread_settings),
                    )
                    .await?;
                let TurnInputSubmission::Started { turn_id, .. } = result else {
                    anyhow::bail!("expected a new turn, got {result:?}");
                };
                turn_id
            }
        };
        timeout(Duration::from_secs(10), async {
            loop {
                let event = codex.next_event().await?;
                if event.id != id {
                    continue;
                }
                match event.msg {
                    EventMsg::ThreadSettingsApplied(applied) => return Ok(applied.thread_settings),
                    EventMsg::Error(error) => {
                        assert_eq!(error.codex_error_info, Some(CodexErrorInfo::BadRequest));
                        anyhow::bail!(error.message);
                    }
                    _ => {}
                }
            }
        })
        .await?
    }
}
