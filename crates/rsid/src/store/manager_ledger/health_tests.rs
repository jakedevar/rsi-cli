//! K10b (#627): per-Epic `Health` inspect rows.
#![allow(clippy::unwrap_used)]
use super::super::inspect::health::HEALTH_BOUND;
use super::*;
use chrono::Duration;
use rsi_common::types::{
    ConversationEvent, EventType, Recurrence, Role, ScheduleSpec, ScheduledJob, WakeMode,
};

struct Health {
    store: Store,
    project: Uuid,
    manager: Uuid,
    /// In-scope (epic, lead) pairs, sorted by Epic id.
    epics: Vec<(Uuid, Uuid)>,
    outside: Uuid,
}

fn stamp(at: DateTime<Utc>) -> String {
    at.to_rfc3339_opts(chrono::SecondsFormat::Nanos, true)
}

fn health_fixture() -> Health {
    let store = Store::open_in_memory().unwrap();
    let project = Uuid::new_v4();
    store
        .insert_project(&Project {
            id: project,
            name: "Fleet health".into(),
            path: None,
            description: None,
            color: Project::DEFAULT_COLOR.into(),
            context_files: None,
            created_at: Utc::now(),
            updated_at: Utc::now(),
        })
        .unwrap();
    let mut manager = test_session(Uuid::new_v4(), PathBuf::from("/var/tmp/ham-health-k10b"));
    manager.session_kind = SessionKind::Standard;
    manager.project_id = Some(project);
    manager.status = SessionStatus::Completed;
    store.insert_session(&manager).unwrap();
    let mut group = manager.clone();
    group.id = Uuid::new_v4();
    group.session_kind = SessionKind::Group;
    store.insert_session(&group).unwrap();
    let mut pairs = Vec::new();
    for title in [
        "Fleet health Epic one",
        "Fleet health Epic two",
        "Outside Epic",
    ] {
        let mut epic = manager.clone();
        epic.id = Uuid::new_v4();
        epic.session_kind = SessionKind::Epic;
        epic.parent_id = Some(group.id);
        epic.title = Some(title.into());
        store.insert_session(&epic).unwrap();
        let mut lead = manager.clone();
        lead.id = Uuid::new_v4();
        lead.session_kind = SessionKind::Feature;
        lead.parent_id = Some(epic.id);
        store.insert_session(&lead).unwrap();
        store
            .conn
            .execute(
                "UPDATE sessions SET lead_session_id=?2 WHERE id=?1",
                params![epic.id.to_string(), lead.id.to_string()],
            )
            .unwrap();
        pairs.push((epic.id, lead.id));
    }
    let outside = pairs.pop().unwrap().0;
    pairs.sort();
    store
        .configure_harness_manager(&ConfigureHarnessManagerRequestV1 {
            group_ids: Vec::new(),
            project_id: project,
            session_id: manager.id,
            epic_ids: Some(pairs.iter().map(|p| p.0).collect()),
            expected_row_version: 0,
        })
        .unwrap();
    // Raw evidence rows below stand in for their producers; their foreign
    // producers (model invocations, transitions) are not under test here.
    store.conn.execute_batch("PRAGMA foreign_keys=OFF").unwrap();
    Health {
        store,
        project,
        manager: manager.id,
        epics: pairs,
        outside,
    }
}

