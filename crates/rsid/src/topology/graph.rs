//! Pure scheduling model for the durable topology executor (#634, plan §2.3).
//!
//! The model itself lives in `rsi_graph::planning::graph` (#2415, #2308 cut 2)
//! so its tests compile without the daemon. This facade keeps the daemon's
//! call sites: it passes the validated `MAX_ITERATIONS` ceiling as a value and
//! maps the planner's `String` errors to [`DaemonError::InvalidParam`].

use std::ops::Deref;

use rsi_graph::format::WorkflowDefinition;
use rsi_graph::planning::graph::GraphShape as PlannedShape;

pub(crate) use rsi_graph::planning::graph::{
    InstanceState, MAX_ATTEMPTS_PER_NODE, MAX_NODE_ATTEMPTS_PER_EXECUTION, RegionDecision,
    RegionProgress,
};

use crate::error::{DaemonError, Result};
use crate::session::topology_ops::MAX_ITERATIONS;

/// The executable shape of one workflow snapshot (daemon-side handle).
#[derive(Debug)]
pub(crate) struct GraphShape(PlannedShape);

impl GraphShape {
    pub(crate) fn from_workflow(workflow: &WorkflowDefinition) -> Result<Self> {
        PlannedShape::from_workflow(workflow, MAX_ITERATIONS)
            .map(Self)
            .map_err(DaemonError::InvalidParam)
    }
}

impl Deref for GraphShape {
    type Target = PlannedShape;

    fn deref(&self) -> &PlannedShape {
        &self.0
    }
}
