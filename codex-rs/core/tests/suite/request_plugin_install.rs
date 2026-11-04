#![allow(clippy::unwrap_used)]

use anyhow::Context;
use anyhow::Result;
use codex_config::types::ToolSuggestDisabledTool;
use codex_config::types::ToolSuggestDiscoverable;
use codex_config::types::ToolSuggestDiscoverableType;
use codex_connectors::ConnectorDirectoryCacheContext;
use codex_connectors::ConnectorDirectoryCacheKey;
use codex_core::NewThread;
use codex_core::StartThreadOptions;
use codex_core::TurnInputRequest;
use codex_core::config::Config;
use codex_core_plugins::startup_sync::curated_plugins_repo_path;
use codex_extension_api::ExtensionFuture;
use codex_extension_api::ExtensionRegistryBuilder;
use codex_extension_api::McpServerContribution;
use codex_extension_api::McpServerContributionContext;
use codex_extension_api::McpServerContributor;
use codex_features::Feature;
use codex_login::CodexAuth;
use codex_models_manager::bundled_models_response;
use codex_protocol::approvals::ElicitationAction;
use codex_protocol::approvals::ElicitationRequest;
use codex_protocol::approvals::ElicitationRequestEvent;
use codex_protocol::config_types::CollaborationMode;
use codex_protocol::config_types::ModeKind;
use codex_protocol::config_types::Settings;
use codex_protocol::models::PermissionProfile;
use codex_protocol::protocol::AskForApproval;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::Op;
use codex_protocol::protocol::SessionSource;
use codex_protocol::protocol::SubAgentSource;
use codex_protocol::protocol::ThreadSettingsOverrides;
use codex_protocol::user_input::UserInput;
use codex_utils_path_uri::PathUri;
use core_test_support::apps_test_server::AppsTestServer;
use core_test_support::context_snapshot;
use core_test_support::context_snapshot::ContextSnapshotOptions;
use core_test_support::responses::ev_assistant_message;
use core_test_support::responses::ev_completed;
use core_test_support::responses::ev_function_call;
use core_test_support::responses::ev_response_created;
use core_test_support::responses::mount_sse_once;
use core_test_support::responses::mount_sse_sequence;
use core_test_support::responses::sse;
use core_test_support::responses::start_mock_server;
use core_test_support::skip_if_no_network;
use core_test_support::skip_if_wine_exec;
use core_test_support::test_codex::TestCodex;
use core_test_support::test_codex::test_codex;
use core_test_support::test_codex::turn_permission_fields;
use core_test_support::wait_for_event;
use core_test_support::wait_for_event_match;
use pretty_assertions::assert_eq;
use serde_json::Value;
use serde_json::json;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::time::Duration;
use std::time::Instant;
use tempfile::TempDir;
use test_case::test_case;
use tokio::sync::Notify;
use wiremock::Mock;
use wiremock::MockGuard;
use wiremock::MockServer;
use wiremock::ResponseTemplate;
use wiremock::matchers::method;
use wiremock::matchers::path;
use wiremock::matchers::query_param;

use super::rmcp_client::remote_aware_environment_id;
use super::rmcp_client::remote_aware_stdio_server_bin;

const TOOL_SEARCH_TOOL_NAME: &str = "tool_search";
const LIST_AVAILABLE_PLUGINS_TO_INSTALL_TOOL_NAME: &str = "list_available_plugins_to_install";
const REQUEST_PLUGIN_INSTALL_TOOL_NAME: &str = "request_plugin_install";
const DISCOVERABLE_GMAIL_ID: &str = "connector_68df038e0ba48191908c8434991bbac2";
const REMOTE_CALENDAR_PLUGIN_CONFIG_ID: &str = "calendar@openai-curated-remote";
const REMOTE_CALENDAR_PLUGIN_ID: &str = "plugin_calendar";
const CALENDAR_CONNECTOR_ID: &str = "calendar";
const CALENDAR_NAMESPACE: &str = "mcp__codex_apps__calendar";
const CALENDAR_CREATE_EVENT_TOOL: &str = "_create_event";
const STEP_PREPARATION_MCP_SERVER: &str = "step_preparation";

struct StartupMcpBarrier {
    block_next: AtomicBool,
    entered: Notify,
    release: Notify,
}

impl McpServerContributor<Config> for StartupMcpBarrier {
    fn id(&self) -> &'static str {
        "startup_recommendation_barrier"
    }

    fn contribute<'a>(
        &'a self,
        _context: McpServerContributionContext<'a, Config>,
    ) -> ExtensionFuture<'a, Vec<McpServerContribution>> {
        Box::pin(async move {
            if self.block_next.swap(false, Ordering::SeqCst) {
                self.entered.notify_one();
                self.release.notified().await;
            }
            Vec::new()
        })
    }
}

