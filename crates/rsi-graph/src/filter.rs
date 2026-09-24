use serde::{Deserialize, Serialize};

use crate::data::{DataSchema, NodeData};
use crate::edge::EdgeId;
use crate::error::GraphError;

/// Field-level filtering for edge data.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum FieldFilter {
    /// Only pass through the listed field names.
    Include(Vec<String>),
    /// Pass through everything except the listed field names.
    Exclude(Vec<String>),
}

impl FieldFilter {
    /// Apply this filter to NodeData, returning a new NodeData with only the
    /// selected fields.
    pub fn apply(&self, data: &NodeData) -> NodeData {
        let mut result = NodeData::new();
        match self {
            FieldFilter::Include(names) => {
                for name in names {
                    if let Some(value) = data.get(name) {
                        result.insert(name.clone(), value.clone());
                    }
                }
            }
            FieldFilter::Exclude(names) => {
                for (key, value) in data.iter() {
                    if !names.contains(key) {
                        result.insert(key.clone(), value.clone());
                    }
                }
            }
        }
        result
    }
}

/// Strategy for merging data from multiple incoming edges at a target node.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum MergeStrategy {
    /// Last-write-wins merge of all incoming data.
    Union,
    /// Error if any field names collide across incoming edges.
    Disjoint,
    /// Merge in priority order (first EdgeId has highest priority).
    Priority(Vec<EdgeId>),
}

impl MergeStrategy {
    /// Merge multiple NodeData according to this strategy.
    pub fn merge(&self, inputs: Vec<(EdgeId, NodeData)>) -> Result<NodeData, GraphError> {
        match self {
            MergeStrategy::Union => {
                let mut result = NodeData::new();
                for (_edge_id, data) in inputs {
                    result.merge(data);
                }
                Ok(result)
            }
            MergeStrategy::Disjoint => {
                let mut result = NodeData::new();
                for (edge_id, data) in inputs {
                    for (key, value) in data.iter() {
                        if result.contains_key(key) {
                            return Err(GraphError::FilterError(format!(
                                "field collision on '{}' from edge '{}'",
                                key, edge_id,
                            )));
                        }
                        result.insert(key.clone(), value.clone());
                    }
                }
                Ok(result)
            }
            MergeStrategy::Priority(priority_order) => {
                // Merge in reverse priority order so highest-priority (first in
                // list) overwrites lowest-priority (last in list).
                let mut result = NodeData::new();

                // First, merge any inputs not in the priority list (lowest priority).
                for (edge_id, data) in &inputs {
                    if !priority_order.contains(edge_id) {
                        result.merge(data.clone());
                    }
                }

                // Then merge in reverse priority order.
                let input_map: std::collections::HashMap<&EdgeId, &NodeData> =
                    inputs.iter().map(|(id, data)| (id, data)).collect();

                for edge_id in priority_order.iter().rev() {
                    if let Some(data) = input_map.get(edge_id) {
                        result.merge((*data).clone());
                    }
                }

                Ok(result)
            }
        }
    }
}

/// Advisory validation: check if filter references fields in the source schema.
pub struct FilterValidation;

