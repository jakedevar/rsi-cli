//! Teaching versions of the RSI skills. Nodes explain their stage; running the
//! DAG does not appoint a manager, dispatch workers, publish, or deploy.
//! Conditional paths are walkthrough topics, not executable branch conditions.

use super::{EdgeDef, NodeDef, WorkflowDefinition, action, loop_arrow, workflow};

const DEMO_TAG: &str = "skill-learning-demo";

fn step(skill: &str, id: &str, name: &str, description: &str) -> NodeDef {
    let mut node = action(
        id,
        name,
        &format!(
            "Learning demo of {skill}. Explain this stage using the upstream context \
             and a hypothetical example. Simulate outcomes in your response only; \
             do not run commands, call tools, change files, spawn sessions, or invoke \
             the skill. Every branch is a teaching topic, not a live decision.\n\n\
             Stage: {name}\n{description}"
        ),
    );
    node.description = description.to_string();
    node.tags.push(DEMO_TAG.to_string());
    node
}

pub(super) fn project_manager() -> WorkflowDefinition {
    let stage = |id, name, description| step("rsi-project-manager", id, name, description);
    workflow(
        "RSI Project Manager",
        "Learning demo of .claude/skills/rsi-project-manager/SKILL.md. One manager \
         integration cycle; loop arrows show later cycles without making the DAG cyclic.",
        vec![
            stage(
                "authority",
                "Verify Manager Seat",
                "Read AgentGetAuthorityCatalog and AgentManagerInspect. Require your current \
                 seat, execute mode, live capabilities and fresh scope/policy fences. \
                 Appointment and grants belong to the operator; a prompt grants no authority.",
            ),
            stage(
                "inbox",
                "Drain Inbox",
                "Drain AgentManagerInbox messages and notices until more_notices is false. \
                 Reconstruct current work from Issues, typed daemon results and git.",
            ),
            stage(
                "fleet",
                "Check Fleet",
                "Check satellite health, cloud capacity, disk space and spend budget. \
                 Keep hub, laptop and approved cloud capacity busy with bounded work.",
            ),
            stage(
                "scope",
                "Choose Issue",
                "Choose one Issue with stated intent and acceptance criteria. \
                 Scope small independent assignments and use engineering judgment.",
            ),
            stage(
                "dispatch",
                "Dispatch Workers",
                "Prepare and commit create_session controls in the owning Epic. Give fresh \
                 short-lived workers one Issue and the worker contract. Confirm \
                 session_established and arm on_terminal watches; end the turn.",
            ),
            stage(
                "hub",
                "Hub Worker",
                "A bounded hub worker implements, tests and commits on its assigned sandbox \
                 branch, then reports RESULT with an exact SHA. Keep hub concurrency small.",
            ),
            stage(
                "satellite",
                "Laptop Worker",
                "Satellite workers and QA share the load. Return exact commits and typed \
                 results to the hub integrator; the hub owns satellite health.",
            ),
            stage(
                "cloud",
                "Cloud Work",
                "Use approved cloud capacity for heavy test gates and QA with a warm build \
                 cache. Respect the spend grant and destroy an idle ephemeral host.",
            ),
            stage(
                "collect",
                "Collect Results",
                "Event wakes resume the manager. Recheck the seat, then read final RESULT \
                 lines, git diffs and typed job results. A queued receipt is not completion.",
            ),
            stage(
                "review",
                "Review If Required",
                "Before merge, get one different-family reviewer pass for new migrations \
                 and credential, IAM or network exposure changes. Other authority and \
                 custody changes get one post-land review.",
            ),
            stage(
                "integrate",
                "Integrate Commits",
                "Fetch rolling and merge ready commits in a detached integration worktree. \
                 Preserve rolling fixes when stale branches conflict. New migrations take \
                 the current schema head plus one in landing order.",
            ),
            stage(
                "compile",
                "Compile Batch",
                "Run cargo check --workspace --all-targets and compile touched rsid shards. \
                 Submit long checks as durable jobs with wake:none.",
            ),
            stage(
                "tests",
                "Test Touched Modules",
                "Choose tests with rsi-test-impact; unset RSI_PROCESS_OWNERSHIP_NAMESPACE. \
                 Arm one when.jobs_terminal wake for the job batch and end the turn. \
                 Landing gate: no new failures relative to rolling.",
            ),
            stage(
                "land",
                "Land On Rolling",
                "Publish through rsi-rolling-land with accepted SHAs and touched-module \
                 filters. Fast-forward only: fetch, integrate and retry a rejected push. \
                 Confirm each source SHA is an ancestor of origin/rolling; close its Issue.",
            ),
            stage(
                "qa",
                "QA Sweep",
                "Run rolling-tip QA on the satellite or as a cloud_sweep job. GREEN lands \
                 thoughts/shared/qa/qa-green.sha; RED files new regression Issues; \
                 INCOMPLETE requires reading evidence and resubmitting.",
            ),
            stage(
                "deploy",
                "Deploy Green SHA",
                "With the operator's Deploy grant, build the QA-green SHA and request a \
                 quiet-point restart. Confirm build_sha and succeeded deployment through \
                 AgentGetDaemonInfo. main promotion and releases remain operator-owned.",
            ),
            stage(
                "handoff",
                "Pass Manager Baton",
                "Commit a handoff carrying operator directives, exact SHAs, Issues and next \
                 actions. Retire enabled resume wakes, fetch fresh authority/custody fences, \
                 request succeed_manager, receive the receipt and end the turn.",
            ),
        ],
        vec![
            EdgeDef::new("authority", "inbox"),
            EdgeDef::new("authority", "fleet"),
            EdgeDef::new("inbox", "scope"),
            EdgeDef::new("fleet", "scope"),
            EdgeDef::new("scope", "dispatch"),
            EdgeDef::new("dispatch", "hub"),
            EdgeDef::new("dispatch", "satellite"),
            EdgeDef::new("dispatch", "cloud"),
            EdgeDef::new("hub", "collect"),
            EdgeDef::new("satellite", "collect"),
            EdgeDef::new("cloud", "collect"),
            EdgeDef::new("collect", "review"),
            EdgeDef::new("review", "integrate"),
            EdgeDef::new("integrate", "compile"),
            EdgeDef::new("compile", "tests"),
            EdgeDef::new("tests", "land"),
            EdgeDef::new("land", "qa"),
            EdgeDef::new("qa", "deploy"),
            EdgeDef::new("deploy", "handoff"),
        ],
        vec![
            loop_arrow("manager-next-batch", "qa", "scope", "next batch"),
            loop_arrow("manager-fix", "tests", "dispatch", "fix failures"),
            loop_arrow("manager-stale-tip", "land", "integrate", "rolling advanced"),
        ],
    )
}

