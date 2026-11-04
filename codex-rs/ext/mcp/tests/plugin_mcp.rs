use anyhow::Context;
use codex_config::AppToolApproval;
use codex_config::McpServerToolConfig;
use codex_core::EnvironmentConfig;
use codex_core::config::Config;
use codex_core::config::ConfigBuilder;
use codex_core::windows_sandbox::WindowsSandboxLevelExt;
use codex_core_plugins::ExecutorPluginProvider;
use codex_core_plugins::PluginCatalogEntry;
use codex_core_plugins::PluginIdentity;
use codex_core_plugins::PluginSourceLocation;
use codex_exec_server::CapabilityRootDiscovery;
use codex_exec_server::EnvironmentManager;
use codex_exec_server::ExecutorCapabilityDiscoveryCache;
use codex_exec_server::ExecutorCapabilityDiscoverySnapshot;
use codex_exec_server::FileSystemSandboxContext;
use codex_exec_server::LOCAL_ENVIRONMENT_ID;
use codex_extension_api::ExtensionData;
use codex_extension_api::ExtensionDataInit;
use codex_extension_api::ExtensionRegistryBuilder;
use codex_extension_api::McpServerContributionContext;
use codex_extension_api::SelectedPluginContribution;
use codex_features::Feature;
use codex_login::CodexAuth;
use codex_mcp_extension::PluginProviders;
use codex_mcp_extension::PluginsThreadState;
use codex_mcp_extension::install_plugin_providers;
use codex_protocol::capabilities::CapabilityRootLocation;
use codex_protocol::capabilities::SelectedCapabilityRoot;
use codex_protocol::config_types::WindowsSandboxLevel;
use codex_protocol::models::PermissionProfileSnapshot;
use codex_utils_path_uri::PathUri;
use core_test_support::apps_test_server::AppsTestServer;
use core_test_support::apps_test_server::recorded_apps_tool_calls;
use core_test_support::responses::ev_assistant_message;
use core_test_support::responses::ev_completed;
use core_test_support::responses::ev_function_call_with_namespace;
use core_test_support::responses::ev_response_created;
use core_test_support::responses::mount_sse_sequence;
use core_test_support::responses::sse;
use core_test_support::responses::start_mock_server;
use core_test_support::skip_if_no_network;
use core_test_support::test_codex::run_test_with_large_stack;
use core_test_support::test_codex::test_codex;
use pretty_assertions::assert_eq;
use serde_json::json;
use std::collections::HashMap;
use std::fs;
use std::sync::Arc;

type TestResult = Result<(), Box<dyn std::error::Error>>;

#[derive(Debug, PartialEq, Eq)]
struct ContributionSummary {
    name: String,
    plugin_id: String,
    plugin_display_name: String,
    source_environment_id: String,
    selection_order: usize,
    enabled: bool,
}

#[derive(Debug, PartialEq, Eq)]
struct PackageSummary {
    plugin_id: String,
    plugin_display_name: String,
    source_environment_id: String,
    connector_ids: Vec<String>,
}

#[tokio::test]
async fn selected_plugin_package_is_contributed_without_servers_or_connectors() -> TestResult {
    let codex_home = tempfile::tempdir()?;
    let plugin_root = tempfile::tempdir()?;
    std::fs::create_dir_all(plugin_root.path().join(".codex-plugin"))?;
    std::fs::create_dir_all(plugin_root.path().join("skills/deploy"))?;
    std::fs::write(
        plugin_root.path().join(".codex-plugin/plugin.json"),
        r#"{"name":"skill-only","interface":{"displayName":"Skill Only"}}"#,
    )?;
    std::fs::write(
        plugin_root.path().join("skills/deploy/SKILL.md"),
        "---\nname: deploy\ndescription: Deploy the project.\n---\n",
    )?;
    let config = ConfigBuilder::default()
        .codex_home(codex_home.path().to_path_buf())
        .fallback_cwd(Some(codex_home.path().to_path_buf()))
        .build()
        .await?;

    let contributions = raw_selected_plugin_contributions(&config, plugin_root.path()).await?;
    let package = contributions
        .into_iter()
        .next()
        .map(|(_, plugin_id, contribution)| PackageSummary {
            plugin_id,
            plugin_display_name: contribution.plugin_display_name,
            source_environment_id: contribution.source_environment_id,
            connector_ids: contribution.connector_ids,
        });

    assert_eq!(
        package,
        Some(PackageSummary {
            plugin_id: "selected-root".to_string(),
            plugin_display_name: "Skill Only".to_string(),
            source_environment_id: LOCAL_ENVIRONMENT_ID.to_string(),
            connector_ids: Vec::new(),
        })
    );
    Ok(())
}

