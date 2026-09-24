use thiserror::Error;

#[derive(Debug, Error)]
pub enum GraphError {
    #[error("node not found: {0}")]
    NodeNotFound(String),
    #[error("edge not found: {0}")]
    EdgeNotFound(String),
    #[error("schema validation failed: {0}")]
    SchemaValidation(String),
    #[error("execution failed: {0}")]
    ExecutionFailed(String),
    #[error("type conversion failed: expected {expected}, got {got}")]
    TypeConversion { expected: String, got: String },
    #[error("state error: {0}")]
    StateError(String),
    #[error("hook error: {0}")]
    HookError(String),
    #[error("filter error: {0}")]
    FilterError(String),
    #[error("cycle detected in graph")]
    CycleDetected,
    #[error("max nesting depth exceeded: {0}")]
    MaxNestingDepth(usize),
}
