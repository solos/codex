//! Verify MCP prompts and elicitation through actual turns.

use anyhow::Result;
use codex_app_server_protocol as app;
use codex_core::StartThreadOptions;
use codex_core::TurnInputRequest;
use codex_core::TurnInputSubmission;
use codex_core::config::Constrained;
use codex_extension_api::ExtensionRegistryBuilder;
use codex_extension_api::McpToolResultInput;
use codex_extension_api::ToolLifecycleContributor;
use codex_extension_api::ToolLifecycleFuture;
use codex_features::Feature;
use codex_login::CodexAuth;
use codex_mcp::CODEX_APPS_MCP_SERVER_NAME;
use codex_protocol::approvals::ElicitationRequest;
use codex_protocol::config_types::ApprovalsReviewer;
use codex_protocol::items::McpToolCallStatus;
use codex_protocol::items::TurnItem;
use codex_protocol::mcp::CallToolResult;
use codex_protocol::models::PermissionProfile;
use codex_protocol::protocol::AskForApproval;
use codex_protocol::protocol::ElicitationAction;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::Op;
use codex_protocol::protocol::SessionSource;
use codex_protocol::protocol::SubAgentSource;
use codex_protocol::protocol::ThreadSettingsOverrides;
use codex_protocol::user_input::UserInput;
use core_test_support::PathExt;
use core_test_support::apps_test_server::AppsTestServer;
use core_test_support::apps_test_server::SEARCH_CALENDAR_CREATE_TOOL;
use core_test_support::apps_test_server::SEARCH_CALENDAR_NAMESPACE;
use core_test_support::apps_test_server::recorded_apps_tool_call_by_call_id;
use core_test_support::apps_test_server::recorded_apps_tool_calls;
use core_test_support::apps_test_server::search_capable_apps_builder;
use core_test_support::responses;
use core_test_support::skip_if_no_network;
use core_test_support::wait_for_event;
use core_test_support::wait_for_mcp_server;
use pretty_assertions::assert_eq;
use serde_json::Value;
use serde_json::json;
use std::sync::Arc;
use std::time::Duration;
use test_case::test_case;
use wiremock::Mock;
use wiremock::Request;
use wiremock::Respond;
use wiremock::ResponseTemplate;
use wiremock::matchers::body_partial_json;
use wiremock::matchers::method;
use wiremock::matchers::path;
use wiremock::matchers::path_regex;

const CALL_ID: &str = "calendar-elicitation-call";
const PRIVATE_SENTINEL: &str = "synthetic-private-value";

#[derive(Clone, Copy)]
enum AuthFailureFormat {
    Text,
    Structured,
}

#[derive(Clone, Copy)]
struct AuthFailureResponder {
    scenario: Scenario,
    format: AuthFailureFormat,
}

impl AuthFailureResponder {
    fn result(self) -> Value {
        let mut response = json!({
            "content": [{
                "type": "text",
                "text": PRIVATE_SENTINEL,
            }],
            "isError": true,
            "_meta": {
                "_codex_apps": {
                    "connector_auth_failure": {
                        "is_auth_failure": true,
                        "auth_reason": "reauthentication_required",
                        "connector_id": "calendar",
                        "link_id": "link_123",
                        "error_code": "UNAUTHORIZED",
                        "error_http_status_code": 401,
                        "error_action": "TRIGGER_REAUTHENTICATION",
                    },
                },
            },
        });
        if self.scenario == Scenario::AuthMetadataRemoved {
            response["_meta"] = json!({"_codex_apps": {"connector_auth_failure": {
                "is_auth_failure": true, "connector_id": "calendar",
            }}});
        }
        if matches!(self.format, AuthFailureFormat::Structured) {
            response["structuredContent"] = json!({
                "error": "synthetic-structured-auth-failure",
                "details": {"reason": "reauthentication_required", "status": 401},
            });
        }
        response
    }
}