impl Health {
    fn page(&self, query: &AgentManagerInspectRequestV2, operator: bool) -> ManagerInspectionV2 {
        if operator {
            self.store.manager_v2_inspect_operator(self.project, query)
        } else {
            self.store.manager_v2_inspect(self.manager, query)
        }
        .unwrap()
    }
    fn row(&self, index: usize) -> Value {
        let (epic, _) = self.epics[index];
        let page = self.page(
            &AgentManagerInspectRequestV2 {
                section: ManagerInspectSectionV2::Health,
                epic_id: Some(epic),
                ..Default::default()
            },
            false,
        );
        assert_eq!(page.rows.len(), 1);
        assert_eq!(page.rows[0]["epic_id"], epic.to_string());
        page.rows[0].clone()
    }
    fn child(&self, index: usize, status: SessionStatus) -> Uuid {
        let (epic, _) = self.epics[index];
        let mut child = self.store.get_session(self.manager).unwrap().unwrap();
        child.id = Uuid::new_v4();
        child.session_kind = SessionKind::Task;
        child.parent_id = Some(epic);
        child.status = status;
        child.title = Some("Health worker".into());
        self.store.insert_session(&child).unwrap();
        child.id
    }
    fn job(&self, watched: Uuid, waker: Uuid, enabled: bool) -> Uuid {
        let now = Utc::now();
        let job = ScheduledJob {
            id: Uuid::new_v4(),
            name: "rsi-watch".into(),
            message: String::new(),
            schedule: ScheduleSpec {
                recurrence: Recurrence::EverySeconds(60),
                anchor: now,
            },
            last_fired_at: None,
            next_fire_at: now,
            enabled,
            working_dir: None,
            provider: None,
            model: None,
            project_id: None,
            created_at: now,
            updated_at: now,
            wake_mode: WakeMode::OnTerminal(watched),
            wake_session_id: Some(waker),
        };
        self.store.insert_scheduled_job(&job).unwrap();
        job.id
    }
    /// A pending `to_manager` notice on a manager watch whose job is `enabled`.
    fn notice(
        &self,
        index: usize,
        enabled: bool,
        delivered: Option<DateTime<Utc>>,
    ) -> (Uuid, Uuid) {
        let (epic, lead) = self.epics[index];
        let job = self.job(lead, self.manager, enabled);
        self.store
            .conn
            .execute(
                "INSERT INTO harness_manager_watches
                 (job_id,project_id,epic_id,scope_version,direction,source_session_id,
                  target_session_id,attention_signature)
                 VALUES(?1,?2,?3,1,'to_manager',?4,?5,'sig')",
                params![
                    job.to_string(),
                    self.project.to_string(),
                    epic.to_string(),
                    lead.to_string(),
                    self.manager.to_string()
                ],
            )
            .unwrap();
        let id = Uuid::new_v4();
        let queued = stamp(Utc::now() - Duration::minutes(45));
        self.store
            .conn
            .execute(
                "INSERT INTO harness_manager_notices
                 (id,job_id,project_id,manager_session_id,scope_version,epic_id,direction,
                  source_session_id,recipient_session_id,kind,subject_id,subject_version,
                  state_json,recorded_at,queued_at,delivered_at)
                 VALUES(?1,?2,?3,?4,1,?5,'to_manager',?6,?4,'message',?1,'1','{}',?7,?7,?8)",
                params![
                    id.to_string(),
                    job.to_string(),
                    self.project.to_string(),
                    self.manager.to_string(),
                    epic.to_string(),
                    lead.to_string(),
                    queued,
                    delivered.map(stamp)
                ],
            )
            .unwrap();
        (id, job)
    }
    fn event(
        &self,
        session: Uuid,
        event_type: EventType,
        role: Option<Role>,
        metadata: Option<Value>,
    ) {
        let sequence: i32 = self
            .store
            .conn
            .query_row(
                "SELECT COALESCE(MAX(sequence)+1,0) FROM conversation_events WHERE session_id=?1",
                [session.to_string()],
                |r| r.get(0),
            )
            .unwrap();
        self.store
            .insert_event(&ConversationEvent {
                id: 0,
                session_id: session,
                sequence,
                event_type,
                role,
                content: "health".into(),
                tool_name: None,
                tool_input: None,
                created_at: Utc::now(),
                offload_id: None,
                tool_use_id: None,
                metadata: metadata.map(Box::new),
            })
            .unwrap();
    }
    fn manager_request(&self, index: usize, recipient: Uuid) -> Uuid {
        let (epic, _) = self.epics[index];
        let id = Uuid::new_v4();
        self.store
            .conn
            .execute(
                "INSERT INTO harness_manager_messages
                 (id,project_id,manager_session_id,epic_id,scope_version,sender_session_id,
                  recipient_session_id,request_id,idempotency_key,request_fingerprint,message,created_at)
                 VALUES(?1,?2,?3,?4,1,?3,?5,NULL,?1,'request-fingerprint','Report status',?6)",
                params![
                    id.to_string(),
                    self.project.to_string(),
                    self.manager.to_string(),
                    epic.to_string(),
                    recipient.to_string(),
                    stamp(Utc::now())
                ],
            )
            .unwrap();
        id
    }
}

