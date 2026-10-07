//! Placement-aware definitions for symbol help. Shapes come from live producers.

use ratatui::style::Color;
use rsi_common::provider_capabilities::CapabilitySource;
use rsi_common::types::{ContextUsageConfidence, SessionProvider, SessionStatus};

use super::*;
use crate::types::InspectorSignal;
use crate::types::context_budget::{
    capability_indicator, capability_source_label, usage_indicator,
};

pub(crate) struct SymbolEntry {
    pub category: &'static str,
    pub glyph: String,
    pub name: String,
    pub location: &'static str,
    pub description: &'static str,
    pub color: Color,
}

fn add(
    entries: &mut Vec<SymbolEntry>,
    category: &'static str,
    glyph: impl Into<String>,
    name: impl Into<String>,
    location: &'static str,
    description: &'static str,
    color: Color,
) {
    entries.push(SymbolEntry {
        category,
        glyph: glyph.into(),
        name: name.into(),
        location,
        description,
        color,
    });
}

/// Resolve colors on each render so the legend follows live theme changes.
pub(crate) fn entries() -> Vec<SymbolEntry> {
    let mut entries = Vec::new();
    for (status, description) in [
        (
            SessionStatus::Starting,
            "Provider is starting; work has not begun yet.",
        ),
        (SessionStatus::Running, "Session is actively running."),
        (
            SessionStatus::WaitingApproval,
            "Session is waiting for approval.",
        ),
        (SessionStatus::Completed, "Session finished successfully."),
        (
            SessionStatus::Failed,
            "Session failed; inspect its failure evidence.",
        ),
        (
            SessionStatus::Interrupted,
            "Session stopped before normal completion.",
        ),
        (SessionStatus::Archived, "Session is archived."),
    ] {
        add(
            &mut entries,
            "Lifecycle",
            crate::types::row::navigator_lifecycle_icon(status),
            status_word(status),
            "Navigator status (S)",
            description,
            theme::status_color(status),
        );
    }
    add(
        &mut entries,
        "Lifecycle",
        CONTAINER_RUNNING,
        "Running descendants",
        "Container status (S)",
        "Group or Epic contains starting or running agents; this summarizes its children.",
        theme::status_running(),
    );
    add(
        &mut entries,
        "Lifecycle",
        SOFT_PAUSE,
        "Operator soft pause",
        "Navigator status (S)",
        "Interrupted session paused by the operator.",
        theme::warning_status(),
    );
    for status in [
        SessionStatus::Starting,
        SessionStatus::WaitingApproval,
        SessionStatus::Failed,
        SessionStatus::Interrupted,
        SessionStatus::Archived,
        SessionStatus::Deleted,
    ] {
        add(
            &mut entries,
            "Lifecycle",
            crate::ui::session::status_icon(status),
            status_word(status),
            "Detail / activity / browsers",
            "Static lifecycle variant used outside the navigator; read its location and label.",
            theme::status_color(status),
        );
    }
    add(
        &mut entries,
        "Lifecycle",
        crate::ui::session::BRAILLE_FRAMES[0],
        "Active spinner",
        "Transcript header",
        "Animated Braille frames mean starting or running.",
        theme::status_running(),
    );

    for (signal, description) in [
        (
            InspectorSignal::NeedsInput,
            "Pending question or approval. In the attention column, ! also flags failure and takes priority over retry, stall and unread.",
        ),
        (
            InspectorSignal::Failed,
            "Failure signal; open the inspector for evidence and recovery.",
        ),
        (
            InspectorSignal::Retry { attempt: 1, max: 3 },
            "Automatic retry is pending; counts show attempt and limit. Different from context rotation ↻.",
        ),
        (
            InspectorSignal::Stalled,
            "No recent output; session may be stalled.",
        ),
        (
            InspectorSignal::Unread,
            "Output arrived since the operator last viewed this session.",
        ),
        (InspectorSignal::Pinned, "Session is pinned."),
        (
            InspectorSignal::TestingNeeded,
            "Session is marked as needing testing.",
        ),
        (
            InspectorSignal::RotationDisabled,
            "Automatic context rotation is disabled.",
        ),
        (
            InspectorSignal::PendingArchive,
            "Session is pending archive.",
        ),
        (InspectorSignal::EpicLead, "Session leads an Epic."),
    ] {
        let (glyph, color) = signal_glyph(signal);
        add(
            &mut entries,
            "Attention and flags",
            glyph,
            signal.label(),
            "Attention column / inspector",
            description,
            color,
        );
    }

    for (glyph, name, location, description) in [
        (
            GROUP_CONTAINER,
            "Group",
            "Function / title",
            "Container organizing sessions and Epics.",
        ),
        (
            EPIC_CONTAINER,
            "Epic",
            "Function / title",
            "Container for coordinated work; keeps its own name.",
        ),
        (
            "M",
            "Manager",
            "Navigator ordinal (#)",
            "Manager row; ordinary rows show their assigned ordinal.",
        ),
        (
            "∞",
            "Pin column",
            "Navigator header",
            "Column heading; a pinned row shows ◆.",
        ),
        (
            MANAGERS,
            "Managers",
            "Navigator group heading",
            "Group of manager sessions.",
        ),
        (
            DESCENDANTS,
            "Descendant totals",
            "Inspector",
            "Summary counts for work beneath a container.",
        ),
        (
            BELOW,
            "Rows below",
            "Navigator footer",
            "Count of additional rows below the visible list.",
        ),
    ] {
        add(
            &mut entries,
            "Identity and hierarchy",
            glyph,
            name,
            location,
            description,
            theme::accent(),
        );
    }

    for (provider, name) in [
        (SessionProvider::Claude, "Claude"),
        (SessionProvider::Codex, "Codex"),
        (SessionProvider::CodexAppServer, "Codex App Server"),
        (SessionProvider::Pioneer, "Pioneer"),
        (SessionProvider::OpenRouter, "OpenRouter"),
        (SessionProvider::Bedrock, "Bedrock"),
        (SessionProvider::Local, "Local"),
        (SessionProvider::Antigravity, "Antigravity"),
        (SessionProvider::Harness, "Harness"),
    ] {
        add(
            &mut entries,
            "Providers",
            provider_glyph(provider),
            name,
            "Provider / model column",
            "Provider mark before the model name; inspector shows the full model ID.",
            provider_color(provider),
        );
    }

    for (glyph, name, description) in [
        (
            CONTEXT,
            "Context fill",
            "Context used as a percentage of the active budget.",
        ),
        (
            CONTEXT_UNKNOWN,
            "Unknown context",
            "Context usage is unknown; it does not mean 0%.",
        ),
        (TURNS, "Turns", "Recorded conversation turn count."),
        (
            EFFORT,
            "Effort",
            "Recorded reasoning effort; bars scale to the model's supported ladder.",
        ),
        (
            "?",
            "Unknown effort",
            "Recorded effort is unrecognized; an absent effort never invents a default.",
        ),
    ] {
        add(
            &mut entries,
            "Usage and effort",
            glyph,
            name,
            "Usage columns / inspector",
            description,
            theme::yellow(),
        );
    }
    add(
        &mut entries,
        "Usage and effort",
        EFFORT_LEVELS.join(""),
        "Effort levels",
        "Model / effort",
        "One-cell height grows from lower to higher effort. Scale depends on the model.",
        theme::yellow(),
    );
    add(
        &mut entries,
        "Usage and effort",
        percent_gauge(50.0, 10),
        "Percentage gauge",
        "Context / CPU inspector",
        "Filled cells show the used fraction; empty cells show remaining capacity.",
        theme::yellow(),
    );
    add(
        &mut entries,
        "Usage and effort",
        format!("{ROTATION}N"),
        "Rotation depth",
        "Title / context / ROT",
        "N counts context rotations in this session lineage.",
        theme::subtext0(),
    );

    for (confidence, name, description) in [
        (
            ContextUsageConfidence::Partial,
            "Approximate usage",
            "Usage is partial or approximate.",
        ),
        (
            ContextUsageConfidence::Stale,
            "Stale usage",
            "Last usage observation is stale; here ! describes freshness, not operator attention.",
        ),
        (
            ContextUsageConfidence::Missing,
            "Missing usage",
            "No usage measurement is available; unknown is distinct from zero.",
        ),
    ] {
        add(
            &mut entries,
            "Context confidence and source",
            usage_indicator(confidence),
            name,
            "After context percentage",
            description,
            theme::yellow(),
        );
    }
    for source in [
        CapabilitySource::RuntimeTelemetry,
        CapabilitySource::Configured,
        CapabilitySource::ProviderCatalog,
        CapabilitySource::RepositoryFallback,
        CapabilitySource::LegacyUnverified,
        CapabilitySource::OfficialDocumentation,
    ] {
        add(
            &mut entries,
            "Context confidence and source",
            format!("·{}", capability_indicator(source)),
            capability_source_label(source),
            "After context confidence",
            "Letter identifies evidence for the context budget, not usage confidence.",
            theme::dim_metadata(),
        );
    }
    for (confidence, source, prefix, name, description) in [
        (
            ContextUsageConfidence::Partial,
            CapabilitySource::RepositoryFallback,
            "42%",
            "Approximate context example",
            "42% used, approximate usage; budget from repository fallback.",
        ),
        (
            ContextUsageConfidence::Stale,
            CapabilitySource::RuntimeTelemetry,
            "42%",
            "Stale context example",
            "42% used, stale usage; budget from runtime telemetry.",
        ),
        (
            ContextUsageConfidence::Missing,
            CapabilitySource::ProviderCatalog,
            "—",
            "Unknown context example",
            "Usage unknown; budget from provider catalog. No confidence suffix means full or counted usage.",
        ),
    ] {
        add(
            &mut entries,
            "Context confidence and source",
            format!(
                "{prefix}{}·{}",
                usage_indicator(confidence),
                capability_indicator(source)
            ),
            name,
            "Context cell / status bar",
            description,
            theme::yellow(),
        );
    }

    for (glyph, name, description) in [
        (
            SESSION_ID,
            "Session ID",
            "Complete UUID in inspector; # in navigator header instead labels ordinals.",
        ),
        (
            WORKING_DIR,
            "Working directory",
            "Directory in which the session runs. In a provider cell, ⌂ instead means Local.",
        ),
        (
            SANDBOX,
            "Sandbox",
            "Isolated worktree assigned to this session.",
        ),
        (
            BRANCH,
            "Git branch",
            "Branch assigned to the worktree; ⊡ ⎇ combines matching sandbox and branch identifiers.",
        ),
        (CREATED, "Created", "Session creation time."),
        (UPDATED, "Updated", "Last session update time."),
        (
            WORK_TIME,
            "Work time",
            "Accumulated active run time; excludes approval waits.",
        ),
        (COST, "Cost", "Recorded spend in US dollars."),
        (
            LOCATION,
            "Location separator",
            "Separates parts of a hierarchy or location path.",
        ),
        (QUOTE_RAIL, "Quoted output", "Latest output excerpt."),
        (
            NEXT,
            "Next action / work",
            "Current work or next suggested action; inspector text explains which.",
        ),
        (ARTIFACT, "Artifact", "Linked work or completion artifact."),
        (HANDOFF, "Handoff", "Handoff note for continued work."),
        (
            FOLLOW_UP,
            "Follow-up",
            "Follow-up work indicator; ⚑ in activity instead labels the queue.",
        ),
        (QUEUE, "Queue", "Queued activity summary."),
        (
            CHANGES,
            "Changes",
            "Change summary; Δ beside a timestamp instead means updated.",
        ),
        (
            REQUIRED,
            "Required input",
            "Input required to proceed; ? in status instead means waiting approval.",
        ),
        (
            PREVIOUS,
            "Previous session",
            "Previous session in the continuation lineage.",
        ),
        (
            "—",
            "Missing value",
            "No recorded value or unassigned metadata; does not mean zero.",
        ),
    ] {
        add(
            &mut entries,
            "Inspector and activity",
            glyph,
            name,
            "Inspector / activity metadata",
            description,
            theme::dim_metadata(),
        );
    }

    for &(role, code, _) in KNOWN_ROLES {
        add(
            &mut entries,
            "Roles",
            code,
            role,
            "Function / title",
            "Two-letter role abbreviation. Color groups lead, plan, build, debug or check roles.",
            role_color(role),
        );
    }
    add(
        &mut entries,
        "Roles",
        role_code("Agent"),
        "Other role abbreviations",
        "Function / title",
        "Unrecognized roles use their first two letters; inspector retains the full role name.",
        theme::overlay1(),
    );
    entries
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn symbol_legend_covers_live_provider_role_signal_and_lifecycle_encodings() {
        let entries = entries();
        for provider in [
            SessionProvider::Claude,
            SessionProvider::Codex,
            SessionProvider::CodexAppServer,
            SessionProvider::Pioneer,
            SessionProvider::OpenRouter,
            SessionProvider::Bedrock,
            SessionProvider::Local,
            SessionProvider::Antigravity,
            SessionProvider::Harness,
        ] {
            assert!(
                entries.iter().any(|entry| entry.category == "Providers"
                    && entry.glyph == provider_glyph(provider))
            );
        }
        for &(role, _, _) in KNOWN_ROLES {
            assert!(entries.iter().any(|entry| entry.category == "Roles"
                && entry.name == role
                && entry.glyph == role_code(role)));
        }
        for status in [
            SessionStatus::Starting,
            SessionStatus::Running,
            SessionStatus::WaitingApproval,
            SessionStatus::Completed,
            SessionStatus::Failed,
            SessionStatus::Interrupted,
            SessionStatus::Archived,
        ] {
            assert!(
                entries
                    .iter()
                    .any(|entry| entry.location == "Navigator status (S)"
                        && entry.glyph == crate::types::row::navigator_lifecycle_icon(status))
            );
        }
        for signal in [
            InspectorSignal::NeedsInput,
            InspectorSignal::Failed,
            InspectorSignal::Retry { attempt: 1, max: 3 },
            InspectorSignal::Stalled,
            InspectorSignal::Unread,
            InspectorSignal::Pinned,
            InspectorSignal::TestingNeeded,
            InspectorSignal::RotationDisabled,
            InspectorSignal::PendingArchive,
            InspectorSignal::EpicLead,
        ] {
            assert!(
                entries
                    .iter()
                    .any(|entry| entry.name == signal.label()
                        && entry.glyph == signal_glyph(signal).0)
            );
        }
    }

    #[test]
    fn symbol_legend_distinguishes_same_shape_by_placement() {
        let entries = entries();
        for (glyph, name, location) in [
            ("◉", "Running descendants", "Container status (S)"),
            ("◉", "Codex App Server", "Provider / model column"),
            ("⌂", "Local", "Provider / model column"),
            ("⌂", "Working directory", "Inspector / activity metadata"),
            ("!", "Stale usage", "After context percentage"),
            ("▣", "Group", "Function / title"),
            ("▣", "archived", "Detail / activity / browsers"),
        ] {
            assert!(
                entries.iter().any(|entry| entry.glyph == glyph
                    && entry.name == name
                    && entry.location == location),
                "{glyph} {name} at {location} must have its own meaning"
            );
        }
    }

    #[test]
    fn symbol_legend_examples_follow_context_meter_encoding() {
        let entries = entries();
        for (confidence, source, name) in [
            (
                ContextUsageConfidence::Partial,
                CapabilitySource::RepositoryFallback,
                "Approximate context example",
            ),
            (
                ContextUsageConfidence::Stale,
                CapabilitySource::RuntimeTelemetry,
                "Stale context example",
            ),
        ] {
            let view = crate::types::ContextBudgetViewModel {
                percent: Some(42.0),
                usage_tokens: None,
                usage_confidence: confidence,
                resolved: None,
            };
            let usage = view.compact_label().unwrap();
            let expected = format!("{usage}·{}", capability_indicator(source));
            assert!(
                entries
                    .iter()
                    .any(|entry| entry.name == name && entry.glyph == expected)
            );
        }
    }
}
