//! Bounded per-runtime rejection tracking and script error capture.

use std::cell::RefCell;
use std::mem;
use std::rc::Rc;

use codex_plugin_host::protocol::ErrorCode;
use codex_plugin_host::protocol::MAX_SCRIPT_ERROR_BYTES;
use codex_plugin_host::protocol::PluginScriptError;
use rquickjs::Coerced;
use rquickjs::Ctx;
use rquickjs::FromJs;
use rquickjs::Persistent;
use rquickjs::Promise;
use rquickjs::Runtime;
use rquickjs::Value;

const MAX_REJECTIONS: usize = 32;
const UNKNOWN_SCRIPT_ERROR: &str = "unknown plugin script error";
const MODULE_LOAD_ERROR: &str = "could not load module";
const DYNAMIC_IMPORT_ERROR: &str = "Not supported";

pub(super) type FailuresRef = Rc<RefCell<Failures>>;

struct Rejected {
    promise: Persistent<Promise<'static>>,
    reason: Persistent<Value<'static>>,
}

#[derive(Default)]
pub(super) struct Failures {
    rejected: Vec<Rejected>,
    overflowed: bool,
    script_error: Option<PluginScriptError>,
}

pub(super) fn install(runtime: &Runtime) -> FailuresRef {
    let failures = Rc::new(RefCell::new(Failures::default()));
    let tracker_failures = Rc::clone(&failures);
    runtime.set_host_promise_rejection_tracker(Some(Box::new(
        move |ctx, promise, reason, is_handled| {
            let Some(promise) = promise.as_promise().cloned() else {
                return;
            };
            let mut failures = tracker_failures.borrow_mut();
            if is_handled {
                failures.rejected.retain(|entry| {
                    entry
                        .promise
                        .clone()
                        .restore(&ctx)
                        .map(|saved| saved != promise)
                        .unwrap_or(true)
                });
                return;
            }
            if failures.rejected.len() == MAX_REJECTIONS {
                failures.overflowed = true;
                return;
            }
            failures.rejected.push(Rejected {
                promise: Persistent::save(&ctx, promise),
                reason: Persistent::save(&ctx, reason),
            });
        },
    )));
    failures
}

pub(super) fn checkpoint(ctx: &Ctx<'_>, failures: &FailuresRef) -> Result<(), ErrorCode> {
    while ctx.execute_pending_job() {}

    let (first, overflowed) = {
        let mut failures = failures.borrow_mut();
        (failures.rejected.pop(), mem::take(&mut failures.overflowed))
    };
    if let Some(rejected) = first {
        if let Ok(reason) = rejected.reason.restore(ctx) {
            record_value(ctx, reason, failures);
        }
        return Err(ErrorCode::ExecutionFailed);
    }
    if overflowed {
        return Err(ErrorCode::QuotaExceeded);
    }
    Ok(())
}

pub(super) fn record_text(failures: &FailuresRef, message: String) {
    record_script_error(failures, script_error(message, None, None));
}

pub(super) fn record_value<'js>(ctx: &Ctx<'js>, reason: Value<'js>, failures: &FailuresRef) {
    let mut script_error = script_error_from_value(ctx, reason);
    if is_module_load_error(&script_error.message) {
        script_error.message = DYNAMIC_IMPORT_ERROR.to_string();
    }
    record_script_error(failures, script_error);
}

pub(super) fn record_caught(ctx: &Ctx<'_>, failures: &FailuresRef) {
    if ctx.has_exception() {
        record_value(ctx, ctx.catch(), failures);
    }
}

pub(super) fn record_caught_module_import(ctx: &Ctx<'_>, failures: &FailuresRef) {
    if ctx.has_exception() {
        let reason = ctx.catch();
        let mut script_error = script_error_from_value(ctx, reason);
        if is_module_load_error(&script_error.message) {
            script_error.message = "module imports are unavailable".to_string();
        }
        record_script_error(failures, script_error);
    }
}

fn record_script_error(failures: &FailuresRef, script_error: PluginScriptError) {
    failures.borrow_mut().script_error = Some(script_error);
}

fn script_error(message: String, line: Option<usize>, column: Option<usize>) -> PluginScriptError {
    let mut bytes = message.into_bytes();
    let truncated = bytes.len() > MAX_SCRIPT_ERROR_BYTES;
    if truncated {
        let mut boundary = MAX_SCRIPT_ERROR_BYTES;
        while !message_boundary(&bytes, boundary) {
            boundary -= 1;
        }
        bytes.truncate(boundary);
    }
    PluginScriptError {
        message: String::from_utf8_lossy(&bytes).into_owned(),
        line,
        column,
        truncated,
    }
}

fn script_error_from_value<'js>(ctx: &Ctx<'js>, reason: Value<'js>) -> PluginScriptError {
    let object = reason.as_object();
    let name = object.and_then(|object| object_string(ctx, object, "name"));
    let message = object.and_then(|object| object_string(ctx, object, "message"));
    let stack = object.and_then(|object| object_string(ctx, object, "stack"));

    let mut text = match (name.filter(|name| !name.is_empty()), message) {
        (Some(name), Some(message)) if !message.is_empty() => format!("{name}: {message}"),
        (Some(name), _) => name,
        (None, Some(message)) if !message.is_empty() => message,
        _ => Coerced::<String>::from_js(ctx, reason)
            .ok()
            .map(|text| text.0)
            .filter(|text| !text.is_empty())
            .unwrap_or_else(|| UNKNOWN_SCRIPT_ERROR.to_string()),
    };
    if let Some(stack) = stack.filter(|stack| !stack.is_empty()) {
        text.push('\n');
        text.push_str(&stack);
    }
    let (line, column) = script_location(&text);
    script_error(text, line, column)
}

fn object_string<'js>(ctx: &Ctx<'js>, object: &rquickjs::Object<'js>, key: &str) -> Option<String> {
    let value = match object.get::<_, Value<'_>>(key) {
        Ok(value) => value,
        Err(_) => {
            if ctx.has_exception() {
                ctx.catch();
            }
            return None;
        }
    };
    Coerced::<String>::from_js(ctx, value)
        .ok()
        .map(|text| text.0)
}

fn is_module_load_error(message: &str) -> bool {
    message.contains(MODULE_LOAD_ERROR)
}

fn script_location(text: &str) -> (Option<usize>, Option<usize>) {
    let marker = "plugin.mjs:";
    let Some(marker_start) = text.find(marker) else {
        return (None, None);
    };
    let location = &text[marker_start + marker.len()..];
    let line_end = location
        .find(|character: char| !character.is_ascii_digit())
        .unwrap_or(location.len());
    let Ok(line) = location[..line_end].parse::<usize>() else {
        return (None, None);
    };
    if location.as_bytes().get(line_end) != Some(&b':') {
        return (Some(line), None);
    }
    let column_start = line_end + 1;
    let column_end = location[column_start..]
        .find(|character: char| !character.is_ascii_digit())
        .map(|offset| column_start + offset)
        .unwrap_or(location.len());
    let column = location[column_start..column_end].parse::<usize>().ok();
    (Some(line), column)
}

fn message_boundary(bytes: &[u8], boundary: usize) -> bool {
    boundary == bytes.len() || std::str::from_utf8(&bytes[..boundary]).is_ok()
}

pub(super) fn take(failures: &FailuresRef) -> Option<PluginScriptError> {
    failures.borrow_mut().script_error.take()
}