fn stuck(row: &Value, code: &str) -> Vec<Value> {
    row["stuck"]
        .as_array()
        .unwrap()
        .iter()
        .find(|entry| entry["code"] == code)
        .unwrap_or_else(|| panic!("{code} missing from {row}"))["evidence"]
        .as_array()
        .unwrap()
        .clone()
}

/// Observation ages move with the wall clock; everything else must match.
fn without_ages(value: &Value) -> Value {
    match value {
        Value::Object(map) => Value::Object(
            map.iter()
                .filter(|(key, _)| !key.ends_with("age_seconds"))
                .map(|(key, value)| (key.clone(), without_ages(value)))
                .collect(),
        ),
        Value::Array(items) => Value::Array(items.iter().map(without_ages).collect()),
        other => other.clone(),
    }
}

#[test]
fn health_pages_exactly_the_scoped_epics_by_id_with_cursor_and_complete() {
    let h = health_fixture();
    let mut query = AgentManagerInspectRequestV2 {
        section: ManagerInspectSectionV2::Health,
        limit: 1,
        ..Default::default()
    };
    let first = h.page(&query, false);
    assert_eq!(first.section, ManagerInspectSectionV2::Health);
    assert_eq!(first.rows.len(), 1);
    assert!(!first.complete);
    query.cursor = Some(first.next_cursor.clone().unwrap());
    let second = h.page(&query, false);
    assert_eq!(second.rows.len(), 1);
    assert!(second.complete);
    assert_eq!(second.next_cursor, None);
    let ids: Vec<_> = first
        .rows
        .iter()
        .chain(&second.rows)
        .map(|row| row["epic_id"].as_str().unwrap().to_owned())
        .collect();
    let expected: Vec<_> = h.epics.iter().map(|(epic, _)| epic.to_string()).collect();
    assert_eq!(ids, expected);
    for (row, (_, lead)) in first.rows.iter().chain(&second.rows).zip(&h.epics) {
        assert_eq!(row["type"], "health");
        assert_eq!(row["key"], row["epic_id"]);
        assert!(
            row["title"]
                .as_str()
                .unwrap()
                .starts_with("Fleet health Epic")
        );
        assert_eq!(row["lead"]["session_id"], lead.to_string());
    }
    let refused = h
        .store
        .manager_v2_inspect(
            h.manager,
            &AgentManagerInspectRequestV2 {
                section: ManagerInspectSectionV2::Health,
                epic_id: Some(h.outside),
                ..Default::default()
            },
        )
        .unwrap_err();
    assert!(refused.to_string().contains("manager_v2_epic_out_of_scope"));
}

#[test]
fn healthy_epic_reports_every_count_and_an_empty_stuck_list() {
    let h = health_fixture();
    let row = h.row(0);
    let (_, lead) = h.epics[0];
    assert_eq!(row["stuck"], json!([]));
    assert_eq!(row["complete"], true);
    assert_eq!(row["truncated"], json!([]));
    assert_eq!(row["lead_state"], "current");
    assert_eq!(row["lead"]["status"], "Completed");
    assert_eq!(row["lead"]["lineage_tip_id"], lead.to_string());
    assert!(row["lead"]["age_seconds"].as_i64().unwrap() >= 0);
    assert!(row["lead"].get("provider").is_some());
    assert_eq!(row["children"]["live"], 0);
    for direction in ["to_manager", "to_lead"] {
        // Appointment arms the Epic's manager watches; counts are always present.
        assert!(row["wakes"]["manager_watches"][direction]["enabled"].is_u64());
        assert_eq!(row["wakes"]["manager_watches"][direction]["disabled"], 0);
        // Appointment may queue its own session-state notice: counts are
        // present and consistent, and none is stalled or unread (empty stuck).
        let n = &row["notices"][direction];
        assert_eq!(
            n["pending"].as_u64().unwrap(),
            n["undelivered"].as_u64().unwrap() + n["delivered_unretrieved"].as_u64().unwrap(),
            "{direction}"
        );
    }
    assert_eq!(row["wakes"]["lead_child_watch"], false);
    assert_eq!(row["reports"]["unanswered_requests"], 0);
    assert_eq!(row["reports"]["active_requests"], 0);
    assert_eq!(row["reports"]["pending_requests"], 0);
    assert_eq!(row["reviews"]["active"], 0);
    assert_eq!(row["reviews"]["failed"], 0);
    assert_eq!(row["facts"]["requests_lead_changed"], json!([]));
}

