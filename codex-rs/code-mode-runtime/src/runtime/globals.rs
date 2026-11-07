use std::cell::RefCell;
use std::rc::Rc;

use codex_code_mode_protocol::EnabledToolMetadata;
use rquickjs::Array;
use rquickjs::Ctx;
use rquickjs::Function;
use rquickjs::Object;
use rquickjs::Result as JsResult;
use rquickjs::Value;
use rquickjs::function::Rest;

use super::RuntimeState;
use super::callbacks::audio_callback;
use super::callbacks::clear_timeout_callback;
use super::callbacks::exit_callback;
use super::callbacks::generated_image_callback;
use super::callbacks::image_callback;
use super::callbacks::load_callback;
use super::callbacks::notify_callback;
use super::callbacks::set_timeout_callback;
use super::callbacks::store_callback;
use super::callbacks::text_callback;
use super::callbacks::tool_callback;
use super::callbacks::yield_control_callback;

pub(super) fn install_globals<'js>(
    ctx: &Ctx<'js>,
    enabled_tools: &[EnabledToolMetadata],
    state: Rc<RefCell<RuntimeState<'js>>>,
) -> Result<(), String> {
    let global = ctx.globals();
    for name in ["console", "Atomics", "SharedArrayBuffer", "WebAssembly"] {
        global
            .remove(name)
            .map_err(|_| format!("failed to remove global `{name}`"))?;
    }

    let tools = Object::new(ctx.clone())
        .map_err(|error| format!("failed to create tools object: {error}"))?;
    for (tool_index, tool) in enabled_tools.iter().enumerate() {
        let callback_state = Rc::clone(&state);
        let function = Function::new(ctx.clone(), move |ctx: Ctx<'js>, args: Rest<Value<'js>>| {
            tool_callback(ctx, args, Rc::clone(&callback_state), tool_index)
        })
        .map_err(|error| format!("failed to create tool function: {error}"))?;
        tools
            .set(tool.global_name.as_str(), function)
            .map_err(|error| format!("failed to add tool `{}`: {error}", tool.global_name))?;
    }

    let all_tools =
        Array::new(ctx.clone()).map_err(|error| format!("failed to create ALL_TOOLS: {error}"))?;
    for (index, tool) in enabled_tools.iter().enumerate() {
        let item = Object::new(ctx.clone())
            .map_err(|error| format!("failed to create ALL_TOOLS item: {error}"))?;
        item.set("name", tool.global_name.as_str())
            .map_err(|error| format!("failed to set ALL_TOOLS name: {error}"))?;
        item.set("description", tool.description.as_str())
            .map_err(|error| format!("failed to set ALL_TOOLS description: {error}"))?;
        all_tools
            .set(index, item)
            .map_err(|error| format!("failed to append ALL_TOOLS metadata: {error}"))?;
    }

    install_helper(ctx, &state, "clearTimeout", clear_timeout_callback)?;
    install_helper(ctx, &state, "setTimeout", set_timeout_callback)?;
    install_helper(ctx, &state, "text", text_callback)?;
    install_helper(ctx, &state, "image", image_callback)?;
    install_helper(ctx, &state, "audio", audio_callback)?;
    install_helper(ctx, &state, "generatedImage", generated_image_callback)?;
    install_helper(ctx, &state, "store", store_callback)?;
    install_helper(ctx, &state, "load", load_callback)?;
    install_helper(ctx, &state, "notify", notify_callback)?;
    install_helper(ctx, &state, "yield_control", yield_control_callback)?;
    install_helper(ctx, &state, "exit", exit_callback)?;

    global
        .set("tools", tools)
        .map_err(|error| format!("failed to set tools: {error}"))?;
    global
        .set("ALL_TOOLS", all_tools)
        .map_err(|error| format!("failed to set ALL_TOOLS: {error}"))?;

    ctx.eval::<(), _>(include_str!("settled.js"))
        .map_err(|error| format!("failed to initialize settlement helpers: {error}"))?;
    Ok(())
}

fn install_helper<'js, F>(
    ctx: &Ctx<'js>,
    state: &Rc<RefCell<RuntimeState<'js>>>,
    name: &str,
    callback: F,
) -> Result<(), String>
where
    F: for<'a> Fn(Ctx<'a>, Rest<Value<'a>>, Rc<RefCell<RuntimeState<'a>>>) -> JsResult<Value<'a>>
        + 'js,
{
    let callback_state = Rc::clone(state);
    let function = Function::new(ctx.clone(), move |ctx: Ctx<'js>, args: Rest<Value<'js>>| {
        callback(ctx, args, Rc::clone(&callback_state))
    })
    .map_err(|error| format!("failed to create helper `{name}`: {error}"))?;
    ctx.globals()
        .set(name, function)
        .map_err(|error| format!("failed to set global `{name}`: {error}"))
}
