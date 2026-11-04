// Release builds exceed rustc's default query depth when laying out the async
// MessageProcessor::handle_initialized_client_request future.
#![recursion_limit = "256"]

//! Shared in-process app-server client facade for CLI surfaces.
//!
//! This crate wraps [`codex_app_server::in_process`] behind a single async API
//! used by surfaces like TUI and exec. It centralizes:
//!
//! - Runtime startup and initialize-capabilities handshake.
//! - Typed caller-provided startup identity (`SessionSource` + client name).
//! - Typed and raw request/notification dispatch.
//! - Server request resolution and rejection.
//! - Ordered, lossless event consumption that cannot block request processing.
//! - Bounded graceful shutdown with abort fallback.
//!
//! The facade interposes a worker task between the caller and the underlying
//! [`InProcessClientHandle`](codex_app_server::in_process::InProcessClientHandle),
//! bridging async `mpsc` channels on both sides. Commands and the underlying
//! runtime remain bounded; the local consumer event queue is unbounded so
//! unread notifications cannot prevent request responses from being delivered.

mod path;
mod server_request;
pub use server_request::AppServerReplyHandle;

use std::error::Error;
use std::fmt;
use std::future::Future;
use std::io::Error as IoError;
use std::io::ErrorKind;
use std::io::Result as IoResult;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

pub use codex_app_server::app_server_control_socket_path;
pub use codex_app_server::in_process::DEFAULT_IN_PROCESS_CHANNEL_CAPACITY;
pub use codex_app_server::in_process::EmbeddedNetworkPolicy;
pub use codex_app_server::in_process::InProcessServerEvent;
use codex_app_server::in_process::InProcessStartArgs;
use codex_app_server::in_process::LogDbLayer;
pub use codex_app_server::in_process::StateDbHandle;
use codex_app_server_protocol::ClientInfo;
use codex_app_server_protocol::ClientNotification;
use codex_app_server_protocol::ClientRequest;
use codex_app_server_protocol::ConfigWarningNotification;
use codex_app_server_protocol::InitializeCapabilities;
use codex_app_server_protocol::InitializeParams;
use codex_app_server_protocol::JSONRPCErrorError;
use codex_app_server_protocol::RequestId;
use codex_app_server_protocol::Result as JsonRpcResult;
use codex_app_server_protocol::ServerNotification;
use codex_app_server_protocol::ServerRequest;
use codex_arg0::Arg0DispatchPaths;
use codex_config::LoaderOverrides;
use codex_config::NoopThreadConfigLoader;
use codex_core::config::Config;
pub use codex_core::otel_init::build_provider as build_otel_provider;
pub use codex_exec_server::EnvironmentManager;
pub use codex_exec_server::ExecServerRuntimeOptions;
use codex_feedback::CodexFeedback;
use codex_protocol::protocol::SessionSource;
use codex_utils_absolute_path::AbsolutePathBuf;
use serde::de::DeserializeOwned;
use tokio::sync::mpsc;
use tokio::sync::oneshot;
use tokio::time::timeout;
use toml::Value as TomlValue;
use tracing::warn;

pub use crate::path::AppServerPath;

/// Transitional access to core-only embedded app-server types.
///
/// New TUI behavior should prefer the app-server protocol methods. This
/// module exists so clients can remove a direct `codex-core` dependency
/// while legacy startup/config paths are migrated to RPCs.
pub mod legacy_core {
    pub mod config {
        pub use codex_core::config::*;

        pub mod edit {
            pub use codex_core::config::edit::*;
        }
    }
}

const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(5);
// Covers the embedded drain and final task join.
const IN_PROCESS_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(25);

/// Raw app-server request result for typed in-process requests.
///
/// Even on the in-process path, successful responses still travel back through
/// the same JSON-RPC result envelope used by socket/stdio transports because
/// `MessageProcessor` continues to produce that shape internally.
pub type RequestResult = std::result::Result<JsonRpcResult, JSONRPCErrorError>;

#[derive(Debug, Clone)]
pub enum AppServerEvent {
    Lagged { skipped: usize },
    ServerNotification(Box<ServerNotification>),
    ServerRequest(Box<ServerRequest>),
    Disconnected { message: String },
}

impl From<InProcessServerEvent> for AppServerEvent {
    fn from(value: InProcessServerEvent) -> Self {
        match value {
            InProcessServerEvent::Lagged { skipped } => Self::Lagged { skipped },
            InProcessServerEvent::ServerNotification(notification) => {
                Self::ServerNotification(notification)
            }
            InProcessServerEvent::ServerRequest(request) => Self::ServerRequest(request),
        }
    }
}

/// Layered error for [`InProcessAppServerClient::request_typed`].
///
/// This keeps transport failures, server-side JSON-RPC failures, and response
/// decode failures distinct so callers can decide whether to retry, surface a
/// server error, or treat the response as an internal request/response mismatch.
#[derive(Debug)]
pub enum TypedRequestError {
    Transport {
        method: String,
        source: IoError,
    },
    Server {
        method: String,
        source: JSONRPCErrorError,
    },
    Deserialize {
        method: String,
        source: serde_json::Error,
    },
}

impl fmt::Display for TypedRequestError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Transport { method, source } => {
                write!(f, "{method} transport error: {source}")
            }
            Self::Server { method, source } => {
                write!(
                    f,
                    "{method} failed: {} (code {})",
                    source.message, source.code
                )?;
                if let Some(data) = source.data.as_ref() {
                    write!(f, ", data: {data}")?;
                }
                Ok(())
            }
            Self::Deserialize { method, source } => {
                write!(f, "{method} response decode error: {source}")
            }
        }
    }
}

impl Error for TypedRequestError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Transport { source, .. } => Some(source),
            Self::Server { .. } => None,
            Self::Deserialize { source, .. } => Some(source),
        }
    }
}

