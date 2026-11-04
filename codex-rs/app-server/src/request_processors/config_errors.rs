use super::*;

pub(crate) fn config_load_error(err: &std::io::Error) -> JSONRPCErrorError {
    let mut current = err
        .get_ref()
        .map(|error| error as &(dyn std::error::Error + 'static));
    while let Some(source) = current {
        if let Some(provider_error) =
            source.downcast_ref::<codex_app_server_protocol::ModelProviderError>()
        {
            let mut error = invalid_request(provider_error.to_string());
            error.data = serde_json::to_value(provider_error).ok();
            return error;
        }
        current = source.source();
    }
    invalid_request(format!("failed to load configuration: {err}"))
}
