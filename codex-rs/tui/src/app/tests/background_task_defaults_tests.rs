//! Request-level coverage for command-center session defaults and worktree creation.

use super::*;
use crate::app::agents_overview::AGENTS_OVERVIEW_VIEW_ID;
use crate::app::tests::session_lifecycle_requests::HistoryCapabilities;
use crate::app::tests::session_lifecycle_requests::recorded_params;
use crate::app::tests::session_lifecycle_requests::start_recording_app_server_with_history;
use crate::model_catalog::ModelCatalog;
use crate::test_support::PathBufExt;
use crate::tui::test_support::make_test_tui;
use codex_state::SqliteConfig;
use pretty_assertions::assert_eq;

async fn confirm_permission_selection(
    app: &mut App,
    server: &mut AppServerSession,
    thread_id: ThreadId,
) -> Result<()> {
    for _ in 0..20 {
        let settings = next_thread_settings_updated(server, thread_id).await;
        app.enqueue_thread_notification(
            thread_id,
            ServerNotification::ThreadSettingsUpdated(settings),
        )
        .await?;
        if !app.pending_server_profiles.contains_key(&thread_id)
            && !app
                .agents_overview
                .requested_permission_profiles
                .contains_key(&thread_id)
        {
            return Ok(());
        }
    }
    color_eyre::eyre::bail!("permission update was not confirmed");
}

fn trust_launch_folder(app: &mut App) {
    for cwd in [
        app.config.cwd.to_path_buf(),
        app.chat_widget.config_ref().cwd.to_path_buf(),
    ] {
        crate::legacy_core::config::set_project_trust_level(
            &app.config.codex_home,
            cwd.as_path(),
            codex_protocol::config_types::TrustLevel::Trusted,
        )
        .expect("persist trusted fixture folder");
    }
    let projects = serde_json::json!({
        app.config.cwd.display().to_string(): {"trust_level": "trusted"},
        app.chat_widget.config_ref().cwd.display().to_string(): {"trust_level": "trusted"},
    });
    app.cli_kv_overrides.push((
        "projects".into(),
        TomlValue::try_from(projects).expect("trust fixture"),
    ));
    app.config.active_project.trust_level = Some(codex_protocol::config_types::TrustLevel::Trusted);
}

#[tokio::test]
async fn command_center_new_keeps_startup_draft_visible_through_handoff() -> Result<()> {
    use crate::custom_terminal::test_support::last_rendered_buffer;

    let mut snapshots = Vec::new();
    for owned in [false, true] {
        let (mut app, mut events, _) = make_test_app_with_channels().await;
        trust_launch_folder(&mut app);
        app.cli_kv_overrides
            .push(("tui.animations".into(), TomlValue::Boolean(false)));
        let mut server = start_config_write_test_app_server(&app).await?;
        let mut tui = make_test_tui()?;
        tui.pause_events();
        tui.set_owned_screen(owned)?;
        app.agents_overview.rendered_full_screen = true;
        app.agents_overview.new_session_draft = Some(Box::new(
            crate::startup_draft::tests::startup_test_pump_with_input("draft during startup"),
        ));

        app.new_agents_overview_session(&mut tui, &mut server, /*cwd*/ None)
            .await?;
        let frame = last_rendered_buffer(&tui.terminal).clone();
        let screen_text: String = frame
            .content
            .iter()
            .map(ratatui::buffer::Cell::symbol)
            .collect();
        assert!(screen_text.contains("draft during startup"));
        assert!(!screen_text.contains("Loading task"));
        assert_eq!(
            app.chat_widget.composer_text_with_pending(),
            "draft during startup"
        );

        while let Ok(event) = events.try_recv() {
            Box::pin(app.handle_event(&mut tui, &mut server, event)).await?;
            assert_eq!(last_rendered_buffer(&tui.terminal), &frame);
        }

        let size = tui.terminal.last_known_screen_size;
        app.render_chat_widget_frame(&mut tui, size)?;
        let final_frame = last_rendered_buffer(&tui.terminal);
        let footer = |buffer: &ratatui::buffer::Buffer| {
            buffer
                .content
                .chunks(usize::from(buffer.area.width))
                .rev()
                .take(5)
                .collect::<Vec<_>>()
                .into_iter()
                .rev()
                .map(|row| {
                    row.iter()
                        .map(ratatui::buffer::Cell::symbol)
                        .collect::<String>()
                        .trim_end()
                        .to_string()
                })
                .collect::<Vec<_>>()
                .join("\n")
        };
        snapshots.push(format!(
            "owned={owned}\nstartup:\n{}\nattached:\n{}",
            footer(&frame),
            footer(final_frame)
        ));
        tui.set_owned_screen(/*owned*/ false)?;
        server.shutdown().await?;
    }
    insta::assert_snapshot!(
        "command_center_new_draft_handoff",
        crate::chatwidget::tests::helpers::normalize_snapshot_paths(snapshots.join("\n\n"))
    );
    Ok(())
}

