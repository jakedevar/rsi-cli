//! Claude Code hook add/edit form overlay.
//!
//! Mirrors the shape of `provider_form.rs` (Tab/BackTab cycling, Enter to
//! save, Esc to cancel) and adds:
//!   * an `event_idx` cyclable picker for the event-name field, scoped to
//!     `KnownHookEvent::ALL` so the user can't accidentally enter an
//!     unrecognized name;
//!   * a Q7 conflict prompt that triggers when the on-disk file has changed
//!     since the form was opened.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::app::App;
use crate::claude_config::{
    self, ClaudeSettings, HookCommand, HookEntry, HookEvent, KnownHookEvent, PendingHookSave,
};
use crate::types::{HookFormEditingTarget, OverlayState};

const FIELD_COUNT: usize = 4;
const FIELD_EVENT: usize = 0;
const FIELD_MATCHER: usize = 1;
const FIELD_COMMAND: usize = 2;
const FIELD_TIMEOUT: usize = 3;

/// Open the Hook form overlay. Pass `None` to add a new entry, or
/// `Some(target)` to edit a specific row.
pub fn open_hook_form(app: &mut App, target: Option<HookFormEditingTarget>) {
    // Always re-load from disk so the snapshot is fresh. If load fails we
    // still let the user open an empty form — the conflict prompt at save
    // time will catch any drift.
    let loaded = match claude_config::load_user_settings() {
        Ok(l) => l,
        Err(e) => {
            app.notify_error(format!("Failed to read ~/.claude/settings.json: {}", e));
            return;
        }
    };

    let (focused_field, event_idx, event_name_other, matcher, command, timeout, editing) =
        match target {
            Some(target) => {
                let entry = loaded
                    .data
                    .hooks
                    .get(&target.event)
                    .and_then(|entries| entries.get(target.entry_index));
                let cmd = entry.and_then(|e| e.hooks.get(target.hook_index));
                let matcher = entry.and_then(|e| e.matcher.clone()).unwrap_or_default();
                let (command, timeout) = match cmd {
                    Some(c) => (c.command.clone(), c.timeout.to_string()),
                    None => (String::new(), "60".to_string()),
                };
                let (idx, other) = match &target.event {
                    HookEvent::Known(k) => (
                        Some(KnownHookEvent::ALL.iter().position(|e| e == k).unwrap_or(0)),
                        None,
                    ),
                    HookEvent::Other(s) => (None, Some(s.clone())),
                };
                (
                    FIELD_COMMAND,
                    idx,
                    other,
                    matcher,
                    command,
                    timeout,
                    Some(target),
                )
            }
            None => (
                FIELD_EVENT,
                Some(0),
                None,
                String::new(),
                String::new(),
                "60".to_string(),
                None,
            ),
        };

    app.overlay = OverlayState::HookForm {
        focused_field,
        event_idx,
        event_name_other,
        matcher,
        command,
        timeout,
        editing,
        snapshot_mtime: loaded.mtime,
        snapshot_bytes: loaded.original_bytes,
    };
}

/// Handle key events inside the HookForm overlay.
pub(super) fn handle_hook_form_key(app: &mut App, key: KeyEvent) {
    let focused_field = match &app.overlay {
        OverlayState::HookForm { focused_field, .. } => *focused_field,
        _ => return,
    };

    match key.code {
        KeyCode::BackTab => cycle_field(app, false),
        KeyCode::Tab if key.modifiers.contains(KeyModifiers::SHIFT) => cycle_field(app, false),
        KeyCode::Tab => cycle_field(app, true),
        KeyCode::Up | KeyCode::Down if focused_field == FIELD_EVENT => {
            cycle_event(app, matches!(key.code, KeyCode::Down));
        }
        KeyCode::Enter => submit_hook_form(app),
        KeyCode::Esc => {
            app.overlay = OverlayState::None;
        }
        KeyCode::Char(c) => append_to_field(app, focused_field, c),
        KeyCode::Backspace => pop_from_field(app, focused_field),
        _ => {}
    }
}

