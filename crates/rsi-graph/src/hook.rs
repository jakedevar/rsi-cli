//! Hook system and lifecycle events for graph execution.
//!
//! Hooks allow injecting cross-cutting behavior at specific stages of node
//! execution without modifying node implementations. The system is fully
//! synchronous and serializable — no closures in public types.

use serde::{Deserialize, Serialize};

use crate::data::{NodeData, Value};
use crate::edge::EdgeId;
use crate::error::GraphError;
use crate::node::NodeId;

/// Lifecycle stage at which a hook fires.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum HookStage {
    BeforeExecute,
    AfterExecute,
    OnError,
    BeforeFilter,
    AfterFilter,
    OnStateWrite,
    OnScopeEnter,
    OnScopeExit,
}

/// Action returned by a hook to control execution flow.
#[derive(Debug)]
pub enum HookAction {
    /// Continue to the next hook or proceed with execution.
    Continue,
    /// Replace the current data with the provided data.
    ModifyData(NodeData),
    /// Abort execution with an error.
    Abort(GraphError),
    /// Skip the current node execution entirely.
    Skip,
}

/// Serializable selector for targeting which nodes/edges a hook applies to.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum Selector {
    /// Matches all nodes.
    All,
    /// Matches a specific node by ID.
    NodeById(NodeId),
    /// Matches nodes that have a specific tag.
    NodeByTag(String),
    /// Matches a specific edge by ID (future work — does not match in node context).
    EdgeById(EdgeId),
}

/// Serializable predicate DSL for conditional hook logic.
///
/// Evaluates against a [`NodeData`] without requiring closures, keeping
/// workflow definitions fully serializable per operating rule #8.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum ConditionExpression {
    /// Always true.
    Always,
    /// Always false.
    Never,
    /// True if the named field exists in the data.
    FieldExists(String),
    /// True if the named field equals the given value.
    FieldEquals { field: String, value: Value },
    /// True if the named field is a number greater than the threshold.
    FieldGreaterThan { field: String, threshold: f64 },
    /// True if the named field is a number less than the threshold.
    FieldLessThan { field: String, threshold: f64 },
    /// Logical AND of all sub-expressions.
    And(Vec<ConditionExpression>),
    /// Logical OR of all sub-expressions.
    Or(Vec<ConditionExpression>),
    /// Logical NOT of the inner expression.
    Not(Box<ConditionExpression>),
}

impl ConditionExpression {
    /// Evaluate this expression against the given data.
    pub fn evaluate(&self, data: &NodeData) -> bool {
        match self {
            ConditionExpression::Always => true,
            ConditionExpression::Never => false,
            ConditionExpression::FieldExists(field) => data.contains_key(field),
            ConditionExpression::FieldEquals { field, value } => {
                data.get(field).is_some_and(|v| v == value)
            }
            ConditionExpression::FieldGreaterThan { field, threshold } => {
                data.get(field).is_some_and(|v| match v {
                    Value::Number(n) => n > threshold,
                    _ => false,
                })
            }
            ConditionExpression::FieldLessThan { field, threshold } => {
                data.get(field).is_some_and(|v| match v {
                    Value::Number(n) => n < threshold,
                    _ => false,
                })
            }
            ConditionExpression::And(exprs) => exprs.iter().all(|e| e.evaluate(data)),
            ConditionExpression::Or(exprs) => exprs.iter().any(|e| e.evaluate(data)),
            ConditionExpression::Not(expr) => !expr.evaluate(data),
        }
    }
}

/// Read-only context provided to hooks during dispatch.
#[derive(Debug)]
pub struct HookContext<'a> {
    pub node_id: &'a NodeId,
    pub node_tags: &'a [String],
    pub data: &'a NodeData,
    pub stage: HookStage,
}

/// A synchronous hook that fires at a specific lifecycle stage.
///
/// Implementors must be `Send + Sync` for use in shared registries.
pub trait Hook: Send + Sync {
    /// The lifecycle stage this hook fires at.
    fn stage(&self) -> HookStage;

