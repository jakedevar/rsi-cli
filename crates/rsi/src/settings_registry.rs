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
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize, serde::Deserialize,
)]
#[serde(rename_all = "snake_case")]
pub enum SettingsGroup {
    Appearance,
    Workspace,
    Models,
    SafetyAndSpend,
    AgentAutomation,
    ProvidersAndSandboxes,
    HostAndBuild,
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
        Self::HostAndBuild,
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
            Self::HostAndBuild => "HOST & BUILD",
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
            Self::HostAndBuild => "What do agents, builds and the merge queue use of this machine?",
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
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize, serde::Deserialize,
)]
#[serde(rename_all = "snake_case")]
pub enum SettingsSection {
    ThemeColors,
    Screen,
    SessionList,
    TranscriptDefaults,
    InputPrompts,
    EditingMode,
    ModelRoles,
    ApiProviders,
    SystemPrompt,
    ModelControl,
    Budgets,
    Usage,
    ProviderKeys,
    CloudSpend,
    RetriesRecovery,
    ContextRotation,
    StallDetection,
    MemoryDreaming,
    Scheduling,
    RecursiveDag,
    CodeIntelligence,
    ProviderIsolation,
    ProviderSubagents,
    ProviderSkills,
    ApiRoutes,
    HarnessTools,
    ClaudeHooks,
    ClaudeSkills,
    ResourceLimits,
    BuildGovernor,
    MergeQueue,
    SandboxStorage,
    ColdStorage,
    BuildCache,
    AgentBuilds,
    MessageBridges,
    Satellites,
    Remote,
    McpServers,
}

