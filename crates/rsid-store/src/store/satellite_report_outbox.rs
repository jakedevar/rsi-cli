//! #1103/#2242: the satellite's process-local report outbox and the delivery
//! state the reporting seat can read.
//!
//! The outbox holds the reports the satellite's manager wrote for the hub
//! until the hub acknowledges them. It is deliberately not durable
//! (informational, at most once), so everything here is a pure in-memory
//! structure: the pull, the acknowledgement and the hub's verdicts move a
//! report through `queued -> fetched -> held | delivered | refused`, and a
//! report nothing happened to for [`SATELLITE_REPORT_UNDELIVERED_AFTER_SECS`]
//! is the typed failure `undelivered`. Time is a parameter, so the clock is
//! never read here.
//!
//! #2453: the outbox itself stays process-local; the durable owed-failure state
//! (a hub verdict, an observed deadline, a queued-before-restart report) lives in
//! the store's `satellite_report_obligations` rows, so nothing here is the source of
//! truth for a notice. [`HubReportOutbox::undelivered_observations`] only names the
//! reports this process sees past the no-pull deadline and why.

use chrono::{DateTime, Duration, SecondsFormat, Utc};
use rsi_common::satellite_dispatch::{
    SATELLITE_REPORT_OUTBOX_MAX, SATELLITE_REPORT_REASON_HUB_NOT_ACKNOWLEDGING,
    SATELLITE_REPORT_REASON_HUB_NOT_PULLING, SATELLITE_REPORT_STATUS_SETTLED_MAX,
    SATELLITE_REPORT_UNDELIVERED_AFTER_SECS, SatelliteReportBacklogV1, SatelliteReportKindCountsV1,
    SatelliteReportKindV1, SatelliteReportOutcomeV1, SatelliteReportStateV1,
    SatelliteReportStatusEntryV1, SatelliteReportStatusV1, SatelliteReportV1,
    SatelliteReportVerdictV1,
};
use std::collections::VecDeque;
use uuid::Uuid;

fn stamp(at: DateTime<Utc>) -> String {
    at.to_rfc3339_opts(SecondsFormat::Nanos, true)
}

#[derive(Debug)]
struct PendingReport {
    report: SatelliteReportV1,
    queued_at: DateTime<Utc>,
    /// First time the hub was handed this report.
    fetched_at: Option<DateTime<Utc>>,
    /// The hub's latest reason for not recording it yet.
    hold: Option<(String, DateTime<Utc>)>,
}

#[derive(Debug)]
struct SettledReport {
    report: SatelliteReportV1,
    queued_at: DateTime<Utc>,
    settled_at: DateTime<Utc>,
    /// `Some(class)` when the hub refused it for good.
    refused: Option<String>,
}

/// How a report failed, as the seat's durable notice names it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ReportFailureState {
    /// The hub could not record it yet and will be asked again.
    Held,
    /// The hub refused it for good.
    Refused,
    /// Nothing recorded it within the deadline.
    Undelivered,
}

impl ReportFailureState {
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Held => "held",
            Self::Refused => "refused",
            Self::Undelivered => "undelivered",
        }
    }
}

/// One failure the seat's manager must learn of (#2453). It carries no report
/// text: the id, kind, typed reason and queue time are the whole obligation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReportFailure {
    pub report_id: Uuid,
    pub kind: SatelliteReportKindV1,
    pub state: ReportFailureState,
    pub reason: String,
    pub queued_at: DateTime<Utc>,
    pub project: Option<Uuid>,
}

/// Reports waiting for the hub, the recent outcomes, and the last hub pull.
#[derive(Debug, Default)]
pub struct HubReportOutbox {
    pending: VecDeque<PendingReport>,
    settled: VecDeque<SettledReport>,
    last_pull: Option<DateTime<Utc>>,
}

impl HubReportOutbox {
    /// Reports the hub has not acknowledged.
    #[must_use]
    pub fn len(&self) -> usize {
        self.pending.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.pending.is_empty()
    }

    const fn deadline() -> Duration {
        Duration::seconds(SATELLITE_REPORT_UNDELIVERED_AFTER_SECS)
    }

