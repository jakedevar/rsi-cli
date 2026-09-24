//! Review contributor provenance from daemon rows. This module never inspects Git.

use super::*;
use std::collections::{BTreeSet, VecDeque};

const MAX_LINEAGE: usize = 64;
const MAX_CONTRIBUTORS: usize = 256;
const MAX_SPAWN_DEPTH: usize = 8;

#[derive(Debug, Default)]
pub(super) struct ContributorSet {
    pub sessions: BTreeSet<Uuid>,
    pub families: BTreeSet<String>,
    lineage_families: BTreeSet<String>,
}

fn unbounded() -> crate::error::DaemonError {
    refused("manager_review_contributors_unbounded")
}

fn family_name(family: ReviewModelFamily) -> &'static str {
    match family {
        ReviewModelFamily::Anthropic => "anthropic",
        ReviewModelFamily::OpenAI => "openai",
        ReviewModelFamily::Google => "google",
        ReviewModelFamily::ZAi => "zai",
        ReviewModelFamily::DeepSeek => "deepseek",
        ReviewModelFamily::Qwen => "qwen",
        ReviewModelFamily::Meta => "meta",
        ReviewModelFamily::Mistral => "mistral",
        ReviewModelFamily::Unknown => "unknown",
    }
}

impl Store {
    /// Add the recorded custody cohort of one session. A shared custody root
    /// is deliberately conservative: all its members could have touched the
    /// sealed bytes, even if an old rotation edge is no longer readable.
    fn review_lineage(&self, id: Uuid) -> Result<BTreeSet<Uuid>> {
        let custody: Option<Option<String>> = self
            .conn
            .query_row(
                "SELECT sandbox_custody_id FROM sessions WHERE id=?1",
                [id.to_string()],
                |row| row.get(0),
            )
            .optional()?;
        let Some(custody) = custody else {
            return Err(unbounded());
        };
        let Some(custody) = custody else {
            return Ok(BTreeSet::from([id]));
        };
        let mut statement = self
            .conn
            .prepare("SELECT id FROM sessions WHERE sandbox_custody_id=?1 LIMIT ?2")?;
        let rows = statement
            .query_map(params![custody, (MAX_LINEAGE + 1) as i64], |row| {
                row.get::<_, String>(0)
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        if rows.len() > MAX_LINEAGE {
            return Err(unbounded());
        }
        let members = rows
            .into_iter()
            .map(|value| Uuid::parse_str(&value).map_err(|_| unbounded()))
            .collect::<Result<BTreeSet<_>>>()?;
        if !members.contains(&id) {
            return Err(unbounded());
        }
        Ok(members)
    }

    /// `root_rowid` orders assignment rows in their own table. Spawn rows have
    /// no cross-table order witness, so include every recorded spawn edge.
    /// This covers clock reversal and legacy requests without stored Σ.
    pub(super) fn review_contributors(
        &self,
        project: Uuid,
        epic: Uuid,
        work_key: &str,
        author: Uuid,
        root_rowid: Option<i64>,
        stored_ids: &[Uuid],
        stored_families: &[String],
    ) -> Result<ContributorSet> {
        let mut set = ContributorSet::default();
        let lineage = self.review_lineage(author)?;
        for member in &lineage {
            let family = self
                .recorded_review_family(*member)
                .map_err(|_| unbounded())?;
            set.lineage_families.insert(family_name(family).into());
        }
        let mut queue = VecDeque::new();
        for member in lineage {
            queue.push_back((member, 0));
        }

        let mut prior = self.conn.prepare(
            "SELECT reviewer_session_id FROM manager_review_assignments
             WHERE project_id=?1 AND epic_id=?2 AND work_key=?3
               AND (?4 IS NULL OR rowid<?4) AND reviewer_session_id IS NOT NULL
             LIMIT ?5",
        )?;
        let reviewers = prior
            .query_map(
                params![
                    project.to_string(),
                    epic.to_string(),
                    work_key,
                    root_rowid,
                    (MAX_CONTRIBUTORS + 1) as i64
                ],
                |row| row.get::<_, String>(0),
            )?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        if reviewers.len() > MAX_CONTRIBUTORS {
            return Err(unbounded());
        }
        for reviewer in reviewers {
            let reviewer = Uuid::parse_str(&reviewer).map_err(|_| unbounded())?;
            for member in self.review_lineage(reviewer)? {
                queue.push_back((member, 0));
            }
        }
        while let Some((id, depth)) = queue.pop_front() {
            if !set.sessions.insert(id) {
                continue;
            }
            if set.sessions.len() > MAX_CONTRIBUTORS {
                return Err(unbounded());
            }
            let family = self.recorded_review_family(id).map_err(|_| unbounded())?;
            set.families.insert(family_name(family).into());
            let mut spawn = self.conn.prepare(
                "SELECT child_session_id FROM agent_spawn_requests WHERE owner_session_id=?1 LIMIT ?2",
            )?;
            let children = spawn
                .query_map(
                    params![id.to_string(), (MAX_CONTRIBUTORS + 1) as i64],
                    |row| row.get::<_, String>(0),
                )?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            if children.len() > MAX_CONTRIBUTORS
                || (depth == MAX_SPAWN_DEPTH && !children.is_empty())
            {
                return Err(unbounded());
            }
            for child in children {
                let child = Uuid::parse_str(&child).map_err(|_| unbounded())?;
                for member in self.review_lineage(child)? {
                    queue.push_back((member, depth + 1));
                }
            }
        }
        for id in stored_ids {
            set.sessions.insert(*id);
            if set.sessions.len() > MAX_CONTRIBUTORS {
                return Err(unbounded());
            }
            let family = self.recorded_review_family(*id).map_err(|_| unbounded())?;
            set.families.insert(family_name(family).into());
        }
        if stored_families.iter().any(|family| {
            !matches!(
                family.as_str(),
                "anthropic"
                    | "openai"
                    | "google"
                    | "zai"
                    | "deepseek"
                    | "qwen"
                    | "meta"
                    | "mistral"
                    | "unknown"
            )
        }) {
            return Err(unbounded());
        }
        set.families.extend(stored_families.iter().cloned());
        if set.families.contains("unknown") || set.lineage_families.contains("unknown") {
            return Err(refused("manager_review_family_conflict"));
        }
        Ok(set)
    }

    pub(super) fn review_contributors_for_assignment(
        &self,
        assignment: &ReviewAssignment,
    ) -> Result<ContributorSet> {
        let mut root = assignment.clone();
        let mut stored_ids = root.request.contributor_session_ids.clone();
        let mut stored_families = root.request.contributor_families.clone();
        for _ in 0..=MAX_INFRA_RELAUNCHES {
            let Some(id) = root.request.infra_retry_of else {
                break;
            };
            let prior = self
                .manager_review_assignment(id)
                .map_err(|_| unbounded())?;
            if prior.project_id != root.project_id
                || prior.epic_id != root.epic_id
                || prior.work_key != root.work_key
                || prior.spec_revision != root.spec_revision
                || prior.author_session_id != root.author_session_id
                || prior.source_sha != root.source_sha
                || prior.assignment_id == root.assignment_id
            {
                return Err(unbounded());
            }
            let before: i64 = self
                .conn
                .query_row(
                    "SELECT rowid FROM manager_review_assignments WHERE assignment_id=?1",
                    [prior.assignment_id.to_string()],
                    |row| row.get(0),
                )
                .optional()?
                .ok_or_else(unbounded)?;
            let after: i64 = self
                .conn
                .query_row(
                    "SELECT rowid FROM manager_review_assignments WHERE assignment_id=?1",
                    [root.assignment_id.to_string()],
                    |row| row.get(0),
                )
                .optional()?
                .ok_or_else(unbounded)?;
            if before >= after {
                return Err(unbounded());
            }
            stored_ids.extend(prior.request.contributor_session_ids.iter().copied());
            stored_families.extend(prior.request.contributor_families.iter().cloned());
            root = prior;
        }
        if root.request.infra_retry_of.is_some() {
            return Err(unbounded());
        }
        let rowid: i64 = self
            .conn
            .query_row(
                "SELECT rowid FROM manager_review_assignments WHERE assignment_id=?1",
                [root.assignment_id.to_string()],
                |row| row.get(0),
            )
            .optional()?
            .ok_or_else(unbounded)?;
        self.review_contributors(
            assignment.project_id,
            assignment.epic_id,
            &assignment.work_key,
            assignment.author_session_id,
            Some(rowid),
            &stored_ids,
            &stored_families,
        )
    }

    /// A decision is usable only when its current version was written by the
    /// manager lineage. A lead's identically named decision is ignored.
    pub(super) fn review_family_override(
        &self,
        config: &HarnessManagerConfigV1,
        work_key: &str,
        revision: i64,
        requested: Option<&str>,
    ) -> Result<Option<String>> {
        let mut best: Option<(i64, String)> = None;
        for family in [
            "anthropic",
            "openai",
            "google",
            "zai",
            "deepseek",
            "qwen",
            "meta",
            "mistral",
        ] {
            let key = format!("review-family-override:{revision}:{family}");
            if requested.is_some_and(|value| value != key) {
                continue;
            }
            let row: Option<(i64, String, Option<String>, i64)> = self
                .conn
                .query_row(
                    "SELECT r.row_version,r.payload_json,e.actor_session_id,e.sequence
                 FROM harness_manager_v2_records r
                 JOIN harness_manager_v2_events e ON e.project_id=r.project_id
                   AND e.manager_session_id=r.manager_session_id AND e.scope_version=r.scope_version
                   AND e.kind=r.kind AND e.record_key=r.record_key AND e.row_version=r.row_version
                 WHERE r.project_id=?1 AND r.manager_session_id=?2 AND r.scope_version=?3
                   AND r.kind='decision' AND r.record_key=?4 AND r.archived=0
                 ORDER BY e.sequence DESC LIMIT 1",
                    params![
                        config.project_id.to_string(),
                        config.manager_session_id.to_string(),
                        config.row_version,
                        key
                    ],
                    |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
                )
                .optional()?;
            let Some((_, payload, Some(actor), sequence)) = row else {
                continue;
            };
            let decision: super::super::manager_ledger::DecisionRecord =
                serde_json::from_str(&payload)?;
            if decision.work_key.as_deref() != Some(work_key) {
                continue;
            }
            let Ok(actor) = Uuid::parse_str(&actor) else {
                continue;
            };
            if self.manager_lineage_tip(actor).ok() != config.current_session_id {
                continue;
            }
            if best.as_ref().is_none_or(|(old, _)| sequence > *old) {
                best = Some((sequence, key));
            }
        }
        Ok(best.map(|(_, key)| key))
    }

    pub(super) fn require_review_contributor_family(
        &self,
        project: Uuid,
        work_key: &str,
        revision: i64,
        set: &ContributorSet,
        override_key: Option<&str>,
        reviewer: ReviewModelFamily,
    ) -> Result<()> {
        if reviewer == ReviewModelFamily::Unknown {
            return Err(refused("manager_review_family_conflict"));
        }
        let config = self.get_harness_manager(project)?.ok_or_else(unbounded)?;
        let verified = override_key
            .map(|key| self.review_family_override(&config, work_key, revision, Some(key)))
            .transpose()?
            .flatten();
        let override_family = verified.as_deref().and_then(|key| key.rsplit(':').next());
        let mut barred = set.families.clone();
        if let Some(family) = override_family {
            barred.remove(family);
        }
        barred.extend(set.lineage_families.iter().cloned());
        let policy = self
            .get_harness_manager_policy(project)?
            .ok_or_else(unbounded)?;
        if !policy.policy.allowed_launches.is_empty()
            && policy.policy.allowed_launches.iter().all(|choice| {
                let family = review_model_family(choice.provider, Some(&choice.model));
                family == ReviewModelFamily::Unknown || barred.contains(family_name(family))
            })
        {
            return Err(refused("manager_review_family_exhausted"));
        }
        if barred.contains(family_name(reviewer)) {
            return Err(refused("manager_review_family_conflict"));
        }
        Ok(())
    }

    pub(super) fn review_receipt_contributor_gate(
        &self,
        assignment: &ReviewAssignment,
        caller: Uuid,
    ) -> Result<()> {
        let set = self.review_contributors_for_assignment(assignment)?;
        self.require_review_contributor_family(
            assignment.project_id,
            &assignment.work_key,
            assignment.spec_revision,
            &set,
            assignment.request.family_override_key.as_deref(),
            self.recorded_review_family(caller)
                .map_err(|_| unbounded())?,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::agent_verbs::tests::test_session;
    use rsi_common::types::{Project, SessionProvider};
    use std::path::PathBuf;

    struct Fixture {
        store: Store,
        project: Uuid,
        manager: Uuid,
        epic: Uuid,
        author: Uuid,
    }

    fn fixture() -> Fixture {
        let store = Store::open_in_memory().unwrap();
        let project = Uuid::new_v4();
        store
            .insert_project(&Project {
                id: project,
                name: "Contributors".into(),
                path: None,
                description: None,
                color: Project::DEFAULT_COLOR.into(),
                context_files: None,
                created_at: Utc::now(),
                updated_at: Utc::now(),
            })
            .unwrap();
        let mut manager = test_session(
            Uuid::new_v4(),
            PathBuf::from("/var/tmp/review-contributors"),
        );
        manager.project_id = Some(project);
        manager.model = Some("gpt-6-sol".into());
        store.insert_session(&manager).unwrap();
        let mut group = manager.clone();
        group.id = Uuid::new_v4();
        group.session_kind = SessionKind::Group;
        store.insert_session(&group).unwrap();
        let mut epic = manager.clone();
        epic.id = Uuid::new_v4();
        epic.session_kind = SessionKind::Epic;
        epic.parent_id = Some(group.id);
        store.insert_session(&epic).unwrap();
        let mut author = manager.clone();
        author.id = Uuid::new_v4();
        author.session_kind = SessionKind::Feature;
        author.parent_id = Some(epic.id);
        store.insert_session(&author).unwrap();
        store
            .configure_harness_manager(
                &rsi_common::harness_manager::ConfigureHarnessManagerRequestV1 {
                    group_ids: vec![],
                    project_id: project,
                    session_id: manager.id,
                    epic_ids: Some(vec![epic.id]),
                    expected_row_version: 0,
                },
            )
            .unwrap();
        store
            .configure_harness_manager_policy(&ConfigureHarnessManagerPolicyRequestV2 {
                project_id: project,
                expected_scope_version: 1,
                expected_policy_version: 0,
                idempotency_key: "contributors-policy".into(),
                policy: ManagerPolicyV2 {
                    allowed_launches: vec![],
                    ..Default::default()
                },
            })
            .unwrap();
        Fixture {
            store,
            project,
            manager: manager.id,
            epic: epic.id,
            author: author.id,
        }
    }

    fn session(f: &Fixture, model: &str) -> Uuid {
        let mut value = f.store.get_session(f.author).unwrap().unwrap();
        value.id = Uuid::new_v4();
        value.model = Some(model.into());
        value.continued_from = None;
        f.store.insert_session(&value).unwrap();
        value.id
    }

    fn spawn(f: &Fixture, owner: Uuid, child: Uuid, stamp: &str) {
        let id = Uuid::new_v4();
        let digest = fingerprint(&json!({"spawn":id})).unwrap();
        let ordinal: i64 = f
            .store
            .conn
            .query_row(
                "SELECT count(*)+1 FROM agent_spawn_requests WHERE epic_id=?1",
                [f.epic.to_string()],
                |row| row.get(0),
            )
            .unwrap();
        f.store.conn.execute(
            "INSERT INTO agent_spawn_requests(spawn_request_id,owner_session_id,idempotency_digest,
             request_fingerprint,request_json,child_session_id,epic_id,kind,state,reserved_at,updated_at,epic_spawn_ordinal)
             VALUES(?1,?2,?3,?3,'{}',?4,?5,'Research','reserved',?6,?6,?7)",
            params![id.to_string(), owner.to_string(), digest, child.to_string(), f.epic.to_string(), stamp, ordinal],
        ).unwrap();
    }

    fn assignment(f: &Fixture, reviewer: Option<Uuid>, legacy: bool, created: &str) -> Uuid {
        assignment_with_stored(f, reviewer, legacy, created, &[])
    }

    fn assignment_with_stored(
        f: &Fixture,
        reviewer: Option<Uuid>,
        legacy: bool,
        created: &str,
        stored: &[Uuid],
    ) -> Uuid {
        let id = Uuid::new_v4();
        let ordinal: i64 = f
            .store
            .conn
            .query_row(
                "SELECT count(*)+1 FROM manager_review_assignments WHERE work_key='work'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        let request = ReviewAllocationRequest {
            requester_session_id: f.manager,
            fence: ManagerFenceV2 {
                scope_version: 1,
                policy_version: 1,
            },
            query: "review".into(),
            launch: ManagerLaunchChoiceV2 {
                provider: SessionProvider::Claude,
                model: "claude-sonnet-5".into(),
                effort: None,
            },
            infra_retry_of: None,
            contributor_session_ids: stored.to_vec(),
            contributor_families: stored
                .iter()
                .map(|id| family_name(f.store.recorded_review_family(*id).unwrap()).to_string())
                .collect(),
            family_override_key: None,
        };
        let mut value = serde_json::to_value(request).unwrap();
        if legacy {
            value
                .as_object_mut()
                .unwrap()
                .remove("contributor_session_ids");
            value
                .as_object_mut()
                .unwrap()
                .remove("contributor_families");
        }
        let state = if reviewer.is_some() {
            "failed"
        } else {
            "reserved"
        };
        f.store
            .conn
            .execute(
                "INSERT INTO manager_review_assignments(assignment_id,project_id,epic_id,
             manager_session_id,scope_version,work_key,spec_revision,author_session_id,
             source_sha,reviewer_session_id,state,row_version,request_json,request_fingerprint,
             failure_code,created_at,updated_at,terminal_at)
             VALUES(?1,?2,?3,?4,1,'work',1,?5,?6,?7,?8,1,?9,?10,?11,?12,?12,?13)",
                params![
                    id.to_string(),
                    f.project.to_string(),
                    f.epic.to_string(),
                    f.manager.to_string(),
                    f.author.to_string(),
                    format!("{ordinal:040x}"),
                    reviewer.map(|id| id.to_string()),
                    state,
                    value.to_string(),
                    fingerprint(&value).unwrap(),
                    reviewer.map(|_| "failed"),
                    created,
                    reviewer.map(|_| created)
                ],
            )
            .unwrap();
        id
    }

    fn contributors(f: &Fixture) -> ContributorSet {
        f.store
            .review_contributors(f.project, f.epic, "work", f.author, None, &[], &[])
            .unwrap()
    }

    fn check(f: &Fixture, set: &ContributorSet, family: ReviewModelFamily) -> Result<()> {
        f.store
            .require_review_contributor_family(f.project, "work", 1, set, None, family)
    }

    fn override_decision(f: &Fixture, actor: Uuid, family: &str) -> String {
        let key = format!("review-family-override:1:{family}");
        let payload = json!({"key":key,"epic_id":f.epic,"question":"Override family",
            "request_id":null,"work_key":"work","target_digest":"test",
            "target_row_version":null,"request_row_version":null,"status":"pending",
            "answer":null,"delivery":null});
        let stamp = now();
        f.store.conn.execute(
            "INSERT INTO harness_manager_v2_records(project_id,manager_session_id,scope_version,
             kind,record_key,epic_id,row_version,payload_json,created_at,updated_at)
             VALUES(?1,?2,1,'decision',?3,?4,1,?5,?6,?6)",
            params![f.project.to_string(), f.manager.to_string(), key,
                f.epic.to_string(), payload.to_string(), stamp],
        ).unwrap();
        f.store
            .conn
            .execute(
                "INSERT INTO harness_manager_v2_events(project_id,manager_session_id,scope_version,
             actor_session_id,kind,record_key,row_version,payload_json,created_at)
             VALUES(?1,?2,1,?3,'decision',?4,1,?5,?6)",
                params![
                    f.project.to_string(),
                    f.manager.to_string(),
                    actor.to_string(),
                    key,
                    payload.to_string(),
                    stamp
                ],
            )
            .unwrap();
        key
    }

