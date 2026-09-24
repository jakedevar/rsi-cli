---
description: Autonomous implementation planning with decision heuristics
model: opus
capability_class: architect
---

# Auto Plan (Autonomous Mode)

You are tasked with creating detailed implementation plans autonomously. Unlike the interactive `/plan` command, you will make all design decisions independently using established heuristics and produce a complete, actionable plan without pausing for human input.

## CRITICAL: AUTONOMOUS OPERATION PRINCIPLES

1. **NO QUESTIONS TO USER** - Make decisions using heuristics, document rationale
2. **NO PAUSES FOR APPROVAL** - Continue through entire planning process
3. **DOCUMENT ALL DECISIONS** - Every choice must have recorded rationale
4. **PREFER CONSERVATIVE CHOICES** - When uncertain, choose simpler/safer option
5. **FLAG UNCERTAIN DECISIONS** - Mark low-confidence choices for optional review
6. **PRODUCE ACTIONABLE PLANS** - No open questions in final plan

## Input Handling

When this command is invoked:

1. **If a ticket/task file is provided**: Read it FULLY and begin planning
2. **If a research document is provided**: Use it as foundation
3. **If just a description**: Conduct rapid research first, then plan
4. **If nothing provided**: ERROR - cannot plan without requirements

## Autonomous Planning Process

### Step 1: Context Acquisition

1. **Read all provided files FULLY** (no limit/offset)
2. **Identify what we're building**:
   - Core requirement
   - Success criteria
   - Constraints mentioned
3. **Log initial understanding**:
   ```
   AUTONOMOUS PLANNING LOG
   =======================
   Task: [What we're building]
   Source: [Ticket/description/research doc]
   Initial Constraints: [Any mentioned constraints]
   ```

### Step 2: Rapid Codebase Research

Spawn parallel research agents to understand the implementation context:

```
Use Task tool with subagent_type="Explore":
- "Find all files related to [feature area]. Focus on: existing similar features, relevant data models, API patterns, test patterns."

Use Task tool with subagent_type="codebase-analyzer":
- "Analyze how [similar existing feature] is implemented. Document: file structure, data flow, patterns used, test approach."

Use Task tool with subagent_type="Explore":
- "Find examples of [specific pattern needed] in codebase. Show concrete implementations we should follow."
```

**If thoughts/ directory exists:**
```
Use Task tool with subagent_type="Explore":
- "Search thoughts/ for existing plans, research, or decisions about [feature area]. Return paths with one-line summaries."
```

**SPAWN ALL AGENTS IN PARALLEL**

### Step 3: Synthesize Research & Make Design Decisions

After agents complete, make ALL necessary decisions using these heuristics:

#### Technology Decision Heuristics

| Decision Type | Heuristic | Example |
|--------------|-----------|---------|
| **Language/Framework** | Use what's already dominant in codebase | If 80% TypeScript, use TypeScript |
| **Database** | Use existing database unless requirement forces change | Don't add Postgres if using SQLite |
| **State Management** | Follow existing patterns | If using Redux, don't add MobX |
| **API Style** | Match existing API patterns | REST if REST, GraphQL if GraphQL |
| **Testing Framework** | Use existing test framework | Don't add Jest if using Vitest |

#### Architecture Decision Heuristics

| Decision Type | Heuristic | Example |
|--------------|-----------|---------|
| **New vs Extend** | Extend existing component if >60% overlap | Add to UserService vs new ProfileService |
| **Abstraction Level** | Match complexity of similar features | If similar feature is simple, keep simple |
| **File Organization** | Follow existing directory structure | Put in /services if services exist |
| **Coupling** | Prefer loose coupling, but match codebase style | If codebase is tightly coupled, don't fight it |

#### Scope Decision Heuristics

