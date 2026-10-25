//! Fail-closed attribution of a whole guard timeout to known base reds (hub #3483).

use super::*;

/// Two minutes of execution for an e2e selection already proven entirely red
/// on this base. Builds/prebuilds retain their existing budgets.
pub(super) const E2E_TIMEOUT: Duration = Duration::from_secs(120);

pub(super) fn candidate_command(command: &GuardCommand, proven: bool) -> GuardCommand {
    let mut bounded = command.clone();
    if runs_e2e_target(command) && proven {
        bounded.timeout = bounded.timeout.min(E2E_TIMEOUT);
    }
    bounded
}

/// List exactly the run's filters, target and features. Unknown command shapes
/// have no exemption. Listing may include ignored libtest cases: that is a
/// conservative superset and cannot hide a non-red test.
pub(super) fn list_command(command: &GuardCommand, tree: ShardTree<'_>) -> Option<GuardCommand> {
    if cargo_test_package(command).is_some() {
        // The lander generates one lib/bin/integration target per invocation.
        // Do not accept Cargo's default (multiple binaries and doc tests).
        let target_count = command
            .args
            .iter()
            .filter(|arg| matches!(arg.as_str(), "--lib" | "--bin" | "--test"))
            .count();
        if target_count != 1 {
            return None;
        }
        let mut list = command.clone();
        if !list.args.iter().any(|arg| arg == "--") {
            list.args.push("--".into());
        }
        list.args.push("--list".into());
        return Some(list);
    }
    let mut list = if rsid_lib_filterset(command).is_some() {
        command.clone()
    } else {
        direct_base_shard_command(command, tree)?
    };
    list.args[1] = "list".into();
    // These nextest run options do not apply to `list`. Keep selection options
    // (including the slow tier/default filter) exactly as on the real run.
    let mut args = Vec::new();
    let mut original = list.args.into_iter();
    while let Some(arg) = original.next() {
        if matches!(
            arg.as_str(),
            "--status-level" | "--final-status-level" | "-j" | "--no-tests"
        ) {
            original.next()?;
        } else {
            args.push(arg);
        }
    }
    args.extend(["--message-format".into(), "json".into()]);
    list.args = args;
    Some(list)
}

/// A successful, complete inventory only. A timeout's `running 1 test` or
/// partial failure list is never sufficient to determine selection.
pub(super) fn selected_tests(
    report: &rsid::integration::GuardCommandReport,
    output_tail_bytes: usize,
) -> Option<BTreeSet<String>> {
    // The tail byte cut is independent of the capture truncation flag. Even
    // a valid summary can follow a cut inside the first test's name.
    if report.status != rsid::integration::GuardStatus::Passed
        || report.output_truncated
        || report.stdout_tail.len() >= output_tail_bytes
    {
        return None;
    }
    if report.args.first().is_some_and(|arg| arg == "nextest") {
        let listing: Value = serde_json::from_str(report.stdout_tail.trim()).ok()?;
        let mut selected = BTreeSet::new();
        for suite in listing.get("rust-suites")?.as_object()?.values() {
            for (name, case) in suite.get("testcases")?.as_object()? {
                let status = case.get("filter-match")?.get("status")?.as_str()?;
                let ignored = case.get("ignored")?.as_bool()?;
                match status {
                    "matches" if !ignored => {
                        selected.insert(name.clone());
                    }
                    "matches" | "mismatch" => {}
                    _ => return None,
                }
            }
        }
        return Some(selected);
    }
    let names = libtest_listed_names(report)?;
    let mut summaries = report.stdout_tail.lines().filter_map(|line| {
        let (tests, benchmarks) = line.trim().split_once(", ")?;
        let number = |text: &str, singular: &str, plural: &str| {
            text.strip_suffix(plural)
                .or_else(|| text.strip_suffix(singular))?
                .parse::<usize>()
                .ok()
        };
        Some((
            number(tests, " test", " tests")?,
            number(benchmarks, " benchmark", " benchmarks")?,
        ))
    });
    let (count, benchmarks) = summaries.next()?;
    (summaries.next().is_none() && count == names.len() && benchmarks == 0).then_some(names)
}

