//! Credential env scrub for every provider-process spawn.
//!
//! Provider CLIs are spawned without `env_clear`, so without this every key
//! the operator's shell exported would reach every agent and its tool shells.
//! [`scrub_credential_env`] removes every slot's env var names plus the
//! generic fallbacks from what the child *inherits*. A value that rsid code
//! explicitly set on the same `Command` (the one route credential injected by
//! [`inject_route_credential`]) is preserved, so scrub and injection compose
//! in any order and repeated scrubs are idempotent.

use super::secret::SecretString;
use super::slots::scrubbed_env_var_names;
use std::ffi::OsStr;
use tokio::process::Command;

/// Remove every inherited credential env var from `cmd`'s child environment.
pub fn scrub_credential_env(cmd: &mut Command) {
    scrub_std_credential_env(cmd.as_std_mut());
}

/// `std::process::Command` form of [`scrub_credential_env`].
pub fn scrub_std_credential_env(cmd: &mut std::process::Command) {
    let explicitly_set: Vec<String> = cmd
        .get_envs()
        .filter(|(_, value)| value.is_some())
        .map(|(key, _)| key.to_string_lossy().into_owned())
        .collect();
    for name in scrubbed_env_var_names() {
        if !explicitly_set.iter().any(|set| set == name) {
            cmd.env_remove(name);
        }
    }
}

/// Inject exactly one route credential.
///
/// For a Codex child also pass
/// `-c shell_environment_policy.exclude=["<VAR>"]`: Codex's default
/// `*KEY*`/`*TOKEN*` excludes are off by default (`ignore_default_excludes`
/// defaults to true), so without the explicit exclude its tool shells would
/// inherit the key.
pub fn inject_route_credential(
    cmd: &mut Command,
    env_var: &'static str,
    secret: &SecretString,
    codex_child: bool,
) {
    cmd.env(env_var, secret.expose());
    if codex_child {
        cmd.arg("-c").arg(codex_shell_exclude_arg(env_var));
    }
    scrub_credential_env(cmd);
}

/// The Codex `-c` override that hides `env_var` from Codex tool shells.
#[must_use]
pub fn codex_shell_exclude_arg(env_var: &str) -> String {
    format!("shell_environment_policy.exclude=[{env_var:?}]")
}

/// Test/diagnostic helper: the credential names a command's child would see
/// explicitly set (names only).
#[must_use]
pub fn explicit_credential_env_names(cmd: &Command) -> Vec<String> {
    let names: Vec<&str> = scrubbed_env_var_names().collect();
    cmd.as_std()
        .get_envs()
        .filter(|(key, value)| value.is_some() && names.iter().any(|name| OsStr::new(name) == *key))
        .map(|(key, _)| key.to_string_lossy().into_owned())
        .collect()
}

#[cfg(any(test, feature = "test-seam"))]
#[allow(clippy::unwrap_used, clippy::expect_used)]
pub mod tests {
    use super::*;
    use std::collections::BTreeMap;

    /// A controlled daemon environment: every scrubbed credential name set
    /// to a fake `sk-test` value, plus ordinary non-secret vars.
    pub fn controlled_parent_env() -> BTreeMap<String, String> {
        let mut env: BTreeMap<String, String> = scrubbed_env_var_names()
            .map(|name| (name.to_string(), format!("sk-test-leak-{name}")))
            .collect();
        for (key, value) in [
            ("PATH", "/usr/bin:/bin"),
            ("HOME", "/home/rsi-test"),
            ("LANG", "C.UTF-8"),
            ("RSI_K1_HARMLESS_VAR", "harmless"),
        ] {
            env.insert(key.to_string(), value.to_string());
        }
        env
    }

    /// The environment an inheriting child of `cmd` would see if the daemon
    /// ran with `parent`: parent vars, then the command's explicit sets and
    /// removals. (Not for `env_clear` commands; use [`env_view`] there.)
    pub fn effective_child_env(
        parent: &BTreeMap<String, String>,
        cmd: &Command,
    ) -> BTreeMap<String, String> {
        let mut env = parent.clone();
        for (key, value) in env_view(cmd) {
            match value {
                Some(value) => {
                    env.insert(key, value);
                }
                None => {
                    env.remove(&key);
                }
            }
        }
        env
    }

    /// Suppression plus a positive end state: every credential name is
    /// absent except `allowed`, and the ordinary vars still reach the child.
    pub fn assert_child_env_suppresses_keys(cmd: &Command, allowed: Option<&str>) {
        let parent = controlled_parent_env();
        let child = effective_child_env(&parent, cmd);
        for name in scrubbed_env_var_names() {
            if Some(name) != allowed {
                assert!(
                    !child.contains_key(name),
                    "credential var {name} reaches the child"
                );
            }
        }
        for key in ["PATH", "HOME", "LANG", "RSI_K1_HARMLESS_VAR"] {
            assert_eq!(
                child.get(key),
                parent.get(key),
                "non-secret {key} must pass"
            );
        }
    }

    /// Effective child env view of a command: `Some(value)` for explicit
    /// sets, `None` for explicit removals. Names only matter for asserts.
    pub fn env_view(cmd: &Command) -> BTreeMap<String, Option<String>> {
        cmd.as_std()
            .get_envs()
            .map(|(key, value)| {
                (
                    key.to_string_lossy().into_owned(),
                    value.map(|value| value.to_string_lossy().into_owned()),
                )
            })
            .collect()
    }

    /// Assert the child would see no credential var except `allowed`
    /// (explicitly injected), and that every other credential var is
    /// explicitly removed (so an inherited daemon value cannot leak).
    pub fn assert_only_injected(cmd: &Command, allowed: Option<&str>) {
        let view = env_view(cmd);
        for name in scrubbed_env_var_names() {
            match view.get(name) {
                Some(Some(_)) => assert_eq!(
                    Some(name),
                    allowed,
                    "credential var {name} would reach the child"
                ),
                Some(None) => assert_ne!(Some(name), allowed, "{name} injected but removed"),
                None => panic!("credential var {name} is inherited, not scrubbed"),
            }
        }
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-05"))]
    #[test]
    fn scrub_removes_every_credential_name() {
        let mut cmd = Command::new("true");
        scrub_credential_env(&mut cmd);
        assert_only_injected(&cmd, None);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-05"))]
    #[test]
    fn injection_survives_later_scrubs_and_adds_codex_exclude() {
        let mut cmd = Command::new("codex");
        inject_route_credential(
            &mut cmd,
            "OPEN_ROUTER",
            &SecretString::new("sk-test-inject".into()),
            true,
        );
        scrub_credential_env(&mut cmd);
        scrub_credential_env(&mut cmd);
        assert_only_injected(&cmd, Some("OPEN_ROUTER"));
        let args: Vec<String> = cmd
            .as_std()
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            args,
            vec![
                "-c".to_string(),
                "shell_environment_policy.exclude=[\"OPEN_ROUTER\"]".to_string()
            ]
        );
        assert_eq!(explicit_credential_env_names(&cmd), vec!["OPEN_ROUTER"]);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-05"))]
    #[test]
    fn non_codex_injection_adds_no_argv() {
        let mut cmd = Command::new("x");
        inject_route_credential(
            &mut cmd,
            "AWS_BEARER_TOKEN_BEDROCK",
            &SecretString::new("bedrock-api-key-test".into()),
            false,
        );
        assert_eq!(cmd.as_std().get_args().count(), 0);
        assert_only_injected(&cmd, Some("AWS_BEARER_TOKEN_BEDROCK"));
    }
}
