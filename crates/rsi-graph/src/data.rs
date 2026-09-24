use std::collections::BTreeMap;
use std::fmt;

use serde::{Deserialize, Serialize};

/// A dynamically-typed value used in node data fields.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Value {
    Null,
    Bool(bool),
    Number(f64),
    String(String),
    List(Vec<Value>),
    Map(BTreeMap<String, Value>),
}

impl fmt::Display for Value {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Value::Null => write!(f, "null"),
            Value::Bool(b) => write!(f, "{b}"),
            Value::Number(n) => write!(f, "{n}"),
            Value::String(s) => write!(f, "{s}"),
            Value::List(items) => {
                write!(f, "[")?;
                for (i, item) in items.iter().enumerate() {
                    if i > 0 {
                        write!(f, ", ")?;
                    }
                    write!(f, "{item}")?;
                }
                write!(f, "]")
            }
            Value::Map(map) => {
                write!(f, "{{")?;
                for (i, (k, v)) in map.iter().enumerate() {
                    if i > 0 {
                        write!(f, ", ")?;
                    }
                    write!(f, "{k}: {v}")?;
                }
                write!(f, "}}")
            }
        }
    }
}

/// A structured data bag associated with a node, backed by a `BTreeMap` for
/// deterministic key ordering.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct NodeData {
    fields: BTreeMap<String, Value>,
}

impl NodeData {
    pub fn new() -> Self {
        Self {
            fields: BTreeMap::new(),
        }
    }

    pub fn insert(&mut self, key: impl Into<String>, value: Value) -> Option<Value> {
        self.fields.insert(key.into(), value)
    }

    pub fn get(&self, key: &str) -> Option<&Value> {
        self.fields.get(key)
    }

    pub fn get_mut(&mut self, key: &str) -> Option<&mut Value> {
        self.fields.get_mut(key)
    }

    pub fn remove(&mut self, key: &str) -> Option<Value> {
        self.fields.remove(key)
    }

    pub fn contains_key(&self, key: &str) -> bool {
        self.fields.contains_key(key)
    }

    pub fn keys(&self) -> impl Iterator<Item = &String> {
        self.fields.keys()
    }

    pub fn len(&self) -> usize {
        self.fields.len()
    }

    pub fn is_empty(&self) -> bool {
        self.fields.is_empty()
    }

    pub fn iter(&self) -> impl Iterator<Item = (&String, &Value)> {
        self.fields.iter()
    }

    /// Merge another `NodeData` into this one. Last-write-wins: keys from
    /// `other` overwrite existing keys in `self`.
    pub fn merge(&mut self, other: NodeData) {
        self.fields.extend(other.fields);
    }
}

impl Default for NodeData {
    fn default() -> Self {
        Self::new()
    }
}

/// The type of a field in a schema declaration.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum FieldType {
    String,
    Number,
    Bool,
    List,
    Map,
    Any,
}

/// A single field declaration within a [`DataSchema`].
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct FieldDeclaration {
    pub name: String,
    pub field_type: FieldType,
    pub description: Option<String>,
    pub required: bool,
}

/// Schema describing the expected inputs and outputs of a node.
/// Validation is advisory — it returns a list of issues rather than failing.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct DataSchema {
    pub inputs: Vec<FieldDeclaration>,
    pub outputs: Vec<FieldDeclaration>,
}

impl DataSchema {
    /// Validate `data` against the input field declarations.
    /// Returns a list of human-readable issues (empty if valid).
    pub fn validate_input(&self, data: &NodeData) -> Vec<String> {
        Self::validate_fields(&self.inputs, data)
    }

    /// Validate `data` against the output field declarations.
    /// Returns a list of human-readable issues (empty if valid).
    pub fn validate_output(&self, data: &NodeData) -> Vec<String> {
        Self::validate_fields(&self.outputs, data)
    }

    fn validate_fields(fields: &[FieldDeclaration], data: &NodeData) -> Vec<String> {
        let mut issues = Vec::new();

        for decl in fields {
            match data.get(&decl.name) {
                None => {
                    if decl.required {
                        issues.push(format!("missing required field: {}", decl.name));
                    }
                }
                Some(value) => {
                    if !Self::type_matches(&decl.field_type, value) {
                        issues.push(format!(
                            "field '{}' expected type {:?}, got {}",
                            decl.name, decl.field_type, value
                        ));
                    }
                }
            }
        }

        issues
    }

