//! Persistent module state with explicit microtask pumping and no dynamic executable loading.

use std::collections::HashMap;
use std::io;

use codex_plugin_host::protocol::*;
use rquickjs::Context;
use rquickjs::Ctx;
use rquickjs::Error as JsError;
use rquickjs::Module;
use rquickjs::Object;
use rquickjs::Persistent;
use rquickjs::Promise;
use rquickjs::Runtime;
use rquickjs::Value;
use rquickjs::promise::PromiseState;

use super::failures;
use super::transport::Transport;
use crate::runtime::RuntimeTermination;

pub(super) struct Engine {
    context: Context,
    namespace: Option<Persistent<Object<'static>>>,
    pending_promises: HashMap<u64, Persistent<Promise<'static>>>,
    pub(super) transport: Transport,
    failures: failures::FailuresRef,
    termination: RuntimeTermination,
    _runtime: Runtime,
}

impl Engine {
    /// Construct a QuickJS runtime; user code runs only in watchdog-protected turns.
    pub(super) fn new(transport: Transport) -> io::Result<Self> {
        let runtime = Runtime::new().map_err(io::Error::other)?;
        runtime.set_memory_limit(64 * 1024 * 1024);
        let termination = RuntimeTermination::new();
        let interrupt_termination = termination.clone();
        runtime.set_interrupt_handler(Some(Box::new(move || {
            interrupt_termination.is_terminated()
        })));
        let failures = failures::install(&runtime);
        let context = Context::full(&runtime).map_err(io::Error::other)?;
        context.with(|ctx| restrict_globals(&ctx))?;
        Ok(Self {
            context,
            namespace: None,
            pending_promises: HashMap::new(),
            transport,
            failures,
            termination,
            _runtime: runtime,
        })
    }

    pub(super) fn handle(&self) -> RuntimeTermination {
        self.termination.clone()
    }

    pub(super) fn turn(
        &mut self,
        message: HostMessage,
    ) -> Result<Vec<RuntimeMessage>, RuntimeMessage> {
        let context = self.context.clone();
        let result = context.with(|ctx| match message {
            HostMessage::Initialize { source } => self
                .load(&ctx, &source)
                .map(|()| vec![RuntimeMessage::Initialized]),
            HostMessage::Dispatch {
                id,
                export,
                return_mode,
            } => self.dispatch(&ctx, id, &export, return_mode),
            HostMessage::Close => Err(ErrorCode::InvalidProgram),
        });
        let script_error = failures::take(&self.failures);
        result.map_err(|code| RuntimeMessage::Error { code, script_error })
    }

    fn load(&mut self, ctx: &Ctx<'_>, source: &str) -> Result<(), ErrorCode> {
        if self.namespace.is_some() || source.len() > MAX_SOURCE {
            return Err(ErrorCode::InvalidProgram);
        }
        let module = match Module::declare(ctx.clone(), "plugin.mjs", source.to_string()) {
            Ok(module) => module,
            Err(error) => return Err(record_static_module_error(ctx, &self.failures, error)),
        };
        let (module, evaluation) = match module.eval() {
            Ok(evaluation) => evaluation,
            Err(error) => {
                return Err(record_js_error(
                    ctx,
                    &self.failures,
                    error,
                    ErrorCode::ExecutionFailed,
                ));
            }
        };
        failures::checkpoint(ctx, &self.failures)?;
        if evaluation.state() != PromiseState::Resolved {
            record_promise_rejection(ctx, &self.failures, evaluation);
            return Err(ErrorCode::ExecutionFailed);
        }
        let namespace = module.namespace().map_err(|error| {
            record_js_error(ctx, &self.failures, error, ErrorCode::InvalidProgram)
        })?;
        self.namespace = Some(Persistent::save(ctx, namespace));
        Ok(())
    }

