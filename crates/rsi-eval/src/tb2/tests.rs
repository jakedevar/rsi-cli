//! Ports of the former TB2 unittest suite, using only native Rust runners.
#![allow(clippy::unwrap_used, clippy::expect_used)]
use super::*;
use audit::{Finding, Item, Terms};
use dataset::Task;
use std::{
    collections::{BTreeMap, BTreeSet},
    path::Path,
};

fn repo() -> PathBuf {
    PathBuf::from(rsi_common::manifest_dir!()).join("../..")
}
fn pinned() -> dataset::Dataset {
    dataset::load(&repo().join(DATASET)).unwrap()
}
fn row(name: &str) -> Task {
    Task {
        name: name.into(),
        difficulty: "medium".into(),
        category: "c".into(),
        cpus: 1,
        memory_mb: 2048,
        agent_timeout_sec: 900.0,
        verifier_timeout_sec: 900.0,
        image: format!("x/{name}:1"),
        image_digest: format!("sha256:{}", "a".repeat(64)),
        image_bytes: 100,
        task_toml_sha256: "0".repeat(64),
    }
}
fn names(list: &[String]) -> BTreeSet<&str> {
    list.iter().map(String::as_str).collect()
}
fn words(text: &str) -> Vec<String> {
    let mut v: Vec<_> = text.split_whitespace().map(str::to_owned).collect();
    v.sort();
    v
}

#[test]
fn header_and_every_task_are_fully_pinned() {
    let d = pinned();
    assert_eq!(d.dataset.commit, dataset::COMMIT);
    assert_eq!(d.dataset.repo, dataset::REPO);
    assert_eq!(d.dataset.harbor_version, "0.24.0");
    assert_eq!((d.dataset.task_count, d.task.len()), (89, 89));
    let digest = regex::Regex::new(r"^sha256:[0-9a-f]{64}$").unwrap();
    let hash = regex::Regex::new(r"^[0-9a-f]{64}$").unwrap();
    let mut difficulties = BTreeMap::new();
    for t in &d.task {
        assert!(digest.is_match(&t.image_digest), "{}", t.name);
        assert!(hash.is_match(&t.task_toml_sha256), "{}", t.name);
        assert!(t.image.starts_with("alexgshaw/"));
        assert!(t.image_bytes > 0 && t.agent_timeout_sec > 0.0 && t.verifier_timeout_sec > 0.0);
        *difficulties.entry(t.difficulty.as_str()).or_insert(0) += 1;
    }
    assert_eq!(
        difficulties,
        BTreeMap::from([("easy", 4), ("medium", 55), ("hard", 30)])
    );
    assert_eq!(
        d.task
            .iter()
            .map(|t| t.agent_timeout_sec)
            .fold(0.0, f64::max),
        12000.0
    );
    assert_eq!(
        d.task
            .iter()
            .find(|t| t.name == "build-pov-ray")
            .unwrap()
            .agent_timeout_sec,
        12000.0
    );
    assert_eq!(
        (d.task.iter().map(|t| t.image_bytes).sum::<u64>() as f64 / 1e8).round(),
        490.0
    );
}

#[test]
fn split_is_deterministic_and_matches_all_plan_lists_and_committed_bytes() {
    let mut tasks = pinned().task;
    let first = split::make_split(&tasks, 3135).unwrap();
    tasks.reverse();
    assert_eq!(first, split::make_split(&tasks, 3135).unwrap());
    assert_eq!(first.seed, 3135);
    assert_eq!(first.algorithm, "tb2-split-v1");
    assert_eq!(
        first.search,
        words(
            "break-filter-js-from-html caffe-cifar-10 code-from-image constraints-scheduling db-wal-recovery dna-assembly extract-elf extract-moves-from-video feal-differential-cryptanalysis financial-document-processor fix-git git-leak-recovery git-multibranch headless-terminal hf-model-inference install-windows-3.11 merge-diff-arc-agi-task multi-source-data-merger openssl-selfsigned-cert password-recovery path-tracing-reverse polyglot-rust-c pytorch-model-recovery raman-fitting regex-chess rstan-to-pystan sparql-university sqlite-with-gcov torch-pipeline-parallelism winning-avg-corewars"
        )
    );
    assert_eq!(
        first.held_out,
        words(
            "adaptive-rejection-sampler bn-fit-modify build-cython-ext build-pmars build-pov-ray cancel-async-tasks chess-best-move circuit-fibsqrt cobol-modernization compile-compcert configure-git-webserver count-dataset-tokens crack-7z-hash custom-memory-heap-crash distribution-search dna-insert feal-linear-cryptanalysis filter-js-from-html fix-code-vulnerability fix-ocaml-gc gcode-to-text gpt2-codegolf kv-store-grpc large-scale-text-editing largest-eigenval llm-inference-batching-scheduler log-summary-date-ranges mailman make-doom-for-mips make-mips-interpreter mcmc-sampling-stan model-extraction-relu-logits modernize-scientific-stack mteb-leaderboard mteb-retrieve nginx-request-logging overfull-hbox path-tracing polyglot-c-py portfolio-optimization protein-assembly prove-plus-comm pypi-server pytorch-model-cli qemu-alpine-ssh qemu-startup query-optimize regex-log reshard-c4-data sam-cell-seg sanitize-git-repo schemelike-metacircular-eval sqlite-db-truncate torch-tensor-parallelism train-fasttext tune-mjcf video-processing vulnerable-secret write-compressor"
        )
    );
    assert_eq!(
        first.smoke,
        words(
            "merge-diff-arc-agi-task openssl-selfsigned-cert sparql-university sqlite-with-gcov torch-pipeline-parallelism"
        )
    );
    assert_eq!(
        first.calibration,
        words(
            "break-filter-js-from-html caffe-cifar-10 dna-assembly extract-elf git-multibranch hf-model-inference password-recovery polyglot-rust-c raman-fitting rstan-to-pystan"
        )
    );
    assert_eq!(
        first.text().unwrap(),
        fs::read_to_string(repo().join(SPLIT)).unwrap()
    );
}