pub(super) fn agent_control() -> WorkflowDefinition {
    let stage = |id, name, description| step("rsi-agent-control", id, name, description);
    workflow(
        "RSI Agent Control",
        "Learning demo of .claude/skills/rsi-agent-control/SKILL.md. Walk through \
         transport, authority and receipt outcomes; retry arrows are visual only.",
        vec![
            stage(
                "catalog",
                "Read Authority Catalog",
                "Start with AgentGetAuthorityCatalog {}. Read current roles, authority_revision \
                 and permitted controls. The catalog describes authority and grants none. \
                 While pending:true, stay within the worker baseline.",
            ),
            stage(
                "transport",
                "Choose Transport",
                "Prefer native rsi_control_* tools, then rsi-rpc with strict typed params. \
                 If both are unavailable, authorized leads may use the documented directive \
                 fallback. With no control surface, compile the worker prompt for the user.",
            ),
            stage(
                "identity",
                "Bind Token Identity",
                "The daemon derives caller identity from RSI_SESSION_TOKEN, supplied \
                 automatically for transport only. Never put tokens in params, files, logs \
                 or prompts, and never supply caller or permission fields. Establishment \
                 re-mints tokens; a restart requires session re-establishment.",
            ),
            stage(
                "request",
                "Read Verb Schema",
                "Read AgentGetAuthorityCatalog with verb for its schema, example and refusals. \
                 Use a permitted control, current observed fences and a stable idempotency_key \
                 per intended write effect. Registration alone grants no authority.",
            ),
            stage(
                "dispatch",
                "Dispatch Control",
                "Send the exact typed request through an RSI control surface. \
                 Every call rechecks current role, policy, scope and sandbox custody.",
            ),
            stage(
                "receipt",
                "Classify Receipt",
                "Distinguish queued/accepted, uncertain transport outcome, stale fence and \
                 authority refusal. Following paths explain alternatives; the demo visits all.",
            ),
            stage(
                "wait",
                "Wait For Event",
                "Queued is not delivered and accepted is not done. Dispatch and end the turn; \
                 rely on terminal watches, job completion or mail. A self-wake uses resume; \
                 a job batch uses one when.jobs_terminal wake. Do not poll.",
            ),
            stage(
                "uncertain",
                "Resolve Uncertain Write",
                "A timeout may follow a committed write. Read state or replay identical \
                 content with the SAME idempotency_key. Exact replay returns the original \
                 receipt; changing content under that key conflicts.",
            ),
            stage(
                "stale",
                "Refresh Stale Fence",
                "Re-read progress, Issue or manager state. Adopt the observed cursor or \
                 version, then decide whether the action still applies. Never guess a fence \
                 or retry stale content unchanged; revised requests use a new key.",
            ),
            stage(
                "refusal",
                "Inspect Refusal",
                "Read the exact code and next_action. Refresh the catalog for that verb, \
                 compare schema/example and permitted, then refresh roles and authority_revision \
                 after a role, custody, pause or policy change.",
            ),
            stage(
                "escalate",
                "Respect Authority Gate",
                "If permission is missing, stop retrying. Human approvals, appointment, scope \
                 and policy belong to the operator. Uniform refusals hide conditions: do not \
                 probe. Report an unknown code with its verb; file defects with AgentCreateIssue.",
            ),
            stage(
                "confirm",
                "Confirm Observed Effect",
                "Inspect the resulting session, action or job before reporting an effect. \
                 A receipt proves recorded state only. Automatic session retries require \
                 explicit policy/budget and respect the live retry kill switch.",
            ),
        ],
        vec![
            EdgeDef::new("catalog", "transport"),
            EdgeDef::new("transport", "identity"),
            EdgeDef::new("identity", "request"),
            EdgeDef::new("request", "dispatch"),
            EdgeDef::new("dispatch", "receipt"),
            outcome("wait", "queued / accepted"),
            outcome("uncertain", "timeout / transport error"),
            outcome("stale", "stale fence"),
            outcome("refusal", "authority refusal"),
            EdgeDef::new("refusal", "escalate"),
            EdgeDef::new("wait", "confirm"),
            EdgeDef::new("uncertain", "confirm"),
            EdgeDef::new("stale", "confirm"),
            EdgeDef::new("escalate", "confirm"),
        ],
        vec![
            loop_arrow(
                "control-replay",
                "uncertain",
                "dispatch",
                "same key + content",
            ),
            loop_arrow(
                "control-redecide",
                "stale",
                "request",
                "fresh fence + new decision",
            ),
            loop_arrow(
                "control-authority",
                "refusal",
                "catalog",
                "refresh authority",
            ),
        ],
    )
}

