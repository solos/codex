use std::ffi::CString;

use rquickjs::Coerced;
use rquickjs::Ctx;
use rquickjs::Error as JsError;
use rquickjs::Exception;
use rquickjs::FromJs;
use rquickjs::Object;
use rquickjs::Result as JsResult;
use rquickjs::Value;
use rquickjs::qjs;
use serde_json::Value as JsonValue;

use codex_code_mode_protocol::DEFAULT_IMAGE_DETAIL;
use codex_code_mode_protocol::FunctionCallOutputContentItem;
use codex_code_mode_protocol::ImageDetail;

use super::RuntimeTermination;
use super::audio::wav_duration_seconds;

const IMAGE_HELPER_EXPECTS_MESSAGE: &str = "image expects a non-empty image URL string, an object with image_url and optional detail, or a raw MCP image block";
const AUDIO_HELPER_EXPECTS_MESSAGE: &str = "audio expects a non-empty audio URL string, an object with audio_url, or a raw MCP audio block";
const REMOTE_IMAGE_URL_ERROR: &str = "Tool call failed: remote image URLs are not supported in tool outputs. Pass a base64 data URI instead";
const INVALID_IMAGE_URL_ERROR: &str =
    "Tool call failed: invalid image output. Pass a base64 data URI instead";
const INVALID_AUDIO_URL_ERROR: &str =
    "Tool call failed: invalid audio output. Pass a base64 data URI instead";
const CODEX_IMAGE_DETAIL_META_KEY: &str = "codex/imageDetail";

pub(super) fn serialize_output_text<'js>(ctx: &Ctx<'js>, value: Value<'js>) -> JsResult<String> {
    if value.is_undefined()
        || value.is_null()
        || value.is_bool()
        || value.is_int()
        || value.is_float()
        || value.is_big_int()
        || value.is_string()
    {
        return Ok(Coerced::<String>::from_js(ctx, value)?.0);
    }

    match ctx.json_stringify(value.clone()) {
        Ok(Some(stringified)) => Ok(stringified.to_string()?),
        Ok(None) => Ok(Coerced::<String>::from_js(ctx, value)?.0),
        Err(error) => Err(exception_error(ctx, error)),
    }
}

pub(super) fn normalize_output_image<'js>(
    ctx: &Ctx<'js>,
    value: Value<'js>,
    detail_override: Option<String>,
    termination: &RuntimeTermination,
) -> JsResult<Option<FunctionCallOutputContentItem>> {
    match normalize_output_image_inner(ctx, value, detail_override, termination) {
        Ok(item) => Ok(Some(item)),
        Err(FormatFailure::Message(message)) => Err(throw_type_error(ctx, &message)),
        Err(FormatFailure::Js(error)) => Err(exception_error(ctx, error)),
    }
}

enum FormatFailure {
    Message(String),
    Js(JsError),
}

impl From<String> for FormatFailure {
    fn from(value: String) -> Self {
        Self::Message(value)
    }
}

impl From<JsError> for FormatFailure {
    fn from(value: JsError) -> Self {
        Self::Js(value)
    }
}

