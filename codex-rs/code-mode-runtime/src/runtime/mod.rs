mod audio;
mod callbacks;
mod globals;
mod module_loader;
mod timers;
mod value;

use std::cell::RefCell;
use std::collections::HashMap;
use std::panic::AssertUnwindSafe;
use std::panic::catch_unwind;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::sync::mpsc as std_mpsc;
use std::thread;

use codex_code_mode_protocol::CodeModeToolKind;
use codex_code_mode_protocol::EnabledToolMetadata;
use codex_code_mode_protocol::ExecuteRequest;
use codex_code_mode_protocol::FunctionCallOutputContentItem;
use codex_code_mode_protocol::normalize_code_mode_identifier;
use codex_protocol::ToolName;
use rquickjs::Context;
use rquickjs::Ctx;
use rquickjs::Function;
use rquickjs::Runtime;
use serde_json::Value as JsonValue;
use tokio::sync::mpsc;

use self::timers::ScheduledTimeout;
use crate::TaskFailureHandler;

#[derive(Clone, Debug)]
pub(crate) struct RuntimeTermination(Arc<AtomicBool>);

impl RuntimeTermination {
    pub(crate) fn new() -> Self {
        Self(Arc::new(AtomicBool::new(false)))
    }

    pub(crate) fn terminate(&self) -> bool {
        !self.0.swap(true, Ordering::AcqRel)
    }

    pub(crate) fn is_terminated(&self) -> bool {
        self.0.load(Ordering::Acquire)
    }
}

#[derive(Debug)]
pub(crate) enum RuntimeCommand {
    ToolResponse { id: String, result: JsonValue },
    ToolError { id: String, error_text: String },
    TimeoutFired { id: u64 },
    ObservePendingFrontier,
    Terminate,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum PendingRuntimeMode {
    #[cfg(test)]
    Continue,
    PauseUntilResumed,
}

#[derive(Debug)]
pub(crate) enum RuntimeControlCommand {
    Continue,
    Resume,
    Terminate,
}

#[derive(Debug)]
pub(crate) enum RuntimeEvent {
    Started,
    Pending,
    ContentItem(FunctionCallOutputContentItem),
    YieldRequested,
    ToolCall {
        id: String,
        name: ToolName,
        kind: CodeModeToolKind,
        input: Option<JsonValue>,
    },
    Notify {
        call_id: String,
        text: String,
    },
    Result {
        stored_value_writes: HashMap<String, Arc<JsonValue>>,
        error_text: Option<String>,
    },
    ThreadPanicked,
}

pub(crate) fn spawn_runtime(
    stored_values: HashMap<String, Arc<JsonValue>>,
    request: ExecuteRequest,
    event_tx: mpsc::UnboundedSender<RuntimeEvent>,
    pending_mode: PendingRuntimeMode,
    task_failure_handler: Option<TaskFailureHandler>,
) -> Result<
    (
        std_mpsc::Sender<RuntimeCommand>,
        std_mpsc::Sender<RuntimeControlCommand>,
        RuntimeTermination,
    ),
    String,
> {
    let (command_tx, command_rx) = std_mpsc::channel();
    let (control_tx, control_rx) = std_mpsc::channel();
    let runtime_command_tx = command_tx.clone();
    let termination = RuntimeTermination::new();
    let runtime_termination = termination.clone();
    let enabled_tools = request
        .enabled_tools
        .into_iter()
        .map(|definition| EnabledToolMetadata {
            global_name: normalize_code_mode_identifier(&definition.name),
            tool_name: definition.tool_name,
            description: definition.description,
            kind: definition.kind,
        })
        .collect::<Vec<_>>();
    let config = RuntimeConfig {
        tool_call_id: request.tool_call_id,
        enabled_tools,
        source: request.source,
        stored_values,
    };
    let (startup_tx, startup_rx) = std_mpsc::sync_channel(1);

    let runtime_handle = tokio::runtime::Handle::current();
    spawn_supervised_runtime_thread(event_tx.clone(), task_failure_handler, move || {
        let _runtime_guard = runtime_handle.enter();
        run_runtime(
            config,
            RuntimeChannels {
                event_tx,
                command_rx,
                control_rx,
                runtime_command_tx,
                startup_tx,
            },
            pending_mode,
            runtime_termination,
        );
    });

    startup_rx
        .recv()
        .map_err(|_| "failed to initialize code mode runtime".to_string())??;
    Ok((command_tx, control_tx, termination))
}

fn spawn_supervised_runtime_thread(
    event_tx: mpsc::UnboundedSender<RuntimeEvent>,
    task_failure_handler: Option<TaskFailureHandler>,
    runtime: impl FnOnce() + Send + 'static,
) {
    thread::spawn(move || {
        if catch_unwind(AssertUnwindSafe(runtime)).is_err() {
            if let Some(task_failure_handler) = task_failure_handler {
                task_failure_handler("code-mode QuickJS runtime thread panicked".to_string());
            }
            let _ = event_tx.send(RuntimeEvent::ThreadPanicked);
        }
    });
}

#[derive(Clone)]
struct RuntimeConfig {
    tool_call_id: String,
    enabled_tools: Vec<EnabledToolMetadata>,
    source: String,
    stored_values: HashMap<String, Arc<JsonValue>>,
}

struct RuntimeChannels {
    event_tx: mpsc::UnboundedSender<RuntimeEvent>,
    command_rx: std_mpsc::Receiver<RuntimeCommand>,
    control_rx: std_mpsc::Receiver<RuntimeControlCommand>,
    runtime_command_tx: std_mpsc::Sender<RuntimeCommand>,
    startup_tx: std_mpsc::SyncSender<Result<(), String>>,
}

// Callback indices refer to this cell's original catalog for its entire lifetime.
struct ToolCallbackMetadata {
    tool_name: ToolName,
    kind: CodeModeToolKind,
}

pub(super) struct PendingToolCall<'js> {
    resolve: Function<'js>,
    reject: Function<'js>,
}

