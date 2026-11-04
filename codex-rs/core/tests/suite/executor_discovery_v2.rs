//! Verifies V2 discovery, plugin acceptance across auth changes, and cache behavior.

use anyhow::Context;
use anyhow::Result;
use codex_core::TurnInputRequest;
use codex_core::config::Config;
use codex_core::windows_sandbox::WindowsSandboxLevelExt;
use codex_core_plugins::ExecutorPluginProvider;
use codex_core_plugins::PluginCatalog;
use codex_core_plugins::PluginCatalogEntry;
use codex_core_plugins::PluginIdentity;
use codex_core_plugins::PluginListQuery;
use codex_core_plugins::PluginProvider;
use codex_core_plugins::PluginProviderFuture;
use codex_core_plugins::PluginSourceLocation;
use codex_exec_server::DiscoverV2CapabilitiesResponse;
use codex_exec_server::ExecutorCapabilityDiscoveryV2Cache;
use codex_exec_server::ExecutorCapabilityDiscoveryV2Snapshot;
use codex_exec_server::HttpRequestBodyDeltaNotification;
use codex_exec_server::HttpRequestParams;
use codex_extension_api::ContextContributor;
use codex_extension_api::ExtensionFuture;
use codex_extension_api::ExtensionRegistryBuilder;
use codex_extension_api::McpServerContribution;
use codex_extension_api::McpServerContributionContext;
use codex_extension_api::McpServerContributor;
use codex_extension_api::SelectedPluginSnapshot;
use codex_extension_api::ToolFinishInput;
use codex_extension_api::ToolLifecycleContributor;
use codex_extension_api::ToolLifecycleFuture;
use codex_extension_api::WorldStateContributionInput;
use codex_extension_api::WorldStateSectionContribution;
use codex_features::Feature;
use codex_login::CodexAuth;
use codex_mcp::CODEX_APPS_MCP_SERVER_NAME;
use codex_mcp::McpResourceClient;
use codex_mcp_extension::PluginProviders;
use codex_mcp_extension::install_plugin_providers;
use codex_protocol::config_types::WindowsSandboxLevel;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::Op;
use codex_protocol::protocol::SKILLS_INSTRUCTIONS_OPEN_TAG;
use codex_protocol::protocol::TurnEnvironmentSelections;
use codex_protocol::user_input::UserInput;
use codex_skills_extension::ExecutorSkillProvider;
use codex_skills_extension::SkillProviders;
use codex_skills_extension::SkillsExtensionConfig;
use codex_skills_extension::catalog::SkillAuthority;
use codex_skills_extension::catalog::SkillCatalog;
use codex_skills_extension::catalog::SkillCatalogEntry;
use codex_skills_extension::catalog::SkillPackageId;
use codex_skills_extension::catalog::SkillResourceId;
use codex_skills_extension::catalog::SkillSourceKind;
use codex_skills_extension::install_with_providers;
use core_test_support::apps_test_server::AppsTestServer;
use core_test_support::apps_test_server::apps_enabled_builder;
use core_test_support::context_snapshot;
use core_test_support::context_snapshot::ContextSnapshotOptions;
use core_test_support::responses;
use core_test_support::skip_if_no_network;
use core_test_support::test_codex::test_codex;
use futures::SinkExt;
use futures::StreamExt;
use pretty_assertions::assert_eq;
use serde_json::Value;
use serde_json::json;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use std::time::Duration;
use tokio::net::TcpListener;
use tokio::sync::Mutex;
use tokio::sync::mpsc;
use tokio::sync::oneshot;
use tokio_tungstenite::accept_async;
use tokio_tungstenite::tungstenite::Message;
use tokio_util::task::AbortOnDropHandle;

struct DiscoveryRecorder {
    snapshots: Arc<Mutex<Vec<ExecutorCapabilityDiscoveryV2Snapshot>>>,
    selected_plugins: Arc<Mutex<Option<SelectedPluginSnapshot>>>,
    mcp_snapshots_per_step: Arc<Mutex<Vec<usize>>>,
}

