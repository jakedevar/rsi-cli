---
description: Compose a self-contained delegation prompt for a downstream command or agent using the ten-step Delegation Compilation algorithm
model: opus
capability_class: architect
---

# Compile Delegation

Produce a single copy-paste-ready prompt block for a downstream agent or command. The output will be read in a cold session with zero access to this conversation. Every claim must be anchored; every term must be defined.

Canonical reference: `docs/prompt-compiler.md#delegation-compilation-mode`.

---

## Inputs (required)

- **Downstream target** — the command or agent that will receive the output: `/master_implement`, `/team_implement`, or a bare agent spawn (specify `subagent_type` and model).
- **Raw user intent** — the informal description of what the next agent must do.
- **Compilation context** — the current conversation state: what has shipped, what is pending, what is blocked.

---

## Ten-Step Algorithm

Execute every step in order. Do not skip or merge steps.

**1. Shipped-vs-gap split**
Label two sections explicitly: `CONTEXT (what has shipped — do not re-do)` and `THE GAP (what must close)`. The boundary between done and pending is the most commonly lost signal in cold sessions.

**2. Anchor to concrete state**
Replace every vague noun with a grep-able token: commit hash, `file/path.rs:line`, verbatim error string, env var name, or doc path. Drop any claim that cannot be anchored.

**3. Research-question surfacing**
Extract every unresolved design decision as a numbered `Q1…QN` list. Planning must answer these before coding begins. Do not answer them yourself unless you have authority to decide.

**4. File budget**
Name the 5–10 key files to `Grep`-then-Read, each with its purpose. Append: "full reads only for files under 400 lines." This is a hard budget, not a suggestion.

**5. Pattern-linked acceptance criteria**
Point every criterion at an existing test file, code pattern, or commit the worker can mirror. Format: "add a test that does X, following the shape of `path/to/reference_test.rs`." Never write "add a good test."

**6. Baseline-calibrated regression gate**
List any pre-existing test failures or clippy warnings verbatim. The worker must know the baseline so they do not chase noise they did not introduce.

**7. Explicit scope exclusions**
List what NOT to touch: adjacent systems with similar names, files that look related but are not in scope, follow-up tickets already planned elsewhere. Minimum three entries or state "none identified."

**8. Deliverables list**
Name every expected artifact: research doc path, plan path, commit/branch naming convention, push target, manual-verification gate format. If the target command defines a handoff block protocol (e.g., `/master_implement`'s `PIPELINE HANDOFF`), cite it verbatim.

**9. Invocation prelude**
Open the compiled prompt with the exact command or spawn sequence. For `/master_implement`: first line is `/master_implement`, followed by the ticket description. For a bare agent: name `subagent_type` and model. Nothing precedes the invocation line.

**10. Self-containment audit**
Re-read the entire drafted prompt assuming zero context from this session. Verify: every term defined, every reference includes a path, every claim is grep-verifiable. Patch every gap found. This step is non-optional.

---

## Output Format

Emit exactly this skeleton, populated from the ten steps above. No wrapper text. No explanation.

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