pub(super) struct RuntimeState<'js> {
    event_tx: mpsc::UnboundedSender<RuntimeEvent>,
    pending_tool_calls: HashMap<String, PendingToolCall<'js>>,
    pending_timeouts: HashMap<u64, ScheduledTimeout<'js>>,
    stored_values: HashMap<String, Arc<JsonValue>>,
    stored_value_writes: HashMap<String, Arc<JsonValue>>,
    enabled_tools: Vec<ToolCallbackMetadata>,
    next_tool_call_id: u64,
    next_timeout_id: u64,
    tool_call_id: String,
    runtime_command_tx: std_mpsc::Sender<RuntimeCommand>,
    termination: RuntimeTermination,
    exit_requested: bool,
}

struct RuntimeStateCleanup<'js>(Rc<RefCell<RuntimeState<'js>>>);

impl Drop for RuntimeStateCleanup<'_> {
    fn drop(&mut self) {
        if let Ok(mut state) = self.0.try_borrow_mut() {
            state.pending_tool_calls.clear();
            state.pending_timeouts.clear();
        }
    }
}

pub(super) enum CompletionState {
    Pending,
    Completed { error_text: Option<String> },
}

fn run_runtime(
    config: RuntimeConfig,
    channels: RuntimeChannels,
    pending_mode: PendingRuntimeMode,
    termination: RuntimeTermination,
) {
    let runtime = match Runtime::new() {
        Ok(runtime) => runtime,
        Err(error) => {
            let _ = channels.startup_tx.send(Err(format!(
                "failed to initialize QuickJS runtime: {error}"
            )));
            return;
        }
    };
    let interrupt_termination = termination.clone();
    runtime.set_interrupt_handler(Some(Box::new(move || {
        interrupt_termination.is_terminated()
    })));
    let context = match Context::full(&runtime) {
        Ok(context) => context,
        Err(error) => {
            let _ = channels.startup_tx.send(Err(format!(
                "failed to initialize QuickJS context: {error}"
            )));
            return;
        }
    };

    context.with(|ctx| {
        run_context(ctx, config, channels, pending_mode, termination);
    });
}

