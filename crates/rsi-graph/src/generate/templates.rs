use std::collections::BTreeMap;

use crate::data::Value;
use crate::format::{
    EdgeDef, GRAPH_VIEW_METADATA_KEY, GRAPH_VIEW_SCHEMA_VERSION, GraphViewMetadata,
    GraphViewVisualEdge, GraphViewVisualEdgeKind, NodeDef, WorkflowDefinition,
};

/// Marks a workflow entry whose instructions must flow to the next node as
/// internal context instead of being repeated in the entry session's response.
pub const PIPELINE_ENTRY_CONTEXT_TAG: &str = "pipeline-entry-context";

/// Runnable starter template surfaced by the graph overlay topology picker.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StarterTemplateSpec {
    pub name: &'static str,
    pub label: &'static str,
    pub description: &'static str,
    pub use_cases: &'static [&'static str],
}

const STARTER_TEMPLATES: &[StarterTemplateSpec] = &[
    StarterTemplateSpec {
        name: "horizontal",
        label: "Pipeline",
        description: "Straight-line handoff from capture to draft to review.",
        use_cases: &["Planning pipeline", "Research -> draft -> review"],
    },
    StarterTemplateSpec {
        name: "pingpong",
        label: "Ping-Pong",
        description: "Proposal and critique stages with a visual iteration loop.",
        use_cases: &["Debate", "Iterative refinement"],
    },
    StarterTemplateSpec {
        name: "hub",
        label: "Hub-Spoke",
        description: "A router fans work out to specialists, then consolidates results.",
        use_cases: &["Manager-worker routing", "Specialist review"],
    },
    StarterTemplateSpec {
        name: "vertical",
        label: "Fan-Out",
        description: "A source fans out to parallel workers before synthesis.",
        use_cases: &["Parallel review", "Batch analysis"],
    },
    StarterTemplateSpec {
        name: "vertical_decision",
        label: "Decision Loop",
        description: "Parallel critics feed a solver with a visual iterate-again cue.",
        use_cases: &["Consensus review", "Judge/solver loop"],
    },
    StarterTemplateSpec {
        name: "brainstorm",
        label: "Brainstorm",
        description: "Sequential idea expansion across multiple perspectives.",
        use_cases: &["Idea generation", "Exploratory synthesis"],
    },
    StarterTemplateSpec {
        name: "instructor_assistant",
        label: "Instructor-Assistant",
        description: "An executor is guided and checked by an instructor loop.",
        use_cases: &["Guided execution", "Teach/evaluate cycles"],
    },
    StarterTemplateSpec {
        name: "master_implement",
        label: "Master Implement",
        description: "Research -> plan -> implement -> push end-to-end pipeline driven by /team_* slash commands.",
        use_cases: &[
            "Full ticket delivery",
            "Autonomous research + plan + implement + push",
        ],
    },
    StarterTemplateSpec {
        name: "master_improve",
        label: "Master Improve",
        description: "Convergence loop: research/plan/implement -> judge -> budget -> push, driver respawns until judge=DONE or safety rail trips.",
        use_cases: &[
            "Iterative goal refinement",
            "Self-converging implementation loop",
        ],
    },
    StarterTemplateSpec {
        name: "master_orchestrate",
        label: "Master Orchestrate",
        description: "Slice conveyor: research -> plan -> implement -> review -> verify -> docs, with a visual review->implement fix loop.",
        use_cases: &[
            "Slice-scoped orchestration",
            "Research + plan + implement + review + verify + docs",
        ],
    },
];

pub fn starter_templates() -> &'static [StarterTemplateSpec] {
    STARTER_TEMPLATES
}

pub fn build_starter_workflow(name: &str) -> Option<WorkflowDefinition> {
    match name {
        "horizontal" => Some(horizontal_template()),
        "pingpong" => Some(pingpong_template()),
        "hub" => Some(hub_template()),
        "vertical" => Some(vertical_template()),
        "vertical_decision" => Some(vertical_decision_template()),
        "brainstorm" => Some(brainstorm_template()),
        "instructor_assistant" => Some(instructor_assistant_template()),
        "master_implement" => Some(master_implement_template()),
        "master_improve" => Some(master_improve_template()),
        "master_orchestrate" => Some(master_orchestrate_template()),
        _ => None,
    }
}

fn horizontal_template() -> WorkflowDefinition {
    workflow(
        "Pipeline",
        "Straight-line capture, draft, and review.",
        vec![
            action(
                "capture",
                "Capture",
                "Collect the request, constraints, and success criteria.",
            ),
            action(
                "draft",
                "Draft",
                "Produce the first complete pass using the captured context.",
            ),
            action(
                "review",
                "Review",
                "Check the draft, tighten gaps, and prepare the final response.",
            ),
        ],
        vec![
            EdgeDef::new("capture", "draft"),
            EdgeDef::new("draft", "review"),
        ],
        Vec::new(),
    )
}

fn pingpong_template() -> WorkflowDefinition {
    workflow(
        "Ping-Pong",
        "Proposal and critique stages with a visual revise loop.",
        vec![
            action(
                "brief",
                "Brief",
                "Summarize the target outcome and the current constraints.",
            ),
            action(
                "proposal",
                "Proposal",
                "Generate the current best attempt for the task.",
            ),
            action(
                "critique",
                "Critique",
                "Challenge the proposal and call out weak assumptions.",
            ),
            action(
                "revision",
                "Revision",
                "Update the proposal using the critique feedback.",
            ),
            action(
                "finalize",
                "Finalize",
                "Condense the strongest revision into a final deliverable.",
            ),
        ],
        vec![
            EdgeDef::new("brief", "proposal"),
            EdgeDef::new("proposal", "critique"),
            EdgeDef::new("critique", "revision"),
            EdgeDef::new("revision", "finalize"),
        ],
        vec![loop_arrow("revise-loop", "revision", "critique", "iterate")],
    )
}

