//! Persistent green candidate-gate receipts (#1312).
//!
//! A landing whose candidate gate passed but whose publication failed (a lost
//! race, a refused push, a killed lander) discards its private workspace; the
//! next invocation used to run the whole gate again on the same content. The
//! lander now records the green verdict here, keyed by what the verdict
//! depends on: the candidate's tree, the base tip, the resolved gate (commands
//! and environment) and the toolchain/lander identity. A later invocation that
//! derives the same key reuses the verdict and runs nothing else differently:
//! the policy checks, the static guards and the fast-forward publication all
//! stay live.
//!
//! A receipt only ever stands in for a gate that ran in full and passed. Every
//! miss (no file, another key, an expired, future-dated or corrupt file) means
//! "run the gate", never an error.

use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const SCHEMA_VERSION: u8 = 1;
/// Longest a receipt stands in for a gate. The base tip, which the key names,
/// moves with every landing, so a receipt this old is already unreachable in
/// practice; the bound only keeps the directory and the trust window small.
pub(super) const MAX_AGE: Duration = Duration::from_secs(12 * 60 * 60);
/// Tolerated clock skew before a receipt dated in the future is refused.
const FUTURE_SKEW_SECS: u64 = 60;

/// The part of a green gate's state the landing report and the queue read
/// afterwards. Restoring it makes a reused verdict report what the original
/// run reported.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
pub(super) struct GateOutcome {
    pub base_reds: BTreeSet<String>,
    pub base_red_texts: BTreeMap<String, String>,
    pub base_red_commands: BTreeMap<String, String>,
    pub flakes: BTreeSet<String>,
    pub base_reused: BTreeSet<String>,
    pub local_base_confirmed: BTreeSet<String>,
    pub base_absent_packages: BTreeSet<String>,
    pub base_absent_targets: BTreeSet<String>,
    pub base_static_inventory_reds: BTreeSet<String>,
    pub disk_scratch_used: BTreeSet<String>,
    pub claimed_green: BTreeSet<String>,
    pub full_gate_secs: u64,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
struct Receipt {
    schema_version: u8,
    candidate_tree: String,
    key: String,
    created_unix: u64,
    outcome: GateOutcome,
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs())
}

