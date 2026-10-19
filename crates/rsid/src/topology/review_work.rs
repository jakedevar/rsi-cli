//! #2607: a topology execution owns the review Work rows it creates, end to
//! end.
//!
//! Each review node opens a `topology:<execution>:<node>` Work row on the
//! project manager's ledger. No manager plans those rows, so nothing but the
//! executor can finish them: a row whose accepted source reached `rolling`
//! records integration at the landed tip (when the land node publishes, and
//! again when the execution settles), and a row of an execution that ended
//! without landing it is retired. Every pass re-derives the answer from the
//! ledger rows and git (`landed_source_tip`), so it is idempotent and a crash
//! between the land and the settlement only delays it to the next pass.

use std::collections::HashMap;
use std::path::PathBuf;

use uuid::Uuid;

use crate::error::Result;
use crate::store::manager_reviews::TopologyReviewWorkSettled;
use crate::topology::executor::{Executor, NodeEffects};

/// What one pass did to an execution's review Work rows.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ReviewWorkReport {
    pub(crate) integrated: usize,
    pub(crate) retired: usize,
    /// Rows still open after the pass (a running execution's unlanded rows).
    pub(crate) open: usize,
}

impl<E: NodeEffects> Executor<E> {
    /// Settle the review Work rows of `execution_id`. `execution_settled`:
    /// the execution reached a terminal status, so a row that did not land is
    /// retired instead of left open.
    ///
    /// # Errors
    /// A store error (no live manager ledger, a row out of scope); the caller
    /// asks again on its next pass.
    pub(crate) async fn settle_review_work(
        &self,
        execution_id: Uuid,
        repo_root: PathBuf,
        execution_settled: bool,
    ) -> Result<ReviewWorkReport> {
        let open = self
            .store
            .lock()
            .await
            .topology_open_review_work(execution_id)?;
        let mut report = ReviewWorkReport::default();
        let mut tips: HashMap<String, Option<String>> = HashMap::new();
        for work in open {
            let tip = match &work.source_commit {
                Some(source) => {
                    if !tips.contains_key(source) {
                        let tip = self
                            .effects
                            .landed_source_tip(repo_root.clone(), source.clone())
                            .await;
                        tips.insert(source.clone(), tip);
                    }
                    tips[source].clone()
                }
                None => None,
            };
            let settled = self.store.lock().await.topology_settle_review_work(
                execution_id,
                &work,
                tip.as_deref(),
                execution_settled,
            )?;
            match settled {
                TopologyReviewWorkSettled::Integrated => report.integrated += 1,
                TopologyReviewWorkSettled::Retired => report.retired += 1,
                TopologyReviewWorkSettled::Open => report.open += 1,
            }
        }
        Ok(report)
    }
}
