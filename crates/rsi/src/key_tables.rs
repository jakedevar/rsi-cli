//! Raw-key dispatch tables for the keys `event.rs` decodes itself.
//!
//! Every chord the event loop matches before (or beside) the modalkit Vim
//! keymap lives here as a [`KeyEntry`]: a chord, a closed-enum guard, a
//! closed-enum effect and an operator-facing label. The decoders in
//! `event.rs` iterate these tables in order (first match wins, exactly like
//! the `else if` chains they replaced) and run the effect through one
//! exhaustive `match`. The operator manual renders its raw-key reference from
//! the same tables ([`KEY_TABLES`]), so documentation cannot drift from
//! dispatch. `key_tables::tests::event_rs_has_no_untabled_chords` keeps
//! `event.rs` free of stray `KeyCode` literals.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

/// Which key codes a chord accepts.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KeyMatch {
    /// Exactly one key code.
    Code(KeyCode),
    /// Any of these key codes (terminal encodings of one physical chord).
    AnyOf(&'static [KeyCode]),
    /// Any printable character.
    AnyChar,
}

/// How a chord's modifiers are compared with the pressed key's modifiers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ModMatch {
    /// The modifiers must be exactly these.
    Exact(KeyModifiers),
    /// The modifiers must include these (others may also be held).
    Contains(KeyModifiers),
    /// Modifiers are ignored.
    Any,
}

/// A key chord plus its display form (registry style: `Ctrl-C`, `Shift-Up`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Chord {
    pub key: KeyMatch,
    pub mods: ModMatch,
    pub display: &'static str,
}

impl Chord {
    #[must_use]
    pub fn matches(&self, key: KeyEvent) -> bool {
        let code_matches = match self.key {
            KeyMatch::Code(code) => key.code == code,
            KeyMatch::AnyOf(codes) => codes.contains(&key.code),
            KeyMatch::AnyChar => matches!(key.code, KeyCode::Char(_)),
        };
        let mods_match = match self.mods {
            ModMatch::Exact(mods) => key.modifiers == mods,
            ModMatch::Contains(mods) => key.modifiers.contains(mods),
            ModMatch::Any => true,
        };
        code_matches && mods_match
    }

    /// The typed character for an [`KeyMatch::AnyChar`] chord.
    #[must_use]
    pub const fn typed_char(key: KeyEvent) -> Option<char> {
        match key.code {
            KeyCode::Char(c) => Some(c),
            _ => None,
        }
    }
}

/// One table row: chord, guard (context), effect and operator label.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct KeyEntry<G: 'static, E: 'static> {
    pub chord: Chord,
    pub guard: G,
    pub effect: E,
    pub label: &'static str,
}

/// Closed guard enums expose an operator-facing context label.
pub trait GuardLabel: Copy {
    fn label(self) -> &'static str;
}

/// First entry whose chord matches `key` and whose guard holds.
pub fn lookup<G: GuardLabel, E: Copy>(
    table: &'static [KeyEntry<G, E>],
    key: KeyEvent,
    mut holds: impl FnMut(G) -> bool,
) -> Option<&'static KeyEntry<G, E>> {
    table
        .iter()
        .find(|entry| entry.chord.matches(key) && holds(entry.guard))
}

const NONE: KeyModifiers = KeyModifiers::NONE;
const CTRL: KeyModifiers = KeyModifiers::CONTROL;
const SHIFT: KeyModifiers = KeyModifiers::SHIFT;
const CTRL_SHIFT: KeyModifiers = KeyModifiers::CONTROL.union(KeyModifiers::SHIFT);
const CTRL_ALT: KeyModifiers = KeyModifiers::CONTROL.union(KeyModifiers::ALT);

const fn chord(key: KeyMatch, mods: ModMatch, display: &'static str) -> Chord {
    Chord { key, mods, display }
}

const fn code(code: KeyCode, mods: ModMatch, display: &'static str) -> Chord {
    chord(KeyMatch::Code(code), mods, display)
}

