//! Checks that tool response delivery resolves and rejects the matching promise.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::mpsc as std_mpsc;

use pretty_assertions::assert_eq;
use rquickjs::Context;
use rquickjs::Runtime;
use rquickjs::promise::PromiseState;
use serde_json::json;
use tokio::sync::mpsc;

use super::super::PendingToolCall;
use super::super::RuntimeState;
use super::super::RuntimeTermination;
use super::super::value::js_value_to_json;
use super::resolve_tool_response;

#[test]
fn tool_response_resolves_the_matching_promise() {
    let runtime = Runtime::new().expect("create runtime");
    let context = Context::full(&runtime).expect("create context");
    context.with(|ctx| {
        let (event_tx, _event_rx) = mpsc::unbounded_channel();
        let (runtime_command_tx, _runtime_command_rx) = std_mpsc::channel();
        let termination = RuntimeTermination::new();
        let state = Rc::new(RefCell::new(RuntimeState {
            event_tx,
            pending_tool_calls: HashMap::new(),
            pending_timeouts: HashMap::new(),
            stored_values: HashMap::new(),
            stored_value_writes: HashMap::new(),
            enabled_tools: Vec::new(),
            next_tool_call_id: 1,
            next_timeout_id: 1,
            tool_call_id: "cell".to_string(),
            runtime_command_tx,
            termination: termination.clone(),
            exit_requested: false,
        }));

        let (promise, resolve, reject) = ctx.promise().expect("create tool promise");
        state
            .borrow_mut()
            .pending_tool_calls
            .insert("tool".to_string(), PendingToolCall { resolve, reject });
        let response = json!({"content": [{"type": "text", "text": "tool result"}]});
        resolve_tool_response(&ctx, "tool", Ok(response.clone()), &state)
            .expect("resolve tool promise");
        while ctx.execute_pending_job() {}

        assert_eq!(promise.state(), PromiseState::Resolved);
        let result = promise.result::<rquickjs::Value<'_>>().unwrap().unwrap();
        assert_eq!(
            js_value_to_json(&ctx, result, &termination),
            Ok(Some(response))
        );
    });
}

#[test]
fn tool_error_rejects_the_matching_promise() {
    let runtime = Runtime::new().expect("create runtime");
    let context = Context::full(&runtime).expect("create context");
    context.with(|ctx| {
        let (event_tx, _event_rx) = mpsc::unbounded_channel();
        let (runtime_command_tx, _runtime_command_rx) = std_mpsc::channel();
        let state = Rc::new(RefCell::new(RuntimeState {
            event_tx,
            pending_tool_calls: HashMap::new(),
            pending_timeouts: HashMap::new(),
            stored_values: HashMap::new(),
            stored_value_writes: HashMap::new(),
            enabled_tools: Vec::new(),
            next_tool_call_id: 1,
            next_timeout_id: 1,
            tool_call_id: "cell".to_string(),
            runtime_command_tx,
            termination: RuntimeTermination::new(),
            exit_requested: false,
        }));

        let (promise, resolve, reject) = ctx.promise().expect("create tool promise");
        state
            .borrow_mut()
            .pending_tool_calls
            .insert("tool".to_string(), PendingToolCall { resolve, reject });
        resolve_tool_response(&ctx, "tool", Err("tool failed".to_string()), &state)
            .expect("reject tool promise");
        while ctx.execute_pending_job() {}

        assert_eq!(promise.state(), PromiseState::Rejected);
    });
}
