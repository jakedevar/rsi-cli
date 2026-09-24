# Prompt Compiler

## What It Is

A preprocessing layer that transforms a user's raw natural language input into a
linguistically complete, structurally valid prompt before it is sent to an LLM.

It operates exclusively on **user-authored prompts**. It never touches LLM responses.

```
User types:   "summarize what changed in this PR"
                        ↓
           [ Prompt Compiler ]
                        ↓
LLM receives: "For each file listed in {{diff}}, return { path, change_type,
               summary }. Verify all files are covered before completing.
               End with: COMPLETE | ERROR:[reason]"
```

The user writes intent. The compiler produces a prompt with zero linguistic
ambiguity, enforced output contracts, and all structural layers filled in.

---

## The Problem It Solves

LLMs are probability distribution selectors. Every token in a prompt either
compresses or expands the model's output distribution. Vague language = high
variance outputs. Structurally complete language = near-zero format variance.

Users write intent. Compilers write prompts. The gap between them is the
entire problem this solves.

---

## The Five Linguistic Layers

A valid compiled prompt must satisfy all five layers. Missing any layer
introduces ambiguity the model fills with its own priors.

### 1. Semantic Layer — Word Choice

Cognitive verbs produce wide distributions. Operational verbs produce narrow ones.

| Input verb   | Compiled verb                        |
|--------------|--------------------------------------|
| analyze      | extract / classify / enumerate       |
| write        | emit / construct / serialize         |
| think about  | evaluate against [criteria]          |
| handle       | on [condition], return [exact format] |
| summarize    | for each X, return [schema]          |

**Rule**: Replace every cognitive verb with the operational verb that matches
the user's intent.

### 2. Syntactic Layer — Sentence Structure

Execution templates — syntactic patterns that co-occur with specific output
behaviors in training data.

```
# Function signature
Given [INPUT], return [OUTPUT].

# Iterative
For each [ITEM] in [COLLECTION], [ACTION].

# Conditional branch
If [CONDITION], then [ACTION_A]. Otherwise, [ACTION_B].

# Precondition gate
Before [ACTION], verify [CONDITION]. Only proceed if verified.

# Constraint definition
[X] is valid if and only if [CONDITIONS].
```

**Rule**: Map the user's intent to the matching execution template.

### 3. Deictic Layer — Scope and Pointing

Every noun and pronoun must have an explicit, unambiguous referent.
Pronouns and implicit references ("it", "the result", "this") are resolved
to their exact targets.

| Uncompiled              | Compiled                                    |
|-------------------------|---------------------------------------------|
| "process it"            | "process the JSON array in {{input}}"       |
| "return the result"     | "return a string array of extracted names"  |
| "from this"             | "from the file list provided in {{files}}"  |
| "in here"               | "within the current session context"        |

**Rule**: Every deictic expression must resolve to a named variable, schema
field, or explicit description.

### 4. Discourse Layer — Sequencing and Causality

Discourse markers are control flow. They define temporal, causal, and logical
relationships between instructions.

```
Sequential:  First... Then... Subsequently... Finally...
             After completing X, proceed to Y.
Conditional: If... When... Unless... Only if...
Causal:      Because... Therefore... Since...
Emphatic:    Critically... Under no circumstances... Always... Never...
```

**Rule**: Multi-step user intent must be connected with explicit discourse
markers. No implicit ordering.

### 5. Pragmatic Layer — Behavioral Frame

The opening frame activates behavioral priors in the model. It sets the
"mode" the model operates in before reading any instruction.

```
# Suppresses prose, activates structured output
"Your output is consumed by a machine parser, not a human."

# Activates spec-compliance behavior
"Treat the following as a specification, not a request."

# Suppresses hedging
"You are executing, not reasoning."
```

**Rule**: Every compiled prompt opens with a frame that matches the desired
output modality (structured, analytical, generative, etc.).

---

## Output Contract

Every compiled prompt ends with an explicit output contract — a terminal
signal the LLM must emit. This is the handshake between the prompt and the
orchestrator.

```
# Standard
Your final output MUST end with exactly one of:
  COMPLETE
  INCOMPLETE:[failed criterion]
  ERROR:[type]:[message]

# Phase-aware
End with: PHASE_COMPLETE | PHASE_INCOMPLETE:[reason] | PHASE_ERROR:[type]:[message]
```

The output contract is **never optional**. Without it, the orchestrator
cannot route deterministically.

---

## Delegation Compilation Mode

