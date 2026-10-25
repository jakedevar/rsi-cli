//! #637: a Mac test payload cannot control the host's launchd services or
//! signal unrelated processes. HOME/PATH stubs and runner provenance cannot
//! establish that boundary for candidate or baseline code from an old tree.
//!
//! This is a bounded lifecycle fence, not a general untrusted-code sandbox.
//! The profile is compiled into the daemon and inherited across exec/fork;
//! neither a source declaration nor a recipe can replace it. The outer job
//! controller, timeout watchdog and custody sweep retain their authority.

use crate::error::{DaemonError, Result};
use rsi_common::agent_jobs::{JOB_TEST_SOURCE_UNCONTAINED, JobKind};

pub(super) const SANDBOX_EXEC: &str = "/usr/bin/sandbox-exec";

// Keep fake launchctl executables usable. The actual system executable is
// denied even when reached by an absolute path or a symlink, independently of
// candidate PATH. job-creation also denies submitting jobs through launchd
// APIs. Signal checks cover shell builtins as well as kill/pkill helpers.
// Children/self remain usable for test cleanup; the live daemon and TUI are
// never children of the payload. No filesystem, network or Cargo policy is
// changed here, and there is no environment switch to disable this fence.
const PROFILE: &str = r#"(version 1)
(allow default)
(deny job-creation)
(deny process-exec
    (literal "/bin/launchctl")
    (literal "/usr/bin/launchctl"))
(deny signal
    (require-all
        (require-not (target self))
        (require-not (target children))))
(deny mach-priv-task-port)
"#;

pub(super) fn check(kind: JobKind, available: bool) -> Result<()> {
    if kind == JobKind::Test && !available {
        return Err(DaemonError::InvalidParam(format!(
            "{JOB_TEST_SOURCE_UNCONTAINED}: macOS test payloads require {SANDBOX_EXEC}; candidate, baseline and recipe code cannot execute without the daemon lifecycle fence; use a supported host"
        )));
    }
    Ok(())
}

pub(super) fn payload(kind: JobKind, argv: &[String]) -> Vec<String> {
    // sandbox-exec fails before exec if the kernel cannot apply the profile.
    // There is deliberately no uncontained fallback. It wraps the entire
    // payload, so at-commit scripts and nested baseline worktrees inherit it.
    let mut protected = Vec::new();
    if kind == JobKind::Test {
        protected.extend([SANDBOX_EXEC.into(), "-p".into(), PROFILE.into()]);
    }
    protected.extend_from_slice(argv);
    protected
}

pub(super) fn setup_failed(exit_code: Option<i32>, log: &str) -> bool {
    exit_code.is_some_and(|code| code != 0)
        && log.lines().any(|line| line.starts_with("sandbox-exec: "))
}

#[cfg(all(
    test,
    any(not(feature = "test-shard-mode"), feature = "test-shard-other-01")
))]
mod tests {
    use super::*;
    use rsi_common::agent_jobs::{AgentSubmitJobRequestV1, JobParams};

    fn params(kind: JobKind, value: serde_json::Value) -> JobParams {
        serde_json::from_value::<AgentSubmitJobRequestV1>(
            serde_json::json!({"kind":kind,"params":value}),
        )
        .unwrap()
        .typed_params_detailed()
        .unwrap_or_else(|refusal| panic!("{}", refusal.detail()))
    }

