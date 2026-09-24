//! Pure gate evaluation over upstream typed outputs (#635, plan §3).
//!
//! The condition is a closed JSON AST; paths are `nodes.<id>.<field>…` over
//! the recorded outputs of ancestor nodes only (validated statically). A
//! missing path or a type mismatch is an error, which fails the gate; it is
//! never coerced to `false`.

use rsi_common::types::GateCondition;
use serde_json::{Map, Value};

/// Digest of the exact condition a gate evaluated.
pub(crate) fn condition_digest(condition: &GateCondition) -> String {
    crate::topology::store::digest(&serde_json::to_string(condition).unwrap_or_default())
}

fn resolve<'a>(nodes: &'a Map<String, Value>, path: &str) -> Option<&'a Value> {
    let mut segments = path.split('.').skip(1);
    let mut current = nodes.get(segments.next()?)?;
    for segment in segments {
        current = current.as_object()?.get(segment)?;
    }
    (!current.is_null()).then_some(current)
}

fn required<'a>(nodes: &'a Map<String, Value>, path: &str) -> Result<&'a Value, String> {
    resolve(nodes, path).ok_or_else(|| format!("gate path {path} is missing"))
}

fn equal(left: &Value, right: &Value) -> bool {
    match (left.as_f64(), right.as_f64()) {
        (Some(left), Some(right)) if left.is_finite() && right.is_finite() => {
            left.total_cmp(&right).is_eq()
        }
        _ => left == right,
    }
}

fn number(nodes: &Map<String, Value>, path: &str) -> Result<f64, String> {
    required(nodes, path)?
        .as_f64()
        .ok_or_else(|| format!("gate path {path} is not a number"))
}

/// Evaluate `condition` over `nodes` (`{node_id: output}`).
pub(crate) fn evaluate(
    condition: &GateCondition,
    nodes: &Map<String, Value>,
) -> Result<bool, String> {
    Ok(match condition {
        GateCondition::Eq { path, value } => equal(required(nodes, path)?, value),
        GateCondition::Ne { path, value } => !equal(required(nodes, path)?, value),
        GateCondition::In { path, values } => {
            let actual = required(nodes, path)?;
            values.iter().any(|value| equal(actual, value))
        }
        GateCondition::Exists { path } => resolve(nodes, path).is_some(),
        GateCondition::Lt { path, value } => number(nodes, path)? < *value,
        GateCondition::Le { path, value } => number(nodes, path)? <= *value,
        GateCondition::Gt { path, value } => number(nodes, path)? > *value,
        GateCondition::Ge { path, value } => number(nodes, path)? >= *value,
        GateCondition::And { args } => {
            for arg in args {
                if !evaluate(arg, nodes)? {
                    return Ok(false);
                }
            }
            true
        }
        GateCondition::Or { args } => {
            for arg in args {
                if evaluate(arg, nodes)? {
                    return Ok(true);
                }
            }
            false
        }
        GateCondition::Not { arg } => !evaluate(arg, nodes)?,
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn nodes() -> Map<String, Value> {
        serde_json::from_value(serde_json::json!({
            "check": {"exit_code": 0, "op": "cargo_check_crate"},
            "a": {"handoff": {"status": "complete"}, "changed": true},
        }))
        .unwrap()
    }

    fn cond(value: Value) -> GateCondition {
        serde_json::from_value(value).unwrap()
    }

    #[test]
    fn closed_ast_evaluates_and_type_errors_fail() {
        let nodes = nodes();
        let ok = cond(serde_json::json!({"op": "and", "args": [
            {"op": "eq", "path": "nodes.check.exit_code", "value": 0.0},
            {"op": "in", "path": "nodes.a.handoff.status", "values": ["complete"]},
            {"op": "not", "arg": {"op": "exists", "path": "nodes.a.missing"}},
            {"op": "le", "path": "nodes.check.exit_code", "value": 0}
        ]}));
        assert_eq!(evaluate(&ok, &nodes), Ok(true));
        let type_error =
            cond(serde_json::json!({"op": "gt", "path": "nodes.a.changed", "value": 1}));
        assert!(evaluate(&type_error, &nodes).is_err());
        let missing = cond(serde_json::json!({"op": "eq", "path": "nodes.a.nope", "value": 1}));
        assert!(evaluate(&missing, &nodes).is_err());
        assert!(condition_digest(&ok).starts_with("sha256:"));
    }
}
