//! #2607: a topology execution owns the review Work rows it creates, end to
//! end.
//!
//! A review node's `topology:<execution>:<node>` Work row is created by the
//! executor (`manager_review_reserve_for_topology`); no manager ever plans it,
//! so no manager would ever record its integration or retire it. The executor
//! therefore settles it itself: a row whose accepted source the land node
//! published records integration (the same `Integration` record and Integration
//! stage a manager's `AgentManagerUpdate integration` writes, without a manager
//! authority because the executor is the actor, as for the reservation), and a
//! row whose execution ended without landing it is retired (archived history).
//! Both are idempotent and re-derived from the rows, never cached.

use super::*;
use crate::store::manager_ledger::Integration;

/// One live review Work row of a topology execution.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TopologyOpenReviewWork {
    pub project_id: Uuid,
    pub work_key: String,
    /// The commit the row was opened for; the only commit it can integrate.
    pub source_commit: Option<String>,
}

/// What settling one review Work row did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TopologyReviewWorkSettled {
    /// Integration was recorded at the landed tip.
    Integrated,
    /// The execution ended without landing the row: it is archived history.
    Retired,
    /// Nothing changed: the execution is still running and the row has not
    /// landed, or an earlier pass already settled it.
    Open,
}

impl Store {
    /// The review Work rows of `execution_id` that are neither integrated nor
    /// retired.
    pub fn topology_open_review_work(
        &self,
        execution_id: Uuid,
    ) -> Result<Vec<TopologyOpenReviewWork>> {
        let prefix = format!("topology:{execution_id}:");
        let mut statement = self.conn.prepare(
            "SELECT project_id,record_key,json_extract(payload_json,'$.source_commit')
               FROM harness_manager_v2_work_facts
              WHERE kind='work' AND archived=0
                AND substr(record_key,1,length(?1))=?1
                AND json_type(payload_json,'$.integration') IS NOT 'object'
              ORDER BY record_key",
        )?;
        let rows = statement
            .query_map([prefix], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, Option<String>>(2)?,
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        rows.into_iter()
            .map(|(project, work_key, source_commit)| {
                Ok(TopologyOpenReviewWork {
                    project_id: parse_uuid(project)?,
                    work_key,
                    source_commit,
                })
            })
            .collect()
    }

    /// Settle one review Work row of `execution_id`.
    ///
    /// `landed_tip` is the published tip when the row's source is an ancestor
    /// of `origin/rolling`. A landed row whose source has an admitted
    /// acceptance records integration at that tip. Otherwise, when
    /// `execution_settled`, the row is retired; while the execution still runs
    /// it stays open.
    ///
    /// # Errors
    /// The project has no live manager ledger, or the row is out of its scope;
    /// the caller asks again on its next pass.
    pub fn topology_settle_review_work(
        &self,
        execution_id: Uuid,
        work: &TopologyOpenReviewWork,
        landed_tip: Option<&str>,
        execution_settled: bool,
    ) -> Result<TopologyReviewWorkSettled> {
        if !work
            .work_key
            .starts_with(&format!("topology:{execution_id}:"))
        {
            return Err(refused("topology_review_work_not_owned"));
        }
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let config = self
            .get_harness_manager(work.project_id)?
            .ok_or_else(|| refused(REVIEW_NO_MANAGER_LEDGER))?;
        let Some(record) = self.manager_v2_record(&config, "work", &work.work_key)? else {
            return Ok(TopologyReviewWorkSettled::Open);
        };
        let mut row: WorkRecord = decode(&record)?;
        if record.archived || row.integration.is_some() {
            return Ok(TopologyReviewWorkSettled::Open);
        }
        let actor = json!({"kind": "topology_executor", "execution_id": execution_id});
        if let (Some(tip), Some(source)) = (landed_tip, row.source_commit.clone())
            && canonical_sha(tip)
            && let Some(acceptance) = self.manager_v2_accepted_source(&config, &row, &source)?
        {
            row.acceptance = Some(acceptance);
            row.integration = Some(Integration {
                source_commit: source,
                target_commit: tip.to_owned(),
                verification: None,
                integrated_at: now(),
            });
            if let Some(stage) = row
                .stages
                .iter_mut()
                .find(|stage| stage.stage == ManagerWorkStageV2::Integration)
            {
                stage.state = ManagerStageStateV2::Passed;
                stage.admission = None;
                stage.updated_at = now();
            }
            let value = serde_json::to_value(&row)?;
            let put = self.manager_v2_put_record(
                &config,
                "work",
                &work.work_key,
                Some(row.epic_id),
                record.row_version,
                &value,
            )?;
            self.manager_v2_event(
                &config,
                None,
                "work",
                &work.work_key,
                put.row_version,
                &value,
            )?;
            self.manager_v2_event(
                &config,
                None,
                "topology_review_work_integrated",
                &work.work_key,
                put.row_version,
                &json!({"actor": actor, "work_key": work.work_key,
                    "source_commit": row.integration.as_ref().map(|i| &i.source_commit),
                    "target_commit": tip}),
            )?;
            tx.commit()?;
            return Ok(TopologyReviewWorkSettled::Integrated);
        }
        if !execution_settled {
            return Ok(TopologyReviewWorkSettled::Open);
        }
        let retired = self.conn.execute(
            "UPDATE harness_manager_v2_work_facts
                SET archived=1,row_version=row_version+1,updated_at=?3
              WHERE project_id=?1 AND kind='work' AND record_key=?2 AND archived=0
                AND json_type(payload_json,'$.integration') IS NOT 'object'",
            params![work.project_id.to_string(), work.work_key, now()],
        )?;
        if retired == 0 {
            return Ok(TopologyReviewWorkSettled::Open);
        }
        self.manager_v2_event(
            &config,
            None,
            "topology_review_work_retired",
            &work.work_key,
            record.row_version + 1,
            &json!({"actor": actor, "work_key": work.work_key,
                "source_commit": row.source_commit, "landed": false}),
        )?;
        tx.commit()?;
        Ok(TopologyReviewWorkSettled::Retired)
    }
}