impl SettingsSection {
    pub const ALL: &'static [Self] = &[
        Self::ThemeColors,
        Self::Screen,
        Self::SessionList,
        Self::TranscriptDefaults,
        Self::InputPrompts,
        Self::EditingMode,
        Self::ModelRoles,
        Self::ApiProviders,
        Self::SystemPrompt,
        Self::ModelControl,
        Self::Budgets,
        Self::Usage,
        Self::ProviderKeys,
        Self::CloudSpend,
        Self::RetriesRecovery,
        Self::ContextRotation,
        Self::StallDetection,
        Self::MemoryDreaming,
        Self::Scheduling,
        Self::RecursiveDag,
        Self::CodeIntelligence,
        Self::ProviderIsolation,
        Self::ProviderSubagents,
        Self::ProviderSkills,
        Self::ApiRoutes,
        Self::HarnessTools,
        Self::ClaudeHooks,
        Self::ClaudeSkills,
        Self::ResourceLimits,
        Self::BuildGovernor,
        Self::MergeQueue,
        Self::SandboxStorage,
        Self::ColdStorage,
        Self::BuildCache,
        Self::AgentBuilds,
        Self::MessageBridges,
        Self::Satellites,
        Self::Remote,
        Self::McpServers,
    ];

    #[must_use]
    pub const fn group(self) -> SettingsGroup {
        match self {
            Self::ThemeColors | Self::Screen => SettingsGroup::Appearance,
            Self::SessionList
            | Self::TranscriptDefaults
            | Self::InputPrompts
            | Self::EditingMode => SettingsGroup::Workspace,
            Self::ModelRoles | Self::ApiProviders | Self::SystemPrompt => SettingsGroup::Models,
            Self::ModelControl
            | Self::Budgets
            | Self::Usage
            | Self::ProviderKeys
            | Self::CloudSpend => SettingsGroup::SafetyAndSpend,
            Self::RetriesRecovery
            | Self::ContextRotation
            | Self::StallDetection
            | Self::MemoryDreaming
            | Self::Scheduling
            | Self::RecursiveDag
            | Self::CodeIntelligence => SettingsGroup::AgentAutomation,
            Self::ProviderIsolation
            | Self::ProviderSubagents
            | Self::ProviderSkills
            | Self::ApiRoutes
            | Self::HarnessTools
            | Self::ClaudeHooks
            | Self::ClaudeSkills => SettingsGroup::ProvidersAndSandboxes,
            Self::ResourceLimits
            | Self::BuildGovernor
            | Self::MergeQueue
            | Self::SandboxStorage
            | Self::ColdStorage
            | Self::BuildCache
            | Self::AgentBuilds => SettingsGroup::HostAndBuild,
            Self::MessageBridges | Self::Satellites | Self::Remote | Self::McpServers => {
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
            Self::EditingMode => "Editing Mode",
            Self::ModelRoles => "Model Roles",
            Self::ApiProviders => "API Providers",
            Self::SystemPrompt => "System Prompt",
            Self::ModelControl => "Model Control",
            Self::Budgets => "Budgets",
            Self::Usage => "Usage",
            Self::ProviderKeys => "Provider Keys",
            Self::CloudSpend => "Cloud Spend",
            Self::RetriesRecovery => "Retries & Recovery",
            Self::ContextRotation => "Context Rotation",
            Self::StallDetection => "Stall Detection",
            Self::MemoryDreaming => "Memory & Dreaming",
            Self::Scheduling => "Scheduling",
            Self::RecursiveDag => "Recursive DAG",
            Self::CodeIntelligence => "Code Intelligence",
            Self::ProviderIsolation => "Provider Isolation",
            Self::ProviderSubagents => "Subagent Governance",
            Self::ProviderSkills => "Provider Skills",
            Self::ApiRoutes => "API Routes",
            Self::HarnessTools => "Harness Tools",
            Self::ClaudeHooks => "Claude Hooks",
            Self::ClaudeSkills => "Claude Skills",
            Self::ResourceLimits => "Resource Limits",
            Self::BuildGovernor => "Build Governor",
            Self::MergeQueue => "Queue & Deploys",
            Self::SandboxStorage => "Sandbox Storage",
            Self::ColdStorage => "Cold Storage",
            Self::BuildCache => "Build Cache",
            Self::AgentBuilds => "Agent Builds",
            Self::MessageBridges => "Message Bridges",
            Self::Satellites => "Satellites",
            Self::Remote => "Remote",
            Self::McpServers => "MCP Servers",
        }
    }

    #[must_use]
    pub const fn summary(self) -> &'static str {
        match self {
            Self::ThemeColors => "The built-in theme and per-role overrides.",
            Self::Screen => "Text-area background, animation styles and detail column placement.",
            Self::SessionList => "What the session navigator and cards show.",
            Self::TranscriptDefaults => "What a newly opened transcript shows.",
            Self::InputPrompts => "How typing and submitting behave.",
            Self::EditingMode => "Standard or Vim text editing in every input.",
            Self::ModelRoles => "Which model does which job.",
            Self::ApiProviders => "OpenAI-compatible endpoints for model selection.",
            Self::SystemPrompt => "The system-prompt preset applied to launches.",
            Self::ModelControl => "Stop or limit model spend daemon-wide.",
            Self::Budgets => "Model budget policies.",
            Self::Usage => "Usage telemetry and stop/cancel actions.",
            Self::ProviderKeys => "Manage provider credentials stored in the key vault.",
            Self::CloudSpend => "Daily cap and stop line for remote (cloud) gate spend.",
            Self::RetriesRecovery => "What the daemon does when a session fails or stalls.",
            Self::ContextRotation => {
                "When a session rotates to a fresh context, and the manager and worker context caps."
            }
            Self::StallDetection => "How stalled sessions are detected and nudged.",
            Self::MemoryDreaming => "Memory extraction and consolidation.",
            Self::Scheduling => {
                "The background queue, the topology executor, how child sessions are kept running and the manager health-alert thresholds."
            }
            Self::RecursiveDag => "Recursive DAG control gates, run leases and the graph views.",
            Self::CodeIntelligence => "Code-graph indexing of project source.",
            Self::ProviderIsolation => "How provider processes are sandboxed and isolated.",
            Self::ProviderSubagents => {
                "Which model Claude and Codex subagents run on and how many may run at once."
            }
            Self::ProviderSkills => "How new provider sessions get the repository's agent skills.",
            Self::ApiRoutes => "Provider profile, OpenRouter and Bedrock routes and their budgets.",
            Self::HarnessTools => {
                "Web access, egress, tool budgets and completion gates for Harness sessions."
            }
            Self::ClaudeHooks => "Claude Code hooks.",
            Self::ClaudeSkills => "Claude user skills.",
            Self::ResourceLimits => {
                "Memory and CPU limits for the daemon, workers and build slice."
            }
            Self::BuildGovernor => {
                "Host-load admission, CPU andon and the build governor's slots and floors."
            }
            Self::MergeQueue => {
                "Merge queue batching and gates, test-job timeouts and deploy drain limits."
            }
            Self::SandboxStorage => {
                "Sandbox disk use, source-worktree settlement, archived sandbox purge and the stored tool-result cap."
            }
            Self::ColdStorage => {
                "The compressed archive of old session data: switch, age, directory, status and a run-now action."
            }
            Self::BuildCache => "Target-cache reclamation and the warm target pool.",
            Self::AgentBuilds => {
                "Build parallelism, line tables and sccache for agent cargo builds."
            }
            Self::MessageBridges => "Reach agents from Signal or iMessage.",
            Self::Satellites => "Registered peers, local links and cached remote sessions.",
            Self::Remote => "Read-only phone view over your tailnet: enable, devices, projects.",
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
    EditingMode,
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
    RetryMaxBackoff,
    RetryOnStall,
    ReconciliationLoop,
    ContextRotation,
    ContextRotationGlobalPct,
    ContextRotationClaudePct,
    ContextRotationCodexPct,
    CoordinatorContextCap,
    WorkerContextCap,
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
    FollowAgentCreatedProjects,
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
    TurnDetachEnabled,
    DeployDrainEnabled,
    DeployDrainHold,
    DeployRestartsPerHour,
    SupervisorShutdownGrace,
    HostLoadAdmissionThreshold,
    WorkerCpuQuotaPercent,
    HubBuildCpuPercent,
    RollingQueueBatchSize,
    RollingQueueGateTimeout,
    RollingQueueFullQaInterval,
    QaTargetMaxGib,
    CodexLiveCatalogTtl,
    CodexRateLimitPoll,
    ProviderLifecycleHooks,
    ProviderSkillDelivery,
    ProviderContractCanary,
    CodexRefuseUntrustedCustodyHook,
    StructuredHandoffDefault,
    JobThermalCeiling,
    HealthDiskUsedPct,
    HealthThermal,
    HealthStuckMinutes,
    JobTestTimeout,
    CpuAndonCpuMinutes,
    CpuAndonHostLoad,
    ProgramHoldWhileChildren,
    ChildKeepaliveEnabled,
    ChildKeepaliveWindow,
    HarnessWebAccess,
    HarnessEgressMode,
    HarnessWebSearchBackend,
    HarnessSearxngUrl,
    HarnessContextEditing,
    HarnessSearchCap,
    HarnessFetchCap,
    HarnessCompletionGates,
    HarnessOutputCap,
    HarnessWebCostCap,
    CodegraphIndexing,
    CodexSandbox,
    ClaudeConfigIsolation,
    CodexConfigIsolation,
    ProviderLaunchOverrides,
    ClaudeSubagentModel,
    ClaudeMaxConcurrentSubagents,
    CodexSubagentModel,
    CodexMaxConcurrentSubagents,
    ClaudeFallbackModels,
    ClaudeMaxBudgetUsd,
    VaultEnvCompat,
    VaultCheckTtl,
    OpenRouterRoute,
    BedrockRoute,
    ProviderProfile,
    ApiRouteFallbackOpenRouter,
    ApiRouteFallbackBedrock,
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
    SourceWorktreeAutoSettlement,
    DiskPressureMinFreeGb,
    LegacyScratchAdoption,
    SandboxMaxSourceRoots,
    SandboxMinFreeGib,
    TargetPoolKeep,
    TargetPoolStatus,
    McpDeferredToolThreshold,
    CloudSpendStatus,
    CloudSpendStopLine,
    CloudSpendDailyCap,
    ArchivedSandboxPurge,
    ColdArchiveEnabled,
    ColdArchiveAfterDays,
    ColdArchiveDir,
    ColdArchiveStatus,
    ColdArchiveLastRun,
    ColdArchiveRunNow,
    ToolResultStorageCap,
    ClaudeHooks,
    ClaudeSkills,
    SignalBridge,
    ImessageBridge,
    CompletedTranscriptCache,
    SatelliteRegistry,
    RemoteAccess,
    SatellitePolling,
    GovernorBuildSlots,
    GovernorLanderSlots,
    GovernorCheckSlots,
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
    /// TUI-owned rows that act the moment they are used: a theme preview, a
    /// form or editor that commits on submit.
    Immediate,
    /// TUI-owned preference rows that stage an edit and take effect when the
    /// Settings page is applied (`w`, #2241).
    OnApply,
    /// An independently launched message bridge reads its file at startup.
    BridgeRestart,
    /// Daemon-owned rows follow the daemon catalog's class.
    Daemon(ApplyClass),
}

impl SettingApply {
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Immediate => "immediately",
            Self::OnApply => "when applied (w)",
            Self::BridgeRestart => "after bridge restart",
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

impl SettingSpec {
    /// When a change to this row takes effect, as the operator reads it. An
    /// action button runs when it is activated; every other row follows its
    /// apply class (a staged edit takes effect when the page is applied).
    #[must_use]
    pub fn apply_label(&self) -> &'static str {
        if self.kind == SettingKind::Action {
            "when activated"
        } else {
            self.apply.label()
        }
    }
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
        id: SettingId::ResetTheme,
        section: SettingsSection::ThemeColors,
        label: "Reset active theme",
        summary: "Clears every semantic role override while keeping the selected theme.",
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
        apply: SettingApply::OnApply,
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
        apply: SettingApply::OnApply,
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
        apply: SettingApply::OnApply,
        destructive: false,
    },
    SettingSpec {
        id: SettingId::ActivityIndicator,
        section: SettingsSection::Screen,
        label: "Activity indicator",
        summary: "Cycles Semantic and five rainbow styles, including Sonic Speed Up and Rainbow Starlight; rainbow colours are tuned to each theme.",
        detail: None,
        keywords: &["indicator", "spinner", "rainbow"],
        kind: SettingKind::Cycle,
        owner: SettingOwner::Tui,
        apply: SettingApply::OnApply,
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
        apply: SettingApply::OnApply,
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
        apply: SettingApply::OnApply,
        destructive: false,
    },
    SettingSpec {
        id: SettingId::NavigatorColumns,
        section: SettingsSection::SessionList,
        label: "Navigator optional columns",
        summary: "Turns optional navigator columns on or off; J/K move the selected column, saved per preset.",
        detail: Some(
            "Optional columns: Navigator age, Navigator model, Navigator retry, Navigator cost, Navigator work, Navigator rotation, Navigator project, Navigator created. J / K move the selected column later / earlier in the active preset's order, which is saved separately for each preset. Required columns cannot be hidden.",
        ),
        keywords: &["navigator", "columns"],
        kind: SettingKind::DynamicList,
        owner: SettingOwner::Tui,
        apply: SettingApply::OnApply,
        destructive: false,
    },
    SettingSpec {
        id: SettingId::CardFields,
        section: SettingsSection::SessionList,
        label: "Card fields",
        summary: "Chooses which facts render on session-list cards.",
        detail: Some(
            "Card fields: Cost, Turn count, Retry info, Pin indicator, Rotation depth, Heat color, Description, Kind pill (TR/BUG), Docregblock pill, Accumulated work time, Created date.",
        ),
        keywords: &["card", "fields"],
        kind: SettingKind::DynamicList,
        owner: SettingOwner::Tui,
        apply: SettingApply::OnApply,
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
        apply: SettingApply::OnApply,
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
        apply: SettingApply::OnApply,
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
        apply: SettingApply::OnApply,
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
        apply: SettingApply::OnApply,
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
        apply: SettingApply::OnApply,
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
        apply: SettingApply::OnApply,
        destructive: false,
    },
    SettingSpec {
        id: SettingId::EditingMode,
        section: SettingsSection::EditingMode,
        label: "Editing mode",
        summary: "Standard edits like a normal text field (typing inserts, arrows move); Vim is the modal editing RSI was built around. Applies when you apply the page (w), with no restart; `unset` asks again at the next start.",
        detail: None,
        keywords: &["editing", "vim", "standard", "modal", "keys"],
        kind: SettingKind::Cycle,
        owner: SettingOwner::Daemon("editing_mode"),
        apply: SettingApply::Daemon(ApplyClass::Live),
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
            "The next classification uses the saved model; an in-flight classification keeps its model. The classifier's thresholds live in AGENT AUTOMATION ▸ Stall Detection.",
        ),
        keywords: &["classifier", "stall", "model"],
        kind: SettingKind::Edit,
        owner: SettingOwner::Daemon("stall_classifier_model"),
        apply: SettingApply::Daemon(ApplyClass::Live),
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
        apply: SettingApply::Daemon(ApplyClass::NextSpawn),
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
        id: SettingId::CloudSpendStatus,
        section: SettingsSection::CloudSpend,
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
        section: SettingsSection::CloudSpend,
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
        section: SettingsSection::CloudSpend,
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
        apply: SettingApply::Daemon(ApplyClass::Live),
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
        apply: SettingApply::Daemon(ApplyClass::Live),
        destructive: false,
    },
    SettingSpec {
        id: SettingId::ContextRotation,
        section: SettingsSection::ContextRotation,
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
        section: SettingsSection::ContextRotation,
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
        section: SettingsSection::ContextRotation,
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
        section: SettingsSection::ContextRotation,
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
        id: SettingId::CoordinatorContextCap,
        section: SettingsSection::ContextRotation,
        label: "Coordinator context cap (0 off, 1-100 = % of window)",
        summary: "Live context at which a manager seat, area manager, Epic lead or global manager is rotated with a daemon-written handoff. Read or set a provider or model override with :context-cap <Provider[/model]> [pct|tokens|default].",
        detail: Some(
            "Default 60 (60% of the session's known context window). Values 1-100 are a percentage of the window, 32000-2000000 are tokens, 0 turns it off. An unknown window disables percentage caps. Needs context rotation on.",
        ),
        keywords: &[
            "rotation",
            "context",
            "cap",
            "coordinator",
            "manager",
            "lead",
            "handoff",
            "tokens",
        ],
        kind: SettingKind::Cycle,
        owner: SettingOwner::Daemon("coordinator_context_cap_tokens"),
        apply: SettingApply::Daemon(ApplyClass::Live),
        destructive: false,
    },
    SettingSpec {
        id: SettingId::WorkerContextCap,
        section: SettingsSection::ContextRotation,
        label: "Worker context cap (0 off, 1-100 = % of window)",
        summary: "Live context at which a worker with a launching manager or Epic lead is told to pass the baton (commit, append a handoff to its Issue, end its turn) and its launcher gets a worker_context_cap notice. Read or set a provider or model override with :worker-context-cap <Provider[/model]> [pct|tokens|default].",
        detail: Some(
            "Default 60 (60% of the session's known context window). Values 1-100 are a percentage of the window, 32000-2000000 are tokens, 0 turns it off. Managers may propose it within operator bounds.",
        ),
        keywords: &[
            "context", "cap", "worker", "baton", "handoff", "tokens", "percent",
        ],
        kind: SettingKind::Cycle,
        owner: SettingOwner::Daemon("worker_context_cap_tokens"),
        apply: SettingApply::Daemon(ApplyClass::Live),
        destructive: false,
    },
    SettingSpec {
        id: SettingId::ProviderLifecycleHooks,
        section: SettingsSection::ContextRotation,
        label: "Provider lifecycle hooks",
        summary: "Lifecycle hooks a new Claude or Codex session installs beyond the custody and mail hooks: none (default), or any set of StopFailure, PreCompact and SessionStart.",
        detail: Some(
            "StopFailure (Claude) records an API error that ended a turn as a typed provider-error row; PreCompact (Claude and Codex) records that the provider is about to compact the context, so the baton/handoff logic can act before it does. SessionStart (Claude and Codex) delivers the session's RSI preamble as hook context instead of the start of the first prompt, when it fits the provider's 10,000-character hook-context cap; a preamble the hook did not deliver goes back in the next turn's prompt. Each hook runs RSI's own rsi-rpc hook command through the same per-session --settings / -c hooks.* path as the custody and mail hooks (Codex trusts it by key and hash, checked by the contract canary). A hook that fails to run is skipped: it never blocks or ends a session. none installs exactly the custody and mail hooks.",
        ),
        keywords: &[
            "provider",
            "lifecycle",
            "hook",
            "stopfailure",
            "precompact",
            "compaction",
            "baton",
            "claude",
            "codex",
        ],
        kind: SettingKind::Cycle,
        owner: SettingOwner::Daemon("provider_lifecycle_hooks"),
        apply: SettingApply::Daemon(ApplyClass::NextSpawn),
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
        apply: SettingApply::Daemon(ApplyClass::Live),
        destructive: false,
    },
    SettingSpec {
        id: SettingId::StallClassifier,
        section: SettingsSection::StallDetection,
        label: "Stall classifier",
        summary: "Asks a model whether a flagged session is really stalled before nudging it.",
        detail: Some(
            "Turning it on or off applies to the next detector tick and received signal. The classifier model is edited in MODELS ▸ Model Roles.",
        ),
        keywords: &["classifier", "stall", "nudge"],
        kind: SettingKind::Bool,
        owner: SettingOwner::Daemon("stall_classifier_enabled"),
        apply: SettingApply::Daemon(ApplyClass::Live),
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
        apply: SettingApply::Daemon(ApplyClass::Live),
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
        apply: SettingApply::Daemon(ApplyClass::Live),
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
        apply: SettingApply::Daemon(ApplyClass::Live),
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
        apply: SettingApply::Daemon(ApplyClass::Live),
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
        apply: SettingApply::Daemon(ApplyClass::Live),
        destructive: false,
    },
    SettingSpec {
        id: SettingId::DreamCooldown,
        section: SettingsSection::MemoryDreaming,
        label: "Dream cooldown",
        summary: "Cooldown used by the next consolidation cycle; an existing cooldown keeps its deadline.",
        detail: None,
        keywords: &["dream", "cooldown"],
        kind: SettingKind::Edit,
        owner: SettingOwner::Daemon("dream_cooldown_secs"),
        apply: SettingApply::Daemon(ApplyClass::Live),
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
        section: SettingsSection::Scheduling,
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
        id: SettingId::TopologyExecutor,
        section: SettingsSection::Scheduling,
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
        section: SettingsSection::Scheduling,
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
        section: SettingsSection::Scheduling,
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
        id: SettingId::ProgramHoldWhileChildren,
        section: SettingsSection::Scheduling,
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
        section: SettingsSection::Scheduling,
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
        section: SettingsSection::Scheduling,
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
        id: SettingId::FollowAgentCreatedProjects,
        section: SettingsSection::Scheduling,
        label: "Follow agent-created projects",
        summary: "Switches to the new project's tab when an agent creates a project (AgentCreateProject).",
        detail: Some(
            "On (default): the TUI jumps to the tab of a project an agent just created and offers, in one key, to add it to the creating global manager's grant. Off: the project still appears in the tab bar and the grant offer still shows, but the view stays where it is.",
        ),
        keywords: &["project", "agent", "created", "follow", "tab", "grant"],
        kind: SettingKind::Bool,
        owner: SettingOwner::Daemon("follow_agent_created_projects"),
        apply: SettingApply::Daemon(ApplyClass::Live),
        destructive: false,
    },
    SettingSpec {
        id: SettingId::HealthDiskUsedPct,
        section: SettingsSection::Scheduling,
        label: "Health alert: hub disk used (%)",
        summary: "Hub root filesystem use at or above which a manager's armed health_anomaly wake fires (0 off, else 50-99 %, default 90).",
        detail: Some(
            "A manager (project manager or portfolio seat) arms one `health_anomaly` wake instead of an hourly check-in turn. The daemon evaluates it every minute with no model call and resumes the manager once, naming each condition, when one holds: this one, a PM seat stuck behind a refused succession or over its rotation threshold, blocked or overrun topology work, a hot CPU, a satellite below its disk floor, a provider fault, or a red rolling QA no Issue tracks. Takes effect at the next evaluation.",
        ),
        keywords: &[
            "health", "disk", "alert", "anomaly", "safety", "manager", "wake",
        ],
        kind: SettingKind::Cycle,
        owner: SettingOwner::Daemon("health_disk_used_pct"),
        apply: SettingApply::Daemon(ApplyClass::Live),
        destructive: false,
    },
    SettingSpec {
        id: SettingId::HealthThermal,
        section: SettingsSection::Scheduling,
        label: "Health alert: CPU hot (C)",
        summary: "CPU temperature that, held for three minutes, fires a manager's armed health_anomaly wake (0 off, else 60-110 C, default 80).",
        detail: Some(
            "Read from the hub's hottest CPU sensor (Linux hwmon). A host with no readable sensor never fires. This is separate from the job thermal hold, which stops new jobs at a higher temperature. Takes effect at the next evaluation.",
        ),
        keywords: &[
            "health",
            "thermal",
            "temperature",
            "hot",
            "alert",
            "tctl",
            "anomaly",
        ],
        kind: SettingKind::Cycle,
        owner: SettingOwner::Daemon("health_thermal_c"),
        apply: SettingApply::Daemon(ApplyClass::Live),
        destructive: false,
    },
    SettingSpec {
        id: SettingId::HealthStuckMinutes,
        section: SettingsSection::Scheduling,
        label: "Health alert: work stuck (min)",
        summary: "Minutes a topology execution may stay blocked, a node attempt may run past its wall limit, or a red rolling QA may go untracked before health_anomaly fires (0 off, else 5-720, default 30).",
        detail: Some(
            "Counted from the moment the execution blocked, the attempt's execution deadline passed, or the red full-library QA run finished with no open Issue tracking it. Takes effect at the next evaluation.",
        ),
        keywords: &[
            "health", "stuck", "blocked", "alert", "qa", "topology", "anomaly",
        ],
        kind: SettingKind::Cycle,
        owner: SettingOwner::Daemon("health_stuck_minutes"),
        apply: SettingApply::Daemon(ApplyClass::Live),
        destructive: false,
    },
    SettingSpec {
        id: SettingId::DagRecoveryControls,
        section: SettingsSection::RecursiveDag,
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
        section: SettingsSection::RecursiveDag,
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
        section: SettingsSection::RecursiveDag,
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
        section: SettingsSection::RecursiveDag,
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
        section: SettingsSection::RecursiveDag,
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
        section: SettingsSection::RecursiveDag,
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
        section: SettingsSection::RecursiveDag,
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
        section: SettingsSection::RecursiveDag,
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
        id: SettingId::StructuredHandoffDefault,
        section: SettingsSection::RecursiveDag,
        label: "Schema-validated topology handoffs",
        summary: "Makes every topology session node ask the harness to validate its repeated handoff's shape, as if marked structured_handoff (default off).",
        detail: Some(
            "When a topology node's handoff is refused (handoff_invalid), the daemon continues the same session once to repeat it. With this on (or on a node marked structured_handoff) that turn carries the handoff JSON Schema: Claude gets --json-schema, Codex gets --output-schema and -o, so the harness checks the shape before RSI parses it. The parser accepts the structured payload as the handoff and still accepts a free-text one. Off launches every turn exactly as before. The effect shows in the handoff_invalid_total and handoff_observed_total counters of AgentGetDaemonInfo, by provider and structured.",
        ),
        keywords: &[
            "handoff",
            "structured",
            "schema",
            "json",
            "topology",
            "claude",
            "codex",
            "invalid",
        ],
        kind: SettingKind::Bool,
        owner: SettingOwner::Daemon("structured_handoff_default"),
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
        id: SettingId::CodexLiveCatalogTtl,
        section: SettingsSection::ProviderIsolation,
        label: "Codex live model catalog TTL (s)",
        summary: "Seconds the per-login Codex model list is cached before it is read again (0 = bundled catalog only, default 300).",
        detail: Some(
            "The Codex model picker, the manager preflight and launch admission use the models this login can actually use, read from the Codex app-server (model/list) and falling back to non-bundled `codex debug models`. A failed read falls back to the bundled catalog, marked degraded, and never refuses a launch by itself. A model absent from a fresh live list is refused model_unavailable before any session or sandbox is created, and admitted again as soon as a later list contains it. 0 reads only the bundled catalog.",
        ),
        keywords: &["codex", "model", "catalog", "live", "login", "ttl", "cache"],
        kind: SettingKind::Cycle,
        owner: SettingOwner::Daemon("codex_live_catalog_ttl_secs"),
        apply: SettingApply::Daemon(ApplyClass::Live),
        destructive: false,
    },
    SettingSpec {
        id: SettingId::CodexRateLimitPoll,
        section: SettingsSection::ProviderIsolation,
        label: "Codex plan-window poll (s)",
        summary: "Seconds between reads of the Codex plan windows (0 = off, default 300).",
        detail: Some(
            "The daemon asks the Codex app-server (account/rateLimits/read, no model turn) how much of the signed-in plan's windows is used and stores them beside Claude's, so launch admission, the manager capacity view and the status bar treat both providers alike. A failed read keeps the last windows and marks them stale; a logged-out login stores nothing. 0 turns the poll off (Claude windows are unaffected).",
        ),
        keywords: &[
            "codex", "rate", "limit", "plan", "window", "usage", "capacity", "poll",
        ],
        kind: SettingKind::Cycle,
        owner: SettingOwner::Daemon("codex_rate_limit_poll_secs"),
        apply: SettingApply::Daemon(ApplyClass::Live),
        destructive: false,
    },
    SettingSpec {
        id: SettingId::ProviderContractCanary,
        section: SettingsSection::ProviderIsolation,
        label: "Provider CLI contract canary",
        summary: "Checks, at daemon start and when a CLI's version changes, that the installed Claude Code and Codex still honour what RSI relies on (default on).",
        detail: Some(
            "A bounded, non-billing check per CLI: Codex hook trust and the app-server handshake (hooks/list with RSI's real hook overrides: both session-flag hooks must be trusted with RSI's hash), and Claude's initialize control request plus every flag RSI passes (help text, then an unknown-option probe for hidden flags) and the non-bare ambient login. A failed check marks the provider degraded with a reason code in the provider status and the Provider Keys list, and notifies the manager. Off skips the check and reports the provider unchecked.",
        ),
        keywords: &[
            "provider", "cli", "contract", "canary", "drift", "codex", "claude", "hook", "version",
        ],
        kind: SettingKind::Bool,
        owner: SettingOwner::Daemon("provider_contract_canary"),
        apply: SettingApply::Daemon(ApplyClass::Live),
        destructive: false,
    },
    SettingSpec {
        id: SettingId::CodexRefuseUntrustedCustodyHook,
        section: SettingsSection::ProviderIsolation,
        label: "Refuse Codex launch when custody hook is untrusted",
        summary: "Refuses a Codex launch when the canary finds the installed codex-cli does not trust RSI's sandbox custody hook (default on).",
        detail: Some(
            "The custody hook stops a Codex session from switching the sandbox branch, stashing, or force-pushing. If a codex-cli update changes how it hashes hooks, the hook is silently not run. With this on, a Codex launch that needs the hook is refused with codex_hook_untrusted, codex_hook_hash_mismatch or codex_hook_missing until RSI's hash is updated; off launches Codex without custody and only reports the provider degraded. Only a definite untrusted result refuses: an unchecked or failed canary never does.",
        ),
        keywords: &[
            "codex", "custody", "hook", "trust", "refuse", "sandbox", "branch",
        ],
        kind: SettingKind::Bool,
        owner: SettingOwner::Daemon("codex_refuse_untrusted_custody_hook"),
        apply: SettingApply::Daemon(ApplyClass::Live),
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
        id: SettingId::CodexConfigIsolation,
        section: SettingsSection::ProviderIsolation,
        label: "Codex user config isolation",
        summary: "Isolation of the operator's Codex config for new Codex processes: off, user (ignore config.toml), rules (ignore execpolicy rules) or all.",
        detail: Some(
            "user passes --ignore-user-config so $CODEX_HOME/config.toml (model, profile, mcp_servers, hooks, approval_policy) is not loaded; rules passes --ignore-rules so user and project execpolicy .rules files are not loaded; all passes both. Auth still uses CODEX_HOME and every override RSI passes still applies. off adds no flag.",
        ),
        keywords: &[
            "codex",
            "isolation",
            "config",
            "rules",
            "ignore",
            "user",
            "execpolicy",
        ],
        kind: SettingKind::Cycle,
        owner: SettingOwner::Daemon("codex_config_isolation"),
        apply: SettingApply::Daemon(ApplyClass::NextSpawn),
        destructive: false,
    },
    SettingSpec {
        id: SettingId::ProviderLaunchOverrides,
        section: SettingsSection::ProviderIsolation,
        label: "Provider launch overrides",
        summary: "Extra Claude Code / Codex CLI launch options, from an allowlist (default none). Enter opens a JSON editor; the daemon checks each option and the editor shows its reason when it refuses.",
        detail: Some(
            "A JSON object of per-provider lists, e.g. {\"codex\":[\"model_reasoning_summary=concise\"],\"claude\":[\"--append-system-prompt=Be terse.\"]}. Claude takes the flags --append-system-prompt and --betas (passed as --flag=value; the fallback chain and spend cap have their own settings); Codex takes the config keys model_reasoning_summary, model_verbosity, service_tier, web_search, hide_agent_reasoning, show_raw_agent_reasoning and agents.enabled (passed as -c key=value). Anything that widens privilege (--dangerously-skip-permissions, --permission-mode, --add-dir, permissions.*, sandbox.*, hooks.*, approval_policy, sandbox_mode, mcp_servers.*) or that RSI sets itself (model, effort, resume, --fallback-model, --max-budget-usd, and the Codex subagent model and concurrency cap, which have their own settings) is refused with provider_launch_override_denied; any other key with provider_launch_override_unknown_key, as is a Claude flag the installed CLI does not know. Applied at the next CLI launch (each Claude turn) and recorded on the session as a launch-overrides row. Empty clears.",
        ),
        keywords: &[
            "provider",
            "launch",
            "override",
            "options",
            "flags",
            "codex",
            "claude",
            "config",
            "allowlist",
            "pass-through",
        ],
        kind: SettingKind::Action,
        owner: SettingOwner::Daemon("provider_launch_overrides"),
        apply: SettingApply::Daemon(ApplyClass::NextSpawn),
        destructive: false,
    },
    SettingSpec {
        id: SettingId::ClaudeFallbackModels,
        section: SettingsSection::ProviderIsolation,
        label: "Claude fallback models",
        summary: "Ordered models a Claude session falls back to when its model is overloaded or unavailable, comma-separated, at most three. Default is empty (no fallback). Enter opens a text editor.",
        detail: Some(
            "Passed as --fallback-model=a,b to each new Claude CLI session (#2762); the CLI tries them in order per turn and retries the primary at the start of each turn. It does not apply to auth, billing or rate-limit errors, and is not passed to Bedrock-routed models or when the list names only the session's own model. A result that shows a chain member served part of the run adds a model_fallback_used row (from, to) to the session transcript. Running sessions keep the value they started with.",
        ),
        keywords: &[
            "claude",
            "fallback",
            "model",
            "overloaded",
            "chain",
            "unavailable",
            "retired",
        ],
        kind: SettingKind::Action,
        owner: SettingOwner::Daemon("claude_fallback_models"),
        apply: SettingApply::Daemon(ApplyClass::NextSpawn),
        destructive: false,
    },
    SettingSpec {
        id: SettingId::ClaudeMaxBudgetUsd,
        section: SettingsSection::ProviderIsolation,
        label: "Claude spend cap per session (USD)",
        summary: "Estimated spend, in USD, at which a new Claude CLI session stops. 0 (the default) is off. Enter opens a text editor.",
        detail: Some(
            "Passed as --max-budget-usd=<n> to each new Claude CLI session (#2762), subagents included; the CLI's own cost estimate decides, and the run ends with the typed stop reason provider_budget_exceeded, which is never retried automatically. Meant for API- or Bedrock-billed sessions; a subscription login is not billed per token, so leave it 0 there. Running sessions keep the value they started with.",
        ),
        keywords: &["claude", "budget", "spend", "cost", "cap", "usd", "limit"],
        kind: SettingKind::Action,
        owner: SettingOwner::Daemon("claude_max_budget_usd"),
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
        id: SettingId::ClaudeSubagentModel,
        section: SettingsSection::ProviderSubagents,
        label: "Claude subagent model",
        summary: "Model every Claude subagent runs on (empty = inherit the parent's model, today's behaviour). Enter opens a text editor.",
        detail: Some(
            "Sets CLAUDE_CODE_SUBAGENT_MODEL for new Claude processes, so subagents run on a cheaper model than the expensive parent. Use a Claude alias (haiku, sonnet, opus) or a full model id; the daemon refuses anything else. Empty sets nothing. A running session keeps the value it started with, and the session shows the effective setting once it starts.",
        ),
        keywords: &["claude", "subagent", "model", "cost", "cheaper", "inherit"],
        kind: SettingKind::Action,
        owner: SettingOwner::Daemon("claude_subagent_model"),
        apply: SettingApply::Daemon(ApplyClass::NextSpawn),
        destructive: false,
    },
    SettingSpec {
        id: SettingId::ClaudeMaxConcurrentSubagents,
        section: SettingsSection::ProviderSubagents,
        label: "Claude max concurrent subagents",
        summary: "Cap on Claude subagents running at once, per session (0 = Claude Code's default of 20).",
        detail: Some(
            "Sets CLAUDE_CODE_MAX_CONCURRENT_SUBAGENTS for new Claude processes (1 to 100). 0 sets nothing. A running session keeps the value it started with, and the session shows the effective cap once it starts.",
        ),
        keywords: &[
            "claude",
            "subagent",
            "concurrent",
            "fan-out",
            "cap",
            "cost",
            "limit",
        ],
        kind: SettingKind::Cycle,
        owner: SettingOwner::Daemon("claude_max_concurrent_subagents"),
        apply: SettingApply::Daemon(ApplyClass::NextSpawn),
        destructive: false,
    },
    SettingSpec {
        id: SettingId::CodexSubagentModel,
        section: SettingsSection::ProviderSubagents,
        label: "Codex subagent model",
        summary: "Model every Codex subagent runs on (empty = Codex's own default). Enter opens a text editor.",
        detail: Some(
            "Passes -c agents.default_subagent_model=MODEL on every codex exec and exec resume turn, so subagents run on a cheaper model than the parent. The daemon refuses anything that is not a plain model identifier. Empty passes nothing. The session shows the effective setting once it starts.",
        ),
        keywords: &["codex", "subagent", "model", "cost", "cheaper", "agents"],
        kind: SettingKind::Action,
        owner: SettingOwner::Daemon("codex_subagent_model"),
        apply: SettingApply::Daemon(ApplyClass::NextSpawn),
        destructive: false,
    },
    SettingSpec {
        id: SettingId::CodexMaxConcurrentSubagents,
        section: SettingsSection::ProviderSubagents,
        label: "Codex max concurrent subagents",
        summary: "Cap on Codex subagent threads running at once, per session (0 = Codex's default).",
        detail: Some(
            "Passes -c agents.max_concurrent_threads_per_session=N on every codex exec and exec resume turn (1 to 100). 0 passes nothing. The session shows the effective cap once it starts.",
        ),
        keywords: &[
            "codex",
            "subagent",
            "concurrent",
            "threads",
            "fan-out",
            "cap",
            "cost",
            "agents",
        ],
        kind: SettingKind::Cycle,
        owner: SettingOwner::Daemon("codex_max_concurrent_subagents"),
        apply: SettingApply::Daemon(ApplyClass::NextSpawn),
        destructive: false,
    },
    SettingSpec {
        id: SettingId::ProviderSkillDelivery,
        section: SettingsSection::ProviderSkills,
        label: "Provider skill delivery",
        summary: "How a new Claude or Codex session gets the repository's agent skills: ambient (the CLI discovers them, default) or explicit (the launch names them).",
        detail: Some(
            "ambient leaves the launch argv exactly as before: the CLI finds skills from the session's working directory and the operator's home. explicit delivers the repository's skills with the launch itself and suppresses every other source, so the loaded set is exactly the delivered one whatever the working directory or the operator's home hold: Claude gets one --plugin-dir (a content-addressed plugin named rsi-skills built from .claude/skills, its skills namespaced rsi-skills:<skill>; a symlinked .claude or .claude/skills is never read) and an empty --setting-sources, which stops ~/.claude/skills, the working directory's skills and settings-enabled plugins from loading (the user and project settings files then no longer apply either); Codex gets -c skills.bundled.enabled=false and one -c skills.config override that enables the repository's .agents/skills and disables every other skill it finds (nested, symlinked, project .codex/skills, user and admin roots). A Claude plugin that cannot be built delivers no skills (never ambient ones). A Codex launch is refused with a typed skill_delivery_* reason when the skill scan cannot prove it named every skill. Claude's init plugins and plugin_errors show whether it loaded.",
        ),
        keywords: &[
            "provider",
            "skill",
            "skills",
            "plugin",
            "plugin-dir",
            "deterministic",
            "bare",
            "claude",
            "codex",
            "ambient",
            "explicit",
        ],
        kind: SettingKind::Cycle,
        owner: SettingOwner::Daemon("provider_skill_delivery"),
        apply: SettingApply::Daemon(ApplyClass::NextSpawn),
        destructive: false,
    },
    SettingSpec {
        id: SettingId::ProviderProfile,
        section: SettingsSection::ApiRoutes,
        label: "Provider profile",
        summary: "all launches every provider as before; aws_only allows only Claude Code on Amazon Bedrock (a Bedrock Claude model id), refuses every other launch with provider_profile_refused, and filters the model pickers. Set the AWS region and Bedrock key with :aws-setup.",
        detail: Some(
            "Checked at every launch, continuation, rotation and spawn. Under aws_only a Claude launch that names no model runs us.anthropic.claude-sonnet-5-v1:0.",
        ),
        keywords: &[
            "provider", "profile", "aws", "bedrock", "claude", "only", "restrict",
        ],
        kind: SettingKind::Cycle,
        owner: SettingOwner::Daemon("provider_profile"),
        apply: SettingApply::Daemon(ApplyClass::Live),
        destructive: false,
    },
    SettingSpec {
        id: SettingId::OpenRouterRoute,
        section: SettingsSection::ApiRoutes,
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
        section: SettingsSection::ApiRoutes,
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
        id: SettingId::ApiRouteFallbackOpenRouter,
        section: SettingsSection::ApiRoutes,
        label: "OpenRouter Codex fallback",
        summary: "Allow a failed Harness preflight to launch OpenRouter through Codex CLI. Off (default): the launch fails loudly and never runs on Codex.",
        detail: None,
        keywords: &["openrouter", "route", "fallback", "codex", "harness"],
        kind: SettingKind::Bool,
        owner: SettingOwner::Daemon("api_route.fallback.openrouter"),
        apply: SettingApply::Daemon(ApplyClass::NextSpawn),
        destructive: false,
    },
    SettingSpec {
        id: SettingId::ApiRouteFallbackBedrock,
        section: SettingsSection::ApiRoutes,
        label: "Bedrock Codex fallback",
        summary: "Allow a failed Harness preflight to launch Bedrock through Codex CLI. The legacy api_route.fallback key is an alias of this setting.",
        detail: None,
        keywords: &["bedrock", "route", "fallback", "codex", "aws"],
        kind: SettingKind::Bool,
        owner: SettingOwner::Daemon("api_route.fallback.bedrock"),
        apply: SettingApply::Daemon(ApplyClass::NextSpawn),
        destructive: false,
    },
    SettingSpec {
        id: SettingId::OpenRouterContextBudget,
        section: SettingsSection::ApiRoutes,
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
        id: SettingId::HarnessWebAccess,
        section: SettingsSection::HarnessTools,
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
        section: SettingsSection::HarnessTools,
        label: "Harness network egress",
        summary: "Default network egress for Harness sessions: deny_private (network tools reach public addresses only; loopback, link-local, cloud metadata and private ranges are refused, after DNS and every redirect) or offline (no network tools; the shell runs in an empty network namespace). In deny_private the shell tool's network is NOT restricted: only offline isolates it. The native web_fetch tool is guarded by it (and web_search refuses offline before any request). A session's own tool policy overrides it.",
        detail: None,
        keywords: &["harness", "network", "egress", "ssrf", "offline", "policy"],
        kind: SettingKind::Cycle,
        owner: SettingOwner::Daemon("harness_egress_mode"),
        apply: SettingApply::Daemon(ApplyClass::NextSpawn),
        destructive: false,
    },
    SettingSpec {
        id: SettingId::HarnessWebSearchBackend,
        section: SettingsSection::HarnessTools,
        label: "Harness web search backend",
        summary: "Which search vendor backs the native web_search tool on Harness routes without a provider-hosted search: auto (the first backend whose key is set under Provider Keys, else the SearXNG instance when its URL is set), serper, exa, searxng (a self-hosted, keyless instance) or disabled. The tool is advertised only when the chosen backend is configured; otherwise the session emits a web_search_unavailable notice.",
        detail: Some(
            "Read at launch, so running sessions keep the value they started with (#2251). Auto tries Serper first, then Exa (#2252), then SearXNG (#2315).",
        ),
        keywords: &[
            "harness", "web", "search", "serper", "exa", "searxng", "backend", "key",
        ],
        kind: SettingKind::Cycle,
        owner: SettingOwner::Daemon("harness_web_search_backend"),
        apply: SettingApply::Daemon(ApplyClass::NextSpawn),
        destructive: false,
    },
    SettingSpec {
        id: SettingId::HarnessSearxngUrl,
        section: SettingsSection::HarnessTools,
        label: "Harness SearXNG URL",
        summary: "Base URL of the SearXNG instance behind the keyless web_search backend. Default is unset. Enter opens a text editor: type an http(s) URL (scripts/searxng-local.sh prints the local one, http://127.0.0.1:8888); the daemon checks it and the editor shows its reason when it refuses.",
        detail: Some(
            "Run scripts/searxng-local.sh once to start a loopback-only instance with the JSON API enabled (#2315). Public SearXNG instances usually block automated JSON queries, so the local instance is the supported path. Read at launch, so running sessions keep the value they started with.",
        ),
        keywords: &[
            "harness", "web", "search", "searxng", "url", "keyless", "local",
        ],
        kind: SettingKind::Action,
        owner: SettingOwner::Daemon("harness_searxng_url"),
        apply: SettingApply::Daemon(ApplyClass::NextSpawn),
        destructive: false,
    },
    SettingSpec {
        id: SettingId::HarnessContextEditing,
        section: SettingsSection::HarnessTools,
        label: "Harness context editing",
        summary: "Default for Harness context editing: when on, direct-Anthropic Claude Harness sessions let the provider clear stale tool results server-side to save context. A session's own tool policy (context_editing) overrides it.",
        detail: Some(
            "Read at launch, so running sessions keep the value they started with (#1111).",
        ),
        keywords: &[
            "harness",
            "context",
            "editing",
            "clear",
            "tool results",
            "anthropic",
        ],
        kind: SettingKind::Bool,
        owner: SettingOwner::Daemon("harness_context_editing"),
        apply: SettingApply::Daemon(ApplyClass::NextSpawn),
        destructive: false,
    },
    SettingSpec {
        id: SettingId::HarnessSearchCap,
        section: SettingsSection::HarnessTools,
        label: "Harness search call cap",
        summary: "Default cap on web_search calls per Harness session, native or provider-hosted; 0 is unlimited. Exhaustion returns a typed tool error and the session continues.",
        detail: None,
        keywords: &["harness", "search", "budget", "policy"],
        kind: SettingKind::Cycle,
        owner: SettingOwner::Daemon("harness_max_search_calls"),
        apply: SettingApply::Daemon(ApplyClass::NextSpawn),
        destructive: false,
    },
    SettingSpec {
        id: SettingId::HarnessFetchCap,
        section: SettingsSection::HarnessTools,
        label: "Harness fetch call cap",
        summary: "Default cap on web_fetch calls per Harness session, native or provider-hosted; 0 is unlimited.",
        detail: None,
        keywords: &["harness", "fetch", "budget", "policy"],
        kind: SettingKind::Cycle,
        owner: SettingOwner::Daemon("harness_max_fetch_calls"),
        apply: SettingApply::Daemon(ApplyClass::NextSpawn),
        destructive: false,
    },
    SettingSpec {
        id: SettingId::HarnessCompletionGates,
        section: SettingsSection::HarnessTools,
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
        section: SettingsSection::HarnessTools,
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
        section: SettingsSection::HarnessTools,
        label: "Harness web cost cap (micro-USD)",
        summary: "Default cap on estimated web cost per Harness session, native or provider-hosted (1000000 = 1 USD, searches estimated at 0.01 USD); 0 is unlimited.",
        detail: None,
        keywords: &["harness", "cost", "budget", "policy"],
        kind: SettingKind::Cycle,
        owner: SettingOwner::Daemon("harness_max_web_cost_usd_micros"),
        apply: SettingApply::Daemon(ApplyClass::NextSpawn),
        destructive: false,
    },
    SettingSpec {
        id: SettingId::McpDeferredToolThreshold,
        section: SettingsSection::HarnessTools,
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
        id: SettingId::HarnessMaxIterations,
        section: SettingsSection::HarnessTools,
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
        id: SettingId::RsidScopeMemoryHigh,
        section: SettingsSection::ResourceLimits,
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
        section: SettingsSection::ResourceLimits,
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
        section: SettingsSection::ResourceLimits,
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
        section: SettingsSection::ResourceLimits,
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
        section: SettingsSection::ResourceLimits,
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
        section: SettingsSection::ResourceLimits,
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
        section: SettingsSection::ResourceLimits,
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
        section: SettingsSection::ResourceLimits,
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
        id: SettingId::WorkerCpuQuotaPercent,
        section: SettingsSection::ResourceLimits,
        label: "Worker CPU quota",
        summary: "CPU cap of each worker, as a percentage of one CPU (400 = four cores, default 400; 0 disables the cap).",
        detail: Some(
            "Each worker session runs in its own systemd scope; this sets that scope's CPUQuota, so one worker's builds, tests or runaway loops cannot take more than this share of the host. A halted worker's whole scope is killed, so no descendant outlives it. Applies to workers started after the change; running workers keep their quota. Linux only: other platforms have no scope and ignore it.",
        ),
        keywords: &["worker", "cpu", "quota", "limit", "scope", "load", "cores"],
        kind: SettingKind::Cycle,
        owner: SettingOwner::Daemon("worker_cpu_quota_percent"),
        apply: SettingApply::Daemon(ApplyClass::Live),
        destructive: false,
    },
    SettingSpec {
        id: SettingId::HubBuildCpuPercent,
        section: SettingsSection::ResourceLimits,
        label: "Hub build CPU budget",
        summary: "Share of the host's CPUs that all cargo builds and tests RSI starts may use together, as a percentage of its logical CPUs (default 50, range 10-100).",
        detail: Some(
            "Every cargo build and test unit RSI starts (job units, scoped-test, merge-queue gates, the library QA run) runs in one systemd user slice, rsi-build.slice, whose CPUQuota is this share of the host's logical CPUs (50% of 32 threads is 16 cores) at a low CPUWeight, so concurrent jobs share one budget and the desktop stays responsive. Each job also sizes its own thread count from the budget. Applying a change rewrites the slice at once, including for units already running; read it back with systemctl --user show rsi-build.slice -p CPUQuotaPerSecUSec. Linux only: other platforms have no slice and report that no budget is available.",
        ),
        keywords: &[
            "build", "cpu", "budget", "slice", "hub", "load", "cores", "desktop", "cargo", "test",
        ],
        kind: SettingKind::Cycle,
        owner: SettingOwner::Daemon("hub_build_cpu_percent"),
        apply: SettingApply::Daemon(ApplyClass::Live),
        destructive: false,
    },
    SettingSpec {
        id: SettingId::CompletedTranscriptCache,
        section: SettingsSection::ResourceLimits,
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
        id: SettingId::HostLoadAdmissionThreshold,
        section: SettingsSection::BuildGovernor,
        label: "Host load limit for new launches",
        summary: "Hold a manager's new worker launches while the host's 1-minute load average is above this (0-1024, default 40; 0 never holds).",
        detail: Some(
            "Several projects' managers share one host, and their workers build and test, so a dozen started together push the load far past the core count. While the 1-minute load plus the launches admitted in the last minute is above this limit, the daemon holds a manager's create_session (Issue workers and topology nodes included): it stays queued, never refused, shows as held: host_load in AgentManagerGetAction and AgentGetDaemonInfo, and starts on its own when the load drops, oldest first. Operator sessions, a worker's own spawns, retries and lead recovery are never held. Platforms without a load average admit everything.",
        ),
        keywords: &[
            "host",
            "load",
            "admission",
            "hold",
            "launch",
            "manager",
            "uptime",
            "busy",
        ],
        kind: SettingKind::Cycle,
        owner: SettingOwner::Daemon("host_load_admission_threshold"),
        apply: SettingApply::Daemon(ApplyClass::Live),
        destructive: false,
    },
    SettingSpec {
        id: SettingId::CpuAndonCpuMinutes,
        section: SettingsSection::BuildGovernor,
        label: "CPU andon: CPU-minutes per tree",
        summary: "One agent process tree (a session's provider scope or a job unit) past this many CPU-minutes is a runaway (0 off, default 240).",
        detail: Some(
            "The daemon samples cgroup CPU time once a minute (Linux; unsupported elsewhere), records a runaway_process friction event and tells the owning manager the session and a suggested halt. It never stops the tree itself.",
        ),
        keywords: &["cpu", "andon", "runaway", "test", "load"],
        kind: SettingKind::Cycle,
        owner: SettingOwner::Daemon("cpu_andon_cpu_minutes"),
        apply: SettingApply::Daemon(ApplyClass::Live),
        destructive: false,
    },
    SettingSpec {
        id: SettingId::CpuAndonHostLoad,
        section: SettingsSection::BuildGovernor,
        label: "CPU andon: host load",
        summary: "At or above this 1-minute host load, one agent process tree using a dominant share of it is a runaway (0 off, default 40).",
        detail: Some(
            "Same andon as the CPU-minutes trigger: a runaway_process friction event and one manager notice per tree, never an automatic stop.",
        ),
        keywords: &["cpu", "andon", "runaway", "load", "uptime"],
        kind: SettingKind::Cycle,
        owner: SettingOwner::Daemon("cpu_andon_host_load"),
        apply: SettingApply::Daemon(ApplyClass::Live),
        destructive: false,
    },
    SettingSpec {
        id: SettingId::GovernorBuildSlots,
        section: SettingsSection::BuildGovernor,
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
        section: SettingsSection::BuildGovernor,
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
        id: SettingId::GovernorCheckSlots,
        section: SettingsSection::BuildGovernor,
        label: "Check slots",
        summary: "Concurrent cargo check runs of the fast compile-feedback lane, a pool of its own that full builds never fill (1-16); default 2.",
        detail: None,
        keywords: &["governor", "check", "slots", "cargo", "compile"],
        kind: SettingKind::Cycle,
        owner: SettingOwner::Daemon("governor_check_slots"),
        apply: SettingApply::Daemon(ApplyClass::Live),
        destructive: false,
    },
    SettingSpec {
        id: SettingId::GovernorMaxLoad,
        section: SettingsSection::BuildGovernor,
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
        section: SettingsSection::BuildGovernor,
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
        section: SettingsSection::BuildGovernor,
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
        section: SettingsSection::BuildGovernor,
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
        id: SettingId::RollingQueueEnabled,
        section: SettingsSection::MergeQueue,
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
        id: SettingId::RollingQueueBatchSize,
        section: SettingsSection::MergeQueue,
        label: "Merge queue batch size",
        summary: "Maximum ready sources merged into one candidate and gated together (1-8).",
        detail: None,
        keywords: &["merge", "queue", "batch", "landing"],
        kind: SettingKind::Cycle,
        owner: SettingOwner::Daemon("rolling_queue_batch_size"),
        apply: SettingApply::Daemon(ApplyClass::Live),
        destructive: false,
    },
    SettingSpec {
        id: SettingId::RollingQueueGateTimeout,
        section: SettingsSection::MergeQueue,
        label: "Merge queue gate timeout (min)",
        summary: "Wall-time budget of one merge-queue batch's gating, bisect included (30-1440 minutes, default 60).",
        detail: Some(
            "When a batch runs past it, the running lander is stopped and every member not yet settled is refused with queue_gate_timeout, which wakes its owner; the queue then moves on to the next batch.",
        ),
        keywords: &["merge", "queue", "gate", "timeout", "landing"],
        kind: SettingKind::Cycle,
        owner: SettingOwner::Daemon("rolling_queue_gate_timeout_mins"),
        apply: SettingApply::Daemon(ApplyClass::Live),
        destructive: false,
    },
    SettingSpec {
        id: SettingId::RollingQueueFullQaInterval,
        section: SettingsSection::MergeQueue,
        label: "Merge queue full-QA interval (min)",
        summary: "Minimum time between two starts of the merge queue's async full-library QA run (0-240 minutes, default 60).",
        detail: Some(
            "A landing gated on queue-derived filters or a bounded selection owes one full rsid and rsid-store library, bin and integration test run; landings published in between coalesce into the next run, which runs at a lower CPU weight than gates. 0 starts it as soon as the previous run ends.",
        ),
        keywords: &[
            "merge", "queue", "qa", "full", "library", "interval", "landing",
        ],
        kind: SettingKind::Cycle,
        owner: SettingOwner::Daemon("rolling_queue_full_qa_interval_mins"),
        apply: SettingApply::Daemon(ApplyClass::Live),
        destructive: false,
    },
    SettingSpec {
        id: SettingId::QaTargetMaxGib,
        section: SettingsSection::MergeQueue,
        label: "QA target size cap (GiB)",
        summary: "Size above which the shared full-library QA build target is emptied before the next run (0 off, else 10-500 GiB, default 30).",
        detail: Some(
            "Each QA run removes the build artifacts a newer run superseded, so the target stays near one commit's footprint (about 15-20 GiB). This cap is the backstop: a target still over it is emptied before the next run starts, which then rebuilds cold. It applies on the hub and on a satellite running a QA run for the hub, and takes effect at the next QA start.",
        ),
        keywords: &[
            "qa", "target", "cap", "disk", "cargo", "build", "size", "library",
        ],
        kind: SettingKind::Cycle,
        owner: SettingOwner::Daemon("qa_target_max_gib"),
        apply: SettingApply::Daemon(ApplyClass::Live),
        destructive: false,
    },
    SettingSpec {
        id: SettingId::JobThermalCeiling,
        section: SettingsSection::MergeQueue,
        label: "Job thermal hold (C)",
        summary: "CPU temperature at or above which no new build or test job starts (0 off, else 60-110 C, default 85).",
        detail: Some(
            "A job submitted while the host's hottest CPU sensor (Linux hwmon: k10temp Tctl/Tdie, coretemp) is at or above this temperature is held, shown as held_for_thermal, with no unit and no timeout clock; it starts, in submission order, once the host is below the ceiling. It applies on the hub and on a satellite alike. Running jobs are not stopped. Hosts without a readable CPU sensor, and other platforms, never hold. Takes effect at the next job start.",
        ),
        keywords: &[
            "thermal",
            "temperature",
            "hot",
            "laptop",
            "satellite",
            "job",
            "tctl",
        ],
        kind: SettingKind::Cycle,
        owner: SettingOwner::Daemon("job_thermal_ceiling_c"),
        apply: SettingApply::Daemon(ApplyClass::Live),
        destructive: false,
    },
    SettingSpec {
        id: SettingId::JobTestTimeout,
        section: SettingsSection::MergeQueue,
        label: "Agent test job timeout (min)",
        summary: "Default wall-clock timeout of an agent's AgentSubmitJob test job (5-180 minutes, default 20); a manager may raise one job.",
        detail: Some(
            "Counted from the command start, excluding build-slot and sandbox/artifact-lock waits. Unit start has a separate ten-minute cap; build admission (waiting for a governor build slot) lasts the job's own timeout for a test job (at least ten minutes) and one hour for a build job, not counted against execution. Past it the daemon stops the job's unit and settles it failed with job_timed_out, which wakes its owner. A job held before launch by a deploy drain is failed job_admission_timed_out after the drain hold cap plus 5 minutes. Candidate-receipt runs keep the unit's own cap unless they name a timeout.",
        ),
        keywords: &["job", "test", "timeout", "runaway", "andon"],
        kind: SettingKind::Cycle,
        owner: SettingOwner::Daemon("job_test_timeout_mins"),
        apply: SettingApply::Daemon(ApplyClass::Live),
        destructive: false,
    },
    SettingSpec {
        id: SettingId::TurnDetachEnabled,
        section: SettingsSection::MergeQueue,
        label: "Detach Claude turns",
        summary: "Run new Claude CLI turns through the durable turn shim (default off).",
        detail: Some(
            "Applies to the next Claude turn. Turning it off reads provider output directly from a pipe. Experimental: enabling is unsafe until transport authority across daemon boots (#1711) lands. Sessions that ran a detached turn are kept in the trash and cannot be purged.",
        ),
        keywords: &["claude", "turn", "detach", "spool", "deploy"],
        kind: SettingKind::Bool,
        owner: SettingOwner::Daemon("turn_detach_enabled"),
        apply: SettingApply::Daemon(ApplyClass::Live),
        destructive: false,
    },
    SettingSpec {
        id: SettingId::DeployDrainEnabled,
        section: SettingsSection::MergeQueue,
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
        id: SettingId::DeployDrainHold,
        section: SettingsSection::MergeQueue,
        label: "Deploy hold limit (s)",
        summary: "How long an agent-requested deploy may hold new launches while it waits for its quiet point (0-3600 seconds, default 600; 0 never holds).",
        detail: Some(
            "A worker turn can run for an hour, so a deploy that held new launches until its max wait starved the manager. Past this limit the deploy keeps waiting for a quiet point (no lander, job or scoped worker mid-turn) without holding anything, then holds again only from its first quiet poll until the restart. An operator restart is not limited. The manager can also cancel its own waiting deploy (AgentRequestDeploy cancel: true).",
        ),
        keywords: &[
            "deploy", "drain", "hold", "limit", "quiet", "restart", "lull",
        ],
        kind: SettingKind::Cycle,
        owner: SettingOwner::Daemon("deploy_drain_hold_secs"),
        apply: SettingApply::Daemon(ApplyClass::Live),
        destructive: false,
    },
    SettingSpec {
        id: SettingId::DeployRestartsPerHour,
        section: SettingsSection::MergeQueue,
        label: "Deploy restarts per hour",
        summary: "Deploy restarts an agent may use in a rolling hour (1-12, default 2).",
        detail: Some(
            "The supervisor uses this persisted setting for its hourly restart budget and adds one restart for rollback or watchdog recovery, with a total ceiling of 13. Past the deploy limit AgentRequestDeploy is refused deploy_restart_budget with a retry_after time (RFC3339): the moment the oldest counted restart leaves the hour. Read at each request, so an applied change takes effect on the next request.",
        ),
        keywords: &["deploy", "restart", "budget", "hour", "limit", "supervisor"],
        kind: SettingKind::Cycle,
        owner: SettingOwner::Daemon("deploy_restarts_per_hour"),
        apply: SettingApply::Daemon(ApplyClass::Live),
        destructive: false,
    },
    SettingSpec {
        id: SettingId::SupervisorShutdownGrace,
        section: SettingsSection::MergeQueue,
        label: "Shutdown grace (s)",
        summary: "How long the supervisor lets a daemon shutdown run before it force-ends it and restarts the daemon (60-3600 seconds, default 120).",
        detail: Some(
            "Once the daemon begins to shut down (a deploy, SIGTERM, a DRAIN restart) the supervisor allows this long. After that it sends SIGTERM again, then SIGKILL to the daemon's process group, and follows the deploy-restart path (a restart, or a rollback to the previous binary when a failed deploy exits quickly). The forced end is recorded as a forced_shutdown friction event and a manager notice on the next daemon start; it is never silent. The supervisor re-reads the value when a shutdown begins. Linux and macOS only; Windows has no supervisor.",
        ),
        keywords: &[
            "shutdown",
            "grace",
            "supervisor",
            "wedge",
            "kill",
            "restart",
            "deploy",
        ],
        kind: SettingKind::Cycle,
        owner: SettingOwner::Daemon("supervisor_shutdown_grace_secs"),
        apply: SettingApply::Daemon(ApplyClass::Live),
        destructive: false,
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
        id: SettingId::LegacyScratchAdoption,
        section: SettingsSection::SandboxStorage,
        label: "Legacy scratch adoption",
        summary: "Lists unrecorded scratch directories (worker TMPDIRs, /var/tmp/rsi-*, lander workspaces) and records chosen ones so scratch reclaim, which is enabled, deletes them automatically once old and unheld. Adopting asks you to confirm every full path first.",
        detail: Some(
            "Each adoption re-runs the full reclaim proof (no live holder, no symlink, an allowed scratch root, clean published git work) and refuses what fails it.",
        ),
        keywords: &["scratch", "adopt", "legacy", "tmpdir", "reclaim"],
        kind: SettingKind::Action,
        owner: SettingOwner::DaemonState("legacy scratch adoption"),
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
        id: SettingId::SourceWorktreeAutoSettlement,
        section: SettingsSection::SandboxStorage,
        label: "Automatic source settlement",
        summary: "Automatically removes clean, merged, terminal sandbox worktrees under disk pressure. Default on; every run has a durable receipt.",
        detail: Some(
            "Uses the same fresh Git and custody proof as manual settlement. Dirty, divergent, active or dependent work is retained. Disabling retains untouched roots, including after restart. Items already moved or removed retain exact recovery.",
        ),
        keywords: &["settlement", "worktree", "automatic", "disk"],
        kind: SettingKind::Bool,
        owner: SettingOwner::Daemon("source_worktree_auto_settlement_enabled"),
        apply: SettingApply::Daemon(ApplyClass::Live),
        destructive: true,
    },
    SettingSpec {
        id: SettingId::DiskPressureMinFreeGb,
        section: SettingsSection::SandboxStorage,
        label: "Disk pressure free space (GB)",
        summary: "Free-space threshold for automatic source settlement; default 100 GB (1024³ bytes per GB), capped at 25% of disk size.",
        detail: Some(
            "Machine local setting, from 1 to 4096 GB. Uses the smaller of this value and 25% of the filesystem: a 192 GB disk has a 48 GB default threshold. Settlement retains untouched roots once free space reaches the effective threshold. Pool trimming has separate controls.",
        ),
        keywords: &["disk", "pressure", "free", "threshold", "settlement"],
        kind: SettingKind::Cycle,
        owner: SettingOwner::Daemon("disk_pressure_min_free_gb"),
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
        id: SettingId::ToolResultStorageCap,
        section: SettingsSection::SandboxStorage,
        label: "Stored tool-result cap (KiB)",
        summary: "Largest body of a tool result kept in the database, in KiB (0 or 4-1024, default 32); a longer one keeps its first and last half around a marker that records its original size.",
        detail: Some(
            "Tool output such as raw cargo logs is most of the database and grows it about 2 GB a month. The cap applies to every provider as a tool result is stored: only the stored copy is cut, so the live session and the TUI still see the whole output. It applies to results stored after the change; rows already stored are never rewritten. A resumed Harness session replays the stored body, marker included. 0 stores every result whole.",
        ),
        keywords: &[
            "tool", "result", "output", "cap", "database", "storage", "truncate", "size",
        ],
        kind: SettingKind::Cycle,
        owner: SettingOwner::Daemon("tool_result_storage_cap_kib"),
        apply: SettingApply::Daemon(ApplyClass::Live),
        destructive: false,
    },
    SettingSpec {
        id: SettingId::ColdArchiveEnabled,
        section: SettingsSection::ColdStorage,
        label: "Cold-storage archive",
        summary: "Moves the bulky rows of old Archived and Deleted sessions into verified, compressed archive files and out of rsi.db (default on).",
        detail: Some(
            "A background pass runs at most every 6 hours at the lowest CPU and I/O priority, outside startup. It writes each cold session's tool rows to an append-only archive file under the archive directory, fsyncs it and reads it back against its checksums; only then, and only while a backup of rsi.db younger than 24 hours exists in ~/.rsi/backups (the pass takes one itself when none does), it deletes those rows from the live database and returns the freed space in small steps. Transcripts still open whole; :cold-restore puts a session's rows back. Shrinking the file once happens at the next supervised restart (:cold-archive compact asks for it). A weekly pass re-reads every archived chunk, and every failure raises an operator to-do. The archive directory is the row below; back it up with the rest of ~/.rsi. The status rows below show what the archive holds and the last pass; the run row (or :cold-archive run) starts a copy-only pass now, :cold-archive move a full one.",
        ),
        keywords: &[
            "cold", "archive", "storage", "compress", "database", "rsi.db",
        ],
        kind: SettingKind::Bool,
        owner: SettingOwner::Daemon("cold_archive_enabled"),
        apply: SettingApply::Daemon(ApplyClass::Live),
        destructive: false,
    },
    SettingSpec {
        id: SettingId::ColdArchiveAfterDays,
        section: SettingsSection::ColdStorage,
        label: "Cold-storage age (days)",
        summary: "A session is cold when its newest event is at least this many days old (3-3650, default 14).",
        detail: Some(
            "Age is measured by the newest conversation event, not the session's updated time, which sandbox maintenance bumps in bulk. Only Archived and Deleted sessions qualify, and a session that a live manager, worker, wake, job, queue entry or topology still references is never cold. Read at the start of each pass.",
        ),
        keywords: &["cold", "archive", "age", "days", "storage"],
        kind: SettingKind::Cycle,
        owner: SettingOwner::Daemon("cold_archive_after_days"),
        apply: SettingApply::Daemon(ApplyClass::Live),
        destructive: false,
    },
    SettingSpec {
        id: SettingId::ColdArchiveDir,
        section: SettingsSection::ColdStorage,
        label: "Cold-storage directory",
        summary: "Where archive files are written: an absolute path outside every sandbox root, or empty for <data dir>/archive. Enter opens a text editor.",
        detail: Some(
            "The daemon checks the path before saving it and again when a pass starts: a symlink or .. that leads into a sandbox root or a .git directory is refused, and the editor shows the reason and keeps your text. Existing archive files keep their recorded path, so a change only affects new files. Back the directory up with the rest of ~/.rsi.",
        ),
        keywords: &["cold", "archive", "directory", "path", "storage", "folder"],
        kind: SettingKind::Action,
        owner: SettingOwner::Daemon("cold_archive_dir"),
        apply: SettingApply::Daemon(ApplyClass::Live),
        destructive: false,
    },
    SettingSpec {
        id: SettingId::ColdArchiveStatus,
        section: SettingsSection::ColdStorage,
        label: "Cold-storage status",
        summary: "Live database size, archive files and sizes, sessions staged, verified and cold, and whether a pass is running; R refreshes it.",
        detail: Some(
            "Read from GetColdArchiveStatus when you open this tab and on R. Open problems (a failed pass, an archive file that failed verification) are listed in the last-run row and by :cold-archive status.",
        ),
        keywords: &["cold", "archive", "status", "size", "database", "storage"],
        kind: SettingKind::ReadOnly,
        owner: SettingOwner::DaemonState("cold archive"),
        apply: SettingApply::Daemon(ApplyClass::Live),
        destructive: false,
    },
    SettingSpec {
        id: SettingId::ColdArchiveLastRun,
        section: SettingsSection::ColdStorage,
        label: "Cold-storage last run",
        summary: "The newest archive pass: its mode, outcome, how many sessions it staged and moved, the bytes written and any problem; R refreshes it.",
        detail: None,
        keywords: &["cold", "archive", "run", "pass", "error", "last"],
        kind: SettingKind::ReadOnly,
        owner: SettingOwner::DaemonState("cold archive"),
        apply: SettingApply::Daemon(ApplyClass::Live),
        destructive: false,
    },
    SettingSpec {
        id: SettingId::ColdArchiveRunNow,
        section: SettingsSection::ColdStorage,
        label: "Run cold-storage pass",
        summary: "Starts a copy-only archive pass now (stage and verify) in the background; the archive must be switched on. :cold-archive move also moves the rows out, :cold-archive verify re-reads every archived chunk.",
        detail: None,
        keywords: &["cold", "archive", "run", "now", "stage", "pass"],
        kind: SettingKind::Action,
        owner: SettingOwner::DaemonState("cold archive"),
        apply: SettingApply::Daemon(ApplyClass::Live),
        destructive: false,
    },
    SettingSpec {
        id: SettingId::SandboxStorageStatus,
        section: SettingsSection::BuildCache,
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
        section: SettingsSection::BuildCache,
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
        section: SettingsSection::BuildCache,
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
        section: SettingsSection::BuildCache,
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
        section: SettingsSection::BuildCache,
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
        section: SettingsSection::BuildCache,
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
        section: SettingsSection::BuildCache,
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
        section: SettingsSection::BuildCache,
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
        section: SettingsSection::BuildCache,
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
        id: SettingId::TargetPoolKeep,
        section: SettingsSection::BuildCache,
        label: "Warm build pools kept",
        summary: "Prebuilt Cargo target pools kept for seeding new workers; older ones are removed after each pool build, and only the newest survives disk pressure.",
        detail: None,
        keywords: &["sandbox", "pool", "target", "build", "cache", "disk"],
        kind: SettingKind::Cycle,
        owner: SettingOwner::Daemon("target_pool_keep"),
        apply: SettingApply::Daemon(ApplyClass::Live),
        destructive: false,
    },
    SettingSpec {
        id: SettingId::TargetPoolStatus,
        section: SettingsSection::BuildCache,
        label: "Warm build pools",
        summary: "Newest good pool, how many commits it trails rolling, and the pools' total size; R refreshes it.",
        detail: None,
        keywords: &["sandbox", "pool", "target", "build", "disk"],
        kind: SettingKind::ReadOnly,
        owner: SettingOwner::DaemonState("target pool"),
        apply: SettingApply::Daemon(ApplyClass::Live),
        destructive: false,
    },
    SettingSpec {
        id: SettingId::AgentBuildJobs,
        section: SettingsSection::AgentBuilds,
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
        section: SettingsSection::AgentBuilds,
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
        section: SettingsSection::AgentBuilds,
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
        section: SettingsSection::AgentBuilds,
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
        section: SettingsSection::AgentBuilds,
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
        id: SettingId::SignalBridge,
        section: SettingsSection::MessageBridges,
        label: "Signal bridge",
        summary: "Signal bridge connection, account and sender allowlist.",
        detail: None,
        keywords: &["signal", "bridge"],
        kind: SettingKind::Edit,
        owner: SettingOwner::File("signal.toml"),
        apply: SettingApply::BridgeRestart,
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
        apply: SettingApply::BridgeRestart,
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
        id: SettingId::RemoteAccess,
        section: SettingsSection::Remote,
        label: "Remote access",
        summary: "Enable the read-only phone view, pick allowed devices and exposed projects, and see the URL.",
        detail: Some(
            "Starts a managed gateway (systemd user unit) and a tailscale serve route; never Funnel. Needs one privilege step, `sudo tailscale set --operator=$USER`, shown here when pending. Disabling stops serving on the next request.",
        ),
        keywords: &["remote", "phone", "tailscale", "gateway", "mobile", "url"],
        kind: SettingKind::Action,
        owner: SettingOwner::DaemonState("remote_access"),
        apply: SettingApply::Immediate,
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
        (
            "action_handler/aws_setup.rs",
            include_str!("action_handler/aws_setup.rs"),
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
                SettingsSection::ContextRotation,
                SettingsSection::StallDetection,
                SettingsSection::MemoryDreaming,
                SettingsSection::Scheduling,
                SettingsSection::RecursiveDag,
                SettingsSection::CodeIntelligence,
            ]
        );
        assert_eq!(
            SettingsGroup::HostAndBuild.sections().collect::<Vec<_>>(),
            vec![
                SettingsSection::ResourceLimits,
                SettingsSection::BuildGovernor,
                SettingsSection::MergeQueue,
                SettingsSection::SandboxStorage,
                SettingsSection::ColdStorage,
                SettingsSection::BuildCache,
                SettingsSection::AgentBuilds,
            ]
        );
    }

    /// #2236: no tab is crowded. Orchestration once held 58 rows and Sandbox
    /// Storage 23; a tab now holds a dozen at most, so a new row goes into the
    /// tab that names it or a new tab, not into a catch-all.
    #[test]
    fn no_section_holds_more_than_a_dozen_rows() {
        for section in SettingsSection::ALL {
            let rows = SETTINGS
                .iter()
                .filter(|spec| spec.section == *section)
                .count();
            assert!(
                rows <= 12,
                "{} holds {rows} rows; split it into named tabs",
                section.label()
            );
        }
    }

    /// #2236: a daemon-owned row with no cached page entry would be skipped
    /// by the page without a trace. Every such row has its entry.
    #[test]
    fn every_daemon_owned_row_has_a_page_entry() {
        let entries = crate::settings::DaemonFeatureEntry::defaults();
        for spec in SETTINGS {
            if let Some(field) = crate::settings_keys::daemon_feature_field_for(spec.id) {
                assert!(
                    entries.iter().any(|entry| entry.field == field),
                    "{:?} ({field}) has a page entry",
                    spec.id
                );
            }
        }
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

    #[test]
    fn live_apply_audit_covers_every_setting_and_converted_control() {
        let audit = include_str!("../../../docs/settings-live-apply-audit.md");
        for spec in SETTINGS {
            assert!(
                audit.contains(&format!("(`{:?}`)", spec.id)),
                "{:?} audit row",
                spec.id
            );
        }
        for id in [
            SettingId::RetryOnStall,
            SettingId::StallDetection,
            SettingId::ReconciliationLoop,
            SettingId::StallClassifier,
            SettingId::ClassifierModel,
            SettingId::ClassifierIdleClaude,
            SettingId::ClassifierIdleCodex,
            SettingId::ClassifierCooldown,
            SettingId::ClassifierMaxPerSession,
            SettingId::ObservationThreshold,
            SettingId::DreamCooldown,
        ] {
            let spec = SETTINGS.iter().find(|spec| spec.id == id).unwrap();
            assert_eq!(spec.apply, SettingApply::Daemon(ApplyClass::Live), "{id:?}");
        }
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
