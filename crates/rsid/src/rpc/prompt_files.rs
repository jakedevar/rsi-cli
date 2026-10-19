//! hub #3040 (local #74): the operator prompt-file RPCs behind the TUI prompt
//! creator and, later, the desktop one.
//!
//! `ListPromptFiles`, `GetPromptFile`, `PutPromptFile` and `DeletePromptFile`
//! are operator-only: they are not in the attributed verb registry, so a
//! tokened caller is default-denied (AGENTS.md rule 10), and the rsi-remote
//! gateway never calls them (it only speaks the `Remote*V1/V2` methods). They
//! are scoped to `<data dir>/prompts` (`RpcServer::prompt_data_dir`, the rsi
//! data directory); the file work is in `crate::prompt_files`.

use super::*;
use crate::prompt_files as prompts;
use rsi_common::prompt_files::{
    CODE_INVALID_REQUEST, DeletePromptFileRequestV1, GetPromptFileRequestV1,
    ListPromptFilesRequestV1, PutPromptFileRequestV1,
};

fn decode<T: serde::de::DeserializeOwned>(request: &RpcRequest) -> Result<T> {
    let params = if request.params.is_null() {
        serde_json::json!({})
    } else {
        request.params.clone()
    };
    serde_json::from_value(params).map_err(|e| prompts::refuse(CODE_INVALID_REQUEST, e))
}

/// Run blocking prompt-file I/O off the async runtime.
async fn blocking<T: Send + 'static>(
    work: impl FnOnce() -> Result<T> + Send + 'static,
) -> Result<serde_json::Value>
where
    T: serde::Serialize,
{
    let value = tokio::task::spawn_blocking(work)
        .await
        .map_err(|e| DaemonError::Process(e.to_string()))??;
    Ok(serde_json::to_value(value)?)
}

impl RpcServer {
    pub(super) async fn handle_list_prompt_files(
        &self,
        request: &RpcRequest,
    ) -> Result<serde_json::Value> {
        let ListPromptFilesRequestV1 {} = decode(request)?;
        let data_dir = self.prompt_data_dir.clone();
        blocking(move || prompts::list(&data_dir)).await
    }

    pub(super) async fn handle_get_prompt_file(
        &self,
        request: &RpcRequest,
    ) -> Result<serde_json::Value> {
        let params: GetPromptFileRequestV1 = decode(request)?;
        let data_dir = self.prompt_data_dir.clone();
        blocking(move || prompts::get(&data_dir, &params.name)).await
    }

    pub(super) async fn handle_put_prompt_file(
        &self,
        request: &RpcRequest,
    ) -> Result<serde_json::Value> {
        let params: PutPromptFileRequestV1 = decode(request)?;
        let data_dir = self.prompt_data_dir.clone();
        blocking(move || prompts::put(&data_dir, &params.name, &params.content, params.create_only))
            .await
    }

    pub(super) async fn handle_delete_prompt_file(
        &self,
        request: &RpcRequest,
    ) -> Result<serde_json::Value> {
        let params: DeletePromptFileRequestV1 = decode(request)?;
        let data_dir = self.prompt_data_dir.clone();
        blocking(move || prompts::delete(&data_dir, &params.name)).await
    }
}