fn hub_template() -> WorkflowDefinition {
    workflow(
        "Hub-Spoke",
        "Router hands work to specialists, then consolidates results.",
        vec![
            action(
                "router",
                "Router",
                "Break the request into specialist-sized assignments.",
            ),
            action(
                "code",
                "Code Specialist",
                "Inspect implementation details and code-level risks.",
            ),
            action(
                "tests",
                "Test Specialist",
                "Look for missing coverage, regressions, and edge cases.",
            ),
            action(
                "docs",
                "Docs Specialist",
                "Check clarity, naming, and user-facing communication.",
            ),
            with_custody_from(
                action(
                    "merge",
                    "Merge",
                    "Synthesize specialist findings into a single response.",
                ),
                "router",
            ),
        ],
        vec![
            EdgeDef::new("router", "code"),
            EdgeDef::new("router", "tests"),
            EdgeDef::new("router", "docs"),
            EdgeDef::new("code", "merge"),
            EdgeDef::new("tests", "merge"),
            EdgeDef::new("docs", "merge"),
        ],
        Vec::new(),
    )
}

fn vertical_template() -> WorkflowDefinition {
    workflow(
        "Fan-Out",
        "Source context fans out to parallel workers and then converges.",
        vec![
            action(
                "source",
                "Source",
                "Frame the task so each parallel worker starts from the same context.",
            ),
            action(
                "alpha",
                "Worker Alpha",
                "Analyze the task from the first perspective.",
            ),
            action(
                "beta",
                "Worker Beta",
                "Analyze the task from a competing perspective.",
            ),
            action(
                "gamma",
                "Worker Gamma",
                "Analyze the task for edge cases and failure modes.",
            ),
            with_custody_from(
                action(
                    "synthesize",
                    "Synthesize",
                    "Combine the worker outputs into a single answer.",
                ),
                "alpha",
            ),
        ],
        vec![
            EdgeDef::new("source", "alpha"),
            EdgeDef::new("source", "beta"),
            EdgeDef::new("source", "gamma"),
            EdgeDef::new("alpha", "synthesize"),
            EdgeDef::new("beta", "synthesize"),
            EdgeDef::new("gamma", "synthesize"),
        ],
        Vec::new(),
    )
}

fn vertical_decision_template() -> WorkflowDefinition {
    workflow(
        "Decision Loop",
        "Parallel critics feed a solver with a visual iterate-again cue.",
        vec![
            action(
                "problem",
                "Problem",
                "State the target decision, constraints, and success bar.",
            ),
            action(
                "critic_a",
                "Critic A",
                "Evaluate the problem framing from the first angle.",
            ),
            action(
                "critic_b",
                "Critic B",
                "Evaluate the problem framing from the second angle.",
            ),
            action(
                "critic_c",
                "Critic C",
                "Evaluate the framing for corner cases and ambiguity.",
            ),
            with_custody_from(
                action(
                    "solver",
                    "Solver",
                    "Resolve the critics into the best current answer.",
                ),
                "critic_a",
            ),
            action(
                "polish",
                "Polish",
                "Turn the current answer into a concise final form.",
            ),
        ],
        vec![
            EdgeDef::new("problem", "critic_a"),
            EdgeDef::new("problem", "critic_b"),
            EdgeDef::new("problem", "critic_c"),
            EdgeDef::new("critic_a", "solver"),
            EdgeDef::new("critic_b", "solver"),
            EdgeDef::new("critic_c", "solver"),
            EdgeDef::new("solver", "polish"),
        ],
        vec![loop_arrow("decision-loop", "polish", "problem", "iterate")],
    )
}

fn brainstorm_template() -> WorkflowDefinition {
    workflow(
        "Brainstorm",
        "Sequential idea passes that widen the search before synthesis.",
        vec![
            action(
                "brief",
                "Brief",
                "Clarify the goal and what counts as a strong idea.",
            ),
            action(
                "wild",
                "Wild Ideas",
                "Generate unconstrained options without filtering.",
            ),
            action(
                "practical",
                "Practical Ideas",
                "Translate promising wild ideas into realistic options.",
            ),
            action(
                "stress",
                "Stress Test",
                "Pressure-test the practical ideas for failure modes.",
            ),
            action(
                "synthesis",
                "Synthesis",
                "Combine the strongest ideas into a final set.",
            ),
        ],
        vec![
            EdgeDef::new("brief", "wild"),
            EdgeDef::new("wild", "practical"),
            EdgeDef::new("practical", "stress"),
            EdgeDef::new("stress", "synthesis"),
        ],
        Vec::new(),
    )
}

fn instructor_assistant_template() -> WorkflowDefinition {
    workflow(
        "Instructor-Assistant",
        "An instructor checks an executor with a visual revise loop.",
        vec![
            action(
                "goals",
                "Goals",
                "Define the objective, acceptance criteria, and hard constraints.",
            ),
            action(
                "assistant",
                "Assistant",
                "Carry out the work according to the current instructions.",
            ),
            action(
                "instructor",
                "Instructor",
                "Review the work, tighten directives, and call for revisions.",
            ),
            action("deliver", "Deliver", "Ship the instructor-approved answer."),
        ],
        vec![
            EdgeDef::new("goals", "assistant"),
            EdgeDef::new("assistant", "instructor"),
            EdgeDef::new("instructor", "deliver"),
        ],
        vec![loop_arrow(
            "coach-loop",
            "instructor",
            "assistant",
            "revise",
        )],
    )
}