    #[test]
    fn copied_then_cleared_child_file_keeps_the_child_family() {
        let f = fixture();
        let child = session(&f, "claude-sonnet-5");
        spawn(&f, f.author, child, &now());
        let set = contributors(&f);
        assert!(set.sessions.contains(&child));
        assert!(
            check(&f, &set, ReviewModelFamily::Anthropic)
                .unwrap_err()
                .to_string()
                .contains("family_conflict")
        );
    }

    #[test]
    fn prior_reviewer_family_stays_even_after_an_accepted_verdict_or_cherry_picked_fix() {
        let f = fixture();
        let reviewer = session(&f, "claude-sonnet-5");
        assignment(&f, Some(reviewer), true, &now());
        let set = contributors(&f);
        assert!(set.sessions.contains(&reviewer));
        assert!(check(&f, &set, ReviewModelFamily::Anthropic).is_err());
    }

    #[test]
    fn prior_reviewers_follow_assignment_rowid_when_timestamps_reverse() {
        let f = fixture();
        let prior = session(&f, "claude-sonnet-5");
        let later = session(&f, "z-ai/glm-5.3-flashx");
        let prior_id = assignment(&f, Some(prior), true, "2099-01-01T00:00:00.000000000Z");
        let root_id = assignment(&f, None, true, "2000-01-01T00:00:00.000000000Z");
        let later_id = assignment(&f, Some(later), true, "1900-01-01T00:00:00.000000000Z");
        let rowid = |id: Uuid| -> i64 {
            f.store
                .conn
                .query_row(
                    "SELECT rowid FROM manager_review_assignments WHERE assignment_id=?1",
                    [id.to_string()],
                    |row| row.get(0),
                )
                .unwrap()
        };
        assert!(rowid(prior_id) < rowid(root_id));
        assert!(rowid(root_id) < rowid(later_id));

        let root = f.store.manager_review_assignment(root_id).unwrap();
        let set = f.store.review_contributors_for_assignment(&root).unwrap();
        assert!(set.sessions.contains(&prior));
        assert!(
            f.store
                .review_receipt_contributor_gate(&root, prior)
                .unwrap_err()
                .to_string()
                .contains("family_conflict")
        );
        assert!(check(&f, &set, ReviewModelFamily::ZAi).is_ok());
    }