#[tokio::test]
async fn denied_malformed_plugin_retains_identity_without_capabilities() -> TestResult {
    let default_off = "[plugins._default]\nenabled = false\n";
    for (manifest, discovery_enabled, expected_denied) in [
        (Some(b"{".as_slice()), false, true),
        (None, false, false),
        (None, true, false),
        (
            Some(br#"{"name":"denied","mcpServers":{"unexpected":{"command":"unexpected-command"}}}"#.as_slice()),
            true,
            true,
        ),
        (Some(b"{".as_slice()), true, true),
        (Some(b"\xff".as_slice()), true, true),
    ] {
        let codex_home = tempfile::tempdir()?;
        let plugin_root = tempfile::tempdir()?;
        let skills_root = plugin_root.path().join("skills");
        fs::create_dir_all(&skills_root)?;
        if let Some(manifest) = manifest {
            fs::create_dir_all(plugin_root.path().join(".codex-plugin"))?;
            fs::write(plugin_root.path().join(".codex-plugin/plugin.json"), manifest)?;
        } else {
            // A nested package does not own its containing skill root.
            fs::create_dir_all(plugin_root.path().join("nested/.codex-plugin"))?;
            fs::write(
                plugin_root.path().join("nested/.codex-plugin/plugin.json"),
                r#"{"name":"nested"}"#,
            )?;
        }
        fs::write(codex_home.path().join("config.toml"), default_off)?;
        let mut config = ConfigBuilder::default()
            .codex_home(codex_home.path().to_path_buf())
            .fallback_cwd(Some(codex_home.path().to_path_buf()))
            .build()
            .await?;
        config.features.set_enabled(Feature::ExecutorCapabilityDiscovery, discovery_enabled)?;

        let expected = if expected_denied {
            vec![(
                ("selected-root".to_string(), "selected-root".to_string()),
                Vec::new(),
                Vec::new(),
            )]
        } else {
            Vec::new()
        };
        for selected_root in [plugin_root.path(), skills_root.as_path()] {
            let contributions = raw_selected_plugin_contributions(&config, selected_root).await?;
            assert_eq!(
                contributions
                    .into_iter()
                    .map(|(root_id, plugin_id, contribution)| (
                        (root_id, plugin_id),
                        contribution.servers,
                        contribution.connector_ids,
                    ))
                    .collect::<Vec<_>>(),
                expected
            );
        }
    }
    Ok(())
}

#[tokio::test]
async fn selected_root_ownership_retries_errors_and_caches_success() -> TestResult {
    let codex_home = tempfile::tempdir()?;
    let plugin_root = tempfile::tempdir()?;
    let standalone_root = tempfile::tempdir()?;
    let skills_root = plugin_root.path().join("skills");
    fs::create_dir_all(&skills_root)?;
    fs::create_dir_all(plugin_root.path().join(".codex-plugin"))?;
    let manifest = plugin_root.path().join(".codex-plugin/plugin.json");
    fs::write(&manifest, r#"{"name":"example"}"#)?;
    fs::write(plugin_root.path().join(".mcp.json"), "{")?;
    let selected_roots = [
        ("selected-root", plugin_root.path()),
        ("owned-subfolder", skills_root.as_path()),
        ("standalone", standalone_root.path()),
    ]
    .into_iter()
    .map(|(id, path)| {
        Ok(SelectedCapabilityRoot {
            id: id.to_string(),
            location: CapabilityRootLocation::Environment {
                environment_id: LOCAL_ENVIRONMENT_ID.to_string(),
                path: PathUri::from_host_native_path(path)?,
            },
        })
    })
    .collect::<anyhow::Result<Vec<_>>>()?;
    let mut builder = ExtensionRegistryBuilder::new();
    codex_mcp_extension::install_plugins(
        &mut builder,
        Arc::new(EnvironmentManager::default_for_tests()),
    );
    let registry = builder.build();
    let thread_init = ExtensionDataInit::new();
    let thread_store = ExtensionData::new_with_init("test-thread", thread_init.clone());
    let default_off = "[plugins._default]\nenabled = false\n";
    let standalone = &selected_roots[2];
    let CapabilityRootLocation::Environment { path, .. } = &standalone.location;
    let sandbox_contexts = HashMap::from([(
        LOCAL_ENVIRONMENT_ID.to_string(),
        FileSystemSandboxContext::from_permission_profile(Default::default(), path.clone()),
    )]);

    for (phase, enabled_root, expected) in [
        (0, None, vec![("standalone", vec![])]),
        (1, None, vec![]),
        (2, Some("owned-subfolder"), vec![("selected-root", vec![])]),
        (
            3,
            None,
            vec![("selected-root", vec![]), ("owned-subfolder", vec![])],
        ),
        (
            4,
            None,
            vec![("selected-root", vec![]), ("owned-subfolder", vec![])],
        ),
        (
            5,
            Some("selected-root"),
            vec![
                ("selected-root", vec!["probe"]),
                ("owned-subfolder", vec![]),
            ],
        ),
    ] {
        if phase == 4 {
            // Stable ownership must survive refresh without probing these files again.
            fs::remove_file(&manifest)?;
            fs::create_dir_all(standalone_root.path().join(".codex-plugin"))?;
            fs::write(
                standalone_root.path().join(".codex-plugin/plugin.json"),
                "{",
            )?;
        } else if phase == 5 {
            fs::write(&manifest, r#"{"name":"example"}"#)?;
            fs::write(
                plugin_root.path().join(".mcp.json"),
                r#"{"mcpServers":{"probe":{"command":"probe-command"}}}"#,
            )?;
            fs::remove_file(standalone_root.path().join(".codex-plugin/plugin.json"))?;
        }
        let policy = enabled_root.map_or_else(
            || default_off.to_string(),
            |id| format!("{default_off}[plugins.{id}]\nenabled = true\n"),
        );
        fs::write(codex_home.path().join("config.toml"), policy)?;
        let config = ConfigBuilder::default()
            .codex_home(codex_home.path().to_path_buf())
            .fallback_cwd(Some(codex_home.path().to_path_buf()))
            .build()
            .await?;
        // A warning requires metadata probing, which the test filesystem cannot sandbox.
        // Complete discovery retries ownership under the same sandbox, root, and policy.
        let discovery = (phase < 2).then(|| {
            ExecutorCapabilityDiscoverySnapshot::new(
                std::slice::from_ref(standalone),
                vec![Ok(Arc::new(CapabilityRootDiscovery {
                    id: standalone.id.clone(),
                    path: path.clone(),
                    plugin: None,
                    skills: Vec::new(),
                    namespace_manifests: Vec::new(),
                    warnings: if phase == 0 {
                        vec!["ownership metadata unavailable".to_string()]
                    } else {
                        Vec::new()
                    },
                    error: None,
                }))],
                sandbox_contexts.clone(),
            )
        });
        let selected = registry.mcp_server_contributors()[0]
            .selected_plugins(
                McpServerContributionContext::for_step(
                    &config,
                    &thread_init,
                    &thread_store,
                    "test_originator",
                    &selected_roots,
                    discovery.as_ref(),
                    /*executor_capability_discovery_v2*/ None,
                ),
                &config.plugins,
            )
            .await;
        let mut actual = Vec::new();
        for plugin in selected {
            let contribution = plugin.mcp.await;
            actual.push((
                plugin.selected_root_id,
                contribution
                    .servers
                    .into_iter()
                    .map(|(name, _)| name)
                    .collect::<Vec<_>>(),
                contribution.connector_ids,
            ));
        }
        assert_eq!(
            actual,
            expected
                .into_iter()
                .map(|(id, servers)| (
                    id.to_string(),
                    servers.into_iter().map(str::to_string).collect::<Vec<_>>(),
                    Vec::new(),
                ))
                .collect::<Vec<_>>(),
            "phase {phase}",
        );
    }
    Ok(())
}
#[tokio::test]
async fn high_level_discovery_matches_the_existing_plugin_provider() -> TestResult {
    let codex_home = tempfile::tempdir()?;
    let plugin_root = tempfile::tempdir()?;
    std::fs::create_dir_all(plugin_root.path().join(".codex-plugin"))?;
    std::fs::write(
        plugin_root.path().join(".codex-plugin/plugin.json"),
        r#"{"name":"demo","interface":{"displayName":"Demo"},"mcpServers":"./servers.json"}"#,
    )?;
    std::fs::write(
        plugin_root.path().join("servers.json"),
        r#"{
  "mcpServers": {
    "first": {
      "command": "first",
      "default_tools_approval_mode": "writes",
      "enabled_tools": ["read", "deploy", "trusted", "package-only"],
      "disabled_tools": ["package-denied"],
      "tools": {
        "read": {"approval_mode": "prompt", "output_token_limit": 12000},
        "deploy": {"approval_mode": "approve", "output_token_limit": 4000},
        "trusted": {"approval_mode": "approve"}
      }
    },
    "second": {
      "command": "second",
      "enabled": false,
      "default_tools_approval_mode": "prompt"
    }
  }
}"#,
    )?;
    std::fs::write(
        codex_home.path().join("config.toml"),
        r#"
[plugins."selected-root".mcp_servers.first]
enabled = false
default_tools_approval_mode = "prompt"
enabled_tools = ["read", "deploy", "trusted", "host-only"]
disabled_tools = ["write"]

[plugins."selected-root".mcp_servers.first.tools.read]
approval_mode = "approve"
output_token_limit = 8000

[plugins."selected-root".mcp_servers.first.tools.deploy]
output_token_limit = 9000

[plugins."selected-root".mcp_servers.first.tools.trusted]
approval_mode = "approve"

[plugins."selected-root".mcp_servers.second]
enabled = true
default_tools_approval_mode = "auto"
"#,
    )?;
    let mut config = ConfigBuilder::default()
        .codex_home(codex_home.path().to_path_buf())
        .fallback_cwd(Some(codex_home.path().to_path_buf()))
        .build()
        .await?;
    let existing = selected_plugin_contributions(&config, plugin_root.path()).await?;
    let mut servers = raw_selected_plugin_contributions(&config, plugin_root.path())
        .await?
        .into_iter()
        .flat_map(|(_, _, contribution)| contribution.servers)
        .collect::<HashMap<_, _>>();
    let server = servers
        .remove("first")
        .expect("disabled selected server remains registered");
    let declared_disabled_server = servers
        .remove("second")
        .expect("package-disabled server remains registered");
    assert_eq!(
        (
            server.enabled,
            server.default_tools_approval_mode,
            server.enabled_tools,
            server.disabled_tools,
            server.tools,
            declared_disabled_server.enabled,
            declared_disabled_server.default_tools_approval_mode,
        ),
        (
            false,
            Some(AppToolApproval::Prompt),
            Some(vec![
                "read".to_string(),
                "deploy".to_string(),
                "trusted".to_string(),
            ]),
            Some(vec!["package-denied".to_string(), "write".to_string()]),
            HashMap::from([
                (
                    "read".to_string(),
                    McpServerToolConfig {
                        approval_mode: Some(AppToolApproval::Prompt),
                        output_token_limit: std::num::NonZeroUsize::new(8_000),
                    },
                ),
                (
                    "deploy".to_string(),
                    McpServerToolConfig {
                        approval_mode: Some(AppToolApproval::Prompt),
                        output_token_limit: std::num::NonZeroUsize::new(4_000),
                    },
                ),
                (
                    "trusted".to_string(),
                    McpServerToolConfig {
                        approval_mode: Some(AppToolApproval::Approve),
                        ..Default::default()
                    },
                ),
            ]),
            false,
            Some(AppToolApproval::Prompt),
        )
    );
    config
        .features
        .enable(Feature::ExecutorCapabilityDiscovery)
        .expect("test config should allow feature update");
    let high_level = selected_plugin_contributions(&config, plugin_root.path()).await?;

    assert_eq!(
        existing
            .iter()
            .map(|contribution| contribution.source_environment_id.as_str())
            .collect::<Vec<_>>(),
        vec![LOCAL_ENVIRONMENT_ID, LOCAL_ENVIRONMENT_ID]
    );
    assert_eq!(high_level, existing);
    Ok(())
}

async fn selected_plugin_contributions(
    config: &Config,
    plugin_root: &std::path::Path,
) -> Result<Vec<ContributionSummary>, Box<dyn std::error::Error>> {
    Ok(raw_selected_plugin_contributions(config, plugin_root)
        .await?
        .into_iter()
        .enumerate()
        .flat_map(|(selection_order, (_, plugin_id, contribution))| {
            contribution
                .servers
                .into_iter()
                .map(move |(name, config)| ContributionSummary {
                    name,
                    plugin_id: plugin_id.clone(),
                    plugin_display_name: contribution.plugin_display_name.clone(),
                    source_environment_id: contribution.source_environment_id.clone(),
                    selection_order,
                    enabled: config.enabled,
                })
        })
        .collect())
}

async fn raw_selected_plugin_contributions(
    config: &Config,
    plugin_root: &std::path::Path,
) -> Result<Vec<(String, String, SelectedPluginContribution)>, Box<dyn std::error::Error>> {
    let mut builder = ExtensionRegistryBuilder::new();
    let environment_manager = Arc::new(EnvironmentManager::default_for_tests());
    codex_mcp_extension::install_plugins(&mut builder, Arc::clone(&environment_manager));
    let registry = builder.build();
    let thread_init = ExtensionDataInit::new();
    let selected_capability_roots = vec![SelectedCapabilityRoot {
        id: "selected-root".to_string(),
        location: CapabilityRootLocation::Environment {
            environment_id: LOCAL_ENVIRONMENT_ID.to_string(),
            path: PathUri::from_host_native_path(plugin_root)?,
        },
    }];
    let thread_store = ExtensionData::new_with_init("test-thread", thread_init.clone());
    let executor_capability_discovery = if config
        .features
        .enabled(Feature::ExecutorCapabilityDiscovery)
    {
        let resolved_roots = environment_manager
            .resolve_selected_capability_roots(
                &selected_capability_roots,
                &HashMap::from([(
                    LOCAL_ENVIRONMENT_ID.to_string(),
                    environment_manager.get_environment(LOCAL_ENVIRONMENT_ID),
                )]),
            )
            .await;
        Some(
            ExecutorCapabilityDiscoveryCache::default()
                .snapshot(&resolved_roots, &Default::default())
                .await,
        )
    } else {
        None
    };

    let selected = registry.mcp_server_contributors()[0]
        .selected_plugins(
            McpServerContributionContext::for_step(
                config,
                &thread_init,
                &thread_store,
                "test_originator",
                &selected_capability_roots,
                executor_capability_discovery.as_ref(),
                /*executor_capability_discovery_v2*/ None,
            ),
            &config.plugins,
        )
        .await;
    let mut contributions = Vec::new();
    for plugin in selected {
        contributions.push((plugin.selected_root_id, plugin.plugin_id, plugin.mcp.await));
    }
    Ok(contributions)
}
