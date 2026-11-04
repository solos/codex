use anyhow::Result;
use codex_core::TurnInputRequest;
use codex_core::config::AgentRoleConfig;
use codex_features::Feature;
use codex_login::CodexAuth;
use codex_protocol::models::PermissionProfile;
use codex_protocol::openai_models::ReasoningEffort;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::Op;
use codex_protocol::protocol::ThreadSettingsOverrides;
use codex_protocol::user_input::UserInput;
use codex_thread_store::ReadThreadParams;
use core_test_support::responses::assert_parent_turn;
use core_test_support::responses::assert_root_turn;
use core_test_support::responses::ev_assistant_message;
use core_test_support::responses::ev_completed;
use core_test_support::responses::ev_function_call_with_namespace;
use core_test_support::responses::ev_response_created;
use core_test_support::responses::mount_sse_once;
use core_test_support::responses::mount_sse_once_match;
use core_test_support::responses::sse;
use core_test_support::responses::sse_response;
use core_test_support::responses::start_mock_server;
use core_test_support::test_codex::test_codex;
use core_test_support::wait_for_event;
use pretty_assertions::assert_eq;
use serde_json::Value;
use serde_json::json;
use std::time::Duration;
use tokio::time::Instant;
use tokio::time::sleep;
use wiremock::Mock;
use wiremock::ResponseTemplate;
use wiremock::matchers::method;
use wiremock::matchers::path;

#[path = "multi_agent_restore_tests.rs"]
mod restore_tests;

const COLLABORATION_NAMESPACE: &str = "collaboration";
const SPAWN_CALL_ID: &str = "spawn-worker";
const NESTED_CALL_ID: &str = "spawn-grandchild";
const QUEUE_CALL_ID: &str = "queue-worker-message";
const FOLLOWUP_CALL_ID: &str = "followup-worker";
const SIBLING_SPAWN_CALL_ID: &str = "spawn-survivor";
const SIBLING_FOLLOWUP_CALL_ID: &str = "followup-survivor";
const INTERRUPT_CALL_ID: &str = "interrupt-worker";
const INITIAL_PROMPT: &str = "spawn a durable worker";
const INITIAL_TASK: &str = "inspect the repository";
const NESTED_TASK: &str = "inspect the nested repository";
const QUEUE_PROMPT: &str = "queue context for the durable worker";
const QUEUED_MESSAGE: &str = "queue-only context from an earlier parent turn";
const FOLLOWUP_PROMPT: &str = "continue the durable worker";
const FOLLOWUP_TASK: &str = "inspect the tests too";
const SIBLING_PROMPT: &str = "spawn a second durable worker";
const SIBLING_TASK: &str = "inspect the release lifecycle";
const SIBLING_FOLLOWUP_PROMPT: &str = "continue the surviving worker";
const SIBLING_FOLLOWUP_TASK: &str = "verify the surviving worker";
const INTERRUPT_PROMPT: &str = "release the interrupted worker";
const SIBLING_NAME: &str = "survivor";
const ROLE_NAME: &str = "durable_worker";
const ROLE_MODEL: &str = "gpt-5.6-sol";
const ROLE_MODEL_PROVIDER_ID: &str = "openai";
const ROLE_DEVELOPER_INSTRUCTIONS: &str = "Keep the durable worker role configuration.";
const SUBAGENT_DEVELOPER_INSTRUCTIONS: &str = "Use the default durable worker instructions.";

fn decoded_body(request: &wiremock::Request) -> Option<Vec<u8>> {
    let is_zstd = request
        .headers
        .get("content-encoding")
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| {
            value
                .split(',')
                .any(|entry| entry.trim().eq_ignore_ascii_case("zstd"))
        });
    if is_zstd {
        zstd::stream::decode_all(std::io::Cursor::new(&request.body)).ok()
    } else {
        Some(request.body.clone())
    }
}

fn body_contains(request: &wiremock::Request, text: &str) -> bool {
    decoded_body(request)
        .and_then(|body| String::from_utf8(body).ok())
        .is_some_and(|body| body.contains(text))
}

fn request_has_model(request: &wiremock::Request, model: &str) -> bool {
    decoded_body(request)
        .and_then(|body| serde_json::from_slice::<Value>(&body).ok())
        .is_some_and(|body| body.get("model").and_then(Value::as_str) == Some(model))
}

fn request_has_input_type(request: &wiremock::Request, input_type: &str) -> bool {
    decoded_body(request)
        .and_then(|body| serde_json::from_slice::<Value>(&body).ok())
        .and_then(|body| body.get("input").and_then(Value::as_array).cloned())
        .is_some_and(|items| {
            items
                .iter()
                .any(|item| item.get("type").and_then(Value::as_str) == Some(input_type))
        })
}

async fn mount_root_collaboration_call(
    server: &wiremock::MockServer,
    prompt: &'static str,
    call_id: &'static str,
    tool_name: &'static str,
    arguments: &str,
) {
    let first_response_id = format!("resp-{call_id}-1");
    mount_sse_once_match(
        server,
        move |request: &wiremock::Request| {
            body_contains(request, prompt) && !request_has_model(request, ROLE_MODEL)
        },
        sse(vec![
            ev_response_created(&first_response_id),
            ev_function_call_with_namespace(call_id, COLLABORATION_NAMESPACE, tool_name, arguments),
            ev_completed(&first_response_id),
        ]),
    )
    .await;

    let second_response_id = format!("resp-{call_id}-2");
    let message_id = format!("msg-{call_id}-2");
    mount_sse_once_match(
        server,
        move |request: &wiremock::Request| {
            body_contains(request, call_id) && !request_has_model(request, ROLE_MODEL)
        },
        sse(vec![
            ev_response_created(&second_response_id),
            ev_assistant_message(&message_id, "collaboration completed"),
            ev_completed(&second_response_id),
        ]),
    )
    .await;
}

fn configure_multi_agent_v2_with_role(
    config: &mut codex_core::config::Config,
    model_provider_base_url: &str,
) {
    config
        .features
        .enable(Feature::Collab)
        .expect("test config should allow feature update");
    config
        .features
        .enable(Feature::MultiAgentV2)
        .expect("test config should allow feature update");
    config.multi_agent_v2.subagent_developer_instructions =
        Some(SUBAGENT_DEVELOPER_INSTRUCTIONS.to_string());
    // Keep the root, worker, grandchild, and sibling resident until explicit shutdown.
    config.multi_agent_v2.max_concurrent_threads_per_session = 4;
    let role_path = config.codex_home.join("durable-worker-role.toml");
    std::fs::write(
        &role_path,
        format!(
            "model = \"{ROLE_MODEL}\"\nmodel_reasoning_effort = \"high\"\ndeveloper_instructions = \"{ROLE_DEVELOPER_INSTRUCTIONS}\"\nsandbox_mode = \"read-only\"\nmodel_provider = \"mock\"\n\n[model_providers.mock]\nname = \"mock\"\nbase_url = \"{model_provider_base_url}\"\nenv_key = \"PATH\"\nwire_api = \"responses\"\n"
        ),
    )
    .expect("write durable worker role config");
    config.agent_roles.insert(
        ROLE_NAME.to_string(),
        AgentRoleConfig {
            description: Some("Durable worker role".to_string()),
            config_file: Some(role_path.to_path_buf()),
            nickname_candidates: None,
        },
    );
}