#[test]
fn split_shape_and_stratification() {
    let tasks = pinned().task;
    let s = split::make_split(&tasks, 3135).unwrap();
    assert_eq!(
        (
            s.search.len(),
            s.held_out.len(),
            s.smoke.len(),
            s.calibration.len()
        ),
        (30, 59, 5, 10)
    );
    assert!(names(&s.search).is_disjoint(&names(&s.held_out)));
    assert_eq!(
        names(&s.search)
            .union(&names(&s.held_out))
            .copied()
            .collect::<BTreeSet<_>>(),
        tasks.iter().map(|t| t.name.as_str()).collect()
    );
    assert!(names(&s.smoke).is_subset(&names(&s.search)));
    assert!(names(&s.calibration).is_subset(&names(&s.search)));
    assert!(names(&s.smoke).is_disjoint(&names(&s.calibration)));
    let count = |d| {
        tasks
            .iter()
            .filter(|t| t.difficulty == d && s.search.contains(&t.name))
            .count()
    };
    assert_eq!((count("easy"), count("medium"), count("hard")), (1, 19, 10));
}

#[test]
fn shard_plan_is_regenerated_and_bounded() {
    let d = pinned();
    let s = split::make_split(&d.task, 3135).unwrap();
    assert_eq!(d.shard, split::plan_shards(&d.task, &s));
    assert_eq!(d.dataset.shard_cap_bytes, 12_000_000_000);
    let mut seen = Vec::new();
    let mut heavy = Vec::new();
    for shard in &d.shard {
        seen.extend(shard.tasks.clone());
        assert_eq!(
            shard.bytes,
            d.task
                .iter()
                .filter(|t| shard.tasks.contains(&t.name))
                .map(|t| t.image_bytes)
                .sum::<u64>()
        );
        let group = match shard.split.as_str() {
            "search" => &s.search,
            "held_out" => &s.held_out,
            other => panic!("{other}"),
        };
        assert!(names(&shard.tasks).is_subset(&names(group)));
        if shard.heavy {
            assert_eq!(shard.tasks.len(), 1);
            heavy.extend(shard.tasks.clone());
        } else {
            assert!(shard.bytes <= d.dataset.shard_cap_bytes);
        }
    }
    seen.sort();
    assert_eq!(
        seen,
        d.task.iter().map(|t| t.name.clone()).collect::<Vec<_>>()
    );
    heavy.sort();
    assert_eq!(
        heavy,
        words("mteb-leaderboard mteb-retrieve hf-model-inference pytorch-model-recovery")
    );
}

#[test]
fn shard_planner_packs_and_isolates_heavy_images() {
    let mut rows: Vec<_> = ["a", "big", "b", "c", "d", "e"].map(row).into();
    for (t, size) in rows.iter_mut().zip([
        7_000_000_000,
        9_000_000_000,
        3_500_000_000,
        3_500_000_000,
        3_500_000_000,
        3_500_000_000,
    ]) {
        t.image_bytes = size;
    }
    let s = split::Split {
        seed: 3135,
        algorithm: "tb2-split-v1".into(),
        search: rows.iter().map(|t| t.name.clone()).collect(),
        held_out: vec![],
        smoke: vec![],
        calibration: vec![],
    };
    let actual: Vec<_> = split::plan_shards(&rows, &s)
        .into_iter()
        .map(|s| (s.tasks, s.heavy))
        .collect();
    assert_eq!(
        actual,
        vec![
            (words("b c d"), false),
            (words("e"), false),
            (words("big"), true),
            (words("a"), true)
        ]
    );
}

