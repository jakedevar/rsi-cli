//! Cargo JSON diagnostics, warning baselines and fail-closed measurement.
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Default, Debug, Deserialize, Serialize)]
pub struct Warnings {
    pub measured: BTreeMap<String, BTreeSet<String>>,
    pub warnings: BTreeMap<String, BTreeMap<String, BTreeSet<String>>>,
    pub unattributed: usize,
    pub degraded: Vec<String>,
}
fn package_name(package: &str) -> Option<String> {
    if let Some((_, tail)) = package.rsplit_once('#') {
        return tail
            .split('@')
            .next()
            .filter(|s| !s.is_empty())
            .map(Into::into);
    }
    package
        .split_whitespace()
        .next()
        .filter(|s| !s.is_empty())
        .map(Into::into)
}
impl Warnings {
    pub fn new(environment: &BTreeMap<String, String>) -> Self {
        let mut result = Self::default();
        for name in ["RUSTFLAGS", "CARGO_ENCODED_RUSTFLAGS"] {
            if let Some(flags) = environment.get(name) {
                let flags = flags.replace('\u{1f}', " ");
                let suppression = crate::re::Re::new(
                    r"(?:^|\s)(?:-A|--allow)[\s=]?(?:warnings|unused[a-z_]*|dead_code)(?=\s|$)|(?:^|\s)--cap-lints\b",
                );
                if suppression.is_match(&flags) {
                    result
                        .degraded
                        .push(format!("{name} suppresses compiler warnings"));
                }
            }
        }
        result
    }
    pub fn observe(&mut self, root: &str, line: &str) -> Option<Value> {
        let Ok(value) = serde_json::from_str::<Value>(line) else {
            if crate::re::Re::new(r"\s*--> \S+:\d+:\d+").is_match(line)
                && !self
                    .degraded
                    .iter()
                    .any(|s| s.contains("rendered diagnostic"))
            {
                self.degraded
                    .push("a rendered diagnostic arrived outside the JSON stream".into());
            }
            return None;
        };
        let package = value["package_id"].as_str().and_then(package_name);
        match value["reason"].as_str() {
            Some("compiler-artifact") => {
                if let Some(package) = package {
                    let kind = value["target"]["kind"][0].as_str().unwrap_or("?");
                    self.measured.entry(package).or_default().insert(format!(
                        "{kind}{}",
                        if value["profile"]["test"].as_bool().unwrap_or(false) {
                            " test"
                        } else {
                            ""
                        }
                    ));
                }
            }
            Some("compiler-message") => {
                let detail = &value["message"];
                let message = detail["message"].as_str().unwrap_or("");
                if detail["level"] == "warning"
                    && !crate::re::Re::new(r"^(?:\d+ warnings? emitted|aborting due to .*)$")
                        .is_match(message)
                {
                    if let Some(package) = package {
                        let spans: Vec<_> =
                            detail["spans"].as_array().into_iter().flatten().collect();
                        let span = spans
                            .iter()
                            .find(|s| s["is_primary"] == true)
                            .or_else(|| spans.first())
                            .copied();
                        let file = span
                            .and_then(|s| s["file_name"].as_str())
                            .unwrap_or("")
                            .replace('\\', "/");
                        let prefix = format!("{}/", root.replace('\\', "/").trim_end_matches('/'));
                        let file = file.strip_prefix(&prefix).unwrap_or(&file);
                        let code = detail["code"]["code"].as_str().unwrap_or("");
                        let position = format!(
                            "{}:{}",
                            span.and_then(|s| s["line_start"].as_u64()).unwrap_or(0),
                            span.and_then(|s| s["column_start"].as_u64()).unwrap_or(0)
                        );
                        self.warnings
                            .entry(package)
                            .or_default()
                            .entry(format!("{file}|{code}|{message}"))
                            .or_default()
                            .insert(position);
                    } else {
                        self.unattributed += 1;
                    }
                }
            }
            _ => (),
        }
        Some(value)
    }
    pub fn merge(&mut self, other: Self) {
        for (package, configs) in other.measured {
            self.measured.entry(package).or_default().extend(configs);
        }
        for (package, keys) in other.warnings {
            for (key, positions) in keys {
                self.warnings
                    .entry(package.clone())
                    .or_default()
                    .entry(key)
                    .or_default()
                    .extend(positions);
            }
        }
        self.unattributed += other.unattributed;
        for reason in other.degraded {
            if !self.degraded.contains(&reason) {
                self.degraded.push(reason);
            }
        }
    }
}