// Await transport in a separate statement before calling this: dropping the
// completed future must precede any caller-provided Deserialize implementation.
fn decode_typed_response<T>(
    method: &str,
    response: IoResult<RequestResult>,
) -> Result<T, TypedRequestError>
where
    T: DeserializeOwned,
{
    let response = response.map_err(|source| TypedRequestError::Transport {
        method: method.to_string(),
        source,
    })?;
    let result = response.map_err(|source| TypedRequestError::Server {
        method: method.to_string(),
        source,
    })?;
    serde_json::from_value(result).map_err(|source| TypedRequestError::Deserialize {
        method: method.to_string(),
        source,
    })
}

#[derive(Clone)]
pub struct InProcessClientStartArgs {
    /// Resolved argv0 dispatch paths used by command execution internals.
    pub arg0_paths: Arg0DispatchPaths,
    /// Shared config used to initialize app-server runtime.
    pub config: Arc<Config>,
    /// CLI config overrides that are already parsed into TOML values.
    pub cli_overrides: Vec<(String, TomlValue)>,
    /// Loader override knobs used by config API paths.
    pub loader_overrides: LoaderOverrides,
    /// Whether config API paths should reject unknown config fields.
    pub strict_config: bool,
    /// Policy shared with transports created by the embedder before startup.
    pub embedded_network_policy: EmbeddedNetworkPolicy,
    /// Feedback sink used by app-server/core telemetry and logs.
    pub feedback: CodexFeedback,
    /// SQLite tracing layer used to flush recently emitted logs before feedback upload.
    pub log_db: Option<LogDbLayer>,
    /// Process-wide SQLite state handle shared with the embedded app-server.
    pub state_db: Option<StateDbHandle>,
    /// Environment manager used by core execution and filesystem operations.
    pub environment_manager: Arc<EnvironmentManager>,
    /// Startup warnings emitted after initialize succeeds.
    pub config_warnings: Vec<ConfigWarningNotification>,
    /// Session source recorded in app-server thread metadata.
    pub session_source: SessionSource,
    /// Whether auth loading should honor the `CODEX_API_KEY` environment variable.
    pub enable_codex_api_key_env: bool,
    /// Client name reported during initialize.
    pub client_name: String,
    /// Client version reported during initialize.
    pub client_version: String,
    /// Whether experimental APIs are requested at initialize time.
    pub experimental_api: bool,
    /// Whether MCP servers may send `openai/form` elicitation requests.
    pub mcp_server_openai_form_elicitation: bool,
    /// Notification methods this client opts out of receiving.
    pub opt_out_notification_methods: Vec<String>,
    /// Queue capacity for command and embedded-runtime channels (clamped to at least 1).
    pub channel_capacity: usize,
}

impl InProcessClientStartArgs {
    /// Builds initialize params from caller-provided metadata.
    pub fn initialize_params(&self) -> InitializeParams {
        let capabilities = InitializeCapabilities {
            explicit_gateway_oauth: false,
            experimental_api: self.experimental_api,
            request_attestation: false,
            extensions: None,
            opt_out_notification_methods: if self.opt_out_notification_methods.is_empty() {
                None
            } else {
                Some(self.opt_out_notification_methods.clone())
            },
            mcp_server_openai_form_elicitation: self.mcp_server_openai_form_elicitation,
        };

        InitializeParams {
            client_info: ClientInfo {
                name: self.client_name.clone(),
                title: None,
                version: self.client_version.clone(),
            },
            capabilities: Some(capabilities),
        }
    }

    fn into_runtime_start_args(self) -> InProcessStartArgs {
        let initialize = self.initialize_params();
        InProcessStartArgs {
            arg0_paths: self.arg0_paths,
            config: self.config,
            cli_overrides: self.cli_overrides,
            loader_overrides: self.loader_overrides,
            strict_config: self.strict_config,
            embedded_network_policy: self.embedded_network_policy,
            thread_config_loader: Arc::new(NoopThreadConfigLoader),
            feedback: self.feedback,
            log_db: self.log_db,
            state_db: self.state_db,
            environment_manager: self.environment_manager,
            config_warnings: self.config_warnings,
            session_source: self.session_source,
            enable_codex_api_key_env: self.enable_codex_api_key_env,
            initialize,
            channel_capacity: self.channel_capacity,
        }
    }
}

/// Internal command sent from public facade methods to the worker task.
///
/// Each variant carries a oneshot sender so the caller can `await` the
/// result without holding a mutable reference to the client.
enum ClientCommand {
    Request {
        request: Box<ClientRequest>,
        response_tx: oneshot::Sender<IoResult<RequestResult>>,
    },
    Notify {
        notification: ClientNotification,
        response_tx: oneshot::Sender<IoResult<()>>,
    },
    ResolveServerRequest {
        request_id: RequestId,
        result: JsonRpcResult,
        response_tx: oneshot::Sender<IoResult<()>>,
    },
    RejectServerRequest {
        request_id: RequestId,
        error: JSONRPCErrorError,
        response_tx: oneshot::Sender<IoResult<()>>,
    },
    Shutdown {
        response_tx: oneshot::Sender<IoResult<()>>,
    },
}

/// Test-only hooks for observing and intercepting in-process requests.
///
/// These hooks keep TUI fixture coverage while ensuring tests exercise the
/// same embedded transport as production, without an out-of-process client.
#[derive(Clone)]
pub struct InProcessRequestHooks {
    pub before_request_async: Arc<
        dyn for<'a> Fn(&'a ClientRequest) -> Pin<Box<dyn Future<Output = ()> + Send + 'a>>
            + Send
            + Sync,
    >,
    pub before_request:
        Arc<dyn Fn(&ClientRequest) -> Option<IoResult<RequestResult>> + Send + Sync>,
    pub after_request: Arc<dyn Fn(&ClientRequest, &mut IoResult<RequestResult>) + Send + Sync>,
    pub server_request_resolved: Arc<dyn Fn(RequestId, &JsonRpcResult, bool) + Send + Sync>,
}

