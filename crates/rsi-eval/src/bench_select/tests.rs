#![allow(clippy::unwrap_used, clippy::expect_used, clippy::float_cmp)]

use std::collections::BTreeMap;

use rsi_common::bench_record::{EffectiveInputs, SCHEMA_VERSION, Timeouts, Totals};

use super::*;

const SEED: u64 = 0x5253_4942_454e_4348;
const COST: f64 = 0.02;

fn search_tasks() -> Vec<String> {
    (0..30).map(|i| format!("s{i:02}")).collect()
}

fn heldout_tasks() -> Vec<String> {
    (0..59).map(|i| format!("h{i:02}")).collect()
}

fn all_tasks() -> Vec<String> {
    let mut tasks = search_tasks();
    tasks.extend(heldout_tasks());
    tasks
}

fn trial(task: &str, attempt: u32, passed: bool, cost: f64, overlay: &str) -> TrialRow {
    TrialRow {
        attempt,
        build_sha: "b".repeat(40),
        cached_tokens: 10,
        cost_usd: cost,
        exception_type: None,
        failure_cause: None,
        harness_source_digest: "sha256:src".to_string(),
        harness_version_hash: format!("hvh-{task}-{overlay}"),
        image_digest: "sha256:aa".to_string(),
        infra_interrupted: false,
        invalid_route: false,
        output_tokens: 5,
        passed,
        prompt_tokens: 20,
        reasoning_tokens: 0,
        reward: f64::from(u8::from(passed)),
        reward_valid: true,
        session_id: None,
        split: "x".to_string(),
        stop_reason: None,
        read_output_calls: 0,
        spills: 0,
        suspected_solution_lookup: false,
        task: task.to_string(),
        trace_sha256: None,
        turns: 3,
        wall_ms: 1000,
    }
}

type Pass<'a> = &'a dyn Fn(&str, u32) -> bool;

fn record(
    split: &str,
    tasks: &[String],
    attempts: &[u32],
    overlay: &str,
    cost: f64,
    pass: Pass<'_>,
) -> BenchRunRecord {
    let mut trials = Vec::new();
    for task in tasks {
        for &attempt in attempts {
            trials.push(trial(task, attempt, pass(task, attempt), cost, overlay));
        }
    }
    let mut record = BenchRunRecord {
        run: RunInfo {
            adapter_tree_hash: "tree".to_string(),
            build_sha: "b".repeat(40),
            concurrency: 1,
            config_digest: "sha256:cfg".to_string(),
            dataset_commit: "2fd12b88".to_string(),
            dataset_repo: "harbor-framework/terminal-bench-2".to_string(),
            effective_inputs: EffectiveInputs {
                overlay: overlay.to_string(),
                ..EffectiveInputs::default()
            },
            effort: "high".to_string(),
            finished_at: "2026-10-19T13:00:00.000000000Z".to_string(),
            harbor_version: "0.24.0".to_string(),
            harness_build_digest: "sha256:build".to_string(),
            harness_source_digest: "sha256:src".to_string(),
            host: "h".to_string(),
            image_digests: BTreeMap::new(),
            k: u32::try_from(attempts.len()).unwrap(),
            model: "m".to_string(),
            observation_fingerprint: String::new(),
            route: "direct".to_string(),
            sampling: BTreeMap::new(),
            split: split.to_string(),
            started_at: "2026-10-19T12:00:00.000000000Z".to_string(),
            tasks: tasks.to_vec(),
            timeouts: Timeouts {
                agent_sec: 900,
                multiplier: 1.0,
                verifier_sec: 600,
            },
            totals: Totals::default(),
        },
        schema_version: SCHEMA_VERSION,
        trials,
    };
    record.seal();
    record
}

/// Index of a task inside its own split (`s07` is 7).
fn idx(task: &str) -> usize {
    task[1..].parse().unwrap()
}

/// Baseline: even tasks always pass, odd tasks never (so p is 0 or 1 and gains are exact).
fn baseline_pass(task: &str, _attempt: u32) -> bool {
    idx(task).is_multiple_of(2)
}

fn baseline() -> BenchRunRecord {
    record("all", &all_tasks(), &[0, 1, 2], "", COST, &baseline_pass)
}

/// What a synthetic candidate does: odd tasks below `extra` flip to pass, even tasks below
/// `loss` flip to fail.
#[derive(Clone, Copy, Default)]
struct Spec {
    extra: usize,
    loss: usize,
}

fn pass_for(spec: Spec) -> impl Fn(&str, u32) -> bool {
    move |task, _| {
        let i = idx(task);
        if i.is_multiple_of(2) {
            i / 2 >= spec.loss
        } else {
            i / 2 < spec.extra
        }
    }
}

/// How a record of a candidate is damaged for the eligibility tests.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Tweak {
    None,
    InvalidRoute,
    Lookup,
    Costly,
    Infra(usize),
}

