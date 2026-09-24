---
name: feature-status
description: Scan workflows and documents to produce a feature pipeline status report
---

# Feature Status Report

You are tasked with producing a comprehensive status report of every feature in the pipeline — both those tracked by the Workflow system and orphaned research/plan documents that predate it.

## CRITICAL: READ-ONLY OPERATION

- DO NOT modify any files
- DO NOT create new documents
- DO NOT advance workflow stages
- DO NOT launch implementation sessions
- ONLY read, cross-reference, and report

## Purpose

The user has ADHD and forgets about features that stall mid-pipeline. This command exists to answer one question: **"What did I start and forget about?"**

## Data Sources

You will cross-reference four sources to build the report:

1. **Workflow table** — query via daemon RPC (`ListWorkflows`). These are features the automated pipeline is tracking.
2. **Research documents** — scan `thoughts/shared/research/*.md` frontmatter for documents that have no corresponding Workflow entry.
3. **Plan documents** — scan `thoughts/shared/plans/*.md` frontmatter for plans whose parent research has no Workflow entry.
4. **Git branches** — check for branches that match research/plan filenames but were never merged.

## Process

### Step 1: Gather Workflow Data

Spawn a Sonnet agent to query the daemon for all workflows:

```bash
# Query via Unix socket
echo '{"jsonrpc":"2.0","id":1,"method":"ListWorkflows","params":{}}' | socat - UNIX-CONNECT:~/.rsi/daemon.sock
```

If the daemon is not running, skip this step and note it in the report. All features will be classified as untracked.

Parse the response into a list of `(workflow_id, title, stage, artifact_path, project_id, updated_at)`.

### Step 2: Scan Research Documents

Spawn a Sonnet agent to:

1. Glob `thoughts/shared/research/*.md`
2. For each file, read the YAML frontmatter (first 20 lines is sufficient)
3. Extract: `title` (from `topic` field or filename), `date`, `status`, `tags`, `pipeline_stage` (if present)
4. Record the file path

### Step 3: Scan Plan Documents

Spawn a Sonnet agent (can run in parallel with Step 2) to:

1. Glob `thoughts/shared/plans/*.md`
2. For each file, read the YAML frontmatter (first 20 lines)
3. Extract: `title` (from `topic` field or filename), `date`, `status`, `parent_research` (if present), `pipeline_stage` (if present)
4. Record the file path

### Step 4: Cross-Reference and Classify

For each research document, determine its pipeline stage:

| Condition | Derived Stage |
|-----------|--------------|
| Has a Workflow entry with stage `Complete` or `ImplementComplete` | **Implemented** |
| Has a Workflow entry with stage `Implementing` | **In Progress** |
| Has a Workflow entry with stage `PlanComplete` or `Planning` | **Planned** |
| Has a Workflow entry with stage `Research` or `ResearchComplete` | **Researched** |
| No Workflow entry, but a matching plan exists in `thoughts/shared/plans/` | **Planned (untracked)** |
| No Workflow entry, no matching plan | **Researched (orphaned)** |

Matching logic for research → plan:
- Compare by date prefix and slug (e.g., `2026-03-08-workflow-status` matches both `research/2026-03-08-workflow-status-and-idea-grouping.md` and `plans/2026-03-08-workflow-status-and-idea-grouping.md`)
- Also check `parent_research` field in plan frontmatter if present

### Step 5: Detect Staleness

For each feature, compute days since last activity:
- For Workflow-tracked features: use `updated_at`
- For untracked features: use file modification time or `date` from frontmatter

Flag as **STALE** if:
- Stage is `Researched` or `Planned` and last activity was > 14 days ago
- Stage is `In Progress` and last activity was > 7 days ago

### Step 6: Produce Report

Output a markdown table sorted by staleness (most stale first), then by stage (earliest stage first):

```
## Feature Pipeline Status Report

Generated: {date}

### Stale Features (Action Needed)

| Feature | Stage | Last Active | Days Stale | Path |
|---------|-------|-------------|------------|------|
| ...     | ...   | ...         | ...        | ...  |

### Active Features

| Feature | Stage | Last Active | Workflow ID | Path |
|---------|-------|-------------|-------------|------|
| ...     | ...   | ...         | ...         | ...  |

### Completed Features

| Feature | Stage | Completed | Workflow ID | Path |
|---------|-------|-----------|-------------|------|
| ...     | ...   | ...       | ...         | ...  |

### Summary

- Total features: {n}
- Stale (needs attention): {n}
- Active (in progress): {n}
- Completed: {n}
- Orphaned (no workflow): {n}
```

## Output

Print the report directly to the conversation. Do NOT write it to a file unless the user explicitly asks.

## Optional Parameters

- If invoked with a project name/ID, filter the report to that project only
- If invoked with `--stale`, show only stale features
- If invoked with `--orphaned`, show only untracked documents
