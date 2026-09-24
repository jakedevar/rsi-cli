//! DB-native review receipt service. Filesystem custody is observed outside
//! the Store lock and revalidated transactionally before the immutable write.

use super::{agent_verbs::AgentControlHandle, manager_ledger::git};
use crate::{error::Result, store::harness_manager_v2::refused};
use rsi_common::harness_manager_v2::{AgentSubmitReviewReceiptRequestV1, ManagerReviewReceiptV1};
use std::path::Path;
use uuid::Uuid;

impl AgentControlHandle {
    pub async fn agent_submit_review_receipt(
        &self,
        caller: Uuid,
        request: AgentSubmitReviewReceiptRequestV1,
    ) -> Result<ManagerReviewReceiptV1> {
        request.validate().map_err(refused)?;
        {
            let store = self.store.lock().await;
            if let Some(receipt) = store.manager_review_receipt_replay(caller, &request)? {
                return Ok(receipt);
            }
            // The refresh can itself settle the assignment as failed (for
            // example receipt_missing); its durable notice is published even
            // though this submission is then refused.
            let failed_notice = if store.refresh_manager_review_assignment(request.assignment_id)? {
                store.manager_review_notice_job(request.assignment_id, "failed")?
            } else {
                None
            };
            drop(store);
            if let Some(job_id) = failed_notice {
                self.event_bus
                    .publish(crate::bus::DaemonEvent::ManagerNoticeQueued { job_id });
            }
        }
        let observed = self
            .store
            .lock()
            .await
            .prepare_manager_review_submission(caller, &request)?;
        git::custody(&observed.custody).await?;
        let root = Path::new(&observed.custody.sandbox_root);
        git::clean(root).await?;
        if git::head(root, "HEAD").await? != observed.source_sha {
            return Err(refused("manager_review_source_mismatch"));
        }
        let (receipt, notice_job) = {
            let store = self.store.lock().await;
            let receipt = store.commit_manager_review_submission(caller, &request, &observed)?;
            let notice_job = if receipt.deduplicated {
                None
            } else {
                store.manager_review_notice_job(receipt.assignment_id, "submitted")?
            };
            (receipt, notice_job)
        };
        if let Some(job_id) = notice_job {
            self.event_bus
                .publish(crate::bus::DaemonEvent::ManagerNoticeQueued { job_id });
        }
        Ok(receipt)
    }
}
