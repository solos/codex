//! Replacement starts use server defaults and preserve the current task on failure.

use super::*;
use codex_app_server_protocol::SkillMetadata;
use codex_app_server_protocol::SkillScope;
use codex_app_server_protocol::SkillsListEntry;
use codex_config::types::HistoryPersistence;
use crossterm::event::KeyCode;
use crossterm::event::KeyEvent;
use crossterm::event::KeyModifiers;
use pretty_assertions::assert_eq;

#[tokio::test]
async fn new_sessions_preserve_yolo_launch_and_later_permission_choices() -> Result<()> {
    let (mut app, _events, _ops) = make_test_app_with_channels().await;
    app.harness_overrides.approval_policy = Some(AskForApproval::Never.to_core());
    app.harness_overrides.sandbox_mode =
        Some(codex_protocol::config_types::SandboxMode::DangerFullAccess);
    let (mut server, requests, proxy) = start_recording_app_server(
        &app.config,
        /*blocked_thread_list*/ None,
        /*failed_thread_name*/ None,
    )
    .await?;
    let mut tui = crate::tui::test_support::make_test_tui()?;
    let mut loaded_config = app.config.clone();
    loaded_config
        .permissions
        .approval_policy
        .set(AskForApproval::OnRequest.to_core())?;
    loaded_config
        .permissions
        .set_permission_profile(PermissionProfile::read_only())?;
    let loaded = server.start_thread(&loaded_config).await?;
    app.select_agents_overview_thread(&mut tui, &mut server, loaded.session.thread_id)
        .await?;
    for picker_change in [false, true] {
        if picker_change {
            app.chat_widget
                .set_approval_policy(AskForApproval::OnRequest);
            app.chat_widget
                .set_permission_profile_from_session_snapshot(PermissionProfileSnapshot::active(
                    PermissionProfile::read_only(),
                    ActivePermissionProfile::new(":read-only"),
                ))?;
            app.adopt_server_permissions();
        }
        app.start_fresh_session(
            &mut tui,
            &mut server,
            /*session_start_source*/ None,
            /*initial_user_message*/ None,
            /*new_thread_name*/ None,
        )
        .await;
        let cached = app.primary_session_configured.as_ref().unwrap().clone();
        let mut resumed = cached.clone();
        resumed.approval_policy = AskForApproval::OnRequest;
        resumed.permission_profile = PermissionProfile::read_only();
        resumed.active_permission_profile = None;
        app.restore_runtime_permissions(&mut resumed, &cached);
        assert_eq!(resumed, cached);
    }
    let params = recorded_params(&requests, "thread/start");
    assert_eq!(
        params
            .iter()
            .skip(1)
            .map(|params| serde_json::json!({
                "approvalPolicy": params["approvalPolicy"],
                "sandbox": params["sandbox"],
                "permissions": params["permissions"],
            }))
            .collect::<Vec<_>>(),
        vec![
            serde_json::json!({"approvalPolicy": "never", "sandbox": "danger-full-access", "permissions": null}),
            serde_json::json!({"approvalPolicy": "on-request", "sandbox": null, "permissions": ":read-only"}),
        ]
    );
    server.shutdown().await?;
    proxy.await??;
    Ok(())
}

