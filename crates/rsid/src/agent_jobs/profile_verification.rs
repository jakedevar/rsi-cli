//! Profile verification evidence is written by the daemon, never by a recipe.
//! Rust projects retain scoped-test's baseline and warning comparison.

use super::project_profile::{self, ProjectCommand, Verification, WarningPolicy};
use super::{AgentJobResultV1, AgentJobV1, JobCommand, JobParams, JobState, MIB};
use crate::error::{DaemonError, Result};
use rsi_common::agent_jobs::{
    ProfileVerificationBindingV1, ProfileVerificationV1, ScopedToolFileV1,
};
use sha2::{Digest, Sha256};
use std::io::Read;
use std::path::{Path, PathBuf};

fn refused(code: &str) -> DaemonError {
    DaemonError::InvalidParam(code.into())
}

/// Rust workspaces cannot opt out of the existing warning/baseline gate by
/// declaring generic warnings or a recipe. A missing profile is resolved too.
fn selected(params: &JobParams, cwd: &Path) -> Result<Option<project_profile::ResolvedProfile>> {
    let JobParams::Test(test) = params else {
        return Ok(None);
    };
    if !test.is_scoped_test() && test.recipe.is_none() {
        return Ok(None);
    }
    // Existing non-repository recipe jobs remain diagnostic jobs. They have
    // no commit to bind and cannot become candidate verification evidence.
    if test.recipe.is_some() && !cwd.join(".git").exists() {
        return Ok(None);
    }
    if cwd.join("Cargo.toml").exists() || cwd.join("scripts/scoped-test").is_file() {
        return Ok(None);
    }
    let profile = project_profile::resolve(cwd).map_err(|_| {
        refused(if test.recipe.is_some() {
            rsi_common::agent_jobs::JOB_RECIPE_INVALID
        } else {
            "project_verification_profile_invalid"
        })
    })?;
    if let Some(name) = &test.recipe {
        let recipe = profile.recipes.get(name).or_else(|| {
            let Verification::Available(commands) = &profile.verification else {
                return None;
            };
            commands.iter().find_map(|command| match command {
                ProjectCommand::Recipe(recipe) if recipe.target == *name => Some(recipe),
                _ => None,
            })
        });
        let Some(recipe) = recipe else {
            return Ok(None);
        };
        if profile.verification
            != Verification::Available(vec![ProjectCommand::Recipe(recipe.clone())])
        {
            return Ok(None);
        }
        if !test.filters.is_empty() {
            return Ok(None);
        }
    }
    if profile.warnings != WarningPolicy::Generic {
        return Err(refused("project_verification_warning_policy_unsupported"));
    }
    match &profile.verification {
        Verification::NoTests => Err(refused("project_verification_no_tests")),
        Verification::Unsupported => Err(refused("project_verification_unsupported")),
        Verification::Available(commands) if commands.is_empty() => {
            Err(refused("project_verification_no_tests"))
        }
        Verification::Available(_) => Ok(Some(profile)),
    }
}

fn digest_file(path: &Path) -> Result<String> {
    let mut file =
        std::fs::File::open(path).map_err(|_| refused("project_verification_tool_unavailable"))?;
    let mut digest = Sha256::new();
    let mut buffer = [0u8; 65536];
    loop {
        let count = file
            .read(&mut buffer)
            .map_err(|_| refused("project_verification_tool_unavailable"))?;
        if count == 0 {
            break;
        }
        digest.update(&buffer[..count]);
    }
    Ok(format!("{:x}", digest.finalize()))
}

fn tool(path: PathBuf, root: &Path) -> Result<ScopedToolFileV1> {
    let path = path
        .canonicalize()
        .map_err(|_| refused("project_verification_tool_unavailable"))?;
    if path.starts_with(root) {
        return Err(refused("project_verification_tool_untrusted"));
    }
    Ok(ScopedToolFileV1 {
        path: path.to_string_lossy().into_owned(),
        blob: Some(digest_file(&path)?),
    })
}

fn quote(text: &str) -> String {
    format!("'{}'", text.replace('\'', "'\\''"))
}

