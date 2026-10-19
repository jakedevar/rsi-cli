// Typed adapter over the graphs and topology Tauri commands
// (src-tauri/src/graphs.rs, hub #3013 slice 11). Each daemon call is one
// method; the Rust side builds the TUI's exact params and validates them.
import { invoke } from './api.js';
import type { ExecutionLookup, ValidationReport, WorkflowDefinition, WorkflowDocument, WorkflowRow } from './graph-model.js';
import type { Caps, Row } from './dag-model.js';
import type { Topology } from './topology-preview.js';

export interface StarterTemplate { name: string; label: string; description: string; use_cases: string[] }

export const graphsApi = {
  capabilities: () => invoke<Caps>('graph_capabilities'),
  // graph review
  listWorkflows: (projectId: string | null) => invoke<WorkflowRow[]>('graph_list_workflows', { projectId }),
  getWorkflow: (workflowId: string) => invoke<{ document: WorkflowDocument }>('graph_get_workflow', { workflowId }),
  upsertWorkflow: (document: WorkflowDocument) => invoke<{ document: WorkflowDocument }>('graph_upsert_workflow', { document }),
  executeWorkflow: (workflowId: string, workflow: WorkflowDefinition, projectId: string | null) =>
    invoke<{ execution_id: string; workflow_id: string; accepted_at: string; dry_run?: boolean }>('graph_execute_workflow', { workflowId, workflow, projectId }),
  bindSessionWorkflow: (sessionId: string, workflowId: string) =>
    invoke<{ ok: boolean }>('graph_bind_session_workflow', { sessionId, workflowId }),
  getExecution: (executionId: string) => invoke<{ lookup: ExecutionLookup }>('graph_get_execution', { executionId }),
  interruptExecution: (executionId: string) => invoke<{ execution_id: string; status: string }>('graph_interrupt_execution', { executionId }),
  listRecursive: (projectId: string | null, includeQuarantined: boolean) =>
    invoke<Row[]>('graph_list_recursive', { projectId, includeQuarantined }),
  recursiveAsWorkflow: (graphId: string) => invoke<{ definition: unknown }>('graph_recursive_as_workflow', { graphId }),
  editRecursiveInstructions: (graphId: string, taskId: string, instructions: string) =>
    invoke<Row>('graph_edit_recursive_instructions', { graphId, taskId, instructions }),
  editRecursiveSettings: (graphId: string, taskId: string, integrationStrategy: string | null, verificationStrategy: string | null) =>
    invoke<Row>('graph_edit_recursive_settings', { graphId, taskId, integrationStrategy, verificationStrategy }),
  starters: () => invoke<StarterTemplate[]>('graph_starters'),
  starterWorkflow: (name: string) => invoke<WorkflowDefinition>('graph_starter_workflow', { name }),
  validate: (workflow: WorkflowDefinition) => invoke<ValidationReport>('graph_validate', { workflow }),
  // topologies
  listTopologies: (namePrefix: string | null) => invoke<Topology[]>('topology_list', { namePrefix }),
  getTopology: (id: string) => invoke<Topology>('topology_get', { id }),
  executeTopology: (topologyId: string, projectId: string | null, issue: number | null, parentId: string | null) =>
    invoke<{ execution_id: string; workflow_id?: string | null }>('topology_execute', { topologyId, projectId, issue, parentId }),
  resolveTopology: (executionId: string, attemptId: string, action: string, expectedRowVersion: number, confirmPreservedCommit: string | null) =>
    invoke<Row>('topology_resolve', { executionId, attemptId, action, expectedRowVersion, confirmPreservedCommit }),
  // recursive DAG browser
  dagGraph: (graphId: string) => invoke<Row>('dag_get_graph', { graphId }),
  dagOperational: (graphId: string) => invoke<Row>('dag_operational_status', { graphId }),
  dagRuns: (graphId: string) => invoke<Row[]>('dag_scheduler_runs', { graphId }),
  dagRun: (runId: string) => invoke<Row>('dag_scheduler_run', { runId }),
  dagRunEvents: (runId: string) => invoke<Row[]>('dag_run_events', { runId }),
  dagCancellations: (graphId: string) => invoke<Row[]>('dag_cancellations', { graphId }),
  dagRecovery: (graphId: string) => invoke<Row>('dag_recovery_status', { graphId }),
  dagStaleHeartbeats: (graphId: string) => invoke<Row[]>('dag_stale_heartbeats', { graphId }),
  dagLiveRecovery: (graphId: string) => invoke<Row>('dag_live_recovery', { graphId }),
  dagLiveAttempts: (graphId: string) => invoke<Row[]>('dag_live_attempts', { graphId }),
  dagValidations: (graphId: string) => invoke<Row[]>('dag_validations', { graphId }),
  dagArtifacts: (graphId: string, cursor: string | null) => invoke<Row>('dag_artifacts', { graphId, cursor }),
  dagFakeRun: (graphId: string, maxSteps: number) => invoke<Row>('dag_fake_run', { graphId, maxSteps }),
  dagLiveRun: (graphId: string, maxSteps: number) => invoke<Row>('dag_live_run', { graphId, maxSteps }),
  dagCancelGraph: (graphId: string, reason: string) => invoke<Row>('dag_cancel_graph', { graphId, reason }),
  dagCancelRun: (runId: string, reason: string) => invoke<Row>('dag_cancel_run', { runId, reason }),
  dagContinueRecovery: (maxGraphs: number, timeBudgetMs: number | null) => invoke<Row>('dag_continue_recovery', { maxGraphs, timeBudgetMs }),
  // ESP Square
  espSave: (score: number, roundsPlayed: number, pValue: number, roundDetails: string) =>
    invoke<unknown>('esp_save_game', { score, roundsPlayed, pValue, roundDetails }),
};