fn tool_names(body: &Value) -> Vec<String> {
    body.get("tools")
        .and_then(Value::as_array)
        .map(|tools| {
            tools
                .iter()
                .filter_map(|tool| {
                    tool.get("name")
                        .or_else(|| tool.get("type"))
                        .and_then(Value::as_str)
                        .map(str::to_string)
                })
                .collect()
        })
        .unwrap_or_default()
}

async fn mount_recommendations(server: &wiremock::MockServer, response: ResponseTemplate) {
    Mock::given(method("GET"))
        .and(path("/ps/plugins/suggested/codex"))
        .and(query_param("scope", "GLOBAL"))
        .respond_with(response)
        .mount(server)
        .await;
}

async fn wait_for_startup_recommendations(server: &MockServer) -> Result<()> {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if server
                .received_requests()
                .await
                .unwrap_or_default()
                .iter()
                .any(|request| request.url.path() == "/ps/plugins/suggested/codex")
            {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .context("session startup should request recommendations before the first turn")
}

fn assert_legacy_tools(body: &Value) {
    let tools = tool_names(body);
    assert!(!tools.iter().any(|name| name == TOOL_SEARCH_TOOL_NAME));
    assert!(
        tools
            .iter()
            .any(|name| name == LIST_AVAILABLE_PLUGINS_TO_INSTALL_TOOL_NAME),
        "legacy mode should expose {LIST_AVAILABLE_PLUGINS_TO_INSTALL_TOOL_NAME}: {tools:?}"
    );
    assert!(
        tools
            .iter()
            .any(|name| name == REQUEST_PLUGIN_INSTALL_TOOL_NAME),
        "legacy mode should expose {REQUEST_PLUGIN_INSTALL_TOOL_NAME}: {tools:?}"
    );
}

async fn build_test(
    server: &wiremock::MockServer,
    apps_server: &AppsTestServer,
) -> Result<TestCodex> {
    let mut builder = test_codex()
        .with_auth(CodexAuth::create_dummy_chatgpt_auth_for_testing())
        .with_config({
            let apps_base_url = apps_server.chatgpt_base_url.clone();
            move |config| {
                config
                    .permissions
                    .set_permission_profile(PermissionProfile::Disabled)
                    .expect("test config should allow disabled permissions");
                configure_apps_without_search_tool(config, apps_base_url.as_str());
            }
        });
    builder.build_with_auto_env(server).await
}

async fn build_gated_step_preparation_test(
    server: &MockServer,
    apps_server: &AppsTestServer,
) -> Result<TestCodex> {
    let command = remote_aware_stdio_server_bin()?;
    let environment_id = remote_aware_environment_id();
    let apps_base_url = apps_server.chatgpt_base_url.clone();
    let mut builder = test_codex()
        .with_auth(CodexAuth::create_dummy_chatgpt_auth_for_testing())
        .with_config(move |config| {
            config
                .permissions
                .set_permission_profile(PermissionProfile::Disabled)
                .expect("test config should allow disabled permissions");
            configure_apps_without_search_tool(config, apps_base_url.as_str());

            let barrier_file = config.cwd.join("allow-step-preparation-initialize");
            let pid_file = config.cwd.join("step-preparation.pid");
            let mut servers = config.mcp_servers.get().clone();
            servers.insert(
                STEP_PREPARATION_MCP_SERVER.to_string(),
                serde_json::from_value(json!({
                    "command": command.clone(),
                    "environment_id": environment_id.clone(),
                    "cwd": config.cwd,
                    "env": {
                        "MCP_TEST_INITIALIZE_BARRIER_FILE": barrier_file,
                        "MCP_TEST_PID_FILE": pid_file,
                    },
                    "enabled_tools": ["echo"],
                    "startup_timeout_sec": 10,
                }))
                .expect("test MCP server configuration"),
            );
            config
                .mcp_servers
                .set(servers)
                .expect("test config should allow MCP servers");
        });
    builder.build_with_auto_env(server).await
}

async fn start_gated_step_preparation(test: &TestCodex, server: &MockServer) -> Result<PathUri> {
    // Settle startup work before clearing its cache so these tests still exercise
    // a fresh recommendation fetch alongside first-turn MCP discovery.
    wait_for_startup_recommendations(server).await?;
    let plugins_manager = test.thread_manager.plugins_manager();
    let auth = test.thread_manager.auth_manager().auth().await;
    plugins_manager
        .recommended_plugins_mode_for_config(&test.config.plugins_config_input(), auth.as_ref())
        .await;
    plugins_manager.clear_recommended_plugins_cache();
    let prior_recommendation_count = server
        .received_requests()
        .await
        .unwrap_or_default()
        .into_iter()
        .filter(|request| request.url.path() == "/ps/plugins/suggested/codex")
        .count();
    let (sandbox_policy, permission_profile) =
        turn_permission_fields(PermissionProfile::Disabled, test.config.cwd.as_path());
    test.codex
        .start_or_steer_turn(
            TurnInputRequest::user_input(vec![UserInput::Text {
                text: "prepare MCP and plugin recommendations".to_string(),
                text_elements: Vec::new(),
            }])
            .with_thread_settings(ThreadSettingsOverrides {
                approval_policy: Some(AskForApproval::Never),
                sandbox_policy: Some(sandbox_policy),
                permission_profile,
                ..Default::default()
            }),
        )
        .await?;

    let fs = test.fs();
    let pid_file = PathUri::from_host_native_path(test.config.cwd.join("step-preparation.pid"))?;
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let mcp_started = fs
                .read_file_text(&pid_file, Default::default(), /*sandbox*/ None)
                .await
                .is_ok_and(|pid| !pid.trim().is_empty());
            let recommendation_count = server
                .received_requests()
                .await
                .unwrap_or_default()
                .into_iter()
                .filter(|request| request.url.path() == "/ps/plugins/suggested/codex")
                .count();
            if mcp_started && recommendation_count > prior_recommendation_count {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .context("MCP startup and plugin recommendations should begin before MCP is released")?;

    PathUri::from_host_native_path(test.config.cwd.join("allow-step-preparation-initialize"))
        .map_err(Into::into)
}

async fn start_install_turn(test: &TestCodex, prompt: &str) -> Result<ElicitationRequestEvent> {
    let (sandbox_policy, permission_profile) =
        turn_permission_fields(PermissionProfile::Disabled, test.config.cwd.as_path());
    test.codex
        .start_or_steer_turn(
            TurnInputRequest::user_input(vec![UserInput::Text {
                text: prompt.to_string(),
                text_elements: Vec::new(),
            }])
            .with_thread_settings(ThreadSettingsOverrides {
                approval_policy: Some(AskForApproval::Never),
                sandbox_policy: Some(sandbox_policy),
                permission_profile,
                collaboration_mode: Some(CollaborationMode {
                    mode: ModeKind::Default,
                    settings: Settings {
                        model: test.session_configured.model.clone(),
                        reasoning_effort: None,
                        developer_instructions: None,
                    },
                }),
                ..Default::default()
            }),
        )
        .await?;

    Ok(wait_for_event_match(&test.codex, |event| match event {
        EventMsg::ElicitationRequest(request) => Some(request.clone()),
        _ => None,
    })
    .await)
}

async fn resolve_install_elicitation(
    test: &TestCodex,
    elicitation: ElicitationRequestEvent,
    decision: ElicitationAction,
) -> Result<()> {
    test.codex
        .submit(Op::ResolveElicitation {
            server_name: elicitation.server_name,
            request_id: elicitation.id,
            decision,
            content: None,
            meta: None,
        })
        .await?;
    wait_for_event(&test.codex, |event| {
        matches!(event, EventMsg::TurnComplete(_))
    })
    .await;
    Ok(())
}

async fn mount_remote_calendar_recommendation(server: &wiremock::MockServer) {
    Mock::given(method("GET"))
        .and(path("/ps/plugins/suggested/codex"))
        .and(query_param("scope", "GLOBAL"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "enabled": true,
            "plugins": [{
                "id": REMOTE_CALENDAR_PLUGIN_ID,
                "name": "calendar",
                "display_name": "Calendar"
            }]
        })))
        .expect(1)
        .mount(server)
        .await;
    Mock::given(method("GET"))
        .and(path(format!("/ps/plugins/{REMOTE_CALENDAR_PLUGIN_ID}")))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": REMOTE_CALENDAR_PLUGIN_ID,
            "name": "calendar",
            "scope": "GLOBAL",
            "status": "ENABLED",
            "installation_policy": "AVAILABLE",
            "authentication_policy": "ON_USE",
            "release": {
                "display_name": "Calendar",
                "description": "Manage calendar events.",
                "app_ids": [CALENDAR_CONNECTOR_ID],
                "interface": {}
            }
        })))
        .expect(1)
        .mount(server)
        .await;
}

