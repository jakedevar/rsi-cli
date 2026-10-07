//! Project-declared gates share the durable test-job lifecycle (#1477).
//! This allowlist selects repository code, just like a Cargo test does; it
//! grants no publishing, cloud or credential authority.

use super::{JOB_TIMEOUT_UNIT_GRACE_SECS, JobCommand, JobTools, MIB, s};
use crate::error::{DaemonError, Result};
use rsi_common::agent_jobs::{
    JOB_INVALID_PARAMS, JOB_MAX_TOKEN_BYTES, JOB_RECIPE_INVALID, JOB_RECIPE_NOT_ALLOWED,
    JOB_TEST_TIMEOUT_MAX_MINS, JOB_TEST_TIMEOUT_MIN_MINS, TestJobParams,
};
use serde::Deserialize;
use std::collections::BTreeMap;
use std::io::Read;
use std::path::Path;

const MAX_MANIFEST_BYTES: u64 = 64 * 1024;
const MAX_RECIPES: usize = 64;
const MAX_CPU_QUOTA_PERCENT: u64 = 1600;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Manifest {
    version: u32,
    recipes: BTreeMap<String, Recipe>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Recipe {
    runner: Runner,
    target: String,
    timeout_minutes: u32,
    cpu_quota_percent: u64,
}

/// The environment variable `scripts/scoped-test` reads its focused filters from.
/// The manifest declares nothing for it (an older daemon refuses unknown manifest
/// fields, so a new field would break every recipe until redeploy); a target that
/// takes no filters refuses them itself (`make check-touched-shards`).
pub(super) const RECIPE_FILTERS_ENV: &str = "RSI_SCOPED_TEST_FILTERS";

#[derive(Clone, Copy, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Runner {
    Just,
    Make,
}

fn invalid() -> DaemonError {
    DaemonError::InvalidParam(JOB_RECIPE_INVALID.into())
}

fn target_token(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= JOB_MAX_TOKEN_BYTES
        && !value.starts_with('-')
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.'))
}

fn load(cwd: &Path) -> Result<Manifest> {
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
    let manifest: Manifest = toml::from_str(&text).map_err(|_| invalid())?;
    if manifest.version != 1
        || manifest.recipes.len() > MAX_RECIPES
        || manifest.recipes.iter().any(|(name, recipe)| {
            !target_token(name)
                || !target_token(&recipe.target)
                || !(JOB_TEST_TIMEOUT_MIN_MINS..=JOB_TEST_TIMEOUT_MAX_MINS)
                    .contains(&recipe.timeout_minutes)
                || !(1..=MAX_CPU_QUOTA_PERCENT).contains(&recipe.cpu_quota_percent)
        })
    {
        return Err(invalid());
    }
    Ok(manifest)
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
    let manifest = load(cwd)?;
    if let (Some(recipe), Some(minutes)) = (manifest.recipes.get(name), test.timeout_minutes) {
        test.timeout_minutes = Some(minutes.min(recipe.timeout_minutes));
    }
    Ok(())
}

pub(super) fn command(tools: &JobTools, test: &TestJobParams, cwd: &Path) -> Result<JobCommand> {
    let manifest = load(cwd)?;
    let recipe = test
        .recipe
        .as_ref()
        .and_then(|name| manifest.recipes.get(name))
        .ok_or_else(|| DaemonError::InvalidParam(JOB_RECIPE_NOT_ALLOWED.into()))?;
    let timeout_minutes = test.timeout_minutes.unwrap_or(recipe.timeout_minutes);
    if timeout_minutes > recipe.timeout_minutes {
        return Err(DaemonError::InvalidParam(JOB_INVALID_PARAMS.into()));
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
        runtime_max_secs: u64::from(timeout_minutes) * 60 + JOB_TIMEOUT_UNIT_GRACE_SECS,
        stop_timeout_secs: 60,
        log_max_bytes: 64 * MIB,
        memory_max_gib: 24,
        cpu_quota_percent: recipe.cpu_quota_percent,
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
            assert_eq!(result.runtime_max_secs, 600 + JOB_TIMEOUT_UNIT_GRACE_SECS);
            assert_eq!(result.cpu_quota_percent, 200);
            assert_eq!(result.memory_max_gib, 24);
            let mut too_long = test_params();
            too_long.timeout_minutes = Some(21);
            assert_eq!(
                refusal(command(&tools, &too_long, dir.path()).unwrap_err()),
                JOB_INVALID_PARAMS
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