    /// The selector determining which nodes this hook applies to.
    fn selector(&self) -> Selector;

    /// Execute the hook, returning an action that controls execution flow.
    fn execute(&self, ctx: &HookContext) -> HookAction;
}

/// Registry that collects hooks and dispatches them in registration order.
pub struct HookRegistry {
    hooks: Vec<Box<dyn Hook>>,
}

impl HookRegistry {
    pub fn new() -> Self {
        Self { hooks: Vec::new() }
    }

    /// Register a hook. Hooks fire in registration order during dispatch.
    pub fn register(&mut self, hook: Box<dyn Hook>) {
        self.hooks.push(hook);
    }

    /// The number of registered hooks.
    pub fn len(&self) -> usize {
        self.hooks.len()
    }

    /// Whether the registry has no hooks.
    pub fn is_empty(&self) -> bool {
        self.hooks.is_empty()
    }

    /// Dispatch all hooks matching the given stage and context.
    ///
    /// Hooks are evaluated in registration order. Short-circuits on
    /// [`HookAction::Abort`] or [`HookAction::Skip`]. For
    /// [`HookAction::ModifyData`], the last modification wins.
    pub fn dispatch(&self, stage: HookStage, ctx: &HookContext) -> HookAction {
        let mut last_modify: Option<NodeData> = None;

        for hook in &self.hooks {
            if hook.stage() != stage {
                continue;
            }

            if !selector_matches(&hook.selector(), ctx) {
                continue;
            }

            match hook.execute(ctx) {
                HookAction::Continue => {}
                HookAction::ModifyData(data) => {
                    last_modify = Some(data);
                }
                action @ HookAction::Abort(_) => return action,
                action @ HookAction::Skip => return action,
            }
        }

        match last_modify {
            Some(data) => HookAction::ModifyData(data),
            None => HookAction::Continue,
        }
    }
}

impl Default for HookRegistry {
    fn default() -> Self {
        Self::new()
    }
}