fn master_implement_template() -> WorkflowDefinition {
    workflow(
        "Master Implement",
        "Research -> plan -> implement -> push end-to-end pipeline.",
        vec![
            // Entry: user edits this node's instructions to carry their goal.
            // The runner forwards the instructions as internal downstream context.
            {
                let mut n = action(
                    "entry",
                    "Entry",
                    "PIPELINE GOAL:\n\
                     Replace this line with the user's goal.\n\n\
                     Reply with exactly `PIPELINE ENTRY READY`, then exit. Do not \
                     quote, restate, or summarize the goal. Do not start work -- \
                     the downstream nodes handle research, planning, and implementation.",
                );
                n.description = "Type your goal here (press I to edit), then run (r).".to_string();
                n.tags.push(PIPELINE_ENTRY_CONTEXT_TAG.to_string());
                n
            },
            // Research: spawn /team_research. Upstream context carries the goal.
            action(
                "research",
                "Research",
                "PIPELINE MODE: true\n\
                 PIPELINE STAGE: research\n\n\
                 The upstream Context block contains the user's goal. \
                 Use the Skill tool to invoke /team_research with that goal as input. \
                 Follow the team_research command instructions in full. \
                 After the research doc is committed, your FINAL MESSAGE MUST CONTAIN ONLY \
                 the PIPELINE HANDOFF -- RESEARCH block defined in /master_implement \
                 (Research document path, Research question, Key findings, Codebase areas, \
                 Open questions).",
            ),
            // Plan: spawn /team_plan with the research doc path from upstream context.
            action(
                "plan",
                "Plan",
                "PIPELINE MODE: true\n\
                 PIPELINE STAGE: planning\n\n\
                 The upstream Context block contains a PIPELINE HANDOFF -- RESEARCH block \
                 with the research document path. \
                 Use the Skill tool to invoke /team_plan with that research doc path \
                 and the original goal as input. Follow the team_plan command \
                 instructions in full. After the plan doc is committed, your FINAL MESSAGE \
                 MUST CONTAIN ONLY the PIPELINE HANDOFF -- PLAN block (doc_path, status, \
                 optional blocker, optional next_action_hint).",
            ),
            // Implement: spawn /team_implement in pipeline mode so it skips its own push.
            action(
                "implement",
                "Implement",
                "PIPELINE MODE: true\n\
                 PIPELINE STAGE: implementation\n\n\
                 The upstream Context block contains a PIPELINE HANDOFF -- PLAN block with \
                 the plan doc path. \
                 Use the Skill tool to invoke /team_implement with that plan doc path as input. \
                 Follow the team_implement command instructions in full INCLUDING Step 0 \
                 (mandatory EnterWorktree). \
                 IMPORTANT: you are in PIPELINE MODE, so SKIP Step 8 (the final git push) -- \
                 a downstream node owns the push. \
                 Your FINAL MESSAGE MUST CONTAIN ONLY the PIPELINE HANDOFF -- IMPLEMENTATION \
                 block (plan doc path, branch name, worktree path, verification checklist, \
                 outstanding issues).",
            ),
            // Push: parse worktree + branch from upstream handoff, cd, push.
            action(
                "push",
                "Push",
                "The upstream Context block contains a PIPELINE HANDOFF -- IMPLEMENTATION block. \
                 Extract the `worktree path` and `branch name` from it. \
                 Then run exactly:\n\
                 cd <worktree path> && git push -u origin HEAD\n\
                 Do NOT merge into main. Report the pushed branch name in your final message \
                 and exit. If the handoff block is missing or malformed, fail loudly with the \
                 reason -- do not attempt a push from an unknown directory.",
            ),
        ],
        vec![
            EdgeDef::new("entry", "research"),
            EdgeDef::new("research", "plan"),
            EdgeDef::new("plan", "implement"),
            EdgeDef::new("implement", "push"),
        ],
        Vec::new(),
    )
}