fn remote_installed_plugins_response(plugins: Vec<Value>) -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_json(json!({
        "plugins": plugins,
        "pagination": {
            "next_page_token": null
        }
    }))
}

async fn mount_empty_remote_installed_plugins(server: &wiremock::MockServer) -> MockGuard {
    Mock::given(method("GET"))
        .and(path("/ps/plugins/installed"))
        .respond_with(remote_installed_plugins_response(Vec::new()))
        .mount_as_scoped(server)
        .await
}

async fn mount_remote_calendar_installed_plugins(server: &wiremock::MockServer) {
    Mock::given(method("GET"))
        .and(path("/ps/plugins/installed"))
        .respond_with(remote_installed_plugins_response(vec![json!({
            "id": REMOTE_CALENDAR_PLUGIN_ID,
            "name": "calendar",
            "scope": "GLOBAL",
            "status": "ENABLED",
            "installation_policy": "AVAILABLE",
            "authentication_policy": "ON_USE",
            "release": {
                "display_name": "Calendar",
                "description": "Manage calendar events.",
                "interface": {}
            },
            "enabled": true
        })]))
        .with_priority(1)
        .mount(server)
        .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mcp_discovery_overlaps_endpoint_plugin_recommendations() -> Result<()> {
    skip_if_wine_exec!(
        Ok(()),
        "requires a Windows test_stdio_server in the Wine-exec environment"
    );
    skip_if_no_network!(Ok(()));

    let server = start_mock_server().await;
    let apps_server = AppsTestServer::mount(&server).await?;
    mount_recommendations(
        &server,
        ResponseTemplate::new(200).set_body_json(json!({
            "enabled": true,
            "plugins": [{
                "id": "plugin_github",
                "name": "github",
                "display_name": "GitHub"
            }]
        })),
    )
    .await;
    let response = mount_sse_once(
        &server,
        sse(vec![
            ev_response_created("concurrent-endpoint"),
            ev_assistant_message("endpoint-message", "done"),
            ev_completed("concurrent-endpoint"),
        ]),
    )
    .await;
    let test = build_gated_step_preparation_test(&server, &apps_server).await?;

    let barrier = start_gated_step_preparation(&test, &server).await?;
    assert!(
        response.requests().is_empty(),
        "sampling should wait for the complete MCP catalog"
    );
    test.fs()
        .write_file(
            &barrier,
            b"ready".to_vec(),
            Default::default(),
            /*sandbox*/ None,
        )
        .await?;
    wait_for_event(&test.codex, |event| {
        matches!(event, EventMsg::TurnComplete(_))
    })
    .await;

    let request = response.single_request();
    assert!(
        request
            .message_input_texts("developer")
            .join("\n")
            .contains("github@openai-curated-remote"),
        "the completed request should preserve endpoint recommendations"
    );
    assert!(
        request
            .tool_by_name("mcp__step_preparation", "echo")
            .is_some(),
        "the completed request should expose the live gated MCP tool"
    );

    test.codex.shutdown_and_wait().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn interrupting_concurrent_step_preparation_prevents_sampling() -> Result<()> {
    skip_if_wine_exec!(
        Ok(()),
        "requires a Windows test_stdio_server in the Wine-exec environment"
    );
    skip_if_no_network!(Ok(()));

    let server = start_mock_server().await;
    let apps_server = AppsTestServer::mount(&server).await?;
    mount_recommendations(
        &server,
        ResponseTemplate::new(200).set_body_json(json!({"enabled": true, "plugins": []})),
    )
    .await;
    let response = mount_sse_once(
        &server,
        sse(vec![
            ev_response_created("cancelled-step-preparation"),
            ev_assistant_message("cancelled-message", "unexpected"),
            ev_completed("cancelled-step-preparation"),
        ]),
    )
    .await;
    let test = build_gated_step_preparation_test(&server, &apps_server).await?;

    let barrier = start_gated_step_preparation(&test, &server).await?;
    assert!(response.requests().is_empty());
    test.codex.submit(Op::Interrupt).await?;
    wait_for_event(&test.codex, |event| {
        matches!(event, EventMsg::TurnAborted(_))
    })
    .await;
    assert!(
        response.requests().is_empty(),
        "cancelling concurrent step preparation must prevent model sampling"
    );

    test.fs()
        .write_file(
            &barrier,
            b"ready".to_vec(),
            Default::default(),
            /*sandbox*/ None,
        )
        .await?;
    test.codex.shutdown_and_wait().await?;
    assert!(
        response.requests().is_empty(),
        "releasing the cancelled MCP startup must not revive the aborted turn"
    );
    Ok(())
}

#[test_case(ResponseTemplate::new(200).set_body_json(json!({"enabled": false, "plugins": []})); "endpoint disabled")]
#[test_case(ResponseTemplate::new(503); "fetch failure")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unavailable_recommendations_preserve_legacy_workflow(
    response: ResponseTemplate,
) -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = start_mock_server().await;
    let apps_server = AppsTestServer::mount(&server).await?;
    mount_recommendations(&server, response).await;
    let call_id = "list-installable-tools";
    let mock = mount_sse_sequence(
        &server,
        vec![
            sse(vec![
                ev_response_created("resp-1"),
                ev_function_call(call_id, LIST_AVAILABLE_PLUGINS_TO_INSTALL_TOOL_NAME, "{}"),
                ev_completed("resp-1"),
            ]),
            sse(vec![
                ev_response_created("resp-2"),
                ev_assistant_message("msg-1", "done"),
                ev_completed("resp-2"),
            ]),
        ],
    )
    .await;
    let auth = CodexAuth::create_dummy_chatgpt_auth_for_testing();
    let mut builder = test_codex()
        .with_auth(auth.clone())
        .with_pre_build_hook(|home| {
            std::fs::write(
                home.join("config.toml"),
                "[apps.calendar]\nenabled = false\n",
            )
            .expect("write disabled calendar app");
        })
        .with_config(move |config| {
            configure_apps_without_search_tool(config, &apps_server.chatgpt_base_url);
            config.tool_suggest.discoverables.extend(
                [CALENDAR_CONNECTOR_ID, "connector_missing"].map(|id| ToolSuggestDiscoverable {
                    kind: ToolSuggestDiscoverableType::Connector,
                    id: id.to_string(),
                }),
            );
        });
    let test = builder.build_with_auto_env(&server).await?;
    let cache_key = ConnectorDirectoryCacheKey::new(
        server.uri(),
        auth.get_account_id(),
        auth.get_chatgpt_user_id(),
        auth.is_workspace_account(),
    );
    let client = codex_login::default_client::create_client();
    codex_connectors::list_all_connectors_with_options(
        ConnectorDirectoryCacheContext::new(test.home.path().to_path_buf(), cache_key),
        auth.is_workspace_account(),
        /*force_refetch*/ true,
        |path| {
            let request = client.get(format!("{}{path}", server.uri()));
            async move { Ok(request.send().await?.json().await?) }
        },
    )
    .await?;
    test.submit_turn_with_approval_and_permission_profile(
        "list tools",
        AskForApproval::Never,
        PermissionProfile::Disabled,
    )
    .await?;

    let requests = mock.requests();
    assert_eq!(requests.len(), 2);
    let request = &requests[0];
    assert!(
        !request
            .message_input_texts("developer")
            .join("\n")
            .contains("<recommended_plugins>")
    );
    assert_legacy_tools(&request.body_json());
    let output = requests[1]
        .function_call_output_text(call_id)
        .expect("list tool output");
    let output: Value = serde_json::from_str(&output)?;
    assert_eq!(
        output["tools"],
        json!([
            {
                "id": DISCOVERABLE_GMAIL_ID,
                "name": "Gmail",
                "description": "Find and summarize email threads.",
                "tool_type": "connector",
                "has_skills": false,
                "mcp_server_names": [],
                "app_connector_ids": []
            },
            {
                "id": "connector_missing",
                "name": "connector_missing",
                "description": null,
                "tool_type": "connector",
                "has_skills": false,
                "mcp_server_names": [],
                "app_connector_ids": []
            }
        ])
    );
    Ok(())
}

