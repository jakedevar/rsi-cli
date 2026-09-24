//! Status line rendering.

use super::theme;
use crate::app::App;
use ratatui::style::Style;
use ratatui::text::Span;
use rsi_common::types::SessionStatus;

pub(crate) fn render_session_meta_segment_for(
    app: &App,
    session_id: uuid::Uuid,
) -> Option<Vec<Span<'static>>> {
    let session_state = app.sessions.get(&session_id)?;
    let session = &session_state.session;
    let mut meta_parts: Vec<String> = Vec::new();

    // Provider label
    let provider_label = match session.provider {
        rsi_common::types::SessionProvider::Claude => "Claude",
        rsi_common::types::SessionProvider::Codex => "Codex",
        rsi_common::types::SessionProvider::Pioneer => "Pioneer",
        rsi_common::types::SessionProvider::OpenRouter => "OpenRouter",
        rsi_common::types::SessionProvider::Bedrock => "Bedrock",
        rsi_common::types::SessionProvider::Antigravity => "Antigravity",
        rsi_common::types::SessionProvider::Local => "Local",
        rsi_common::types::SessionProvider::CodexAppServer => "Codex(AS)",
        rsi_common::types::SessionProvider::Harness => "Harness",
        _ => "?",
    };
    meta_parts.push(provider_label.to_string());

    // Show active segment model (if model was switched), else session-level model
    let current_model = session_state
        .model_segments
        .iter()
        .rev()
        .find(|seg| seg.to_sequence.is_none())
        .map(|seg| seg.model_id.as_str())
        .or(session.model.as_deref());
    if let Some(model) = current_model {
        meta_parts.push(crate::ui::session::abbreviate_model_name(model));
    }
    if matches!(
        session.status,
        SessionStatus::Running | SessionStatus::Starting
    ) {
        // `work_time_ms` is the daemon-maintained, cumulative total of time
        // spent running (excluding approval waits). It must be used for live
        // sessions rather than deriving an "elapsed" value from `created_at`:
        // sessions can wait, resume, and continue long after their creation.
        if let Some(elapsed) = session
            .work_time_ms
            .map(crate::ui::session::format_work_time_ms)
        {
            meta_parts.push(elapsed);
        }
    } else if let Some(duration) = session.duration_ms {
        let secs = duration / 1000;
        if secs >= 60 {
            meta_parts.push(format!("{}m{}s", secs / 60, secs % 60));
        } else {
            meta_parts.push(format!("{}s", secs));
        }
    }
    if meta_parts.is_empty() {
        None
    } else {
        Some(vec![Span::styled(
            meta_parts.join("  \u{00B7}  "),
            Style::default().fg(theme::metadata_text()),
        )])
    }
}

pub(crate) fn render_context_percent_segment_for(
    app: &App,
    session_id: uuid::Uuid,
) -> Option<Vec<Span<'static>>> {
    let session_state = app.sessions.get(&session_id)?;
    let session = &session_state.session;

    // Single source of truth: the daemon-published pct. The TUI no longer
    // re-derives from `total_input_tokens` / `daemon_input_tokens + daemon_output_tokens`
    // — the daemon's resolver and `live_context_state` have already selected
    // the active denominator, numerator, provenance, and confidence.
    let context = crate::types::compute_context_budget_view(session_state);
    let compact = context.compact_label()?;

    let color = match context.percent {
        Some(pct) if pct < 50.0 => theme::green(),
        Some(pct) if pct < 80.0 => theme::yellow(),
        Some(_) => theme::peach(),
        None => theme::dim_metadata(),
    };

    let (_, rotation_count) = super::session::resolve_session_display(session, &app.sessions);
    let rotation_suffix = if rotation_count > 0 {
        format!(" \u{21BB}{}", rotation_count)
    } else {
        String::new()
    };

    let context_span = Span::styled(
        format!("{} ctx{rotation_suffix}", compact),
        Style::default().fg(color),
    );
    Some(vec![context_span])
}

/// Short label for a provider plan window, e.g. `five_hour` -> `5h` (V99, P1-B).
///
/// Unknown keys fall back to the raw key rather than being hidden: a provider
/// that adds a third window should be visible immediately, even unpolished,
/// rather than silently dropped from the status bar.
fn rate_limit_window_label(window_key: &str) -> &str {
    match window_key {
        "five_hour" => "5h",
        "seven_day" => "7d",
        other => other,
    }
}