    fn hub_recently_pulled(&self, now: DateTime<Utc>) -> bool {
        self.last_pull
            .is_some_and(|pulled| now - pulled <= Self::deadline())
    }

    /// The hub is demonstrably not pulling: a report has waited past the
    /// deadline and no pull has been seen for as long. An empty outbox, or a
    /// satellite that just started, is never unreachable (nothing proves it).
    #[must_use]
    pub fn hub_unreachable(&self, now: DateTime<Utc>) -> bool {
        self.pending
            .front()
            .is_some_and(|oldest| now - oldest.queued_at > Self::deadline())
            && !self.hub_recently_pulled(now)
    }

    /// What is waiting for the hub: depth against the bound, the oldest
    /// pending report and the per-kind counts (#135).
    #[must_use]
    pub fn backlog(&self, now: DateTime<Utc>) -> SatelliteReportBacklogV1 {
        let mut by_kind = SatelliteReportKindCountsV1::default();
        for pending in &self.pending {
            match pending.report.kind {
                SatelliteReportKindV1::Result => by_kind.result += 1,
                SatelliteReportKindV1::Enqueue => by_kind.enqueue += 1,
                SatelliteReportKindV1::DeployReady => by_kind.deploy_ready += 1,
            }
        }
        let oldest = self.pending.front();
        SatelliteReportBacklogV1 {
            depth: self.pending.len(),
            max: SATELLITE_REPORT_OUTBOX_MAX,
            oldest_queued_at: oldest.map(|pending| stamp(pending.queued_at)),
            oldest_age_secs: oldest.map(|pending| (now - pending.queued_at).num_seconds().max(0)),
            by_kind,
        }
    }

    pub fn push(&mut self, report: SatelliteReportV1, now: DateTime<Utc>) {
        self.pending.push_back(PendingReport {
            report,
            queued_at: now,
            fetched_at: None,
            hold: None,
        });
    }

    /// The failure a pending report is in now, if any: the hub's hold, else a
    /// report past the deadline (the status read's own precedence).
    fn pending_failure(
        &self,
        pending: &PendingReport,
        now: DateTime<Utc>,
    ) -> Option<(ReportFailureState, String)> {
        if let Some((reason, _)) = &pending.hold {
            return Some((ReportFailureState::Held, reason.clone()));
        }
        if now - pending.queued_at <= Self::deadline() {
            return None;
        }
        let reason = if self.hub_recently_pulled(now) {
            SATELLITE_REPORT_REASON_HUB_NOT_ACKNOWLEDGING
        } else {
            SATELLITE_REPORT_REASON_HUB_NOT_PULLING
        };
        Some((ReportFailureState::Undelivered, reason.to_string()))
    }

    /// Reports this process sees past the no-pull deadline with no hub hold,
    /// as `(report id, typed reason)`. A hold is the hub's own verdict and a
    /// refusal its acknowledgement; both reach the store through the wire
    /// outcomes, not from here.
    #[must_use]
    pub fn undelivered_observations(&self, now: DateTime<Utc>) -> Vec<(Uuid, String)> {
        self.pending
            .iter()
            .filter_map(|pending| match self.pending_failure(pending, now) {
                Some((ReportFailureState::Undelivered, reason)) => {
                    Some((pending.report.report_id, reason))
                }
                _ => None,
            })
            .collect()
    }

    /// Is this report in the outbox, pending or settled? A durable obligation
    /// that is not was queued by an earlier process (#2453).
    #[must_use]
    pub fn tracks(&self, report_id: Uuid) -> bool {
        self.pending
            .iter()
            .any(|pending| pending.report.report_id == report_id)
            || self
                .settled
                .iter()
                .any(|settled| settled.report.report_id == report_id)
    }