/// Check if a selector matches the given hook context.
fn selector_matches(selector: &Selector, ctx: &HookContext) -> bool {
    match selector {
        Selector::All => true,
        Selector::NodeById(id) => ctx.node_id == id,
        Selector::NodeByTag(tag) => ctx.node_tags.contains(tag),
        // Edge selectors don't match in node context — future work.
        Selector::EdgeById(_) => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- ConditionExpression tests ---

    #[test]
    fn condition_always_evaluates_true() {
        let data = NodeData::new();
        assert!(ConditionExpression::Always.evaluate(&data));
    }

    #[test]
    fn condition_never_evaluates_false() {
        let data = NodeData::new();
        assert!(!ConditionExpression::Never.evaluate(&data));
    }

    #[test]
    fn condition_field_exists() {
        let mut data = NodeData::new();
        assert!(!ConditionExpression::FieldExists("x".into()).evaluate(&data));
        data.insert("x", Value::Null);
        assert!(ConditionExpression::FieldExists("x".into()).evaluate(&data));
    }

    #[test]
    fn condition_field_equals() {
        let mut data = NodeData::new();
        data.insert("status", Value::String("ready".into()));

        let expr = ConditionExpression::FieldEquals {
            field: "status".into(),
            value: Value::String("ready".into()),
        };
        assert!(expr.evaluate(&data));

        let expr_miss = ConditionExpression::FieldEquals {
            field: "status".into(),
            value: Value::String("pending".into()),
        };
        assert!(!expr_miss.evaluate(&data));
    }

    #[test]
    fn condition_field_greater_than() {
        let mut data = NodeData::new();
        data.insert("score", Value::Number(75.0));

        assert!(
            ConditionExpression::FieldGreaterThan {
                field: "score".into(),
                threshold: 50.0,
            }
            .evaluate(&data)
        );

        assert!(
            !ConditionExpression::FieldGreaterThan {
                field: "score".into(),
                threshold: 100.0,
            }
            .evaluate(&data)
        );
    }

    #[test]
    fn condition_field_less_than() {
        let mut data = NodeData::new();
        data.insert("temp", Value::Number(20.0));

        assert!(
            ConditionExpression::FieldLessThan {
                field: "temp".into(),
                threshold: 30.0,
            }
            .evaluate(&data)
        );

        assert!(
            !ConditionExpression::FieldLessThan {
                field: "temp".into(),
                threshold: 10.0,
            }
            .evaluate(&data)
        );
    }

    #[test]
    fn condition_and_or_not() {
        let mut data = NodeData::new();
        data.insert("a", Value::Bool(true));
        data.insert("b", Value::Number(5.0));

        // AND: both true
        let and_expr = ConditionExpression::And(vec![
            ConditionExpression::FieldExists("a".into()),
            ConditionExpression::FieldExists("b".into()),
        ]);
        assert!(and_expr.evaluate(&data));

        // AND: one false
        let and_fail = ConditionExpression::And(vec![
            ConditionExpression::FieldExists("a".into()),
            ConditionExpression::FieldExists("c".into()),
        ]);
        assert!(!and_fail.evaluate(&data));

        // OR: one true
        let or_expr = ConditionExpression::Or(vec![
            ConditionExpression::FieldExists("c".into()),
            ConditionExpression::FieldExists("a".into()),
        ]);
        assert!(or_expr.evaluate(&data));

        // OR: none true
        let or_fail = ConditionExpression::Or(vec![
            ConditionExpression::FieldExists("c".into()),
            ConditionExpression::FieldExists("d".into()),
        ]);
        assert!(!or_fail.evaluate(&data));

        // NOT
        let not_expr = ConditionExpression::Not(Box::new(ConditionExpression::Never));
        assert!(not_expr.evaluate(&data));
    }

    #[test]
    fn condition_empty_and_is_true() {
        let data = NodeData::new();
        assert!(ConditionExpression::And(vec![]).evaluate(&data));
    }

    #[test]
    fn condition_empty_or_is_false() {
        let data = NodeData::new();
        assert!(!ConditionExpression::Or(vec![]).evaluate(&data));
    }

    #[test]
    fn condition_non_numeric_field_greater_than_is_false() {
        let mut data = NodeData::new();
        data.insert("name", Value::String("alice".into()));
        assert!(
            !ConditionExpression::FieldGreaterThan {
                field: "name".into(),
                threshold: 0.0,
            }
            .evaluate(&data)
        );
    }

    // --- Hook dispatch tests ---

    /// A simple test hook that returns a fixed action.
    struct TestHook {
        stage: HookStage,
        selector: Selector,
        action: std::sync::Mutex<Option<HookAction>>,
        fallback_continue: bool,
    }

    impl TestHook {
        fn new(stage: HookStage, selector: Selector, action: HookAction) -> Self {
            Self {
                stage,
                selector,
                action: std::sync::Mutex::new(Some(action)),
                fallback_continue: false,
            }
        }

        /// A hook that returns Continue every time (reusable).
        fn continuing(stage: HookStage, selector: Selector) -> Self {
            Self {
                stage,
                selector,
                action: std::sync::Mutex::new(None),
                fallback_continue: true,
            }
        }
    }

    impl Hook for TestHook {
        fn stage(&self) -> HookStage {
            self.stage
        }

        fn selector(&self) -> Selector {
            self.selector.clone()
        }

        fn execute(&self, _ctx: &HookContext) -> HookAction {
            let mut guard = self.action.lock().unwrap();
            match guard.take() {
                Some(action) => action,
                None => {
                    if self.fallback_continue {
                        HookAction::Continue
                    } else {
                        HookAction::Continue
                    }
                }
            }
        }
    }

    fn make_ctx<'a>(
        node_id: &'a NodeId,
        tags: &'a [String],
        data: &'a NodeData,
        stage: HookStage,
    ) -> HookContext<'a> {
        HookContext {
            node_id,
            node_tags: tags,
            data,
            stage,
        }
    }

    #[test]
    fn before_execute_hook_all_fires_and_returns_continue() {
        let mut registry = HookRegistry::new();
        registry.register(Box::new(TestHook::continuing(
            HookStage::BeforeExecute,
            Selector::All,
        )));

        let id = NodeId::new("n1");
        let tags: Vec<String> = vec![];
        let data = NodeData::new();
        let ctx = make_ctx(&id, &tags, &data, HookStage::BeforeExecute);

        let result = registry.dispatch(HookStage::BeforeExecute, &ctx);
        assert!(matches!(result, HookAction::Continue));
    }

    #[test]
    fn modify_data_hook_injects_field() {
        let mut registry = HookRegistry::new();

        let mut modified = NodeData::new();
        modified.insert("injected", Value::Bool(true));

        registry.register(Box::new(TestHook::new(
            HookStage::BeforeExecute,
            Selector::All,
            HookAction::ModifyData(modified),
        )));

        let id = NodeId::new("n1");
        let tags: Vec<String> = vec![];
        let data = NodeData::new();
        let ctx = make_ctx(&id, &tags, &data, HookStage::BeforeExecute);

        let result = registry.dispatch(HookStage::BeforeExecute, &ctx);
        match result {
            HookAction::ModifyData(d) => {
                assert_eq!(d.get("injected"), Some(&Value::Bool(true)));
            }
            _ => panic!("expected ModifyData"),
        }
    }

    #[test]
    fn abort_hook_short_circuits() {
        let mut registry = HookRegistry::new();

        // First hook aborts
        registry.register(Box::new(TestHook::new(
            HookStage::BeforeExecute,
            Selector::All,
            HookAction::Abort(GraphError::HookError("aborted".into())),
        )));

        // Second hook should never fire
        let mut modified = NodeData::new();
        modified.insert("should_not_appear", Value::Bool(true));
        registry.register(Box::new(TestHook::new(
            HookStage::BeforeExecute,
            Selector::All,
            HookAction::ModifyData(modified),
        )));

        let id = NodeId::new("n1");
        let tags: Vec<String> = vec![];
        let data = NodeData::new();
        let ctx = make_ctx(&id, &tags, &data, HookStage::BeforeExecute);

        let result = registry.dispatch(HookStage::BeforeExecute, &ctx);
        assert!(matches!(result, HookAction::Abort(_)));
    }

    #[test]
    fn skip_hook_short_circuits() {
        let mut registry = HookRegistry::new();

        registry.register(Box::new(TestHook::new(
            HookStage::BeforeExecute,
            Selector::All,
            HookAction::Skip,
        )));

        let id = NodeId::new("n1");
        let tags: Vec<String> = vec![];
        let data = NodeData::new();
        let ctx = make_ctx(&id, &tags, &data, HookStage::BeforeExecute);

        let result = registry.dispatch(HookStage::BeforeExecute, &ctx);
        assert!(matches!(result, HookAction::Skip));
    }

    #[test]
    fn node_by_tag_selector_fires_only_for_tagged_nodes() {
        let mut registry = HookRegistry::new();

        let mut modified = NodeData::new();
        modified.insert("tagged", Value::Bool(true));

        registry.register(Box::new(TestHook::new(
            HookStage::BeforeExecute,
            Selector::NodeByTag("important".into()),
            HookAction::ModifyData(modified),
        )));

        let id = NodeId::new("n1");
        let data = NodeData::new();

        // Without matching tag — should get Continue
        let no_tags: Vec<String> = vec!["other".into()];
        let ctx = make_ctx(&id, &no_tags, &data, HookStage::BeforeExecute);
        let result = registry.dispatch(HookStage::BeforeExecute, &ctx);
        assert!(matches!(result, HookAction::Continue));

        // With matching tag — should get ModifyData
        let tags: Vec<String> = vec!["important".into()];
        let ctx = make_ctx(&id, &tags, &data, HookStage::BeforeExecute);
        let result = registry.dispatch(HookStage::BeforeExecute, &ctx);
        assert!(matches!(result, HookAction::ModifyData(_)));
    }

    #[test]
    fn node_by_id_selector() {
        let mut registry = HookRegistry::new();

        let mut modified = NodeData::new();
        modified.insert("matched", Value::Bool(true));

        registry.register(Box::new(TestHook::new(
            HookStage::AfterExecute,
            Selector::NodeById(NodeId::new("target")),
            HookAction::ModifyData(modified),
        )));

        let data = NodeData::new();
        let tags: Vec<String> = vec![];

        // Non-matching ID
        let other_id = NodeId::new("other");
        let ctx = make_ctx(&other_id, &tags, &data, HookStage::AfterExecute);
        assert!(matches!(
            registry.dispatch(HookStage::AfterExecute, &ctx),
            HookAction::Continue
        ));

        // Matching ID
        let target_id = NodeId::new("target");
        let ctx = make_ctx(&target_id, &tags, &data, HookStage::AfterExecute);
        assert!(matches!(
            registry.dispatch(HookStage::AfterExecute, &ctx),
            HookAction::ModifyData(_)
        ));
    }

    #[test]
    fn edge_by_id_selector_never_matches_node_context() {
        let mut registry = HookRegistry::new();

        registry.register(Box::new(TestHook::new(
            HookStage::BeforeExecute,
            Selector::EdgeById(EdgeId::new("e1")),
            HookAction::Skip,
        )));

        let id = NodeId::new("n1");
        let tags: Vec<String> = vec![];
        let data = NodeData::new();
        let ctx = make_ctx(&id, &tags, &data, HookStage::BeforeExecute);

        // EdgeById should not match — result is Continue
        assert!(matches!(
            registry.dispatch(HookStage::BeforeExecute, &ctx),
            HookAction::Continue
        ));
    }

    #[test]
    fn multiple_hooks_last_modify_data_wins() {
        let mut registry = HookRegistry::new();

        let mut first = NodeData::new();
        first.insert("version", Value::Number(1.0));
        registry.register(Box::new(TestHook::new(
            HookStage::BeforeExecute,
            Selector::All,
            HookAction::ModifyData(first),
        )));

        let mut second = NodeData::new();
        second.insert("version", Value::Number(2.0));
        registry.register(Box::new(TestHook::new(
            HookStage::BeforeExecute,
            Selector::All,
            HookAction::ModifyData(second),
        )));

        let id = NodeId::new("n1");
        let tags: Vec<String> = vec![];
        let data = NodeData::new();
        let ctx = make_ctx(&id, &tags, &data, HookStage::BeforeExecute);

        let result = registry.dispatch(HookStage::BeforeExecute, &ctx);
        match result {
            HookAction::ModifyData(d) => {
                assert_eq!(d.get("version"), Some(&Value::Number(2.0)));
            }
            _ => panic!("expected ModifyData"),
        }
    }

    #[test]
    fn hooks_only_fire_for_matching_stage() {
        let mut registry = HookRegistry::new();

        registry.register(Box::new(TestHook::new(
            HookStage::AfterExecute,
            Selector::All,
            HookAction::Skip,
        )));

        let id = NodeId::new("n1");
        let tags: Vec<String> = vec![];
        let data = NodeData::new();
        let ctx = make_ctx(&id, &tags, &data, HookStage::BeforeExecute);

        // Dispatching BeforeExecute should not trigger AfterExecute hook
        assert!(matches!(
            registry.dispatch(HookStage::BeforeExecute, &ctx),
            HookAction::Continue
        ));
    }

    #[test]
    fn registry_len_and_is_empty() {
        let mut registry = HookRegistry::new();
        assert!(registry.is_empty());
        assert_eq!(registry.len(), 0);

        registry.register(Box::new(TestHook::continuing(
            HookStage::BeforeExecute,
            Selector::All,
        )));
        assert!(!registry.is_empty());
        assert_eq!(registry.len(), 1);
    }
}
