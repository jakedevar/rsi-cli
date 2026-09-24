use rsid::integration::{
    Candidate, CandidateKind, CommitIdentity, GuardCommand, GuardSpec, IntegrationConfig,
    IntegrationError, Prepared, Refusal, discard_candidate, prepare_candidate, run_guard,
};
use serde::Deserialize;
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Output, Stdio};
use std::time::Duration;
use tokio::io::AsyncReadExt;
use tokio::process::Command as TokioCommand;

#[path = "rsi-rolling-land/landing_policy.rs"]
mod landing_policy;

const PROVISIONAL_SCRIPT: &str = "tools/rolling-migration-renumber.py";

#[derive(Debug, Clone)]
struct ProvisionalLanding {
    base: String,
    source: String,
    unit_path: PathBuf,
    unit_candidate: String,
    proof_path: PathBuf,
    assigned_version: u32,
    proof: Value,
}

#[cfg(not(test))]
const REMOTE_LOOKUP_TIMEOUT: Duration = Duration::from_secs(30);
#[cfg(not(test))]
const REMOTE_PUSH_TIMEOUT: Duration = Duration::from_secs(120);
#[cfg(not(test))]
const REMOTE_CHILD_CLEANUP_TIMEOUT: Duration = Duration::from_secs(2);

const TARGET: &str = "refs/heads/rolling";

#[derive(Debug, Clone)]
struct AcceptedPair {
    // Empty only while parsing SOURCE without an explicit historical base.
    base: String,
    source: String,
}

#[derive(Debug, Clone)]
struct Options {
    repo: PathBuf,
    remote: String,
    accepted: Vec<AcceptedPair>,
    test_filters: Vec<String>,
}