#[test]
fn health_row_includes_latest_durable_watchdog_restart() {
    let fixture = health_fixture();
    assert!(fixture.row(0)["latest_daemon_restart"].is_null());
    let restart = crate::watchdog::RestartRecord {
        version: 1,
        id: Uuid::new_v4(),
        observed_at: Utc::now(),
        last_healthy_at: Utc::now() - Duration::seconds(10),
        failed_probes: vec!["store_probe_timeout".into()],
    };
    fixture
        .store
        .persist_daemon_restart_record(&restart)
        .unwrap();
    let row = fixture.row(0);
    assert_eq!(row["latest_daemon_restart"]["id"], restart.id.to_string());
    assert_eq!(
        row["latest_daemon_restart"]["failed_probes"],
        serde_json::json!(["store_probe_timeout"])
    );
}

#[test]
fn operator_health_returns_the_manager_rows_without_caller_scoping() {
    let h = health_fixture();
    h.child(0, SessionStatus::Running);
    let query = AgentManagerInspectRequestV2 {
        section: ManagerInspectSectionV2::Health,
        ..Default::default()
    };
    let manager = h.page(&query, false);
    let operator = h.page(&query, true);
    assert_eq!(operator.rows.len(), 2);
    assert!(operator.complete);
    assert_eq!(
        operator.rows.iter().map(without_ages).collect::<Vec<_>>(),
        manager.rows.iter().map(without_ages).collect::<Vec<_>>()
    );
    assert_eq!(
        stuck(&operator.rows[0], "manager_watch_missing")[0],
        h.epics[0].1.to_string()
    );
}

#[test]
fn idle_lead_with_a_live_child_and_no_watch_is_manager_watch_missing() {
    let h = health_fixture();
    let (_, lead) = h.epics[0];
    let child = h.child(0, SessionStatus::Running);
    h.child(0, SessionStatus::Completed);
    let row = h.row(0);
    assert_eq!(row["children"]["live"], 1);
    assert_eq!(row["children"]["oldest_live_session_id"], child.to_string());
    assert_eq!(
        stuck(&row, "manager_watch_missing"),
        vec![json!(lead.to_string()), json!(child.to_string())]
    );
    h.job(child, lead, true);
    let row = h.row(0);
    assert_eq!(row["wakes"]["lead_child_watch"], true);
    assert_eq!(row["wakes"]["lead_child_watches"], 1);
    assert_eq!(row["stuck"], json!([]));
}

#[test]
fn pending_notice_on_a_disabled_watch_is_transport_stalled() {
    let h = health_fixture();
    let base = h.row(0);
    let armed = base["wakes"]["manager_watches"]["to_manager"]["enabled"].clone();
    let count = |row: &Value, field: &str| row["notices"]["to_manager"][field].as_u64().unwrap();
    let (_, job) = h.notice(0, false, None);
    let row = h.row(0);
    assert_eq!(
        stuck(&row, "manager_notice_transport_stalled"),
        vec![json!(job.to_string())]
    );
    assert_eq!(count(&row, "pending"), count(&base, "pending") + 1);
    assert_eq!(count(&row, "undelivered"), count(&base, "undelivered") + 1);
    assert!(row["notices"]["to_manager"]["oldest_pending_age_seconds"].is_i64());
    assert!(row["notices"]["to_manager"]["oldest_pending_at"].is_string());
    assert_eq!(row["wakes"]["manager_watches"]["to_manager"]["disabled"], 1);
    assert_eq!(
        row["wakes"]["manager_watches"]["to_manager"]["enabled"],
        armed
    );
}

