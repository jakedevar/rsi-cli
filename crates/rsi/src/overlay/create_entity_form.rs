//! Unified entity-creation form overlay (Group / Epic / Story / Task / Bug …).
//!
//! Replaces the single-field ContainerForm overlay. V1 (P2.1) owns three
//! rows: Kind segmented selector, Name input, mandatory tag-chip field.
//! V2 (P2.3) extends with:
//!   - Topology dropdown (Pattern 2 inline filter, Epic-only)
//!   - Provider / Model / Effort / Sandbox execution fields (leaf-only)
//!   - User-editable Parent row via `gp` form-local chord
//!   - Topology preview pane (`Space` toggle when topology focused)
//!
//! Locked decisions: §1.1 (unified modal), §1.2 (state machine field
//! visibility), §1.3/§1.4 (vim grammar), §1.5 (aesthetic), §B5/§B6
//! (tag chip + normalization), §B7 (auto-draft to DevState).

use crate::action_handler::session::parent_kind_of;
use crate::app::App;
use crate::types::{ChipStatus, CreateEntityDraft, CreateEntityField, OverlayState, TagChip};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use rsi_common::tag::normalize_tag;
use rsi_common::types::{SessionKind, Topology, is_container_kind, is_leaf_kind, legal_children};

/// Kind-driven field visibility state machine (Decision 3). Returns the
/// ordered list of focusable fields for a given Kind. Compile-time-constant
/// slices keep this allocation-free and the single choke point for ALL
/// visibility logic — Tab cycle, field hotkeys, and Kind-toggle focus-jump
/// all consult this helper.
pub(crate) mod visibility {
    use super::CreateEntityField;
    use rsi_common::types::{SessionKind, is_container_kind, is_leaf_kind};

    pub(crate) fn field_visibility(kind: SessionKind) -> &'static [CreateEntityField] {
        match kind {
            SessionKind::Group => &[
                CreateEntityField::Kind,
                CreateEntityField::Name,
                CreateEntityField::Tag,
                CreateEntityField::Parent,
            ],
            SessionKind::Epic => &[
                CreateEntityField::Kind,
                CreateEntityField::Name,
                CreateEntityField::Tag,
                CreateEntityField::Parent,
                CreateEntityField::Topology,
            ],
            k if is_leaf_kind(k) => &[
                CreateEntityField::Kind,
                CreateEntityField::Name,
                CreateEntityField::Body,
                CreateEntityField::Tag,
                CreateEntityField::Parent,
                CreateEntityField::Provider,
                CreateEntityField::Model,
                CreateEntityField::Effort,
                CreateEntityField::Sandbox,
            ],
            // Defensive fallback (should be unreachable given legal_children gating).
            k if is_container_kind(k) => &[
                CreateEntityField::Kind,
                CreateEntityField::Name,
                CreateEntityField::Tag,
                CreateEntityField::Parent,
            ],
            _ => &[
                CreateEntityField::Kind,
                CreateEntityField::Name,
                CreateEntityField::Tag,
            ],
        }
    }

    /// Returns true if `field` is in the visibility list for `kind`.
    pub(crate) fn is_visible(kind: SessionKind, field: CreateEntityField) -> bool {
        field_visibility(kind).contains(&field)
    }
}

/// Exact live create-entity leaf surface owned by one semantic launch request.
/// The visible form is frozen while this snapshot is pending; the snapshot can
/// therefore reopen the same surface if another transition displaced it before
/// an error or dropped transport reached the App task.
#[derive(Debug, Clone)]
pub(crate) struct PendingCreateEntityLeafForm {
    kind: SessionKind,
    name: String,
    body: crate::input_surface::InputSurface,
    tags: Vec<TagChip>,
    focused_field: CreateEntityField,
    insert_mode: bool,
    parent_id: Option<uuid::Uuid>,
    error: Option<String>,
    g_pending: bool,
    topology_id: Option<uuid::Uuid>,
    topology_filter: String,
    topology_selected_index: usize,
    topology_choices: Vec<Topology>,
    preview_open: bool,
    provider: Option<rsi_common::types::SessionProvider>,
    model: Option<String>,
    effort: Option<String>,
    sandbox: bool,
    model_dropdown: Option<crate::types::ModelDropdownState>,
    fallback_provider: rsi_common::types::SessionProvider,
}

impl PendingCreateEntityLeafForm {
    fn capture(app: &App) -> Option<Self> {
        let OverlayState::CreateEntityForm {
            kind,
            name,
            body,
            tags,
            focused_field,
            insert_mode,
            parent_id,
            error,
            g_pending,
            topology_id,
            topology_filter,
            topology_selected_index,
            topology_choices,
            preview_open,
            provider,
            model,
            effort,
            sandbox,
            model_dropdown,
        } = &app.overlay
        else {
            return None;
        };
        Some(Self {
            kind: *kind,
            name: name.clone(),
            body: body.clone(),
            tags: tags.clone(),
            focused_field: *focused_field,
            insert_mode: *insert_mode,
            parent_id: *parent_id,
            error: error.clone(),
            g_pending: *g_pending,
            topology_id: *topology_id,
            topology_filter: topology_filter.clone(),
            topology_selected_index: *topology_selected_index,
            topology_choices: topology_choices.clone(),
            preview_open: *preview_open,
            provider: *provider,
            model: model.clone(),
            effort: effort.clone(),
            sandbox: *sandbox,
            model_dropdown: model_dropdown.clone(),
            fallback_provider: app.selected_provider,
        })
    }

    fn into_overlay(self, error: Option<String>) -> OverlayState {
        OverlayState::CreateEntityForm {
            kind: self.kind,
            name: self.name,
            body: self.body,
            tags: self.tags,
            focused_field: self.focused_field,
            insert_mode: self.insert_mode,
            parent_id: self.parent_id,
            error: error.or(self.error),
            g_pending: self.g_pending,
            topology_id: self.topology_id,
            topology_filter: self.topology_filter,
            topology_selected_index: self.topology_selected_index,
            topology_choices: self.topology_choices,
            preview_open: self.preview_open,
            provider: self.provider,
            model: self.model,
            effort: self.effort,
            sandbox: self.sandbox,
            model_dropdown: self.model_dropdown,
        }
    }
}

pub(crate) fn finish_create_entity_leaf_launch(
    app: &mut App,
    pending: PendingCreateEntityLeafForm,
    accepted_session_id: uuid::Uuid,
) {
    let committed: Vec<String> = pending
        .tags
        .iter()
        .filter(|chip| chip.status == ChipStatus::Committed)
        .map(|chip| chip.value.clone())
        .collect();
    app.modal_defaults = crate::state::ModalDefaults {
        kind: Some(pending.kind),
        topology_id: pending.topology_id,
        tags: (!committed.is_empty()).then_some(committed),
        provider: pending.provider.or(Some(pending.fallback_provider)),
        model: pending.model.clone(),
        effort: pending.effort.clone(),
        sandbox: pending.sandbox,
    };
    crate::state::PersistedState::capture(app).save();
    app.create_entity_draft = None;
    crate::state::DevState::capture(app).save();
    app.overlay = OverlayState::None;
    app.notify_success(format!(
        "Created {:?}: {}",
        pending.kind,
        pending.name.trim()
    ));
    app.push_notification(
        crate::types::NotificationKind::SessionLaunching,
        crate::types::NotificationPriority::Low,
        "Session launching...".to_string(),
        Some(accepted_session_id),
    );
    app.mark_dirty();
}

pub(crate) fn restore_create_entity_leaf_launch(
    app: &mut App,
    pending: PendingCreateEntityLeafForm,
    error: String,
) {
    app.overlay = pending.into_overlay(Some(format!("Create failed: {error}")));
    auto_save_draft(app);
    app.mark_dirty();
}

/// Open the unified entity-creation form for the given kind under the
/// resolved parent. Containment is pre-checked; on rejection the overlay
/// still opens with `focused_field = Kind` and an error banner so the
/// user can pick a legal Kind without re-firing the chord. Auto-draft is
/// restored from `app.create_entity_draft` if present and matches the
/// current parent context.
///
/// P2.3: async to fetch the topology list once at open time
/// (Decision 4). Failure surfaces as an empty `topology_choices` plus a
/// placeholder render — never crashes the modal.
pub async fn open_create_entity_form(
    app: &mut App,
    kind: SessionKind,
    parent_id: Option<uuid::Uuid>,
) {
    let parent_kind = parent_kind_of(app, parent_id);
    let allowed = legal_children(parent_kind);

    let (error, focused_field) = if allowed.contains(&kind) {
        (None, CreateEntityField::Name)
    } else {
        let parent_label = parent_kind
            .map(|k| format!("{k:?}"))
            .unwrap_or_else(|| "root".to_string());
        (
            Some(format!("{kind:?} not allowed under {parent_label}")),
            CreateEntityField::Kind,
        )
    };

    // Restore the persistable subset from draft when parent matches.
    let (
        restored_name,
        restored_body,
        restored_tags,
        restored_topology,
        restored_provider,
        restored_model,
        restored_effort,
        restored_sandbox,
    ): (
        String,
        Vec<String>,
        Vec<TagChip>,
        Option<uuid::Uuid>,
        Option<rsi_common::types::SessionProvider>,
        Option<String>,
        Option<String>,
        bool,
    ) = match app.create_entity_draft.as_ref() {
        Some(draft) if draft.parent_id == parent_id => {
            let tags = draft
                .tags
                .iter()
                .map(|v| TagChip {
                    value: v.clone(),
                    status: ChipStatus::Committed,
                })
                .collect();
            (
                draft.name.clone(),
                draft.body.clone(),
                tags,
                draft.topology_id,
                draft.provider,
                draft.model.clone(),
                draft.effort.clone(),
                draft.sandbox,
            )
        }
        _ => {
            // P2.4 cold-start prefill from ModalDefaults. Keep the caller's
            // `kind` parameter intact (chord routes already pin it). Use
            // `app.modal_defaults` for the remaining dropdown fields, falling
            // back to None/empty/false on a true cold start.
            //
            // Note: open_create_entity_form(app, kind, parent_id) receives the
            // `kind` as a parameter; P2.2's chord routing pins it. This ticket
            // does NOT change that contract. The `cold_start_kind` helper +
            // tests ship in P2.4 for consumers (no-kind entrypoint) yet to be
            // added.
            let defaults = app.modal_defaults.clone();
            let tags: Vec<TagChip> = defaults
                .tags
                .clone()
                .map(|vs| {
                    vs.into_iter()
                        .map(|v| TagChip {
                            value: v,
                            status: ChipStatus::Committed,
                        })
                        .collect()
                })
                .unwrap_or_default();
            (
                String::new(),
                // Body is content, not a default — it never enters
                // `modal_defaults` (F-011); cold start is always empty.
                Vec::new(),
                tags,
                defaults.topology_id,
                defaults.provider,
                defaults.model.clone(),
                defaults.effort.clone(),
                defaults.sandbox,
            )
        }
    };

    // Body surface: restored from draft when non-empty, else default. The
    // surface sits unfocused at open, so it must idle in Normal mode
    // (F-016/F-028); focus transitions enter Insert (Phase 2).
    let body = if restored_body.iter().any(|l| !l.is_empty()) {
        let mut surface =
            crate::input_surface::InputSurface::new_insert_with_content(restored_body);
        surface.mode = crate::types::PopupMode::Normal;
        surface
    } else {
        crate::input_surface::InputSurface::default()
    };

    let insert_mode = matches!(
        focused_field,
        CreateEntityField::Name | CreateEntityField::Tag
    );

    // Fetch the topology list once at open time (Decision 4). On failure,
    // store an empty list — render path surfaces `[topologies unavailable]`
    // and gates the `t` / `Space` chords (Phase 2/3 handlers check
    // `topology_choices` before acting).
    let topology_choices: Vec<Topology> = match app.client.list_topologies(None).await {
        Ok(list) => list,
        Err(e) => {
            tracing::warn!(error = ?e, "list_topologies failed; topology field will show placeholder");
            Vec::new()
        }
    };

    app.overlay = OverlayState::CreateEntityForm {
        kind,
        name: restored_name,
        body,
        tags: restored_tags,
        focused_field,
        insert_mode,
        parent_id,
        error,
        g_pending: false,
        topology_id: restored_topology,
        topology_filter: String::new(),
        topology_selected_index: 0,
        topology_choices,
        preview_open: false,
        provider: restored_provider,
        model: restored_model,
        effort: restored_effort,
        sandbox: restored_sandbox,
        model_dropdown: None,
    };
    app.mark_dirty();
}

/// Handle keys in the unified entity-creation overlay. See the ticket §3
/// grammar table for the full key map.
pub(super) async fn handle_create_entity_form_key(app: &mut App, key: KeyEvent) {
    if app.interactive_create_entity_launch_pending() {
        app.notify_error("Entity creation is awaiting daemon acceptance; form preserved");
        return;
    }

    // Capture the current shape for routing decisions.
    let (focused_field, insert_mode, model_dropdown_open) = match &app.overlay {
        OverlayState::CreateEntityForm {
            focused_field,
            insert_mode,
            model_dropdown,
            ..
        } => (
            *focused_field,
            *insert_mode,
            model_dropdown.as_ref().map(|m| m.open).unwrap_or(false),
        ),
        _ => return,
    };

    // Model sub-overlay intercept (Phase 4). When the model dropdown is
    // open inside the form variant, route ALL keys to the dropdown widget
    // handler and act on the returned action. Esc closes only the dropdown.
    if model_dropdown_open {
        handle_model_dropdown_intercept(app, key);
        return;
    }

    // Explicit discard (Ctrl-D): wipe draft, close overlay. Highest priority.
    if let KeyCode::Char('d') = key.code {
        if key.modifiers.contains(KeyModifiers::CONTROL) {
            app.create_entity_draft = None;
            crate::state::DevState::capture(app).save();
            app.overlay = OverlayState::None;
            app.mark_dirty();
            return;
        }
    }

    // Submit unconditionally on Ctrl-Enter.
    if key.code == KeyCode::Enter && key.modifiers.contains(KeyModifiers::CONTROL) {
        submit_create_entity_form(app).await;
        return;
    }

    // Body two-tier routing (S1, Decision 5 — ProviderForm precedent):
    // while Body is focused, Tab/BackTab still cycle fields and Esc is
    // mode-gated, but every other key forwards to the surface with
    // `submit_on_enter: false` (plain Enter inserts a newline, never
    // submits). This pre-empts the form's normal-mode hotkey table
    // (Topology-filter precedent, F-008). The Ctrl-D / Ctrl+Enter form
    // intercepts above already fired (F-006).
    if focused_field == CreateEntityField::Body {
        handle_body_key(app, key).await;
        return;
    }

    // Insert-mode branches first (text/tag editing); normal-mode branches
    // afterwards (Tab cycle, field hotkeys, segment motions). Only Name and
    // Tag have insert mode — every other focus falls through to normal mode.
    if insert_mode {
        match focused_field {
            CreateEntityField::Name => handle_name_insert(app, key),
            CreateEntityField::Tag => handle_tag_insert(app, key),
            _ => {
                if handle_normal_mode(app, key).await {
                    return;
                }
            }
        }
        return;
    }

    handle_normal_mode(app, key).await;
}

/// Route a key to the in-form model dropdown widget when it is open. On
/// Selected the picked model is written back into form.model + dropdown
/// closed. On Dismissed the dropdown is closed without mutating form.model.
fn handle_model_dropdown_intercept(app: &mut App, key: KeyEvent) {
    use crate::widget::model_dropdown::{ModelDropdownAction, handle_model_dropdown_key};
    // Snapshot custom_providers because handle_model_dropdown_key takes a
    // separate slice argument; clone keeps the borrow checker happy.
    let custom_providers = app.settings.custom_providers.clone();
    let action: ModelDropdownAction = if let OverlayState::CreateEntityForm {
        model_dropdown: Some(state),
        ..
    } = &mut app.overlay
    {
        handle_model_dropdown_key(state, &key, &custom_providers)
    } else {
        return;
    };
    match action {
        ModelDropdownAction::Selected(model_id) => {
            let selected_provider = app.selected_provider;
            let selected_model = app.selected_model.clone();
            if let OverlayState::CreateEntityForm {
                model,
                model_dropdown,
                provider,
                effort,
                ..
            } = &mut app.overlay
            {
                *model = Some(model_id);
                // Carry the dropdown's chosen provider into the form's
                // provider field (the user may have cycled providers
                // inside the dropdown before picking a model).
                if let Some(ds) = model_dropdown.as_ref() {
                    *provider = Some(ds.provider);
                }
                let effective_model = model
                    .as_deref()
                    .or(selected_model.as_deref())
                    .unwrap_or_else(|| default_effort_model(provider.unwrap_or(selected_provider)));
                rsi_common::model_utils::reconcile_effort(effective_model, effort);
                *model_dropdown = None;
            }
            auto_save_draft(app);
            app.mark_dirty();
        }
        ModelDropdownAction::Dismissed => {
            if let OverlayState::CreateEntityForm { model_dropdown, .. } = &mut app.overlay {
                *model_dropdown = None;
            }
            app.mark_dirty();
        }
        ModelDropdownAction::ProviderCycled => {
            let cycled_provider = match &app.overlay {
                OverlayState::CreateEntityForm {
                    model_dropdown: Some(state),
                    ..
                } => Some(state.provider),
                _ => None,
            };
            if let Some(provider) = cycled_provider {
                request_model_refresh_for_dropdown(app, provider);
            }
            app.mark_dirty();
        }
        ModelDropdownAction::Consumed | ModelDropdownAction::Ignored => {
            app.mark_dirty();
        }
    }
}