fn normalize_output_image_inner<'js>(
    ctx: &Ctx<'js>,
    value: Value<'js>,
    detail_override: Option<String>,
    termination: &RuntimeTermination,
) -> Result<FunctionCallOutputContentItem, FormatFailure> {
    let (image_url, detail) = if value.is_string() {
        (Coerced::<String>::from_js(ctx, value)?.0, None)
    } else if value.is_object() && !value.is_array() {
        let object = value
            .as_object()
            .ok_or_else(|| IMAGE_HELPER_EXPECTS_MESSAGE.to_string())?;
        if let Some(image) = parse_non_mcp_output_image(ctx, object, termination)? {
            image
        } else {
            parse_mcp_output_image(ctx, value)?
        }
    } else {
        return Err(IMAGE_HELPER_EXPECTS_MESSAGE.to_string().into());
    };

    if image_url.is_empty() {
        return Err(IMAGE_HELPER_EXPECTS_MESSAGE.to_string().into());
    }
    let Some((scheme, _)) = image_url.split_once(':') else {
        return Err(INVALID_IMAGE_URL_ERROR.to_string().into());
    };
    if scheme.eq_ignore_ascii_case("http") || scheme.eq_ignore_ascii_case("https") {
        return Err(REMOTE_IMAGE_URL_ERROR.to_string().into());
    }
    if !scheme.eq_ignore_ascii_case("data") {
        return Err(INVALID_IMAGE_URL_ERROR.to_string().into());
    }

    let detail = detail_override.or(detail);
    let detail = match detail {
        Some(detail) => Some(match detail.to_ascii_lowercase().as_str() {
            "auto" => ImageDetail::Auto,
            "low" => ImageDetail::Low,
            "high" => ImageDetail::High,
            "original" => ImageDetail::Original,
            _ => {
                return Err("image detail must be one of: auto, low, high, original"
                    .to_string()
                    .into());
            }
        }),
        None => Some(DEFAULT_IMAGE_DETAIL),
    };

    Ok(FunctionCallOutputContentItem::InputImage { image_url, detail })
}

fn parse_non_mcp_output_image<'js>(
    ctx: &Ctx<'js>,
    object: &Object<'js>,
    termination: &RuntimeTermination,
) -> Result<Option<(String, Option<String>)>, FormatFailure> {
    let image_url: Value<'_> = match object.get("image_url") {
        Ok(image_url) => image_url,
        Err(JsError::Exception) if !termination.is_terminated() => {
            let _ = ctx.catch();
            return Ok(None);
        }
        Err(error) => return Err(error.into()),
    };
    if image_url.is_undefined() {
        return Ok(None);
    }
    if !image_url.is_string() {
        return Err(IMAGE_HELPER_EXPECTS_MESSAGE.to_string().into());
    }
    let image_url = Coerced::<String>::from_js(ctx, image_url)?.0;
    let detail: Value<'_> = match object.get("detail") {
        Ok(detail) => detail,
        Err(JsError::Exception) if !termination.is_terminated() => {
            let _ = ctx.catch();
            return Ok(Some((image_url, None)));
        }
        Err(error) => return Err(error.into()),
    };
    let detail = parse_image_detail_value(ctx, Some(detail))?;
    Ok(Some((image_url, detail)))
}

fn parse_mcp_output_image<'js>(
    ctx: &Ctx<'js>,
    value: Value<'js>,
) -> Result<(String, Option<String>), FormatFailure> {
    let Some(result) = value_to_json(ctx, value)? else {
        return Err(IMAGE_HELPER_EXPECTS_MESSAGE.to_string().into());
    };
    let JsonValue::Object(result) = result else {
        return Err(IMAGE_HELPER_EXPECTS_MESSAGE.to_string().into());
    };
    let Some(item_type) = result.get("type").and_then(JsonValue::as_str) else {
        return Err(IMAGE_HELPER_EXPECTS_MESSAGE.to_string().into());
    };
    if item_type != "image" {
        return Err(format!("image only accepts MCP image blocks, got \"{item_type}\"").into());
    }
    let data = result
        .get("data")
        .and_then(JsonValue::as_str)
        .ok_or_else(|| "image expected MCP image data".to_string())?;
    if data.is_empty() {
        return Err("image expected MCP image data".to_string().into());
    }

    let image_url = if data.to_ascii_lowercase().starts_with("data:") {
        data.to_string()
    } else {
        let mime_type = result
            .get("mimeType")
            .or_else(|| result.get("mime_type"))
            .and_then(JsonValue::as_str)
            .filter(|mime_type| !mime_type.is_empty())
            .unwrap_or("application/octet-stream");
        format!("data:{mime_type};base64,{data}")
    };
    let detail = result
        .get("_meta")
        .and_then(JsonValue::as_object)
        .and_then(|meta| meta.get(CODEX_IMAGE_DETAIL_META_KEY))
        .and_then(JsonValue::as_str)
        .filter(|detail| matches!(*detail, "auto" | "low" | "high" | "original"))
        .map(str::to_string);
    Ok((image_url, detail))
}