#[test]
fn delivered_notice_unretrieved_past_the_threshold_is_unread() {
    let h = health_fixture();
    let armed = h.row(0)["wakes"]["manager_watches"]["to_manager"]["enabled"]
        .as_u64()
        .unwrap();
    let (fresh, _) = h.notice(0, true, Some(Utc::now()));
    let (old, _) = h.notice(0, true, Some(Utc::now() - Duration::seconds(31 * 60)));
    let row = h.row(0);
    assert_eq!(
        stuck(&row, "manager_notice_unread"),
        vec![json!(old.to_string())]
    );
    assert!(
        row["notices"]["to_manager"]["delivered_unretrieved"]
            .as_u64()
            .unwrap()
            >= 2
    );
    assert_eq!(
        row["wakes"]["manager_watches"]["to_manager"]["enabled"],
        armed + 2
    );
    let _ = fresh;
}

#[test]
fn uncertain_successor_reservation_is_reported_with_its_id() {
    let h = health_fixture();
    let (epic, lead) = h.epics[1];
    let reservation = Uuid::new_v4();
    let now = stamp(Utc::now());
    h.store
        .conn
        .execute(
            "INSERT INTO agent_successor_reservations
             (reservation_id,predecessor_session_id,epic_id,candidate_session_id,
              caller_key_digest,request_json,request_fingerprint,candidate_kind,
              inherited_launch_json,expected_lead_session_id,expected_lead_generation,
              state,state_version,launch_attempt_id,model_invocation_id,
              terminal_reason,safe_error_class,reserved_at,updated_at)
             VALUES(?1,?2,?3,?4,?5,'{}',?5,'Task','{}',?2,1,'uncertain',2,?6,?7,
                    'establishment unknown','launch_uncertain',?8,?8)",
            params![
                reservation.to_string(),
                lead.to_string(),
                epic.to_string(),
                Uuid::new_v4().to_string(),
                format!("sha256:{}", "b".repeat(64)),
                Uuid::new_v4().to_string(),
                Uuid::new_v4().to_string(),
                now
            ],
        )
        .unwrap();
    let row = h.row(1);
    assert_eq!(
        stuck(&row, "agent_successor_uncertain"),
        vec![json!(reservation.to_string())]
    );
    assert_eq!(
        row["facts"]["successor_uncertain"],
        json!([reservation.to_string()])
    );
}

#[test]
fn failed_review_assignment_surfaces_its_existing_failure_code() {
    let h = health_fixture();
    let (epic, lead) = h.epics[0];
    let insert = |state: &str, code: Option<&str>, terminal: bool| {
        let id = Uuid::new_v4();
        let now = stamp(Utc::now());
        h.store
            .conn
            .execute(
                "INSERT INTO manager_review_assignments(
                    assignment_id,project_id,epic_id,manager_session_id,scope_version,
                    work_key,spec_revision,author_session_id,source_sha,state,row_version,
                    request_json,request_fingerprint,failure_code,created_at,updated_at,terminal_at)
                 VALUES(?1,?2,?3,?4,1,'slice',1,?5,?6,?7,1,'{}',?8,?9,?10,?10,?11)",
                params![
                    id.to_string(),
                    h.project.to_string(),
                    epic.to_string(),
                    h.manager.to_string(),
                    lead.to_string(),
                    format!("{:040x}", id.as_u128() & 0xffff),
                    state,
                    format!("sha256:{}", "a".repeat(64)),
                    code,
                    now,
                    terminal.then(|| now.clone())
                ],
            )
            .unwrap();
        id
    };
    let failed = insert("failed", Some("manager_review_custody_unavailable"), true);
    insert("reserved", None, false);
    let row = h.row(0);
    assert_eq!(
        stuck(&row, "manager_review_custody_unavailable"),
        vec![json!(failed.to_string())]
    );
    assert_eq!(row["reviews"]["active"], 1);
    assert_eq!(row["reviews"]["failed"], 1);
    assert_eq!(
        row["reviews"]["failure_codes"],
        json!([{"assignment_id":failed,"failure_code":"manager_review_custody_unavailable"}])
    );
}