pub(super) fn all_known(selected: Option<&BTreeSet<String>>, snapshot: Option<&Snapshot>) -> bool {
    let (Some(selected), Some(snapshot)) = (selected, snapshot) else {
        return false;
    };
    !selected.is_empty()
        && selected.iter().all(|name| {
            snapshot
                .live_records()
                .any(|entry| entry.record.test_id == *name)
        })
}

/// A signature match on actual base failure text, plus base failure membership
/// for every selected test. Cached names without signature evidence fail closed.
pub(super) fn proven_reds(
    selected: Option<&BTreeSet<String>>,
    base: Option<&BaseTestEvidence>,
    snapshot: Option<&Snapshot>,
    texts: &BTreeMap<String, String>,
    claimed: &BTreeSet<String>,
) -> Option<BTreeSet<String>> {
    if !all_known(selected, snapshot) {
        return None;
    }
    let selected = selected?;
    let BaseTestEvidence::Observed(base) = base? else {
        return None;
    };
    (selected.is_subset(base)
        && selected.is_disjoint(claimed)
        && selected.iter().all(|name| {
            texts.get(name).is_some_and(|text| {
                matches!(
                    classify(snapshot.unwrap(), name, text, None),
                    Classification::Known { .. }
                )
            })
        }))
    .then(|| selected.clone())
}

#[cfg(test)]
mod tests {
    use super::*;

    const RED: &str = "fresh_install::test_e2e_fresh_install_appoint_project_manager";

    fn snapshot() -> Snapshot {
        serde_json::from_value(serde_json::json!({
            "schema_version": 1, "exported_at": "2026-10-21T00:00:00Z",
            "records": [{"record": {"test_id": RED,
                "matcher": {"contains": ["fixture base failure"]},
                "issue": 3431, "class": "env"}, "issue_status": "Open"}]
        }))
        .unwrap()
    }

    #[test]
    fn known_red_timeout_requires_every_selected_test_and_base_signature() {
        let selected = BTreeSet::from([RED.to_owned()]);
        let base = BaseTestEvidence::Observed(selected.clone());
        let texts = BTreeMap::from([(RED.to_owned(), "fixture base failure".into())]);
        let snapshot = snapshot();
        let claimed = BTreeSet::new();
        assert_eq!(
            proven_reds(
                Some(&selected),
                Some(&base),
                Some(&snapshot),
                &texts,
                &claimed
            ),
            Some(selected.clone())
        );
        let mixed = BTreeSet::from([RED.to_owned(), "fresh_install::green".into()]);
        assert_eq!(
            proven_reds(Some(&mixed), Some(&base), Some(&snapshot), &texts, &claimed),
            None
        );
        assert_eq!(
            proven_reds(None, Some(&base), Some(&snapshot), &texts, &claimed),
            None
        );
        assert_eq!(
            proven_reds(
                Some(&BTreeSet::new()),
                Some(&base),
                Some(&snapshot),
                &texts,
                &claimed
            ),
            None
        );
        assert_eq!(
            proven_reds(Some(&selected), None, Some(&snapshot), &texts, &claimed),
            None
        );
        assert_eq!(
            proven_reds(
                Some(&selected),
                Some(&BaseTestEvidence::CompilationUnavailable),
                Some(&snapshot),
                &texts,
                &claimed
            ),
            None
        );
        assert_eq!(
            proven_reds(
                Some(&selected),
                Some(&BaseTestEvidence::Observed(BTreeSet::new())),
                Some(&snapshot),
                &texts,
                &claimed
            ),
            None
        );
        assert_eq!(
            proven_reds(Some(&selected), Some(&base), None, &texts, &claimed),
            None
        );
        assert_eq!(
            proven_reds(
                Some(&selected),
                Some(&base),
                Some(&snapshot),
                &BTreeMap::new(),
                &claimed
            ),
            None
        );
        assert_eq!(
            proven_reds(
                Some(&selected),
                Some(&base),
                Some(&snapshot),
                &BTreeMap::from([(RED.to_owned(), "different failure".into())]),
                &claimed
            ),
            None
        );
        assert_eq!(
            proven_reds(
                Some(&selected),
                Some(&base),
                Some(&snapshot),
                &texts,
                &selected
            ),
            None
        );
        let mut closed = snapshot.clone();
        closed.records[0].issue_status = "Closed".into();
        assert_eq!(
            proven_reds(
                Some(&selected),
                Some(&base),
                Some(&closed),
                &texts,
                &claimed
            ),
            None
        );
    }

