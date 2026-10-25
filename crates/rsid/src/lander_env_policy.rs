//! #3391: the environment a merge-queue lander and its gate children get.
//!
//! The lander builds and tests agent-written sources (cargo build scripts,
//! `RUSTC_WRAPPER`, python test modules, git hooks), so anything in its
//! environment is readable by agent-written code. The daemon therefore starts
//! the lander with an allowlisted environment ([`apply`]) and the lander takes
//! its own startup snapshot through the same filter ([`filter`]), so every
//! child it starts sees the same allowlist.
//!
//! A name passes when it is
//! 1. in [`EXACT`] or starts with a [`PREFIXES`] family the gate needs, and
//!    does not look like a credential ([`looks_like_credential`]); or
//! 2. in [`EXTRA`], an exact name with a documented reason that wins over the
//!    credential patterns.
//!
//! `RSI_SESSION_TOKEN` is always dropped. Every other name is dropped.

use std::ffi::{OsStr, OsString};

/// Never passed, whatever else matches (daemon authority).
const ALWAYS_DROP: &[&str] = &["RSI_SESSION_TOKEN"];

/// Exact names (compared upper-cased) the gate's tools need.
const EXACT: &[&str] = &[
    "PATH",
    "HOME",
    "USER",
    "LOGNAME",
    "SHELL",
    "LANG",
    "LANGUAGE",
    "TZ",
    "TMPDIR",
    "TMP",
    "TEMP",
    "TERM",
    "NO_COLOR",
    "CLICOLOR",
    "CLICOLOR_FORCE",
    "COLORTERM",
    "MAKEFLAGS",
    "CC",
    "CXX",
    "AR",
    "CFLAGS",
    "CXXFLAGS",
    "CPPFLAGS",
    "LDFLAGS",
    "LD_LIBRARY_PATH",
    "LIBRARY_PATH",
    "CPATH",
    "C_INCLUDE_PATH",
    "CPLUS_INCLUDE_PATH",
    "PKG_CONFIG",
    "SDKROOT",
    "MACOSX_DEPLOYMENT_TARGET",
    "DEVELOPER_DIR",
    "GIT_SSH",
    "GIT_SSH_COMMAND",
    "GIT_TERMINAL_PROMPT",
    "GIT_CEILING_DIRECTORIES",
    "GIT_CONFIG_GLOBAL",
    "GIT_CONFIG_SYSTEM",
    "GIT_CONFIG_NOSYSTEM",
    "GIT_AUTHOR_NAME",
    "GIT_AUTHOR_EMAIL",
    "GIT_COMMITTER_NAME",
    "GIT_COMMITTER_EMAIL",
    // Windows process basics.
    "SYSTEMROOT",
    "COMSPEC",
    "PATHEXT",
    "WINDIR",
];

/// Name families (compared upper-cased) the gate needs: locale, cargo and
/// rustup, the Rust toolchain (`RUSTC_WRAPPER`, `RUSTFLAGS`, `RUST_BACKTRACE`),
/// sccache, nextest, pkg-config, XDG directories and the lander's own `RSI_`
/// knobs (`RSI_LANDER_*`, `RSI_ROLLING_*`, `RSI_QA_*`, ...).
const PREFIXES: &[&str] = &[
    "LC_",
    "CARGO_",
    "RUSTUP_",
    "RUST",
    "SCCACHE_",
    "NEXTEST_",
    "PKG_CONFIG_",
    "XDG_",
    "RSI_",
];

