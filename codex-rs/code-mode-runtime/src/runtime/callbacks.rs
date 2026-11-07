use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;

use codex_code_mode_protocol::FunctionCallOutputContentItem;
use rquickjs::Ctx;
use rquickjs::Result as JsResult;
use rquickjs::Value;
use rquickjs::function::Rest;

use super::PendingToolCall;
use super::RuntimeEvent;
use super::RuntimeState;
use super::timers;
use super::value::js_value_to_json;
use super::value::json_to_js;
use super::value::normalize_output_audio;
use super::value::normalize_output_image;
use super::value::serialize_output_text;
use super::value::throw_type_error;
use super::value::throw_uncatchable;

pub(super) fn tool_callback<'js>(
    ctx: Ctx<'js>,
    args: Rest<Value<'js>>,
    state: Rc<RefCell<RuntimeState<'js>>>,
    tool_index: usize,
) -> JsResult<Value<'js>> {
    let input = match args.0.first() {
        Some(value) => {
            let termination = state.borrow().termination.clone();
            js_value_to_json(&ctx, value.clone(), &termination)
                .map_err(|message| throw_type_error(&ctx, &message))?
        }
        None => None,
    };

    let (tool_name, tool_kind) = {
        let state = state.borrow();
        let tool = state
            .enabled_tools
            .get(tool_index)
            .ok_or_else(|| throw_type_error(&ctx, "tool callback data is out of range"))?;
        (tool.tool_name.clone(), tool.kind)
    };

    let (promise, resolve, reject) = ctx.promise()?;
    let id = {
        let mut state = state.borrow_mut();
        let id = format!("tool-{}", state.next_tool_call_id);
        state.next_tool_call_id = state.next_tool_call_id.saturating_add(1);
        state
            .pending_tool_calls
            .insert(id.clone(), PendingToolCall { resolve, reject });
        let _ = state.event_tx.send(RuntimeEvent::ToolCall {
            id: id.clone(),
            name: tool_name,
            kind: tool_kind,
            input,
        });
        id
    };
    let _ = id;
    Ok(promise.into_value())
}

pub(super) fn text_callback<'js>(
    ctx: Ctx<'js>,
    args: Rest<Value<'js>>,
    state: Rc<RefCell<RuntimeState<'js>>>,
) -> JsResult<Value<'js>> {
    let value = args
        .0
        .into_iter()
        .next()
        .unwrap_or_else(|| Value::new_undefined(ctx.clone()));
    let text = serialize_output_text(&ctx, value)?;
    let state = state.borrow();
    if !state.termination.is_terminated() {
        let _ = state.event_tx.send(RuntimeEvent::ContentItem(
            FunctionCallOutputContentItem::InputText { text },
        ));
    }
    Ok(Value::new_undefined(ctx.clone()))
}

pub(super) fn audio_callback<'js>(
    ctx: Ctx<'js>,
    args: Rest<Value<'js>>,
    state: Rc<RefCell<RuntimeState<'js>>>,
) -> JsResult<Value<'js>> {
    let value = args
        .0
        .into_iter()
        .next()
        .unwrap_or_else(|| Value::new_undefined(ctx.clone()));
    let termination = state.borrow().termination.clone();
    if let Some(audio_item) = normalize_output_audio(&ctx, value, &termination)? {
        let state = state.borrow();
        if !state.termination.is_terminated() {
            let _ = state.event_tx.send(RuntimeEvent::ContentItem(audio_item));
        }
    }
    Ok(Value::new_undefined(ctx.clone()))
}

pub(super) fn image_callback<'js>(
    ctx: Ctx<'js>,
    args: Rest<Value<'js>>,
    state: Rc<RefCell<RuntimeState<'js>>>,
) -> JsResult<Value<'js>> {
    let mut args = args.0.into_iter();
    let value = args
        .next()
        .unwrap_or_else(|| Value::new_undefined(ctx.clone()));
    let detail_override =
        match args.next() {
            Some(detail) if detail.is_string() => Some(detail.get::<String>().map_err(|_| {
                throw_type_error(&ctx, "image detail must be a string when provided")
            })?),
            Some(detail) if detail.is_null() || detail.is_undefined() => None,
            Some(_) => {
                return Err(throw_type_error(
                    &ctx,
                    "image detail must be a string when provided",
                ));
            }
            None => None,
        };
    let termination = state.borrow().termination.clone();
    if let Some(image_item) = normalize_output_image(&ctx, value, detail_override, &termination)? {
        let state = state.borrow();
        if !state.termination.is_terminated() {
            let _ = state.event_tx.send(RuntimeEvent::ContentItem(image_item));
        }
    }
    Ok(Value::new_undefined(ctx.clone()))
}

pub(super) fn generated_image_callback<'js>(
    ctx: Ctx<'js>,
    args: Rest<Value<'js>>,
    state: Rc<RefCell<RuntimeState<'js>>>,
) -> JsResult<Value<'js>> {
    let value = args
        .0
        .into_iter()
        .next()
        .unwrap_or_else(|| Value::new_undefined(ctx.clone()));
    let output_hint = generated_image_output_hint(&ctx, value.clone())?;
    let termination = state.borrow().termination.clone();
    if let Some(image_item) =
        normalize_output_image(&ctx, value, /*detail_override*/ None, &termination)?
    {
        let state = state.borrow();
        if !state.termination.is_terminated() {
            let _ = state.event_tx.send(RuntimeEvent::ContentItem(image_item));
            if let Some(text) = output_hint {
                let _ = state.event_tx.send(RuntimeEvent::ContentItem(
                    FunctionCallOutputContentItem::InputText { text },
                ));
            }
        }
    }
    Ok(Value::new_undefined(ctx.clone()))
}