#[allow(clippy::too_many_lines)]
fn master_improve_template() -> WorkflowDefinition {
    workflow(
        "Master Improve",
        "Convergence loop: research/plan/implement -> judge -> budget -> push, \
         driver respawns until judge=DONE or safety rail trips.",
        vec![
            // Entry: copied VERBATIM from master_implement_template().
            // Driver mutates instructions on respawn to inject ITERATION CONTEXT.
            {
                let mut n = action(
                    "entry",
                    "Entry",
                    "PIPELINE GOAL:\n\
                     Replace this line with the user's goal.\n\n\
                     Reply with exactly `PIPELINE ENTRY READY`, then exit. Do not \
                     quote, restate, or summarize the goal. Do not start work -- \
                     the downstream nodes handle research, planning, and implementation.",
                );
                n.description = "Type your goal here (press I to edit), then run (r).".to_string();
                n.tags.push(PIPELINE_ENTRY_CONTEXT_TAG.to_string());
                n
            },
            // Research: copied VERBATIM from master_implement_template().
            action(
                "research",
                "Research",
                "PIPELINE MODE: true\n\
                 PIPELINE STAGE: research\n\n\
                 The upstream Context block contains the user's goal. \
                 Use the Skill tool to invoke /team_research with that goal as input. \
                 Follow the team_research command instructions in full. \
                 After the research doc is committed, your FINAL MESSAGE MUST CONTAIN ONLY \
                 the PIPELINE HANDOFF -- RESEARCH block defined in /master_implement \
                 (Research document path, Research question, Key findings, Codebase areas, \
                 Open questions).",
            ),
            // Plan: copied VERBATIM from master_implement_template().
            action(
                "plan",
                "Plan",
                "PIPELINE MODE: true\n\
                 PIPELINE STAGE: planning\n\n\
                 The upstream Context block contains a PIPELINE HANDOFF -- RESEARCH block \
                 with the research document path. \
                 Use the Skill tool to invoke /team_plan with that research doc path \
                 and the original goal as input. Follow the team_plan command \
                 instructions in full. After the plan doc is committed, your FINAL MESSAGE \
                 MUST CONTAIN ONLY the PIPELINE HANDOFF -- PLAN block (doc_path, status, \
                 optional blocker, optional next_action_hint).",
            ),
            // Implement: copied VERBATIM from master_implement_template().
            action(
                "implement",
                "Implement",
                "PIPELINE MODE: true\n\
                 PIPELINE STAGE: implementation\n\n\
                 The upstream Context block contains a PIPELINE HANDOFF -- PLAN block with \
                 the plan doc path. \
                 Use the Skill tool to invoke /team_implement with that plan doc path as input. \
                 Follow the team_implement command instructions in full INCLUDING Step 0 \
                 (mandatory EnterWorktree). \
                 IMPORTANT: you are in PIPELINE MODE, so SKIP Step 8 (the final git push) -- \
                 a downstream node owns the push. \
                 Your FINAL MESSAGE MUST CONTAIN ONLY the PIPELINE HANDOFF -- IMPLEMENTATION \
                 block (plan doc path, branch name, worktree path, verification checklist, \
                 outstanding issues).",
            ),
            // Judge: NEW. Prompt grammar from research §2 (lines 86-133).
            action(
                "judge",
                "Judge",
                "You are the JUDGE node of the master_improve convergence loop.\n\n\
                 Your input (in upstream context as ### content) is the PIPELINE HANDOFF -- \
                 IMPLEMENTATION block from this iteration's implement node. It contains:\n\
                   - plan doc path (thoughts/shared/plans/YYYY-MM-DD-...md) -- read it to see \
                     the original goal and the plan's success-criteria checklist\n\
                   - branch name\n\
                   - worktree path\n\
                   - verification checklist (manual items the implement worker flagged)\n\
                   - outstanding issues (what the implement worker could not finish)\n\n\
                 Your job: decide whether the implementation HAS FULFILLED THE ORIGINAL GOAL.\n\n\
                 Read the plan's 'Success Criteria' section. Read the worktree's git log\n\
                 (cd <worktree path> && git log --oneline main..HEAD) to see what was committed.\n\
                 You MAY run `cargo test --workspace 2>&1` in the worktree only if the handoff \
                 block analysis is insufficient -- the budget node will run tests for the \
                 regression gate regardless.\n\n\
                 Your FINAL MESSAGE -- your last assistant message in this session -- MUST BE \
                 EXACTLY one of these three forms, with NO other content before or after:\n\n\
                   DONE\n\n\
                 OR\n\n\
                   CONTINUE: <a refined goal that is more specific than the original and addresses \
                             the gap you identified>\n\n\
                 OR (only if you cannot determine an answer):\n\n\
                   CONTINUE: HALT_JUDGE_BLOCKED <reason>\n\n\
                 Rules for refinement:\n\
                 - The CONTINUE goal must be a one-line restatement that the next iteration's \
                   research node can act on as its sole input.\n\
                 - If the implementation regressed (broke a test that previously passed), say\n\
                   CONTINUE: Fix the regression in <test-name> introduced by <commit>.\n\
                 - If outstanding issues remain, fold them into the CONTINUE text.\n\
                 - If the goal is functionally fulfilled but polish remains (docs, edge cases), \
                   prefer DONE over CONTINUE for trivial polish -- convergence loops should converge.",
            ),
            // Budget: NEW. Prompt grammar from research §3 (lines 145-167).
            // CRITICAL: writes full payload to ~/.rsi/chain/<chain_id>/iter-<n>-budget.txt and
            // emits ONLY the short PROCEED:/HALT: prefix in its final message (works around the
            // 120-char output_preview cap at graph_executions.rs:734-743).
            action(
                "budget",
                "Budget",
                "You are the BUDGET node of the master_improve convergence loop.\n\n\
                 Your inputs (in upstream context):\n\
                   - The judge's verdict (its final assistant message: DONE or CONTINUE: ...)\n\
                   - The implement node's PIPELINE HANDOFF -- IMPLEMENTATION block (worktree path)\n\
                   - The entry node's ITERATION CONTEXT preamble (chain_id, iteration_index, \
                     cap, pre_failure_count) -- the daemon driver injects this before each \
                     iteration; for iteration 0 it is also stamped into entry's instructions\n\n\
                 Your job (3 steps, in order):\n\n\
                 1. STOP-FILE PRE-FLIGHT: run `test -f ~/.rsi/STOP && echo STOP_PRESENT || echo NO_STOP`. \
                    If STOP_PRESENT, your final message MUST be exactly:\n\
                      HALT: stop-file\n\
                    Skip steps 2 and 3.\n\n\
                 2. REGRESSION GATE: parse the worktree path from the implement handoff block. \
                    Run `cd <worktree path> && cargo test --workspace 2>&1 | tee /tmp/master_improve_test_output.log`. \
                    Tally the total `test result: ... N failed` count across all crates. Call this \
                    `post_failure_count`. Read `pre_failure_count` from the ITERATION CONTEXT in upstream. \
                    If `post_failure_count > pre_failure_count`, your final message MUST be exactly:\n\
                      HALT: regression pre=<pre> post=<post>\n\
                    Skip step 3.\n\
                    If `cargo test` itself fails to run (compile error, panic, harness crash), your \
                    final message MUST be exactly:\n\
                      HALT: error <one-line reason>\n\n\
                 3. JUDGE TRANSDUCTION: read the judge's final message from upstream context.\n\
                    - If judge said `DONE`, write the full payload `PROCEED: judge=DONE` to \
                      `~/.rsi/chain/<chain_id>/iter-<iteration_index>-budget.txt` (create the dir \
                      if needed via `mkdir -p`). Then emit final message exactly:\n\
                        PROCEED: judge=DONE\n\
                    - If judge said `CONTINUE: <text>` (NOT HALT_JUDGE_BLOCKED), write the full \
                      payload `PROCEED: judge=CONTINUE refined_goal=<text>` to the same artifact \
                      file. Then emit final message exactly:\n\
                        PROCEED: judge=CONTINUE refined_goal=<see artifact file path>\n\
                      The driver reads the artifact file when the preview is truncated.\n\
                    - If judge said `CONTINUE: HALT_JUDGE_BLOCKED <reason>`, your final message \
                      MUST be exactly:\n\
                        HALT: judge-blocked <reason>\n\
                    - If the judge's message is malformed (matches none of the three forms), your \
                      final message MUST be exactly:\n\
                        HALT: judge-malformed\n\n\
                 The driver parses your final message via regex; it MUST start with `HALT:` or \
                 `PROCEED:` followed by a single space. Do NOT add other content.",
            ),
            // Push: divergent from master_implement (budget-conditional).
            action(
                "push",
                "Push",
                "The upstream Context block contains a PIPELINE HANDOFF -- IMPLEMENTATION block \
                 AND a budget node final message. \
                 Read the budget node's last assistant message (in upstream context).\n\n\
                 If it starts with EXACTLY `PROCEED: judge=DONE`, then:\n\
                   1. Extract the `worktree path` and `branch name` from the IMPLEMENTATION block.\n\
                   2. Run exactly: cd <worktree path> && git push -u origin HEAD\n\
                   3. Report the pushed branch name in your final message and exit.\n\n\
                 If the budget message starts with anything else (`PROCEED: judge=CONTINUE ...` or \
                 `HALT: ...`), do NOT push. Instead, log the verdict in your final message:\n\
                   skip-push: budget said <verdatim budget message line>\n\
                 and exit successfully. The chain driver reads the budget output and decides next \
                 steps (respawn or halt).",
            ),
        ],
        vec![
            EdgeDef::new("entry", "research"),
            EdgeDef::new("research", "plan"),
            EdgeDef::new("plan", "implement"),
            EdgeDef::new("implement", "judge"),
            EdgeDef::new("judge", "budget"),
            EdgeDef::new("budget", "push"),
        ],
        // Visual loop arrow — picker UX hint that the loop closes at the driver
        // (not in the DAG). Precedent: pingpong, vertical_decision, instructor_assistant.
        vec![loop_arrow("improve-loop", "budget", "entry", "respawn")],
    )
}