Delegation Compilation is the specialized pipeline mode that applies when the compiler's output will be pasted into a fresh agent session — one with zero access to the current conversation. The input is a request of the form "write a prompt for the next agent to do X." The output is a single, copy-paste-ready prompt block.

**Cold-session premise:** every referenced noun must be self-contained. The receiving agent cannot ask clarifying questions, cannot read scroll-back, and cannot infer what "it," "the result," or "the thing we just shipped" means. Every claim must be anchored to a grep-able token — a commit hash, a file path with line numbers, a verbatim error string, or an env var name. Claims that cannot be anchored are either rewritten or dropped.

### Ten-Step Algorithm

1. **Shipped-vs-gap split** — Open with explicit `CONTEXT (what has shipped — do not re-do)` and `THE GAP (what must close)` sections. The most common failure mode in a cold session is re-doing finished work because the boundary between done and pending is invisible. Forcing the label forces the boundary.

2. **Anchor to concrete state** — Replace every vague noun with a concrete handle: commit hash, `file/path.rs:line`, verbatim error string, env var name, or reference doc path. If a claim cannot be anchored to a token a cold agent can `Grep`, either find the anchor or drop the claim.

3. **Research-question surfacing** — Extract every unresolved design decision as a numbered `Q1…QN` list. The planning phase must answer them before writing any code. Do not answer them yourself unless you have authority to decide; surfacing decisions prevents workers from burying them silently in implementation.

4. **File budget** — Name the 5–10 key files to `Grep`-then-Read, each annotated with its purpose. Append the standard caveat: full reads only for files under 400 lines. This is the read-context budget for the downstream agent, not a suggestion.

5. **Pattern-linked acceptance criteria** — Every criterion must point at an existing test file, code pattern, or prior commit the worker can mirror (e.g., "mirror `sandbox_e2e.rs`"). Never write "add a good test" — always write "add a test that does X, following the shape of Y at `path:line`."

6. **Baseline-calibrated regression gate** — Call out any pre-existing test failures or clippy warnings. The worker must be told the baseline so they do not waste time investigating noise they did not introduce.

7. **Explicit scope exclusions** — List what NOT to touch. Include at minimum: adjacent systems with similar names, files that look related but aren't in scope, and follow-up tickets already planned elsewhere. This is the inverse of acceptance criteria and the primary defense against scope creep.

