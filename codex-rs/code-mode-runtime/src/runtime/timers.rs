use std::cell::RefCell;
use std::rc::Rc;
use std::time::Duration;

use rquickjs::Coerced;
use rquickjs::Ctx;
use rquickjs::FromJs;
use rquickjs::Function;
use rquickjs::Result as JsResult;
use rquickjs::Value;
use tokio_util::task::AbortOnDropHandle;

use super::RuntimeCommand;
use super::RuntimeState;
use super::value::exception_text;
use super::value::throw_type_error;

pub(super) struct ScheduledTimeout<'js> {
    callback: Function<'js>,
    // Clearing the timeout or dropping the runtime also cancels its sleep.
    _task: AbortOnDropHandle<()>,
}

pub(super) fn schedule_timeout<'js>(
    ctx: &Ctx<'js>,
    args: Vec<Value<'js>>,
    state: Rc<RefCell<RuntimeState<'js>>>,
) -> JsResult<Option<u64>> {
    let Some(callback) = args.first().and_then(Value::as_function).cloned() else {
        return Err(throw_type_error(
            ctx,
            "setTimeout expects a function callback",
        ));
    };

    let delay_ms = match args.get(1) {
        Some(value) => {
            Coerced::<f64>::from_js(ctx, value.clone()).map(|value| normalize_delay_ms(value.0))?
        }
        None => 0,
    };

    let mut state = state.borrow_mut();
    if state.termination.is_terminated() {
        return Ok(None);
    }
    let timeout_id = state.next_timeout_id;
    state.next_timeout_id = state.next_timeout_id.saturating_add(1);
    let runtime_command_tx = state.runtime_command_tx.clone();
    let sleep = tokio::time::sleep(Duration::from_millis(delay_ms));
    let task = tokio::spawn(async move {
        sleep.await;
        let _ = runtime_command_tx.send(RuntimeCommand::TimeoutFired { id: timeout_id });
    });
    state.pending_timeouts.insert(
        timeout_id,
        ScheduledTimeout {
            callback,
            _task: AbortOnDropHandle::new(task),
        },
    );
    Ok(Some(timeout_id))
}

pub(super) fn clear_timeout<'js>(
    ctx: &Ctx<'js>,
    args: Vec<Value<'js>>,
    state: Rc<RefCell<RuntimeState<'js>>>,
) -> JsResult<()> {
    let Some(value) = args.first() else {
        return Ok(());
    };
    if value.is_null() || value.is_undefined() {
        return Ok(());
    }
    let timeout_id = Coerced::<f64>::from_js(ctx, value.clone())?.0;
    if !timeout_id.is_finite() || timeout_id <= 0.0 {
        return Ok(());
    }
    let timeout_id = timeout_id.trunc().min(u64::MAX as f64) as u64;
    let mut state = state.borrow_mut();
    if !state.termination.is_terminated() {
        state.pending_timeouts.remove(&timeout_id);
    }
    Ok(())
}

pub(super) fn invoke_timeout_callback<'js>(
    ctx: &Ctx<'js>,
    timeout_id: u64,
    state: &Rc<RefCell<RuntimeState<'js>>>,
) -> Result<(), String> {
    let callback = {
        let mut state = state.borrow_mut();
        state.pending_timeouts.remove(&timeout_id)
    };
    let Some(callback) = callback else {
        return Ok(());
    };
    callback
        .callback
        .call::<_, ()>(())
        .map_err(|error| exception_text(ctx, error))
}

fn normalize_delay_ms(delay_ms: f64) -> u64 {
    if !delay_ms.is_finite() || delay_ms <= 0.0 {
        0
    } else {
        delay_ms.trunc().min(u64::MAX as f64) as u64
    }
}
