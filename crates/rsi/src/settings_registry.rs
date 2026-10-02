//! Settings registry: the settings page's single source of truth.
//!
//! It holds the information architecture (groups, sections, rows) and each
//! row's summary, owner and apply class (Epic M design A.3, D.1, D.2, D.5).
//!
//! Slice (a) ships the metadata and the operator manual's settings chapter
//! reads it. The settings page itself is migrated onto this registry in
//! slice (c) (`row_value` / `activate`); until then the page keeps its own
//! row tables.

use rsi_common::daemon_config_catalog::ApplyClass;

/// Settings rail groups, ordered by how often the operator uses them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum SettingsGroup {
    Appearance,
    Workspace,
    Models,
    SafetyAndSpend,
    AgentAutomation,
    ProvidersAndSandboxes,
    Integrations,
}

impl SettingsGroup {
    pub const ALL: &'static [Self] = &[
        Self::Appearance,
        Self::Workspace,
        Self::Models,
        Self::SafetyAndSpend,
        Self::AgentAutomation,
        Self::ProvidersAndSandboxes,
        Self::Integrations,
    ];

    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Appearance => "APPEARANCE",
            Self::Workspace => "WORKSPACE",
            Self::Models => "MODELS",
            Self::SafetyAndSpend => "SAFETY & SPEND",
            Self::AgentAutomation => "AGENT AUTOMATION",
            Self::ProvidersAndSandboxes => "PROVIDERS & SANDBOXES",
            Self::Integrations => "INTEGRATIONS",
        }
    }

    /// The operator question the group answers.
    #[must_use]
    pub const fn summary(self) -> &'static str {
        match self {
            Self::Appearance => "How does it look?",
            Self::Workspace => "What do I see in the list and a new transcript?",
            Self::Models => "Which model does which job?",
            Self::SafetyAndSpend => "How do I stop or limit spend?",
            Self::AgentAutomation => "What does the daemon do on its own when agents fail?",
            Self::ProvidersAndSandboxes => "Where and how do agents run?",
            Self::Integrations => "How do I reach agents from elsewhere?",
        }
    }

    /// The group's sections in page order: the tabs the settings view shows
    /// while this group is selected in the category rail.
    pub fn sections(self) -> impl Iterator<Item = SettingsSection> {
        SettingsSection::ALL
            .iter()
            .copied()
            .filter(move |section| section.group() == self)
    }

    /// The tab a category opens on when the rail selects it.
    #[must_use]
    pub fn first_section(self) -> SettingsSection {
        self.sections()
            .next()
            .unwrap_or(SettingsSection::ThemeColors)
    }

    /// Position of the group in [`Self::ALL`] (rail order).
    #[must_use]
    pub fn position(self) -> usize {
        Self::ALL
            .iter()
            .position(|group| *group == self)
            .unwrap_or(0)
    }
}

/// Settings sections (the rail entries inside a group), in page order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum SettingsSection {
    ThemeColors,
    Screen,
    SessionList,
    TranscriptDefaults,
    InputPrompts,
    ModelRoles,
    ApiProviders,
    SystemPrompt,
    ModelControl,
    Budgets,
    Usage,
    ProviderKeys,
    RetriesRecovery,
    StallDetection,
    MemoryDreaming,
    Orchestration,
    CodeIntelligence,
    ProviderIsolation,
    SandboxStorage,
    ClaudeHooks,
    ClaudeSkills,
    MessageBridges,
    Satellites,
    McpServers,
}

impl SettingsSection {
    pub const ALL: &'static [Self] = &[
        Self::ThemeColors,
        Self::Screen,
        Self::SessionList,
        Self::TranscriptDefaults,
        Self::InputPrompts,
        Self::ModelRoles,
        Self::ApiProviders,
        Self::SystemPrompt,
        Self::ModelControl,
        Self::Budgets,
        Self::Usage,
        Self::ProviderKeys,
        Self::RetriesRecovery,
        Self::StallDetection,
        Self::MemoryDreaming,
        Self::Orchestration,
        Self::CodeIntelligence,
        Self::ProviderIsolation,
        Self::SandboxStorage,
        Self::ClaudeHooks,
        Self::ClaudeSkills,
        Self::MessageBridges,
        Self::Satellites,
        Self::McpServers,
    ];

    #[must_use]
    pub const fn group(self) -> SettingsGroup {
        match self {
            Self::ThemeColors | Self::Screen => SettingsGroup::Appearance,
            Self::SessionList | Self::TranscriptDefaults | Self::InputPrompts => {
                SettingsGroup::Workspace
            }
            Self::ModelRoles | Self::ApiProviders | Self::SystemPrompt => SettingsGroup::Models,
            Self::ModelControl | Self::Budgets | Self::Usage | Self::ProviderKeys => {
                SettingsGroup::SafetyAndSpend
            }
            Self::RetriesRecovery
            | Self::StallDetection
            | Self::MemoryDreaming
            | Self::Orchestration
            | Self::CodeIntelligence => SettingsGroup::AgentAutomation,
            Self::ProviderIsolation
            | Self::SandboxStorage
            | Self::ClaudeHooks
            | Self::ClaudeSkills => SettingsGroup::ProvidersAndSandboxes,
            Self::MessageBridges | Self::Satellites | Self::McpServers => {
                SettingsGroup::Integrations
            }
        }
    }

    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::ThemeColors => "Theme & Colors",
            Self::Screen => "Screen",
            Self::SessionList => "Session List",
            Self::TranscriptDefaults => "Transcript Defaults",
            Self::InputPrompts => "Input & Prompts",
            Self::ModelRoles => "Model Roles",
            Self::ApiProviders => "API Providers",
            Self::SystemPrompt => "System Prompt",
            Self::ModelControl => "Model Control",
            Self::Budgets => "Budgets",
            Self::Usage => "Usage",
            Self::ProviderKeys => "Provider Keys",
            Self::RetriesRecovery => "Retries & Recovery",
            Self::StallDetection => "Stall Detection",
            Self::MemoryDreaming => "Memory & Dreaming",
            Self::Orchestration => "Orchestration",
            Self::CodeIntelligence => "Code Intelligence",
            Self::ProviderIsolation => "Provider Isolation",
            Self::SandboxStorage => "Sandbox Storage",
            Self::ClaudeHooks => "Claude Hooks",
            Self::ClaudeSkills => "Claude Skills",
            Self::MessageBridges => "Message Bridges",
            Self::Satellites => "Satellites",
            Self::McpServers => "MCP Servers",
        }
    }

    #[must_use]
    pub const fn summary(self) -> &'static str {
        match self {
            Self::ThemeColors => "The built-in theme, per-role overrides and legacy colors.",
            Self::Screen => "Text-area background, animation styles and detail column placement.",
            Self::SessionList => "What the session navigator and cards show.",
            Self::TranscriptDefaults => "What a newly opened transcript shows.",
            Self::InputPrompts => "How typing and submitting behave.",
            Self::ModelRoles => "Which model does which job.",
            Self::ApiProviders => "OpenAI-compatible endpoints for model selection.",
            Self::SystemPrompt => "The system-prompt preset applied to launches.",
            Self::ModelControl => "Stop or limit model spend daemon-wide.",
            Self::Budgets => "Model budget policies.",
            Self::Usage => "Usage telemetry and stop/cancel actions.",
            Self::ProviderKeys => "Manage provider credentials stored in the key vault.",
            Self::RetriesRecovery => "What the daemon does when a session fails.",
            Self::StallDetection => "How stalled sessions are detected and nudged.",
            Self::MemoryDreaming => "Memory extraction and consolidation.",
            Self::Orchestration => "Background queue and recursive DAG controls.",
            Self::CodeIntelligence => "Code-graph indexing of project source.",
            Self::ProviderIsolation => "How provider processes are sandboxed and isolated.",
            Self::SandboxStorage => "Sandbox disk use, cache reclamation and settlement.",
            Self::ClaudeHooks => "Claude Code hooks.",
            Self::ClaudeSkills => "Claude user skills.",
            Self::MessageBridges => "Reach agents from Signal or iMessage.",
            Self::Satellites => "Registered peers, local links and cached remote sessions.",
            Self::McpServers => "MCP server definitions and credential metadata.",
        }
    }
}