    /// One JS turn; unresolved invocations yield back to the IPC dispatcher.
    fn dispatch(
        &mut self,
        ctx: &Ctx<'_>,
        id: u64,
        export: &str,
        return_mode: ReturnMode,
    ) -> Result<Vec<RuntimeMessage>, ErrorCode> {
        if export.is_empty() || export.len() > MAX_METHOD || self.pending_promises.contains_key(&id)
        {
            return Err(ErrorCode::QuotaExceeded);
        }
        let namespace = self
            .namespace
            .as_ref()
            .ok_or(ErrorCode::InvalidProgram)?
            .clone()
            .restore(ctx)
            .map_err(|_| ErrorCode::InvalidProgram)?;
        let function: Value<'_> = namespace.get(export).map_err(|error| {
            record_js_error(ctx, &self.failures, error, ErrorCode::ExecutionFailed)
        })?;
        let function = function
            .as_function()
            .cloned()
            .ok_or(ErrorCode::InvalidProgram)?;
        let result: Value<'_> = function.call(()).map_err(|error| {
            record_js_error(ctx, &self.failures, error, ErrorCode::ExecutionFailed)
        })?;

        let mut replies = Vec::new();
        if return_mode == ReturnMode::MaybeAsync && result.is_promise() {
            if self.pending_promises.len() >= MAX_PENDING_PROMISES {
                return Err(ErrorCode::QuotaExceeded);
            }
            let promise = result
                .as_promise()
                .cloned()
                .ok_or(ErrorCode::InvalidResult)?;
            self.pending_promises
                .insert(id, Persistent::save(ctx, promise));
        } else {
            replies.push(RuntimeMessage::Reply {
                id,
                value: result_string(result)?,
            });
        }

        failures::checkpoint(ctx, &self.failures)?;
        let completed: Vec<u64> = self
            .pending_promises
            .iter()
            .filter_map(|(id, promise)| {
                let promise = promise.clone().restore(ctx).ok()?;
                (promise.state() != PromiseState::Pending).then_some(*id)
            })
            .collect();
        for completed_id in completed {
            let promise = self
                .pending_promises
                .remove(&completed_id)
                .ok_or(ErrorCode::InvalidProgram)?
                .restore(ctx)
                .map_err(|_| ErrorCode::InvalidProgram)?;
            if promise.state() != PromiseState::Resolved {
                record_promise_rejection(ctx, &self.failures, promise);
                return Err(ErrorCode::ExecutionFailed);
            }
            let value = promise
                .result::<Value<'_>>()
                .ok_or(ErrorCode::InvalidResult)?
                .map_err(|error| {
                    record_js_error(ctx, &self.failures, error, ErrorCode::ExecutionFailed)
                })?;
            replies.push(RuntimeMessage::Reply {
                id: completed_id,
                value: result_string(value)?,
            });
        }
        if self.pending_promises.contains_key(&id) {
            replies.push(RuntimeMessage::Pending { id });
        }
        Ok(replies)
    }
}

fn restrict_globals(ctx: &Ctx<'_>) -> io::Result<()> {
    let global = ctx.globals();
    for name in [
        "console",
        "eval",
        "ArrayBuffer",
        "SharedArrayBuffer",
        "Atomics",
        "WebAssembly",
        "DataView",
        "Int8Array",
        "Uint8Array",
        "Uint8ClampedArray",
        "Int16Array",
        "Uint16Array",
        "Int32Array",
        "Uint32Array",
        "Float16Array",
        "Float32Array",
        "Float64Array",
        "BigInt64Array",
        "BigUint64Array",
    ] {
        global
            .remove(name)
            .map_err(|_| io::Error::other("failed to restrict UI globals"))?;
    }
    ctx.eval::<(), _>(
        r#"
            const deny = () => {
                throw new EvalError("code generation from strings is disallowed");
            };
            const intrinsicFunction = Function;
            const denyConstructor = function Function() {
                deny();
            };
            globalThis.eval = deny;
            globalThis.Function = denyConstructor;
            for (const prototype of [
                intrinsicFunction.prototype,
                Object.getPrototypeOf(async function () {}),
                Object.getPrototypeOf(function* () {}),
                Object.getPrototypeOf(async function* () {}),
            ]) {
                Object.defineProperty(prototype, "constructor", {
                    value: denyConstructor,
                    writable: false,
                    configurable: false,
                });
            }
        "#,
    )
    .map_err(io::Error::other)?;
    Ok(())
}

fn record_js_error(
    ctx: &Ctx<'_>,
    failures: &failures::FailuresRef,
    error: JsError,
    code: ErrorCode,
) -> ErrorCode {
    match error {
        JsError::Exception => failures::record_caught(ctx, failures),
        error => failures::record_text(failures, error.to_string()),
    }
    code
}

fn record_static_module_error(
    ctx: &Ctx<'_>,
    failures: &failures::FailuresRef,
    error: JsError,
) -> ErrorCode {
    match error {
        JsError::Exception => failures::record_caught_module_import(ctx, failures),
        error => failures::record_text(failures, error.to_string()),
    }
    ErrorCode::InvalidProgram
}

fn record_promise_rejection<'js>(
    ctx: &Ctx<'js>,
    failures: &failures::FailuresRef,
    promise: Promise<'js>,
) {
    let reason = match promise.result::<Value<'_>>() {
        Some(Ok(value)) => value,
        Some(Err(_)) => ctx.catch(),
        None => Value::new_undefined(ctx.clone()),
    };
    failures::record_value(ctx, reason, failures);
}

fn result_string(result: Value<'_>) -> Result<String, ErrorCode> {
    let value = result.as_string().ok_or(ErrorCode::InvalidResult)?;
    let value = value.to_string().map_err(|_| ErrorCode::InvalidResult)?;
    if value.len() > MAX_RESULT {
        return Err(ErrorCode::QuotaExceeded);
    }
    Ok(value)
}
