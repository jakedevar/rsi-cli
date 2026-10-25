//! The read behind the bench post-run route audit (#3177).
//!
//! A benchmark session must run Harness-only, so every model invocation it
//! wrote has purpose `session.harness.turn` or `session.harness.compaction`.
//! The audit itself lives in `rsid` (`session::bench_route`); the store only
//! answers which purposes a session's invocations carry.

use super::Store;
use crate::error::Result;
use uuid::Uuid;

/// One distinct route of a session's model invocations (#3935): the purpose and the
/// provider, model and backend it ran on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InvocationRoute {
    pub purpose: String,
    pub provider: Option<String>,
    pub model: Option<String>,
    pub backend: Option<String>,
}

impl Store {
    /// The distinct `model_invocations.purpose` values of `session_id`, sorted.
    /// Every row counts, admitted or not.
    pub fn session_model_invocation_purposes(&self, session_id: Uuid) -> Result<Vec<String>> {
        let mut stmt = self.conn.prepare(
            "SELECT DISTINCT purpose FROM model_invocations WHERE session_id = ?1 ORDER BY purpose",
        )?;
        let purposes = stmt
            .query_map([session_id.to_string()], |row| row.get::<_, String>(0))?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(purposes)
    }

    /// The distinct `(purpose, provider, model, backend)` routes of `session_id`'s
    /// `model_invocations`, sorted. Every row counts, admitted or not. The bench route audit
    /// (#3935) compares a resume's route with the launch's.
    pub fn session_model_invocation_routes(
        &self,
        session_id: Uuid,
    ) -> Result<Vec<InvocationRoute>> {
        let mut stmt = self.conn.prepare(
            "SELECT DISTINCT purpose, provider, model, backend FROM model_invocations \
             WHERE session_id = ?1 ORDER BY purpose, provider, model, backend",
        )?;
        let routes = stmt
            .query_map([session_id.to_string()], |row| {
                Ok(InvocationRoute {
                    purpose: row.get(0)?,
                    provider: row.get(1)?,
                    model: row.get(2)?,
                    backend: row.get(3)?,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(routes)
    }

    /// Test fixture: like [`Self::insert_invocation_purpose_for_test`], with the route columns.
    #[cfg(any(test, feature = "test-seam"))]
    pub fn insert_invocation_route_for_test(
        &self,
        session_id: Uuid,
        n: usize,
        route: &InvocationRoute,
    ) -> Result<()> {
        self.conn.execute(
            "INSERT INTO model_invocations(id, purpose, invocation_kind, foreground,
                                           paid_risk, admission_status, status,
                                           provider, model, backend,
                                           trigger_source, session_id, created_at)
             VALUES(?1, ?2, 'session_lifecycle', 'foreground', 'paid_capable',
                    'admitted', 'completed', ?3, ?4, ?5, 'test', ?6, '2026-10-19T00:00:00Z')",
            rusqlite::params![
                format!("inv-route-{session_id}-{n}"),
                route.purpose,
                route.provider,
                route.model,
                route.backend,
                session_id.to_string()
            ],
        )?;
        Ok(())
    }

    /// Test fixture: one invocation row of `session_id` with `purpose`, written
    /// without admission. `n` keeps the row ids distinct within a store.
    #[cfg(any(test, feature = "test-seam"))]
    pub fn insert_invocation_purpose_for_test(
        &self,
        session_id: Uuid,
        n: usize,
        purpose: &str,
    ) -> Result<()> {
        self.conn.execute(
            "INSERT INTO model_invocations(id, purpose, invocation_kind, foreground,
                                           paid_risk, admission_status, status,
                                           trigger_source, session_id, created_at)
             VALUES(?1, ?2, 'session_lifecycle', 'foreground', 'paid_capable',
                    'admitted', 'completed', 'test', ?3, '2026-10-19T00:00:00Z')",
            rusqlite::params![
                format!("inv-{session_id}-{n}"),
                purpose,
                session_id.to_string()
            ],
        )?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
    #[test]
    fn a_session_reads_back_the_distinct_purposes_of_its_own_invocations() {
        let store = Store::open_in_memory().unwrap();
        let mine = Uuid::new_v4();
        let other = Uuid::new_v4();
        for (id, session) in [(mine, mine), (other, other)] {
            store
                .insert_session(&crate::test_support::test_session(
                    id,
                    std::path::PathBuf::from("/tmp/bench-route-audit"),
                ))
                .unwrap();
            assert!(
                store
                    .session_model_invocation_purposes(session)
                    .unwrap()
                    .is_empty()
            );
        }
        for (n, (purpose, session)) in [
            ("session.harness.turn", mine),
            ("session.harness.turn", mine),
            ("session.harness.compaction", mine),
            ("session.launch_fresh", other),
        ]
        .into_iter()
        .enumerate()
        {
            store
                .insert_invocation_purpose_for_test(session, n, purpose)
                .unwrap();
        }
        assert_eq!(
            store.session_model_invocation_purposes(mine).unwrap(),
            ["session.harness.compaction", "session.harness.turn"]
        );
        assert_eq!(
            store.session_model_invocation_purposes(other).unwrap(),
            ["session.launch_fresh"]
        );
        assert!(
            store
                .session_model_invocation_purposes(Uuid::new_v4())
                .unwrap()
                .is_empty()
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
    #[test]
    fn a_session_reads_back_the_distinct_routes_of_its_invocations() {
        let store = Store::open_in_memory().unwrap();
        let session = Uuid::new_v4();
        store
            .insert_session(&crate::test_support::test_session(
                session,
                std::path::PathBuf::from("/tmp/bench-route-audit"),
            ))
            .unwrap();
        let route = |purpose: &str, provider: &str| InvocationRoute {
            purpose: purpose.into(),
            provider: Some(provider.into()),
            model: Some("m".into()),
            backend: Some("b".into()),
        };
        for (n, r) in [
            route("session.launch.fresh", "OpenRouter"),
            route("session.continue.resume", "OpenRouter"),
            route("session.continue.resume", "OpenRouter"),
            route("session.title", "Codex"),
        ]
        .iter()
        .enumerate()
        {
            store
                .insert_invocation_route_for_test(session, n, r)
                .unwrap();
        }
        let routes = store.session_model_invocation_routes(session).unwrap();
        let purposes: Vec<_> = routes.iter().map(|r| r.purpose.as_str()).collect();
        assert_eq!(
            purposes,
            [
                "session.continue.resume",
                "session.launch.fresh",
                "session.title"
            ],
            "duplicate rows collapse"
        );
        assert_eq!(routes[2].provider.as_deref(), Some("Codex"));
        assert!(
            store
                .session_model_invocation_routes(Uuid::new_v4())
                .unwrap()
                .is_empty()
        );
    }
}
