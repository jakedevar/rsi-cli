//! Operator adoption of legacy scratch (#1147) RPC handlers.
//!
//! `ListLegacyScratch` and `AdoptLegacyScratch` are operator-only: they are not
//! declared in the attributed verb registry, so a tokened caller is
//! default-denied (AGENTS.md rule 10). Adopting records a directory so the
//! existing reclaim pass may delete it later; it deletes nothing itself.

use std::path::PathBuf;
use std::time::SystemTime;

use super::{RpcRequest, RpcServer};
use crate::agent_scratch_reclaim::{self, ScratchConfig};
use crate::error::{DaemonError, Result};
use rsi_common::scratch_adopt::{
    AdoptLegacyScratchRequestV1, AdoptLegacyScratchResponseV1, AdoptLegacyScratchResultV1,
    LegacyScratchCandidateV1, ListLegacyScratchResponseV1, SCRATCH_ADOPT_INVALID_REQUEST,
};

fn invalid() -> DaemonError {
    DaemonError::InvalidParam(SCRATCH_ADOPT_INVALID_REQUEST.into())
}

/// The listing for `config`, mapped to the wire type.
pub(crate) fn list_response(
    config: &ScratchConfig,
    now: SystemTime,
) -> ListLegacyScratchResponseV1 {
    let listing = agent_scratch_reclaim::list_legacy(config, now);
    ListLegacyScratchResponseV1 {
        candidates: listing
            .candidates
            .into_iter()
            .map(|c| LegacyScratchCandidateV1 {
                path: c.path.display().to_string(),
                kind: c.kind.to_string(),
                blocker: c.blocker,
                bytes: c.bytes,
                detail: c.detail,
            })
            .collect(),
        refused_roots: listing.refused_roots,
        budget_exhausted: listing.budget_exhausted,
    }
}

/// Adopt `request.paths` under `config`, mapped to the wire type.
pub(crate) fn adopt_response(
    config: &ScratchConfig,
    now: SystemTime,
    request: &AdoptLegacyScratchRequestV1,
) -> AdoptLegacyScratchResponseV1 {
    let paths: Vec<PathBuf> = request.paths.iter().map(PathBuf::from).collect();
    let results = agent_scratch_reclaim::adopt_legacy(config, now, &paths)
        .into_iter()
        .map(|outcome| AdoptLegacyScratchResultV1 {
            path: outcome.path.display().to_string(),
            adopted: outcome.adopted,
            refusal: outcome.refusal,
            detail: outcome.detail,
        })
        .collect();
    AdoptLegacyScratchResponseV1 { results }
}

impl RpcServer {
    pub(super) async fn handle_list_legacy_scratch(
        &self,
        request: &RpcRequest,
    ) -> Result<serde_json::Value> {
        if !request.params.is_null() && request.params != serde_json::json!({}) {
            return Err(invalid());
        }
        // The proof walks /proc and the trees: keep it off the async workers.
        let response = tokio::task::spawn_blocking(|| {
            list_response(&ScratchConfig::standard(), SystemTime::now())
        })
        .await
        .map_err(|_| DaemonError::Process("legacy scratch listing task failed".into()))?;
        Ok(serde_json::to_value(response)?)
    }

    pub(super) async fn handle_adopt_legacy_scratch(
        &self,
        request: &RpcRequest,
    ) -> Result<serde_json::Value> {
        let params: AdoptLegacyScratchRequestV1 =
            serde_json::from_value(request.params.clone()).map_err(|_| invalid())?;
        params.validate().map_err(|_| invalid())?;
        let response = tokio::task::spawn_blocking(move || {
            adopt_response(&ScratchConfig::standard(), SystemTime::now(), &params)
        })
        .await
        .map_err(|_| DaemonError::Process("legacy scratch adoption task failed".into()))?;
        for result in &response.results {
            tracing::info!(
                path = %result.path,
                adopted = result.adopted,
                refusal = ?result.refusal,
                "operator legacy scratch adoption"
            );
        }
        Ok(serde_json::to_value(response)?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent_scratch_reclaim::RootKind;
    use rsi_common::scratch_adopt::ScratchAdoptRefusal;
    use std::os::unix::fs::PermissionsExt;
    use std::time::Duration;

    fn config(
        root: &std::path::Path,
        registry: &std::path::Path,
        proc: &std::path::Path,
    ) -> ScratchConfig {
        std::fs::set_permissions(registry, std::fs::Permissions::from_mode(0o700)).unwrap();
        // Hermetic process view: an empty proc (nothing holds anything), so
        // this test does not depend on what else runs on the host.
        ScratchConfig {
            roots: vec![(root.to_path_buf(), RootKind::VarTmp)],
            registry: registry.to_path_buf(),
            proc_root: proc.to_path_buf(),
            ..ScratchConfig::standard()
        }
    }

    fn make_old(dir: &std::path::Path) {
        let old = SystemTime::now() - Duration::from_secs(10 * 24 * 3600);
        for entry in std::fs::read_dir(dir).unwrap() {
            std::fs::File::open(entry.unwrap().path())
                .unwrap()
                .set_modified(old)
                .unwrap();
        }
        std::fs::File::open(dir).unwrap().set_modified(old).unwrap();
    }

    // The operator wire shapes: list shows the unrecorded directory as
    // adoptable, adopting records it (nothing is deleted), a second adoption
    // and an out-of-root path get typed refusals.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn list_then_adopt_maps_the_core_verdicts_onto_the_wire_types() {
        let root = tempfile::tempdir().unwrap();
        let registry = tempfile::tempdir().unwrap();
        let proc = tempfile::tempdir().unwrap();
        let config = config(root.path(), registry.path(), proc.path());
        let legacy = root.path().join("rsi-wire-legacy");
        std::fs::create_dir(&legacy).unwrap();
        std::fs::write(legacy.join("data.bin"), b"payload").unwrap();
        make_old(&legacy);

        let listed = list_response(&config, SystemTime::now());
        assert_eq!(listed.candidates.len(), 1, "{listed:?}");
        assert_eq!(listed.candidates[0].path, legacy.display().to_string());
        assert_eq!(listed.candidates[0].kind, "var_tmp");
        assert_eq!(listed.candidates[0].blocker, None);

        let request = AdoptLegacyScratchRequestV1 {
            paths: vec![
                legacy.display().to_string(),
                legacy.display().to_string(),
                "/etc".to_string(),
            ],
        };
        let adopted = adopt_response(&config, SystemTime::now(), &request);
        let outcomes: Vec<_> = adopted
            .results
            .iter()
            .map(|r| (r.adopted, r.refusal))
            .collect();
        assert_eq!(
            outcomes,
            vec![
                (true, None),
                (false, Some(ScratchAdoptRefusal::AlreadyRecorded)),
                (false, Some(ScratchAdoptRefusal::OutsideRoots)),
            ]
        );
        assert!(legacy.join("data.bin").is_file(), "adopt deletes nothing");
        assert!(
            list_response(&config, SystemTime::now())
                .candidates
                .is_empty()
        );
    }
}