#[test]
fn dataset_render_round_trips_and_preserves_pinned_bytes() {
    let d = pinned();
    assert_eq!(
        dataset::render(&d.task, &d.shard),
        fs::read_to_string(repo().join(DATASET)).unwrap()
    );
    let rows = vec![row("a"), row("b")];
    let parsed: dataset::Dataset = toml::from_str(&dataset::render(&rows, &[])).unwrap();
    assert_eq!(parsed.task, rows);
    assert_eq!(parsed.dataset.commit, dataset::COMMIT);
    let rows: Vec<_> = (0..32)
        .map(|i| {
            let mut t = row(&format!("t{i}"));
            t.difficulty = if i < 2 {
                "easy"
            } else if i < 22 {
                "medium"
            } else {
                "hard"
            }
            .into();
            t
        })
        .collect();
    assert_eq!(split::make_split(&rows, 3135).unwrap().search.len(), 30);
}

fn write_checkout(root: &Path, list: &[&str]) -> Vec<Task> {
    // Read-only Git fixture: rev-parse needs a repository marker and a HEAD ref, no Git
    // mutation command or objects. Keep this isolated from repository-location overrides.
    let git = root.join(".git");
    fs::create_dir_all(git.join("objects")).unwrap();
    fs::create_dir_all(git.join("refs/heads")).unwrap();
    fs::write(git.join("HEAD"), "ref: refs/heads/pinned\n").unwrap();
    fs::write(
        git.join("refs/heads/pinned"),
        format!("{}\n", dataset::COMMIT),
    )
    .unwrap();
    list.iter()
        .map(|name| {
            let p = root.join(name).join("task.toml");
            fs::create_dir_all(p.parent().unwrap()).unwrap();
            fs::write(&p, format!("[metadata]\ndifficulty = \"easy\"\n# {name}\n")).unwrap();
            let mut t = row(name);
            t.task_toml_sha256 = dataset::task_hash(&p).unwrap();
            t
        })
        .collect()
}

#[test]
fn a_checkout_requires_a_readable_git_head_to_prove_its_commit_pin() {
    let tmp = tempfile::tempdir().unwrap();
    let tasks = write_checkout(tmp.path(), &["alpha"]);
    assert!(
        pin::checkout(tmp.path(), dataset::COMMIT, &tasks)
            .unwrap()
            .is_empty()
    );
    fs::write(tmp.path().join(".git/HEAD"), "unresolvable\n").unwrap();
    assert!(
        pin::checkout(tmp.path(), dataset::COMMIT, &tasks)
            .unwrap_err()
            .to_string()
            .contains("git rev-parse failed")
    );
    fs::remove_dir_all(tmp.path().join(".git")).unwrap();
    assert!(
        pin::checkout(tmp.path(), dataset::COMMIT, &tasks)
            .unwrap_err()
            .to_string()
            .contains("no .git")
    );
}

#[test]
fn clean_and_crlf_checkout_have_no_drift() {
    let tmp = tempfile::tempdir().unwrap();
    let tasks = write_checkout(tmp.path(), &["alpha", "beta"]);
    assert!(
        pin::checkout(tmp.path(), dataset::COMMIT, &tasks)
            .unwrap()
            .is_empty()
    );
    let path = tmp.path().join("alpha/task.toml");
    fs::write(
        &path,
        fs::read_to_string(&path).unwrap().replace('\n', "\r\n"),
    )
    .unwrap();
    assert!(
        pin::checkout(tmp.path(), dataset::COMMIT, &tasks)
            .unwrap()
            .is_empty()
    );
}

#[test]
fn missing_extra_and_changed_tasks_are_named() {
    let tmp = tempfile::tempdir().unwrap();
    let tasks = write_checkout(tmp.path(), &["alpha", "beta"]);
    fs::write(tmp.path().join("alpha/task.toml"), "changed = true\n").unwrap();
    fs::remove_file(tmp.path().join("beta/task.toml")).unwrap();
    write_checkout(tmp.path(), &["gamma"]);
    let drift = pin::checkout(tmp.path(), dataset::COMMIT, &tasks).unwrap();
    assert_eq!(drift.len(), 3);
    assert_eq!(drift[0], "task missing from checkout: beta");
    assert_eq!(drift[1], "task not in the pinned set: gamma");
    assert!(drift[2].starts_with("task.toml changed: alpha"));
}

