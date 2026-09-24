# Flywheel vs Gastown vs ChatDev 2.0 Feature Analysis

## Overview

This directory contains the results of exhaustive static code analysis comparing AI agent orchestration tools across three distinct architectural layers:

- **Flywheel** (`/home/jakedevar/flywheel`) — A Vim-like TUI for managing multiple AI coding sessions across providers. Built in Rust using ratatui + crossterm + modalkit. **Layer: Interface/Observation.**
- **Gastown** (`/home/jakedevar/gastown`) — A multi-agent orchestration system for Claude Code with persistent work tracking. Built in Go using cobra + charmbracelet/bubbletea. **Layer: Operational Orchestration.**
- **ChatDev 2.0 (DevAll)** (`/home/jakedevar/ChatDev`) — A zero-code multi-agent platform for building and executing customized multi-agent systems through YAML graph definitions. Built in Python using FastAPI + Vue 3. **Layer: Computational Graph/Workflow Definition.**

The analysis covers all source files across all three projects.

---

## Summary Statistics

| Metric | Flywheel | Gastown | ChatDev 2.0 |
|--------|---------|---------|-------------|
| **Features documented** | 52 | 43 | — |
| **Flywheel↔Gastown matrix rows** | 52 | — | — |
| **Full equivalences (FW↔GT)** | 1 (Context Rotation/Handoff) | — | — |
| **Partial equivalences (FW↔GT)** | 30 | — | — |
| **No equivalence (FW only vs GT)** | 21 | — | — |
| **No equivalence (GT only vs FW)** | 13 | — | — |
| **GT unique → FW candidates** | 14 | — | — |
| **Node types** | N/A (sessions, not nodes) | N/A (roles, not nodes) | 8 built-in |
| **Execution strategies** | State machine | Formula workflows | 3 (DAG, Cycle, Majority Vote) |
| **Language** | Rust | Go | Python 3.12+ |

---

## Architectural Layer Model

The three systems occupy complementary layers in a full-stack agent orchestration platform:

```
+-----------------------------------------------------------+
| Layer 4: Interface / Observation                           |
|  Flywheel: vim-native TUI for session management           |
|  - Vim grammar, 18 overlays, split panes, tabs            |
|  - SQLite persistence, live token/cost display             |
+-----------------------------------------------------------+
| Layer 3: Operational Orchestration                          |
|  Gastown: multi-agent workforce management                 |
|  - Agent hierarchy, health monitoring, work tracking       |
|  - Beads/Convoys/Formulas, inter-agent communication       |
+-----------------------------------------------------------+
| Layer 1: Computational Graph / Workflow Definition          |
|  ChatDev 2.0: graph-based agent workflow engine            |
|  - 8 node types, YAML-defined DAGs/cycles                  |
|  - Edge conditions, dynamic fan-out, memory/thinking       |
|  - Function calling + MCP tool integration                 |
+-----------------------------------------------------------+
```

---

## Files in This Directory

### [flywheel_features.md](flywheel_features.md)

Comprehensive documentation of all 52 features identified in the Flywheel codebase, organized across categories:
- Session Management (lifecycle, rotation, TaskRabbit, docregblocks, pipeline)
- Provider Integration (Claude, Codex, Local/Ollama, Gemini, custom)
- Layout System (split trees, tabs/workspaces)
- Overlay System (17 distinct overlays documented)
- Input System (modalkit vim navigation, vim_textarea editing, suggestions, prompt correction)
- RPC Protocol (6 method groups: session lifecycle, conversations, archive, projects, discovery/health, memory)
- Persistence (SQLite schema/migrations, async write worker)
- Architecture (event bus, push notifications, poll controller)
- Rendering (markdown parsing, syntax highlighting, height pre-computation, audio, theme system, status bar)
- State Management (PersistedState, DevState)
- Navigation (jumplist, attention queue, search, folds, event visibility, yank)
- Analytics (token tracking, turn metrics, model discovery)
- Operational (daemon auto-start, log rotation, hot-reload, profiling)