#[test_case(false; "legacy connector")]
#[test_case(true; "recommended plugin")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn subagent_install_request_returns_root_only_error(
    recommended_plugins_enabled: bool,
) -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = start_mock_server().await;
    let apps_server = AppsTestServer::mount(&server).await?;
    let (recommendations, arguments) = if recommended_plugins_enabled {
        (
            json!({
                "enabled": true,
                "plugins": [{
                    "id": REMOTE_CALENDAR_PLUGIN_ID,
                    "name": "calendar",
                    "display_name": "Calendar"
                }]
            }),
            json!({
                "plugin_id": REMOTE_CALENDAR_PLUGIN_CONFIG_ID,
                "suggest_reason": "Use Calendar for this task"
            }),
        )
    } else {
        (
            json!({"enabled": false, "plugins": []}),
            json!({
                "tool_type": "connector",
                "action_type": "install",
                "tool_id": DISCOVERABLE_GMAIL_ID,
                "suggest_reason": "Use Gmail for this task"
            }),
        )
    };
    mount_recommendations(
        &server,
        ResponseTemplate::new(200).set_body_json(recommendations),
    )
    .await;
    let call_id = "subagent-plugin-install";
    let mock = mount_sse_sequence(
        &server,
        vec![
            sse(vec![
                ev_response_created("resp-1"),
                ev_function_call(
                    call_id,
                    REQUEST_PLUGIN_INSTALL_TOOL_NAME,
                    &arguments.to_string(),
                ),
                ev_completed("resp-1"),
            ]),
            sse(vec![
                ev_response_created("resp-2"),
                ev_assistant_message("msg-1", "done"),
                ev_completed("resp-2"),
            ]),
        ],
    )
    .await;
    let test = build_test(&server, &apps_server).await?;
    let NewThread {
        thread: subagent, ..
    } = test
        .thread_manager
        .start_thread(StartThreadOptions {
            session_source: Some(SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
                parent_thread_id: test.session_configured.thread_id,
                depth: 1,
                agent_path: None,
                agent_nickname: None,
                agent_role: None,
            })),
            ..StartThreadOptions::new(test.config.clone())
        })
        .await?;
    subagent
        .start_or_steer_turn(
            TurnInputRequest::user_input(vec![UserInput::Text {
                text: "Use the requested integration.".to_string(),
                text_elements: Vec::new(),
            }])
            .with_thread_settings(ThreadSettingsOverrides {
                approval_policy: Some(AskForApproval::Never),
                permission_profile: Some(PermissionProfile::Disabled),
                ..Default::default()
            }),
        )
        .await?;
    wait_for_event(&subagent, |event| {
        assert!(
            !matches!(event, EventMsg::ElicitationRequest(_)),
            "subagents must not request plugin installation"
        );
        matches!(event, EventMsg::TurnComplete(_))
    })
    .await;

    let requests = mock.requests();
    assert_eq!(requests.len(), 2);
    assert_eq!(
        requests[1].function_call_output_text(call_id).as_deref(),
        Some("request_plugin_install can only be used by the root thread")
    );
    subagent.shutdown_and_wait().await?;
    test.codex.shutdown_and_wait().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn endpoint_mode_injects_candidates_hides_list_and_rejects_invented_ids() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = start_mock_server().await;
    let apps_server = AppsTestServer::mount(&server).await?;
    mount_recommendations(
        &server,
        ResponseTemplate::new(200).set_body_json(json!({
            "enabled": true,
            "plugins": [
                {
                    "id": "plugin_google_calendar",
                    "name": "google-calendar",
                    "display_name": "Google Calendar"
                },
                {
                    "id": "plugin_github",
                    "name": "github",
                    "display_name": "GitHub"
                }
            ]
        })),
    )
    .await;
    let call_id = "invented-plugin";
    let mock = mount_sse_sequence(
        &server,
        vec![
            sse(vec![
                ev_response_created("resp-1"),
                ev_function_call(
                    call_id,
                    REQUEST_PLUGIN_INSTALL_TOOL_NAME,
                    &serde_json::to_string(&json!({
                        "plugin_id": "invented@openai-curated-remote",
                        "suggest_reason": "Try this"
                    }))?,
                ),
                ev_completed("resp-1"),
            ]),
            sse(vec![
                ev_response_created("resp-2"),
                ev_assistant_message("msg-1", "done"),
                ev_completed("resp-2"),
            ]),
        ],
    )
    .await;
    let test = build_test(&server, &apps_server).await?;

    test.submit_turn("suggest a plugin").await?;

    let requests = mock.requests();
    assert_eq!(requests.len(), 2);
    for request in &requests {
        let recommendations = request
            .message_input_texts("developer")
            .into_iter()
            .filter(|text| text.starts_with("<recommended_plugins>"))
            .collect::<Vec<_>>();
        assert_eq!(
            recommendations,
            vec![concat!(
                "<recommended_plugins>\n",
                "Here is a list of plugins that are available but not installed.\n\n",
                "- GitHub (github@openai-curated-remote)\n",
                "- Google Calendar (google-calendar@openai-curated-remote)\n",
                "</recommended_plugins>",
            )]
        );
    }
    let body = requests[0].body_json();
    let tools = tool_names(&body);
    assert!(
        !tools
            .iter()
            .any(|name| name == LIST_AVAILABLE_PLUGINS_TO_INSTALL_TOOL_NAME)
    );
    assert!(
        tools
            .iter()
            .any(|name| name == REQUEST_PLUGIN_INSTALL_TOOL_NAME)
    );
    let output = requests[1]
        .function_call_output_text(call_id)
        .expect("request tool output");
    assert!(output.contains("<recommended_plugins> list"));
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn endpoint_recommendation_hydrates_install_identity_after_selection() -> Result<()> {
    skip_if_no_network!(Ok(()));

    run_remote_plugin_install_metadata_case().await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn endpoint_recommendation_skips_unavailable_plugin_elicitation() -> Result<()> {
    skip_if_no_network!(Ok(()));

    const REMOTE_PLUGIN_ID: &str = "plugin_connector_github";

    let server = start_mock_server().await;
    let apps_server = AppsTestServer::mount(&server).await?;
    mount_recommendations(
        &server,
        ResponseTemplate::new(200).set_body_json(json!({
            "enabled": true,
            "plugins": [{
                "id": REMOTE_PLUGIN_ID,
                "name": "github",
                "display_name": "GitHub"
            }]
        })),
    )
    .await;
    Mock::given(method("GET"))
        .and(path(format!("/ps/plugins/{REMOTE_PLUGIN_ID}")))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": REMOTE_PLUGIN_ID,
            "name": "github",
            "scope": "GLOBAL",
            "status": "DISABLED_BY_ADMIN",
            "installation_policy": "AVAILABLE",
            "authentication_policy": "ON_USE",
            "release": {
                "display_name": "GitHub",
                "description": "Work with GitHub repositories.",
                "app_ids": ["connector_github"],
                "interface": {}
            }
        })))
        .expect(1)
        .mount(&server)
        .await;
    let call_id = "install-unavailable-github";
    let mock = mount_sse_sequence(
        &server,
        vec![
            sse(vec![
                ev_response_created("resp-1"),
                ev_function_call(
                    call_id,
                    REQUEST_PLUGIN_INSTALL_TOOL_NAME,
                    &serde_json::to_string(&json!({
                        "plugin_id": "github@openai-curated-remote",
                        "suggest_reason": "Use GitHub for this request"
                    }))?,
                ),
                ev_completed("resp-1"),
            ]),
            sse(vec![
                ev_response_created("resp-2"),
                ev_assistant_message("msg-1", "done"),
                ev_completed("resp-2"),
            ]),
        ],
    )
    .await;
    let test = build_test(&server, &apps_server).await?;

    test.submit_turn("use GitHub").await?;

    let requests = mock.requests();
    assert_eq!(requests.len(), 2);
    assert_eq!(
        requests[1].function_call_output_text(call_id).as_deref(),
        Some("The recommended plugins for this turn are no longer available.")
    );
    Ok(())
}