impl Default for InProcessRequestHooks {
    fn default() -> Self {
        Self {
            before_request_async: Arc::new(|_| Box::pin(async {})),
            before_request: Arc::new(|_| None),
            after_request: Arc::new(|_, _| {}),
            server_request_resolved: Arc::new(|_, _, _| {}),
        }
    }
}

/// Async facade over the in-process app-server runtime.
///
/// This type owns a worker task that bridges between:
/// - caller-facing async `mpsc` channels used by TUI/exec
/// - [`codex_app_server::in_process::InProcessClientHandle`], which speaks to
///   the embedded `MessageProcessor`
///
/// The facade intentionally preserves the server's request/notification/event
/// model instead of exposing direct core runtime handles. That keeps in-process
/// callers aligned with app-server behavior while still avoiding a process
/// boundary.
pub struct InProcessAppServerClient {
    command_tx: mpsc::Sender<ClientCommand>,
    event_rx: mpsc::UnboundedReceiver<InProcessServerEvent>,
    worker_handle: tokio::task::JoinHandle<()>,
}

#[derive(Clone)]
pub struct InProcessAppServerRequestHandle {
    command_tx: mpsc::Sender<ClientCommand>,
}

#[derive(Clone)]
pub enum AppServerRequestHandle {
    InProcess(InProcessAppServerRequestHandle),
}

pub enum AppServerClient {
    InProcess(InProcessAppServerClient),
}

impl InProcessAppServerClient {
    /// Starts the in-process runtime and facade worker task.
    ///
    /// The returned client is ready for requests and ordered event consumption.
    /// Request queues remain bounded without blocking on unread notifications.
    pub async fn start(args: InProcessClientStartArgs) -> IoResult<Self> {
        Self::start_inner(args, None).await
    }

    /// Starts the in-process runtime with test-only request hooks.
    #[cfg(feature = "test-support")]
    pub async fn start_with_request_hooks(
        args: InProcessClientStartArgs,
        hooks: InProcessRequestHooks,
    ) -> IoResult<Self> {
        Self::start_inner(args, Some(hooks)).await
    }

    async fn start_inner(
        args: InProcessClientStartArgs,
        request_hooks: Option<InProcessRequestHooks>,
    ) -> IoResult<Self> {
        let channel_capacity = args.channel_capacity.max(1);
        let mut handle =
            codex_app_server::in_process::start(args.into_runtime_start_args()).await?;
        let request_sender = handle.sender();
        let (command_tx, mut command_rx) = mpsc::channel::<ClientCommand>(channel_capacity);
        // e9996ec62a preserved transcript events by awaiting a bounded queue, but that can
        // deadlock a foreground request whose response is behind unread notifications.
        // Only this local consumer queue is unbounded; commands and the embedded runtime stay
        // bounded and events remain ordered.
        let (event_tx, event_rx) = mpsc::unbounded_channel::<InProcessServerEvent>();

        let worker_handle = tokio::spawn(async move {
            let mut event_stream_enabled = true;
            loop {
                tokio::select! {
                    command = command_rx.recv() => {
                        match command {
                            Some(ClientCommand::Request { request, response_tx }) => {
                                let request_sender = request_sender.clone();
                                let request_hooks = request_hooks.clone();
                                // Request waits happen on a detached task so
                                // this loop can keep draining runtime events
                                // while the request is blocked on client input.
                                tokio::spawn(async move {
                                    // Device ceremonies belong to the waiting UI. Preserve
                                    // its cancellation through this buffering task.
                                    let cancellable = matches!(*request,
                                        ClientRequest::UserVerificationStatus { .. }
                                        | ClientRequest::UserVerificationEnroll { .. }
                                        | ClientRequest::UserVerificationDelete { .. }
                                        | ClientRequest::UserVerificationVerify { .. });
                                    let mut response_tx = response_tx;
                                    tokio::select! {
                                        _ = response_tx.closed(), if cancellable => {}
                                        result = async {
                                            let request = *request;
                                            if let Some(hooks) = request_hooks.as_ref() {
                                                if let Some(result) =
                                                    (hooks.before_request)(&request)
                                                {
                                                    return result;
                                                }
                                                (hooks.before_request_async)(&request).await;
                                            }
                                            let mut result = request_sender.request(request.clone()).await;
                                            if let Some(hooks) = request_hooks.as_ref() {
                                                (hooks.after_request)(&request, &mut result);
                                            }
                                            result
                                        } => {
                                            let _ = response_tx.send(result);
                                        }
                                    }
                                });
                            }
                            Some(ClientCommand::Notify {
                                notification,
                                response_tx,
                            }) => {
                                let result = request_sender.notify(notification);
                                let _ = response_tx.send(result);
                            }
                            Some(ClientCommand::ResolveServerRequest {
                                request_id,
                                result,
                                response_tx,
                            }) => {
                                if let Some(hooks) = request_hooks.as_ref() {
                                    (hooks.server_request_resolved)(request_id.clone(), &result, false);
                                }
                                let send_result =
                                    request_sender.respond_to_server_request(request_id, result);
                                let _ = response_tx.send(send_result);
                            }
                            Some(ClientCommand::RejectServerRequest {
                                request_id,
                                error,
                                response_tx,
                            }) => {
                                if let Some(hooks) = request_hooks.as_ref() {
                                    (hooks.server_request_resolved)(request_id.clone(), &serde_json::Value::Null, true);
                                }
                                let send_result = request_sender.fail_server_request(request_id, error);
                                let _ = response_tx.send(send_result);
                            }
                            Some(ClientCommand::Shutdown { response_tx }) => {
                                let shutdown_result = handle.shutdown().await;
                                let _ = response_tx.send(shutdown_result);
                                break;
                            }
                            None => {
                                let _ = handle.shutdown().await;
                                break;
                            }
                        }
                    }
                    event = handle.next_event(), if event_stream_enabled => {
                        let Some(event) = event else {
                            break;
                        };
                        if let InProcessServerEvent::ServerRequest(request) = &event
                            && let ServerRequest::ChatgptAuthTokensRefresh { request_id, .. } =
                                request.as_ref()
                        {
                            let send_result = request_sender.fail_server_request(
                                request_id.clone(),
                                JSONRPCErrorError {
                                    code: -32000,
                                    message: "chatgpt auth token refresh is not supported for in-process app-server clients".to_string(),
                                    data: None,
                                },
                            );
                            if let Err(err) = send_result {
                                warn!(
                                    "failed to reject unsupported chatgpt auth token refresh request: {err}"
                                );
                            }
                            continue;
                        }

                        if event_tx.send(event).is_err() {
                            event_stream_enabled = false;
                        }
                    }
                }
            }
        });

        Ok(Self {
            command_tx,
            event_rx,
            worker_handle,
        })
    }