#[test]
fn images_match_mismatch_unpulled_and_task_filter() {
    let rows = [row("alpha"), row("beta"), row("gamma")];
    let inspect = |tag: &str| -> Result<_> {
        Ok(match tag {
            "x/alpha:1" => Some(vec![format!("x/alpha@{}", rows[0].image_digest)]),
            "x/beta:1" => Some(vec!["x/beta@sha256:wrong".into()]),
            _ => None,
        })
    };
    let drift = pin::images(&rows, &[], false, inspect).unwrap();
    assert_eq!(drift.len(), 1);
    assert!(drift[0].starts_with("image digest drift: x/beta:1 (beta)"));
    let strict = pin::images(&rows, &[], true, inspect).unwrap();
    assert_eq!(strict.len(), 2);
    assert_eq!(strict[1], "image not pulled: x/gamma:1 (gamma)");
    assert!(
        pin::images(&rows, &["alpha".into()], true, inspect)
            .unwrap()
            .is_empty()
    );
}

const EXPECTED: &str = "The quarterly lantern total is 48213 widgets";
struct AuditFixture {
    tmp: tempfile::TempDir,
    checkout: PathBuf,
    dataset: PathBuf,
    split: PathBuf,
}
impl AuditFixture {
    fn new() -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let checkout = tmp.path().join("checkout");
        let tasks = write_checkout(
            &checkout,
            &[
                "regex-log",
                "path-tracing",
                "path-tracing-reverse",
                "search-only",
            ],
        );
        for (name, file, text) in [
            (
                "regex-log",
                "test_regexlog_unique.py",
                format!(
                    "def test_x():\n    assert out == \"{EXPECTED}\"\n    \"\"\"a docstring that is long enough to ignore\"\"\"\n"
                ),
            ),
            ("path-tracing", "test_outputs.py", "assert True\n".into()),
            (
                "path-tracing-reverse",
                "test_outputs.py",
                "assert True\n".into(),
            ),
            (
                "search-only",
                "test_outputs.py",
                "assert s == \"search side expected output text\"\n".into(),
            ),
        ] {
            let dir = checkout.join(name).join("tests");
            fs::create_dir_all(&dir).unwrap();
            fs::write(dir.join(file), text).unwrap();
            fs::write(
                dir.join("test.sh"),
                "uv run pytest -rA /tests/shared_boiler_line\n",
            )
            .unwrap();
        }
        let dataset = tmp.path().join("dataset.toml");
        fs::write(&dataset, dataset::render(&tasks, &[])).unwrap();
        let split = tmp.path().join("split.json");
        fs::write(&split,r#"{"held_out":["regex-log","path-tracing"],"search":["path-tracing-reverse","search-only"]}"#).unwrap();
        Self {
            tmp,
            checkout,
            dataset,
            split,
        }
    }
    fn audit(&self, items: &[Item], mode: Terms, excludes: &[String]) -> Vec<Finding> {
        audit::audit_items(
            self.tmp.path(),
            &self.dataset,
            &self.split,
            Some(&self.checkout),
            mode,
            items,
            excludes,
        )
        .unwrap()
    }
    fn hits(&self, path: &str, text: &str) -> Vec<Finding> {
        self.audit(
            &[(path.into(), 2, text.into())],
            Terms::HeldOut,
            &audit::DEFAULT_EXCLUDES.map(str::to_owned),
        )
    }
    fn candidate(&self) -> PathBuf {
        let dir = self.tmp.path().join("eval/bench/tb2/candidates/c1");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("overlay.toml"), "instructions = \"custom.md\"\n").unwrap();
        fs::write(dir.join("custom.md"), "be careful\n").unwrap();
        fs::write(dir.join("notes.md"), "fine\n").unwrap();
        dir
    }
}
fn diff(path: &str, added: &str, removed: &str) -> String {
    format!(
        "diff --git a/{path} b/{path}\n--- a/{path}\n+++ b/{path}\n@@ -1,1 +1,2 @@\n context\n-{removed}\n+{added}\n"
    )
}

#[test]
fn held_out_name_is_a_redacted_hit_and_prefix_collisions_are_clean() {
    let fx = AuditFixture::new();
    assert_eq!(
        fx.hits("prompts/system.md", "Remember the regex-log trick."),
        vec![Finding {
            label: "HIT",
            file: "prompts/system.md".into(),
            line: 2,
            kind: "task-name"
        }]
    );
    for text in [
        "search-only and path-tracing-reverse are fair",
        "xregex-log regex-logs my-path-tracing",
    ] {
        assert!(fx.hits("p.md", text).is_empty());
    }
    let hits = fx.hits("p.md", "see (path-tracing).");
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].to_string(), "HIT p.md:2 task-name");
}