fn tweaked(mut record: BenchRunRecord, tweak: Tweak) -> BenchRunRecord {
    match tweak {
        Tweak::None => {}
        Tweak::InvalidRoute => record.trials[0].invalid_route = true,
        Tweak::Lookup => record.trials[0].suspected_solution_lookup = true,
        Tweak::Costly => {
            for t in &mut record.trials {
                t.cost_usd = COST * 1.6;
            }
        }
        Tweak::Infra(n) => {
            for t in record.trials.iter_mut().take(n) {
                t.infra_interrupted = true;
            }
        }
    }
    record.seal();
    record
}

fn screen_record(id: &str, spec: Spec, tweak: Tweak) -> BenchRunRecord {
    tweaked(
        record("search", &search_tasks(), &[0], id, COST, &pass_for(spec)),
        tweak,
    )
}

fn extension_record(id: &str, spec: Spec, tweak: Tweak) -> BenchRunRecord {
    tweaked(
        record(
            "search",
            &search_tasks(),
            &[1, 2],
            id,
            COST,
            &pass_for(spec),
        ),
        tweak,
    )
}

fn validation_record(id: &str, spec: Spec, k: u32) -> BenchRunRecord {
    let attempts: Vec<u32> = (0..k).collect();
    record(
        "held_out",
        &heldout_tasks(),
        &attempts,
        id,
        COST,
        &pass_for(spec),
    )
}

fn screened(id: &str, extra: usize) -> CandidateData {
    CandidateData {
        extension: Vec::new(),
        id: id.to_string(),
        screen: vec![screen_record(id, Spec { extra, loss: 0 }, Tweak::None)],
        validation: Vec::new(),
        verify_clean: true,
    }
}

fn dirty(mut candidate: CandidateData) -> CandidateData {
    candidate.verify_clean = false;
    candidate
}

fn extended(mut candidate: CandidateData, spec: Spec) -> CandidateData {
    candidate.extension = vec![extension_record(&candidate.id, spec, Tweak::None)];
    candidate
}

fn params(spent: f64) -> SelectParams {
    SelectParams {
        spent_usd: spent,
        ..SelectParams::default()
    }
}

fn run(candidates: Vec<CandidateData>, params: SelectParams) -> Selection {
    select(
        &SelectData {
            baseline: vec![baseline()],
            candidates,
            params,
        },
        SEED,
    )
    .unwrap()
}

fn ids(names: &[&str]) -> Vec<String> {
    names.iter().map(|s| (*s).to_string()).collect()
}

fn with_hypothesis_exhausted(spent: f64) -> SelectParams {
    SelectParams {
        no_further_hypothesis: true,
        spent_usd: spent,
        ..SelectParams::default()
    }
}

// ---- statistics ----------------------------------------------------------------------------

#[test]
fn sign_flip_p_is_small_for_a_clear_gain_one_for_none_and_reproducible() {
    assert!(sign_flip_p(&[1.0; 12], SEED) < 0.002);
    assert_eq!(sign_flip_p(&[0.0; 10], SEED), 1.0);
    let symmetric = sign_flip_p(&[1.0, -1.0], SEED);
    assert!((0.70..0.80).contains(&symmetric), "{symmetric}");
    let mixed = [0.5, 0.25, -0.25, 0.75, 0.0, 0.5];
    assert_eq!(sign_flip_p(&mixed, SEED), sign_flip_p(&mixed, SEED));
    assert!(sign_flip_p(&[-1.0; 12], SEED) > 0.99);
}

#[test]
fn the_bootstrap_interval_is_90_percent_clustered_and_reproducible() {
    assert_eq!(bootstrap_interval_90(&[], SEED), None);
    let constant = bootstrap_interval_90(&[0.1; 30], SEED).unwrap();
    assert!((constant.lo - 0.1).abs() < 1e-12 && (constant.hi - 0.1).abs() < 1e-12);
    let diffs: Vec<f64> = (0..30).map(|i| if i < 6 { 1.0 } else { 0.0 }).collect();
    let ci = bootstrap_interval_90(&diffs, SEED).unwrap();
    assert!(
        ci.lo < 0.2 && 0.2 < ci.hi && ci.lo > 0.0 && ci.hi < 0.4,
        "{ci:?}"
    );
    assert_eq!(Some(ci), bootstrap_interval_90(&diffs, SEED));
}

#[test]
fn the_expected_maximum_of_normals_matches_the_known_values() {
    assert_eq!(expected_max_normal(1), 0.0);
    for (n, expected) in [(2, 0.5642), (3, 0.8463), (6, 1.2672), (8, 1.4236)] {
        let got = expected_max_normal(n);
        assert!((got - expected).abs() < 1e-3, "n={n}: {got}");
    }
}

// ---- pooling and pairing -------------------------------------------------------------------

