//! The committed run config (`eval/bench/tb2/runs/*.toml`) and the pinned
//! dataset it selects from (`eval/bench/tb2/{dataset.toml,split.json}`).
//!
//! A run config names what to run (a split or an explicit task list), how often
//! (`k`), where (`host`) and with which agent. It carries no secret, no path and
//! no command line. Resolving it against the dataset yields the task set with
//! each task's pinned image digest, the shard plan (the dataset's shards cut to
//! the selected tasks) and the sizes the laptop preflight needs.

use rsi_common::bench_profile::LoopKnobs;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};

/// The only run config schema.
pub(crate) const RUN_CONFIG_SCHEMA: u32 = 1;
/// The most attempts per task a config may ask for.
pub(crate) const MAX_ATTEMPTS: u32 = 10;
/// The most USD a single task may be capped at.
pub(crate) const MAX_TASK_CAP_USD: f64 = 100.0;

/// What drives the trials.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum AgentKind {
    /// Harbor's own `oracle` agent: it replays the task's reference solution. No
    /// model is called, so the run costs $0 and needs no launch grant or spend cap.
    Oracle,
    /// RSI's native harness through `RsiHarnessAgent`: OpenRouter-billed sessions.
    Rsi,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct AgentSection {
    pub kind: AgentKind,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub effort: Option<String>,
    /// The per-task USD cap (the daemon stops a session that reaches it).
    #[serde(default)]
    pub per_task_cap_usd: Option<f64>,
}

/// `eval/bench/tb2/runs/<name>.toml`.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RunFile {
    pub schema_version: u32,
    pub name: String,
    #[serde(default)]
    pub description: String,
    /// The docker host the containers run on, `ssh://<laptop host>`.
    pub host: String,
    /// One of the dataset's splits (`smoke`, `calibration`, `search`, `held_out`) ...
    #[serde(default)]
    pub split: Option<String>,
    /// ... or an explicit task list. Exactly one of the two.
    #[serde(default)]
    pub tasks: Vec<String>,
    /// Attempts per task (Harbor `-k`).
    #[serde(default = "one")]
    pub k: u32,
    /// Harbor's timeout multiplier. Only 1.0 is leaderboard-shaped.
    #[serde(default = "one_f64")]
    pub timeout_multiplier: f64,
    /// Concurrent trials; the operator's `bench.max_concurrency` lowers it.
    #[serde(default = "four")]
    pub concurrency: u32,
    pub agent: AgentSection,
    #[serde(default, rename = "loop")]
    pub loop_knobs: LoopKnobs,
    /// #3191: the committed run overlay (a Meta-Harness candidate), a repository
    /// path `eval/bench/tb2/candidates/<id>/overlay.toml`. Its values are applied
    /// per launch on top of `[loop]`; it never changes a daemon setting.
    #[serde(default)]
    pub overlay: Option<String>,
}

const fn one() -> u32 {
    1
}
const fn four() -> u32 {
    4
}
const fn one_f64() -> f64 {
    1.0
}