/// Exact names that are passed although they are not in a family above or
/// look like a credential, each with the reason the gate needs it. Adding a
/// name here is a credential-exposure decision: it needs a reason.
pub const EXTRA: &[(&str, &str)] = &[
    (
        "SSH_AUTH_SOCK",
        "git fetch/push to an ssh `origin` authenticates through the agent socket",
    ),
    (
        "SSH_ASKPASS",
        "git over ssh asks for a passphrase through this program, never a secret value",
    ),
    (
        "GIT_ASKPASS",
        "git over https asks for credentials through this program, never a secret value",
    ),
    (
        "DBUS_SESSION_BUS_ADDRESS",
        "systemd-run --user (the build-slice scope) reaches the user manager through it",
    ),
    (
        "HTTP_PROXY",
        "cargo and git egress behind the operator's proxy",
    ),
    (
        "HTTPS_PROXY",
        "cargo and git egress behind the operator's proxy",
    ),
    (
        "ALL_PROXY",
        "cargo and git egress behind the operator's proxy",
    ),
    ("NO_PROXY", "proxy exclusions for the same egress"),
    ("SSL_CERT_FILE", "TLS trust store for cargo, git and curl"),
    ("SSL_CERT_DIR", "TLS trust store for cargo, git and curl"),
    ("CURL_CA_BUNDLE", "TLS trust store for cargo, git and curl"),
    (
        "REQUESTS_CA_BUNDLE",
        "TLS trust store for python test modules",
    ),
];

/// Name families that are credentials whatever follows (compared upper-cased).
const CREDENTIAL_PREFIXES: &[&str] = &[
    "AWS_",
    "ANTHROPIC_",
    "OPENAI_",
    "OPENROUTER_",
    "AZURE_",
    "GOOGLE_",
    "GCP_",
    "GCLOUD_",
    "GH_",
    "GITHUB_",
    "GITLAB_",
    "HF_",
    "HUGGING",
    "NPM_",
];

/// Name endings that mark a credential (compared upper-cased).
const CREDENTIAL_SUFFIXES: &[&str] = &[
    "_TOKEN",
    "_TOKENS",
    "_SECRET",
    "_SECRETS",
    "_API_KEY",
    "_APIKEY",
    "_ACCESS_KEY",
    "_SECRET_KEY",
    "_PRIVATE_KEY",
    "_PASSWORD",
    "_PASSWD",
    "_PASSPHRASE",
    "_CREDENTIAL",
    "_CREDENTIALS",
];

fn extra_reason(upper: &str) -> Option<&'static str> {
    EXTRA
        .iter()
        .find(|(name, _)| *name == upper)
        .map(|(_, reason)| *reason)
}

/// Whether `upper` (an upper-cased name) looks like a credential.
fn looks_like_credential(upper: &str) -> bool {
    matches!(
        upper,
        "TOKEN" | "SECRET" | "PASSWORD" | "API_KEY" | "APIKEY"
    ) || CREDENTIAL_PREFIXES
        .iter()
        .any(|prefix| upper.starts_with(prefix))
        || CREDENTIAL_SUFFIXES
            .iter()
            .any(|suffix| upper.ends_with(suffix))
}

/// Whether the lander and its gate children may inherit the variable `name`.
/// A name that is not valid Unicode is dropped.
pub fn is_allowed(name: &OsStr) -> bool {
    let Some(name) = name.to_str() else {
        return false;
    };
    let upper = name.to_ascii_uppercase();
    if ALWAYS_DROP.contains(&upper.as_str()) {
        return false;
    }
    if extra_reason(&upper).is_some() {
        return true;
    }
    if looks_like_credential(&upper) {
        return false;
    }
    EXACT.contains(&upper.as_str()) || PREFIXES.iter().any(|prefix| upper.starts_with(prefix))
}

/// `vars` reduced to the allowed names.
pub fn filter(vars: impl IntoIterator<Item = (OsString, OsString)>) -> Vec<(OsString, OsString)> {
    vars.into_iter()
        .filter(|(name, _)| is_allowed(name))
        .collect()
}

/// Give `command`'s child exactly the allowed part of this process's
/// environment. Call it before the `env` calls that add the child's own
/// variables: `env_clear` drops anything set earlier.
pub fn apply(command: &mut std::process::Command) -> &mut std::process::Command {
    command.env_clear().envs(filter(std::env::vars_os()))
}

/// [`apply`] for an async-runtime command.
pub fn apply_tokio(command: &mut tokio::process::Command) {
    apply(command.as_std_mut());
}

#[cfg(test)]
mod tests {
    use super::*;