#[test]
fn a_k1_screen_and_a_k2_extension_pool_to_k3() {
    let spec = Spec { extra: 4, loss: 0 };
    let screen = screen_record("c01", spec, Tweak::None);
    let ext = extension_record("c01", spec, Tweak::None);
    let pooled = pool("c01", &[&screen, &ext], true).unwrap();
    assert_eq!(pooled.k(), 3);
    assert_eq!(pooled.trials.len(), 90);
    assert_eq!(pooled.tasks.len(), 30);
    assert_eq!(pool("c01", &[&screen], true).unwrap().k(), 1);
}

#[test]
fn pooling_refuses_reused_attempts_and_incomparable_records() {
    let a = screen_record("c01", Spec::default(), Tweak::None);
    let again = screen_record("c01", Spec::default(), Tweak::None);
    let error = pool("c01", &[&a, &again], true).unwrap_err().to_string();
    assert!(error.contains("appears twice"), "{error}");

    let mut other_model = extension_record("c01", Spec::default(), Tweak::None);
    other_model.run.model = "other".to_string();
    other_model.seal();
    let error = pool("c01", &[&a, &other_model], true)
        .unwrap_err()
        .to_string();
    assert!(error.contains("model"), "{error}");

    let mut other_timeouts = extension_record("c01", Spec::default(), Tweak::None);
    other_timeouts.run.timeouts.agent_sec = 1;
    other_timeouts.seal();
    let error = pool("c01", &[&a, &other_timeouts], true)
        .unwrap_err()
        .to_string();
    assert!(
        error.contains("timeouts") || error.contains("not comparable"),
        "{error}"
    );
}

#[test]
fn a_rerun_replaces_an_infra_interrupted_trial_only() {
    let mut first = screen_record("c01", Spec::default(), Tweak::None);
    first.trials[0].infra_interrupted = true;
    first.seal();
    let mut rerun = record(
        "search",
        &["s00".to_string()],
        &[0],
        "c01",
        COST,
        &|_, _| true,
    );
    rerun.run.tasks = vec!["s00".to_string()];
    rerun.seal();
    let pooled = pool("c01", &[&first, &rerun], true).unwrap();
    assert_eq!(pooled.trials.len(), 30);
    let s00 = pooled.trials.iter().find(|t| t.task == "s00").unwrap();
    assert!(!s00.infra_interrupted && s00.passed);
}

#[test]
fn a_candidate_pairs_with_the_baseline_despite_its_overlay_but_not_another_model() {
    let base = pool("baseline", &[&baseline()], false).unwrap();
    let cand = pool(
        "c01",
        &[&screen_record(
            "c01",
            Spec { extra: 3, loss: 0 },
            Tweak::None,
        )],
        true,
    )
    .unwrap();
    assert_ne!(
        base.run.effective_inputs.overlay,
        cand.run.effective_inputs.overlay
    );
    let stats = paired(&base, &cand, SEED).unwrap();
    assert_eq!(stats.tasks, 30);
    assert!((stats.gain - 0.1).abs() < 1e-9, "{}", stats.gain);

    let mut other = screen_record("c01", Spec::default(), Tweak::None);
    other.run.model = "other".to_string();
    other.seal();
    let other = pool("c01", &[&other], true).unwrap();
    let error = paired(&base, &other, SEED).unwrap_err().to_string();
    assert!(error.contains("model differs"), "{error}");

    let mut unavailable = screen_record("c01", Spec::default(), Tweak::None);
    unavailable.run.harness_source_digest = "unavailable:no git".to_string();
    for t in &mut unavailable.trials {
        t.harness_source_digest = "unavailable:no git".to_string();
    }
    unavailable.seal();
    let unavailable_base = {
        let mut b = baseline();
        b.run.harness_source_digest = "unavailable:no git".to_string();
        for t in &mut b.trials {
            t.harness_source_digest = "unavailable:no git".to_string();
        }
        b.seal();
        b
    };
    let a = pool("b", &[&unavailable_base], false).unwrap();
    let b = pool("c", &[&unavailable], true).unwrap();
    assert!(paired(&a, &b, SEED).is_err());
}

// ---- eligibility ---------------------------------------------------------------------------

fn eligibility_of(verify_clean: bool, tweak: Tweak) -> Eligibility {
    let base = pool("baseline", &[&baseline()], false).unwrap();
    let screen = screen_record("c01", Spec { extra: 3, loss: 0 }, tweak);
    let set = pool("c01", &[&screen], true).unwrap();
    eligibility(verify_clean, &set, &base)
}

#[test]
fn eligibility_requires_every_one_of_the_five_checks() {
    assert!(eligibility_of(true, Tweak::None).eligible);
    for (clean, tweak, needle) in [
        (false, Tweak::None, "verify.json"),
        (true, Tweak::InvalidRoute, "invalid_route"),
        (true, Tweak::Lookup, "solution-lookup"),
        (true, Tweak::Costly, "cost per trial"),
        (true, Tweak::Infra(4), "infra_interrupted"),
    ] {
        let e = eligibility_of(clean, tweak);
        assert!(!e.eligible, "{needle}");
        assert!(
            e.failures.iter().any(|f| f.contains(needle)),
            "{needle}: {e:?}"
        );
    }
}

