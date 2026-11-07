use std::cell::RefCell;
use std::rc::Rc;

use rquickjs::Ctx;
use rquickjs::Module;
use rquickjs::Promise;
use rquickjs::Value;
use rquickjs::promise::PromiseState;
use serde_json::Value as JsonValue;

use super::CompletionState;
use super::RuntimeState;
use super::RuntimeTermination;
use super::value::exception_text;
use super::value::json_to_js;
use super::value::value_to_error_text;

pub(super) fn evaluate_main_module<'js>(
    ctx: &Ctx<'js>,
    source_text: &str,
) -> Result<Promise<'js>, String> {
    let module = Module::declare(ctx.clone(), "exec_main.mjs", source_text.to_string())
        .map_err(|error| exception_text(ctx, error))?;
    let (_, promise) = module.eval().map_err(|error| exception_text(ctx, error))?;
    Ok(promise)
}

pub(super) fn resolve_tool_response<'js>(
    ctx: &Ctx<'js>,
    id: &str,
    response: Result<JsonValue, String>,
    state: &Rc<RefCell<RuntimeState<'js>>>,
) -> Result<(), String> {
    let pending = {
        let mut state = state.borrow_mut();
        state.pending_tool_calls.remove(id)
    }
    .ok_or_else(|| format!("unknown tool call `{id}`"))?;

    match response {
        Ok(result) => {
            let value = json_to_js(ctx, &result).map_err(|error| exception_text(ctx, error))?;
            pending
                .resolve
                .call::<_, ()>((value,))
                .map_err(|error| exception_text(ctx, error))?;
        }
        Err(error_text) => {
            let value = rquickjs::String::from_str(ctx.clone(), &error_text)
                .map_err(|error| exception_text(ctx, error))?
                .into_value();
            pending
                .reject
                .call::<_, ()>((value,))
                .map_err(|error| exception_text(ctx, error))?;
        }
    }
    Ok(())
}

pub(super) fn completion_state<'js>(
    ctx: &Ctx<'js>,
    pending_promise: &Promise<'js>,
    termination: &RuntimeTermination,
) -> CompletionState {
    if termination.is_terminated() {
        return CompletionState::Completed { error_text: None };
    }

    match pending_promise.state() {
        PromiseState::Pending => CompletionState::Pending,
        PromiseState::Resolved => CompletionState::Completed { error_text: None },
        PromiseState::Rejected => {
            let reason = match pending_promise.result::<Value<'js>>() {
                Some(Ok(value)) => value,
                Some(Err(_)) => ctx.catch(),
                None => return CompletionState::Pending,
            };
            CompletionState::Completed {
                error_text: Some(value_to_error_text(ctx, reason, termination)),
            }
        }
    }
}

#[cfg(test)]
#[path = "module_loader_tests.rs"]
mod tests;
