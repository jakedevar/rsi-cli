//! Rendering for the operator source-worktree settlement overlay.

use crate::types::SourceWorktreeSettlementOverlayState;
use crate::ui::theme;
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Clear, Padding, Paragraph, Wrap};

use super::fixed_centered_rect;

pub(super) fn render_source_worktree_settlement(
    frame: &mut Frame,
    area: Rect,
    state: &SourceWorktreeSettlementOverlayState,
) {
    let width = area.width.saturating_sub(4).min(140).max(20);
    let height = area.height.saturating_sub(4).min(46).max(10);
    let popup = fixed_centered_rect(area, width, height);
    frame.render_widget(Clear, popup);
    let block = theme::overlay_block()
        .title(Line::from(Span::styled(
            " Source Worktree Settlement — destructive local maintenance ",
            Style::default()
                .fg(theme::overlay_title())
                .add_modifier(Modifier::BOLD),
        )))
        .padding(Padding::horizontal(1));
    let inner = block.inner(popup);
    frame.render_widget(block, popup);
    if inner.height < 3 {
        return;
    }

    let mut lines = Vec::new();
    lines.push(Line::from(vec![
        key("j/k"),
        muted(" select  "),
        key("Enter"),
        muted(" restart audit  "),
        key("n"),
        muted(" next page  "),
        key("A"),
        muted(" apply  "),
        key("r"),
        muted(" receipt  "),
        key("J/K"),
        muted(" scroll  "),
        key("Esc"),
        muted(" close"),
    ]));
    lines.push(Line::default());

    if state.cohorts.is_empty() {
        lines.push(Line::from(Span::styled(
            "No Live source-worktree cohorts or durable settlement receipts.",
            Style::default().fg(theme::subtext0()),
        )));
    } else {
        lines.push(section("Daemon-discovered cohorts"));
        for (index, cohort) in state.cohorts.iter().enumerate() {
            let selected = index == state.selected_index;
            let marker = if selected { "▸" } else { " " };
            let style = if selected {
                Style::default().fg(theme::text()).bg(theme::surface2())
            } else {
                Style::default().fg(theme::subtext0())
            };
            lines.push(Line::from(Span::styled(
                format!(
                    "{marker} {}  live roots {}  terminal roots {}{}",
                    cohort.canonical_repo_dir,
                    cohort.live_roots,
                    cohort.terminal_roots,
                    if cohort.live_roots == 0 {
                        "  receipt-backed"
                    } else {
                        ""
                    }
                ),
                style,
            )));
            lines.push(Line::from(Span::styled(
                format!("    identity {}", cohort.repository_identity),
                Style::default().fg(theme::subtext0()),
            )));
        }
    }

    if !state.recent_runs.is_empty() {
        lines.push(Line::default());
        lines.push(section(
            "Recent settlement runs (automatic, agent and operator)",
        ));
        for run in &state.recent_runs {
            let origin = attribution_label(run.attribution.as_ref());
            lines.push(normal(format!(
                "{} · {} · {:?} · settled {} · refused {} · recovery {}",
                run.run_id,
                origin,
                run.state,
                run.counts.settled,
                run.counts.refused,
                run.counts.recovery_required
            )));
            lines.push(normal(format!(
                "    {} · {}",
                run.created_at, run.repository_identity
            )));
        }
    }

    if let Some(audit) = &state.audit {
        lines.push(Line::default());
        lines.push(section(if is_receipt_recovery_audit(state, audit) {
            "Receipt recovery audit (writes=0)"
        } else {
            "Fresh live audit (writes=0)"
        }));
        lines.push(normal(format!(
            "target {} @ {}",
            audit.target_ref.as_deref().unwrap_or("<unavailable>"),
            audit
                .target_oid
                .as_ref()
                .map(|oid| oid.as_str())
                .unwrap_or("<unavailable>")
        )));
        lines.push(normal(format!("plan digest {}", audit.plan_digest)));
        lines.push(normal(format!(
            "observed {} · eligible {} · retained {} · applyable {}",
            audit.counts.observed,
            audit.counts.eligible,
            audit.counts.retained,
            if audit.applyable { "yes" } else { "no" }
        )));
        if let Some(refusal) = &audit.refusal {
            lines.push(error_line(format!("cohort refusal: {refusal}")));
        }
        if let Some(phrase) = &audit.authorization_phrase {
            lines.push(Line::from(vec![
                Span::styled(
                    "type exactly: ",
                    Style::default().fg(theme::warning_status()),
                ),
                Span::styled(
                    phrase,
                    Style::default()
                        .fg(theme::text())
                        .add_modifier(Modifier::BOLD),
                ),
            ]));
        }
        if let Some(page) = &audit.traversal {
            lines.push(normal(format!(
                "Page: {} examined of {} Live roots · {} unexamined · {}",
                audit.counts.observed,
                page.snapshot_root_count,
                page.snapshot_root_count
                    .saturating_sub(audit.counts.observed),
                if page.has_more {
                    "n: next page"
                } else {
                    "end of traversal"
                },
            )));
            lines.push(normal(format!("Snapshot {}", page.snapshot_digest)));
        }
        for item in &audit.items {
            let diagnostic = item
                .diagnostic
                .as_ref()
                .map(|value| format!(" · {value}"))
                .unwrap_or_default();
            lines.push(normal(format!(
                "{} · {:?} · {:?}{}",
                item.session_id, item.disposition, item.proof, diagnostic
            )));
        }
    }

    if state.authorization_active {
        lines.push(Line::default());
        lines.push(section("Authorization input (not prefilled)"));
        let style = Style::default().fg(theme::warning_status());
        let mut spans = vec![Span::styled("> ", style)];
        spans.extend(crate::field_edit::draw(&state.authorization_input, style));
        lines.push(Line::from(spans));
        lines.push(Line::from(Span::styled(
            "Enter submit · Esc cancel",
            Style::default().fg(theme::subtext0()),
        )));
    }

    if let Some(receipt) = &state.receipt {
        lines.push(Line::default());
        lines.push(section("Durable receipt"));
        lines.push(normal(format!(
            "origin {}",
            attribution_label(receipt.attribution.as_ref())
        )));
        lines.push(normal(format!(
            "run {} · state {:?} · updated {}",
            receipt.run_id, receipt.state, receipt.updated_at
        )));
        lines.push(normal(format!(
            "settled {} · refused {} · recovery {} · unattempted {}",
            receipt.counts.settled,
            receipt.counts.refused,
            receipt.counts.recovery_required,
            receipt.counts.unattempted
        )));
        if let Some(error) = &receipt.terminal_error {
            lines.push(error_line(format!("last error: {error}")));
        }
        for item in &receipt.items {
            lines.push(normal(format!(
                "#{} {} · {:?}{}",
                item.sequence,
                item.source_ref,
                item.phase,
                item.refusal_code
                    .as_ref()
                    .map(|code| format!(" · {code:?}"))
                    .unwrap_or_default()
            )));
            if let Some(after) = &item.after_observation {
                lines.push(Line::from(Span::styled(
                    format!("    {after}"),
                    Style::default().fg(theme::subtext0()),
                )));
            }
        }
        if !receipt.unproven_survivors.is_empty() {
            // #149: processes the run could not prove and so left running.
            lines.push(section(
                "Unproven survivors (left running: settlement never kills an unproven process)",
            ));
            for survivor in &receipt.unproven_survivors {
                lines.push(Line::from(Span::styled(
                    format!(
                        "pid {} · {} · {} · {}",
                        survivor.pid,
                        survivor
                            .executable
                            .as_deref()
                            .unwrap_or("<executable unknown>"),
                        survivor.sandbox_root.as_ref().map_or_else(
                            || format!("cohort repository {}", receipt.canonical_repo_dir),
                            |root| format!("root {root}"),
                        ),
                        survivor.reason.label()
                    ),
                    Style::default().fg(theme::warning_status()),
                )));
            }
            lines.push(Line::from(Span::styled(
                "    stop these by hand if they belong to the settled work",
                Style::default().fg(theme::subtext0()),
            )));
        }
    }

    if let Some(error) = &state.last_error {
        lines.push(Line::default());
        lines.push(error_line(error.clone()));
    }

    frame.render_widget(
        Paragraph::new(lines)
            .style(Style::default().bg(theme::overlay_bg()))
            .wrap(Wrap { trim: false })
            .scroll((state.scroll_offset.min(u16::MAX as usize) as u16, 0)),
        inner,
    );
}