#[test]
fn delivery_abandoned_fact_newer_than_provider_output_is_stuck_until_output_follows() {
    let h = health_fixture();
    let (_, lead) = h.epics[0];
    let job = Uuid::new_v4();
    let watched = Uuid::new_v4();
    let abandoned_at = stamp(Utc::now());
    h.event(lead, EventType::Message, Some(Role::Assistant), None);
    // Exactly the stored K10d (#648) fact shape.
    h.event(
        lead,
        EventType::System,
        None,
        Some(json!({"health_fact":"delivery_abandoned","job_id":job,
            "watched_session_id":watched,"minutes_unconsumed":20,"abandoned_at":abandoned_at})),
    );
    let row = h.row(0);
    assert_eq!(
        stuck(&row, "lead_delivery_abandoned"),
        vec![json!(job.to_string())]
    );
    assert_eq!(
        row["facts"]["delivery_abandoned"]["job_id"],
        job.to_string()
    );
    assert_eq!(row["facts"]["delivery_abandoned"]["at"], abandoned_at);
    h.event(lead, EventType::Message, Some(Role::Assistant), None);
    let row = h.row(0);
    assert_eq!(
        row["facts"]["delivery_abandoned"]["job_id"],
        job.to_string()
    );
    assert_eq!(row["stuck"], json!([]));
}

#[test]
fn open_request_to_a_replaced_lead_is_request_lead_changed() {
    let h = health_fixture();
    let (epic, old_lead) = h.epics[0];
    let stale = h.manager_request(0, old_lead);
    let mut new_lead = h.store.get_session(old_lead).unwrap().unwrap();
    new_lead.id = Uuid::new_v4();
    h.store.insert_session(&new_lead).unwrap();
    h.store
        .conn
        .execute(
            "UPDATE sessions SET lead_session_id=?2 WHERE id=?1",
            params![epic.to_string(), new_lead.id.to_string()],
        )
        .unwrap();
    h.manager_request(0, new_lead.id);
    let row = h.row(0);
    assert_eq!(
        stuck(&row, "manager_request_lead_changed"),
        vec![json!(stale.to_string())]
    );
    assert_eq!(row["reports"]["unanswered_requests"], 1);
    assert_eq!(
        row["facts"]["requests_lead_changed"],
        json!([{"request_id":stale,"recipient_session_id":old_lead}])
    );
}

/// Replace Epic `index`'s lead with a fresh (non-lineage) session.
fn replace_lead(h: &Health, index: usize) -> Uuid {
    let (epic, old_lead) = h.epics[index];
    let mut new_lead = h.store.get_session(old_lead).unwrap().unwrap();
    new_lead.id = Uuid::new_v4();
    h.store.insert_session(&new_lead).unwrap();
    h.store
        .conn
        .execute(
            "UPDATE sessions SET lead_session_id=?2 WHERE id=?1",
            params![epic.to_string(), new_lead.id.to_string()],
        )
        .unwrap();
    new_lead.id
}

#[test]
fn settled_or_released_orphan_is_not_request_lead_changed() {
    // Review 7c6df190: Health must share the #664 open predicate, so an orphan
    // the daemon settled as `lead_replaced` (or one the lead released) is
    // closed mail, never `manager_request_lead_changed`.
    let h = health_fixture();
    let (epic, old_lead) = h.epics[0];
    let settled = h.manager_request(0, old_lead);
    let released = h.manager_request(0, old_lead);
    let new_lead = replace_lead(&h, 0);
    let baseline = h.row(0);
    assert_eq!(
        stuck(&baseline, "manager_request_lead_changed").len(),
        2,
        "both orphans are open before settlement"
    );
    let config = h.store.get_harness_manager(h.project).unwrap().unwrap();
    h.store
        .manager_v2_put_record(
            &config,
            crate::store::harness_manager::REQUEST_RELEASED_KIND,
            &released.to_string(),
            Some(epic),
            0,
            &json!({"state":"failed","actor":old_lead}),
        )
        .unwrap();
    assert_eq!(
        h.store.settle_orphaned_manager_requests(&config).unwrap(),
        1
    );
    assert_eq!(
        h.store
            .manager_request_settlement(&config, settled)
            .unwrap()
            .as_deref(),
        Some("lead_replaced")
    );
    let live = h.manager_request(0, new_lead);
    let row = h.row(0);
    assert_eq!(row["facts"]["requests_lead_changed"], json!([]));
    assert_eq!(row["reports"]["unanswered_requests"], 1);
    assert_eq!(row["truncated"], json!([]));
    // The only remaining open request is the live one to the current lead.
    let unanswered = h
        .store
        .manager_v2_request_rows(&config, Some(epic), "", 32, true)
        .unwrap();
    assert_eq!(
        unanswered
            .iter()
            .map(|r| r["request_id"].clone())
            .collect::<Vec<_>>(),
        vec![json!(live.to_string())]
    );
}

