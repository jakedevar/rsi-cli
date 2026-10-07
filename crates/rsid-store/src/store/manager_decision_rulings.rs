//! #1415: a decision record is a real gate (operator-only) or a non-gate that
//! a delegated manager may settle. This module holds the classification, the
//! board projection (who may answer, what it blocks, age, staleness), the
//! audited manager ruling and withdrawal, and the operator's stale list and
//! bulk archive. Records are JSON payloads in `harness_manager_v2_records`:
//! no schema change, and nothing is ever hard-deleted (archive sets the
//! existing `archived` flag).

use chrono::{DateTime, Utc};
use rsi_common::harness_manager::{HarnessManagerConfigV1, HarnessManagerScopeModeV1};
use rsi_common::harness_manager_v2::*;
use rusqlite::{Transaction, TransactionBehavior, params};
use serde_json::{Value, json};
use uuid::Uuid;

use super::Store;
use super::harness_manager_v2::{ManagerAuthorityV2, ManagerRecordV2, now, refused};
use super::manager_ledger::record_row;
use super::portfolio_nodes::node_row_on;
use crate::error::Result;

/// Audit entries one record keeps; the oldest after the first is dropped.
const HISTORY_LIMIT: usize = 32;
/// Pending records one stale scan reads before it reports an incomplete page.
const STALE_SCAN: i64 = 2048;
/// Rows one `rulings` page may carry.
const RULINGS_PAGE: usize = 65;

/// Why a record is operator-only.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct DecisionGate {
    pub class: &'static str,
    /// `reserved_key` (daemon-created gate), `declared` (the asker's class) or
    /// `daemon_scan` (the question names a real gate the asker did not).
    pub source: &'static str,
}

fn gate_class(raw: &str) -> &'static str {
    match raw {
        "main_or_release" => "main_or_release",
        "spend" => "spend",
        "credentials" => "credentials",
        "data_deletion" => "data_deletion",
        _ => "human_approval",
    }
}

/// Phrases that name a real gate. Defence in depth for an undeclared question:
/// a hit sends the record to the operator, so a false positive costs one
/// operator answer. This scan is conservative; askers still declare gates,
/// and the underlying effects retain their own authority checks.
const GATE_PHRASES: &[(&str, &[&str])] = &[
    (
        "main_or_release",
        &[
            "merge to main",
            "merge into main",
            "push to main",
            "push main",
            "main branch",
            "cut a release",
            "release candidate",
            "publish a release",
            "tag a release",
            "deploy to production",
            "deploy to prod",
            "production deploy",
        ],
    ),
    (
        "spend",
        &[
            "spend",
            "budget",
            "billing",
            "purchase",
            "subscription",
            "invoice",
            "paid plan",
            "real money",
            "pay for",
        ],
    ),
    (
        "credentials",
        &[
            "credential",
            "password",
            "api key",
            "access key",
            "secret key",
            "private key",
            "client secret",
            "signing key",
        ],
    ),
    (
        "data_deletion",
        &[
            "delete user data",
            "delete customer",
            "delete production",
            "drop the database",
            "drop table",
            "purge",
            "hard delete",
            "hard-delete",
            "wipe",
        ],
    ),
    ("human_approval", &["human approval", "operator approval"]),
];