/// Body-focused key routing (S1, Decision 5). Two-tier: form-level keys
/// (Tab/BackTab cycle, mode-gated Esc) first, everything else forwards to
/// the shared `input_surface` engine with `submit_on_enter: false` so plain
/// Enter inserts a newline. The surface's own Ctrl+Enter submit is shadowed
/// by the form intercept in `handle_create_entity_form_key`, so its
/// newline-collapsing `content_for_send` path is never reached (F-019).
async fn handle_body_key(app: &mut App, key: KeyEvent) {
    // Tab / BackTab always cycle fields (unchanged semantics).
    match key.code {
        KeyCode::Tab => {
            cycle_field(app, true);
            app.mark_dirty();
            return;
        }
        KeyCode::BackTab => {
            cycle_field(app, false);
            app.mark_dirty();
            return;
        }
        _ => {}
    }

    // Esc is mode-gated (ProviderForm precedent, F-029): Normal-mode Esc
    // saves the draft and closes; Insert-mode Esc falls through to the
    // surface (drops to Normal, form stays open).
    let body_mode = match &app.overlay {
        OverlayState::CreateEntityForm { body, .. } => body.mode,
        _ => return,
    };
    if key.code == KeyCode::Esc && body_mode == crate::types::PopupMode::Normal {
        auto_save_draft(app);
        app.overlay = OverlayState::None;
        app.mark_dirty();
        return;
    }

    let action = {
        let OverlayState::CreateEntityForm { body, .. } = &mut app.overlay else {
            return;
        };
        crate::input_surface::handle_key(
            body,
            key,
            &crate::input_surface::InputSurfaceConfig {
                pass_through_unhandled: false,
                available_commands: &[],
                working_dir: None,
                submit_on_enter: false,
            },
        )
    };
    // Keystroke-granular draft persistence, parity with Name/Tag (F-027).
    auto_save_draft(app);
    match action {
        // Defensive: Submit can only arise from the surface's internal
        // Ctrl+Enter, which the form intercept already swallowed.
        crate::input_surface::InputAction::Submit(_) => {
            submit_create_entity_form(app).await;
        }
        // `q` in surface-normal mode maps to Close for overlay configs;
        // the body is a field, not a closable overlay — ignore.
        crate::input_surface::InputAction::Close
        | crate::input_surface::InputAction::Consumed
        | crate::input_surface::InputAction::Passthrough(_)
        | crate::input_surface::InputAction::CompileDecision { .. } => {}
    }
    app.mark_dirty();
}

fn handle_name_insert(app: &mut App, key: KeyEvent) {
    match key.code {
        KeyCode::Esc => {
            if let OverlayState::CreateEntityForm { insert_mode, .. } = &mut app.overlay {
                *insert_mode = false;
            }
            app.mark_dirty();
        }
        KeyCode::Backspace => {
            if let OverlayState::CreateEntityForm { name, .. } = &mut app.overlay {
                name.pop();
            }
            auto_save_draft(app);
            app.mark_dirty();
        }
        KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
            if let OverlayState::CreateEntityForm { name, .. } = &mut app.overlay {
                name.push(c);
            }
            auto_save_draft(app);
            app.mark_dirty();
        }
        _ => {}
    }
}

fn handle_tag_insert(app: &mut App, key: KeyEvent) {
    match key.code {
        KeyCode::Esc => {
            commit_pending_chip(app);
            if let OverlayState::CreateEntityForm { insert_mode, .. } = &mut app.overlay {
                *insert_mode = false;
            }
            auto_save_draft(app);
            app.mark_dirty();
        }
        KeyCode::Char(' ' | ',') | KeyCode::Tab => {
            commit_pending_chip(app);
            auto_save_draft(app);
            app.mark_dirty();
        }
        KeyCode::Backspace => {
            let pending_is_empty = match &app.overlay {
                OverlayState::CreateEntityForm { tags, .. } => match tags.last() {
                    Some(chip) if chip.status == ChipStatus::Pending => chip.value.is_empty(),
                    _ => true,
                },
                _ => return,
            };

            if pending_is_empty {
                if let OverlayState::CreateEntityForm { tags, .. } = &mut app.overlay {
                    if let Some(chip) = tags.last() {
                        if chip.status == ChipStatus::Pending {
                            tags.pop();
                        }
                    }
                    tags.pop();
                }
            } else if let OverlayState::CreateEntityForm { tags, .. } = &mut app.overlay {
                if let Some(chip) = tags.last_mut() {
                    chip.value.pop();
                }
            }
            auto_save_draft(app);
            app.mark_dirty();
        }
        KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
            if let OverlayState::CreateEntityForm { tags, .. } = &mut app.overlay {
                let needs_new_pending = match tags.last() {
                    Some(chip) => chip.status != ChipStatus::Pending,
                    None => true,
                };
                if needs_new_pending {
                    tags.push(TagChip {
                        value: String::new(),
                        status: ChipStatus::Pending,
                    });
                }
                if let Some(chip) = tags.last_mut() {
                    chip.value.push(c);
                }
            }
            auto_save_draft(app);
            app.mark_dirty();
        }
        _ => {}
    }
}

async fn handle_normal_mode(app: &mut App, key: KeyEvent) -> bool {
    // ─── `g`-leader chord state machine (Phase 5) ────────────────────────
    // P2.3 Decision 6: the create-entity form is in the plain_text_overlay
    // whitelist, so the global vim machine's `<Space>gP` chord (OpenPromptCreator)
    // is never reached when the form is active. We handle `g`-leader
    // form-locally to support `gp` → open parent picker. Any other key
    // after `g` clears the pending state and re-dispatches as one-shot.
    let g_pending = matches!(
        &app.overlay,
        OverlayState::CreateEntityForm {
            g_pending: true,
            ..
        }
    );
    if g_pending {
        if let OverlayState::CreateEntityForm { g_pending, .. } = &mut app.overlay {
            *g_pending = false;
        }
        match key.code {
            KeyCode::Char('p') if key.modifiers.is_empty() => {
                push_to_parent_picker_sub_overlay(app);
                app.mark_dirty();
                return true;
            }
            KeyCode::Char('g') => {
                app.mark_dirty();
                return true;
            }
            KeyCode::Esc => {
                app.mark_dirty();
                return true;
            }
            _ => {
                // Unknown follow-key: fall through to one-shot dispatch
                // (no early return — the key reaches the match below).
            }
        }
    }

    // Set the `g`-leader (only when the topology field is NOT focused —
    // Topology has its own typed-char branch and uses `g` as filter input).
    if matches!(key.code, KeyCode::Char('g'))
        && !matches!(
            &app.overlay,
            OverlayState::CreateEntityForm {
                focused_field: CreateEntityField::Topology,
                ..
            }
        )
    {
        if let OverlayState::CreateEntityForm { g_pending, .. } = &mut app.overlay {
            *g_pending = true;
        }
        app.mark_dirty();
        return true;
    }

    // ─── Topology dropdown branch (Pattern 2 inline filter) ──────────────
    // When focused on Topology, typed chars mutate the filter buffer
    // (highest priority — pre-empts the global field-hotkey table so the
    // user can type "t" into the filter). Enter commits; Esc clears the
    // selection and returns focus to Kind.
    if let OverlayState::CreateEntityForm {
        focused_field: CreateEntityField::Topology,
        ..
    } = &app.overlay
    {
        if handle_topology_key(app, key) {
            return true;
        }
    }

    match key.code {
        KeyCode::Esc => {
            // Esc with overlay open + not in insert: persist draft, close.
            auto_save_draft(app);
            app.overlay = OverlayState::None;
            app.mark_dirty();
            true
        }
        KeyCode::Tab => {
            cycle_field(app, true);
            app.mark_dirty();
            true
        }
        KeyCode::BackTab => {
            cycle_field(app, false);
            app.mark_dirty();
            true
        }
        KeyCode::Enter => {
            submit_create_entity_form(app).await;
            true
        }
        KeyCode::Char('n') => {
            focus_field(app, CreateEntityField::Name, true);
            app.mark_dirty();
            true
        }
        // Body field hotkey (leaf-only; visibility helper rejects when
        // hidden). Focusing puts the body surface in Insert mode (S1).
        KeyCode::Char('b') => {
            focus_field(app, CreateEntityField::Body, false);
            app.mark_dirty();
            true
        }
        KeyCode::Char('k') => {
            focus_field(app, CreateEntityField::Kind, false);
            app.mark_dirty();
            true
        }
        KeyCode::Char('T') => {
            focus_field(app, CreateEntityField::Tag, true);
            app.mark_dirty();
            true
        }
        // Topology field hotkey (Epic-only; visibility helper rejects when hidden).
        KeyCode::Char('t') => {
            focus_field(app, CreateEntityField::Topology, false);
            // Reset filter + selection on focus entry (mirrors project_picker UX).
            if let OverlayState::CreateEntityForm {
                topology_filter,
                topology_selected_index,
                focused_field: CreateEntityField::Topology,
                ..
            } = &mut app.overlay
            {
                topology_filter.clear();
                *topology_selected_index = 0;
            }
            app.mark_dirty();
            true
        }
        // Execution field hotkeys (visibility-gated for leaf kinds only).
        //
        // Cycle direction: Ctrl+p = forward, Ctrl+Shift+p = backward.
        // Keep provider on a control chord so plain text-like keys stay free.
        // `e` cycles effort forward, `E` (Shift+e) backward.
        // `s` toggles sandbox. `m` opens the model sub-overlay.
        KeyCode::Char('p') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            // Visibility-gated: also focuses the field if it's hidden, focus
            // is a no-op. Hidden-field guard via focus_field's contract.
            let kind = match &app.overlay {
                OverlayState::CreateEntityForm { kind, .. } => *kind,
                _ => return true,
            };
            if !visibility::is_visible(kind, CreateEntityField::Provider) {
                return true;
            }
            focus_field(app, CreateEntityField::Provider, false);
            cycle_provider(app, true);
            auto_save_draft(app);
            app.mark_dirty();
            true
        }
        KeyCode::Char('P') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            let kind = match &app.overlay {
                OverlayState::CreateEntityForm { kind, .. } => *kind,
                _ => return true,
            };
            if !visibility::is_visible(kind, CreateEntityField::Provider) {
                return true;
            }
            focus_field(app, CreateEntityField::Provider, false);
            cycle_provider(app, false);
            auto_save_draft(app);
            app.mark_dirty();
            true
        }
        KeyCode::Char('e') => {
            let kind = match &app.overlay {
                OverlayState::CreateEntityForm { kind, .. } => *kind,
                _ => return true,
            };
            if !visibility::is_visible(kind, CreateEntityField::Effort) {
                return true;
            }
            focus_field(app, CreateEntityField::Effort, false);
            cycle_effort(app, true);
            auto_save_draft(app);
            app.mark_dirty();
            true
        }
        KeyCode::Char('E') => {
            let kind = match &app.overlay {
                OverlayState::CreateEntityForm { kind, .. } => *kind,
                _ => return true,
            };
            if !visibility::is_visible(kind, CreateEntityField::Effort) {
                return true;
            }
            focus_field(app, CreateEntityField::Effort, false);
            cycle_effort(app, false);
            auto_save_draft(app);
            app.mark_dirty();
            true
        }
        KeyCode::Char('s') => {
            let kind = match &app.overlay {
                OverlayState::CreateEntityForm { kind, .. } => *kind,
                _ => return true,
            };
            if !visibility::is_visible(kind, CreateEntityField::Sandbox) {
                return true;
            }
            focus_field(app, CreateEntityField::Sandbox, false);
            if let OverlayState::CreateEntityForm { sandbox, .. } = &mut app.overlay {
                *sandbox = !*sandbox;
            }
            auto_save_draft(app);
            app.mark_dirty();
            true
        }
        KeyCode::Char('m') => {
            let kind = match &app.overlay {
                OverlayState::CreateEntityForm { kind, .. } => *kind,
                _ => return true,
            };
            if !visibility::is_visible(kind, CreateEntityField::Model) {
                return true;
            }
            focus_field(app, CreateEntityField::Model, false);
            open_model_sub_overlay(app);
            app.mark_dirty();
            true
        }
        KeyCode::Char('i') => {
            // Enter insert mode on the currently focused text field.
            if let OverlayState::CreateEntityForm {
                focused_field,
                insert_mode,
                ..
            } = &mut app.overlay
            {
                if matches!(
                    focused_field,
                    CreateEntityField::Name | CreateEntityField::Tag
                ) {
                    *insert_mode = true;
                }
            }
            app.mark_dirty();
            true
        }
        KeyCode::Char('h') => {
            if matches!(
                &app.overlay,
                OverlayState::CreateEntityForm {
                    focused_field: CreateEntityField::Kind,
                    ..
                }
            ) {
                cycle_kind_segment(app, false);
                auto_save_draft(app);
                app.mark_dirty();
            }
            true
        }
        KeyCode::Char('l') => {
            if matches!(
                &app.overlay,
                OverlayState::CreateEntityForm {
                    focused_field: CreateEntityField::Kind,
                    ..
                }
            ) {
                cycle_kind_segment(app, true);
                auto_save_draft(app);
                app.mark_dirty();
            }
            true
        }
        _ => false,
    }
}

/// Pattern 2 filter: returns the indices into `choices` whose `name`
/// (case-insensitive) contains `filter`. Empty filter = all choices.
pub(crate) fn topology_filtered_indices(choices: &[Topology], filter: &str) -> Vec<usize> {
    let q = filter.to_lowercase();
    choices
        .iter()
        .enumerate()
        .filter(|(_, t)| q.is_empty() || t.name.to_lowercase().contains(&q))
        .map(|(i, _)| i)
        .collect()
}