#[test]
fn removed_and_context_lines_are_not_audited() {
    let fx = AuditFixture::new();
    let d = diff("p.md", "fine", "regex-log").replace(" context", " regex-log");
    assert!(
        fx.audit(&audit::added_lines(&d), Terms::HeldOut, &[])
            .is_empty()
    );
    assert_eq!(
        audit::added_lines(&diff("p.md", "regex-log", "old")),
        vec![("p.md".into(), 2, "regex-log".into())]
    );
    assert!(audit::added_lines("+++ /dev/null\n@@ -1 +0,0 @@\n-old\n").is_empty());
}

#[test]
fn expected_output_literals_and_unique_filenames_are_hits() {
    let fx = AuditFixture::new();
    let hits = fx.hits(
        "memory/notes.txt",
        &format!("{EXPECTED} apparently; see test_regexlog_unique.py"),
    );
    assert_eq!(
        hits.iter().map(|f| f.kind).collect::<Vec<_>>(),
        ["test-file", "literal"]
    );
    assert!(fx.hits("memory/notes.txt","search side expected output text; test_outputs.py; shared_boiler_line; a docstring that is long enough").is_empty());
}

#[test]
fn pin_files_are_excluded_in_all_path_forms_and_candidates_always_audited() {
    let fx = AuditFixture::new();
    for file in audit::DEFAULT_EXCLUDES {
        let absolute = fx.tmp.path().join(file);
        fs::create_dir_all(absolute.parent().unwrap()).unwrap();
        fs::write(&absolute, "regex-log\n").unwrap();
        for path in [
            file.into(),
            format!("./{file}"),
            absolute.to_string_lossy().into_owned(),
        ] {
            assert!(fx.hits(&path, "regex-log").is_empty());
            assert_eq!(
                fx.audit(&[(path, 1, "regex-log".into())], Terms::HeldOut, &[])
                    .len(),
                1
            );
        }
    }
    assert_eq!(fx.hits("eval/bench/tb2/note.md", "regex-log").len(), 1);
    let file = "eval/bench/tb2/candidates/c1/overlay.toml";
    assert_eq!(fx.hits(file, "regex-log").len(), 1);
    assert_eq!(
        fx.audit(
            &[(file.into(), 1, "regex-log".into())],
            Terms::HeldOut,
            &["eval/bench/tb2/*".into()]
        )
        .len(),
        1
    );
}

#[test]
fn candidate_directory_audits_overlay_custom_instructions_and_notes() {
    let fx = AuditFixture::new();
    let dir = fx.candidate();
    assert!(
        fx.audit(
            &audit::file_lines(&[dir.clone()]).unwrap(),
            Terms::HeldOut,
            &[]
        )
        .is_empty()
    );
    fs::write(dir.join("notes.md"), "fine\nregex-log was easy\n").unwrap();
    fs::write(dir.join("custom.md"), "path-tracing first\n").unwrap();
    fs::write(
        dir.join("overlay.toml"),
        "instructions = 'custom.md'\n# regex-log\n",
    )
    .unwrap();
    let hits = fx.audit(&audit::file_lines(&[dir]).unwrap(), Terms::HeldOut, &[]);
    let actual: BTreeSet<_> = hits.iter().map(|f| (f.file.as_str(), f.line)).collect();
    assert_eq!(
        actual,
        BTreeSet::from([
            ("eval/bench/tb2/candidates/c1/notes.md", 2),
            ("eval/bench/tb2/candidates/c1/custom.md", 1),
            ("eval/bench/tb2/candidates/c1/overlay.toml", 2)
        ])
    );
    let empty = fx.tmp.path().join("empty");
    fs::create_dir(&empty).unwrap();
    assert!(audit::file_lines(&[empty]).is_err());
}

#[test]
fn candidate_hits_are_identical_for_absolute_relative_and_dot_paths() {
    let fx = AuditFixture::new();
    let dir = fx.candidate();
    let rel = "eval/bench/tb2/candidates/c1/instructions.md";
    let file = dir.join("instructions.md");
    fs::write(&file, "ok\nuse the regex-log trick\n").unwrap();
    let absolute = fx.audit(&audit::file_lines(&[file]).unwrap(), Terms::HeldOut, &[]);
    for path in [rel.to_string(), format!("./{rel}")] {
        assert_eq!(
            absolute,
            fx.audit(
                &[(path, 2, "use the regex-log trick".into())],
                Terms::HeldOut,
                &[]
            )
        );
    }
    assert_eq!(
        absolute,
        fx.audit(
            &audit::added_lines(&diff(rel, "regex-log", "old")),
            Terms::HeldOut,
            &[]
        )
    );
}

