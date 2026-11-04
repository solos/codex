//! TUI-only app-server reconnection. Each attempt initializes a fresh client and rejoins existing
//! threads using ordinary resume/history semantics. Confirmed submissions are reconciled before
//! unsent queues resume; uncertain submissions and old async completions remain quarantined.

use super::*;
use crate::app_server_session::ResumeModelSettings;
use crate::app_server_session::is_server_shutting_down_error;
use crate::dynamic_tools_mcp::ThreadToolTransport;

#[derive(Clone, Copy, Default, PartialEq, Eq)]
pub(super) enum ReconnectPresentation {
    #[default]
    Conversation,
    Overview,
    Startup(tokio::time::Instant),
}

#[derive(Default)]
pub(super) struct ReconnectState {
    pub(super) offline: bool,
    pub(super) failed: bool,
    pub(super) presentation: ReconnectPresentation,
}

pub(super) struct Reconnected {
    session: AppServerSession,
    bootstrap: AppServerBootstrap,
    thread: Option<AppServerStartedThread>,
}

#[expect(
    clippy::too_many_arguments,
    reason = "retain logical ownership across connection replacement"
)]
pub(super) async fn reconnect(
    _backend_id: crate::backend_id::BackendId,
    _target: AppServerTarget,
    _config: Config,
    _local_settings: crate::local_settings::LocalSettings,
    _thread_id: Option<ThreadId>,
    _remote_cwd: Option<PathBuf>,
    _task_tools: ThreadToolTransport,
    _presentation: ReconnectPresentation,
) -> Result<Reconnected> {
    color_eyre::eyre::bail!("app-server reconnection is not supported")
}

impl App {
    pub(crate) fn is_offline(&self) -> bool {
        self.reconnect.offline
    }

    // Preserve local choices for future input, without replaying failed settings writes or
    // changing the server's authorization for work that was already admitted.
    pub(super) fn restore_runtime_permissions(
        &self,
        session: &mut ThreadSessionState,
        cached: &ThreadSessionState,
    ) {
        if self.current_displayed_thread_id() != Some(session.thread_id)
            && self.primary_thread_id != Some(session.thread_id)
        {
            return;
        }
        // Side conversations can replace the app-wide overrides. Only copy choices
        // that still match this conversation's own cached settings.
        if let Some(policy) = self
            .runtime_approval_policy_override
            .map(RuntimeApprovalPolicyOverride::policy)
            && policy == cached.approval_policy
        {
            session.approval_policy = policy;
        }
        if let Some(reviewer) = self.runtime_approvals_reviewer_override
            && reviewer == cached.approvals_reviewer
        {
            session.approvals_reviewer = reviewer;
        }
        if let Some(profile) = &self.runtime_permission_profile_override
            && profile.permission_profile == cached.permission_profile
            && profile.active_permission_profile == cached.active_permission_profile
            && self.config.approvals_reviewer == cached.approvals_reviewer
        {
            session.permission_profile = profile.permission_profile.clone();
            session.active_permission_profile = profile.active_permission_profile.clone();
            session.approvals_reviewer = self.config.approvals_reviewer;
        }
    }

    pub(super) fn thread_unavailable(&self, id: ThreadId) -> bool {
        let _ = id;
        false
    }

    pub(super) fn recover_transport_error(&mut self, error: &color_eyre::Report) -> bool {
        let disconnected = matches!(
            error.downcast_ref::<TypedRequestError>(),
            Some(TypedRequestError::Transport { .. })
        );
        disconnected && self.begin_reconnect()
    }

    pub(super) fn begin_reconnect(&mut self) -> bool {
        self.pending_startup_thread_start = false;
        false
    }

    pub(super) fn begin_reconnect_for_rejected_startup(&mut self) -> bool {
        false
    }