/// Formatting and intervening words must not turn an operator gate into a
/// manager decision (for example, "merge this into `main`" or "delete all
/// user data"). Match action/target words independently of their order.
fn scan_decision_gate(question: &str) -> Option<&'static str> {
    let lower = question.to_lowercase();
    let words: Vec<_> = lower
        .split(|c: char| !c.is_alphanumeric())
        .filter(|word| !word.is_empty())
        .collect();
    let normalized = words.join(" ");
    if let Some(class) = GATE_PHRASES.iter().find_map(|(class, phrases)| {
        phrases
            .iter()
            .any(|phrase| lower.contains(phrase) || normalized.contains(phrase))
            .then_some(*class)
    }) {
        return Some(class);
    }
    let has = |candidates: &[&str]| words.iter().any(|word| candidates.contains(word));
    let names_version = has(&["version", "versions"])
        || words.iter().any(|word| {
            word.strip_prefix('v')
                .and_then(|suffix| suffix.chars().next())
                .is_some_and(|c| c.is_ascii_digit())
        });
    if has(&["release", "releases"])
        || (has(&["main"]) && has(&["merge", "merging", "push", "pushing"]))
        || (names_version && has(&["publish", "publishing", "tag", "tagging", "ship"]))
    {
        return Some("main_or_release");
    }
    let names_amount = lower.char_indices().any(|(index, c)| {
        matches!(c, '$' | '€' | '£' | '¥')
            && lower[index + c.len_utf8()..]
                .trim_start()
                .chars()
                .next()
                .is_some_and(|c| c.is_ascii_digit())
    });
    if names_amount || has(&["buy", "buying", "pay", "paying", "rent", "renting"]) {
        return Some("spend");
    }
    if has(&["oauth", "passphrase"])
        || (has(&["token", "tokens", "key", "keys"])
            && has(&[
                "auth",
                "authentication",
                "access",
                "refresh",
                "bearer",
                "api",
                "secret",
                "ssh",
            ]))
    {
        return Some("credentials");
    }
    if has(&[
        "delete", "deleting", "remove", "removing", "erase", "destroy", "drop", "truncate",
    ]) && has(&[
        "user",
        "users",
        "customer",
        "customers",
        "production",
        "database",
        "data",
        "records",
    ]) {
        return Some("data_deletion");
    }
    if has(&["human", "operator"])
        && (has(&["approval", "approve", "consent", "permission", "signoff"])
            || (has(&["sign"]) && has(&["off"])))
    {
        return Some("human_approval");
    }
    None
}

/// Classify one decision record. `has_target` is true when the daemon created
/// the record for an exact session question or approval.
pub(crate) fn classify_decision(
    key: &str,
    payload: &Value,
    has_target: bool,
) -> Option<DecisionGate> {
    if has_target
        || key.starts_with("accept:")
        || key.starts_with("question:")
        || key.starts_with("approval:")
    {
        return Some(DecisionGate {
            class: "human_approval",
            source: "reserved_key",
        });
    }
    if let Some(declared) = payload["gate"].as_str() {
        return Some(DecisionGate {
            class: gate_class(declared),
            source: "declared",
        });
    }
    scan_decision_gate(payload["question"].as_str()?).map(|class| DecisionGate {
        class,
        source: "daemon_scan",
    })
}

/// Append one audit entry, keeping the first (the question) and the newest.
pub(crate) fn push_history(decision: &mut Value, event: &str, actor: &Value, note: Option<&str>) {
    let mut history = decision["history"].as_array().cloned().unwrap_or_default();
    let mut entry = json!({"event":event,"at":actor["at"],"actor":actor});
    if let Some(note) = note {
        entry["note"] = json!(note);
    }
    history.push(entry);
    while history.len() > HISTORY_LIMIT {
        history.remove(1);
    }
    decision["history"] = Value::Array(history);
}

pub(crate) fn operator_actor() -> Value {
    json!({"kind":"operator","session_id":null,"node_label":null,"at":now()})
}

fn ledger_config(project: Uuid, manager: Uuid, scope: i64) -> HarnessManagerConfigV1 {
    HarnessManagerConfigV1 {
        project_id: project,
        manager_session_id: manager,
        current_session_id: None,
        epic_ids: Vec::new(),
        scope_mode: HarnessManagerScopeModeV1::Project,
        selected_epic_ids: None,
        group_ids: Vec::new(),
        row_version: scope,
        updated_at: Utc::now(),
    }
}

fn age_seconds(created_at: &str) -> i64 {
    DateTime::parse_from_rfc3339(created_at).map_or(0, |at| {
        Utc::now()
            .signed_duration_since(at.with_timezone(&Utc))
            .num_seconds()
            .max(0)
    })
}

fn too_old(created_at: &str, days: u32) -> bool {
    age_seconds(created_at) > i64::from(days) * 86_400
}

struct Settlement {
    owner: HarnessManagerConfigV1,
    record: ManagerRecordV2,
    epic: Uuid,
    /// `self`, `project_manager` or `portfolio_manager`.
    relation: &'static str,
}