#[test]
fn generality_and_all_modes_flag_search_names_and_unique_filenames_only() {
    let fx = AuditFixture::new();
    fs::write(
        fx.checkout
            .join("search-only/tests/test_searchonly_unique.py"),
        "x = 1\n",
    )
    .unwrap();
    let item=("p.md".into(),1,"path-tracing-reverse and search-only; see test_searchonly_unique.py and test_outputs.py; search side expected output text".into());
    assert!(fx.audit(&[item.clone()], Terms::HeldOut, &[]).is_empty());
    for mode in [Terms::Search, Terms::All] {
        let hits = fx.audit(&[item.clone()], mode, &[]);
        assert_eq!(hits.len(), 3);
        assert!(hits.iter().all(|f| f.label == "GENERALITY"));
        assert_eq!(
            hits.iter().map(|f| f.kind).collect::<Vec<_>>(),
            ["task-name", "task-name", "test-file"]
        );
    }
    let hits = fx.audit(
        &[(
            "p.md".into(),
            1,
            "path-tracing and path-tracing-reverse".into(),
        )],
        Terms::All,
        &[],
    );
    assert_eq!(
        hits.iter().map(|f| f.label).collect::<Vec<_>>(),
        ["HIT", "GENERALITY"]
    );
}

#[test]
fn file_list_mode_reads_every_text_file_and_reports_missing_inputs() {
    let fx = AuditFixture::new();
    let dirty = fx.tmp.path().join("tool_description.md");
    let clean = fx.tmp.path().join("clean.md");
    fs::write(&dirty, "fine line\nuse regex-log answers\n").unwrap();
    fs::write(&clean, "nothing to see\n").unwrap();
    let hits = fx.audit(
        &audit::file_lines(&[dirty, clean.clone()]).unwrap(),
        Terms::HeldOut,
        &[],
    );
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].to_string(), "HIT tool_description.md:2 task-name");
    assert!(
        fx.audit(&audit::file_lines(&[clean]).unwrap(), Terms::HeldOut, &[])
            .is_empty()
    );
    assert!(audit::file_lines(&[fx.tmp.path().join("missing.md")]).is_err());
}

#[test]
fn every_real_held_out_name_is_audited_without_a_checkout() {
    let root = repo();
    let s: split::Split =
        serde_json::from_str(&fs::read_to_string(root.join(SPLIT)).unwrap()).unwrap();
    for (list, expected) in [(&s.held_out, 1), (&s.search, 0)] {
        for name in list {
            let findings = audit::audit_items(
                &root,
                &root.join(DATASET),
                &root.join(SPLIT),
                None,
                Terms::HeldOut,
                &[("p.md".into(), 1, format!("tip: {name}."))],
                &[],
            )
            .unwrap();
            assert_eq!(findings.len(), expected, "{name}");
        }
    }
}

#[test]
fn split_cli_checks_drift_and_writes_exact_bytes() {
    let tmp = tempfile::tempdir().unwrap();
    let target = tmp.path().join("split.json");
    fs::write(&target, "{}\n").unwrap();
    let run = |write, out: &mut Vec<u8>, err: &mut Vec<u8>| {
        execute(
            Command::Split {
                dataset: repo().join(DATASET),
                split: target.clone(),
                write,
                check: !write,
            },
            out,
            err,
        )
        .unwrap()
    };
    let mut out = Vec::new();
    let mut err = Vec::new();
    assert_eq!(run(false, &mut out, &mut err), 1);
    assert!(String::from_utf8(err.clone()).unwrap().contains("drift:"));
    assert_eq!(run(true, &mut out, &mut err), 0);
    assert_eq!(run(false, &mut out, &mut err), 0);
    assert_eq!(
        fs::read(target).unwrap(),
        fs::read(repo().join(SPLIT)).unwrap()
    );
}

#[test]
fn pin_cli_reports_clean_drift_and_input_errors() {
    let tmp = tempfile::tempdir().unwrap();
    let checkout = tmp.path().join("checkout");
    let tasks = write_checkout(&checkout, &["alpha", "beta"]);
    let dataset = tmp.path().join("dataset.toml");
    fs::write(&dataset, dataset::render(&tasks, &[])).unwrap();
    let run = |root| {
        let mut out = Vec::new();
        let mut err = Vec::new();
        let code = execute(
            Command::PinCheck {
                checkout: root,
                dataset: dataset.clone(),
                images: false,
                require_pulled: false,
                tasks: vec![],
            },
            &mut out,
            &mut err,
        );
        (code, String::from_utf8(out).unwrap())
    };
    let (code, out) = run(checkout.clone());
    assert_eq!(code.unwrap(), 0);
    assert_eq!(out, "pin_check: ok (2 tasks at 2fd12b88)\n");
    fs::write(checkout.join("alpha/task.toml"), "changed = true\n").unwrap();
    let (code, out) = run(checkout);
    assert_eq!(code.unwrap(), 1);
    assert!(out.contains("DRIFT task.toml changed: alpha"));
    assert!(run(tmp.path().join("missing")).0.is_err());
}

