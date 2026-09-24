//! Native read-only Codegraph tools bound to one durable session identity.

use std::path::Path;
use std::sync::Arc;

use tokio::sync::Mutex;
use uuid::Uuid;

use super::HarnessTool;
use crate::codegraph::{
    IndexHandle, NativeCodegraphBinding, NativeCodegraphToolKind, execute_native_read,
};
use crate::session::harness::types::ToolResult;
use crate::store::Store;

pub struct CodegraphTool {
    kind: NativeCodegraphToolKind,
    handle: IndexHandle,
    store: Arc<Mutex<Store>>,
    session_id: Uuid,
    binding: NativeCodegraphBinding,
    schema: String,
}

impl CodegraphTool {
    pub fn new(
        kind: NativeCodegraphToolKind,
        handle: IndexHandle,
        store: Arc<Mutex<Store>>,
        session_id: Uuid,
        binding: NativeCodegraphBinding,
    ) -> Self {
        Self {
            kind,
            handle,
            store,
            session_id,
            binding,
            schema: kind.schema().to_string(),
        }
    }
}

#[async_trait::async_trait]
impl HarnessTool for CodegraphTool {
    fn name(&self) -> &str {
        self.kind.name()
    }

    fn description(&self) -> &str {
        self.kind.description()
    }

    fn parameters_json(&self) -> &str {
        &self.schema
    }

    async fn execute(&self, args: serde_json::Value, _working_dir: &Path) -> ToolResult {
        match execute_native_read(
            self.handle.clone(),
            Arc::clone(&self.store),
            self.session_id,
            self.binding.clone(),
            self.kind,
            args,
        )
        .await
        {
            Ok(value) => ToolResult {
                success: true,
                output: value.to_string(),
                error_msg: None,
            },
            Err(error) => ToolResult {
                success: false,
                output: String::new(),
                error_msg: Some(error.to_string()),
            },
        }
    }
}