fn run_context<'js>(
    ctx: Ctx<'js>,
    config: RuntimeConfig,
    channels: RuntimeChannels,
    pending_mode: PendingRuntimeMode,
    termination: RuntimeTermination,
) {
    let RuntimeChannels {
        event_tx,
        command_rx,
        control_rx,
        runtime_command_tx,
        startup_tx,
    } = channels;
    let state = Rc::new(RefCell::new(RuntimeState {
        event_tx: event_tx.clone(),
        pending_tool_calls: HashMap::new(),
        pending_timeouts: HashMap::new(),
        stored_values: config.stored_values,
        stored_value_writes: HashMap::new(),
        enabled_tools: config
            .enabled_tools
            .iter()
            .map(|tool| ToolCallbackMetadata {
                tool_name: tool.tool_name.clone(),
                kind: tool.kind,
            })
            .collect(),
        next_tool_call_id: 1,
        next_timeout_id: 1,
        tool_call_id: config.tool_call_id,
        runtime_command_tx,
        termination: termination.clone(),
        exit_requested: false,
    }));
    let _state_cleanup = RuntimeStateCleanup(Rc::clone(&state));

    if let Err(error_text) =
        globals::install_globals(&ctx, &config.enabled_tools, Rc::clone(&state))
    {
        let _ = startup_tx.send(Ok(()));
        send_scope_result(&state, Some(error_text));
        return;
    }
    let _ = startup_tx.send(Ok(()));
    let _ = event_tx.send(RuntimeEvent::Started);

    let pending_promise = match module_loader::evaluate_main_module(&ctx, &config.source) {
        Ok(pending_promise) => pending_promise,
        Err(error_text) => {
            send_scope_result(&state, Some(error_text));
            return;
        }
    };

    loop {
        drain_jobs(&ctx);
        if termination.is_terminated() {
            let error_text = (!state.borrow().exit_requested)
                .then(|| "JavaScript execution terminated".to_string());
            send_scope_result(&state, error_text);
            return;
        }
        match module_loader::completion_state(&ctx, &pending_promise, &termination) {
            CompletionState::Completed { error_text } => {
                send_scope_result(&state, error_text);
                return;
            }
            CompletionState::Pending => {}
        }

        let Some(command) = next_runtime_command(&event_tx, &command_rx, &control_rx, pending_mode)
        else {
            return;
        };
        match command {
            RuntimeCommand::Terminate => {
                termination.terminate();
                send_scope_result(&state, Some("JavaScript execution terminated".to_string()));
                return;
            }
            RuntimeCommand::ToolResponse { id, result } => {
                if let Err(error_text) =
                    module_loader::resolve_tool_response(&ctx, &id, Ok(result), &state)
                {
                    send_scope_result(&state, Some(error_text));
                    return;
                }
            }
            RuntimeCommand::ToolError { id, error_text } => {
                if let Err(runtime_error) =
                    module_loader::resolve_tool_response(&ctx, &id, Err(error_text), &state)
                {
                    send_scope_result(&state, Some(runtime_error));
                    return;
                }
            }
            RuntimeCommand::TimeoutFired { id } => {
                if let Err(runtime_error) = timers::invoke_timeout_callback(&ctx, id, &state) {
                    send_scope_result(&state, Some(runtime_error));
                    return;
                }
            }
            RuntimeCommand::ObservePendingFrontier => {}
        }
    }
}

fn drain_jobs(ctx: &Ctx<'_>) {
    while ctx.execute_pending_job() {}
}

fn next_runtime_command(
    event_tx: &mpsc::UnboundedSender<RuntimeEvent>,
    command_rx: &std_mpsc::Receiver<RuntimeCommand>,
    control_rx: &std_mpsc::Receiver<RuntimeControlCommand>,
    pending_mode: PendingRuntimeMode,
) -> Option<RuntimeCommand> {
    loop {
        match command_rx.try_recv() {
            Ok(command) => return Some(command),
            Err(std_mpsc::TryRecvError::Disconnected) => return None,
            Err(std_mpsc::TryRecvError::Empty) => {}
        }

        let _ = event_tx.send(RuntimeEvent::Pending);
        match pending_mode {
            #[cfg(test)]
            PendingRuntimeMode::Continue => return command_rx.recv().ok(),
            PendingRuntimeMode::PauseUntilResumed => match control_rx.recv().ok()? {
                RuntimeControlCommand::Continue => return command_rx.recv().ok(),
                RuntimeControlCommand::Resume => continue,
                RuntimeControlCommand::Terminate => return Some(RuntimeCommand::Terminate),
            },
        }
    }
}