impl Store {
    /// Who is acting: the audit identity recorded in a record's history.
    pub(crate) fn manager_v2_decision_actor(&self, a: &ManagerAuthorityV2) -> Result<Value> {
        let at = now();
        if !a.is_manager {
            return Ok(json!({"kind":"lead","session_id":a.caller,"node_label":null,"at":at}));
        }
        let head = self
            .portfolio_chain_heads(a.config.project_id)?
            .into_iter()
            .find(|head| {
                head.seat_root == a.config.manager_session_id && head.epoch == a.config.row_version
            });
        if let Some(head) = head {
            let label = node_row_on(&self.conn, head.node_id)?.map(|node| node.tier_label);
            return Ok(
                json!({"kind":"portfolio_manager","session_id":a.caller,"node_label":label,"at":at}),
            );
        }
        Ok(json!({"kind":"project_manager","session_id":a.caller,"node_label":null,"at":at}))
    }

    /// Stamp a posted question: who asked, and the audit trail (a re-ask keeps
    /// the original asker and the trail).
    pub(crate) fn manager_v2_decision_ask_audit(
        &self,
        a: &ManagerAuthorityV2,
        prior: Option<&ManagerRecordV2>,
        value: &mut Value,
    ) -> Result<()> {
        let actor = self.manager_v2_decision_actor(a)?;
        if let Some(prior) = prior {
            value["history"] = prior.payload["history"].clone();
            if value["history"].is_null() {
                value["history"] = json!([]);
            }
            value["asked_by"] = match &prior.payload["asked_by"] {
                Value::Null => actor.clone(),
                asked => asked.clone(),
            };
        } else {
            value["asked_by"] = actor.clone();
        }
        let event = if prior.is_some() { "reasked" } else { "asked" };
        push_history(value, event, &actor, None);
        Ok(())
    }