### [gastown_features.md](gastown_features.md)

Comprehensive documentation of all 43 features identified in the Gastown codebase, organized across categories:
- Workspace (Town/Rig/Crew hierarchy, install/bootstrap)
- Agent Management (Mayor, Deacon, Witness, Polecats, Refinery, Dogs, crew management)
- Work Tracking (Beads integration, Convoy system, Hook system, Molecule/Wisp lifecycle)
- Work Dispatch (Sling, Formula system, Scheduler)
- Merge Management (MQ batch-bisect, Refinery integration)
- Communication (Nudge, Mail, DND, Signal system)
- Session Management (Prime, Handoff, Seance, Checkpoint)
- Operations (Doctor, Dolt Server, Daemon, Vitals, Patrol Digest, Six-Stage Data Lifecycle)
- Analytics (Costs, Agent State, Telemetry, Reputation/CV)
- Configuration (Config system, Operational config, Upgrade/migration)
- Extensibility (Plugin system)
- Federation (Wasteland/DoltHub)

### [comparison_matrix.md](comparison_matrix.md)

A comprehensive mapping of every Flywheel feature to its Gastown equivalent (or None), with equivalence classification (Full/Partial/None) and detailed notes explaining where the implementations diverge. 52 rows covering every Flywheel feature.

Key findings:
- **Full equivalence (1):** Context Rotation/Handoff — both solve the context window exhaustion problem with session handoff mechanisms.
- **Partial equivalence (30):** Most features have rough functional analogs in the other system, but implemented with completely different architectures (TUI vs CLI, single-agent vs multi-agent, SQLite vs Dolt).
- **No Flywheel equivalent in Gastown (20):** TUI-specific features (split panes, overlays, vim bindings, themes, mouse, syntax highlighting, audio, hot-reload) and Flywheel-specific integrations (Local/Ollama native, custom OpenAI provider UI).
- **No Gastown equivalent in Flywheel (13):** Multi-agent features (Witness health monitoring, Convoy system, Formula/Molecule workflows, Wasteland federation), operational features (Checkpoint crash recovery, Dolt server, patrol digests), and role taxonomy (Dogs, Deacon, Refinery as agents).

### [gastown_unique_for_flywheel.md](gastown_unique_for_flywheel.md)

14 Gastown capabilities with no Flywheel equivalent, each with:
- How Gastown implements it
- A concrete proposed Flywheel implementation (in Rust/TUI/daemon context)
- Priority rating (High/Medium/Low)
- Rationale for why it would benefit Flywheel users

**High priority candidates:**
1. **Witness — Automated Session Health Monitoring** — detect stalled sessions automatically, reducing oversight overhead
2. **Durable Hook System** — session state survives rotation/restart via `active_task` persistence
3. **Role-Based Context Injection** — automatic project README, memory, and git context injection at session start

**Medium priority candidates:**
4. **Multi-Agent Orchestration (Convoy + Sling)** — lightweight session grouping with shared-goal tracking
5. **`gt seance` — Predecessor Session Querying** — launch read-only resume of old sessions for context retrieval
6. **Formula System** — reusable prompt templates with variable substitution via slash-commands
7. **Cost Tracking and Analytics** — historical cost aggregation by time period (data already collected, aggregation missing)
8. **Checkpoint** — session crash recovery via per-session work context notes

**Low priority candidates:**
9. Molecule/Durable Multi-Step Workflows
10. DND flag to prevent accidental interruption
11. Agent Reputation/CV Analytics
12. Capacity-Controlled Dispatch Scheduler
13. Structured Session Signaling
14. Upgrade/Migration System

---

## Key Architectural Differences (Three-System View)

