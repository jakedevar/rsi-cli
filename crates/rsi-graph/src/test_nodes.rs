#[cfg(test)]
pub(crate) use inner::*;

#[cfg(test)]
mod inner {
    use crate::data::{NodeData, Value};
    use crate::error::GraphError;
    use crate::node::{Node, NodeContext, NodeId};

    /// Returns input unchanged.
    pub struct PassthroughNode {
        id: NodeId,
        name: String,
    }

    impl PassthroughNode {
        pub fn new(id: &str) -> Self {
            Self {
                id: NodeId::new(id),
                name: format!("passthrough-{id}"),
            }
        }
    }

    impl Node for PassthroughNode {
        fn id(&self) -> &NodeId {
            &self.id
        }

        fn name(&self) -> &str {
            &self.name
        }

        fn execute(&self, input: NodeData, _ctx: &mut NodeContext) -> Result<NodeData, GraphError> {
            Ok(input)
        }
    }

    /// Appends a field to the data.
    pub struct AppendFieldNode {
        id: NodeId,
        name: String,
        field_name: String,
        field_value: Value,
    }

    impl AppendFieldNode {
        pub fn new(id: &str, field_name: &str, value: Value) -> Self {
            Self {
                id: NodeId::new(id),
                name: format!("append-{id}"),
                field_name: field_name.to_string(),
                field_value: value,
            }
        }
    }

    impl Node for AppendFieldNode {
        fn id(&self) -> &NodeId {
            &self.id
        }

        fn name(&self) -> &str {
            &self.name
        }

        fn execute(
            &self,
            mut input: NodeData,
            _ctx: &mut NodeContext,
        ) -> Result<NodeData, GraphError> {
            input.insert(self.field_name.clone(), self.field_value.clone());
            Ok(input)
        }
    }

    /// Reads a value from shared state and adds it to output.
    /// Since Node::execute doesn't have direct store access, this test node
    /// adds a marker field to show it executed with the requested state key.
    pub struct StateReaderNode {
        id: NodeId,
        name: String,
        state_key: String,
        output_field: String,
    }

    impl StateReaderNode {
        pub fn new(id: &str, state_key: &str, output_field: &str) -> Self {
            Self {
                id: NodeId::new(id),
                name: format!("state-reader-{id}"),
                state_key: state_key.to_string(),
                output_field: output_field.to_string(),
            }
        }
    }

    impl Node for StateReaderNode {
        fn id(&self) -> &NodeId {
            &self.id
        }

        fn name(&self) -> &str {
            &self.name
        }

        fn execute(
            &self,
            mut input: NodeData,
            _ctx: &mut NodeContext,
        ) -> Result<NodeData, GraphError> {
            input.insert(
                self.output_field.clone(),
                Value::String(format!("state_reader_{}", self.state_key)),
            );
            Ok(input)
        }
    }

    /// A node that always fails.
    pub struct FailingNode {
        id: NodeId,
        name: String,
    }

    impl FailingNode {
        pub fn new(id: &str) -> Self {
            Self {
                id: NodeId::new(id),
                name: format!("failing-{id}"),
            }
        }
    }

    impl Node for FailingNode {
        fn id(&self) -> &NodeId {
            &self.id
        }

        fn name(&self) -> &str {
            &self.name
        }

        fn execute(
            &self,
            _input: NodeData,
            _ctx: &mut NodeContext,
        ) -> Result<NodeData, GraphError> {
            Err(GraphError::ExecutionFailed(
                "intentional failure".to_string(),
            ))
        }
    }
}
