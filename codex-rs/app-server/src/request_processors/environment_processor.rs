use super::*;

#[derive(Clone)]
pub(crate) struct EnvironmentRequestProcessor {
    environment_manager: Arc<EnvironmentManager>,
}

impl EnvironmentRequestProcessor {
    pub(crate) fn new(environment_manager: Arc<EnvironmentManager>) -> Self {
        Self {
            environment_manager,
        }
    }

    pub(crate) async fn environment_add(
        &self,
        _params: EnvironmentAddParams,
    ) -> Result<Option<ClientResponsePayload>, JSONRPCErrorError> {
        Err(invalid_request(
            "remote execution environments are not supported",
        ))
    }

    pub(crate) async fn environment_info(
        &self,
        params: EnvironmentInfoParams,
    ) -> Result<Option<ClientResponsePayload>, JSONRPCErrorError> {
        let environment_id = params.environment_id;
        let environment = self
            .environment_manager
            .get_environment(&environment_id)
            .ok_or_else(|| invalid_request(format!("unknown environment id `{environment_id}`")))?;
        let info = environment.force_info().await.map_err(|err| {
            internal_error(format!(
                "failed to get info for environment `{environment_id}`: {err}"
            ))
        })?;
        Ok(Some(
            EnvironmentInfoResponse {
                shell: EnvironmentShellInfo {
                    name: info.shell.name,
                    path: info.shell.path,
                },
                cwd: info.cwd,
            }
            .into(),
        ))
    }

    pub(crate) async fn environment_status(
        &self,
        params: EnvironmentStatusParams,
    ) -> Result<Option<ClientResponsePayload>, JSONRPCErrorError> {
        let environment_id = params.environment_id;
        let (status, error) = match self
            .environment_manager
            .get_environment_status(&environment_id)
            .await
        {
            Some(EnvironmentObservedStatus::Ready) => (EnvironmentStatusKind::Ready, None),
            Some(EnvironmentObservedStatus::Pending) => (EnvironmentStatusKind::Pending, None),
            Some(EnvironmentObservedStatus::Disconnected { error }) => {
                (EnvironmentStatusKind::Disconnected, Some(error))
            }
            None => (
                EnvironmentStatusKind::Unknown,
                Some(format!("unknown environment id `{environment_id}`")),
            ),
        };
        Ok(Some(EnvironmentStatusResponse { status, error }.into()))
    }
}