#[test]
fn eligibility_boundaries_are_inclusive() {
    // 3 of 30 trials interrupted is exactly 10%: eligible; 4 is not.
    assert!(eligibility_of(true, Tweak::Infra(3)).eligible);
    assert!(!eligibility_of(true, Tweak::Infra(4)).eligible);
    // Exactly 1.5x the baseline's cost per trial is eligible.
    let base = pool("baseline", &[&baseline()], false).unwrap();
    let mut screen = screen_record("c01", Spec::default(), Tweak::None);
    for t in &mut screen.trials {
        t.cost_usd = COST * 1.5;
    }
    screen.seal();
    let set = pool("c01", &[&screen], true).unwrap();
    assert!(eligibility(true, &set, &base).eligible);
}

// ---- go / no-go and defer ------------------------------------------------------------------

fn start(c: f64) -> Selection {
    run(
        Vec::new(),
        SelectParams {
            measured_usd_per_trial: Some(c),
            ..SelectParams::default()
        },
    )
}

#[test]
fn the_cost_go_no_go_uses_357c_then_239c_against_the_stop_line() {
    let s = start(0.03);
    assert_eq!(s.cost.go_no_go, GoNoGo::GoK3);
    assert!((s.cost.plan_k3_usd - 357.0 * 0.03).abs() < 1e-9);
    assert!((s.cost.plan_k1_usd - 239.0 * 0.03).abs() < 1e-9);
    assert_eq!((s.action, s.cost.validation_k), (Action::Propose, Some(3)));

    let s = start(0.04);
    assert_eq!(s.cost.go_no_go, GoNoGo::GoK1);
    assert_eq!((s.action, s.cost.validation_k), (Action::Propose, Some(1)));

    let s = start(0.05);
    assert_eq!(s.cost.go_no_go, GoNoGo::Defer);
    assert_eq!((s.action, s.cost.validation_k), (Action::Defer, None));
    assert_eq!(s.action.exit_code(), 14);

    // The thresholds of the plan: $0.0308 and $0.0460.
    assert_eq!(start(0.0308).cost.go_no_go, GoNoGo::GoK3);
    assert_eq!(start(0.0309).cost.go_no_go, GoNoGo::GoK1);
    assert_eq!(start(0.0460).cost.go_no_go, GoNoGo::GoK1);
    assert_eq!(start(0.0461).cost.go_no_go, GoNoGo::Defer);
}

#[test]
fn the_cost_defaults_to_the_baseline_mean_and_the_run_budget_is_1_5x() {
    let s = run(Vec::new(), params(0.0));
    assert_eq!(s.cost.usd_per_trial_source, "baseline");
    assert!((s.cost.usd_per_trial - COST).abs() < 1e-12);
    // propose: 30 trials.
    assert!((s.cost.run_budget_usd.unwrap() - 1.5 * 30.0 * COST).abs() < 1e-9);
}

// ---- stop rules ----------------------------------------------------------------------------

#[test]
fn stop_rule_a_fires_at_eight_screens_and_extension_picks_the_top_two() {
    let cands: Vec<CandidateData> = (1..=8).map(|i| screened(&format!("c{i:02}"), i)).collect();
    let s = run(cands, params(4.8));
    assert!(s.stop.a_max_screens);
    assert_eq!(s.action, Action::Extend);
    assert_eq!(s.extend, ids(&["c08", "c07"]));
    assert_eq!(s.cost.extension_candidates_m, 2);
    assert!((s.cost.run_budget_usd.unwrap() - 1.5 * 60.0 * COST).abs() < 1e-9);
    assert_eq!(s.action.exit_code(), 11);
}

#[test]
fn seven_screens_with_progress_and_budget_keep_proposing() {
    let cands: Vec<CandidateData> = (1..=7).map(|i| screened(&format!("c{i:02}"), i)).collect();
    let s = run(cands, params(4.2));
    assert!(!s.stop.a_max_screens && !s.stop.stop_proposing);
    assert_eq!(s.action, Action::Propose);
    assert_eq!(s.action.exit_code(), 10);
}