/// Topology dropdown key handler (Pattern 2). Returns true if the key was
/// consumed by the dropdown.
fn handle_topology_key(app: &mut App, key: KeyEvent) -> bool {
    use crate::overlay::list;

    // Snapshot data for navigation bounds + Enter commit lookup.
    let (visible_count, chosen_topology) = {
        let (choices, filter, selected_index) = match &app.overlay {
            OverlayState::CreateEntityForm {
                topology_choices,
                topology_filter,
                topology_selected_index,
                ..
            } => (
                topology_choices.clone(),
                topology_filter.clone(),
                *topology_selected_index,
            ),
            _ => return false,
        };
        let visible = topology_filtered_indices(&choices, &filter);
        let count = visible.len();
        // Resolve highlighted topology (for Enter commit).
        let chosen = visible
            .get(selected_index)
            .and_then(|i| choices.get(*i))
            .map(|t| t.id);
        (count, chosen)
    };

    // j/k/G/Down/Up nav (but NOT `g` — Phase 5 reserves `g` for the form's
    // `gp` leader chord; here `g` falls through to be inserted into the
    // filter buffer).
    match key.code {
        KeyCode::Char('j') | KeyCode::Down => {
            if let OverlayState::CreateEntityForm {
                topology_selected_index,
                ..
            } = &mut app.overlay
            {
                if *topology_selected_index + 1 < visible_count {
                    *topology_selected_index += 1;
                }
            }
            app.mark_dirty();
            return true;
        }
        KeyCode::Char('k') | KeyCode::Up => {
            if let OverlayState::CreateEntityForm {
                topology_selected_index,
                ..
            } = &mut app.overlay
            {
                if *topology_selected_index > 0 {
                    *topology_selected_index -= 1;
                }
            }
            app.mark_dirty();
            return true;
        }
        KeyCode::Char('G') => {
            if let OverlayState::CreateEntityForm {
                topology_selected_index,
                ..
            } = &mut app.overlay
            {
                *topology_selected_index = visible_count.saturating_sub(1);
            }
            app.mark_dirty();
            return true;
        }
        _ => {}
    }
    // Acknowledge that `list` is referenced for parity with project_picker
    // even when the branches above route around the helper.
    let _ = list::handle_list_nav_key;

    match key.code {
        KeyCode::Enter => {
            // Commit the highlighted topology and advance focus.
            if let OverlayState::CreateEntityForm {
                topology_id,
                topology_filter,
                topology_selected_index,
                ..
            } = &mut app.overlay
            {
                if let Some(id) = chosen_topology {
                    *topology_id = Some(id);
                    topology_filter.clear();
                    *topology_selected_index = 0;
                }
            }
            // Advance focus to the next field in the visibility list (mirrors
            // Tab semantics so the user can flow forward).
            cycle_field(app, true);
            auto_save_draft(app);
            app.mark_dirty();
            true
        }
        KeyCode::Esc => {
            // Esc on the topology field clears the selection and returns
            // focus to Kind (per ticket §3).
            if let OverlayState::CreateEntityForm {
                topology_id,
                topology_filter,
                topology_selected_index,
                focused_field,
                preview_open,
                ..
            } = &mut app.overlay
            {
                *topology_id = None;
                topology_filter.clear();
                *topology_selected_index = 0;
                *preview_open = false;
                *focused_field = CreateEntityField::Kind;
            }
            auto_save_draft(app);
            app.mark_dirty();
            true
        }
        KeyCode::Backspace => {
            if let OverlayState::CreateEntityForm {
                topology_filter,
                topology_selected_index,
                ..
            } = &mut app.overlay
            {
                topology_filter.pop();
                *topology_selected_index = 0;
            }
            app.mark_dirty();
            true
        }
        // Space toggles the preview pane when topology is focused (Phase 3).
        KeyCode::Char(' ') => {
            if let OverlayState::CreateEntityForm { preview_open, .. } = &mut app.overlay {
                *preview_open = !*preview_open;
            }
            app.mark_dirty();
            true
        }
        KeyCode::Char(c) => {
            // Pattern 2 typing: mutate filter, reset selection.
            // Excludes the global `g` leader (Phase 5) — emitted before this
            // branch via the early-return tree.
            if let OverlayState::CreateEntityForm {
                topology_filter,
                topology_selected_index,
                ..
            } = &mut app.overlay
            {
                topology_filter.push(c);
                *topology_selected_index = 0;
            }
            app.mark_dirty();
            true
        }
        _ => false,
    }
}

/// Cycle focus through the kind-specific visibility list.
fn cycle_field(app: &mut App, forward: bool) {
    if let OverlayState::CreateEntityForm {
        kind,
        focused_field,
        insert_mode,
        body,
        ..
    } = &mut app.overlay
    {
        let list = visibility::field_visibility(*kind);
        if list.is_empty() {
            return;
        }
        let current_idx = list.iter().position(|f| f == focused_field).unwrap_or(0);
        let next_idx = if forward {
            (current_idx + 1) % list.len()
        } else {
            (current_idx + list.len() - 1) % list.len()
        };
        *focused_field = list[next_idx];
        // The form-level insert flag stays a Name/Tag concern; Body's mode
        // lives in its own surface (F-001/F-007 untouched).
        *insert_mode = matches!(
            focused_field,
            CreateEntityField::Name | CreateEntityField::Tag
        );
        // Focusing Body enters Insert so Tab-into-Body types immediately
        // (symmetric with Name/Tag auto-insert on focus).
        if *focused_field == CreateEntityField::Body && body.mode != crate::types::PopupMode::Insert
        {
            body.enter_insert(crate::modalkit_types::InsertStyle::Insert);
        }
    }
}

/// Focus `field` if it is in the current Kind's visibility list. Otherwise
/// no-op (Phase 1 — visibility-gated field hotkeys per ticket §2).
fn focus_field(app: &mut App, field: CreateEntityField, enter_insert: bool) {
    if let OverlayState::CreateEntityForm {
        kind,
        focused_field,
        insert_mode,
        body,
        ..
    } = &mut app.overlay
    {
        if !visibility::is_visible(*kind, field) {
            return;
        }
        *focused_field = field;
        *insert_mode =
            enter_insert && matches!(field, CreateEntityField::Name | CreateEntityField::Tag);
        // Focusing Body always enters surface Insert mode (Decision 5).
        if field == CreateEntityField::Body && body.mode != crate::types::PopupMode::Insert {
            body.enter_insert(crate::modalkit_types::InsertStyle::Insert);
        }
    }
}

/// Provider cycle order (built-in providers only; matches the order in
/// `widget/model_dropdown.rs::cycle_provider`).
const PROVIDER_CYCLE: &[rsi_common::types::SessionProvider] = &[
    rsi_common::types::SessionProvider::Claude,
    rsi_common::types::SessionProvider::Codex,
    rsi_common::types::SessionProvider::Pioneer,
    rsi_common::types::SessionProvider::OpenRouter,
    rsi_common::types::SessionProvider::Bedrock,
    rsi_common::types::SessionProvider::Local,
    rsi_common::types::SessionProvider::Antigravity,
    rsi_common::types::SessionProvider::Harness,
];

/// Cycle `form.provider` through the built-in providers. None on first
/// press defaults to the first builtin.
fn cycle_provider(app: &mut App, forward: bool) {
    if let OverlayState::CreateEntityForm {
        provider,
        model,
        effort,
        ..
    } = &mut app.overlay
    {
        let current_idx = provider
            .as_ref()
            .and_then(|p| PROVIDER_CYCLE.iter().position(|q| q == p));
        let next_idx = match current_idx {
            Some(idx) if forward => (idx + 1) % PROVIDER_CYCLE.len(),
            Some(idx) => (idx + PROVIDER_CYCLE.len() - 1) % PROVIDER_CYCLE.len(),
            None => 0,
        };
        *provider = Some(PROVIDER_CYCLE[next_idx]);
        // Provider, model, and effort are one form-local tuple. Do not carry
        // a model/effort selection into a provider that did not supply it.
        *model = None;
        *effort = None;
    }
}

const CLAUDE_DEFAULT_EFFORT_MODEL: &str = "claude-opus-5-5";
const CODEX_DEFAULT_EFFORT_MODEL: &str = "gpt-6-astra";

fn default_effort_model(provider: rsi_common::types::SessionProvider) -> &'static str {
    match provider {
        rsi_common::types::SessionProvider::Claude => CLAUDE_DEFAULT_EFFORT_MODEL,
        rsi_common::types::SessionProvider::Codex
        | rsi_common::types::SessionProvider::Pioneer
        | rsi_common::types::SessionProvider::OpenRouter
        | rsi_common::types::SessionProvider::Bedrock
        | rsi_common::types::SessionProvider::CodexAppServer => CODEX_DEFAULT_EFFORT_MODEL,
        _ => "",
    }
}

fn effort_ladder_for_selection(
    provider: Option<rsi_common::types::SessionProvider>,
    model: Option<&str>,
    selected_provider: rsi_common::types::SessionProvider,
    selected_model: Option<&str>,
) -> &'static [&'static str] {
    let use_selected_model = provider.is_none_or(|provider| provider == selected_provider);
    let provider = provider.unwrap_or(selected_provider);
    let effective_model = model
        .filter(|model| !model.is_empty())
        .or_else(|| {
            use_selected_model
                .then_some(selected_model)
                .flatten()
                .filter(|model| !model.is_empty())
        })
        .unwrap_or_else(|| default_effort_model(provider));
    let ladder = rsi_common::model_utils::effort_ladder(effective_model);
    if ladder.is_empty() {
        rsi_common::model_utils::effort_ladder(default_effort_model(provider))
    } else {
        ladder
    }
}

/// Cycle `form.effort` through the levels supported by the selected provider/model.
fn cycle_effort(app: &mut App, forward: bool) {
    let selected_provider = app.selected_provider;
    let selected_model = app.selected_model.clone();
    if let OverlayState::CreateEntityForm {
        effort,
        provider,
        model,
        ..
    } = &mut app.overlay
    {
        let ladder = effort_ladder_for_selection(
            *provider,
            model.as_deref(),
            selected_provider,
            selected_model.as_deref(),
        );
        if ladder.is_empty() {
            *effort = None;
            return;
        }

        *effort = match effort.as_deref() {
            None if forward => ladder.first().map(|level| (*level).to_string()),
            None => ladder.last().map(|level| (*level).to_string()),
            Some(current) => ladder
                .iter()
                .position(|level| *level == current)
                .and_then(|index| {
                    if forward {
                        ladder.get(index + 1)
                    } else {
                        index
                            .checked_sub(1)
                            .and_then(|previous| ladder.get(previous))
                    }
                })
                .map(|level| (*level).to_string()),
        };
    }
}

/// Open the parent-picker sub-overlay from the form's `gp` chord (Phase 5,
/// Decision 6/7). Snapshots the in-flight form into `app.create_entity_form_pending`
/// and replaces `app.overlay` with a `ParentPicker` variant using
/// `Uuid::nil()` as the form-pending sentinel. The picker's submit and
/// Esc paths recognize the sentinel and restore the form (writing back
/// the chosen parent_id on submit).
fn push_to_parent_picker_sub_overlay(app: &mut App) {
    // Snapshot the form's Kind so we can compute legal parent candidates.
    let form_kind = match &app.overlay {
        OverlayState::CreateEntityForm { kind, .. } => *kind,
        _ => return,
    };
    let candidates = crate::overlay::parent_picker::compute_parent_candidates(app, form_kind, None);
    // Move the form out, store it, and push the picker on top.
    let form = std::mem::replace(&mut app.overlay, OverlayState::None);
    app.create_entity_form_pending = Some(Box::new(form));
    app.overlay = OverlayState::ParentPicker {
        session_id: uuid::Uuid::nil(),
        candidates,
        selected: 0,
        query: String::new(),
    };
}

/// Open the model sub-overlay inside the form variant. Initializes the
/// dropdown for the currently-selected provider (or app.selected_provider
/// when None) and seeds it with the static `models_for_provider(provider)`
/// table, same as before. What's new: it also kicks off a background
/// discovery refresh for that provider, so a stale/minimal static fallback
/// (e.g. Pioneer's single offline entry — real discovery always refreshes
/// the live account catalog) is replaced once the daemon responds. See the
/// MODEL DISCOVERY RESULT arm in `event.rs`, which now also syncs this
/// form's `model_dropdown` field back when the result lands — previously it
/// synced the global/Prompt/settings dropdowns but not this one, so this
/// dropdown was permanently stuck on the static table. The dropdown's
/// selected model is pre-set to the form's current `model` if it appears in
/// the list.
fn open_model_sub_overlay(app: &mut App) {
    use crate::types::ModelDropdownState;

    let provider = match &app.overlay {
        OverlayState::CreateEntityForm { provider, .. } => {
            provider.unwrap_or(app.selected_provider)
        }
        _ => return,
    };
    let models = crate::app::models_for_provider(provider);
    let current_model: Option<String> = match &app.overlay {
        OverlayState::CreateEntityForm { model, .. } => {
            model.clone().or_else(|| app.selected_model.clone())
        }
        _ => None,
    };
    let state = ModelDropdownState::new(provider, models, current_model.as_deref());

    if let OverlayState::CreateEntityForm { model_dropdown, .. } = &mut app.overlay {
        *model_dropdown = Some(state);
    }
    request_model_refresh_for_dropdown(app, provider);
}

/// Trigger the shared background model-discovery task for `provider`.
/// Mirrors the `Prompt` overlay's `ProviderCycled` arm in `overlay/mod.rs`:
/// if a discovery task for a different provider is already in flight, this
/// queues behind it (`event.rs`'s kick-off arm requires `model_discovery_rx`
/// to be empty) rather than canceling it. Both feed the same `event.rs`
/// MODEL DISCOVERY RESULT arm that this form's dropdown is now synced from.
fn request_model_refresh_for_dropdown(app: &mut App, provider: rsi_common::types::SessionProvider) {
    app.model_refresh_provider = Some(provider);
    app.needs_model_refresh = true;
}

/// Cycle the Kind segment selector. After mutation, if the focused field
/// is no longer in the new kind's visibility list, jump focus to the
/// first element of the new list (the "Kind-toggle focus-jump" rule).
fn cycle_kind_segment(app: &mut App, forward: bool) {
    let parent_id = match &app.overlay {
        OverlayState::CreateEntityForm { parent_id, .. } => *parent_id,
        _ => return,
    };
    let parent_kind = parent_kind_of(app, parent_id);
    let allowed: Vec<SessionKind> = legal_children(parent_kind).iter().copied().collect();
    if allowed.is_empty() {
        return;
    }
    if let OverlayState::CreateEntityForm {
        kind,
        focused_field,
        insert_mode,
        ..
    } = &mut app.overlay
    {
        let current_idx = allowed.iter().position(|k| k == kind);
        let next_idx = match current_idx {
            Some(idx) if forward => (idx + 1) % allowed.len(),
            Some(idx) => (idx + allowed.len() - 1) % allowed.len(),
            None => 0,
        };
        *kind = allowed[next_idx];

        // Kind-toggle focus-jump: if the focused field is no longer in the
        // new visibility list, jump to the first legal field.
        let new_list = visibility::field_visibility(*kind);
        if !new_list.contains(focused_field) {
            if let Some(first) = new_list.first() {
                *focused_field = *first;
                *insert_mode = matches!(first, CreateEntityField::Name | CreateEntityField::Tag);
            }
        }
    }
}

/// Commit the trailing Pending chip via `normalize_tag`. Flips the chip's
/// status to `Committed` (and replaces its value with the normalized form)
/// or `Invalid` with the error message.
fn commit_pending_chip(app: &mut App) {
    if let OverlayState::CreateEntityForm { tags, .. } = &mut app.overlay {
        if let Some(chip) = tags.last_mut() {
            if chip.status != ChipStatus::Pending {
                return;
            }
            match normalize_tag(&chip.value) {
                Ok(normalized) => {
                    chip.value = normalized;
                    chip.status = ChipStatus::Committed;
                }
                Err(e) => {
                    chip.status = ChipStatus::Invalid(e.to_string());
                }
            }
        }
    }
}

/// Snapshot the live `CreateEntityForm` into `app.create_entity_draft` and
/// persist to `DevState`. Per locked decision §B7.
fn auto_save_draft(app: &mut App) {
    let Some(draft) = extract_draft(&app.overlay) else {
        return;
    };
    app.create_entity_draft = Some(draft);
    crate::state::DevState::capture(app).save();
}

fn extract_draft(overlay: &OverlayState) -> Option<CreateEntityDraft> {
    if let OverlayState::CreateEntityForm {
        kind,
        name,
        body,
        tags,
        parent_id,
        topology_id,
        provider,
        model,
        effort,
        sandbox,
        ..
    } = overlay
    {
        let committed: Vec<String> = tags
            .iter()
            .filter_map(|c| {
                if c.status == ChipStatus::Committed {
                    Some(c.value.clone())
                } else {
                    None
                }
            })
            .collect();
        Some(CreateEntityDraft {
            kind: *kind,
            name: name.clone(),
            body: body.textarea.lines().to_vec(),
            tags: committed,
            parent_id: *parent_id,
            topology_id: *topology_id,
            provider: *provider,
            model: model.clone(),
            effort: effort.clone(),
            sandbox: *sandbox,
        })
    } else {
        None
    }
}

fn creation_working_dir(
    app: &App,
    parent_id: Option<uuid::Uuid>,
) -> Result<std::path::PathBuf, String> {
    if let Some(project_id) = app.current_project_id {
        let project = app
            .projects
            .iter()
            .find(|project| project.id == project_id)
            .ok_or_else(|| format!("Current project {project_id} is not loaded"))?;
        return project.path.clone().ok_or_else(|| {
            format!(
                "Project '{}' has no working directory configured",
                project.name
            )
        });
    }

    if let Some(parent_id) = parent_id
        && let Some(parent) = app.sessions.get(&parent_id)
    {
        return Ok(parent.session.working_dir.clone());
    }

    if let Some(selected) = app.selected_session_state() {
        return Ok(selected.session.working_dir.clone());
    }

    std::env::current_dir().map_err(|e| format!("Could not resolve current directory: {e}"))
}