fn cycle_field(app: &mut App, forward: bool) {
    if let OverlayState::HookForm { focused_field, .. } = &mut app.overlay {
        if forward {
            *focused_field = (*focused_field + 1) % FIELD_COUNT;
        } else {
            *focused_field = (*focused_field + FIELD_COUNT - 1) % FIELD_COUNT;
        }
    }
}

fn cycle_event(app: &mut App, forward: bool) {
    if let OverlayState::HookForm {
        event_idx,
        event_name_other,
        ..
    } = &mut app.overlay
    {
        // If the form is editing an `Other(String)` event, leave the event
        // alone — preserving the unknown name verbatim is more important
        // than offering edits.
        if event_name_other.is_some() {
            return;
        }
        let len = KnownHookEvent::ALL.len();
        let i = event_idx.unwrap_or(0);
        *event_idx = Some(if forward {
            (i + 1) % len
        } else {
            (i + len - 1) % len
        });
    }
}

fn append_to_field(app: &mut App, field: usize, c: char) {
    if let OverlayState::HookForm {
        matcher,
        command,
        timeout,
        ..
    } = &mut app.overlay
    {
        match field {
            FIELD_MATCHER => matcher.push(c),
            FIELD_COMMAND => command.push(c),
            FIELD_TIMEOUT => {
                if c.is_ascii_digit() {
                    timeout.push(c);
                }
            }
            _ => {}
        }
    }
}

fn pop_from_field(app: &mut App, field: usize) {
    if let OverlayState::HookForm {
        matcher,
        command,
        timeout,
        ..
    } = &mut app.overlay
    {
        match field {
            FIELD_MATCHER => {
                matcher.pop();
            }
            FIELD_COMMAND => {
                command.pop();
            }
            FIELD_TIMEOUT => {
                timeout.pop();
            }
            _ => {}
        }
    }
}

/// Build the new (event, matcher, command, timeout, editing) snapshot, validate,
/// then either save or surface a conflict prompt.
fn submit_hook_form(app: &mut App) {
    // Borrow the form fields up front and clone them out so we can drop the borrow.
    let (
        event_idx,
        event_name_other,
        matcher_str,
        command_str,
        timeout_str,
        editing,
        snapshot_mtime,
        snapshot_bytes,
    ) = match &app.overlay {
        OverlayState::HookForm {
            focused_field: _,
            event_idx,
            event_name_other,
            matcher,
            command,
            timeout,
            editing,
            snapshot_mtime,
            snapshot_bytes,
        } => (
            *event_idx,
            event_name_other.clone(),
            matcher.clone(),
            command.clone(),
            timeout.clone(),
            editing.clone(),
            *snapshot_mtime,
            snapshot_bytes.clone(),
        ),
        _ => return,
    };

    // Resolve the event name.
    let event = match (&event_name_other, event_idx) {
        (Some(s), _) => HookEvent::Other(s.clone()),
        (None, Some(i)) => match KnownHookEvent::ALL.get(i) {
            Some(k) => HookEvent::Known(*k),
            None => {
                app.notify_error("Invalid event selection");
                return;
            }
        },
        (None, None) => {
            app.notify_error("Event required");
            return;
        }
    };

    // --- Structural validation (Q6) --------------------------------------
    let trimmed_command = command_str.trim().to_string();
    if trimmed_command.is_empty() {
        app.notify("Command required");
        return;
    }
    let timeout: u32 = match timeout_str.trim().parse::<u32>() {
        Ok(n) if n > 0 => n,
        _ => {
            app.notify("Timeout must be a positive integer");
            return;
        }
    };

    let matcher_opt = if event.supports_matcher() {
        let m = matcher_str.trim();
        if m.is_empty() {
            None
        } else {
            Some(m.to_string())
        }
    } else {
        None
    };

    let new_cmd = HookCommand {
        kind: "command".to_string(),
        command: trimmed_command,
        timeout,
    };

    // --- Reload from disk + apply mutation -------------------------------
    // We re-load instead of carrying the open-time snapshot forward, so the
    // mutation lands on top of any changes the user made through the form
    // while the file evolved on disk. Conflict detection compares the raw
    // bytes captured at form-open against current bytes; if different, we
    // route to the prompt overlay and let the user choose.
    let current_loaded = match claude_config::load_user_settings() {
        Ok(l) => l,
        Err(e) => {
            app.notify_error(format!("Failed to re-read ~/.claude/settings.json: {}", e));
            return;
        }
    };

    let conflict = current_loaded.original_bytes != snapshot_bytes
        || (current_loaded.mtime != snapshot_mtime && !snapshot_bytes.is_empty());

    let mutated = apply_mutation(
        current_loaded.data.clone(),
        &editing,
        event,
        matcher_opt,
        new_cmd,
    );

    if conflict {
        app.overlay = OverlayState::HookConflictPrompt {
            pending: Box::new(PendingHookSave { data: mutated }),
        };
        return;
    }

    write_settings(app, &mutated);
}

