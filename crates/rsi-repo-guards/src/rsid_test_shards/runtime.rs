//! Runtime Nextest identity verification; exceptions never waive feature mounts.
use super::*;
use serde_json::{Value, json};
#[derive(Clone, Debug)]
pub struct RuntimeExceptions(pub BTreeMap<String, BTreeMap<String, usize>>);
impl Default for RuntimeExceptions {
    fn default() -> Self {
        let rows = [
            (
                "store-01",
                vec![
                    "v100_exact_v99_sources_converge_without_title_or_lineage_data_loss",
                    "v100_failpoints_roll_back_both_exact_v99_sources_and_retry",
                    "v100_unknown_or_hybrid_v99_catalog_fails_closed",
                ],
            ),
            (
                "session-03",
                vec!["proc_map_device_identity_rejects_oversized_macos_components"],
            ),
            (
                "other-03",
                vec!["refuses_reclaim_without_linux_openat2_containment"],
            ),
        ];
        Self(
            rows.into_iter()
                .map(|(s, n)| (s.into(), n.into_iter().map(|n| (n.into(), 1)).collect()))
                .collect(),
        )
    }
}
fn counts<'a>(names: impl Iterator<Item = &'a str>) -> BTreeMap<String, usize> {
    let mut c = BTreeMap::new();
    for n in names {
        *c.entry(n.into()).or_default() += 1;
    }
    c
}
fn py_list(names: &[String]) -> String {
    format!(
        "[{}]",
        names
            .iter()
            .map(|s| crate::py::repr(s))
            .collect::<Vec<_>>()
            .join(", ")
    )
}
pub fn check_runtime(
    directory: &Path,
    inventory: &Inventory,
    shards: &[Shard],
    selected: &[String],
    exceptions: &RuntimeExceptions,
) -> Result<String, String> {
    let mut seen = BTreeSet::new();
    let mut total = 0;
    let mut conditional_absent = 0;
    let mut expected_total = 0;
    for shard in selected {
        let packages = &shards
            .iter()
            .find(|s| s.name == *shard)
            .ok_or_else(|| format!("unknown runtime shard {shard}"))?
            .packages;
        let path = directory.join(format!("{shard}.json"));
        let display = path.display();
        let data: Value = serde_json::from_slice(&fs::read(&path).map_err(|e| e.to_string())?)
            .map_err(|e| e.to_string())?;
        let suites = data
            .get("rust-suites")
            .and_then(Value::as_object)
            .ok_or_else(|| {
                format!(
                    "{display}: expected one library suite for each of {}",
                    py_list(packages)
                )
            })?;
        if suites.keys().cloned().collect::<BTreeSet<_>>() != packages.iter().cloned().collect() {
            return Err(format!(
                "{display}: expected one library suite for each of {}",
                py_list(packages)
            ));
        }
        let mut optional = exceptions.0.get(shard).cloned().unwrap_or_default();
        if let Some(host) = inventory.platform_conditional.get(shard) {
            for (name, count) in host {
                let v = optional.entry(name.clone()).or_default();
                *v = (*v).max(*count);
            }
        }
        let expected = counts(
            inventory
                .tests
                .iter()
                .filter(|t| t.shard == *shard)
                .map(|t| t.name.as_str()),
        );
        if optional
            .iter()
            .any(|(n, c)| expected.get(n).copied().unwrap_or(0) < *c)
        {
            return Err(format!(
                "{display}: conditional runtime exception is absent from the static manifest"
            ));
        }
        let expected_count: usize = expected.values().sum();
        expected_total += expected_count;
        let mut names = Vec::new();
        let mut shard_missing = 0;
        for (binary, suite) in suites {
            if suite.get("binary-id").and_then(Value::as_str) != Some(binary.as_str())
                || suite.get("testcases").and_then(Value::as_object).is_none()
            {
                return Err(format!("{display}: malformed Nextest library suite"));
            }
            let cases = suite["testcases"]
                .as_object()
                .ok_or_else(|| format!("{display}: malformed Nextest library suite"))?;
            let mut suite_names = Vec::new();
            for (name, metadata) in cases {
                let matched = metadata
                    .get("filter-match")
                    .cloned()
                    .unwrap_or_else(|| json!({}));
                let ignored = metadata.get("ignored").and_then(Value::as_bool);
                let selected_case = matched.get("status").and_then(Value::as_str)
                    == Some("matches")
                    || (ignored == Some(true)
                        && matched == json!({"status":"mismatch","reason":"ignored"}));
                if !selected_case || ignored.is_none() {
                    return Err(format!("{display}: filtered or malformed test {name}"));
                }
                let identity = format!("{binary}::{name}");
                if !seen.insert(identity.clone()) {
                    return Err(format!(
                        "runtime test appears in multiple shards: {identity}"
                    ));
                }
                suite_names.push(name.rsplit("::").next().unwrap_or(name).to_string());
            }
            let expected = counts(
                inventory
                    .tests
                    .iter()
                    .filter(|t| {
                        t.shard == *shard && package_of(&t.source).ok() == Some(binary.as_str())
                    })
                    .map(|t| t.name.as_str()),
            );
            let actual = counts(suite_names.iter().map(String::as_str));
            let mut missing = Vec::new();
            let mut extra = Vec::new();
            let mut visited = BTreeSet::new();
            // Counter subtraction follows first occurrence in the source inventory.
            for test in inventory.tests.iter().filter(|t| {
                t.shard == *shard && package_of(&t.source).ok() == Some(binary.as_str())
            }) {
                if !visited.insert(test.name.clone()) {
                    continue;
                }
                let absent = expected[&test.name]
                    .saturating_sub(actual.get(&test.name).copied().unwrap_or(0));
                shard_missing += absent;
                missing.extend(std::iter::repeat_n(
                    test.name.clone(),
                    absent.saturating_sub(optional.get(&test.name).copied().unwrap_or(0)),
                ));
            }
            for (n, c) in &actual {
                extra.extend(std::iter::repeat_n(
                    n.clone(),
                    c.saturating_sub(expected.get(n).copied().unwrap_or(0)),
                ));
            }
            if !missing.is_empty() || !extra.is_empty() {
                return Err(format!(
                    "{display}: runtime/static test identities differ in {binary}: missing={}, extra={}",
                    py_list(&missing[..missing.len().min(10)]),
                    py_list(&extra[..extra.len().min(10)])
                ));
            }
            names.extend(suite_names);
        }
        conditional_absent += shard_missing;
        if names.len() != expected_count - shard_missing {
            return Err(format!(
                "{display}: expected {} runtime tests, observed {}",
                expected_count - shard_missing,
                names.len()
            ));
        }
        total += names.len();
    }
    if total != expected_total - conditional_absent {
        return Err(format!("runtime union count differs: {total}"));
    }
    Ok(format!(
        "runtime union: {total} distinct rsid library tests in {} shard(s); {conditional_absent} conditional source tests absent",
        selected.len()
    ))
}
