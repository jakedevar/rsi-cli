---
description: Create comprehensive project specifications for new coding projects
model: opus
capability_class: architect
---

# Project Spec

You are tasked with helping users transform project ideas into comprehensive, actionable specifications through collaborative research and iteration.

## Initial Response

At invocation, send this opening response:
```
I'll help you create a project specification. Let's start by understanding your vision.

Please share:
1. What problem are you trying to solve? (or what do you want to build?)
2. Who is this for? (yourself, a team, public users?)
3. Any technology preferences or constraints?

I'll research options, ask clarifying questions, and help you build a complete spec.
```

Wait for the project details before starting the spec.

## Process Steps

### Step 1: Vision Extraction & Clarification

1. **Listen actively** to the user's initial description
2. **Ask probing questions** to uncover:
   - Core problem being solved
   - Target users and their needs
   - Scale expectations (personal tool vs. production system)
   - Timeline and resource constraints
   - Must-have vs. nice-to-have features
   - Any prior art or inspiration

3. **Summarize understanding** back to user:
   ```
   Let me confirm my understanding:

   **Problem**: [What you're solving]
   **Users**: [Who this is for]
   **Core Value**: [Why this matters]
   **Key Constraints**: [Time, tech, scale limitations]

   Is this accurate? What am I missing?
   ```

### Step 2: Technology Research

1. **Spawn parallel research tasks** to explore options:

   Use **web-search-researcher** to investigate:
   - Similar projects and how they approached the problem
   - Current best practices for this type of application
   - Technology comparisons relevant to constraints
   - Potential pitfalls and lessons learned by others

   Use **Explore** (if in existing repo) to find:
   - Existing patterns to leverage
   - Integration points to consider

2. **Present technology options with tradeoffs**:
   ```
   Based on your requirements, here are the technology options I'd recommend:

   **Option A: [Stack Name]**
   - Pros: [specific benefits for their use case]
   - Cons: [honest drawbacks]
   - Best if: [when to choose this]

   **Option B: [Stack Name]**
   - Pros: [specific benefits]
   - Cons: [honest drawbacks]
   - Best if: [when to choose this]

   Given [their specific constraint], I'd lean toward [recommendation]. Thoughts?
   ```

3. **Get explicit buy-in** on technology choices before proceeding

### Step 3: Architecture & Scope Definition

1. **Define MVP scope explicitly**:
   ```
   Let's define the Minimum Viable Product - what's the smallest thing that delivers value?

   **Must Have (MVP)**:
   - [Feature 1] - [why essential]
   - [Feature 2] - [why essential]

   **Should Have (v1.1)**:
   - [Feature] - [why deferred]

   **Won't Have (explicitly out of scope)**:
   - [Feature] - [why excluded]

   Does this scope feel right for your timeline?
   ```

2. **Sketch high-level architecture**:
   - Data flow
   - Key components
   - External dependencies
   - Deployment model

### Step 4: Write the Project Specification

1. **Generate metadata** (date, researcher info)
2. **Write to** `thoughts/shared/project/YYYY-MM-DD-project-name.md`
3. **Use this template**:

````markdown
---
date: [ISO timestamp]
author: [Name]
status: draft
project_type: [cli|web|api|library|desktop|mobile]
primary_language: [language]
tags: [project, specification, relevant-tags]
---

# Project: [Project Name]

## Vision

[2-3 sentence description of what this project is and why it matters]

## Problem Statement

[Clear articulation of the problem being solved]

### Who Has This Problem
[Target users and their context]

### Current Solutions & Their Gaps
[What exists today and why it's insufficient]

## Goals & Non-Goals

### Goals
- [Specific, measurable goal]
- [Another goal]

### Non-Goals (Explicitly Out of Scope)
- [What we're NOT building and why]
- [Another non-goal]

## Core Features (MVP)

### 1. [Feature Name]
**What**: [Description]
**Why**: [User value]
**Acceptance Criteria**:
- [ ] [Specific testable criterion]
- [ ] [Another criterion]

### 2. [Feature Name]
[Same structure...]

## Technology Stack

| Component | Choice | Rationale |
|-----------|--------|-----------|
| Language | [X] | [Why] |
| Framework | [X] | [Why] |
| Database | [X] | [Why] |
| Deployment | [X] | [Why] |

### Key Dependencies
- `[package]` - [purpose]

## Architecture Overview

```
[ASCII diagram or description of key components]
```

### Data Model
[Key entities and relationships]

### API Surface (if applicable)
[Main endpoints or interfaces]

## Project Structure

```
project-name/
├── src/
│   ├── [key directories explained]
├── tests/
├── docs/
└── [other key files]
```

## Development Workflow

### Setup
```bash
# Commands to get started
```

### Running Locally
```bash
# Commands to run
```

### Testing
```bash
# Commands to test
```

## Success Criteria

### Technical Success
- [ ] [Measurable technical criterion]
- [ ] [Another criterion]

### User Success
- [ ] [User-focused criterion]

## Risks & Mitigations

| Risk | Likelihood | Impact | Mitigation |
|------|------------|--------|------------|
| [Risk] | [H/M/L] | [H/M/L] | [Strategy] |

## Open Questions

[Any decisions still to be made - aim to resolve these before implementation]

## References

- [Inspiration/similar project]
- [Relevant documentation]
- [Research links from web search]

## Next Steps

1. [ ] Finalize any open questions
2. [ ] Create initial project structure
3. [ ] Implement [first feature]
````

### Step 5: Iterate & Refine

1. **Present draft for review**:
   ```
   I've created the project specification at:
   `thoughts/shared/project/YYYY-MM-DD-project-name.md`

   Key decisions documented:
   - [Decision 1]
   - [Decision 2]

   Please review and let me know:
   - Does the scope feel right?
   - Any missing features or constraints?
   - Ready to start implementation?
   ```

2. **Iterate until user is satisfied**

3. **Offer to scaffold the project**:
   ```
   Spec is complete! Would you like me to:
   1. Create the initial project structure and files?
   2. Generate a `/plan` for the first feature?
   3. Just keep the spec as reference for now?
   ```

## Important Guidelines

1. **Start with WHY**: Understand the problem deeply before jumping to solutions
2. **Be opinionated but flexible**: Recommend specific choices, but adapt to user preferences
3. **Scope ruthlessly**: Help users cut scope to reach value faster
4. **Make tradeoffs explicit**: Every choice has costs - surface them
5. **Include sources**: Link to research that informed recommendations
6. **No placeholder answers**: Every section should be filled with real decisions
7. **Assume nothing**: Ask about deployment, users, scale - don't assume personal project defaults

## Common Patterns by Project Type

### CLI Tool
- Focus on: argument parsing, output formatting, error handling
- Consider: shell completion, config files, man pages

### Web Application
- Focus on: authentication, data persistence, deployment
- Consider: SEO, accessibility, mobile responsiveness

### Library/Package
- Focus on: API design, documentation, versioning
- Consider: backwards compatibility, bundle size, dependencies

### API Service
- Focus on: endpoint design, authentication, rate limiting
- Consider: documentation, versioning, monitoring
