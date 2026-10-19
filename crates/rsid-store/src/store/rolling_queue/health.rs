//! #2743: whether `rolling` is known good, read for a project: its open `qa`
//! Issues (a full-library QA red, a red a gate saw on the base, incomplete QA
//! coverage) and the newest full-library-green SHA with its age.
//!
//! `AgentManagerOverview`, a successor's launch prompt and the
//! `AgentRequestDeploy` receipt all read this one snapshot, so a red `rolling`
//! reaches the seat by every path it chooses work or a deploy SHA on.

use super::*;
use rsi_common::rolling_queue::{ROLLING_HEALTH_MAX_QA_ISSUES, RollingHealthV1, RollingQaIssueV1};

const OPEN_QA_ISSUES_WHERE: &str = "project_id = ?1 \
     AND status IN ('Open', 'InProgress') \
     AND archived_at IS NULL \
     AND EXISTS(SELECT 1 FROM json_each(labels) label WHERE label.value = 'qa')";

impl Store {
    /// The project's open, non-archived `qa` Issues, most urgent first
    /// (priority number, then oldest), capped at `limit`, with the total.
    ///
    /// # Errors
    /// A persistence error.
    pub fn open_qa_issues(
        &self,
        project: Uuid,
        limit: usize,
    ) -> Result<(Vec<RollingQaIssueV1>, u32)> {
        let total: i64 = self.conn.query_row(
            &format!("SELECT count(*) FROM issues WHERE {OPEN_QA_ISSUES_WHERE}"),
            [project.to_string()],
            |row| row.get(0),
        )?;
        let mut statement = self.conn.prepare(&format!(
            "SELECT display_number, title, status, priority, created_at FROM issues \
             WHERE {OPEN_QA_ISSUES_WHERE} \
             ORDER BY COALESCE(priority, 99), display_number LIMIT ?2"
        ))?;
        let issues = statement
            .query_map(
                params![
                    project.to_string(),
                    i64::try_from(limit).unwrap_or(i64::MAX)
                ],
                |row| {
                    Ok(RollingQaIssueV1 {
                        number: row.get(0)?,
                        title: row.get(1)?,
                        status: row.get(2)?,
                        priority: row.get(3)?,
                        created_at: row.get(4)?,
                    })
                },
            )?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok((issues, u32::try_from(total).unwrap_or(u32::MAX)))
    }

    /// The newest `rolling` SHA a full-library QA run proved green among the
    /// project's queue entries (the same receipt `rolling_qa_green_for_agent`
    /// reads, without the caller-scope narrowing: the project is the scope).
    ///
    /// # Errors
    /// A persistence error.
    pub fn rolling_qa_green_for_project(&self, project: Uuid) -> Result<Option<RollingQaGreenV1>> {
        let found = self
            .conn
            .query_row(
                "SELECT json_extract(e.detail_json, '$.sha'), e.at, \
                        json_extract(e.detail_json, '$.job_id') \
                 FROM rolling_queue_events e \
                 JOIN rolling_queue_entries q ON q.id = e.entry_id \
                 WHERE e.kind = 'bounded_qa_finished' \
                   AND json_extract(e.detail_json, '$.passed') = 1 \
                   AND q.project_id = ?1 \
                 ORDER BY e.id DESC LIMIT 1",
                [project.to_string()],
                |row| {
                    Ok((
                        row.get::<_, Option<String>>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, Option<String>>(2)?,
                    ))
                },
            )
            .optional()?;
        Ok(found.and_then(|(sha, passed_at, job)| {
            Some(RollingQaGreenV1 {
                sha: sha?,
                passed_at,
                job_id: job.and_then(|job| Uuid::parse_str(&job).ok()),
            })
        }))
    }

    /// The project's rolling health at `now`.
    ///
    /// # Errors
    /// A persistence error.
    pub fn rolling_health_for_project(
        &self,
        project: Uuid,
        now: DateTime<Utc>,
    ) -> Result<RollingHealthV1> {
        let qa_green = self.rolling_qa_green_for_project(project)?;
        let qa_green_age_secs = qa_green.as_ref().and_then(|green| {
            DateTime::parse_from_rfc3339(&green.passed_at)
                .ok()
                .map(|at| (now - at.with_timezone(&Utc)).num_seconds().max(0))
        });
        let (open_qa_issues, open_qa_issues_total) =
            self.open_qa_issues(project, ROLLING_HEALTH_MAX_QA_ISSUES)?;
        Ok(RollingHealthV1 {
            qa_green,
            qa_green_age_secs,
            open_qa_issues,
            open_qa_issues_total,
        })
    }
}

#[cfg(test)]
mod tests {
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
    use super::*;
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
    use rsi_common::types::{NewIssue, Project};

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
    fn project(store: &Store) -> Uuid {
        let id = Uuid::new_v4();
        let now = Utc::now();
        store
            .insert_project(&Project {
                id,
                name: format!("health-fixture-{id}"),
                path: None,
                description: None,
                color: Project::DEFAULT_COLOR.into(),
                context_files: None,
                created_at: now,
                updated_at: now,
            })
            .unwrap();
        id
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
    fn issue(
        store: &Store,
        project: Uuid,
        title: &str,
        labels: &[&str],
        priority: u8,
    ) -> rsi_common::types::Issue {
        store
            .create_issue(&NewIssue {
                project_id: project,
                title: title.into(),
                body: String::new(),
                priority: Some(priority),
                labels: labels.iter().map(|label| (*label).to_string()).collect(),
                created_by_session_id: None,
                assignee: None,
                idea_id: None,
                source_event_id: None,
                source_finding_ref: None,
            })
            .unwrap()
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
    #[test]
    fn open_qa_issues_lists_only_this_projects_open_qa_issues_most_urgent_first() {
        let store = Store::open_in_memory().unwrap();
        let project = project(&store);
        let other = self::project(&store);
        let low = issue(&store, project, "qa low", &["qa", "bug"], 3).display_number;
        let urgent = issue(&store, project, "qa urgent", &["qa"], 1);
        issue(&store, project, "not qa", &["bug"], 1);
        issue(&store, other, "other project qa", &["qa"], 1);

        let (issues, total) = store.open_qa_issues(project, 8).unwrap();
        assert_eq!(total, 2);
        assert_eq!(
            issues.iter().map(|i| i.number).collect::<Vec<_>>(),
            vec![urgent.display_number, low]
        );
        let (capped, total) = store.open_qa_issues(project, 1).unwrap();
        assert_eq!((capped.len(), total), (1, 2), "the cap keeps the total");

        store
            .update_issue_status(urgent.id, rsi_common::types::IssueStatus::Closed)
            .unwrap();
        let (issues, total) = store.open_qa_issues(project, 8).unwrap();
        assert_eq!(
            (issues.len(), total),
            (1, 1),
            "a closed qa Issue is not open"
        );
    }
}
