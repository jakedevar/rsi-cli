# Verification Manifest Schema

Verification manifests are markdown files with YAML frontmatter. They live at:

```text
thoughts/shared/verification/<ticket-id>-<YYYY-MM-DD>.md
```

The manifest is the single source of truth for verification state. Worker
handoffs point at it with `Manifest path:`; they do not carry inline manual
verification checklists.

## Frontmatter

Required keys:

```yaml
ticket: V1.1
plan_doc: thoughts/shared/plans/2026-05-06-example.md
branch: v1.1-example
generated: 2026-05-06T14:23:00Z
phases_sealed: [1, 2]
status: pending_verification
```

`branch` is optional. `status` must be one of:

- `pending_verification`
- `tui_only_pending`
- `verified`
- `failed`

`phases_sealed` must match the `## Phase N` blocks present in the body. A new
skeleton manifest may have `phases_sealed: []` and no phase blocks.

## Phase Blocks

Every sealed phase block must contain all three bucket headings:

```markdown
## Phase 1 - title

### Automated
- cargo test --workspace

### Daemon-level
- [PENDING] rsi-rpc ListSessions returns JSON
  check: RSI_DAEMON_SOCKET_PATH=/tmp/rsi/daemon.sock rsi-rpc ListSessions
  expected: stdout parses as JSON object with result key

### TUI manual
- [ ] gv overlay highlights the expected row
```

Bucket headings may include parenthetical notes, such as
`### Automated (PASSED in CI)`, but the heading must start with the bucket name.

## Bucket Rules

Automated items are CI or Rust test commands. If behavior can be asserted with
`#[test]` or `#[tokio::test]`, it belongs here and should not be emitted as a
manual item.

Daemon-level items are autonomous verifier work. Every daemon item must include
both `check:` and `expected:` lines.

TUI manual items are only for checks requiring visual rendering, focus, color,
or keyboard interaction outcomes that do not yet have an automated TUI harness.

## Validation

Run:

```bash
cargo run -q -p rsi-common --bin rsi-manifest-validate -- thoughts/shared/verification/<file>.md
```

Exit code `0` means valid. Exit code `2` means the manifest is malformed or
violates schema rules. The validator rejects missing daemon `check:` lines,
missing bucket headings, malformed frontmatter, and mid-phase/sub-task phase
headings.