    #[test]
    fn known_red_timeout_selection_rejects_incomplete_listings() {
        let mut report = rsid::integration::GuardCommandReport {
            program: "cargo".into(),
            args: Vec::new(),
            status: rsid::integration::GuardStatus::Passed,
            stdout_tail: format!("{RED}: test\n\n1 test, 0 benchmarks\n"),
            stderr_tail: String::new(),
            output_truncated: false,
            duration: Duration::ZERO,
        };
        assert_eq!(
            selected_tests(&report, 16384),
            Some(BTreeSet::from([RED.into()]))
        );
        let length = report.stdout_tail.len();
        assert_eq!(
            selected_tests(&report, length + 1),
            Some(BTreeSet::from([RED.into()]))
        );
        assert_eq!(selected_tests(&report, length), None);
        assert_eq!(selected_tests(&report, length - 1), None);
        report.output_truncated = true;
        assert_eq!(selected_tests(&report, 16384), None);
        report.output_truncated = false;
        report.stdout_tail = format!("{RED}: test\n\n2 tests, 0 benchmarks\n");
        assert_eq!(selected_tests(&report, 16384), None);
        report.stdout_tail = format!("running 1 test\nbase_red={RED}\nknown_red={RED}:#3431:env\n");
        assert_eq!(selected_tests(&report, 16384), None);
        report.args = vec!["nextest".into(), "list".into()];
        let mut listing = serde_json::json!({"rust-suites": {"e2e": {"testcases": {
            RED: {"filter-match": {"status": "matches"}, "ignored": false},
            "green": {"filter-match": {"status": "mismatch"}, "ignored": false}
        }}}});
        report.stdout_tail = listing.to_string();
        assert_eq!(
            selected_tests(&report, 16384),
            Some(BTreeSet::from([RED.into()]))
        );
        listing["rust-suites"]["e2e"]["testcases"]["green"]["filter-match"]["status"] =
            "matches".into();
        report.stdout_tail = listing.to_string();
        assert_eq!(
            selected_tests(&report, 16384),
            Some(BTreeSet::from([RED.into(), "green".into()]))
        );
        listing["rust-suites"]["e2e"]["testcases"]["green"]["filter-match"] = Value::Null;
        report.stdout_tail = listing.to_string();
        assert_eq!(selected_tests(&report, 16384), None);
    }

    #[test]
    fn known_red_e2e_timeout_cap_requires_proof_and_keeps_shorter_budgets() {
        let mut e2e = cargo_guard_command("rsi", Some("test:e2e_tui:test(fresh_install::)"));
        e2e.timeout = Duration::from_secs(2400);
        assert_eq!(
            candidate_command(&e2e, true).timeout,
            Duration::from_secs(120)
        );
        assert_eq!(candidate_command(&e2e, false).timeout, e2e.timeout);
        let ordinary = cargo_guard_command("rsi-common", Some("demo::red"));
        assert_eq!(candidate_command(&ordinary, true).timeout, ordinary.timeout);
        e2e.timeout = Duration::from_secs(30);
        assert_eq!(candidate_command(&e2e, true).timeout, e2e.timeout);
    }

    #[test]
    fn known_red_timeout_listing_preserves_the_shards_filters() {
        let command = cargo_guard_command("rsi", Some("test:e2e_tui:test(fresh_install::)"));
        let list = list_command(&command, ShardTree::Static).unwrap();
        assert_eq!(&list.args[..command.args.len()], &command.args);
        assert_eq!(list.args.last().unwrap(), "--list");
        assert_eq!(list.timeout, command.timeout);
        assert!(
            list_command(
                &GuardCommand {
                    program: "unknown".into(),
                    ..command
                },
                ShardTree::Static
            )
            .is_none()
        );
        let command = rsid_lib_filter_command("test(=demo::red)");
        let list = list_command(&command, ShardTree::Static).unwrap();
        assert_eq!(list.args[1], "list");
        assert!(
            list.args
                .windows(2)
                .any(|pair| pair == ["--filterset", "test(=demo::red)"])
        );
    }
}
