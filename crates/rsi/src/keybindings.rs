//! Vim keybinding setup for flywheel.
//!
//! Builds a VimMachine with default vim bindings plus custom flywheel
//! mappings, then wraps it in a KeyManager.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use editor_types::Action;
use editor_types::prelude::{EditTarget, MoveDir1D, MovePosition, MoveType, RepeatType};
use keybindings::{BindingMachine, EdgeEvent, EdgeRepeat};
use modalkit::editing::key::KeyManager;
use modalkit::env::vim::VimMode;
use modalkit::env::vim::keybindings::{InputStep, VimMachine, default_vim_keys};
use modalkit::key::TerminalKey;

use crate::modalkit_types::{InsertStyle, LcAction, LcInfo};

/// Concrete types used throughout the keybinding system.
pub type LcVimMachine = VimMachine<TerminalKey, LcInfo>;
pub type LcKeyManager = KeyManager<TerminalKey, Action<LcInfo>, RepeatType>;
type EP = Vec<(
    EdgeRepeat,
    EdgeEvent<TerminalKey, modalkit::env::CommonKeyClass>,
)>;

/// Build a TerminalKey from a KeyCode (no modifiers).
fn tk(code: KeyCode) -> TerminalKey {
    TerminalKey::from(code)
}

/// Build a two-key EdgePath (e.g., `]a`, `[e`, `ZQ`).
#[cfg(test)]
fn edge2(code1: KeyCode, code2: KeyCode) -> EP {
    vec![
        (EdgeRepeat::Once, EdgeEvent::Key(tk(code1))),
        (EdgeRepeat::Once, EdgeEvent::Key(tk(code2))),
    ]
}

/// Build an InputStep that produces a single LcAction.
fn lc_step(action: LcAction) -> InputStep<LcInfo> {
    InputStep::new().actions(vec![Action::Application(action)])
}