#[test]
fn more_than_the_bound_of_settled_orphans_cannot_hide_a_later_open_request() {
    // Review 7c6df190: closed rows are filtered before the LIMIT, so 33
    // settled orphans (more than HEALTH_BOUND) neither truncate the report nor
    // crowd out the genuinely open request sent afterwards.
    let h = health_fixture();
    let (_, old_lead) = h.epics[0];
    for _ in 0..=HEALTH_BOUND {
        h.manager_request(0, old_lead);
    }
    let new_lead = replace_lead(&h, 0);
    let config = h.store.get_harness_manager(h.project).unwrap().unwrap();
    assert_eq!(
        h.store.settle_orphaned_manager_requests(&config).unwrap(),
        HEALTH_BOUND + 1
    );
    let row = h.row(0);
    assert_eq!(row["reports"]["unanswered_requests"], 0);
    assert_eq!(row["facts"]["requests_lead_changed"], json!([]));
    assert_eq!(row["truncated"], json!([]));
    let orphan = h.manager_request(0, old_lead);
    h.manager_request(0, new_lead);
    let row = h.row(0);
    assert_eq!(row["reports"]["unanswered_requests"], 1);
    assert_eq!(
        row["facts"]["requests_lead_changed"],
        json!([{"request_id":orphan,"recipient_session_id":old_lead}])
    );
    assert_eq!(row["truncated"], json!([]));
    assert_eq!(row["complete"], true);
}

#[test]
fn reaching_an_aggregate_bound_marks_the_row_and_page_incomplete() {
    let h = health_fixture();
    for _ in 0..=HEALTH_BOUND {
        h.child(0, SessionStatus::Running);
    }
    let row = h.row(0);
    assert_eq!(row["children"]["live"], HEALTH_BOUND);
    assert_eq!(row["complete"], false);
    assert_eq!(row["truncated"], json!(["children.live"]));
    assert_eq!(stuck(&row, "manager_watch_missing").len(), HEALTH_BOUND);
    let page = h.page(
        &AgentManagerInspectRequestV2 {
            section: ManagerInspectSectionV2::Health,
            ..Default::default()
        },
        true,
    );
    assert!(!page.complete);
    assert_eq!(page.next_cursor, None);
}

// --- Round 2 (review bd1ca7a1) -------------------------------------------

fn set_status(h: &Health, session: Uuid, status: &str) {
    h.store
        .conn
        .execute(
            "UPDATE sessions SET status=?2 WHERE id=?1",
            params![session.to_string(), status],
        )
        .unwrap();
}

#[test]
fn lead_rotation_refused_reports_evidence_until_a_later_completed_rotation() {
    let h = health_fixture();
    let (_, lead) = h.epics[0];
    h.store
        .insert_rotation_event(lead, "rot-1", "started", "rotation_started", None)
        .unwrap();
    // Exactly the stored F3 refusal shape (session/rotation.rs record_rotation_refusal).
    h.store
        .insert_rotation_event(lead, "rot-1", "completed", "refused:custody_changed", None)
        .unwrap();
    let row = h.row(0);
    let evidence = stuck(&row, "lead_rotation_refused");
    assert_eq!(evidence[0], "rot-1");
    assert_eq!(evidence[1], "custody_changed");
    assert!(evidence[2].is_string());
    assert_eq!(row["facts"]["rotation_refused"]["rotation_id"], "rot-1");
    assert_eq!(row["facts"]["rotation_refused"]["code"], "custody_changed");
    h.store
        .insert_rotation_event(lead, "rot-2", "completed", "completed", None)
        .unwrap();
    let row = h.row(0);
    assert_eq!(row["stuck"], json!([]));
    assert_eq!(row["facts"]["rotation_refused"], Value::Null);
}