#[allow(clippy::too_many_lines)]
fn master_orchestrate_template() -> WorkflowDefinition {
    workflow(
        "Master Orchestrate",
        "Slice conveyor: research -> plan -> implement -> review -> verify -> docs, \
         with a visual review->implement fix loop.",
        vec![
            // Entry: copied VERBATIM from master_implement_template().
            {
                let mut n = action(
                    "entry",
                    "Entry",
                    "PIPELINE GOAL:\n\
                     Replace this line with the user's goal.\n\n\
                     Reply with exactly `PIPELINE ENTRY READY`, then exit. Do not \
                     quote, restate, or summarize the goal. Do not start work -- \
                     the downstream nodes handle research, planning, and implementation.",
                );
                n.description = "Type your goal here (press I to edit), then run (r).".to_string();
                n.tags.push(PIPELINE_ENTRY_CONTEXT_TAG.to_string());
                n
            },
            // Research: copied VERBATIM from master_implement_template().
            action(
                "research",
                "Research",
                "PIPELINE MODE: true\n\
                 PIPELINE STAGE: research\n\n\
                 The upstream Context block contains the user's goal. \
                 Use the Skill tool to invoke /team_research with that goal as input. \
                 Follow the team_research command instructions in full. \
                 After the research doc is committed, your FINAL MESSAGE MUST CONTAIN ONLY \
                 the PIPELINE HANDOFF -- RESEARCH block defined in /master_implement \
                 (Research document path, Research question, Key findings, Codebase areas, \
                 Open questions).",
            ),
            // Plan: copied VERBATIM from master_implement_template().
            action(
                "plan",
                "Plan",
                "PIPELINE MODE: true\n\
                 PIPELINE STAGE: planning\n\n\
                 The upstream Context block contains a PIPELINE HANDOFF -- RESEARCH block \
                 with the research document path. \
                 Use the Skill tool to invoke /team_plan with that research doc path \
                 and the original goal as input. Follow the team_plan command \
                 instructions in full. After the plan doc is committed, your FINAL MESSAGE \
                 MUST CONTAIN ONLY the PIPELINE HANDOFF -- PLAN block (doc_path, status, \
                 optional blocker, optional next_action_hint).",
            ),
            // Implement: copied VERBATIM from master_implement_template() -- PIPELINE MODE
            // skips its own push; the docs node owns the terminal push.
            action(
                "implement",
                "Implement",
                "PIPELINE MODE: true\n\
                 PIPELINE STAGE: implementation\n\n\
                 The upstream Context block contains a PIPELINE HANDOFF -- PLAN block with \
                 the plan doc path. \
                 Use the Skill tool to invoke /team_implement with that plan doc path as input. \
                 Follow the team_implement command instructions in full INCLUDING Step 0 \
                 (mandatory EnterWorktree). \
                 IMPORTANT: you are in PIPELINE MODE, so SKIP Step 8 (the final git push) -- \
                 the docs node owns the push. \
                 Your FINAL MESSAGE MUST CONTAIN ONLY the PIPELINE HANDOFF -- IMPLEMENTATION \
                 block (plan doc path, branch name, worktree path, verification checklist, \
                 outstanding issues).",
            ),
            // Review: NEW. Reviews the worktree diff and emits a machine-parseable verdict.
            action(
                "review",
                "Review",
                "PIPELINE MODE: true\n\
                 PIPELINE STAGE: review\n\n\
                 The upstream Context block contains a PIPELINE HANDOFF -- IMPLEMENTATION block \
                 with the worktree path and branch name. \
                 Extract the `worktree path`, then review the change on that branch:\n\
                   cd <worktree path> && git diff main...HEAD\n\
                 Use the Skill tool to invoke /code-review on that diff (or review manually if \
                 the skill is unavailable). Focus on correctness bugs and reuse/simplification \
                 issues; do NOT expand scope beyond the plan.\n\n\
                 Your FINAL MESSAGE MUST CONTAIN ONLY the PIPELINE HANDOFF -- REVIEW block:\n\
                   - doc_path (path to any review notes written, or 'none')\n\
                   - branch name\n\
                   - worktree path\n\
                   - findings_count (integer count of must-fix findings)\n\
                   - verdict: EXACTLY `verdict: PASS` if findings_count == 0, otherwise \
                     `verdict: FIX` with a one-line summary of the top must-fix items.\n\n\
                 A `verdict: FIX` is the cue for the operator to re-run the implement node \
                 (the visual `fix` loop arrow) with the review findings folded into its goal. \
                 A `verdict: PASS` proceeds to verification.",
            ),
            // Verify: NEW. Regression gate on the worktree before docs/push.
            action(
                "verify",
                "Verify",
                "PIPELINE MODE: true\n\
                 PIPELINE STAGE: verify\n\n\
                 The upstream Context block contains a PIPELINE HANDOFF -- IMPLEMENTATION block \
                 (worktree path) and a PIPELINE HANDOFF -- REVIEW block (verdict). \
                 If the review verdict was `FIX`, STOP: your final message MUST be exactly\n\
                   PIPELINE HANDOFF -- VERIFY\n\
                   status: blocked\n\
                   blocker: review verdict FIX -- re-run implement before verifying\n\
                 and exit. Do not verify an unreviewed change.\n\n\
                 Otherwise, extract the `worktree path` and run the regression gate:\n\
                   cd <worktree path> && git diff --check && cargo test --workspace 2>&1\n\
                 Tally any `test result: ... N failed` across crates.\n\n\
                 Your FINAL MESSAGE MUST CONTAIN ONLY the PIPELINE HANDOFF -- VERIFY block:\n\
                   - worktree path\n\
                   - branch name\n\
                   - status: `complete` if tests pass and `git diff --check` is clean, else `blocked`\n\
                   - manifest_path (path to any verification manifest, or 'none')\n\
                   - blocker (one sentence, only if status is blocked)\n\
                   - docs_bearing: `true` if the change touches user-facing behavior, keybindings, \
                     RPC surface, or schema (docs node must update docs), else `false`.",
            ),
            // Docs: NEW terminal node. Updates docs when flagged, then owns the push.
            action(
                "docs",
                "Docs",
                "The upstream Context block contains PIPELINE HANDOFF -- VERIFY and \
                 -- IMPLEMENTATION blocks. \
                 If the VERIFY status is `blocked`, do NOT push. Your final message MUST be exactly\n\
                   skip-push: verify blocked -- <verbatim blocker line>\n\
                 and exit successfully.\n\n\
                 Otherwise:\n\
                   1. Extract the `worktree path` and `branch name` from the IMPLEMENTATION block.\n\
                   2. If VERIFY `docs_bearing` is `true`, update the relevant docs in the worktree \
                      (docs/keybindings.md for keybinding changes, thoughts/shared/reference/* or \
                      the top-level AGENTS.md/CLAUDE.md as appropriate) and commit them:\n\
                      cd <worktree path> && git add -A && git commit -m 'docs: update for <slice>'\n\
                      If `docs_bearing` is `false`, skip the docs commit.\n\
                   3. Push the branch: cd <worktree path> && git push -u origin HEAD\n\
                   4. Report the pushed branch name in your final message and exit. \
                 Do NOT merge into main. If the handoff blocks are missing or malformed, fail \
                 loudly with the reason -- do not push from an unknown directory.",
            ),
        ],
        vec![
            EdgeDef::new("entry", "research"),
            EdgeDef::new("research", "plan"),
            EdgeDef::new("plan", "implement"),
            EdgeDef::new("implement", "review"),
            EdgeDef::new("review", "verify"),
            EdgeDef::new("verify", "docs"),
        ],
        // Visual fix-loop arrow -- picker UX hint that a FIX verdict sends the operator
        // back to implement. Manual cue only (no daemon driver), matching the pingpong /
        // vertical_decision / instructor_assistant precedent. The DAG edges stay acyclic.
        vec![loop_arrow(
            "orchestrate-fix-loop",
            "review",
            "implement",
            "fix",
        )],
    )
}