fn outcome(target: &str, label: &str) -> EdgeDef {
    let mut edge = EdgeDef::new("receipt", target);
    edge.label = Some(label.to_string());
    edge
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::generate::templates::build_starter_workflow;

    #[test]
    fn skill_demos_compile_and_preserve_teaching_context_through_json() {
        for name in ["rsi_project_manager", "rsi_agent_control"] {
            let workflow = build_starter_workflow(name).expect("registered skill demo");
            crate::compiler::compile(&workflow).expect("demo must compile as a DAG");
            let json = serde_json::to_string(&workflow).unwrap();
            let restored: WorkflowDefinition = serde_json::from_str(&json).unwrap();
            assert_eq!(restored, workflow);
            assert!(restored.description.contains("SKILL.md"));
            assert!(restored.nodes.iter().all(|node| {
                !node.description.is_empty()
                    && node.tags.iter().any(|tag| tag == DEMO_TAG)
                    && node.instructions.contains("Simulate outcomes")
            }));
            assert_eq!(
                restored
                    .graph_view_metadata()
                    .unwrap()
                    .unwrap()
                    .visual_edges
                    .len(),
                3
            );
        }
    }

    #[test]
    fn manager_demo_fans_workers_out_and_joins_before_integration() {
        let workflow = project_manager();
        for worker in ["hub", "satellite", "cloud"] {
            assert!(
                workflow
                    .edges
                    .iter()
                    .any(|edge| { edge.source == "dispatch" && edge.target == worker })
            );
            assert!(
                workflow
                    .edges
                    .iter()
                    .any(|edge| { edge.source == worker && edge.target == "collect" })
            );
        }
        assert_eq!(workflow.nodes.last().unwrap().name, "Pass Manager Baton");
    }

    #[test]
    fn control_demo_distinguishes_receipt_outcomes_before_confirmation() {
        let workflow = agent_control();
        let outcomes: Vec<_> = workflow
            .edges
            .iter()
            .filter(|edge| edge.source == "receipt")
            .map(|edge| (edge.target.as_str(), edge.label.as_deref().unwrap()))
            .collect();
        assert_eq!(
            outcomes,
            vec![
                ("wait", "queued / accepted"),
                ("uncertain", "timeout / transport error"),
                ("stale", "stale fence"),
                ("refusal", "authority refusal"),
            ]
        );
        assert_eq!(
            workflow.nodes.last().unwrap().name,
            "Confirm Observed Effect"
        );
    }
}
