use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

use crate::data::DataSchema;

/// Type of context source available in the graph.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum ContextSourceType {
    /// Vector store for semantic search
    Memory,
    /// Workspace-aware file index
    FileIndex,
    /// Event log / scratchpad
    Blackboard,
    /// Bounded FIFO per-node message buffer
    ConversationHistory {
        max_messages: Option<usize>,
        max_tokens: Option<usize>,
    },
}

/// Metadata about a context source registered in the graph.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContextSource {
    /// Unique name for this source
    pub name: String,
    /// Human-readable description
    pub description: String,
    /// Type of source
    pub source_type: ContextSourceType,
    /// Schema of the data this source provides
    pub output_schema: Option<DataSchema>,
    /// Tags for categorization/filtering
    pub tags: Vec<String>,
    /// Whether this source supports retrieval queries
    pub retrievable: bool,
}

/// Registry of context sources available during graph execution.
/// Sources are registered at build time, not dynamically during execution.
#[derive(Debug, Clone, Default)]
pub struct ContextRegistry {
    sources: BTreeMap<String, ContextSource>,
}

impl ContextRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register(&mut self, source: ContextSource) {
        self.sources.insert(source.name.clone(), source);
    }

    pub fn get(&self, name: &str) -> Option<&ContextSource> {
        self.sources.get(name)
    }

    pub fn list_sources(&self) -> Vec<&ContextSource> {
        self.sources.values().collect()
    }

    pub fn list_retrievable(&self) -> Vec<&ContextSource> {
        self.sources.values().filter(|s| s.retrievable).collect()
    }

    pub fn list_by_type(&self, source_type: &ContextSourceType) -> Vec<&ContextSource> {
        self.sources
            .values()
            .filter(|s| {
                std::mem::discriminant(&s.source_type) == std::mem::discriminant(source_type)
            })
            .collect()
    }

    pub fn list_by_tag(&self, tag: &str) -> Vec<&ContextSource> {
        self.sources
            .values()
            .filter(|s| s.tags.contains(&tag.to_string()))
            .collect()
    }

    /// Create a scoped sub-registry with only explicitly imported sources.
    pub fn scoped(&self, import_names: &[String]) -> ContextRegistry {
        let mut scoped = ContextRegistry::new();
        for name in import_names {
            if let Some(source) = self.sources.get(name) {
                scoped.register(source.clone());
            }
        }
        scoped
    }

    pub fn len(&self) -> usize {
        self.sources.len()
    }

    pub fn is_empty(&self) -> bool {
        self.sources.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_source(
        name: &str,
        source_type: ContextSourceType,
        retrievable: bool,
        tags: Vec<&str>,
    ) -> ContextSource {
        ContextSource {
            name: name.to_string(),
            description: format!("{name} source"),
            source_type,
            output_schema: None,
            tags: tags.into_iter().map(|t| t.to_string()).collect(),
            retrievable,
        }
    }

    fn populated_registry() -> ContextRegistry {
        let mut reg = ContextRegistry::new();
        reg.register(make_source(
            "memory-main",
            ContextSourceType::Memory,
            true,
            vec!["search", "core"],
        ));
        reg.register(make_source(
            "file-idx",
            ContextSourceType::FileIndex,
            true,
            vec!["files", "core"],
        ));
        reg.register(make_source(
            "blackboard",
            ContextSourceType::Blackboard,
            false,
            vec!["scratch"],
        ));
        reg.register(make_source(
            "conv-hist",
            ContextSourceType::ConversationHistory {
                max_messages: Some(100),
                max_tokens: Some(8000),
            },
            false,
            vec!["history"],
        ));
        reg
    }

    #[test]
    fn register_and_list_sources() {
        let reg = populated_registry();
        assert_eq!(reg.len(), 4);
        assert!(!reg.is_empty());
        let sources = reg.list_sources();
        assert_eq!(sources.len(), 4);
    }

    #[test]
    fn list_retrievable_filters_correctly() {
        let reg = populated_registry();
        let retrievable = reg.list_retrievable();
        assert_eq!(retrievable.len(), 2);
        for s in &retrievable {
            assert!(s.retrievable);
        }
    }

    #[test]
    fn list_by_type_memory() {
        let reg = populated_registry();
        let memory = reg.list_by_type(&ContextSourceType::Memory);
        assert_eq!(memory.len(), 1);
        assert_eq!(memory[0].name, "memory-main");
    }

    #[test]
    fn list_by_type_file_index() {
        let reg = populated_registry();
        let files = reg.list_by_type(&ContextSourceType::FileIndex);
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].name, "file-idx");
    }

    #[test]
    fn list_by_type_conversation_history_ignores_fields() {
        let reg = populated_registry();
        // Should match by variant discriminant, ignoring inner fields
        let hist = reg.list_by_type(&ContextSourceType::ConversationHistory {
            max_messages: None,
            max_tokens: None,
        });
        assert_eq!(hist.len(), 1);
        assert_eq!(hist[0].name, "conv-hist");
    }

    #[test]
    fn list_by_tag() {
        let reg = populated_registry();
        let core = reg.list_by_tag("core");
        assert_eq!(core.len(), 2);
        let names: Vec<&str> = core.iter().map(|s| s.name.as_str()).collect();
        assert!(names.contains(&"memory-main"));
        assert!(names.contains(&"file-idx"));
    }

    #[test]
    fn scoped_creates_sub_registry() {
        let reg = populated_registry();
        let scoped = reg.scoped(&["memory-main".to_string(), "blackboard".to_string()]);
        assert_eq!(scoped.len(), 2);
        assert!(scoped.get("memory-main").is_some());
        assert!(scoped.get("blackboard").is_some());
    }

    #[test]
    fn scoped_does_not_leak_non_imported() {
        let reg = populated_registry();
        let scoped = reg.scoped(&["memory-main".to_string()]);
        assert_eq!(scoped.len(), 1);
        assert!(scoped.get("file-idx").is_none());
        assert!(scoped.get("blackboard").is_none());
        assert!(scoped.get("conv-hist").is_none());
    }

    #[test]
    fn context_source_serde_roundtrip() {
        let source = make_source(
            "test-src",
            ContextSourceType::ConversationHistory {
                max_messages: Some(50),
                max_tokens: None,
            },
            true,
            vec!["a", "b"],
        );

        let json = serde_json::to_string(&source).expect("serialize");
        let deserialized: ContextSource = serde_json::from_str(&json).expect("deserialize");

        assert_eq!(deserialized.name, "test-src");
        assert_eq!(
            deserialized.source_type,
            ContextSourceType::ConversationHistory {
                max_messages: Some(50),
                max_tokens: None,
            }
        );
        assert!(deserialized.retrievable);
        assert_eq!(deserialized.tags, vec!["a", "b"]);
    }

    #[test]
    fn empty_registry() {
        let reg = ContextRegistry::new();
        assert!(reg.is_empty());
        assert_eq!(reg.len(), 0);
        assert!(reg.list_sources().is_empty());
        assert!(reg.list_retrievable().is_empty());
    }

    #[test]
    fn get_returns_registered_source() {
        let reg = populated_registry();
        let src = reg.get("memory-main").expect("should exist");
        assert_eq!(src.name, "memory-main");
        assert!(reg.get("nonexistent").is_none());
    }

    #[test]
    fn scoped_ignores_unknown_names() {
        let reg = populated_registry();
        let scoped = reg.scoped(&["nonexistent".to_string(), "memory-main".to_string()]);
        assert_eq!(scoped.len(), 1);
        assert!(scoped.get("memory-main").is_some());
    }
}