#[tokio::test]
async fn review_regression_agents_overview_creation_is_fresh_but_returning_is_not() -> Result<()> {
    let render = |chat: &ChatWidget| {
        crate::terminal_palette::with_test_default_colors(
            crate::terminal_probe::DefaultColors {
                fg: (230, 216, 255),
                bg: (36, 27, 53),
            },
            || render_bottom_popup(chat, /*width*/ 80),
        )
    };
    let has_stars = |text: &str| text.chars().any(|ch| "⠁⠂⠄⠈⠐⠠⡀⢀".contains(ch));
    for model in ["gpt-6-astra", "gpt-5.5"] {
        let (mut app, _events, _ops) = make_test_app_with_channels().await;
        trust_launch_folder(&mut app);
        app.cli_kv_overrides.extend([
            ("tui.animations".into(), TomlValue::Boolean(true)),
            ("tui.effects.starfield".into(), TomlValue::Boolean(true)),
        ]);
        app.harness_overrides.model = Some(model.into());
        let mut server = start_config_write_test_app_server(&app).await?;
        let mut tui = make_test_tui()?;
        tui.pause_events();
        app.new_agents_overview_session(&mut tui, &mut server, /*cwd*/ None)
            .await?;
        let original = app.chat_widget.thread_id().expect("new dashboard task");
        assert_eq!(app.chat_widget.current_model(), model);
        let created = render(&app.chat_widget);
        assert_eq!(has_stars(&created), model == "gpt-6-astra", "{model}");
        if model != "gpt-6-astra" {
            app.chat_widget.set_model("gpt-6-astra");
            app.chat_widget
                .on_sparkle_model_selected_from_picker("gpt-6-astra");
            assert!(has_stars(&render(&app.chat_widget)));
        }
        app.harness_overrides.model = Some("gpt-6-astra".into());
        app.new_agents_overview_session(&mut tui, &mut server, /*cwd*/ None)
            .await?;
        assert_ne!(app.chat_widget.thread_id(), Some(original));
        app.select_agents_overview_thread(&mut tui, &mut server, original)
            .await?;
        assert_eq!(app.chat_widget.thread_id(), Some(original));
        let returned = render(&app.chat_widget);
        assert!(!has_stars(&returned));
        app.chat_widget
            .on_sparkle_model_selected_from_picker("gpt-6-astra");
        assert!(!has_stars(&render(&app.chat_widget)));
        if model == "gpt-6-astra" {
            let before_footer = |output: &str| {
                output
                    .lines()
                    .take_while(|line| !line.contains("GPT-6-Astra"))
                    .collect::<Vec<_>>()
                    .join("\n")
            };
            insta::assert_snapshot!(format!(
                "created from the dashboard:\n{}\nreturned to the existing task:\n{}",
                before_footer(&created),
                before_footer(&returned)
            ));
        }
        server.shutdown().await?;
    }
    Ok(())
}
#[tokio::test]
async fn command_center_new_preserves_explicit_choices_and_managed_defaults() -> Result<()> {
    let mut tui = make_test_tui()?;
    tui.pause_events();
    for (choice, expected_model, expected_effort) in [
        ("saved", "server-model", "high"),
        ("cli_effort", "server-model", "low"),
        ("profile_model", "profile-model", "high"),
        ("profile_managed", "profile-model", "low"),
        ("managed", "managed-model", "medium"),
    ] {
        let client_home = tempdir()?;
        let server_home = tempdir()?;
        std::fs::write(
            client_home.path().join("config.toml"),
            "model = \"client-model\"\nmodel_reasoning_effort = \"low\"\n",
        )?;
        std::fs::write(
            server_home.path().join("config.toml"),
            "model = \"server-model\"\nmodel_reasoning_effort = \"high\"\n",
        )?;
        if choice == "managed" || choice == "profile_managed" || choice.starts_with("cli_") {
            std::fs::write(
                server_home.path().join("requirements.toml"),
                "[models.new_thread]\nmodel = \"managed-model\"\nmodel_reasoning_effort = \"medium\"\n",
            )?;
        }
        let mut app = make_test_app_with_channels().await.0;
        trust_launch_folder(&mut app);
        match choice {
            "cli_effort" => app.cli_kv_overrides.push((
                "model_reasoning_effort".into(),
                TomlValue::String("low".into()),
            )),
            "profile_model" | "profile_managed" => {
                let path = client_home.path().join("work.config.toml");
                std::fs::write(
                    &path,
                    if choice == "profile_managed" {
                        "model = \"profile-model\"\nmodel_reasoning_effort = \"low\"\n"
                    } else {
                        "model = \"profile-model\"\n"
                    },
                )?;
                app.loader_overrides.user_config_path = Some(path.abs());
                app.loader_overrides.user_config_profile = Some("work".parse()?);
            }
            _ => {}
        }
        app.config = ConfigBuilder::default()
            .codex_home(client_home.path().to_path_buf())
            .loader_overrides(app.loader_overrides.clone())
            .cli_overrides(app.cli_kv_overrides.clone())
            .harness_overrides(app.harness_overrides.clone())
            .build()
            .await?;
        trust_launch_folder(&mut app);
        for cwd in [
            app.config.cwd.as_path(),
            app.chat_widget.config_ref().cwd.as_path(),
        ] {
            crate::legacy_core::config::set_project_trust_level(
                server_home.path(),
                cwd,
                codex_protocol::config_types::TrustLevel::Trusted,
            )
            .map_err(std::io::Error::other)?;
        }
        let mut server_config = app.config.clone();
        server_config.codex_home = server_home.path().to_path_buf().abs();
        server_config.sqlite = SqliteConfig::new_for_testing(server_home.path().abs());
        let (mut server, requests, proxy) = start_recording_app_server_with_history(
            &server_config,
            HistoryCapabilities::Current,
            /*blocked_thread_list*/ None,
            /*failed_thread_name*/ None,
            crate::app_server_session::ThreadParamsMode::Embedded,
            LoaderOverrides {
                user_config_path: Some(server_home.path().join("config.toml").abs()),
                system_requirements_path: Some(server_home.path().join("requirements.toml")),
                ..LoaderOverrides::default()
            },
        )
        .await?;
        server.bootstrap(&app.config).await?;
        app.new_agents_overview_session(&mut tui, &mut server, /*cwd*/ None)
            .await?;
        let starts = recorded_params(&requests, "thread/start");
        assert_eq!(starts.len(), 1, "{choice}");
        assert_eq!(
            (
                &starts[0]["model"],
                &starts[0]["config"]["model_reasoning_effort"]
            ),
            (
                &serde_json::json!(expected_model),
                &serde_json::json!(expected_effort)
            ),
            "{choice}"
        );
        assert_eq!(
            recorded_params(&requests, "turn/start").len(),
            0,
            "{choice}"
        );
        server.shutdown().await?;
        proxy.await??;
    }
    Ok(())
}

