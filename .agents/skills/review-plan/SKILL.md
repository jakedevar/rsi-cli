---
name: review-plan
description: Review an existing implementation plan for technical correctness
---

# Review Plan

You are tasked with reviewing an existing implementation plan for technical correctness, type compatibility, and completeness.

## Initial Response

At invocation:

1. **If a plan path is provided**: Read it FULLY and begin review immediately
2. **If no plan path provided**:
   ```
   I'll help you review an implementation plan.

   Send the plan path to review, for example:
   `/review_plan thoughts/shared/plans/2026-01-23-feature.md`

   Tip: Use `rg --files thoughts/shared/plans` to locate recent plans.
   ```
   Wait for user input.

## Review Process

### Step 1: Read the Plan

1. **Read the entire plan file** using the Read tool WITHOUT limit/offset
2. **Identify key elements**:
   - Phases and their scope
   - Types and data structures referenced
   - File paths mentioned
   - Success criteria
   - Code examples

### Step 2: Spawn Plan-Reviewer Agent

```
Use Task tool with subagent_type="plan-reviewer":
- Prompt: "Review the implementation plan at [plan_path]. Check for:
  1. Type compatibility with actual codebase types
  2. Accuracy of file paths and references
  3. Completeness of phases and success criteria
  4. Any technical issues that would cause implementation to fail

  Focus on the types and patterns in this codebase. Return structured review results."
```

### Step 3: Process and Present Results

1. **Parse the structured output** from the plan-reviewer agent
2. **Present findings to user**:

```
## Plan Review: [plan filename]

### Overall Assessment
**Status**: [READY | NEEDS_REVISION | MAJOR_ISSUES]
**Complexity**: [Simple | Complex]

### Critical Issues Found: [count]
[List each critical issue with location and suggested fix]

### Improvements Recommended: [count]
[List each improvement with location and suggestion]

### Type Compatibility
[Summary of type checks - any mismatches found]

### Next Steps
[Based on findings - either "Ready to implement" or specific actions needed]
```

### Step 4: Offer to Apply Fixes

If critical issues were found:

```
Would you like me to apply the suggested fixes to the plan?

Critical issues to fix:
1. [Issue 1] - [Fix description]
2. [Issue 2] - [Fix description]

Reply "yes" to apply fixes, or specify which issues to address.
```

If user approves:
1. Apply fixes using Edit tool
2. Update plan frontmatter:
   ```yaml
   last_reviewed: [ISO timestamp]
   review_iterations: 1
   critical_issues_fixed: [count]
   ```
3. Optionally re-run review to verify fixes

## Important Guidelines

1. **Always read the full plan** before spawning the reviewer
2. **Present findings clearly** - prioritize critical issues
3. **Be actionable** - every issue should have a concrete fix
4. **Don't auto-apply** - always ask before making changes
5. **Track iterations** - if re-reviewing after fixes, note the iteration count
6. **Check internal consistency** - constraints and design decisions must be
   mutually satisfiable. Flag any decision that violates a constraint the same
   plan declares; that contradiction is a critical issue, not a nit.
7. **Check for identity loss** - if the plan removes a user-visible name,
   title, label, or identifier, confirm it names where the user finds it
   instead. Unreachable entities are a critical issue.

## Example Usage

```
User: /review_plan thoughts/shared/plans/2026-01-23-daemon-core.md