#[tokio::test]
async fn new_session_preserves_vim_line_yank() -> Result<()> {
    let (mut app, _events, _ops) = make_test_app_with_channels().await;
    let home = tempdir()?;
    app.config.codex_home = home.path().to_path_buf().abs();
    app.config.sqlite = SqliteConfig::new_for_testing(home.path().abs());
    app.chat_widget.toggle_vim_mode_and_notify();
    app.chat_widget.insert_str("saved line");
    for code in [KeyCode::Esc, KeyCode::Char('d'), KeyCode::Char('d')] {
        app.chat_widget
            .handle_key_event(KeyEvent::new(code, KeyModifiers::NONE));
    }
    assert_eq!(app.chat_widget.composer_text_with_pending(), "");

    let (mut server, _requests, proxy) = start_recording_app_server_with_history(
        &app.config,
        HistoryCapabilities::Current,
        /*blocked_thread_list*/ None,
        /*failed_thread_name*/ None,
        crate::app_server_session::ThreadParamsMode::Embedded,
        LoaderOverrides::default(),
    )
    .await?;
    let mut tui = crate::tui::test_support::make_test_tui()?;
    app.start_fresh_session(
        &mut tui,
        &mut server,
        /*session_start_source*/ None,
        /*initial_user_message*/ None,
        /*new_thread_name*/ None,
    )
    .await;

    app.chat_widget.toggle_vim_mode_and_notify();
    app.chat_widget.insert_str("new line");
    app.chat_widget
        .handle_key_event(KeyEvent::new(KeyCode::Char('p'), KeyModifiers::NONE));
    assert_eq!(
        app.chat_widget.composer_text_with_pending(),
        "new line\nsaved line"
    );
    let composer_lines = render_bottom_popup(&app.chat_widget, /*width*/ 80)
        .lines()
        .take(2)
        .collect::<Vec<_>>()
        .join("\n");
    insta::assert_snapshot!(composer_lines);
    server.shutdown().await?;
    proxy.await??;
    Ok(())
}

