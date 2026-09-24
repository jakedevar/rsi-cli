---
status: implementation-ready
---

# Inferred Acceptance Negative Fixture

## Implementation Approach

- [source] Existing migrations are versioned. (`AGENTS.md`)
- [inferred] The proposed helper can emit the exact DDL without executing or
  inspecting the helper.

## Success Criteria

- [ ] [observed] The repository exposes the pinned Rust toolchain.
- [ ] [source] Existing migration fingerprints remain immutable. (`AGENTS.md`)
- [ ] [inferred] The unexecuted SQL creates every required index and constraint.