| Aspect | Flywheel | Gastown | ChatDev 2.0 |
|--------|---------|---------|-------------|
| **User model** | Single user, power-user TUI | Multi-agent orchestration platform | Zero-code workflow author |
| **Session model** | Flywheel manages sessions; user monitors | Agents self-manage via gt CLI | Graph nodes execute sequentially/parallel |
| **UI paradigm** | vim-like TUI (ratatui + modalkit) | CLI commands + tmux sessions | Web UI (Vue 3) + visual editor |
| **Storage** | SQLite (WAL mode) via daemon RPC | Dolt (git-backed MySQL) per-rig | File system only (WareHouse/) |
| **Communication** | JSON-RPC 2.0 over Unix socket | tmux send-keys + beads + nudge | Edge-based typed messages |
| **Language** | Rust (tokio async) | Go (cobra CLI) | Python 3.12+ (FastAPI + threads) |
| **Scale target** | 1 user, 5-20 sessions | 20-30 simultaneous agents | Single workflow execution |
| **Agent coordination** | N/A (single user) | Mayor/Witness/Deacon hierarchy | Graph topology (DAG/cycle) |
| **State persistence** | JSON files + SQLite | Dolt (git-backed SQL) + JSON state | In-memory (no cross-run state) |
| **Memory** | Hybrid FTS5+vector (nomic embeddings) | Key-value store (beads kv) | FAISS + semantic rerank |
| **LLM integration** | CLI subprocess spawning + HTTP | Black-box tmux runtime | Direct API calls (OpenAI, Gemini) |
| **Control flow** | Session state machine | Formula dependencies | Edge conditions + cycle detection |
| **Human interaction** | TUI approval (a/d keys) | CLI/Mail escalation | WebSocket-blocking graph nodes |
| **Extension model** | Compile-time Rust | Plugin system (.md + TOML) | Schema registry (6 registries) |

---

## Analysis Method

Analysis was performed by reading all source files across all three projects:

**Flywheel (Rust):**
- `/home/jakedevar/flywheel/CLAUDE.md` and all `/home/jakedevar/flywheel/docs/*.md`
- All files in `/home/jakedevar/flywheel/crates/flywheel/src/` (including action_handler/, overlay/, ui/ subdirectories)
- All files in `/home/jakedevar/flywheel/crates/flywheeld/src/`
- All files in `/home/jakedevar/flywheel/crates/flywheel-common/src/`

**Gastown (Go):**
- `/home/jakedevar/gastown/README.md`, `AGENTS.md`, `CHANGELOG.md`, `go.mod`, `Makefile`
- All files in `/home/jakedevar/gastown/docs/`
- All files in `/home/jakedevar/gastown/internal/cmd/` (500+ .go files)

**ChatDev 2.0 (Python):**
- `/home/jakedevar/ChatDev/README.md`, `pyproject.toml`, `run.py`, `server_main.py`
- All files in `/home/jakedevar/ChatDev/runtime/` (node types, executors, edge conditions, memory, providers)
- All files in `/home/jakedevar/ChatDev/workflow/` (graph executor, topology builder, cycle manager, strategies)
- All files in `/home/jakedevar/ChatDev/entity/` (config loader, graph config, node/edge configs, messages)
- All files in `/home/jakedevar/ChatDev/server/` (FastAPI app, routes, services, WebSocket manager)
- All files in `/home/jakedevar/ChatDev/functions/` (function calling tools, edge conditions)
- All files in `/home/jakedevar/ChatDev/docs/user_guide/en/` (all English documentation)

---

## Previous Analysis

All three systems are advanced AI orchestration tools, but they approach the problem from fundamentally different philosophical directions:

- **Flywheel** is optimized for a single power user needing high information density, low-latency keyboard workflows (Vim bindings), and deep visibility into parallel LLM sessions. It acts as a sophisticated TUI client over a local JSON-RPC daemon, treating agents as individual parallel sessions.
- **Gastown** is a multi-agent orchestrated system designed for complex, durable, asynchronous workflows. It utilizes a git-backed persistence layer (Dolt), background watchdogs (Deacon, Witness), and explicit multi-agent coordination (Convoys, Beads, Mayor) to execute work reliably across sessions.
- **ChatDev 2.0 (DevAll)** is a computational graph framework for defining and executing multi-agent workflows through YAML configuration. It provides the lowest-level workflow primitives — typed nodes, conditioned edges, cycle detection, dynamic fan-out, and hierarchical subgraph composition — enabling users to define agent interaction topologies without code.

