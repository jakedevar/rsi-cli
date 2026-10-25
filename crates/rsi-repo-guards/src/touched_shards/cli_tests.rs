//! The command line contract: explicit verifier modes, refusals, receipt output
//! and the cargo prefix. Every Git call goes through a temporary repository.
use super::derive::{self, Failure, Options};
use super::derive_tests::{Case, gate};
use super::modes;
use crate::Ctx;
use serde_json::{Value, json};
use std::path::Path;

const MANIFEST: &str =
    "[features]\ndefault = []\ntest-shard-mode = []\ntest-shard-store-02 = [\"test-shard-mode\"]\n";
const STORE: &str =
    "[features]\ndefault = []\ntest-shard-mode = []\ntest-shard-store-01 = [\"test-shard-mode\"]\n";

struct Cli {
    case: Case,
    bin: std::path::PathBuf,
    log: std::path::PathBuf,
}

/// A repository whose change touches one `store-02` test, plus a cargo stub that
/// records each call and refuses everything except metadata (and `check` on request).
fn cli() -> Cli {
    let mut case = Case::empty();
    case.manifests = true;
    case.write("crates/rsid/Cargo.toml", MANIFEST);
    case.write("crates/rsid-store/Cargo.toml", STORE);
    case.write("crates/rsid/src/lib.rs", "mod foo;\n");
    case.write("crates/rsid-store/src/lib.rs", &gate("store-01", "s"));
    case.write("crates/rsid/src/foo.rs", &gate("store-02", "a"));
    case.commit();
    case.rebase_base_here();
    case.write(
        "crates/rsid/src/foo.rs",
        &(gate("store-02", "a") + "pub fn changed() {}\n"),
    );
    case.commit();
    let dir = case.fx.path().join(".stub");
    std::fs::create_dir_all(&dir).unwrap();
    let log = dir.join("cargo.log");
    let stub = dir.join("cargo-stub");
    std::fs::write(
        &stub,
        format!(
            "#!/bin/sh\necho \"$*\" >> '{}'\ncase \"$*\" in \"metadata --no-deps --offline --format-version 1\") echo '{{\"target_directory\": \"/nonexistent-target\"}}'; exit 0;; esac\nif [ -e '{allow}' ]; then case \"$1\" in check) exit 0;; esac; fi\necho \"stub cargo: refusing $*\" >&2\nexit 97\n",
            log.display(),
            allow = dir.join("allow").display()
        ),
    )
    .unwrap();
    #[cfg(unix)]
    std::fs::set_permissions(&stub, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
    Cli {
        case,
        bin: stub,
        log,
    }
}

impl Cli {
    fn run(&self, args: &[&str], extra: &[(&str, &str)]) -> (i32, String, String) {
        let mut vars = vec![
            ("RSI_TOUCHED_SHARDS_STAGED", "1"),
            ("CHECK_TOUCHED_SHARDS_CARGO", self.bin.to_str().unwrap()),
        ];
        vars.extend_from_slice(extra);
        let ctx = Ctx::new(self.case.fx.path(), &vars);
        let mut argv: Vec<String> = [
            "--repo",
            self.case.fx.path().to_str().unwrap(),
            "--base",
            &self.case.base,
            "--head",
            &self.case.head,
        ]
        .iter()
        .map(|s| (*s).to_owned())
        .collect();
        argv.extend(args.iter().map(|s| (*s).to_owned()));
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let code = super::main(&argv, &ctx, &mut out, &mut err);
        (
            code,
            String::from_utf8(out).unwrap(),
            String::from_utf8(err).unwrap(),
        )
    }
    fn cargo_calls(&self) -> Vec<String> {
        std::fs::read_to_string(&self.log)
            .map(|t| t.lines().map(str::to_owned).collect())
            .unwrap_or_default()
    }
}

#[test]
fn filters_only_plans_the_filters_without_any_cargo_call() {
    let cli = cli();
    let (code, out, err) = cli.run(&["--filters-only", "--json"], &[]);
    assert_eq!(code, 0, "{err}");
    let receipt: Value = serde_json::from_str(&out).unwrap();
    assert!(cli.cargo_calls().is_empty());
    assert!(
        !receipt["suggested_filters"].as_array().unwrap().is_empty(),
        "{receipt}"
    );
    assert_eq!(
        receipt["compile"],
        json!({"mode": "filters_only", "status": "skipped", "cargo_check_invoked": false, "shards_checked": [],
               "detail": "planning only: compile validation belongs to the supervised scoped_test job"})
    );
}

#[test]
fn no_compile_maps_the_shards_without_any_cargo_call_and_says_skipped() {
    let cli = cli();
    let (code, out, err) = cli.run(&["--no-compile", "--json"], &[]);
    assert_eq!(code, 0, "{err}");
    let receipt: Value = serde_json::from_str(&out).unwrap();
    assert!(cli.cargo_calls().is_empty());
    assert_eq!(
        receipt["shards"]["map"],
        json!({"store-02": ["crates/rsid/src/foo.rs"]})
    );
    assert_eq!(
        (&receipt["compile"]["mode"], &receipt["compile"]["status"]),
        (&json!("no_compile"), &json!("skipped"))
    );
    assert_eq!(receipt["shards"]["compile_skipped"], json!(true));
}

#[test]
fn a_bare_run_is_refused_instead_of_silently_compiling() {
    let cli = cli();
    let (code, out, err) = cli.run(&[], &[]);
    assert_eq!(code, 2, "{out}");
    assert!(err.contains("explicit verifier mode"), "{err}");
    assert!(err.contains("--filters-only"));
    assert_eq!(out, "");
    assert!(cli.cargo_calls().is_empty());
}

#[test]
fn compile_cannot_be_combined_with_a_planning_mode() {
    for planning in ["--filters-only", "--no-compile"] {
        let cli = cli();
        let (code, out, _) = cli.run(&["--compile", planning], &[]);
        assert_eq!(code, 2, "{planning}: {out}");
        assert!(cli.cargo_calls().is_empty());
    }
}

#[test]
fn compile_mode_runs_the_shard_check_and_labels_it_executed() {
    let cli = cli();
    std::fs::write(cli.bin.parent().unwrap().join("allow"), "").unwrap();
    let (code, out, err) = cli.run(&["--compile", "--json"], &[]);
    assert_eq!(code, 0, "{err}");
    let receipt: Value = serde_json::from_str(&out).unwrap();
    let calls = cli.cargo_calls();
    assert_eq!(
        calls,
        ["check -p rsid --lib --tests --no-default-features --features test-shard-store-02"]
    );
    assert_eq!(
        receipt["compile"],
        json!({"mode": "compile", "status": "executed", "cargo_check_invoked": true, "shards_checked": ["store-02"]})
    );
    assert_eq!(receipt["shards"]["compiled_ok"], json!(["store-02"]));
}

#[test]
fn a_failing_compile_still_fails_the_gate() {
    let cli = cli();
    let (code, out, _) = cli.run(&["--compile", "--json"], &[]);
    assert_eq!(code, 1, "{out}");
    let receipt: Value = serde_json::from_str(&out).unwrap();
    assert_eq!(receipt["ok"], json!(false));
    assert_eq!(receipt["shards"]["compiled_failed"], json!(["store-02"]));
    assert_eq!(receipt["compile"]["status"], json!("executed"));
}

#[test]
fn the_staged_derivation_stays_closed_without_its_opt_in() {
    let cli = cli();
    let ctx = Ctx::new(cli.case.fx.path(), &[]);
    let argv: Vec<String> = [
        "--repo",
        cli.case.fx.path().to_str().unwrap(),
        "--base",
        &cli.case.base,
        "--head",
        &cli.case.head,
        "--filters-only",
        "--json",
    ]
    .iter()
    .map(|s| (*s).to_owned())
    .collect();
    let (mut out, mut err) = (Vec::new(), Vec::new());
    assert_eq!(super::main(&argv, &ctx, &mut out, &mut err), 2);
    assert!(out.is_empty());
    assert!(
        String::from_utf8(err)
            .unwrap()
            .contains("incomplete_contract")
    );
}

#[test]
fn the_receipt_file_and_line_carry_the_same_final_receipt() {
    let cli = cli();
    let path = cli.case.fx.path().join("out/receipt.json");
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let (code, out, err) = cli.run(
        &[
            "--filters-only",
            "--receipt-line",
            "--receipt",
            path.to_str().unwrap(),
        ],
        &[],
    );
    assert_eq!(code, 0, "{err}");
    let last = out.lines().last().unwrap();
    let line: Value = serde_json::from_str(last.strip_prefix("RECEIPT_JSON ").unwrap()).unwrap();
    let file: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    assert_eq!(line["partial"], json!(false));
    assert_eq!(line, file);
    assert!(
        out.lines()
            .next()
            .unwrap()
            .starts_with("check-touched-shards OK")
    );
    assert!(!path.with_file_name("receipt.json.partial").exists());
}

fn prefix(vars: &[(&str, &str)]) -> Vec<String> {
    modes::cargo_prefix(&Ctx::new(".", vars))
}

#[test]
fn held_slot_is_not_acquired_again() {
    let home = tempfile::tempdir().unwrap();
    let slot = home.path().join(".rsi/bin/cargo-slot");
    std::fs::create_dir_all(slot.parent().unwrap()).unwrap();
    std::fs::write(&slot, "#!/bin/sh\n").unwrap();
    let held = prefix(&[
        ("RSI_CARGO_SLOT_HELD", "1"),
        ("HOME", home.path().to_str().unwrap()),
    ]);
    assert_eq!(held.last().map(String::as_str), Some("cargo"));
    assert!(!held.iter().any(|p| p.ends_with("cargo-slot")));
    let free = prefix(&[("HOME", home.path().to_str().unwrap())]);
    assert_eq!(free[0], slot.to_string_lossy());
    assert_eq!(
        &free[1..],
        ["env", "-u", "RSI_PROCESS_OWNERSHIP_NAMESPACE", "cargo"]
    );
}

#[test]
fn an_override_replaces_the_prefix_entirely() {
    assert_eq!(
        prefix(&[("CHECK_TOUCHED_SHARDS_CARGO", "true --flag")]),
        ["true", "--flag"]
    );
}

#[test]
fn a_killed_run_leaves_a_partial_receipt_of_the_shards_it_finished() {
    // #1718: a unit budget that SIGTERMs the check mid-shard must not discard the shards
    // that already compiled; a successor resumes only the pending ones.
    let mut cli = cli();
    cli.case.write(
        "crates/rsid/Cargo.toml",
        &format!("{MANIFEST}test-shard-other-03 = [\"test-shard-mode\"]\n"),
    );
    cli.case
        .write("crates/rsid/src/bar.rs", &gate("other-03", "b"));
    cli.case.commit();
    cli.case.rebase_base_here();
    cli.case.write(
        "crates/rsid/src/bar.rs",
        &(gate("other-03", "b") + "const EDIT: u8 = 1;\n"),
    );
    cli.case.write(
        "crates/rsid/src/foo.rs",
        &(gate("store-02", "a") + "const EDIT: u8 = 2;\n"),
    );
    cli.case.commit();
    let dir = cli.case.fx.path().join(".sleeper");
    std::fs::create_dir_all(&dir).unwrap();
    let log = dir.join("cargo.log");
    let script = dir.join("cargo.sh");
    std::fs::write(
        &script,
        format!("#!/bin/sh\necho \"$@\" >> '{}'\ncase \"$*\" in *test-shard-store-02*) exec sleep 60;; esac\n", log.display()),
    )
    .unwrap();
    #[cfg(unix)]
    std::fs::set_permissions(&script, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
    cli.case.cargo = script.to_string_lossy().into_owned();
    let flag = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let trigger = flag.clone();
    let watch = log.clone();
    let watcher = std::thread::spawn(move || {
        for _ in 0..1200 {
            if std::fs::read_to_string(&watch).is_ok_and(|t| t.contains("test-shard-store-02")) {
                trigger.store(true, std::sync::atomic::Ordering::SeqCst);
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
    });
    let loaded = cli.case.load(&cli.case.base, &cli.case.head).unwrap();
    let receipt_path = cli.case.fx.path().join("partial.json");
    let options = Options {
        compile: true,
        env: Some(vec![(
            "CHECK_TOUCHED_SHARDS_CARGO".into(),
            cli.case.cargo.clone(),
        )]),
        interrupt: Some(flag),
        receipt: Some(receipt_path.clone()),
        ..Options::default()
    };
    let Err(Failure::Interrupted {
        code,
        partial,
        message,
    }) = derive::derive(&loaded, &options)
    else {
        panic!("the run was not interrupted");
    };
    watcher.join().unwrap();
    assert_eq!(code, 128 + 15);
    assert_eq!(partial["partial"], json!(true));
    assert_eq!(partial["phase"], json!("interrupted"));
    assert_eq!(partial["shards"]["compiled_ok"], json!(["other-03"]));
    assert_eq!(partial["shards"]["compile_pending"], json!(["store-02"]));
    assert_eq!(
        partial["shards"]["runs"]
            .as_object()
            .unwrap()
            .keys()
            .collect::<Vec<_>>(),
        ["other-03"]
    );
    assert!(message.contains("interrupted by signal"));
    let on_disk: Value =
        serde_json::from_str(&std::fs::read_to_string(Path::new(&receipt_path)).unwrap()).unwrap();
    assert_eq!(on_disk, partial);
}

#[test]
fn the_slow_tier_decision_prints_as_sorted_python_json() {
    let cli = cli();
    let (code, out, err) = cli.run(&["--slow-tier-only"], &[]);
    assert_eq!(code, 0, "{err}");
    assert_eq!(
        out,
        "{\"mode\": \"full\", \"reason\": \"no_slow_tier_config\", \"tests\": 0}\n"
    );
}

#[test]
fn receipts_escape_non_ascii_like_the_legacy_writer() {
    assert_eq!(
        modes::python_json(&json!({"k": "é😀"}), false),
        "{\"k\":\"\\u00e9\\ud83d\\ude00\"}"
    );
    assert_eq!(
        modes::dumps_sorted(&json!({"b": 1, "a": [1, 2]})),
        "{\"a\": [1, 2], \"b\": 1}"
    );
}