/// Binding includes raw declaration/recipe bytes as well as the resolved view:
/// even a semantically equivalent profile edit invalidates prior evidence.
pub(crate) fn resolve_binding(
    params: &JobParams,
    cwd: &Path,
) -> Result<Option<ProfileVerificationBindingV1>> {
    let Some(profile) = selected(params, cwd)? else {
        return Ok(None);
    };
    if !cfg!(unix) {
        return Err(refused("project_verification_platform_unsupported"));
    }
    let state = crate::sandbox::git_worktree::with_repository_mutation(&profile.root, || {
        crate::sandbox::git_worktree::observe_worktree_ignoring_ignored_locked(
            &profile.root,
            &profile.root,
        )
    })
    .map_err(|_| refused("project_verification_source_unreadable"))?;
    let head = state
        .head_oid
        .ok_or_else(|| refused("project_verification_source_unreadable"))?;
    if !state.clean {
        return Err(refused("project_verification_source_dirty"));
    }
    if let JobParams::Test(test) = params
        && let Some(scoped) = &test.scoped_test
        && scoped
            .head
            .as_deref()
            .is_some_and(|pin| pin != "HEAD" && pin != head)
    {
        return Err(refused("project_verification_head_mismatch"));
    }
    let mut digest = Sha256::new();
    digest.update(
        serde_json::to_vec(&profile)
            .map_err(|_| refused("project_verification_profile_invalid"))?,
    );
    for file in [
        project_profile::PROFILE_PATH,
        ".rsi/jobs.toml",
        "Makefile",
        "justfile",
        "Cargo.toml",
    ] {
        let text = project_profile::local_text(&profile.root, file)
            .map_err(|_| refused("project_verification_profile_invalid"))?;
        digest.update(
            serde_json::to_vec(&(file, text))
                .map_err(|_| refused("project_verification_profile_invalid"))?,
        );
    }
    let shell = tool(PathBuf::from("/bin/sh"), &profile.root)?;
    // Adapter version is bound independently of project code. The daemon
    // binary is immutable during a run; a restart with another adapter must
    // still understand this version before accepting it.
    static DAEMON_TOOL: std::sync::OnceLock<std::result::Result<ScopedToolFileV1, String>> =
        std::sync::OnceLock::new();
    let daemon = DAEMON_TOOL
        .get_or_init(|| {
            let path = std::env::current_exe().map_err(|error| error.to_string())?;
            tool(path, Path::new("/nonexistent-profile-root")).map_err(|error| error.to_string())
        })
        .as_ref()
        .map_err(|_| refused("project_verification_tool_unavailable"))?;
    let mut tools = vec![shell.clone(), daemon.clone()];
    let Verification::Available(commands) = &profile.verification else {
        unreachable!()
    };
    let mut script = String::from("set -e\n");
    for command in commands {
        let ProjectCommand::Recipe(recipe) = command else {
            return Err(refused("project_verification_unsupported"));
        };
        let (runner, flag, file) = match recipe.runner {
            project_profile::Runner::Make => ("make", "-f", "Makefile"),
            project_profile::Runner::Just => ("just", "--justfile", "justfile"),
        };
        let runner = tool(
            which::which(runner).map_err(|_| refused("project_verification_tool_unavailable"))?,
            &profile.root,
        )?;
        script.push_str(&format!(
            "{} {} {} -- {}\n",
            quote(&runner.path),
            flag,
            quote(&profile.root.join(file).to_string_lossy()),
            quote(&recipe.target)
        ));
        if !tools.contains(&runner) {
            tools.push(runner);
        }
    }
    Ok(Some(ProfileVerificationBindingV1 {
        version: 1,
        head,
        cwd: profile.root.to_string_lossy().into_owned(),
        clean: true,
        profile_digest: format!("{:x}", digest.finalize()),
        tools,
        argv: vec![shell.path, "-c".into(), script],
        commands: commands.len() as u32,
        warning_policy: "generic".into(),
    }))
}

pub(super) fn profile_command(params: &JobParams, cwd: &Path) -> Result<Option<JobCommand>> {
    let Some(binding) = resolve_binding(params, cwd)? else {
        return Ok(None);
    };
    let profile = project_profile::resolve(cwd)
        .map_err(|_| refused("project_verification_profile_invalid"))?;
    let Verification::Available(commands) = profile.verification else {
        unreachable!()
    };
    let cpu_quota_percent = commands
        .iter()
        .filter_map(|command| match command {
            ProjectCommand::Recipe(recipe) => Some(recipe.cpu_quota_percent),
            _ => None,
        })
        .min()
        .unwrap_or(100);
    let declaration_minutes: u64 = commands
        .iter()
        .filter_map(|command| match command {
            ProjectCommand::Recipe(recipe) => Some(u64::from(recipe.timeout_minutes)),
            _ => None,
        })
        .sum();
    let requested = super::job_timeout_secs(params).unwrap_or(20 * 60);
    Ok(Some(JobCommand {
        argv: binding.argv,
        scoped_tool: None,
        runtime_max_secs: requested.min(declaration_minutes * 60)
            + super::JOB_TIMEOUT_UNIT_GRACE_SECS,
        stop_timeout_secs: 60,
        log_max_bytes: 64 * MIB,
        memory_max_gib: 24,
        cpu_quota_percent,
        background: false,
        admission_max_secs: None,
    }))
}