While Flywheel excels in immediate interactive control and UI layout, Gastown provides superior guarantees for unattended multi-step execution and robust crash recovery, and ChatDev 2.0 provides the most flexible workflow definition language with first-class support for branching, looping, and multi-agent graph topologies.

## Documents

1. **[Flywheel Features](flywheel_features.md)**: A complete inventory of Flywheel's capabilities, detailing its UI-first, synchronous approach, Rust/TUI implementation details, and provider integrations.
2. **[Gastown Features](gastown_features.md)**: A comprehensive breakdown of Gastown's multi-agent architecture, including its hierarchical workspace (Town/Rig/Crew), specialized agent roles, and git-backed data model.
3. **[Comparison Matrix](comparison_matrix.md)**: A side-by-side mapping of equivalent features between Flywheel and Gastown, highlighting how each solves common orchestration problems (e.g., context rotation, workspace organization, state persistence).
4. **[Gastown Capabilities Unique to Flywheel](gastown_unique_for_flywheel.md)**: A prioritized proposal for implementing Gastown's most powerful unique features within Flywheel's single-user architecture.

### Cross-References (Standalone Comparison Documents)

5. **[ChatDev 2.0 vs Flywheel Comparison](../thoughts/shared/research/2026-03-12-chatdev2-flywheel-comparison.md)**: Full standalone comparison between ChatDev 2.0 (DevAll) and Flywheel, using Gastown as the comparison baseline. Covers graph engine architecture, node types, message passing, control flow, memory systems, human interaction, and a three-system comparison matrix.
6. **[MASFactory vs Flywheel Comparison](../thoughts/shared/research/2026-03-12-masfactory-flywheel-comparison.md)**: Comparison with MASFactory (a separate Python graph framework, distinct from ChatDev 2.0). Includes four-system architectural layer analysis with Symphony (OpenAI).
7. **[Symphony vs Flywheel Comparison](../thoughts/shared/research/2026-03-12-symphony-flywheel-comparison.md)**: Comparison with Symphony (OpenAI's issue-driven dispatch service).

## Key Findings

- **Three-Layer Complementarity**: Flywheel (interface), Gastown (operational orchestration), and ChatDev 2.0 (computational graph) occupy distinct and complementary layers. A unified system could use ChatDev 2.0 to define workflows, Gastown to operationally orchestrate agents, and Flywheel to observe and control the process.
- **Multi-Agent vs. Parallel Sessions vs. Graph Nodes**: Gastown coordinates autonomous agents, Flywheel manages parallel interactive sessions, and ChatDev 2.0 executes graph-defined agent topologies. These are three fundamentally different models of "running multiple AI agents."
- **State Persistence Spectrum**: ChatDev 2.0 is entirely in-memory (no cross-run state), Flywheel uses SQLite with 14 migrations, and Gastown uses Dolt (git-versioned SQL) with a six-stage data lifecycle. The persistence sophistication correlates inversely with workflow definition flexibility.
- **Context Management**: Flywheel auto-rotates at 65% context fill; Gastown uses `gt prime` + hooks; ChatDev 2.0 handles context within the graph via `context_window` per-node settings and edge `clear_context` flags. Each approach matches its architectural paradigm.
- **Workflow Definition Power**: ChatDev 2.0's YAML workflow language with cycle detection, dynamic fan-out, and subgraph composition is the most expressive workflow definition system of the three. Neither Flywheel's docregblock chains nor Gastown's TOML formulas approach its flexibility.
- **Immediate Opportunities for Flywheel**: Implementing lightweight equivalents of Gastown's Witness (stall detection), Hooks (durable task context), and Seance (querying predecessor sessions) would significantly enhance Flywheel's reliability during long-running workflows.