fn is_receipt_recovery_audit(
    state: &SourceWorktreeSettlementOverlayState,
    audit: &rsi_common::cohort_settlement::SourceWorktreeCohortAuditV1,
) -> bool {
    audit.items.is_empty()
        && audit.run_id.is_some()
        && state
            .cohorts
            .get(state.selected_index)
            .is_some_and(|cohort| {
                cohort.live_roots == 0
                    && cohort.repository_identity == audit.repository_identity
                    && cohort.canonical_repo_dir == audit.canonical_repo_dir
            })
}

fn attribution_label(
    attribution: Option<&rsi_common::cohort_settlement::SourceWorktreeSettlementAttributionV1>,
) -> String {
    use rsi_common::cohort_settlement::SourceWorktreeSettlementOriginV1;
    match attribution {
        Some(value) => match value.origin {
            SourceWorktreeSettlementOriginV1::Automatic => "automatic daemon".into(),
            SourceWorktreeSettlementOriginV1::Operator => "operator".into(),
            SourceWorktreeSettlementOriginV1::Agent => format!(
                "agent {}",
                value
                    .agent_session_id
                    .map(|id| id.to_string())
                    .unwrap_or_default()
            ),
        },
        None => "historical (unattributed)".into(),
    }
}

fn key(value: &'static str) -> Span<'static> {
    Span::styled(
        value,
        Style::default()
            .fg(theme::accent())
            .add_modifier(Modifier::BOLD),
    )
}

fn muted(value: &'static str) -> Span<'static> {
    Span::styled(value, Style::default().fg(theme::subtext0()))
}

fn section(value: &'static str) -> Line<'static> {
    Line::from(Span::styled(
        value,
        Style::default()
            .fg(theme::accent())
            .add_modifier(Modifier::BOLD),
    ))
}

fn normal(value: String) -> Line<'static> {
    Line::from(Span::styled(value, Style::default().fg(theme::text())))
}

fn error_line(value: String) -> Line<'static> {
    Line::from(Span::styled(
        value,
        Style::default()
            .fg(theme::error_status())
            .add_modifier(Modifier::BOLD),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::{Terminal, backend::TestBackend};
    use rsi_common::cohort_settlement::{
        SOURCE_WORKTREE_EMPTY_DEPENDENCY_DIGEST,
        SOURCE_WORKTREE_EMPTY_SESSION_PATH_DEPENDENCY_DIGEST,
        SOURCE_WORKTREE_SETTLEMENT_POLICY_VERSION, SOURCE_WORKTREE_SETTLEMENT_SCHEMA_VERSION,
        SourceWorktreeAuditItemV1, SourceWorktreeCohortAuditV1, SourceWorktreeCohortSummaryV1,
        SourceWorktreeDispositionV1, SourceWorktreeGitOidV1, SourceWorktreeProofV1,
        SourceWorktreeSettlementCountsV1, SourceWorktreeSettlementItemV1,
        SourceWorktreeSettlementPhaseV1, SourceWorktreeSettlementRunStateV1,
        SourceWorktreeSettlementRunV1, SourceWorktreeUnprovenReasonV1,
        SourceWorktreeUnprovenSurvivorV1,
    };
    use rsi_common::types::Sha256Digest;

    fn empty_state(scroll_offset: usize) -> SourceWorktreeSettlementOverlayState {
        SourceWorktreeSettlementOverlayState {
            recent_runs: Vec::new(),
            cohorts: Vec::new(),
            selected_index: 0,
            scroll_offset,
            audit: None,
            receipt: None,
            authorization_input: String::new(),
            authorization_active: false,
            idempotency_key: None,
            last_error: None,
        }
    }

    #[test]
    fn settlement_history_shows_automatic_run_origin_and_result() {
        use rsi_common::cohort_settlement::{
            SourceWorktreeSettlementAttributionV1, SourceWorktreeSettlementOriginV1,
            SourceWorktreeSettlementRunSummaryV1,
        };
        let mut state = empty_state(0);
        let run_id = uuid::Uuid::new_v4();
        state
            .recent_runs
            .push(SourceWorktreeSettlementRunSummaryV1 {
                run_id,
                repository_identity: "repo:automatic-history".into(),
                state: SourceWorktreeSettlementRunStateV1::Settled,
                counts: SourceWorktreeSettlementCountsV1 {
                    settled: 3,
                    ..Default::default()
                },
                created_at: "2026-10-12T00:00:00.000000000Z".into(),
                attribution: Some(SourceWorktreeSettlementAttributionV1 {
                    origin: SourceWorktreeSettlementOriginV1::Automatic,
                    agent_session_id: None,
                }),
            });
        let backend = TestBackend::new(150, 30);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| render_source_worktree_settlement(frame, frame.area(), &state))
            .unwrap();
        let text: String = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect();
        assert!(text.contains("Recent settlement runs"));
        assert!(text.contains("automatic daemon"));
        assert!(text.contains("settled 3"));
        assert!(text.contains(&run_id.to_string()));
    }

    #[test]
    fn settlement_overlay_renders_narrow_wide_and_scrolled_views() {
        for (width, height, scroll) in [(80, 18, 0), (160, 55, 0), (80, 18, 12)] {
            let backend = TestBackend::new(width, height);
            let mut terminal = Terminal::new(backend).expect("test terminal");
            let state = empty_state(scroll);
            terminal
                .draw(|frame| {
                    let area = frame.area();
                    render_source_worktree_settlement(frame, area, &state);
                })
                .expect("render settlement overlay");
            let text: String = terminal
                .backend()
                .buffer()
                .content
                .iter()
                .map(|cell| cell.symbol())
                .collect();
            assert!(text.contains("Source Worktree Settlement"));
            if scroll == 0 {
                assert!(text.contains("Enter"));
            }
        }
    }

    #[test]
    fn historical_run_on_a_live_bound_refusal_is_not_labeled_receipt_recovery() {
        let identity = "/repo/.git".to_string();
        let audit = SourceWorktreeCohortAuditV1 {
            traversal: None,
            schema_version: SOURCE_WORKTREE_SETTLEMENT_SCHEMA_VERSION,
            policy_version: SOURCE_WORKTREE_SETTLEMENT_POLICY_VERSION,
            repository_identity: identity.clone(),
            canonical_repo_dir: "/repo".into(),
            target_ref: None,
            target_oid: None,
            plan_digest: Sha256Digest::parse(format!("sha256:{}", "a".repeat(64))).unwrap(),
            authorization_phrase: None,
            writes: 0,
            applyable: false,
            counts: SourceWorktreeSettlementCountsV1::default(),
            items: Vec::new(),
            run_id: Some(uuid::Uuid::new_v4()),
            last_run_retained_roots: Vec::new(),
            refusal: Some("source-worktree cohort exceeds bounded maximum of 256 roots".into()),
        }
        .validate_wire()
        .expect("wire-valid live bound refusal");
        let mut state = empty_state(0);
        state.cohorts.push(SourceWorktreeCohortSummaryV1 {
            schema_version: SOURCE_WORKTREE_SETTLEMENT_SCHEMA_VERSION,
            policy_version: SOURCE_WORKTREE_SETTLEMENT_POLICY_VERSION,
            repository_identity: identity,
            canonical_repo_dir: "/repo".into(),
            live_roots: 257,
            terminal_roots: 0,
        });
        state.audit = Some(audit);

        let render = |state: &SourceWorktreeSettlementOverlayState| {
            let backend = TestBackend::new(180, 50);
            let mut terminal = Terminal::new(backend).expect("test terminal");
            terminal
                .draw(|frame| render_source_worktree_settlement(frame, frame.area(), state))
                .expect("render settlement audit label");
            terminal
                .backend()
                .buffer()
                .content
                .iter()
                .map(|cell| cell.symbol())
                .collect::<String>()
        };

        let live_refusal = render(&state);
        assert!(live_refusal.contains("Fresh live audit (writes=0)"));
        assert!(!live_refusal.contains("Receipt recovery audit (writes=0)"));

        state.cohorts[0].live_roots = 0;
        let receipt_recovery = render(&state);
        assert!(receipt_recovery.contains("Receipt recovery audit (writes=0)"));
    }

    #[test]
    fn fresh_live_audit_and_historical_receipt_render_together() {
        let run_id = uuid::Uuid::new_v4();
        let identity = "/repo/.git".to_string();
        let digest = Sha256Digest::parse(format!("sha256:{}", "a".repeat(64))).unwrap();
        let phrase = format!("APPLY {identity} {digest}");
        let session_id = uuid::Uuid::new_v4();
        let custody_id = uuid::Uuid::new_v4();
        let audit = SourceWorktreeCohortAuditV1 {
            traversal: None,
            schema_version: SOURCE_WORKTREE_SETTLEMENT_SCHEMA_VERSION,
            policy_version: SOURCE_WORKTREE_SETTLEMENT_POLICY_VERSION,
            repository_identity: identity.clone(),
            canonical_repo_dir: "/repo".into(),
            target_ref: Some("refs/heads/main".into()),
            target_oid: Some(SourceWorktreeGitOidV1::parse("b".repeat(40)).unwrap()),
            plan_digest: digest.clone(),
            authorization_phrase: Some(phrase.clone()),
            writes: 0,
            applyable: true,
            counts: SourceWorktreeSettlementCountsV1 {
                observed: 1,
                eligible: 1,
                ..Default::default()
            },
            items: vec![SourceWorktreeAuditItemV1 {
                session_id,
                status: "Completed".into(),
                updated_at: "2026-08-24T00:00:02.000000000Z".into(),
                custody_id,
                custody_generation: 1,
                scheduled_dependency_count: 0,
                scheduled_dependency_digest: Sha256Digest::parse(
                    SOURCE_WORKTREE_EMPTY_DEPENDENCY_DIGEST,
                )
                .unwrap(),
                session_path_dependency_count: 0,
                session_path_dependency_digest: Sha256Digest::parse(
                    SOURCE_WORKTREE_EMPTY_SESSION_PATH_DEPENDENCY_DIGEST,
                )
                .unwrap(),
                sandbox_root: "/sandboxes/live".into(),
                source_ref: "refs/heads/rsi/live".into(),
                source_oid: Some(SourceWorktreeGitOidV1::parse("a".repeat(40)).unwrap()),
                clean_state_digest: Some(digest.clone()),
                proof: SourceWorktreeProofV1::IntegratedAncestor,
                evidence_digest: digest.clone(),
                disposition: SourceWorktreeDispositionV1::EligibleIntegratedAncestor,
                diagnostic: None,
            }],
            run_id: Some(run_id),
            last_run_retained_roots: Vec::new(),
            refusal: None,
        }
        .validate_wire()
        .expect("wire-valid fresh live audit");
        let receipt = SourceWorktreeSettlementRunV1 {
            attribution: None,
            schema_version: SOURCE_WORKTREE_SETTLEMENT_SCHEMA_VERSION,
            policy_version: SOURCE_WORKTREE_SETTLEMENT_POLICY_VERSION,
            run_id,
            repository_identity: identity.clone(),
            canonical_repo_dir: "/repo".into(),
            target_ref: "refs/heads/main".into(),
            target_oid: SourceWorktreeGitOidV1::parse("b".repeat(40)).unwrap(),
            plan_digest: digest,
            idempotency_key: "historical".into(),
            state: SourceWorktreeSettlementRunStateV1::Settled,
            counts: SourceWorktreeSettlementCountsV1 {
                observed: 1,
                eligible: 1,
                settled: 1,
                ..Default::default()
            },
            items: vec![SourceWorktreeSettlementItemV1 {
                sequence: 0,
                session_id,
                custody_id,
                source_ref: "refs/heads/rsi/live".into(),
                expected_source_oid: SourceWorktreeGitOidV1::parse("a".repeat(40)).unwrap(),
                phase: SourceWorktreeSettlementPhaseV1::Settled,
                refusal_code: None,
                before_observation: None,
                after_observation: Some("settled".into()),
            }],
            retained_roots: Vec::new(),
            unproven_survivors: vec![
                SourceWorktreeUnprovenSurvivorV1 {
                    sequence: 0,
                    pid: 4242,
                    start_time_micros: 1_700_000_000_000_000,
                    executable: Some("/opt/hardened/bin/claude".into()),
                    session_id: Some(session_id),
                    custody_id: Some(custody_id),
                    sandbox_root: Some("/sandboxes/live".into()),
                    reason: SourceWorktreeUnprovenReasonV1::EnvironmentWithheldByMacos,
                },
                SourceWorktreeUnprovenSurvivorV1 {
                    sequence: 1,
                    pid: 4343,
                    start_time_micros: 1_700_000_000_000_001,
                    executable: None,
                    session_id: None,
                    custody_id: None,
                    sandbox_root: None,
                    reason: SourceWorktreeUnprovenReasonV1::EnvironmentWithheldByMacos,
                },
            ],
            created_at: "2026-08-24T00:00:00.000000000Z".into(),
            updated_at: "2026-08-24T00:00:01.000000000Z".into(),
            finished_at: Some("2026-08-24T00:00:01.000000000Z".into()),
            terminal_error: None,
        }
        .validate_wire()
        .expect("wire-valid historical receipt");
        let state = SourceWorktreeSettlementOverlayState {
            recent_runs: Vec::new(),
            cohorts: vec![SourceWorktreeCohortSummaryV1 {
                schema_version: SOURCE_WORKTREE_SETTLEMENT_SCHEMA_VERSION,
                policy_version: SOURCE_WORKTREE_SETTLEMENT_POLICY_VERSION,
                repository_identity: identity.clone(),
                canonical_repo_dir: "/repo".into(),
                live_roots: 1,
                terminal_roots: 1,
            }],
            selected_index: 0,
            scroll_offset: 0,
            audit: Some(audit),
            receipt: Some(receipt),
            authorization_input: String::new(),
            authorization_active: false,
            idempotency_key: None,
            last_error: None,
        };
        let backend = TestBackend::new(180, 60);
        let mut terminal = Terminal::new(backend).expect("test terminal");
        terminal
            .draw(|frame| {
                let area = frame.area();
                render_source_worktree_settlement(frame, area, &state);
            })
            .expect("render audit and receipt");
        let text: String = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect();
        assert!(text.contains("Fresh live audit (writes=0)"));
        assert!(text.contains(&phrase));
        assert!(text.contains("Durable receipt"));
        assert!(text.contains(&run_id.to_string()));
        // #149: each unproven survivor shows its pid, executable, root (or
        // the cohort repository) and why it was left running.
        assert!(text.contains("Unproven survivors (left running"));
        assert!(text.contains(
            "pid 4242 · /opt/hardened/bin/claude · root /sandboxes/live · environment withheld by macOS"
        ));
        assert!(text.contains(
            "pid 4343 · <executable unknown> · cohort repository /repo · environment withheld by macOS"
        ));
    }

    #[test]
    fn refused_audit_renders_its_refusal_reason_and_the_apply_error() {
        let identity = "/repo/.git".to_string();
        let refusal = "source-worktree cohort exceeds bounded maximum of 256 roots";
        let mut state = empty_state(0);
        state.cohorts.push(SourceWorktreeCohortSummaryV1 {
            schema_version: SOURCE_WORKTREE_SETTLEMENT_SCHEMA_VERSION,
            policy_version: SOURCE_WORKTREE_SETTLEMENT_POLICY_VERSION,
            repository_identity: identity.clone(),
            canonical_repo_dir: "/repo".into(),
            live_roots: 257,
            terminal_roots: 0,
        });
        state.audit = Some(
            SourceWorktreeCohortAuditV1 {
                traversal: None,
                schema_version: SOURCE_WORKTREE_SETTLEMENT_SCHEMA_VERSION,
                policy_version: SOURCE_WORKTREE_SETTLEMENT_POLICY_VERSION,
                repository_identity: identity,
                canonical_repo_dir: "/repo".into(),
                target_ref: None,
                target_oid: None,
                plan_digest: Sha256Digest::parse(format!("sha256:{}", "a".repeat(64))).unwrap(),
                authorization_phrase: None,
                writes: 0,
                applyable: false,
                counts: SourceWorktreeSettlementCountsV1::default(),
                items: Vec::new(),
                run_id: None,
                last_run_retained_roots: Vec::new(),
                refusal: Some(refusal.into()),
            }
            .validate_wire()
            .expect("wire-valid refused audit"),
        );
        state.last_error = Some(format!("Apply refused by audit: {refusal}"));

        let backend = TestBackend::new(200, 50);
        let mut terminal = Terminal::new(backend).expect("test terminal");
        terminal
            .draw(|frame| render_source_worktree_settlement(frame, frame.area(), &state))
            .expect("render refused audit");
        let text: String = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect();
        assert!(text.contains(&format!("cohort refusal: {refusal}")));
        assert!(text.contains(&format!("Apply refused by audit: {refusal}")));
    }
}
