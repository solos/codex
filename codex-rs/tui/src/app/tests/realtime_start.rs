//! Voice startup rejection coverage without an audio device.

use super::*;
use crate::app::tests::session_lifecycle_requests::recorded_params;
use crate::app::tests::session_lifecycle_requests::start_recording_cloud_app_server;
use pretty_assertions::assert_eq;
#[tokio::test]
async fn background_voice_closes_with_its_owner_not_the_visible_thread() -> Result<()> {
    for owner_closed in [false, true] {
        let (mut app, _events, mut ops) = make_test_app_with_channels().await;
        let owner = ThreadId::new();
        let visible = ThreadId::new();
        crate::chatwidget::activate_voice_for_thread(&mut app.chat_widget, owner);
        let (replacement, _, _, _) =
            crate::chatwidget::tests::make_chatwidget_manual_with_sender().await;
        app.replace_chat_widget(replacement);
        app.chat_widget
            .handle_thread_session_quiet(test_thread_session(
                visible,
                app.config.cwd.to_path_buf(),
            ));
        app.active_thread_id = Some(visible);
        let closed = if owner_closed { owner } else { visible };
        app.enqueue_thread_notification(
            closed,
            ServerNotification::ThreadClosed(codex_app_server_protocol::ThreadClosedNotification {
                thread_id: closed.to_string(),
            }),
        )
        .await?;
        assert_eq!(
            app.voice_owner_thread_id(),
            (!owner_closed).then_some(owner)
        );
        assert!(ops.try_recv().is_err());
    }
    Ok(())
}
#[tokio::test]
async fn voice_mute_shortcut_reaches_the_active_widget() -> Result<()> {
    let (mut app, mut events, _) = make_test_app_with_channels().await;
    crate::chatwidget::activate_voice_for_thread(&mut app.chat_widget, ThreadId::new());
    let mut server = crate::start_embedded_app_server_for_picker(&app.config).await?;
    let mut tui = crate::tui::test_support::make_test_tui()?;
    app.handle_tui_event(
        &mut tui,
        &mut server,
        tui::TuiEvent::Key(KeyEvent::new(KeyCode::F(9), KeyModifiers::NONE)),
    )
    .await?;
    // The fixture has no microphone handle, so reaching mute reports that limitation.
    assert!(std::iter::from_fn(|| events.try_recv().ok()).any(|event| {
        matches!(event, AppEvent::InsertHistoryCell(cell) if cell.display_lines(/*width*/ 80)
            .iter().any(|line| line.to_string().contains("Start voice mode before muting")))
    }));
    server.shutdown().await?;
    Ok(())
}