#[derive(Debug)]
struct LandReport {
    candidate: String,
    kind: CandidateKind,
    fetched_tip: String,
    published_tip: String,
    provisional: Vec<ProvisionalLanding>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PublicationState {
    NotPublished,
    Published,
    Unknown,
}

impl PublicationState {
    const fn label(self) -> &'static str {
        match self {
            Self::NotPublished => "not_published",
            Self::Published => "published",
            Self::Unknown => "unknown",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FailureKind {
    General,
    Policy,
    Cleanup,
    CanaryRed,
    DescendantGreen,
}

#[derive(Debug)]
struct LandFailure {
    state: PublicationState,
    kind: FailureKind,
    policy_fence: Option<landing_policy::PolicyFence>,
    candidate: Option<String>,
    fetched_tip: Option<String>,
    published_tip: Option<String>,
    observed_tip: Option<String>,
    forward_revert_id: Option<String>,
    forward_revert_status: Option<PublicationState>,
    recovery_path: Option<PathBuf>,
    message: String,
}

impl From<String> for LandFailure {
    fn from(message: String) -> Self {
        Self {
            state: PublicationState::NotPublished,
            kind: FailureKind::General,
            policy_fence: None,
            candidate: None,
            fetched_tip: None,
            published_tip: None,
            observed_tip: None,
            forward_revert_id: None,
            forward_revert_status: None,
            recovery_path: None,
            message,
        }
    }
}

impl LandFailure {
    fn candidate(
        state: PublicationState,
        message: impl Into<String>,
        candidate: &Candidate,
        fetched_tip: &str,
    ) -> Self {
        Self {
            state,
            kind: FailureKind::General,
            policy_fence: None,
            candidate: Some(candidate.oid.clone()),
            fetched_tip: Some(fetched_tip.to_string()),
            published_tip: (state == PublicationState::Published).then(|| candidate.oid.clone()),
            observed_tip: None,
            forward_revert_id: None,
            forward_revert_status: None,
            recovery_path: None,
            message: message.into(),
        }
    }

    fn policy(
        refusal: landing_policy::PolicyRefusal,
        candidate: &Candidate,
        fetched_tip: &str,
    ) -> Self {
        let mut failure = Self::candidate(
            PublicationState::NotPublished,
            refusal.message,
            candidate,
            fetched_tip,
        );
        failure.kind = FailureKind::Policy;
        failure.policy_fence = Some(refusal.fence);
        failure
    }

    fn evidence_lines(&self) -> Vec<String> {
        let mut lines = vec![
            format!("publication_status={}", self.state.label()),
            format!("landing_outcome={}", self.outcome_label()),
            format!("exit_code={}", self.exit_code()),
        ];
        if let Some(fence) = self.policy_fence {
            lines.push(format!("policy_fence={}", fence.label()));
        }
        if let Some(candidate) = &self.candidate {
            lines.push(format!("candidate_id={candidate}"));
        }
        if let Some(fetched_tip) = &self.fetched_tip {
            lines.push(format!("fetched_target_id={fetched_tip}"));
        }
        if let Some(published_tip) = &self.published_tip {
            lines.push(format!("published_target_id={published_tip}"));
        }
        if let Some(observed_tip) = &self.observed_tip {
            lines.push(format!("observed_target_id={observed_tip}"));
        }
        if let Some(forward_revert_id) = &self.forward_revert_id {
            lines.push(format!("forward_revert_id={forward_revert_id}"));
        }
        if let Some(forward_revert_status) = self.forward_revert_status {
            lines.push(format!(
                "forward_revert_status={}",
                forward_revert_status.label()
            ));
        }
        if let Some(recovery_path) = &self.recovery_path {
            lines.push(format!("recovery_path={}", recovery_path.display()));
        }
        lines
    }

    const fn outcome_label(&self) -> &'static str {
        match (self.kind, self.forward_revert_status, self.state) {
            (FailureKind::Policy, _, _) => "policy_refused",
            (FailureKind::Cleanup, _, _) => "published_cleanup_failed",
            (FailureKind::CanaryRed, Some(PublicationState::Published), _) => "canary_red_reverted",
            (FailureKind::CanaryRed, Some(PublicationState::Unknown), _) => {
                "canary_red_revert_unknown"
            }
            (FailureKind::CanaryRed, _, _) => "canary_red_unreverted",
            (FailureKind::DescendantGreen, _, _) => "published_descendant_unverified",
            (_, _, PublicationState::NotPublished) => "not_published",
            (_, _, PublicationState::Published) => "published_unverified",
            (_, _, PublicationState::Unknown) => "publication_unknown",
        }
    }

    const fn exit_code(&self) -> i32 {
        match (self.kind, self.forward_revert_status, self.state) {
            (FailureKind::Policy, _, _) => 8,
            (FailureKind::Cleanup, _, _) => 2,
            (FailureKind::CanaryRed, Some(PublicationState::Published), _) => 4,
            (FailureKind::CanaryRed, Some(PublicationState::Unknown), _) => 3,
            (FailureKind::CanaryRed, _, _) => 5,
            (FailureKind::DescendantGreen, _, _) => 7,
            (_, _, PublicationState::NotPublished) => 1,
            (_, _, PublicationState::Published) => 6,
            (_, _, PublicationState::Unknown) => 3,
        }
    }
}

fn main() {
    let result = parse_args(std::env::args().skip(1))
        .map_err(|error| Box::new(LandFailure::from(error)))
        .and_then(|options| {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .map_err(|error| {
                    Box::new(LandFailure::from(format!(
                        "cannot start async runtime: {error}"
                    )))
                })?;
            runtime.block_on(land(options)).map_err(Box::new)
        });
    match result {
        Ok(report) => {
            println!("publication_status=published");
            println!("candidate_id={}", report.candidate);
            println!("candidate_kind={:?}", report.kind);
            println!("fetched_target_id={}", report.fetched_tip);
            println!("published_target_id={}", report.published_tip);
            for (index, unit) in report.provisional.iter().enumerate() {
                println!("migration_{index}_source_id={}", unit.source);
                println!("migration_{index}_final_version={}", unit.assigned_version);
                println!("migration_{index}_published_id={}", report.published_tip);
                println!("migration_{index}_proof={}", unit.proof);
            }
        }
        Err(error) => {
            for line in error.evidence_lines() {
                println!("{line}");
            }
            eprintln!("rsi-rolling-land: {}", error.message);
            std::process::exit(error.exit_code());
        }
    }
}

fn parse_args(args: impl IntoIterator<Item = String>) -> Result<Options, String> {
    let mut args = args.into_iter();
    let mut repo = std::env::current_dir().map_err(|error| error.to_string())?;
    let mut remote = "origin".to_string();
    let mut accepted = Vec::new();
    let mut test_filters = Vec::new();
    while let Some(argument) = args.next() {
        match argument.as_str() {
            "--repo" => repo = PathBuf::from(next_value(&mut args, "--repo")?),
            "--remote" => remote = next_value(&mut args, "--remote")?,
            "--accepted" => {
                let value = next_value(&mut args, "--accepted")?;
                let (base, source) = value.split_once(':').unwrap_or(("", &value));
                if source.is_empty() || value.starts_with(':') || value.matches(':').count() > 1 {
                    return Err("--accepted expects SOURCE or BASE:SOURCE IDs".to_string());
                }
                accepted.push(AcceptedPair {
                    base: base.to_string(),
                    source: source.to_string(),
                });
            }
            "--test-filter" => test_filters.push(next_value(&mut args, "--test-filter")?),
            "--help" | "-h" => return Err(usage().to_string()),
            _ => return Err(format!("unknown argument `{argument}`\n{}", usage())),
        }
    }
    if accepted.is_empty() {
        return Err(format!(
            "at least one --accepted SOURCE or BASE:SOURCE is required\n{}",
            usage()
        ));
    }
    Ok(Options {
        repo,
        remote,
        accepted,
        test_filters,
    })
}

fn next_value(args: &mut impl Iterator<Item = String>, flag: &str) -> Result<String, String> {
    args.next()
        .ok_or_else(|| format!("{flag} requires a value"))
}

const fn usage() -> &'static str {
    "usage: rsi-rolling-land [--repo PATH] [--remote NAME] --accepted SOURCE|BASE:SOURCE [--accepted SOURCE|BASE:SOURCE ...] [--test-filter PACKAGE=FILTER ...]\nFor a landing candidate, omitted BASE is merge-base(SOURCE, current landing target tip); an explicit BASE must equal it. Already-integrated sources require the explicit historical BASE."
}

#[allow(clippy::too_many_lines)] // Keeps fetch, preparation, and owned cleanup visibly ordered.
async fn land(options: Options) -> Result<LandReport, LandFailure> {
    let mut options = options;
    let cargo_target_dir = validate_cargo_target_dir()?;
    let repo = options
        .repo
        .canonicalize()
        .map_err(|error| format!("cannot resolve repository: {error}"))?;
    let remote_url = git_text(&repo, &["remote", "get-url", "--push", &options.remote])?;
    let workspace_parent = landing_workspace_parent(&cargo_target_dir);
    let temp = tempfile::Builder::new()
        .prefix("rsi-rolling-land-")
        .permissions(std::fs::Permissions::from_mode(0o700))
        .tempdir_in(workspace_parent)
        .map_err(|error| format!("cannot create private landing workspace: {error}"))?;
    let private_repo = temp.path().join("repo");
    git_ok(
        &repo,
        &[
            "clone",
            "--shared",
            "--no-checkout",
            "--quiet",
            &repo.to_string_lossy(),
            &private_repo.to_string_lossy(),
        ],
    )?;
    // Preserve both committed guards without checking out the full source tree.
    for script in [
        "scripts/rolling-landing-guard.py",
        "tools/check-released-migrations.py",
    ] {
        copy_committed_script(&private_repo, script)?;
    }
    if git_output(
        &private_repo,
        &["show", &format!("HEAD:{PROVISIONAL_SCRIPT}")],
    )?
    .status
    .success()
    {
        copy_committed_script(&private_repo, PROVISIONAL_SCRIPT)?;
    }
    for pair in &mut options.accepted {
        if !pair.base.is_empty() {
            pair.base = git_text(
                &private_repo,
                &[
                    "rev-parse",
                    "--verify",
                    &format!("{}^{{commit}}", pair.base),
                ],
            )?;
        }
        pair.source = git_text(
            &private_repo,
            &[
                "rev-parse",
                "--verify",
                &format!("{}^{{commit}}", pair.source),
            ],
        )?;
    }
    for (key, value) in [
        ("maintenance.auto", "false"),
        ("maintenance.autoDetach", "false"),
        ("gc.auto", "0"),
        ("gc.autoDetach", "false"),
        ("fetch.writeCommitGraph", "false"),
    ] {
        git_ok(&private_repo, &["config", "--local", key, value])?;
    }
    git_ok(&private_repo, &["remote", "add", "publish", &remote_url])?;
    // Keep the clone's primary worktree empty. Only engine-owned candidate
    // worktrees materialize files; retained recovery custody stays small.
    git_ok(
        &private_repo,
        &["symbolic-ref", "HEAD", "refs/heads/rsi-private-unborn"],
    )?;
    git_ok(&private_repo, &["update-ref", "-d", TARGET])?;

    // This fetch is intentionally adjacent to candidate preparation. It writes
    // only the private clone's rolling ref, never the caller's local branch.
    remote_fetch(&private_repo).await?;
    let fetched_tip = git_text(
        &private_repo,
        &["rev-parse", "--verify", "refs/heads/rolling^{commit}"],
    )?;
    let config = IntegrationConfig {
        allowed_targets: vec![TARGET.to_string()],
        identity: CommitIdentity {
            name: "rsi rolling landing".to_string(),
            email: "rsi-rolling-land@rsi.invalid".to_string(),
        },
        git_timeout: Duration::from_secs(120),
    };
    let scratch = temp.path().join("candidate-scratch");
    std::fs::create_dir_all(&scratch)
        .map_err(|error| format!("cannot create candidate scratch directory: {error}"))?;
    let mut expected_tip = fetched_tip.clone();
    let mut candidate: Option<Candidate> = None;
    let mut provisional = Vec::<ProvisionalLanding>::new();
    let guard_options = options.clone();
    for pair in &mut options.accepted {
        resolve_accepted_base(&private_repo, pair, &expected_tip)?;
        let pair = pair.clone();
        let unit_path = if !git_is_ancestor(&private_repo, &pair.source, &expected_tip)?
            && has_provisional_declaration(&private_repo, &pair)?
        {
            let mut unit = provisional_command(
                &private_repo,
                &[
                    "--transform",
                    "--base",
                    &pair.base,
                    "--source",
                    &pair.source,
                    "--target",
                    &expected_tip,
                ],
            )?;
            unit["prior_units"] = Value::Array(
                provisional
                    .iter()
                    .map(|prior| {
                        serde_json::json!({
                            "base": prior.base,
                            "source": prior.source,
                            "target": prior.proof["target"],
                            "unit_candidate": prior.unit_candidate,
                        })
                    })
                    .collect(),
            );
            let path = temp
                .path()
                .join(format!("provisional-unit-{}.json", provisional.len()));
            std::fs::write(&path, unit.to_string())
                .map_err(|error| format!("cannot record provisional unit: {error}"))?;
            Some((path, unit))
        } else {
            None
        };
        let prepared: Result<Prepared, String> = async {
            if let Some((path, _unit)) = &unit_path {
                let unit_arg = path.to_string_lossy().into_owned();
                let scratch_arg = scratch.to_string_lossy().into_owned();
                let built = provisional_command(
                    &private_repo,
                    &[
                        "--build",
                        "--unit-file",
                        &unit_arg,
                        "--scratch",
                        &scratch_arg,
                    ],
                )?;
                let exact_candidate = provisional_oid(&built, "candidate")?;
                let prepared = prepare_candidate(
                    &config,
                    &private_repo,
                    TARGET,
                    &expected_tip,
                    exact_candidate,
                    &scratch,
                )
                .await
                .map_err(|error| stale_error(error, &fetched_tip))?;
                Ok(match prepared {
                    Prepared::Candidate(mut next) => {
                        // The commit's checked parents, rather than the engine's
                        // fast-forward classification, describe this candidate.
                        next.kind = CandidateKind::Merge;
                        Prepared::Candidate(next)
                    }
                    other => other,
                })
            } else {
                prepare_candidate(
                    &config,
                    &private_repo,
                    TARGET,
                    &expected_tip,
                    &pair.source,
                    &scratch,
                )
                .await
                .map_err(|error| stale_error(error, &fetched_tip))
            }
        }
        .await;
        let prepared = match prepared {
            Ok(prepared) => prepared,
            Err(error) => {
                if let Some(previous) = candidate.take() {
                    let cleanup = discard_candidate(&config, &private_repo, &previous.handle).await;
                    if let Err(cleanup) = cleanup {
                        return Err(format!("{error}; candidate cleanup failed: {cleanup}").into());
                    }
                }
                return Err(error.into());
            }
        };
        let next = match prepared {
            Prepared::AlreadyIntegrated => continue,
            Prepared::Candidate(next) => next,
        };
        if let Some((unit_path, unit)) = unit_path {
            let unit_arg = unit_path.to_string_lossy().into_owned();
            let checked = (|| -> Result<(Value, PathBuf), String> {
                let proof = provisional_command(
                    &private_repo,
                    &[
                        "--prove",
                        "--unit-file",
                        &unit_arg,
                        "--unit-candidate",
                        &next.oid,
                        "--candidate",
                        &next.oid,
                    ],
                )?;
                let proof_path = temp
                    .path()
                    .join(format!("provisional-proof-{}.json", provisional.len()));
                std::fs::write(&proof_path, proof.to_string())
                    .map_err(|error| format!("cannot record provisional proof: {error}"))?;
                Ok((proof, proof_path))
            })();
            let checked = match checked {
                Ok((proof, proof_path)) => {
                    let guard = run_guard_pair_with_proof(
                        &private_repo,
                        &guard_options,
                        &pair,
                        &expected_tip,
                        &next.oid,
                        None,
                        false,
                        Some(&proof_path),
                    )
                    .await;
                    guard.map(|()| (proof, proof_path))
                }
                Err(error) => Err(error),
            };
            let (proof, proof_path) = match checked {
                Ok(checked) => checked,
                Err(error) => {
                    let mut message = error;
                    if let Err(cleanup) =
                        discard_candidate(&config, &private_repo, &next.handle).await
                    {
                        message.push_str(&format!("; candidate cleanup failed: {cleanup}"));
                    }
                    if let Some(previous) = candidate.take()
                        && let Err(cleanup) =
                            discard_candidate(&config, &private_repo, &previous.handle).await
                    {
                        message.push_str(&format!("; prior cleanup failed: {cleanup}"));
                    }
                    return Err(message.into());
                }
            };
            println!("provisional_unit_proof={proof}");
            provisional.push(ProvisionalLanding {
                base: pair.base.clone(),
                source: pair.source.clone(),
                unit_path,
                unit_candidate: next.oid.clone(),
                proof_path,
                assigned_version: provisional_version(&unit, "new_version")?,
                proof,
            });
        }
        if let Err(error) = git_ok(
            &private_repo,
            &["update-ref", TARGET, &next.oid, &expected_tip],
        ) {
            let next_cleanup = discard_candidate(&config, &private_repo, &next.handle).await;
            if let Some(previous) = candidate.take() {
                let previous_cleanup =
                    discard_candidate(&config, &private_repo, &previous.handle).await;
                if let Err(cleanup) = previous_cleanup {
                    return Err(
                        format!("{error}; prior candidate cleanup failed: {cleanup}").into(),
                    );
                }
            }
            if let Err(cleanup) = next_cleanup {
                return Err(format!("{error}; candidate cleanup failed: {cleanup}").into());
            }
            return Err(error.into());
        }
        if let Some(previous) = candidate.take()
            && let Err(error) = discard_candidate(&config, &private_repo, &previous.handle).await
        {
            let next_cleanup = discard_candidate(&config, &private_repo, &next.handle).await;
            return Err(match next_cleanup {
                Ok(()) => format!("cannot clean superseded candidate: {error}").into(),
                Err(cleanup) => format!(
                    "cannot clean superseded candidate: {error}; current candidate cleanup failed: {cleanup}"
                )
                .into(),
            });
        }
        expected_tip = next.oid.clone();
        candidate = Some(next);
    }
    let Some(candidate) = candidate else {
        for pair in &options.accepted {
            run_guard_pair(
                &private_repo,
                &options,
                pair,
                &fetched_tip,
                &fetched_tip,
                None,
                false,
            )
            .await?;
        }
        return Err(format!(
            "all accepted sources are ancestors of rolling at {fetched_tip}; lost-hunk guard verified them, and no landing candidate was created"
        )
        .into());
    };
    if let Err(error) = finalize_provisional_proofs(&private_repo, &candidate.oid, &mut provisional)
    {
        let cleanup = discard_candidate(&config, &private_repo, &candidate.handle).await;
        return Err(match cleanup {
            Ok(()) => error.into(),
            Err(cleanup) => format!("{error}; candidate cleanup failed: {cleanup}").into(),
        });
    }
    let result = land_candidate(
        &private_repo,
        &repo,
        &remote_url,
        &options,
        &candidate,
        &fetched_tip,
        &provisional,
    )
    .await;
    let cleanup = discard_candidate(&config, &private_repo, &candidate.handle).await;
    let mut outcome = match (result, cleanup) {
        (Ok(report), Ok(())) => Ok(report),
        (Err(error), Ok(())) => Err(error),
        (Ok(_), Err(error)) => {
            let mut failure = LandFailure::candidate(
                PublicationState::Published,
                format!("landing completed but candidate cleanup failed: {error}"),
                &candidate,
                &fetched_tip,
            );
            failure.kind = FailureKind::Cleanup;
            Err(failure)
        }
        (Err(error), Err(cleanup)) => Err(LandFailure {
            message: format!(
                "{}; candidate cleanup also failed: {cleanup}",
                error.message
            ),
            ..error
        }),
    };
    if let Err(error) = &mut outcome
        && (error.state == PublicationState::Unknown
            || error
                .forward_revert_status
                .is_some_and(|state| state != PublicationState::Published))
    {
        // An uncertain remote effect or failed forward revert needs its exact
        // candidate objects for reconciliation. Keep only this private clone;
        // the caller's source and checked-out rolling worktrees are untouched.
        error.recovery_path = Some(temp.keep());
    }
    outcome
}

fn finalize_provisional_proofs(
    repo: &Path,
    final_candidate: &str,
    provisional: &mut [ProvisionalLanding],
) -> Result<(), String> {
    for unit in provisional {
        let unit_arg = unit.unit_path.to_string_lossy().into_owned();
        let proof = provisional_command(
            repo,
            &[
                "--prove",
                "--unit-file",
                &unit_arg,
                "--unit-candidate",
                &unit.unit_candidate,
                "--candidate",
                final_candidate,
            ],
        )?;
        std::fs::write(&unit.proof_path, proof.to_string())
            .map_err(|error| format!("cannot record final provisional proof: {error}"))?;
        unit.proof = proof;
    }
    Ok(())
}

fn resolve_accepted_base(repo: &Path, pair: &mut AcceptedPair, target: &str) -> Result<(), String> {
    if git_is_ancestor(repo, &pair.source, target)? {
        if pair.base.is_empty() {
            return Err(format!(
                "accepted source {} is already integrated; supply --accepted BASE:{} with its historical accepted base for the plan-only lost-hunk check",
                pair.source, pair.source
            ));
        }
        return Ok(());
    }
    let merge_base = git_text(repo, &["merge-base", &pair.source, target])?;
    if pair.base.is_empty() {
        pair.base = merge_base;
    } else if pair.base != merge_base {
        return Err(format!(
            "accepted base {} does not match merge-base(source {}, target {}) = {}; use --accepted {}:{} or omit BASE",
            pair.base, pair.source, target, merge_base, merge_base, pair.source
        ));
    }
    Ok(())
}

fn has_provisional_declaration(repo: &Path, pair: &AcceptedPair) -> Result<bool, String> {
    let output = git_output(
        repo,
        &[
            "diff",
            "--no-renames",
            "--name-only",
            "-z",
            &pair.base,
            &pair.source,
        ],
    )?;
    if !output.status.success() {
        return Err("cannot inspect accepted migration declaration paths".into());
    }
    Ok(output
        .stdout
        .split(|byte| *byte == 0)
        .any(|path| path.starts_with(b"tools/provisional-migrations/") && path.ends_with(b".json")))
}

fn provisional_command(repo: &Path, args: &[&str]) -> Result<Value, String> {
    let script = repo.join(PROVISIONAL_SCRIPT);
    if !script.is_file() {
        return Err(format!(
            "committed provisional migration tool is missing: {}",
            script.display()
        ));
    }
    let mut command = Command::new("python3");
    command
        .arg(&script)
        .arg("--repo")
        .arg(repo)
        .args(args)
        .current_dir(repo)
        .env("PYTHONDONTWRITEBYTECODE", "1")
        .env_remove("PYTHONPATH")
        .env_remove("PYTHONHOME");
    for (key, _) in std::env::vars_os() {
        if key.to_string_lossy().starts_with("GIT_") || key.to_string_lossy().starts_with("RSI_") {
            command.env_remove(key);
        }
    }
    let output = command
        .output()
        .map_err(|error| format!("cannot start provisional migration tool: {error}"))?;
    if !output.status.success() {
        return Err(format!(
            "provisional migration refused: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    serde_json::from_slice(&output.stdout)
        .map_err(|error| format!("provisional migration tool returned invalid JSON: {error}"))
}

fn provisional_version(value: &Value, name: &str) -> Result<u32, String> {
    value[name]
        .as_u64()
        .and_then(|version| u32::try_from(version).ok())
        .ok_or_else(|| format!("provisional migration result lacks {name}"))
}

fn provisional_oid<'a>(value: &'a Value, name: &str) -> Result<&'a str, String> {
    let oid = value[name]
        .as_str()
        .ok_or_else(|| format!("provisional migration result lacks {name}"))?;
    if !matches!(oid.len(), 40 | 64)
        || !oid
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(format!("provisional migration result has invalid {name}"));
    }
    Ok(oid)
}

fn validate_cargo_target_dir() -> Result<PathBuf, String> {
    let configured = std::env::var_os("CARGO_TARGET_DIR").ok_or_else(|| {
        "CARGO_TARGET_DIR must point to the current session's existing sandbox target directory"
            .to_string()
    })?;
    let path = PathBuf::from(configured)
        .canonicalize()
        .map_err(|error| format!("CARGO_TARGET_DIR is unavailable: {error}"))?;
    if !path.is_dir() {
        return Err("CARGO_TARGET_DIR must be an existing directory".into());
    }
    let temp = std::env::temp_dir()
        .canonicalize()
        .map_err(|error| format!("cannot resolve the system temporary directory: {error}"))?;
    if path.starts_with(&temp) {
        return Err("CARGO_TARGET_DIR must not be inside the system temporary directory".into());
    }
    Ok(path)
}

fn landing_workspace_parent(cargo_target_dir: &Path) -> &Path {
    // Keep retained recovery custody outside Cargo's cleanable target tree
    // when the target belongs to an assigned sandbox worktree.
    if cargo_target_dir
        .file_name()
        .is_some_and(|name| name == "target")
        && let Some(sandbox) = cargo_target_dir.parent()
        && sandbox.join(".git").exists()
    {
        return sandbox;
    }
    cargo_target_dir
}

fn stale_error(error: IntegrationError, fetched_tip: &str) -> String {
    match error {
        IntegrationError::Refused(Refusal::StaleTarget { observed }) => {
            format!("stale target: fetched {fetched_tip}, observed {observed:?}")
        }
        other => other.to_string(),
    }
}

#[allow(clippy::too_many_lines)] // Keeps published-tip canary and remote settlement in order.
async fn land_candidate(
    repo: &Path,
    source_repo: &Path,
    remote_url: &str,
    options: &Options,
    candidate: &Candidate,
    fetched_tip: &str,
    provisional: &[ProvisionalLanding],
) -> Result<LandReport, LandFailure> {
    for pair in &options.accepted {
        let proof = provisional
            .iter()
            .find(|unit| unit.source == pair.source && unit.base == pair.base)
            .map(|unit| unit.proof_path.as_path());
        run_guard_pair_with_proof(
            repo,
            options,
            pair,
            fetched_tip,
            &candidate.oid,
            Some(&candidate.handle.worktree),
            false,
            proof,
        )
        .await
        .map_err(|error| {
            LandFailure::candidate(
                PublicationState::NotPublished,
                error,
                candidate,
                fetched_tip,
            )
        })?;
    }

    verify_remote_binding(source_repo, &options.remote, remote_url).map_err(|error| {
        LandFailure::candidate(
            PublicationState::NotPublished,
            error,
            candidate,
            fetched_tip,
        )
    })?;
    let proved_versions = provisional
        .iter()
        .map(|unit| unit.assigned_version)
        .collect::<Vec<_>>();
    landing_policy::check_with_proof(
        repo,
        repo,
        fetched_tip,
        &candidate.oid,
        &options.accepted,
        &proved_versions,
    )
    .map_err(|error| LandFailure::policy(error, candidate, fetched_tip))?;
    let published_tip = publish_candidate(repo, candidate, fetched_tip).await?;
    for pair in &options.accepted {
        let proof = provisional
            .iter()
            .find(|unit| unit.source == pair.source && unit.base == pair.base)
            .map(|unit| unit.proof_path.as_path());
        if let Err(error) = run_guard_pair_with_proof(
            repo,
            options,
            pair,
            fetched_tip,
            &published_tip,
            Some(&candidate.handle.worktree),
            false,
            proof,
        )
        .await
        {
            return Err(forward_revert_after_canary(
                repo,
                source_repo,
                options,
                remote_url,
                candidate,
                fetched_tip,
                error,
            )
            .await);
        }
    }
    match remote_tip(repo, "publish").await {
        Ok(Some(tip)) if tip == published_tip => {}
        Ok(Some(observed)) => {
            // Another ordinary push may have landed on our candidate while
            // the canary ran. Fetch into private custody to prove ancestry;
            // this reports a distinct outcome because the new tip was not
            // covered by our candidate canary.
            if remote_fetch(repo).await.is_ok()
                && git_is_ancestor(repo, &published_tip, &observed).unwrap_or(false)
            {
                let mut failure = LandFailure::candidate(
                    PublicationState::Published,
                    "remote advanced from the green candidate; combined tip needs verification",
                    candidate,
                    fetched_tip,
                );
                failure.kind = FailureKind::DescendantGreen;
                failure.observed_tip = Some(observed);
                return Err(failure);
            }
            let mut failure = LandFailure::candidate(
                PublicationState::Published,
                "post-canary remote tip changed; exact published-tip verification is required",
                candidate,
                fetched_tip,
            );
            failure.observed_tip = Some(observed);
            return Err(failure);
        }
        Ok(None) => {
            let failure = LandFailure::candidate(
                PublicationState::Published,
                "post-canary remote tip is missing; exact published-tip verification is required",
                candidate,
                fetched_tip,
            );
            return Err(failure);
        }
        Err(error) => {
            return Err(LandFailure::candidate(
                PublicationState::Published,
                format!("post-canary remote verification failed: {error}"),
                candidate,
                fetched_tip,
            ));
        }
    }
    verify_remote_binding(source_repo, &options.remote, remote_url).map_err(|error| {
        LandFailure::candidate(PublicationState::Published, error, candidate, fetched_tip)
    })?;
    Ok(LandReport {
        candidate: candidate.oid.clone(),
        kind: candidate.kind,
        fetched_tip: fetched_tip.to_string(),
        published_tip,
        provisional: provisional.to_vec(),
    })
}

fn verify_remote_binding(repo: &Path, remote: &str, expected_url: &str) -> Result<(), String> {
    let observed = git_text(repo, &["remote", "get-url", "--push", remote])?;
    if observed != expected_url {
        return Err("configured publishing remote changed during landing".into());
    }
    Ok(())
}

#[derive(Deserialize)]
struct GuardPlan {
    accepted_source: String,
    candidate: String,
    lost_hunks: Vec<String>,
    affected_crates: Vec<String>,
}

#[derive(Deserialize)]
struct WorkspaceMetadata {
    workspace_members: Vec<String>,
    packages: Vec<WorkspacePackage>,
}

#[derive(Deserialize)]
struct WorkspacePackage {
    id: String,
    name: String,
    dependencies: Vec<WorkspaceDependency>,
}

#[derive(Deserialize)]
struct WorkspaceDependency {
    name: String,
    path: Option<PathBuf>,
}

fn workspace_metadata(worktree: &Path) -> Result<WorkspaceMetadata, String> {
    let output = Command::new("cargo")
        .args([
            "metadata",
            "--no-deps",
            "--format-version",
            "1",
            "--offline",
        ])
        .current_dir(worktree)
        .output()
        .map_err(|error| format!("cannot inspect candidate workspace: {error}"))?;
    if !output.status.success() {
        return Err(format!(
            "cannot inspect candidate workspace: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    serde_json::from_slice(&output.stdout)
        .map_err(|error| format!("invalid candidate workspace metadata: {error}"))
}

fn reverse_workspace_dependents(
    metadata: &WorkspaceMetadata,
    affected: &[String],
) -> Result<Vec<String>, String> {
    let members: BTreeSet<&str> = metadata
        .workspace_members
        .iter()
        .map(String::as_str)
        .collect();
    let packages: BTreeMap<&str, &WorkspacePackage> = metadata
        .packages
        .iter()
        .filter(|package| members.contains(package.id.as_str()))
        .map(|package| (package.name.as_str(), package))
        .collect();
    for name in affected {
        if !packages.contains_key(name.as_str()) {
            return Err(format!("affected crate {name} is not a workspace package"));
        }
    }
    let mut reached: BTreeSet<&str> = affected.iter().map(String::as_str).collect();
    loop {
        let newly_reached: Vec<&str> = packages
            .iter()
            .filter(|(name, package)| {
                !reached.contains(*name)
                    && package.dependencies.iter().any(|dependency| {
                        dependency.path.is_some() && reached.contains(dependency.name.as_str())
                    })
            })
            .map(|(name, _)| *name)
            .collect();
        if newly_reached.is_empty() {
            break;
        }
        reached.extend(newly_reached);
    }
    Ok(reached
        .into_iter()
        .filter(|name| !affected.iter().any(|affected| affected == name))
        .map(str::to_owned)
        .collect())
}

async fn run_guard_pair(
    repo: &Path,
    options: &Options,
    pair: &AcceptedPair,
    fetched_tip: &str,
    candidate: &str,
    worktree: Option<&Path>,
    allow_lost_hunks: bool,
) -> Result<(), String> {
    run_guard_pair_with_proof(
        repo,
        options,
        pair,
        fetched_tip,
        candidate,
        worktree,
        allow_lost_hunks,
        None,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
async fn run_guard_pair_with_proof(
    repo: &Path,
    options: &Options,
    pair: &AcceptedPair,
    fetched_tip: &str,
    candidate: &str,
    worktree: Option<&Path>,
    allow_lost_hunks: bool,
    proof: Option<&Path>,
) -> Result<(), String> {
    let script = repo.join("scripts/rolling-landing-guard.py");
    if !script.is_file() {
        return Err(format!(
            "landing guard script is missing: {}",
            script.display()
        ));
    }
    let args = vec![
        script.to_string_lossy().into_owned(),
        "--repo".into(),
        repo.to_string_lossy().into_owned(),
        "--base".into(),
        pair.base.clone(),
        "--source".into(),
        pair.source.clone(),
        "--target".into(),
        fetched_tip.into(),
        "--candidate".into(),
        candidate.into(),
        "--plan-only".into(),
    ];
    let mut args = args;
    if let Some(proof) = proof {
        args.extend(["--proof".into(), proof.to_string_lossy().into_owned()]);
    }
    for filter in &options.test_filters {
        args.extend(["--test-filter".into(), filter.clone()]);
    }
    let plan_report = run_guard(
        repo,
        &GuardSpec {
            commands: vec![GuardCommand {
                program: "python3".into(),
                args,
                timeout: Duration::from_secs(120),
            }],
            env: BTreeMap::new(),
            output_tail_bytes: 256 * 1024,
        },
    )
    .await;
    let Some(plan_command) = plan_report.commands.first() else {
        return Err("rolling landing guard did not run".into());
    };
    let plan: GuardPlan = serde_json::from_str(&plan_command.stdout_tail).map_err(|error| {
        format!(
            "landing guard rejected accepted pair {}:{} ({:?}): {error}; {}",
            pair.base, pair.source, plan_command.status, plan_command.stderr_tail
        )
    })?;
    if plan.accepted_source != pair.source || plan.candidate != candidate {
        return Err("landing guard plan does not match the accepted source and candidate".into());
    }
    if let Some(worktree) = worktree {
        if git_text(worktree, &["rev-parse", "HEAD"])? != candidate
            || !git_text(worktree, &["status", "--porcelain=v1"])?.is_empty()
        {
            return Err("landing guard worktree must be clean at the candidate".into());
        }
        let metadata = workspace_metadata(worktree)?;
        let spec = affected_crate_guard_spec(
            fetched_tip,
            candidate,
            &plan.affected_crates,
            &options.test_filters,
            proof.is_some(),
            &metadata,
        )?;
        let report = run_guard(worktree, &spec).await;
        if !report.passed {
            let failed = report
                .commands
                .last()
                .ok_or("landing guard executed no command")?;
            return Err(format!(
                "affected-crate guard failed ({:?}) running {} {:?}: {} {}",
                failed.status, failed.program, failed.args, failed.stdout_tail, failed.stderr_tail
            ));
        }
    }
    if !plan.lost_hunks.is_empty() && !allow_lost_hunks {
        return Err(format!(
            "rolling landing guard rejected accepted pair {}:{} after affected-crate tests: {}",
            pair.base,
            pair.source,
            plan.lost_hunks.join("; ")
        ));
    }
    if !plan_report.passed && (!allow_lost_hunks || plan.lost_hunks.is_empty()) {
        return Err(format!(
            "rolling landing guard rejected accepted pair {}:{} ({:?}): {}",
            pair.base, pair.source, plan_command.status, plan_command.stderr_tail
        ));
    }
    if proof.is_some() {
        landing_policy::check_released_migrations(repo, repo, fetched_tip, candidate)?;
    }
    Ok(())
}

fn affected_crate_guard_spec(
    target: &str,
    candidate: &str,
    packages: &[String],
    filters: &[String],
    provisional: bool,
    metadata: &WorkspaceMetadata,
) -> Result<GuardSpec, String> {
    let mut selected: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
    for filter in filters {
        let (package, value) = filter
            .split_once('=')
            .ok_or_else(|| format!("invalid test filter: {filter}"))?;
        if !packages.iter().any(|affected| affected == package)
            || value.is_empty()
            || value.starts_with('-')
        {
            return Err(format!("invalid test filter: {filter}"));
        }
        selected.entry(package).or_default().push(value);
    }
    let mut commands = vec![GuardCommand {
        program: "git".into(),
        args: vec![
            "diff".into(),
            "--check".into(),
            target.into(),
            candidate.into(),
        ],
        timeout: Duration::from_secs(30),
    }];
    if provisional {
        for filter in [
            "store::tests::rewind_tears_down_the_non_idempotent_migration_tail",
            "store::tests::every_recovered_migration_step_actually_executes",
        ] {
            commands.push(cargo_guard_command("rsid", Some(filter)));
        }
    }
    for package in packages {
        let package_filters = selected.get(package.as_str());
        if let Some(package_filters) = package_filters {
            for filter in package_filters {
                commands.push(cargo_guard_command(package, Some(filter)));
            }
        } else {
            commands.push(cargo_guard_command(package, None));
        }
    }
    for dependent in reverse_workspace_dependents(metadata, packages)? {
        commands.push(cargo_check_command(&dependent));
    }
    Ok(GuardSpec {
        commands,
        env: BTreeMap::from([
            ("CARGO_BUILD_JOBS".into(), "4".into()),
            ("CARGO_PROFILE_DEV_DEBUG".into(), "line-tables-only".into()),
        ]),
        output_tail_bytes: 16 * 1024,
    })
}

fn cargo_check_command(package: &str) -> GuardCommand {
    GuardCommand {
        program: "cargo".into(),
        args: vec![
            "check".into(),
            "-p".into(),
            package.into(),
            "--all-targets".into(),
        ],
        timeout: Duration::from_secs(20 * 60),
    }
}

fn cargo_guard_command(package: &str, filter: Option<&str>) -> GuardCommand {
    let mut args = vec!["test".into(), "-p".into(), package.into(), "--lib".into()];
    if let Some(filter) = filter {
        args.push(filter.into());
    }
    args.extend(["--".into(), "--test-threads=4".into()]);
    GuardCommand {
        program: "cargo".into(),
        args,
        timeout: Duration::from_secs(20 * 60),
    }
}

async fn forward_revert_after_canary(
    repo: &Path,
    source_repo: &Path,
    options: &Options,
    remote_url: &str,
    candidate: &Candidate,
    fetched_tip: &str,
    canary_error: String,
) -> LandFailure {
    let mut failure = LandFailure::candidate(
        PublicationState::Published,
        format!("published-tip canary failed: {canary_error}"),
        candidate,
        fetched_tip,
    );
    failure.kind = FailureKind::CanaryRed;
    if let Err(error) = verify_remote_binding(source_repo, &options.remote, remote_url) {
        failure.message.push_str("; forward revert blocked: ");
        failure.message.push_str(&error);
        failure.forward_revert_status = Some(PublicationState::NotPublished);
        return failure;
    }
    let tree = match git_text(repo, &["rev-parse", &format!("{fetched_tip}^{{tree}}")]) {
        Ok(tree) => tree,
        Err(error) => {
            failure
                .message
                .push_str("; cannot prepare forward revert: ");
            failure.message.push_str(&error);
            failure.forward_revert_status = Some(PublicationState::NotPublished);
            return failure;
        }
    };
    // A new child of the published candidate restores the prior rolling tree.
    // Accepted source remains in the ancestry even after a red canary.
    let revert = match git_text(
        repo,
        &[
            "-c",
            "user.name=rsi rolling landing",
            "-c",
            "user.email=rsi-rolling-land@rsi.invalid",
            "commit-tree",
            &tree,
            "-p",
            &candidate.oid,
            "-m",
            "Forward revert failed rolling canary",
        ],
    ) {
        Ok(revert) => revert,
        Err(error) => {
            failure
                .message
                .push_str("; cannot prepare forward revert: ");
            failure.message.push_str(&error);
            failure.forward_revert_status = Some(PublicationState::NotPublished);
            return failure;
        }
    };
    failure.forward_revert_id = Some(revert.clone());
    let mut revert_candidate = candidate.clone();
    revert_candidate.oid = revert.clone();
    let publication = publish_candidate(repo, &revert_candidate, &candidate.oid).await;
    let publication = match publication {
        Err(error) if error.state == PublicationState::NotPublished => {
            retry_forward_revert(
                repo,
                source_repo,
                options,
                remote_url,
                candidate,
                fetched_tip,
                error,
            )
            .await
        }
        other => other,
    };
    match publication {
        Ok(observed) => {
            failure.forward_revert_status = Some(PublicationState::Published);
            failure.forward_revert_id = Some(observed.clone());
            failure.observed_tip = Some(observed);
            failure
                .message
                .push_str("; forward revert published and verified");
        }
        Err(error) => {
            failure.forward_revert_status = Some(error.state);
            failure.forward_revert_id = error.candidate;
            failure.observed_tip = error.observed_tip;
            failure.message.push_str("; forward revert not verified: ");
            failure.message.push_str(&error.message);
        }
    }
    failure
}

/// Rebuild the inverse landing delta on a freshly observed descendant. The
/// original pre-landing tree is never substituted for another lead's work.
async fn retry_forward_revert(
    repo: &Path,
    source_repo: &Path,
    options: &Options,
    remote_url: &str,
    original: &Candidate,
    fetched_tip: &str,
    mut stale: LandFailure,
) -> Result<String, LandFailure> {
    for _attempt in 0..2 {
        let Some(observed) = stale.observed_tip.clone() else {
            break;
        };
        if remote_fetch(repo).await.is_err()
            || !git_is_ancestor(repo, &original.oid, &observed).unwrap_or(false)
        {
            break;
        }
        let scratch = match tempfile::Builder::new()
            .prefix("rsi-forward-revert-")
            .permissions(std::fs::Permissions::from_mode(0o700))
            .tempdir_in(repo.parent().unwrap_or(repo))
        {
            Ok(scratch) => scratch,
            Err(error) => {
                stale.message = format!("cannot create forward-revert scratch: {error}");
                break;
            }
        };
        let tree_path = scratch.path().join("tree");
        let tree_arg = tree_path.to_string_lossy().to_string();
        if let Err(error) = git_ok(
            repo,
            &[
                "worktree", "add", "--detach", "--quiet", &tree_arg, &observed,
            ],
        ) {
            stale.message = format!("cannot prepare forward-revert worktree: {error}");
            break;
        }
        let attempt = prepare_descendant_revert(
            repo,
            &tree_path,
            scratch.path(),
            original,
            fetched_tip,
            &observed,
        );
        let attempt = match attempt {
            Ok(revert) => {
                let mut guard_result = Ok(());
                for pair in &options.accepted {
                    if let Err(error) = run_guard_pair(
                        repo,
                        options,
                        pair,
                        &observed,
                        &revert,
                        Some(&tree_path),
                        true,
                    )
                    .await
                    {
                        guard_result = Err(error);
                        break;
                    }
                }
                guard_result.map(|()| revert)
            }
            Err(error) => Err(error),
        };
        let cleanup = git_ok(repo, &["worktree", "remove", "--force", &tree_arg]);
        let revert = match (attempt, cleanup) {
            (Ok(revert), Ok(())) => revert,
            (Err(error), Ok(())) | (Ok(_), Err(error)) => {
                stale.message = format!("forward revert retry blocked: {error}");
                break;
            }
            (Err(error), Err(cleanup)) => {
                stale.message = format!(
                    "forward revert retry blocked: {error}; worktree cleanup failed: {cleanup}"
                );
                break;
            }
        };
        let mut candidate = original.clone();
        candidate.oid = revert;
        if let Err(error) = verify_remote_binding(source_repo, &options.remote, remote_url) {
            stale.message = format!("forward revert retry blocked by remote rebinding: {error}");
            stale.candidate = Some(candidate.oid);
            break;
        }
        match publish_candidate(repo, &candidate, &observed).await {
            Ok(tip) => return Ok(tip),
            Err(error) if error.state == PublicationState::NotPublished => stale = error,
            Err(error) => return Err(error),
        }
    }
    Err(stale)
}

fn prepare_descendant_revert(
    repo: &Path,
    worktree: &Path,
    scratch: &Path,
    original: &Candidate,
    fetched_tip: &str,
    observed: &str,
) -> Result<String, String> {
    let patch = git_output(
        repo,
        &["diff", "--binary", &original.oid, fetched_tip, "--"],
    )?;
    if !patch.status.success() {
        return Err("cannot compute inverse landing delta".into());
    }
    let patch_path = scratch.join("inverse.patch");
    std::fs::write(&patch_path, patch.stdout)
        .map_err(|error| format!("cannot record inverse landing delta: {error}"))?;
    git_ok(
        worktree,
        &["apply", "--3way", "--index", &patch_path.to_string_lossy()],
    )?;
    let tree = git_text(worktree, &["write-tree"])?;
    let revert = git_text(
        repo,
        &[
            "-c",
            "user.name=rsi rolling landing",
            "-c",
            "user.email=rsi-rolling-land@rsi.invalid",
            "commit-tree",
            &tree,
            "-p",
            observed,
            "-m",
            "Forward revert failed rolling canary on advanced target",
        ],
    )?;
    git_ok(worktree, &["reset", "--hard", &revert])?;
    Ok(revert)
}

async fn publish_candidate(
    repo: &Path,
    candidate: &Candidate,
    fetched_tip: &str,
) -> Result<String, LandFailure> {
    let observed = remote_tip(repo, "publish").await.map_err(|error| {
        LandFailure::candidate(
            PublicationState::NotPublished,
            error,
            candidate,
            fetched_tip,
        )
    })?;
    if observed.as_deref() != Some(fetched_tip) {
        let mut failure = LandFailure::candidate(
            PublicationState::NotPublished,
            format!(
                "stale target before push: fetched {fetched_tip}, remote now resolves to {observed:?}"
            ),
            candidate,
            fetched_tip,
        );
        failure.observed_tip = observed;
        return Err(failure);
    }

    // A normal push has Git's default fast-forward-only behavior. The full
    // candidate OID is the source, so no branch name or force refspec is used.
    let refspec = format!("{}:refs/heads/rolling", candidate.oid);
    let mut push = remote_git_command(repo, &["push", "--porcelain", "publish", &refspec]);
    push.stdin(Stdio::null())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit());
    let mut push = push.spawn().map_err(|error| {
        LandFailure::candidate(
            PublicationState::NotPublished,
            format!("cannot push candidate to remote: {error}"),
            candidate,
            fetched_tip,
        )
    })?;
    let push_pid = child_pid(&push, "push candidate to remote").map_err(|error| {
        LandFailure::candidate(PublicationState::Unknown, error, candidate, fetched_tip)
    })?;
    let push_result = tokio::time::timeout(remote_push_timeout(), push.wait()).await;
    let (context, reported_success) = match push_result {
        Ok(Ok(status)) if status.success() => ("push reported success".to_string(), true),
        Ok(Ok(status)) => (format!("push exited {}", status_code(status)), false),
        Ok(Err(error)) => {
            terminate_and_reap(&mut push, push_pid).await;
            (format!("cannot wait for candidate push: {error}"), false)
        }
        Err(_) => {
            terminate_and_reap(&mut push, push_pid).await;
            ("fast-forward-only push timed out".to_string(), false)
        }
    };
    confirm_post_push(repo, candidate, fetched_tip, &context, reported_success).await
}

async fn confirm_post_push(
    repo: &Path,
    candidate: &Candidate,
    fetched_tip: &str,
    context: &str,
    reported_success: bool,
) -> Result<String, LandFailure> {
    match remote_tip(repo, "publish").await {
        Ok(Some(tip)) if tip == candidate.oid => Ok(tip),
        Ok(Some(tip)) if tip == fetched_tip && !reported_success => Err(LandFailure::candidate(
            PublicationState::NotPublished,
            format!("{context}; remote rolling remained at {fetched_tip}"),
            candidate,
            fetched_tip,
        )),
        Ok(observed) => {
            let mut failure = LandFailure::candidate(
                PublicationState::Unknown,
                format!(
                    "{context}; remote rolling resolves to {observed:?}, publication is unconfirmed"
                ),
                candidate,
                fetched_tip,
            );
            failure.observed_tip = observed;
            Err(failure)
        }
        Err(error) => Err(LandFailure::candidate(
            PublicationState::Unknown,
            format!("{context}; remote verification failed: {error}"),
            candidate,
            fetched_tip,
        )),
    }
}

async fn remote_fetch(repo: &Path) -> Result<(), String> {
    let mut command = remote_git_command(
        repo,
        &[
            "fetch",
            "--no-tags",
            "--no-auto-maintenance",
            "publish",
            "refs/heads/rolling:refs/heads/rolling",
        ],
    );
    command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let mut child = command
        .spawn()
        .map_err(|error| format!("cannot fetch remote rolling: {error}"))?;
    let pid = child_pid(&child, "remote rolling fetch")?;
    match tokio::time::timeout(remote_lookup_timeout(), child.wait()).await {
        Ok(Ok(status)) if status.success() => Ok(()),
        Ok(Ok(status)) => Err(format!(
            "remote rolling fetch failed (exit {})",
            status_code(status)
        )),
        Ok(Err(error)) => {
            terminate_and_reap(&mut child, pid).await;
            Err(format!("cannot wait for remote rolling fetch: {error}"))
        }
        Err(_) => {
            terminate_and_reap(&mut child, pid).await;
            Err(format!(
                "remote rolling fetch timed out after {:?}",
                remote_lookup_timeout()
            ))
        }
    }
}

async fn remote_tip(repo: &Path, remote: &str) -> Result<Option<String>, String> {
    const MAX_REMOTE_RESPONSE: usize = 1024;
    let mut command = remote_git_command(repo, &["ls-remote", remote, "refs/heads/rolling"]);
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    let mut child = command
        .spawn()
        .map_err(|error| format!("cannot read remote rolling tip: {error}"))?;
    let pid = child_pid(&child, "remote rolling lookup")?;
    let Some(stdout) = child.stdout.take() else {
        terminate_and_reap(&mut child, pid).await;
        return Err("cannot read remote rolling tip: stdout unavailable".to_string());
    };
    let mut bytes = Vec::new();
    let deadline = tokio::time::Instant::now() + remote_lookup_timeout();
    let read_result = tokio::time::timeout_at(
        deadline,
        stdout
            .take((MAX_REMOTE_RESPONSE + 1) as u64)
            .read_to_end(&mut bytes),
    )
    .await;
    match read_result {
        Ok(Ok(_)) => {}
        Ok(Err(error)) => {
            terminate_and_reap(&mut child, pid).await;
            return Err(format!("cannot read remote rolling tip: {error}"));
        }
        Err(_) => {
            terminate_and_reap(&mut child, pid).await;
            return Err(format!(
                "remote rolling lookup timed out after {:?}",
                remote_lookup_timeout()
            ));
        }
    }
    if bytes.len() > MAX_REMOTE_RESPONSE {
        terminate_and_reap(&mut child, pid).await;
        return Err("remote rolling lookup exceeded the 1024-byte response limit".into());
    }
    let status = match tokio::time::timeout_at(deadline, child.wait()).await {
        Ok(Ok(status)) => status,
        Ok(Err(error)) => {
            terminate_and_reap(&mut child, pid).await;
            return Err(format!("cannot wait for remote rolling lookup: {error}"));
        }
        Err(_) => {
            terminate_and_reap(&mut child, pid).await;
            return Err(format!(
                "remote rolling lookup timed out after {:?}",
                remote_lookup_timeout()
            ));
        }
    };
    if !status.success() {
        return Err(format!(
            "cannot read remote rolling tip: git ls-remote exited {}",
            status_code(status)
        ));
    }
    let value = std::str::from_utf8(&bytes)
        .map_err(|_| "remote rolling lookup returned invalid UTF-8".to_string())?;
    let Some(line) = value.lines().next() else {
        return Ok(None);
    };
    if value.lines().nth(1).is_some() {
        return Err("remote rolling lookup returned more than one ref line".into());
    }
    let mut fields = line.split_whitespace();
    let oid = fields
        .next()
        .ok_or_else(|| "remote rolling lookup returned a malformed ref line".to_string())?;
    let reference = fields
        .next()
        .ok_or_else(|| "remote rolling lookup returned a malformed ref line".to_string())?;
    if fields.next().is_some()
        || !matches!(oid.len(), 40 | 64)
        || !oid
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        || reference != "refs/heads/rolling"
    {
        return Err("remote rolling lookup returned a malformed ref line".into());
    }
    Ok(Some(oid.to_string()))
}

fn remote_git_command(repo: &Path, args: &[&str]) -> TokioCommand {
    let mut command = TokioCommand::new("git");
    command
        .current_dir(repo)
        .args([
            "--no-optional-locks",
            "-c",
            "core.hooksPath=/dev/null",
            "-c",
            "commit.gpgsign=false",
            "-c",
            "maintenance.auto=false",
            "-c",
            "gc.auto=0",
            "-c",
            "fetch.writeCommitGraph=false",
        ])
        .args(args);
    for (key, _) in std::env::vars_os() {
        if key.to_string_lossy().starts_with("GIT_") {
            command.env_remove(key);
        }
    }
    command.env("GIT_TERMINAL_PROMPT", "0");
    command.as_std_mut().process_group(0);
    command.kill_on_drop(true);
    command
}

fn child_pid(child: &tokio::process::Child, operation: &str) -> Result<nix::unistd::Pid, String> {
    child
        .id()
        .map(|pid| nix::unistd::Pid::from_raw(pid.cast_signed()))
        .ok_or_else(|| format!("cannot identify {operation} process"))
}

async fn terminate_and_reap(child: &mut tokio::process::Child, process_group: nix::unistd::Pid) {
    use nix::sys::signal::{Signal, kill, killpg};

    let _ = killpg(process_group, Signal::SIGKILL);
    let reaped = tokio::time::timeout(remote_cleanup_timeout(), child.wait()).await;
    if !matches!(reaped, Ok(Ok(_))) {
        let _ = kill(process_group, Signal::SIGKILL);
        let _ = child.start_kill();
        let _ = child.wait().await;
    }
}

const fn remote_lookup_timeout() -> Duration {
    #[cfg(test)]
    {
        Duration::from_secs(2)
    }
    #[cfg(not(test))]
    {
        REMOTE_LOOKUP_TIMEOUT
    }
}

const fn remote_push_timeout() -> Duration {
    #[cfg(test)]
    {
        Duration::from_secs(2)
    }
    #[cfg(not(test))]
    {
        REMOTE_PUSH_TIMEOUT
    }
}

const fn remote_cleanup_timeout() -> Duration {
    #[cfg(test)]
    {
        Duration::from_secs(1)
    }
    #[cfg(not(test))]
    {
        REMOTE_CHILD_CLEANUP_TIMEOUT
    }
}

fn status_code(status: ExitStatus) -> String {
    status
        .code()
        .map_or_else(|| "signal".to_string(), |code| code.to_string())
}

fn git_text(repo: &Path, args: &[&str]) -> Result<String, String> {
    let output = git_output(repo, args)?;
    if !output.status.success() {
        return Err(format!(
            "git {} failed: {}",
            args.first().copied().unwrap_or("?"),
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

fn git_ok(repo: &Path, args: &[&str]) -> Result<(), String> {
    git_text(repo, args).map(|_| ())
}

fn git_is_ancestor(repo: &Path, older: &str, newer: &str) -> Result<bool, String> {
    let output = git_output(repo, &["merge-base", "--is-ancestor", older, newer])?;
    match output.status.code() {
        Some(0) => Ok(true),
        Some(1) => Ok(false),
        _ => Err("cannot verify observed remote ancestry".into()),
    }
}

fn git_output(repo: &Path, args: &[&str]) -> Result<Output, String> {
    Command::new("git")
        .args([
            "--no-optional-locks",
            "-c",
            "core.hooksPath=/dev/null",
            "-c",
            "commit.gpgsign=false",
            "-c",
            "maintenance.auto=false",
            "-c",
            "gc.auto=0",
            "-c",
            "fetch.writeCommitGraph=false",
            "-C",
        ])
        .arg(repo)
        .args(args)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_INDEX_FILE")
        .output()
        .map_err(|error| format!("cannot start git: {error}"))
}

fn copy_committed_script(repo: &Path, relative_path: &str) -> Result<(), String> {
    let script = git_output(repo, &["show", &format!("HEAD:{relative_path}")])?;
    if !script.status.success() {
        return Err(format!("committed {relative_path} is missing"));
    }
    let path = repo.join(relative_path);
    let parent = path.parent().ok_or("committed script path has no parent")?;
    std::fs::create_dir_all(parent)
        .map_err(|error| format!("cannot create private script directory: {error}"))?;
    std::fs::write(path, script.stdout)
        .map_err(|error| format!("cannot copy committed {relative_path}: {error}"))
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::await_holding_lock,
        clippy::expect_used,
        clippy::similar_names,
        clippy::significant_drop_tightening,
        clippy::unwrap_used,
        clippy::used_underscore_binding
    )]

    use super::*;
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use std::process::Stdio;
    use std::sync::Mutex;
    use tempfile::TempDir;

    static ENV_LOCK: Mutex<()> = Mutex::new(());

    fn guard_workspace_metadata() -> WorkspaceMetadata {
        fn package(name: &str, dependencies: &[&str]) -> WorkspacePackage {
            WorkspacePackage {
                id: name.into(),
                name: name.into(),
                dependencies: dependencies
                    .iter()
                    .map(|dependency| WorkspaceDependency {
                        name: (*dependency).into(),
                        path: Some(PathBuf::from(format!("crates/{dependency}"))),
                    })
                    .collect(),
            }
        }
        WorkspaceMetadata {
            workspace_members: ["rsi-common", "rsi-graph", "rsi", "rsid", "rsi-baseline"]
                .map(str::to_owned)
                .to_vec(),
            packages: vec![
                package("rsi-common", &[]),
                package("rsi-graph", &["rsi-common"]),
                package("rsi", &["rsi-graph"]),
                package("rsid", &["rsi-common", "rsi-graph"]),
                package("rsi-baseline", &[]),
            ],
        }
    }

    struct Fixture {
        _environment_lock: std::sync::MutexGuard<'static, ()>,
        root: TempDir,
        repo: PathBuf,
        bare: PathBuf,
        base: String,
        bin: PathBuf,
    }

    impl Fixture {
        fn new() -> Self {
            let environment_lock = ENV_LOCK
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let root = tempfile::tempdir().expect("temp root");
            let repo = root.path().join("repo");
            let bare = root.path().join("remote.git");
            let bin = root.path().join("bin");
            fs::create_dir_all(&repo).expect("repository directory");
            fs::create_dir_all(&bin).expect("bin directory");
            git_run(&repo, &["init", "-q", "-b", "rolling"]);
            write(
                &repo,
                "Cargo.toml",
                "[workspace]\nmembers = [\"crates/demo\"]\nresolver = \"2\"\n",
            );
            write(
                &repo,
                "crates/demo/Cargo.toml",
                "[package]\nname = \"demo\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
            );
            write(
                &repo,
                "crates/demo/src/lib.rs",
                "pub fn value() -> &'static str { \"base\" }\n",
            );
            let source_guard = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("../../scripts/rolling-landing-guard.py");
            fs::create_dir_all(repo.join("scripts")).expect("scripts dir");
            fs::copy(source_guard, repo.join("scripts/rolling-landing-guard.py"))
                .expect("copy current guard");
            fs::create_dir_all(repo.join("tools")).expect("tools dir");
            fs::copy(
                PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                    .join("../../tools/check-released-migrations.py"),
                repo.join("tools/check-released-migrations.py"),
            )
            .expect("copy released guard");
            write(
                &repo,
                "tools/released-migrations.json",
                "{\"migration_file\":\"crates/rsid/src/store/mod.rs\",\"protected_sections\":{}}\n",
            );
            git_run(&repo, &["add", "."]);
            git_run(&repo, &["commit", "-q", "-m", "base"]);
            let base = git_value(&repo, &["rev-parse", "HEAD"]);
            git_run(
                root.path(),
                &["init", "--bare", "-q", bare.to_str().unwrap()],
            );
            git_run(&repo, &["remote", "add", "origin", bare.to_str().unwrap()]);
            git_run(&repo, &["push", "-q", "origin", "rolling"]);
            Self {
                _environment_lock: environment_lock,
                root,
                repo,
                bare,
                base,
                bin,
            }
        }

        fn commit(&self, parent: &str, file: &str, content: &str, message: &str) -> String {
            let path = self
                .root
                .path()
                .join(format!("builder-{}", uuid::Uuid::new_v4()));
            git_run(
                &self.repo,
                &[
                    "worktree",
                    "add",
                    "-q",
                    "--detach",
                    path.to_str().unwrap(),
                    parent,
                ],
            );
            write(&path, file, content);
            git_run(&path, &["add", file]);
            git_run(&path, &["commit", "-q", "-m", message]);
            let oid = git_value(&path, &["rev-parse", "HEAD"]);
            git_run(&self.repo, &["worktree", "remove", path.to_str().unwrap()]);
            oid
        }

        fn add_fake_cargo(&self, extra: &str) {
            let script = format!("#!/bin/sh\n{extra}\nexit 0\n");
            let cargo = self.bin.join("cargo");
            fs::write(&cargo, script).expect("fake cargo");
            let mut permissions = fs::metadata(&cargo).expect("metadata").permissions();
            permissions.set_mode(0o755);
            fs::set_permissions(cargo, permissions).expect("chmod fake cargo");
        }

        fn add_fake_git(&self, ls_remote: &str) {
            self.add_fake_git_behaviors(ls_remote, "");
        }

        fn add_fake_git_behaviors(&self, ls_remote: &str, push: &str) {
            self.add_fake_git_behaviors_with_fetch(ls_remote, push, "");
        }

        fn add_fake_git_behaviors_with_fetch(&self, ls_remote: &str, push: &str, fetch: &str) {
            let ls_remote_case = if ls_remote.is_empty() {
                String::new()
            } else {
                format!("  if [ \"$arg\" = ls-remote ]; then\n{ls_remote}\n    exit 0\n  fi\n")
            };
            let push_case = if push.is_empty() {
                String::new()
            } else {
                format!("  if [ \"$arg\" = push ]; then\n{push}\n    exit 0\n  fi\n")
            };
            let fetch_case = if fetch.is_empty() {
                String::new()
            } else {
                format!("  if [ \"$arg\" = fetch ]; then\n{fetch}\n    exit 0\n  fi\n")
            };
            let script = format!(
                "#!/bin/sh\nfor arg in \"$@\"; do\n{ls_remote_case}{push_case}{fetch_case}done\nexec /usr/bin/git \"$@\"\n"
            );
            let git = self.bin.join("git");
            fs::write(&git, script).expect("fake git");
            let mut permissions = fs::metadata(&git).expect("metadata").permissions();
            permissions.set_mode(0o755);
            fs::set_permissions(git, permissions).expect("chmod fake git");
        }

        fn add_fake_cargo_log(&self) -> PathBuf {
            let args = self.root.path().join("cargo-args.log");
            let env = self.root.path().join("cargo-env.log");
            let script = format!(
                "#!/bin/sh\nprintf '%s\\n' \"$@\" >> '{}'\nprintf '%s\\n%s\\n%s\\n%s\\n' \"$CARGO_TARGET_DIR\" \"$CARGO_BUILD_JOBS\" \"$CARGO_PROFILE_DEV_DEBUG\" \"$(pwd -P)\" >> '{}'\nexit 0\n",
                args.display(),
                env.display()
            );
            let cargo = self.bin.join("cargo");
            fs::write(&cargo, script).expect("fake cargo with invocation log");
            let mut permissions = fs::metadata(&cargo).expect("metadata").permissions();
            permissions.set_mode(0o755);
            fs::set_permissions(cargo, permissions).expect("chmod fake cargo");
            args
        }

        async fn land(&self, accepted: Vec<AcceptedPair>) -> Result<LandReport, LandFailure> {
            self.land_with_filters(accepted, Vec::new()).await
        }

        async fn land_with_filters(
            &self,
            accepted: Vec<AcceptedPair>,
            test_filters: Vec<String>,
        ) -> Result<LandReport, LandFailure> {
            land(Options {
                repo: self.repo.clone(),
                remote: "origin".into(),
                accepted,
                test_filters,
            })
            .await
        }
    }

    fn write(root: &Path, relative: &str, contents: &str) {
        let path = root.join(relative);
        fs::create_dir_all(path.parent().unwrap()).expect("parent directory");
        fs::write(path, contents).expect("fixture file");
    }

    fn git_run(cwd: &Path, args: &[&str]) {
        let output = Command::new("/usr/bin/git")
            .current_dir(cwd)
            .args(args)
            .env("GIT_AUTHOR_NAME", "Fixture")
            .env("GIT_AUTHOR_EMAIL", "fixture@example.invalid")
            .env("GIT_COMMITTER_NAME", "Fixture")
            .env("GIT_COMMITTER_EMAIL", "fixture@example.invalid")
            .env("HOME", "/nonexistent")
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE")
            .env_remove("GIT_INDEX_FILE")
            .stdin(Stdio::null())
            .output()
            .expect("git launch");
        assert!(
            output.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn git_value(cwd: &Path, args: &[&str]) -> String {
        let output = Command::new("/usr/bin/git")
            .current_dir(cwd)
            .args(args)
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE")
            .env_remove("GIT_INDEX_FILE")
            .output()
            .expect("git launch");
        assert!(
            output.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).trim().to_string()
    }

    fn use_fake_path(fixture: &Fixture) -> Option<std::ffi::OsString> {
        let old = std::env::var_os("PATH");
        let path = format!(
            "{}:{}",
            fixture.bin.display(),
            old.as_deref().unwrap_or_default().to_string_lossy()
        );
        // Tests serialize all PATH changes through ENV_LOCK.
        unsafe { std::env::set_var("PATH", path) };
        old
    }

    fn restore_path(old: Option<std::ffi::OsString>) {
        // Tests serialize all PATH changes through ENV_LOCK.
        unsafe {
            if let Some(old) = old {
                std::env::set_var("PATH", old);
            } else {
                std::env::remove_var("PATH");
            }
        }
    }

    fn policy_work(source: &str, domain: &str, version: u32) -> serde_json::Value {
        serde_json::json!({
            "key": "landing-work",
            "epic_id": "epic-j",
            "source_commit": source,
            "source_accepted": true,
            "ownership": [{
                "work_key": "landing-work",
                "active": true,
                "mode": "exclusive",
                "domain": domain,
                "files": [domain]
            }],
            "migration_reservations": [{
                "work_key": "landing-work",
                "version": version,
                "row_version": 1
            }]
        })
    }

    #[test]
    fn committed_released_guard_replaces_dirty_worktree_copy() {
        let fixture = Fixture::new();
        let script = "tools/check-released-migrations.py";
        let committed = git_output(&fixture.repo, &["show", &format!("HEAD:{script}")])
            .expect("committed script");
        write(&fixture.repo, script, "raise SystemExit(0)\n");
        copy_committed_script(&fixture.repo, script).expect("restore committed script");
        assert_eq!(
            fs::read(fixture.repo.join(script)).unwrap(),
            committed.stdout
        );
    }

    #[test]
    fn hermetic_hot_file_requires_live_claim_bound_to_accepted_source() {
        let fixture = Fixture::new();
        let source = fixture.commit(&fixture.base, "AGENTS.md", "policy\n", "hot file");
        let accepted = [AcceptedPair {
            base: fixture.base.clone(),
            source: source.clone(),
        }];
        let check = |rows| {
            landing_policy::check_with_work(
                &fixture.repo,
                &fixture.repo,
                &fixture.base,
                &source,
                &accepted,
                || Ok(rows),
            )
        };
        check(vec![policy_work(&source, "AGENTS.md", 125)]).expect("live claim passes");
        assert!(
            check(vec![])
                .unwrap_err()
                .message
                .contains("lacks a visible live Work")
        );
        let wrong_source = fixture.base.clone();
        assert!(
            check(vec![policy_work(&wrong_source, "AGENTS.md", 125)])
                .unwrap_err()
                .message
                .contains("lacks a visible live Work")
        );
        assert!(
            check(vec![policy_work(&source, "other.md", 125)])
                .unwrap_err()
                .message
                .contains("no live exclusive landing ownership")
        );
        assert_eq!(
            check(vec![policy_work(&source, "other.md", 125)])
                .unwrap_err()
                .fence,
            landing_policy::PolicyFence::HotFileUnowned
        );
    }

    #[test]
    fn hermetic_migration_order_requires_each_new_version_reservation() {
        let fixture = Fixture::new();
        let store = "crates/rsid/src/store/mod.rs";
        let v124 = fixture.commit(
            &fixture.base,
            store,
            "pub const LATEST_SCHEMA_VERSION: i32 = 124;\n",
            "schema 124",
        );
        let v126 = fixture.commit(
            &v124,
            store,
            "pub const LATEST_SCHEMA_VERSION: i32 = 126;\n",
            "schema 126",
        );
        // The released-migration script has its own fixture suite. This fixture
        // isolates the landing policy's comparison and reservation sequence.
        write(
            fixture.root.path(),
            "tools/check-released-migrations.py",
            "import sys\nsys.exit(0)\n",
        );
        let accepted = [AcceptedPair {
            base: v124.clone(),
            source: v126.clone(),
        }];
        let check = |rows| {
            landing_policy::check_with_work(
                &fixture.repo,
                fixture.root.path(),
                &v124,
                &v126,
                &accepted,
                || Ok(rows),
            )
        };
        let mut row = policy_work(&v126, store, 126);
        assert!(
            check(vec![row.clone()])
                .unwrap_err()
                .message
                .contains("V125")
        );
        row["migration_reservations"] = serde_json::json!([
            {"work_key":"landing-work","version":125,"row_version":1},
            {"work_key":"landing-work","version":126,"row_version":1}
        ]);
        check(vec![row]).expect("contiguous new versions are reserved");
        landing_policy::check_with_work_and_proof(
            &fixture.repo,
            fixture.root.path(),
            &v124,
            &v126,
            &accepted,
            &[125, 126],
            || Ok(vec![policy_work(&v126, store, 999)]),
        )
        .expect("proved migration versions need no pre-landing reservation");
        let error = landing_policy::check_with_work_and_proof(
            &fixture.repo,
            fixture.root.path(),
            &v124,
            &v126,
            &accepted,
            &[127],
            || Ok(vec![policy_work(&v126, store, 999)]),
        )
        .expect_err("proof outside the appended range must refuse");
        assert_eq!(error.fence, landing_policy::PolicyFence::SchemaVersion);
    }

    #[test]
    fn provisional_guard_keeps_rewind_and_replay_tests_with_user_filter() {
        let spec = affected_crate_guard_spec(
            "1111111111111111111111111111111111111111",
            "2222222222222222222222222222222222222222",
            &["rsid".into()],
            &["rsid=topology_v129".into()],
            true,
            &guard_workspace_metadata(),
        )
        .expect("valid affected crate filter");
        assert_eq!(
            spec.env.get("CARGO_BUILD_JOBS").map(String::as_str),
            Some("4")
        );
        for command in spec
            .commands
            .iter()
            .filter(|command| command.program == "cargo")
        {
            assert_eq!(
                command.args[command.args.len() - 2..],
                ["--", "--test-threads=4"]
            );
        }
        let commands = spec
            .commands
            .iter()
            .map(|command| command.args.join(" "))
            .collect::<Vec<_>>();
        assert!(commands.iter().any(|command| {
            command.contains("rewind_tears_down_the_non_idempotent_migration_tail")
        }));
        assert!(commands.iter().any(|command| command.contains("every_recovered_migration_step_actually_executes")));
        assert!(
            commands
                .iter()
                .any(|command| command.contains("topology_v129"))
        );
    }

    #[test]
    fn changed_library_checks_transitive_workspace_dependents() {
        let spec = affected_crate_guard_spec(
            "1111111111111111111111111111111111111111",
            "2222222222222222222222222222222222222222",
            &["rsi-common".into()],
            &["rsi-common=manager_operator_delegation".into()],
            false,
            &guard_workspace_metadata(),
        )
        .expect("valid workspace graph");
        let cargo_commands = spec
            .commands
            .iter()
            .filter(|command| command.program == "cargo")
            .map(|command| command.args.join(" "))
            .collect::<Vec<_>>();
        assert_eq!(
            cargo_commands,
            [
                "test -p rsi-common --lib manager_operator_delegation -- --test-threads=4",
                "check -p rsi --all-targets",
                "check -p rsi-graph --all-targets",
                "check -p rsid --all-targets",
            ]
        );

        let direct = affected_crate_guard_spec(
            "1111111111111111111111111111111111111111",
            "2222222222222222222222222222222222222222",
            &["rsid".into()],
            &[],
            false,
            &guard_workspace_metadata(),
        )
        .expect("valid single-crate change");
        let cargo_commands = direct
            .commands
            .iter()
            .filter(|command| command.program == "cargo")
            .map(|command| command.args.join(" "))
            .collect::<Vec<_>>();
        assert_eq!(cargo_commands, ["test -p rsid --lib -- --test-threads=4"]);
    }

    #[test]
    fn released_guard_runs_for_pinned_file_without_store_mod_change() {
        let fixture = Fixture::new();
        let protected = "crates/rsid/src/store/manager_ledger/facts.rs";
        let manifest = format!(
            "{{\"migration_file\":\"crates/rsid/src/store/mod.rs\",\"protected_sections\":{{\"facts\":{{\"path\":\"{protected}\"}}}}}}\n"
        );
        let base = fixture.commit(
            &fixture.base,
            "tools/released-migrations.json",
            &manifest,
            "pin facts",
        );
        let source = fixture.commit(&base, protected, "edited\n", "change pinned facts");
        write(
            fixture.root.path(),
            "tools/check-released-migrations.py",
            "import sys\nsys.exit(2)\n",
        );
        let error = landing_policy::check_with_work(
            &fixture.repo,
            fixture.root.path(),
            &base,
            &source,
            &[AcceptedPair {
                base: base.clone(),
                source: source.clone(),
            }],
            || Ok(vec![]),
        )
        .unwrap_err();
        assert_eq!(error.fence, landing_policy::PolicyFence::ReleasedMigration);
        assert!(error.message.contains("released-migration guard refused"));
    }

    #[test]
    fn lowered_schema_version_has_stable_policy_fence() {
        let fixture = Fixture::new();
        let store = "crates/rsid/src/store/mod.rs";
        let base = fixture.commit(
            &fixture.base,
            store,
            "pub const LATEST_SCHEMA_VERSION: i32 = 126;\n",
            "schema 126",
        );
        let source = fixture.commit(
            &base,
            store,
            "pub const LATEST_SCHEMA_VERSION: i32 = 125;\n",
            "lower schema",
        );
        write(
            fixture.root.path(),
            "tools/check-released-migrations.py",
            "import sys\nsys.exit(0)\n",
        );
        let error = landing_policy::check_with_work(
            &fixture.repo,
            fixture.root.path(),
            &base,
            &source,
            &[AcceptedPair {
                base: base.clone(),
                source: source.clone(),
            }],
            || Ok(vec![]),
        )
        .unwrap_err();
        assert_eq!(error.fence, landing_policy::PolicyFence::SchemaVersion);
    }

    #[tokio::test]
    async fn hot_file_without_live_ledger_refuses_before_push() {
        let fixture = Fixture::new();
        let source = fixture.commit(&fixture.base, "AGENTS.md", "policy\n", "hot file");
        fixture.add_fake_cargo("");
        let old_token = std::env::var_os("RSI_SESSION_TOKEN");
        unsafe { std::env::remove_var("RSI_SESSION_TOKEN") };
        let old_path = use_fake_path(&fixture);
        let result = fixture
            .land(vec![AcceptedPair {
                base: fixture.base.clone(),
                source,
            }])
            .await;
        restore_path(old_path);
        if let Some(token) = old_token {
            unsafe { std::env::set_var("RSI_SESSION_TOKEN", token) };
        }
        let refusal = result.expect_err("missing ledger access must refuse");
        assert_eq!(refusal.state, PublicationState::NotPublished);
        assert_eq!(refusal.exit_code(), 8);
        assert!(
            refusal
                .evidence_lines()
                .contains(&"landing_outcome=policy_refused".into())
        );
        assert!(
            refusal
                .evidence_lines()
                .contains(&"policy_fence=ledger_unavailable".into())
        );
        assert!(
            refusal
                .message
                .contains("requires an rsi-managed Epic lead")
        );
        assert_eq!(
            git_value(&fixture.bare, &["rev-parse", "refs/heads/rolling"]),
            fixture.base
        );
        assert_eq!(
            git_value(&fixture.repo, &["rev-parse", "refs/heads/rolling"]),
            fixture.base
        );
    }

    #[tokio::test]
    async fn fast_forward_candidate_pushes_exact_source_oid() {
        let fixture = Fixture::new();
        let source = fixture.commit(
            &fixture.base,
            "crates/demo/src/lib.rs",
            "pub fn value() -> &'static str { \"accepted\" }\n",
            "accepted source",
        );
        fixture.add_fake_cargo("");
        let old_path = use_fake_path(&fixture);
        let old_git_dir = std::env::var_os("GIT_DIR");
        unsafe {
            std::env::set_var("GIT_DIR", fixture.root.path().join("not-a-git-dir"));
        }
        let result = fixture
            .land(vec![AcceptedPair {
                base: fixture.base[..12].to_string(),
                source: source[..12].to_string(),
            }])
            .await;
        restore_path(old_path);
        unsafe {
            if let Some(old_git_dir) = old_git_dir {
                std::env::set_var("GIT_DIR", old_git_dir);
            } else {
                std::env::remove_var("GIT_DIR");
            }
        }
        let report = result.expect("landing succeeds");
        assert_eq!(report.candidate, source);
        assert_eq!(report.kind, CandidateKind::FastForward);
        assert_eq!(
            git_value(&fixture.repo, &["rev-parse", "refs/heads/rolling"]),
            fixture.base
        );
        assert_eq!(
            git_value(&fixture.bare, &["rev-parse", "refs/heads/rolling"]),
            source
        );
    }

    #[tokio::test]
    async fn derived_base_allows_landing_after_rolling_regenerates_merged_file() {
        let fixture = Fixture::new();
        let feature = fixture.commit(&fixture.base, "feature.txt", "feature\n", "branch feature");
        let generated = fixture.commit(
            &fixture.base,
            "crates/demo/src/lib.rs",
            "pub fn value() -> &'static str { \"generated\" }\n",
            "regenerate on rolling",
        );
        let source_tree = fixture.root.path().join("source-merge");
        git_run(
            &fixture.repo,
            &[
                "worktree",
                "add",
                "-q",
                "--detach",
                source_tree.to_str().unwrap(),
                &feature,
            ],
        );
        git_run(
            &source_tree,
            &["merge", "-q", "--no-ff", &generated, "-m", "merge rolling"],
        );
        let source = git_value(&source_tree, &["rev-parse", "HEAD"]);
        git_run(
            &fixture.repo,
            &["worktree", "remove", source_tree.to_str().unwrap()],
        );
        let target = fixture.commit(
            &generated,
            "crates/demo/src/lib.rs",
            "pub fn value() -> &'static str { \"base\" }\n",
            "regenerate again on rolling",
        );
        git_run(
            &fixture.repo,
            &[
                "push",
                "-q",
                "origin",
                &format!("{target}:refs/heads/rolling"),
            ],
        );
        assert_eq!(
            git_value(&fixture.repo, &["merge-base", &source, &target]),
            generated
        );

        let old_base = fixture
            .land(vec![AcceptedPair {
                base: fixture.base.clone(),
                source: source.clone(),
            }])
            .await
            .expect_err("historical base predates the derived merge-base");
        assert!(old_base.message.contains(&generated), "{old_base:?}");
        assert!(old_base.message.contains("omit BASE"), "{old_base:?}");
        assert_eq!(old_base.state, PublicationState::NotPublished);

        let report = fixture
            .land(vec![AcceptedPair {
                base: String::new(),
                source: source.clone(),
            }])
            .await
            .expect("derived-base landing succeeds");
        assert_eq!(report.kind, CandidateKind::Merge);
        assert_eq!(
            git_value(&fixture.bare, &["rev-parse", "refs/heads/rolling"]),
            report.published_tip
        );
        assert_eq!(
            git_value(
                &fixture.bare,
                &["show", &format!("{}:feature.txt", report.candidate)]
            ),
            "feature"
        );
        assert_eq!(
            git_value(
                &fixture.bare,
                &[
                    "show",
                    &format!("{}:crates/demo/src/lib.rs", report.candidate)
                ]
            ),
            "pub fn value() -> &'static str { \"base\" }"
        );
    }

    #[tokio::test]
    async fn explicit_nonancestor_base_must_equal_derived_merge_base() {
        let fixture = Fixture::new();
        let first_hunk = fixture.commit(
            &fixture.base,
            "crates/demo/src/lib.rs",
            "pub fn value() -> &'static str { \"accepted\" }\n",
            "first branch hunk",
        );
        let source = fixture.commit(
            &first_hunk,
            "feature.txt",
            "feature\n",
            "second branch hunk",
        );
        let unrelated = fixture.commit(&fixture.base, "unrelated.txt", "other\n", "other branch");
        let target = fixture.commit(&fixture.base, "target.txt", "target\n", "rolling change");
        git_run(
            &fixture.repo,
            &[
                "push",
                "-q",
                "origin",
                &format!("{target}:refs/heads/rolling"),
            ],
        );

        for wrong_base in [first_hunk, unrelated] {
            let error = fixture
                .land(vec![AcceptedPair {
                    base: wrong_base.clone(),
                    source: source.clone(),
                }])
                .await
                .expect_err("wrong explicit base must be refused before candidate preparation");
            assert_eq!(error.state, PublicationState::NotPublished);
            assert!(
                error.message.contains("does not match merge-base"),
                "{error:?}"
            );
            assert!(error.message.contains(&wrong_base), "{error:?}");
            assert!(error.message.contains(&fixture.base), "{error:?}");
            assert!(error.message.contains("omit BASE"), "{error:?}");
            assert_eq!(
                git_value(&fixture.bare, &["rev-parse", "refs/heads/rolling"]),
                target
            );
        }
    }

    #[tokio::test]
    async fn derived_base_still_detects_reverted_branch_only_hunk() {
        let fixture = Fixture::new();
        let source = fixture.commit(
            &fixture.base,
            "crates/demo/src/lib.rs",
            "pub fn value() -> &'static str { \"accepted\" }\n",
            "branch-only hunk",
        );
        let target = fixture.commit(&fixture.base, "target.txt", "target\n", "rolling change");
        let reverted = fixture.commit(
            &source,
            "crates/demo/src/lib.rs",
            "pub fn value() -> &'static str { \"base\" }\n",
            "revert branch hunk",
        );
        let candidate_tree = fixture.root.path().join("candidate-merge");
        git_run(
            &fixture.repo,
            &[
                "worktree",
                "add",
                "-q",
                "--detach",
                candidate_tree.to_str().unwrap(),
                &reverted,
            ],
        );
        git_run(
            &candidate_tree,
            &["merge", "-q", "--no-ff", &target, "-m", "merge target"],
        );
        let candidate = git_value(&candidate_tree, &["rev-parse", "HEAD"]);
        git_run(
            &fixture.repo,
            &["worktree", "remove", candidate_tree.to_str().unwrap()],
        );
        let mut pair = AcceptedPair {
            base: String::new(),
            source,
        };
        resolve_accepted_base(&fixture.repo, &mut pair, &target).expect("derive base");
        assert_eq!(pair.base, fixture.base);
        let options = Options {
            repo: fixture.repo.clone(),
            remote: "origin".into(),
            accepted: vec![pair.clone()],
            test_filters: Vec::new(),
        };
        let error = run_guard_pair(
            &fixture.repo,
            &options,
            &pair,
            &target,
            &candidate,
            None,
            false,
        )
        .await
        .expect_err("reverted source hunk must be detected");
        assert!(error.contains("restored base line"), "{error}");
    }

    #[tokio::test]
    async fn red_published_tip_canary_creates_ancestry_preserving_forward_revert() {
        let fixture = Fixture::new();
        let source = fixture.commit(
            &fixture.base,
            "crates/demo/src/lib.rs",
            "pub fn value() -> &'static str { \"accepted\" }\n",
            "accepted source",
        );
        let first_run = fixture.root.path().join("prepublish-guard-passed");
        fixture.add_fake_cargo(&format!(
            "if [ -e '{}' ]; then exit 42; fi\n: > '{}'",
            first_run.display(),
            first_run.display()
        ));
        let old_path = use_fake_path(&fixture);
        let result = fixture
            .land(vec![AcceptedPair {
                base: fixture.base.clone(),
                source: source.clone(),
            }])
            .await;
        restore_path(old_path);
        let failure = result.expect_err("red post-publish canary must fail the landing");
        assert_eq!(failure.state, PublicationState::Published);
        assert_eq!(
            failure.forward_revert_status,
            Some(PublicationState::Published)
        );
        let revert = failure.forward_revert_id.expect("forward revert commit");
        assert_eq!(failure.observed_tip.as_deref(), Some(revert.as_str()));
        assert_eq!(
            git_value(&fixture.bare, &["rev-parse", "refs/heads/rolling"]),
            revert
        );
        assert_eq!(
            git_value(&fixture.bare, &["rev-parse", &format!("{revert}^1")]),
            source
        );
        assert_eq!(
            git_value(&fixture.bare, &["rev-parse", &format!("{revert}^{{tree}}")]),
            git_value(
                &fixture.repo,
                &["rev-parse", &format!("{}^{{tree}}", fixture.base)]
            )
        );
        assert_eq!(
            git_value(&fixture.repo, &["rev-parse", "refs/heads/rolling"]),
            fixture.base
        );
    }

    #[tokio::test]
    async fn red_merge_canary_reverts_to_prior_target_tree_without_losing_source_ancestry() {
        let fixture = Fixture::new();
        let source = fixture.commit(
            &fixture.base,
            "crates/demo/src/lib.rs",
            "pub fn value() -> &'static str { \"accepted\" }\n",
            "accepted source",
        );
        let target = fixture.commit(&fixture.base, "target.txt", "target\n", "target side");
        git_run(
            &fixture.repo,
            &[
                "push",
                "-q",
                "origin",
                &format!("{target}:refs/heads/rolling"),
            ],
        );
        let first_run = fixture.root.path().join("prepublish-guard-passed");
        fixture.add_fake_cargo(&format!(
            "if [ -e '{}' ]; then exit 42; fi\n: > '{}'",
            first_run.display(),
            first_run.display()
        ));
        let old_path = use_fake_path(&fixture);
        let result = fixture
            .land(vec![AcceptedPair {
                base: fixture.base.clone(),
                source: source.clone(),
            }])
            .await;
        restore_path(old_path);
        let failure = result.expect_err("red merge canary must forward revert");
        let candidate = failure.candidate.expect("merge candidate");
        let revert = failure.forward_revert_id.expect("forward revert commit");
        assert_eq!(
            failure.forward_revert_status,
            Some(PublicationState::Published)
        );
        assert_eq!(
            git_value(&fixture.bare, &["rev-parse", &format!("{revert}^1")]),
            candidate
        );
        assert_eq!(
            git_value(&fixture.bare, &["rev-parse", &format!("{revert}^{{tree}}")]),
            git_value(&fixture.bare, &["rev-parse", &format!("{target}^{{tree}}")])
        );
        git_run(
            &fixture.bare,
            &["merge-base", "--is-ancestor", &source, &revert],
        );
    }

    #[tokio::test]
    async fn concurrent_remote_advance_blocks_forward_revert_without_overwrite() {
        let fixture = Fixture::new();
        let source = fixture.commit(
            &fixture.base,
            "crates/demo/src/lib.rs",
            "pub fn value() -> &'static str { \"accepted\" }\n",
            "accepted source",
        );
        let other = fixture.commit(&source, "other.txt", "later work\n", "concurrent work");
        let first_run = fixture.root.path().join("prepublish-guard-passed");
        fixture.add_fake_cargo(&format!(
            "if [ -e '{}' ]; then /usr/bin/git -C '{}' push -q origin '{}:refs/heads/rolling' || exit 43; exit 42; fi\n: > '{}'",
            first_run.display(),
            fixture.repo.display(),
            other,
            first_run.display()
        ));
        let old_path = use_fake_path(&fixture);
        let result = fixture
            .land(vec![AcceptedPair {
                base: fixture.base.clone(),
                source: source.clone(),
            }])
            .await;
        restore_path(old_path);
        let failure = result.expect_err("red canary with concurrent advance must fail");
        assert_eq!(failure.state, PublicationState::Published);
        assert_eq!(
            failure.forward_revert_status,
            Some(PublicationState::NotPublished)
        );
        assert!(failure.forward_revert_id.is_some());
        assert_eq!(
            git_value(&fixture.bare, &["rev-parse", "refs/heads/rolling"]),
            other
        );
        assert_eq!(
            git_value(&fixture.repo, &["rev-parse", "refs/heads/rolling"]),
            fixture.base
        );
        let recovery_path = failure.recovery_path.expect("private recovery clone");
        assert!(recovery_path.join("repo/.git").exists());
        fs::remove_dir_all(recovery_path).expect("cleanup private test recovery clone");
    }

    #[tokio::test]
    async fn concurrent_descendant_revert_preserves_other_landing() {
        let fixture = Fixture::new();
        let source = fixture.commit(
            &fixture.base,
            "crates/demo/src/lib.rs",
            "pub fn value() -> &'static str { \"accepted\" }\n",
            "accepted source",
        );
        let other = fixture.commit(&source, "other.txt", "later work\n", "concurrent work");
        let first = fixture.root.path().join("prepublish-guard-passed");
        let second = fixture.root.path().join("red-canary-observed");
        fixture.add_fake_cargo(&format!(
            "if [ ! -e '{}' ]; then : > '{}'; exit 0; fi\nif [ ! -e '{}' ]; then /usr/bin/git -C '{}' push -q origin '{}:refs/heads/rolling' || exit 43; : > '{}'; exit 42; fi",
            first.display(), first.display(), second.display(),
            fixture.repo.display(), other, second.display()
        ));
        let old_path = use_fake_path(&fixture);
        let result = fixture
            .land(vec![AcceptedPair {
                base: fixture.base.clone(),
                source: source.clone(),
            }])
            .await;
        restore_path(old_path);
        let failure = result.expect_err("red canary requires a forward revert");
        assert_eq!(
            failure.forward_revert_status,
            Some(PublicationState::Published)
        );
        assert_eq!(failure.exit_code(), 4);
        let revert = failure.forward_revert_id.expect("verified revert commit");
        assert_eq!(
            git_value(&fixture.bare, &["rev-parse", &format!("{revert}^1")]),
            other
        );
        assert_eq!(
            git_value(&fixture.bare, &["show", &format!("{revert}:other.txt")]),
            "later work"
        );
        assert_eq!(
            git_value(
                &fixture.bare,
                &["show", &format!("{revert}:crates/demo/src/lib.rs")]
            ),
            "pub fn value() -> &'static str { \"base\" }"
        );
        assert_eq!(
            git_value(&fixture.bare, &["rev-parse", "refs/heads/rolling"]),
            revert
        );
    }

    #[tokio::test]
    async fn two_stale_revert_attempts_keep_recovery_and_remote_work() {
        let fixture = Fixture::new();
        let source = fixture.commit(
            &fixture.base,
            "crates/demo/src/lib.rs",
            "pub fn value() -> &'static str { \"accepted\" }\n",
            "accepted source",
        );
        let other1 = fixture.commit(&source, "other1.txt", "one\n", "first advance");
        let other2 = fixture.commit(&other1, "other2.txt", "two\n", "second advance");
        let other3 = fixture.commit(&other2, "other3.txt", "three\n", "third advance");
        let marker = fixture.root.path().join("guard-run-count");
        fixture.add_fake_cargo(&format!(
            "count=$(cat '{}' 2>/dev/null || echo 0)\ncount=$((count + 1))\necho \"$count\" > '{}'\ncase $count in\n  2) /usr/bin/git -C '{}' push -q origin '{}:refs/heads/rolling' || exit 43; exit 42;;\n  3) /usr/bin/git -C '{}' push -q origin '{}:refs/heads/rolling' || exit 43;;\n  4) /usr/bin/git -C '{}' push -q origin '{}:refs/heads/rolling' || exit 43;;\nesac",
            marker.display(), marker.display(), fixture.repo.display(), other1,
            fixture.repo.display(), other2, fixture.repo.display(), other3,
        ));
        let old_path = use_fake_path(&fixture);
        let result = fixture
            .land(vec![AcceptedPair {
                base: fixture.base.clone(),
                source,
            }])
            .await;
        restore_path(old_path);
        let failure = result.expect_err("bounded retries must stop after two stale tips");
        assert_eq!(
            failure.forward_revert_status,
            Some(PublicationState::NotPublished)
        );
        assert_eq!(failure.exit_code(), 5);
        assert_eq!(failure.observed_tip.as_deref(), Some(other3.as_str()));
        assert_eq!(fs::read_to_string(marker).unwrap().trim(), "4");
        assert_eq!(
            git_value(&fixture.bare, &["rev-parse", "refs/heads/rolling"]),
            other3
        );
        let recovery_path = failure.recovery_path.expect("red remote retains custody");
        assert!(recovery_path.join("repo/.git").exists());
        fs::remove_dir_all(recovery_path).expect("cleanup private test recovery clone");
    }

    #[tokio::test]
    async fn green_canary_reports_descendant_tip_separately() {
        let fixture = Fixture::new();
        let source = fixture.commit(
            &fixture.base,
            "crates/demo/src/lib.rs",
            "pub fn value() -> &'static str { \"accepted\" }\n",
            "accepted source",
        );
        let other = fixture.commit(&source, "other.txt", "later work\n", "concurrent work");
        let first = fixture.root.path().join("prepublish-guard-passed");
        fixture.add_fake_cargo(&format!(
            "if [ -e '{}' ]; then /usr/bin/git -C '{}' push -q origin '{}:refs/heads/rolling' || exit 43; else : > '{}'; fi",
            first.display(), fixture.repo.display(), other, first.display()
        ));
        let old_path = use_fake_path(&fixture);
        let result = fixture
            .land(vec![AcceptedPair {
                base: fixture.base.clone(),
                source,
            }])
            .await;
        restore_path(old_path);
        let failure = result.expect_err("combined descendant was not canary-tested");
        assert_eq!(failure.kind, FailureKind::DescendantGreen);
        assert_eq!(failure.exit_code(), 7);
        assert_eq!(failure.observed_tip.as_deref(), Some(other.as_str()));
        assert_eq!(
            git_value(&fixture.bare, &["rev-parse", "refs/heads/rolling"]),
            other
        );
    }

    #[tokio::test]
    async fn changed_configured_push_remote_blocks_publication() {
        let fixture = Fixture::new();
        let source = fixture.commit(
            &fixture.base,
            "crates/demo/src/lib.rs",
            "pub fn value() -> &'static str { \"accepted\" }\n",
            "accepted source",
        );
        let other_bare = fixture.root.path().join("other-remote.git");
        git_run(
            fixture.root.path(),
            &["init", "--bare", "-q", other_bare.to_str().unwrap()],
        );
        fixture.add_fake_cargo(&format!(
            "/usr/bin/git -C '{}' remote set-url --push origin '{}'",
            fixture.repo.display(),
            other_bare.display()
        ));
        let old_path = use_fake_path(&fixture);
        let result = fixture
            .land(vec![AcceptedPair {
                base: fixture.base.clone(),
                source,
            }])
            .await;
        restore_path(old_path);
        let failure = result.expect_err("changed remote must block publication");
        assert_eq!(failure.state, PublicationState::NotPublished);
        assert!(
            failure
                .message
                .contains("configured publishing remote changed")
        );
        assert_eq!(
            git_value(&fixture.bare, &["rev-parse", "refs/heads/rolling"]),
            fixture.base
        );
    }

    #[tokio::test]
    async fn canary_remote_rebinding_blocks_forward_revert_to_old_remote() {
        let fixture = Fixture::new();
        let source = fixture.commit(
            &fixture.base,
            "crates/demo/src/lib.rs",
            "pub fn value() -> &'static str { \"accepted\" }\n",
            "accepted source",
        );
        let other_bare = fixture.root.path().join("other-remote.git");
        git_run(
            fixture.root.path(),
            &["init", "--bare", "-q", other_bare.to_str().unwrap()],
        );
        let first_run = fixture.root.path().join("prepublish-guard-passed");
        fixture.add_fake_cargo(&format!(
            "if [ -e '{}' ]; then /usr/bin/git -C '{}' remote set-url --push origin '{}' || exit 43; exit 42; fi\n: > '{}'",
            first_run.display(),
            fixture.repo.display(),
            other_bare.display(),
            first_run.display()
        ));
        let old_path = use_fake_path(&fixture);
        let result = fixture
            .land(vec![AcceptedPair {
                base: fixture.base.clone(),
                source: source.clone(),
            }])
            .await;
        restore_path(old_path);
        let failure = result.expect_err("remote rebinding must block forward revert");
        assert_eq!(failure.state, PublicationState::Published);
        assert_eq!(
            failure.forward_revert_status,
            Some(PublicationState::NotPublished)
        );
        assert!(
            failure
                .message
                .contains("configured publishing remote changed")
        );
        assert_eq!(
            git_value(&fixture.bare, &["rev-parse", "refs/heads/rolling"]),
            source
        );
        let recovery_path = failure.recovery_path.expect("private recovery clone");
        fs::remove_dir_all(recovery_path).expect("cleanup private test recovery clone");
    }

    #[tokio::test]
    async fn test_filter_is_forwarded_with_exact_cargo_invocation_and_session_target() {
        let fixture = Fixture::new();
        let source = fixture.commit(
            &fixture.base,
            "crates/demo/src/lib.rs",
            "pub fn value() -> &'static str { \"accepted\" }\n",
            "accepted source",
        );
        let args_log = fixture.add_fake_cargo_log();
        let old_path = use_fake_path(&fixture);
        let result = fixture
            .land_with_filters(
                vec![AcceptedPair {
                    base: fixture.base.clone(),
                    source,
                }],
                vec!["demo=accepted::focus".into(), "demo=other::focus".into()],
            )
            .await;
        restore_path(old_path);
        result.expect("focused landing succeeds");
        assert_eq!(
            fs::read_to_string(args_log).expect("cargo args"),
            "test\n-p\ndemo\n--lib\naccepted::focus\n--\n--test-threads=4\ntest\n-p\ndemo\n--lib\nother::focus\n--\n--test-threads=4\ntest\n-p\ndemo\n--lib\naccepted::focus\n--\n--test-threads=4\ntest\n-p\ndemo\n--lib\nother::focus\n--\n--test-threads=4\n"
        );
        let env_log = fs::read_to_string(fixture.root.path().join("cargo-env.log"))
            .expect("cargo environment");
        let mut lines = env_log.lines();
        let configured_target =
            std::env::var("CARGO_TARGET_DIR").expect("session target configured");
        let target = validate_cargo_target_dir().expect("session target directory");
        let workspace_parent = landing_workspace_parent(&target);
        for _ in 0..4 {
            assert_eq!(lines.next(), Some(configured_target.as_str()));
            assert_eq!(lines.next(), Some("4"));
            assert_eq!(lines.next(), Some("line-tables-only"));
            let guard_worktree = lines.next().expect("guard worktree directory");
            let relative = Path::new(guard_worktree)
                .strip_prefix(workspace_parent)
                .expect("guard worktree is inside the selected workspace parent");
            let workspace = relative.components().next().expect("private workspace");
            assert!(
                workspace
                    .as_os_str()
                    .to_string_lossy()
                    .starts_with("rsi-rolling-land-"),
                "guard worktree has no private landing workspace: {guard_worktree}"
            );
        }
        assert_eq!(lines.next(), None);
        assert!(!fixture.repo.join("target").exists());
    }

    #[test]
    fn private_landing_workspace_prefers_sandbox_root_for_recovery() {
        let fixture = Fixture::new();
        let sandbox_target = fixture.repo.join("target");
        fs::create_dir(&sandbox_target).expect("sandbox target");
        assert!(fixture.repo.join(".git").exists());
        assert_eq!(landing_workspace_parent(&sandbox_target), fixture.repo);

        let custom_target = fixture.root.path().join("custom-cargo-target");
        fs::create_dir(&custom_target).expect("custom target");
        assert_eq!(landing_workspace_parent(&custom_target), custom_target);
    }

    #[tokio::test]
    async fn merge_candidate_has_exactly_two_parents_and_is_published() {
        let fixture = Fixture::new();
        let source = fixture.commit(
            &fixture.base,
            "crates/demo/src/lib.rs",
            "pub fn value() -> &'static str { \"source\" }\n",
            "source",
        );
        let target = fixture.commit(&fixture.base, "target.txt", "target\n", "target side");
        git_run(
            &fixture.repo,
            &[
                "push",
                "-q",
                "origin",
                &format!("{target}:refs/heads/rolling"),
            ],
        );
        fixture.add_fake_cargo("");
        let old_path = use_fake_path(&fixture);
        let result = fixture
            .land(vec![AcceptedPair {
                base: fixture.base.clone(),
                source: source.clone(),
            }])
            .await;
        restore_path(old_path);
        let report = result.expect("merge landing succeeds");
        assert_eq!(report.kind, CandidateKind::Merge);
        let parents = git_value(
            &fixture.bare,
            &["rev-list", "--parents", "-n", "1", &report.candidate],
        );
        assert_eq!(parents.split_whitespace().count(), 3);
        assert!(parents.contains(&target));
        assert!(parents.contains(&source));
        assert_eq!(
            git_value(&fixture.bare, &["rev-parse", "refs/heads/rolling"]),
            report.candidate
        );
    }

    #[tokio::test]
    async fn lost_hunk_guard_rejects_without_changing_remote() {
        let fixture = Fixture::new();
        let args_log = fixture.add_fake_cargo_log();
        let old_path = use_fake_path(&fixture);
        let accepted = fixture.commit(
            &fixture.base,
            "crates/demo/src/lib.rs",
            "pub fn value() -> &'static str { \"accepted\" }\n",
            "accepted",
        );
        let reverted = fixture.commit(
            &accepted,
            "crates/demo/src/lib.rs",
            "pub fn value() -> &'static str { \"base\" }\n",
            "revert accepted hunk",
        );
        let followup = fixture.commit(
            &reverted,
            "crates/demo/src/other.rs",
            "pub fn other() {}\n",
            "unrelated affected-crate followup",
        );
        let result = fixture
            .land(vec![
                AcceptedPair {
                    base: fixture.base.clone(),
                    source: accepted.clone(),
                },
                AcceptedPair {
                    base: accepted,
                    source: followup,
                },
            ])
            .await;
        restore_path(old_path);
        let error = result.unwrap_err();
        assert!(error.message.contains("guard rejected"), "{error:?}");
        assert!(
            fs::read_to_string(args_log)
                .expect("affected-crate tests ran despite lost hunk")
                .contains("demo"),
            "lost-hunk diagnostics must not skip the affected-crate test phase"
        );
        assert_eq!(
            git_value(&fixture.bare, &["rev-parse", "refs/heads/rolling"]),
            fixture.base
        );
    }

    #[tokio::test]
    async fn ancestor_remote_advance_during_guard_fails_closed_before_push() {
        let fixture = Fixture::new();
        let intermediate = fixture.commit(
            &fixture.base,
            "crates/demo/src/lib.rs",
            "pub fn value() -> &'static str { \"intermediate\" }\n",
            "intermediate accepted source",
        );
        let candidate = fixture.commit(
            &intermediate,
            "crates/demo/src/extra.rs",
            "pub const EXTRA: bool = true;\n",
            "final accepted source",
        );
        git_run(
            &fixture.repo,
            &[
                "push",
                "-q",
                "origin",
                &format!("{intermediate}:refs/heads/intermediate"),
            ],
        );
        fixture.add_fake_cargo(&format!(
            "git --git-dir='{}' update-ref refs/heads/rolling {intermediate}",
            fixture.bare.display()
        ));
        let old_path = use_fake_path(&fixture);
        let old_git_dir = std::env::var_os("GIT_DIR");
        // The remote read/push subprocesses must ignore this inherited repo override.
        unsafe {
            std::env::set_var("GIT_DIR", fixture.root.path().join("not-a-git-dir"));
        }
        let result = fixture
            .land(vec![
                AcceptedPair {
                    base: fixture.base.clone(),
                    source: intermediate.clone(),
                },
                AcceptedPair {
                    base: intermediate.clone(),
                    source: candidate.clone(),
                },
            ])
            .await;
        restore_path(old_path);
        unsafe {
            if let Some(old_git_dir) = old_git_dir {
                std::env::set_var("GIT_DIR", old_git_dir);
            } else {
                std::env::remove_var("GIT_DIR");
            }
        }
        let error = result.unwrap_err();
        assert!(error.message.contains("stale target"), "{error:?}");
        assert!(
            Command::new("/usr/bin/git")
                .current_dir(&fixture.repo)
                .args(["merge-base", "--is-ancestor", &intermediate, &candidate])
                .status()
                .expect("check candidate ancestry")
                .success()
        );
        assert_eq!(
            git_value(&fixture.bare, &["rev-parse", "refs/heads/rolling"]),
            intermediate
        );
    }

    #[tokio::test]
    async fn remote_lookup_rejects_oversized_output_without_waiting_for_eof() {
        let fixture = Fixture::new();
        fixture.add_fake_git("    yes x\n    exit 0");
        let old_path = use_fake_path(&fixture);
        let result = remote_tip(&fixture.repo, "origin").await;
        restore_path(old_path);
        assert_eq!(
            result.unwrap_err(),
            "remote rolling lookup exceeded the 1024-byte response limit"
        );
    }

    #[tokio::test]
    async fn stalled_remote_lookup_times_out_and_reaps_git_child() {
        let fixture = Fixture::new();
        let pid_file = fixture.root.path().join("stalled-git.pid");
        fixture.add_fake_git(&format!(
            "    printf '%s\\n' \"$$\" > '{}'\n    exec sleep 30",
            pid_file.display()
        ));
        let old_path = use_fake_path(&fixture);
        let started = std::time::Instant::now();
        let result = remote_tip(&fixture.repo, "origin").await;
        restore_path(old_path);
        let elapsed = started.elapsed();
        let error = result.unwrap_err();
        assert!(
            error.contains("remote rolling lookup timed out"),
            "{error:?}"
        );
        assert!(elapsed < Duration::from_secs(4), "lookup took {elapsed:?}");
        let pid = fs::read_to_string(pid_file)
            .expect("fake Git wrote its pid")
            .trim()
            .parse::<i32>()
            .expect("valid pid");
        assert!(matches!(
            nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid), None),
            Err(nix::errno::Errno::ESRCH)
        ));
    }

    #[tokio::test]
    async fn stalled_initial_fetch_is_bounded_and_private_maintenance_is_disabled() {
        let fixture = Fixture::new();
        let pid_file = fixture.root.path().join("stalled-fetch.pid");
        let config_file = fixture.root.path().join("private-config.txt");
        fixture.add_fake_git_behaviors_with_fetch(
            "",
            "",
            &format!(
                "    /usr/bin/git config --local --get maintenance.auto > '{}'\n    /usr/bin/git config --local --get maintenance.autoDetach >> '{}'\n    /usr/bin/git config --local --get gc.autoDetach >> '{}'\n    printf '%s\\n' \"$$\" > '{}'\n    exec sleep 30",
                config_file.display(),
                config_file.display(),
                config_file.display(),
                pid_file.display()
            ),
        );
        let old_path = use_fake_path(&fixture);
        let started = std::time::Instant::now();
        let result = fixture
            .land(vec![AcceptedPair {
                base: fixture.base.clone(),
                source: fixture.base.clone(),
            }])
            .await;
        restore_path(old_path);
        let error = result.unwrap_err();
        assert_eq!(error.state, PublicationState::NotPublished);
        assert!(
            error.message.contains("remote rolling fetch timed out"),
            "{error:?}"
        );
        assert!(started.elapsed() < Duration::from_secs(4));
        assert_eq!(
            fs::read_to_string(config_file).unwrap(),
            "false\nfalse\nfalse\n"
        );
        assert_eq!(
            git_value(&fixture.bare, &["rev-parse", "refs/heads/rolling"]),
            fixture.base
        );
        let pid = fs::read_to_string(pid_file)
            .unwrap()
            .trim()
            .parse::<i32>()
            .unwrap();
        assert!(matches!(
            nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid), None),
            Err(nix::errno::Errno::ESRCH)
        ));
    }

    #[tokio::test]
    async fn already_integrated_source_with_lost_hunk_is_rejected() {
        let fixture = Fixture::new();
        let source = fixture.commit(
            &fixture.base,
            "crates/demo/src/lib.rs",
            "pub fn value() -> &'static str { \"accepted\" }\n",
            "accepted source",
        );
        let reverted = fixture.commit(
            &source,
            "crates/demo/src/lib.rs",
            "pub fn value() -> &'static str { \"base\" }\n",
            "revert accepted hunk",
        );
        git_run(
            &fixture.repo,
            &[
                "push",
                "-q",
                "origin",
                &format!("{reverted}:refs/heads/rolling"),
            ],
        );
        let error = fixture
            .land(vec![AcceptedPair {
                base: fixture.base.clone(),
                source,
            }])
            .await
            .unwrap_err();
        assert_eq!(error.state, PublicationState::NotPublished);
        assert!(error.message.contains("guard rejected"), "{error:?}");
        assert_eq!(
            git_value(&fixture.bare, &["rev-parse", "refs/heads/rolling"]),
            reverted
        );
    }

    #[tokio::test]
    async fn already_integrated_source_requires_historical_base() {
        let fixture = Fixture::new();
        let source = fixture.commit(&fixture.base, "source.txt", "accepted\n", "accepted source");
        git_run(
            &fixture.repo,
            &[
                "push",
                "-q",
                "origin",
                &format!("{source}:refs/heads/rolling"),
            ],
        );
        let error = fixture
            .land(vec![AcceptedPair {
                base: String::new(),
                source: source.clone(),
            }])
            .await
            .expect_err("already-integrated source needs historical base");
        assert!(
            error.message.contains("historical accepted base"),
            "{error:?}"
        );
        assert_eq!(error.state, PublicationState::NotPublished);
        assert_eq!(
            git_value(&fixture.bare, &["rev-parse", "refs/heads/rolling"]),
            source
        );
    }

    #[tokio::test]
    async fn post_push_remote_divergence_reports_unknown_with_observed_tip() {
        let fixture = Fixture::new();
        let source = fixture.commit(
            &fixture.base,
            "crates/demo/src/lib.rs",
            "pub fn value() -> &'static str { \"accepted\" }\n",
            "accepted source",
        );
        let other = fixture.commit(&fixture.base, "other.txt", "other\n", "other source");
        git_run(
            &fixture.repo,
            &["push", "-q", "origin", &format!("{other}:refs/heads/other")],
        );
        fixture.add_fake_cargo("");
        fixture.add_fake_git_behaviors(
            "",
            &format!(
                "      /usr/bin/git --git-dir='{}' update-ref refs/heads/rolling {other}\n      exit 1",
                fixture.bare.display()
            ),
        );
        let old_path = use_fake_path(&fixture);
        let result = fixture
            .land(vec![AcceptedPair {
                base: fixture.base.clone(),
                source,
            }])
            .await;
        restore_path(old_path);
        let error = result.unwrap_err();
        assert_eq!(error.state, PublicationState::Unknown);
        assert_eq!(error.exit_code(), 3);
        let recovery_path = error
            .recovery_path
            .as_ref()
            .expect("private recovery clone");
        assert!(recovery_path.join("repo/.git").exists());
        assert_eq!(
            fs::metadata(recovery_path)
                .expect("recovery mode")
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
        assert!(
            !recovery_path.join("repo/crates/demo/src/lib.rs").exists(),
            "retained clone has no primary checkout"
        );
        assert!(
            error
                .evidence_lines()
                .contains(&format!("observed_target_id={other}"))
        );
        assert_eq!(
            git_value(&fixture.bare, &["rev-parse", "refs/heads/rolling"]),
            other
        );
        fs::remove_dir_all(recovery_path).expect("cleanup private test recovery clone");
    }

    #[test]
    fn published_failure_reports_distinct_exit_and_target_evidence() {
        let error = LandFailure {
            state: PublicationState::Published,
            kind: FailureKind::Cleanup,
            policy_fence: None,
            candidate: Some("candidate".into()),
            fetched_tip: Some("old".into()),
            published_tip: Some("candidate".into()),
            observed_tip: None,
            forward_revert_id: None,
            forward_revert_status: None,
            recovery_path: None,
            message: "cleanup failed".into(),
        };
        assert_eq!(error.exit_code(), 2);
        assert_eq!(
            error.evidence_lines(),
            [
                "publication_status=published",
                "landing_outcome=published_cleanup_failed",
                "exit_code=2",
                "candidate_id=candidate",
                "fetched_target_id=old",
                "published_target_id=candidate"
            ]
        );
    }

    #[test]
    fn canary_red_exit_codes_distinguish_revert_outcomes() {
        for (status, expected_code, expected_outcome) in [
            (PublicationState::Published, 4, "canary_red_reverted"),
            (PublicationState::NotPublished, 5, "canary_red_unreverted"),
            (PublicationState::Unknown, 3, "canary_red_revert_unknown"),
        ] {
            let failure = LandFailure {
                state: PublicationState::Published,
                kind: FailureKind::CanaryRed,
                policy_fence: None,
                candidate: Some("candidate".into()),
                fetched_tip: Some("prior".into()),
                published_tip: Some("candidate".into()),
                observed_tip: None,
                forward_revert_id: Some("revert".into()),
                forward_revert_status: Some(status),
                recovery_path: None,
                message: "published-tip canary failed".into(),
            };
            assert_eq!(failure.exit_code(), expected_code);
            assert!(
                failure
                    .evidence_lines()
                    .contains(&format!("landing_outcome={expected_outcome}"))
            );
        }
    }

    #[tokio::test]
    async fn stalled_push_times_out_and_leaves_remote_tip_unchanged() {
        let fixture = Fixture::new();
        let source = fixture.commit(
            &fixture.base,
            "crates/demo/src/lib.rs",
            "pub fn value() -> &'static str { \"accepted\" }\n",
            "accepted source",
        );
        fixture.add_fake_cargo("");
        let pid_file = fixture.root.path().join("stalled-push.pid");
        fixture.add_fake_git_behaviors(
            "",
            &format!(
                "      printf '%s\\n' \"$$\" > '{}'\n      echo 'fake push started'\n      exec sleep 30",
                pid_file.display()
            ),
        );
        let old_path = use_fake_path(&fixture);
        let result = fixture
            .land(vec![AcceptedPair {
                base: fixture.base.clone(),
                source,
            }])
            .await;
        restore_path(old_path);
        let error = result.unwrap_err();
        assert!(
            error.message.contains("fast-forward-only push timed out"),
            "{error:?}"
        );
        assert_eq!(
            git_value(&fixture.bare, &["rev-parse", "refs/heads/rolling"]),
            fixture.base
        );
        let pid = fs::read_to_string(pid_file)
            .expect("fake Git wrote its pid")
            .trim()
            .parse::<i32>()
            .expect("valid pid");
        assert!(matches!(
            nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid), None),
            Err(nix::errno::Errno::ESRCH)
        ));
    }

    #[test]
    fn requires_an_accepted_pair() {
        assert!(
            parse_args(Vec::<String>::new())
                .unwrap_err()
                .contains("at least one --accepted")
        );
    }

    #[test]
    fn parses_source_only_and_explicit_accepted_pairs() {
        let options = parse_args([
            "--accepted".to_string(),
            "source".to_string(),
            "--accepted".to_string(),
            "base:other".to_string(),
        ])
        .expect("accepted pairs");
        assert_eq!(options.accepted[0].base, "");
        assert_eq!(options.accepted[0].source, "source");
        assert_eq!(options.accepted[1].base, "base");
        assert_eq!(options.accepted[1].source, "other");
        for malformed in ["", ":source", "base:", "a:b:c"] {
            assert!(
                parse_args(["--accepted".to_string(), malformed.to_string()]).is_err(),
                "{malformed:?} must be rejected"
            );
        }
    }
}
