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
    ];

    #[must_use]
    pub const fn group(self) -> SettingsGroup {
        match self {
            Self::ThemeColors | Self::Screen => SettingsGroup::Appearance,
            Self::SessionList | Self::TranscriptDefaults | Self::InputPrompts => {
                SettingsGroup::Workspace
            }
            Self::ModelRoles | Self::ApiProviders | Self::SystemPrompt => SettingsGroup::Models,
            Self::ModelControl | Self::Budgets | Self::Usage => SettingsGroup::SafetyAndSpend,
            Self::RetriesRecovery
            | Self::StallDetection
            | Self::MemoryDreaming
            | Self::Orchestration
            | Self::CodeIntelligence => SettingsGroup::AgentAutomation,
            Self::ProviderIsolation
            | Self::SandboxStorage
            | Self::ClaudeHooks
            | Self::ClaudeSkills => SettingsGroup::ProvidersAndSandboxes,
            Self::MessageBridges => SettingsGroup::Integrations,
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
        }
    }

    #[must_use]
    pub const fn summary(self) -> &'static str {
        match self {
            Self::ThemeColors => "The built-in theme, per-role overrides and legacy colors.",
            Self::Screen => "Text-area background and animation styles.",
            Self::SessionList => "What the session navigator and cards show.",
            Self::TranscriptDefaults => "What a newly opened transcript shows.",
            Self::InputPrompts => "How typing and submitting behave.",
            Self::ModelRoles => "Which model does which job.",
            Self::ApiProviders => "OpenAI-compatible endpoints for model selection.",
            Self::SystemPrompt => "The system-prompt preset applied to launches.",
            Self::ModelControl => "Stop or limit model spend daemon-wide.",
            Self::Budgets => "Model budget policies.",
            Self::Usage => "Usage telemetry and stop/cancel actions.",
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
        }
    }
}

/// One logical settings row. Dynamic lists (hooks, skills, providers,
/// budgets, stats) are one variant each.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum SettingId {
    BuiltInTheme,
    ThemeRoles,
    LegacyColors,
    ResetTheme,
    TextAreaBackground,
    BackgroundColor,
    FormulationAnimation,
    ActivityIndicator,
    NavigatorPreset,
    NavigatorColumns,
    CardFields,
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
    RetryOnFailure,
    MaxRetries,
    RetryMaxBackoff,
    RetryOnStall,
    ReconciliationLoop,
    ContextRotation,
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
    RsidScopeMemoryHigh,
    RsidScopeMemoryMax,
    RsidScopeMemorySwapMax,
    RsidScopeCpuWeight,
    CodegraphIndexing,
    CodexSandbox,
    ClaudeConfigIsolation,
    SandboxStorageStatus,
    CacheReclaim,
    CacheReclaimTtl,
    CacheReclaimInterval,
    CachePressureHigh,
    CachePressureLow,
    CacheReclaimPassLimit,
    PreviewReclaim,
    ReclaimNow,
    SourceWorktreeSettlement,
    ClaudeHooks,
    ClaudeSkills,
    SignalBridge,
    ImessageBridge,
}