#[test]
fn stop_rule_b_fires_when_one_more_screen_would_eat_the_reserve() {
    let four =
        || -> Vec<CandidateData> { (1..=4).map(|i| screened(&format!("c{i:02}"), i)).collect() };
    // Remaining 5.0 at k=3: after one more screen (0.6) 4.4 covers neither two extensions +
    // validation (5.94) nor one extension (1.20) + validation (3.54) = 4.74: rule (b) fires.
    let s = run(four(), params(6.0));
    assert!(s.stop.b_reserve && s.stop.stop_proposing);
    assert_eq!(s.cost.reserve_extensions_m, 1);
    assert!((s.cost.reserve_usd - 4.74).abs() < 1e-9);
    assert_eq!(s.action, Action::Extend);
    // Remaining 7.0: 6.4 >= 5.94 covers two extensions, so the reserve is 5.94: keep proposing.
    let s = run(four(), params(4.0));
    assert!(!s.stop.b_reserve);
    assert_eq!(s.cost.reserve_extensions_m, 2);
    assert!((s.cost.reserve_usd - 5.94).abs() < 1e-9);
    assert_eq!(s.action, Action::Propose);
}

#[test]
fn stop_rule_b_uses_one_extension_when_only_one_fits_after_the_next_screen() {
    let four =
        || -> Vec<CandidateData> { (1..=4).map(|i| screened(&format!("c{i:02}"), i)).collect() };
    // Remaining 6.0: after one more screen 5.4 covers one extension + validation (4.74) but not
    // two (5.94). The preregistered m = 1 reserve applies, so screening goes on.
    let s = run(four(), params(5.0));
    assert!(!s.stop.b_reserve && !s.stop.stop_proposing);
    assert_eq!(s.cost.reserve_extensions_m, 1);
    assert!((s.cost.reserve_usd - 4.74).abs() < 1e-9);
    assert_eq!(s.action, Action::Propose);
    // Remaining 5.34: after 4.74 exactly covers the m = 1 reserve (within EPS): still proposing.
    let s = run(four(), params(5.66));
    assert!(!s.stop.b_reserve);
    assert_eq!(s.action, Action::Propose);
}

#[test]
fn stop_rule_c_needs_four_screens_and_a_last_three_that_did_not_beat_the_best() {
    let screens = |extras: &[usize]| -> Selection {
        run(
            extras
                .iter()
                .enumerate()
                .map(|(i, e)| screened(&format!("c{:02}", i + 1), *e))
                .collect(),
            params(0.6 * extras.len() as f64),
        )
    };
    assert!(screens(&[5, 1, 2, 3]).stop.c_plateau);
    assert!(!screens(&[1, 2, 3, 4]).stop.c_plateau);
    // Only three screens: never.
    assert!(!screens(&[5, 1, 2]).stop.c_plateau);
    // A tie with the earlier best does not beat it.
    assert!(screens(&[3, 3, 3, 3]).stop.c_plateau);
    // The fourth beats the earlier best.
    assert!(!screens(&[3, 1, 1, 4]).stop.c_plateau);
    assert_eq!(screens(&[5, 1, 2, 3]).action, Action::Extend);
}

#[test]
fn an_ineligible_screen_is_never_progress_for_rule_c() {
    let cands = vec![
        screened("c01", 5),
        dirty(screened("c02", 6)),
        dirty(screened("c03", 7)),
        dirty(screened("c04", 8)),
    ];
    let s = run(cands, params(2.4));
    assert!(s.stop.c_plateau);
    // Only the eligible c01 is extended.
    assert_eq!(s.extend, ids(&["c01"]));
}

#[test]
fn stop_rule_d_follows_the_proposer() {
    let s = run(vec![screened("c01", 2)], with_hypothesis_exhausted(0.6));
    assert!(s.stop.d_no_further_hypothesis && s.stop.stop_proposing);
    assert_eq!(s.stop.fired.len(), 1);
    assert_eq!(s.action, Action::Extend);
    assert_eq!(s.extend, ids(&["c01"]));
}

#[test]
fn a_candidate_without_a_screen_is_pending_and_not_counted() {
    let pending = CandidateData {
        extension: Vec::new(),
        id: "c02".to_string(),
        screen: Vec::new(),
        validation: Vec::new(),
        verify_clean: true,
    };
    let s = run(vec![screened("c01", 2), pending], params(0.6));
    assert_eq!(s.stop.screened, 1);
    assert_eq!(s.candidates[1].status, CandidateStatus::Pending);
    assert_eq!(s.action, Action::Propose);
}

// ---- extension -----------------------------------------------------------------------------

#[test]
fn extension_takes_eligible_candidates_with_a_positive_screen_gain_by_gain() {
    let cands = vec![
        screened("c01", 3),
        dirty(screened("c02", 5)),
        screened("c03", 4),
        screened("c04", 0),
        screened("c05", 2),
    ];
    let s = run(cands, with_hypothesis_exhausted(3.0));
    assert_eq!(s.action, Action::Extend);
    assert_eq!(s.extend, ids(&["c03", "c01"]));
}