impl McpServerContributor<Config> for DiscoveryRecorder {
    fn id(&self) -> &'static str {
        "discovery-recorder"
    }

    fn contribute<'a>(
        &'a self,
        context: McpServerContributionContext<'a, Config>,
    ) -> ExtensionFuture<'a, Vec<McpServerContribution>> {
        Box::pin(async move {
            if let Some(snapshots) = context.executor_capability_discovery_v2() {
                self.snapshots.lock().await.extend_from_slice(snapshots);
            }
            vec![McpServerContribution::HostedPluginConnectors {
                plugin_id: "plugin_sentinel".to_string(),
                plugin_display_name: "Inert sentinel".to_string(),
                connector_ids: Vec::new(),
            }]
        })
    }

    fn hosted_plugin_ids(&self, _context: McpServerContributionContext<'_, Config>) -> Vec<String> {
        vec!["plugin_sentinel".to_string()]
    }

    fn selected_plugins<'a>(
        &'a self,
        context: McpServerContributionContext<'a, Config>,
        _plugins_config: &'a codex_config::types::PluginsConfigToml,
    ) -> ExtensionFuture<'a, Vec<codex_extension_api::SelectedPlugin<'a>>> {
        Box::pin(async move {
            let mut package_roots = vec!["unavailable-root".to_string()];
            let selected_roots = context
                .ready_selected_capability_roots()
                .unwrap_or_default();
            // Core must reject configured legacy roots when V2 supplies the inventory.
            package_roots.extend(selected_roots.iter().map(|root| root.id.clone()));
            if !selected_roots.is_empty() && context.executor_capability_discovery_v2().is_none() {
                let legacy = context.executor_capability_discovery();
                assert_eq!(
                    legacy.map(|snapshot| snapshot.roots().len()),
                    Some(selected_roots.len())
                );
                assert!(legacy.is_some_and(|snapshot| {
                    snapshot.roots().iter().all(|root| root.result.is_ok())
                }));
            }
            if let Some(snapshots) = context.executor_capability_discovery_v2() {
                for snapshot in snapshots {
                    if let Ok(inventory) = &snapshot.result {
                        package_roots.extend(
                            inventory
                                .plugins
                                .iter()
                                .map(|plugin| format!("{}:{}", snapshot.environment_id, plugin.id)),
                        );
                    }
                }
            }
            package_roots
                .into_iter()
                .map(|selected_root_id| codex_extension_api::SelectedPlugin {
                    selected_root_id,
                    identity: PluginIdentity::Local {
                        plugin_id: "inert-sentinel@market".to_string(),
                    },
                    plugin_id: "inert-sentinel@market".to_string(),
                    enabled: true,
                    mcp: Box::pin(async {
                        codex_extension_api::SelectedPluginContribution {
                            plugin_display_name: "Inert sentinel".to_string(),
                            source_environment_id: codex_exec_server::REMOTE_ENVIRONMENT_ID
                                .to_string(),
                            servers: Vec::new(),
                            connector_ids: Vec::new(),
                        }
                    }),
                })
                .collect()
        })
    }
}

impl codex_extension_api::ToolContributor for DiscoveryRecorder {
    fn prepare_for_step<'a>(
        &'a self,
        input: codex_extension_api::StepPreparationInput<'a>,
    ) -> ExtensionFuture<'a, ()> {
        Box::pin(async move {
            *self.selected_plugins.lock().await = Some(input.selected_plugins.clone());
        })
    }
}

impl ContextContributor for DiscoveryRecorder {
    fn contribute_world_state<'a>(
        &'a self,
        _input: WorldStateContributionInput<'a>,
    ) -> ExtensionFuture<'a, Vec<WorldStateSectionContribution>> {
        Box::pin(async move {
            let snapshot_count = self.snapshots.lock().await.len();
            self.mcp_snapshots_per_step
                .lock()
                .await
                .push(snapshot_count);
            Vec::new()
        })
    }
}

struct Executor {
    url: String,
    inventory: Arc<Mutex<Option<Value>>>,
    requests: Arc<Mutex<Vec<Value>>>,
    _task: AbortOnDropHandle<Result<()>>,
}

#[derive(Clone, Copy)]
enum DiscoveryFailure {
    None,
    Server,
    Disconnect,
}

impl Executor {
    async fn start(supports_v2: bool, failure: DiscoveryFailure) -> Result<Self> {
        Self::start_with_version(supports_v2, failure, "0.164.0").await
    }