/// Build an InputStep that consumes a key sequence without producing behavior.
fn noop_step() -> InputStep<LcInfo> {
    InputStep::new().actions(vec![Action::NoOp])
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum RegisteredNormalEffect {
    Application(LcAction),
    DefaultVimMotion,
}

/// The key-machine effect of a registered Normal binding. `sequence` selects
/// the member of a parameterised family (`<Space>1..9`, `gf1..9`, `i a o O`).
fn registered_normal_effect(
    id: crate::action_registry::ActionId,
    sequence: &str,
) -> Option<RegisteredNormalEffect> {
    use crate::action_registry::ActionId;

    let digit = || {
        sequence
            .chars()
            .last()
            .and_then(|c| c.to_digit(10))
            .and_then(|d| u8::try_from(d).ok())
            .filter(|d| (1..=9).contains(d))
    };
    let effect = match id {
        ActionId::ContextHelp => {
            RegisteredNormalEffect::Application(LcAction::ToggleKeybindingsHelp)
        }
        ActionId::MoveDown | ActionId::MoveUp | ActionId::JumpTop | ActionId::JumpBottom => {
            RegisteredNormalEffect::DefaultVimMotion
        }
        ActionId::Search => RegisteredNormalEffect::Application(LcAction::EnterSearch),
        ActionId::Open => RegisteredNormalEffect::Application(LcAction::EnterSession),
        ActionId::Refresh => RegisteredNormalEffect::Application(LcAction::RefreshNavigation),
        ActionId::OpenThemePicker => RegisteredNormalEffect::Application(LcAction::OpenThemePicker),
        ActionId::OpenLegacyColors => {
            RegisteredNormalEffect::Application(LcAction::OpenColorCustomizer)
        }
        ActionId::OpenIssuesWorkspace => {
            RegisteredNormalEffect::Application(LcAction::OpenIssuesWorkspace)
        }
        ActionId::ManagerPolicy => {
            RegisteredNormalEffect::Application(LcAction::EditHarnessManagerPolicy)
        }
        ActionId::ManagerBoard => {
            RegisteredNormalEffect::Application(LcAction::OpenHarnessManagerBoard)
        }
        ActionId::ManagerDecisions => {
            RegisteredNormalEffect::Application(LcAction::OpenHarnessManagerDecisions)
        }
        ActionId::InterruptSession => {
            RegisteredNormalEffect::Application(LcAction::InterruptSession)
        }
        ActionId::ContinueSession => RegisteredNormalEffect::Application(LcAction::QuickContinue),
        ActionId::TogglePin => RegisteredNormalEffect::Application(LcAction::TogglePinSession),
        ActionId::ArchiveSession => RegisteredNormalEffect::Application(LcAction::ArchiveSession),
        ActionId::UnarchiveSession => {
            RegisteredNormalEffect::Application(LcAction::UnarchiveSession)
        }
        ActionId::ToggleTestingNeeded => {
            RegisteredNormalEffect::Application(LcAction::ToggleTestingNeeded)
        }
        ActionId::AscendHierarchy => RegisteredNormalEffect::Application(LcAction::AscendContainer),
        // `yy` retains one LcAction; its handler selects transcript-yank or
        // registered session-UUID copy from the live focus context.
        ActionId::CopySessionUuid => {
            RegisteredNormalEffect::Application(LcAction::YankEventContent)
        }
        // Epic M slice (a): chords migrated from hand-installed mappings.
        ActionId::DeleteSession => RegisteredNormalEffect::Application(LcAction::DeleteSession),
        ActionId::RotateSession => RegisteredNormalEffect::Application(LcAction::RotateSession),
        ActionId::Archives => RegisteredNormalEffect::Application(LcAction::GoToArchiveZone),
        ActionId::Quit => RegisteredNormalEffect::Application(LcAction::Quit),
        ActionId::Projects => RegisteredNormalEffect::Application(LcAction::OpenProjectPicker),
        ActionId::SettingsCommand => RegisteredNormalEffect::Application(LcAction::OpenSettings),
        ActionId::StopAll => RegisteredNormalEffect::Application(LcAction::EmergencyStopAll),
        ActionId::Alerts => RegisteredNormalEffect::Application(LcAction::ToggleNotifications),
        ActionId::Graph => RegisteredNormalEffect::Application(LcAction::OpenGraphReview),
        ActionId::Lead => RegisteredNormalEffect::Application(LcAction::SetEpicLead),
        ActionId::Task => RegisteredNormalEffect::Application(LcAction::TaskRabbitPrompt),
        ActionId::Blank => RegisteredNormalEffect::Application(LcAction::BlankPrompt),
        ActionId::TabPrev => RegisteredNormalEffect::Application(LcAction::PrevTab),
        ActionId::TabNext => RegisteredNormalEffect::Application(LcAction::NextTab),
        ActionId::ClosePane => RegisteredNormalEffect::Application(LcAction::CloseFocusedPane),
        ActionId::SearchNext => RegisteredNormalEffect::Application(LcAction::NextSearchMatch),
        ActionId::SearchPrev => RegisteredNormalEffect::Application(LcAction::PrevSearchMatch),
        ActionId::NextAttention => RegisteredNormalEffect::Application(LcAction::NextAttention),
        ActionId::PrevAttention => RegisteredNormalEffect::Application(LcAction::PrevAttention),
        ActionId::JumpAttention => {
            RegisteredNormalEffect::Application(LcAction::JumpAttentionN(digit()?))
        }
        ActionId::NextLabelGroup => {
            RegisteredNormalEffect::Application(LcAction::NextLabelBoundary)
        }
        ActionId::PrevLabelGroup => {
            RegisteredNormalEffect::Application(LcAction::PrevLabelBoundary)
        }
        ActionId::GoToMainZone => RegisteredNormalEffect::Application(LcAction::GoToSessionsZone),
        ActionId::GoToTaskRabbitZone => {
            RegisteredNormalEffect::Application(LcAction::GoToTaskRabbitZone)
        }
        ActionId::GoToJobsZone => RegisteredNormalEffect::Application(LcAction::GoToJobsZone),
        ActionId::RecentCompletions => {
            RegisteredNormalEffect::Application(LcAction::OpenRecentCompletions)
        }
        ActionId::NavigateRight => RegisteredNormalEffect::Application(LcAction::NavigateRight),
        ActionId::AscendOrBack => RegisteredNormalEffect::Application(LcAction::AscendOrBack),
        ActionId::SortPicker => RegisteredNormalEffect::Application(LcAction::OpenSortPicker),
        ActionId::Trash => RegisteredNormalEffect::Application(LcAction::GoToTrash),
        ActionId::EnterInputInsert => {
            RegisteredNormalEffect::Application(LcAction::EnterInputBarInsert(match sequence {
                "i" => InsertStyle::Insert,
                "a" => InsertStyle::Append,
                "o" => InsertStyle::OpenBelow,
                "O" => InsertStyle::OpenAbove,
                _ => return None,
            }))
        }
        ActionId::EnterSessionNormal => {
            RegisteredNormalEffect::Application(LcAction::EnterSessionNormalMode)
        }
        ActionId::PromptPreview => {
            RegisteredNormalEffect::Application(LcAction::TogglePromptPreview)
        }
        ActionId::NextUserMessage => RegisteredNormalEffect::Application(LcAction::NextUserMessage),
        ActionId::PrevUserMessage => RegisteredNormalEffect::Application(LcAction::PrevUserMessage),
        ActionId::OpenFold => RegisteredNormalEffect::Application(LcAction::OpenFold),
        ActionId::CloseFold => RegisteredNormalEffect::Application(LcAction::CloseFold),
        ActionId::ToggleFold => RegisteredNormalEffect::Application(LcAction::ToggleFold),
        ActionId::CloseAllFolds => RegisteredNormalEffect::Application(LcAction::CloseAllFolds),
        ActionId::OpenAllFolds => RegisteredNormalEffect::Application(LcAction::OpenAllFolds),
        ActionId::ToggleSystemEvents => {
            RegisteredNormalEffect::Application(LcAction::ToggleSystemEvents)
        }
        ActionId::ToggleThinkingEvents => {
            RegisteredNormalEffect::Application(LcAction::ToggleThinkingEvents)
        }
        ActionId::SessionInfo => {
            RegisteredNormalEffect::Application(LcAction::OpenSessionInfoPanel)
        }
        ActionId::RenameSession => RegisteredNormalEffect::Application(LcAction::RenameSession),
        ActionId::ModelDropdown => {
            RegisteredNormalEffect::Application(LcAction::ToggleModelDropdown)
        }
        ActionId::ReassignProject => {
            RegisteredNormalEffect::Application(LcAction::ReassignSessionProject)
        }
        ActionId::ToggleRotation => {
            RegisteredNormalEffect::Application(LcAction::ToggleRotationDisabled)
        }
        ActionId::CancelRetry => RegisteredNormalEffect::Application(LcAction::CancelRetry),
        ActionId::ExecuteDocRegBlocks => {
            RegisteredNormalEffect::Application(LcAction::ExecuteDocRegBlocks)
        }
        ActionId::CommitAndPush => RegisteredNormalEffect::Application(LcAction::CommitAndPush),
        ActionId::OpenSessionInNewTab => {
            RegisteredNormalEffect::Application(LcAction::OpenSessionInNewTab)
        }
        ActionId::GitPanel => RegisteredNormalEffect::Application(LcAction::ToggleGitPanel),
        ActionId::FileExplorer => RegisteredNormalEffect::Application(LcAction::ToggleFileExplorer),
        ActionId::Telescope => RegisteredNormalEffect::Application(LcAction::OpenTelescope),
        ActionId::OpenRecentFile => {
            RegisteredNormalEffect::Application(LcAction::OpenRecentFileN(digit()?))
        }
        ActionId::PromptCreator => RegisteredNormalEffect::Application(LcAction::OpenPromptCreator),
        ActionId::CommandPalette => {
            RegisteredNormalEffect::Application(LcAction::OpenCommandPalette)
        }
        ActionId::QuestionModal => RegisteredNormalEffect::Application(LcAction::OpenQuestionModal),
        ActionId::MemorySearch => RegisteredNormalEffect::Application(LcAction::OpenMemorySearch),
        ActionId::ScheduleBrowserOpen => {
            RegisteredNormalEffect::Application(LcAction::OpenScheduleBrowser)
        }
        ActionId::EspSquare => RegisteredNormalEffect::Application(LcAction::OpenEspSquare),
        ActionId::RunEpicTopology => RegisteredNormalEffect::Application(LcAction::RunEpicTopology),
        ActionId::CreateGroup => RegisteredNormalEffect::Application(LcAction::CreateGroup),
        ActionId::CreateEpic => RegisteredNormalEffect::Application(LcAction::CreateEpic),
        ActionId::CreateStory => RegisteredNormalEffect::Application(LcAction::CreateStory),
        ActionId::CreateTask => RegisteredNormalEffect::Application(LcAction::CreateTask),
        ActionId::CreateBug => RegisteredNormalEffect::Application(LcAction::CreateBug),
        ActionId::MoveToParent => RegisteredNormalEffect::Application(LcAction::MoveToParent),
        ActionId::MoveToRoot => RegisteredNormalEffect::Application(LcAction::MoveToRoot),
        _ => return None,
    };
    Some(effect)
}

fn normal_sequence_keys(sequence: &str) -> Option<Vec<KeyCode>> {
    if sequence == "Enter" {
        return Some(vec![KeyCode::Enter]);
    }

    let mut rest = sequence;
    let mut keys = Vec::new();
    while !rest.is_empty() {
        if let Some(tail) = rest.strip_prefix("<Space>") {
            keys.push(KeyCode::Char(' '));
            rest = tail;
            continue;
        }
        let character = rest.chars().next()?;
        keys.push(KeyCode::Char(character));
        rest = &rest[character.len_utf8()..];
    }
    (!keys.is_empty()).then_some(keys)
}

/// Whether a registered Normal action is dispatched only through its
/// `LcAction` handler (Epic M design A.1.3): its chord emits an application
/// action that `request_from_lc_action` does not map back to a registry
/// request, so no registry executor arm owns it.
#[cfg(test)]
pub(crate) fn is_lc_dispatch_only(id: crate::action_registry::ActionId) -> bool {
    use crate::action_registry::{ACTION_DESCRIPTORS, ActionId, ActionRoute};

    // `yy` emits YankEventContent, but the list-surface UUID copy has its own
    // registry executor arm (see `dispatch_registered_action`).
    if id == ActionId::CopySessionUuid {
        return false;
    }
    ACTION_DESCRIPTORS
        .iter()
        .filter(|descriptor| descriptor.id == id)
        .flat_map(|descriptor| descriptor.bindings.iter())
        .filter(|binding| binding.route == ActionRoute::Normal)
        .filter_map(|binding| registered_normal_effect(id, binding.sequence))
        .any(|effect| match effect {
            RegisteredNormalEffect::Application(action) => {
                crate::action_registry::request_from_lc_action(&action).is_none()
            }
            RegisteredNormalEffect::DefaultVimMotion => false,
        })
}

/// Whole-sequence key names used by migrated Normal bindings. Every other
/// sequence is parsed character by character by `normal_sequence_keys`.
fn named_normal_key(sequence: &str) -> Option<KeyEvent> {
    let (code, modifiers) = match sequence {
        "Backspace" => (KeyCode::Backspace, KeyModifiers::NONE),
        "F2" => (KeyCode::F(2), KeyModifiers::NONE),
        "F3" => (KeyCode::F(3), KeyModifiers::NONE),
        "Ctrl-M" => (KeyCode::Char('m'), KeyModifiers::CONTROL),
        _ => return None,
    };
    Some(KeyEvent::new(code, modifiers))
}

/// Key events for a registered Normal sequence.
fn normal_sequence_events(sequence: &str) -> Option<Vec<KeyEvent>> {
    if let Some(key) = named_normal_key(sequence) {
        return Some(vec![key]);
    }
    normal_sequence_keys(sequence).map(|keys| {
        keys.into_iter()
            .map(|code| KeyEvent::new(code, KeyModifiers::NONE))
            .collect()
    })
}

fn normal_binding_edge(sequence: &str) -> Option<EP> {
    normal_sequence_events(sequence).map(|keys| {
        keys.into_iter()
            .map(|key| (EdgeRepeat::Once, EdgeEvent::Key(TerminalKey::from(key))))
            .collect()
    })
}

fn request_for_normal_machine_action(
    action: &Action<LcInfo>,
) -> Option<crate::action_registry::ActionRequest> {
    use crate::action_registry::{ActionId, ActionRequest};

    let id = match action {
        Action::Application(action) => {
            return crate::action_registry::request_from_lc_action(action);
        }
        Action::Editor(editor_types::EditorAction::Edit(_, EditTarget::Motion(move_type, _))) => {
            match move_type {
                MoveType::Line(MoveDir1D::Next) => ActionId::MoveDown,
                MoveType::Line(MoveDir1D::Previous) => ActionId::MoveUp,
                MoveType::BufferPos(MovePosition::Beginning) => ActionId::JumpTop,
                MoveType::BufferPos(MovePosition::End) => ActionId::JumpBottom,
                _ => return None,
            }
        }
        _ => return None,
    };
    Some(ActionRequest::plain(id))
}

fn default_vim_request_for(sequence: &str) -> Option<crate::action_registry::ActionRequest> {
    let keys = normal_sequence_keys(sequence)?;
    let mut manager = KeyManager::new(default_vim_keys::<LcInfo>());
    for key in keys {
        manager.input_key(tk(key));
    }
    while let Some((action, _)) = manager.pop() {
        if let Some(request) = request_for_normal_machine_action(&action) {
            return Some(request);
        }
    }
    None
}

fn install_registered_normal_bindings(machine: &mut LcVimMachine) {
    use crate::action_registry::{
        ACTION_DESCRIPTORS, ActionRequest, ActionRoute, NORMAL_KEY_RESERVATIONS,
        NormalReservationKind,
    };

    for reservation in NORMAL_KEY_RESERVATIONS {
        let edge = normal_binding_edge(reservation.sequence).unwrap_or_else(|| {
            panic!(
                "unsupported reserved Normal sequence {}",
                reservation.sequence
            )
        });
        let step = match reservation.kind {
            // An empty step leaves the node waiting for the next key.
            NormalReservationKind::Prefix => InputStep::new(),
            NormalReservationKind::Inert => noop_step(),
        };
        machine.add_mapping(VimMode::Normal, &edge, &step);
    }

    for descriptor in ACTION_DESCRIPTORS {
        for binding in descriptor
            .bindings
            .iter()
            .filter(|binding| binding.route == ActionRoute::Normal)
        {
            let effect =
                registered_normal_effect(descriptor.id, binding.sequence).unwrap_or_else(|| {
                    panic!(
                        "registered Normal action {:?} has no keybinding effect",
                        descriptor.id
                    )
                });
            match effect {
                RegisteredNormalEffect::Application(action) => {
                    let edge = normal_binding_edge(binding.sequence).unwrap_or_else(|| {
                        panic!(
                            "unsupported registered Normal sequence {}",
                            binding.sequence
                        )
                    });
                    machine.add_mapping(VimMode::Normal, &edge, &lc_step(action));
                }
                RegisteredNormalEffect::DefaultVimMotion => {
                    assert_eq!(
                        default_vim_request_for(binding.sequence),
                        Some(ActionRequest::plain(descriptor.id)),
                        "registry Normal sequence {} must retain its modalkit motion effect",
                        binding.sequence
                    );
                }
            }
        }
    }
}

/// Build a VimMachine with default vim bindings plus all flywheel custom mappings.
pub fn build_vim_machine() -> LcVimMachine {
    let mut machine: LcVimMachine = default_vim_keys::<LcInfo>();

    // Registered normal-mode mappings take their sequences from the action registry.
    // Native j/k/gg/G motion steps stay owned by modalkit and are verified against
    // the same metadata so their count/motion semantics remain unchanged.
    install_registered_normal_bindings(&mut machine);

    machine
}

/// Build the full KeyManager, ready for use in the event loop.
pub fn build_key_manager() -> LcKeyManager {
    let machine = build_vim_machine();
    KeyManager::new(machine)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::action_registry::{ACTION_DESCRIPTORS, ActionId, ActionRequest, ActionRoute};
    use keybindings::BindingMachine;
    use std::collections::BTreeSet;

    fn actions_for(keys: &[KeyCode]) -> Vec<Action<LcInfo>> {
        let mut km = build_key_manager();
        for key in keys {
            km.input_key(TerminalKey::from(*key));
        }

        let mut actions = Vec::new();
        while let Some((action, _ctx)) = km.pop() {
            actions.push(action);
        }
        actions
    }

    fn application_actions_for(keys: &[KeyCode]) -> Vec<LcAction> {
        actions_for(keys)
            .into_iter()
            .filter_map(|action| match action {
                Action::Application(action) => Some(action),
                _ => None,
            })
            .collect()
    }

    fn application_actions_for_events(keys: &[KeyEvent]) -> Vec<LcAction> {
        let mut km = build_key_manager();
        for key in keys {
            km.input_key(TerminalKey::from(*key));
        }

        let mut actions = Vec::new();
        while let Some((action, _ctx)) = km.pop() {
            if let Action::Application(action) = action {
                actions.push(action);
            }
        }
        actions
    }

    #[test]
    fn space_semicolon_opens_command_palette() {
        assert_eq!(
            application_actions_for(&[KeyCode::Char(' '), KeyCode::Char(';')]),
            vec![LcAction::OpenCommandPalette]
        );
        assert_eq!(
            application_actions_for(&[KeyCode::Char(' '), KeyCode::Char(' ')]),
            vec![LcAction::OpenTelescope]
        );
    }

    #[test]
    fn pre_remap_space_sequence_inventory_is_preserved() {
        use LcAction::*;

        // Pin each installed Space sequence before the #552 remap. Keep these
        // rows explicit so a later binding cannot silently claim an old slot.
        let cases: &[(&str, &[LcAction])] = &[
            ("<Space>", &[]),
            ("<Space> ", &[OpenTelescope]),
            ("<Space>;", &[OpenCommandPalette]),
            ("<Space>,", &[OpenSettings]),
            ("<Space>1", &[JumpAttentionN(1)]),
            ("<Space>2", &[JumpAttentionN(2)]),
            ("<Space>3", &[JumpAttentionN(3)]),
            ("<Space>4", &[JumpAttentionN(4)]),
            ("<Space>5", &[JumpAttentionN(5)]),
            ("<Space>6", &[JumpAttentionN(6)]),
            ("<Space>7", &[JumpAttentionN(7)]),
            ("<Space>8", &[JumpAttentionN(8)]),
            ("<Space>9", &[JumpAttentionN(9)]),
            ("<Space>a", &[ArchiveSession]),
            ("<Space>b", &[OpenColorCustomizer]),
            ("<Space>c", &[QuickContinue]),
            ("<Space>C", &[ReassignSessionProject]),
            ("<Space>e", &[ToggleFileExplorer]),
            ("<Space>g", &[]),
            ("<Space>gg", &[ToggleGitPanel]),
            ("<Space>gb", &[OpenHarnessManagerBoard]),
            ("<Space>gd", &[OpenHarnessManagerDecisions]),
            ("<Space>gp", &[EditHarnessManagerPolicy]),
            ("<Space>gr", &[]),
            ("<Space>i", &[OpenIssuesWorkspace]),
            ("<Space>k", &[CancelRetry]),
            ("<Space>m", &[BlankPrompt]),
            ("<Space>M", &[OpenMemorySearch]),
            ("<Space>o", &[TaskRabbitPrompt]),
            ("<Space>p", &[OpenProjectPicker]),
            ("<Space>q", &[CloseFocusedPane]),
            ("<Space>r", &[ToggleRotationDisabled]),
            ("<Space>R", &[]),
            ("<Space>s", &[OpenSortPicker]),
            ("<Space>S", &[EmergencyStopAll]),
            ("<Space>t", &[ToggleTestingNeeded]),
            ("<Space>T", &[OpenSessionInNewTab]),
            ("<Space>x", &[ExecuteDocRegBlocks]),
            ("<Space>X", &[CommitAndPush]),
        ];

        for (sequence, expected) in cases {
            let keys = normal_sequence_keys(sequence)
                .unwrap_or_else(|| panic!("invalid pinned Space sequence: {sequence}"));
            assert_eq!(
                application_actions_for(&keys),
                *expected,
                "pre-remap Space effect changed for {sequence}"
            );
        }
    }

    #[test]
    #[allow(clippy::too_many_lines)] // The full migration matrix stays together for auditability.
    fn g_leader_migration_table_dispatches_and_preserves_each_binding() {
        use LcAction::*;

        // `Some(destination)` rows migrate; `None` rows retain their g chord
        // for navigation, vim motion, mutations, or an intentional no-op.
        let cases: &[(&str, Option<&str>, Vec<LcAction>, &str)] = &[
            ("gX", Some("<Space>gX"), vec![GoToTrash], "modal launcher"),
            (
                "gq",
                Some("<Space>gq"),
                vec![OpenQuestionModal],
                "modal launcher",
            ),
            (
                "gn",
                Some("<Space>n"),
                vec![ToggleNotifications],
                "modal launcher",
            ),
            (
                "gc",
                Some("<Space>gc"),
                vec![OpenEspSquare],
                "modal launcher",
            ),
            (
                "gv",
                Some("<Space>v"),
                vec![OpenGraphReview],
                "modal launcher",
            ),
            (
                "gp",
                Some("<Space>gP"),
                vec![OpenPromptCreator],
                "modal launcher; form-local gp is separate",
            ),
            (
                "gK",
                Some("<Space>K"),
                vec![OpenScheduleBrowser],
                "modal launcher",
            ),
            (
                "gG",
                Some("<Space>G"),
                vec![CreateGroup],
                "hierarchy creation launcher",
            ),
            (
                "gE",
                Some("<Space>E"),
                vec![CreateEpic],
                "hierarchy creation launcher",
            ),
            (
                "gS",
                Some("<Space>gS"),
                vec![CreateStory],
                "hierarchy creation launcher",
            ),
            (
                "gT",
                Some("<Space>gT"),
                vec![CreateTask],
                "hierarchy creation launcher",
            ),
            (
                "gB",
                Some("<Space>B"),
                vec![CreateBug],
                "hierarchy creation launcher",
            ),
            ("gs", None, vec![GoToSessionsZone], "zone navigation"),
            ("ga", None, vec![GoToArchiveZone], "zone navigation"),
            ("gt", None, vec![GoToTaskRabbitZone], "zone navigation"),
            ("gj", None, vec![GoToJobsZone], "zone navigation"),
            (
                "gr",
                None,
                vec![OpenRecentCompletions],
                "recent-completions navigation",
            ),
            ("gL", None, vec![SetEpicLead], "lead mutation"),
            ("gR", None, vec![RunEpicTopology], "topology mutation"),
            (
                "gf1",
                None,
                vec![OpenRecentFileN(1)],
                "recent-file navigation",
            ),
            (
                "gf2",
                None,
                vec![OpenRecentFileN(2)],
                "recent-file navigation",
            ),
            (
                "gf3",
                None,
                vec![OpenRecentFileN(3)],
                "recent-file navigation",
            ),
            (
                "gf4",
                None,
                vec![OpenRecentFileN(4)],
                "recent-file navigation",
            ),
            (
                "gf5",
                None,
                vec![OpenRecentFileN(5)],
                "recent-file navigation",
            ),
            (
                "gf6",
                None,
                vec![OpenRecentFileN(6)],
                "recent-file navigation",
            ),
            (
                "gf7",
                None,
                vec![OpenRecentFileN(7)],
                "recent-file navigation",
            ),
            (
                "gf8",
                None,
                vec![OpenRecentFileN(8)],
                "recent-file navigation",
            ),
            (
                "gf9",
                None,
                vec![OpenRecentFileN(9)],
                "recent-file navigation",
            ),
            ("gg", None, vec![], "vim jump-top motion"),
            ("gm", None, vec![], "retired no-op"),
            ("g?", None, vec![], "retired no-op"),
        ];

        let mut destinations = BTreeSet::new();
        for (before, destination, expected, rationale) in cases {
            let old_keys = normal_sequence_keys(before)
                .unwrap_or_else(|| panic!("invalid g sequence: {before}"));
            if let Some(destination) = destination {
                assert!(
                    destinations.insert(*destination),
                    "duplicate migrated destination {destination}"
                );
                let new_keys = normal_sequence_keys(destination)
                    .unwrap_or_else(|| panic!("invalid destination: {destination}"));
                assert_eq!(
                    application_actions_for(&new_keys),
                    *expected,
                    "{destination} must dispatch the prior effect of {before}"
                );
                assert_eq!(
                    application_actions_for(&old_keys),
                    Vec::<LcAction>::new(),
                    "migrated source {before} must not dispatch an application action"
                );
            } else if *before == "gg" {
                assert_eq!(
                    default_vim_request_for(before),
                    Some(ActionRequest::plain(ActionId::JumpTop)),
                    "{before} stays the vim jump-top motion: {rationale}"
                );
            } else {
                assert_eq!(
                    application_actions_for(&old_keys),
                    *expected,
                    "retained {before} must keep its effect: {rationale}"
                );
            }
        }

        assert_eq!(
            destinations.len(),
            12,
            "all planned modal rows migrate once"
        );
    }

    /// Parse a pinned Normal sequence, including the non-character keys the
    /// hand-installed mappings used (`<BS>`, `<F2>`, `<F3>`, `<C-m>`).
    fn pinned_sequence_events(sequence: &str) -> Vec<KeyEvent> {
        match sequence {
            "<BS>" => vec![KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE)],
            "<F2>" => vec![KeyEvent::new(KeyCode::F(2), KeyModifiers::NONE)],
            "<F3>" => vec![KeyEvent::new(KeyCode::F(3), KeyModifiers::NONE)],
            "<C-m>" => vec![KeyEvent::new(KeyCode::Char('m'), KeyModifiers::CONTROL)],
            other => normal_sequence_keys(other)
                .unwrap_or_else(|| panic!("invalid pinned sequence: {other}"))
                .into_iter()
                .map(|code| KeyEvent::new(code, KeyModifiers::NONE))
                .collect(),
        }
    }

    fn all_actions_for_events(keys: &[KeyEvent]) -> Vec<Action<LcInfo>> {
        let mut km = build_key_manager();
        for key in keys {
            km.input_key(TerminalKey::from(*key));
        }
        let mut actions = Vec::new();
        while let Some((action, _ctx)) = km.pop() {
            // modalkit appends an undo-history checkpoint after each completed
            // Normal sequence; it is bookkeeping, not a key effect.
            if !matches!(
                action,
                Action::Editor(editor_types::EditorAction::History(
                    editor_types::HistoryAction::Checkpoint
                ))
            ) {
                actions.push(action);
            }
        }
        actions
    }

    /// Epic M slice (a) pre-migration pin. Records the observable effect of
    /// every Normal-mode mapping that `build_vim_machine` installed by hand
    /// before the registry migration (the 92 `add_mapping` call sites, with
    /// the three loops expanded). It must stay green after the migration: the
    /// registry-sourced keymap has to reproduce each row exactly.
    #[test]
    #[allow(clippy::too_many_lines)] // One auditable row per pinned mapping.
    fn pre_migration_hand_mapping_effects_are_preserved() {
        use InsertStyle::*;
        use LcAction::*;

        enum Pin {
            /// The sequence dispatches exactly this application action.
            Dispatch(LcAction),
            /// The sequence is an explicit inert guard (`Action::NoOp`).
            Inert,
            /// The sequence is a reserved prefix that emits nothing on its own.
            Prefix,
            /// The hand mapping never fires: a shorter registry binding
            /// completes first and dispatches this action instead. Recorded
            /// as observed so the migration preserves it exactly.
            Shadowed(LcAction),
        }
        use Pin::*;

        let mut cases: Vec<(String, Pin)> = [
            ("n", Dispatch(NextSearchMatch)),
            ("N", Dispatch(PrevSearchMatch)),
            ("R", Dispatch(RotateSession)),
            // `r` (registry `Refresh`) completes before the second key, so
            // the hand-installed `rc`/`rm` mappings were unreachable.
            ("rc", Shadowed(RefreshNavigation)),
            ("rm", Shadowed(RefreshNavigation)),
            ("]a", Dispatch(NextAttention)),
            ("[a", Dispatch(PrevAttention)),
            ("]g", Dispatch(NextLabelBoundary)),
            ("[g", Dispatch(PrevLabelBoundary)),
            ("]u", Dispatch(NextUserMessage)),
            ("[u", Dispatch(PrevUserMessage)),
            ("gs", Dispatch(GoToSessionsZone)),
            ("<BS>", Dispatch(AscendOrBack)),
            ("ZQ", Dispatch(Quit)),
            ("ZZ", Dispatch(Quit)),
            ("<C-m>", Dispatch(ToggleModelDropdown)),
            ("D", Prefix),
            ("DD", Dispatch(DeleteSession)),
            ("<Space>s", Dispatch(OpenSortPicker)),
            ("<Space>C", Dispatch(ReassignSessionProject)),
            ("i", Dispatch(EnterInputBarInsert(Insert))),
            ("a", Dispatch(EnterInputBarInsert(Append))),
            ("o", Dispatch(EnterInputBarInsert(OpenBelow))),
            ("O", Dispatch(EnterInputBarInsert(OpenAbove))),
            ("e", Dispatch(EnterSessionNormalMode)),
            ("zo", Dispatch(OpenFold)),
            ("zc", Dispatch(CloseFold)),
            ("za", Dispatch(ToggleFold)),
            ("zM", Dispatch(CloseAllFolds)),
            ("zR", Dispatch(OpenAllFolds)),
            ("zs", Dispatch(ToggleSystemEvents)),
            ("zt", Dispatch(ToggleThinkingEvents)),
            ("y", Prefix),
            ("yy", Dispatch(YankEventContent)),
            ("<Space>p", Dispatch(OpenProjectPicker)),
            ("<Space>gg", Dispatch(ToggleGitPanel)),
            ("p", Dispatch(TogglePromptPreview)),
            ("ga", Dispatch(GoToArchiveZone)),
            ("<Space>gX", Dispatch(GoToTrash)),
            ("gt", Dispatch(GoToTaskRabbitZone)),
            ("gj", Dispatch(GoToJobsZone)),
            ("<Space>gq", Dispatch(OpenQuestionModal)),
            ("gr", Dispatch(OpenRecentCompletions)),
            ("<Space>x", Dispatch(ExecuteDocRegBlocks)),
            ("<Space>X", Dispatch(CommitAndPush)),
            ("l", Dispatch(NavigateRight)),
            ("H", Dispatch(AscendContainer)),
            ("L", Dispatch(EnterSession)),
            ("<", Dispatch(PrevTab)),
            (">", Dispatch(NextTab)),
            ("<Space>", Prefix),
            ("<Space>gr", Inert),
            ("<Space>R", Inert),
            ("gm", Inert),
            ("g?", Inert),
            ("<Space>M", Dispatch(OpenMemorySearch)),
            ("<Space>K", Dispatch(OpenScheduleBrowser)),
            ("<Space>o", Dispatch(TaskRabbitPrompt)),
            ("<Space>m", Dispatch(BlankPrompt)),
            ("<Space>r", Dispatch(ToggleRotationDisabled)),
            ("<Space>k", Dispatch(CancelRetry)),
            ("<Space>S", Dispatch(EmergencyStopAll)),
            ("<Space>T", Dispatch(OpenSessionInNewTab)),
            ("<Space>q", Dispatch(CloseFocusedPane)),
            ("<Space>,", Dispatch(OpenSettings)),
            ("<Space>n", Dispatch(ToggleNotifications)),
            ("<Space>e", Dispatch(ToggleFileExplorer)),
            ("<Space>gc", Dispatch(OpenEspSquare)),
            ("<Space> ", Dispatch(OpenTelescope)),
            ("<Space>;", Dispatch(OpenCommandPalette)),
            ("<Space>v", Dispatch(OpenGraphReview)),
            ("gL", Dispatch(SetEpicLead)),
            ("gR", Dispatch(RunEpicTopology)),
            ("<Space>gP", Dispatch(OpenPromptCreator)),
            ("<Space>G", Dispatch(CreateGroup)),
            ("<Space>E", Dispatch(CreateEpic)),
            ("<Space>gS", Dispatch(CreateStory)),
            ("<Space>gT", Dispatch(CreateTask)),
            ("<Space>B", Dispatch(CreateBug)),
            ("gX", Inert),
            ("gq", Inert),
            ("gn", Inert),
            ("gc", Inert),
            ("gv", Inert),
            ("gp", Inert),
            ("gK", Inert),
            ("gG", Inert),
            ("gE", Inert),
            ("gS", Inert),
            ("gT", Inert),
            ("gB", Inert),
            ("mp", Dispatch(MoveToParent)),
            ("mo", Dispatch(MoveToRoot)),
            ("-", Dispatch(AscendContainer)),
            ("<F2>", Dispatch(RenameSession)),
            ("<F3>", Dispatch(OpenSessionInfoPanel)),
            ("gf", Prefix),
        ]
        .into_iter()
        .map(|(sequence, pin)| (sequence.to_string(), pin))
        .collect();
        for n in 1u8..=9 {
            cases.push((format!("gf{n}"), Dispatch(OpenRecentFileN(n))));
            cases.push((format!("<Space>{n}"), Dispatch(JumpAttentionN(n))));
        }

        // The four retired mini-DAG chords are excluded from the old 92-call
        // inventory; the three loops expand to 12 + 9 + 9 mappings.
        assert_eq!(cases.len(), 92 - 3 + 12 + 9 + 9 - 4);

        for (sequence, pin) in &cases {
            let actions = all_actions_for_events(&pinned_sequence_events(sequence));
            let expected: Vec<Action<LcInfo>> = match pin {
                Dispatch(action) | Shadowed(action) => vec![Action::Application(action.clone())],
                Inert => vec![Action::NoOp],
                Prefix => Vec::new(),
            };
            assert_eq!(
                actions, expected,
                "pre-migration Normal effect changed for {sequence:?}"
            );
        }
    }

    /// T5: the Normal keymap is registry-sourced with no handwritten mappings.
    #[test]
    fn normal_keymap_is_registry_sourced() {
        let source = include_str!("keybindings.rs");
        let production = source
            .split("#[cfg(test)]\nmod tests {")
            .next()
            .expect("production code");
        let installer_start = production
            .find("fn install_registered_normal_bindings")
            .expect("installer present");
        let installer_end = installer_start
            + production[installer_start..]
                .find("\n}\n")
                .expect("installer ends");
        let mut outside = BTreeSet::new();
        for (offset, _) in production.match_indices("add_mapping(") {
            if (installer_start..installer_end).contains(&offset) {
                continue;
            }
            let window = &production[offset..(offset + 240).min(production.len())];
            let action = window
                .split("LcAction::")
                .nth(1)
                .and_then(|rest| rest.split(')').next())
                .unwrap_or("<no LcAction>");
            outside.insert(action.to_string());
        }
        assert!(
            outside.is_empty(),
            "unexpected handwritten mappings: {outside:?}"
        );
        assert_eq!(
            production.matches("add_mapping(").count(),
            2,
            "only the installer setup and registry bindings may call add_mapping"
        );
    }

    #[test]
    fn mini_dag_sequences_emit_no_application_action() {
        for sequence in ["]d", "[d", "]D", "[D"] {
            let actions = all_actions_for_events(&pinned_sequence_events(sequence));
            assert!(
                actions
                    .iter()
                    .all(|action| !matches!(action, Action::Application(_))),
                "retired sequence {sequence:?} emitted an application action: {actions:?}"
            );
        }
    }

    /// T6: every registry Normal binding drives `build_key_manager()` to its
    /// expected effect, and the extracted (sequence, route, effect) triples
    /// equal the Normal rows of the manual model.
    #[test]
    fn normal_keymap_triples_match_manual() {
        fn effect_label(id: ActionId, effect: &RegisteredNormalEffect) -> String {
            match effect {
                RegisteredNormalEffect::Application(action) => format!("{action:?}"),
                RegisteredNormalEffect::DefaultVimMotion => format!("vim-motion:{id:?}"),
            }
        }

        let mut extracted = BTreeSet::new();
        for descriptor in ACTION_DESCRIPTORS {
            for binding in descriptor
                .bindings
                .iter()
                .filter(|binding| binding.route == ActionRoute::Normal)
            {
                // The text-entry help chord is intercepted by the event loop
                // (key_tables GLOBAL_KEY_INTERCEPTS), not the Vim machine.
                if binding.sequence == "Ctrl-Alt-G" {
                    continue;
                }
                let effect = registered_normal_effect(descriptor.id, binding.sequence)
                    .expect("registered effect");
                let events = normal_sequence_events(binding.sequence).expect("parseable");
                let actions = all_actions_for_events(&events);
                match &effect {
                    RegisteredNormalEffect::Application(action) => assert_eq!(
                        actions,
                        vec![Action::Application(action.clone())],
                        "{} yields its registered action",
                        binding.sequence
                    ),
                    RegisteredNormalEffect::DefaultVimMotion => assert_eq!(
                        actions
                            .iter()
                            .filter_map(request_for_normal_machine_action)
                            .collect::<Vec<_>>(),
                        vec![ActionRequest::plain(descriptor.id)],
                        "{} keeps its Vim motion",
                        binding.sequence
                    ),
                }
                extracted.insert((
                    binding.sequence.to_string(),
                    ActionRoute::Normal,
                    effect_label(descriptor.id, &effect),
                ));
            }
        }

        let manual = crate::manual::model::build_manual();
        let from_manual: BTreeSet<(String, ActionRoute, String)> = manual
            .normal_bindings
            .iter()
            .filter(|(sequence, _)| *sequence != "Ctrl-Alt-G")
            .map(|(sequence, id)| {
                let effect = registered_normal_effect(*id, sequence).expect("registered effect");
                (
                    sequence.to_string(),
                    ActionRoute::Normal,
                    effect_label(*id, &effect),
                )
            })
            .collect();
        assert_eq!(from_manual, extracted);
    }

    #[test]
    fn test_build_vim_machine_succeeds() {
        let _machine = build_vim_machine();
    }

    #[test]
    fn test_build_key_manager_succeeds() {
        let _km = build_key_manager();
    }

    #[test]
    fn test_edge_helpers() {
        let single = normal_binding_edge("n").expect("single key");
        assert_eq!(single.len(), 1);

        let modified = normal_binding_edge("Ctrl-M").expect("named modified key");
        assert_eq!(modified.len(), 1);

        let double = edge2(KeyCode::Char(']'), KeyCode::Char('a'));
        assert_eq!(double.len(), 2);
    }

    #[test]
    fn model_dropdown_uses_ctrl_m_without_claiming_plain_m() {
        assert_eq!(
            application_actions_for_events(&[KeyEvent::new(
                KeyCode::Char('m'),
                KeyModifiers::CONTROL,
            )]),
            vec![LcAction::ToggleModelDropdown]
        );
        assert!(application_actions_for(&[KeyCode::Char('M')]).is_empty());
    }

    #[test]
    fn registered_normal_sequences_resolve_to_their_registry_request_and_effect_family() {
        let mut bindings = BTreeSet::new();
        for descriptor in ACTION_DESCRIPTORS {
            for binding in descriptor
                .bindings
                .iter()
                .filter(|binding| binding.route == ActionRoute::Normal)
            {
                if descriptor.id == ActionId::ContextHelp && binding.sequence == "Ctrl-Alt-G" {
                    // The text-entry help chord is intercepted by the event loop
                    // before the Vim key manager; `event::tests` verifies its
                    // dispatch and draft-preserving behavior.
                    continue;
                }
                assert!(
                    bindings.insert((binding.route, binding.sequence)),
                    "duplicate registered route/sequence: {:?} {}",
                    binding.route,
                    binding.sequence
                );
                let effect = registered_normal_effect(descriptor.id, binding.sequence)
                    .unwrap_or_else(|| panic!("missing effect family for {:?}", descriptor.id));
                let events = normal_sequence_events(binding.sequence)
                    .unwrap_or_else(|| panic!("unparseable sequence {}", binding.sequence));
                let requests = all_actions_for_events(&events)
                    .iter()
                    .filter_map(request_for_normal_machine_action)
                    .collect::<Vec<_>>();
                let dispatch_only = match &effect {
                    RegisteredNormalEffect::Application(action) => {
                        crate::action_registry::request_from_lc_action(action).is_none()
                    }
                    RegisteredNormalEffect::DefaultVimMotion => false,
                };
                if descriptor.id == ActionId::CopySessionUuid {
                    // Focus decides yy at the existing YankEventContent handler;
                    // request_from_lc_action stays deliberately unchanged so
                    // transcript event yanks cannot be preempted by list policy.
                    assert!(
                        requests.is_empty(),
                        "contextual yy must bypass context-free request mapping"
                    );
                } else if dispatch_only {
                    // Migrated chords keep their LcAction handler as the sole
                    // authority (design A.1.3); the machine must emit exactly
                    // the registered effect.
                    let RegisteredNormalEffect::Application(action) = effect else {
                        unreachable!("dispatch-only effects are applications")
                    };
                    assert_eq!(
                        all_actions_for_events(&events),
                        vec![Action::Application(action)],
                        "machine effect diverged for {}",
                        binding.sequence
                    );
                } else {
                    assert_eq!(
                        requests,
                        vec![ActionRequest::plain(descriptor.id)],
                        "machine effect diverged for {}",
                        binding.sequence
                    );
                }
            }
        }
    }

    #[test]
    fn active_hierarchy_keys_match_v1_docs_contract() {
        assert_eq!(
            application_actions_for(&[KeyCode::Char(' '), KeyCode::Char('g'), KeyCode::Char('T')]),
            vec![LcAction::CreateTask]
        );
        assert_eq!(
            application_actions_for(&[KeyCode::Backspace]),
            vec![LcAction::AscendOrBack]
        );
        assert_eq!(
            application_actions_for(&[KeyCode::Char('D'), KeyCode::Char('D')]),
            vec![LcAction::DeleteSession]
        );
    }

    #[test]
    fn yazi_container_nav_keys_match_v1_docs_contract() {
        // T4: H/L relocated to yazi-style container nav; PrevTab/NextTab
        // relocated to </>.
        assert_eq!(
            application_actions_for(&[KeyCode::Char('H')]),
            vec![LcAction::AscendContainer]
        );
        assert_eq!(
            application_actions_for(&[KeyCode::Char('L')]),
            vec![LcAction::EnterSession]
        );
        assert_eq!(
            application_actions_for(&[KeyCode::Char('<')]),
            vec![LcAction::PrevTab]
        );
        assert_eq!(
            application_actions_for(&[KeyCode::Char('>')]),
            vec![LcAction::NextTab]
        );
    }

    #[test]
    fn quarantined_normal_mode_overlay_keys_are_unbound() {
        // ST-COCKPIT-OVERLAYS reconnected `Space M`/`Space K`; only the genuinely
        // quarantined chords (command-mode reachable only) remain unbound here.
        let retired: &[&[KeyCode]] = &[
            &[KeyCode::Char(' '), KeyCode::Char('g'), KeyCode::Char('r')],
            &[KeyCode::Char(' '), KeyCode::Char('R')],
            &[KeyCode::Char('g'), KeyCode::Char('m')],
            &[KeyCode::Char('g'), KeyCode::Char('?')],
        ];

        for keys in retired {
            assert_eq!(
                application_actions_for(keys),
                Vec::<LcAction>::new(),
                "retired key sequence should not dispatch an application action: {keys:?}"
            );
        }
    }

    #[test]
    fn reconnected_overlay_keys_dispatch() {
        // ST-COCKPIT-OVERLAYS (S7): Memory Search and Schedule Browser are
        // reachable through their current Space leader chords.
        assert_eq!(
            application_actions_for(&[KeyCode::Char(' '), KeyCode::Char('M')]),
            vec![LcAction::OpenMemorySearch]
        );
        assert_eq!(
            application_actions_for(&[KeyCode::Char(' '), KeyCode::Char('K')]),
            vec![LcAction::OpenScheduleBrowser]
        );
    }

    #[test]
    fn manager_chords_dispatch_the_manager_actions() {
        // RSI #415: the <Space>g manager subnamespace binds the exact LcAction
        // variants the `:manager policy` / `:manager board` / `:manager decisions`
        // commands dispatch — no duplicated command behavior.
        assert_eq!(
            application_actions_for(&[KeyCode::Char(' '), KeyCode::Char('g'), KeyCode::Char('p'),]),
            vec![LcAction::EditHarnessManagerPolicy]
        );
        assert_eq!(
            application_actions_for(&[KeyCode::Char(' '), KeyCode::Char('g'), KeyCode::Char('b'),]),
            vec![LcAction::OpenHarnessManagerBoard]
        );
        assert_eq!(
            application_actions_for(&[KeyCode::Char(' '), KeyCode::Char('g'), KeyCode::Char('d'),]),
            vec![LcAction::OpenHarnessManagerDecisions]
        );
    }

    #[test]
    fn manager_chords_do_not_collide_with_existing_space_chords() {
        // The three-key manager chords must not shadow existing Space muscle
        // memory, the pre-existing <Space>gg Git panel chord, or the retired
        // <Space>gr sequence. All pre-existing chords keep their mappings.
        assert_eq!(
            application_actions_for(&[KeyCode::Char(' '), KeyCode::Char('p')]),
            vec![LcAction::OpenProjectPicker]
        );
        assert_eq!(
            application_actions_for(&[KeyCode::Char(' '), KeyCode::Char('g'), KeyCode::Char('g'),]),
            vec![LcAction::ToggleGitPanel]
        );
        assert_eq!(
            application_actions_for(&[KeyCode::Char(' '), KeyCode::Char('g'), KeyCode::Char('r'),]),
            Vec::<LcAction>::new()
        );
        // The <Space> and <Space>g prefix nodes stay unbound rather than
        // dispatching a partial action.
        assert_eq!(
            application_actions_for(&[KeyCode::Char(' ')]),
            Vec::<LcAction>::new()
        );
        assert_eq!(
            application_actions_for(&[KeyCode::Char(' '), KeyCode::Char('g')]),
            Vec::<LcAction>::new()
        );
    }
}
