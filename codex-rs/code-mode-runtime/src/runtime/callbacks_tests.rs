//! Verifies that exit is uncatchable and admits no effects after it is requested.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::mpsc as std_mpsc;

use codex_code_mode_protocol::CodeModeToolKind;
use codex_code_mode_protocol::EnabledToolMetadata;
use codex_protocol::ToolName;
use pretty_assertions::assert_eq;
use rquickjs::Context;
use rquickjs::Runtime;
use serde_json::json;
use tokio::sync::mpsc;

use super::super::RuntimeEvent;
use super::super::RuntimeState;
use super::super::RuntimeTermination;
use super::super::ToolCallbackMetadata;
use super::super::globals;
use crate::FunctionCallOutputContentItem;

#[tokio::test]
async fn host_callbacks_admit_no_new_effects_after_exit_is_requested() {
    for (label, source) in [
        (
            "ordinary calls",
            r#"
exit();
text("still working");
image("data:image/png;base64,AAA");
audio("data:audio/mpeg;base64,YXVkaW8=");
generatedImage({image_url: "data:image/png;base64,AAA", output_hint: "late"});
notify("late"); store("phase", "late");
tools.echo("late"); yield_control();
clearTimeout(earlyTimer); setTimeout(() => {}, 60000);
"#,
        ),
        ("text conversion", r#"text({toJSON() { exit(); }});"#),
        ("notify conversion", r#"notify({toJSON() { exit(); }});"#),
        (
            "store conversion",
            r#"store("phase", {toJSON() { exit(); }});"#,
        ),
        ("store key", r#"store({toString() { exit(); }}, "late");"#),
        ("tool conversion", r#"tools.echo({toJSON() { exit(); }});"#),
        ("image getter", r#"image({get image_url() { exit(); }});"#),
        ("audio getter", r#"audio({get audio_url() { exit(); }});"#),
        (
            "generated image hint",
            r#"generatedImage({image_url: "data:image/png;base64,AAA", get output_hint() {exit(); }});"#,
        ),
        (
            "timer delay",
            r#"setTimeout(() => {}, {valueOf() {exit(); }});"#,
        ),
        ("clear timer id", r#"clearTimeout({valueOf() {exit(); }});"#),
    ] {
        let runtime = Runtime::new().expect("create runtime");
        let termination = RuntimeTermination::new();
        let interrupt_termination = termination.clone();
        runtime.set_interrupt_handler(Some(Box::new(move || {
            interrupt_termination.is_terminated()
        })));
        let context = Context::full(&runtime).expect("create context");
        let (event_tx, mut event_rx) = mpsc::unbounded_channel();
        let (runtime_command_tx, _runtime_command_rx) = std_mpsc::channel();
        let tool_name = ToolName::plain("echo");
        context.with(|ctx| {
            let state = Rc::new(RefCell::new(RuntimeState {
                event_tx,
                pending_tool_calls: HashMap::new(),
                pending_timeouts: HashMap::new(),
                stored_values: HashMap::new(),
                stored_value_writes: HashMap::new(),
                enabled_tools: vec![ToolCallbackMetadata {
                    tool_name,
                    kind: CodeModeToolKind::Function,
                }],
                next_tool_call_id: 1,
                next_timeout_id: 1,
                tool_call_id: "cell".to_string(),
                runtime_command_tx,
                termination: termination.clone(),
                exit_requested: false,
            }));
            globals::install_globals(
                &ctx,
                &[EnabledToolMetadata {
                    global_name: "echo".to_string(),
                    tool_name: ToolName::plain("echo"),
                    description: String::new(),
                    kind: CodeModeToolKind::Function,
                }],
                Rc::clone(&state),
            )
            .expect("install globals");

            let program = format!(
                r#"
text("before"); store("phase", "before"); notify("before");
tools.echo("before");
const earlyTimer = setTimeout(() => text("early timer"), 60000);
{source}
"#
            );
            assert!(
                ctx.eval::<(), _>(program).is_err(),
                "{label}: exit should throw"
            );
            let _ = ctx.catch();

            assert!(
                matches!(event_rx.try_recv(), Ok(RuntimeEvent::ContentItem(
                    FunctionCallOutputContentItem::InputText { text }
                )) if text == "before"),
                "{label}: pre-exit output"
            );
            assert!(
                matches!(event_rx.try_recv(), Ok(RuntimeEvent::Notify { text, .. }) if text == "before"),
                "{label}: pre-exit notification"
            );
            assert!(
                matches!(event_rx.try_recv(), Ok(RuntimeEvent::ToolCall { id, input, .. })
                    if id == "tool-1" && input == Some(json!("before"))),
                "{label}: pre-exit tool"
            );
            assert!(
                matches!(event_rx.try_recv(), Err(mpsc::error::TryRecvError::Empty)),
                "{label}: emitted an effect after exit"
            );

            let mut state = state.borrow_mut();
            assert_eq!(
                state.stored_value_writes,
                HashMap::from([("phase".to_string(), Arc::new(json!("before")))]),
                "{label}: stored writes"
            );
            assert_eq!(
                state.stored_values, state.stored_value_writes,
                "{label}: cell storage"
            );
            assert_eq!(state.next_tool_call_id, 2, "{label}: allocated tool ID");
            assert_eq!(state.next_timeout_id, 2, "{label}: allocated timeout ID");
            assert!(
                state.pending_tool_calls.contains_key("tool-1"),
                "{label}: lost prior tool"
            );
            assert!(
                state.pending_timeouts.contains_key(&1),
                "{label}: lost prior timer"
            );
            state.pending_tool_calls.clear();
            state.pending_timeouts.clear();
        });
    }
}