fn creation_project_id(app: &App, parent_id: Option<uuid::Uuid>) -> Option<uuid::Uuid> {
    parent_id
        .and_then(|parent_id| app.sessions.get(&parent_id))
        .and_then(|parent| parent.session.project_id)
        .or(app.current_project_id)
}

/// Build the launch options for a create-entity-modal leaf submission.
///
/// The pure mirror of [`crate::app::App::quick_new_launch_options`]: both feed
/// the single App-owned response-bearing launch dispatch, so the
/// two session-creation entry points produce identical sessions for equivalent
/// inputs (ST-NEWSESSION-UNIFY / #7). Unset form overrides
/// (`form_provider`/`form_model`/`form_effort`) fall back to the app's
/// `selected_*` defaults — the exact defaults the input bar uses.
#[allow(clippy::too_many_arguments)]
fn leaf_launch_options(
    app: &App,
    kind: SessionKind,
    name: &str,
    body: &str,
    parent_id: Option<uuid::Uuid>,
    working_dir: std::path::PathBuf,
    form_provider: Option<rsi_common::types::SessionProvider>,
    form_model: Option<String>,
    form_effort: Option<String>,
    form_sandbox: bool,
    tags: Vec<String>,
) -> crate::app::LaunchOptions {
    let provider = form_provider.unwrap_or(app.selected_provider);
    // A form provider or explicitly selected model that differs from the app
    // selection establishes a fresh tuple: its cleared model/effort must not
    // fall back to the global tuple. Matching restored defaults, and absent
    // form fields, retain the ordinary global fallback.
    let form_model_overrides_global = form_model
        .as_ref()
        .is_some_and(|model| Some(model) != app.selected_model.as_ref());
    let (model, effort) = if form_provider.is_some_and(|provider| provider != app.selected_provider)
        || form_model_overrides_global
    {
        (form_model, form_effort)
    } else {
        (
            form_model.or_else(|| app.selected_model.clone()),
            form_effort.or_else(|| app.selected_effort.clone()),
        )
    };
    crate::app::LaunchOptions {
        query: body.to_string(),
        title: Some(name.to_string()),
        working_dir: Some(working_dir),
        provider,
        model,
        // The modal has no system-prompt field; preset injection is an
        // input-bar-only concern. Preserved as None (prior modal behavior).
        system_prompt: None,
        session_kind: Some(kind),
        project_id: creation_project_id(app, parent_id),
        max_retries: None,
        effort,
        parent_id,
        sandbox: if form_sandbox {
            Some(rsi_common::types::SandboxSpec {
                kind: Some(rsi_common::types::SandboxKind::GitWorktree),
                branch: None,
            })
        } else {
            None
        },
        tags,
        workflow_id: None,
        // workflow_id_override stays None per ticket §7 (Phase 4 leaf-topology work).
        workflow_id_override: None,
    }
}

/// Submit logic: validate, enforce mandatory tags, race-check containment,
/// dispatch to response-bearing container creation or the App-owned semantic
/// launch task for leaves. A leaf keeps this exact surface and draft until the
/// matching daemon-returned session identity is applied on the main task.
async fn submit_create_entity_form(app: &mut App) {
    // 1. Auto-commit any trailing Pending chip before validation.
    commit_pending_chip(app);

    // 2. Extract canonical fields. P2.3 extends with topology + execution.
    let (
        kind,
        name,
        body_content,
        tags_snapshot,
        parent_id,
        form_topology_id,
        form_provider,
        form_model,
        form_effort,
        form_sandbox,
    ) = match &app.overlay {
        OverlayState::CreateEntityForm {
            kind,
            name,
            body,
            tags,
            parent_id,
            topology_id,
            provider,
            model,
            effort,
            sandbox,
            ..
        } => (
            *kind,
            name.trim().to_string(),
            body.content_for_send(),
            tags.clone(),
            *parent_id,
            *topology_id,
            *provider,
            model.clone(),
            effort.clone(),
            *sandbox,
        ),
        _ => return,
    };

    // 3. Name non-empty.
    if name.is_empty() {
        set_error(app, "Name required");
        return;
    }

    // 4. Reject if any chip is Invalid.
    if tags_snapshot
        .iter()
        .any(|c| matches!(c.status, ChipStatus::Invalid(_)))
    {
        set_error(app, "fix invalid tags before submitting");
        return;
    }

    // 5. Mandatory-tag enforcement (locked decision §B5).
    let committed: Vec<String> = tags_snapshot
        .iter()
        .filter_map(|c| {
            if c.status == ChipStatus::Committed {
                Some(c.value.clone())
            } else {
                None
            }
        })
        .collect();
    if committed.is_empty() {
        set_error(app, "at least one tag required");
        return;
    }

    // 6. Race-condition guard: re-check containment.
    let parent_kind = parent_kind_of(app, parent_id);
    if !legal_children(parent_kind).contains(&kind) {
        let parent_label = parent_kind
            .map(|k| format!("{k:?}"))
            .unwrap_or_else(|| "root".to_string());
        let msg = format!("{kind:?} not allowed under {parent_label}");
        set_error(app, &msg);
        return;
    }

    // 7. Dispatch.
    let project_id = creation_project_id(app, parent_id);
    let working_dir = match creation_working_dir(app, parent_id) {
        Ok(path) => path,
        Err(e) => {
            set_error(app, &e);
            return;
        }
    };

    let result: Result<(), String> = if is_container_kind(kind) {
        // P2.3 §7: topology_id is only meaningful for Epic. Defensive
        // TUI-side gate: Group submissions always pass None (daemon also
        // validates Epic-only). Mirrors P1.6 wire contract.
        let topology = if kind == SessionKind::Epic {
            form_topology_id
        } else {
            None
        };
        app.client
            .create_container(kind, &name, parent_id, project_id, &committed, topology)
            .await
            .map(|_| ())
            .map_err(|e| format!("{e}"))
    } else if is_leaf_kind(kind) {
        // P2.3 §7: thread provider/model/effort/sandbox through the unified
        // launch path. Unset form overrides defer to app.selected_* — the same
        // defaults the input-bar quick-new path uses (ST-NEWSESSION-UNIFY).
        //
        // P2.4: clone() rather than move so the post-success write-back can
        // still see form_model / form_effort / committed.
        let opts = leaf_launch_options(
            app,
            kind,
            &name,
            &body_content,
            parent_id,
            working_dir.clone(),
            form_provider,
            form_model.clone(),
            form_effort.clone(),
            form_sandbox,
            committed.clone(),
        );
        auto_save_draft(app);
        let pending = PendingCreateEntityLeafForm::capture(app)
            .expect("leaf submission still owns its create-entity form");
        if let Err(error) = app.request_create_entity_leaf_launch(opts, pending) {
            set_error(app, &format!("Create failed: {error}"));
        }
        return;
    } else {
        Err(format!("{kind:?} is neither a container nor a leaf kind"))
    };

    match result {
        Ok(()) => {
            // P2.4: capture last-used dropdown values for the next open.
            // Tag tri-state encoding: empty committed-chip list -> None
            // (forces B7 red-border on the next cold-start open). Non-empty
            // -> Some(values) (prefills as Committed chips).
            //
            // Critical ordering: this MUST live inside the Ok(()) arm —
            // a rejected submit must not poison defaults (Reliability).
            let tags_for_defaults = if committed.is_empty() {
                None
            } else {
                Some(committed.clone())
            };
            app.modal_defaults = crate::state::ModalDefaults {
                kind: Some(kind),
                topology_id: form_topology_id,
                tags: tags_for_defaults,
                provider: form_provider.or(Some(app.selected_provider)),
                model: form_model.clone(),
                effort: form_effort.clone(),
                sandbox: form_sandbox,
            };
            crate::state::PersistedState::capture(app).save();

            app.create_entity_draft = None;
            crate::state::DevState::capture(app).save();
            app.overlay = OverlayState::None;
            app.notify_success(format!("Created {kind:?}: {name}"));
            if is_container_kind(kind) {
                app.invalidate_hierarchy_node(parent_id);
            }
            app.mark_dirty();
        }
        Err(e) => {
            set_error(app, &format!("Create failed: {e}"));
        }
    }
}

