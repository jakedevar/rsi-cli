use std::fmt;

use serde::{Deserialize, Serialize};

use crate::data::NodeData;
use crate::filter::FieldFilter;
use crate::node::NodeId;

/// Unique identifier for an edge within a graph.
#[derive(Clone, Debug, Hash, Eq, PartialEq, Serialize, Deserialize)]
pub struct EdgeId(String);

impl EdgeId {
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for EdgeId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// A directed edge connecting two nodes in the graph.
///
/// The `filter` field optionally restricts which data fields flow through
/// this edge to the target node.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Edge {
    pub id: EdgeId,
    pub source: NodeId,
    pub target: NodeId,
    pub data: Option<NodeData>,
    /// Optional field-level filter applied to data flowing through this edge.
    pub filter: Option<FieldFilter>,
}

impl Edge {
    pub fn new(
        id: impl Into<String>,
        source: impl Into<String>,
        target: impl Into<String>,
    ) -> Self {
        Self {
            id: EdgeId::new(id),
            source: NodeId::new(source),
            target: NodeId::new(target),
            data: None,
            filter: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data::Value;

    #[test]
    fn edge_new_defaults() {
        let edge = Edge::new("e1", "src", "dst");
        assert_eq!(edge.id.as_str(), "e1");
        assert_eq!(edge.source.as_str(), "src");
        assert_eq!(edge.target.as_str(), "dst");
        assert!(edge.data.is_none());
        assert!(edge.filter.is_none());
    }

    #[test]
    fn edge_with_data() {
        let mut edge = Edge::new("e2", "a", "b");
        let mut data = NodeData::new();
        data.insert("weight", Value::Number(0.5));
        edge.data = Some(data);

        assert!(edge.data.is_some());
        let d = edge.data.as_ref().unwrap();
        assert_eq!(d.get("weight"), Some(&Value::Number(0.5)));
    }

    #[test]
    fn edge_id_display() {
        let id = EdgeId::new("edge-42");
        assert_eq!(id.to_string(), "edge-42");
    }
}