impl Respond for AuthFailureResponder {
    fn respond(&self, request: &Request) -> ResponseTemplate {
        let body: Value =
            serde_json::from_slice(&request.body).expect("tools/call request should be valid JSON");
        ResponseTemplate::new(/*status*/ 200).set_body_json(json!({
            "jsonrpc": "2.0",
            "id": body.get("id").cloned().unwrap_or(Value::Null),
            "result": self.result(),
        }))
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Scenario {
    DefaultAuth,
    ModernAuth,
    AuthMetadataRemoved,
    LegacySuccess,
    LegacyCancelled,
    ModernDeclined,
}

struct RemoveAuthMetadata;

impl ToolLifecycleContributor for RemoveAuthMetadata {
    fn on_mcp_tool_result<'a>(&'a self, input: McpToolResultInput<'a>) -> ToolLifecycleFuture<'a> {
        Box::pin(async move { input.result.meta = None })
    }
}

#[derive(Clone, Copy)]
enum SubagentRequestKind {
    ToolApproval,
    LegacyToolApproval,
    ConnectorAuth(AuthFailureFormat),
}

#[test_case(SubagentRequestKind::ToolApproval, ElicitationAction::Accept; "child_tool_approval_waits_for_user_before_execution")]
#[test_case(SubagentRequestKind::LegacyToolApproval, ElicitationAction::Accept; "child_legacy_tool_approval_is_rejected_before_execution")]
#[test_case(SubagentRequestKind::ConnectorAuth(AuthFailureFormat::Text), ElicitationAction::Accept; "child_connector_auth_accepts_text_failure")]
#[test_case(SubagentRequestKind::ConnectorAuth(AuthFailureFormat::Structured), ElicitationAction::Accept; "child_connector_auth_accepts_structured_failure")]
#[test_case(SubagentRequestKind::ConnectorAuth(AuthFailureFormat::Structured), ElicitationAction::Decline; "child_declined_connector_auth_preserves_original_failure")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn core_generated_mcp_elicitations_support_subagents(
    kind: SubagentRequestKind,
    decision: ElicitationAction,
) -> Result<()> {
    skip_if_no_network!(Ok(()));
    let server = responses::start_mock_server().await;
    let apps = AppsTestServer::mount_searchable(&server).await?;
    if let SubagentRequestKind::ConnectorAuth(format) = kind {
        Mock::given(method("POST"))
            .and(path_regex("^/api/codex/ps/mcp/?$"))
            .and(body_partial_json(json!({
                "method": "tools/call",
                "params": {"name": "calendar_create_event"}
            })))
            .respond_with(AuthFailureResponder {
                scenario: Scenario::DefaultAuth,
                format,
            })
            .with_priority(/*p*/ 1)
            .mount(&server)
            .await;
    }
    let test = search_capable_apps_builder(apps.chatgpt_base_url)
        .with_config(move |config| {
            config.permissions.approval_policy = Constrained::allow_any(AskForApproval::OnRequest);
            config.approvals_reviewer = ApprovalsReviewer::User;
            config
                .features
                .enable(Feature::AuthElicitation)
                .expect("enable auth elicitation");
            config
                .features
                .set_enabled(
                    Feature::ToolCallMcpElicitation,
                    !matches!(kind, SubagentRequestKind::LegacyToolApproval),
                )
                .expect("configure MCP approval transport");
            let approval_mode = match kind {
                SubagentRequestKind::ToolApproval | SubagentRequestKind::LegacyToolApproval => {
                    "prompt"
                }
                SubagentRequestKind::ConnectorAuth(_) => "approve",
            };
            let user_config = toml::from_str(&format!(
                r#"[apps.calendar]
default_tools_approval_mode = "{approval_mode}"
approvals_reviewer = "user"
"#
            ))
            .expect("apps config");
            config.config_layer_stack = config
                .config_layer_stack
                .with_user_config(&config.codex_home.join("config.toml").abs(), user_config)
                .expect("apply apps config");
        })
        .build_with_auto_env(&server)
        .await?;
    let child = test
        .thread_manager
        .start_thread(StartThreadOptions {
            session_source: Some(SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
                parent_thread_id: test.session_configured.thread_id,
                depth: 1,
                agent_path: None,
                agent_nickname: None,
                agent_role: None,
            })),
            environments: Some(vec![test.executor_environment().request()]),
            ..StartThreadOptions::new(test.config.clone())
        })
        .await?
        .thread;
    wait_for_mcp_server(&child, CODEX_APPS_MCP_SERVER_NAME).await?;
    responses::mount_sse_once(
        &server,
        responses::sse(vec![
            responses::ev_function_call_with_namespace(
                "calendar-call",
                SEARCH_CALENDAR_NAMESPACE,
                SEARCH_CALENDAR_CREATE_TOOL,
                &json!({"title": "Lunch", "starts_at": "2026-06-18T12:00:00Z"}).to_string(),
            ),
            responses::ev_completed("tool-response"),
        ]),
    )
    .await;
    let follow_up = responses::mount_sse_once(&server, responses::sse_completed("done")).await;
    child
        .start_or_steer_turn(
            TurnInputRequest::user_input(vec![UserInput::Text {
                text: "Use [$calendar](app://calendar) to create a calendar event.".into(),
                text_elements: Vec::new(),
            }])
            .with_thread_settings(ThreadSettingsOverrides {
                approval_policy: Some(AskForApproval::OnRequest),
                permission_profile: Some(PermissionProfile::Disabled),
                ..Default::default()
            }),
        )
        .await?;
    let mut completed_status = None;
    let mut completed_result = None;
    let mut prompts = 0;
    loop {
        let event = wait_for_event(&child, |event| {
            assert!(
                !matches!(event, EventMsg::RequestUserInput(_)),
                "subagents must not fall back to request_user_input"
            );
            if let EventMsg::ItemCompleted(event) = event
                && let TurnItem::McpToolCall(item) = &event.item
                && item.id == "calendar-call"
            {
                completed_status = Some(item.status);
                completed_result = item.result.clone();
            }
            matches!(
                event,
                EventMsg::ElicitationRequest(_) | EventMsg::TurnComplete(_)
            )
        })
        .await;
        let EventMsg::ElicitationRequest(request) = event else {
            break;
        };
        assert_eq!(prompts, 0, "one tool call must not prompt twice");
        prompts += 1;
        assert_eq!(request.server_name, CODEX_APPS_MCP_SERVER_NAME);
        assert!(
            follow_up.requests().is_empty(),
            "the tool must wait for input"
        );
        match kind {
            SubagentRequestKind::ToolApproval => {
                assert!(matches!(request.request, ElicitationRequest::Form { .. }));
                assert!(recorded_apps_tool_calls(&server).await.is_empty());
            }
            SubagentRequestKind::ConnectorAuth(_) => {
                assert!(matches!(request.request, ElicitationRequest::Url { .. }));
                assert_eq!(recorded_apps_tool_calls(&server).await.len(), 1);
            }
            SubagentRequestKind::LegacyToolApproval => {
                panic!("legacy subagent approval must be rejected before prompting");
            }
        }
        child
            .submit(Op::ResolveElicitation {
                server_name: request.server_name,
                request_id: request.id,
                decision,
                content: None,
                meta: None,
            })
            .await?;
    }
    let expects_prompt = !matches!(kind, SubagentRequestKind::LegacyToolApproval);
    assert_eq!(prompts, usize::from(expects_prompt));
    assert_eq!(
        completed_status,
        Some(if matches!(kind, SubagentRequestKind::ToolApproval) {
            McpToolCallStatus::Completed
        } else {
            McpToolCallStatus::Failed
        })
    );
    assert_eq!(
        recorded_apps_tool_calls(&server).await.len(),
        usize::from(expects_prompt)
    );
    let output = follow_up
        .single_request()
        .function_call_output("calendar-call");
    let output_text = match &output["output"] {
        Value::String(text) => text.clone(),
        Value::Array(items) => items
            .iter()
            .filter_map(|item| item["text"].as_str())
            .collect::<Vec<_>>()
            .join("\n"),
        other => panic!("unexpected MCP output: {other}"),
    };
    if matches!(kind, SubagentRequestKind::LegacyToolApproval) {
        assert!(output_text.contains("parent agent"));
    }
    if let SubagentRequestKind::ConnectorAuth(format) = kind {
        let original: CallToolResult = serde_json::from_value(
            AuthFailureResponder {
                scenario: Scenario::DefaultAuth,
                format,
            }
            .result(),
        )?;
        let expected_result = if decision == ElicitationAction::Accept {
            let text =
                "Authentication for Calendar was requested and accepted. Retry this tool call now.";
            assert!(output_text.contains(text));
            assert!(!output_text.contains(PRIVATE_SENTINEL));
            CallToolResult {
                content: vec![json!({"type": "text", "text": text})],
                structured_content: None,
                is_error: Some(true),
                meta: original.meta,
            }
        } else {
            assert!(output_text.contains("synthetic-structured-auth-failure"));
            original
        };
        assert_eq!(completed_result, Some(expected_result));
        assert!(
            !output_text.contains("link_123"),
            "private auth metadata must not reach the model"
        );
    }
    child.shutdown_and_wait().await?;
    test.codex.shutdown_and_wait().await?;
    Ok(())
}