fn hex40(value: &str) -> bool {
    value.len() == 40 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn hex64(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

pub(super) fn default_root() -> Result<PathBuf, String> {
    if let Some(path) = std::env::var_os("RSI_ROLLING_CANDIDATE_RECEIPT_DIR") {
        return Ok(PathBuf::from(path));
    }
    dirs::home_dir()
        .map(|home| home.join(".rsi/cache/rolling-candidate-receipts-v1"))
        .ok_or_else(|| "cannot locate home for rolling candidate receipts".into())
}

/// `sha256` hex of the receipt's identity text. The caller names every input
/// the verdict depends on; this module only makes the digest.
pub(super) fn key_of(identity: &str) -> String {
    use sha2::Digest as _;
    hex::encode(sha2::Sha256::digest(identity.as_bytes()))
}

fn receipt_path(root: &Path, tree: &str, key: &str) -> Result<PathBuf, String> {
    if !hex40(tree) || !hex64(key) {
        return Err("invalid candidate receipt key".into());
    }
    Ok(root.join(tree).join(format!("{key}.json")))
}

fn load_at(root: &Path, tree: &str, key: &str, now: u64) -> Option<(GateOutcome, u64)> {
    let path = receipt_path(root, tree, key).ok()?;
    let receipt: Receipt = serde_json::from_slice(&fs::read(path).ok()?).ok()?;
    let age = now.checked_sub(receipt.created_unix);
    let fresh = match age {
        Some(age) => age <= MAX_AGE.as_secs(),
        None => receipt.created_unix - now <= FUTURE_SKEW_SECS,
    };
    (receipt.schema_version == SCHEMA_VERSION
        && receipt.candidate_tree == tree
        && receipt.key == key
        && fresh)
        .then(|| (receipt.outcome, age.unwrap_or(0)))
}

/// The recorded green verdict for exactly this tree and key, with its age in
/// seconds, or `None` for anything else.
pub(super) fn load(root: &Path, tree: &str, key: &str) -> Option<(GateOutcome, u64)> {
    load_at(root, tree, key, now_unix())
}

fn record_at(
    root: &Path,
    tree: &str,
    key: &str,
    outcome: &GateOutcome,
    now: u64,
) -> Result<(), String> {
    let path = receipt_path(root, tree, key)?;
    let parent = path.parent().ok_or("receipt has no parent")?;
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(parent)
        .map_err(|error| format!("cannot create candidate receipt directory: {error}"))?;
    let receipt = Receipt {
        schema_version: SCHEMA_VERSION,
        candidate_tree: tree.into(),
        key: key.into(),
        created_unix: now,
        outcome: outcome.clone(),
    };
    let mut temporary = tempfile::NamedTempFile::new_in(parent)
        .map_err(|error| format!("cannot create candidate receipt temp: {error}"))?;
    serde_json::to_writer(&mut temporary, &receipt)
        .map_err(|error| format!("cannot serialize candidate receipt: {error}"))?;
    temporary
        .as_file()
        .sync_all()
        .map_err(|error| format!("cannot sync candidate receipt temp: {error}"))?;
    temporary
        .persist(&path)
        .map_err(|error| format!("cannot publish candidate receipt: {}", error.error))?;
    prune(root, now);
    Ok(())
}

/// Record a green verdict, then drop receipt directories no lookup can use.
pub(super) fn record(
    root: &Path,
    tree: &str,
    key: &str,
    outcome: &GateOutcome,
) -> Result<(), String> {
    record_at(root, tree, key, outcome, now_unix())
}

/// Remove tree directories untouched for longer than [`MAX_AGE`]. Best effort:
/// the receipts only save time, so a failure here is ignored.
fn prune(root: &Path, now: u64) {
    let Ok(entries) = fs::read_dir(root) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        if !name.to_str().is_some_and(hex40) {
            continue;
        }
        let expired = entry
            .metadata()
            .and_then(|metadata| metadata.modified())
            .ok()
            .and_then(|modified| modified.duration_since(UNIX_EPOCH).ok())
            .is_some_and(|modified| now.saturating_sub(modified.as_secs()) > MAX_AGE.as_secs());
        if expired {
            let _ = fs::remove_dir_all(entry.path());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TREE: &str = "0123456789abcdef0123456789abcdef01234567";

    fn outcome() -> GateOutcome {
        GateOutcome {
            base_reds: BTreeSet::from(["demo::red".to_owned()]),
            flakes: BTreeSet::from(["demo::flaky".to_owned()]),
            full_gate_secs: 42,
            ..GateOutcome::default()
        }
    }

    #[test]
    fn a_recorded_verdict_is_read_back_for_its_exact_tree_and_key() {
        let root = tempfile::tempdir().unwrap();
        let key = key_of("gate one");
        assert_eq!(load(root.path(), TREE, &key), None);
        record_at(root.path(), TREE, &key, &outcome(), 1_000).unwrap();
        assert_eq!(
            load_at(root.path(), TREE, &key, 1_030),
            Some((outcome(), 30))
        );
    }

    #[test]
    fn another_key_or_tree_is_a_miss() {
        let root = tempfile::tempdir().unwrap();
        let key = key_of("gate one");
        record_at(root.path(), TREE, &key, &outcome(), 1_000).unwrap();
        assert_eq!(load_at(root.path(), TREE, &key_of("gate two"), 1_001), None);
        let other_tree = "fedcba9876543210fedcba9876543210fedcba98";
        assert_eq!(load_at(root.path(), other_tree, &key, 1_001), None);
        // A receipt copied under another key's name never matches that key.
        let other = key_of("gate two");
        let from = receipt_path(root.path(), TREE, &key).unwrap();
        fs::copy(&from, receipt_path(root.path(), TREE, &other).unwrap()).unwrap();
        assert_eq!(load_at(root.path(), TREE, &other, 1_001), None);
    }

    #[test]
    fn an_expired_future_or_corrupt_receipt_is_a_miss() {
        let root = tempfile::tempdir().unwrap();
        let key = key_of("gate one");
        record_at(root.path(), TREE, &key, &outcome(), 100_000).unwrap();
        let limit = 100_000 + MAX_AGE.as_secs();
        assert!(load_at(root.path(), TREE, &key, limit).is_some());
        assert_eq!(load_at(root.path(), TREE, &key, limit + 1), None);
        // Dated in the future beyond the skew.
        assert_eq!(
            load_at(root.path(), TREE, &key, 100_000 - FUTURE_SKEW_SECS - 1),
            None
        );
        let path = receipt_path(root.path(), TREE, &key).unwrap();
        fs::write(&path, b"{not json").unwrap();
        assert_eq!(load_at(root.path(), TREE, &key, 100_001), None);
    }

    #[test]
    fn malformed_keys_are_refused_before_touching_the_disk() {
        let root = tempfile::tempdir().unwrap();
        assert!(record_at(root.path(), "../etc", &key_of("x"), &outcome(), 1).is_err());
        assert!(record_at(root.path(), TREE, "short", &outcome(), 1).is_err());
        assert_eq!(load_at(root.path(), TREE, "short", 1), None);
    }

    #[test]
    fn recording_drops_expired_tree_directories_only() {
        let root = tempfile::tempdir().unwrap();
        let stale = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        let unrelated = root.path().join("not-a-tree");
        fs::create_dir_all(root.path().join(stale)).unwrap();
        fs::create_dir_all(&unrelated).unwrap();
        let aged = SystemTime::now() - MAX_AGE - Duration::from_secs(10);
        fs::File::open(root.path().join(stale))
            .unwrap()
            .set_modified(aged)
            .unwrap();
        record_at(root.path(), TREE, &key_of("fresh"), &outcome(), now_unix()).unwrap();
        assert!(!root.path().join(stale).exists());
        assert!(
            unrelated.exists(),
            "only 40-hex tree directories are pruned"
        );
        assert!(root.path().join(TREE).exists());
    }
}
