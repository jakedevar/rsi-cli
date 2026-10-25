//! The mandatory static gates. `whitespace` is one bounded Git call; every
//! other gate goes through `Contracts::guard`, which runs the owner's Rust gate as a
//! killable child of this binary under a hard deadline (and is `incomplete_contract`,
//! red, for any gate whose owner has not published it). A derivation audit verdict
//! (`ok`/`fail`) replaces a gate's own run, as it always has.
use super::{
    contracts::{Contracts, INCOMPLETE},
    gitx::Git,
    orchestration::REQUIRED_GUARDS,
};
use serde_json::{Map, Value, json};
use std::collections::BTreeMap;
use std::path::Path;

fn entry(status: &str, detail: &str) -> Value {
    json!({"status": status, "detail": detail})
}

pub struct Inputs<'a> {
    pub repo: &'a Path,
    pub base: &'a str,
    pub head: &'a str,
    pub audits: &'a Value,
    pub environment: &'a BTreeMap<String, String>,
}

/// Run every mandatory gate and return one entry per name. Never stops early:
/// each gate's evidence is retained even when an earlier one is red.
pub fn run(
    inputs: &Inputs,
    git: &Git,
    contracts: &mut dyn Contracts,
    mut announce: impl FnMut(&str),
) -> BTreeMap<String, Value> {
    let mut result = BTreeMap::new();
    let audit = |name: &str| -> Option<Value> {
        let audit = inputs.audits.get(name)?;
        let verdict = audit["verdict"]
            .as_str()
            .filter(|v| ["ok", "fail"].contains(v))?;
        Some(entry(verdict, audit["detail"].as_str().unwrap_or("")))
    };
    for name in REQUIRED_GUARDS {
        let value = if let Some(value) = audit(name) {
            value
        } else if *name == "whitespace" {
            announce(&format!(
                "static guard: git diff --check {} {}",
                inputs.base, inputs.head
            ));
            match git.builtin(&["diff", "--check", inputs.base, inputs.head], &[]) {
                Ok(out) => entry(
                    if out.ok() { "ok" } else { "fail" },
                    format!("{}{}", out.stdout, out.stderr).trim(),
                ),
                Err(error) => entry("fail", &error.to_string()),
            }
        } else {
            announce(&format!("static guard: {name}"));
            match contracts.guard(name, inputs.repo, inputs.base, inputs.head) {
                Ok(value) => value,
                Err(incomplete) => incomplete.value(),
            }
        };
        announce(&format!(
            "scoped-test: static guard {name}={}{}",
            value["status"].as_str().unwrap_or("?"),
            value["detail"]
                .as_str()
                .filter(|d| !d.is_empty())
                .map(|d| format!(": {d}"))
                .unwrap_or_default()
        ));
        result.insert((*name).to_string(), value);
    }
    result
}

/// True when any gate is not `ok` (the legacy `skipped` status is not accepted:
/// the receipt contract requires an observed verdict for every gate).
pub fn failed(guards: &BTreeMap<String, Value>) -> bool {
    REQUIRED_GUARDS
        .iter()
        .any(|name| guards.get(*name).is_none_or(|v| v["status"] != "ok"))
        || guards.values().any(|v| v["status"] != "ok")
}

pub fn incomplete(guards: &BTreeMap<String, Value>) -> Vec<String> {
    guards
        .iter()
        .filter(|(_, v)| v["status"] == INCOMPLETE)
        .map(|(name, _)| name.clone())
        .collect()
}

pub fn to_object(guards: &BTreeMap<String, Value>) -> Value {
    Value::Object(
        guards
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect::<Map<_, _>>(),
    )
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::scoped_runner::contracts::Staged;
    use crate::testutil;
    use std::sync::atomic::AtomicUsize;

    #[test]
    fn every_required_gate_has_a_verdict_and_unfinished_contracts_stay_red() {
        let dir = tempfile::tempdir().unwrap();
        testutil::git(dir.path(), &["init", "-q", "-b", "main"]);
        testutil::write(dir.path(), "a.txt", "one\n");
        testutil::git(dir.path(), &["add", "a.txt"]);
        testutil::git(
            dir.path(),
            &[
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@t",
                "commit",
                "-qm",
                "one",
            ],
        );
        let head = testutil::git(dir.path(), &["rev-parse", "HEAD"])
            .trim()
            .to_string();
        let env = super::super::gitx::test_environment();
        let cancel = AtomicUsize::new(0);
        let git = Git {
            repo: dir.path(),
            env: &env,
            cancel: &cancel,
        };
        let audits = json!({"released_migrations": {"verdict": "ok", "detail": "derived"}});
        let inputs = Inputs {
            repo: dir.path(),
            base: &head,
            head: &head,
            audits: &audits,
            environment: &env,
        };
        let mut lines = Vec::new();
        let guards = run(&inputs, &git, &mut Staged, |line| {
            lines.push(line.to_string())
        });
        assert_eq!(guards.len(), REQUIRED_GUARDS.len());
        assert_eq!(guards["whitespace"]["status"], "ok");
        // A derivation audit verdict replaces the gate's own run.
        assert_eq!(guards["released_migrations"]["detail"], "derived");
        // Every gate but `whitespace` and the audited one is delegated: under the
        // unfinished default each is red, never skipped.
        for name in [
            "shard_gates",
            "target_reclaim",
            "guard_regressions",
            "store_boundary",
        ] {
            assert_eq!(guards[name]["status"], INCOMPLETE, "{name}");
            assert_eq!(guards[name]["owner"], "#362");
        }
        assert!(failed(&guards));
        let delegated: Vec<String> = REQUIRED_GUARDS
            .iter()
            .filter(|n| !["whitespace", "released_migrations"].contains(n))
            .map(|n| (*n).to_string())
            .collect();
        let mut red = incomplete(&guards);
        red.sort();
        let mut expected = delegated;
        expected.sort();
        assert_eq!(red, expected);
        assert!(
            lines
                .iter()
                .any(|l| l.contains("static guard shard_gates=incomplete_contract"))
        );
        // Every gate ok means the set is not failed; one non-ok gate fails it.
        let mut ok: BTreeMap<String, Value> = REQUIRED_GUARDS
            .iter()
            .map(|n| ((*n).to_string(), json!({"status":"ok"})))
            .collect();
        assert!(!failed(&ok));
        ok.insert("agents_md_size".into(), json!({"status":"skipped"}));
        assert!(failed(&ok));
        ok.remove("agents_md_size");
        assert!(failed(&ok));
        assert!(to_object(&ok).is_object());
    }
}