#[test]
fn extension_count_is_one_when_the_reserve_does_not_allow_two() {
    let four = || {
        vec![
            screened("c01", 3),
            screened("c02", 4),
            screened("c03", 1),
            screened("c04", 2),
        ]
    };
    // 237c = 4.74 (one extension + k=3 validation) fits 5.5, but 297c = 5.94 (two) does not: m = 1.
    let s = run(four(), with_hypothesis_exhausted(5.5));
    assert_eq!(s.cost.go_no_go, GoNoGo::GoK3);
    assert_eq!(s.cost.extension_candidates_m, 1);
    assert_eq!(s.extend, ids(&["c02"]));
    // Plenty of budget: m = 2.
    let s = run(four(), with_hypothesis_exhausted(2.4));
    assert_eq!(s.cost.extension_candidates_m, 2);
    assert_eq!(s.extend, ids(&["c02", "c01"]));
}

#[test]
fn no_positive_eligible_screen_means_stop() {
    let s = run(
        vec![screened("c01", 0), dirty(screened("c02", 4))],
        with_hypothesis_exhausted(1.2),
    );
    assert_eq!(s.action, Action::Stop);
    assert_eq!(
        s.outcome.as_deref(),
        Some("no_candidate_beat_baseline_on_screen")
    );
    assert_eq!(s.action.exit_code(), 13);
}

#[test]
fn a_partial_extension_asks_for_the_rest() {
    let cands = vec![
        extended(screened("c01", 3), Spec { extra: 3, loss: 0 }),
        screened("c02", 4),
    ];
    // c01 was extended, c02 is still pending, and c02 ranks first: it is in the set (m = 2).
    let s = run(cands, with_hypothesis_exhausted(2.4));
    assert_eq!(s.action, Action::Extend);
    assert_eq!(s.extend, ids(&["c02"]));
}

#[test]
fn extension_records_outside_the_preregistered_set_are_refused() {
    let cands = vec![
        screened("c01", 5),
        extended(screened("c02", 0), Spec { extra: 4, loss: 0 }),
    ];
    let error = select(
        &SelectData {
            baseline: vec![baseline()],
            candidates: cands,
            params: with_hypothesis_exhausted(1.8),
        },
        SEED,
    )
    .unwrap_err()
    .to_string();
    assert!(error.contains("preregistered extension set"), "{error}");
}

// ---- winner and promotion ------------------------------------------------------------------

fn two_extended(fresh_c1: usize, fresh_c2: usize) -> Vec<CandidateData> {
    vec![
        extended(
            screened("c01", 6),
            Spec {
                extra: fresh_c1,
                loss: 0,
            },
        ),
        extended(
            screened("c02", 4),
            Spec {
                extra: fresh_c2,
                loss: 0,
            },
        ),
    ]
}

#[test]
fn the_winner_has_the_highest_pooled_gain_and_is_promoted_on_a_positive_fresh_gain() {
    // c01: screen 6, fresh 0 -> pooled 6/3 = 2 tasks worth 6.7; c02: screen 4, fresh 4 -> 13.3.
    let s = run(two_extended(0, 4), with_hypothesis_exhausted(3.6));
    let winner = s.winner.as_ref().unwrap();
    assert_eq!(winner.id, "c02");
    assert!(winner.promoted);
    assert!((winner.pooled_gain - 4.0 / 30.0).abs() < 1e-9);
    assert!((winner.fresh_gain - 4.0 / 30.0).abs() < 1e-9);
    assert_eq!(winner.extended, ids(&["c01", "c02"]));
    assert_eq!(s.action, Action::Validate);
    assert_eq!(s.cost.validation_k, Some(3));
    // 177 held-out trials at 1.5x the baseline cost.
    assert!((s.cost.run_budget_usd.unwrap() - 1.5 * 177.0 * COST).abs() < 1e-9);
    assert_eq!(s.action.exit_code(), 12);
    assert_eq!(s.candidates[0].pooled_k, Some(3));
    let fresh = s.candidates[1].extension_fresh.as_ref().unwrap();
    assert_eq!(fresh.tasks, 30);
}

#[test]
fn the_best_of_n_note_scales_the_winners_se_by_the_expected_maximum() {
    let s = run(two_extended(0, 4), with_hypothesis_exhausted(3.6));
    let note = &s.winner.as_ref().unwrap().best_of_n;
    assert_eq!(note.n, 2);
    assert!((note.expected_max_z - expected_max_normal(2)).abs() < 1e-12);
    let pooled = s.candidates[1].pooled.as_ref().unwrap();
    assert_eq!(note.se, pooled.se);
    assert!((note.noise_floor.unwrap() - pooled.se.unwrap() * note.expected_max_z).abs() < 1e-12);
    let md = render_markdown(&s);
    assert!(md.contains("Best-of-N note"), "{md}");
}

#[test]
fn a_winner_whose_fresh_extension_gain_is_not_above_zero_is_not_promoted() {
    // c01 alone: screen +6 tasks at attempt 0, nothing on the two fresh attempts.
    let s = run(
        vec![extended(screened("c01", 6), Spec::default())],
        with_hypothesis_exhausted(1.8),
    );
    let winner = s.winner.as_ref().unwrap();
    assert!(!winner.promoted && winner.fresh_gain.abs() < 1e-12);
    assert!(winner.pooled_gain > 0.0);
    assert_eq!(s.action, Action::Stop);
    assert_eq!(
        s.outcome.as_deref(),
        Some("no_candidate_replicated_on_search")
    );
    assert!(s.reason.contains("no candidate replicated on search"));
}