#[tokio::test]
async fn widget_replacement_preserves_mention_usage_with_history_disabled() -> Result<()> {
    let (mut app, _events, _ops) = make_test_app_with_channels().await;
    app.local_settings.history.persistence = HistoryPersistence::None;
    let mut tui = crate::tui::test_support::make_test_tui()?;
    let skills = SkillsListResponse {
        data: vec![SkillsListEntry {
            cwd: app.config.cwd.to_path_buf(),
            skills: ["statsig", "status"]
                .into_iter()
                .map(|name| SkillMetadata {
                    name: name.to_owned(),
                    description: String::new(),
                    short_description: None,
                    interface: None,
                    dependencies: None,
                    path: app
                        .config
                        .cwd
                        .to_path_buf()
                        .join(name)
                        .join("SKILL.md")
                        .abs()
                        .into(),
                    scope: SkillScope::User,
                    enabled: true,
                    plugin_id: None,
                })
                .collect(),
            errors: Vec::new(),
        }],
    };
    let init = app.chatwidget_init_for_forked_or_resumed_thread(
        &mut tui,
        app.config.clone(),
        /*initial_user_message*/ None,
    );
    app.replace_chat_widget(ChatWidget::new_with_app_event(init));
    app.chat_widget.set_skills_from_response(&skills);
    app.chat_widget.insert_str("$status");
    app.chat_widget.handle_key_event(KeyCode::Enter.into());

    let init = app.chatwidget_init_for_forked_or_resumed_thread(
        &mut tui,
        app.config.clone(),
        /*initial_user_message*/ None,
    );
    app.replace_chat_widget(ChatWidget::new_with_app_event(init));
    app.chat_widget.set_skills_from_response(&skills);
    app.chat_widget.insert_str("$stat");
    insta::assert_snapshot!(normalize_snapshot_paths(render_bottom_popup(
        &app.chat_widget,
        /*width*/ 80
    )));
    assert!(
        !app.local_settings
            .codex_home
            .join("mention_usage.json")
            .exists()
    );
    Ok(())
}
#[tokio::test]
async fn replacement_failure_keeps_current_task_and_restores_input() -> Result<()> {
    for fail_read in [false, true] {
        let (mut app, mut events, _ops) = make_test_app_with_channels().await;
        let home = tempdir()?;
        app.config.codex_home = home.path().to_path_buf().abs();
        app.config.sqlite = SqliteConfig::new_for_testing(home.path().abs());
        let current = ThreadId::new();
        app.enqueue_primary_thread_session(
            test_thread_session(current, app.config.cwd.to_path_buf()),
            Vec::new(),
        )
        .await?;
        while events.try_recv().is_ok() {}
        let original_config = app.config.clone();
        let (mut server, requests, proxy) = start_recording_app_server_with_history(
            &app.config,
            HistoryCapabilities::ThreadStartFails,
            /*blocked_thread_list*/ None,
            /*failed_thread_name*/ None,
            crate::app_server_session::ThreadParamsMode::Embedded,
            LoaderOverrides::default(),
        )
        .await?;
        if fail_read {
            std::fs::write(home.path().join("config.toml"), "invalid = [")?;
        }
        let mut tui = crate::tui::test_support::make_test_tui()?;
        app.start_fresh_session(
            &mut tui,
            &mut server,
            /*session_start_source*/ None,
            Some("keep this request".into()),
            /*new_thread_name*/ None,
        )
        .await;
        assert_eq!(app.chat_widget.thread_id(), Some(current));
        assert_eq!(app.active_thread_id, Some(current));
        assert!(app.active_thread_rx.is_some());
        assert_eq!(app.config, original_config);
        assert_eq!(
            app.chat_widget.composer_text_with_pending(),
            "keep this request"
        );
        assert!(recorded_params(&requests, "thread/unsubscribe").is_empty());
        assert_eq!(
            recorded_params(&requests, "thread/start").len(),
            usize::from(!fail_read)
        );
        let errors = std::iter::from_fn(|| events.try_recv().ok())
            .filter_map(|event| match event {
                AppEvent::InsertHistoryCell(cell) => {
                    Some(lines_to_single_string(&cell.display_lines(/*width*/ 1000)))
                }
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n");
        if fail_read {
            insta::assert_snapshot!(errors, @"■ Failed to read new session defaults: config/read failed in TUI");
        } else {
            insta::assert_snapshot!(errors, @"■ Failed to start a fresh session through the app server: thread/start failed during TUI bootstrap: thread/start failed: replacement unavailable (code -32603)");
        }
        server.shutdown().await?;
        proxy.await??;
    }
    Ok(())
}

#[tokio::test]
async fn replacement_preserves_remote_launch_paths_and_older_servers() -> Result<()> {
    for (remote_cwd, capabilities) in [
        (None, HistoryCapabilities::Current),
        (Some(PathBuf::from(".")), HistoryCapabilities::Current),
        (None, HistoryCapabilities::ConfigReadUnsupported(-32601)),
        (None, HistoryCapabilities::ConfigReadUnsupported(-32600)),
    ] {
        let (mut app, _events, _ops) = make_test_app_with_channels().await;
        let home = tempdir()?;
        app.config.codex_home = home.path().to_path_buf().abs();
        app.config.sqlite = SqliteConfig::new_for_testing(home.path().abs());
        std::fs::write(
            home.path().join("config.toml"),
            "model = \"configured-model\"\n",
        )?;
        app.enqueue_primary_thread_session(
            test_thread_session(ThreadId::new(), app.config.cwd.to_path_buf()),
            Vec::new(),
        )
        .await?;
        let (server, requests, proxy) = start_recording_app_server_with_history(
            &app.config,
            capabilities,
            /*blocked_thread_list*/ None,
            /*failed_thread_name*/ None,
            crate::app_server_session::ThreadParamsMode::Embedded,
            LoaderOverrides::default(),
        )
        .await?;
        let mut server = server.with_remote_cwd_override(remote_cwd.clone());
        let mut tui = crate::tui::test_support::make_test_tui()?;
        app.start_fresh_session(
            &mut tui,
            &mut server,
            /*session_start_source*/ None,
            /*initial_user_message*/ None,
            /*new_thread_name*/ None,
        )
        .await;
        assert_eq!(
            recorded_params(&requests, "config/read"),
            vec![serde_json::json!({"cwd": ".", "includeLayers": true}),]
        );
        let starts = recorded_params(&requests, "thread/start");
        assert_eq!(starts.len(), 1);
        assert_eq!(
            (&starts[0]["cwd"], &starts[0]["model"]),
            (
                &serde_json::json!(remote_cwd),
                &serde_json::json!("configured-model")
            ),
        );
        assert_eq!(app.chat_widget.current_model(), "configured-model");
        server.shutdown().await?;
        proxy.await??;
    }
    Ok(())
}