#[tokio::test]
async fn command_center_new_read_failure_keeps_overview_and_does_not_start() -> Result<()> {
    let mut tui = make_test_tui()?;
    tui.pause_events();
    for capability in [
        HistoryCapabilities::ConfigReadFails,
        HistoryCapabilities::ThreadStartFails,
        HistoryCapabilities::ThreadStartWhileShuttingDown,
    ] {
        let (mut app, mut events, _) = make_test_app_with_channels().await;
        trust_launch_folder(&mut app);
        app.harness_overrides.model = Some("local-model".into());
        let view = app.agents_overview_view(Vec::new(), /*selected_thread_id*/ None);
        app.chat_widget.show_bottom_pane_view(Box::new(view));
        let (mut server, requests, proxy) = start_recording_app_server_with_history(
            &app.config,
            capability,
            /*blocked_thread_list*/ None,
            /*failed_thread_name*/ None,
            crate::app_server_session::ThreadParamsMode::Embedded,
            LoaderOverrides::default(),
        )
        .await?;
        let source_colors = !app.config.tui_status_line_use_colors;
        app.local_settings.tui.status_line_use_colors = source_colors;
        app.new_agents_overview_session(&mut tui, &mut server, /*cwd*/ None)
            .await?;
        let reads = recorded_params(&requests, "config/read")
            .into_iter()
            .filter(|params| params["includeLayers"] == true)
            .collect::<Vec<_>>();
        assert!(!reads.is_empty());
        assert!(reads.iter().all(|params| params["cwd"] == reads[0]["cwd"]));
        assert_eq!(
            recorded_params(&requests, "thread/start").len(),
            usize::from(capability != HistoryCapabilities::ConfigReadFails)
        );
        assert_eq!(recorded_params(&requests, "turn/start").len(), 0);
        assert_eq!(app.local_settings.tui.status_line_use_colors, source_colors);
        assert!(app.agents_overview.dispatched_requests.is_empty());
        assert!(
            app.chat_widget
                .selected_index_for_present_view(AGENTS_OVERVIEW_VIEW_ID)
                .is_some()
        );
        let error = std::iter::from_fn(|| events.try_recv().ok())
            .filter_map(|event| match event {
                AppEvent::InsertHistoryCell(cell) => {
                    Some(lines_to_single_string(&cell.display_lines(/*width*/ 80)))
                }
                _ => None,
            })
            .find(|message| {
                message.contains("Failed to")
                    || message.contains("Unable to check folder trust")
                    || message.contains("Codex's background server is shutting down")
            })
            .expect("visible read error");
        if capability == HistoryCapabilities::ConfigReadFails {
            insta::assert_snapshot!(error, @"■ Unable to check folder trust: config/read failed while checking remote project trust");
        } else if matches!(
            capability,
            HistoryCapabilities::ThreadStartWhileShuttingDown
        ) {
            insta::assert_snapshot!(
                "command_center_embedded_server_shutting_down",
                crate::chatwidget::tests::helpers::render_bottom_popup(
                    &app.chat_widget,
                    /*width*/ 80,
                )
            );
        } else {
            insta::assert_snapshot!(
                "command_center_session_start_error",
                crate::chatwidget::tests::helpers::render_bottom_popup(
                    &app.chat_widget,
                    /*width*/ 80,
                )
            );
        }
        server.shutdown().await?;
        proxy.await??;
    }
    Ok(())
}

