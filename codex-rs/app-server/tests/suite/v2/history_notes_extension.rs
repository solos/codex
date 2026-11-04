use std::time::Duration;

use anyhow::Result;
use app_test_support::MockResponsesConfig;
use app_test_support::TestAppServer;
use codex_app_server_protocol::ThreadStartParams;
use codex_app_server_protocol::TurnStartParams;
use codex_app_server_protocol::UserInput;
use core_test_support::load_default_config_for_test;
use core_test_support::responses;
use pretty_assertions::assert_eq;
use serde_json::Value;
use serde_json::json;
use tempfile::TempDir;
use test_case::test_case;
use tokio::time::timeout;
use wiremock::Mock;
use wiremock::Request;
use wiremock::ResponseTemplate;
use wiremock::matchers::body_partial_json;
use wiremock::matchers::method;
use wiremock::matchers::path;

const DEFAULT_READ_TIMEOUT: Duration = Duration::from_secs(30);

const THREAD_HINT: &str =
    "Recent notes (up to 5, most-recent first):\n- /root/notes/latest.md (2 lines, 14 UTF-8 bytes)";
const BRIDGE_HINT: &str = "unstructured notes/thread_hint fixture result";

#[test_case(true, true, 200, THREAD_HINT; "native_hint")]
#[test_case(true, false, 200, THREAD_HINT; "native_hint_without_experimental_capability")]
#[test_case(true, true, 200, ""; "no_notes")]
#[test_case(true, true, 503, THREAD_HINT; "native_failure_does_not_use_bridge")]
#[test_case(false, true, 200, THREAD_HINT; "bridge_hint")]
#[tokio::test]
async fn app_server_uses_configured_notes_backend_for_context_window_hints(
    use_history_notes_extension: bool,
    supports_experimental_context: bool,
    hint_status: u16,
    hint_text: &str,
) -> Result<()> {
    let server = responses::start_mock_server().await;
    let backend = responses::start_mock_server().await;
    let response_mock = responses::mount_sse_once(
        &server,
        responses::sse(vec![
            responses::ev_response_created("resp-1"),
            responses::ev_assistant_message("msg-1", "done"),
            responses::ev_completed("resp-1"),
        ]),
    )
    .await;
    // HTTP MCP stays on the app-server host, including with a remote executor.
    Mock::given(method("POST"))
        .and(path("/mcp"))
        .respond_with(|request: &Request| {
            let body: Value = request.body_json().expect("MCP JSON-RPC request");
            let Some(id) = body.get("id") else {
                return ResponseTemplate::new(202);
            };
            let result = match body["method"].as_str() {
                Some("initialize") => json!({
                    "protocolVersion": body["params"]["protocolVersion"],
                    "capabilities": {"tools": {}},
                    "serverInfo": {"name": "notes-fixture", "version": "1.0.0"},
                }),
                Some("tools/list") => json!({
                    "tools": [{
                        "name": "thread_hint",
                        "inputSchema": {"type": "object", "properties": {}},
                        "annotations": {"readOnlyHint": true},
                        "_meta": {"ui": {"visibility": []}},
                    }],
                }),
                Some("tools/call") => {
                    assert_eq!(body["params"]["name"], "thread_hint");
                    let thread_id = body["params"]["_meta"]["threadId"]
                        .as_str()
                        .expect("threadId metadata");
                    json!({"content": [
                        {"type": "text", "text": format!("manual history hint for thread {thread_id}")},
                        {"type": "text", "text": BRIDGE_HINT},
                    ]})
                }
                _ => {
                    return ResponseTemplate::new(200).set_body_json(json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "error": {"code": -32601, "message": "Method not found"},
                    }));
                }
            };
            ResponseTemplate::new(200).set_body_json(json!({
                "jsonrpc": "2.0", "id": id, "result": result,
            }))
        })
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/mcp"))
        .respond_with(ResponseTemplate::new(405))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/backend-api/codex/alpha/notes/v2/thread_hint"))
        .respond_with(
            ResponseTemplate::new(hint_status).set_body_json(serde_json::json!({
                "text": hint_text
            })),
        )
        .mount(&server)
        .await;

    let codex_home = TempDir::new()?;
    let config = load_default_config_for_test(&codex_home).await;
    let mut model = codex_core::test_support::construct_model_info_offline("mock-model", &config);
    model.supports_experimental_context = supports_experimental_context;
    let catalog_path = codex_home.path().join("models.json");
    std::fs::write(
        &catalog_path,
        serde_json::to_vec(&json!({"models": [model]}))?,
    )?;
    MockResponsesConfig::new(&server.uri())
        .with_root_config(&format!("chatgpt_base_url = \"{}\"", backend.uri()))
        .with_root_config(&format!(
            "model_catalog_json = {}",
            serde_json::to_string(&catalog_path)?
        ))
        .with_model_provider("openai-custom")
        .with_provider_name("OpenAI")
        .with_provider_base_url(&format!("{}/backend-api/codex", server.uri()))
        .with_provider_config("supports_websockets = false\nrequires_openai_auth = true")
        .with_extra_config(&format!(
            "[features.context_management]\nexperimental_mode = false\n\n[features.token_budget]\nenabled = true\nuse_history_notes_extension = {use_history_notes_extension}\n\n[mcp_servers.notes]\nurl = \"{}/mcp\"\nstartup_timeout_sec = 10\n",
            server.uri(),
        ))
        .write(codex_home.path())?;

    let mut app_server = TestAppServer::builder()
        .with_codex_home(codex_home.path())
        .with_env_overrides(&[("OPENAI_API_KEY", None)])
        .build_initialized()
        .await?;
    let thread = app_server
        .start_thread(ThreadStartParams::default())
        .await?
        .thread;
    timeout(
        Duration::from_secs(10),
        app_server.start_turn_and_wait_for_completion(TurnStartParams {
            thread_id: thread.id.clone(),
            input: vec![UserInput::Text {
                text: "inspect history and notes".to_string(),
                text_elements: Vec::new(),
            }],
            ..Default::default()
        }),
    )
    .await??;

    let request = response_mock.single_request();
    if use_history_notes_extension {
        for (namespace, tool_name) in [
            ("history", "list_windows"),
            ("history", "list_items"),
            ("history", "read_item"),
            ("history", "search_contents"),
            ("notes", "list_files_by_prefix"),
            ("notes", "read_file"),
            ("notes", "search_contents"),
            ("notes", "append_to_file"),
            ("notes", "write_file"),
        ] {
            assert!(
                request.tool_by_name(namespace, tool_name).is_some(),
                "app-server should expose {namespace}.{tool_name} to the model"
            );
        }
    }
    assert!(request.tool_by_name("notes", "thread_hint").is_none());

    let input = request.input();
    let bridge_hint_present = request
        .message_input_texts("developer")
        .iter()
        .any(|text| text.contains(BRIDGE_HINT));
    assert_eq!(bridge_hint_present, !use_history_notes_extension);
    let requests = server.received_requests().await.expect("recorded requests");
    let native_requests = requests
        .iter()
        .filter(|request| request.url.path() == "/backend-api/codex/alpha/notes/v2/thread_hint")
        .count();
    assert_eq!(native_requests, usize::from(use_history_notes_extension));
    let bridge_calls = requests
        .iter()
        .filter(|request| request.url.path() == "/mcp")
        .filter(|request| {
            request
                .body_json::<Value>()
                .is_ok_and(|body| body["method"] == "tools/call")
        })
        .count();
    assert_eq!(bridge_calls, usize::from(!use_history_notes_extension));
    let developer_messages: Vec<_> = request
        .message_input_text_groups("developer")
        .into_iter()
        .map(|parts| parts.concat())
        .collect();
    let context_window = developer_messages
        .iter()
        .find(|text| text.contains("<context_window>"))
        .expect("context-window developer message");
    let window_body = context_window
        .split_once("<context_window>")
        .expect("opening tag")
        .1
        .split_once("</context_window>")
        .expect("closing tag")
        .0;
    assert_eq!(
        window_body.contains(THREAD_HINT),
        use_history_notes_extension && hint_status == 200 && !hint_text.is_empty(),
    );
    assert!(!input.iter().any(|item| {
        item["type"] == "function_call"
            && item["namespace"] == "notes"
            && item["name"] == "thread_hint"
    }));

    Ok(())
}