fn workflow(
    name: &str,
    description: &str,
    nodes: Vec<NodeDef>,
    edges: Vec<EdgeDef>,
    visual_edges: Vec<GraphViewVisualEdge>,
) -> WorkflowDefinition {
    let mut workflow = WorkflowDefinition::new(name);
    workflow.description = description.to_string();
    workflow.nodes = nodes;
    workflow.edges = edges;

    if !visual_edges.is_empty() {
        let metadata = GraphViewMetadata {
            schema_version: GRAPH_VIEW_SCHEMA_VERSION,
            visual_edges,
            extra: BTreeMap::new(),
        };
        let graph_view = serde_json::to_value(metadata).expect("graph view metadata should encode");
        let value: Value =
            serde_json::from_value(graph_view).expect("graph view metadata should round-trip");
        workflow
            .metadata
            .insert(GRAPH_VIEW_METADATA_KEY.to_string(), value);
    }

    workflow
}

fn action(id: &str, name: &str, instructions: &str) -> NodeDef {
    let mut node = NodeDef::action(id, name);
    node.instructions = instructions.to_string();
    node
}

fn with_custody_from(mut node: NodeDef, source: &str) -> NodeDef {
    node.tags.push(format!("custody.from=node:{source}"));
    node
}

fn loop_arrow(id: &str, source: &str, target: &str, label: &str) -> GraphViewVisualEdge {
    GraphViewVisualEdge {
        id: id.to_string(),
        kind: GraphViewVisualEdgeKind::LoopArrow,
        source: source.to_string(),
        target: target.to_string(),
        source_port: None,
        target_port: None,
        label: Some(label.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::validate::validate_executable_workflow;

    #[test]
    fn starter_templates_expose_all_patterns() {
        assert_eq!(starter_templates().len(), 10);
    }

    #[test]
    fn all_starter_workflows_validate() {
        for template in starter_templates() {
            let workflow = build_starter_workflow(template.name)
                .unwrap_or_else(|| panic!("missing workflow for {}", template.name));
            let report = validate_executable_workflow(&workflow);
            assert!(
                !report.has_errors(),
                "template {} should be executable: {:?}",
                template.name,
                report.diagnostics
            );
        }
    }

    #[test]
    fn iterative_templates_use_visual_loop_edges_only() {
        for name in ["pingpong", "vertical_decision", "instructor_assistant"] {
            let workflow = build_starter_workflow(name).expect("template should exist");
            assert!(workflow.graph_view_metadata().unwrap().is_some());
            assert!(workflow.edges.iter().all(|edge| edge.source != edge.target));
        }
    }

    #[test]
    fn master_implement_chain_is_linear_slash_command_pipeline() {
        let workflow = build_starter_workflow("master_implement")
            .expect("master_implement template should be registered");

        // Structure.
        let ids: Vec<&str> = workflow.nodes.iter().map(|n| n.id.as_str()).collect();
        assert_eq!(ids, vec!["entry", "research", "plan", "implement", "push"]);
        assert_eq!(workflow.edges.len(), 4);

        let edge_pairs: Vec<(&str, &str)> = workflow
            .edges
            .iter()
            .map(|e| (e.source.as_str(), e.target.as_str()))
            .collect();
        assert_eq!(
            edge_pairs,
            vec![
                ("entry", "research"),
                ("research", "plan"),
                ("plan", "implement"),
                ("implement", "push"),
            ]
        );

        // Slash-command markers in instructions.
        let node = |id: &str| workflow.nodes.iter().find(|n| n.id == id).unwrap();
        assert!(node("research").instructions.contains("/team_research"));
        assert!(node("plan").instructions.contains("/team_plan"));
        assert!(node("implement").instructions.contains("/team_implement"));
        // Pipeline-mode flag on stages that need it.
        assert!(
            node("research")
                .instructions
                .contains("PIPELINE MODE: true")
        );
        assert!(node("plan").instructions.contains("PIPELINE MODE: true"));
        assert!(
            node("implement")
                .instructions
                .contains("PIPELINE MODE: true")
        );
        // Implement must skip its own push.
        assert!(node("implement").instructions.contains("SKIP Step 8"));
        // Entry acknowledges without repeating the goal; the runner carries it
        // forward as internal context.
        assert!(node("entry").instructions.contains("PIPELINE ENTRY READY"));
        assert!(
            node("entry")
                .tags
                .iter()
                .any(|tag| tag == PIPELINE_ENTRY_CONTEXT_TAG)
        );
        assert!(!node("entry").instructions.to_lowercase().contains("echo"));
        // Push runs the git push.
        assert!(
            node("push")
                .instructions
                .contains("git push -u origin HEAD")
        );
    }

    #[test]
    fn master_improve_chain_is_loop_extension() {
        let mi = master_implement_template();
        let mr = master_improve_template();

        // 7 nodes named exactly: entry, research, plan, implement, judge, budget, push
        let mr_node_ids: Vec<&str> = mr.nodes.iter().map(|n| n.id.as_str()).collect();
        assert_eq!(
            mr_node_ids,
            vec![
                "entry",
                "research",
                "plan",
                "implement",
                "judge",
                "budget",
                "push"
            ]
        );

        // 6 edges forming the linear chain
        assert_eq!(mr.edges.len(), 6);
        let edge_pairs: Vec<(&str, &str)> = mr
            .edges
            .iter()
            .map(|e| (e.source.as_str(), e.target.as_str()))
            .collect();
        assert_eq!(
            edge_pairs,
            vec![
                ("entry", "research"),
                ("research", "plan"),
                ("plan", "implement"),
                ("implement", "judge"),
                ("judge", "budget"),
                ("budget", "push"),
            ]
        );

        // BYTE-IDENTITY GUARD: 4 shared nodes (entry, research, plan, implement) must
        // match master_implement_template() exactly. The push node DIVERGES (budget-conditional).
        for shared_id in &["entry", "research", "plan", "implement"] {
            let mi_node = mi.nodes.iter().find(|n| n.id == *shared_id).unwrap();
            let mr_node = mr.nodes.iter().find(|n| n.id == *shared_id).unwrap();
            assert_eq!(
                mi_node.instructions, mr_node.instructions,
                "shared node '{}' instructions MUST be byte-identical between \
                 master_implement and master_improve",
                shared_id
            );
        }

        // GRAMMAR GUARD: judge instructions contain the wire grammar literals
        let judge = mr.nodes.iter().find(|n| n.id == "judge").unwrap();
        assert!(judge.instructions.contains("DONE\n"));
        assert!(judge.instructions.contains("CONTINUE: HALT_JUDGE_BLOCKED"));

        // GRAMMAR GUARD: budget instructions contain wire grammar literals
        let budget = mr.nodes.iter().find(|n| n.id == "budget").unwrap();
        assert!(budget.instructions.contains("PROCEED: judge=DONE"));
        assert!(
            budget
                .instructions
                .contains("PROCEED: judge=CONTINUE refined_goal=")
        );
        assert!(budget.instructions.contains("HALT: stop-file"));
        assert!(budget.instructions.contains("HALT: regression"));
        assert!(budget.instructions.contains("HALT: judge-blocked"));
        assert!(budget.instructions.contains("HALT: judge-malformed"));

        // PUSH DIVERGENCE GUARD: master_improve's push must be conditional on budget
        let mr_push = mr.nodes.iter().find(|n| n.id == "push").unwrap();
        assert!(mr_push.instructions.contains("PROCEED: judge=DONE"));
        assert!(mr_push.instructions.contains("skip-push"));

        // VISUAL LOOP ARROW: confirm the loop_arrow visual edge is registered
        // (rendered by picker; ignored by graph_runner).
        let view = mr
            .graph_view_metadata()
            .unwrap()
            .expect("graph_view metadata");
        assert!(view.visual_edges.iter().any(|e| {
            e.source == "budget"
                && e.target == "entry"
                && matches!(e.kind, GraphViewVisualEdgeKind::LoopArrow)
        }));
    }

    #[test]
    fn master_orchestrate_chain_is_review_verify_docs_extension() {
        let mi = master_implement_template();
        let mo = master_orchestrate_template();

        // 7 nodes named exactly: entry, research, plan, implement, review, verify, docs
        let mo_node_ids: Vec<&str> = mo.nodes.iter().map(|n| n.id.as_str()).collect();
        assert_eq!(
            mo_node_ids,
            vec![
                "entry",
                "research",
                "plan",
                "implement",
                "review",
                "verify",
                "docs"
            ]
        );

        // 6 edges forming the linear chain
        assert_eq!(mo.edges.len(), 6);
        let edge_pairs: Vec<(&str, &str)> = mo
            .edges
            .iter()
            .map(|e| (e.source.as_str(), e.target.as_str()))
            .collect();
        assert_eq!(
            edge_pairs,
            vec![
                ("entry", "research"),
                ("research", "plan"),
                ("plan", "implement"),
                ("implement", "review"),
                ("review", "verify"),
                ("verify", "docs"),
            ]
        );

        // BYTE-IDENTITY GUARD: 3 shared nodes (entry, research, plan) must match
        // master_implement_template() exactly. The implement node DIVERGES because its
        // push-ownership note points at the docs node, so it is excluded here.
        for shared_id in &["entry", "research", "plan"] {
            let mi_node = mi.nodes.iter().find(|n| n.id == *shared_id).unwrap();
            let mo_node = mo.nodes.iter().find(|n| n.id == *shared_id).unwrap();
            assert_eq!(
                mi_node.instructions, mo_node.instructions,
                "shared node '{}' instructions MUST be byte-identical between \
                 master_implement and master_orchestrate",
                shared_id
            );
        }

        // Slash-command markers on the reused stages.
        let node = |id: &str| mo.nodes.iter().find(|n| n.id == id).unwrap();
        assert!(node("research").instructions.contains("/team_research"));
        assert!(node("plan").instructions.contains("/team_plan"));
        assert!(node("implement").instructions.contains("/team_implement"));
        assert!(node("implement").instructions.contains("SKIP Step 8"));

        // GRAMMAR GUARD: review emits the machine-parseable verdict wire literals.
        let review = node("review");
        assert!(review.instructions.contains("verdict: PASS"));
        assert!(review.instructions.contains("verdict: FIX"));
        assert!(review.instructions.contains("findings_count"));

        // GRAMMAR GUARD: verify is a regression gate that blocks on a FIX verdict.
        let verify = node("verify");
        assert!(verify.instructions.contains("cargo test --workspace"));
        assert!(verify.instructions.contains("git diff --check"));
        assert!(verify.instructions.contains("docs_bearing"));

        // Docs is the terminal push owner, gated on verify status.
        let docs = node("docs");
        assert!(docs.instructions.contains("git push -u origin HEAD"));
        assert!(docs.instructions.contains("skip-push"));

        // VISUAL FIX-LOOP ARROW: review -> implement, rendered by picker, ignored by runner.
        let view = mo
            .graph_view_metadata()
            .unwrap()
            .expect("graph_view metadata");
        assert!(view.visual_edges.iter().any(|e| {
            e.source == "review"
                && e.target == "implement"
                && matches!(e.kind, GraphViewVisualEdgeKind::LoopArrow)
        }));
    }
}