fn set_error(app: &mut App, msg: &str) {
    if let OverlayState::CreateEntityForm { error, .. } = &mut app.overlay {
        *error = Some(msg.to_string());
    }
    app.mark_dirty();
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    //! Tests for the unified entity-creation form.
    //!
    //! The `test_app()` fixture below mirrors the one previously at
    //! `crates/rsi/src/overlay/container_form.rs:99-103` (deleted in
    //! P2.1 phase 2). P2.2's dispatch-action tests are expected to
    //! reference this fixture; keep the symbol stable.

    use super::*;
    use crate::client::DaemonClient;
    use crate::state::{DevState, PersistedState};
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use std::path::PathBuf;

    #[test]
    fn claude_default_effort_model_matches_first_offered_opus() {
        let default = default_effort_model(rsi_common::types::SessionProvider::Claude);
        let first_opus = rsi_common::claude_catalog::CLAUDE_MODEL_MENU
            .iter()
            .find(|(id, _)| id.starts_with("claude-opus"))
            .unwrap()
            .0;
        assert_eq!(default, "claude-opus-5-5");
        assert_eq!(default, first_opus);
    }

    fn test_app() -> App {
        DevState::clear();
        PersistedState::default().save();
        let mut app = App::new(DaemonClient::new(PathBuf::from("/tmp/test.sock")));
        app.current_project_id = None;
        app.projects.clear();
        app.create_entity_draft = None;
        app.modal_defaults = crate::state::ModalDefaults::default();
        app.selected_provider = rsi_common::types::SessionProvider::Claude;
        app.selected_model = None;
        app.selected_effort = None;
        for tab in &mut app.tabs {
            tab.project_id = None;
        }
        app
    }

    fn make_project(
        id: uuid::Uuid,
        name: &str,
        path: Option<PathBuf>,
    ) -> rsi_common::types::Project {
        rsi_common::types::Project {
            id,
            name: name.to_string(),
            path,
            description: None,
            color: rsi_common::types::Project::DEFAULT_COLOR.to_string(),
            context_files: None,
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
        }
    }

    /// Build a baseline `Session` row of the given Kind. Used by P2.3 tests
    /// that need real session entries in `app.sessions` to act as legal
    /// parent candidates.
    fn make_session(kind: SessionKind) -> rsi_common::types::Session {
        use rsi_common::types::{ContextUsageConfidence, Session, SessionProvider, SessionStatus};
        Session {
            context_fill_pct: None,
            id: uuid::Uuid::new_v4(),
            provider: SessionProvider::Claude,
            claude_session_id: None,
            query: format!("query {:?}", kind),
            title: None,
            agent_role: None,
            epic_spawn_ordinal: None,
            description: None,
            short_summary: None,
            pending_question: None,
            pending_archive: false,
            working_dir: PathBuf::from("/tmp"),
            git_branch: None,
            status: SessionStatus::Running,
            project_id: None,
            session_kind: kind,
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
            cost_usd: None,
            duration_ms: None,
            num_turns: None,
            model: None,
            input_tokens: None,
            output_tokens: None,
            context_window: None,
            resolved_context_budget: None,
            total_input_tokens: None,
            total_output_tokens: None,
            total_cache_creation_tokens: None,
            total_cache_read_tokens: None,
            stop_reason: None,
            context_usage_confidence: ContextUsageConfidence::default(),
            continued_from: None,
            pinned_at: None,
            testing_needed_at: None,
            rotation_disabled_at: None,
            handoff_filepath: None,
            active_task: None,
            group_id: None,
            scheduled_job_id: None,
            pipeline_artifact: None,
            workflow_id: None,
            workflow_id_override: None,
            rotation_depth: 0,
            retry_attempt: None,
            max_retries: None,
            daemon_input_tokens: None,
            daemon_output_tokens: None,
            effort: None,
            issue_identifier: None,
            issue_url: None,
            issue_tracker_id: None,
            rating: None,
            harness_version_hash: None,
            test_passed: None,
            clippy_passed: None,
            turn_count: None,
            retry_count: None,
            approval_wait_ms: None,
            work_time_ms: None,
            approval_started_at: None,
            sandbox_kind: None,
            sandbox_root: None,
            sandbox_branch: None,
            sandbox_cleanup_state: None,
            tag: String::new(),
            tags: Vec::new(),
            parent_id: None,
            lead_session_id: None,
            is_eval: false,
            capability_class: None,
            topology_node_id: None,
            topology_iteration: 0,
            provider_cli_version: None,
            provider_capabilities: Vec::new(),
            thinking_tokens: None,
            service_tier: None,
            cache_creation_1h_tokens: None,
            cache_creation_5m_tokens: None,
            permission_denial_count: None,
            subagent_stats_json: None,
            queued_turn_count: None,
            terminal_reason: None,
        }
    }

    /// ST-NEWSESSION-UNIFY parity gate: the input-bar quick-new path and the
    /// create-entity modal must build an identical `LaunchOptions` for the same
    /// inputs, so both entry points create identical sessions through the one
    /// App-owned response-bearing launch dispatch. Compared via serialized
    /// value (field-order-independent; covers every field, sandbox included).
    #[test]
    fn input_bar_and_modal_build_identical_launch_options() {
        let mut app = test_app();
        // Shared, user-controllable inputs both entry points resolve.
        app.selected_provider = rsi_common::types::SessionProvider::Codex;
        app.selected_model = Some("gpt-5-codex".to_string());
        app.selected_effort = Some("high".to_string());
        let project_id = uuid::Uuid::new_v4();
        app.current_project_id = Some(project_id);

        let working_dir = PathBuf::from("/tmp/parity");
        let query = "ship the parity slice";

        // Input bar: query + app defaults, Standard leaf, placeholder tag.
        let input_opts = app.quick_new_launch_options(query, Some(working_dir.clone()), None);

        // Modal: same Standard leaf, no form overrides (defer to selected_*),
        // same working_dir, same single tag, no parent.
        let mut modal_opts = leaf_launch_options(
            &app,
            SessionKind::Standard,
            query,
            query,
            None,
            working_dir.clone(),
            None, // form_provider -> selected_provider
            None, // form_model    -> selected_model
            None, // form_effort   -> selected_effort
            false,
            vec![crate::types::PHASE1_PLACEHOLDER_TAG.to_string()],
        );
        modal_opts.title = None;

        assert_eq!(
            serde_json::to_value(&input_opts).unwrap(),
            serde_json::to_value(&modal_opts).unwrap(),
            "input-bar and modal must produce identical launch options for equivalent inputs"
        );
    }

    /// Guards the one structural divergence that is intentional: the modal sets
    /// an explicit kind while the input bar is always Standard. A modal launch
    /// of a *non*-Standard kind differs from quick-new only in `session_kind` —
    /// every other launch field still matches.
    #[test]
    fn modal_kind_is_the_only_intended_divergence_from_quick_new() {
        let mut app = test_app();
        app.selected_provider = rsi_common::types::SessionProvider::Claude;
        let working_dir = PathBuf::from("/tmp/parity");

        let input_opts = app.quick_new_launch_options("x", Some(working_dir.clone()), None);
        let modal_task = leaf_launch_options(
            &app,
            SessionKind::Task,
            "x",
            "x",
            None,
            working_dir,
            None,
            None,
            None,
            false,
            vec![crate::types::PHASE1_PLACEHOLDER_TAG.to_string()],
        );

        assert_eq!(input_opts.session_kind, Some(SessionKind::Standard));
        assert_eq!(modal_task.session_kind, Some(SessionKind::Task));
        assert_ne!(input_opts.session_kind, modal_task.session_kind);
        // Everything a user does not pick still resolves identically.
        assert_eq!(input_opts.provider, modal_task.provider);
        assert_eq!(input_opts.working_dir, modal_task.working_dir);
        assert_eq!(input_opts.tags, modal_task.tags);
        assert_eq!(input_opts.project_id, modal_task.project_id);
    }

    #[test]
    fn creation_working_dir_prefers_current_project_path() {
        let mut app = test_app();
        let project_id = uuid::Uuid::new_v4();
        let project_dir = PathBuf::from("/tmp/acme");
        app.projects.push(make_project(
            project_id,
            "Acme",
            Some(project_dir.clone()),
        ));
        app.current_project_id = Some(project_id);

        assert_eq!(
            creation_working_dir(&app, None).unwrap(),
            project_dir,
            "project-scoped entity creation must use the project working_dir"
        );
    }

    #[test]
    fn creation_working_dir_rejects_project_without_path() {
        let mut app = test_app();
        let project_id = uuid::Uuid::new_v4();
        app.projects.push(make_project(project_id, "NoPath", None));
        app.current_project_id = Some(project_id);

        let error = creation_working_dir(&app, None).unwrap_err();
        assert!(error.contains("NoPath"));
        assert!(error.contains("no working directory"));
    }

    #[test]
    fn creation_working_dir_inherits_parent_without_project_filter() {
        let mut app = test_app();
        app.current_project_id = None;
        let parent_id = uuid::Uuid::new_v4();
        let mut parent = make_session(SessionKind::Epic);
        parent.id = parent_id;
        parent.working_dir = PathBuf::from("/tmp/parent-project");
        app.sessions
            .insert(parent_id, crate::types::SessionState::new(parent));

        assert_eq!(
            creation_working_dir(&app, Some(parent_id)).unwrap(),
            PathBuf::from("/tmp/parent-project")
        );
    }

    #[test]
    fn creation_project_id_inherits_parent_without_project_filter() {
        let mut app = test_app();
        let project_id = uuid::Uuid::new_v4();
        let mut parent = make_session(SessionKind::Epic);
        let parent_id = parent.id;
        parent.project_id = Some(project_id);
        app.sessions
            .insert(parent_id, crate::types::SessionState::new(parent));

        assert_eq!(creation_project_id(&app, Some(parent_id)), Some(project_id));
    }

    // === Re-homed from container_form.rs (deleted in P2.1 phase 2) ===

    #[tokio::test]
    async fn open_create_entity_form_sets_overlay_state() {
        let mut app = test_app();
        open_create_entity_form(&mut app, SessionKind::Group, None).await;
        match &app.overlay {
            OverlayState::CreateEntityForm {
                kind,
                name,
                tags,
                parent_id,
                ..
            } => {
                assert_eq!(*kind, SessionKind::Group);
                assert!(name.is_empty());
                assert!(tags.is_empty());
                assert!(parent_id.is_none());
            }
            _ => panic!("Expected CreateEntityForm overlay"),
        }
    }

    #[tokio::test]
    async fn open_create_entity_form_with_parent_id() {
        let mut app = test_app();
        let pid = uuid::Uuid::new_v4();
        open_create_entity_form(&mut app, SessionKind::Epic, Some(pid)).await;
        match &app.overlay {
            OverlayState::CreateEntityForm {
                kind, parent_id, ..
            } => {
                assert_eq!(*kind, SessionKind::Epic);
                assert_eq!(*parent_id, Some(pid));
            }
            _ => panic!("Expected CreateEntityForm overlay"),
        }
    }

    // === New tests per ticket §Acceptance criteria ===

    #[tokio::test]
    async fn containment_rejection_sets_error_banner() {
        // Epic + parent=None → legal_children(None) returns [Standard, Group].
        // Epic is NOT legal under root. The overlay still opens but with
        // error = Some("Epic not allowed under root") and focused_field=Kind.
        let mut app = test_app();
        open_create_entity_form(&mut app, SessionKind::Epic, None).await;
        match &app.overlay {
            OverlayState::CreateEntityForm {
                error,
                focused_field,
                ..
            } => {
                assert!(
                    error.is_some(),
                    "expected error banner for illegal Epic-under-root"
                );
                assert_eq!(*focused_field, CreateEntityField::Kind);
            }
            _ => panic!("Expected CreateEntityForm overlay"),
        }
    }

    #[test]
    fn tag_chip_commit_valid_input_produces_committed_status() {
        let raw = "foo-bar";
        let result = normalize_tag(raw);
        assert_eq!(result.unwrap(), "foo-bar");
    }

    #[test]
    fn tag_chip_commit_invalid_input_produces_error() {
        let raw = "Foo Bar!";
        let result = normalize_tag(raw);
        assert!(result.is_err(), "Foo Bar! contains invalid chars");
    }

    #[tokio::test]
    async fn esc_with_draft_preserves_name_and_tags_in_devstate() {
        // Simulate: open overlay, mutate name + commit a tag, call
        // auto_save_draft, then verify app.create_entity_draft.
        let mut app = test_app();
        open_create_entity_form(&mut app, SessionKind::Group, None).await;
        if let OverlayState::CreateEntityForm { name, tags, .. } = &mut app.overlay {
            name.push_str("hello");
            tags.push(TagChip {
                value: "foo-bar".to_string(),
                status: ChipStatus::Committed,
            });
        }
        auto_save_draft(&mut app);
        let draft = app.create_entity_draft.as_ref().expect("draft saved");
        assert_eq!(draft.name, "hello");
        assert_eq!(draft.tags, vec!["foo-bar".to_string()]);
        assert_eq!(draft.kind, SessionKind::Group);
    }

    #[tokio::test]
    async fn ctrl_d_clears_draft_and_closes_overlay() {
        let mut app = test_app();
        app.create_entity_draft = Some(CreateEntityDraft {
            kind: SessionKind::Group,
            name: "x".into(),
            body: Vec::new(),
            tags: vec!["t".into()],
            parent_id: None,
            topology_id: None,
            provider: None,
            model: None,
            effort: None,
            sandbox: false,
        });
        open_create_entity_form(&mut app, SessionKind::Group, None).await;
        let key = KeyEvent::new(KeyCode::Char('d'), KeyModifiers::CONTROL);
        handle_create_entity_form_key(&mut app, key).await;
        assert!(app.create_entity_draft.is_none(), "draft must be cleared");
        assert!(
            matches!(app.overlay, OverlayState::None),
            "overlay must be closed"
        );
    }

    #[tokio::test]
    async fn tag_chip_state_machine_pending_to_committed_then_backspace_delete() {
        // Drive the chip state machine through commit_pending_chip directly.
        let mut app = test_app();
        open_create_entity_form(&mut app, SessionKind::Group, None).await;
        if let OverlayState::CreateEntityForm { tags, .. } = &mut app.overlay {
            tags.push(TagChip {
                value: "foo".into(),
                status: ChipStatus::Pending,
            });
        }
        commit_pending_chip(&mut app);
        match &app.overlay {
            OverlayState::CreateEntityForm { tags, .. } => {
                assert_eq!(tags.len(), 1);
                assert_eq!(tags[0].status, ChipStatus::Committed);
                assert_eq!(tags[0].value, "foo");
            }
            _ => panic!("overlay shape lost"),
        }
        // Pending+Backspace-on-empty simulation: drop the rightmost
        // committed chip.
        if let OverlayState::CreateEntityForm { tags, .. } = &mut app.overlay {
            tags.pop();
        }
        match &app.overlay {
            OverlayState::CreateEntityForm { tags, .. } => assert!(tags.is_empty()),
            _ => panic!("overlay shape lost"),
        }
    }

    #[tokio::test]
    async fn tag_chip_invalid_input_flips_to_invalid_status() {
        let mut app = test_app();
        open_create_entity_form(&mut app, SessionKind::Group, None).await;
        if let OverlayState::CreateEntityForm { tags, .. } = &mut app.overlay {
            tags.push(TagChip {
                value: "Foo Bar!".into(),
                status: ChipStatus::Pending,
            });
        }
        commit_pending_chip(&mut app);
        match &app.overlay {
            OverlayState::CreateEntityForm { tags, .. } => {
                assert_eq!(tags.len(), 1);
                assert!(matches!(tags[0].status, ChipStatus::Invalid(_)));
            }
            _ => panic!("overlay shape lost"),
        }
    }

    // ═══════════════════════════════════════════════════════════════════════
    // P2.3 Phase 1 — visibility state machine + Kind-toggle focus-jump
    // ═══════════════════════════════════════════════════════════════════════

    #[test]
    fn field_visibility_group() {
        let list = visibility::field_visibility(SessionKind::Group);
        assert_eq!(
            list,
            &[
                CreateEntityField::Kind,
                CreateEntityField::Name,
                CreateEntityField::Tag,
                CreateEntityField::Parent,
            ]
        );
    }

    #[test]
    fn field_visibility_epic_includes_topology() {
        let list = visibility::field_visibility(SessionKind::Epic);
        assert!(list.contains(&CreateEntityField::Topology));
        assert!(!list.contains(&CreateEntityField::Provider));
        assert!(!list.contains(&CreateEntityField::Sandbox));
    }

    #[test]
    fn field_visibility_leaf_includes_execution_group() {
        for k in [
            SessionKind::Task,
            SessionKind::Story,
            SessionKind::Bug,
            SessionKind::Feature,
            SessionKind::Refactor,
            SessionKind::Research,
            SessionKind::Standard,
        ] {
            let list = visibility::field_visibility(k);
            assert!(list.contains(&CreateEntityField::Provider), "{k:?}");
            assert!(list.contains(&CreateEntityField::Model), "{k:?}");
            assert!(list.contains(&CreateEntityField::Effort), "{k:?}");
            assert!(list.contains(&CreateEntityField::Sandbox), "{k:?}");
            assert!(!list.contains(&CreateEntityField::Topology), "{k:?}");
        }
    }

    #[tokio::test]
    async fn kind_toggle_moves_focus_when_hiding_active_field() {
        // Open a Task form (leaf), focus Provider, then cycle Kind to a
        // container — focus must jump to a legal field in the new list.
        let mut app = test_app();
        // Open with Standard (legal under root) so we can cycle into Group.
        open_create_entity_form(&mut app, SessionKind::Standard, None).await;
        if let OverlayState::CreateEntityForm { focused_field, .. } = &mut app.overlay {
            *focused_field = CreateEntityField::Provider;
        }
        // Cycle kind forward — under root, allowed kinds are [Standard, Group].
        // Standard → Group → focus should jump (Provider not visible in Group).
        cycle_kind_segment(&mut app, true);
        match &app.overlay {
            OverlayState::CreateEntityForm {
                kind,
                focused_field,
                ..
            } => {
                assert_eq!(*kind, SessionKind::Group);
                assert!(
                    visibility::field_visibility(*kind).contains(focused_field),
                    "focused field {:?} must be in visibility list for {:?}",
                    focused_field,
                    kind
                );
            }
            _ => panic!("overlay shape lost"),
        }
    }

    #[test]
    fn create_entity_draft_round_trip_serde() {
        // CreateEntityDraft round-trips all fields via serde_json, including
        // a multi-line multibyte body (S1; multibyte fixture per S0 lesson).
        let draft = CreateEntityDraft {
            kind: SessionKind::Task,
            name: "task-1".to_string(),
            body: vec!["para1".to_string(), String::new(), "para2 é✨".to_string()],
            tags: vec!["alpha".to_string(), "beta".to_string()],
            parent_id: Some(uuid::Uuid::new_v4()),
            topology_id: Some(uuid::Uuid::new_v4()),
            provider: Some(rsi_common::types::SessionProvider::Codex),
            model: Some("gpt-5".to_string()),
            effort: Some("high".to_string()),
            sandbox: true,
        };
        let json = serde_json::to_string(&draft).expect("serialize");
        let back: CreateEntityDraft = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(draft, back);
    }

    // ═══════════════════════════════════════════════════════════════════════
    // P2.3 Phase 2 — Topology dropdown (Pattern 2 inline filter)
    // ═══════════════════════════════════════════════════════════════════════

    fn fake_topology(name: &str) -> rsi_common::types::Topology {
        rsi_common::types::Topology {
            id: uuid::Uuid::new_v4(),
            name: name.to_string(),
            definition: rsi_common::types::TopologyDefinition {
                nodes: Vec::new(),
                edges: Vec::new(),
                until: None,
            },
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
        }
    }

    #[tokio::test]
    async fn topology_dropdown_filter_substring_match() {
        // filter "alp" over ["alpha", "alpine", "beta"] yields 2 visible rows.
        let choices = vec![
            fake_topology("alpha"),
            fake_topology("alpine"),
            fake_topology("beta"),
        ];
        let visible = topology_filtered_indices(&choices, "alp");
        assert_eq!(visible.len(), 2);
        assert!(visible.contains(&0));
        assert!(visible.contains(&1));
        // Case-insensitive
        let visible = topology_filtered_indices(&choices, "ALP");
        assert_eq!(visible.len(), 2);
        // Empty filter — all visible.
        let visible = topology_filtered_indices(&choices, "");
        assert_eq!(visible.len(), 3);
    }

    #[tokio::test]
    async fn topology_dropdown_enter_commits_id() {
        let mut app = test_app();
        // Open form as Epic under a Group parent (Epic legal under Group).
        let group_id = uuid::Uuid::new_v4();
        let mut group = make_session(SessionKind::Group);
        group.id = group_id;
        app.sessions
            .insert(group_id, crate::types::SessionState::new(group));
        open_create_entity_form(&mut app, SessionKind::Epic, Some(group_id)).await;

        let topology_a = fake_topology("alpha");
        let alpha_id = topology_a.id;
        let topology_b = fake_topology("beta");
        if let OverlayState::CreateEntityForm {
            topology_choices,
            focused_field,
            ..
        } = &mut app.overlay
        {
            *topology_choices = vec![topology_a, topology_b];
            *focused_field = CreateEntityField::Topology;
        }
        // Highlight index 0 (alpha) then Enter.
        let key = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
        handle_create_entity_form_key(&mut app, key).await;
        match &app.overlay {
            OverlayState::CreateEntityForm {
                topology_id,
                topology_filter,
                ..
            } => {
                assert_eq!(*topology_id, Some(alpha_id));
                assert!(topology_filter.is_empty());
            }
            _ => panic!("overlay shape lost"),
        }
    }

    #[tokio::test]
    async fn topology_dropdown_esc_clears_id() {
        let mut app = test_app();
        let group_id = uuid::Uuid::new_v4();
        let mut group = make_session(SessionKind::Group);
        group.id = group_id;
        app.sessions
            .insert(group_id, crate::types::SessionState::new(group));
        open_create_entity_form(&mut app, SessionKind::Epic, Some(group_id)).await;

        let topology_a = fake_topology("alpha");
        let alpha_id = topology_a.id;
        if let OverlayState::CreateEntityForm {
            topology_id,
            topology_choices,
            focused_field,
            ..
        } = &mut app.overlay
        {
            *topology_id = Some(alpha_id);
            *topology_choices = vec![topology_a];
            *focused_field = CreateEntityField::Topology;
        }
        // Esc with topology focused clears topology_id and returns focus to Kind.
        let key = KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE);
        handle_create_entity_form_key(&mut app, key).await;
        match &app.overlay {
            OverlayState::CreateEntityForm {
                topology_id,
                focused_field,
                ..
            } => {
                assert!(topology_id.is_none());
                assert_eq!(*focused_field, CreateEntityField::Kind);
            }
            _ => panic!("expected form still open after topology Esc"),
        }
    }

    #[tokio::test]
    async fn topology_hidden_for_non_epic() {
        // Kind=Group → visibility excludes Topology; `t` is a no-op.
        let mut app = test_app();
        open_create_entity_form(&mut app, SessionKind::Group, None).await;
        let initial_focus = match &app.overlay {
            OverlayState::CreateEntityForm { focused_field, .. } => *focused_field,
            _ => panic!("overlay shape lost"),
        };
        let key = KeyEvent::new(KeyCode::Char('t'), KeyModifiers::NONE);
        handle_create_entity_form_key(&mut app, key).await;
        match &app.overlay {
            OverlayState::CreateEntityForm { focused_field, .. } => {
                assert_eq!(
                    *focused_field, initial_focus,
                    "t must be no-op when Topology is hidden"
                );
            }
            _ => panic!("overlay shape lost"),
        }
    }

    // ═══════════════════════════════════════════════════════════════════════
    // P2.3 Phase 4 — Execution fields (Provider / Model / Effort / Sandbox)
    // ═══════════════════════════════════════════════════════════════════════

    #[tokio::test]
    async fn execution_fields_visible_for_leaves() {
        // Kind = Task (leaf) → visibility list contains Provider/Model/Effort/Sandbox.
        let list = visibility::field_visibility(SessionKind::Task);
        for f in [
            CreateEntityField::Provider,
            CreateEntityField::Model,
            CreateEntityField::Effort,
            CreateEntityField::Sandbox,
        ] {
            assert!(list.contains(&f), "Task must include {f:?}");
        }
    }

    #[tokio::test]
    async fn execution_fields_hidden_for_containers() {
        // Kind = Group → none of the execution fields are visible; p/m/e/s no-ops.
        let mut app = test_app();
        open_create_entity_form(&mut app, SessionKind::Group, None).await;
        if let OverlayState::CreateEntityForm { insert_mode, .. } = &mut app.overlay {
            *insert_mode = false;
        }
        let initial_focus = match &app.overlay {
            OverlayState::CreateEntityForm { focused_field, .. } => *focused_field,
            _ => panic!("overlay shape lost"),
        };
        for ch in ['p', 'm', 'e', 's'] {
            let key = KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE);
            handle_create_entity_form_key(&mut app, key).await;
        }
        match &app.overlay {
            OverlayState::CreateEntityForm { focused_field, .. } => {
                assert_eq!(
                    *focused_field, initial_focus,
                    "execution-field hotkeys must be no-op when fields hidden"
                );
            }
            _ => panic!("overlay shape lost"),
        }
    }

    /// Open a leaf-Task form, then immediately drop insert_mode so the
    /// test can dispatch normal-mode chords directly. Returns the
    /// (mut) app for chaining.
    async fn open_task_in_normal_mode(app: &mut App, epic_id: uuid::Uuid) {
        let mut epic = make_session(SessionKind::Epic);
        epic.id = epic_id;
        app.sessions
            .insert(epic_id, crate::types::SessionState::new(epic));
        open_create_entity_form(app, SessionKind::Task, Some(epic_id)).await;
        if let OverlayState::CreateEntityForm { insert_mode, .. } = &mut app.overlay {
            *insert_mode = false;
        }
    }

    #[tokio::test]
    async fn provider_cycle_forward_and_back() {
        // For an Epic, Provider is hidden — use a Task (leaf) under an Epic.
        let mut app = test_app();
        let epic_id = uuid::Uuid::new_v4();
        open_task_in_normal_mode(&mut app, epic_id).await;

        // Forward: None → Claude → Codex → Pioneer → OpenRouter → Bedrock → … → Harness → Claude (wrap).
        for expected in [
            rsi_common::types::SessionProvider::Claude,
            rsi_common::types::SessionProvider::Codex,
            rsi_common::types::SessionProvider::Pioneer,
            rsi_common::types::SessionProvider::OpenRouter,
            rsi_common::types::SessionProvider::Bedrock,
            rsi_common::types::SessionProvider::Local,
            rsi_common::types::SessionProvider::Antigravity,
            rsi_common::types::SessionProvider::Harness,
            rsi_common::types::SessionProvider::Claude,
        ] {
            let key = KeyEvent::new(KeyCode::Char('p'), KeyModifiers::CONTROL);
            handle_create_entity_form_key(&mut app, key).await;
            match &app.overlay {
                OverlayState::CreateEntityForm { provider, .. } => {
                    assert_eq!(*provider, Some(expected));
                }
                _ => panic!("overlay shape lost"),
            }
        }
        // Backward (Ctrl+Shift+p sends KeyCode::Char('P')).
        let key = KeyEvent::new(
            KeyCode::Char('P'),
            KeyModifiers::CONTROL | KeyModifiers::SHIFT,
        );
        handle_create_entity_form_key(&mut app, key).await;
        match &app.overlay {
            OverlayState::CreateEntityForm { provider, .. } => {
                assert_eq!(*provider, Some(rsi_common::types::SessionProvider::Harness));
            }
            _ => panic!("overlay shape lost"),
        }
    }

    #[tokio::test]
    async fn story_model_picker_includes_astra() {
        let mut app = test_app();
        app.selected_provider = rsi_common::types::SessionProvider::Codex;
        let epic_id = uuid::Uuid::new_v4();
        let mut epic = make_session(SessionKind::Epic);
        epic.id = epic_id;
        app.sessions
            .insert(epic_id, crate::types::SessionState::new(epic));

        open_create_entity_form(&mut app, SessionKind::Story, Some(epic_id)).await;
        if let OverlayState::CreateEntityForm { insert_mode, .. } = &mut app.overlay {
            *insert_mode = false;
        }
        handle_create_entity_form_key(
            &mut app,
            KeyEvent::new(KeyCode::Char('m'), KeyModifiers::NONE),
        )
        .await;

        match &app.overlay {
            OverlayState::CreateEntityForm {
                kind,
                model_dropdown: Some(dropdown),
                ..
            } => {
                assert_eq!(*kind, SessionKind::Story);
                assert_eq!(dropdown.provider, rsi_common::types::SessionProvider::Codex);
                // The catalog order puts newer GPT-6 models first (0be6bf37e);
                // this test pins that Astra stays offered, not its rank.
                assert!(
                    dropdown
                        .models
                        .iter()
                        .any(|(id, name)| id == "gpt-6-astra" && name == "GPT-6-Astra"),
                    "Story model picker offers GPT-6-Astra: {:?}",
                    dropdown.models
                );
            }
            _ => panic!("Story model picker did not open"),
        }
    }

    /// Regression for the reported bug: the Story (and Group/Epic/Task/Bug)
    /// create-entity form's model picker showed only Pioneer's minimal
    /// static fallback (`PIONEER_MODELS`), never the live discovered
    /// catalog that the global/new-session modal shows, because opening the
    /// dropdown never requested a discovery refresh. Asserts the half of
    /// the fix owned by this module: opening the dropdown for a provider
    /// with a thin static fallback schedules `needs_model_refresh` for that
    /// exact provider. The other half — `event.rs`'s MODEL DISCOVERY RESULT
    /// arm syncing `model_dropdown.models` back once the result lands — is
    /// exercised by that arm's existing sibling syncs (global/Prompt/
    /// settings dropdowns) and is not independently unit-testable here
    /// since it lives inside the `tokio::select!` event loop.
    #[tokio::test]
    async fn opening_story_model_picker_for_pioneer_requests_a_discovery_refresh() {
        let mut app = test_app();
        app.selected_provider = rsi_common::types::SessionProvider::Pioneer;
        app.needs_model_refresh = false;
        app.model_refresh_provider = None;
        let epic_id = uuid::Uuid::new_v4();
        let mut epic = make_session(SessionKind::Epic);
        epic.id = epic_id;
        app.sessions
            .insert(epic_id, crate::types::SessionState::new(epic));

        open_create_entity_form(&mut app, SessionKind::Story, Some(epic_id)).await;
        if let OverlayState::CreateEntityForm { insert_mode, .. } = &mut app.overlay {
            *insert_mode = false;
        }
        handle_create_entity_form_key(
            &mut app,
            KeyEvent::new(KeyCode::Char('m'), KeyModifiers::NONE),
        )
        .await;

        match &app.overlay {
            OverlayState::CreateEntityForm {
                model_dropdown: Some(dropdown),
                ..
            } => {
                // Static fallback still seeds the dropdown immediately...
                assert_eq!(
                    dropdown.provider,
                    rsi_common::types::SessionProvider::Pioneer
                );
                assert_eq!(
                    dropdown.models,
                    crate::app::models_for_provider(rsi_common::types::SessionProvider::Pioneer)
                );
            }
            _ => panic!("Story model picker did not open"),
        }
        // ...but a refresh was also requested so event.rs replaces it with
        // the live catalog once discovery responds.
        assert!(app.needs_model_refresh);
        assert_eq!(
            app.model_refresh_provider,
            Some(rsi_common::types::SessionProvider::Pioneer)
        );
    }

    #[tokio::test]
    async fn provider_cycle_keeps_empty_provider_tuple_effortless_on_submission() {
        let mut app = test_app();
        let epic_id = uuid::Uuid::new_v4();
        open_task_in_normal_mode(&mut app, epic_id).await;
        app.selected_provider = rsi_common::types::SessionProvider::Codex;
        app.selected_model = Some("gpt-6-astra".to_string());
        app.selected_effort = Some("ultra".to_string());

        if let OverlayState::CreateEntityForm {
            provider,
            model,
            effort,
            ..
        } = &mut app.overlay
        {
            *provider = Some(rsi_common::types::SessionProvider::Codex);
            *model = Some("gpt-6-astra".to_string());
            *effort = Some("ultra".to_string());
        }

        // Codex -> Pioneer -> OpenRouter -> Bedrock -> Local.
        cycle_provider(&mut app, true);
        cycle_provider(&mut app, true);
        cycle_provider(&mut app, true);
        cycle_provider(&mut app, true);
        handle_create_entity_form_key(
            &mut app,
            KeyEvent::new(KeyCode::Char('e'), KeyModifiers::NONE),
        )
        .await;
        let (form_provider, form_model, form_effort) = match &app.overlay {
            OverlayState::CreateEntityForm {
                provider,
                model,
                effort,
                ..
            } => {
                assert_eq!(*provider, Some(rsi_common::types::SessionProvider::Local));
                assert_eq!(model, &None);
                assert_eq!(effort, &None);
                (*provider, model.clone(), effort.clone())
            }
            _ => panic!("overlay shape lost"),
        };
        let options = leaf_launch_options(
            &app,
            SessionKind::Task,
            "task",
            "body",
            Some(epic_id),
            PathBuf::from("/tmp/provider-cycle"),
            form_provider,
            form_model,
            form_effort,
            false,
            vec![crate::types::PHASE1_PLACEHOLDER_TAG.to_string()],
        );
        assert_eq!(options.provider, rsi_common::types::SessionProvider::Local);
        assert_eq!(options.model, None);
        assert_eq!(options.effort, None);

        cycle_provider(&mut app, true);
        cycle_provider(&mut app, true);
        assert!(
            crate::app::models_for_provider(rsi_common::types::SessionProvider::Harness).is_empty()
        );
        match &app.overlay {
            OverlayState::CreateEntityForm {
                provider,
                model,
                effort,
                ..
            } => {
                assert_eq!(*provider, Some(rsi_common::types::SessionProvider::Harness));
                assert_eq!(model, &None);
                assert_eq!(effort, &None);
            }
            _ => panic!("overlay shape lost"),
        }
    }

    #[tokio::test]
    async fn effort_cycle_with_none() {
        let mut app = test_app();
        let epic_id = uuid::Uuid::new_v4();
        open_task_in_normal_mode(&mut app, epic_id).await;

        // The form's explicit model is the effective submission model even
        // when provider falls back to the app default.
        if let OverlayState::CreateEntityForm {
            provider, model, ..
        } = &mut app.overlay
        {
            *provider = None;
            *model = Some("claude-opus-4-6".to_string());
        }
        for expected in [
            Some("low".to_string()),
            Some("medium".to_string()),
            Some("high".to_string()),
            Some("max".to_string()),
            None,
        ] {
            let key = KeyEvent::new(KeyCode::Char('e'), KeyModifiers::NONE);
            handle_create_entity_form_key(&mut app, key).await;
            match &app.overlay {
                OverlayState::CreateEntityForm { effort, .. } => {
                    assert_eq!(*effort, expected);
                }
                _ => panic!("overlay shape lost"),
            }
        }
    }

    #[tokio::test]
    async fn opus_5_effort_cycle_preserves_forward_and_backward_none_boundaries() {
        let mut app = test_app();
        let epic_id = uuid::Uuid::new_v4();
        open_task_in_normal_mode(&mut app, epic_id).await;
        if let OverlayState::CreateEntityForm {
            provider, model, ..
        } = &mut app.overlay
        {
            *provider = Some(rsi_common::types::SessionProvider::Claude);
            *model = Some("claude-opus-5".to_string());
        }

        for expected in ["low", "medium", "high", "xhigh", "max"] {
            cycle_effort(&mut app, true);
            match &app.overlay {
                OverlayState::CreateEntityForm { effort, .. } => {
                    assert_eq!(effort.as_deref(), Some(expected));
                }
                _ => panic!("overlay shape lost"),
            }
        }
        cycle_effort(&mut app, true);
        match &app.overlay {
            OverlayState::CreateEntityForm { effort, .. } => assert!(effort.is_none()),
            _ => panic!("overlay shape lost"),
        }

        for expected in ["max", "xhigh", "high", "medium", "low"] {
            cycle_effort(&mut app, false);
            match &app.overlay {
                OverlayState::CreateEntityForm { effort, .. } => {
                    assert_eq!(effort.as_deref(), Some(expected));
                }
                _ => panic!("overlay shape lost"),
            }
        }
        cycle_effort(&mut app, false);
        match &app.overlay {
            OverlayState::CreateEntityForm { effort, .. } => assert!(effort.is_none()),
            _ => panic!("overlay shape lost"),
        }
    }

    /// Regression: selecting a Claude model whose effort ladder is empty
    /// must NOT disable effort cycling. It should fall back to the Claude
    /// baseline (the current Opus 5 ladder) rather than silently doing
    /// nothing on `e`.
    #[tokio::test]
    async fn effort_cycle_falls_back_for_zero_effort_model() {
        let mut app = test_app();
        let epic_id = uuid::Uuid::new_v4();
        open_task_in_normal_mode(&mut app, epic_id).await;

        // Set form to Claude provider + a pre-4.6 model (effort_level_count = 0).
        if let OverlayState::CreateEntityForm {
            provider, model, ..
        } = &mut app.overlay
        {
            *provider = Some(rsi_common::types::SessionProvider::Claude);
            *model = Some("claude-sonnet-4-5".to_string());
        }

        // Despite the model reporting 0 effort levels, `e` must cycle through
        // the full current Claude baseline.
        for expected in [
            Some("low".to_string()),
            Some("medium".to_string()),
            Some("high".to_string()),
            Some("xhigh".to_string()),
            Some("max".to_string()),
            None,
        ] {
            let key = KeyEvent::new(KeyCode::Char('e'), KeyModifiers::NONE);
            handle_create_entity_form_key(&mut app, key).await;
            match &app.overlay {
                OverlayState::CreateEntityForm { effort, .. } => {
                    assert_eq!(
                        *effort, expected,
                        "effort should cycle even for 0-level model"
                    );
                }
                _ => panic!("overlay shape lost"),
            }
        }

        // Also verify haiku (also 0 levels) gets the same fallback.
        if let OverlayState::CreateEntityForm { model, .. } = &mut app.overlay {
            *model = Some("claude-haiku-4-5-20251001".to_string());
        }
        let key = KeyEvent::new(KeyCode::Char('e'), KeyModifiers::NONE);
        handle_create_entity_form_key(&mut app, key).await;
        match &app.overlay {
            OverlayState::CreateEntityForm { effort, .. } => {
                assert_eq!(
                    *effort,
                    Some("low".to_string()),
                    "haiku should fall back to baseline, not stay stuck at none"
                );
            }
            _ => panic!("overlay shape lost"),
        }
    }

    #[tokio::test]
    async fn codex_effort_selection_uses_effective_submission_model() {
        let mut app = test_app();
        let epic_id = uuid::Uuid::new_v4();
        open_task_in_normal_mode(&mut app, epic_id).await;
        let ultra_ladder = &["low", "medium", "high", "xhigh", "max", "ultra"];
        assert_eq!(
            effort_ladder_for_selection(
                Some(rsi_common::types::SessionProvider::Codex),
                None,
                rsi_common::types::SessionProvider::Codex,
                Some("gpt-6-astra")
            ),
            ultra_ladder
        );
        assert_eq!(
            effort_ladder_for_selection(
                Some(rsi_common::types::SessionProvider::Codex),
                Some("gpt-6-astra"),
                rsi_common::types::SessionProvider::Codex,
                Some("gpt-6-astra")
            ),
            ultra_ladder
        );
        assert_eq!(
            effort_ladder_for_selection(
                Some(rsi_common::types::SessionProvider::Codex),
                Some("gpt-6-astra"),
                rsi_common::types::SessionProvider::Codex,
                Some("gpt-6-astra")
            ),
            ultra_ladder
        );
        assert_eq!(
            effort_ladder_for_selection(
                Some(rsi_common::types::SessionProvider::CodexAppServer),
                None,
                rsi_common::types::SessionProvider::CodexAppServer,
                None
            ),
            ultra_ladder
        );
        if let OverlayState::CreateEntityForm {
            provider,
            model,
            effort,
            ..
        } = &mut app.overlay
        {
            *provider = Some(rsi_common::types::SessionProvider::Codex);
            *model = Some("gpt-6-astra".to_string());
            *effort = None;
        }

        for expected in [
            Some("low".to_string()),
            Some("medium".to_string()),
            Some("high".to_string()),
            Some("xhigh".to_string()),
            Some("max".to_string()),
            Some("ultra".to_string()),
            None,
        ] {
            cycle_effort(&mut app, true);
            match &app.overlay {
                OverlayState::CreateEntityForm { effort, .. } => assert_eq!(*effort, expected),
                _ => panic!("overlay shape lost"),
            }
        }
    }

    #[tokio::test]
    async fn form_model_selection_clears_unsupported_effort() {
        let mut app = test_app();
        let epic_id = uuid::Uuid::new_v4();
        open_task_in_normal_mode(&mut app, epic_id).await;
        app.selected_provider = rsi_common::types::SessionProvider::Codex;
        app.selected_model = Some("gpt-6-astra".to_string());
        app.selected_effort = Some("ultra".to_string());
        if let OverlayState::CreateEntityForm {
            provider,
            model,
            effort,
            model_dropdown,
            ..
        } = &mut app.overlay
        {
            *provider = Some(rsi_common::types::SessionProvider::Codex);
            *model = Some("gpt-6-astra".to_string());
            *effort = Some("ultra".to_string());
            *model_dropdown = Some(crate::types::ModelDropdownState::new(
                rsi_common::types::SessionProvider::Codex,
                vec![("gpt-5.5".to_string(), "GPT-5.5".to_string())],
                None,
            ));
        }

        handle_model_dropdown_intercept(
            &mut app,
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
        );

        let (form_provider, form_model, form_effort) = match &app.overlay {
            OverlayState::CreateEntityForm {
                provider,
                model,
                effort,
                ..
            } => {
                assert_eq!(model.as_deref(), Some("gpt-5.5"));
                assert_eq!(effort, &None);
                (*provider, model.clone(), effort.clone())
            }
            _ => panic!("overlay shape lost"),
        };

        let options = leaf_launch_options(
            &app,
            SessionKind::Task,
            "task",
            "body",
            Some(epic_id),
            PathBuf::from("/tmp/model-selection"),
            form_provider,
            form_model,
            form_effort,
            false,
            vec![crate::types::PHASE1_PLACEHOLDER_TAG.to_string()],
        );
        assert_eq!(options.provider, rsi_common::types::SessionProvider::Codex);
        assert_eq!(options.model.as_deref(), Some("gpt-5.5"));
        assert_eq!(options.effort, None);
    }

    #[tokio::test]
    async fn sandbox_toggle_persists_in_draft() {
        let mut app = test_app();
        let epic_id = uuid::Uuid::new_v4();
        open_task_in_normal_mode(&mut app, epic_id).await;
        // s flips sandbox false -> true and writes to draft.
        let key = KeyEvent::new(KeyCode::Char('s'), KeyModifiers::NONE);
        handle_create_entity_form_key(&mut app, key).await;
        match &app.overlay {
            OverlayState::CreateEntityForm { sandbox, .. } => {
                assert!(*sandbox, "sandbox should be true after toggle");
            }
            _ => panic!("overlay shape lost"),
        }
        let draft = app
            .create_entity_draft
            .as_ref()
            .expect("draft saved after sandbox toggle");
        assert!(draft.sandbox, "draft.sandbox must mirror form sandbox");
    }

    /// P2.4: a cold-start modal open (no matching in-flight draft) must
    /// prefill `sandbox` from `app.modal_defaults.sandbox` — the value
    /// written back on the last successful submit. This is the "always
    /// default to what the user last set it to" contract for the sandbox
    /// toggle; nothing previously exercised the read side (only the
    /// draft-mirrors-toggle side above).
    #[tokio::test]
    async fn sandbox_toggle_cold_start_prefills_from_modal_defaults() {
        let mut app = test_app();
        app.modal_defaults.sandbox = true;
        // No create_entity_draft present, so open_create_entity_form must
        // take the cold-start branch and read app.modal_defaults.
        assert!(app.create_entity_draft.is_none());

        let epic_id = uuid::Uuid::new_v4();
        open_task_in_normal_mode(&mut app, epic_id).await;

        match &app.overlay {
            OverlayState::CreateEntityForm { sandbox, .. } => {
                assert!(
                    *sandbox,
                    "cold-start modal must prefill sandbox=true from modal_defaults, \
                     not reset to the hardcoded false default"
                );
            }
            _ => panic!("overlay shape lost"),
        }
    }

    #[tokio::test]
    async fn model_dropdown_open_via_m_writes_back_on_selected() {
        let mut app = test_app();
        let epic_id = uuid::Uuid::new_v4();
        open_task_in_normal_mode(&mut app, epic_id).await;

        // Press `m` — model dropdown opens.
        let key = KeyEvent::new(KeyCode::Char('m'), KeyModifiers::NONE);
        handle_create_entity_form_key(&mut app, key).await;
        match &app.overlay {
            OverlayState::CreateEntityForm { model_dropdown, .. } => {
                assert!(model_dropdown.is_some());
                if let Some(state) = model_dropdown.as_ref() {
                    assert!(state.open);
                }
            }
            _ => panic!("overlay shape lost"),
        }
        // Inject a stub model list + select index 0.
        if let OverlayState::CreateEntityForm {
            model_dropdown: Some(state),
            ..
        } = &mut app.overlay
        {
            state.models = vec![("opus-4".to_string(), "Opus 4".to_string())];
            state.selected_index = 0;
        }
        // Press Enter — dropdown intercept handler should commit and close.
        let key = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
        handle_create_entity_form_key(&mut app, key).await;
        match &app.overlay {
            OverlayState::CreateEntityForm {
                model,
                model_dropdown,
                ..
            } => {
                assert!(
                    model_dropdown.is_none(),
                    "dropdown should close on Selected"
                );
                assert_eq!(*model, Some("opus-4".to_string()));
            }
            _ => panic!("overlay shape lost"),
        }
    }

    // ═══════════════════════════════════════════════════════════════════════
    // P2.3 Phase 6 — Submit dispatch + auto-draft round-trip
    // ═══════════════════════════════════════════════════════════════════════

    /// Drive the form to a submittable state with `topology_id` set, then
    /// call submit. The daemon socket is not present in tests so the RPC
    /// fails with `NotConnected` — but the test passes regardless because
    /// (a) the submit dispatch path is reached (proving the dispatch
    /// destructure works) and (b) the form re-renders with an error banner
    /// (proving the failure is mapped through set_error correctly).
    #[tokio::test]
    async fn submit_epic_with_topology_calls_create_container() {
        let mut app = test_app();
        let group_id = uuid::Uuid::new_v4();
        let mut g = make_session(SessionKind::Group);
        g.id = group_id;
        app.sessions
            .insert(group_id, crate::types::SessionState::new(g));
        open_create_entity_form(&mut app, SessionKind::Epic, Some(group_id)).await;
        let topology_uuid = uuid::Uuid::new_v4();
        // Inject name + tags + topology_id directly.
        if let OverlayState::CreateEntityForm {
            name,
            tags,
            topology_id,
            insert_mode,
            ..
        } = &mut app.overlay
        {
            *name = "epic-1".to_string();
            tags.push(TagChip {
                value: "test".to_string(),
                status: ChipStatus::Committed,
            });
            *topology_id = Some(topology_uuid);
            *insert_mode = false;
        }
        // Submit — daemon RPC fails (NotConnected) but the dispatch path runs.
        submit_create_entity_form(&mut app).await;
        // The form remains open with an error banner.
        match &app.overlay {
            OverlayState::CreateEntityForm {
                error, topology_id, ..
            } => {
                assert!(
                    error.is_some(),
                    "expected RPC error banner after NotConnected"
                );
                // Critical: topology_id was preserved through submit.
                assert_eq!(*topology_id, Some(topology_uuid));
            }
            _ => panic!("form should still be open after RPC failure"),
        }
    }

    #[tokio::test]
    async fn submit_leaf_threads_execution_fields() {
        let mut app = test_app();
        let epic_id = uuid::Uuid::new_v4();
        open_task_in_normal_mode(&mut app, epic_id).await;
        // Inject name + tags + execution fields.
        if let OverlayState::CreateEntityForm {
            name,
            tags,
            provider,
            model,
            effort,
            sandbox,
            ..
        } = &mut app.overlay
        {
            *name = "task-1".to_string();
            tags.push(TagChip {
                value: "test".to_string(),
                status: ChipStatus::Committed,
            });
            *provider = Some(rsi_common::types::SessionProvider::Codex);
            *model = Some("gpt-5".to_string());
            *effort = Some("high".to_string());
            *sandbox = true;
        }
        submit_create_entity_form(&mut app).await;
        // Daemon NotConnected → form stays open with error banner.
        match &app.overlay {
            OverlayState::CreateEntityForm {
                error,
                provider,
                model,
                effort,
                sandbox,
                ..
            } => {
                assert!(error.is_some(), "expected RPC error banner");
                // Execution fields preserved (submit path read them, did not mutate).
                assert_eq!(*provider, Some(rsi_common::types::SessionProvider::Codex));
                assert_eq!(*model, Some("gpt-5".to_string()));
                assert_eq!(*effort, Some("high".to_string()));
                assert!(*sandbox);
            }
            _ => panic!("form should still be open after RPC failure"),
        }
    }

    async fn response_owned_leaf_form(app: &mut App, socket_path: PathBuf) {
        let epic_id = uuid::Uuid::new_v4();
        open_task_in_normal_mode(app, epic_id).await;
        app.client = DaemonClient::new(socket_path);
        app.poll.connected = true;
        app.poll.authoritative_config_ready = true;
        if let OverlayState::CreateEntityForm {
            name,
            body,
            tags,
            provider,
            model,
            effort,
            sandbox,
            ..
        } = &mut app.overlay
        {
            *name = "Exact response-owned task".to_string();
            body.textarea.insert_str("preserve this exact body");
            tags.clear();
            tags.push(TagChip {
                value: "response-owned".to_string(),
                status: ChipStatus::Committed,
            });
            *provider = Some(rsi_common::types::SessionProvider::Claude);
            *model = Some("claude-sonnet-5".to_string());
            *effort = Some("high".to_string());
            *sandbox = true;
        } else {
            panic!("response-owned leaf form must be open");
        }
        auto_save_draft(app);
    }

    fn assert_response_owned_leaf_preserved(app: &App, error: &str) {
        match &app.overlay {
            OverlayState::CreateEntityForm {
                kind,
                name,
                body,
                tags,
                provider,
                model,
                effort,
                sandbox,
                error: visible_error,
                ..
            } => {
                assert_eq!(*kind, SessionKind::Task);
                assert_eq!(name, "Exact response-owned task");
                assert_eq!(body.content(), "preserve this exact body");
                assert!(tags.iter().any(|chip| {
                    chip.value == "response-owned" && chip.status == ChipStatus::Committed
                }));
                assert_eq!(*provider, Some(rsi_common::types::SessionProvider::Claude));
                assert_eq!(model.as_deref(), Some("claude-sonnet-5"));
                assert_eq!(effort.as_deref(), Some("high"));
                assert!(*sandbox);
                assert!(
                    visible_error
                        .as_deref()
                        .is_some_and(|text| text.contains(error))
                );
            }
            _ => panic!("exact response-owned create-entity form must remain open"),
        }
        let draft = app
            .create_entity_draft
            .as_ref()
            .expect("response-owned leaf draft remains persisted");
        assert_eq!(draft.name, "Exact response-owned task");
        assert_eq!(draft.body, vec!["preserve this exact body".to_string()]);
        assert_eq!(draft.tags, vec!["response-owned".to_string()]);
    }

    #[tokio::test]
    async fn create_entity_leaf_protocol_pending_rejection_stale_and_retry_acceptance() {
        use std::sync::{Arc, Mutex};
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
        use tokio::sync::{Notify, mpsc};

        let temp_dir = tempfile::tempdir().expect("temporary create-entity socket directory");
        let socket_path = temp_dir.path().join("daemon.sock");
        let listener = tokio::net::UnixListener::bind(&socket_path).expect("create listener");
        let requests = Arc::new(Mutex::new(Vec::<serde_json::Value>::new()));
        let server_requests = Arc::clone(&requests);
        let first_release = Arc::new(Notify::new());
        let second_release = Arc::new(Notify::new());
        let server_first_release = Arc::clone(&first_release);
        let server_second_release = Arc::clone(&second_release);
        let (entered_tx, mut entered_rx) = mpsc::unbounded_channel();
        let accepted_id = uuid::Uuid::new_v4();
        let server = tokio::spawn(async move {
            for step in 0..2 {
                let (stream, _) = listener.accept().await.expect("create client");
                let (reader, mut writer) = stream.into_split();
                let mut lines = BufReader::new(reader).lines();
                let line = lines
                    .next_line()
                    .await
                    .expect("create request read")
                    .expect("create request line");
                let request: serde_json::Value =
                    serde_json::from_str(&line).expect("create request JSON");
                assert_eq!(request["method"], "LaunchSession");
                server_requests
                    .lock()
                    .expect("create request log")
                    .push(request.clone());
                let _ = entered_tx.send(step);
                if step == 0 {
                    server_first_release.notified().await;
                } else {
                    server_second_release.notified().await;
                }
                let response = if step == 0 {
                    serde_json::json!({
                        "jsonrpc": "2.0",
                        "id": request["id"].clone(),
                        "error": {"code": -32081, "message": "leaf rejected exactly"},
                    })
                } else {
                    serde_json::json!({
                        "jsonrpc": "2.0",
                        "id": request["id"].clone(),
                        "result": {"session_id": accepted_id},
                    })
                };
                writer
                    .write_all(format!("{response}\n").as_bytes())
                    .await
                    .expect("create response write");
            }
        });

        let mut app = test_app();
        app.modal_defaults.tags = Some(vec!["prior-default".to_string()]);
        let prior_defaults = app.modal_defaults.clone();
        response_owned_leaf_form(&mut app, socket_path).await;
        let submit = KeyEvent::new(KeyCode::Enter, KeyModifiers::CONTROL);
        handle_create_entity_form_key(&mut app, submit).await;
        assert_eq!(
            tokio::time::timeout(std::time::Duration::from_secs(1), entered_rx.recv())
                .await
                .expect("first request deadline"),
            Some(0)
        );
        handle_create_entity_form_key(&mut app, submit).await;
        handle_create_entity_form_key(&mut app, KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE))
            .await;
        assert!(app.interactive_create_entity_launch_pending());
        assert_eq!(requests.lock().expect("create request log").len(), 1);
        assert!(matches!(app.overlay, OverlayState::CreateEntityForm { .. }));
        assert_eq!(app.modal_defaults, prior_defaults);

        first_release.notify_one();
        let rejected = tokio::time::timeout(
            std::time::Duration::from_secs(1),
            app.interactive_launch_rx.recv(),
        )
        .await
        .expect("semantic rejection deadline")
        .expect("semantic rejection");
        let stale = rejected.clone();
        assert!(app.apply_interactive_launch_result(rejected));
        assert_response_owned_leaf_preserved(&app, "leaf rejected exactly");
        assert_eq!(app.modal_defaults, prior_defaults);

        handle_create_entity_form_key(&mut app, submit).await;
        assert_eq!(
            tokio::time::timeout(std::time::Duration::from_secs(1), entered_rx.recv())
                .await
                .expect("retry request deadline"),
            Some(1)
        );
        let retry_generation = app
            .interactive_launch_pending
            .as_ref()
            .expect("retry owner")
            .generation;
        assert!(!app.apply_interactive_launch_result(stale));
        assert_eq!(
            app.interactive_launch_pending
                .as_ref()
                .map(|pending| pending.generation),
            Some(retry_generation)
        );
        assert!(matches!(app.overlay, OverlayState::CreateEntityForm { .. }));

        second_release.notify_one();
        let accepted = tokio::time::timeout(
            std::time::Duration::from_secs(1),
            app.interactive_launch_rx.recv(),
        )
        .await
        .expect("retry acceptance deadline")
        .expect("retry acceptance");
        assert!(app.apply_interactive_launch_result(accepted));
        assert!(matches!(app.overlay, OverlayState::None));
        assert!(app.create_entity_draft.is_none());
        assert_eq!(app.modal_defaults.kind, Some(SessionKind::Task));
        assert_eq!(
            app.modal_defaults.tags,
            Some(vec!["response-owned".to_string()])
        );
        assert!(app.notifications.iter().any(|notification| {
            notification.message == "Created Task: Exact response-owned task"
        }));
        assert!(app.notifications.iter().any(|notification| {
            notification.session_id == Some(accepted_id)
                && notification.kind == crate::types::NotificationKind::SessionLaunching
        }));
        let requests = requests.lock().expect("create request log");
        assert_eq!(requests.len(), 2);
        for request in requests.iter() {
            assert_eq!(request["params"]["title"], "Exact response-owned task");
            assert_eq!(request["params"]["query"], "preserve this exact body");
            assert_eq!(
                request["params"]["tags"],
                serde_json::json!(["response-owned"])
            );
        }
        server.await.expect("create server");
    }

    #[tokio::test]
    async fn create_entity_leaf_protocol_transport_drop_reopens_exact_form_then_retries() {
        use std::sync::{Arc, Mutex};
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

        let temp_dir = tempfile::tempdir().expect("temporary create-entity socket directory");
        let socket_path = temp_dir.path().join("daemon.sock");
        let listener = tokio::net::UnixListener::bind(&socket_path).expect("create listener");
        let requests = Arc::new(Mutex::new(Vec::<serde_json::Value>::new()));
        let server_requests = Arc::clone(&requests);
        let accepted_id = uuid::Uuid::new_v4();
        let server = tokio::spawn(async move {
            for step in 0..2 {
                let (stream, _) = listener.accept().await.expect("create client");
                let (reader, mut writer) = stream.into_split();
                let mut lines = BufReader::new(reader).lines();
                let line = lines
                    .next_line()
                    .await
                    .expect("create request read")
                    .expect("create request line");
                let request: serde_json::Value =
                    serde_json::from_str(&line).expect("create request JSON");
                server_requests
                    .lock()
                    .expect("create request log")
                    .push(request.clone());
                if step == 0 {
                    continue;
                }
                let response = serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": request["id"].clone(),
                    "result": {"session_id": accepted_id},
                });
                writer
                    .write_all(format!("{response}\n").as_bytes())
                    .await
                    .expect("create response write");
            }
        });

        let mut app = test_app();
        response_owned_leaf_form(&mut app, socket_path).await;
        let submit = KeyEvent::new(KeyCode::Enter, KeyModifiers::CONTROL);
        handle_create_entity_form_key(&mut app, submit).await;
        let dropped = tokio::time::timeout(
            std::time::Duration::from_secs(1),
            app.interactive_launch_rx.recv(),
        )
        .await
        .expect("transport drop deadline")
        .expect("transport drop result");
        assert!(app.apply_interactive_launch_result(dropped));
        assert_response_owned_leaf_preserved(&app, "Connection closed");

        handle_create_entity_form_key(&mut app, submit).await;
        let accepted = tokio::time::timeout(
            std::time::Duration::from_secs(1),
            app.interactive_launch_rx.recv(),
        )
        .await
        .expect("transport retry deadline")
        .expect("transport retry result");
        assert!(app.apply_interactive_launch_result(accepted));
        assert!(matches!(app.overlay, OverlayState::None));
        assert!(app.notifications.iter().any(|notification| {
            notification.session_id == Some(accepted_id)
                && notification.message == "Session launching..."
        }));
        server.await.expect("create server");
        assert_eq!(requests.lock().expect("create request log").len(), 2);
    }

    #[tokio::test]
    async fn submit_group_ignores_topology_id() {
        // Even when form.topology_id is Some, a Group submission defensively
        // passes None — Group is not an Epic, no topology applies.
        // We can't directly inspect the RPC payload, but we can confirm
        // the submit path doesn't reject the Group (it just fails with
        // NotConnected after the dispatch).
        let mut app = test_app();
        open_create_entity_form(&mut app, SessionKind::Group, None).await;
        let topology_uuid = uuid::Uuid::new_v4();
        if let OverlayState::CreateEntityForm {
            name,
            tags,
            topology_id,
            insert_mode,
            ..
        } = &mut app.overlay
        {
            *name = "group-1".to_string();
            tags.push(TagChip {
                value: "test".to_string(),
                status: ChipStatus::Committed,
            });
            *topology_id = Some(topology_uuid); // ← intentionally Some for Group
            *insert_mode = false;
        }
        submit_create_entity_form(&mut app).await;
        // Even with topology_id set on a Group form, submit must reach the
        // dispatch path (no kind-rejection error). The defensive gate inside
        // submit_create_entity_form passes None to create_container for Group.
        // The RPC fails with NotConnected → set_error sets a Create failed banner.
        match &app.overlay {
            OverlayState::CreateEntityForm { error, .. } => {
                let err = error.as_deref().unwrap_or("");
                assert!(
                    err.starts_with("Create failed:"),
                    "expected RPC error, not validation error; got: {err}"
                );
            }
            _ => panic!("form should still be open"),
        }
    }

    #[tokio::test]
    async fn auto_draft_round_trips_all_new_fields() {
        let mut app = test_app();
        let epic_id = uuid::Uuid::new_v4();
        open_task_in_normal_mode(&mut app, epic_id).await;
        let topology_uuid = uuid::Uuid::new_v4();
        // Mutate the form directly with all 5 new persistable fields.
        if let OverlayState::CreateEntityForm {
            topology_id,
            provider,
            model,
            effort,
            sandbox,
            body,
            ..
        } = &mut app.overlay
        {
            *topology_id = Some(topology_uuid);
            *provider = Some(rsi_common::types::SessionProvider::Codex);
            *model = Some("gpt-5".to_string());
            *effort = Some("high".to_string());
            *sandbox = true;
            // S1: multi-line multibyte body (multibyte fixture per S0 lesson).
            body.textarea.insert_str("para1\n\npara2 é✨");
        }
        auto_save_draft(&mut app);
        let draft = app.create_entity_draft.as_ref().expect("draft saved");
        // Round-trip through serde JSON to confirm extract_draft +
        // CreateEntityDraft serialize symmetry.
        let json = serde_json::to_string(draft).expect("serialize");
        let back: CreateEntityDraft = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back.topology_id, Some(topology_uuid));
        assert_eq!(
            back.provider,
            Some(rsi_common::types::SessionProvider::Codex)
        );
        assert_eq!(back.model, Some("gpt-5".to_string()));
        assert_eq!(back.effort, Some("high".to_string()));
        assert!(back.sandbox);
        assert_eq!(
            back.body,
            vec!["para1".to_string(), String::new(), "para2 é✨".to_string()],
            "multi-line multibyte body must survive draft round-trip"
        );
    }

    /// S1 Phase 1: opening the form with a matching-parent draft restores the
    /// body content into the `InputSurface`, which must sit in Normal mode at
    /// open (unfocused; focus transitions enter Insert in Phase 2).
    #[tokio::test]
    async fn open_with_matching_draft_restores_body_in_normal_mode() {
        let mut app = test_app();
        app.create_entity_draft = Some(CreateEntityDraft {
            kind: SessionKind::Standard,
            name: String::new(),
            body: vec!["para1".to_string(), String::new(), "para2 é✨".to_string()],
            tags: vec!["t".into()],
            parent_id: None,
            topology_id: None,
            provider: None,
            model: None,
            effort: None,
            sandbox: false,
        });
        open_create_entity_form(&mut app, SessionKind::Standard, None).await;
        match &app.overlay {
            OverlayState::CreateEntityForm { body, .. } => {
                assert_eq!(body.content(), "para1\n\npara2 é✨");
                assert_eq!(
                    body.mode,
                    crate::types::PopupMode::Normal,
                    "body surface must idle in Normal mode at open"
                );
            }
            _ => panic!("Expected CreateEntityForm overlay"),
        }
    }

    /// S1 Phase 1: the `gp` parent-picker push/restore round-trip carries the
    /// whole variant via `std::mem::replace`, so the body surface (content,
    /// cursor, and mode) survives with NO per-field snapshot code. Pins the
    /// mem::replace invariant against future refactors.
    #[tokio::test]
    async fn gp_round_trip_preserves_body_content_cursor_and_mode() {
        let mut app = test_app();
        open_create_entity_form(&mut app, SessionKind::Standard, None).await;
        if let OverlayState::CreateEntityForm {
            body, insert_mode, ..
        } = &mut app.overlay
        {
            *insert_mode = false;
            body.textarea.insert_str("line é✨\nsecond");
            // Leave the surface in Insert with a mid-buffer cursor.
            body.mode = crate::types::PopupMode::Insert;
            body.textarea
                .move_cursor(tui_textarea::CursorMove::Jump(0, 3));
        }
        // gp chord: push to the parent picker sub-overlay.
        for ch in ['g', 'p'] {
            let key = KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE);
            handle_create_entity_form_key(&mut app, key).await;
        }
        assert!(matches!(app.overlay, OverlayState::ParentPicker { .. }));
        // Esc restores the form.
        let esc = KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE);
        crate::overlay::parent_picker::handle_parent_picker_key(&mut app, esc).await;
        match &app.overlay {
            OverlayState::CreateEntityForm { body, .. } => {
                assert_eq!(body.content(), "line é✨\nsecond");
                assert_eq!(body.textarea.cursor(), (0, 3), "cursor must survive");
                assert_eq!(
                    body.mode,
                    crate::types::PopupMode::Insert,
                    "mode must survive"
                );
            }
            _ => panic!("form must be restored after picker Esc"),
        }
    }

    // ═══════════════════════════════════════════════════════════════════════
    // P2.3 Phase 5 — Parent picker integration (gp form-local intercept)
    // ═══════════════════════════════════════════════════════════════════════

    #[tokio::test]
    async fn gp_chord_opens_parent_picker_not_prompt_creator() {
        // With the create-entity form active, dispatching `g` then `p` opens
        // OverlayState::ParentPicker (NOT OpenPromptCreator). The form is
        // snapshotted into app.create_entity_form_pending for restore.
        let mut app = test_app();
        open_create_entity_form(&mut app, SessionKind::Group, None).await;
        if let OverlayState::CreateEntityForm { insert_mode, .. } = &mut app.overlay {
            *insert_mode = false;
        }
        // First key: `g` sets g_pending.
        let g_key = KeyEvent::new(KeyCode::Char('g'), KeyModifiers::NONE);
        handle_create_entity_form_key(&mut app, g_key).await;
        // The form should still be the visible overlay (no overlay swap yet).
        assert!(matches!(app.overlay, OverlayState::CreateEntityForm { .. }));
        match &app.overlay {
            OverlayState::CreateEntityForm { g_pending, .. } => assert!(*g_pending),
            _ => panic!("form should still be active after `g`"),
        }
        // Second key: `p` consumes leader and opens the parent picker.
        let p_key = KeyEvent::new(KeyCode::Char('p'), KeyModifiers::NONE);
        handle_create_entity_form_key(&mut app, p_key).await;
        // Now overlay should be ParentPicker with sentinel session_id == nil.
        match &app.overlay {
            OverlayState::ParentPicker { session_id, .. } => {
                assert!(session_id.is_nil(), "sentinel: form-pending picker");
            }
            _ => panic!("expected ParentPicker overlay after `gp` chord"),
        }
        assert!(
            app.create_entity_form_pending.is_some(),
            "form should be snapshotted into pending slot"
        );
    }

    #[tokio::test]
    async fn parent_picker_filters_by_legal_children() {
        // form.kind == Epic → picker candidates contain only Group-kind
        // sessions (Epic is legal only under Group).
        let mut app = test_app();
        // Two candidates: a Group (legal parent for Epic) and a Standard
        // (NOT legal parent for Epic).
        let group_id = uuid::Uuid::new_v4();
        let mut g = make_session(SessionKind::Group);
        g.id = group_id;
        app.sessions
            .insert(group_id, crate::types::SessionState::new(g));
        let std_id = uuid::Uuid::new_v4();
        let mut s = make_session(SessionKind::Standard);
        s.id = std_id;
        app.sessions
            .insert(std_id, crate::types::SessionState::new(s));

        open_create_entity_form(&mut app, SessionKind::Epic, Some(group_id)).await;
        if let OverlayState::CreateEntityForm { insert_mode, .. } = &mut app.overlay {
            *insert_mode = false;
        }
        // Run gp chord.
        for ch in ['g', 'p'] {
            let key = KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE);
            handle_create_entity_form_key(&mut app, key).await;
        }
        let candidates = match &app.overlay {
            OverlayState::ParentPicker { candidates, .. } => candidates.clone(),
            _ => panic!("expected ParentPicker"),
        };
        // Should contain group_id (legal parent), should NOT contain std_id.
        assert!(
            candidates.contains(&group_id),
            "Group must be a candidate parent for Epic"
        );
        assert!(
            !candidates.contains(&std_id),
            "Standard must NOT be a candidate parent for Epic"
        );
        // Should NOT include the [root] sentinel — Epic is not legal at root.
        assert!(
            !candidates.iter().any(|id| id.is_nil()),
            "[root] not legal for Epic"
        );
    }

    #[tokio::test]
    async fn parent_picker_legal_for_form_kind_not_session_kind() {
        // Same env, different form.kind → different candidate set.
        let mut app = test_app();
        let group_id = uuid::Uuid::new_v4();
        let mut g = make_session(SessionKind::Group);
        g.id = group_id;
        app.sessions
            .insert(group_id, crate::types::SessionState::new(g));
        // Open form for Standard (legal at root) — root candidate appears.
        open_create_entity_form(&mut app, SessionKind::Standard, None).await;
        if let OverlayState::CreateEntityForm { insert_mode, .. } = &mut app.overlay {
            *insert_mode = false;
        }
        for ch in ['g', 'p'] {
            let key = KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE);
            handle_create_entity_form_key(&mut app, key).await;
        }
        let candidates_for_standard = match &app.overlay {
            OverlayState::ParentPicker { candidates, .. } => candidates.clone(),
            _ => panic!("expected ParentPicker"),
        };
        // Standard is legal under root AND under Group.
        assert!(
            candidates_for_standard.iter().any(|id| id.is_nil()),
            "[root] must be legal for Standard"
        );
        assert!(
            candidates_for_standard.contains(&group_id),
            "Group must be legal parent for Standard"
        );
    }

    #[tokio::test]
    async fn parent_picker_esc_restores_form() {
        let mut app = test_app();
        open_create_entity_form(&mut app, SessionKind::Group, None).await;
        if let OverlayState::CreateEntityForm { insert_mode, .. } = &mut app.overlay {
            *insert_mode = false;
        }
        for ch in ['g', 'p'] {
            let key = KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE);
            handle_create_entity_form_key(&mut app, key).await;
        }
        // Picker now active. Esc on the picker restores the form.
        assert!(matches!(app.overlay, OverlayState::ParentPicker { .. }));
        let esc = KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE);
        crate::overlay::parent_picker::handle_parent_picker_key(&mut app, esc).await;
        assert!(matches!(app.overlay, OverlayState::CreateEntityForm { .. }));
        assert!(
            app.create_entity_form_pending.is_none(),
            "pending slot drained after restore"
        );
    }

    #[tokio::test]
    async fn topology_filter_typed_chars_mutate_buffer() {
        let mut app = test_app();
        let group_id = uuid::Uuid::new_v4();
        let mut group = make_session(SessionKind::Group);
        group.id = group_id;
        app.sessions
            .insert(group_id, crate::types::SessionState::new(group));
        open_create_entity_form(&mut app, SessionKind::Epic, Some(group_id)).await;
        if let OverlayState::CreateEntityForm {
            focused_field,
            topology_choices,
            ..
        } = &mut app.overlay
        {
            *focused_field = CreateEntityField::Topology;
            *topology_choices = vec![fake_topology("alpha"), fake_topology("beta")];
        }
        // Type "a" then "l" — filter should be "al" + selection reset.
        for ch in ['a', 'l'] {
            let key = KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE);
            handle_create_entity_form_key(&mut app, key).await;
        }
        match &app.overlay {
            OverlayState::CreateEntityForm {
                topology_filter,
                topology_selected_index,
                ..
            } => {
                assert_eq!(topology_filter, "al");
                assert_eq!(*topology_selected_index, 0);
            }
            _ => panic!("overlay shape lost"),
        }
    }

    #[test]
    fn create_entity_draft_backward_compat_missing_new_fields() {
        // Older dev-state.json (V46) lacks the 5 new keys — they must
        // deserialize to defaults (None / false) via #[serde(default)].
        let legacy = r#"{
            "kind": "Group",
            "name": "g",
            "tags": [],
            "parent_id": null
        }"#;
        let draft: CreateEntityDraft = serde_json::from_str(legacy).expect("legacy parses");
        assert_eq!(draft.kind, SessionKind::Group);
        assert!(draft.topology_id.is_none());
        assert!(draft.provider.is_none());
        assert!(draft.model.is_none());
        assert!(draft.effort.is_none());
        assert!(!draft.sandbox);
        // S1: legacy JSON without a `body` key defaults to empty (F-030).
        assert!(draft.body.is_empty());
    }
}
