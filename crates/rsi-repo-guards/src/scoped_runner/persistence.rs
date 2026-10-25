//! Immutable selected scope plus compact red progress. Terminal acceptance
//! evidence remains inline; only the three large selection fields move aside.
use crate::wire_json::{legacy_json, sha256};
use serde_json::{Value, json};
use std::fs::OpenOptions;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

pub const SELECTION_FIELDS: &[&str] = &["filters", "downstream_compile", "downstream_tests"];
const INLINE_BYTES: usize = 16 * 1024;
const PROGRESS_DETAIL_BYTES: usize = 4096;

fn bytes(value: &Value) -> Vec<u8> {
    legacy_json(value, true).into_bytes()
}
fn summary(value: &Value, data: &[u8]) -> Value {
    let mut detail = json!({"bytes":data.len(),"sha256":sha256(data)});
    match value {
        Value::Array(items) => detail["count"] = json!(items.len()),
        Value::Object(items) => {
            detail["count"] = json!(items.len());
            detail["counts"] = json!(
                items
                    .iter()
                    .filter_map(|(key, value)| value
                        .as_array()
                        .map(|a| (key.clone(), json!(a.len()))))
                    .collect::<serde_json::Map<_, _>>()
            );
        }
        _ => (),
    }
    detail
}
struct Temporary(PathBuf);
impl Drop for Temporary {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}
fn staged(path: &Path, data: &[u8]) -> io::Result<Temporary> {
    let parent = path
        .parent()
        .ok_or_else(|| io::Error::other("missing artifact parent"))?;
    let path = parent.join(format!(".rsi-receipt-{}.tmp", uuid::Uuid::new_v4()));
    let temporary = Temporary(path.clone());
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)?;
    file.write_all(data)?;
    file.sync_all()?;
    Ok(temporary)
}
fn immutable(path: &Path, data: &[u8]) -> io::Result<()> {
    let temporary = staged(path, data)?;
    // Atomic no-clobber publication. Even an interrupted writer cannot leave
    // a truncated selection that later progress mistakenly attests to.
    match std::fs::hard_link(&temporary.0, path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
            let metadata = std::fs::symlink_metadata(path)?;
            if !metadata.is_file()
                || metadata.len() != data.len() as u64
                || std::fs::read(path)? != data
            {
                return Err(io::Error::other(
                    "selection artifact already exists with different bytes",
                ));
            }
            Ok(())
        }
        Err(error) => Err(error),
    }
}
/// Call before any lane, after the planner populated all three selection
/// fields. The log directory belongs to this run and must already exist.
pub fn persist_selection(log_dir: &Path, receipt: &mut Value) -> io::Result<()> {
    let selection = Value::Object(
        SELECTION_FIELDS
            .iter()
            .filter_map(|key| {
                receipt
                    .get(*key)
                    .map(|value| ((*key).into(), value.clone()))
            })
            .collect(),
    );
    let mut data = bytes(&selection);
    data.push(b'\n');
    let path = log_dir.join("selection.json");
    immutable(&path, &data)?;
    let mut identity = summary(&selection, &data);
    identity["path"] = json!(path);
    identity["fields"] = json!(
        selection
            .as_object()
            .expect("selection object")
            .iter()
            .map(|(key, value)| (key.clone(), summary(value, &bytes(value))))
            .collect::<serde_json::Map<_, _>>()
    );
    receipt["selection"] = identity;
    receipt["log_dir"] = json!(log_dir);
    Ok(())
}
fn progress_detail(value: &Value) -> Value {
    if (value.is_array() || value.is_string()) && bytes(value).len() > PROGRESS_DETAIL_BYTES {
        return summary(value, &bytes(value));
    }
    if let Some(fields) = value.as_object() {
        return Value::Object(
            fields
                .iter()
                .map(|(key, value)| (key.clone(), progress_detail(value)))
                .collect(),
        );
    }
    value.clone()
}
/// A copy only: the underlying receipt retains its full evidence and immutable
/// selection. Terminal failures, conflict paths and coverage are never trimmed.
pub fn wire(receipt: &Value, progress: bool) -> Value {
    let mut value = receipt.clone();
    if let Some(fields) = receipt["selection"]["fields"].as_object() {
        for (key, detail) in fields {
            if !SELECTION_FIELDS.contains(&key.as_str()) {
                continue;
            }
            if progress
                || detail["bytes"]
                    .as_u64()
                    .is_some_and(|n| n > INLINE_BYTES as u64)
            {
                let mut compact = detail.clone();
                compact["detail_path"] = receipt["selection"]["path"].clone();
                if key == "downstream_compile" {
                    compact["packages"] = json!(
                        receipt[key]["packages"]
                            .as_array()
                            .into_iter()
                            .flatten()
                            .map(|p| {
                                Value::Object(
                                    ["package", "status", "execution"]
                                        .iter()
                                        .filter_map(|field| {
                                            p.get(*field).map(|v| ((*field).into(), v.clone()))
                                        })
                                        .collect(),
                                )
                            })
                            .collect::<Vec<_>>()
                    );
                }
                value[key] = compact;
            }
        }
    }
    if progress {
        let fields = [
            "schema",
            "base",
            "head",
            "dry_run",
            "tool",
            "packages",
            "plan",
            "compile",
            "python",
            "residual",
            "desktop",
            "linux_check",
            "admission",
            "log_dir",
            "selection",
            "selected_tests",
            "run_tests",
            "selected_filters",
            "run_filters",
            "gate_covers_rest",
            "failure_class",
            "filters",
            "downstream_compile",
            "downstream_tests",
            "pid_isolation",
            "pid_isolation_reason",
        ];
        value
            .as_object_mut()
            .expect("receipt object")
            .retain(|key, _| fields.contains(&key.as_str()));
        for (key, item) in value.as_object_mut().expect("receipt object").iter_mut() {
            // Keep the outer package list: its per-package counters remain
            // readable, while each oversized diagnostic is independently bound.
            if key == "packages" {
                if let Some(packages) = item.as_array_mut() {
                    for package in packages {
                        *package = progress_detail(package);
                    }
                }
            } else {
                *item = progress_detail(item);
            }
        }
        value["ok"] = json!(false);
        value["exit_code"] = json!(1);
        value["partial"] = json!(true);
    }
    value
}
pub fn persist_progress(log_dir: &Path, receipt: &Value) -> io::Result<Value> {
    let progress = wire(receipt, true);
    let mut data = bytes(&progress);
    data.push(b'\n');
    let path = log_dir.join("progress.json");
    let temporary = staged(&path, &data)?;
    std::fs::rename(&temporary.0, path)?;
    Ok(progress)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn huge_selection_is_persisted_once_and_bounded_receipts_keep_terminal_evidence() {
        let dir = tempfile::tempdir().unwrap();
        let filters = (0..7000)
            .map(|n| {
                json!(format!(
                    "rsid=shard:store-01:test(module_{n}::dependent_tests)"
                ))
            })
            .collect::<Vec<_>>();
        let items = filters
            .iter()
            .enumerate()
            .map(|(n, f)| json!({"package":"rsid","module":format!("module_{n}"),"filter":f}))
            .collect::<Vec<_>>();
        let failure = json!({"package":"rsid","kind":"lib","target":"rsid","test":"existing_red"});
        let mut receipt = json!({"schema":1,"ok":true,"exit_code":0,"head":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","packages":[{"package":"rsid","status":"pending","completed":0}],"filters":filters,"downstream_compile":{"items":items,"filters":filters,"packages":[{"package":"rsid","status":"pending"}]},"downstream_tests":{"items":items,"filters":filters},"failures":[failure],"baseline_reds":[failure],"new_failures":[],"plan":"targeted_subset","gate_covers_rest":true,"selected_tests":7000,"run_tests":100,"merges_cleanly_onto_rolling":false,"conflict_paths":["scripts/scoped-test"],"pid_isolation":"unavailable","pid_isolation_reason":"not_linux"});
        persist_selection(dir.path(), &mut receipt).unwrap();
        let saved = std::fs::read(dir.path().join("selection.json")).unwrap();
        let selected: Value = serde_json::from_slice(&saved).unwrap();
        assert_eq!(selected["filters"], json!(filters));
        assert_eq!(receipt["selection"]["sha256"], sha256(&saved));
        let selection_path = dir.path().join("selection.json");
        let modified = std::fs::metadata(&selection_path)
            .unwrap()
            .modified()
            .unwrap();
        for phase in ["building", "listed", "testing", "test_failed"] {
            receipt["packages"][0]["phase"] = json!(phase);
            receipt["packages"][0]["completed"] = json!(100);
            let progress = persist_progress(dir.path(), &receipt).unwrap();
            assert!(bytes(&progress).len() < 64 * 1024);
            assert_eq!(progress["ok"], false);
            assert_eq!(progress["partial"], true);
            assert_eq!(progress["packages"][0]["phase"], phase);
            assert_eq!(progress["pid_isolation_reason"], "not_linux");
            assert_eq!(
                serde_json::from_slice::<Value>(
                    &std::fs::read(dir.path().join("progress.json")).unwrap()
                )
                .unwrap(),
                progress
            );
        }
        receipt["downstream_compile"]["packages"][0]["status"] = json!("passed");
        let terminal = wire(&receipt, false);
        assert!(bytes(&terminal).len() < 64 * 1024);
        for key in [
            "failures",
            "baseline_reds",
            "new_failures",
            "head",
            "plan",
            "gate_covers_rest",
            "selected_tests",
            "run_tests",
            "merges_cleanly_onto_rolling",
            "conflict_paths",
            "packages",
        ] {
            assert_eq!(terminal[key], receipt[key], "{key}");
        }
        assert_eq!(terminal["filters"]["count"], 7000);
        assert_eq!(
            terminal["downstream_compile"]["packages"][0]["status"],
            "passed"
        );
        for key in SELECTION_FIELDS {
            assert_eq!(terminal[*key]["sha256"], sha256(bytes(&selected[*key])));
        }
        assert_eq!(std::fs::read(&selection_path).unwrap(), saved);
        assert_eq!(
            std::fs::metadata(&selection_path)
                .unwrap()
                .modified()
                .unwrap(),
            modified
        );
        assert_eq!(receipt["filters"], json!(filters));
    }
    #[test]
    fn selection_publication_is_immutable_and_failures_remain_explicit() {
        let dir = tempfile::tempdir().unwrap();
        let mut receipt = json!({"filters":["pkg=test(=one)"]});
        persist_selection(dir.path(), &mut receipt).unwrap();
        let first = receipt["selection"].clone();
        persist_selection(dir.path(), &mut receipt).unwrap();
        assert_eq!(receipt["selection"], first);
        receipt["filters"] = json!(["pkg=test(=two)"]);
        assert!(persist_selection(dir.path(), &mut receipt).is_err());
        assert!(persist_progress(&dir.path().join("missing"), &receipt).is_err());
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
    }
    #[test]
    fn ascii_sorted_json_and_summary_hash_match_the_existing_wire_contract() {
        let value: Value = serde_json::from_str(r#"{"z":"é😀","a":[1.0,0.000001,1e16]}"#).unwrap();
        assert_eq!(
            legacy_json(&value, true),
            r#"{"a":[1.0,1e-06,1e+16],"z":"\u00e9\ud83d\ude00"}"#
        );
        assert_eq!(
            sha256(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        let mut receipt = json!({"packages":[{"package":"pkg","completed":1,"unfinished_tests":["x".repeat(5000)]}],"ok":true,"ok_reason":"baseline_known_failure"});
        let progress = wire(&receipt, true);
        assert_eq!(progress["ok"], false);
        assert_eq!(progress["packages"][0]["unfinished_tests"]["count"], 1);
        assert_eq!(receipt["ok"], true);
        receipt["filters"] = json!(["small"]);
        let dir = tempfile::tempdir().unwrap();
        persist_selection(dir.path(), &mut receipt).unwrap();
        assert_eq!(wire(&receipt, false)["filters"], json!(["small"]));
    }
}