/// All verification commands share the most restrictive declared timeout.
/// This conservative aggregate cap never lets one recipe borrow another's
/// larger allowance. The admitted operator timeout can only lower it.
pub(super) fn fit_profile_timeout(params: &mut JobParams, cwd: &Path) -> Result<()> {
    let Some(profile) = selected(params, cwd)? else {
        return Ok(());
    };
    let Verification::Available(commands) = profile.verification else {
        return Ok(());
    };
    let cap = commands
        .iter()
        .filter_map(|command| match command {
            ProjectCommand::Recipe(recipe) => Some(recipe.timeout_minutes),
            _ => None,
        })
        .min()
        .ok_or_else(|| refused("project_verification_no_tests"))?;
    if let JobParams::Test(test) = params {
        test.timeout_minutes = Some(test.timeout_minutes.unwrap_or(cap).min(cap));
    }
    Ok(())
}

pub(crate) fn recorded_profile_binding(job: &AgentJobV1) -> Option<&ProfileVerificationBindingV1> {
    Some(&job.result.as_ref()?.profile_verification.as_ref()?.binding)
}

/// Called at launch, during every running poll, at settlement and candidate
/// read. Unreadable source/profile/tools fail closed, including after restart.
pub(crate) fn check_profile_binding(job: &AgentJobV1) -> std::result::Result<(), AgentJobResultV1> {
    let Some(before) = recorded_profile_binding(job) else {
        return Ok(());
    };
    let now = resolve_binding(&job.params, Path::new(&job.cwd));
    if matches!(&now, Ok(Some(now)) if now == before) {
        return Ok(());
    }
    Err(AgentJobResultV1 {
        refusal: Some("project_verification_source_changed".into()),
        detail: Some("source HEAD, cleanliness, profile bytes or daemon-selected runner changed or became unreadable; rerun verification at the committed source".into()),
        ..AgentJobResultV1::default()
    })
}

pub(crate) fn finish_profile_verification(
    job: &AgentJobV1,
    state: &mut JobState,
    result: &mut AgentJobResultV1,
) {
    let Some(binding) = recorded_profile_binding(job) else {
        return;
    };
    if let Err(changed) = check_profile_binding(job) {
        *state = JobState::Failed;
        result.refusal = changed.refusal;
        result.detail = changed.detail;
    }
    let completed =
        *state == JobState::Succeeded && result.exit_code == Some(0) && result.refusal.is_none();
    result.receipt = None; // Ignore even a forged SCOPED_TEST_RECEIPT log line.
    result.source_head = Some(binding.head.clone());
    result.source_dirty = Some(!binding.clean);
    result.profile_verification = Some(ProfileVerificationV1 {
        binding: binding.clone(),
        completed_commands: if completed { binding.commands } else { 0 },
        warning_verdict: if completed {
            "generic_command_exit"
        } else {
            "not_verified"
        }
        .into(),
    });
}

#[cfg(all(
    test,
    unix,
    any(not(feature = "test-shard-mode"), feature = "test-shard-other-01")
))]
pub(super) mod tests {
    use super::*;
    use rsi_repo_guards::scratch_git::Policy;

    pub(in crate::agent_jobs) fn repo() -> tempfile::TempDir {
        let policy = Policy::from_process(Path::new(rsi_common::manifest_dir!())).unwrap();
        let dir = policy.tempdir("profile-verification-").unwrap();
        std::fs::write(
            dir.path().join("Makefile"),
            ".PHONY: test\ntest:\n\t@echo verified\n",
        )
        .unwrap();
        commit(dir.path());
        dir
    }

