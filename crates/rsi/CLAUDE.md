# TUI crate (`rsi`)

## Keybinding Changes

When modifying keybindings, update `docs/keybindings.md` by running `make manual`
(regenerates its `rsi:generated` regions and `docs/manual/`; drift tests fail otherwise).

Sources:
- Normal mode: `ActionDescriptor` bindings in `crates/rsi/src/action_registry.rs`, installed by `crates/rsi/src/keybindings.rs`
- Commands: `crates/rsi/src/commands.rs`
- Overlays: `crates/rsi/src/overlay/mod.rs` (directory `crates/rsi/src/overlay/`)
- Input bar: `crates/rsi/src/input_bar.rs`

Adding an action end-to-end:
1. Add `LcAction` in `modalkit_types.rs`.
2. Add an `ActionDescriptor` (with `summary`) and its binding in `action_registry.rs`, plus its effect arm in
   `keybindings.rs` `registered_normal_effect` (hand `add_mapping` calls fail `normal_keymap_is_registry_sourced`).
3. Handle action in `action_handler/mod.rs` or the relevant submodule.
4. Run `make manual` to update `docs/keybindings.md` and `docs/manual/`.

## Session-list and row rendering

Rows are a projection, not a place to reassign meaning. A container
(`SessionKind::Group` / `Epic`) must render its OWN name as the primary title.
Surfacing child or rollup detail is fine as an addition, never as a replacement
-- a container whose title is overwritten becomes unfindable in the session
list, and `resolve_session_display` is the only place display naming is decided.

Order composite titles identity-first: titles are truncated to the available
column width, so whatever the row is *called* must come before any preview text
if it is to survive on narrow panes.

Do not write render tests that assert a user-visible name, title, or label is
ABSENT (`!text.contains(...)`). Assert the positive end state. Such a test once
pinned a regression that made every Group unfindable; see
`thoughts/shared/plans/2026-09-01-container-top-sorted-item.md`.
