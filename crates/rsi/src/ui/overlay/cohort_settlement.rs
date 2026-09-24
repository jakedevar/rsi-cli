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
        muted(" audit  "),
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
        lines.push(Line::from(Span::styled(
            format!("> {}█", state.authorization_input),
            Style::default().fg(theme::warning_status()),
        )));
        lines.push(Line::from(Span::styled(
            "Enter submit · Esc cancel",
            Style::default().fg(theme::subtext0()),
        )));
    }

    if let Some(receipt) = &state.receipt {
        lines.push(Line::default());
        lines.push(section("Durable receipt"));
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
        SourceWorktreeSettlementRunV1,
    };
    use rsi_common::types::Sha256Digest;

    fn empty_state(scroll_offset: usize) -> SourceWorktreeSettlementOverlayState {
        SourceWorktreeSettlementOverlayState {
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
            refusal: None,
        }
        .validate_wire()
        .expect("wire-valid fresh live audit");
        let receipt = SourceWorktreeSettlementRunV1 {
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
            created_at: "2026-08-24T00:00:00.000000000Z".into(),
            updated_at: "2026-08-24T00:00:01.000000000Z".into(),
            finished_at: Some("2026-08-24T00:00:01.000000000Z".into()),
            terminal_error: None,
        }
        .validate_wire()
        .expect("wire-valid historical receipt");
        let state = SourceWorktreeSettlementOverlayState {
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
    }
}