    fn os(value: &str) -> OsString {
        OsString::from(value)
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-04"))]
    #[test]
    fn credentials_are_dropped_and_gate_variables_kept() {
        for name in [
            "FOO_API_KEY",
            "RSI_SESSION_TOKEN",
            "ANTHROPIC_AUTH",
            "AWS_ACCESS_KEY_ID",
            "AWS_PROFILE",
            "OPENAI_ORG",
            "OPENROUTER_BASE",
            "GITHUB_TOKEN",
            "GH_TOKEN",
            "CARGO_REGISTRY_TOKEN",
            "CARGO_REGISTRIES_PRIVATE_TOKEN",
            "SCCACHE_AWS_SECRET_ACCESS_KEY",
            "RSI_LINEAR_API_KEY",
            "RSI_DREAM_API_KEY",
            "SOME_PASSWORD",
            "SECRET",
            "UNLISTED_VARIABLE",
            "DISPLAY",
            "GIT_TRACE",
            "GIT_DIR",
            "PYTHONPATH",
        ] {
            assert!(!is_allowed(OsStr::new(name)), "{name} must be dropped");
        }
        for name in [
            "PATH",
            "HOME",
            "LANG",
            "LC_ALL",
            "TZ",
            "TMPDIR",
            "RSI_LANDER_EXACT_GATE",
            "RSI_ROLLING_BASE_CACHE_DIR",
            "RSI_KNOWN_FAILURES_SNAPSHOT",
            "RSI_QUEUE_TOKEN_THRESHOLD",
            "CARGO_TARGET_DIR",
            "CARGO_HOME",
            "CARGO_BUILD_JOBS",
            "RUSTUP_HOME",
            "RUSTC_WRAPPER",
            "RUSTFLAGS",
            "RUST_BACKTRACE",
            "SCCACHE_DIR",
            "XDG_RUNTIME_DIR",
            "SSH_AUTH_SOCK",
            "DBUS_SESSION_BUS_ADDRESS",
            "https_proxy",
            "GIT_CEILING_DIRECTORIES",
        ] {
            assert!(is_allowed(OsStr::new(name)), "{name} must be kept");
        }
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-04"))]
    #[test]
    fn filter_keeps_only_allowed_pairs() {
        let kept = filter([
            (os("PATH"), os("/bin")),
            (os("FOO_API_KEY"), os("planted")),
            (os("RSI_SESSION_TOKEN"), os("planted")),
            (os("CARGO_TARGET_DIR"), os("/t")),
        ]);
        assert_eq!(
            kept,
            vec![(os("PATH"), os("/bin")), (os("CARGO_TARGET_DIR"), os("/t"))]
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-04"))]
    #[test]
    fn every_explicit_extra_has_a_reason_and_wins_over_credential_patterns() {
        for (name, reason) in EXTRA {
            assert!(!reason.trim().is_empty(), "{name} needs a reason");
            assert!(is_allowed(OsStr::new(name)), "{name} is allowlisted");
        }
        assert!(
            EXTRA.iter().all(|(name, _)| !ALWAYS_DROP.contains(name)),
            "the session token can never be allowlisted"
        );
    }

    /// A real child process: planted credentials never reach it, while a
    /// required gate variable does.
    #[cfg(target_os = "linux")]
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-04"))]
    #[test]
    fn a_child_sees_gate_variables_and_not_planted_credentials() {
        let mut command = std::process::Command::new("env");
        command.env_clear().envs(filter([
            (os("PATH"), std::env::var_os("PATH").unwrap_or_default()),
            (os("FOO_API_KEY"), os("planted-key")),
            (os("RSI_SESSION_TOKEN"), os("planted-token")),
            (os("CARGO_TARGET_DIR"), os("/queue/target")),
            (os("RSI_LANDER_EXACT_GATE"), os("1")),
        ]));
        let output = command.output().expect("env runs");
        let text = String::from_utf8_lossy(&output.stdout);
        assert!(text.contains("CARGO_TARGET_DIR=/queue/target"), "{text}");
        assert!(text.contains("RSI_LANDER_EXACT_GATE=1"), "{text}");
        assert!(text.contains("PATH="), "{text}");
        assert!(!text.contains("planted"), "{text}");
    }
}