/// One task of the pinned dataset.
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct DatasetTask {
    pub name: String,
    /// Container limits the runner applies and its pin check compares (R3b).
    pub cpus: u64,
    pub memory_mb: u64,
    pub agent_timeout_sec: f64,
    pub verifier_timeout_sec: f64,
    pub image: String,
    pub image_digest: String,
    pub image_bytes: u64,
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct DatasetHeader {
    pub repo: String,
    pub commit: String,
    pub harbor_version: String,
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct DatasetShard {
    pub name: String,
    pub tasks: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
struct DatasetFile {
    dataset: DatasetHeader,
    #[serde(default)]
    task: Vec<DatasetTask>,
    #[serde(default)]
    shard: Vec<DatasetShard>,
}

/// `split.json`: the seeded split and its two named subsets.
#[derive(Debug, Clone, Deserialize)]
struct SplitFile {
    search: Vec<String>,
    held_out: Vec<String>,
    smoke: Vec<String>,
    calibration: Vec<String>,
}

/// The pinned dataset and its split.
#[derive(Debug, Clone)]
pub(crate) struct Dataset {
    pub header: DatasetHeader,
    pub tasks: BTreeMap<String, DatasetTask>,
    pub shards: Vec<DatasetShard>,
    split: SplitFile,
}

/// One shard of a run: the dataset shard cut to the selected tasks.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PlannedShard {
    pub name: String,
    pub tasks: Vec<String>,
    pub images: Vec<String>,
    /// Compressed image bytes of the selected tasks.
    pub bytes: u64,
}

/// A run config resolved against the dataset.
#[derive(Debug, Clone)]
pub(crate) struct ResolvedRun {
    pub file: RunFile,
    /// `sha256:<hex>` of the config file's exact bytes.
    pub config_digest: String,
    /// The split label the record carries (`smoke`, `calibration`, `search`,
    /// `held_out` or `custom`).
    pub split_label: String,
    /// Selected tasks, sorted.
    pub tasks: Vec<String>,
    pub shards: Vec<PlannedShard>,
    pub dataset: Dataset,
}

impl ResolvedRun {
    #[must_use]
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn largest_shard_bytes(&self) -> u64 {
        self.shards.iter().map(|s| s.bytes).max().unwrap_or(0)
    }

    /// Whether the run launches OpenRouter-billed sessions.
    #[must_use]
    pub(crate) const fn is_paid(&self) -> bool {
        matches!(self.file.agent.kind, AgentKind::Rsi)
    }

    /// Expected trials (tasks x k).
    #[must_use]
    pub(crate) fn expected_trials(&self) -> u32 {
        u32::try_from(self.tasks.len())
            .unwrap_or(u32::MAX)
            .saturating_mul(self.file.k)
    }

    /// The task's pinned entry.
    #[must_use]
    pub(crate) fn task(&self, name: &str) -> Option<&DatasetTask> {
        self.dataset.tasks.get(name)
    }

    /// The split a task belongs to (`held_out` or `search`).
    #[must_use]
    pub(crate) fn task_split(&self, name: &str) -> &'static str {
        if self.dataset.split.held_out.iter().any(|t| t == name) {
            "held_out"
        } else {
            "search"
        }
    }
}

fn token_ok(text: &str) -> bool {
    !text.is_empty()
        && text.len() <= 100
        && !text.starts_with(['-', '.'])
        && text
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
}

/// `sha256:<hex>` of `bytes`.
#[must_use]
pub(crate) fn sha256_prefixed(bytes: &[u8]) -> String {
    format!("sha256:{:x}", Sha256::digest(bytes))
}

impl Dataset {
    /// Parse `dataset.toml` and `split.json`.
    ///
    /// # Errors
    /// The `bench_config_invalid` detail: a malformed or inconsistent dataset.
    pub(crate) fn parse(dataset_toml: &str, split_json: &str) -> Result<Self, String> {
        let file: DatasetFile =
            toml::from_str(dataset_toml).map_err(|e| format!("dataset.toml: {e}"))?;
        let split: SplitFile =
            serde_json::from_str(split_json).map_err(|e| format!("split.json: {e}"))?;
        let mut tasks = BTreeMap::new();
        for task in file.task {
            if !token_ok(&task.name) {
                return Err(format!("dataset.toml: bad task name {:?}", task.name));
            }
            tasks.insert(task.name.clone(), task);
        }
        if tasks.is_empty() {
            return Err("dataset.toml: no tasks".into());
        }
        for name in split
            .search
            .iter()
            .chain(&split.held_out)
            .chain(&split.smoke)
            .chain(&split.calibration)
        {
            if !tasks.contains_key(name) {
                return Err(format!("split.json: {name} is not a dataset task"));
            }
        }
        for shard in &file.shard {
            if let Some(missing) = shard.tasks.iter().find(|t| !tasks.contains_key(*t)) {
                return Err(format!(
                    "dataset.toml: shard {} names {missing}, not a task",
                    shard.name
                ));
            }
        }
        Ok(Self {
            header: file.dataset,
            tasks,
            shards: file.shard,
            split,
        })
    }

    fn split_tasks(&self, name: &str) -> Option<&[String]> {
        match name {
            "search" => Some(&self.split.search),
            "held_out" => Some(&self.split.held_out),
            "smoke" => Some(&self.split.smoke),
            "calibration" => Some(&self.split.calibration),
            _ => None,
        }
    }
}

impl RunFile {
    /// Parse and validate the run config's text.
    ///
    /// # Errors
    /// The `bench_config_invalid` detail.
    pub(crate) fn parse(text: &str) -> Result<Self, String> {
        let file: Self = toml::from_str(text).map_err(|e| format!("run config: {e}"))?;
        file.validate()?;
        Ok(file)
    }

    fn validate(&self) -> Result<(), String> {
        if self.schema_version != RUN_CONFIG_SCHEMA {
            return Err(format!(
                "schema_version must be {RUN_CONFIG_SCHEMA}, got {}",
                self.schema_version
            ));
        }
        if !token_ok(&self.name) {
            return Err("name must be a plain token".into());
        }
        if !rsi_common::exec_target::docker_host_syntax_ok(&self.host) {
            return Err(format!("host {:?} is not a docker host", self.host));
        }
        if self.split.is_some() == !self.tasks.is_empty() {
            // Both set, or neither set.
            return Err("set exactly one of split and tasks".into());
        }
        if !(1..=MAX_ATTEMPTS).contains(&self.k) {
            return Err(format!("k must be in 1..={MAX_ATTEMPTS}"));
        }
        if (self.timeout_multiplier - 1.0).abs() > f64::EPSILON {
            return Err(
                "timeout_multiplier must be 1.0 (any other value is not leaderboard-shaped)".into(),
            );
        }
        if !(1..=rsi_common::bench_settings::MAX_BENCH_MAX_CONCURRENCY)
            .contains(&u64::from(self.concurrency))
        {
            return Err("concurrency must be in 1..=8".into());
        }
        self.loop_knobs.validate().map_err(str::to_string)?;
        if let Some(overlay) = &self.overlay {
            if !rsi_common::bench_overlay::overlay_path_ok(overlay) {
                return Err(format!(
                    "overlay {overlay:?} must be {}<id>/{}",
                    rsi_common::bench_overlay::OVERLAY_CANDIDATES_DIR,
                    rsi_common::bench_overlay::OVERLAY_FILE_NAME
                ));
            }
            if self.agent.kind == AgentKind::Oracle {
                return Err("the oracle agent calls no model: remove overlay".into());
            }
        }
        match self.agent.kind {
            AgentKind::Oracle => {
                if self.agent.model.is_some()
                    || self.agent.effort.is_some()
                    || self.agent.per_task_cap_usd.is_some()
                {
                    return Err(
                        "the oracle agent calls no model: remove model, effort and per_task_cap_usd"
                            .into(),
                    );
                }
            }
            AgentKind::Rsi => {
                let model = self.agent.model.as_deref().unwrap_or_default();
                let effort = self.agent.effort.as_deref().unwrap_or_default();
                if model.is_empty() || model.len() > 128 || model.chars().any(char::is_control) {
                    return Err("agent.model is required for the rsi agent".into());
                }
                if effort.is_empty() || !token_ok(effort) {
                    return Err("agent.effort is required for the rsi agent".into());
                }
                match self.agent.per_task_cap_usd {
                    Some(cap) if cap.is_finite() && cap > 0.0 && cap <= MAX_TASK_CAP_USD => {}
                    _ => {
                        return Err(format!(
                            "agent.per_task_cap_usd must be in (0, {MAX_TASK_CAP_USD}]"
                        ));
                    }
                }
            }
        }
        Ok(())
    }

    /// Resolve against the dataset: the task set, each task's pin and the shard plan.
    ///
    /// # Errors
    /// The `bench_config_invalid` detail: an unknown split or task, or a task list
    /// with duplicates.
    pub(crate) fn resolve(
        self,
        config_text: &str,
        dataset: Dataset,
    ) -> Result<ResolvedRun, String> {
        let (split_label, mut tasks): (String, Vec<String>) = match &self.split {
            Some(name) => {
                let list = dataset
                    .split_tasks(name)
                    .ok_or_else(|| format!("unknown split {name:?}"))?;
                (name.clone(), list.to_vec())
            }
            None => {
                let mut seen = BTreeSet::new();
                for task in &self.tasks {
                    if !dataset.tasks.contains_key(task) {
                        return Err(format!("{task} is not a dataset task"));
                    }
                    if !seen.insert(task.as_str()) {
                        return Err(format!("{task} is listed twice"));
                    }
                }
                ("custom".to_string(), self.tasks.clone())
            }
        };
        tasks.sort();
        tasks.dedup();
        let selected: BTreeSet<&str> = tasks.iter().map(String::as_str).collect();
        let mut shards = Vec::new();
        let mut planned = BTreeSet::new();
        for shard in &dataset.shards {
            let in_shard: Vec<String> = shard
                .tasks
                .iter()
                .filter(|t| selected.contains(t.as_str()))
                .cloned()
                .collect();
            if in_shard.is_empty() {
                continue;
            }
            let mut bytes = 0_u64;
            let mut images = Vec::new();
            for name in &in_shard {
                let task = &dataset.tasks[name];
                bytes = bytes.saturating_add(task.image_bytes);
                images.push(task.image.clone());
                planned.insert(name.clone());
            }
            shards.push(PlannedShard {
                name: shard.name.clone(),
                tasks: in_shard,
                images,
                bytes,
            });
        }
        if let Some(orphan) = tasks.iter().find(|t| !planned.contains(*t)) {
            return Err(format!("{orphan} is in no dataset shard"));
        }
        Ok(ResolvedRun {
            config_digest: sha256_prefixed(config_text.as_bytes()),
            split_label,
            tasks,
            shards,
            dataset,
            file: self,
        })
    }
}

#[cfg(test)]
pub(crate) mod fixtures {
    //! A tiny dataset for the bench job tests.
    pub(crate) const DATASET_TOML: &str = r#"
[dataset]
name = "terminal-bench-2"
repo = "https://github.com/harbor-framework/terminal-bench-2"
commit = "2fd12b88aafdd04a52c298e3940bcb189f9766d6"
harbor_version = "0.24.0"

[[task]]
name = "fix-git"
cpus = 1
memory_mb = 2048
agent_timeout_sec = 900.0
verifier_timeout_sec = 900.0
image = "alexgshaw/fix-git:20251031"
image_digest = "sha256:1111111111111111111111111111111111111111111111111111111111111111"
image_bytes = 1000000000

[[task]]
name = "regex-log"
cpus = 1
memory_mb = 2048
agent_timeout_sec = 600.0
verifier_timeout_sec = 600.0
image = "alexgshaw/regex-log:20251031"
image_digest = "sha256:2222222222222222222222222222222222222222222222222222222222222222"
image_bytes = 500000000

[[task]]
name = "big-one"
cpus = 1
memory_mb = 2048
agent_timeout_sec = 1200.0
verifier_timeout_sec = 1200.0
image = "alexgshaw/big-one:20251031"
image_digest = "sha256:3333333333333333333333333333333333333333333333333333333333333333"
image_bytes = 6000000000

[[shard]]
name = "search-01"
tasks = ["fix-git", "regex-log"]

[[shard]]
name = "search-02"
tasks = ["big-one"]
"#;

    pub(crate) const SPLIT_JSON: &str = r#"{
  "seed": 3135,
  "algorithm": "tb2-split-v1",
  "search": ["fix-git", "big-one"],
  "held_out": ["regex-log"],
  "smoke": ["fix-git"],
  "calibration": ["fix-git", "big-one"]
}"#;

    pub(crate) fn oracle_config() -> String {
        r#"schema_version = 1
name = "smoke-oracle"
host = "ssh://arch-laptop"
split = "smoke"
k = 1
concurrency = 2

[agent]
kind = "oracle"
"#
        .to_string()
    }

    pub(crate) fn rsi_config() -> String {
        r#"schema_version = 1
name = "calibration-flash"
host = "ssh://arch-laptop"
split = "calibration"
k = 1
concurrency = 4

[agent]
kind = "rsi"
model = "deepseek/deepseek-v4.1-flash"
effort = "high"
per_task_cap_usd = 0.40

[loop]
max_iterations = 200
"#
        .to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::fixtures::*;
    use super::*;

    fn resolve(text: &str) -> Result<ResolvedRun, String> {
        let dataset = Dataset::parse(DATASET_TOML, SPLIT_JSON).unwrap();
        RunFile::parse(text)?.resolve(text, dataset)
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn a_split_resolves_to_its_tasks_the_shard_plan_and_the_config_digest() {
        let run = resolve(&rsi_config()).unwrap();
        assert_eq!(run.tasks, ["big-one", "fix-git"]);
        assert_eq!(run.split_label, "calibration");
        assert_eq!(run.expected_trials(), 2);
        assert!(run.is_paid());
        // The dataset shards, cut to the selected tasks, in dataset order.
        let plan: Vec<(&str, &[String], u64)> = run
            .shards
            .iter()
            .map(|s| (s.name.as_str(), s.tasks.as_slice(), s.bytes))
            .collect();
        assert_eq!(
            plan,
            [
                ("search-01", &["fix-git".to_string()][..], 1_000_000_000),
                ("search-02", &["big-one".to_string()][..], 6_000_000_000),
            ]
        );
        assert_eq!(run.largest_shard_bytes(), 6_000_000_000);
        assert_eq!(
            run.shards[1].images,
            ["alexgshaw/big-one:20251031".to_string()]
        );
        assert_eq!(run.config_digest, sha256_prefixed(rsi_config().as_bytes()));
        assert_eq!(run.task_split("regex-log"), "held_out");
        assert_eq!(run.task_split("fix-git"), "search");
        assert_eq!(run.file.loop_knobs.max_iterations, Some(200));
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn an_explicit_task_list_is_a_custom_split() {
        let text = oracle_config().replace("split = \"smoke\"", "tasks = [\"regex-log\"]");
        let run = resolve(&text).unwrap();
        assert_eq!(run.split_label, "custom");
        assert!(!run.is_paid());
        assert_eq!(run.tasks, ["regex-log"]);
        assert_eq!(run.shards.len(), 1);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn a_malformed_config_is_refused_with_the_reason() {
        let cases: [(String, &str); 12] = [
            (
                oracle_config().replace("schema_version = 1", "schema_version = 2"),
                "schema_version",
            ),
            (
                oracle_config().replace("split = \"smoke\"", ""),
                "exactly one",
            ),
            (
                oracle_config().replace(
                    "split = \"smoke\"",
                    "split = \"smoke\"\ntasks = [\"fix-git\"]",
                ),
                "exactly one",
            ),
            (oracle_config().replace("k = 1", "k = 11"), "k must"),
            (
                oracle_config().replace("k = 1", "k = 1\ntimeout_multiplier = 2.0"),
                "timeout_multiplier",
            ),
            (
                oracle_config().replace("concurrency = 2", "concurrency = 9"),
                "concurrency",
            ),
            (
                oracle_config().replace("ssh://arch-laptop", "laptop"),
                "docker host",
            ),
            (
                oracle_config().replace("kind = \"oracle\"", "kind = \"oracle\"\nmodel = \"x\""),
                "oracle agent",
            ),
            (
                rsi_config().replace("per_task_cap_usd = 0.40", ""),
                "per_task_cap_usd",
            ),
            (
                rsi_config().replace("model = \"deepseek/deepseek-v4.1-flash\"\n", ""),
                "agent.model",
            ),
            (
                rsi_config().replace("max_iterations = 200", "max_iterations = 0"),
                "max_iterations",
            ),
            (
                oracle_config().replace("name = \"smoke-oracle\"", "name = \"a b\""),
                "name",
            ),
        ];
        for (text, needle) in cases {
            let error = resolve(&text).unwrap_err();
            assert!(error.contains(needle), "{needle}: {error}");
        }
        for (text, needle) in [
            (
                oracle_config().replace("split = \"smoke\"", "split = \"nope\""),
                "unknown split",
            ),
            (
                oracle_config().replace("split = \"smoke\"", "tasks = [\"missing\"]"),
                "not a dataset task",
            ),
            (
                oracle_config().replace("split = \"smoke\"", "tasks = [\"fix-git\", \"fix-git\"]"),
                "twice",
            ),
        ] {
            let error = resolve(&text).unwrap_err();
            assert!(error.contains(needle), "{needle}: {error}");
        }
        assert!(RunFile::parse("not toml = [").is_err());
        assert!(
            RunFile::parse(&format!("{}\nsurprise = 1", oracle_config())).is_err(),
            "an unknown top-level field is refused"
        );
    }

    /// #3191: a config names a run overlay by a candidate path, and only a model-driven
    /// run can carry one.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn an_overlay_is_a_candidate_path_and_never_rides_an_oracle_run() {
        let with = |config: String, path: &str| {
            config.replace("[agent]", &format!("overlay = \"{path}\"\n\n[agent]"))
        };
        let ok = "eval/bench/tb2/candidates/c03-1a2b3c4d5e6f/overlay.toml";
        let run = resolve(&with(rsi_config(), ok)).unwrap();
        assert_eq!(run.file.overlay.as_deref(), Some(ok));
        assert_eq!(resolve(&rsi_config()).unwrap().file.overlay, None);
        for bad in [
            "eval/bench/tb2/runs/overlay.toml",
            "eval/bench/tb2/candidates/overlay.toml",
            "eval/bench/tb2/candidates/../runs/c/overlay.toml",
            "eval/bench/tb2/candidates/c/other.toml",
            "/eval/bench/tb2/candidates/c/overlay.toml",
        ] {
            let error = resolve(&with(rsi_config(), bad)).unwrap_err();
            assert!(error.contains("overlay"), "{bad}: {error}");
        }
        let error = resolve(&with(oracle_config(), ok)).unwrap_err();
        assert!(
            error.contains("oracle") && error.contains("overlay"),
            "{error}"
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn the_committed_dataset_and_split_parse_and_every_committed_run_config_resolves() {
        let root = std::path::Path::new(rsi_common::manifest_dir!()).join("../..");
        let dataset_text =
            std::fs::read_to_string(root.join("eval/bench/tb2/dataset.toml")).unwrap();
        let split_text = std::fs::read_to_string(root.join("eval/bench/tb2/split.json")).unwrap();
        let dataset = Dataset::parse(&dataset_text, &split_text).unwrap();
        assert_eq!(dataset.tasks.len(), 89);
        assert_eq!(dataset.header.harbor_version, "0.24.0");
        let runs = root.join("eval/bench/tb2/runs");
        let mut seen = 0;
        for entry in std::fs::read_dir(&runs).unwrap() {
            let path = entry.unwrap().path();
            if path.extension().and_then(|e| e.to_str()) != Some("toml") {
                continue;
            }
            let text = std::fs::read_to_string(&path).unwrap();
            let run = RunFile::parse(&text)
                .and_then(|file| file.resolve(&text, dataset.clone()))
                .unwrap_or_else(|e| panic!("{}: {e}", path.display()));
            assert!(
                rsi_common::agent_jobs::bench_config_path_ok(&format!(
                    "{}{}",
                    rsi_common::agent_jobs::JOB_BENCH_CONFIG_DIR,
                    path.file_name().unwrap().to_string_lossy()
                )),
                "{} is a submittable path",
                path.display()
            );
            assert!(!run.tasks.is_empty());
            seen += 1;
        }
        assert!(seen >= 2, "the committed smoke and calibration configs");
    }
}