    // Pure construction: no processes, services or installer suites.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn every_test_payload_requires_the_daemon_lifecycle_fence() {
        for value in [
            serde_json::json!({"package":"rsid","lib_only":true,"exact":true,"filters":["tests::inert"]}),
            serde_json::json!({"package":"rsi-repo-guards","test_target":"install_release_stage","exact":true,"filters":["now_restart_does_not_stage_for_the_daemon"]}),
            serde_json::json!({"package":"rsi-repo-guards","lib_only":true}),
            serde_json::json!({"package":"rsi-common","lib_only":true,"exact":true,"filters":["tests::inert"],"at_commit":"0123456789012345678901234567890123456789"}),
            serde_json::json!({"scoped_test":{"base":"old-base","head":"old-candidate","rolling_tool":true}}),
            serde_json::json!({"recipe":"rolling-library-qa"}),
            serde_json::json!({"recipe":"manual"}),
        ] {
            let params = params(JobKind::Test, value);
            let argv = vec!["/usr/bin/true".into()];
            check(params.kind(), true).unwrap();
            assert_eq!(
                payload(params.kind(), &argv),
                [SANDBOX_EXEC, "-p", PROFILE, "/usr/bin/true"]
            );
            let error = check(params.kind(), false).unwrap_err().to_string();
            assert!(error.contains(JOB_TEST_SOURCE_UNCONTAINED), "{error}");
        }
        // Build/install/deploy commands retain their existing policy. This
        // fence is a job property, never an installer environment convention.
        let build = params(
            JobKind::Build,
            serde_json::json!({"command":"build","package":"rsid"}),
        );
        assert_eq!(
            payload(build.kind(), &["/usr/bin/true".into()]),
            ["/usr/bin/true"]
        );
        check(build.kind(), false).unwrap();
        assert!(setup_failed(
            Some(65),
            "sandbox-exec: sandbox_apply: denied"
        ));
        assert!(!setup_failed(Some(0), "sandbox-exec: candidate diagnostic"));
        assert!(!setup_failed(Some(1), "ordinary test failure"));
    }

    /// Only true and static launchctl help are possible payloads, even if the
    /// exec fence regresses. No service verb, domain, label or signal is used.
    /// Signal and job-creation enforcement are deliberately outside this test.
    #[cfg(target_os = "macos")]
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn mac_kernel_denies_real_launchctl_and_its_symlink_under_the_lifecycle_profile() {
        use std::os::unix::fs::{PermissionsExt, symlink};
        use std::path::Path;
        use std::process::{Command, Stdio};

        let run = |argv: &[String]| {
            let executable = Path::new(&argv[0]);
            let metadata = std::fs::metadata(executable).expect("probe executable exists");
            assert!(
                metadata.is_file(),
                "{} must be a file",
                executable.display()
            );
            assert_ne!(
                metadata.permissions().mode() & 0o111,
                0,
                "{} must be executable",
                executable.display()
            );
            // Use the production payload builder and its compiled PROFILE,
            // rather than a test copy or a candidate-controlled profile file.
            let protected = payload(JobKind::Test, argv);
            Command::new(&protected[0])
                .args(&protected[1..])
                .env_remove("RSI_SESSION_TOKEN")
                .env_remove(super::super::STRIP_NAMESPACE_ENV)
                .env("LC_ALL", "C")
                .stdin(Stdio::null())
                .output()
                .expect("system sandbox-exec must start")
        };

        // A parse/apply failure must fail here, before any exec-denial probe:
        // a broken profile is never evidence that launchctl was contained.
        let true_argv = vec!["/usr/bin/true".into()];
        let initialized = run(&true_argv);
        assert!(
            initialized.status.success(),
            "lifecycle profile initialization failed: {}: {}",
            initialized.status,
            String::from_utf8_lossy(&initialized.stderr)
        );

        let launchctl = Path::new("/bin/launchctl");
        let real = launchctl
            .canonicalize()
            .expect("real system launchctl exists");
        let scratch = crate::test_support::disk_backed_tempdir("637-launchctl-exec-alias");
        let alias = scratch.path().join("lifecycle-help");
        symlink(launchctl, &alias).expect("create alias to existing system executable");
        for executable in [launchctl, alias.as_path()] {
            assert_eq!(executable.canonicalize().unwrap(), real);
            let denied = run(&[
                executable.to_str().expect("probe path is UTF-8").into(),
                "help".into(),
            ]);
            let stderr = String::from_utf8_lossy(&denied.stderr);
            // sandbox-exec's execvp diagnostic distinguishes kernel exec
            // denial from profile parse/apply failure or a missing binary.
            let diagnostic = format!(
                "sandbox-exec: execvp() of '{}' failed: ",
                executable.display()
            );
            assert!(
                !denied.status.success()
                    && stderr.lines().any(|line| {
                        line.strip_prefix(&diagnostic).is_some_and(|reason| {
                            reason == "Operation not permitted" || reason == "Permission denied"
                        })
                    }),
                "expected kernel exec-permission denial for {}: {}: {stderr}; stdout: {}",
                executable.display(),
                denied.status,
                String::from_utf8_lossy(&denied.stdout)
            );
        }

        // Once deployed, this regression's job already carries PROFILE.
        // Establish that its own sandbox-exec probes can still initialize;
        // do not disable the outer production fence if nesting is unsupported.
        let nested = run(&payload(JobKind::Test, &true_argv));
        assert!(
            nested.status.success(),
            "nested lifecycle profile initialization failed: {}: {}",
            nested.status,
            String::from_utf8_lossy(&nested.stderr)
        );
    }
}