/// Declares `SettingId` and `SettingId::ALL` from one list of variants, so
/// adding a setting is a single line. Page order is not encoded here: it is
/// the declaration order of the `SETTINGS` slice below (#1013), so no
/// positional ordinal is ever renumbered.
macro_rules! setting_ids {
    ($($(#[$meta:meta])* $id:ident,)+) => {
        /// One logical settings row. Dynamic lists (hooks, skills, providers,
        /// budgets, stats) are one variant each.
        #[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
        pub enum SettingId {
            $($(#[$meta])* $id,)+
        }

        impl SettingId {
            /// Every setting id (guards `SETTINGS` against omissions and
            /// duplicates; carries no ordering meaning).
            pub const ALL: &'static [Self] = &[$(Self::$id,)+];
        }
    };
}

setting_ids! {
    BuiltInTheme,
    ThemeRoles,
    LegacyColors,
    ResetTheme,
    TextAreaBackground,
    BackgroundColor,
    FormulationAnimation,
    FormulationSpeed,
    ActivityIndicator,
    DetailColumn,
    NavigatorPreset,
    NavigatorColumns,
    CardFields,
    SessionRetentionEnabled,
    SessionRetentionWindowHours,
    ShowSystemEvents,
    ShowThinkingEvents,
    HideToolResults,
    SubmitOnEnter,
    AutoOpenQuestionPanel,
    PromptCompiler,
    DefaultModel,
    TitleModel,
    PromptCompilerModel,
    MemoryModel,
    DreamModel,
    ClassifierModel,
    ApiProviders,
    SystemPromptPreset,
    ModelControlMode,
    EmergencyStop,
    MaxChildEffort,
    BudgetPolicies,
    UsageStats,
    UsageActions,
    ProviderCredentialSlots,
    RetryOnFailure,
    MaxRetries,
    RetryMaxBackoff,
    RetryOnStall,
    ReconciliationLoop,
    ContextRotation,
    ContextRotationGlobalPct,
    ContextRotationClaudePct,
    ContextRotationCodexPct,
    StallDetection,
    StallClassifier,
    ClassifierIdleClaude,
    ClassifierIdleCodex,
    ClassifierCooldown,
    ClassifierMaxPerSession,
    ClassifierConfidenceFloor,
    MemorySystem,
    DreamConsolidation,
    ObservationThreshold,
    DreamCooldown,
    DreamIdle,
    DialecticEngine,
    BackgroundQueue,
    DagRecoveryControls,
    DagSchedulerControls,
    DagCancellationControls,
    DagLiveSchedulerControl,
    DagRunLeaseTtl,
    DagMaxConcurrentGraphs,
    GvRenderRecursiveOrigin,
    GvInfoDashboard,
    TopologyExecutor,
    TopologyBuildNodes,
    TopologyBulkFanout,
    RsidScopeMemoryHigh,
    RsidScopeMemoryMax,
    RsidScopeMemorySwapMax,
    RsidScopeCpuWeight,
    WorkerScopeMemoryHigh,
    WorkerScopeMemoryMax,
    WorkerScopeMemorySwapMax,
    WorkerScopeCpuWeight,
    RollingQueueEnabled,
    DeployDrainEnabled,
    RollingQueueBatchSize,
    RollingQueueSpeculationDepth,
    ProgramHoldWhileChildren,
    ChildKeepaliveEnabled,
    ChildKeepaliveWindow,
    HarnessWebAccess,
    HarnessEgressMode,
    HarnessSearchCap,
    HarnessFetchCap,
    HarnessCompletionGates,
    HarnessOutputCap,
    HarnessWebCostCap,
    CodegraphIndexing,
    CodexSandbox,
    ClaudeConfigIsolation,
    VaultEnvCompat,
    VaultCheckTtl,
    OpenRouterRoute,
    BedrockRoute,
    ApiRouteFallback,
    OpenRouterContextBudget,
    HarnessMaxIterations,
    SandboxStorageStatus,
    CacheReclaim,
    CacheReclaimTtl,
    CacheReclaimInterval,
    CachePressureHigh,
    CachePressureLow,
    CacheReclaimPassLimit,
    AgentBuildJobs,
    AgentBuildLineTables,
    AgentBuildSccache,
    AgentBuildSccacheSize,
    AgentBuildSlots,
    PreviewReclaim,
    ReclaimNow,
    SourceWorktreeSettlement,
    SandboxMaxSourceRoots,
    SandboxMinFreeGib,
    McpDeferredToolThreshold,
    CloudSpendStatus,
    CloudSpendStopLine,
    CloudSpendDailyCap,
    ArchivedSandboxPurge,
    ClaudeHooks,
    ClaudeSkills,
    SignalBridge,
    ImessageBridge,
    CompletedTranscriptCache,
    SatelliteRegistry,
    SatellitePolling,
    GovernorBuildSlots,
    GovernorLanderSlots,
    GovernorMaxLoad,
    GovernorMinFreeDisk,
    GovernorMinAvailMem,
    GovernorMaxWorkersSlice,
    McpServerConfigurations,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettingKind {
    Bool,
    Cycle,
    Edit,
    Action,
    ReadOnly,
    DynamicList,
}

impl SettingKind {
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Bool => "toggle",
            Self::Cycle => "choice",
            Self::Edit => "edit",
            Self::Action => "action",
            Self::ReadOnly => "read-only",
            Self::DynamicList => "list",
        }
    }
}

/// Where a row's value lives.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettingOwner {
    /// The TUI's `state.json`.
    Tui,
    /// One persisted daemon runtime-config field.
    Daemon(&'static str),
    /// Several persisted daemon fields edited as one row (a model role's
    /// model, provider, base URL and fallback).
    DaemonFields(&'static [&'static str]),
    /// Daemon-owned state outside the runtime config (model control, budgets,
    /// usage, sandbox storage), named by its RPC family.
    DaemonState(&'static str),
    /// A file outside rsi's own state.
    File(&'static str),
}

impl SettingOwner {
    /// The persisted daemon fields this row writes.
    #[must_use]
    pub fn daemon_fields(self) -> Vec<&'static str> {
        match self {
            Self::Daemon(field) => vec![field],
            Self::DaemonFields(fields) => fields.to_vec(),
            Self::Tui | Self::DaemonState(_) | Self::File(_) => Vec::new(),
        }
    }

    #[must_use]
    pub fn label(self) -> String {
        match self {
            Self::Tui => "TUI (state.json)".to_string(),
            Self::Daemon(field) => format!("daemon field `{field}`"),
            Self::DaemonFields(fields) => format!(
                "daemon fields {}",
                fields
                    .iter()
                    .map(|field| format!("`{field}`"))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            Self::DaemonState(family) => format!("daemon ({family})"),
            Self::File(path) => format!("file `{path}`"),
        }
    }
}

/// When a change to a row takes effect.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettingApply {
    /// TUI-owned rows apply immediately.
    Immediate,
    /// Daemon-owned rows follow the daemon catalog's class.
    Daemon(ApplyClass),
}

impl SettingApply {
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Immediate => "immediately",
            Self::Daemon(class) => class.label(),
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct SettingSpec {
    pub id: SettingId,
    pub section: SettingsSection,
    pub label: &'static str,
    pub summary: &'static str,
    pub detail: Option<&'static str>,
    pub keywords: &'static [&'static str],
    pub kind: SettingKind,
    pub owner: SettingOwner,
    pub apply: SettingApply,
    pub destructive: bool,
}

/// Every settings row, in page order (sections in `SettingsSection::ALL`
/// order).
pub static SETTINGS: &[SettingSpec] = &[
    SettingSpec {
        id: SettingId::BuiltInTheme,
        section: SettingsSection::ThemeColors,
        label: "Built-in theme",
        summary: "Selects the active built-in color theme; the preview applies immediately.",
        detail: None,
        keywords: &["theme", "palette", "colors"],
        kind: SettingKind::Cycle,
        owner: SettingOwner::Tui,
        apply: SettingApply::Immediate,
        destructive: false,
    },
    SettingSpec {
        id: SettingId::ThemeRoles,
        section: SettingsSection::ThemeColors,
        label: "Theme roles",
        summary: "Overrides individual semantic color roles of the active theme with a live contrast check.",
        detail: Some(
            "Each role opens the theme role editor: type a hex color, Enter previews then commits, Delete resets the role.",
        ),
        keywords: &["role", "override", "contrast"],
        kind: SettingKind::DynamicList,
        owner: SettingOwner::Tui,
        apply: SettingApply::Immediate,
        destructive: false,
    },
    SettingSpec {
        id: SettingId::LegacyColors,
        section: SettingsSection::ThemeColors,
        label: "Legacy message/editor colors",
        summary: "Edits the legacy message-border and editor-cursor color slots.",
        detail: None,
        keywords: &["legacy", "border", "cursor"],
        kind: SettingKind::Action,
        owner: SettingOwner::Tui,
        apply: SettingApply::Immediate,
        destructive: false,
    },
    SettingSpec {
        id: SettingId::ResetTheme,
        section: SettingsSection::ThemeColors,
        label: "Reset active theme",
        summary: "Clears every semantic role override while keeping the selected theme and legacy colors.",
        detail: None,
        keywords: &["reset", "overrides"],
        kind: SettingKind::Action,
        owner: SettingOwner::Tui,
        apply: SettingApply::Immediate,
        destructive: true,
    },
    SettingSpec {
        id: SettingId::TextAreaBackground,
        section: SettingsSection::Screen,
        label: "Text area background",
        summary: "Paints the configured background color behind text-entry surfaces.",
        detail: None,
        keywords: &["backfill", "background"],
        kind: SettingKind::Bool,
        owner: SettingOwner::Tui,
        apply: SettingApply::Immediate,
        destructive: false,
    },
    SettingSpec {
        id: SettingId::BackgroundColor,
        section: SettingsSection::Screen,
        label: "Background color",
        summary: "Sets the hex color used when the text area background is on.",
        detail: None,
        keywords: &["hex", "backfill"],
        kind: SettingKind::Edit,
        owner: SettingOwner::Tui,
        apply: SettingApply::Immediate,
        destructive: false,
    },
    SettingSpec {
        id: SettingId::FormulationAnimation,
        section: SettingsSection::Screen,
        label: "Formulation animation",
        summary: "Reveals each new live message with a top-to-bottom wipe. Off by default.",
        detail: None,
        keywords: &["animation", "formulation", "reveal"],
        kind: SettingKind::Bool,
        owner: SettingOwner::Tui,
        apply: SettingApply::Immediate,
        destructive: false,
    },
    SettingSpec {
        id: SettingId::FormulationSpeed,
        section: SettingsSection::Screen,
        label: "Formulation speed",
        summary: "Sets how long the formulation reveal runs when the animation is on.",
        detail: None,
        keywords: &["animation", "formulation", "duration"],
        kind: SettingKind::Cycle,
        owner: SettingOwner::Tui,
        apply: SettingApply::Immediate,
        destructive: false,
    },
    SettingSpec {
        id: SettingId::ActivityIndicator,
        section: SettingsSection::Screen,
        label: "Activity indicator",
        summary: "Cycles Semantic and five rainbow styles, including Sonic Speed Up and Rainbow Starlight.",
        detail: None,
        keywords: &["indicator", "spinner", "rainbow"],
        kind: SettingKind::Cycle,
        owner: SettingOwner::Tui,
        apply: SettingApply::Immediate,
        destructive: false,
    },
    SettingSpec {
        id: SettingId::DetailColumn,
        section: SettingsSection::Screen,
        label: "Detail column",
        summary: "Places the session-detail transcript column: Dynamic, Left Aligned or Center Aligned.",
        detail: Some(
            "Ctrl-Left / Ctrl-Right in a focused session detail move the column and switch to Dynamic; moving it to the left edge snaps to Left Aligned.",
        ),
        keywords: &["column", "align", "center", "left", "layout"],
        kind: SettingKind::Cycle,
        owner: SettingOwner::Tui,
        apply: SettingApply::Immediate,
        destructive: false,
    },
    SettingSpec {
        id: SettingId::NavigatorPreset,
        section: SettingsSection::SessionList,
        label: "Navigator preset",
        summary: "Cycles the session navigator column preset: Dense, Operations or Cost.",
        detail: Some(
            "Presets: Dense (default), Operations, Cost. Each preset sets which columns the navigator shows; the optional columns below add to it.",
        ),
        keywords: &["navigator", "columns", "preset"],
        kind: SettingKind::Cycle,
        owner: SettingOwner::Tui,
        apply: SettingApply::Immediate,
        destructive: false,
    },
    SettingSpec {
        id: SettingId::NavigatorColumns,
        section: SettingsSection::SessionList,
        label: "Navigator optional columns",
        summary: "Turns optional navigator columns on or off; J/K move the selected column, saved per preset.",
        detail: Some(
            "Optional columns: Navigator age, Navigator model / effort, Navigator retry, Navigator cost, Navigator work, Navigator rotation, Navigator project, Navigator created. J / K move the selected column later / earlier in the active preset's order, which is saved separately for each preset. Required columns cannot be hidden.",
        ),
        keywords: &["navigator", "columns"],
        kind: SettingKind::DynamicList,
        owner: SettingOwner::Tui,
        apply: SettingApply::Immediate,
        destructive: false,
    },
    SettingSpec {
        id: SettingId::CardFields,
        section: SettingsSection::SessionList,
        label: "Card fields",
        summary: "Chooses which facts render on session-list cards.",
        detail: Some(
            "Card fields: Context bar, Cost, Turn count, Retry info, Pin indicator, Rotation depth, Heat color, Description, Kind pill (TR/BUG), Docregblock pill, Accumulated work time, Created date.",
        ),
        keywords: &["card", "fields"],
        kind: SettingKind::DynamicList,
        owner: SettingOwner::Tui,
        apply: SettingApply::Immediate,
        destructive: false,
    },
    SettingSpec {
        id: SettingId::SessionRetentionEnabled,
        section: SettingsSection::SessionList,
        label: "Automatic session archive",
        summary: "Archives delivered or issue-filed terminal sessions after an idle window.",
        detail: None,
        keywords: &["archive", "retention", "sessions"],
        kind: SettingKind::Bool,
        owner: SettingOwner::Daemon("session_retention_enabled"),
        apply: SettingApply::Daemon(ApplyClass::Live),
        destructive: false,
    },
    SettingSpec {
        id: SettingId::SessionRetentionWindowHours,
        section: SettingsSection::SessionList,
        label: "Archive idle hours",
        summary: "Idle hours before an eligible terminal session is archived.",
        detail: None,
        keywords: &["archive", "retention", "idle"],
        kind: SettingKind::Cycle,
        owner: SettingOwner::Daemon("session_retention_window_hours"),
        apply: SettingApply::Daemon(ApplyClass::Live),
        destructive: false,
    },
    SettingSpec {
        id: SettingId::ShowSystemEvents,
        section: SettingsSection::TranscriptDefaults,
        label: "Show system events",
        summary: "Default visibility of system events in newly opened transcripts (zs toggles one transcript).",
        detail: None,
        keywords: &["system", "events", "transcript"],
        kind: SettingKind::Bool,
        owner: SettingOwner::Tui,
        apply: SettingApply::Immediate,
        destructive: false,
    },
    SettingSpec {
        id: SettingId::ShowThinkingEvents,
        section: SettingsSection::TranscriptDefaults,
        label: "Show thinking events",
        summary: "Default visibility of model thinking events in newly opened transcripts (zt toggles one transcript).",
        detail: None,
        keywords: &["thinking", "events", "transcript"],
        kind: SettingKind::Bool,
        owner: SettingOwner::Tui,
        apply: SettingApply::Immediate,
        destructive: false,
    },
    SettingSpec {
        id: SettingId::HideToolResults,
        section: SettingsSection::TranscriptDefaults,
        label: "Hide tool results",
        summary: "Hides tool-result events in newly opened transcripts.",
        detail: None,
        keywords: &["tool", "results"],
        kind: SettingKind::Bool,
        owner: SettingOwner::Tui,
        apply: SettingApply::Immediate,
        destructive: false,
    },
    SettingSpec {
        id: SettingId::SubmitOnEnter,
        section: SettingsSection::InputPrompts,
        label: "Submit on Enter",
        summary: "Plain Enter submits in submit-capable inputs; Shift-Enter inserts a newline. Document editors always insert a newline.",
        detail: None,
        keywords: &["enter", "submit", "newline"],
        kind: SettingKind::Bool,
        owner: SettingOwner::Tui,
        apply: SettingApply::Immediate,
        destructive: false,
    },
    SettingSpec {
        id: SettingId::AutoOpenQuestionPanel,
        section: SettingsSection::InputPrompts,
        label: "Auto-open question panel",
        summary: "Opens a waiting session's question panel automatically when nothing else is open.",
        detail: None,
        keywords: &["question", "modal"],
        kind: SettingKind::Bool,
        owner: SettingOwner::Tui,
        apply: SettingApply::Immediate,
        destructive: false,
    },
    SettingSpec {
        id: SettingId::PromptCompiler,
        section: SettingsSection::InputPrompts,
        label: "Prompt compiler",
        summary: "Enables prompt compilation (Ctrl-Y in the input bar); off, compile requests return nothing.",
        detail: None,
        keywords: &["compile", "processor"],
        kind: SettingKind::Bool,
        owner: SettingOwner::Tui,
        apply: SettingApply::Immediate,
        destructive: false,
    },
    SettingSpec {
        id: SettingId::DefaultModel,
        section: SettingsSection::ModelRoles,
        label: "Default model",
        summary: "Provider and model used for newly launched sessions.",
        detail: None,
        keywords: &["model", "default", "launch"],
        kind: SettingKind::Cycle,
        owner: SettingOwner::Tui,
        apply: SettingApply::Immediate,
        destructive: false,
    },
    SettingSpec {
        id: SettingId::TitleModel,
        section: SettingsSection::ModelRoles,
        label: "Title model",
        summary: "Provider and model that generate session titles (with a fallback model).",
        detail: None,
        keywords: &["title", "model"],
        kind: SettingKind::Edit,
        owner: SettingOwner::DaemonFields(&[
            "title_model_local",
            "title_model_provider",
            "title_model_base_url",
            "title_model_fallback",
        ]),
        apply: SettingApply::Daemon(ApplyClass::Live),
        destructive: false,
    },
    SettingSpec {
        id: SettingId::PromptCompilerModel,
        section: SettingsSection::ModelRoles,
        label: "Prompt compiler model",
        summary: "Provider and model that compile and refine prompts.",
        detail: None,
        keywords: &["compile", "processor", "model"],
        kind: SettingKind::Edit,
        owner: SettingOwner::DaemonFields(&[
            "prompt_compile_model_local",
            "prompt_compile_model_provider",
            "prompt_compile_model_base_url",
        ]),
        apply: SettingApply::Daemon(ApplyClass::Live),
        destructive: false,
    },
    SettingSpec {
        id: SettingId::MemoryModel,
        section: SettingsSection::ModelRoles,
        label: "Memory model",
        summary: "Local and fallback models for observation extraction and summarization.",
        detail: None,
        keywords: &["memory", "fallback", "model"],
        kind: SettingKind::Edit,
        owner: SettingOwner::DaemonFields(&[
            "memory_model_local",
            "memory_model_fallback",
            "memory_model_fallback_provider",
            "memory_model_fallback_base_url",
        ]),
        apply: SettingApply::Daemon(ApplyClass::Live),
        destructive: false,
    },
    SettingSpec {
        id: SettingId::DreamModel,
        section: SettingsSection::ModelRoles,
        label: "Dream model",
        summary: "Provider and model that run memory consolidation and deduction.",
        detail: None,
        keywords: &["dream", "model"],
        kind: SettingKind::Edit,
        owner: SettingOwner::DaemonFields(&[
            "dream_model",
            "dream_model_provider",
            "dream_model_base_url",
        ]),
        apply: SettingApply::Daemon(ApplyClass::Live),
        destructive: false,
    },
    SettingSpec {
        id: SettingId::ClassifierModel,
        section: SettingsSection::ModelRoles,
        label: "Stall classifier model",
        summary: "Model the stall classifier calls to judge a stalled session.",
        detail: Some(
            "The classifier is built when the daemon starts, so a new model applies after a daemon restart. The classifier's thresholds live in AGENT AUTOMATION ▸ Stall Detection.",
        ),
        keywords: &["classifier", "stall", "model"],
        kind: SettingKind::Edit,
        owner: SettingOwner::Daemon("stall_classifier_model"),
        apply: SettingApply::Daemon(ApplyClass::DaemonRestart),
        destructive: false,
    },
    SettingSpec {
        id: SettingId::ApiProviders,
        section: SettingsSection::ApiProviders,
        label: "API providers",
        summary: "OpenAI-compatible endpoints and keys offered by model selection; a adds, Enter edits, d deletes.",
        detail: None,
        keywords: &["provider", "openai", "endpoint", "api key"],
        kind: SettingKind::DynamicList,
        owner: SettingOwner::Tui,
        apply: SettingApply::Immediate,
        destructive: false,
    },
    SettingSpec {
        id: SettingId::SystemPromptPreset,
        section: SettingsSection::SystemPrompt,
        label: "System prompt preset",
        summary: "System-prompt preset applied to launches: Default, Concise, Code Only or Caveman.",
        detail: None,
        keywords: &["system prompt", "preset"],
        kind: SettingKind::Cycle,
        owner: SettingOwner::Daemon("system_prompt_preset"),
        apply: SettingApply::Daemon(ApplyClass::DaemonRestart),
        destructive: false,
    },
    SettingSpec {
        id: SettingId::ModelControlMode,
        section: SettingsSection::ModelControl,
        label: "Model control mode",
        summary: "Daemon-wide model control: normal, pause-background, deny-paid, local-only or stop-all.",
        detail: None,
        keywords: &["model control", "pause", "deny paid", "local only"],
        kind: SettingKind::Cycle,
        owner: SettingOwner::DaemonState("model control"),
        apply: SettingApply::Daemon(ApplyClass::Live),
        destructive: false,
    },
    SettingSpec {
        id: SettingId::EmergencyStop,
        section: SettingsSection::ModelControl,
        label: "Emergency stop",
        summary: "Immediately denies new paid work and cancels every live model invocation.",
        detail: None,
        keywords: &["stop", "emergency", "cancel"],
        kind: SettingKind::Action,
        owner: SettingOwner::DaemonState("model control"),
        apply: SettingApply::Daemon(ApplyClass::Live),
        destructive: true,
    },
    SettingSpec {
        id: SettingId::MaxChildEffort,
        section: SettingsSection::ModelControl,
        label: "Orchestration max child effort",
        summary: "Ceiling on the reasoning effort a spawned child agent may request at admission.",
        detail: None,
        keywords: &["effort", "child", "orchestration"],
        kind: SettingKind::Cycle,
        owner: SettingOwner::Daemon("orchestration_max_child_effort"),
        apply: SettingApply::Daemon(ApplyClass::Live),
        destructive: false,
    },
    SettingSpec {
        id: SettingId::BudgetPolicies,
        section: SettingsSection::Budgets,
        label: "Budget policies",
        summary: "Daemon model budget policies; a adds, Enter edits, d deletes (an empty list shows the add hint).",
        detail: None,
        keywords: &["budget", "tokens", "limit"],
        kind: SettingKind::DynamicList,
        owner: SettingOwner::DaemonState("model budgets"),
        apply: SettingApply::Daemon(ApplyClass::Live),
        destructive: false,
    },
    SettingSpec {
        id: SettingId::UsageStats,
        section: SettingsSection::Usage,
        label: "Usage and telemetry",
        summary: "Read-only daemon usage and model-control telemetry.",
        detail: None,
        keywords: &["usage", "stats", "telemetry"],
        kind: SettingKind::ReadOnly,
        owner: SettingOwner::DaemonState("usage statistics"),
        apply: SettingApply::Daemon(ApplyClass::Live),
        destructive: false,
    },
    SettingSpec {
        id: SettingId::UsageActions,
        section: SettingsSection::Usage,
        label: "Stop-all and cancel",
        summary: "Destructive actions from the stats view: emergency stop-all and cancel a live invocation.",
        detail: None,
        keywords: &["stop", "cancel", "invocation"],
        kind: SettingKind::Action,
        owner: SettingOwner::DaemonState("model control"),
        apply: SettingApply::Daemon(ApplyClass::Live),
        destructive: true,
    },
    SettingSpec {
        id: SettingId::ProviderCredentialSlots,
        section: SettingsSection::ProviderKeys,
        label: "Provider credential slots",
        summary: "Twenty-three masked vault slots; s sets, r rotates, c checks, d clears after confirmation, and i imports from environment.",
        detail: None,
        keywords: &["provider", "credential", "key", "vault", "secret"],
        kind: SettingKind::DynamicList,
        owner: SettingOwner::DaemonState("key vault"),
        apply: SettingApply::Daemon(ApplyClass::Live),
        destructive: false,
    },
    SettingSpec {
        id: SettingId::RetryOnFailure,
        section: SettingsSection::RetriesRecovery,
        label: "Retry on failure",
        summary: "Kill-switch for durable automatic retry of failed sessions.",
        detail: None,
        keywords: &["retry", "failure"],
        kind: SettingKind::Bool,
        owner: SettingOwner::Daemon("retry_enabled"),
        apply: SettingApply::Daemon(ApplyClass::Live),
        destructive: false,
    },
    SettingSpec {
        id: SettingId::MaxRetries,
        section: SettingsSection::RetriesRecovery,
        label: "Max retries",
        summary: "Persisted daemon retry cap (`retry_max_default`); it is not applied to default launches.",
        detail: Some(
            "rsid gives every session kind zero automatic retries unless the launch carries an explicit retry policy (fail-closed; rsid session/retry_policy.rs:14-18). Changing this value does not give default launches retries.",
        ),
        keywords: &["retry", "max", "attempts"],
        kind: SettingKind::ReadOnly,
        owner: SettingOwner::Daemon("retry_max_default"),
        apply: SettingApply::Daemon(ApplyClass::NotApplied),
        destructive: false,
    },
    SettingSpec {
        id: SettingId::RetryMaxBackoff,
        section: SettingsSection::RetriesRecovery,
        label: "Retry max backoff",
        summary: "Upper bound, in milliseconds, on the delay between automatic retries.",
        detail: None,
        keywords: &["retry", "backoff", "delay"],
        kind: SettingKind::Cycle,
        owner: SettingOwner::Daemon("retry_max_backoff_ms"),
        apply: SettingApply::Daemon(ApplyClass::Live),
        destructive: false,
    },
    SettingSpec {
        id: SettingId::RetryOnStall,
        section: SettingsSection::RetriesRecovery,
        label: "Retry on stall",
        summary: "Runs the stall-retry handler that relaunches stalled sessions.",
        detail: None,
        keywords: &["retry", "stall"],
        kind: SettingKind::Bool,
        owner: SettingOwner::Daemon("retry_on_stall"),
        apply: SettingApply::Daemon(ApplyClass::DaemonRestart),
        destructive: false,
    },
    SettingSpec {
        id: SettingId::ReconciliationLoop,
        section: SettingsSection::RetriesRecovery,
        label: "Reconciliation loop",
        summary: "Runs the background loop that reconciles session liveness and consistency.",
        detail: None,
        keywords: &["reconciliation", "liveness"],
        kind: SettingKind::Bool,
        owner: SettingOwner::Daemon("reconciliation_enabled"),
        apply: SettingApply::Daemon(ApplyClass::DaemonRestart),
        destructive: false,
    },
    SettingSpec {
        id: SettingId::ContextRotation,
        section: SettingsSection::RetriesRecovery,
        label: "Context rotation",
        summary: "Near its limit, asks a manager or Epic lead to pass its seat at the next idle boundary; workers keep going on native compaction.",
        detail: Some("Manual rotation still works for any session."),
        keywords: &["rotation", "context"],
        kind: SettingKind::Bool,
        owner: SettingOwner::Daemon("context_rotation_enabled"),
        apply: SettingApply::Daemon(ApplyClass::PartialLive),
        destructive: false,
    },
    SettingSpec {
        id: SettingId::ContextRotationGlobalPct,
        section: SettingsSection::RetriesRecovery,
        label: "Context rotation threshold (global)",
        summary: "Overrides Claude Code and Codex rotation thresholds when set.",
        detail: Some("Default clears the override; choose 1–99%."),
        keywords: &["rotation", "context", "threshold", "global"],
        kind: SettingKind::Cycle,
        owner: SettingOwner::Daemon("context_rotation_global_pct"),
        apply: SettingApply::Daemon(ApplyClass::Live),
        destructive: false,
    },
    SettingSpec {
        id: SettingId::ContextRotationClaudePct,
        section: SettingsSection::RetriesRecovery,
        label: "Context rotation threshold (Claude Code)",
        summary: "Claude Code rotation threshold when no global override is set.",
        detail: Some("Default uses the built-in 65%; choose 1–99%."),
        keywords: &["rotation", "context", "threshold", "claude"],
        kind: SettingKind::Cycle,
        owner: SettingOwner::Daemon("context_rotation_claude_pct"),
        apply: SettingApply::Daemon(ApplyClass::Live),
        destructive: false,
    },
    SettingSpec {
        id: SettingId::ContextRotationCodexPct,
        section: SettingsSection::RetriesRecovery,
        label: "Context rotation threshold (Codex)",
        summary: "Codex, Pioneer and Codex App Server rotation threshold when no global override is set.",
        detail: Some("Default uses the built-in 65%; choose 1–99%."),
        keywords: &["rotation", "context", "threshold", "codex", "pioneer"],
        kind: SettingKind::Cycle,
        owner: SettingOwner::Daemon("context_rotation_codex_pct"),
        apply: SettingApply::Daemon(ApplyClass::Live),
        destructive: false,
    },
    SettingSpec {
        id: SettingId::StallDetection,
        section: SettingsSection::StallDetection,
        label: "Stall detection",
        summary: "Runs the stall detector that flags sessions which stop making progress.",
        detail: None,
        keywords: &["stall", "detector"],
        kind: SettingKind::Bool,
        owner: SettingOwner::Daemon("stall_detection_enabled"),
        apply: SettingApply::Daemon(ApplyClass::DaemonRestart),
        destructive: false,
    },
    SettingSpec {
        id: SettingId::StallClassifier,
        section: SettingsSection::StallDetection,
        label: "Stall classifier",
        summary: "Asks a model whether a flagged session is really stalled before nudging it.",
        detail: Some(
            "Turning it off applies now; turning it on needs a daemon restart. The classifier model is edited in MODELS ▸ Model Roles.",
        ),
        keywords: &["classifier", "stall", "nudge"],
        kind: SettingKind::Bool,
        owner: SettingOwner::Daemon("stall_classifier_enabled"),
        apply: SettingApply::Daemon(ApplyClass::LiveOffRestartOn),
        destructive: false,
    },
    SettingSpec {
        id: SettingId::ClassifierIdleClaude,
        section: SettingsSection::StallDetection,
        label: "Classifier idle threshold (Claude)",
        summary: "Idle seconds before the classifier considers a Claude session stalled.",
        detail: None,
        keywords: &["classifier", "idle", "claude"],
        kind: SettingKind::Edit,
        owner: SettingOwner::Daemon("stall_classifier_idle_secs"),
        apply: SettingApply::Daemon(ApplyClass::DaemonRestart),
        destructive: false,
    },
    SettingSpec {
        id: SettingId::ClassifierIdleCodex,
        section: SettingsSection::StallDetection,
        label: "Classifier idle threshold (Codex)",
        summary: "Idle seconds before the classifier considers a Codex session stalled.",
        detail: None,
        keywords: &["classifier", "idle", "codex"],
        kind: SettingKind::Edit,
        owner: SettingOwner::Daemon("stall_classifier_idle_secs_codex"),
        apply: SettingApply::Daemon(ApplyClass::DaemonRestart),
        destructive: false,
    },
    SettingSpec {
        id: SettingId::ClassifierCooldown,
        section: SettingsSection::StallDetection,
        label: "Classifier cooldown",
        summary: "Minimum seconds between classifier nudges to one session.",
        detail: None,
        keywords: &["classifier", "cooldown"],
        kind: SettingKind::Edit,
        owner: SettingOwner::Daemon("stall_classifier_cooldown_secs"),
        apply: SettingApply::Daemon(ApplyClass::DaemonRestart),
        destructive: false,
    },
    SettingSpec {
        id: SettingId::ClassifierMaxPerSession,
        section: SettingsSection::StallDetection,
        label: "Classifier max per session",
        summary: "Cap on classifier nudges over one session's lifetime.",
        detail: None,
        keywords: &["classifier", "max"],
        kind: SettingKind::Edit,
        owner: SettingOwner::Daemon("stall_classifier_max_per_session"),
        apply: SettingApply::Daemon(ApplyClass::DaemonRestart),
        destructive: false,
    },
    SettingSpec {
        id: SettingId::ClassifierConfidenceFloor,
        section: SettingsSection::StallDetection,
        label: "Classifier confidence floor",
        summary: "Minimum classifier confidence before a nudge is sent.",
        detail: None,
        keywords: &["classifier", "confidence"],
        kind: SettingKind::Edit,
        owner: SettingOwner::Daemon("stall_classifier_confidence_floor"),
        apply: SettingApply::Daemon(ApplyClass::Live),
        destructive: false,
    },
    SettingSpec {
        id: SettingId::MemorySystem,
        section: SettingsSection::MemoryDreaming,
        label: "Memory system",
        summary: "OFF stops new memory work live; ON may require a daemon restart.",
        detail: Some(
            "OFF stops automatic indexing and new observation extraction while retaining existing index search, status and file reads. ON requires a daemon restart if no memory worker was started. An existing worker can resume on its next sync.",
        ),
        keywords: &["memory"],
        kind: SettingKind::Bool,
        owner: SettingOwner::Daemon("memory_enabled"),
        apply: SettingApply::Daemon(ApplyClass::LiveOffRestartOn),
        destructive: false,
    },
    SettingSpec {
        id: SettingId::DreamConsolidation,
        section: SettingsSection::MemoryDreaming,
        label: "Dream consolidation",
        summary: "Enables periodic memory consolidation (dreaming).",
        detail: None,
        keywords: &["dream", "consolidation"],
        kind: SettingKind::Bool,
        owner: SettingOwner::Daemon("dream_enabled"),
        apply: SettingApply::Daemon(ApplyClass::Live),
        destructive: false,
    },
    SettingSpec {
        id: SettingId::ObservationThreshold,
        section: SettingsSection::MemoryDreaming,
        label: "Observation threshold",
        summary: "Number of new observations that triggers a consolidation cycle.",
        detail: None,
        keywords: &["dream", "observations", "threshold"],
        kind: SettingKind::Edit,
        owner: SettingOwner::Daemon("dream_observation_threshold"),
        apply: SettingApply::Daemon(ApplyClass::DaemonRestart),
        destructive: false,
    },
    SettingSpec {
        id: SettingId::DreamCooldown,
        section: SettingsSection::MemoryDreaming,
        label: "Dream cooldown",
        summary: "Minimum seconds between consolidation cycles.",
        detail: None,
        keywords: &["dream", "cooldown"],
        kind: SettingKind::Edit,
        owner: SettingOwner::Daemon("dream_cooldown_secs"),
        apply: SettingApply::Daemon(ApplyClass::DaemonRestart),
        destructive: false,
    },
    SettingSpec {
        id: SettingId::DreamIdle,
        section: SettingsSection::MemoryDreaming,
        label: "Dream idle wait",
        summary: "Idle seconds the daemon waits for before starting a consolidation cycle.",
        detail: None,
        keywords: &["dream", "idle"],
        kind: SettingKind::Cycle,
        owner: SettingOwner::Daemon("dream_idle_secs"),
        apply: SettingApply::Daemon(ApplyClass::Live),
        destructive: false,
    },
    SettingSpec {
        id: SettingId::DialecticEngine,
        section: SettingsSection::MemoryDreaming,
        label: "Dialectic engine",
        summary: "Enables the dialectic question engine behind :ask.",
        detail: None,
        keywords: &["dialectic", "ask"],
        kind: SettingKind::Bool,
        owner: SettingOwner::Daemon("dialectic_enabled"),
        apply: SettingApply::Daemon(ApplyClass::DaemonRestart),
        destructive: false,
    },
    SettingSpec {
        id: SettingId::BackgroundQueue,
        section: SettingsSection::Orchestration,
        label: "Background queue",
        summary: "Enables the daemon's background work queue.",
        detail: None,
        keywords: &["queue", "background"],
        kind: SettingKind::Bool,
        owner: SettingOwner::Daemon("queue_enabled"),
        apply: SettingApply::Daemon(ApplyClass::DaemonRestart),
        destructive: false,
    },
    SettingSpec {
        id: SettingId::DagRecoveryControls,
        section: SettingsSection::Orchestration,
        label: "Recursive DAG recovery controls",
        summary: "Allows recovery controls on recursive DAG runs.",
        detail: None,
        keywords: &["dag", "recovery"],
        kind: SettingKind::Bool,
        owner: SettingOwner::Daemon("recursive_dag_recovery_controls_enabled"),
        apply: SettingApply::Daemon(ApplyClass::Live),
        destructive: false,
    },
    SettingSpec {
        id: SettingId::DagSchedulerControls,
        section: SettingsSection::Orchestration,
        label: "Recursive DAG scheduler controls",
        summary: "Allows scheduler controls on recursive DAG runs.",
        detail: None,
        keywords: &["dag", "scheduler"],
        kind: SettingKind::Bool,
        owner: SettingOwner::Daemon("recursive_dag_scheduler_controls_enabled"),
        apply: SettingApply::Daemon(ApplyClass::Live),
        destructive: false,
    },
    SettingSpec {
        id: SettingId::DagCancellationControls,
        section: SettingsSection::Orchestration,
        label: "Recursive DAG cancellation controls",
        summary: "Allows cancelling recursive DAG runs.",
        detail: None,
        keywords: &["dag", "cancel"],
        kind: SettingKind::Bool,
        owner: SettingOwner::Daemon("recursive_dag_cancellation_controls_enabled"),
        apply: SettingApply::Daemon(ApplyClass::Live),
        destructive: false,
    },
    SettingSpec {
        id: SettingId::DagLiveSchedulerControl,
        section: SettingsSection::Orchestration,
        label: "Recursive DAG live scheduler",
        summary: "Allows the live scheduler to drive recursive DAG runs.",
        detail: None,
        keywords: &["dag", "live", "scheduler"],
        kind: SettingKind::Bool,
        owner: SettingOwner::Daemon("recursive_dag_live_scheduler_control_enabled"),
        apply: SettingApply::Daemon(ApplyClass::Live),
        destructive: false,
    },
    SettingSpec {
        id: SettingId::DagRunLeaseTtl,
        section: SettingsSection::Orchestration,
        label: "Recursive DAG run lease TTL",
        summary: "Lease lifetime, in milliseconds, for a recursive DAG run.",
        detail: None,
        keywords: &["dag", "lease", "ttl"],
        kind: SettingKind::Cycle,
        owner: SettingOwner::Daemon("recursive_dag_run_lease_ttl_ms"),
        apply: SettingApply::Daemon(ApplyClass::Live),
        destructive: false,
    },
    SettingSpec {
        id: SettingId::DagMaxConcurrentGraphs,
        section: SettingsSection::Orchestration,
        label: "Recursive DAG max concurrent graphs",
        summary: "Maximum recursive DAG graphs running at once.",
        detail: None,
        keywords: &["dag", "concurrency"],
        kind: SettingKind::Cycle,
        owner: SettingOwner::Daemon("recursive_dag_max_concurrent_graphs"),
        apply: SettingApply::Daemon(ApplyClass::Live),
        destructive: false,
    },
    SettingSpec {
        id: SettingId::GvRenderRecursiveOrigin,
        section: SettingsSection::Orchestration,
        label: "Graph overlay: render recursive origin",
        summary: "Draws each node's recursive-spawn origin edge in the graph overlay (:dag).",
        detail: None,
        keywords: &["graph", "overlay", "dag", "origin"],
        kind: SettingKind::Bool,
        owner: SettingOwner::Daemon("gv_render_recursive_origin"),
        apply: SettingApply::Daemon(ApplyClass::Live),
        destructive: false,
    },
    SettingSpec {
        id: SettingId::GvInfoDashboard,
        section: SettingsSection::Orchestration,
        label: "Graph overlay: info dashboard",
        summary: "Shows the info dashboard panel in the graph overlay (:dag).",
        detail: None,
        keywords: &["graph", "overlay", "dag", "dashboard"],
        kind: SettingKind::Bool,
        owner: SettingOwner::Daemon("gv_info_dashboard"),
        apply: SettingApply::Daemon(ApplyClass::Live),
        destructive: false,
    },
    SettingSpec {
        id: SettingId::TopologyExecutor,
        section: SettingsSection::Orchestration,
        label: "Durable topology executor",
        summary: "Kill switch for the durable topology executor (#634); off, no durable execution advances or recovers and live topology runs use the legacy in-memory runner.",
        detail: None,
        keywords: &["topology", "executor", "durable", "kill switch"],
        kind: SettingKind::Bool,
        owner: SettingOwner::Daemon("topology_executor_enabled"),
        apply: SettingApply::Daemon(ApplyClass::Live),
        destructive: false,
    },
    SettingSpec {
        id: SettingId::TopologyBuildNodes,
        section: SettingsSection::Orchestration,
        label: "Topology build nodes",
        summary: "Most topology build nodes the durable executor runs at once (1-16; the page cycles 1-4).",
        detail: None,
        keywords: &["topology", "build", "concurrency", "nodes"],
        kind: SettingKind::Cycle,
        owner: SettingOwner::Daemon("topology_max_concurrent_build_nodes"),
        apply: SettingApply::Daemon(ApplyClass::Live),
        destructive: false,
    },
    SettingSpec {
        id: SettingId::TopologyBulkFanout,
        section: SettingsSection::Orchestration,
        label: "Topology bulk fan-out on OpenRouter (0 off)",
        summary: "Minimum leaf count for OpenRouter topology bulk fan-out; 0 disables it.",
        detail: None,
        keywords: &["topology", "openrouter", "bulk", "fan-out"],
        kind: SettingKind::Cycle,
        owner: SettingOwner::Daemon("topology_bulk_fanout_min_openrouter"),
        apply: SettingApply::Daemon(ApplyClass::Live),
        destructive: false,
    },
    SettingSpec {
        id: SettingId::CompletedTranscriptCache,
        section: SettingsSection::Orchestration,
        label: "Completed transcript cache (bytes)",
        summary: "Daemon RAM limit for completed transcript reads; 0 disables retention.",
        detail: None,
        keywords: &["transcript", "cache", "memory", "ram"],
        kind: SettingKind::Cycle,
        owner: SettingOwner::Daemon("completed_transcript_cache_max_bytes"),
        apply: SettingApply::Daemon(ApplyClass::Live),
        destructive: false,
    },
    SettingSpec {
        id: SettingId::RsidScopeMemoryHigh,
        section: SettingsSection::Orchestration,
        label: "rsid MemoryHigh (MiB)",
        summary: "Systemd memory pressure threshold for the rsid daemon scope; applies after restarting rsid.",
        detail: None,
        keywords: &["rsid", "memory", "systemd", "scope", "resource"],
        kind: SettingKind::Cycle,
        owner: SettingOwner::Daemon("rsid_scope_memory_high_mib"),
        apply: SettingApply::Daemon(ApplyClass::DaemonRestart),
        destructive: false,
    },
    SettingSpec {
        id: SettingId::RsidScopeMemoryMax,
        section: SettingsSection::Orchestration,
        label: "rsid MemoryMax (MiB)",
        summary: "Hard systemd memory ceiling for the rsid daemon scope; applies after restarting rsid.",
        detail: None,
        keywords: &["rsid", "memory", "systemd", "scope", "resource"],
        kind: SettingKind::Cycle,
        owner: SettingOwner::Daemon("rsid_scope_memory_max_mib"),
        apply: SettingApply::Daemon(ApplyClass::DaemonRestart),
        destructive: false,
    },
    SettingSpec {
        id: SettingId::RsidScopeMemorySwapMax,
        section: SettingsSection::Orchestration,
        label: "rsid MemorySwapMax (MiB)",
        summary: "Swap ceiling for the rsid daemon systemd scope; applies after restarting rsid.",
        detail: None,
        keywords: &["rsid", "swap", "systemd", "scope", "resource"],
        kind: SettingKind::Cycle,
        owner: SettingOwner::Daemon("rsid_scope_memory_swap_max_mib"),
        apply: SettingApply::Daemon(ApplyClass::DaemonRestart),
        destructive: false,
    },
    SettingSpec {
        id: SettingId::RsidScopeCpuWeight,
        section: SettingsSection::Orchestration,
        label: "rsid CPUWeight",
        summary: "Relative CPU weight of the rsid daemon systemd scope; applies after restarting rsid.",
        detail: None,
        keywords: &["rsid", "cpu", "systemd", "scope", "resource"],
        kind: SettingKind::Cycle,
        owner: SettingOwner::Daemon("rsid_scope_cpu_weight"),
        apply: SettingApply::Daemon(ApplyClass::DaemonRestart),
        destructive: false,
    },
    SettingSpec {
        id: SettingId::WorkerScopeMemoryHigh,
        section: SettingsSection::Orchestration,
        label: "Worker slice MemoryHigh (MiB)",
        summary: "Aggregate worker MemoryHigh, initially 55% of host RAM; applies after restarting rsid.",
        detail: None,
        keywords: &["worker", "memory", "systemd", "scope", "resource"],
        kind: SettingKind::Cycle,
        owner: SettingOwner::Daemon("worker_scope_memory_high_mib"),
        apply: SettingApply::Daemon(ApplyClass::DaemonRestart),
        destructive: false,
    },
    SettingSpec {
        id: SettingId::WorkerScopeMemoryMax,
        section: SettingsSection::Orchestration,
        label: "Worker slice MemoryMax (MiB)",
        summary: "Aggregate worker MemoryMax, initially 70% of host RAM; applies after restarting rsid.",
        detail: None,
        keywords: &["worker", "memory", "systemd", "scope", "resource"],
        kind: SettingKind::Cycle,
        owner: SettingOwner::Daemon("worker_scope_memory_max_mib"),
        apply: SettingApply::Daemon(ApplyClass::DaemonRestart),
        destructive: false,
    },
    SettingSpec {
        id: SettingId::WorkerScopeMemorySwapMax,
        section: SettingsSection::Orchestration,
        label: "Worker slice MemorySwapMax (MiB)",
        summary: "Aggregate worker swap ceiling; applies after restarting rsid.",
        detail: None,
        keywords: &["worker", "swap", "systemd", "scope", "resource"],
        kind: SettingKind::Cycle,
        owner: SettingOwner::Daemon("worker_scope_memory_swap_max_mib"),
        apply: SettingApply::Daemon(ApplyClass::DaemonRestart),
        destructive: false,
    },
    SettingSpec {
        id: SettingId::WorkerScopeCpuWeight,
        section: SettingsSection::Orchestration,
        label: "Worker slice CPUWeight",
        summary: "Relative CPU weight of the aggregate worker slice; applies after restarting rsid.",
        detail: None,
        keywords: &["worker", "cpu", "systemd", "scope", "resource"],
        kind: SettingKind::Cycle,
        owner: SettingOwner::Daemon("worker_scope_cpu_weight"),
        apply: SettingApply::Daemon(ApplyClass::DaemonRestart),
        destructive: false,
    },
    SettingSpec {
        id: SettingId::RollingQueueEnabled,
        section: SettingsSection::Orchestration,
        label: "Rolling merge queue",
        summary: "Daemon-owned queue that gates each enqueued source once and fast-forwards it onto rolling; off refuses new enqueues.",
        detail: Some(
            "When on, the current manager or an Epic lead enqueues an accepted source and ends the turn; the daemon runs the lander gate and wakes the owner once with the landed SHA, refusal or failing tests. Turning it off stops new enqueues and claims; entries already gating finish.",
        ),
        keywords: &["merge", "queue", "landing", "rolling", "lander"],
        kind: SettingKind::Bool,
        owner: SettingOwner::Daemon("rolling_queue_enabled"),
        apply: SettingApply::Daemon(ApplyClass::Live),
        destructive: false,
    },
    SettingSpec {
        id: SettingId::DeployDrainEnabled,
        section: SettingsSection::Orchestration,
        label: "Hold new work while a deploy waits",
        summary: "While an agent-requested deploy waits for its quiet point, hold new child launches, child continuations, scheduled child wakes and new agent jobs.",
        detail: Some(
            "Running turns and jobs are never interrupted, and parentless operator sessions and the deploy's caller are never held. Held work runs after the deploy settles; the hold is released at the deploy's max wait even if the hub never went quiet. Held work is listed in AgentGetDaemonInfo (deploy_drain) with the reason deploy_draining. Turn off to let a deploy wait without holding anything.",
        ),
        keywords: &["deploy", "drain", "hold", "quiet", "restart"],
        kind: SettingKind::Bool,
        owner: SettingOwner::Daemon("deploy_drain_enabled"),
        apply: SettingApply::Daemon(ApplyClass::Live),
        destructive: false,
    },
    SettingSpec {
        id: SettingId::RollingQueueBatchSize,
        section: SettingsSection::Orchestration,
        label: "Merge queue batch size",
        summary: "Maximum ready sources merged into one candidate and gated once (1-8); the current runner gates one source at a time.",
        detail: None,
        keywords: &["merge", "queue", "batch", "landing"],
        kind: SettingKind::Cycle,
        owner: SettingOwner::Daemon("rolling_queue_batch_size"),
        apply: SettingApply::Daemon(ApplyClass::Live),
        destructive: false,
    },
    SettingSpec {
        id: SettingId::RollingQueueSpeculationDepth,
        section: SettingsSection::Orchestration,
        label: "Merge queue speculation depth",
        summary: "How many candidates are prepared on top of the batch being gated (0-2).",
        detail: None,
        keywords: &["merge", "queue", "speculation", "landing"],
        kind: SettingKind::Cycle,
        owner: SettingOwner::Daemon("rolling_queue_speculation_depth"),
        apply: SettingApply::Daemon(ApplyClass::Live),
        destructive: false,
    },
    SettingSpec {
        id: SettingId::ProgramHoldWhileChildren,
        section: SettingsSection::Orchestration,
        label: "Hold program wakes while children run",
        summary: "Program-mode masters' due resume wakes wait while their spawned children run, then deliver once per keep-alive window; the wake stays armed meanwhile.",
        detail: Some(
            "The held wake stays enabled and exact, so the no-idle invariant is unchanged. It delivers when the last child settles (the child watch wakes the master) or once when the window elapses; an operator trigger-now bypasses the hold. Non-program wakes are never held.",
        ),
        keywords: &["program", "hold", "wake", "children", "idle"],
        kind: SettingKind::Bool,
        owner: SettingOwner::Daemon("program_hold_while_children_run"),
        apply: SettingApply::Daemon(ApplyClass::Live),
        destructive: false,
    },
    SettingSpec {
        id: SettingId::ChildKeepaliveEnabled,
        section: SettingsSection::Orchestration,
        label: "Child keep-alive valve",
        summary: "Off by default. When on, an idle parent whose children keep running gets one same-session resume per window so it can unblock hung children.",
        detail: Some(
            "The valve inserts at most one daemon-owned one-shot Resume row per window, only for a Completed parent with no other enabled resume wake, pending question, approval, pause or capacity incident, and retires it undelivered if every child settled first. It never launches a Fresh session.",
        ),
        keywords: &["keepalive", "children", "resume", "wake", "hung"],
        kind: SettingKind::Bool,
        owner: SettingOwner::Daemon("child_keepalive_enabled"),
        apply: SettingApply::Daemon(ApplyClass::Live),
        destructive: false,
    },
    SettingSpec {
        id: SettingId::ChildKeepaliveWindow,
        section: SettingsSection::Orchestration,
        label: "Child keep-alive window (s)",
        summary: "Length of the keep-alive and hold window (300-21600 seconds, default 1500).",
        detail: None,
        keywords: &["keepalive", "window", "hold", "children"],
        kind: SettingKind::Cycle,
        owner: SettingOwner::Daemon("child_keepalive_window_secs"),
        apply: SettingApply::Daemon(ApplyClass::Live),
        destructive: false,
    },
    SettingSpec {
        id: SettingId::GovernorBuildSlots,
        section: SettingsSection::Orchestration,
        label: "Build slots",
        summary: "Concurrent cargo build/test runs the resource governor admits (1-16); default 4.",
        detail: None,
        keywords: &["governor", "build", "slots", "cargo"],
        kind: SettingKind::Cycle,
        owner: SettingOwner::Daemon("governor_build_slots"),
        apply: SettingApply::Daemon(ApplyClass::Live),
        destructive: false,
    },
    SettingSpec {
        id: SettingId::GovernorLanderSlots,
        section: SettingsSection::Orchestration,
        label: "Lander slots",
        summary: "Concurrent rsi-rolling-land runs the resource governor admits (1-16); default 5.",
        detail: None,
        keywords: &["governor", "lander", "slots"],
        kind: SettingKind::Cycle,
        owner: SettingOwner::Daemon("governor_lander_slots"),
        apply: SettingApply::Daemon(ApplyClass::Live),
        destructive: false,
    },
    SettingSpec {
        id: SettingId::GovernorMaxLoad,
        section: SettingsSection::Orchestration,
        label: "Governor max load (0 auto)",
        summary: "1-minute load at or above which no new build or lander starts; 0 means 1.25 x cores.",
        detail: None,
        keywords: &["governor", "load"],
        kind: SettingKind::Cycle,
        owner: SettingOwner::Daemon("governor_max_load"),
        apply: SettingApply::Daemon(ApplyClass::Live),
        destructive: false,
    },
    SettingSpec {
        id: SettingId::GovernorMinFreeDisk,
        section: SettingsSection::Orchestration,
        label: "Governor min free disk (GB)",
        summary: "Free space on / below which no new build or lander starts; default 30.",
        detail: None,
        keywords: &["governor", "disk", "free"],
        kind: SettingKind::Cycle,
        owner: SettingOwner::Daemon("governor_min_free_disk_gb"),
        apply: SettingApply::Daemon(ApplyClass::Live),
        destructive: false,
    },
    SettingSpec {
        id: SettingId::GovernorMinAvailMem,
        section: SettingsSection::Orchestration,
        label: "Governor min available memory (GB)",
        summary: "MemAvailable below which no new build or lander starts; default 16.",
        detail: None,
        keywords: &["governor", "memory", "available"],
        kind: SettingKind::Cycle,
        owner: SettingOwner::Daemon("governor_min_avail_mem_gb"),
        apply: SettingApply::Daemon(ApplyClass::Live),
        destructive: false,
    },
    SettingSpec {
        id: SettingId::GovernorMaxWorkersSlice,
        section: SettingsSection::Orchestration,
        label: "Governor max workers-slice memory (GB)",
        summary: "Anonymous + shmem memory of the workers slice at or above which no new build or lander starts (page cache is not counted); default 30.",
        detail: None,
        keywords: &["governor", "memory", "workers", "slice"],
        kind: SettingKind::Cycle,
        owner: SettingOwner::Daemon("governor_max_workers_slice_gb"),
        apply: SettingApply::Daemon(ApplyClass::Live),
        destructive: false,
    },
    SettingSpec {
        id: SettingId::HarnessWebAccess,
        section: SettingsSection::Orchestration,
        label: "Harness web access",
        summary: "Default web_access for Harness sessions: enabled, hosted_only (provider-hosted tools only) or disabled (no web tool advertised or run). A session's own tool policy overrides it.",
        detail: None,
        keywords: &["harness", "web", "search", "policy", "benchmark"],
        kind: SettingKind::Cycle,
        owner: SettingOwner::Daemon("harness_web_access"),
        apply: SettingApply::Daemon(ApplyClass::NextSpawn),
        destructive: false,
    },
    SettingSpec {
        id: SettingId::HarnessEgressMode,
        section: SettingsSection::Orchestration,
        label: "Harness network egress",
        summary: "Default network egress for Harness sessions: deny_private (network tools reach public addresses only; loopback, link-local, cloud metadata and private ranges are refused, after DNS and every redirect) or offline (no network tools; the shell runs in an empty network namespace). In deny_private the shell tool's network is NOT restricted: only offline isolates it. No Harness network tool exists yet (#748), so the fetch guard has no production caller until one lands. A session's own tool policy overrides it.",
        detail: None,
        keywords: &["harness", "network", "egress", "ssrf", "offline", "policy"],
        kind: SettingKind::Cycle,
        owner: SettingOwner::Daemon("harness_egress_mode"),
        apply: SettingApply::Daemon(ApplyClass::NextSpawn),
        destructive: false,
    },
    SettingSpec {
        id: SettingId::HarnessSearchCap,
        section: SettingsSection::Orchestration,
        label: "Harness search call cap",
        summary: "Default cap on hosted web searches per Harness session; 0 is unlimited. Exhaustion returns a typed tool error and the session continues.",
        detail: None,
        keywords: &["harness", "search", "budget", "policy"],
        kind: SettingKind::Cycle,
        owner: SettingOwner::Daemon("harness_max_search_calls"),
        apply: SettingApply::Daemon(ApplyClass::NextSpawn),
        destructive: false,
    },
    SettingSpec {
        id: SettingId::HarnessFetchCap,
        section: SettingsSection::Orchestration,
        label: "Harness fetch call cap",
        summary: "Default cap on hosted web fetches per Harness session; 0 is unlimited.",
        detail: None,
        keywords: &["harness", "fetch", "budget", "policy"],
        kind: SettingKind::Cycle,
        owner: SettingOwner::Daemon("harness_max_fetch_calls"),
        apply: SettingApply::Daemon(ApplyClass::NextSpawn),
        destructive: false,
    },
    SettingSpec {
        id: SettingId::HarnessCompletionGates,
        section: SettingsSection::Orchestration,
        label: "Harness completion gates",
        summary: "Kill switch for Harness completion gates. When off, Harness sessions launched afterwards skip their configured gate commands and record a visible disabled-gate event instead.",
        detail: Some(
            "Read at launch, so running sessions keep the value they started with (#794).",
        ),
        keywords: &["harness", "completion", "gate", "verify", "kill switch"],
        kind: SettingKind::Bool,
        owner: SettingOwner::Daemon("completion_gates_enabled"),
        apply: SettingApply::Daemon(ApplyClass::NextSpawn),
        destructive: false,
    },
    SettingSpec {
        id: SettingId::HarnessOutputCap,
        section: SettingsSection::Orchestration,
        label: "Harness tool output cap (bytes)",
        summary: "Default cap on total tool output bytes per Harness session; 0 is unlimited. Once used up, further tool calls return a typed error.",
        detail: None,
        keywords: &["harness", "bytes", "output", "budget", "policy"],
        kind: SettingKind::Cycle,
        owner: SettingOwner::Daemon("harness_max_result_bytes"),
        apply: SettingApply::Daemon(ApplyClass::NextSpawn),
        destructive: false,
    },
    SettingSpec {
        id: SettingId::HarnessWebCostCap,
        section: SettingsSection::Orchestration,
        label: "Harness web cost cap (micro-USD)",
        summary: "Default cap on estimated hosted web cost per Harness session (1000000 = 1 USD, searches estimated at 0.01 USD); 0 is unlimited.",
        detail: None,
        keywords: &["harness", "cost", "budget", "policy"],
        kind: SettingKind::Cycle,
        owner: SettingOwner::Daemon("harness_max_web_cost_usd_micros"),
        apply: SettingApply::Daemon(ApplyClass::NextSpawn),
        destructive: false,
    },
    SettingSpec {
        id: SettingId::McpDeferredToolThreshold,
        section: SettingsSection::Orchestration,
        label: "MCP deferred tool threshold",
        summary: "Above this many permitted MCP tools, advertise only tool_search; revealed tools become callable.",
        detail: Some(
            "Set to 0 to always defer MCP tools. The maximum is 256, matching the per-session MCP tool cap.",
        ),
        keywords: &["mcp", "tools", "search", "deferred", "harness"],
        kind: SettingKind::Cycle,
        owner: SettingOwner::Daemon("mcp.deferred_tool_threshold"),
        apply: SettingApply::Daemon(ApplyClass::NextSpawn),
        destructive: false,
    },
    SettingSpec {
        id: SettingId::CloudSpendStatus,
        section: SettingsSection::Orchestration,
        label: "Cloud spend",
        summary: "Remote-gate spend estimate for today (UTC) and in total, with the last run, against the caps below.",
        detail: Some(
            "Read from the spend ledger (~/.rsi/cloud/spend.md). The full per-run and per-day view is the GetCloudSpend RPC.",
        ),
        keywords: &["cloud", "spend", "budget", "aws", "gate", "remote"],
        kind: SettingKind::ReadOnly,
        owner: SettingOwner::DaemonState("cloud spend"),
        apply: SettingApply::Daemon(ApplyClass::Live),
        destructive: false,
    },
    SettingSpec {
        id: SettingId::CloudSpendStopLine,
        section: SettingsSection::Orchestration,
        label: "Cloud spend stop line (USD)",
        summary: "Cumulative remote-gate spend at which a new remote run is refused.",
        detail: Some(
            "Whole dollars. Replaces the stop line in the spend ledger header; scripts/cloud-spend.py reads it from the caps file the daemon writes.",
        ),
        keywords: &["cloud", "spend", "budget", "stop", "grant", "gate"],
        kind: SettingKind::Cycle,
        owner: SettingOwner::Daemon("cloud_spend_stop_line_usd"),
        apply: SettingApply::Daemon(ApplyClass::Live),
        destructive: false,
    },
    SettingSpec {
        id: SettingId::CloudSpendDailyCap,
        section: SettingsSection::Orchestration,
        label: "Cloud spend daily cap (USD)",
        summary: "Remote-gate spend per UTC day at which a new remote run is refused.",
        detail: Some(
            "Whole dollars. The AWS Budget rsi-cloud-us-west-1-daily ($15) stays the external backstop.",
        ),
        keywords: &["cloud", "spend", "budget", "daily", "gate"],
        kind: SettingKind::Cycle,
        owner: SettingOwner::Daemon("cloud_spend_daily_cap_usd"),
        apply: SettingApply::Daemon(ApplyClass::Live),
        destructive: false,
    },
    SettingSpec {
        id: SettingId::CodegraphIndexing,
        section: SettingsSection::CodeIntelligence,
        label: "Codegraph indexing",
        summary: "Indexes project source into the code graph used by code-intelligence views.",
        detail: None,
        keywords: &["codegraph", "index", "code intelligence"],
        kind: SettingKind::Bool,
        owner: SettingOwner::Daemon("codegraph_indexing_enabled"),
        apply: SettingApply::Daemon(ApplyClass::Live),
        destructive: false,
    },
    SettingSpec {
        id: SettingId::CodexSandbox,
        section: SettingsSection::ProviderIsolation,
        label: "Codex sandbox",
        summary: "Sandbox policy for new Codex processes: read-only, workspace-write or danger-full-access.",
        detail: None,
        keywords: &["codex", "sandbox"],
        kind: SettingKind::Cycle,
        owner: SettingOwner::Daemon("codex_sandbox_mode"),
        apply: SettingApply::Daemon(ApplyClass::NextSpawn),
        destructive: false,
    },
    SettingSpec {
        id: SettingId::ClaudeConfigIsolation,
        section: SettingsSection::ProviderIsolation,
        label: "Claude project config isolation",
        summary: "Isolation of Claude project config for new Claude processes: off, settings or strict.",
        detail: None,
        keywords: &["claude", "isolation", "config"],
        kind: SettingKind::Cycle,
        owner: SettingOwner::Daemon("claude_config_isolation"),
        apply: SettingApply::Daemon(ApplyClass::NextSpawn),
        destructive: false,
    },
    SettingSpec {
        id: SettingId::VaultEnvCompat,
        section: SettingsSection::ProviderIsolation,
        label: "Vault: legacy env fallback",
        summary: "Allow provider keys from legacy environment variables when no vault credential exists.",
        detail: None,
        keywords: &["vault", "environment", "fallback", "credential"],
        kind: SettingKind::Bool,
        owner: SettingOwner::Daemon("vault.env_compat"),
        apply: SettingApply::Daemon(ApplyClass::Live),
        destructive: false,
    },
    SettingSpec {
        id: SettingId::VaultCheckTtl,
        section: SettingsSection::ProviderIsolation,
        label: "Vault: check TTL",
        summary: "Cache provider key check results for this many seconds.",
        detail: None,
        keywords: &["vault", "check", "ttl", "credential"],
        kind: SettingKind::Cycle,
        owner: SettingOwner::Daemon("vault.check_ttl_secs"),
        apply: SettingApply::Daemon(ApplyClass::Live),
        destructive: false,
    },
    SettingSpec {
        id: SettingId::OpenRouterRoute,
        section: SettingsSection::ProviderIsolation,
        label: "OpenRouter engine",
        summary: "Choose the engine for new OpenRouter sessions. Read or set a model override with :openrouter-route <model> [codex_cli|harness|default].",
        detail: None,
        keywords: &["openrouter", "route", "harness", "codex"],
        kind: SettingKind::Cycle,
        owner: SettingOwner::Daemon("api_route.openrouter"),
        apply: SettingApply::Daemon(ApplyClass::NextSpawn),
        destructive: false,
    },
    SettingSpec {
        id: SettingId::BedrockRoute,
        section: SettingsSection::ProviderIsolation,
        label: "Bedrock engine",
        summary: "Choose the engine for new Bedrock sessions: codex_cli runs GPT in Codex and Claude in Claude Code; harness runs both in RSI's Harness.",
        detail: None,
        keywords: &["bedrock", "route", "harness", "codex", "claude", "aws"],
        kind: SettingKind::Cycle,
        owner: SettingOwner::Daemon("api_route.bedrock"),
        apply: SettingApply::Daemon(ApplyClass::NextSpawn),
        destructive: false,
    },
    SettingSpec {
        id: SettingId::ApiRouteFallback,
        section: SettingsSection::ProviderIsolation,
        label: "API route fallback",
        summary: "Allow a failed Harness preflight to launch OpenRouter through Codex CLI.",
        detail: None,
        keywords: &["openrouter", "route", "fallback", "codex"],
        kind: SettingKind::Bool,
        owner: SettingOwner::Daemon("api_route.fallback"),
        apply: SettingApply::Daemon(ApplyClass::NextSpawn),
        destructive: false,
    },
    SettingSpec {
        id: SettingId::OpenRouterContextBudget,
        section: SettingsSection::ProviderIsolation,
        label: "OpenRouter context budget (0 off)",
        summary: "Live-context tokens at which an OpenRouter session compacts; 0 keeps the model's own limit.",
        detail: None,
        keywords: &[
            "openrouter",
            "context",
            "budget",
            "compact",
            "cost",
            "tokens",
        ],
        kind: SettingKind::Cycle,
        owner: SettingOwner::Daemon("openrouter_context_budget_tokens"),
        apply: SettingApply::Daemon(ApplyClass::NextSpawn),
        destructive: false,
    },
    SettingSpec {
        id: SettingId::HarnessMaxIterations,
        section: SettingsSection::ProviderIsolation,
        label: "Harness iterations per turn",
        summary: "Agent-loop steps one Harness or OpenRouter turn may take before it wraps up (10-1000).",
        detail: None,
        keywords: &[
            "harness",
            "openrouter",
            "iterations",
            "steps",
            "turn",
            "cap",
        ],
        kind: SettingKind::Cycle,
        owner: SettingOwner::Daemon("harness_max_iterations_per_turn"),
        apply: SettingApply::Daemon(ApplyClass::NextSpawn),
        destructive: false,
    },
    SettingSpec {
        id: SettingId::SandboxStorageStatus,
        section: SettingsSection::SandboxStorage,
        label: "Sandbox storage",
        summary: "Shows sandbox storage usage; R refreshes it.",
        detail: None,
        keywords: &["sandbox", "storage", "disk"],
        kind: SettingKind::ReadOnly,
        owner: SettingOwner::DaemonState("sandbox storage"),
        apply: SettingApply::Daemon(ApplyClass::Live),
        destructive: false,
    },
    SettingSpec {
        id: SettingId::CacheReclaim,
        section: SettingsSection::SandboxStorage,
        label: "Sandbox cache reclaim",
        summary: "Enables periodic reclamation of sandbox build caches.",
        detail: None,
        keywords: &["reclaim", "cache"],
        kind: SettingKind::Bool,
        owner: SettingOwner::Daemon("sandbox_build_cache_reclaim_enabled"),
        apply: SettingApply::Daemon(ApplyClass::Live),
        destructive: false,
    },
    SettingSpec {
        id: SettingId::CacheReclaimTtl,
        section: SettingsSection::SandboxStorage,
        label: "Cache reclaim TTL",
        summary: "Seconds a build cache may live before it can be reclaimed.",
        detail: None,
        keywords: &["reclaim", "ttl"],
        kind: SettingKind::Edit,
        owner: SettingOwner::Daemon("sandbox_build_cache_reclaim_ttl_secs"),
        apply: SettingApply::Daemon(ApplyClass::Live),
        destructive: false,
    },
    SettingSpec {
        id: SettingId::CacheReclaimInterval,
        section: SettingsSection::SandboxStorage,
        label: "Cache reclaim interval",
        summary: "Seconds between periodic reclaim passes.",
        detail: None,
        keywords: &["reclaim", "interval"],
        kind: SettingKind::Edit,
        owner: SettingOwner::Daemon("sandbox_build_cache_reclaim_interval_secs"),
        apply: SettingApply::Daemon(ApplyClass::Live),
        destructive: false,
    },
    SettingSpec {
        id: SettingId::CachePressureHigh,
        section: SettingsSection::SandboxStorage,
        label: "Cache pressure high",
        summary: "Disk-use percent that arms reclaim pressure.",
        detail: None,
        keywords: &["reclaim", "watermark", "high"],
        kind: SettingKind::Edit,
        owner: SettingOwner::Daemon("sandbox_build_cache_reclaim_high_watermark_pct"),
        apply: SettingApply::Daemon(ApplyClass::Live),
        destructive: false,
    },
    SettingSpec {
        id: SettingId::CachePressureLow,
        section: SettingsSection::SandboxStorage,
        label: "Cache pressure low",
        summary: "Disk-use percent that clears reclaim pressure.",
        detail: None,
        keywords: &["reclaim", "watermark", "low"],
        kind: SettingKind::Edit,
        owner: SettingOwner::Daemon("sandbox_build_cache_reclaim_low_watermark_pct"),
        apply: SettingApply::Daemon(ApplyClass::Live),
        destructive: false,
    },
    SettingSpec {
        id: SettingId::CacheReclaimPassLimit,
        section: SettingsSection::SandboxStorage,
        label: "Cache reclaim pass limit",
        summary: "Maximum caches deleted in one reclaim pass.",
        detail: None,
        keywords: &["reclaim", "limit"],
        kind: SettingKind::Edit,
        owner: SettingOwner::Daemon("sandbox_build_cache_reclaim_max_candidates"),
        apply: SettingApply::Daemon(ApplyClass::Live),
        destructive: false,
    },
    SettingSpec {
        id: SettingId::AgentBuildJobs,
        section: SettingsSection::SandboxStorage,
        label: "Agent build jobs",
        summary: "Cargo jobs per build in a sandboxed session.",
        detail: None,
        keywords: &["cargo", "build", "jobs"],
        kind: SettingKind::Edit,
        owner: SettingOwner::Daemon("agent_build_jobs"),
        apply: SettingApply::Daemon(ApplyClass::NextSpawn),
        destructive: false,
    },
    SettingSpec {
        id: SettingId::AgentBuildLineTables,
        section: SettingsSection::SandboxStorage,
        label: "Agent line tables",
        summary: "Use line-tables-only debuginfo for sandbox builds.",
        detail: None,
        keywords: &["cargo", "debug"],
        kind: SettingKind::Bool,
        owner: SettingOwner::Daemon("agent_build_line_tables_only"),
        apply: SettingApply::Daemon(ApplyClass::NextSpawn),
        destructive: false,
    },
    SettingSpec {
        id: SettingId::AgentBuildSccache,
        section: SettingsSection::SandboxStorage,
        label: "Worker sccache",
        summary: "Use sccache and disable incremental builds for fresh workers.",
        detail: None,
        keywords: &["cargo", "cache", "worker"],
        kind: SettingKind::Bool,
        owner: SettingOwner::Daemon("agent_build_sccache_enabled"),
        apply: SettingApply::Daemon(ApplyClass::NextSpawn),
        destructive: false,
    },
    SettingSpec {
        id: SettingId::AgentBuildSccacheSize,
        section: SettingsSection::SandboxStorage,
        label: "sccache cap (GiB)",
        summary: "Machine local sccache disk cap for new worker processes.",
        detail: None,
        keywords: &["cargo", "cache", "disk"],
        kind: SettingKind::Edit,
        owner: SettingOwner::Daemon("agent_build_sccache_cache_gib"),
        apply: SettingApply::Daemon(ApplyClass::NextSpawn),
        destructive: false,
    },
    SettingSpec {
        id: SettingId::AgentBuildSlots,
        section: SettingsSection::SandboxStorage,
        label: "Machine build slots",
        summary: "Maximum concurrent compiler processes across agent sandboxes.",
        detail: None,
        keywords: &["cargo", "build", "slots"],
        kind: SettingKind::Edit,
        owner: SettingOwner::Daemon("agent_build_slots"),
        apply: SettingApply::Daemon(ApplyClass::NextSpawn),
        destructive: false,
    },
    SettingSpec {
        id: SettingId::PreviewReclaim,
        section: SettingsSection::SandboxStorage,
        label: "Preview cache reclaim",
        summary: "Runs a dry-run reclaim pass and reports what it would free.",
        detail: None,
        keywords: &["reclaim", "preview", "dry run"],
        kind: SettingKind::Action,
        owner: SettingOwner::DaemonState("sandbox storage"),
        apply: SettingApply::Daemon(ApplyClass::Live),
        destructive: false,
    },
    SettingSpec {
        id: SettingId::ReclaimNow,
        section: SettingsSection::SandboxStorage,
        label: "Reclaim sandbox caches now",
        summary: "Runs a real reclaim pass now and reports the result.",
        detail: None,
        keywords: &["reclaim", "now"],
        kind: SettingKind::Action,
        owner: SettingOwner::DaemonState("sandbox storage"),
        apply: SettingApply::Daemon(ApplyClass::Live),
        destructive: true,
    },
    SettingSpec {
        id: SettingId::SourceWorktreeSettlement,
        section: SettingsSection::SandboxStorage,
        label: "Source-worktree settlement",
        summary: "Opens the source-worktree settlement audit, which can delete settled source worktrees after confirmation.",
        detail: None,
        keywords: &["settlement", "worktree"],
        kind: SettingKind::Action,
        owner: SettingOwner::DaemonState("source-worktree settlement"),
        apply: SettingApply::Daemon(ApplyClass::Live),
        destructive: true,
    },
    SettingSpec {
        id: SettingId::SandboxMaxSourceRoots,
        section: SettingsSection::SandboxStorage,
        label: "Maximum sandbox roots",
        summary: "Maximum direct source roots under the sandbox base before a new allocation is refused.",
        detail: None,
        keywords: &["sandbox", "allocation", "count", "capacity"],
        kind: SettingKind::Cycle,
        owner: SettingOwner::Daemon("sandbox_max_source_roots"),
        apply: SettingApply::Daemon(ApplyClass::Live),
        destructive: false,
    },
    SettingSpec {
        id: SettingId::SandboxMinFreeGib,
        section: SettingsSection::SandboxStorage,
        label: "Minimum free space (GiB)",
        summary: "Free filesystem space required before a new sandbox is allocated.",
        detail: None,
        keywords: &["sandbox", "allocation", "disk", "capacity"],
        kind: SettingKind::Cycle,
        owner: SettingOwner::Daemon("sandbox_min_free_gib"),
        apply: SettingApply::Daemon(ApplyClass::Live),
        destructive: false,
    },
    SettingSpec {
        id: SettingId::ArchivedSandboxPurge,
        section: SettingsSection::SandboxStorage,
        label: "Purge archived sandboxes",
        summary: "Every 10 minutes deletes up to 32 archived sandboxes whose commits are on rolling, or preserved on origin after 24 h unlanded.",
        detail: None,
        keywords: &["sandbox", "purge", "archive", "disk"],
        kind: SettingKind::Bool,
        owner: SettingOwner::Daemon("archived_sandbox_purge_enabled"),
        apply: SettingApply::Daemon(ApplyClass::Live),
        destructive: true,
    },
    SettingSpec {
        id: SettingId::ClaudeHooks,
        section: SettingsSection::ClaudeHooks,
        label: "Claude hooks",
        summary: "Claude Code hooks from settings.json; Enter edits, a adds, d deletes.",
        detail: None,
        keywords: &["hooks", "claude"],
        kind: SettingKind::DynamicList,
        owner: SettingOwner::File("~/.claude/settings.json"),
        apply: SettingApply::Immediate,
        destructive: false,
    },
    SettingSpec {
        id: SettingId::ClaudeSkills,
        section: SettingsSection::ClaudeSkills,
        label: "Claude skills",
        summary: "Claude user skills; Enter previews, e enables or disables.",
        detail: None,
        keywords: &["skills", "claude"],
        kind: SettingKind::DynamicList,
        owner: SettingOwner::File("~/.claude/skills/"),
        apply: SettingApply::Immediate,
        destructive: false,
    },
    SettingSpec {
        id: SettingId::SignalBridge,
        section: SettingsSection::MessageBridges,
        label: "Signal bridge",
        summary: "Signal bridge connection, account and sender allowlist.",
        detail: None,
        keywords: &["signal", "bridge"],
        kind: SettingKind::Edit,
        owner: SettingOwner::File("signal.toml"),
        apply: SettingApply::Immediate,
        destructive: false,
    },
    SettingSpec {
        id: SettingId::ImessageBridge,
        section: SettingsSection::MessageBridges,
        label: "iMessage bridge",
        summary: "iMessage bridge connection, account and sender allowlist.",
        detail: None,
        keywords: &["imessage", "bridge"],
        kind: SettingKind::Edit,
        owner: SettingOwner::File("imessage.toml"),
        apply: SettingApply::Immediate,
        destructive: false,
    },
    SettingSpec {
        id: SettingId::SatelliteRegistry,
        section: SettingsSection::Satellites,
        label: "Satellite registry",
        summary: "Manage paired peers and local socket links; browse cached remote sessions.",
        detail: Some(
            "Peer sockets carry full operator authority. Verify SSH trust and the peer installation ID before enabling reads.",
        ),
        keywords: &["satellite", "peer", "remote", "socket", "registry"],
        kind: SettingKind::Action,
        owner: SettingOwner::DaemonState("satellite_registry"),
        apply: SettingApply::Immediate,
        destructive: false,
    },
    SettingSpec {
        id: SettingId::SatellitePolling,
        section: SettingsSection::Satellites,
        label: "Satellite polling",
        summary: "Global switch for hub satellite polling; off keeps registry rows and cached observations.",
        detail: Some(
            "Turning this off stops every registry-driven probe and link inspection on the next tick; paired peers and cached observations are retained and polling resumes when it is turned back on.",
        ),
        keywords: &["satellite", "poll", "polling", "peer", "network"],
        kind: SettingKind::Bool,
        owner: SettingOwner::Daemon("satellite_polling_enabled"),
        apply: SettingApply::Daemon(ApplyClass::Live),
        destructive: false,
    },
    SettingSpec {
        id: SettingId::McpServerConfigurations,
        section: SettingsSection::McpServers,
        label: "MCP servers",
        summary: "Daemon-configured MCP server definitions and credential metadata; a adds, Enter edits, t enables or disables.",
        detail: Some(
            "Server definitions remain disabled until enabled. Credentials are entered through a masked form and stored only in the operator vault.",
        ),
        keywords: &["mcp", "server", "credential", "vault"],
        kind: SettingKind::DynamicList,
        owner: SettingOwner::DaemonState("mcp servers"),
        apply: SettingApply::Immediate,
        destructive: false,
    },
];

#[cfg(test)]
#[allow(clippy::expect_used)] // Test fixtures fail loudly on a broken invariant.
mod tests {
    use super::*;
    use rsi_common::daemon_config_catalog::{
        DAEMON_CONFIG_FIELDS, OperatorSurface, daemon_field_spec,
    };
    use std::collections::BTreeMap;

    /// Sources of TUI files that may be named as an `Elsewhere` writer.
    const WRITER_SOURCES: &[(&str, &str)] = &[
        (
            "action_handler/daemon_config.rs",
            include_str!("action_handler/daemon_config.rs"),
        ),
        ("overlay/graph.rs", include_str!("overlay/graph.rs")),
    ];

    /// Whether `source` contains a daemon-config write call whose arguments
    /// name `field` (the write starts within the call's first 300 bytes).
    fn writes_field(source: &str, field: &str) -> bool {
        let quoted = format!("\"{field}\"");
        ["update_daemon_config(", "sync_config_field("]
            .iter()
            .flat_map(|call| source.match_indices(call))
            .any(|(offset, _)| {
                let end = (offset + 300).min(source.len());
                source
                    .get(offset..end)
                    .is_some_and(|window| window.contains(&quoted))
            })
    }

    /// The settings view shows one rail row per group and one tab per
    /// section: every group owns at least one tab, the tabs of all groups
    /// together are exactly `SettingsSection::ALL` in page order, and a
    /// group opens on its first tab.
    #[test]
    fn group_tabs_partition_every_section_in_page_order() {
        let mut tabs = Vec::new();
        for (position, group) in SettingsGroup::ALL.iter().enumerate() {
            let sections: Vec<SettingsSection> = group.sections().collect();
            assert!(!sections.is_empty(), "{group:?} owns at least one tab");
            assert_eq!(group.first_section(), sections[0]);
            assert_eq!(group.position(), position);
            assert!(sections.iter().all(|section| section.group() == *group));
            tabs.extend(sections);
        }
        assert_eq!(tabs, SettingsSection::ALL.to_vec());
        assert_eq!(
            SettingsGroup::AgentAutomation
                .sections()
                .collect::<Vec<_>>(),
            vec![
                SettingsSection::RetriesRecovery,
                SettingsSection::StallDetection,
                SettingsSection::MemoryDreaming,
                SettingsSection::Orchestration,
                SettingsSection::CodeIntelligence,
            ]
        );
    }

    /// The write proof recognises real TUI writers.
    #[test]
    fn writer_proof_recognises_real_daemon_config_writes() {
        let (_, source) = WRITER_SOURCES[0];
        assert!(writes_field(source, "system_prompt_preset"));
        assert!(writes_field(source, "title_model_local"));
    }

    /// T10 (rsi side): every catalogued `SettingsPage` daemon field is written
    /// by exactly one settings row; every `Elsewhere` field names a pinned TUI
    /// file proven to write it; every field without a TUI editor is listed in
    /// the manual's gap table with how it is set today; and each daemon row's
    /// apply class is the catalog's class.
    #[test]
    fn every_daemon_catalog_field_has_one_surface() {
        let mut claims: BTreeMap<&str, Vec<SettingId>> = BTreeMap::new();
        for spec in SETTINGS {
            for field in spec.owner.daemon_fields() {
                claims.entry(field).or_default().push(spec.id);
                let catalog = daemon_field_spec(field)
                    .unwrap_or_else(|| panic!("{:?} writes uncatalogued field {field}", spec.id));
                assert_eq!(
                    spec.apply,
                    SettingApply::Daemon(catalog.apply),
                    "{:?} apply class matches the catalog for {field}",
                    spec.id
                );
            }
        }
        let gap_rows: Vec<Vec<String>> = crate::manual::model::build_manual()
            .daemon_field_gap_rows()
            .to_vec();
        for catalog in DAEMON_CONFIG_FIELDS {
            match catalog.operator_surface {
                OperatorSurface::SettingsPage => assert_eq!(
                    claims.get(catalog.field).map(Vec::len),
                    Some(1),
                    "{} has exactly one settings row",
                    catalog.field
                ),
                OperatorSurface::Elsewhere { surface, writer } => {
                    let source = WRITER_SOURCES
                        .iter()
                        .find(|(path, _)| *path == writer)
                        .map_or_else(
                            || panic!("{} writer {writer} is pinned", catalog.field),
                            |(_, source)| *source,
                        );
                    assert!(
                        writes_field(source, catalog.field),
                        "{} is written by {surface} ({writer})",
                        catalog.field
                    );
                }
                OperatorSurface::NoTuiEditor { set_via, tracking } => {
                    assert!(
                        gap_rows.contains(&vec![
                            crate::manual::model::code(catalog.field),
                            set_via.to_string(),
                            tracking.to_string(),
                        ]),
                        "{} is listed in the manual's no-editor table",
                        catalog.field
                    );
                }
            }
        }
    }

    /// Review 39917a88: the Max retries row states that default launches get
    /// zero automatic retries (rsid retry_policy.rs:14-18) and is classed as
    /// stored-but-not-applied, not as a live control.
    #[test]
    fn max_retries_row_states_default_launches_get_zero_retries() {
        let spec = SETTINGS
            .iter()
            .find(|spec| spec.id == SettingId::MaxRetries)
            .expect("max retries row");
        assert_eq!(spec.apply, SettingApply::Daemon(ApplyClass::NotApplied));
        assert_eq!(spec.kind, SettingKind::ReadOnly);
        assert!(spec.summary.contains("not applied to default launches"));
        assert!(
            spec.detail
                .is_some_and(|detail| detail.contains("zero automatic retries")
                    && detail.contains("retry_policy.rs:14-18"))
        );
    }

    #[test]
    fn every_setting_has_nonempty_summary() {
        for spec in SETTINGS {
            assert!(!spec.label.is_empty(), "{:?} has a label", spec.id);
            assert!(!spec.summary.is_empty(), "{:?} has a summary", spec.id);
            assert!(!spec.keywords.is_empty(), "{:?} has keywords", spec.id);
        }
        for section in SettingsSection::ALL {
            assert!(!section.label().is_empty() && !section.summary().is_empty());
        }
        for group in SettingsGroup::ALL {
            assert!(!group.label().is_empty() && !group.summary().is_empty());
        }
    }

    #[test]
    fn settings_are_in_section_page_order() {
        let order: Vec<usize> = SETTINGS
            .iter()
            .map(|spec| {
                SettingsSection::ALL
                    .iter()
                    .position(|section| *section == spec.section)
                    .expect("section listed in ALL")
            })
            .collect();
        let mut sorted = order.clone();
        sorted.sort_unstable();
        assert_eq!(order, sorted);
        let groups: Vec<SettingsGroup> = SettingsSection::ALL
            .iter()
            .map(|section| section.group())
            .collect();
        let mut sorted_groups = groups.clone();
        sorted_groups.sort_unstable();
        assert_eq!(groups, sorted_groups, "sections are grouped in rail order");
    }

    #[test]
    fn every_section_has_at_least_one_setting() {
        for section in SettingsSection::ALL {
            assert!(
                SETTINGS.iter().any(|spec| spec.section == *section),
                "{section:?} has a row"
            );
        }
    }

    /// #1013: page order is the declaration order of `SETTINGS`; there is no
    /// positional ordinal table. Every id is declared exactly once.
    #[test]
    fn setting_ids_are_declared_exactly_once() {
        assert_eq!(SETTINGS.len(), SettingId::ALL.len());
        for id in SettingId::ALL {
            let declared = SETTINGS.iter().filter(|spec| spec.id == *id).count();
            assert_eq!(declared, 1, "{id:?} is declared once in SETTINGS");
        }
    }

    /// #1013: an entry's position is derived from declaration order, so
    /// inserting a setting shifts no other entry's relative order (two
    /// branches adding settings never renumber each other's rows).
    #[test]
    fn declaration_order_is_stable_when_an_entry_is_added() {
        let ids: Vec<SettingId> = SETTINGS.iter().map(|spec| spec.id).collect();
        let position = |order: &[SettingId], id: SettingId| {
            order.iter().position(|candidate| *candidate == id).unwrap()
        };
        let removed = ids[ids.len() / 2];
        let without: Vec<SettingId> = ids.iter().copied().filter(|id| *id != removed).collect();
        for (a, b) in ids.iter().zip(ids.iter().skip(1)) {
            if *a == removed || *b == removed {
                continue;
            }
            assert_eq!(
                position(&without, *a) < position(&without, *b),
                position(&ids, *a) < position(&ids, *b),
                "adding {removed:?} keeps {a:?} before {b:?}"
            );
        }
        // Order matches the slice itself, not any id-derived number.
        for (position, spec) in SETTINGS.iter().enumerate() {
            assert_eq!(
                SETTINGS.iter().position(|s| s.id == spec.id),
                Some(position)
            );
        }
    }

    /// VP-150: the navigator and card rows document every real option.
    #[test]
    fn navigator_and_card_details_list_every_option() {
        let detail = |id: SettingId| {
            SETTINGS
                .iter()
                .find(|spec| spec.id == id)
                .and_then(|spec| spec.detail)
                .expect("row has detail")
        };
        for preset in [
            crate::types::NavigatorPreset::Dense,
            crate::types::NavigatorPreset::Operations,
            crate::types::NavigatorPreset::Cost,
        ] {
            assert!(detail(SettingId::NavigatorPreset).contains(preset.label()));
        }
        for column in crate::types::NavigatorOptionalColumn::ALL {
            assert!(detail(SettingId::NavigatorColumns).contains(column.label()));
        }
        for field in crate::types::CardField::ALL {
            assert!(detail(SettingId::CardFields).contains(field.label()));
        }
    }
}