/// Render the account-level plan-window utilization segment (V99, P1-B).
///
/// Shows the MOST-CONSUMED window, because that is the one that throttles
/// first and therefore the one worth the status bar's scarce width. Colored on
/// the same <50 / <80 / else thresholds the context segment already uses, so
/// the two read as one system.
///
/// Returns `None` when no snapshot has been observed for the session's
/// provider, in which case the bottom strip renders exactly as it did before —
/// no placeholder, no layout shift.
pub(crate) fn render_rate_limit_segment_for(
    app: &App,
    session_id: uuid::Uuid,
) -> Option<Vec<Span<'static>>> {
    let session_state = app.sessions.get(&session_id)?;
    let snapshot = app
        .provider_rate_limits
        .get(&session_state.session.provider)?;
    let window = snapshot.peak_window()?;

    let pct = (window.utilization * 100.0).clamp(0.0, 100.0);
    let color = if pct < 50.0 {
        theme::green()
    } else if pct < 80.0 {
        theme::yellow()
    } else {
        theme::peach()
    };

    Some(vec![Span::styled(
        format!(
            " {:.0}% {} ",
            pct,
            rate_limit_window_label(&window.window_key)
        ),
        Style::default().fg(color),
    )])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::DaemonClient;
    use crate::state::{DevState, PersistedState};
    use std::path::PathBuf;

    fn test_app() -> App {
        // Reset persisted state so App::new() comes up with the default
        // single-tab layout that contains a SessionList pane. Without this,
        // a saved PersistedState from a prior test run can override the
        // default layout and leave no focused SessionList for tests that
        // need to install a session.
        DevState::clear();
        PersistedState::default().save();
        App::new(DaemonClient::new(PathBuf::from("/tmp/test.sock")))
    }

    // ── render_context_percent_segment tests ──────────────────────────────
    //
    // These tests validate the Phase 2 contract: TUI reads `live_context_pct`
    // directly; no re-derivation from `total_input_tokens` / `daemon_*`
    // fields. And the Phase 3 contract: the `Stale` confidence variant
    // renders a `!` glyph.

    fn make_test_session(
        confidence: rsi_common::types::ContextUsageConfidence,
    ) -> rsi_common::types::Session {
        rsi_common::types::Session {
            context_fill_pct: None,
            id: uuid::Uuid::new_v4(),
            provider: rsi_common::types::SessionProvider::Claude,
            claude_session_id: None,
            query: "test".to_string(),
            title: None,
            agent_role: None,
            epic_spawn_ordinal: None,
            description: None,
            short_summary: None,
            working_dir: PathBuf::from("/tmp"),
            git_branch: None,
            status: rsi_common::types::SessionStatus::Running,
            project_id: None,
            session_kind: rsi_common::types::SessionKind::Standard,
            pending_question: None,
            pending_archive: false,
            pinned_at: None,
            testing_needed_at: None,
            rotation_disabled_at: None,
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
            cost_usd: None,
            duration_ms: None,
            num_turns: None,
            model: None,
            input_tokens: None,
            output_tokens: None,
            context_window: Some(200_000),
            resolved_context_budget: None,
            total_input_tokens: None,
            total_output_tokens: None,
            total_cache_creation_tokens: None,
            total_cache_read_tokens: None,
            stop_reason: None,
            context_usage_confidence: confidence,
            continued_from: None,
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

    /// Install a session and focus the SessionList pane onto it, so
    /// `app.selected_session_state()` returns `Some(&state)`.
    fn install_session(app: &mut App, state: crate::types::SessionState) -> uuid::Uuid {
        use crate::types::Pane;
        let id = state.session.id;
        app.session_order.push(id);
        app.sessions.insert(id, state);
        app.set_project_filter(None);
        let mut installed = false;
        if let Some(Pane::SessionList {
            selected_session, ..
        }) = app.focused_pane_mut()
        {
            *selected_session = Some(id);
            installed = true;
        }
        assert!(
            installed,
            "install_session: focused pane was not SessionList; test_app must provide one"
        );
        assert!(
            app.selected_session_state().is_some(),
            "install_session: selected_session_state() still None after install — check focused_pane vs. sessions map"
        );
        id
    }

    /// Collect a span vector into a plain string (concatenated span contents).
    fn spans_text(spans: &[Span<'static>]) -> String {
        spans.iter().map(|s| s.content.as_ref()).collect::<String>()
    }

    #[test]
    fn running_meta_uses_compact_provider_model_and_accumulated_work_time() {
        let mut app = test_app();
        let mut session = make_test_session(rsi_common::types::ContextUsageConfidence::Full);
        session.model = Some("claude-sonnet-5".to_string());
        session.work_time_ms = Some(3_720_000);
        let id = install_session(&mut app, crate::types::SessionState::new(session));

        let spans = render_session_meta_segment_for(&app, id).expect("segment renders");
        let text = spans_text(&spans);

        assert!(text.contains("Claude"), "got: {text:?}");
        assert!(text.contains("Sonnet 5"), "got: {text:?}");
        assert!(text.contains("1h2m"), "got: {text:?}");
        assert!(!text.contains("created"), "got: {text:?}");
    }

    #[test]
    fn renders_live_context_pct_when_present() {
        use rsi_common::types::ContextUsageConfidence;
        let mut app = test_app();
        let session = make_test_session(ContextUsageConfidence::Full);
        let mut state = crate::types::SessionState::new(session);
        state.live_context_pct = Some(42.0);
        let id = install_session(&mut app, state);

        let spans = render_context_percent_segment_for(&app, id).expect("segment renders");
        let text = spans_text(&spans);
        // " 42% ctx " — no glyph because Full
        assert!(
            text.contains("42%"),
            "expected '42%' in rendered text, got: {:?}",
            text
        );
        assert!(
            text.contains(" ctx"),
            "expected ' ctx' separator in text, got: {:?}",
            text
        );
        // Full should emit no confidence glyph ("" directly after pct).
        assert!(
            !text.contains('!') && !text.contains('?') && !text.contains('≈'),
            "Full confidence must not render any glyph, got: {:?}",
            text
        );
    }

    #[test]
    fn renders_stale_glyph() {
        use rsi_common::types::ContextUsageConfidence;
        let mut app = test_app();
        let session = make_test_session(ContextUsageConfidence::Stale);
        let mut state = crate::types::SessionState::new(session);
        state.live_context_pct = Some(67.0);
        let id = install_session(&mut app, state);

        let spans = render_context_percent_segment_for(&app, id).expect("segment renders");
        let text = spans_text(&spans);
        assert!(
            text.contains('!'),
            "Stale confidence must render '!' glyph, got: {:?}",
            text
        );
        assert!(text.contains("67%"), "pct preserved on Stale: {:?}", text);
    }

    #[test]
    fn codex_context_push_and_refresh_agree_in_row_and_header() {
        use rsi_common::types::{ContextUsageConfidence as Confidence, SessionProvider};
        let mut app = test_app();
        let mut session = make_test_session(Confidence::Missing);
        session.provider = SessionProvider::Codex;
        session.total_input_tokens = Some(50_000);
        session.resolved_context_budget = Some(
            rsi_common::ResolvedContextBudget::new(
                258_400,
                rsi_common::ContextCapacity::default(),
                rsi_common::CapabilityEvidence {
                    source: rsi_common::CapabilitySource::RuntimeTelemetry,
                    source_version: None,
                    source_digest: None,
                    observed_at: None,
                    confidence: rsi_common::CapabilityConfidence::Authoritative,
                },
            )
            .unwrap(),
        );
        let id = install_session(&mut app, crate::types::SessionState::new(session));
        // Exact recorded current counts plus explicit zero/fraction/missing edges.
        for (tokens, pct, confidence, label) in [
            (85_558, 33.110681114551085, Confidence::Full, "33%·T"),
            (224_418, 86.84907120743034, Confidence::Full, "87%·T"),
            (14_589, 5.6458978328173375, Confidence::Full, "6%·T"),
            (1, 100.0 / 258_400.0, Confidence::Full, "<1%·T"),
            (0, 0.0, Confidence::Full, "0%·T"),
            (85_558, 33.110681114551085, Confidence::Stale, "33%!·T"),
            (0, 0.0, Confidence::Missing, "—?·T"),
        ] {
            assert!(app.apply_push_event(rsi_common::rpc::BusEvent {
                event_type: "context_usage_updated".into(),
                timestamp: chrono::Utc::now(),
                data: serde_json::json!({
                    "session_id":id, "pct":pct, "input_tokens":tokens,
                    "output_tokens":0, "daemon_total":0, "confidence":confidence,
                    "context_window":258400,
                }),
            }));
            // A normal session refresh consumes the same daemon projection.
            let refreshed = app.sessions[&id].session.clone();
            app.update_sessions(vec![refreshed]);
            let row = crate::types::row::compute_session_row(
                &app,
                id,
                &crate::settings::UserSettings::default(),
            );
            let crate::types::ContextCell::Display {
                label: row_label,
                pct: row_pct,
            } = row.context_pct
            else {
                panic!("context cell must carry the explicit state");
            };
            assert_eq!(row_label, label);
            assert_eq!(row_pct, (confidence != Confidence::Missing).then_some(pct));
            let header = spans_text(&render_context_percent_segment_for(&app, id).unwrap());
            assert_eq!(header, format!("{label} ctx"));
            assert_eq!(
                app.sessions[&id].session.input_tokens,
                (confidence != Confidence::Missing).then_some(tokens)
            );
            assert_eq!(
                app.sessions[&id].session.total_input_tokens,
                Some(50_000),
                "current context must not overwrite cumulative billing"
            );
        }
    }

    #[test]
    fn compact_context_distinguishes_fresh_stale_cold_and_degraded_sources() {
        use rsi_common::provider_capabilities::{
            CapabilityConfidence, CapabilityEvidence, CapabilitySource, ContextCapacity,
            ResolvedContextBudget,
        };
        use rsi_common::types::ContextUsageConfidence;

        let cases = [
            (
                CapabilitySource::RuntimeTelemetry,
                CapabilityConfidence::Authoritative,
                ContextUsageConfidence::Full,
                Some(42.0),
                "42%·T",
            ),
            (
                CapabilitySource::RuntimeTelemetry,
                CapabilityConfidence::Authoritative,
                ContextUsageConfidence::Stale,
                Some(42.0),
                "42%!·T",
            ),
            (
                CapabilitySource::ProviderCatalog,
                CapabilityConfidence::Verified,
                ContextUsageConfidence::Missing,
                None,
                "—?·C",
            ),
            (
                CapabilitySource::RepositoryFallback,
                CapabilityConfidence::Degraded,
                ContextUsageConfidence::Partial,
                Some(42.0),
                "42%≈·R",
            ),
            (
                CapabilitySource::LegacyUnverified,
                CapabilityConfidence::Degraded,
                ContextUsageConfidence::Missing,
                None,
                "—?·L",
            ),
        ];

        for (source, evidence_confidence, usage_confidence, pct, expected) in cases {
            let mut app = test_app();
            let mut session = make_test_session(usage_confidence);
            session.resolved_context_budget = Some(ResolvedContextBudget {
                active_tokens: 258_400,
                capacity: ContextCapacity::default(),
                evidence: CapabilityEvidence {
                    source,
                    source_version: None,
                    source_digest: None,
                    observed_at: None,
                    confidence: evidence_confidence,
                },
            });
            let mut state = crate::types::SessionState::new(session);
            state.live_context_pct = pct;
            let id = install_session(&mut app, state);

            let spans = render_context_percent_segment_for(&app, id).expect("segment renders");
            let text = spans_text(&spans);
            assert!(
                text.contains(expected),
                "compact state {expected:?} must render positively: {text:?}"
            );
        }
    }

    #[test]
    fn returns_none_without_pct() {
        use rsi_common::types::ContextUsageConfidence;
        let mut app = test_app();
        // live_context_pct is None by default — cold-start TUI state.
        let session = make_test_session(ContextUsageConfidence::Full);
        let state = crate::types::SessionState::new(session);
        let id = install_session(&mut app, state);

        assert!(
            render_context_percent_segment_for(&app, id).is_none(),
            "No pct in TUI state → segment must not render (no stale content from prior frames)"
        );
    }

    #[test]
    fn ignores_session_token_fields_when_pct_absent() {
        // Regression guard: even if the old re-derivation inputs are present
        // on `Session` (total_input_tokens + context_window), the segment
        // must still return None when `live_context_pct` is None.
        use rsi_common::types::ContextUsageConfidence;
        let mut app = test_app();
        let mut session = make_test_session(ContextUsageConfidence::Full);
        session.total_input_tokens = Some(100_000);
        session.context_window = Some(200_000);
        let mut state = crate::types::SessionState::new(session);
        state.live_context_pct = None;
        let id = install_session(&mut app, state);

        assert!(
            render_context_percent_segment_for(&app, id).is_none(),
            "Segment must consult only live_context_pct, not session.total_input_tokens"
        );
    }

    // === V99 / P1-B: plan-window utilization segment ===
    //
    // These assert the POSITIVE rendered content. The "no snapshot" case
    // asserts that the segment function returns None (so the strip renders
    // exactly as before), never that some label is absent from the output.

    fn rate_limit_snapshot(
        provider: rsi_common::types::SessionProvider,
        windows: &[(&str, f64)],
    ) -> rsi_common::rpc::ProviderRateLimitSnapshot {
        rsi_common::rpc::ProviderRateLimitSnapshot {
            provider,
            status: Some("allowed".to_string()),
            rate_limit_type: Some("five_hour".to_string()),
            overage_status: Some("rejected".to_string()),
            is_using_overage: false,
            observed_at: chrono::Utc::now(),
            windows: windows
                .iter()
                .map(
                    |(key, utilization)| rsi_common::rpc::ProviderRateLimitWindow {
                        window_key: (*key).to_string(),
                        utilization: *utilization,
                        resets_at_epoch: Some(1_788_402_000),
                    },
                )
                .collect(),
        }
    }

    #[test]
    fn renders_peak_plan_window_utilization() {
        use rsi_common::types::{ContextUsageConfidence, SessionProvider};
        let mut app = test_app();
        let session = make_test_session(ContextUsageConfidence::Full);
        let id = install_session(&mut app, crate::types::SessionState::new(session));
        app.provider_rate_limits.insert(
            SessionProvider::Claude,
            rate_limit_snapshot(
                SessionProvider::Claude,
                &[("five_hour", 0.27), ("seven_day", 0.05)],
            ),
        );

        let spans = render_rate_limit_segment_for(&app, id).expect("segment renders");
        let text = spans_text(&spans);

        assert!(
            text.contains("27%"),
            "expected the peak window's percentage, got: {text:?}"
        );
        assert!(
            text.contains("5h"),
            "expected the peak window's label, got: {text:?}"
        );
    }

    #[test]
    fn renders_the_seven_day_window_when_it_is_the_peak() {
        // Which window shows depends on which one will throttle first, not on
        // map order — so a busier seven-day window must win.
        use rsi_common::types::{ContextUsageConfidence, SessionProvider};
        let mut app = test_app();
        let session = make_test_session(ContextUsageConfidence::Full);
        let id = install_session(&mut app, crate::types::SessionState::new(session));
        app.provider_rate_limits.insert(
            SessionProvider::Claude,
            rate_limit_snapshot(
                SessionProvider::Claude,
                &[("five_hour", 0.11), ("seven_day", 0.64)],
            ),
        );

        let spans = render_rate_limit_segment_for(&app, id).expect("segment renders");
        let text = spans_text(&spans);
        assert!(text.contains("64%"), "got: {text:?}");
        assert!(text.contains("7d"), "got: {text:?}");
    }

    #[test]
    fn renders_an_unrecognized_window_key_verbatim() {
        // A provider adding a window must be visible immediately, even without
        // a short label for it.
        use rsi_common::types::{ContextUsageConfidence, SessionProvider};
        let mut app = test_app();
        let session = make_test_session(ContextUsageConfidence::Full);
        let id = install_session(&mut app, crate::types::SessionState::new(session));
        app.provider_rate_limits.insert(
            SessionProvider::Claude,
            rate_limit_snapshot(SessionProvider::Claude, &[("thirty_day", 0.42)]),
        );

        let spans = render_rate_limit_segment_for(&app, id).expect("segment renders");
        let text = spans_text(&spans);
        assert!(text.contains("42%"), "got: {text:?}");
        assert!(
            text.contains("thirty_day"),
            "an unknown window key renders verbatim rather than vanishing, got: {text:?}"
        );
    }

    #[test]
    fn utilization_thresholds_match_the_context_segment() {
        let _pinned_theme = crate::ui::theme::pin_theme_state();
        use rsi_common::types::{ContextUsageConfidence, SessionProvider};
        for (utilization, expected) in [
            (0.10_f64, theme::green()),
            (0.49_f64, theme::green()),
            (0.50_f64, theme::yellow()),
            (0.79_f64, theme::yellow()),
            (0.80_f64, theme::peach()),
            (0.99_f64, theme::peach()),
        ] {
            let mut app = test_app();
            let session = make_test_session(ContextUsageConfidence::Full);
            let id = install_session(&mut app, crate::types::SessionState::new(session));
            app.provider_rate_limits.insert(
                SessionProvider::Claude,
                rate_limit_snapshot(SessionProvider::Claude, &[("five_hour", utilization)]),
            );

            let spans = render_rate_limit_segment_for(&app, id).expect("segment renders");
            assert_eq!(
                spans[0].style.fg,
                Some(expected),
                "utilization {utilization} took the wrong threshold branch"
            );
        }
    }

    #[test]
    fn returns_none_without_a_snapshot() {
        // Fresh daemon, nothing observed yet: the segment does not render, so
        // the bottom strip is byte-identical to its pre-V99 output.
        use rsi_common::types::ContextUsageConfidence;
        let mut app = test_app();
        let session = make_test_session(ContextUsageConfidence::Full);
        let id = install_session(&mut app, crate::types::SessionState::new(session));

        assert!(
            render_rate_limit_segment_for(&app, id).is_none(),
            "no observation yet must render nothing, not a placeholder"
        );
    }

    #[test]
    fn returns_none_for_a_provider_with_no_observation() {
        // A Claude snapshot must not be shown against a Codex session: plan
        // windows are per-account-per-provider.
        use rsi_common::types::{ContextUsageConfidence, SessionProvider};
        let mut app = test_app();
        let mut session = make_test_session(ContextUsageConfidence::Full);
        session.provider = SessionProvider::Codex;
        let id = install_session(&mut app, crate::types::SessionState::new(session));
        app.provider_rate_limits.insert(
            SessionProvider::Claude,
            rate_limit_snapshot(SessionProvider::Claude, &[("five_hour", 0.27)]),
        );

        assert!(
            render_rate_limit_segment_for(&app, id).is_none(),
            "a Codex session must not display Claude's plan windows"
        );
    }

    #[test]
    fn returns_none_when_a_snapshot_carries_no_windows() {
        use rsi_common::types::{ContextUsageConfidence, SessionProvider};
        let mut app = test_app();
        let session = make_test_session(ContextUsageConfidence::Full);
        let id = install_session(&mut app, crate::types::SessionState::new(session));
        app.provider_rate_limits.insert(
            SessionProvider::Claude,
            rate_limit_snapshot(SessionProvider::Claude, &[]),
        );

        assert!(render_rate_limit_segment_for(&app, id).is_none());
    }

    #[test]
    fn context_segment_is_unaffected_by_the_new_segment() {
        // P1-B is purely additive: the context segment must render exactly as
        // it did before, snapshot present or not.
        use rsi_common::types::{ContextUsageConfidence, SessionProvider};
        let mut app = test_app();
        let session = make_test_session(ContextUsageConfidence::Full);
        let mut state = crate::types::SessionState::new(session);
        state.live_context_pct = Some(42.0);
        let id = install_session(&mut app, state);

        let before = spans_text(
            &render_context_percent_segment_for(&app, id).expect("context segment renders"),
        );
        app.provider_rate_limits.insert(
            SessionProvider::Claude,
            rate_limit_snapshot(SessionProvider::Claude, &[("five_hour", 0.27)]),
        );
        let after = spans_text(
            &render_context_percent_segment_for(&app, id).expect("context segment renders"),
        );

        assert_eq!(before, after, "context segment must be untouched by P1-B");
        assert!(after.contains("42%"), "got: {after:?}");
    }
}