    /// One hub pull: record it, settle what the hub acknowledged (a report the
    /// hub also refused keeps its class), re-read the hub's holds, and hand
    /// over the oldest `page` reports still waiting.
    pub fn pull(
        &mut self,
        now: DateTime<Utc>,
        acked: &[Uuid],
        outcomes: &[SatelliteReportOutcomeV1],
        page: usize,
    ) -> Vec<SatelliteReportV1> {
        self.last_pull = Some(now);
        let mut kept = VecDeque::with_capacity(self.pending.len());
        for pending in self.pending.drain(..) {
            let id = pending.report.report_id;
            if acked.contains(&id) {
                let refused = outcomes
                    .iter()
                    .find(|outcome| {
                        outcome.report_id == id
                            && outcome.verdict == SatelliteReportVerdictV1::Refused
                    })
                    .map(|outcome| outcome.reason.clone());
                self.settled.push_back(SettledReport {
                    report: pending.report,
                    queued_at: pending.queued_at,
                    settled_at: now,
                    refused,
                });
            } else {
                kept.push_back(pending);
            }
        }
        self.pending = kept;
        while self.settled.len() > SATELLITE_REPORT_STATUS_SETTLED_MAX {
            self.settled.pop_front();
        }
        // Holds are recomputed on every pull: the hub repeats a hold for as
        // long as it applies, so a report it stopped mentioning is no longer
        // held.
        for pending in &mut self.pending {
            pending.hold = outcomes
                .iter()
                .find(|outcome| {
                    outcome.report_id == pending.report.report_id
                        && outcome.verdict == SatelliteReportVerdictV1::Held
                })
                .map(|outcome| (outcome.reason.clone(), now));
        }
        self.pending
            .iter_mut()
            .take(page)
            .map(|pending| {
                pending.fetched_at.get_or_insert(now);
                pending.report.clone()
            })
            .collect()
    }