#[derive(Clone, Debug, Deserialize)]
pub struct BaselineEntry {
    pub configs: Vec<String>,
    pub warnings: BTreeMap<String, usize>,
}
#[derive(Clone, Debug, Default, Deserialize)]
pub struct HostBaseline {
    #[serde(default)]
    pub packages: BTreeMap<String, BaselineEntry>,
}
#[derive(Clone, Debug, Deserialize)]
pub struct Baseline {
    pub schema: u32,
    pub packages: BTreeMap<String, BaselineEntry>,
    #[serde(default)]
    pub hosts: BTreeMap<String, HostBaseline>,
}
impl Baseline {
    pub fn parse(text: &str) -> Result<Self, String> {
        let value: Self = serde_json::from_str(text).map_err(|e| e.to_string())?;
        if value.schema != 1 {
            return Err("warning baseline schema must be 1".into());
        }
        for (name, entry) in value
            .packages
            .iter()
            .chain(value.hosts.values().flat_map(|host| host.packages.iter()))
        {
            if entry.configs.is_empty() || entry.warnings.values().any(|v| *v == 0) {
                return Err(format!(
                    "package {name}: needs a non-empty `configs` list and `warnings` counts of 1 or more"
                ));
            }
        }
        Ok(value)
    }

    /// Overlay this machine's host-specific warning keys on the portable baseline.
    pub fn for_host(&self, host: &str) -> Self {
        let mut packages = self.packages.clone();
        if let Some(host_baseline) = self.hosts.get(host) {
            for (name, entry) in &host_baseline.packages {
                let merged = packages
                    .entry(name.clone())
                    .or_insert_with(|| BaselineEntry {
                        configs: Vec::new(),
                        warnings: BTreeMap::new(),
                    });
                for config in &entry.configs {
                    if !merged.configs.contains(config) {
                        merged.configs.push(config.clone());
                    }
                }
                merged.warnings.extend(entry.warnings.clone());
            }
        }
        Self {
            schema: self.schema,
            packages,
            hosts: BTreeMap::new(),
        }
    }
}

pub fn evaluate(
    observed: &Warnings,
    candidate: &Baseline,
    base: Option<&Baseline>,
    base_state: &str,
) -> Value {
    evaluate_for_host(observed, candidate, base, base_state, std::env::consts::OS)
}