#[test]
fn failed_or_interrupted_lead_is_lead_unavailable() {
    let h = health_fixture();
    for (index, status) in [(0, "Failed"), (1, "Interrupted")] {
        let (_, lead) = h.epics[index];
        set_status(&h, lead, status);
        let row = h.row(index);
        let evidence = stuck(&row, "lead_unavailable");
        assert_eq!(evidence[0], lead.to_string());
        assert_eq!(evidence[1], status);
        assert!(evidence[2].is_string());
        assert_eq!(row["lead"]["status"], status);
    }
}

#[test]
fn failed_lead_with_an_open_capacity_incident_stays_unavailable_with_owner_evidence() {
    let h = health_fixture();
    let (_, lead) = h.epics[0];
    set_status(&h, lead, "Failed");
    let incident = Uuid::new_v4();
    let now = stamp(Utc::now());
    h.store
        .conn
        .execute(
            "INSERT INTO master_no_idle_capacity_incidents(
                incident_id,program_guard_job_id,controller_session_id,capacity_class,
                outage_epoch,state,backoff_bucket,wake_job_id,issue_id,project_id,
                provider,model,working_dir,last_capacity_model_invocation_id,
                last_terminal_sequence,next_due_slot,opened_at,updated_at,closed_at,close_reason
             ) VALUES(?1,?2,?3,'codex_usage_limit',1,'open',1,?4,NULL,NULL,
                      'Codex',NULL,'/var/tmp/ham-health-k10b',?5,0,?6,?6,?6,NULL,NULL)",
            params![
                incident.to_string(),
                Uuid::new_v4().to_string(),
                lead.to_string(),
                Uuid::new_v4().to_string(),
                Uuid::new_v4().to_string(),
                now
            ],
        )
        .unwrap();
    let row = h.row(0);
    let evidence = stuck(&row, "lead_unavailable");
    assert_eq!(evidence[0], lead.to_string());
    assert_eq!(evidence[1], "Failed");
    assert!(
        evidence.contains(&json!(incident.to_string())),
        "{evidence:?}"
    );
    assert_eq!(row["facts"]["lead_retry_owner"], incident.to_string());
    assert_eq!(row["lead"]["status"], "Failed");
}

fn plan(h: &Health, sql: &str) -> Vec<String> {
    let mut statement = h
        .store
        .conn
        .prepare(&format!("EXPLAIN QUERY PLAN {sql}"))
        .unwrap();
    let count = statement.parameter_count();
    let values: Vec<String> = (0..count).map(|i| format!("p{i}")).collect();
    statement
        .query_map(rusqlite::params_from_iter(values), |r| {
            r.get::<_, String>(3)
        })
        .unwrap()
        .collect::<rusqlite::Result<Vec<_>>>()
        .unwrap()
}

#[test]
fn per_epic_successor_and_report_reads_seek_by_the_epic_children() {
    use super::super::inspect::health::{LATEST_REPORT_SQL, UNCERTAIN_SUCCESSOR_SQL};
    let h = health_fixture();
    // The Epic is the outer seek (any sessions index keyed on parent_id); the
    // joined table is then probed by its per-session key, never scanned.
    for (sql, inner) in [
        (UNCERTAIN_SUCCESSOR_SQL, "SEARCH r USING"),
        (LATEST_REPORT_SQL, "SEARCH m USING"),
    ] {
        let steps = plan(&h, sql);
        eprintln!("{steps:?}");
        let outer = steps
            .iter()
            .position(|s| s.starts_with("SEARCH s USING") && s.contains("(parent_id=?)"))
            .unwrap_or_else(|| panic!("no Epic seek in {steps:?}"));
        let joined = steps
            .iter()
            .position(|s| s.starts_with(inner) && s.contains("_session_id=?)"))
            .unwrap_or_else(|| panic!("no keyed {inner} in {steps:?}"));
        assert!(outer < joined, "{steps:?}");
    }
}