    pub fn request_handle(&self) -> InProcessAppServerRequestHandle {
        InProcessAppServerRequestHandle {
            command_tx: self.command_tx.clone(),
        }
    }

    /// Sends a typed client request and returns raw JSON-RPC result.
    ///
    /// Callers that expect a concrete response type should usually prefer
    /// [`request_typed`](Self::request_typed).
    pub async fn request(&self, request: ClientRequest) -> IoResult<RequestResult> {
        let (response_tx, response_rx) = oneshot::channel();
        self.command_tx
            .send(ClientCommand::Request {
                request: Box::new(request),
                response_tx,
            })
            .await
            .map_err(|_| {
                IoError::new(
                    ErrorKind::BrokenPipe,
                    "in-process app-server worker channel is closed",
                )
            })?;
        response_rx.await.map_err(|_| {
            IoError::new(
                ErrorKind::BrokenPipe,
                "in-process app-server request channel is closed",
            )
        })?
    }

    /// Sends a typed client request and decodes the successful response body.
    ///
    /// This still deserializes from a JSON value produced by app-server's
    /// JSON-RPC result envelope. Because the caller chooses `T`, `Deserialize`
    /// failures indicate an internal request/response mismatch at the call site
    /// (or an in-process bug), not transport skew from an external client.
    pub async fn request_typed<T>(&self, request: ClientRequest) -> Result<T, TypedRequestError>
    where
        T: DeserializeOwned,
    {
        let method = request.method_name();
        let response = self.request(request).await;
        decode_typed_response(method, response)
    }

    /// Sends a typed client notification.
    pub async fn notify(&self, notification: ClientNotification) -> IoResult<()> {
        let (response_tx, response_rx) = oneshot::channel();
        self.command_tx
            .send(ClientCommand::Notify {
                notification,
                response_tx,
            })
            .await
            .map_err(|_| {
                IoError::new(
                    ErrorKind::BrokenPipe,
                    "in-process app-server worker channel is closed",
                )
            })?;
        response_rx.await.map_err(|_| {
            IoError::new(
                ErrorKind::BrokenPipe,
                "in-process app-server notify channel is closed",
            )
        })?
    }

    /// Resolves a pending server request.
    ///
    /// This should only be called with request IDs obtained from the current
    /// client's event stream.
    pub async fn resolve_server_request(
        &self,
        request_id: RequestId,
        result: JsonRpcResult,
    ) -> IoResult<()> {
        self.request_handle()
            .resolve_server_request(request_id, result)
            .await
    }

    /// Rejects a pending server request with JSON-RPC error payload.
    pub async fn reject_server_request(
        &self,
        request_id: RequestId,
        error: JSONRPCErrorError,
    ) -> IoResult<()> {
        self.request_handle()
            .reject_server_request(request_id, error)
            .await
    }

    /// Returns the next in-process event, or `None` when worker exits.
    ///
    /// Events remain ordered and are retained while callers await requests.
    pub async fn next_event(&mut self) -> Option<InProcessServerEvent> {
        self.event_rx.recv().await
    }

    /// Shuts down worker and in-process runtime with bounded wait.
    ///
    /// If graceful shutdown exceeds timeout, the worker task is aborted to
    /// avoid leaking background tasks in embedding callers.
    pub async fn shutdown(self) -> IoResult<()> {
        let Self {
            command_tx,
            event_rx,
            worker_handle,
        } = self;
        let mut worker_handle = worker_handle;
        // Stop forwarding caller-facing events before asking the worker to shut down.
        drop(event_rx);
        let (response_tx, response_rx) = oneshot::channel();
        if command_tx
            .send(ClientCommand::Shutdown { response_tx })
            .await
            .is_ok()
            && let Ok(command_result) = timeout(IN_PROCESS_SHUTDOWN_TIMEOUT, response_rx).await
        {
            command_result.map_err(|_| {
                IoError::new(
                    ErrorKind::BrokenPipe,
                    "in-process app-server shutdown channel is closed",
                )
            })??;
        }

        if let Err(_elapsed) = timeout(IN_PROCESS_SHUTDOWN_TIMEOUT, &mut worker_handle).await {
            worker_handle.abort();
            let _ = worker_handle.await;
        }
        Ok(())
    }
}

impl InProcessAppServerRequestHandle {
    pub(crate) async fn resolve_server_request(
        &self,
        request_id: RequestId,
        result: JsonRpcResult,
    ) -> IoResult<()> {
        let (response_tx, response_rx) = oneshot::channel();
        self.command_tx
            .send(ClientCommand::ResolveServerRequest {
                request_id,
                result,
                response_tx,
            })
            .await
            .map_err(|_| {
                IoError::new(
                    ErrorKind::BrokenPipe,
                    "in-process app-server worker channel is closed",
                )
            })?;
        response_rx.await.map_err(|_| {
            IoError::new(
                ErrorKind::BrokenPipe,
                "in-process app-server resolve channel is closed",
            )
        })?
    }