/// Apply an add or edit mutation to a `ClaudeSettings`. Pure function so it
/// composes with the conflict-prompt overwrite path.
fn apply_mutation(
    mut data: ClaudeSettings,
    editing: &Option<HookFormEditingTarget>,
    event: HookEvent,
    matcher: Option<String>,
    new_cmd: HookCommand,
) -> ClaudeSettings {
    match editing {
        Some(target) => {
            // If the event was changed, remove the old cell and re-insert in the new bucket.
            let mut moved_to_new_event = false;
            if target.event != event {
                if let Some(entries) = data.hooks.get_mut(&target.event) {
                    if target.entry_index < entries.len() {
                        let entry = &mut entries[target.entry_index];
                        if target.hook_index < entry.hooks.len() {
                            entry.hooks.remove(target.hook_index);
                        }
                        if entry.hooks.is_empty() {
                            entries.remove(target.entry_index);
                        }
                    }
                    if entries.is_empty() {
                        data.hooks.remove(&target.event);
                    }
                }
                moved_to_new_event = true;
            }

            if moved_to_new_event {
                let bucket = data.hooks.entry(event).or_default();
                bucket.push(HookEntry {
                    matcher,
                    hooks: vec![new_cmd],
                });
            } else if let Some(entries) = data.hooks.get_mut(&event) {
                if let Some(entry) = entries.get_mut(target.entry_index) {
                    entry.matcher = matcher;
                    if let Some(slot) = entry.hooks.get_mut(target.hook_index) {
                        *slot = new_cmd;
                    } else {
                        entry.hooks.push(new_cmd);
                    }
                } else {
                    entries.push(HookEntry {
                        matcher,
                        hooks: vec![new_cmd],
                    });
                }
            } else {
                data.hooks.insert(
                    event,
                    vec![HookEntry {
                        matcher,
                        hooks: vec![new_cmd],
                    }],
                );
            }
        }
        None => {
            // New entry — append a fresh single-command bucket so each row is
            // independently editable.
            data.hooks.entry(event).or_default().push(HookEntry {
                matcher,
                hooks: vec![new_cmd],
            });
        }
    }
    data
}

/// Three-choice handler for the external-edit conflict prompt.
pub(super) fn handle_hook_conflict_key(app: &mut App, key: KeyEvent) {
    match key.code {
        KeyCode::Char('o') | KeyCode::Char('O') => {
            let pending = match &app.overlay {
                OverlayState::HookConflictPrompt { pending } => pending.data.clone(),
                _ => return,
            };
            write_settings(app, &pending);
        }
        KeyCode::Char('r') | KeyCode::Char('R') => {
            app.overlay = OverlayState::None;
            app.invalidate_claude_settings_cache();
            app.notify("Reloaded ~/.claude/settings.json from disk");
        }
        KeyCode::Char('c') | KeyCode::Char('C') | KeyCode::Esc => {
            app.overlay = OverlayState::None;
            app.notify("Save canceled");
        }
        _ => {}
    }
}

/// Persist the mutated settings, invalidate the cache, notify, and dismiss
/// the overlay.
fn write_settings(app: &mut App, data: &ClaudeSettings) {
    match claude_config::save_user_settings(data) {
        Ok(_) => {
            app.invalidate_claude_settings_cache();
            app.notify_success("Saved. Applies to new Claude sessions.");
            app.overlay = OverlayState::None;
        }
        Err(e) => {
            app.notify_error(format!("Failed to save: {}", e));
        }
    }
}