    #[test]
    fn merge_squash_cherry_pick_and_rebase_merge_all_keep_the_spawned_family() {
        let f = fixture();
        let child = session(&f, "claude-sonnet-5");
        spawn(&f, f.author, child, &now());
        for _history_shape in ["merge", "squash", "cherry-pick", "rebase-merge"] {
            assert!(check(&f, &contributors(&f), ReviewModelFamily::Anthropic).is_err());
        }
    }

    #[test]
    fn exhausted_allowed_launches_refuse_family_exhausted_and_empty_allowed_launches_is_unrestricted()
     {
        let f = fixture();
        let set = contributors(&f);
        assert!(check(&f, &set, ReviewModelFamily::Anthropic).is_ok());
        let policy = f
            .store
            .get_harness_manager_policy(f.project)
            .unwrap()
            .unwrap();
        let mut next = policy.policy;
        next.allowed_launches = vec![ManagerLaunchChoiceV2 {
            provider: SessionProvider::Codex,
            model: "gpt-6-sol".into(),
            effort: None,
        }];
        f.store
            .configure_harness_manager_policy(&ConfigureHarnessManagerPolicyRequestV2 {
                project_id: f.project,
                expected_scope_version: 1,
                expected_policy_version: 1,
                idempotency_key: "finite".into(),
                policy: next,
            })
            .unwrap();
        assert!(
            check(&f, &set, ReviewModelFamily::Anthropic)
                .unwrap_err()
                .to_string()
                .contains("family_exhausted")
        );
    }