    /// The reporting seat's view, newest first.
    #[must_use]
    pub fn status(&self, now: DateTime<Utc>) -> SatelliteReportStatusV1 {
        let mut entries: Vec<(DateTime<Utc>, SatelliteReportStatusEntryV1)> = Vec::new();
        for pending in &self.pending {
            let (state, reason, updated) = if let Some((reason, at)) = &pending.hold {
                (SatelliteReportStateV1::Held, Some(reason.clone()), *at)
            } else if now - pending.queued_at > Self::deadline() {
                let reason = if self.hub_recently_pulled(now) {
                    SATELLITE_REPORT_REASON_HUB_NOT_ACKNOWLEDGING
                } else {
                    SATELLITE_REPORT_REASON_HUB_NOT_PULLING
                };
                (
                    SatelliteReportStateV1::Undelivered,
                    Some(reason.to_string()),
                    pending.queued_at,
                )
            } else if let Some(fetched) = pending.fetched_at {
                (SatelliteReportStateV1::Fetched, None, fetched)
            } else {
                (SatelliteReportStateV1::Queued, None, pending.queued_at)
            };
            entries.push((
                pending.queued_at,
                SatelliteReportStatusEntryV1 {
                    report_id: pending.report.report_id,
                    kind: pending.report.kind,
                    state,
                    reason,
                    queued_at: stamp(pending.queued_at),
                    updated_at: stamp(updated),
                },
            ));
        }
        for settled in &self.settled {
            let (state, reason) = settled
                .refused
                .as_ref()
                .map_or((SatelliteReportStateV1::Delivered, None), |class| {
                    (SatelliteReportStateV1::Refused, Some(class.clone()))
                });
            entries.push((
                settled.queued_at,
                SatelliteReportStatusEntryV1 {
                    report_id: settled.report.report_id,
                    kind: settled.report.kind,
                    state,
                    reason,
                    queued_at: stamp(settled.queued_at),
                    updated_at: stamp(settled.settled_at),
                },
            ));
        }
        entries.sort_by_key(|entry| std::cmp::Reverse(entry.0));
        SatelliteReportStatusV1 {
            last_hub_pull_at: self.last_pull.map(stamp),
            hub_pulling: !self.hub_unreachable(now),
            backlog: self.backlog(now),
            reports: entries.into_iter().map(|(_, entry)| entry).collect(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rsi_common::satellite_dispatch::SatelliteReportKindV1;

    fn report(text: &str) -> SatelliteReportV1 {
        SatelliteReportV1 {
            binaries_dir: None,
            report_id: Uuid::new_v4(),
            kind: SatelliteReportKindV1::Enqueue,
            text: text.into(),
        }
    }

    fn at(secs: i64) -> DateTime<Utc> {
        DateTime::<Utc>::from_timestamp(1_800_000_000 + secs, 0).unwrap()
    }

    fn states(outbox: &HubReportOutbox, now: DateTime<Utc>) -> Vec<SatelliteReportStateV1> {
        outbox
            .status(now)
            .reports
            .iter()
            .map(|entry| entry.state)
            .collect()
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-01"))]
    #[test]
    fn a_report_moves_from_queued_to_fetched_to_delivered_with_the_hubs_ack() {
        let mut outbox = HubReportOutbox::default();
        let queued = report("ENQUEUE a");
        let id = queued.report_id;
        outbox.push(queued, at(0));
        assert_eq!(states(&outbox, at(1)), [SatelliteReportStateV1::Queued]);
        assert_eq!(outbox.pull(at(15), &[], &[], 8).len(), 1);
        assert_eq!(states(&outbox, at(16)), [SatelliteReportStateV1::Fetched]);
        assert!(outbox.pull(at(30), &[id], &[], 8).is_empty());
        let status = outbox.status(at(31));
        assert_eq!(status.reports.len(), 1);
        assert_eq!(status.reports[0].state, SatelliteReportStateV1::Delivered);
        assert_eq!(status.reports[0].reason, None);
        assert!(status.hub_pulling);
        assert!(status.last_hub_pull_at.is_some());
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-01"))]
    #[test]
    fn the_backlog_names_depth_the_oldest_report_and_the_kinds() {
        let mut outbox = HubReportOutbox::default();
        let empty = outbox.backlog(at(5));
        assert_eq!(
            (empty.depth, empty.oldest_queued_at, empty.oldest_age_secs),
            (0, None, None)
        );
        assert_eq!(empty.max, SATELLITE_REPORT_OUTBOX_MAX);
        let mut deploy = report("DEPLOY-READY x");
        deploy.kind = SatelliteReportKindV1::DeployReady;
        outbox.push(deploy, at(10));
        outbox.push(report("ENQUEUE y"), at(20));
        let backlog = outbox.backlog(at(70));
        assert_eq!(backlog.depth, 2);
        assert_eq!(backlog.oldest_age_secs, Some(60));
        assert_eq!(backlog.oldest_queued_at, Some(stamp(at(10))));
        assert_eq!(
            (
                backlog.by_kind.result,
                backlog.by_kind.enqueue,
                backlog.by_kind.deploy_ready
            ),
            (0, 1, 1)
        );
        assert_eq!(outbox.status(at(70)).backlog, backlog);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-01"))]
    #[test]
    fn the_hubs_verdicts_surface_as_held_and_refused_with_typed_reasons() {
        let mut outbox = HubReportOutbox::default();
        let (held, refused) = (report("ENQUEUE held"), report("RESULT refused"));
        let (held_id, refused_id) = (held.report_id, refused.report_id);
        outbox.push(held, at(0));
        outbox.push(refused, at(1));
        outbox.pull(at(15), &[], &[], 8);
        let verdicts = [
            SatelliteReportOutcomeV1 {
                report_id: held_id,
                verdict: SatelliteReportVerdictV1::Held,
                reason: "manager_unavailable".into(),
            },
            SatelliteReportOutcomeV1 {
                report_id: refused_id,
                verdict: SatelliteReportVerdictV1::Refused,
                reason: "peer_not_authorized".into(),
            },
        ];
        outbox.pull(at(30), &[refused_id], &verdicts, 8);
        let status = outbox.status(at(31));
        let held_entry = status
            .reports
            .iter()
            .find(|entry| entry.report_id == held_id)
            .unwrap();
        assert_eq!(held_entry.state, SatelliteReportStateV1::Held);
        assert_eq!(held_entry.reason.as_deref(), Some("manager_unavailable"));
        let refused_entry = status
            .reports
            .iter()
            .find(|entry| entry.report_id == refused_id)
            .unwrap();
        assert_eq!(refused_entry.state, SatelliteReportStateV1::Refused);
        assert_eq!(refused_entry.reason.as_deref(), Some("peer_not_authorized"));
        // The refused one is gone from the queue; the held one is re-offered.
        assert_eq!(outbox.len(), 1);
        // A hold the hub stopped repeating is no longer a hold.
        outbox.pull(at(45), &[], &[], 8);
        assert_eq!(
            outbox.status(at(46)).reports[1].state,
            SatelliteReportStateV1::Fetched
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-01"))]
    #[test]
    fn a_report_the_hub_never_pulls_becomes_the_typed_failure_hub_not_pulling() {
        let mut outbox = HubReportOutbox::default();
        assert!(
            !outbox.hub_unreachable(at(1_000)),
            "an empty outbox proves nothing"
        );
        outbox.push(report("ENQUEUE stuck"), at(0));
        let early = at(SATELLITE_REPORT_UNDELIVERED_AFTER_SECS);
        assert!(!outbox.hub_unreachable(early));
        assert_eq!(states(&outbox, early), [SatelliteReportStateV1::Queued]);
        let late = at(SATELLITE_REPORT_UNDELIVERED_AFTER_SECS + 1);
        assert!(outbox.hub_unreachable(late));
        let status = outbox.status(late);
        assert!(!status.hub_pulling);
        assert_eq!(status.reports[0].state, SatelliteReportStateV1::Undelivered);
        assert_eq!(
            status.reports[0].reason.as_deref(),
            Some(SATELLITE_REPORT_REASON_HUB_NOT_PULLING)
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-01"))]
    #[test]
    fn a_pulling_hub_that_never_acknowledges_is_hub_not_acknowledging_not_unreachable() {
        let mut outbox = HubReportOutbox::default();
        outbox.push(report("RESULT unacked"), at(0));
        let late = at(SATELLITE_REPORT_UNDELIVERED_AFTER_SECS + 10);
        outbox.pull(late, &[], &[], 8);
        assert!(!outbox.hub_unreachable(late), "the hub is pulling");
        let status = outbox.status(late);
        assert!(status.hub_pulling);
        assert_eq!(status.reports[0].state, SatelliteReportStateV1::Undelivered);
        assert_eq!(
            status.reports[0].reason.as_deref(),
            Some(SATELLITE_REPORT_REASON_HUB_NOT_ACKNOWLEDGING)
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-01"))]
    #[test]
    fn the_settled_history_is_bounded_and_listed_newest_first() {
        let mut outbox = HubReportOutbox::default();
        let mut ids = Vec::new();
        for index in 0..(SATELLITE_REPORT_STATUS_SETTLED_MAX + 5) {
            let next = report(&format!("RESULT {index}"));
            ids.push(next.report_id);
            outbox.push(next, at(i64::try_from(index).unwrap()));
        }
        outbox.pull(at(100), &ids, &[], 8);
        let status = outbox.status(at(101));
        assert_eq!(status.reports.len(), SATELLITE_REPORT_STATUS_SETTLED_MAX);
        assert_eq!(status.reports[0].report_id, *ids.last().unwrap());
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-01"))]
    #[test]
    fn a_report_past_the_deadline_is_observed_undelivered_with_the_status_reason() {
        let mut outbox = HubReportOutbox::default();
        let stuck = report("RESULT stuck");
        let id = stuck.report_id;
        outbox.push(stuck, at(0));
        assert!(
            outbox
                .undelivered_observations(at(SATELLITE_REPORT_UNDELIVERED_AFTER_SECS))
                .is_empty()
        );
        let late = at(SATELLITE_REPORT_UNDELIVERED_AFTER_SECS + 1);
        assert_eq!(
            outbox.undelivered_observations(late),
            [(id, SATELLITE_REPORT_REASON_HUB_NOT_PULLING.to_string())]
        );
        // A hub hold is the hub's verdict, not an undelivered observation.
        let outcomes = [SatelliteReportOutcomeV1 {
            report_id: id,
            verdict: SatelliteReportVerdictV1::Held,
            reason: "manager_unavailable".into(),
        }];
        outbox.pull(late, &[], &outcomes, 8);
        assert!(outbox.undelivered_observations(late).is_empty());
    }
}