    pub(super) async fn finish_reconnect(
        &mut self,
        tui: &mut tui::Tui,
        app_server: &mut AppServerSession,
        app_event_rx: &mut mpsc::UnboundedReceiver<AppEvent>,
        connected: Reconnected,
        client_version: &str,
    ) -> Result<()> {
        // Keep input paused while the disconnected view is replaced so its keys cannot reach the
        // restored composer. The final drain below drops anything typed during the handoff.
        let discard_replaced_view_input = self.chat_widget.cancel_active_disconnected_view();
        if discard_replaced_view_input {
            tui.pause_events();
        }
        let Reconnected {
            mut session,
            bootstrap,
            thread,
        } = connected;
        session.model_provider_override = self.harness_overrides.model_provider.clone();
        let selected = self
            .chat_widget
            .selected_index_for_present_view(agents_overview::AGENTS_OVERVIEW_VIEW_ID)
            .and_then(|index| self.agents_overview.visible_thread_ids.get(index).copied());
        let displayed = self.current_displayed_thread_id();
        let mut input = self.chat_widget.capture_thread_input_state();
        if let Some(input) = input.as_mut() {
            input.reconnect_pending = true;
        }
        self.store_active_thread_receiver().await;
        // Old request handles stay attached to the dead connection. Rotating the event channel
        // prevents their late completions from submitting follow-up operations on the new one.
        let (tx, rx) = mpsc::unbounded_channel();
        self.app_event_tx = AppEventSender::new(tx);
        *app_event_rx = rx;
        // The old timer targets the discarded channel, so a new preview can be scheduled.
        self.config_notification_test_pending = None;
        {
            let mut state = self
                .agents_overview
                .view_state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            state.connection_notice = None;
            if state.creating_worktree {
                state.creating_worktree = false;
                self.pending_managed_worktree_creation = false;
            }
        }
        self.agent_navigation.picker_refresh = None;
        self.last_subagent_backfill_attempt = None;
        self.rate_limit_refresh_state.invalidate_recovery();
        session.worktree_source_config_builder = app_server.worktree_source_config_builder.clone();
        session.inherit_task_tool_capabilities(app_server);
        *app_server = session;
        #[cfg(any(target_os = "windows", test))]
        let interrupted_windows_setup = self.windows_sandbox.pending_setup.take().is_some();
        #[cfg(any(target_os = "windows", test))]
        {
            if interrupted_windows_setup {
                self.windows_sandbox.setup_started_at = None;
                self.chat_widget.clear_windows_sandbox_setup_status();
                self.chat_widget.windows_sandbox_elevated_setup_complete = false;
            }
        }
        self.chat_widget.snapshot_local_images = self.app_server_target.uses_remote_workspace();
        self.chat_widget.set_local_worktree_operations(
            !crate::uses_remote_workspace_or_environment(
                &self.app_server_target,
                self.environment_manager.as_ref(),
            ),
        );
        self.chat_widget.windows_sandbox_local_server =
            !self.app_server_target.uses_remote_workspace()
                && app_server.app_server_platform_os() == Some("windows");
        self.chat_widget.windows_sandbox_host = WindowsSandboxHost::Unknown;
        self.chat_widget.requires_openai_auth = bootstrap.requires_openai_auth;
        self.workspace_command_runner = Some(Arc::new(AppServerWorkspaceCommandRunner::new(
            app_server.request_handle(),
        )));
        self.file_search =
            FileSearchManager::new(self.config.cwd.to_path_buf(), self.app_event_tx.clone());
        self.model_catalog = Arc::new(
            ModelCatalog::new(bootstrap.available_models)
                .with_collaboration_modes(bootstrap.collaboration_modes),
        );
        self.pending_app_server_requests.clear();
        // The displayed task was resumed above. Keep offscreen selections pending until those
        // tasks can be resumed from the server too; their old confirmations cannot arrive.
        let pending_displayed_profile = displayed.and_then(|id| {
            self.pending_server_profiles.remove(&id).or_else(|| {
                self.agents_overview
                    .requested_permission_profiles
                    .remove(&id)
            })
        });
        if let Some(selection) = &pending_displayed_profile {
            self.runtime_approval_policy_override = None;
            if selection.approvals_reviewer.is_some() {
                self.runtime_approvals_reviewer_override = None;
            }
            self.runtime_permission_profile_override = None;
        }
        self.pending_primary_events.clear();
        self.pending_plugin_enabled_writes.clear();
        self.pending_hook_enabled_writes.clear();
        self.temporary_structured_requests.clear();
        for (_, cancellation) in self.pending_thread_titles.drain() {
            cancellation.cancel();
        }
        self.agents_overview.dispatched_requests.clear();
        self.agents_overview.request_id = None;
        self.agents_overview.refresh_show_more = false;
        self.agents_overview.active_refresh_thread_ids.clear();
        self.agents_overview.refresh_pending = false;
        for input in self.agents_overview.input_states.values_mut() {
            input.reconnect_pending = true;
        }
        self.pending_startup_thread_start = matches!(
            self.reconnect.presentation,
            ReconnectPresentation::Startup(_)
        );
        // Move cached UI state into fresh channels. Old producers retain the old sender/store,
        // so their late requests cannot leak into recovery. Background threads attach on selection.
        for channel in self.thread_event_channels.values_mut() {
            let mut replacement = ThreadEventChannel::new(THREAD_EVENT_CHANNEL_CAPACITY);
            if channel.attachment() == ThreadEventAttachment::ExternalWriter {
                replacement.mark_external_writer();
            } else {
                replacement.mark_replay_only();
            }
            let mut store = std::mem::replace(
                &mut *channel.store.lock().await,
                ThreadEventStore::new(THREAD_EVENT_CHANNEL_CAPACITY),
            );
            store.pending_interactive_replay = Default::default();
            store.pending_interrupt_turn_id = None;
            store.active = false;
            store
                .buffer
                .retain(|event| matches!(event, ThreadBufferedEvent::Notification(_)));
            if let Some(input) = store.input_state.as_mut() {
                input.reconnect_pending = true;
            }
            *replacement.store.lock().await = store;
            *channel = replacement;
        }
        if let Some(id) = self.current_displayed_thread_id()
            && let Some(mut input) = input.clone()
        {
            input.reconnect_pending = true;
            self.agents_overview.input_states.insert(id, input);
        }
        if let Some(mut started) = thread {
            let id = started.session.thread_id;
            if pending_displayed_profile.is_none()
                && let Some(channel) = self.thread_event_channels.get(&id)
                && let Some(cached) = channel.store.lock().await.session.as_ref()
            {
                self.restore_runtime_permissions(&mut started.session, cached);
            }
            self.agents_overview.input_states.remove(&id);
            if started
                .turns
                .iter()
                .any(|turn| turn.status == TurnStatus::InProgress)
            {
                self.agent_navigation.mark_running(id);
            } else {
                self.agent_navigation.mark_stopped(id);
            }
            if self.primary_thread_id == Some(id) {
                self.primary_session_configured = Some(started.session.clone());
            }
            if started.blocks_direct_input {
                self.agent_navigation.mark_parent_owned(id);
            }
            let channel = ThreadEventChannel::new(THREAD_EVENT_CHANNEL_CAPACITY);
            {
                let mut store = channel.store.lock().await;
                store.set_session(started.session, started.turns);
                store.input_state = input.clone();
            }
            self.thread_event_channels.insert(id, channel);
        }
        if let Some(id) = displayed
            && let Some((receiver, snapshot)) = self.activate_thread_for_replay(id).await
        {
            self.active_thread_id = Some(id);
            self.active_thread_rx = Some(receiver);
            self.recap.seed_from_turns(&snapshot.turns, Instant::now());
            if let Err(err) = self.render_thread_snapshot(
                tui, app_server, id, snapshot, /*resume_restored_queue*/ false,
            ) {
                if discard_replaced_view_input
                    && let Err(drain_err) = tui.discard_pending_input_before_interactive_screen()
                {
                    tracing::warn!(%drain_err, "failed to discard input after reconnecting");
                }
                return Err(err);
            }
            self.config = self.chat_widget.config_ref().clone();
            self.refresh_pending_thread_approvals().await;
            if self.thread_unavailable(id) && !self.chat_widget.is_external_writer_view() {
                self.agent_navigation.mark_stopped(id);
                self.chat_widget.pause_unavailable_thread();
                self.chat_widget.add_info_message("This conversation is unavailable. Its cached transcript and draft remain here; input is paused. Open the agent picker or return to the parent to continue.".into(), /*hint*/ None);
            } else {
                self.schedule_recap_check(id, Instant::now());
            }
        } else {
            self.active_thread_id = None;
            self.active_thread_rx = None;
            self.primary_thread_id = None;
            self.primary_session_configured = None;
            let init = self.chatwidget_init_for_forked_or_resumed_thread(
                tui,
                self.config.clone(),
                /*initial_user_message*/ None,
            );
            self.replace_chat_widget(ChatWidget::new_with_app_event(init));
            self.chat_widget.restore_reconnected_input(input, &[]);
        }
        // Discover tasks whose notifications were missed, without clearing retained rows.
        // A hidden overview performs this discovery when it is next opened.
        self.agents_overview.initialized = false;
        if self.reconnect.presentation == ReconnectPresentation::Overview {
            let threads = self
                .agents_overview
                .threads
                .values()
                .flatten()
                .cloned()
                .collect();
            let view = self.agents_overview_view(threads, selected);
            self.chat_widget.show_bottom_pane_view(Box::new(view));
            self.refresh_agents_overview_threads(app_server);
            // The Overview survived request cleanup, but terminal typeahead may have survived too.
            if self.startup_protected_input_boundary {
                self.render_startup_frame(tui, app_event_rx)?;
            }
        }
        #[cfg(any(target_os = "windows", test))]
        if interrupted_windows_setup {
            self.chat_widget.add_error_message(
                "Windows sandbox setup was interrupted. Restart Codex before using Agent mode."
                    .to_string(),
            );
        }
        // Only accept fresh task-tool calls once this connection and its event queue are adopted.
        if let ThreadToolTransport::Mcp(server) = app_server.thread_tool_transport() {
            server.reconnect(app_server.request_handle(), self.app_event_tx.clone());
        }
        self.reconnect.offline = false;
        self.chat_widget.update_account_state(
            bootstrap.status_account_display,
            bootstrap.plan_type,
            bootstrap.has_chatgpt_account,
            matches!(bootstrap.auth_mode, Some(TelemetryAuthMode::Chatgpt)),
        );
        if self.chat_widget.has_chatgpt_account() {
            crate::security_setup::prefetch(
                &self.config,
                app_server,
                self.app_event_tx.clone(),
                self.chat_widget.security_setup_request_id,
            );
        }
        self.feedback_audience = bootstrap.feedback_audience;
        if displayed.is_some_and(|id| !self.thread_unavailable(id)) {
            self.chat_widget.maybe_send_next_queued_input();
        }
        let _ = client_version;
        if self.pending_startup_thread_start {
            self.chat_widget
                .set_queue_submissions_until_session_configured(/*queue*/ true);
        }
        if discard_replaced_view_input
            && let Err(err) = tui.discard_pending_input_before_interactive_screen()
        {
            tracing::warn!(%err, "failed to discard input after reconnecting");
        }
        Ok(())
    }
}