    /// Whether the manager that owns this ledger can still act: the project's
    /// current manager with a live seat and an unrevoked grant, an active
    /// portfolio node, or an active area node.
    pub(crate) fn manager_v2_ledger_live(
        &self,
        project: Uuid,
        manager: Uuid,
        scope: i64,
    ) -> Result<bool> {
        if let Some(config) = self.get_harness_manager(project)?
            && config.manager_session_id == manager
            && config.row_version == scope
        {
            let granted = self
                .get_harness_manager_policy(project)?
                .is_some_and(|grant| !grant.revoked);
            let seated = match config.current_session_id {
                Some(seat) => self.get_session(seat)?.is_some_and(|session| {
                    !matches!(
                        session.status,
                        rsi_common::types::SessionStatus::Archived
                            | rsi_common::types::SessionStatus::Deleted
                    )
                }),
                None => false,
            };
            return Ok(granted && seated);
        }
        if self
            .portfolio_chain_heads(project)?
            .iter()
            .any(|head| head.seat_root == manager && head.epoch == scope)
        {
            return Ok(true);
        }
        Ok(self.conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM manager_nodes n JOIN manager_node_scopes s ON s.node_id=n.id
             WHERE n.state='active' AND n.parent_node_id IS NOT NULL AND s.project_id=?1
               AND n.seat_root_session_id=?2 AND n.authority_epoch=?3)",
            params![project.to_string(), manager.to_string(), scope],
            |row| row.get(0),
        )?)
    }

    /// `manager_gone` or `older_than_days` for an open record, else `None`.
    fn manager_v2_decision_stale_reason(
        &self,
        ledger: (Uuid, Uuid, i64),
        created_at: &str,
        days: u32,
    ) -> Result<Option<&'static str>> {
        if !self.manager_v2_ledger_live(ledger.0, ledger.1, ledger.2)? {
            return Ok(Some("manager_gone"));
        }
        Ok(too_old(created_at, days).then_some("older_than_days"))
    }

    /// Add the board fields to one `decision` row built from `record`:
    /// `gate`, `gate_source`, `answerable_by`, `blocks`, `archived`, `age_seconds`,
    /// `stale`, `stale_reason`, `owner_manager_session_id` and `scope_version`.
    pub(crate) fn manager_v2_decorate_decision_row(
        &self,
        config: &HarnessManagerConfigV1,
        row: &mut Value,
        archived: bool,
        has_target: bool,
    ) -> Result<()> {
        let key = row["key"].as_str().unwrap_or_default().to_owned();
        let status = row["status"].as_str().unwrap_or_default().to_owned();
        let gate = classify_decision(&key, row, has_target);
        let pending = status == "pending" && !archived;
        let blocking = matches!(status.as_str(), "pending" | "answer_queued") && !archived;
        row["gate"] = json!(gate.map(|g| g.class));
        row["gate_source"] = json!(gate.map(|g| g.source));
        row["answerable_by"] = match (pending, gate) {
            (false, _) => Value::Null,
            (true, Some(_)) => json!("operator"),
            (true, None) => json!("manager"),
        };
        row["blocks"] = json!({
            "launches": blocking,
            "epic_ids": if blocking { vec![row["epic_id"].clone()] } else { Vec::new() },
            "work_key": row["work_key"],
            "request_id": row["request_id"],
            "reason": "manager_v2_pending_operator_decision",
        });
        row["archived"] = json!(archived);
        row["owner_manager_session_id"] = json!(config.manager_session_id);
        row["scope_version"] = json!(config.row_version);
        let created = row["created_at"].as_str().unwrap_or_default().to_owned();
        row["age_seconds"] = json!(age_seconds(&created));
        let reason = if pending {
            self.manager_v2_decision_stale_reason(
                (
                    config.project_id,
                    config.manager_session_id,
                    config.row_version,
                ),
                &created,
                MANAGER_DECISION_STALE_DAYS_DEFAULT,
            )?
        } else {
            None
        };
        row["stale"] = json!(reason.is_some());
        row["stale_reason"] = json!(reason);
        row["stale_after_days"] = json!(MANAGER_DECISION_STALE_DAYS_DEFAULT);
        Ok(())
    }

    /// A human-gate row the daemon projects without a decision record (a
    /// legacy approval, an unresolved question): operator-only.
    pub(crate) fn manager_v2_mark_operator_gate_row(row: &mut Value) {
        row["gate"] = json!("human_approval");
        row["gate_source"] = json!("reserved_key");
        row["answerable_by"] = json!("operator");
    }

    /// The ledger a settlement acts on and the caller's relation to it.
    fn manager_v2_decision_owner(
        &self,
        a: &ManagerAuthorityV2,
        owner: Option<Uuid>,
        ruling: bool,
    ) -> Result<(HarnessManagerConfigV1, &'static str)> {
        let own = &a.config;
        let Some(id) = owner.filter(|id| *id != own.manager_session_id) else {
            return Ok((own.clone(), "self"));
        };
        if !ruling {
            // Only the owning manager withdraws its own record.
            return Err(refused("manager_v2_decision_owner_denied"));
        }
        let project = own.project_id;
        let (owner_config, relation) = if let Some(pm) = self
            .get_harness_manager(project)?
            .filter(|pm| pm.manager_session_id == id)
        {
            (pm, "project_manager")
        } else if let Some(head) = self
            .portfolio_chain_heads(project)?
            .into_iter()
            .find(|head| head.seat_root == id)
        {
            let mut config = ledger_config(project, head.seat_root, head.epoch);
            config.epic_ids = self.global_project_epics(project)?;
            (config, "portfolio_manager")
        } else {
            return Err(refused("manager_v2_decision_owner_unknown"));
        };
        // Only a portfolio seat strictly above the owning ledger may rule.
        let (ancestors, _) = self.portfolio_ancestors_of(&owner_config)?;
        if !ancestors
            .iter()
            .any(|head| head.seat_root == own.manager_session_id && head.epoch == own.row_version)
        {
            return Err(refused("manager_v2_decision_owner_denied"));
        }
        Ok((owner_config, relation))
    }

    /// Resolve who may settle which record. `admission` stops after the checks
    /// that cannot change under a retry (authority, ledger, Epic scope), so a
    /// replay of an already settled request still reaches its receipt; the
    /// state checks (version, digest, gate, target) run at commit.
    fn manager_v2_resolve_settlement(
        &self,
        a: &ManagerAuthorityV2,
        change: &ManagerUpdateV2,
        admission: bool,
    ) -> Result<Settlement> {
        if !a.is_manager {
            return Err(refused("manager_v2_manager_required"));
        }
        let (key, expected, owner, ruling) = match change {
            ManagerUpdateV2::DecisionRuling {
                key,
                expected_row_version,
                owner_manager_session_id,
                ..
            } => (key, *expected_row_version, *owner_manager_session_id, true),
            ManagerUpdateV2::DecisionWithdraw {
                key,
                expected_row_version,
                ..
            } => (key, *expected_row_version, None, false),
            _ => return Err(refused("manager_v2_invalid_update")),
        };
        // WorkPlan area managers may post and withdraw their own questions,
        // but ruling authority belongs to the PM and portfolio seats only.
        let portfolio =
            self.is_global_principal_anchor(a.config.project_id, a.config.manager_session_id)?;
        if ruling && !portfolio {
            let pm = self.get_harness_manager(a.config.project_id)?;
            if !pm.is_some_and(|pm| {
                pm.manager_session_id == a.config.manager_session_id
                    && pm.row_version == a.config.row_version
            }) {
                return Err(refused("manager_v2_decision_owner_denied"));
            }
        }
        // A portfolio seat acts only in Execute mode and unpaused; a Status
        // mode seat reads.
        if portfolio
            && !(a.grant.policy.mode == ManagerOperatingModeV2::Execute && !a.grant.policy.paused)
        {
            return Err(refused("manager_v2_capability_denied"));
        }
        let (owner_config, relation) = self.manager_v2_decision_owner(a, owner, ruling)?;
        let record = self
            .manager_v2_record(&owner_config, "decision", key)?
            .ok_or_else(|| refused("manager_v2_decision_missing"))?;
        let epic = record
            .epic_id
            .ok_or_else(|| refused("manager_v2_decision_target_required"))?;
        if !owner_config.epic_ids.contains(&epic) {
            return Err(refused("manager_v2_scope_denied"));
        }
        if admission {
            return Ok(Settlement {
                owner: owner_config,
                record,
                epic,
                relation,
            });
        }
        if record.archived {
            return Err(refused("manager_v2_decision_archived"));
        }
        if record.row_version != expected || record.payload["status"] != "pending" {
            return Err(refused("manager_v2_decision_changed"));
        }
        let has_target = self
            .manager_v2_record(&owner_config, "decision_target", key)?
            .is_some();
        let gate = classify_decision(key, &record.payload, has_target);
        if let ManagerUpdateV2::DecisionRuling { target_digest, .. } = change {
            if record.payload["target_digest"] != *target_digest {
                return Err(refused("manager_v2_decision_changed"));
            }
            if gate.is_some() {
                return Err(refused("manager_v2_decision_operator_gate"));
            }
            self.manager_v2_check_plain_decision_target(&owner_config, epic, &record.payload)?;
        } else if gate.is_some_and(|gate| gate.source == "reserved_key") {
            // Daemon-created gates are settled by their own flow.
            return Err(refused("manager_v2_reserved_decision_key"));
        }
        Ok(Settlement {
            owner: owner_config,
            record,
            epic,
            relation,
        })
    }

    /// Admission check for a ruling or withdrawal (read-only, retry-stable).
    pub(crate) fn manager_v2_check_decision_settlement(
        &self,
        a: &ManagerAuthorityV2,
        change: &ManagerUpdateV2,
    ) -> Result<()> {
        self.manager_v2_resolve_settlement(a, change, true)
            .map(|_| ())
    }

    /// The work and request a plain manager question names must be exactly as
    /// the asker saw them, or the answer targets something that has moved.
    pub(crate) fn manager_v2_check_plain_decision_target(
        &self,
        config: &HarnessManagerConfigV1,
        epic: Uuid,
        decision: &Value,
    ) -> Result<()> {
        if let Some(work_key) = decision["work_key"].as_str() {
            let work = self
                .manager_v2_record(config, "work", work_key)?
                .ok_or_else(|| refused("manager_v2_decision_target_changed"))?;
            if work.epic_id != Some(epic)
                || Some(work.row_version) != decision["target_row_version"].as_i64()
            {
                return Err(refused("manager_v2_decision_target_changed"));
            }
        }
        if let Some(request_id) = decision["request_id"].as_str() {
            let id =
                Uuid::parse_str(request_id).map_err(|_| refused("manager_v2_request_changed"))?;
            if !self.manager_v2_operator_request_live(config, epic, id)? {
                return Err(refused("manager_v2_request_changed"));
            }
            let version = self
                .manager_v2_record(config, "request", request_id)?
                .map_or(0, |r| r.row_version);
            if Some(version) != decision["request_row_version"].as_i64() {
                return Err(refused("manager_v2_request_changed"));
            }
        }
        Ok(())
    }

    /// Commit a ruling or withdrawal: the record flips in its owner's ledger,
    /// the receipt is the caller's (so a retry replays), the audit trail names
    /// who settled it.
    pub(crate) fn manager_v2_commit_decision_settlement(
        &self,
        caller: Uuid,
        request: &AgentManagerUpdateRequestV2,
    ) -> Result<ManagerMutationReceiptV2> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let context = self.manager_v2_prepare_update(caller, request)?;
        let a = &context.authority;
        let payload = json!({"actor":caller,"request":request});
        if let Some(receipt) =
            self.manager_v2_replay(&a.config, &request.idempotency_key, &payload)?
        {
            tx.commit()?;
            return Ok(serde_json::from_value(receipt)?);
        }
        let settlement = self.manager_v2_resolve_settlement(a, &request.change, false)?;
        let actor = self.manager_v2_decision_actor(a)?;
        let mut decision = settlement.record.payload.clone();
        let (event, key) = match &request.change {
            ManagerUpdateV2::DecisionRuling { key, answer, .. } => {
                decision["status"] = json!("answered");
                decision["answer"] = json!(answer);
                decision["delivery"] = json!({
                    "state":"available_in_scoped_inbox","actor":"manager",
                    "actor_kind":actor["kind"],"request_id":decision["request_id"],
                    "work_key":decision["work_key"],"target_digest":decision["target_digest"]
                });
                decision["answered_by"] = actor.clone();
                push_history(&mut decision, "ruled", &actor, Some(settlement.relation));
                ("decision_ruling", key)
            }
            ManagerUpdateV2::DecisionWithdraw { key, reason, .. } => {
                decision["status"] = json!("withdrawn");
                push_history(&mut decision, "withdrawn", &actor, Some(reason));
                ("decision_withdrawn", key)
            }
            _ => return Err(refused("manager_v2_invalid_update")),
        };
        let updated = self.manager_v2_put_record(
            &settlement.owner,
            "decision",
            key,
            Some(settlement.epic),
            settlement.record.row_version,
            &decision,
        )?;
        let sequence = self.manager_v2_event(
            &settlement.owner,
            Some(caller),
            event,
            key,
            updated.row_version,
            &decision,
        )?;
        let receipt = ManagerMutationReceiptV2 {
            event_sequence: sequence,
            key: key.clone(),
            row_version: updated.row_version,
            deduplicated: false,
        };
        self.manager_v2_save_receipt(
            &a.config,
            Some(caller),
            a.grant.row_version,
            "ledger",
            &request.idempotency_key,
            &payload,
            &serde_json::to_value(&receipt)?,
        )?;
        tx.commit()?;
        Ok(receipt)
    }

    /// `rulings` inspection page: the caller's pending non-gate decision
    /// records, in its own ledger and in every ledger below it.
    pub(crate) fn manager_v2_rulings_rows(
        &self,
        config: &HarnessManagerConfigV1,
        epic: Option<Uuid>,
        limit: usize,
    ) -> Result<(Vec<Value>, bool)> {
        let limit = limit.clamp(1, RULINGS_PAGE);
        let mut owners = vec![(config.clone(), "self")];
        let heads = self.portfolio_chain_heads(config.project_id)?;
        if let Some(position) = heads
            .iter()
            .position(|h| h.seat_root == config.manager_session_id && h.epoch == config.row_version)
        {
            for head in &heads[position + 1..] {
                let mut below = ledger_config(config.project_id, head.seat_root, head.epoch);
                below.epic_ids = self.global_project_epics(config.project_id)?;
                owners.push((below, "portfolio_manager"));
            }
            if let Some(pm) = self.get_harness_manager(config.project_id)? {
                owners.push((pm, "project_manager"));
            }
        }
        let mut rows = Vec::new();
        for (owner, relation) in owners {
            let mut statement = self.conn.prepare(
                "SELECT record_key FROM harness_manager_v2_records
                 WHERE project_id=?1 AND manager_session_id=?2 AND scope_version=?3
                   AND kind='decision' AND archived=0
                   AND json_extract(payload_json,'$.status')='pending'
                   AND (?4 IS NULL OR epic_id=?4)
                 ORDER BY created_at,record_key LIMIT ?5",
            )?;
            let keys = statement
                .query_map(
                    params![
                        owner.project_id.to_string(),
                        owner.manager_session_id.to_string(),
                        owner.row_version,
                        epic.map(|id| id.to_string()),
                        i64::try_from(limit + 1).unwrap_or(i64::MAX),
                    ],
                    |row| row.get::<_, String>(0),
                )?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            drop(statement);
            for key in keys {
                let Some(record) = self.manager_v2_record(&owner, "decision", &key)? else {
                    continue;
                };
                let has_target = self
                    .manager_v2_record(&owner, "decision_target", &key)?
                    .is_some();
                if classify_decision(&key, &record.payload, has_target).is_some()
                    || record.epic_id.is_some_and(|e| !owner.epic_ids.contains(&e))
                {
                    continue;
                }
                let archived = record.archived;
                let mut row = record_row(record);
                self.manager_v2_decorate_decision_row(&owner, &mut row, archived, has_target)?;
                row["owner_role"] = json!(relation);
                rows.push(row);
            }
        }
        rows.sort_by(|a, b| {
            (a["created_at"].as_str(), a["key"].as_str())
                .cmp(&(b["created_at"].as_str(), b["key"].as_str()))
        });
        let complete = rows.len() <= limit;
        rows.truncate(limit);
        Ok((rows, complete))
    }

    /// Operator: the pending decision records that are stale.
    pub fn manager_v2_list_stale_decisions(
        &self,
        request: &ListStaleManagerDecisionsRequestV2,
    ) -> Result<ListStaleManagerDecisionsResponseV2> {
        request.validate().map_err(refused)?;
        let days = request
            .older_than_days
            .unwrap_or(MANAGER_DECISION_STALE_DAYS_DEFAULT);
        let limit = usize::from(
            request
                .limit
                .unwrap_or(u16::try_from(MANAGER_DECISION_STALE_PAGE).unwrap_or(128)),
        )
        .min(MANAGER_DECISION_STALE_PAGE);
        let mut statement = self.conn.prepare(
            "SELECT project_id,manager_session_id,scope_version,record_key,row_version,epic_id,
                    payload_json,created_at
             FROM harness_manager_v2_records
             WHERE kind='decision' AND archived=0
               AND json_extract(payload_json,'$.status')='pending'
               AND (?1 IS NULL OR project_id=?1)
             ORDER BY created_at,project_id,manager_session_id,scope_version,record_key LIMIT ?2",
        )?;
        let scanned = statement
            .query_map(
                params![request.project_id.map(|id| id.to_string()), STALE_SCAN + 1],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, i64>(2)?,
                        row.get::<_, String>(3)?,
                        row.get::<_, i64>(4)?,
                        row.get::<_, Option<String>>(5)?,
                        row.get::<_, String>(6)?,
                        row.get::<_, String>(7)?,
                    ))
                },
            )?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        drop(statement);
        let mut complete = i64::try_from(scanned.len()).unwrap_or(i64::MAX) <= STALE_SCAN;
        let mut rows = Vec::new();
        for (project, manager, scope, key, version, epic, payload, created) in scanned
            .into_iter()
            .take(usize::try_from(STALE_SCAN).unwrap_or(2048))
        {
            let (project, manager) = (parse_id(&project)?, parse_id(&manager)?);
            let Some(reason) =
                self.manager_v2_decision_stale_reason((project, manager, scope), &created, days)?
            else {
                continue;
            };
            if rows.len() >= limit {
                complete = false;
                break;
            }
            let payload: Value = serde_json::from_str(&payload)?;
            let config = ledger_config(project, manager, scope);
            let has_target = self
                .manager_v2_record(&config, "decision_target", &key)?
                .is_some();
            let gate = classify_decision(&key, &payload, has_target);
            rows.push(StaleManagerDecisionV2 {
                decision: ManagerDecisionRefV2 {
                    project_id: project,
                    owner_manager_session_id: manager,
                    scope_version: scope,
                    key,
                    expected_row_version: version,
                },
                epic_id: epic.as_deref().map(parse_id).transpose()?,
                question: payload["question"].as_str().unwrap_or_default().to_owned(),
                status: payload["status"].as_str().unwrap_or_default().to_owned(),
                answerable_by: if gate.is_some() {
                    "operator"
                } else {
                    "manager"
                }
                .into(),
                gate: gate.map(|g| g.class.to_owned()),
                created_at: created.clone(),
                age_seconds: age_seconds(&created),
                stale_reason: reason.into(),
            });
        }
        Ok(ListStaleManagerDecisionsResponseV2 {
            older_than_days: days,
            rows,
            complete,
        })
    }

    /// Operator: archive stale decision records. Each item is re-checked: its
    /// row version still matches, it is still pending and it is still stale.
    /// Nothing is deleted; an archived record keeps its payload and history.
    pub fn manager_v2_archive_stale_decisions(
        &self,
        request: &ArchiveStaleManagerDecisionsRequestV2,
    ) -> Result<ArchiveStaleManagerDecisionsResponseV2> {
        request.validate().map_err(refused)?;
        let days = request
            .older_than_days
            .unwrap_or(MANAGER_DECISION_STALE_DAYS_DEFAULT);
        let mut archived = Vec::new();
        let mut skipped = Vec::new();
        for item in &request.items {
            let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
            let config = ledger_config(
                item.project_id,
                item.owner_manager_session_id,
                item.scope_version,
            );
            let skip = |reason: &str| SkippedManagerDecisionV2 {
                decision: item.clone(),
                reason: reason.into(),
            };
            let Some(record) = self.manager_v2_record(&config, "decision", &item.key)? else {
                skipped.push(skip("missing"));
                continue;
            };
            if record.archived {
                skipped.push(skip("already_archived"));
                continue;
            }
            if record.row_version != item.expected_row_version {
                skipped.push(skip("changed"));
                continue;
            }
            if record.payload["status"] != "pending" {
                skipped.push(skip("not_open"));
                continue;
            }
            let ledger = (
                item.project_id,
                item.owner_manager_session_id,
                item.scope_version,
            );
            if self
                .manager_v2_decision_stale_reason(ledger, &record.created_at, days)?
                .is_none()
            {
                skipped.push(skip("not_stale"));
                continue;
            }
            let actor = operator_actor();
            let mut decision = record.payload.clone();
            decision["status"] = json!("archived");
            push_history(
                &mut decision,
                "archived",
                &actor,
                Some("stale decision archived by the operator"),
            );
            let updated = self.manager_v2_put_record(
                &config,
                "decision",
                &item.key,
                record.epic_id,
                record.row_version,
                &decision,
            )?;
            self.conn.execute(
                "UPDATE harness_manager_v2_records SET archived=1
                 WHERE project_id=?1 AND manager_session_id=?2 AND scope_version=?3
                   AND kind='decision' AND record_key=?4",
                params![
                    item.project_id.to_string(),
                    item.owner_manager_session_id.to_string(),
                    item.scope_version,
                    item.key
                ],
            )?;
            self.manager_v2_event(
                &config,
                None,
                "decision_archived",
                &item.key,
                updated.row_version,
                &decision,
            )?;
            tx.commit()?;
            archived.push(ManagerDecisionRefV2 {
                expected_row_version: updated.row_version,
                ..item.clone()
            });
        }
        Ok(ArchiveStaleManagerDecisionsResponseV2 { archived, skipped })
    }

    /// A pending-record lookup the tests and the TUI share: the exact record.
    pub fn manager_v2_decision_record(
        &self,
        project: Uuid,
        manager: Uuid,
        scope: i64,
        key: &str,
    ) -> Result<Option<ManagerRecordV2>> {
        self.manager_v2_record(&ledger_config(project, manager, scope), "decision", key)
    }
}

fn parse_id(raw: &str) -> Result<Uuid> {
    Uuid::parse_str(raw).map_err(|_| refused("manager_v2_invalid_stored_identity"))
}

#[cfg(test)]
mod tests;