fn parse_image_detail_value<'js>(
    ctx: &Ctx<'js>,
    value: Option<Value<'js>>,
) -> Result<Option<String>, FormatFailure> {
    match value {
        Some(value) if value.is_string() => Ok(Some(Coerced::<String>::from_js(ctx, value)?.0)),
        Some(value) if value.is_null() || value.is_undefined() => Ok(None),
        Some(_) => Err("image detail must be a string when provided"
            .to_string()
            .into()),
        None => Ok(None),
    }
}

pub(super) fn normalize_output_audio<'js>(
    ctx: &Ctx<'js>,
    value: Value<'js>,
    termination: &RuntimeTermination,
) -> JsResult<Option<FunctionCallOutputContentItem>> {
    match normalize_output_audio_inner(ctx, value, termination) {
        Ok(item) => Ok(Some(item)),
        Err(FormatFailure::Message(message)) => Err(throw_type_error(ctx, &message)),
        Err(FormatFailure::Js(error)) => Err(exception_error(ctx, error)),
    }
}

fn normalize_output_audio_inner<'js>(
    ctx: &Ctx<'js>,
    value: Value<'js>,
    termination: &RuntimeTermination,
) -> Result<FunctionCallOutputContentItem, FormatFailure> {
    let audio_url = if value.is_string() {
        Coerced::<String>::from_js(ctx, value)?.0
    } else if value.is_object() && !value.is_array() {
        let object = value
            .as_object()
            .ok_or_else(|| AUDIO_HELPER_EXPECTS_MESSAGE.to_string())?;
        if let Some(audio_url) = parse_non_mcp_output_audio(ctx, object, termination)? {
            audio_url
        } else {
            parse_mcp_output_audio(ctx, value)?
        }
    } else {
        return Err(AUDIO_HELPER_EXPECTS_MESSAGE.to_string().into());
    };

    if audio_url.is_empty() {
        return Err(AUDIO_HELPER_EXPECTS_MESSAGE.to_string().into());
    }
    let Some((scheme, _)) = audio_url.split_once(':') else {
        return Err(INVALID_AUDIO_URL_ERROR.to_string().into());
    };
    if !scheme.eq_ignore_ascii_case("data") {
        return Err(INVALID_AUDIO_URL_ERROR.to_string().into());
    }

    if wav_duration_seconds(&audio_url).is_some_and(|duration| duration < 0.025) {
        return Ok(FunctionCallOutputContentItem::InputText {
            text: "Audio output omitted because the clip is shorter than 25 ms; use a longer clip."
                .to_string(),
        });
    }

    Ok(FunctionCallOutputContentItem::InputAudio { audio_url })
}

fn parse_non_mcp_output_audio<'js>(
    ctx: &Ctx<'js>,
    object: &Object<'js>,
    termination: &RuntimeTermination,
) -> Result<Option<String>, FormatFailure> {
    let audio_url: Value<'_> = match object.get("audio_url") {
        Ok(audio_url) => audio_url,
        Err(JsError::Exception) if !termination.is_terminated() => {
            let _ = ctx.catch();
            return Ok(None);
        }
        Err(error) => return Err(error.into()),
    };
    if audio_url.is_undefined() {
        return Ok(None);
    }
    if !audio_url.is_string() {
        return Err(AUDIO_HELPER_EXPECTS_MESSAGE.to_string().into());
    }
    Ok(Some(Coerced::<String>::from_js(ctx, audio_url)?.0))
}