fn generated_image_output_hint(ctx: &Ctx<'_>, value: Value<'_>) -> JsResult<Option<String>> {
    let Some(object) = value.as_object().cloned() else {
        return Err(throw_type_error(
            ctx,
            "generatedImage expects an image generation result object",
        ));
    };
    let output_hint: Value<'_> = object.get("output_hint")?;
    if output_hint.is_undefined() {
        return Ok(None);
    }
    if !output_hint.is_string() {
        return Err(throw_type_error(
            ctx,
            "generatedImage output_hint must be a string when provided",
        ));
    }
    output_hint
        .get::<String>()
        .map(Some)
        .map_err(|_| throw_type_error(ctx, "generatedImage output_hint must be a string"))
}

pub(super) fn store_callback<'js>(
    ctx: Ctx<'js>,
    args: Rest<Value<'js>>,
    state: Rc<RefCell<RuntimeState<'js>>>,
) -> JsResult<Value<'js>> {
    let mut args = args.0.into_iter();
    let Some(key) = args.next() else {
        return Err(throw_type_error(&ctx, "store key must be a string"));
    };
    let key = key
        .get::<rquickjs::Coerced<String>>()
        .map_err(|_| throw_type_error(&ctx, "store key must be a string"))?
        .0;
    let value = args
        .next()
        .unwrap_or_else(|| Value::new_undefined(ctx.clone()));
    let termination = state.borrow().termination.clone();
    let serialized = match js_value_to_json(&ctx, value, &termination) {
        Ok(Some(value)) => value,
        Ok(None) => {
            return Err(throw_type_error(
                &ctx,
                &format!("Unable to store {key:?}. Only plain serializable objects can be stored."),
            ));
        }
        Err(message) => return Err(throw_type_error(&ctx, &message)),
    };
    let mut state = state.borrow_mut();
    if !state.termination.is_terminated() {
        let serialized = Arc::new(serialized);
        state
            .stored_values
            .insert(key.clone(), Arc::clone(&serialized));
        state.stored_value_writes.insert(key, serialized);
    }
    Ok(Value::new_undefined(ctx.clone()))
}

pub(super) fn load_callback<'js>(
    ctx: Ctx<'js>,
    args: Rest<Value<'js>>,
    state: Rc<RefCell<RuntimeState<'js>>>,
) -> JsResult<Value<'js>> {
    let Some(key) = args.0.first() else {
        return Err(throw_type_error(&ctx, "load key must be a string"));
    };
    let key = key
        .get::<rquickjs::Coerced<String>>()
        .map_err(|_| throw_type_error(&ctx, "load key must be a string"))?
        .0;
    let value = state.borrow().stored_values.get(&key).cloned();
    let Some(value) = value else {
        return Ok(Value::new_undefined(ctx.clone()));
    };
    json_to_js(&ctx, &value)
}

pub(super) fn notify_callback<'js>(
    ctx: Ctx<'js>,
    args: Rest<Value<'js>>,
    state: Rc<RefCell<RuntimeState<'js>>>,
) -> JsResult<Value<'js>> {
    let value = args
        .0
        .into_iter()
        .next()
        .unwrap_or_else(|| Value::new_undefined(ctx.clone()));
    let text = serialize_output_text(&ctx, value)?;
    if text.trim().is_empty() {
        return Err(throw_type_error(&ctx, "notify expects non-empty text"));
    }
    let state = state.borrow();
    if !state.termination.is_terminated() {
        let _ = state.event_tx.send(RuntimeEvent::Notify {
            call_id: state.tool_call_id.clone(),
            text,
        });
    }
    Ok(Value::new_undefined(ctx.clone()))
}

pub(super) fn set_timeout_callback<'js>(
    ctx: Ctx<'js>,
    args: Rest<Value<'js>>,
    state: Rc<RefCell<RuntimeState<'js>>>,
) -> JsResult<Value<'js>> {
    let timeout_id = timers::schedule_timeout(&ctx, args.0, state)?;
    Ok(Value::new_number(
        ctx.clone(),
        timeout_id.unwrap_or_default() as f64,
    ))
}

pub(super) fn clear_timeout_callback<'js>(
    ctx: Ctx<'js>,
    args: Rest<Value<'js>>,
    state: Rc<RefCell<RuntimeState<'js>>>,
) -> JsResult<Value<'js>> {
    timers::clear_timeout(&ctx, args.0, state)?;
    Ok(Value::new_undefined(ctx.clone()))
}

pub(super) fn yield_control_callback<'js>(
    ctx: Ctx<'js>,
    _args: Rest<Value<'js>>,
    state: Rc<RefCell<RuntimeState<'js>>>,
) -> JsResult<Value<'js>> {
    let state = state.borrow();
    if !state.termination.is_terminated() {
        let _ = state.event_tx.send(RuntimeEvent::YieldRequested);
    }
    Ok(Value::new_undefined(ctx.clone()))
}

pub(super) fn exit_callback<'js>(
    ctx: Ctx<'js>,
    _args: Rest<Value<'js>>,
    state: Rc<RefCell<RuntimeState<'js>>>,
) -> JsResult<Value<'js>> {
    let mut state = state.borrow_mut();
    state.exit_requested = true;
    state.termination.terminate();
    Err(throw_uncatchable(&ctx, "JavaScript execution terminated"))
}

#[cfg(test)]
#[path = "callbacks_tests.rs"]
mod tests;