impl SettingId {
    /// Exhaustive position in page order (guards `SETTINGS` against omissions).
    #[must_use]
    pub const fn index(self) -> usize {
        match self {
            Self::BuiltInTheme => 0,
            Self::ThemeRoles => 1,
            Self::LegacyColors => 2,
            Self::ResetTheme => 3,
            Self::TextAreaBackground => 4,
            Self::BackgroundColor => 5,
            Self::FormulationAnimation => 6,
            Self::ActivityIndicator => 7,
            Self::NavigatorPreset => 8,
            Self::NavigatorColumns => 9,
            Self::CardFields => 10,
            Self::ShowSystemEvents => 11,
            Self::ShowThinkingEvents => 12,
            Self::HideToolResults => 13,
            Self::SubmitOnEnter => 14,
            Self::AutoOpenQuestionPanel => 15,
            Self::PromptCompiler => 16,
            Self::DefaultModel => 17,
            Self::TitleModel => 18,
            Self::PromptCompilerModel => 19,
            Self::MemoryModel => 20,
            Self::DreamModel => 21,
            Self::ClassifierModel => 22,
            Self::ApiProviders => 23,
            Self::SystemPromptPreset => 24,
            Self::ModelControlMode => 25,
            Self::EmergencyStop => 26,
            Self::MaxChildEffort => 27,
            Self::BudgetPolicies => 28,
            Self::UsageStats => 29,
            Self::UsageActions => 30,
            Self::RetryOnFailure => 31,
            Self::MaxRetries => 32,
            Self::RetryMaxBackoff => 33,
            Self::RetryOnStall => 34,
            Self::ReconciliationLoop => 35,
            Self::ContextRotation => 36,
            Self::StallDetection => 37,
            Self::StallClassifier => 38,
            Self::ClassifierIdleClaude => 39,
            Self::ClassifierIdleCodex => 40,
            Self::ClassifierCooldown => 41,
            Self::ClassifierMaxPerSession => 42,
            Self::ClassifierConfidenceFloor => 43,
            Self::MemorySystem => 44,
            Self::DreamConsolidation => 45,
            Self::ObservationThreshold => 46,
            Self::DreamCooldown => 47,
            Self::DreamIdle => 48,
            Self::DialecticEngine => 49,
            Self::BackgroundQueue => 50,
            Self::DagRecoveryControls => 51,
            Self::DagSchedulerControls => 52,
            Self::DagCancellationControls => 53,
            Self::DagLiveSchedulerControl => 54,
            Self::DagRunLeaseTtl => 55,
            Self::DagMaxConcurrentGraphs => 56,
            Self::GvRenderRecursiveOrigin => 57,
            Self::GvInfoDashboard => 58,
            Self::TopologyExecutor => 59,
            Self::TopologyBuildNodes => 60,
            Self::RsidScopeMemoryHigh => 61,
            Self::RsidScopeMemoryMax => 62,
            Self::RsidScopeMemorySwapMax => 63,
            Self::RsidScopeCpuWeight => 64,
            Self::CodegraphIndexing => 65,
            Self::CodexSandbox => 66,
            Self::ClaudeConfigIsolation => 67,
            Self::SandboxStorageStatus => 68,
            Self::CacheReclaim => 69,
            Self::CacheReclaimTtl => 70,
            Self::CacheReclaimInterval => 71,
            Self::CachePressureHigh => 72,
            Self::CachePressureLow => 73,
            Self::CacheReclaimPassLimit => 74,
            Self::PreviewReclaim => 75,
            Self::ReclaimNow => 76,
            Self::SourceWorktreeSettlement => 77,
            Self::ClaudeHooks => 78,
            Self::ClaudeSkills => 79,
            Self::SignalBridge => 80,
            Self::ImessageBridge => 81,
        }
    }
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
        summary: "Sets how long the in-progress message grow animation runs.",
        detail: None,
        keywords: &["animation", "formulation"],
        kind: SettingKind::Cycle,
        owner: SettingOwner::Tui,
        apply: SettingApply::Immediate,
        destructive: false,
    },
    SettingSpec {
        id: SettingId::ActivityIndicator,
        section: SettingsSection::Screen,
        label: "Activity indicator",
        summary: "Chooses the working-indicator style: Semantic, Rainbow Classic or Rainbow Compact.",
        detail: None,
        keywords: &["indicator", "spinner", "rainbow"],
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
        summary: "Turns individual optional navigator columns on or off.",
        detail: Some(
            "Optional columns: Navigator age, Navigator model / effort, Navigator retry, Navigator cost, Navigator work, Navigator rotation, Navigator project, Navigator created. Required columns cannot be hidden.",
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
        summary: "Rotates a session into a fresh context near its limit, carrying a handoff forward.",
        detail: None,
        keywords: &["rotation", "context"],
        kind: SettingKind::Bool,
        owner: SettingOwner::Daemon("context_rotation_enabled"),
        apply: SettingApply::Daemon(ApplyClass::PartialLive),
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
        summary: "Enables the daemon memory subsystem (observation extraction and search).",
        detail: None,
        keywords: &["memory"],
        kind: SettingKind::Bool,
        owner: SettingOwner::Daemon("memory_enabled"),
        apply: SettingApply::Daemon(ApplyClass::DaemonRestart),
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

    #[test]
    fn setting_ids_are_unique_and_in_index_order() {
        for (position, spec) in SETTINGS.iter().enumerate() {
            assert_eq!(spec.id.index(), position, "{:?} is at its index", spec.id);
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