fn parse_mcp_output_audio<'js>(ctx: &Ctx<'js>, value: Value<'js>) -> Result<String, FormatFailure> {
    let Some(result) = value_to_json(ctx, value)? else {
        return Err(AUDIO_HELPER_EXPECTS_MESSAGE.to_string().into());
    };
    let JsonValue::Object(result) = result else {
        return Err(AUDIO_HELPER_EXPECTS_MESSAGE.to_string().into());
    };
    let Some(item_type) = result.get("type").and_then(JsonValue::as_str) else {
        return Err(AUDIO_HELPER_EXPECTS_MESSAGE.to_string().into());
    };
    if item_type != "audio" {
        return Err(format!("audio only accepts MCP audio blocks, got \"{item_type}\"").into());
    }
    let data = result
        .get("data")
        .and_then(JsonValue::as_str)
        .ok_or_else(|| "audio expected MCP audio data".to_string())?;
    if data.is_empty() {
        return Err("audio expected MCP audio data".to_string().into());
    }

    if data.to_ascii_lowercase().starts_with("data:") {
        Ok(data.to_string())
    } else {
        let mime_type = result
            .get("mimeType")
            .or_else(|| result.get("mime_type"))
            .and_then(JsonValue::as_str)
            .filter(|mime_type| !mime_type.is_empty())
            .unwrap_or("application/octet-stream");
        Ok(format!("data:{mime_type};base64,{data}"))
    }
}

pub(super) fn js_value_to_json<'js>(
    ctx: &Ctx<'js>,
    value: Value<'js>,
    termination: &RuntimeTermination,
) -> Result<Option<JsonValue>, String> {
    if termination.is_terminated() {
        return Err("JavaScript execution terminated".to_string());
    }
    if value.is_undefined() {
        return Ok(None);
    }
    value_to_json(ctx, value).map_err(|error| match error {
        FormatFailure::Message(message) => message,
        FormatFailure::Js(error) => exception_text(ctx, error),
    })
}

fn value_to_json<'js>(
    ctx: &Ctx<'js>,
    value: Value<'js>,
) -> Result<Option<JsonValue>, FormatFailure> {
    let Some(stringified) = ctx.json_stringify(value)? else {
        return Ok(None);
    };
    let json = stringified.to_string()?;
    if [
        r#"{"$serde_json::private::RawValue":"#,
        r#","$serde_json::private::RawValue":"#,
        r#"{"$serde_json::private::Number":"#,
        r#","$serde_json::private::Number":"#,
    ]
    .into_iter()
    .any(|key| json.contains(key))
    {
        return Err(
            "failed to serialize JavaScript value: reserved JSON object key"
                .to_string()
                .into(),
        );
    }
    serde_json::from_str(&json)
        .map(Some)
        .map_err(|error| format!("failed to serialize JavaScript value: {error}").into())
}

pub(super) fn json_to_js<'js>(ctx: &Ctx<'js>, value: &JsonValue) -> JsResult<Value<'js>> {
    let json = serde_json::to_string(value)
        .map_err(|error| Exception::throw_message(ctx, &error.to_string()))?;
    ctx.json_parse(json)
}

pub(super) fn value_to_error_text<'js>(
    ctx: &Ctx<'js>,
    value: Value<'js>,
    termination: &RuntimeTermination,
) -> String {
    if let Some(object) = value.as_object() {
        let stack = object
            .get::<_, Value<'_>>("stack")
            .ok()
            .filter(Value::is_string)
            .and_then(|stack| Coerced::<String>::from_js(ctx, stack).ok())
            .map(|stack| stack.0);
        if termination.is_terminated() {
            return String::new();
        }
        let message = object
            .get::<_, Value<'_>>("message")
            .ok()
            .filter(Value::is_string)
            .and_then(|message| Coerced::<String>::from_js(ctx, message).ok())
            .map(|message| message.0);
        if termination.is_terminated() {
            return String::new();
        }
        if let Some(message) = message.filter(|message| !message.is_empty()) {
            let name = object
                .get::<_, Value<'_>>("name")
                .ok()
                .filter(Value::is_string)
                .and_then(|name| Coerced::<String>::from_js(ctx, name).ok())
                .map(|name| name.0)
                .filter(|name| !name.is_empty());
            if termination.is_terminated() {
                return String::new();
            }
            let message = normalize_json_error_message(&message);
            let header = name
                .map(|name| format!("{name}: {message}"))
                .unwrap_or(message);
            return match stack.filter(|stack| !stack.is_empty()) {
                Some(stack) => format!("{header}\n{stack}"),
                None => header,
            };
        }
        if let Some(stack) = stack.filter(|stack| !stack.is_empty()) {
            return stack;
        }
    }
    if termination.is_terminated() {
        return String::new();
    }
    Coerced::<String>::from_js(ctx, value)
        .map(|value| value.0)
        .unwrap_or_else(|_| "unknown code mode exception".to_string())
}

