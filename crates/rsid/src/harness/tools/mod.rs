//! Trait-based tool system for the agent harness.
//!
//! Each tool is a struct implementing `HarnessTool`. Built-in tools provide
//! file I/O, shell execution, and git operations. The registry collects tools
//! and dispatches execution by name.

pub mod file;
pub mod git;
pub mod memory;
pub mod shell;

use crate::harness::types::{HarnessToolSpec, ToolResult};
use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

/// Trait for tools executable by the agent harness.
#[async_trait::async_trait]
pub trait HarnessTool: Send + Sync {
    /// Tool name (must be unique within a registry).
    fn name(&self) -> &str;

    /// Human-readable description for the LLM.
    fn description(&self) -> &str;

    /// JSON Schema string for input parameters.
    fn parameters_json(&self) -> &str;

    /// Execute the tool with the given arguments.
    async fn execute(&self, args: serde_json::Value, working_dir: &Path) -> ToolResult;

    /// Convert to a HarnessToolSpec for provider API calls.
    fn to_spec(&self) -> HarnessToolSpec {
        HarnessToolSpec {
            name: self.name().to_string(),
            description: self.description().to_string(),
            parameters_json: self.parameters_json().to_string(),
        }
    }
}

/// Registry of available tools for the agent loop.
pub struct HarnessToolRegistry {
    tools: HashMap<String, Arc<dyn HarnessTool>>,
}

impl HarnessToolRegistry {
    pub fn new() -> Self {
        Self {
            tools: HashMap::new(),
        }
    }

    pub fn register(&mut self, tool: Arc<dyn HarnessTool>) {
        self.tools.insert(tool.name().to_string(), tool);
    }

    pub fn specs(&self) -> Vec<HarnessToolSpec> {
        self.tools.values().map(|t| t.to_spec()).collect()
    }

    pub async fn execute(
        &self,
        name: &str,
        args: serde_json::Value,
        working_dir: &Path,
    ) -> ToolResult {
        match self.tools.get(name) {
            Some(tool) => tool.execute(args, working_dir).await,
            None => ToolResult {
                success: false,
                output: String::new(),
                error_msg: Some(format!("Unknown tool: {name}")),
            },
        }
    }

    /// Build the default tool set for harness sessions.
    pub fn default_tools() -> Self {
        let mut registry = Self::new();
        registry.register(Arc::new(file::ReadFileTool));
        registry.register(Arc::new(file::WriteFileTool));
        registry.register(Arc::new(file::EditFileTool));
        registry.register(Arc::new(shell::ShellTool::default()));
        registry.register(Arc::new(git::GitTool::default()));
        registry
    }
}

impl Default for HarnessToolRegistry {
    fn default() -> Self {
        Self::new()
    }
}

/// System blocklist -- these paths are never writable even with wildcard allowed_paths.
const SYSTEM_BLOCKLIST: &[&str] = &[
    "/bin",
    "/sbin",
    "/usr/bin",
    "/usr/sbin",
    "/usr/lib",
    "/etc",
    "/dev",
    "/proc",
    "/sys",
    "/boot",
];

/// Check if a resolved path falls within the system blocklist.
pub fn is_system_blocked(path: &Path) -> bool {
    let path_str = path.to_string_lossy();
    SYSTEM_BLOCKLIST
        .iter()
        .any(|blocked| path_str.starts_with(blocked))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_is_system_blocked() {
        assert!(is_system_blocked(Path::new("/etc/passwd")));
        assert!(is_system_blocked(Path::new("/bin/bash")));
        assert!(is_system_blocked(Path::new("/usr/bin/ls")));
        assert!(is_system_blocked(Path::new("/proc/self/maps")));
        assert!(!is_system_blocked(Path::new("/home/user/project/file.rs")));
        assert!(!is_system_blocked(Path::new("/tmp/workdir/file.txt")));
    }

    #[test]
    fn test_registry_unknown_tool() {
        let registry = HarnessToolRegistry::new();
        let rt = tokio::runtime::Runtime::new().unwrap();
        let result = rt.block_on(registry.execute(
            "nonexistent",
            serde_json::Value::Null,
            Path::new("/tmp"),
        ));
        assert!(!result.success);
        assert!(result.error_msg.unwrap().contains("Unknown tool"));
    }

    #[test]
    fn test_default_tools_has_all_builtins() {
        let registry = HarnessToolRegistry::default_tools();
        let names: Vec<String> = registry.tools.keys().cloned().collect();
        assert!(names.contains(&"read_file".to_string()));
        assert!(names.contains(&"write_file".to_string()));
        assert!(names.contains(&"file_edit".to_string()));
        assert!(names.contains(&"shell".to_string()));
        assert!(names.contains(&"git".to_string()));
    }

    #[test]
    fn test_specs_returns_all() {
        let registry = HarnessToolRegistry::default_tools();
        let specs = registry.specs();
        assert_eq!(specs.len(), 5);
    }

    #[test]
    fn test_register_and_specs() {
        let mut registry = HarnessToolRegistry::new();
        assert_eq!(registry.specs().len(), 0);
        registry.register(Arc::new(file::ReadFileTool));
        assert_eq!(registry.specs().len(), 1);
        let spec = &registry.specs()[0];
        assert_eq!(spec.name, "read_file");
    }
}
