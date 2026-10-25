//! Project-declared gates share the durable test-job lifecycle (#1477).
//! This allowlist selects repository code, just like a Cargo test does; it
//! grants no publishing, cloud or credential authority.

use super::{JOB_TIMEOUT_UNIT_GRACE_SECS, JobCommand, JobTools, MIB, s};
use crate::error::{DaemonError, Result};
use rsi_common::agent_jobs::{
    JOB_MAX_TOKEN_BYTES, JOB_RECIPE_INVALID, JOB_RECIPE_NOT_ALLOWED, JOB_RECIPE_USE_SCOPED_TEST,
    JOB_TEST_TIMEOUT_MAX_MINS, JOB_TEST_TIMEOUT_MIN_MINS, JobRefusal, TestJobParams,
};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
#[cfg(test)]
use std::io::Read;
use std::path::Path;

pub(super) const MAX_MANIFEST_BYTES: u64 = 64 * 1024;
const MAX_RECIPES: usize = 64;
const MAX_CPU_QUOTA_PERCENT: u64 = 1600;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Manifest {
    version: u32,
    pub(super) recipes: BTreeMap<String, Recipe>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Recipe {
    pub runner: Runner,
    pub target: String,
    pub timeout_minutes: u32,
    pub cpu_quota_percent: u64,
}

/// The environment variable `scripts/scoped-test` reads its focused filters from.
/// The manifest declares nothing for it (an older daemon refuses unknown manifest
/// fields, so a new field would break every recipe until redeploy); a target that
/// takes no filters refuses them itself (`make check-touched-shards`).
pub(super) const RECIPE_FILTERS_ENV: &str = "RSI_SCOPED_TEST_FILTERS";

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Runner {
    Just,
    Make,
}

fn invalid() -> DaemonError {
    DaemonError::InvalidParam(JOB_RECIPE_INVALID.into())
}

pub(super) fn target_token(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= JOB_MAX_TOKEN_BYTES
        && !value.starts_with('-')
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.'))
}

#[cfg(test)]
pub(super) fn load(cwd: &Path) -> Result<Manifest> {
    let root = cwd.canonicalize().map_err(|_| invalid())?;
    let path = root.join(".rsi/jobs.toml");
    let path = path.canonicalize().map_err(|error| {
        if error.kind() == std::io::ErrorKind::NotFound {
            DaemonError::InvalidParam(JOB_RECIPE_NOT_ALLOWED.into())
        } else {
            invalid()
        }
    })?;
    if !path.starts_with(&root) || !path.is_file() {
        return Err(invalid());
    }
    let mut text = String::new();
    std::fs::File::open(path)
        .and_then(|file| file.take(MAX_MANIFEST_BYTES + 1).read_to_string(&mut text))
        .map_err(|_| invalid())?;
    if text.len() as u64 > MAX_MANIFEST_BYTES {
        return Err(invalid());
    }
    parse_manifest(&text)
}

pub(super) fn validate_recipes(recipes: &BTreeMap<String, Recipe>) -> Result<()> {
    if recipes.len() > MAX_RECIPES
        || recipes.iter().any(|(name, recipe)| {
            !target_token(name)
                || !target_token(&recipe.target)
                || !(JOB_TEST_TIMEOUT_MIN_MINS..=JOB_TEST_TIMEOUT_MAX_MINS)
                    .contains(&recipe.timeout_minutes)
                || !(1..=MAX_CPU_QUOTA_PERCENT).contains(&recipe.cpu_quota_percent)
        })
    {
        return Err(invalid());
    }
    Ok(())
}

pub(super) fn parse_manifest(text: &str) -> Result<Manifest> {
    if text.len() as u64 > MAX_MANIFEST_BYTES {
        return Err(invalid());
    }
    let manifest: Manifest = toml::from_str(text).map_err(|_| invalid())?;
    if manifest.version != 1 {
        return Err(invalid());
    }
    validate_recipes(&manifest.recipes)?;
    Ok(manifest)
}

fn declared_recipe(cwd: &Path, name: &str) -> Result<Recipe> {
    let retired = |value: &str| matches!(value, "scoped-test" | "scoped-test-tmpfs");
    // Refuse even in a stale sandbox with the old declarations, or no manifest.
    // Aliases targeting the same gate must also use the rolling-tool job path.
    if retired(name) {
        return Err(DaemonError::InvalidParam(JOB_RECIPE_USE_SCOPED_TEST.into()));
    }
    let profile = super::project_profile::resolve(cwd).map_err(|_| invalid())?;
    let recipe = profile
        .recipes
        .get(name)
        .cloned()
        .or_else(|| {
            let super::project_profile::Verification::Available(commands) = &profile.verification
            else {
                return None;
            };
            commands.iter().find_map(|command| match command {
                super::project_profile::ProjectCommand::Recipe(recipe) if recipe.target == name => {
                    Some(recipe.clone())
                }
                _ => None,
            })
        })
        .ok_or_else(|| DaemonError::InvalidParam(JOB_RECIPE_NOT_ALLOWED.into()))?;
    if retired(&recipe.target) {
        return Err(DaemonError::InvalidParam(JOB_RECIPE_USE_SCOPED_TEST.into()));
    }
    Ok(recipe)
}

/// The operator's default timeout is stamped before the manifest is read, so a
/// recipe that declares less than that default would be refused for a request
/// that named no timeout. Clamp the stored timeout to the declaration (never
/// above the operator policy already applied); `command` still refuses a
/// timeout that exceeds it.
pub(super) fn fit_timeout(
    params: &mut rsi_common::agent_jobs::JobParams,
    cwd: &Path,
) -> Result<()> {
    let rsi_common::agent_jobs::JobParams::Test(test) = params else {
        return Ok(());
    };
    let Some(name) = test.recipe.as_ref() else {
        return Ok(());
    };
    let recipe = declared_recipe(cwd, name)?;
    if let Some(minutes) = test.timeout_minutes {
        test.timeout_minutes = Some(minutes.min(recipe.timeout_minutes));
    }
    Ok(())
}

pub(super) fn command(tools: &JobTools, test: &TestJobParams, cwd: &Path) -> Result<JobCommand> {
    let name = test
        .recipe
        .as_deref()
        .ok_or_else(|| DaemonError::InvalidParam(JOB_RECIPE_NOT_ALLOWED.into()))?;
    let recipe = declared_recipe(cwd, name)?;
    let timeout_minutes = test.timeout_minutes.unwrap_or(recipe.timeout_minutes);
    if timeout_minutes > recipe.timeout_minutes {
        return Err(super::job_refusal(JobRefusal::timeout_over_recipe(
            timeout_minutes,
            recipe.timeout_minutes,
        )));
    }
    // Explicit local recipe files prevent just/make from discovering a file
    // in an ancestor directory. Neither the request nor the manifest supplies
    // executable paths, arguments, environment variables or shell text.
    let mut argv = vec![
        tools.cargo_slot.display().to_string(),
        s("env"),
        s("-u"),
        s(super::STRIP_NAMESPACE_ENV),
    ];
    if !test.filters.is_empty() {
        // One argv element holding validated, control-free filters: no shell
        // text, flag or path is supplied by the request.
        argv.push(format!("{RECIPE_FILTERS_ENV}={}", test.filters.join("\n")));
    }
    match recipe.runner {
        Runner::Just => argv.extend([
            s("just"),
            s("--justfile"),
            cwd.join("justfile").display().to_string(),
        ]),
        Runner::Make => argv.extend([
            s("make"),
            s("-f"),
            cwd.join("Makefile").display().to_string(),
        ]),
    }
    argv.extend([s("--"), recipe.target.clone()]);
    Ok(JobCommand {
        argv,
        runtime_max_secs: u64::from(timeout_minutes) * 60
            + JOB_TIMEOUT_UNIT_GRACE_SECS
            + super::JOB_SLOT_WAIT_ALLOWANCE_SECS,
        stop_timeout_secs: 60,
        log_max_bytes: 64 * MIB,
        memory_max_gib: 24,
        cpu_quota_percent: recipe.cpu_quota_percent,
        background: false,
        admission_max_secs: None,
        scoped_tool: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use rsi_common::agent_jobs::{AgentSubmitJobRequestV1, JobParams};

    fn refusal(error: DaemonError) -> String {
        let DaemonError::InvalidParam(code) = error else {
            panic!("expected a typed refusal: {error}");
        };
        code
    }

    fn test_params() -> TestJobParams {
        let request: AgentSubmitJobRequestV1 = serde_json::from_value(serde_json::json!({
            "kind":"test", "params":{"recipe":"check-cpu","timeout_minutes":10}
        }))
        .unwrap();
        let JobParams::Test(test) = request.typed_params().unwrap() else {
            panic!("test");
        };
        test
    }

    fn write_manifest(dir: &Path, text: &str) {
        std::fs::create_dir_all(dir.join(".rsi")).unwrap();
        std::fs::write(dir.join(".rsi/jobs.toml"), text).unwrap();
    }

    fn manifest(runner: &str) -> String {
        format!(
            "version = 1\n[recipes.check-cpu]\nrunner = '{runner}'\ntarget = 'check-cpu'\ntimeout_minutes = 20\ncpu_quota_percent = 200\n"
        )
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn declared_just_and_make_targets_use_literal_argv_and_resource_caps() {
        let dir = crate::test_support::disk_backed_tempdir("recipe-command");
        let tools = JobTools {
            cargo_slot: "slot".into(),
            lander: "lander".into(),
        };
        for (runner, flag, file) in [
            ("just", "--justfile", "justfile"),
            ("make", "-f", "Makefile"),
        ] {
            write_manifest(dir.path(), &manifest(runner));
            let result = command(&tools, &test_params(), dir.path()).unwrap();
            assert_eq!(
                result.argv,
                vec![
                    s("slot"),
                    s("env"),
                    s("-u"),
                    s(super::super::STRIP_NAMESPACE_ENV),
                    s(runner),
                    s(flag),
                    dir.path().join(file).display().to_string(),
                    s("--"),
                    s("check-cpu")
                ]
            );
            assert_eq!(
                result.runtime_max_secs,
                600 + JOB_TIMEOUT_UNIT_GRACE_SECS + super::super::JOB_SLOT_WAIT_ALLOWANCE_SECS
            );
            assert_eq!(result.cpu_quota_percent, 200);
            assert_eq!(result.memory_max_gib, 24);
            let mut too_long = test_params();
            too_long.timeout_minutes = Some(21);
            let text = refusal(command(&tools, &too_long, dir.path()).unwrap_err());
            assert!(
                text.starts_with("job_invalid_params: params.timeout_minutes: 21 exceeds"),
                "{text}"
            );
        }
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn recipes_require_a_valid_bounded_local_allowlist() {
        let dir = crate::test_support::disk_backed_tempdir("recipe-allowlist");
        assert_eq!(
            refusal(load(dir.path()).err().unwrap()),
            JOB_RECIPE_NOT_ALLOWED
        );
        let valid = manifest("make");
        for bad in [
            "invalid TOML".into(),
            valid.replace("version = 1", "version = 2"),
            valid.replace("'make'", "'sh'"),
            valid.replace("target = 'check-cpu'", "target = 'X=Y'"),
            valid.replace("target = 'check-cpu'", "target = '--eval'"),
            valid.replace("timeout_minutes = 20", "timeout_minutes = 181"),
            valid.replace("timeout_minutes = 20", "timeout_minutes = 0"),
            valid.replace("cpu_quota_percent = 200", "cpu_quota_percent = 0"),
            valid.replace("cpu_quota_percent = 200", "cpu_quota_percent = 1601"),
            format!("{valid}argv = ['sh']\n"),
            "x".repeat(MAX_MANIFEST_BYTES as usize + 1),
            format!("version = 1\n{}", (0..=MAX_RECIPES).map(|n| {
                format!("[recipes.r{n}]\nrunner = 'make'\ntarget = 'gate'\ntimeout_minutes = 20\ncpu_quota_percent = 200\n")
            }).collect::<String>()),
        ] {
            write_manifest(dir.path(), &bad);
            assert_eq!(refusal(load(dir.path()).err().unwrap()), JOB_RECIPE_INVALID);
        }
        write_manifest(dir.path(), &valid);
        let mut missing = test_params();
        missing.recipe = Some("not-declared".into());
        let tools = JobTools {
            cargo_slot: "slot".into(),
            lander: "lander".into(),
        };
        assert_eq!(
            refusal(command(&tools, &missing, dir.path()).unwrap_err()),
            JOB_RECIPE_NOT_ALLOWED
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn scoped_test_recipes_and_target_aliases_direct_callers_to_scoped_test() {
        let dir = crate::test_support::disk_backed_tempdir("recipe-retired-scoped-test");
        let tools = JobTools {
            cargo_slot: "slot".into(),
            lander: "lander".into(),
        };
        for target in ["scoped-test", "scoped-test-tmpfs"] {
            let mut test = test_params();
            test.recipe = Some(target.into());
            // No manifest still gives the migration hint, rather than a hint
            // to declare a recipe that would run a stale verification tool.
            assert_eq!(
                refusal(command(&tools, &test, dir.path()).unwrap_err()),
                JOB_RECIPE_USE_SCOPED_TEST
            );
            for runner in ["make", "just"] {
                let stale = manifest(runner).replace("check-cpu", target);
                write_manifest(dir.path(), &stale);
                for name in [target, "verify"] {
                    write_manifest(
                        dir.path(),
                        &stale
                            .replace(&format!("[recipes.{target}]"), &format!("[recipes.{name}]")),
                    );
                    test.recipe = Some(name.into());
                    assert_eq!(
                        refusal(command(&tools, &test, dir.path()).unwrap_err()),
                        JOB_RECIPE_USE_SCOPED_TEST
                    );
                    let mut params = JobParams::Test(test.clone());
                    assert_eq!(
                        refusal(fit_timeout(&mut params, dir.path()).unwrap_err()),
                        JOB_RECIPE_USE_SCOPED_TEST
                    );
                }
            }
            std::fs::remove_file(dir.path().join(".rsi/jobs.toml")).unwrap();
        }
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn focused_filters_reach_the_target_as_one_environment_entry() {
        let dir = crate::test_support::disk_backed_tempdir("recipe-filters");
        let tools = JobTools {
            cargo_slot: "slot".into(),
            lander: "lander".into(),
        };
        write_manifest(dir.path(), &manifest("make"));
        let mut focused = test_params();
        focused.filters = vec![
            "rsid-store=shard:store-01:test(store_open_)".into(),
            "rsid=agent_jobs".into(),
        ];
        let result = command(&tools, &focused, dir.path()).unwrap();
        assert_eq!(
            result.argv[..6].to_vec(),
            vec![
                s("slot"),
                s("env"),
                s("-u"),
                s(super::super::STRIP_NAMESPACE_ENV),
                format!(
                    "{RECIPE_FILTERS_ENV}=rsid-store=shard:store-01:test(store_open_)\nrsid=agent_jobs"
                ),
                s("make"),
            ]
        );
        // No filters: the argv is unchanged and the recipe derives its own.
        assert!(
            !command(&tools, &test_params(), dir.path())
                .unwrap()
                .argv
                .iter()
                .any(|a| a.starts_with(RECIPE_FILTERS_ENV))
        );
    }

    /// #3239, #3453: the repository's own manifest declares `watchdog-pressure`,
    /// which runs its make target with the declared budget, and only that fixed
    /// target: a request cannot name another path, argument or environment.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn the_repository_declares_the_watchdog_pressure_recipe() {
        let root = Path::new(rsi_common::manifest_dir!()).join("../..");
        let tools = JobTools {
            cargo_slot: "slot".into(),
            lander: "lander".into(),
        };
        let mut test = test_params();
        test.recipe = Some("watchdog-pressure".into());
        test.timeout_minutes = None;
        let result = command(&tools, &test, &root).unwrap();
        assert_eq!(
            result.argv.last().map(String::as_str),
            Some("watchdog-pressure")
        );
        assert_eq!(result.argv[result.argv.len() - 2], "--");
        assert!(result.cpu_quota_percent >= 200);
        assert!(result.runtime_max_secs >= 20 * 60);
        let makefile = std::fs::read_to_string(root.join("Makefile")).unwrap();
        assert!(
            makefile.contains("watchdog-pressure:\n\tpython3 scripts/watchdog-compile-pressure.py")
        );
        assert!(root.join("scripts/watchdog-compile-pressure.py").is_file());
        // An unlisted name, such as another target of the same Makefile, is
        // still refused with the typed code.
        test.recipe = Some("watchdog-pressure-arbitrary".into());
        assert_eq!(
            refusal(command(&tools, &test, &root).unwrap_err()),
            JOB_RECIPE_NOT_ALLOWED
        );
    }

    #[cfg(unix)]
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn a_recipe_manifest_cannot_resolve_outside_the_worktree() {
        let dir = crate::test_support::disk_backed_tempdir("recipe-local");
        let other = crate::test_support::disk_backed_tempdir("recipe-external");
        write_manifest(other.path(), &manifest("make"));
        std::os::unix::fs::symlink(other.path().join(".rsi"), dir.path().join(".rsi")).unwrap();
        assert_eq!(refusal(load(dir.path()).err().unwrap()), JOB_RECIPE_INVALID);
    }
}