    /// Rejects a pending server request with JSON-RPC error payload.
    pub(crate) async fn reject_server_request(
        &self,
        request_id: RequestId,
        error: JSONRPCErrorError,
    ) -> IoResult<()> {
        let (response_tx, response_rx) = oneshot::channel();
        self.command_tx
            .send(ClientCommand::RejectServerRequest {
                request_id,
                error,
                response_tx,
            })
            .await
            .map_err(|_| {
                IoError::new(
                    ErrorKind::BrokenPipe,
                    "in-process app-server worker channel is closed",
                )
            })?;
        response_rx.await.map_err(|_| {
            IoError::new(
                ErrorKind::BrokenPipe,
                "in-process app-server reject channel is closed",
            )
        })?
    }

    pub async fn request(&self, request: ClientRequest) -> IoResult<RequestResult> {
        let (response_tx, response_rx) = oneshot::channel();
        self.command_tx
            .send(ClientCommand::Request {
                request: Box::new(request),
                response_tx,
            })
            .await
            .map_err(|_| {
                IoError::new(
                    ErrorKind::BrokenPipe,
                    "in-process app-server worker channel is closed",
                )
            })?;
        response_rx.await.map_err(|_| {
            IoError::new(
                ErrorKind::BrokenPipe,
                "in-process app-server request channel is closed",
            )
        })?
    }

    pub async fn request_typed<T>(&self, request: ClientRequest) -> Result<T, TypedRequestError>
    where
        T: DeserializeOwned,
    {
        let method = request.method_name();
        let response = self.request(request).await;
        decode_typed_response(method, response)
    }
}

impl AppServerRequestHandle {
    pub(crate) async fn resolve_server_request(
        &self,
        request_id: RequestId,
        result: JsonRpcResult,
    ) -> IoResult<()> {
        match self {
            Self::InProcess(handle) => handle.resolve_server_request(request_id, result).await,
        }
    }

    pub(crate) async fn reject_server_request(
        &self,
        request_id: RequestId,
        error: JSONRPCErrorError,
    ) -> IoResult<()> {
        match self {
            Self::InProcess(handle) => handle.reject_server_request(request_id, error).await,
        }
    }

    pub async fn request(&self, request: ClientRequest) -> IoResult<RequestResult> {
        match self {
            Self::InProcess(handle) => handle.request(request).await,
        }
    }

    pub async fn request_typed<T>(&self, request: ClientRequest) -> Result<T, TypedRequestError>
    where
        T: DeserializeOwned,
    {
        let method = request.method_name();
        let response = self.request(request).await;
        decode_typed_response(method, response)
    }
}

impl AppServerClient {
    /// App-server platform family, which can differ from the executor's platform.
    pub fn platform_family(&self) -> Option<&str> {
        match self {
            Self::InProcess(_) => Some(std::env::consts::FAMILY),
        }
    }

    /// App-server operating system as reported at initialization, including unknown values.
    pub fn platform_os(&self) -> Option<&str> {
        match self {
            Self::InProcess(_) => Some(std::env::consts::OS),
        }
    }

    pub fn codex_home(&self, local_codex_home: &AbsolutePathBuf) -> Option<AppServerPath> {
        match self {
            Self::InProcess(_) => Some(AppServerPath::from_app_server(
                local_codex_home.display().to_string(),
            )),
        }
    }

    pub async fn request(&self, request: ClientRequest) -> IoResult<RequestResult> {
        match self {
            Self::InProcess(client) => client.request(request).await,
        }
    }

    pub async fn request_typed<T>(&self, request: ClientRequest) -> Result<T, TypedRequestError>
    where
        T: DeserializeOwned,
    {
        let method = request.method_name();
        let response = self.request(request).await;
        decode_typed_response(method, response)
    }

    pub async fn notify(&self, notification: ClientNotification) -> IoResult<()> {
        match self {
            Self::InProcess(client) => client.notify(notification).await,
        }
    }

    pub async fn resolve_server_request(
        &self,
        request_id: RequestId,
        result: JsonRpcResult,
    ) -> IoResult<()> {
        match self {
            Self::InProcess(client) => client.resolve_server_request(request_id, result).await,
        }
    }

    pub async fn reject_server_request(
        &self,
        request_id: RequestId,
        error: JSONRPCErrorError,
    ) -> IoResult<()> {
        match self {
            Self::InProcess(client) => client.reject_server_request(request_id, error).await,
        }
    }

    pub async fn next_event(&mut self) -> Option<AppServerEvent> {
        match self {
            Self::InProcess(client) => client.next_event().await.map(Into::into),
        }
    }

    pub async fn shutdown(self) -> IoResult<()> {
        match self {
            Self::InProcess(client) => client.shutdown().await,
        }
    }