fn send_scope_result(state: &Rc<RefCell<RuntimeState<'_>>>, error_text: Option<String>) {
    let state = state.borrow();
    let stored_value_writes = state.stored_value_writes.clone();
    let error_text = error_text.filter(|_| !state.exit_requested);
    send_result(&state.event_tx, stored_value_writes, error_text);
}

fn send_result(
    event_tx: &mpsc::UnboundedSender<RuntimeEvent>,
    stored_value_writes: HashMap<String, Arc<JsonValue>>,
    error_text: Option<String>,
) {
    let _ = event_tx.send(RuntimeEvent::Result {
        stored_value_writes,
        error_text,
    });
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::time::Duration;

    use pretty_assertions::assert_eq;
    use tokio::sync::mpsc;

    use super::ExecuteRequest;
    use super::PendingRuntimeMode;
    use super::RuntimeCommand;
    use super::RuntimeControlCommand;
    use super::RuntimeEvent;
    use super::spawn_runtime;
    use super::spawn_supervised_runtime_thread;
    use crate::FunctionCallOutputContentItem;

    fn execute_request(source: &str) -> ExecuteRequest {
        ExecuteRequest {
            tool_call_id: "call_1".to_string(),
            enabled_tools: Vec::new(),
            source: source.to_string(),
            yield_time_ms: Some(1),
            max_output_tokens: None,
        }
    }

    #[tokio::test]
    async fn runtime_thread_panic_before_initialization_is_reported_directly() {
        let (event_tx, event_rx) = mpsc::unbounded_channel();
        drop(event_rx);
        let (failure_tx, mut failure_rx) = mpsc::unbounded_channel();
        spawn_supervised_runtime_thread(
            event_tx,
            Some(std::sync::Arc::new(move |reason| {
                let _ = failure_tx.send(reason);
            })),
            || panic!("runtime thread panic probe"),
        );

        assert_eq!(
            tokio::time::timeout(Duration::from_secs(1), failure_rx.recv())
                .await
                .expect("runtime failure timeout")
                .expect("runtime failure"),
            "code-mode QuickJS runtime thread panicked"
        );
    }

    #[tokio::test]
    async fn runtime_thread_panic_is_forwarded_without_owner_supervision() {
        let (event_tx, mut event_rx) = mpsc::unbounded_channel();
        spawn_supervised_runtime_thread(
            event_tx,
            /*task_failure_handler*/ None,
            || panic!("runtime thread panic probe"),
        );

        assert!(matches!(
            tokio::time::timeout(Duration::from_secs(1), event_rx.recv())
                .await
                .expect("runtime panic event timeout"),
            Some(RuntimeEvent::ThreadPanicked)
        ));
    }

    #[tokio::test]
    async fn terminate_execution_stops_javascript() {
        for (source_index, source) in [
            r#"notify("started"); while (true) {}"#,
            r#"
let reads = 0;
const value = {
    get image_url() {
        if (++reads === 1) throw new Error("enter fallback");
        notify("started");
        while (true) {}
    }
};
try { image(value); } catch {}
text("continued after cancellation");
"#,
        ]
        .into_iter()
        .enumerate()
        {
            let (event_tx, mut event_rx) = mpsc::unbounded_channel();
            let (_runtime_tx, _runtime_control_tx, runtime_terminate_handle) = spawn_runtime(
                HashMap::new(),
                execute_request(source),
                event_tx,
                PendingRuntimeMode::Continue,
                /*task_failure_handler*/ None,
            )
            .unwrap();

            let started_event = tokio::time::timeout(Duration::from_secs(1), event_rx.recv())
                .await
                .unwrap()
                .unwrap();
            assert!(matches!(started_event, RuntimeEvent::Started));
            assert!(
                matches!(
                    tokio::time::timeout(Duration::from_secs(1), event_rx.recv())
                        .await
                        .unwrap(),
                    Some(RuntimeEvent::Notify { .. })
                ),
                "source {source_index} did not emit its notification"
            );
            assert!(runtime_terminate_handle.terminate());

            let result_event = tokio::time::timeout(Duration::from_secs(1), event_rx.recv())
                .await
                .unwrap()
                .unwrap();
            let RuntimeEvent::Result { error_text, .. } = result_event else {
                panic!("expected runtime result after termination");
            };
            assert!(error_text.is_some());

            assert!(
                tokio::time::timeout(Duration::from_secs(1), event_rx.recv())
                    .await
                    .unwrap()
                    .is_none()
            );
        }
    }

    #[tokio::test]
    async fn pending_mode_freezes_runtime_commands_until_resume() {
        let (event_tx, mut event_rx) = mpsc::unbounded_channel();
        let (runtime_tx, runtime_control_tx, _runtime_terminate_handle) = spawn_runtime(
            HashMap::new(),
            execute_request(
                r#"
await new Promise((resolve) => setTimeout(resolve, 60_000));
text("after");
await new Promise(() => {});
"#,
            ),
            event_tx,
            PendingRuntimeMode::PauseUntilResumed,
            /*task_failure_handler*/ None,
        )
        .unwrap();

        assert!(matches!(
            tokio::time::timeout(Duration::from_secs(1), event_rx.recv())
                .await
                .unwrap()
                .unwrap(),
            RuntimeEvent::Started
        ));
        assert!(matches!(
            tokio::time::timeout(Duration::from_secs(1), event_rx.recv())
                .await
                .unwrap()
                .unwrap(),
            RuntimeEvent::Pending
        ));

        runtime_tx
            .send(RuntimeCommand::TimeoutFired { id: 1 })
            .unwrap();
        assert!(
            tokio::time::timeout(Duration::from_secs(1), event_rx.recv())
                .await
                .is_err()
        );

        runtime_control_tx
            .send(RuntimeControlCommand::Resume)
            .unwrap();

        let content_event = tokio::time::timeout(Duration::from_secs(1), event_rx.recv())
            .await
            .unwrap()
            .unwrap();
        let RuntimeEvent::ContentItem(FunctionCallOutputContentItem::InputText { text }) =
            content_event
        else {
            panic!("expected resumed runtime output");
        };
        assert_eq!(text, "after");
        assert!(matches!(
            tokio::time::timeout(Duration::from_secs(1), event_rx.recv())
                .await
                .unwrap()
                .unwrap(),
            RuntimeEvent::Pending
        ));

        runtime_control_tx
            .send(RuntimeControlCommand::Terminate)
            .unwrap();
    }

    #[tokio::test]
    async fn timers_release_tasks_when_cleared_or_the_cell_finishes() {
        let (event_tx, mut event_rx) = mpsc::unbounded_channel();
        let (_runtime_tx, _runtime_control_tx, _runtime_terminate_handle) = spawn_runtime(
            HashMap::new(),
            execute_request(
                r#"
clearTimeout(setTimeout(() => text("cancelled"), 3_600_000));
await new Promise((resolve) => setTimeout(resolve, 3_600_000));
text("done");
setTimeout(() => text("late"), 3_600_000);
"#,
            ),
            event_tx,
            PendingRuntimeMode::Continue,
            /*task_failure_handler*/ None,
        )
        .unwrap();

        loop {
            match tokio::time::timeout(Duration::from_secs(1), event_rx.recv())
                .await
                .unwrap()
            {
                Some(RuntimeEvent::Pending) => break,
                Some(_) => {}
                None => panic!("runtime closed before the timer was pending"),
            }
        }
        tokio::task::yield_now().await;
        assert_eq!(
            tokio::runtime::Handle::current()
                .metrics()
                .num_alive_tasks(),
            1
        );
        tokio::time::pause();
        tokio::time::advance(Duration::from_secs(3_600)).await;
        tokio::time::resume();

        let mut output = Vec::new();
        while let Some(event) = tokio::time::timeout(Duration::from_secs(1), event_rx.recv())
            .await
            .expect("timer runtime should finish")
        {
            match event {
                RuntimeEvent::ContentItem(item) => output.push(item),
                RuntimeEvent::Result { error_text, .. } => assert_eq!(error_text, None),
                _ => {}
            }
        }
        assert_eq!(
            output,
            vec![FunctionCallOutputContentItem::InputText {
                text: "done".to_string()
            }]
        );
        tokio::task::yield_now().await;
        assert_eq!(
            tokio::runtime::Handle::current()
                .metrics()
                .num_alive_tasks(),
            0
        );
    }
}
