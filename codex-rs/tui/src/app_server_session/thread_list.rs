//! Session listing preserves grouped filters while tolerating older single-CWD servers.

use super::*;
use codex_app_server_protocol::ThreadListCwdFilter;

impl AppServerSession {
    pub(crate) async fn thread_list(
        &mut self,
        mut params: ThreadListParams,
    ) -> Result<ThreadListResponse> {
        loop {
            let request_id = self.next_request_id();
            let response = self
                .client
                .request_typed(ClientRequest::ThreadList {
                    request_id,
                    params: params.clone(),
                })
                .await;
            if let Err(TypedRequestError::Server { source, .. }) = &response
                && matches!(
                    source.code,
                    JSONRPC_INVALID_REQUEST | JSONRPC_INVALID_PARAMS
                )
                && source.message.contains("invalid type: sequence")
                && source.message.contains("expected a string")
                && let Some(ThreadListCwdFilter::Many(cwds)) = &params.cwd
                && let Some(cwd) = cwds.first()
            {
                // Repository discovery puts the originally requested CWD first.
                params.cwd = Some(ThreadListCwdFilter::One(cwd.clone()));
                continue;
            }
            return response.wrap_err("thread/list failed during TUI session lookup");
        }
    }
}