    pub fn request_handle(&self) -> AppServerRequestHandle {
        match self {
            Self::InProcess(client) => AppServerRequestHandle::InProcess(client.request_handle()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use codex_app_server_protocol::AccountUpdatedNotification;
    use codex_app_server_protocol::ConfigRequirementsReadResponse;
    use codex_app_server_protocol::GetAccountResponse;
    use codex_app_server_protocol::ServerNotification;
    use codex_app_server_protocol::SessionSource as ApiSessionSource;
    use codex_app_server_protocol::ThreadSettingsUpdateParams;
    use codex_app_server_protocol::ThreadSettingsUpdateResponse;
    use codex_app_server_protocol::ThreadStartParams;
    use codex_app_server_protocol::ThreadStartResponse;
    use codex_app_server_protocol::ToolRequestUserInputParams;
    use codex_app_server_protocol::ToolRequestUserInputQuestion;
    use codex_core::config::ConfigBuilder;
    use codex_core::init_state_db;
    use codex_protocol::config_types::Personality;
    use futures::poll;
    use pretty_assertions::assert_eq;
    use std::ops::Deref;
    use std::path::Path;
    use tempfile::TempDir;
    use tokio::time::Duration;
    use tokio::time::timeout;

    async fn build_test_config() -> Config {
        match ConfigBuilder::default().build().await {
            Ok(config) => config,
            Err(_) => Config::load_default_with_cli_overrides(Vec::new())
                .await
                .expect("default config should load"),
        }
    }

    async fn build_test_config_for_codex_home(codex_home: &Path) -> Config {
        match ConfigBuilder::default()
            .codex_home(codex_home.to_path_buf())
            .build()
            .await
        {
            Ok(config) => config,
            Err(_) => Config::load_default_with_cli_overrides_for_codex_home(
                codex_home.to_path_buf(),
                Vec::new(),
            )
            .await
            .expect("default config should load"),
        }
    }

    struct TestClient {
        _codex_home: TempDir,
        client: InProcessAppServerClient,
    }

    impl Deref for TestClient {
        type Target = InProcessAppServerClient;

        fn deref(&self) -> &Self::Target {
            &self.client
        }
    }

    impl TestClient {
        async fn shutdown(self) -> IoResult<()> {
            self.client.shutdown().await
        }
    }

    async fn start_test_client_with_capacity(
        session_source: SessionSource,
        channel_capacity: usize,
    ) -> TestClient {
        let codex_home = TempDir::new().expect("temp dir");
        let config = Arc::new(build_test_config_for_codex_home(codex_home.path()).await);
        let state_db = init_state_db(config.as_ref())
            .await
            .expect("state db should initialize for in-process test");
        let client = InProcessAppServerClient::start(InProcessClientStartArgs {
            arg0_paths: Arg0DispatchPaths::default(),
            config,
            cli_overrides: Vec::new(),
            loader_overrides: LoaderOverrides::default(),
            strict_config: false,
            embedded_network_policy: Default::default(),
            feedback: CodexFeedback::new(),
            log_db: None,
            state_db: Some(state_db),
            environment_manager: Arc::new(EnvironmentManager::default_for_tests()),
            config_warnings: Vec::new(),
            session_source,
            enable_codex_api_key_env: false,
            client_name: "codex-app-server-client-test".to_string(),
            client_version: "0.0.0-test".to_string(),
            experimental_api: true,
            mcp_server_openai_form_elicitation: false,
            opt_out_notification_methods: Vec::new(),
            channel_capacity,
        })
        .await
        .expect("in-process app-server client should start");

        TestClient {
            _codex_home: codex_home,
            client,
        }
    }

    async fn start_test_client(session_source: SessionSource) -> TestClient {
        start_test_client_with_capacity(session_source, DEFAULT_IN_PROCESS_CHANNEL_CAPACITY).await
    }
    fn command_execution_output_delta_notification(delta: &str) -> ServerNotification {
        ServerNotification::CommandExecutionOutputDelta(
            codex_app_server_protocol::CommandExecutionOutputDeltaNotification {
                thread_id: "thread".to_string(),
                turn_id: "turn".to_string(),
                item_id: "item".to_string(),
                delta: delta.to_string(),
            },
        )
    }

    fn agent_message_delta_notification(delta: &str) -> ServerNotification {
        ServerNotification::AgentMessageDelta(
            codex_app_server_protocol::AgentMessageDeltaNotification {
                thread_id: "thread".to_string(),
                turn_id: "turn".to_string(),
                item_id: "item".to_string(),
                delta: delta.to_string(),
            },
        )
    }

    fn item_completed_notification(text: &str) -> ServerNotification {
        ServerNotification::ItemCompleted(
            codex_app_server_protocol::ItemCompletedNotification::new(
                "thread".to_string(),
                "turn".to_string(),
                codex_app_server_protocol::ThreadItemPayload::AgentMessage {
                    id: "item".to_string(),
                    text: text.to_string(),
                    phase: None,
                    memory_citation: None,
                    delivery: None,
                    questions: None,
                },
                /*started_at_ms*/ None,
                /*completed_at_ms*/ 0,
            ),
        )
    }

    fn turn_completed_notification() -> ServerNotification {
        ServerNotification::TurnCompleted(codex_app_server_protocol::TurnCompletedNotification {
            thread_id: "thread".to_string(),
            turn: codex_app_server_protocol::Turn {
                id: "turn".to_string(),
                root_turn_id: None,
                items_view: codex_app_server_protocol::TurnItemsView::Full,
                items: Vec::new(),
                status: codex_app_server_protocol::TurnStatus::Completed,
                error: None,
                started_at: None,
                completed_at: Some(0),
                duration_ms: Some(1),
            },
        })
    }
    #[tokio::test]
    async fn typed_request_drop_cancels_waiting_send_and_reply() {
        let (command_tx, mut commands) = mpsc::channel(/*buffer*/ 1);
        let handle =
            AppServerRequestHandle::InProcess(InProcessAppServerRequestHandle { command_tx });
        let request = ClientRequest::ConfigRequirementsRead {
            request_id: RequestId::String("typed-request".to_string()),
            params: None,
        };
        let mut pending = Box::pin(handle.request_typed::<serde_json::Value>(request.clone()));
        assert!(matches!(
            commands.try_recv(),
            Err(mpsc::error::TryRecvError::Empty)
        ));
        assert!(poll!(&mut pending).is_pending());
        let mut blocked = Box::pin(handle.request_typed::<serde_json::Value>(request.clone()));
        assert!(poll!(&mut blocked).is_pending());
        drop(blocked);

        let ClientCommand::Request {
            request: sent,
            response_tx,
        } = commands.try_recv().expect("first request should be queued")
        else {
            panic!("expected request command");
        };
        assert_eq!(*sent, request);
        assert!(!response_tx.is_closed());
        drop(pending);
        assert!(response_tx.is_closed());
        assert!(matches!(
            commands.try_recv(),
            Err(mpsc::error::TryRecvError::Empty)
        ));
    }

    #[tokio::test]
    async fn typed_request_roundtrip_works() {
        let TestClient {
            _codex_home,
            client,
        } = start_test_client(SessionSource::Exec).await;
        let client = AppServerClient::InProcess(client);
        assert_eq!(
            (client.platform_family(), client.platform_os()),
            (Some(std::env::consts::FAMILY), Some(std::env::consts::OS))
        );
        let _response: ConfigRequirementsReadResponse = client
            .request_typed(ClientRequest::ConfigRequirementsRead {
                request_id: RequestId::Integer(1),
                params: None,
            })
            .await
            .expect("typed request should succeed");
        client.shutdown().await.expect("shutdown should complete");
    }

    #[tokio::test]
    async fn typed_request_reports_json_rpc_errors() {
        let client = start_test_client(SessionSource::Exec).await;
        let err = client
            .request_typed::<ConfigRequirementsReadResponse>(ClientRequest::ThreadRead {
                request_id: RequestId::Integer(99),
                params: codex_app_server_protocol::ThreadReadParams {
                    thread_id: "missing-thread".to_string(),
                    include_turns: false,
                },
            })
            .await
            .expect_err("missing thread should return a JSON-RPC error");
        assert!(
            err.to_string().starts_with("thread/read failed:"),
            "expected method-qualified JSON-RPC failure message"
        );
        client.shutdown().await.expect("shutdown should complete");
    }

    #[tokio::test]
    async fn caller_provided_session_source_is_applied() {
        for (session_source, expected_source) in [
            (SessionSource::Exec, ApiSessionSource::Exec),
            (SessionSource::Cli, ApiSessionSource::Cli),
        ] {
            let client = start_test_client(session_source).await;
            let parsed: ThreadStartResponse = client
                .request_typed(ClientRequest::ThreadStart {
                    request_id: RequestId::Integer(2),
                    params: ThreadStartParams {
                        ephemeral: Some(true),
                        ..ThreadStartParams::default()
                    },
                })
                .await
                .expect("thread/start should succeed");
            assert_eq!(parsed.thread.source, expected_source);
            client.shutdown().await.expect("shutdown should complete");
        }
    }

    #[tokio::test]
    async fn threads_started_via_app_server_are_visible_through_typed_requests() {
        let client = start_test_client(SessionSource::Cli).await;

        let response: ThreadStartResponse = client
            .request_typed(ClientRequest::ThreadStart {
                request_id: RequestId::Integer(3),
                params: ThreadStartParams {
                    ephemeral: Some(true),
                    ..ThreadStartParams::default()
                },
            })
            .await
            .expect("thread/start should succeed");
        let read = client
            .request_typed::<codex_app_server_protocol::ThreadReadResponse>(
                ClientRequest::ThreadRead {
                    request_id: RequestId::Integer(4),
                    params: codex_app_server_protocol::ThreadReadParams {
                        thread_id: response.thread.id.clone(),
                        include_turns: false,
                    },
                },
            )
            .await
            .expect("thread/read should return the newly started thread");
        assert_eq!(read.thread.id, response.thread.id);

        client.shutdown().await.expect("shutdown should complete");
    }

    #[tokio::test]
    async fn tiny_channel_capacity_still_supports_request_roundtrip() {
        let client =
            start_test_client_with_capacity(SessionSource::Exec, /*channel_capacity*/ 1).await;
        let _response: ConfigRequirementsReadResponse = client
            .request_typed(ClientRequest::ConfigRequirementsRead {
                request_id: RequestId::Integer(1),
                params: None,
            })
            .await
            .expect("typed request should succeed");
        client.shutdown().await.expect("shutdown should complete");
    }

    #[tokio::test]
    async fn unread_lossless_notifications_do_not_block_in_process_requests() {
        let mut client =
            start_test_client_with_capacity(SessionSource::Cli, /*channel_capacity*/ 1).await;
        let thread: ThreadStartResponse = client
            .request_typed(ClientRequest::ThreadStart {
                request_id: RequestId::Integer(1),
                params: ThreadStartParams {
                    ephemeral: Some(true),
                    personality: Some(Personality::None),
                    ..ThreadStartParams::default()
                },
            })
            .await
            .expect("thread/start should succeed");
        let request_handle = client.request_handle();

        timeout(Duration::from_secs(2), async {
            for (index, personality) in [
                Personality::Friendly,
                Personality::Pragmatic,
                Personality::Friendly,
                Personality::Pragmatic,
            ]
            .into_iter()
            .enumerate()
            {
                let _: ThreadSettingsUpdateResponse = request_handle
                    .request_typed(ClientRequest::ThreadSettingsUpdate {
                        request_id: RequestId::Integer((index + 2) as i64),
                        params: ThreadSettingsUpdateParams {
                            thread_id: thread.thread.id.clone(),
                            personality: Some(personality),
                            ..ThreadSettingsUpdateParams::default()
                        },
                    })
                    .await
                    .expect("thread/settings/update should succeed");
            }

            let _: ConfigRequirementsReadResponse = request_handle
                .request_typed(ClientRequest::ConfigRequirementsRead {
                    request_id: RequestId::Integer(10),
                    params: None,
                })
                .await
                .expect("configuration request should succeed");
        })
        .await
        .expect("unread lossless notifications must not block app-server requests");

        let mut personalities = Vec::new();
        timeout(Duration::from_secs(2), async {
            while personalities.len() < 4 {
                if let Some(InProcessServerEvent::ServerNotification(notification)) =
                    client.client.next_event().await
                    && let ServerNotification::ThreadSettingsUpdated(notification) =
                        notification.as_ref()
                {
                    personalities.push(notification.thread_settings.personality);
                }
            }
        })
        .await
        .expect("queued settings notifications should remain readable");
        assert_eq!(
            personalities,
            vec![
                Some(Personality::Friendly),
                Some(Personality::Pragmatic),
                Some(Personality::Friendly),
                Some(Personality::Pragmatic),
            ]
        );

        client.shutdown().await.expect("shutdown should complete");
    }
    #[test]
    fn typed_request_error_exposes_sources() {
        let transport = TypedRequestError::Transport {
            method: "config/read".to_string(),
            source: IoError::new(ErrorKind::BrokenPipe, "closed"),
        };
        assert_eq!(std::error::Error::source(&transport).is_some(), true);

        let server = TypedRequestError::Server {
            method: "thread/read".to_string(),
            source: JSONRPCErrorError {
                code: -32603,
                data: Some(serde_json::json!({"detail": "config lock mismatch"})),
                message: "internal".to_string(),
            },
        };
        assert_eq!(std::error::Error::source(&server).is_some(), false);
        assert_eq!(
            server.to_string(),
            "thread/read failed: internal (code -32603), data: {\"detail\":\"config lock mismatch\"}"
        );

        let deserialize = TypedRequestError::Deserialize {
            method: "thread/start".to_string(),
            source: serde_json::from_str::<u32>("\"nope\"")
                .expect_err("invalid integer should return deserialize error"),
        };
        assert_eq!(std::error::Error::source(&deserialize).is_some(), true);
    }

    #[tokio::test]
    async fn next_event_surfaces_lagged_markers() {
        let (command_tx, _) = mpsc::channel(1);
        let (event_tx, event_rx) = mpsc::unbounded_channel();
        let worker_handle = tokio::spawn(async {});
        event_tx
            .send(InProcessServerEvent::Lagged { skipped: 3 })
            .expect("lagged marker should enqueue");
        drop(event_tx);

        let mut client = InProcessAppServerClient {
            command_tx,
            event_rx,
            worker_handle,
        };

        let event = timeout(Duration::from_secs(2), client.next_event())
            .await
            .expect("lagged marker should arrive before timeout");
        assert!(matches!(
            event,
            Some(InProcessServerEvent::Lagged { skipped: 3 })
        ));

        client.shutdown().await.expect("shutdown should complete");
    }

    #[tokio::test]
    async fn runtime_start_args_forward_environment_manager_and_openai_form_capability() {
        let config = Arc::new(build_test_config().await);
        let environment_manager = Arc::new(
            EnvironmentManager::create_for_tests(
                Some("ws://127.0.0.1:8765".to_string()),
                Some(
                    ExecServerRuntimeOptions::new(
                        std::env::current_exe().expect("current exe"),
                        /*codex_linux_sandbox_exe*/ None,
                    )
                    .expect("runtime paths"),
                ),
            )
            .await,
        );

        let runtime_args = InProcessClientStartArgs {
            arg0_paths: Arg0DispatchPaths::default(),
            config: config.clone(),
            cli_overrides: Vec::new(),
            loader_overrides: LoaderOverrides::default(),
            strict_config: false,
            embedded_network_policy: Default::default(),
            feedback: CodexFeedback::new(),
            log_db: None,
            state_db: None,
            environment_manager: environment_manager.clone(),
            config_warnings: Vec::new(),
            session_source: SessionSource::Exec,
            enable_codex_api_key_env: false,
            client_name: "codex-app-server-client-test".to_string(),
            client_version: "0.0.0-test".to_string(),
            experimental_api: true,
            mcp_server_openai_form_elicitation: true,
            opt_out_notification_methods: Vec::new(),
            channel_capacity: DEFAULT_IN_PROCESS_CHANNEL_CAPACITY,
        }
        .into_runtime_start_args();

        assert_eq!(runtime_args.config, config);
        assert!(
            runtime_args
                .initialize
                .capabilities
                .expect("initialize capabilities")
                .mcp_server_openai_form_elicitation
        );
        assert!(Arc::ptr_eq(
            &runtime_args.environment_manager,
            &environment_manager
        ));
        assert!(
            runtime_args
                .environment_manager
                .default_environment()
                .expect("default environment")
                .is_remote()
        );
    }

    #[tokio::test]
    async fn shutdown_completes_promptly_without_retained_managers() {
        let client = start_test_client(SessionSource::Cli).await;

        timeout(Duration::from_secs(1), client.shutdown())
            .await
            .expect("shutdown should not wait for the 5s fallback timeout")
            .expect("shutdown should complete");
    }

    #[tokio::test(start_paused = true)]
    async fn shutdown_waits_for_in_process_drain() {
        use std::sync::atomic::AtomicBool;
        use std::sync::atomic::Ordering;

        let (command_tx, mut command_rx) = mpsc::channel(1);
        let (_event_tx, event_rx) = mpsc::unbounded_channel();
        let completed = Arc::new(AtomicBool::new(false));
        let worker_completed = Arc::clone(&completed);
        let worker_handle = tokio::spawn(async move {
            let response_tx = match command_rx.recv().await {
                Some(ClientCommand::Shutdown { response_tx }) => response_tx,
                _ => panic!("expected shutdown command"),
            };
            tokio::time::sleep(Duration::from_secs(30)).await;
            worker_completed.store(true, Ordering::Release);
            let _ = response_tx.send(Ok(()));
        });
        let client = InProcessAppServerClient {
            command_tx,
            event_rx,
            worker_handle,
        };

        client.shutdown().await.expect("shutdown should complete");
        assert!(completed.load(Ordering::Acquire));
    }
}