#[test]
fn native_cli_help_and_argument_contract() {
    for command in ["leak-audit", "pin-check", "split", "refresh-dataset"] {
        let error = Cli::try_parse_from(["tb2", command, "--help"]).unwrap_err();
        assert_eq!(error.kind(), clap::error::ErrorKind::DisplayHelp);
    }
    assert!(Cli::try_parse_from(["tb2", "leak-audit"]).is_err());
    assert!(Cli::try_parse_from(["tb2", "leak-audit", "--diff", "-", "--files", "x"]).is_err());
    assert!(Cli::try_parse_from(["tb2", "split", "--write", "--check"]).is_err());
    let cli = Cli::try_parse_from([
        "tb2",
        "leak-audit",
        "--files",
        "x",
        "--exclude",
        "one",
        "--exclude",
        "two",
    ])
    .unwrap();
    let Command::LeakAudit { exclude, .. } = cli.command else {
        panic!("wrong command")
    };
    assert_eq!(exclude.unwrap(), ["one", "two"]);
}

#[test]
fn refresh_from_checkout_and_captured_manifests_is_offline_and_reproducible() {
    let tmp = tempfile::tempdir().unwrap();
    let checkout = tmp.path().join("checkout");
    let manifests = BTreeMap::from([(
        "x/shared:1",
        serde_json::json!({"digest":"sha256:abc", "bytes":1234}),
    )]);
    let path = tmp.path().join("manifests.json");
    fs::write(&path, serde_json::to_string(&manifests).unwrap()).unwrap();
    for i in 0..32 {
        let name = format!("t{i:02}");
        let file = checkout.join(&name).join("task.toml");
        fs::create_dir_all(file.parent().unwrap()).unwrap();
        let difficulty = if i < 2 {
            "easy"
        } else if i < 22 {
            "medium"
        } else {
            "hard"
        };
        fs::write(file, format!("[metadata]\ndifficulty = {difficulty:?}\ncategory = \"c\"\n[environment]\ncpus = 1\nmemory_mb = 2048\ndocker_image = \"x/shared:1\"\n[agent]\ntimeout_sec = 900\n[verifier]\ntimeout_sec = 120.0\n")).unwrap();
    }
    let output = tmp.path().join("dataset.toml");
    assert_eq!(
        execute(
            Command::RefreshDataset {
                checkout: checkout.clone(),
                manifests: Some(path.clone()),
                out: output.clone()
            },
            &mut Vec::new(),
            &mut Vec::new()
        )
        .unwrap(),
        0
    );
    let captured: BTreeMap<String, dataset::Manifest> =
        serde_json::from_str(&fs::read_to_string(path).unwrap()).unwrap();
    let tasks = dataset::read_tasks(&checkout, Some(&captured)).unwrap();
    assert_eq!(tasks.len(), 32);
    for t in &tasks {
        assert_eq!(
            (
                t.cpus,
                t.memory_mb,
                t.agent_timeout_sec,
                t.verifier_timeout_sec
            ),
            (1, 2048, 900.0, 120.0)
        );
        assert_eq!((&*t.image_digest, t.image_bytes), ("sha256:abc", 1234));
        assert_eq!(
            t.task_toml_sha256,
            dataset::task_hash(&checkout.join(&t.name).join("task.toml")).unwrap()
        );
    }
    let split = split::make_split(&tasks, 3135).unwrap();
    assert_eq!(
        fs::read_to_string(output).unwrap(),
        dataset::render(&tasks, &split::plan_shards(&tasks, &split))
    );
    assert!(dataset::read_tasks(&checkout, Some(&BTreeMap::new())).is_err());
}

fn task_config(cpus: &str, memory: &str) -> String {
    format!(
        "schema_version = \"1.1\"\nartifacts = []\n[metadata]\ndifficulty = \"easy\"\ncategory = \"c\"\n[environment]\ncpus = {cpus}\nmemory_mb = {memory}\ndocker_image = \"x/shared:1\"\nbuild_timeout_sec = 600.0\nstorage_mb = 10240\ngpus = 0\nallow_internet = true\nmcp_servers = []\n[agent]\ntimeout_sec = 900.0\n[verifier]\ntimeout_sec = 120.0\n[verifier.env]\n[environment.env]\n[solution.env]\n"
    )
}

