use anyhow::Result;
use app_test_support::MockResponsesConfig;
use app_test_support::TestAppServer;
use codex_app_server_protocol::ClientRequest;
use codex_app_server_protocol::CurrentTimeReadResponse;
use codex_app_server_protocol::ItemCompletedNotification;
use codex_app_server_protocol::ItemStartedNotification;
use codex_app_server_protocol::JSONRPCErrorError;
use codex_app_server_protocol::ServerRequest;
use codex_app_server_protocol::SleepItem;
use codex_app_server_protocol::ThreadItemPayload;
use codex_app_server_protocol::ThreadStartParams;
use codex_app_server_protocol::ThreadStartResponse;
use codex_app_server_protocol::TurnCompletedNotification;
use codex_app_server_protocol::TurnStartParams;
use codex_app_server_protocol::TurnStartResponse;
use codex_app_server_protocol::TurnStatus;
use codex_app_server_protocol::UserInput as V2UserInput;
use core_test_support::responses;
use pretty_assertions::assert_eq;
use serde_json::json;
use std::time::Duration;
use tempfile::TempDir;
use test_case::test_case;
use tokio::time::timeout;

#[cfg(windows)]
const DEFAULT_READ_TIMEOUT: Duration = Duration::from_secs(25);
#[cfg(not(windows))]
const DEFAULT_READ_TIMEOUT: Duration = Duration::from_secs(10);
const CURRENT_TIME_AT: i64 = 1_781_717_655;

#[test_case(0; "initial read fails")]
#[test_case(1; "polling read fails")]
#[test_case(3; "successful reads")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn external_sleep_polls_current_time_and_emits_items(
    successful_sleep_reads: usize,
) -> Result<()> {
    const CALL_ID: &str = "sleep-1";
    const DURATION_MS: u64 = 2_000;
    let read_fails = successful_sleep_reads < 3;

    let server = responses::start_mock_server().await;
    let model = responses::mount_sse_sequence(
        &server,
        vec![
            responses::sse(vec![
                responses::ev_response_created("resp-1"),
                responses::ev_function_call_with_namespace(
                    CALL_ID,
                    "clock",
                    "sleep",
                    &serde_json::json!({ "duration_ms": DURATION_MS }).to_string(),
                ),
                responses::ev_completed("resp-1"),
            ]),
            responses::sse(vec![
                responses::ev_assistant_message("msg-1", "Done"),
                responses::ev_completed("resp-2"),
            ]),
        ],
    )
    .await;

    let codex_home = TempDir::new()?;
    MockResponsesConfig::new(&server.uri())
        .with_root_config("include_environment_context = false")
        .with_extra_config(&format!(
            r#"[features]
nonfatal_clock_read_errors = {read_fails}
[features.current_time_reminder]
enabled = true
sleep_tool = true
clock_source = "external"
"#,
        ))
        .write(codex_home.path())?;

    let mut mcp = TestAppServer::builder()
        .with_codex_home(codex_home.path())
        .build_initialized()
        .await?;

    let ThreadStartResponse { thread, .. } = mcp
        .start_thread(ThreadStartParams {
            model: Some("mock-model".to_string()),
            ..Default::default()
        })
        .await?;

    let TurnStartResponse { turn, .. } = mcp
        .request(|request_id| ClientRequest::TurnStart {
            request_id,
            params: TurnStartParams {
                thread_id: thread.id.clone(),
                client_user_message_id: None,
                input: vec![V2UserInput::Text {
                    text: "Sleep briefly".to_string(),
                    text_elements: Vec::new(),
                }],
                ..Default::default()
            },
        })
        .await?;

    // Read once for the initial reminder, then once to establish the sleep deadline.
    respond_to_current_time_read(&mut mcp, &thread.id, CURRENT_TIME_AT).await?;
    let started = wait_for_sleep_started(&mut mcp, CALL_ID).await?;
    for current_time_at in [CURRENT_TIME_AT, CURRENT_TIME_AT + 1, CURRENT_TIME_AT + 2]
        .into_iter()
        .take(successful_sleep_reads)
    {
        respond_to_current_time_read(&mut mcp, &thread.id, current_time_at).await?;
    }
    if read_fails {
        let request = timeout(
            DEFAULT_READ_TIMEOUT,
            mcp.read_stream_until_request_message(),
        )
        .await??;
        let ServerRequest::CurrentTimeRead { request_id, params } = request else {
            panic!("expected clock read, got {request:?}");
        };
        assert_eq!(params.thread_id, thread.id);
        mcp.send_error(
            request_id,
            JSONRPCErrorError {
                code: -32_000,
                message: "PRIVATE_CLOCK_ERROR".to_string(),
                data: None,
            },
        )
        .await?;
    }

    let completed = wait_for_sleep_completed(&mut mcp, CALL_ID).await?;

    // The next inference boundary reads the same external clock after the tool returns.
    respond_to_current_time_read(&mut mcp, &thread.id, CURRENT_TIME_AT + 2).await?;
    let turn_completed: TurnCompletedNotification = timeout(
        DEFAULT_READ_TIMEOUT,
        mcp.read_notification("turn/completed"),
    )
    .await??;
    assert_eq!(turn_completed.turn.status, TurnStatus::Completed);
    if read_fails {
        let requests = model.requests();
        assert_eq!(requests.len(), 2);
        assert_eq!(
            requests[1].function_call_output_text(CALL_ID).as_deref(),
            Some("failed to read current time")
        );
    }

    let expected_item = ThreadItemPayload::Sleep(SleepItem {
        id: CALL_ID.to_string(),
        duration_ms: DURATION_MS,
    });
    assert!(completed.completed_at_ms >= started.started_at_ms);
    assert_eq!(
        started,
        ItemStartedNotification::new(
            thread.id.clone(),
            turn.id.clone(),
            expected_item.clone(),
            started.started_at_ms
        )
    );
    assert_eq!(
        completed,
        ItemCompletedNotification::new(
            thread.id,
            turn.id,
            expected_item,
            Some(started.started_at_ms),
            completed.completed_at_ms
        )
    );

    Ok(())
}

async fn wait_for_sleep_started(
    mcp: &mut TestAppServer,
    call_id: &str,
) -> Result<ItemStartedNotification> {
    loop {
        let started: ItemStartedNotification =
            timeout(DEFAULT_READ_TIMEOUT, mcp.read_notification("item/started")).await??;
        if matches!(&started.item.payload, ThreadItemPayload::Sleep(item) if item.id == call_id) {
            return Ok(started);
        }
    }
}

async fn wait_for_sleep_completed(
    mcp: &mut TestAppServer,
    call_id: &str,
) -> Result<ItemCompletedNotification> {
    loop {
        let completed: ItemCompletedNotification = timeout(
            DEFAULT_READ_TIMEOUT,
            mcp.read_notification("item/completed"),
        )
        .await??;
        if matches!(&completed.item.payload, ThreadItemPayload::Sleep(item) if item.id == call_id) {
            return Ok(completed);
        }
    }
}

async fn respond_to_current_time_read(
    mcp: &mut TestAppServer,
    thread_id: &str,
    current_time_at: i64,
) -> Result<()> {
    let request = timeout(
        DEFAULT_READ_TIMEOUT,
        mcp.read_stream_until_request_message(),
    )
    .await??;
    let ServerRequest::CurrentTimeRead { request_id, params } = request else {
        panic!("expected CurrentTimeRead request, got: {request:?}");
    };
    assert_eq!(params.thread_id, thread_id);
    mcp.send_response(
        request_id,
        serde_json::to_value(CurrentTimeReadResponse { current_time_at })?,
    )
    .await?;
    Ok(())
}