#[derive(Clone, Copy)]
enum RefreshedAppsTools {
    Available,
    Missing,
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn remote_plugin_install_refreshes_plugin_and_apps_tool_caches() -> Result<()> {
    skip_if_no_network!(Ok(()));

    run_remote_plugin_install_refresh_case(RefreshedAppsTools::Available).await?;
    run_remote_plugin_install_refresh_case(RefreshedAppsTools::Missing).await
}

async fn run_remote_plugin_install_refresh_case(refreshed_tools: RefreshedAppsTools) -> Result<()> {
    let server = start_mock_server().await;
    let tools_available = Arc::new(AtomicBool::new(false));
    let apps_server = match refreshed_tools {
        RefreshedAppsTools::Available => {
            AppsTestServer::mount_with_tools_available_when(&server, Arc::clone(&tools_available))
                .await?
        }
        RefreshedAppsTools::Missing => AppsTestServer::mount_without_tools(&server).await?,
    };
    mount_remote_calendar_recommendation(&server).await;
    let initial_remote_installed_plugins = mount_empty_remote_installed_plugins(&server).await;

    let install_call_id = "install-calendar";
    let suggest_reason = "Use Calendar for this request";
    let mock = mount_sse_sequence(
        &server,
        vec![
            sse(vec![
                ev_response_created("resp-1"),
                ev_function_call(
                    install_call_id,
                    REQUEST_PLUGIN_INSTALL_TOOL_NAME,
                    &serde_json::to_string(&json!({
                        "plugin_id": REMOTE_CALENDAR_PLUGIN_CONFIG_ID,
                        "suggest_reason": suggest_reason
                    }))?,
                ),
                ev_completed("resp-1"),
            ]),
            sse(vec![
                ev_response_created("resp-2"),
                ev_assistant_message("msg-1", "done"),
                ev_completed("resp-2"),
            ]),
            sse(vec![
                ev_response_created("resp-3"),
                ev_assistant_message("msg-2", "catalog still current"),
                ev_completed("resp-3"),
            ]),
        ],
    )
    .await;
    let test = build_test(&server, &apps_server).await?;

    let elicitation = start_install_turn(&test, "use Calendar").await?;
    mount_remote_calendar_installed_plugins(&server).await;
    drop(initial_remote_installed_plugins);
    tools_available.store(
        matches!(refreshed_tools, RefreshedAppsTools::Available),
        Ordering::SeqCst,
    );
    resolve_install_elicitation(&test, elicitation, ElicitationAction::Accept).await?;

    let requests = mock.requests();
    assert_eq!(requests.len(), 2);
    assert!(
        requests[0]
            .tool_by_name(CALENDAR_NAMESPACE, CALENDAR_CREATE_EVENT_TOOL)
            .is_none(),
        "calendar tool should be absent before the remote install"
    );
    let completed = matches!(refreshed_tools, RefreshedAppsTools::Available);
    assert_eq!(
        serde_json::from_str::<Value>(
            &requests[1]
                .function_call_output_text(install_call_id)
                .expect("install tool output")
        )?,
        json!({
            "completed": completed,
            "user_confirmed": true,
            "tool_type": "plugin",
            "action_type": "install",
            "tool_id": REMOTE_CALENDAR_PLUGIN_CONFIG_ID,
            "tool_name": "Calendar",
            "suggest_reason": suggest_reason
        })
    );
    assert_eq!(
        requests[1]
            .tool_by_name(CALENDAR_NAMESPACE, CALENDAR_CREATE_EVENT_TOOL)
            .is_some(),
        completed,
        "the resumed router should reflect the refreshed Apps tools"
    );
    assert!(
        !tool_names(&requests[1].body_json())
            .iter()
            .any(|name| name == REQUEST_PLUGIN_INSTALL_TOOL_NAME),
        "the refreshed installed-plugin cache should filter the cached recommendation"
    );
    drop(requests);
    let current_config = test.codex.config().await;
    let _ = test
        .codex
        .refresh_runtime_config(current_config, test.config.clone())
        .await;
    test.submit_turn("check whether Calendar is still installed")
        .await?;
    let requests = mock.requests();
    assert_eq!(requests.len(), 3);
    assert_eq!(
        requests[2]
            .tool_by_name(CALENDAR_NAMESPACE, CALENDAR_CREATE_EVENT_TOOL)
            .is_some(),
        completed,
        "an unrelated runtime publication must retain the refreshed Apps catalog"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn endpoint_mode_with_no_eligible_candidates_exposes_no_suggestion_tools() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = start_mock_server().await;
    let apps_server = AppsTestServer::mount(&server).await?;
    mount_recommendations(
        &server,
        ResponseTemplate::new(200).set_body_json(json!({
            "enabled": true,
            "plugins": [{
                "id": "plugin_google_calendar",
                "name": "google-calendar",
                "display_name": "Google Calendar"
            }]
        })),
    )
    .await;
    let mock = mount_sse_once(
        &server,
        sse(vec![
            ev_response_created("resp-1"),
            ev_assistant_message("msg-1", "done"),
            ev_completed("resp-1"),
        ]),
    )
    .await;
    let mut builder = test_codex()
        .with_auth(CodexAuth::create_dummy_chatgpt_auth_for_testing())
        .with_config({
            let apps_base_url = apps_server.chatgpt_base_url.clone();
            move |config| {
                configure_apps_without_search_tool(config, apps_base_url.as_str());
                config.tool_suggest.disabled_tools = vec![ToolSuggestDisabledTool::plugin(
                    "google-calendar@openai-curated-remote",
                )];
            }
        });
    let test = builder.build_with_auto_env(&server).await?;

    test.submit_turn("list tools").await?;

    let request = mock.single_request();
    assert!(
        !request
            .message_input_texts("developer")
            .join("\n")
            .contains("<recommended_plugins>")
    );
    let tools = tool_names(&request.body_json());
    assert!(
        !tools
            .iter()
            .any(|name| name == LIST_AVAILABLE_PLUGINS_TO_INSTALL_TOOL_NAME)
    );
    assert!(
        !tools
            .iter()
            .any(|name| name == REQUEST_PLUGIN_INSTALL_TOOL_NAME)
    );
    Ok(())
}