#[test]
fn an_extended_candidate_that_turns_ineligible_cannot_win() {
    let mut c = extended(screened("c01", 6), Spec { extra: 6, loss: 0 });
    c.extension = vec![extension_record(
        "c01",
        Spec { extra: 6, loss: 0 },
        Tweak::InvalidRoute,
    )];
    let s = run(vec![c], with_hypothesis_exhausted(1.8));
    assert!(s.winner.is_none());
    assert_eq!(s.action, Action::Stop);
    assert_eq!(s.outcome.as_deref(), Some("no_eligible_extended_candidate"));
}

#[test]
fn a_promoted_winner_defers_when_validation_no_longer_fits() {
    let s = run(
        vec![extended(screened("c01", 6), Spec { extra: 6, loss: 0 })],
        with_hypothesis_exhausted(10.9),
    );
    assert!(s.winner.as_ref().unwrap().promoted);
    assert_eq!(s.action, Action::Defer);
    assert_eq!(s.cost.validation_k, None);
}

// ---- validation ----------------------------------------------------------------------------

fn validated_scenario(valid: Spec, k: u32) -> Selection {
    let mut cands = two_extended(0, 4);
    cands[1].validation = vec![validation_record("c02", valid, k)];
    run(cands, with_hypothesis_exhausted(7.0))
}

#[test]
fn validated_needs_a_positive_held_out_gain_and_p_below_ten_percent() {
    // 8 of 59 tasks flip: gain +13.6, p about 1/256.
    let s = validated_scenario(Spec { extra: 8, loss: 0 }, 1);
    let v = s.validation.as_ref().unwrap();
    assert!(
        v.validated && v.stats.p_value < 0.01,
        "{:?}",
        v.stats.p_value
    );
    assert!((v.stats.gain - 8.0 / 59.0).abs() < 1e-9);
    assert!(v.stats.ci90.is_some());
    assert_eq!(
        (s.action, s.outcome.as_deref()),
        (Action::Stop, Some("validated"))
    );
}

#[test]
fn a_positive_but_noisy_held_out_gain_is_not_validated_and_is_still_reported() {
    // 2 flips: p = 1/4.
    let s = validated_scenario(Spec { extra: 2, loss: 0 }, 1);
    let v = s.validation.as_ref().unwrap();
    assert!(!v.validated && v.stats.gain > 0.0 && v.stats.p_value > 0.10);
    assert_eq!(s.outcome.as_deref(), Some("not_validated"));
    assert!(render_markdown(&s).contains("not validated"));

    // A regression is reported as measured.
    let s = validated_scenario(Spec { extra: 0, loss: 3 }, 3);
    let v = s.validation.as_ref().unwrap();
    assert!(!v.validated && v.stats.gain < 0.0);
    assert!(v.stats.ci90.is_some());
}

#[test]
fn validation_records_must_belong_to_the_promoted_winner_and_the_held_out_split() {
    let mut cands = two_extended(0, 4);
    // c01 is not the winner.
    cands[0].validation = vec![validation_record("c01", Spec { extra: 8, loss: 0 }, 1)];
    let error = select(
        &SelectData {
            baseline: vec![baseline()],
            candidates: cands,
            params: with_hypothesis_exhausted(7.0),
        },
        SEED,
    )
    .unwrap_err()
    .to_string();
    assert!(error.contains("not the promoted winner"), "{error}");

    let mut cands = two_extended(0, 4);
    cands[1].validation = vec![record(
        "held_out",
        &search_tasks(),
        &[3],
        "c02",
        COST,
        &pass_for(Spec { extra: 8, loss: 0 }),
    )];
    let error = select(
        &SelectData {
            baseline: vec![baseline()],
            candidates: cands,
            params: with_hypothesis_exhausted(7.0),
        },
        SEED,
    )
    .unwrap_err()
    .to_string();
    assert!(error.contains("also a search task"), "{error}");
}

// ---- MDE -----------------------------------------------------------------------------------