    async fn start_with_version(
        supports_v2: bool,
        failure: DiscoveryFailure,
        version: &'static str,
    ) -> Result<Self> {
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let url = format!("ws://{}", listener.local_addr()?);
        let requests = Arc::new(Mutex::new(Vec::new()));
        let inventory = Arc::new(Mutex::new(None::<Value>));
        let served_inventory = Arc::clone(&inventory);
        let recorded = Arc::clone(&requests);
        let discovery_attempts = Arc::new(AtomicUsize::new(0));
        let http = codex_exec_server::Environment::create_for_tests(/*exec_server_url*/ None)?
            .get_http_client();
        let task = tokio::spawn(async move {
            let mut connections = tokio::task::JoinSet::new();
            while let Ok((stream, _)) = listener.accept().await {
                let recorded = Arc::clone(&recorded);
                let inventory = Arc::clone(&served_inventory);
                let discovery_attempts = Arc::clone(&discovery_attempts);
                let http = Arc::clone(&http);
                connections.spawn(async move {
                    let mut socket = accept_async(stream).await?;
                    while let Some(Ok(message)) = socket.next().await {
                        let request: Value = match message {
                            Message::Text(text) => serde_json::from_str(&text)?,
                            Message::Binary(bytes) => serde_json::from_slice(&bytes)?,
                            Message::Close(_) => break,
                            _ => continue,
                        };
                        if request.get("id").is_none() { continue; }
                        recorded.lock().await.push(request.clone());
                        if request["method"] == "http/request" {
                            let params: HttpRequestParams = serde_json::from_value(request["params"].clone())?;
                            let request_id = params.request_id.clone();
                            let streaming = params.stream_response;
                            let mut response = http.http_request(params).await?;
                            let delta = streaming.then(|| HttpRequestBodyDeltaNotification {
                                request_id, seq: 1,
                                delta: std::mem::replace(&mut response.body, Vec::new().into()),
                                done: true, error: None,
                            });
                            socket.send(Message::Text(json!({"id":request["id"],"result":response}).to_string().into())).await?;
                            if let Some(delta) = delta {
                                socket.send(Message::Text(json!({"method":"http/request/bodyDelta","params":delta}).to_string().into())).await?;
                            }
                            continue;
                        }
                        if request["method"] == "capabilities/discoverV2" {
                            let attempt = discovery_attempts.fetch_add(1, Ordering::Relaxed);
                            match failure {
                                // Exhaust the environment's one transport replay so the cache sees Err.
                                DiscoveryFailure::Disconnect if attempt < 2 => break,
                                DiscoveryFailure::Server if attempt == 0 => {
                                    socket.send(Message::Text(json!({"id":request["id"],"error":{"code":-32004,"message":"fixture discovery failure"}}).to_string().into())).await?;
                                    continue;
                                }
                                DiscoveryFailure::None | DiscoveryFailure::Server | DiscoveryFailure::Disconnect => {}
                            }
                        }
                        let result = match request["method"].as_str() {
                            Some("initialize") => Some(json!({"sessionId":"acquisition-only", "environmentInfo": {
                                "shell":{"name":"bash","path":"/bin/bash"},
                                "executorVersion":version,
                                "capabilities":{"sandboxedFileStreaming":true,"capabilityDiscoveryV2":supports_v2}
                            }})),
                            Some("environment/info") => Some(json!({
                                "shell":{"name":"bash","path":"/bin/bash"},
                                "executorVersion":version,
                                "capabilities":{"sandboxedFileStreaming":true,"capabilityDiscoveryV2":supports_v2}
                            })),
                            Some("capabilityRoots/discoverV1") => Some(json!({
                                "roots": request["params"]["roots"].as_array().context("V1 roots")?
                                    .iter().map(|root| json!({
                                        "id":root["id"],"path":root["path"],
                                        "skills":[{"instructions":{
                                            "path":format!("{}/inert-sentinel/SKILL.md", root["path"].as_str().expect("root URI").trim_end_matches('/')),
                                            "contents":"---\nname: inert-sentinel\ndescription: Executor skill used by the acquisition test\n---\n# Sentinel instructions from the captured executor"
                                        }}]
                                    })).collect::<Vec<_>>()
                            })),
                            Some("capabilities/discoverV2") => Some(inventory.lock().await.clone().unwrap_or_else(|| json!({
                                "plugins":[{"id":"inert-sentinel@market","displayName":"inert-sentinel","remotePluginId":null,
                                    "version":"1","root":"file:///inert-sentinel",
                                    "files":{"manifest":{"path":"file:///inert-sentinel/.codex-plugin/plugin.json",
                                        "contents":"{\"name\":\"inert-sentinel\",\"mcpServers\":{\"inert-sentinel\":{\"command\":\"never-run\"}}}"},
                                        "mcpConfig":{"path":"file:///inert-sentinel/.codex-plugin/plugin.json",
                                            "contents":"{\"inert-sentinel\":{\"command\":\"never-run\"}}"},
                                        "appsConfig":null},"skills":[]}],
                                "skills":[{"name":"inert-sentinel","description":"Executor skill used by the acquisition test",
                                    "shortDescription":null,"path":"file:///inert-sentinel/SKILL.md",
                                    "discoveryRoot":{"id":"codex-home-skills","path":"file:///"},"metadata":null}],
                                "warnings":[]
                            }))),
                            Some("fs/open") if request["params"]["path"] == "file:///inert-sentinel/SKILL.md" => Some(json!({"handleId": request["params"]["handleId"]})),
                            Some("fs/readBlock") => Some(json!(codex_exec_server::FsReadBlockResponse {
                                chunk: b"# Sentinel instructions from the captured executor".to_vec().into(),
                                eof: true,
                            })),
                            Some("fs/close") => Some(json!({})),
                            Some("fs/canonicalize") => Some(json!({"path":request["params"]["path"]})),
                            Some("fs/walk") => Some(json!({"entries":[],"errors":[],"truncated":false})),
                            _ => None,
                        };
                        let response = match result {
                            Some(result) => json!({"id":request["id"],"result":result}),
                            None => json!({"id":request["id"],"error":{"code":-32004,"message":"fixture discovery failure"}}),
                        };
                        if socket.send(Message::Text(response.to_string().into())).await.is_err() { break; }
                    }
                    anyhow::Ok(())
                });
            }
            anyhow::Ok(())
        });
        Ok(Self {
            url,
            inventory,
            requests,
            _task: AbortOnDropHandle::new(task),
        })
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn v2_disabled_skips_inventory_discovery() -> Result<()> {
    check_acquisition(/*enabled*/ false, "0.164.0").await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn v2_shares_inventory_with_mcp_and_skills_until_next_turn() -> Result<()> {
    check_acquisition(/*enabled*/ true, "0.164.0").await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn v2_changed_inventory_refreshes_mcp_without_selection_changes() -> Result<()> {
    skip_if_no_network!(Ok(()));

    #[derive(Default)]
    struct InventoryRecorder {
        projections: AtomicUsize,
        projected: Mutex<Option<Arc<DiscoverV2CapabilitiesResponse>>>,
        steps: Mutex<
            Vec<(
                Arc<DiscoverV2CapabilitiesResponse>,
                Arc<DiscoverV2CapabilitiesResponse>,
            )>,
        >,
    }

    impl McpServerContributor<Config> for InventoryRecorder {
        fn id(&self) -> &'static str {
            "inventory-refresh-recorder"
        }

        fn contribute<'a>(
            &'a self,
            context: McpServerContributionContext<'a, Config>,
        ) -> ExtensionFuture<'a, Vec<McpServerContribution>> {
            Box::pin(async move {
                self.projections.fetch_add(1, Ordering::SeqCst);
                *self.projected.lock().await = context
                    .executor_capability_discovery_v2()
                    .and_then(|snapshots| snapshots.first())
                    .and_then(|snapshot| snapshot.result.as_ref().ok())
                    .cloned();
                Vec::new()
            })
        }
    }

    impl codex_extension_api::ToolContributor for InventoryRecorder {
        fn prepare_for_step<'a>(
            &'a self,
            input: codex_extension_api::StepPreparationInput<'a>,
        ) -> ExtensionFuture<'a, ()> {
            Box::pin(async move {
                let snapshots = input
                    .executor_capability_discovery_v2
                    .expect("captured inventories");
                let inventory = snapshots
                    .first()
                    .and_then(|snapshot| snapshot.result.as_ref().ok())
                    .expect("step V2 inventory")
                    .clone();
                let projected = self.projected.lock().await.clone().expect("MCP inventory");
                let mut steps = self.steps.lock().await;
                steps.push((inventory, projected));
                // Model a fresh inventory after each step, without changing selected roots.
                input
                    .thread_store
                    .insert(ExecutorCapabilityDiscoveryV2Cache::default());
            })
        }
    }

    let executor = Executor::start(/*supports_v2*/ true, DiscoveryFailure::None).await?;
    let model = responses::start_mock_server().await;
    let recorder = Arc::new(InventoryRecorder::default());
    let mut extensions = ExtensionRegistryBuilder::new();
    extensions.tool_contributor(recorder.clone());
    extensions.mcp_server_contributor(recorder.clone());
    let test = test_codex()
        .with_extensions(Arc::new(extensions.build()))
        .with_exec_server_url(executor.url.clone())
        .with_config(|config| {
            config.update_plan_enabled = true;
            config
                .features
                .enable(Feature::ExecutorCapabilityDiscoveryV2)
                .expect("enable discovery V2");
        })
        .build_with_auto_env(&model)
        .await?;
    let first = responses::mount_sse_once(
        &model,
        responses::sse(vec![
            responses::ev_response_created("first"),
            responses::ev_function_call(
                "plan-call",
                "update_plan",
                r#"{"plan":[{"step":"Say hello","status":"completed"}]}"#,
            ),
            responses::ev_completed("first"),
        ]),
    )
    .await;
    let second = responses::mount_sse_once(
        &model,
        responses::sse(vec![
            responses::ev_response_created("second"),
            responses::ev_completed("second"),
        ]),
    )
    .await;
    test.submit_turn("Say hello.").await?;
    first.single_request();
    second.single_request();
    {
        let steps = recorder.steps.lock().await;
        let (initial, _) = steps.first().context("first captured inventory")?;
        assert!(
            steps
                .iter()
                .any(|(inventory, _)| !Arc::ptr_eq(initial, inventory)),
            "the next step must acquire a fresh inventory"
        );
        for (inventory, projected) in steps.iter() {
            assert!(
                Arc::ptr_eq(inventory, projected),
                "MCP must project the step's changed inventory even when selections are unchanged"
            );
        }
    }
    let projections = recorder.projections.load(Ordering::SeqCst);
    // Refresh before another step can discover the new inventory. This fixture has no Apps
    // server, but the refresh must still publish exactly once before reporting that absence.
    let error = test
        .codex
        .refresh_codex_apps_tools()
        .await
        .err()
        .expect("no Apps server");
    assert!(error.to_string().contains("unknown MCP server"));
    assert_eq!(recorder.projections.load(Ordering::SeqCst), projections + 1);
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn v2_discovery_failures_retry_or_remain_cached_by_error_kind() -> Result<()> {
    skip_if_no_network!(Ok(()));
    for failure in [DiscoveryFailure::Server, DiscoveryFailure::Disconnect] {
        let executor = Executor::start(/*supports_v2*/ true, failure).await?;
        let manager = codex_exec_server::EnvironmentManager::create_for_tests(
            Some(executor.url.clone()),
            /*local_runtime_paths*/ None,
        )
        .await;
        let environment = manager
            .default_environment()
            .context("remote environment")?;
        let cache = codex_exec_server::ExecutorCapabilityDiscoveryV2Cache::default();
        let request = codex_exec_server::DiscoverV2CapabilitiesRequest {
            cwd: codex_utils_path_uri::PathUri::parse("file:///repo")?,
            sandbox: None,
        };
        assert!(
            cache
                .discover("executor", &environment, request.clone())
                .await
                .is_err()
        );
        assert!(!cache.take_recovered_discovery());
        let (first, concurrent) = tokio::join!(
            cache.discover("executor", &environment, request.clone()),
            cache.discover("executor", &environment, request.clone()),
        );
        let expected_calls = match failure {
            DiscoveryFailure::Server => {
                assert!(first.is_err());
                assert!(concurrent.is_err());
                let next_turn = codex_exec_server::ExecutorCapabilityDiscoveryV2Cache::default();
                next_turn
                    .discover("executor", &environment, request.clone())
                    .await
                    .map_err(anyhow::Error::msg)?;
                assert!(!next_turn.take_recovered_discovery());
                assert!(!cache.take_recovered_discovery());
                2
            }
            DiscoveryFailure::Disconnect => {
                let first = first.map_err(anyhow::Error::msg)?;
                let concurrent = concurrent.map_err(anyhow::Error::msg)?;
                assert!(Arc::ptr_eq(&first, &concurrent));
                assert!(cache.take_recovered_discovery());
                assert!(!cache.take_recovered_discovery());
                let reused = cache
                    .discover("executor", &environment, request.clone())
                    .await
                    .map_err(anyhow::Error::msg)?;
                assert!(Arc::ptr_eq(&first, &reused));
                assert!(!cache.take_recovered_discovery());
                // Two disconnected attempts, then one shared successful retry in the same cache.
                3
            }
            DiscoveryFailure::None => unreachable!("the test only exercises failures"),
        };
        assert_eq!(
            executor
                .requests
                .lock()
                .await
                .iter()
                .filter(|request| request["method"] == "capabilities/discoverV2")
                .count(),
            expected_calls,
        );
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn v2_old_executor_retains_v1_selected_roots() -> Result<()> {
    check_acquisition(/*enabled*/ true, "0.163.0").await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn v2_discovery_cache_reuses_matching_requests_and_isolates_execution_contexts() -> Result<()>
{
    skip_if_no_network!(Ok(()));
    let root = tempfile::tempdir()?;
    let executor = Executor::start(/*supports_v2*/ true, DiscoveryFailure::None).await?;
    let manager = codex_exec_server::EnvironmentManager::create_for_tests(
        Some(executor.url.clone()),
        /*local_runtime_paths*/ None,
    )
    .await;
    let environment = manager
        .default_environment()
        .context("remote environment")?;
    let cache = codex_exec_server::ExecutorCapabilityDiscoveryV2Cache::default();
    let request = codex_exec_server::DiscoverV2CapabilitiesRequest {
        cwd: codex_utils_path_uri::PathUri::from_host_native_path(root.path())?,
        sandbox: None,
    };
    let calls = || async {
        executor
            .requests
            .lock()
            .await
            .iter()
            .filter(|request| request["method"] == "capabilities/discoverV2")
            .count()
    };
    assert_eq!(
        calls().await,
        0,
        "creating a cache must not request discovery"
    );
    let (first, concurrent) = tokio::join!(
        cache.discover("executor", &environment, request.clone()),
        cache.discover("executor", &environment, request.clone()),
    );
    let first = first.map_err(anyhow::Error::msg)?;
    assert!(Arc::ptr_eq(
        &first,
        &concurrent.map_err(anyhow::Error::msg)?
    ));
    assert_eq!(calls().await, 1);
    let restricted = codex_exec_server::DiscoverV2CapabilitiesRequest {
        sandbox: Some(
            codex_exec_server::FileSystemSandboxContext::from_legacy_sandbox_policy(
                codex_protocol::protocol::SandboxPolicy::new_read_only_policy(),
                request.cwd.clone(),
            )?,
        ),
        ..request.clone()
    };
    cache
        .discover("executor", &environment, restricted)
        .await
        .map_err(anyhow::Error::msg)?;
    let original = cache
        .discover("executor", &environment, request.clone())
        .await
        .map_err(anyhow::Error::msg)?;
    assert!(Arc::ptr_eq(&first, &original));
    assert_eq!(calls().await, 2);
    cache
        .discover(
            "executor",
            &environment,
            codex_exec_server::DiscoverV2CapabilitiesRequest {
                cwd: request.cwd.join("other")?,
                ..request.clone()
            },
        )
        .await
        .map_err(anyhow::Error::msg)?;
    assert_eq!(
        calls().await,
        3,
        "another cwd must not reuse the same inventory"
    );
    let other = cache
        .discover("other-executor", &environment, request.clone())
        .await
        .map_err(anyhow::Error::msg)?;
    assert!(Arc::ptr_eq(&first, &other));
    assert_eq!(calls().await, 3);
    let replacement_manager = codex_exec_server::EnvironmentManager::create_for_tests(
        Some(executor.url.clone()),
        /*local_runtime_paths*/ None,
    )
    .await;
    let replacement = replacement_manager
        .default_environment()
        .context("replacement environment")?;
    cache
        .discover("executor", &replacement, request.clone())
        .await
        .map_err(anyhow::Error::msg)?;
    assert_eq!(calls().await, 4);
    let other_reused = cache
        .discover("other-executor", &environment, request.clone())
        .await
        .map_err(anyhow::Error::msg)?;
    assert!(Arc::ptr_eq(&other, &other_reused));
    assert_eq!(
        calls().await,
        4,
        "a new environment instance must not invalidate the original instance"
    );

    // Four entries remain; reserve enough capacity for a selection larger than 16.
    cache.ensure_capacity(/*minimum*/ 17);
    for index in 0..13 {
        cache
            .discover(
                "executor",
                &environment,
                codex_exec_server::DiscoverV2CapabilitiesRequest {
                    cwd: request.cwd.join(&format!("context-{index}"))?,
                    ..request.clone()
                },
            )
            .await
            .map_err(anyhow::Error::msg)?;
    }
    let oldest = cache
        .discover("other-executor", &environment, request.clone())
        .await
        .map_err(anyhow::Error::msg)?;
    assert!(Arc::ptr_eq(&other, &oldest));
    assert_eq!(calls().await, 17);

    cache
        .discover(
            "executor",
            &environment,
            codex_exec_server::DiscoverV2CapabilitiesRequest {
                cwd: request.cwd.join("overflow")?,
                ..request.clone()
            },
        )
        .await
        .map_err(anyhow::Error::msg)?;
    // Reading the oldest entry did not promote it: eviction is FIFO, not LRU.
    let reloaded = cache
        .discover("other-executor", &environment, request)
        .await
        .map_err(anyhow::Error::msg)?;
    assert_eq!(calls().await, 19);
    assert!(!Arc::ptr_eq(&other, &reloaded));
    assert_eq!(other.as_ref(), reloaded.as_ref());

    Ok(())
}

struct Cloud {
    available: AtomicBool,
    ready: mpsc::UnboundedSender<(Arc<McpResourceClient>, oneshot::Sender<()>)>,
}

impl PluginProvider for Cloud {}

impl ToolLifecycleContributor for Cloud {
    fn on_tool_finish<'a>(&'a self, input: ToolFinishInput<'a>) -> ToolLifecycleFuture<'a> {
        Box::pin(async move {
            let (resume, wait) = oneshot::channel();
            let client = input
                .session_store
                .get::<McpResourceClient>()
                .expect("MCP resource client should be installed");
            self.ready.send((client, resume)).expect("step observer");
            wait.await.expect("resume after auth rotation");
        })
    }
}

#[test_case::test_case("Stop"; "stop")]
#[test_case::test_case("Interrupt"; "interrupt")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn v2_cleanup_hooks_follow_plugin_selection_without_legacy_roots(
    hook_event: &'static str,
) -> Result<()> {
    skip_if_no_network!(Ok(()));
    let model = responses::start_mock_server().await;
    let hook_calls = Arc::new(AtomicUsize::new(0));
    let recorded = Arc::clone(&hook_calls);
    wiremock::Mock::given(wiremock::matchers::path("/hooks"))
        .respond_with(move |request: &wiremock::Request| {
            let request: Value =
                serde_json::from_slice(&request.body).expect("MCP request should be valid JSON");
            let result = match request["method"].as_str() {
                Some("initialize") => json!({
                    "protocolVersion": request["params"]["protocolVersion"],
                    "capabilities": {"tools": {}}, "serverInfo": {"name": "hooks", "version": "1"}
                }),
                Some("notifications/initialized") => return wiremock::ResponseTemplate::new(202),
                Some("tools/list") => json!({"tools": [{
                    "name": "turn_ended", "inputSchema": {"type": "object"}
                }]}),
                Some("tools/call") => {
                    assert_eq!(request["params"]["name"], "turn_ended");
                    assert_eq!(
                        request["params"]["arguments"]["hook_event_name"],
                        hook_event
                    );
                    recorded.fetch_add(1, Ordering::SeqCst);
                    json!({"content": []})
                }
                other => panic!("unexpected MCP request: {other:?}"),
            };
            wiremock::ResponseTemplate::new(200)
                .set_body_json(json!({"jsonrpc": "2.0", "id": request["id"], "result": result}))
        })
        .mount(&model)
        .await;
    let manifest = json!({"name": "computer-use", "hooks": {"hooks": {(hook_event): [{"hooks": [{
        "type": "mcp_tool", "server": "node_repl", "tool": "turn_ended",
        "input": {"hook_event_name": "${hook_event_name}", "session_id": "${session_id}", "turn_id": "${turn_id}"}
    }]}]}}});
    let inventory = json!({"plugins": [{
        "id": "computer-use@openai-bundled", "displayName": "Computer use",
        "remotePluginId": null, "version": "1", "root": "file:///plugins/computer-use",
        "files": {"manifest": {"path": "file:///plugins/computer-use/.codex-plugin/plugin.json",
            "contents": manifest.to_string()}, "mcpConfig": null, "appsConfig": null}, "skills": []
    }], "skills": [], "warnings": []});
    let executor = Executor::start(/*supports_v2*/ true, DiscoveryFailure::None).await?;
    *executor.inventory.lock().await = Some(inventory);
    let hook_url = format!("{}/hooks", model.uri());
    let hook_server = serde_json::from_value(json!({
        "url": hook_url, "environment_id": codex_exec_server::REMOTE_ENVIRONMENT_ID
    }))?;
    let mut extensions = ExtensionRegistryBuilder::new();
    codex_mcp_extension::install_plugins(
        &mut extensions,
        Arc::new(codex_exec_server::EnvironmentManager::default_for_tests()),
    );
    let home = Arc::new(tempfile::TempDir::new()?);
    std::fs::write(
        home.path().join("config.toml"),
        "[plugins.\"computer-use@openai-bundled\"]\nenabled = true\n",
    )?;
    let test = test_codex()
        .with_home(home)
        .with_extensions(Arc::new(extensions.build()))
        .with_exec_server_url(executor.url.clone())
        .with_config(move |config| {
            config
                .features
                .enable(Feature::Plugins)
                .expect("enable plugins");
            config
                .features
                .enable(Feature::ExecutorCapabilityDiscoveryV2)
                .expect("enable V2 discovery");
            config
                .features
                .disable(Feature::ExecutorCapabilityDiscovery)
                .expect("disable V1 discovery");
            config
                .mcp_servers
                .set(
                    [(String::from("node_repl"), hook_server)]
                        .into_iter()
                        .collect(),
                )
                .expect("configure the executor hook server");
        })
        .build_with_auto_env(&model)
        .await?;
    core_test_support::wait_for_mcp_server(&test.codex, "node_repl").await?;
    let mut selection = test.executor_environment().selection().clone();
    selection.environment_id = codex_exec_server::REMOTE_ENVIRONMENT_ID.to_string();
    selection.selected_capability_roots = Default::default();
    for disabled in [false, true] {
        core_test_support::submit_thread_settings(
            &test.codex,
            codex_protocol::protocol::ThreadSettingsOverrides {
                environments: Some(
                    TurnEnvironmentSelections::new(
                        test.config.cwd.clone(),
                        vec![selection.clone()],
                    )
                    .into_requests(),
                ),
                disabled_plugin_ids: Some(if disabled {
                    vec!["computer-use@openai-bundled".to_string()]
                } else {
                    Vec::new()
                }),
                ..Default::default()
            },
        )
        .await?;
        let mut response = responses::sse_response(responses::sse(vec![
            responses::ev_response_created("response"),
            responses::ev_assistant_message("message", "Done."),
            responses::ev_completed("response"),
        ]));
        if hook_event == "Interrupt" {
            response = response.set_delay(std::time::Duration::from_secs(60));
        }
        let requests = responses::mount_response_once(&model, response).await;
        test.codex
            .start_or_steer_turn(TurnInputRequest::user_input(vec![UserInput::Text {
                text: "Finish this turn.".to_string(),
                text_elements: Vec::new(),
            }]))
            .await?;
        if hook_event == "Interrupt" {
            tokio::time::timeout(std::time::Duration::from_secs(10), async {
                while requests.requests().is_empty() {
                    tokio::task::yield_now().await;
                }
            })
            .await
            .context("interrupted turn should reach inference with its prepared step")?;
            test.codex.submit(Op::Interrupt).await?;
        }
        core_test_support::wait_for_event(&test.codex, |event| {
            if hook_event == "Interrupt" {
                matches!(event, EventMsg::TurnAborted(_))
            } else {
                matches!(event, EventMsg::TurnComplete(_))
            }
        })
        .await;
        if !disabled {
            tokio::time::timeout(std::time::Duration::from_secs(10), async {
                while hook_calls.load(Ordering::SeqCst) == 0 {
                    tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                }
            })
            .await
            .context("V2 cleanup hook should run")?;
        } else {
            // Cleanup is asynchronous; turn completion alone cannot prove disablement.
            assert!(
                tokio::time::timeout(std::time::Duration::from_secs(1), async {
                    while hook_calls.load(Ordering::SeqCst) == 1 {
                        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                    }
                })
                .await
                .is_err(),
                "disabled cleanup hook must remain silent"
            );
        }
        assert_eq!(hook_calls.load(Ordering::SeqCst), 1);
    }
    let requests = executor.requests.lock().await;
    assert!(
        requests
            .iter()
            .any(|request| request["method"] == "capabilities/discoverV2")
    );
    assert!(
        !requests
            .iter()
            .any(|request| request["method"] == "capabilityRoots/discoverV1")
    );
    Ok(())
}