pub(super) fn exception_text(ctx: &Ctx<'_>, error: JsError) -> String {
    match error {
        JsError::Exception => caught_value_text(ctx, ctx.catch()),
        error => error.to_string(),
    }
}

fn caught_value_text<'js>(ctx: &Ctx<'js>, value: Value<'js>) -> String {
    if let Some(object) = value.as_object() {
        let stack = object
            .get::<_, Value<'_>>("stack")
            .ok()
            .filter(Value::is_string)
            .and_then(|stack| Coerced::<String>::from_js(ctx, stack).ok())
            .map(|stack| stack.0);
        let message = object
            .get::<_, Value<'_>>("message")
            .ok()
            .filter(Value::is_string)
            .and_then(|message| Coerced::<String>::from_js(ctx, message).ok())
            .map(|message| message.0)
            .filter(|message| !message.is_empty());
        if let Some(message) = message {
            let name = object
                .get::<_, Value<'_>>("name")
                .ok()
                .filter(Value::is_string)
                .and_then(|name| Coerced::<String>::from_js(ctx, name).ok())
                .map(|name| name.0)
                .filter(|name| !name.is_empty());
            let message = normalize_json_error_message(&message);
            let header = name
                .map(|name| format!("{name}: {message}"))
                .unwrap_or(message);
            return match stack.filter(|stack| !stack.is_empty()) {
                Some(stack) => format!("{header}\n{stack}"),
                None => header,
            };
        }
        if let Some(stack) = stack.filter(|stack| !stack.is_empty()) {
            return stack;
        }
    }
    Coerced::<String>::from_js(ctx, value)
        .map(|value| value.0)
        .unwrap_or_else(|_| "unknown code mode exception".to_string())
}

fn normalize_json_error_message(message: &str) -> String {
    if message == "circular reference" {
        "Converting circular structure to JSON".to_string()
    } else {
        message.to_string()
    }
}

fn exception_error(ctx: &Ctx<'_>, error: JsError) -> JsError {
    match error {
        JsError::Exception => {
            let value = ctx.catch();
            ctx.throw(value)
        }
        error => Exception::throw_message(ctx, &error.to_string()),
    }
}

pub(super) fn throw_type_error(ctx: &Ctx<'_>, message: &str) -> JsError {
    match rquickjs::String::from_str(ctx.clone(), message) {
        Ok(message) => ctx.throw(message.into_value()),
        Err(_) => Exception::throw_message(ctx, message),
    }
}

pub(super) fn throw_uncatchable(ctx: &Ctx<'_>, message: &str) -> JsError {
    let message = CString::new(message).unwrap_or_default();
    unsafe {
        qjs::JS_ThrowInternalError(ctx.as_raw().as_ptr(), c"%s".as_ptr(), message.as_ptr());
        let value = qjs::JS_GetException(ctx.as_raw().as_ptr());
        qjs::JS_SetUncatchableError(ctx.as_raw().as_ptr(), value);
        qjs::JS_Throw(ctx.as_raw().as_ptr(), value);
    }
    JsError::Exception
}