/// The key fed into modalkit to leave its internal command-line mode after
/// the TUI's own command/search editor finishes. It is synthesized, not
/// pressed, so it is not a table row.
pub const MODALKIT_RESET_KEY: KeyCode = KeyCode::Esc;

// ---------------------------------------------------------------------------
// Clipboard paste (runs before the press-only filter)
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PasteGuard {
    /// Press or release of the chord (some terminals deliver paste as release).
    PressOrRelease,
}

impl GuardLabel for PasteGuard {
    fn label(self) -> &'static str {
        match self {
            Self::PressOrRelease => "any context (press or release)",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PasteEffect {
    PasteClipboard,
}

pub static PASTE_KEYS: &[KeyEntry<PasteGuard, PasteEffect>] = &[KeyEntry {
    chord: chord(
        KeyMatch::AnyOf(&[KeyCode::Char('v'), KeyCode::Char('V')]),
        ModMatch::Contains(CTRL),
        "Ctrl-V",
    ),
    guard: PasteGuard::PressOrRelease,
    effect: PasteEffect::PasteClipboard,
    label: "paste the clipboard into the active overlay or input bar (text or image)",
}];

// ---------------------------------------------------------------------------
// Submissions refused until the daemon configuration is authoritative
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConfigGateGuard {
    /// Quick-launch input line (`InputMode::Input` for a new session).
    QuickLaunchInput,
    /// A launch prompt overlay (not a continue prompt).
    LaunchPrompt,
    /// Create-entity form with its model dropdown closed.
    CreateForm,
    /// Create-entity form, dropdown closed, not inserting, on a one-line field.
    CreateFormNavigating,
}

impl GuardLabel for ConfigGateGuard {
    fn label(self) -> &'static str {
        match self {
            Self::QuickLaunchInput => "quick-launch input, daemon config pending",
            Self::LaunchPrompt => "launch prompt, daemon config pending",
            Self::CreateForm => "create form, daemon config pending",
            Self::CreateFormNavigating => {
                "create form (normal mode, one-line field), daemon config pending"
            }
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConfigGateEffect {
    RefuseUntilConfigReady,
}

const REFUSE_LABEL: &str =
    "refused with a notice until the daemon config is authoritative; the draft is kept";

pub static CONFIG_GATED_SUBMIT_KEYS: &[KeyEntry<ConfigGateGuard, ConfigGateEffect>] = &[
    KeyEntry {
        chord: code(KeyCode::Enter, ModMatch::Exact(NONE), "Enter"),
        guard: ConfigGateGuard::QuickLaunchInput,
        effect: ConfigGateEffect::RefuseUntilConfigReady,
        label: REFUSE_LABEL,
    },
    KeyEntry {
        chord: code(KeyCode::Enter, ModMatch::Contains(CTRL), "Ctrl-Enter"),
        guard: ConfigGateGuard::LaunchPrompt,
        effect: ConfigGateEffect::RefuseUntilConfigReady,
        label: REFUSE_LABEL,
    },
    KeyEntry {
        chord: code(KeyCode::Char('t'), ModMatch::Contains(CTRL), "Ctrl-T"),
        guard: ConfigGateGuard::LaunchPrompt,
        effect: ConfigGateEffect::RefuseUntilConfigReady,
        label: REFUSE_LABEL,
    },
    KeyEntry {
        chord: code(KeyCode::Char('s'), ModMatch::Contains(CTRL), "Ctrl-S"),
        guard: ConfigGateGuard::LaunchPrompt,
        effect: ConfigGateEffect::RefuseUntilConfigReady,
        label: REFUSE_LABEL,
    },
    KeyEntry {
        chord: code(KeyCode::Enter, ModMatch::Contains(CTRL), "Ctrl-Enter"),
        guard: ConfigGateGuard::CreateForm,
        effect: ConfigGateEffect::RefuseUntilConfigReady,
        label: REFUSE_LABEL,
    },
    KeyEntry {
        chord: code(KeyCode::Enter, ModMatch::Exact(NONE), "Enter"),
        guard: ConfigGateGuard::CreateFormNavigating,
        effect: ConfigGateEffect::RefuseUntilConfigReady,
        label: REFUSE_LABEL,
    },
];

// ---------------------------------------------------------------------------
// Global intercepts (ahead of overlays, input surfaces and the Vim keymap)
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GlobalGuard {
    Always,
    /// The embedded terminal overlay is open.
    TerminalOverlay,
    /// Normal mode, no overlay, input bar not inserting.
    Unobstructed,
    /// `Unobstructed` with the session list focused.
    UnobstructedSessionList,
    /// `Unobstructed` with any pane other than the session list focused.
    UnobstructedOtherPane,
    /// `UnobstructedOtherPane`, except an Issues editor showing a stale-version
    /// conflict (which keeps Ctrl-L for its own reload).
    UnobstructedOtherPaneNoStaleIssueEditor,
    /// An overlay with movable geometry (launch prompt, input modal) is open.
    GeometryOverlay,
    /// Normal mode and no overlay (the input bar may be inserting).
    NormalNoOverlay,
}

impl GuardLabel for GlobalGuard {
    fn label(self) -> &'static str {
        match self {
            Self::Always => "any context",
            Self::TerminalOverlay => "embedded terminal open",
            Self::Unobstructed => "normal mode, no overlay, not inserting",
            Self::UnobstructedSessionList => "session list focused (normal mode, no overlay)",
            Self::UnobstructedOtherPane => "other pane focused (normal mode, no overlay)",
            Self::UnobstructedOtherPaneNoStaleIssueEditor => {
                "other pane focused (normal mode, no overlay; not a stale Issues editor)"
            }
            Self::GeometryOverlay => "launch prompt or input modal open",
            Self::NormalNoOverlay => "normal mode, no overlay",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GlobalEffect {
    ToggleHelp,
    InterruptTerminal,
    Quit,
    ToggleTerminal,
    JumpBack,
    JumpForward,
    SessionListZonePrev,
    SessionListZoneNext,
    FocusLeft,
    FocusRight,
    /// Resize the overlay by (width, height) cells.
    ResizeOverlay(i16, i16),
    /// Move the overlay by (x, y) cells.
    MoveOverlay(i16, i16),
    ResetOverlayGeometry,
    GrowSidebar,
    ShrinkSidebar,
    NextEvent,
    PrevEvent,
}

/// Overlay geometry step in cells.
const STEP: i16 = 2;

pub static GLOBAL_KEY_INTERCEPTS: &[KeyEntry<GlobalGuard, GlobalEffect>] = &[
    KeyEntry {
        chord: code(KeyCode::Char('g'), ModMatch::Exact(CTRL_ALT), "Ctrl-Alt-G"),
        guard: GlobalGuard::Always,
        effect: GlobalEffect::ToggleHelp,
        label: "toggle contextual help without typing into the active editor",
    },
    KeyEntry {
        chord: code(KeyCode::Char('c'), ModMatch::Contains(CTRL), "Ctrl-C"),
        guard: GlobalGuard::TerminalOverlay,
        effect: GlobalEffect::InterruptTerminal,
        label: "send SIGINT to the embedded terminal's shell",
    },
    KeyEntry {
        chord: code(KeyCode::Char('c'), ModMatch::Contains(CTRL), "Ctrl-C"),
        guard: GlobalGuard::Always,
        effect: GlobalEffect::Quit,
        label: "quit rsi",
    },
    KeyEntry {
        chord: code(KeyCode::Char('\\'), ModMatch::Contains(CTRL), "Ctrl-\\"),
        guard: GlobalGuard::Always,
        effect: GlobalEffect::ToggleTerminal,
        label: "toggle the embedded terminal overlay (the shell keeps running)",
    },
    KeyEntry {
        chord: code(KeyCode::Char('o'), ModMatch::Contains(CTRL), "Ctrl-O"),
        guard: GlobalGuard::Unobstructed,
        effect: GlobalEffect::JumpBack,
        label: "jump back in the session jumplist",
    },
    KeyEntry {
        chord: code(KeyCode::Char('i'), ModMatch::Contains(CTRL), "Ctrl-I"),
        guard: GlobalGuard::Unobstructed,
        effect: GlobalEffect::JumpForward,
        label: "jump forward in the session jumplist",
    },
    KeyEntry {
        chord: code(KeyCode::Char('h'), ModMatch::Contains(CTRL), "Ctrl-H"),
        guard: GlobalGuard::UnobstructedSessionList,
        effect: GlobalEffect::SessionListZonePrev,
        label: "previous session-list zone (Main ← TaskRabbit ← Jobs ← Archive, wrapping)",
    },
    KeyEntry {
        chord: code(KeyCode::Char('h'), ModMatch::Contains(CTRL), "Ctrl-H"),
        guard: GlobalGuard::UnobstructedOtherPane,
        effect: GlobalEffect::FocusLeft,
        label: "focus the pane to the left",
    },
    KeyEntry {
        chord: code(KeyCode::Char('l'), ModMatch::Contains(CTRL), "Ctrl-L"),
        guard: GlobalGuard::UnobstructedSessionList,
        effect: GlobalEffect::SessionListZoneNext,
        label: "next session-list zone (Main → TaskRabbit → Jobs → Archive, wrapping)",
    },
    KeyEntry {
        chord: code(KeyCode::Char('l'), ModMatch::Contains(CTRL), "Ctrl-L"),
        guard: GlobalGuard::UnobstructedOtherPaneNoStaleIssueEditor,
        effect: GlobalEffect::FocusRight,
        label: "focus the pane to the right",
    },
    KeyEntry {
        chord: code(KeyCode::Up, ModMatch::Contains(CTRL_SHIFT), "Ctrl-Shift-Up"),
        guard: GlobalGuard::GeometryOverlay,
        effect: GlobalEffect::ResizeOverlay(0, -STEP),
        label: "make the overlay shorter",
    },
    KeyEntry {
        chord: code(
            KeyCode::Down,
            ModMatch::Contains(CTRL_SHIFT),
            "Ctrl-Shift-Down",
        ),
        guard: GlobalGuard::GeometryOverlay,
        effect: GlobalEffect::ResizeOverlay(0, STEP),
        label: "make the overlay taller",
    },
    KeyEntry {
        chord: code(
            KeyCode::Left,
            ModMatch::Contains(CTRL_SHIFT),
            "Ctrl-Shift-Left",
        ),
        guard: GlobalGuard::GeometryOverlay,
        effect: GlobalEffect::ResizeOverlay(-STEP, 0),
        label: "make the overlay narrower",
    },
    KeyEntry {
        chord: code(
            KeyCode::Right,
            ModMatch::Contains(CTRL_SHIFT),
            "Ctrl-Shift-Right",
        ),
        guard: GlobalGuard::GeometryOverlay,
        effect: GlobalEffect::ResizeOverlay(STEP, 0),
        label: "make the overlay wider",
    },
    KeyEntry {
        chord: code(KeyCode::Up, ModMatch::Exact(CTRL), "Ctrl-Up"),
        guard: GlobalGuard::GeometryOverlay,
        effect: GlobalEffect::MoveOverlay(0, -STEP),
        label: "move the overlay up",
    },
    KeyEntry {
        chord: code(KeyCode::Down, ModMatch::Exact(CTRL), "Ctrl-Down"),
        guard: GlobalGuard::GeometryOverlay,
        effect: GlobalEffect::MoveOverlay(0, STEP),
        label: "move the overlay down",
    },
    KeyEntry {
        chord: code(KeyCode::Left, ModMatch::Exact(CTRL), "Ctrl-Left"),
        guard: GlobalGuard::GeometryOverlay,
        effect: GlobalEffect::MoveOverlay(-STEP, 0),
        label: "move the overlay left",
    },
    KeyEntry {
        chord: code(KeyCode::Right, ModMatch::Exact(CTRL), "Ctrl-Right"),
        guard: GlobalGuard::GeometryOverlay,
        effect: GlobalEffect::MoveOverlay(STEP, 0),
        label: "move the overlay right",
    },
    KeyEntry {
        chord: code(KeyCode::Char('0'), ModMatch::Exact(CTRL), "Ctrl-0"),
        guard: GlobalGuard::GeometryOverlay,
        effect: GlobalEffect::ResetOverlayGeometry,
        label: "reset the overlay's size and position",
    },
    KeyEntry {
        chord: code(
            KeyCode::Right,
            ModMatch::Contains(CTRL_SHIFT),
            "Ctrl-Shift-Right",
        ),
        guard: GlobalGuard::NormalNoOverlay,
        effect: GlobalEffect::GrowSidebar,
        label: "widen the session-list sidebar",
    },
    KeyEntry {
        chord: code(
            KeyCode::Left,
            ModMatch::Contains(CTRL_SHIFT),
            "Ctrl-Shift-Left",
        ),
        guard: GlobalGuard::NormalNoOverlay,
        effect: GlobalEffect::ShrinkSidebar,
        label: "narrow the session-list sidebar",
    },
    KeyEntry {
        chord: code(KeyCode::Left, ModMatch::Exact(CTRL), "Ctrl-Left"),
        guard: GlobalGuard::Unobstructed,
        effect: GlobalEffect::FocusLeft,
        label: "focus the pane to the left (zones: gs / gt / gj / ga)",
    },
    KeyEntry {
        chord: code(KeyCode::Right, ModMatch::Exact(CTRL), "Ctrl-Right"),
        guard: GlobalGuard::Unobstructed,
        effect: GlobalEffect::FocusRight,
        label: "focus the pane to the right (zones: gs / gt / gj / ga)",
    },
    KeyEntry {
        chord: code(KeyCode::Down, ModMatch::Contains(SHIFT), "Shift-Down"),
        guard: GlobalGuard::Unobstructed,
        effect: GlobalEffect::NextEvent,
        label: "select the next transcript event",
    },
    KeyEntry {
        chord: code(KeyCode::Up, ModMatch::Contains(SHIFT), "Shift-Up"),
        guard: GlobalGuard::Unobstructed,
        effect: GlobalEffect::PrevEvent,
        label: "select the previous transcript event",
    },
];

// ---------------------------------------------------------------------------
// Session-list navigation from the list or a detail pane
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SessionListNavGuard {
    /// Session list or session detail focused, input bar not inserting.
    ListOrDetail,
}

impl GuardLabel for SessionListNavGuard {
    fn label(self) -> &'static str {
        match self {
            Self::ListOrDetail => "session list or detail focused, not inserting",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SessionListNavEffect {
    Prev,
    Next,
    PrevAndOpen,
    NextAndOpen,
}

pub static SESSION_LIST_NAV_KEYS: &[KeyEntry<SessionListNavGuard, SessionListNavEffect>] = &[
    KeyEntry {
        chord: code(KeyCode::Left, ModMatch::Exact(NONE), "Left"),
        guard: SessionListNavGuard::ListOrDetail,
        effect: SessionListNavEffect::Prev,
        label: "previous session in the list",
    },
    KeyEntry {
        chord: code(KeyCode::Right, ModMatch::Exact(NONE), "Right"),
        guard: SessionListNavGuard::ListOrDetail,
        effect: SessionListNavEffect::Next,
        label: "next session in the list",
    },
    KeyEntry {
        chord: code(KeyCode::Left, ModMatch::Exact(SHIFT), "Shift-Left"),
        guard: SessionListNavGuard::ListOrDetail,
        effect: SessionListNavEffect::PrevAndOpen,
        label: "previous session and open its detail",
    },
    KeyEntry {
        chord: code(KeyCode::Right, ModMatch::Exact(SHIFT), "Shift-Right"),
        guard: SessionListNavGuard::ListOrDetail,
        effect: SessionListNavEffect::NextAndOpen,
        label: "next session and open its detail",
    },
    KeyEntry {
        chord: code(KeyCode::Tab, ModMatch::Exact(CTRL), "Ctrl-Tab"),
        guard: SessionListNavGuard::ListOrDetail,
        effect: SessionListNavEffect::Prev,
        label: "previous session in the list",
    },
    KeyEntry {
        chord: chord(
            KeyMatch::AnyOf(&[KeyCode::BackTab, KeyCode::Tab]),
            ModMatch::Contains(CTRL_SHIFT),
            "Ctrl-Shift-Tab",
        ),
        guard: SessionListNavGuard::ListOrDetail,
        effect: SessionListNavEffect::Next,
        label: "next session in the list",
    },
];

// ---------------------------------------------------------------------------
// Session-detail scrolling
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DetailScrollGuard {
    DetailFocused,
}

impl GuardLabel for DetailScrollGuard {
    fn label(self) -> &'static str {
        match self {
            Self::DetailFocused => "session detail focused",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DetailScrollEffect {
    ScrollUp,
    ScrollDown,
}

pub static DETAIL_SCROLL_KEYS: &[KeyEntry<DetailScrollGuard, DetailScrollEffect>] = &[
    KeyEntry {
        chord: code(KeyCode::Up, ModMatch::Exact(NONE), "Up"),
        guard: DetailScrollGuard::DetailFocused,
        effect: DetailScrollEffect::ScrollUp,
        label: "scroll the transcript up 3 lines (selection stays visible)",
    },
    KeyEntry {
        chord: code(KeyCode::Down, ModMatch::Exact(NONE), "Down"),
        guard: DetailScrollGuard::DetailFocused,
        effect: DetailScrollEffect::ScrollDown,
        label: "scroll the transcript down 3 lines (selection stays visible)",
    },
];

// ---------------------------------------------------------------------------
// Normal-mode follow-ups that run after the Vim keymap
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NormalFollowUpGuard {
    /// A confirmed `/` search query is active.
    ConfirmedSearch,
}

impl GuardLabel for NormalFollowUpGuard {
    fn label(self) -> &'static str {
        match self {
            Self::ConfirmedSearch => "normal mode with a confirmed search",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NormalFollowUpEffect {
    ClearConfirmedSearch,
}

pub static NORMAL_FOLLOW_UP_KEYS: &[KeyEntry<NormalFollowUpGuard, NormalFollowUpEffect>] =
    &[KeyEntry {
        chord: code(KeyCode::Esc, ModMatch::Any, "Esc"),
        guard: NormalFollowUpGuard::ConfirmedSearch,
        effect: NormalFollowUpEffect::ClearConfirmedSearch,
        label: "clear the search query, matches and filter (after the Vim keymap runs)",
    }];

// ---------------------------------------------------------------------------
// The three one-line editors owned by event.rs
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LineEditor {
    /// Quick-launch / continue query line (`InputMode::Input`).
    QuickInput,
    /// `:` command line (`InputMode::Command`).
    Command,
    /// `/` search line (`InputMode::Search`).
    Search,
}

impl GuardLabel for LineEditor {
    fn label(self) -> &'static str {
        match self {
            Self::QuickInput => "quick-launch / continue input line",
            Self::Command => ": command line",
            Self::Search => "/ search line",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LineEditEffect {
    Submit,
    Cancel,
    DeleteBackward,
    InsertChar,
}

pub static LINE_EDIT_KEYS: &[KeyEntry<LineEditor, LineEditEffect>] = &[
    KeyEntry {
        chord: code(KeyCode::Enter, ModMatch::Any, "Enter"),
        guard: LineEditor::QuickInput,
        effect: LineEditEffect::Submit,
        label: "launch a new session (or continue the target) with the typed query",
    },
    KeyEntry {
        chord: code(KeyCode::Esc, ModMatch::Any, "Esc"),
        guard: LineEditor::QuickInput,
        effect: LineEditEffect::Cancel,
        label: "discard the query and return to normal mode (kept while a launch is pending)",
    },
    KeyEntry {
        chord: code(KeyCode::Backspace, ModMatch::Any, "Backspace"),
        guard: LineEditor::QuickInput,
        effect: LineEditEffect::DeleteBackward,
        label: "delete the last character",
    },
    KeyEntry {
        chord: chord(KeyMatch::AnyChar, ModMatch::Any, "<char>"),
        guard: LineEditor::QuickInput,
        effect: LineEditEffect::InsertChar,
        label: "append the typed character",
    },
    KeyEntry {
        chord: code(KeyCode::Enter, ModMatch::Any, "Enter"),
        guard: LineEditor::Command,
        effect: LineEditEffect::Submit,
        label: "run the ex command and return to normal mode",
    },
    KeyEntry {
        chord: code(KeyCode::Esc, ModMatch::Any, "Esc"),
        guard: LineEditor::Command,
        effect: LineEditEffect::Cancel,
        label: "discard the command and return to normal mode",
    },
    KeyEntry {
        chord: code(KeyCode::Backspace, ModMatch::Any, "Backspace"),
        guard: LineEditor::Command,
        effect: LineEditEffect::DeleteBackward,
        label: "delete the last character",
    },
    KeyEntry {
        chord: chord(KeyMatch::AnyChar, ModMatch::Any, "<char>"),
        guard: LineEditor::Command,
        effect: LineEditEffect::InsertChar,
        label: "append the typed character",
    },
    KeyEntry {
        chord: code(KeyCode::Enter, ModMatch::Any, "Enter"),
        guard: LineEditor::Search,
        effect: LineEditEffect::Submit,
        label: "confirm the search; the filter and match position stay active",
    },
    KeyEntry {
        chord: code(KeyCode::Esc, ModMatch::Any, "Esc"),
        guard: LineEditor::Search,
        effect: LineEditEffect::Cancel,
        label: "cancel the search and clear the query, matches and filter",
    },
    KeyEntry {
        chord: code(KeyCode::Backspace, ModMatch::Any, "Backspace"),
        guard: LineEditor::Search,
        effect: LineEditEffect::DeleteBackward,
        label: "delete the last character and re-run the search",
    },
    KeyEntry {
        chord: chord(KeyMatch::AnyChar, ModMatch::Any, "<char>"),
        guard: LineEditor::Search,
        effect: LineEditEffect::InsertChar,
        label: "append the typed character and re-run the search (incremental)",
    },
];

// ---------------------------------------------------------------------------
// Manual rendering surface
// ---------------------------------------------------------------------------

/// One rendered row of a key table: chord, context (guard label), effect.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct KeyTableRow {
    pub chord: String,
    pub context: &'static str,
    pub effect: &'static str,
}

/// A dispatch table as the manual sees it.
pub struct KeyTable {
    pub id: &'static str,
    pub title: &'static str,
    pub summary: &'static str,
    pub rows: fn() -> Vec<KeyTableRow>,
}

fn rows_of<G: GuardLabel, E: Copy>(table: &'static [KeyEntry<G, E>]) -> Vec<KeyTableRow> {
    table
        .iter()
        .map(|entry| KeyTableRow {
            chord: entry.chord.display.to_string(),
            context: entry.guard.label(),
            effect: entry.label,
        })
        .collect()
}

/// Every raw-key dispatch table, in the order the event loop consults them.
pub static KEY_TABLES: &[KeyTable] = &[
    KeyTable {
        id: "paste",
        title: "Clipboard paste",
        summary: "Checked before anything else, on key press or release.",
        rows: || rows_of(PASTE_KEYS),
    },
    KeyTable {
        id: "config-gate",
        title: "Submissions held until the daemon config is ready",
        summary: "Until the TUI has applied an authoritative daemon configuration, these submissions are refused without closing the editor.",
        rows: || rows_of(CONFIG_GATED_SUBMIT_KEYS),
    },
    KeyTable {
        id: "global",
        title: "Global intercepts",
        summary: "Consulted in order ahead of overlays, input surfaces and the Vim keymap; the first row whose context holds wins.",
        rows: || rows_of(GLOBAL_KEY_INTERCEPTS),
    },
    KeyTable {
        id: "session-list-nav",
        title: "Session-list arrows",
        summary: "Consulted after overlays, the file viewer and the input bar decline the key.",
        rows: || rows_of(SESSION_LIST_NAV_KEYS),
    },
    KeyTable {
        id: "detail-scroll",
        title: "Session-detail scrolling",
        summary: "Plain arrows scroll the focused transcript; Shift-Up/Down select events instead.",
        rows: || rows_of(DETAIL_SCROLL_KEYS),
    },
    KeyTable {
        id: "normal-follow-up",
        title: "Normal-mode follow-ups",
        summary: "Run after the Vim keymap has handled the same key.",
        rows: || rows_of(NORMAL_FOLLOW_UP_KEYS),
    },
    KeyTable {
        id: "line-edit",
        title: "One-line editors",
        summary: "The quick-launch input, `:` command line and `/` search line.",
        rows: || rows_of(LINE_EDIT_KEYS),
    },
];

#[cfg(test)]
#[allow(clippy::expect_used)] // Test fixtures fail loudly on a broken invariant.
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    /// T8a: `event.rs` decodes no chord outside these tables.
    #[test]
    fn event_rs_has_no_untabled_chords() {
        let source = include_str!("event.rs");
        let production = source
            .split("#[cfg(test)]\nmod tests {")
            .next()
            .expect("event.rs has production code");
        let offending: Vec<&str> = production
            .lines()
            .filter(|line| line.contains("KeyCode::"))
            .collect();
        assert_eq!(offending, Vec::<&str>::new());
    }

    /// T8b: each dispatch table's (chord, context, effect) rows equal the
    /// manual's raw-key table for it.
    #[test]
    fn key_table_triples_match_manual() {
        let manual = crate::manual::model::build_manual();
        let rendered = manual.region_tables("raw");
        assert_eq!(
            rendered.len(),
            KEY_TABLES.len(),
            "one manual table per key table"
        );
        for (table, manual_table) in KEY_TABLES.iter().zip(rendered) {
            let expected: BTreeSet<(String, String, String)> = (table.rows)()
                .into_iter()
                .map(|row| {
                    (
                        crate::manual::model::code(&row.chord),
                        row.context.to_string(),
                        row.effect.to_string(),
                    )
                })
                .collect();
            let actual: BTreeSet<(String, String, String)> = manual_table
                .rows
                .iter()
                .map(|row| (row[0].clone(), row[1].clone(), row[2].clone()))
                .collect();
            assert_eq!(actual, expected, "manual rows of {}", table.id);
        }
    }

    #[test]
    fn every_table_row_has_labels() {
        let mut ids = BTreeSet::new();
        for table in KEY_TABLES {
            assert!(ids.insert(table.id), "duplicate key table id {}", table.id);
            assert!(!table.title.is_empty() && !table.summary.is_empty());
            let rows = (table.rows)();
            assert!(!rows.is_empty(), "table {} has rows", table.id);
            for row in rows {
                assert!(!row.chord.is_empty());
                assert!(!row.context.is_empty());
                assert!(!row.effect.is_empty());
            }
        }
    }

    #[test]
    fn chords_match_their_modifier_rules() {
        let ctrl_shift_c = KeyEvent::new(KeyCode::Char('c'), CTRL_SHIFT);
        assert!(GLOBAL_KEY_INTERCEPTS[2].chord.matches(ctrl_shift_c));
        let ctrl_shift_left = KeyEvent::new(KeyCode::Left, CTRL_SHIFT);
        let exact_ctrl_left = code(KeyCode::Left, ModMatch::Exact(CTRL), "Ctrl-Left");
        assert!(exact_ctrl_left.matches(KeyEvent::new(KeyCode::Left, CTRL)));
        assert!(code(KeyCode::Left, ModMatch::Contains(CTRL_SHIFT), "x").matches(ctrl_shift_left));
        assert_eq!(
            Chord::typed_char(KeyEvent::new(KeyCode::Char('q'), NONE)),
            Some('q')
        );
    }
}
