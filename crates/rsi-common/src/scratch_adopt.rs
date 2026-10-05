//! #1147: operator adoption of legacy scratch directories.
//!
//! #1140 reclaims only scratch directories that carry a creation record. These
//! are the operator-only wire types for listing the unrecorded ones and
//! adopting chosen ones (recording them so the existing reclaim pass may delete
//! them later). Adoption deletes nothing. The methods are deliberately absent
//! from the attributed verb registry: a session-token caller is denied
//! (AGENTS.md rule 10).

use serde::{Deserialize, Serialize};

pub const SCRATCH_ADOPT_INVALID_REQUEST: &str = "scratch_adopt_invalid_request";

/// Most paths one `AdoptLegacyScratch` call takes (one reclaim pass worth).
pub const MAX_ADOPT_PATHS: usize = 32;

/// Why a legacy directory was not (or could not be) adopted. The reclaim
/// proof's verdicts, minus provenance.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ScratchAdoptRefusal {
    /// Not a direct, allowlisted child of a configured scratch root.
    OutsideRoots,
    Missing,
    /// The final path component is a symlink.
    Symlink,
    NotDirectory,
    /// A process or thread holds it.
    Held,
    Young,
    DirtyWorktree,
    Unpublished,
    /// Not provable safe (inventory incomplete, mount or identity doubt, no
    /// birth time, root not authentic, budget).
    Unproven,
    /// It already has a bound creation record.
    AlreadyRecorded,
    Changed,
    /// Writing the registry entry or record failed.
    Failed,
}

/// One unrecorded directory and what an adoption would do now.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LegacyScratchCandidateV1 {
    pub path: String,
    /// `worker_tmp`, `var_tmp` or `lander`.
    pub kind: String,
    /// `None` when every proof passed (adoptable now).
    #[serde(default)]
    pub blocker: Option<ScratchAdoptRefusal>,
    pub bytes: u64,
    /// When the holder proof could not be completed: why, and the blocking
    /// processes (pid, comm, uid; kernel-reported, display only), one line.
    #[serde(default)]
    pub detail: Option<String>,
}

/// `ListLegacyScratch` response. The request takes no parameters.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ListLegacyScratchResponseV1 {
    pub candidates: Vec<LegacyScratchCandidateV1>,
    #[serde(default)]
    pub refused_roots: u32,
    #[serde(default)]
    pub budget_exhausted: bool,
}

/// `AdoptLegacyScratch`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AdoptLegacyScratchRequestV1 {
    /// Absolute paths, each a direct child of a scratch root.
    pub paths: Vec<String>,
}

impl AdoptLegacyScratchRequestV1 {
    /// # Errors
    /// `scratch_adopt_invalid_request` for no paths, too many, or a path that
    /// is not absolute.
    pub fn validate(&self) -> Result<(), &'static str> {
        if self.paths.is_empty()
            || self.paths.len() > MAX_ADOPT_PATHS
            || self.paths.iter().any(|path| !path.starts_with('/'))
        {
            return Err(SCRATCH_ADOPT_INVALID_REQUEST);
        }
        Ok(())
    }
}

/// The result for one requested path.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AdoptLegacyScratchResultV1 {
    pub path: String,
    pub adopted: bool,
    #[serde(default)]
    pub refusal: Option<ScratchAdoptRefusal>,
    /// As [`LegacyScratchCandidateV1::detail`].
    #[serde(default)]
    pub detail: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AdoptLegacyScratchResponseV1 {
    pub results: Vec<AdoptLegacyScratchResultV1>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_validation_bounds_the_batch_and_requires_absolute_paths() {
        let ok = |paths: Vec<String>| AdoptLegacyScratchRequestV1 { paths }.validate();
        assert_eq!(ok(vec!["/var/tmp/rsi-a".into()]), Ok(()));
        assert_eq!(ok(vec![]), Err(SCRATCH_ADOPT_INVALID_REQUEST));
        assert_eq!(ok(vec!["rsi-a".into()]), Err(SCRATCH_ADOPT_INVALID_REQUEST));
        assert_eq!(
            ok(vec!["/var/tmp/rsi-a".into(); MAX_ADOPT_PATHS + 1]),
            Err(SCRATCH_ADOPT_INVALID_REQUEST)
        );
    }

    #[test]
    fn refusals_use_snake_case_on_the_wire() {
        let text = serde_json::to_string(&ScratchAdoptRefusal::DirtyWorktree).unwrap();
        assert_eq!(text, "\"dirty_worktree\"");
        let back: ScratchAdoptRefusal = serde_json::from_str("\"outside_roots\"").unwrap();
        assert_eq!(back, ScratchAdoptRefusal::OutsideRoots);
    }
}