    pub(in crate::agent_jobs) fn commit(dir: &Path) {
        let policy = Policy::from_process(Path::new(rsi_common::manifest_dir!())).unwrap();
        let mut paths = vec!["add", "Makefile"];
        if dir.join(".rsi/project.toml").is_file() {
            paths.push(".rsi/project.toml");
        }
        for args in [
            vec!["init", "-q"],
            paths,
            vec![
                "-c",
                "user.name=test",
                "-c",
                "user.email=test@example.invalid",
                "-c",
                "commit.gpgsign=false",
                "commit",
                "-qm",
                "fixture",
            ],
        ] {
            let output = policy.command(dir, &args).unwrap().output().unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
    }

    fn params() -> JobParams {
        serde_json::from_value(serde_json::json!({"kind":"test", "scoped_test":{"base":"HEAD"}}))
            .unwrap()
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn profile_binding_tracks_exact_bytes_source_and_daemon_selected_tools() {
        let dir = repo();
        let original = resolve_binding(&params(), dir.path()).unwrap().unwrap();
        assert_eq!(original.commands, 1);
        assert_eq!(original.warning_policy, "generic");
        assert!(original.tools.len() >= 3);
        assert!(
            original
                .tools
                .iter()
                .all(|tool| tool.blob.as_ref().unwrap().len() == 64)
        );
        assert_eq!(
            resolve_binding(&params(), dir.path()).unwrap().unwrap(),
            original
        );
        std::fs::write(
            dir.path().join("Makefile"),
            ".PHONY: test\ntest:\n\t@echo changed\n",
        )
        .unwrap();
        assert!(
            resolve_binding(&params(), dir.path())
                .unwrap_err()
                .to_string()
                .contains("source_dirty")
        );
        commit(dir.path());
        let changed = resolve_binding(&params(), dir.path()).unwrap().unwrap();
        assert_ne!(original.head, changed.head);
        assert_ne!(original.profile_digest, changed.profile_digest);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn absent_verification_and_unsupported_warning_policies_never_pass() {
        let dir = repo();
        std::fs::create_dir(dir.path().join(".rsi")).unwrap();
        std::fs::write(
            dir.path().join(".rsi/project.toml"),
            "version=1\nverification=[]\n",
        )
        .unwrap();
        assert!(
            resolve_binding(&params(), dir.path())
                .unwrap_err()
                .to_string()
                .contains("no_tests")
        );
        std::fs::write(
            dir.path().join(".rsi/project.toml"),
            "version=1\nwarnings='no_new_rust_warnings'\n",
        )
        .unwrap();
        assert!(
            resolve_binding(&params(), dir.path())
                .unwrap_err()
                .to_string()
                .contains("warning_policy_unsupported")
        );
        std::fs::remove_file(dir.path().join(".rsi/project.toml")).unwrap();
        std::fs::write(dir.path().join("Makefile"), "build:\n\t@echo build\n").unwrap();
        assert!(
            resolve_binding(&params(), dir.path())
                .unwrap_err()
                .to_string()
                .contains("unsupported")
        );
        std::fs::write(dir.path().join("Cargo.toml"), "[workspace]\n").unwrap();
        assert!(
            resolve_binding(&params(), dir.path()).unwrap().is_none(),
            "Rust retains scoped-test"
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn ordered_profile_commands_use_one_conservative_budget_and_bind_raw_profile_bytes() {
        let dir = repo();
        std::fs::create_dir(dir.path().join(".rsi")).unwrap();
        let text = "version=1\nverification=['first','second']\n[recipes.first]\nrunner='make'\ntarget='test'\ntimeout_minutes=5\ncpu_quota_percent=100\n[recipes.second]\nrunner='make'\ntarget='test'\ntimeout_minutes=10\ncpu_quota_percent=200\n";
        std::fs::write(dir.path().join(".rsi/project.toml"), text).unwrap();
        commit(dir.path());
        let mut params = params();
        fit_profile_timeout(&mut params, dir.path()).unwrap();
        assert_eq!(super::super::job_timeout_secs(&params), Some(300));
        let binding = resolve_binding(&params, dir.path()).unwrap().unwrap();
        assert_eq!(binding.commands, 2);
        let command = profile_command(&params, dir.path()).unwrap().unwrap();
        assert_eq!(command.cpu_quota_percent, 100);
        let output = std::process::Command::new(&command.argv[0])
            .args(&command.argv[1..])
            .current_dir(dir.path())
            .env_remove("RSI_SESSION_TOKEN")
            .output()
            .unwrap();
        assert!(output.status.success());
        assert_eq!(
            String::from_utf8_lossy(&output.stdout)
                .matches("verified")
                .count(),
            2
        );
        std::fs::write(
            dir.path().join(".rsi/project.toml"),
            format!("{text}\n# changed bytes\n"),
        )
        .unwrap();
        commit(dir.path());
        assert_ne!(
            binding.profile_digest,
            resolve_binding(&params, dir.path())
                .unwrap()
                .unwrap()
                .profile_digest
        );
    }
}