    fn type_matches(field_type: &FieldType, value: &Value) -> bool {
        match field_type {
            FieldType::Any => true,
            FieldType::String => matches!(value, Value::String(_)),
            FieldType::Number => matches!(value, Value::Number(_)),
            FieldType::Bool => matches!(value, Value::Bool(_)),
            FieldType::List => matches!(value, Value::List(_)),
            FieldType::Map => matches!(value, Value::Map(_)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn node_data_insert_get_remove() {
        let mut data = NodeData::new();
        assert!(data.is_empty());

        data.insert("name", Value::String("test".into()));
        data.insert("count", Value::Number(42.0));
        assert_eq!(data.len(), 2);
        assert!(data.contains_key("name"));

        assert_eq!(data.get("name"), Some(&Value::String("test".into())));
        assert_eq!(data.get("missing"), None);

        let removed = data.remove("count");
        assert_eq!(removed, Some(Value::Number(42.0)));
        assert_eq!(data.len(), 1);
    }

    #[test]
    fn node_data_merge_last_write_wins() {
        let mut a = NodeData::new();
        a.insert("x", Value::Number(1.0));
        a.insert("y", Value::Number(2.0));

        let mut b = NodeData::new();
        b.insert("y", Value::Number(99.0));
        b.insert("z", Value::Number(3.0));

        a.merge(b);
        assert_eq!(a.get("x"), Some(&Value::Number(1.0)));
        assert_eq!(a.get("y"), Some(&Value::Number(99.0)));
        assert_eq!(a.get("z"), Some(&Value::Number(3.0)));
    }

    #[test]
    fn value_display() {
        assert_eq!(Value::Null.to_string(), "null");
        assert_eq!(Value::Bool(true).to_string(), "true");
        assert_eq!(Value::Number(2.5).to_string(), "2.5");
        assert_eq!(Value::String("hello".into()).to_string(), "hello");
        assert_eq!(
            Value::List(vec![Value::Number(1.0), Value::Number(2.0)]).to_string(),
            "[1, 2]"
        );
    }

    #[test]
    fn value_variants() {
        let map = BTreeMap::from([("a".to_string(), Value::Bool(false))]);
        let v = Value::Map(map);
        assert_eq!(v.to_string(), "{a: false}");
    }

    #[test]
    fn schema_validate_input_required_missing() {
        let schema = DataSchema {
            inputs: vec![FieldDeclaration {
                name: "prompt".into(),
                field_type: FieldType::String,
                description: Some("The prompt".into()),
                required: true,
            }],
            outputs: vec![],
        };

        let data = NodeData::new();
        let issues = schema.validate_input(&data);
        assert_eq!(issues.len(), 1);
        assert!(issues[0].contains("missing required field"));
    }

    #[test]
    fn schema_validate_input_wrong_type() {
        let schema = DataSchema {
            inputs: vec![FieldDeclaration {
                name: "count".into(),
                field_type: FieldType::Number,
                description: None,
                required: true,
            }],
            outputs: vec![],
        };

        let mut data = NodeData::new();
        data.insert("count", Value::String("not a number".into()));
        let issues = schema.validate_input(&data);
        assert_eq!(issues.len(), 1);
        assert!(issues[0].contains("expected type"));
    }

    #[test]
    fn schema_validate_input_valid() {
        let schema = DataSchema {
            inputs: vec![
                FieldDeclaration {
                    name: "name".into(),
                    field_type: FieldType::String,
                    description: None,
                    required: true,
                },
                FieldDeclaration {
                    name: "optional".into(),
                    field_type: FieldType::Any,
                    description: None,
                    required: false,
                },
            ],
            outputs: vec![],
        };

        let mut data = NodeData::new();
        data.insert("name", Value::String("valid".into()));
        let issues = schema.validate_input(&data);
        assert!(issues.is_empty());
    }

    #[test]
    fn schema_validate_output() {
        let schema = DataSchema {
            inputs: vec![],
            outputs: vec![FieldDeclaration {
                name: "result".into(),
                field_type: FieldType::Bool,
                description: None,
                required: true,
            }],
        };

        let mut data = NodeData::new();
        data.insert("result", Value::Bool(true));
        assert!(schema.validate_output(&data).is_empty());
    }

    #[test]
    fn node_data_get_mut() {
        let mut data = NodeData::new();
        data.insert("val", Value::Number(1.0));

        if let Some(v) = data.get_mut("val") {
            *v = Value::Number(2.0);
        }

        assert_eq!(data.get("val"), Some(&Value::Number(2.0)));
    }

    #[test]
    fn node_data_keys_and_iter() {
        let mut data = NodeData::new();
        data.insert("b", Value::Null);
        data.insert("a", Value::Null);

        let keys: Vec<&String> = data.keys().collect();
        // BTreeMap gives sorted order
        assert_eq!(keys, vec!["a", "b"]);

        let pairs: Vec<_> = data.iter().collect();
        assert_eq!(pairs.len(), 2);
    }
}