#[test]
fn the_mde_table_uses_the_baseline_pass_rate_variance_and_the_chosen_k() {
    // Tasks whose index (in the combined list) is divisible by 3 pass once in three attempts.
    let tasks = all_tasks();
    let mixed: Vec<bool> = (0..tasks.len()).map(|i| i % 3 == 0).collect();
    let by_task: BTreeMap<String, bool> =
        tasks.iter().cloned().zip(mixed.iter().copied()).collect();
    let pass = move |task: &str, attempt: u32| {
        if by_task[task] { attempt == 0 } else { false }
    };
    let base = record("all", &tasks, &[0, 1, 2], "", COST, &pass);
    let s = select(
        &SelectData {
            baseline: vec![base],
            candidates: Vec::new(),
            params: SelectParams {
                measured_usd_per_trial: Some(0.03),
                ..SelectParams::default()
            },
        },
        SEED,
    )
    .unwrap();
    // p = 1/3 on those tasks: p(1-p) = 2/9, unbiased at n = 3 by 3/2 = 1/3.
    let share = mixed.iter().filter(|m| **m).count() as f64 / mixed.len() as f64;
    let pq = share / 3.0;
    assert_eq!(s.mde.pq_source, "baseline");
    assert!((s.mde.pq - pq).abs() < 1e-12);
    let mde = |n: f64, kc: f64| 2.486 * (pq * (1.0 / 3.0 + 1.0 / kc) / n).sqrt();
    assert_eq!(s.mde.rows.len(), 3);
    assert!((s.mde.rows[0].mde - mde(30.0, 1.0)).abs() < 1e-12);
    assert!((s.mde.rows[1].mde - mde(30.0, 3.0)).abs() < 1e-12);
    // 0.03 per trial: go with k=3.
    assert_eq!(s.mde.rows[2].candidate_k, 3);
    assert!((s.mde.rows[2].mde - mde(59.0, 3.0)).abs() < 1e-12);
}

#[test]
fn the_mde_falls_back_to_the_plans_variance_when_the_baseline_has_one_trial_per_task() {
    let base = record("all", &all_tasks(), &[0], "", COST, &baseline_pass);
    let s = select(
        &SelectData {
            baseline: vec![base],
            candidates: Vec::new(),
            params: SelectParams::default(),
        },
        SEED,
    )
    .unwrap();
    assert_eq!((s.mde.pq_source.as_str(), s.mde.pq), ("default", 0.14));
}

// ---- reproducibility and the CLI -----------------------------------------------------------

#[test]
fn numbers_reproduce_with_the_fixed_seed() {
    let make = || run(two_extended(1, 4), with_hypothesis_exhausted(3.6));
    let (a, b) = (make(), make());
    assert_eq!(a, b);
    assert_eq!(
        serde_json::to_string(&a).unwrap(),
        serde_json::to_string(&b).unwrap()
    );
    assert_eq!((a.flips, a.bootstrap_resamples), (10_000, 10_000));
}

#[test]
fn the_state_file_drives_the_cli_and_the_exit_code_names_the_action() {
    let dir = tempfile::tempdir().unwrap();
    let write = |name: &str, record: &BenchRunRecord| {
        std::fs::write(dir.path().join(name), record.to_json().unwrap()).unwrap();
    };
    write("baseline.json", &baseline());
    write(
        "c01-screen.json",
        &screen_record("c01", Spec { extra: 3, loss: 0 }, Tweak::None),
    );
    let state = |extra: &str| {
        format!(
            r#"{{"baseline_records":["baseline.json"],"spent_usd":0.6,{extra}
               "candidates":[{{"id":"c01","verify_clean":true,"screen":["c01-screen.json"]}}]}}"#
        )
    };
    std::fs::write(dir.path().join("state.json"), state("")).unwrap();
    let out = dir.path().join("select.json");
    let action = run_action(
        &dir.path().join("state.json"),
        SEED,
        SelectFormat::Json,
        Some(&out),
    )
    .unwrap();
    assert_eq!(action, Action::Propose);
    let json: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&out).unwrap()).unwrap();
    assert_eq!(json["action"], "propose");
    assert_eq!(json["seed"], SEED);

    std::fs::write(
        dir.path().join("state.json"),
        state(r#""no_further_hypothesis":true,"#),
    )
    .unwrap();
    let action = run_action(
        &dir.path().join("state.json"),
        SEED,
        SelectFormat::Markdown,
        None,
    )
    .unwrap();
    assert_eq!((action, action.exit_code()), (Action::Extend, 11));

    std::fs::write(
        dir.path().join("state.json"),
        r#"{"baseline_records":[],"unknown":1,"candidates":[]}"#,
    )
    .unwrap();
    assert!(
        run_action(
            &dir.path().join("state.json"),
            SEED,
            SelectFormat::Json,
            None
        )
        .is_err()
    );
}

#[test]
fn the_markdown_report_names_the_action_candidates_stop_rules_cost_and_mde() {
    let s = run(two_extended(0, 4), with_hypothesis_exhausted(3.6));
    let md = render_markdown(&s);
    for needle in [
        "**Next action**: `validate` (exit 12)",
        "## Candidates (search, exploratory)",
        "| c02 |",
        "## Stop rules",
        "## Cost go/no-go",
        "plan with k=3 validation",
        "## MDE",
        "## Winner",
        "Suggested run `budget_usd`",
    ] {
        assert!(md.contains(needle), "{needle}\n{md}");
    }
}
