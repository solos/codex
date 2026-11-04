//! Queue user messages through the local app server.

use crate::app_server_session::AppServerSession;
use crate::named_session_lookup::SessionCollection;
use crate::named_session_lookup::lookup;
use crate::session_archive_commands::SessionArchiveCommandOptions;
use crate::session_archive_commands::start_app_server_for_session_command;
use codex_app_server_protocol::ClientRequest;
use codex_app_server_protocol::ThreadQueueAddParams;
use codex_app_server_protocol::ThreadQueueAddResponse;
use codex_app_server_protocol::UserInput;
use codex_protocol::ThreadId;
use codex_utils_home_dir::find_codex_home;
use color_eyre::eyre::Result;
use color_eyre::eyre::WrapErr;
use color_eyre::eyre::eyre;
use std::path::Path;
use uuid::Uuid;

pub async fn run_session_queue_command(
    target: String,
    message: String,
    options: SessionArchiveCommandOptions,
) -> Result<String> {
    let codex_home = find_codex_home().wrap_err("failed to find Codex home")?;
    let mut app_server =
        start_app_server_for_session_command(options, codex_home.to_path_buf()).await?;
    let client_message_id = Uuid::now_v7().to_string();

    let (thread_id, response) = run_session_queue_action_with_app_server(
        &mut app_server,
        codex_home.as_path(),
        &target,
        &message,
        &client_message_id,
    )
    .await?;

    Ok(format!(
        "Queued message {} for thread {}.",
        response.queued_submission.id, thread_id
    ))
}

pub(super) async fn run_session_queue_action_with_app_server(
    app_server: &mut AppServerSession,
    codex_home: &Path,
    target: &str,
    message: &str,
    client_message_id: &str,
) -> Result<(ThreadId, ThreadQueueAddResponse)> {
    let thread_id = if let Ok(thread_id) = ThreadId::from_string(target) {
        thread_id
    } else {
        let thread = lookup(
            app_server,
            codex_home,
            target,
            &[SessionCollection::Active],
            &[
                super::resume_source_kinds(/*include_non_interactive*/ true),
                // An empty filter includes Atlas/ChatGPT sessions.
                Vec::new(),
            ],
            /*model_provider*/ None,
        )
        .await?
        .ok_or_else(|| eyre!("No active session found matching '{target}'."))?;
        ThreadId::from_string(&thread.id)
            .wrap_err_with(|| format!("app server returned invalid session id `{}`", thread.id))?
    };
    let request_id = app_server.next_request_id();
    let response = app_server
        .request_handle()
        .request_typed(ClientRequest::ThreadQueueAdd {
            request_id,
            params: ThreadQueueAddParams {
                thread_id: thread_id.to_string(),
                input: vec![UserInput::Text {
                    text: message.to_string(),
                    text_elements: Vec::new(),
                }],
                client_user_message_id: client_message_id.to_string(),
            },
        })
        .await
        .wrap_err("failed to queue session message")?;
    Ok((thread_id, response))
}

#[cfg(test)]
#[path = "session_queue_commands_tests.rs"]
mod tests;
