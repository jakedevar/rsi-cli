use std::collections::HashMap;

use crate::data::Value;
use crate::error::GraphError;

/// Identifier for a scope in the state hierarchy.
#[derive(Debug, Clone, Hash, Eq, PartialEq)]
pub struct ScopeId(pub(crate) String);

impl ScopeId {
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    pub fn root() -> Self {
        Self("__root__".to_string())
    }
}

/// A scope in the state hierarchy. Each scope has its own key-value store
/// and optionally a parent scope for lexical lookup.
#[derive(Debug)]
struct Scope {
    #[allow(dead_code)]
    id: ScopeId,
    parent: Option<ScopeId>,
    values: HashMap<String, Value>,
    /// Keys explicitly imported from parent (for subgraph isolation).
    imports: Vec<String>,
}

/// Hierarchical state store. Reads walk up the scope chain through imported keys.
/// Writes are local to the current scope unless `set_at` is used.
pub struct StateStore {
    scopes: HashMap<ScopeId, Scope>,
}

impl StateStore {
    /// Creates a new store with a root scope.
    pub fn new() -> Self {
        let root_id = ScopeId::root();
        let root = Scope {
            id: root_id.clone(),
            parent: None,
            values: HashMap::new(),
            imports: Vec::new(),
        };
        let mut scopes = HashMap::new();
        scopes.insert(root_id, root);
        Self { scopes }
    }

    /// Creates a child scope. If `parent` is `Some`, only imported keys are
    /// visible from the parent during lookups.
    pub fn create_scope(
        &mut self,
        id: ScopeId,
        parent: Option<ScopeId>,
        imports: Vec<String>,
    ) -> Result<(), GraphError> {
        if self.scopes.contains_key(&id) {
            return Err(GraphError::StateError(format!(
                "scope already exists: {:?}",
                id.0
            )));
        }
        if let Some(ref parent_id) = parent
            && !self.scopes.contains_key(parent_id)
        {
            return Err(GraphError::StateError(format!(
                "parent scope not found: {:?}",
                parent_id.0
            )));
        }
        let scope = Scope {
            id: id.clone(),
            parent,
            values: HashMap::new(),
            imports,
        };
        self.scopes.insert(id, scope);
        Ok(())
    }

    /// Looks in the current scope first, then walks up through imported keys.
    pub fn get(&self, scope: &ScopeId, key: &str) -> Option<&Value> {
        let s = self.scopes.get(scope)?;
        if let Some(val) = s.values.get(key) {
            return Some(val);
        }
        // Walk up to parent if key is in the imports list.
        if let Some(ref parent_id) = s.parent
            && s.imports.contains(&key.to_string())
        {
            return self.get(parent_id, key);
        }
        None
    }

    /// Writes to the current scope only.
    pub fn set(&mut self, scope: &ScopeId, key: &str, value: Value) -> Result<(), GraphError> {
        let s = self
            .scopes
            .get_mut(scope)
            .ok_or_else(|| GraphError::StateError(format!("scope not found: {:?}", scope.0)))?;
        s.values.insert(key.to_string(), value);
        Ok(())
    }

    /// Writes to a specific target scope (escape hatch). The caller's scope
    /// must exist for the call to be valid.
    pub fn set_at(
        &mut self,
        scope: &ScopeId,
        target_scope: &ScopeId,
        key: &str,
        value: Value,
    ) -> Result<(), GraphError> {
        if !self.scopes.contains_key(scope) {
            return Err(GraphError::StateError(format!(
                "caller scope not found: {:?}",
                scope.0
            )));
        }
        let target = self.scopes.get_mut(target_scope).ok_or_else(|| {
            GraphError::StateError(format!("target scope not found: {:?}", target_scope.0))
        })?;
        target.values.insert(key.to_string(), value);
        Ok(())
    }

    /// Drops the scope and its local state.
    pub fn remove_scope(&mut self, scope: &ScopeId) {
        self.scopes.remove(scope);
    }

    /// Gets a value and converts it via `TryFrom<Value>`.
    pub fn get_as<T: TryFrom<Value, Error = GraphError>>(
        &self,
        scope: &ScopeId,
        key: &str,
    ) -> Result<T, GraphError> {
        let val = self
            .get(scope, key)
            .ok_or_else(|| GraphError::StateError(format!("key not found: {key}")))?;
        T::try_from(val.clone())
    }
}

impl Default for StateStore {
    fn default() -> Self {
        Self::new()
    }
}

/// Convenience `TryFrom<Value>` for `f64` to support `get_as::<f64>`.
impl TryFrom<Value> for f64 {
    type Error = GraphError;

    fn try_from(value: Value) -> Result<Self, Self::Error> {
        match value {
            Value::Number(n) => Ok(n),
            other => Err(GraphError::TypeConversion {
                expected: "Number".to_string(),
                got: format!("{other:?}"),
            }),
        }
    }
}

/// Convenience `TryFrom<Value>` for `String`.
impl TryFrom<Value> for String {
    type Error = GraphError;

    fn try_from(value: Value) -> Result<Self, Self::Error> {
        match value {
            Value::String(s) => Ok(s),
            other => Err(GraphError::TypeConversion {
                expected: "String".to_string(),
                got: format!("{other:?}"),
            }),
        }
    }
}

/// Convenience `TryFrom<Value>` for `bool`.
impl TryFrom<Value> for bool {
    type Error = GraphError;

