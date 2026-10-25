//! Interfaces the production runner consumes from sibling conversions. Each
//! method names its owner. The staged default for every contract fails closed
//! with `incomplete_contract`: no sibling gate can be skipped, defaulted or
//! turned into an empty lane before its owner supplies the real implementation.
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};
use std::io;
use std::path::Path;

pub const INCOMPLETE: &str = "incomplete_contract";

#[derive(Clone, Debug)]
pub struct Incomplete {
    pub owner: &'static str,
    pub contract: &'static str,
    /// Why an otherwise-wired contract could not answer (empty when merely unpublished).
    pub detail: Option<String>,
}
pub fn gap(owner: &'static str, contract: &'static str) -> Incomplete {
    Incomplete {
        owner,
        contract,
        detail: None,
    }
}
impl Incomplete {
    pub fn value(&self) -> Value {
        json!({
            "status": INCOMPLETE,
            "owner": self.owner,
            "contract": self.contract,
            "detail": self.detail.clone().unwrap_or_else(|| format!("{} ({}) has not published its Python-free contract; failing closed", self.contract, self.owner)),
        })
    }
    pub fn error(&self) -> io::Error {
        io::Error::other(match &self.detail {
            Some(detail) => format!("{INCOMPLETE}: {} ({}): {detail}", self.contract, self.owner),
            None => format!(
                "{INCOMPLETE}: {} ({}) is not integrated",
                self.contract, self.owner
            ),
        })
    }
}

/// `scripts/check-touched-shards --filters-only --json` as the queue consumes it.
#[derive(Debug)]
pub struct Derivation {
    pub exit_code: i32,
    /// Exactly the JSON the legacy derivation printed (empty object when none).
    pub data: Value,
    /// Raw standard output, replayed verbatim on a failed derivation.
    pub stdout: String,
}
#[derive(Debug)]
pub struct SlowTier {
    pub package: String,
    pub tests: BTreeSet<String>,
}
pub enum Renumbered {
    /// The candidate declares no provisional migration.
    Absent,
    Done {
        source: String,
        target: String,
        old_version: u64,
        new_version: u64,
        candidate: String,
    },
    Refused {
        source: String,
        reason: String,
    },
}

/// #426 (derivation, inventories), #361 (migrations), #362 (guards) and #366
/// (residual suites). A method returning `Err` is an unfinished contract.
pub trait Contracts {
    fn derive(&mut self, repo: &Path, base: &str, head: &str) -> Result<Derivation, Incomplete>;
    fn shard_packages(&mut self, repo: &Path) -> Result<BTreeMap<String, Vec<String>>, Incomplete>;
    fn feature_test_inventory(
        &mut self,
        repo: &Path,
        head: &str,
        package: &str,
    ) -> Result<Vec<(String, Vec<String>)>, Incomplete>;
    fn load_slow_tier(&mut self, repo: &Path, head: &str) -> Result<Option<SlowTier>, Incomplete>;
    fn downstream_test_plan(
        &mut self,
        repo: &Path,
        base: &str,
        head: &str,
    ) -> Result<Value, Incomplete>;
    /// #361 renumber step for a candidate that declares a provisional migration.
    fn renumber(
        &mut self,
        repo: &Path,
        head: &str,
        rolling: &str,
    ) -> Result<Renumbered, Incomplete>;
    /// The named static gate that has no in-crate Rust implementation yet.
    fn guard(
        &mut self,
        name: &str,
        repo: &Path,
        base: &str,
        head: &str,
    ) -> Result<Value, Incomplete>;
    /// #366 residual suites; their verdict entries keep the receipt lane shape.
    fn residual(&mut self, repo: &Path, base: &str, head: &str) -> Result<Vec<Value>, Incomplete>;
}

pub struct Staged;
impl Contracts for Staged {
    fn derive(&mut self, _: &Path, _: &str, _: &str) -> Result<Derivation, Incomplete> {
        Err(gap("#426", "check-touched-shards derivation"))
    }
    fn shard_packages(&mut self, _: &Path) -> Result<BTreeMap<String, Vec<String>>, Incomplete> {
        Err(gap("#426", "shard ownership inventory"))
    }
    fn feature_test_inventory(
        &mut self,
        _: &Path,
        _: &str,
        _: &str,
    ) -> Result<Vec<(String, Vec<String>)>, Incomplete> {
        Err(gap("#426", "required-feature test inventory"))
    }
    fn load_slow_tier(&mut self, _: &Path, _: &str) -> Result<Option<SlowTier>, Incomplete> {
        Err(gap("#426", "slow-tier configuration"))
    }
    fn downstream_test_plan(&mut self, _: &Path, _: &str, _: &str) -> Result<Value, Incomplete> {
        Err(gap("#426", "downstream compile/test plan"))
    }
    fn renumber(&mut self, _: &Path, _: &str, _: &str) -> Result<Renumbered, Incomplete> {
        Err(gap("#361", "provisional migration renumber"))
    }
    fn guard(&mut self, name: &str, _: &Path, _: &str, _: &str) -> Result<Value, Incomplete> {
        Err(gap(
            match name {
                "released_migrations" => "#361",
                _ => "#362",
            },
            match name {
                "shard_gates" => "check-rsid-test-shards --require-gates",
                "released_migrations" => "check-released-migrations",
                "target_reclaim" => "check-target-reclaim",
                "guard_regressions" => "check-guard-regressions",
                "store_boundary" => "check-store-boundary",
                _ => "unknown static gate",
            },
        ))
    }
    fn residual(&mut self, _: &Path, _: &str, _: &str) -> Result<Vec<Value>, Incomplete> {
        Err(gap("#366", "residual suite selection and execution"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn every_staged_contract_fails_closed_naming_its_owner() {
        let mut staged = Staged;
        let repo = Path::new(".");
        assert_eq!(staged.derive(repo, "b", "h").unwrap_err().owner, "#426");
        assert_eq!(staged.shard_packages(repo).unwrap_err().owner, "#426");
        assert_eq!(
            staged
                .renumber(repo, "h", "r")
                .map(|_| ())
                .unwrap_err()
                .owner,
            "#361"
        );
        assert_eq!(
            staged
                .guard("released_migrations", repo, "b", "h")
                .unwrap_err()
                .owner,
            "#361"
        );
        for name in [
            "shard_gates",
            "target_reclaim",
            "guard_regressions",
            "store_boundary",
        ] {
            let error = staged.guard(name, repo, "b", "h").unwrap_err();
            assert_eq!(error.owner, "#362", "{name}");
            assert_eq!(error.value()["status"], INCOMPLETE);
        }
        assert_eq!(staged.residual(repo, "b", "h").unwrap_err().owner, "#366");
    }
}
