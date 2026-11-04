//! Filesystem RPCs on the app-server host, usable by sessions and background requests.

use super::AppServerSession;
use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use codex_app_server_client::AppServerPath;
use codex_app_server_client::AppServerRequestHandle;
use codex_app_server_protocol::ClientRequest;
use codex_app_server_protocol::FsCreateDirectoryParams;
use codex_app_server_protocol::FsCreateDirectoryResponse;
use codex_app_server_protocol::FsReadFileParams;
use codex_app_server_protocol::FsReadFileResponse;
use codex_app_server_protocol::FsRemoveParams;
use codex_app_server_protocol::FsRemoveResponse;
use codex_app_server_protocol::FsWriteFileParams;
use codex_app_server_protocol::FsWriteFileResponse;
use codex_app_server_protocol::RequestId;
use codex_utils_absolute_path::AbsolutePathBuf;
use color_eyre::eyre::Result;
use color_eyre::eyre::WrapErr;
use serde::de::DeserializeOwned;
use uuid::Uuid;

impl AppServerSession {
    pub(crate) fn file_system(&self) -> AppServerFileSystem {
        AppServerFileSystem {
            request_handle: self.request_handle(),
        }
    }
}

pub(crate) struct AppServerFileSystem {
    pub(crate) request_handle: AppServerRequestHandle,
}

impl AppServerFileSystem {
    pub(crate) async fn fs_create_directory_all_path(&self, path: &AppServerPath) -> Result<()> {
        self.request_fs_path::<FsCreateDirectoryResponse>(
            "fs/createDirectory",
            path,
            |request_id, path| ClientRequest::FsCreateDirectory {
                request_id,
                params: FsCreateDirectoryParams {
                    path,
                    recursive: Some(true),
                },
            },
        )
        .await
        .map(drop)
    }

    pub(crate) async fn fs_write_file_path(
        &self,
        path: &AppServerPath,
        bytes: Vec<u8>,
    ) -> Result<()> {
        let data_base64 = STANDARD.encode(bytes);
        self.request_fs_path::<FsWriteFileResponse>("fs/writeFile", path, |request_id, path| {
            ClientRequest::FsWriteFile {
                request_id,
                params: FsWriteFileParams {
                    path,
                    data_base64: data_base64.clone(),
                },
            }
        })
        .await
        .map(drop)
    }

    pub(crate) async fn fs_read_file_path(&self, path: &AppServerPath) -> Result<Vec<u8>> {
        let response: FsReadFileResponse = self
            .request_fs_path("fs/readFile", path, |request_id, path| {
                ClientRequest::FsReadFile {
                    request_id,
                    params: FsReadFileParams { path },
                }
            })
            .await?;
        STANDARD
            .decode(response.data_base64)
            .wrap_err("fs/readFile returned invalid base64 data")
    }

    pub(crate) async fn fs_remove_path(&self, path: &AppServerPath) -> Result<()> {
        self.request_fs_path::<FsRemoveResponse>("fs/remove", path, |request_id, path| {
            ClientRequest::FsRemove {
                request_id,
                params: FsRemoveParams {
                    path,
                    recursive: None,
                    force: None,
                },
            }
        })
        .await
        .map(drop)
    }

    async fn request_fs_path<T: DeserializeOwned>(
        &self,
        method: &str,
        path: &AppServerPath,
        local_request: impl FnOnce(RequestId, AbsolutePathBuf) -> ClientRequest,
    ) -> Result<T> {
        let request_id = RequestId::String(format!("tui-fs-{}", Uuid::new_v4()));
        match &self.request_handle {
            AppServerRequestHandle::InProcess(_) => {
                let path = AbsolutePathBuf::from_absolute_path_checked(path.as_str())
                    .wrap_err_with(|| format!("invalid local app-server fs path {path}"))?;
                self.request_handle
                    .request_typed(local_request(request_id, path))
                    .await
                    .wrap_err_with(|| format!("{method} failed in TUI"))
            }
        }
    }
}