    #[test]
    fn contributor_closure_over_bounds_refuses_unbounded() {
        let f = fixture();
        for _ in 0..=MAX_CONTRIBUTORS {
            let child = session(&f, "claude-sonnet-5");
            spawn(&f, f.author, child, &now());
        }
        assert!(
            f.store
                .review_contributors(f.project, f.epic, "work", f.author, None, &[], &[])
                .unwrap_err()
                .to_string()
                .contains("contributors_unbounded")
        );
    }

    #[test]
    fn receipt_rechecks_the_reviewer_against_stored_contributors() {
        let f = fixture();
        let child = session(&f, "claude-sonnet-5");
        let root = assignment_with_stored(&f, None, false, &now(), &[child]);
        let assignment = f.store.manager_review_assignment(root).unwrap();
        assert!(
            f.store
                .review_receipt_contributor_gate(&assignment, child)
                .unwrap_err()
                .to_string()
                .contains("family_conflict")
        );
    }

    #[test]
    fn manager_override_admits_a_family_for_one_revision_but_never_the_lineage_family_and_lead_key_is_ignored()
     {
        let f = fixture();
        let reviewer = session(&f, "claude-sonnet-5");
        assignment(&f, Some(reviewer), true, &now());
        let set = contributors(&f);
        let lead = session(&f, "gpt-6-luna");
        let lead_key = override_decision(&f, lead, "anthropic");
        assert!(
            f.store
                .review_family_override(
                    &f.store.get_harness_manager(f.project).unwrap().unwrap(),
                    "work",
                    1,
                    Some(&lead_key)
                )
                .unwrap()
                .is_none()
        );
        assert!(
            f.store
                .require_review_contributor_family(
                    f.project,
                    "work",
                    1,
                    &set,
                    Some(&lead_key),
                    ReviewModelFamily::Anthropic
                )
                .is_err()
        );
        f.store.conn.execute(
            "UPDATE harness_manager_v2_records SET row_version=2 WHERE project_id=?1 AND record_key=?2",
            params![f.project.to_string(), lead_key],
        ).unwrap();
        f.store
            .conn
            .execute(
                "INSERT INTO harness_manager_v2_events(project_id,manager_session_id,scope_version,
             actor_session_id,kind,record_key,row_version,payload_json,created_at)
             VALUES(?1,?2,1,?3,'decision',?4,2,'{}',?5)",
                params![
                    f.project.to_string(),
                    f.manager.to_string(),
                    f.manager.to_string(),
                    lead_key,
                    now()
                ],
            )
            .unwrap();
        assert!(
            f.store
                .require_review_contributor_family(
                    f.project,
                    "work",
                    1,
                    &set,
                    Some(&lead_key),
                    ReviewModelFamily::Anthropic
                )
                .is_ok()
        );
        let lineage_key = override_decision(&f, f.manager, "openai");
        assert!(
            f.store
                .require_review_contributor_family(
                    f.project,
                    "work",
                    1,
                    &set,
                    Some(&lineage_key),
                    ReviewModelFamily::OpenAI
                )
                .is_err()
        );
    }

    #[test]
    fn finite_allowed_list_applies_the_override_before_exhaustion() {
        let f = fixture();
        f.store
            .conn
            .execute(
                "UPDATE sessions SET model='claude-sonnet-5' WHERE id=?1",
                [f.author.to_string()],
            )
            .unwrap();
        let openai = session(&f, "gpt-6-sol");
        let zai = session(&f, "z-ai/glm-5.3-flashx");
        assignment(&f, Some(openai), true, &now());
        assignment(&f, Some(zai), true, &now());
        let set = contributors(&f);
        let policy = f
            .store
            .get_harness_manager_policy(f.project)
            .unwrap()
            .unwrap();
        let mut next = policy.policy;
        next.allowed_launches = ["gpt-6-sol", "z-ai/glm-5.3-flashx"]
            .into_iter()
            .map(|model| ManagerLaunchChoiceV2 {
                provider: SessionProvider::OpenRouter,
                model: model.into(),
                effort: None,
            })
            .collect();
        f.store
            .configure_harness_manager_policy(&ConfigureHarnessManagerPolicyRequestV2 {
                project_id: f.project,
                expected_scope_version: 1,
                expected_policy_version: 1,
                idempotency_key: "two-families".into(),
                policy: next,
            })
            .unwrap();
        assert!(
            check(&f, &set, ReviewModelFamily::OpenAI)
                .unwrap_err()
                .to_string()
                .contains("family_exhausted")
        );
        let key = override_decision(&f, f.manager, "openai");
        assert!(
            f.store
                .require_review_contributor_family(
                    f.project,
                    "work",
                    1,
                    &set,
                    Some(&key),
                    ReviewModelFamily::OpenAI
                )
                .is_ok()
        );
        assert!(
            f.store
                .require_review_contributor_family(
                    f.project,
                    "work",
                    1,
                    &set,
                    Some(&key),
                    ReviewModelFamily::ZAi
                )
                .unwrap_err()
                .to_string()
                .contains("family_conflict")
        );
    }

    #[test]
    fn in_flight_legacy_assignment_receipt_recomputes_contributors_across_rollout_with_reversed_timestamps()
     {
        let f = fixture();
        let child = session(&f, "claude-sonnet-5");
        spawn(&f, f.author, child, "2099-01-01T00:00:00.000000000Z");
        let root = assignment(&f, None, true, "2000-01-01T00:00:00.000000000Z");
        let assignment = f.store.manager_review_assignment(root).unwrap();
        assert!(
            f.store
                .review_receipt_contributor_gate(&assignment, child)
                .unwrap_err()
                .to_string()
                .contains("family_conflict")
        );
        let missing = Uuid::new_v4();
        spawn(&f, f.author, missing, "2099-01-02T00:00:00.000000000Z");
        assert!(
            f.store
                .review_receipt_contributor_gate(&assignment, child)
                .unwrap_err()
                .to_string()
                .contains("contributors_unbounded")
        );
    }

    #[test]
    fn legacy_infra_retry_recomputes_contributors_and_fails_closed_with_reversed_timestamps() {
        let f = fixture();
        let child = session(&f, "claude-sonnet-5");
        spawn(&f, f.author, child, "2099-01-01T00:00:00.000000000Z");
        let root = assignment(&f, None, true, "2000-01-01T00:00:00.000000000Z");
        let current = f.store.manager_review_assignment(root).unwrap();
        assert!(f.store.retry_manager_review_after_infra(&current).unwrap());
        let code: String = f
            .store
            .conn
            .query_row(
                "SELECT failure_code FROM manager_review_assignments WHERE assignment_id=?1",
                [root.to_string()],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(code, "manager_review_family_conflict");
        assert_eq!(
            f.store
                .conn
                .query_row("SELECT count(*) FROM manager_review_assignments", [], |r| r
                    .get::<_, i64>(0))
                .unwrap(),
            1
        );
        let unbounded = fixture();
        let missing = Uuid::new_v4();
        spawn(
            &unbounded,
            unbounded.author,
            missing,
            "2099-01-02T00:00:00.000000000Z",
        );
        let root = assignment(&unbounded, None, true, "2000-01-01T00:00:00.000000000Z");
        let current = unbounded.store.manager_review_assignment(root).unwrap();
        assert!(
            unbounded
                .store
                .retry_manager_review_after_infra(&current)
                .unwrap()
        );
        let code: String = unbounded
            .store
            .conn
            .query_row(
                "SELECT failure_code FROM manager_review_assignments WHERE assignment_id=?1",
                [root.to_string()],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(code, "manager_review_contributors_unbounded");
        let admitted = fixture();
        let prior = session(&admitted, "z-ai/glm-5.3-flashx");
        spawn(&admitted, admitted.author, prior, &now());
        let root = assignment(&admitted, None, true, &now());
        let next = admitted
            .store
            .manager_review_infra_retry_for_test(root)
            .unwrap();
        let request = &admitted
            .store
            .manager_review_assignment(next)
            .unwrap()
            .request;
        assert!(request.contributor_session_ids.contains(&prior));
        assert!(request.contributor_families.contains(&"zai".to_string()));
    }
}