impl FilterValidation {
    /// Returns warnings for filter fields not present in the source schema's
    /// output fields.
    pub fn validate(filter: &FieldFilter, schema: &DataSchema) -> Vec<String> {
        let schema_field_names: Vec<&str> =
            schema.outputs.iter().map(|f| f.name.as_str()).collect();

        let filter_fields = match filter {
            FieldFilter::Include(names) => names,
            FieldFilter::Exclude(names) => names,
        };

        let mut warnings = Vec::new();
        for field in filter_fields {
            if !schema_field_names.contains(&field.as_str()) {
                warnings.push(format!(
                    "filter references field '{}' not found in schema outputs",
                    field,
                ));
            }
        }
        warnings
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data::{FieldDeclaration, FieldType, Value};

    fn make_abcd_data() -> NodeData {
        let mut data = NodeData::new();
        data.insert("a", Value::Number(1.0));
        data.insert("b", Value::Number(2.0));
        data.insert("c", Value::Number(3.0));
        data.insert("d", Value::Number(4.0));
        data
    }

    #[test]
    fn include_filter_keeps_named_fields() {
        let filter = FieldFilter::Include(vec!["a".into(), "c".into()]);
        let result = filter.apply(&make_abcd_data());

        assert_eq!(result.len(), 2);
        assert_eq!(result.get("a"), Some(&Value::Number(1.0)));
        assert_eq!(result.get("c"), Some(&Value::Number(3.0)));
        assert!(result.get("b").is_none());
        assert!(result.get("d").is_none());
    }

    #[test]
    fn exclude_filter_removes_named_fields() {
        let filter = FieldFilter::Exclude(vec!["b".into(), "d".into()]);
        let result = filter.apply(&make_abcd_data());

        assert_eq!(result.len(), 2);
        assert_eq!(result.get("a"), Some(&Value::Number(1.0)));
        assert_eq!(result.get("c"), Some(&Value::Number(3.0)));
        assert!(result.get("b").is_none());
        assert!(result.get("d").is_none());
    }

    #[test]
    fn include_filter_missing_fields_does_not_crash() {
        let filter =
            FieldFilter::Include(vec!["a".into(), "missing".into(), "also_missing".into()]);
        let result = filter.apply(&make_abcd_data());

        assert_eq!(result.len(), 1);
        assert_eq!(result.get("a"), Some(&Value::Number(1.0)));
    }

    #[test]
    fn union_merge_last_write_wins() {
        let mut data1 = NodeData::new();
        data1.insert("x", Value::Number(1.0));
        data1.insert("shared", Value::String("from_e1".into()));

        let mut data2 = NodeData::new();
        data2.insert("y", Value::Number(2.0));
        data2.insert("shared", Value::String("from_e2".into()));

        let inputs = vec![(EdgeId::new("e1"), data1), (EdgeId::new("e2"), data2)];

        let result = MergeStrategy::Union.merge(inputs).unwrap();
        assert_eq!(result.get("x"), Some(&Value::Number(1.0)));
        assert_eq!(result.get("y"), Some(&Value::Number(2.0)));
        // Last write wins: e2 overwrites e1's "shared"
        assert_eq!(result.get("shared"), Some(&Value::String("from_e2".into())));
    }

    #[test]
    fn disjoint_merge_errors_on_collision() {
        let mut data1 = NodeData::new();
        data1.insert("x", Value::Number(1.0));
        data1.insert("shared", Value::String("from_e1".into()));

        let mut data2 = NodeData::new();
        data2.insert("shared", Value::String("from_e2".into()));

        let inputs = vec![(EdgeId::new("e1"), data1), (EdgeId::new("e2"), data2)];

        let err = MergeStrategy::Disjoint.merge(inputs).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("field collision"));
        assert!(msg.contains("shared"));
    }

    #[test]
    fn disjoint_merge_succeeds_without_collision() {
        let mut data1 = NodeData::new();
        data1.insert("x", Value::Number(1.0));

        let mut data2 = NodeData::new();
        data2.insert("y", Value::Number(2.0));

        let inputs = vec![(EdgeId::new("e1"), data1), (EdgeId::new("e2"), data2)];

        let result = MergeStrategy::Disjoint.merge(inputs).unwrap();
        assert_eq!(result.get("x"), Some(&Value::Number(1.0)));
        assert_eq!(result.get("y"), Some(&Value::Number(2.0)));
    }

    #[test]
    fn priority_merge_higher_priority_wins() {
        let mut data1 = NodeData::new();
        data1.insert("shared", Value::String("high_priority".into()));
        data1.insert("only_e1", Value::Number(1.0));

        let mut data2 = NodeData::new();
        data2.insert("shared", Value::String("low_priority".into()));
        data2.insert("only_e2", Value::Number(2.0));

        let inputs = vec![(EdgeId::new("e1"), data1), (EdgeId::new("e2"), data2)];

        // e1 is first in priority list = highest priority
        let strategy = MergeStrategy::Priority(vec![EdgeId::new("e1"), EdgeId::new("e2")]);
        let result = strategy.merge(inputs).unwrap();

        assert_eq!(
            result.get("shared"),
            Some(&Value::String("high_priority".into()))
        );
        assert_eq!(result.get("only_e1"), Some(&Value::Number(1.0)));
        assert_eq!(result.get("only_e2"), Some(&Value::Number(2.0)));
    }

    #[test]
    fn filter_validation_warns_on_unknown_fields() {
        let schema = DataSchema {
            inputs: vec![],
            outputs: vec![
                FieldDeclaration {
                    name: "a".into(),
                    field_type: FieldType::Number,
                    description: None,
                    required: true,
                },
                FieldDeclaration {
                    name: "b".into(),
                    field_type: FieldType::Number,
                    description: None,
                    required: false,
                },
            ],
        };

        let filter =
            FieldFilter::Include(vec!["a".into(), "unknown".into(), "also_unknown".into()]);
        let warnings = FilterValidation::validate(&filter, &schema);

        assert_eq!(warnings.len(), 2);
        assert!(warnings[0].contains("unknown"));
        assert!(warnings[1].contains("also_unknown"));
    }

    #[test]
    fn filter_validation_no_warnings_when_fields_exist() {
        let schema = DataSchema {
            inputs: vec![],
            outputs: vec![FieldDeclaration {
                name: "a".into(),
                field_type: FieldType::Number,
                description: None,
                required: true,
            }],
        };

        let filter = FieldFilter::Include(vec!["a".into()]);
        let warnings = FilterValidation::validate(&filter, &schema);
        assert!(warnings.is_empty());
    }
}
