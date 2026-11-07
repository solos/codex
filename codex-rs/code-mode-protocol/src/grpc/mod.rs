tonic::include_proto!("codex.code_mode.v1");

pub const MAX_IDENTIFIER_BYTES: usize = 256;

/// Decimal nanoseconds a completed result was buffered before a wait or terminate retrieved it.
/// Zero means immediate delivery. Hosts omit this metadata if no completed result was retrieved.
pub const RESULT_BUFFERED_FOR_NS_METADATA: &str = "codex-code-mode-result-buffered-for-ns";