fn read_task_config(config: &str) -> Result<Vec<Task>> {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("fixture");
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join("task.toml"), config).unwrap();
    let manifests = BTreeMap::from([(
        "x/shared:1".into(),
        dataset::Manifest {
            digest: "sha256:abc".into(),
            bytes: 1,
        },
    )]);
    dataset::read_tasks(tmp.path(), Some(&manifests))
}

#[test]
fn unsupported_task_settings_are_rejected_instead_of_silently_ignored() {
    let supported = task_config("1", "2048");
    for (from, to, field) in [
        (
            "storage_mb = 10240",
            "storage_mb = 20480",
            "environment.storage_mb",
        ),
        (
            "storage_mb = 10240",
            "storage_mb = 10240.5",
            "environment.storage_mb",
        ),
        (
            "storage_mb = 10240",
            "storage_mb = \"10240\"",
            "environment.storage_mb",
        ),
        ("gpus = 0", "gpus = 1", "environment.gpus"),
        (
            "allow_internet = true",
            "allow_internet = false",
            "environment.allow_internet",
        ),
        (
            "mcp_servers = []",
            "mcp_servers = [\"server\"]",
            "environment.mcp_servers",
        ),
        (
            "build_timeout_sec = 600.0",
            "build_timeout_sec = 42.0",
            "environment.build_timeout_sec",
        ),
        ("[agent]", "[agent]\nuser = \"other\"", "agent.user"),
        (
            "[verifier]",
            "[verifier]\nfuture_option = true",
            "verifier.future_option",
        ),
        (
            "[environment.env]",
            "[environment.env]\nKEY = \"value\"",
            "environment.env.KEY",
        ),
        (
            "[verifier.env]",
            "[verifier.env]\nKEY = \"value\"",
            "verifier.env.KEY",
        ),
        (
            "[solution.env]",
            "[solution.env]\nKEY = \"value\"",
            "solution.env.KEY",
        ),
        (
            "artifacts = []",
            "future_runtime = true\nartifacts = []",
            "future_runtime",
        ),
        ("artifacts = []", "artifacts = [\"/extra\"]", "artifacts"),
    ] {
        let error = read_task_config(&supported.replace(from, to))
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("unsupported") && error.contains(field),
            "{field}: {error}"
        );
    }
}

#[test]
fn resource_limits_must_be_positive_integral_and_representable() {
    for bad in ["1.5", "0", "-1", "nan", "inf", "18446744073709551616.0"] {
        for (cpus, memory, field) in [(bad, "2048", "cpus"), ("1", bad, "memory_mb")] {
            let error = read_task_config(&task_config(cpus, memory))
                .unwrap_err()
                .to_string();
            assert!(
                error.contains(field) && error.contains("positive integral"),
                "{error}"
            );
        }
    }
    let rows = read_task_config(&task_config("2.0", "4096.0")).unwrap();
    assert_eq!((rows[0].cpus, rows[0].memory_mb), (2, 4096));
}

#[test]
fn every_pinned_task_limit_is_accepted_without_rounding() {
    // Runtime-field inventory of the pinned source archive (2fd12b88). The source's optional
    // env tables are empty, GPU count is zero, internet is enabled and MCP list is empty.
    for task in pinned().task {
        let config = task_config(&task.cpus.to_string(), &task.memory_mb.to_string())
            .replace(
                "timeout_sec = 900.0",
                &format!("timeout_sec = {:?}", task.agent_timeout_sec),
            )
            .replace(
                "timeout_sec = 120.0",
                &format!("timeout_sec = {:?}", task.verifier_timeout_sec),
            );
        let rows = read_task_config(&config).unwrap();
        assert_eq!(
            (
                rows[0].cpus,
                rows[0].memory_mb,
                rows[0].agent_timeout_sec,
                rows[0].verifier_timeout_sec
            ),
            (
                task.cpus,
                task.memory_mb,
                task.agent_timeout_sec,
                task.verifier_timeout_sec
            ),
            "{}",
            task.name
        );
    }
}

#[test]
fn registry_manifest_selects_only_linux_amd64_and_totals_compressed_bytes() {
    let entry = serde_json::json!({"Descriptor":{"platform":{"os":"linux","architecture":"amd64"},"digest":"sha256:abc"},"SchemaV2Manifest":{"config":{"size":13},"layers":[{"size":21},{"size":34}]}});
    let mut other = entry.clone();
    other["Descriptor"]["platform"]["architecture"] = "arm64".into();
    for doc in [entry.clone(), serde_json::json!([other.clone(), entry])] {
        let info = dataset::manifest_info(&doc).unwrap();
        assert_eq!((&*info.digest, info.bytes), ("sha256:abc", 68));
    }
    assert!(dataset::manifest_info(&other).is_err());
    assert!(dataset::manifest_info(&serde_json::json!({})).is_err());
}