#[tokio::test]
async fn command_center_new_preserves_permissions_across_sessions() -> Result<()> {
    let mut app = make_test_app_with_channels().await.0;
    trust_launch_folder(&mut app);
    let (mut server, requests, proxy) = start_recording_app_server_with_history(
        &app.config,
        HistoryCapabilities::Current,
        /*blocked_thread_list*/ None,
        /*failed_thread_name*/ None,
        crate::app_server_session::ThreadParamsMode::Embedded,
        LoaderOverrides::default(),
    )
    .await?;
    assert!(
        app.apply_permission_profile_selection(PermissionProfileSelection {
            profile_id: ":read-only".into(),
            approval_policy: Some(AskForApproval::UnlessTrusted),
            approvals_reviewer: Some(ApprovalsReviewer::User),
            display_label: "Read Only".into(),
        })
        .await
    );
    let mut tui = make_test_tui()?;
    tui.pause_events();
    for _ in 0..2 {
        app.new_agents_overview_session(&mut tui, &mut server, /*cwd*/ None)
            .await?;
        assert_eq!(
            (
                app.chat_widget
                    .config_ref()
                    .permissions
                    .permission_profile(),
                app.chat_widget
                    .config_ref()
                    .permissions
                    .approval_policy
                    .value(),
                app.chat_widget
                    .config_ref()
                    .permissions
                    .active_permission_profile(),
            ),
            (
                &codex_protocol::models::PermissionProfile::read_only(),
                codex_protocol::protocol::AskForApproval::UnlessTrusted,
                Some(ActivePermissionProfile::new(":read-only")),
            ),
        );
    }
    assert_eq!(recorded_params(&requests, "thread/start").len(), 2);
    server.shutdown().await?;
    proxy.await??;
    Ok(())
}
#[tokio::test]
async fn command_center_new_checkout_and_worktree_preserve_source_and_default_branch() -> Result<()>
{
    use codex_worktree::CreateWorktree;
    use codex_worktree::WorktreeManager;
    use codex_worktree::WorktreeSettings;
    for (remote_only, stale_local) in [(false, false), (true, false), (false, true)] {
        let directory = tempdir()?;
        let source = directory.path().join("project");
        std::fs::create_dir_all(source.join("subdir"))?;
        let git = |args: &[&str]| {
            let output = std::process::Command::new("git")
                .args(["-c", "commit.gpgSign=false"])
                .args(args)
                .current_dir(&source)
                .env("GIT_AUTHOR_NAME", "Test")
                .env("GIT_AUTHOR_EMAIL", "test@example.com")
                .env("GIT_COMMITTER_NAME", "Test")
                .env("GIT_COMMITTER_EMAIL", "test@example.com")
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
        };
        git(&["init", "-b", "main"]);
        std::fs::write(source.join("subdir/file"), "default branch")?;
        git(&["add", "."]);
        git(&["commit", "-m", "default"]);
        if remote_only {
            git(&["update-ref", "refs/remotes/origin/release/default", "HEAD"]);
            git(&[
                "symbolic-ref",
                "refs/remotes/origin/HEAD",
                "refs/remotes/origin/release/default",
            ]);
        }
        git(&["checkout", "-b", "feature"]);
        std::fs::write(source.join("subdir/file"), "feature commit")?;
        git(&["commit", "-am", "feature"]);
        if remote_only {
            git(&["branch", "-D", "main"]);
        }
        if stale_local {
            git(&["update-ref", "refs/remotes/origin/main", "main"]);
            git(&[
                "symbolic-ref",
                "refs/remotes/origin/HEAD",
                "refs/remotes/origin/main",
            ]);
            git(&["branch", "-f", "main", "feature"]);
        }
        let home = directory.path().join("home");
        std::fs::create_dir(&home)?;
        std::fs::write(
            home.join("config.toml"),
            format!(
                "approvals_reviewer = \"auto_review\"\nsandbox_mode = \"workspace-write\"\napproval_policy = \"on-request\"\nwindows.sandbox = \"unelevated\"\n[projects.{:?}]\ntrust_level = \"trusted\"\n",
                codex_config::loader::project_trust_key(&source),
            ),
        )?;
        let manager =
            WorktreeManager::new(WorktreeSettings::for_cli(&home, /*desktop*/ None).unwrap());
        let selected = manager
            .create(&CreateWorktree {
                source_cwd: source.join("subdir"),
                base: Some("feature".into()),
            })
            .unwrap();
        manager
            .bind_thread(&selected.root, "existing-owner")
            .unwrap();
        std::fs::write(selected.cwd.join("file"), "dirty edit")?;
        std::fs::write(selected.cwd.join("untracked"), "untracked")?;
        for receiver_closed in [true, false] {
            let unclaimed = manager
                .create(&CreateWorktree {
                    source_cwd: source.join("subdir"),
                    base: Some("feature".into()),
                })
                .unwrap();
            let root = unclaimed.root.clone();
            let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
            let receiver = (!receiver_closed).then_some(rx);
            AppEventSender::new(tx).send(AppEvent::AgentsOverviewWorktreeCreated(Ok(
                PendingWorktree {
                    manager: manager.clone(),
                    checkout: Some(unclaimed),
                },
            )));
            drop(receiver);
            assert!(!root.exists());
            assert_eq!(manager.list(&source).unwrap().len(), 1);
        }
        let (mut app, mut events, _) = make_test_app_with_channels().await;
        app.config.codex_home = home.clone().abs();
        app.config.cwd = selected.cwd.clone().abs();
        app.chat_widget.windows_sandbox_local_server = cfg!(target_os = "windows");
        app.harness_overrides.cwd = Some(selected.cwd.clone());
        trust_launch_folder(&mut app);
        let (mut server, requests, proxy) = start_recording_app_server_with_history(
            &app.config,
            HistoryCapabilities::Current,
            /*blocked_thread_list*/ None,
            /*failed_thread_name*/ None,
            crate::app_server_session::ThreadParamsMode::Embedded,
            LoaderOverrides::default(),
        )
        .await?;
        let mut tui = make_test_tui()?;
        tui.pause_events();
        let new_session = app.new_agents_overview_session(
            &mut tui,
            &mut server,
            Some(selected.cwd.clone().abs()),
        );
        let size = std::mem::size_of_val(&new_session);
        assert!(
            size < 64 * 1024,
            "new-session wrapper future is {size} bytes"
        );
        new_session.await?;
        let first = app.chat_widget.thread_id().expect("new checkout session");
        assert!(!crate::session_resume::cwds_differ(
            app.chat_widget.config_ref().cwd.as_path(),
            &selected.cwd
        ));
        assert_eq!(
            manager.owner(&selected.root).unwrap(),
            Some("existing-owner".into())
        );
        // Exercise the menu's ordered events, not the separate profile-selection API.
        app.chat_widget
            .set_feature_enabled(Feature::GuardianApproval, /*enabled*/ true);
        Box::pin(app.handle_event(&mut tui, &mut server, AppEvent::OpenPermissionsPopup)).await?;
        tokio::time::timeout(Duration::from_secs(3), async {
            while let Some(event) = events.recv().await {
                let loaded = matches!(&event, AppEvent::PermissionProfilesLoaded { .. });
                Box::pin(app.handle_event(&mut tui, &mut server, event)).await?;
                if loaded {
                    return Ok::<_, color_eyre::Report>(());
                }
            }
            color_eyre::eyre::bail!("permissions menu event stream closed")
        })
        .await??;
        app.chat_widget.handle_key_event(KeyCode::Up.into());
        app.chat_widget.handle_key_event(KeyCode::Enter.into());
        while let Ok(event) = events.try_recv() {
            Box::pin(app.handle_event(&mut tui, &mut server, event)).await?;
        }
        confirm_permission_selection(&mut app, &mut server, first).await?;
        assert_eq!(
            app.chat_widget.config_ref().approvals_reviewer,
            ApprovalsReviewer::User
        );
        app.new_agents_overview_worktree(&mut tui, &mut server, Some(selected.cwd.clone().abs()))
            .await;
        assert!(app.pending_managed_worktree_creation);
        let result = tokio::time::timeout(Duration::from_secs(30), async {
            loop {
                if let Some(AppEvent::AgentsOverviewWorktreeCreated(result)) = events.recv().await {
                    break result;
                }
            }
        })
        .await?
        .map_err(|message| color_eyre::eyre::eyre!(message))?;
        let checkout = result.checkout.as_ref().unwrap().clone();
        assert_eq!(
            std::fs::read_to_string(checkout.cwd.join("file"))?,
            "default branch"
        );
        assert!(!checkout.cwd.join("untracked").exists());
        assert_eq!(checkout.cwd, checkout.root.join("subdir"));
        Box::pin(app.handle_event(
            &mut tui,
            &mut server,
            AppEvent::AgentsOverviewWorktreeCreated(Ok(result)),
        ))
        .await?;
        let second = app.chat_widget.thread_id().expect("new worktree session");
        assert_ne!(first, second);
        assert_eq!(
            recorded_params(&requests, "thread/start").last().unwrap()["approvalsReviewer"],
            serde_json::json!("user")
        );
        assert_eq!(
            app.chat_widget.config_ref().approvals_reviewer,
            ApprovalsReviewer::User
        );
        assert_eq!(
            manager.owner(&checkout.root).unwrap(),
            Some(second.to_string())
        );
        assert_eq!(recorded_params(&requests, "turn/start").len(), 0);
        assert_eq!(
            std::fs::read_to_string(selected.cwd.join("file"))?,
            "dirty edit"
        );
        server.shutdown().await?;
        proxy.await??;
        if !remote_only && !stale_local {
            let unused = manager
                .create(&CreateWorktree {
                    source_cwd: source.join("subdir"),
                    base: Some("main".into()),
                })
                .unwrap();
            let (mut failed_server, _, failed_proxy) = start_recording_app_server_with_history(
                &app.config,
                HistoryCapabilities::ThreadStartFails,
                /*blocked_thread_list*/ None,
                /*failed_thread_name*/ None,
                crate::app_server_session::ThreadParamsMode::Embedded,
                LoaderOverrides::default(),
            )
            .await?;
            let view = app.agents_overview_view(Vec::new(), /*selected_thread_id*/ None);
            app.chat_widget.show_bottom_pane_view(Box::new(view));
            app.start_agents_overview_session(
                &mut tui,
                &mut failed_server,
                Some(unused.cwd.clone().abs()),
                Some((manager.clone(), unused.clone())),
                /*startup_draft*/ None,
            )
            .await?;
            assert_eq!(app.chat_widget.thread_id(), Some(second));
            assert_eq!(manager.owner(&unused.root).unwrap(), None);
            assert!(unused.root.exists());
            let rendered = crate::chatwidget::tests::helpers::render_bottom_popup(
                &app.chat_widget,
                /*width*/ 500,
            );
            insta::assert_snapshot!(
                "command_center_retained_worktree_error",
                rendered.replace(&unused.root.display().to_string(), "<worktree>")
            );
            failed_server.shutdown().await?;
            failed_proxy.await??;
        }
    }
    Ok(())
}