| Decision Type | Heuristic | Example |
|--------------|-----------|---------|
| **Ambiguous requirement** | Choose minimal interpretation | "Notifications" = in-app only, not email |
| **Nice-to-have** | Defer to future phase | Put in "What We're NOT Doing" |
| **Edge cases** | Handle top 3 common cases | 80/20 rule |
| **Error handling** | Match existing error handling patterns | Don't over-engineer if codebase doesn't |

#### Confidence Classification

For each decision, assign confidence:
- **HIGH**: Clear from codebase patterns or explicit requirement
- **MEDIUM**: Reasonable inference, but alternatives exist
- **LOW**: Best guess, human should review

### Step 4: Structure the Implementation

Break the work into phases following these rules:

1. **Phase Sizing**: Each phase should be 1-4 hours of work
2. **Phase Independence**: Each phase should be testable alone
3. **Risk Ordering**: Riskiest/most uncertain work first
4. **Dependency Ordering**: Foundation before features
5. **Phase Count**: Typically 2-5 phases for most features

**Standard Phase Pattern:**
```
Phase 1: Data Model / Schema (if needed)
Phase 2: Core Logic / Business Rules
Phase 3: API / Interface Layer
Phase 4: UI / Client Integration (if needed)
Phase 5: Polish / Edge Cases (if needed)
```

### Step 5: Write the Implementation Plan

**Filename**: `thoughts/shared/plans/YYYY-MM-DD-[task-kebab-case].md`

```markdown
---
date: [ISO timestamp]
planner: autonomous-agent
git_commit: [commit hash]
branch: [branch name]
source_ticket: [ticket reference if any]
autonomous: true
decisions_made: [count]
low_confidence_decisions: [count]
status: ready_for_implementation
---

# [Feature Name] Implementation Plan

## Autonomous Planning Summary

> This plan was created autonomously. Review flagged decisions before implementation.

| Metric | Value |
|--------|-------|
| Decisions Made | [count] |
| High Confidence | [count] |
| Medium Confidence | [count] |
| Low Confidence (review recommended) | [count] |

## Overview

[2-3 sentence description of what we're implementing]

## Source Requirements

[Quote or summarize the original requirements]

## Current State Analysis

[What exists now based on research]

### Key Discoveries:
- [Finding with file:line reference]
- [Pattern to follow]
- [Constraint discovered]

## Desired End State

[Specification of done state]

**Verification Method**: [How to confirm success]

## Autonomous Decisions Made

### High Confidence Decisions
| Decision | Choice | Rationale |
|----------|--------|-----------|
| [What] | [Choice] | [Why - based on codebase evidence] |

### Medium Confidence Decisions
| Decision | Choice | Rationale | Alternative Considered |
|----------|--------|-----------|----------------------|
| [What] | [Choice] | [Why] | [Other option] |

### Low Confidence Decisions (REVIEW RECOMMENDED)
| Decision | Choice | Rationale | Risk if Wrong |
|----------|--------|-----------|---------------|
| [What] | [Choice] | [Best guess why] | [What breaks] |

## What We're NOT Doing

[Explicitly out of scope - include anything deferred]

- [Scope item] - Reason: [why excluded]
- [Nice-to-have] - Deferred to: [future work]

## Implementation Approach

[High-level strategy]

---

## Phase 1: [Phase name]

### Overview
[What this phase accomplishes - 1-2 sentences]

### Changes Required:

#### 1. [Component/File]
**File**: `path/to/file.ext`
**Action**: [Create/Modify/Delete]
**Changes**:
[Description of changes]

```[language]
// The concrete change
// Be as concrete as possible
```

#### 2. [Next Component]
[Same structure...]

### Success Criteria:

#### Automated Verification:
- [ ] `[command]` passes - [what it verifies]
- [ ] `[command]` passes - [what it verifies]
- [ ] File exists: `path/to/expected/file`

#### Manual Verification:
- [ ] [Specific thing to test manually]
- [ ] [Another manual check]

**Phase Complete When**: All automated checks pass. Manual verification deferred to end.

---

## Phase 2: [Phase name]

[Same structure as Phase 1...]

---

## Testing Strategy

### Unit Tests
- [ ] `path/to/test_file.ext` - Tests [what]
- [ ] [Additional test files]

### Integration Tests
- [ ] [End-to-end scenario]

### Manual Testing Checklist (End of Implementation)
1. [ ] [Specific manual test]
2. [ ] [Another test]
3. [ ] [Edge case to verify]

## Rollback Plan

If implementation fails:
1. [How to revert]
2. [What to check]

## Performance Considerations

[Any performance implications - or "None expected" if minimal feature]

## References

- Source: `[ticket/doc path]`
- Similar Implementation: `[file:line]`
- Research: `[research doc if used]`
```