    fn try_from(value: Value) -> Result<Self, Self::Error> {
        match value {
            Value::Bool(b) => Ok(b),
            other => Err(GraphError::TypeConversion {
                expected: "Bool".to_string(),
                got: format!("{other:?}"),
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn root_scope_get_set() {
        let mut store = StateStore::new();
        let root = ScopeId::root();

        store.set(&root, "x", Value::Number(42.0)).unwrap();
        assert_eq!(store.get(&root, "x"), Some(&Value::Number(42.0)));
        assert_eq!(store.get(&root, "missing"), None);
    }

    #[test]
    fn child_scope_reads_imported_parent_keys() {
        let mut store = StateStore::new();
        let root = ScopeId::root();
        store
            .set(&root, "shared", Value::String("hello".into()))
            .unwrap();
        store
            .set(&root, "secret", Value::String("hidden".into()))
            .unwrap();

        let child = ScopeId::new("child-1");
        store
            .create_scope(child.clone(), Some(root.clone()), vec!["shared".into()])
            .unwrap();

        // Imported key is visible.
        assert_eq!(
            store.get(&child, "shared"),
            Some(&Value::String("hello".into()))
        );
    }

    #[test]
    fn child_scope_cannot_read_non_imported_parent_keys() {
        let mut store = StateStore::new();
        let root = ScopeId::root();
        store
            .set(&root, "shared", Value::String("hello".into()))
            .unwrap();
        store
            .set(&root, "secret", Value::String("hidden".into()))
            .unwrap();

        let child = ScopeId::new("child-1");
        store
            .create_scope(child.clone(), Some(root.clone()), vec!["shared".into()])
            .unwrap();

        // Non-imported key is NOT visible (isolation).
        assert_eq!(store.get(&child, "secret"), None);
    }

    #[test]
    fn child_writes_do_not_clobber_parent() {
        let mut store = StateStore::new();
        let root = ScopeId::root();
        store.set(&root, "x", Value::Number(1.0)).unwrap();

        let child = ScopeId::new("child");
        store
            .create_scope(child.clone(), Some(root.clone()), vec!["x".into()])
            .unwrap();

        // Write to child scope — should shadow, not overwrite parent.
        store.set(&child, "x", Value::Number(99.0)).unwrap();

        assert_eq!(store.get(&child, "x"), Some(&Value::Number(99.0)));
        assert_eq!(store.get(&root, "x"), Some(&Value::Number(1.0)));
    }

    #[test]
    fn set_at_writes_to_specific_parent_scope() {
        let mut store = StateStore::new();
        let root = ScopeId::root();
        store.set(&root, "counter", Value::Number(0.0)).unwrap();

        let child = ScopeId::new("child");
        store
            .create_scope(child.clone(), Some(root.clone()), vec!["counter".into()])
            .unwrap();

        // Use escape hatch to write directly to root.
        store
            .set_at(&child, &root, "counter", Value::Number(10.0))
            .unwrap();

        assert_eq!(store.get(&root, "counter"), Some(&Value::Number(10.0)));
    }

    #[test]
    fn scope_removal_drops_local_state() {
        let mut store = StateStore::new();
        let scope = ScopeId::new("temp");
        store.create_scope(scope.clone(), None, vec![]).unwrap();
        store.set(&scope, "data", Value::Bool(true)).unwrap();
        assert_eq!(store.get(&scope, "data"), Some(&Value::Bool(true)));

        store.remove_scope(&scope);
        assert_eq!(store.get(&scope, "data"), None);
    }

    #[test]
    fn get_as_type_conversion() {
        let mut store = StateStore::new();
        let root = ScopeId::root();

        store.set(&root, "x", Value::Number(2.5)).unwrap();
        let val: f64 = store.get_as(&root, "x").unwrap();
        assert!((val - 2.5).abs() < f64::EPSILON);

        store
            .set(&root, "name", Value::String("alice".into()))
            .unwrap();
        let name: String = store.get_as(&root, "name").unwrap();
        assert_eq!(name, "alice");

        store.set(&root, "flag", Value::Bool(true)).unwrap();
        let flag: bool = store.get_as(&root, "flag").unwrap();
        assert!(flag);
    }

    #[test]
    fn get_as_type_mismatch_error() {
        let mut store = StateStore::new();
        let root = ScopeId::root();
        store
            .set(&root, "val", Value::String("not a number".into()))
            .unwrap();

        let result: Result<f64, _> = store.get_as(&root, "val");
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(
            matches!(err, GraphError::TypeConversion { .. }),
            "expected TypeConversion, got: {err:?}"
        );
    }

    #[test]
    fn get_as_missing_key_error() {
        let store = StateStore::new();
        let root = ScopeId::root();

        let result: Result<f64, _> = store.get_as(&root, "nope");
        assert!(result.is_err());
    }

    #[test]
    fn duplicate_scope_is_error() {
        let mut store = StateStore::new();
        let scope = ScopeId::new("s1");
        store.create_scope(scope.clone(), None, vec![]).unwrap();
        let result = store.create_scope(scope, None, vec![]);
        assert!(result.is_err());
    }

    #[test]
    fn missing_parent_scope_is_error() {
        let mut store = StateStore::new();
        let child = ScopeId::new("orphan");
        let result = store.create_scope(child, Some(ScopeId::new("nonexistent")), vec![]);
        assert!(result.is_err());
    }
}