8. **Deliverables list** — Name concrete artifacts: research doc path, plan path, commit or branch naming convention, push target, and manual-verification gate format. If the downstream command emits specific deliverables (e.g., `/master_implement`'s `PIPELINE HANDOFF` block), cite its protocol verbatim.

9. **Invocation prelude** — Begin the compiled prompt with the exact command or flag sequence. If the target is `/master_implement`, the first line is `/master_implement` followed by the ticket description. If the target is a bare agent spawn, name the `subagent_type` and model choice.

10. **Self-containment audit** — Re-read the drafted prompt assuming zero context from the current session. Every term must be defined, every reference must include a path, every claim must be grep-verifiable. Patch gaps. This step is non-optional; skip it and the next agent will spend its first turn asking questions instead of working.

### Output Shape

```
/<invocation-command>

TICKET: <one-line problem statement>

CONTEXT (what has shipped — do not re-do):
  - <commit-hash>: <description>
  - <file:line-range>: <what was done>

THE GAP:
  <concrete description of what remains>

RESEARCH SCOPE (planning phase must answer before coding):
  Q1. <unresolved design decision>
  Q2. <unresolved design decision>

KEY FILES (Grep-then-Read; full reads only for <400 lines):
  - <path:line-range>  — <purpose>
  - <path:line-range>  — <purpose>

ACCEPTANCE CRITERIA:
  Automated: <test command and expected result, mirroring <reference test file>>
  Manual: <step-by-step verification sequence>

BASELINE (pre-existing failures/warnings — do not treat as regressions):
  <verbatim output or "none">

OUT OF SCOPE:
  - <adjacent system or file not to touch>
  - <follow-up ticket reference>

DELIVERABLES:
  - <artifact path or branch name>
  - <push target or PR reference>
```

See `/compile_delegation` for the slash command that invokes this mode.

---

## Compilation Pipeline

```
User Input
    │
    ▼
┌─────────────────────────────────────┐
│ 1. INTENT PARSER                    │
│    Extract: verb, object, context   │
│    Identify: task type              │
└──────────────────┬──────────────────┘
                   │
                   ▼
┌─────────────────────────────────────┐
│ 2. SEMANTIC OPTIMIZER               │
│    cognitive verb → operational     │
│    vague modifier → specific        │
└──────────────────┬──────────────────┘
                   │
                   ▼
┌─────────────────────────────────────┐
│ 3. DEIXIS RESOLVER                  │
│    pronouns → named referents       │
│    implicit scope → explicit scope  │
└──────────────────┬──────────────────┘
                   │
                   ▼
┌─────────────────────────────────────┐
│ 4. SYNTAX MAPPER                    │
│    intent → execution template      │
│    (function sig, iterative, cond.) │
└──────────────────┬──────────────────┘
                   │
                   ▼
┌─────────────────────────────────────┐
│ 5. DISCOURSE LINKER                 │
│    multi-step → sequenced with      │
│    explicit markers                 │
└──────────────────┬──────────────────┘
                   │
                   ▼
┌─────────────────────────────────────┐
│ 6. FRAME INJECTOR                   │
│    prepend pragmatic frame          │
│    matching output modality         │
└──────────────────┬──────────────────┘
                   │
                   ▼
┌─────────────────────────────────────┐
│ 7. CONTRACT APPENDER                │
│    append output contract           │
│    matching session phase           │
└──────────────────┬──────────────────┘
                   │
                   ▼
┌─────────────────────────────────────┐
│ 8. LAYER VALIDATOR                  │
│    all 5 layers present? → pass     │
│    missing layer → repair or error  │
└──────────────────┬──────────────────┘
                   │
                   ▼
            Compiled Prompt
         (sent to target LLM)
```

When the user's intent is delegation — writing a prompt for a cold downstream agent — the pipeline enters **Delegation Compilation Mode**: after Step 2 (Semantic Optimizer), execution branches to the ten-step delegation algorithm (see `## Delegation Compilation Mode`) before rejoining at Step 8 (Layer Validator).

---

## Compiler Prompt (Meta-Prompt)

This is the prompt given to the LLM that performs compilation. It is the
specification the compiler LLM executes against.

```
ROLE: You are a prompt compiler. Your output is a compiled prompt, not a
response to a question. You compile user intent into linguistically complete,
structurally valid prompt text.

COMPILATION RULES:
  1. SEMANTIC: Replace all cognitive verbs (analyze, think, consider, handle)
     with operational verbs (extract, return, emit, classify, verify, compare).

  2. SYNTAX: Map the user's intent to the correct execution template:
     - Transformation task  → "Given [INPUT], return [OUTPUT]."
     - Iterative task       → "For each [ITEM] in [COLLECTION], [ACTION]."
     - Conditional task     → "If [CONDITION], [ACTION_A]. Otherwise [ACTION_B]."
     - Validation task      → "Verify [CONDITION]. Only proceed if verified."

  3. DEIXIS: Resolve all pronouns and implicit references to explicit named
     referents. Every noun must be unambiguous.

  4. DISCOURSE: Connect multi-step instructions with explicit markers.
     Use: First, Then, Subsequently, Finally, After X, Only if, Unless.

  5. FRAME: Open with a pragmatic frame matching the output type:
     - Structured output    → "Your output is machine-parsed."
     - Analytical output    → "Treat the following as a specification."
     - Execution output     → "You are executing, not reasoning."

  6. CONTRACT: End with the output contract for this phase:
     "End your response with exactly one of: COMPLETE | INCOMPLETE:[reason] | ERROR:[type]:[message]"

CONSTRAINTS:
  - Never alter the user's intent, only its linguistic structure.
  - Never add tasks the user did not specify.
  - Preserve all domain-specific nouns exactly as written.
  - If the user's intent is ambiguous, emit: COMPILE_ERROR:AMBIGUOUS_INTENT:[description]

INPUT: {{user_prompt}}

OUTPUT: The compiled prompt only. No explanation. No wrapper text.
```

---

## What the Compiler Does Not Do

- Does not modify LLM responses
- Does not add tasks beyond the user's stated intent
- Does not interpret ambiguous intent — it errors and requests clarification
- Does not decide which LLM to use (that is the orchestrator's concern)
- Does not manage session state (that is the session layer's concern)

---

## Integration Point in Rsi

```
User Input
    │
    ▼
[ Prompt Compiler ]    ← this component
    │
    ▼
[ Session Context Injector ]
    │
    ▼
[ Target LLM ]
    │
    ▼
[ Contract Parser ]
    │
    ▼
[ Orchestrator / Router ]
```

The compiler sits between raw user input and the session context injector.
It is the first transformation in the pipeline. Everything downstream receives
a structurally guaranteed prompt, never raw user text.