### Step 5.5: Automated Plan Review

After writing the plan, review it for technical correctness:

1. **Spawn the plan-reviewer agent**:
   ```
   Use Task tool with subagent_type="plan-reviewer":
   - Prompt: "Review the implementation plan at [plan_path]. Verify type compatibility, file paths, and completeness. Return structured review results."
   ```

2. **Process review results autonomously**:
   - Parse structured output
   - Apply fixes for all critical issues without human intervention
   - Log improvements as medium-confidence decisions

3. **Iteration logic**:
   - Check plan complexity from "Autonomous Planning Summary" section
   - Complex (3+ phases): Allow up to 2 review iterations
   - Simple (1-2 phases): Allow 1 review iteration
   - If issues remain after max iterations, flag as low-confidence

4. **Update plan with review metadata**:
   Add to the plan's frontmatter:
   ```yaml
   reviewed: true
   review_iterations: [count]
   critical_issues_fixed: [count]
   ```

5. **Add to decision log**:
   Any fixes applied should be added to the "Autonomous Decisions Made" section as high-confidence decisions with rationale "Identified and fixed by automated plan review."

**Autonomous Review Flow** (no user interaction):
```
PLAN REVIEW
===========
Spawning plan-reviewer agent...
Status: NEEDS_REVISION
Critical Issues: 1
- Phase 2: Wrong type for session_id (Uuid vs String)

Applying fix...
Re-reviewing...
Status: READY

Review complete. 1 critical issue fixed autonomously.
```

### Step 6: Validate Plan Completeness

Before finishing, verify:

1. **No placeholder text** - All sections filled with real content
2. **No open questions** - All decisions made
3. **Concrete code examples** - Not just descriptions
4. **Testable success criteria** - Commands that can be run
5. **File paths exist or are clearly new** - No guessing

If anything is incomplete, use heuristics to fill it or mark as low confidence.

### Step 7: Sync and Report

1. Ensure the updated plan is saved and committed so `thoughts/` stays consistent across environments
2. Output completion summary:

```
AUTONOMOUS PLANNING COMPLETE
============================
Plan: thoughts/shared/plans/[filename].md
Phases: [count]
Estimated Implementation: [X-Y] sub-agent turns

Decision Summary:
- High Confidence: [count] (proceed without review)
- Medium Confidence: [count] (review optional)
- Low Confidence: [count] (REVIEW RECOMMENDED)

Low Confidence Decisions to Review:
1. [Decision]: [Choice made] - Risk: [what could go wrong]
2. [If any more...]

Ready for: /auto_implement thoughts/shared/plans/[filename].md
```

## Error Handling

**Recoverable** (continue with flags):
- Can't determine best approach → Choose simplest, flag as low confidence
- Missing information → Make assumption, document it
- Conflicting patterns in codebase → Pick most recent, document

**Blocking** (stop and report):
- No requirements provided
- Cannot identify what to build
- Codebase is completely unfamiliar (no similar patterns)

## Important Notes

- **No human interaction** - All decisions autonomous
- **Conservative defaults** - When uncertain, choose simpler option
- **Audit trail** - Every decision documented with rationale
- **Actionable output** - Plan must be implementable without clarification
- **Parallel research** - Spawn all research agents in single message
- **Full file reads** - Never use limit/offset on files