fn evaluate_for_host(
    observed: &Warnings,
    candidate: &Baseline,
    base: Option<&Baseline>,
    base_state: &str,
    host: &str,
) -> Value {
    let mut packages = serde_json::Map::new();
    let mut refusals = Vec::new();
    let compare_scope = |scope: &str,
                         candidate: &BTreeMap<String, BaselineEntry>,
                         before: Option<&BTreeMap<String, BaselineEntry>>,
                         refusals: &mut Vec<String>| {
        for (name, entry) in candidate {
            if let Some(before) = before.and_then(|b| b.get(name)) {
                for (key, count) in &entry.warnings {
                    let old = before.warnings.get(key).copied().unwrap_or(0);
                    if *count > old {
                        refusals.push(format!(
                            "{scope}{name}: baseline raised for {key:?}: {old} -> {count}"
                        ));
                    }
                }
            }
        }
    };
    compare_scope(
        "",
        &candidate.packages,
        base.map(|b| &b.packages),
        &mut refusals,
    );
    for (host, host_baseline) in &candidate.hosts {
        compare_scope(
            &format!("{host} "),
            &host_baseline.packages,
            base.and_then(|b| b.hosts.get(host)).map(|b| &b.packages),
            &mut refusals,
        );
    }
    if let Some(base) = base {
        for name in base
            .packages
            .keys()
            .filter(|name| !candidate.packages.contains_key(*name))
        {
            refusals.push(format!(
                "{name}: its baseline was removed; lower the counts instead"
            ));
        }
    }
    let candidate = candidate.for_host(host);
    let mut unavailable = observed.degraded.clone();
    if observed.unattributed > 0 {
        unavailable.push(format!(
            "{} warning(s) carried no package id",
            observed.unattributed
        ));
    }
    // An unresolved/unreadable base cannot establish that the baseline was not raised.
    if ["unresolved", "unreadable"].contains(&base_state) {
        unavailable.push(format!("base warning baseline is {base_state}"));
    }
    let mut measured = 0;
    let mut new_warnings = false;
    for (name, entry) in &candidate.packages {
        let baseline: usize = entry.warnings.values().sum();
        let Some(configs) = observed.measured.get(name) else {
            packages.insert(
                name.clone(),
                json!({"status":"not_measured","baseline":baseline}),
            );
            continue;
        };
        measured += 1;
        let counts: BTreeMap<_, _> = observed
            .warnings
            .get(name)
            .into_iter()
            .flat_map(|keys| keys.iter())
            .map(|(key, places)| (key.clone(), places.len()))
            .collect();
        let new: Vec<_> = counts
            .iter()
            .filter_map(|(key, count)| {
                let before = entry.warnings.get(key).copied().unwrap_or(0);
                (*count > before).then(|| json!({"key":key,"count":count-before}))
            })
            .collect();
        let complete = entry.configs.iter().all(|c| configs.contains(c));
        let fixed: Vec<_> = if complete {
            entry
                .warnings
                .iter()
                .filter_map(|(key, count)| {
                    let actual = counts.get(key).copied().unwrap_or(0);
                    (*count > actual).then(|| json!({"key":key,"count":count-actual}))
                })
                .collect()
        } else {
            vec![]
        };
        new_warnings |= !new.is_empty();
        packages.insert(name.clone(),json!({"status":if new.is_empty(){"ok"}else{"new_warnings"},"observed":counts.values().sum::<usize>(),"baseline":baseline,"configs":configs,"complete":complete,"new":new.iter().take(20).collect::<Vec<_>>(),"new_total":new.iter().filter_map(|x|x["count"].as_u64()).sum::<u64>(),"fixed":fixed.iter().take(20).collect::<Vec<_>>(),"fixed_total":fixed.iter().filter_map(|x|x["count"].as_u64()).sum::<u64>()}));
    }
    let status = if !refusals.is_empty() {
        "baseline_raised"
    } else if !unavailable.is_empty()
        && (measured > 0 || ["unresolved", "unreadable"].contains(&base_state))
    {
        "unavailable"
    } else if new_warnings {
        "new_warnings"
    } else if measured > 0 {
        "ok"
    } else {
        "not_measured"
    };
    let mut summary =
        json!({"status":status,"packages":packages,"base_baseline":base_state,"host":host});
    if !refusals.is_empty() {
        summary["refusals"] = json!(refusals);
    }
    if !unavailable.is_empty() && status == "unavailable" {
        summary["unavailable"] = json!(unavailable);
    }
    summary
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn warning_positions_deduplicate_across_build_units() {
        let mut warnings = Warnings::default();
        let artifact = json!({"reason":"compiler-artifact","package_id":"path+file:///repo/crate#pkg@0.1.0","target":{"kind":["lib"]},"profile":{"test":true}});
        warnings.observe("/repo", &artifact.to_string());
        let message = json!({"reason":"compiler-message","package_id":"pkg 0.1.0 (path+file:///repo/crate)","message":{"level":"warning","message":"unused","code":{"code":"dead_code"},"spans":[{"file_name":"/repo/lib.rs","line_start":7,"column_start":1,"is_primary":true}]}});
        warnings.observe("/repo", &message.to_string());
        warnings.observe("/repo", &message.to_string());
        assert_eq!(warnings.warnings["pkg"]["lib.rs|dead_code|unused"].len(), 1);
        assert_eq!(
            warnings.measured["pkg"],
            BTreeSet::from(["lib test".into()])
        );
    }
    #[test]
    fn new_warnings_raised_baselines_and_untrusted_diagnostics_are_red() {
        let baseline = Baseline::parse(
            r#"{"schema":1,"packages":{"pkg":{"configs":["lib test"],"warnings":{}}}}"#,
        )
        .unwrap();
        let raised = Baseline::parse(
            r#"{"schema":1,"packages":{"pkg":{"configs":["lib test"],"warnings":{"new":1}}}}"#,
        )
        .unwrap();
        let mut observed = Warnings::default();
        observed
            .measured
            .insert("pkg".into(), BTreeSet::from(["lib test".into()]));
        observed.warnings.insert(
            "pkg".into(),
            BTreeMap::from([("new".into(), BTreeSet::from(["1:1".into()]))]),
        );
        assert_eq!(
            evaluate(&observed, &baseline, Some(&baseline), "present")["status"],
            "new_warnings"
        );
        assert_eq!(
            evaluate(&observed, &raised, Some(&baseline), "present")["status"],
            "baseline_raised"
        );
        observed.degraded.push("suppressed".into());
        assert_eq!(
            evaluate(&observed, &baseline, Some(&baseline), "present")["status"],
            "unavailable"
        );
        assert_eq!(
            evaluate(&Warnings::default(), &baseline, None, "unresolved")["status"],
            "unavailable"
        );
    }
    #[test]
    fn missing_measurements_are_not_called_clean_and_invalid_baselines_fail() {
        let baseline =
            Baseline::parse(r#"{"schema":1,"packages":{"pkg":{"configs":["lib"],"warnings":{}}}}"#)
                .unwrap();
        assert_eq!(
            evaluate(&Warnings::default(), &baseline, None, "absent")["status"],
            "not_measured"
        );
        assert!(
            Baseline::parse(r#"{"schema":1,"packages":{"pkg":{"configs":[],"warnings":{}}}}"#)
                .is_err()
        );
        let env = BTreeMap::from([("RUSTFLAGS".into(), "--cap-lints allow".into())]);
        assert_eq!(Warnings::new(&env).degraded.len(), 1);
    }

    #[test]
    fn host_warning_baselines_apply_only_on_the_selected_host_and_cannot_be_raised() {
        let baseline = Baseline::parse(
            r#"{"schema":1,"packages":{"pkg":{"configs":["lib"],"warnings":{}}},"hosts":{"macos":{"packages":{"pkg":{"configs":["lib"],"warnings":{"mac-only":1}}}}}}"#,
        )
        .unwrap();
        let mut observed = Warnings::default();
        observed
            .measured
            .insert("pkg".into(), BTreeSet::from(["lib".into()]));
        observed.warnings.insert(
            "pkg".into(),
            BTreeMap::from([("mac-only".into(), BTreeSet::from(["1:1".into()]))]),
        );
        assert_eq!(
            evaluate_for_host(&observed, &baseline, Some(&baseline), "present", "macos")["status"],
            "ok"
        );
        assert_eq!(
            evaluate_for_host(&observed, &baseline, Some(&baseline), "present", "macos")["host"],
            "macos"
        );
        assert_eq!(
            evaluate_for_host(&observed, &baseline, Some(&baseline), "present", "linux")["status"],
            "new_warnings"
        );
        let raised = Baseline::parse(
            r#"{"schema":1,"packages":{"pkg":{"configs":["lib"],"warnings":{}}},"hosts":{"macos":{"packages":{"pkg":{"configs":["lib"],"warnings":{"mac-only":2}}}}}}"#,
        )
        .unwrap();
        assert_eq!(
            evaluate_for_host(&observed, &raised, Some(&baseline), "present", "linux")["status"],
            "baseline_raised"
        );
    }
}
