//! Transient per-user launchd services. No PID files or daemon-held children:
//! launchd owns the job process group across session and daemon termination.

use super::{JobParams, JobRuntime, LaunchSpec, WRAPPER, s, tail};
use rsi_common::agent_jobs::JOB_PLATFORM_UNSUPPORTED;
use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Output};

#[derive(Debug, Clone)]
pub(super) struct LaunchdJobRuntime {
    launchctl: PathBuf,
    domain: String,
}

impl Default for LaunchdJobRuntime {
    fn default() -> Self {
        let uid = nix::unistd::Uid::current();
        let gui = format!("gui/{uid}");
        let gui_available = Command::new("/bin/launchctl")
            .args(["print", &gui])
            .env_remove("RSI_SESSION_TOKEN")
            .env_remove(super::STRIP_NAMESPACE_ENV)
            .output()
            .is_ok_and(|output| output.status.success());
        Self {
            launchctl: PathBuf::from("/bin/launchctl"),
            domain: if gui_available {
                gui
            } else {
                format!("user/{uid}")
            },
        }
    }
}

fn valid_label(label: &str) -> bool {
    label
        .strip_prefix("rsi-job-")
        .is_some_and(|id| uuid::Uuid::parse_str(id).is_ok_and(|uuid| uuid.to_string() == id))
}

fn xml_string(value: &str) -> String {
    format!(
        "<string>{}</string>",
        value
            .replace('&', "&amp;")
            .replace('<', "&lt;")
            .replace('>', "&gt;")
            .replace('"', "&quot;")
            .replace('\'', "&apos;")
    )
}

/// The watchdog targets the launchd service, never a captured PID. On normal
/// completion or timeout bootout removes the service, and launchd kills its
/// remaining process group (including the watchdog). No KeepAlive/relaunch.
fn wrapper() -> String {
    format!(
        r#"target=$1; timeout=$2; shift 2
( sleep "$timeout"; exec /bin/launchctl bootout "$target" ) </dev/null >/dev/null 2>&1 &
{WRAPPER}
/bin/launchctl bootout "$target" >/dev/null 2>&1
"#
    )
}

fn plist(spec: &LaunchSpec, target: &str) -> String {
    // launchd's environment belongs to the user manager, rather than the
    // submitting process. Strip transport/ownership variables even if someone
    // previously set them in that manager's environment. Never serialize them.
    let mut argv = vec![
        s("/usr/bin/env"),
        s("-u"),
        s("RSI_SESSION_TOKEN"),
        s("-u"),
        s(super::STRIP_NAMESPACE_ENV),
        s("/bin/sh"),
        s("-c"),
        wrapper(),
        s("rsi-job"),
        target.into(),
        spec.command.runtime_max_secs.to_string(),
        spec.log_path.display().to_string(),
        spec.status_path.display().to_string(),
        spec.command.log_max_bytes.to_string(),
    ];
    argv.extend(spec.command.argv.iter().cloned());
    let mut env = String::new();
    for key in ["HOME", "PATH"] {
        if let Ok(value) = std::env::var(key) {
            env.push_str(&format!("<key>{key}</key>{}", xml_string(&value)));
        }
    }
    if let Some(build) = &spec.build_environment {
        for (key, value) in [
            ("TMPDIR", build.tmp_dir.display().to_string()),
            ("CARGO_TARGET_DIR", build.target_dir.display().to_string()),
            ("CARGO_BUILD_JOBS", s("2")),
            ("CARGO_PROFILE_TEST_DEBUG", s("0")),
            ("CARGO_PROFILE_DEV_DEBUG", s("0")),
        ] {
            env.push_str(&format!("<key>{key}</key>{}", xml_string(&value)));
        }
    }
    // Linux's MemoryMax/CPUQuota have no launchd equivalent. Mac builds use
    // fewer workers instead. Linux-only shard/candidate/landing/cloud scripts
    // are refused by validate; no shard cleaner can race these Mac jobs.
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<plist version="1.0"><dict>
<key>Label</key>{label}
<key>ProgramArguments</key><array>{args}</array>
<key>WorkingDirectory</key>{cwd}
<key>EnvironmentVariables</key><dict>{env}</dict>
<key>RunAtLoad</key><false/>
<key>KeepAlive</key><false/>
<key>AbandonProcessGroup</key><false/>
<key>ExitTimeOut</key><integer>{stop_timeout}</integer>
<key>StandardErrorPath</key>{errors}
</dict></plist>
"#,
        label = xml_string(target.rsplit('/').next().unwrap_or(&spec.unit_name)),
        args = argv.iter().map(|arg| xml_string(arg)).collect::<String>(),
        cwd = xml_string(&spec.cwd.display().to_string()),
        stop_timeout = spec.command.stop_timeout_secs,
        errors = xml_string(
            &spec
                .log_path
                .with_extension("launchd.log")
                .display()
                .to_string()
        )
    )
}

#[derive(Debug, PartialEq, Eq)]
enum ServiceState {
    Absent,
    Inactive,
    Active,
}

/// An unknown/control failure is an error, never proof that a service died.
/// Only launchctl's explicit missing-service reply proves absence. Parsing
/// state is restricted to the top-level dictionary, not an environment value.
fn service_state(output: &Output) -> Result<ServiceState, String> {
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    if !output.status.success() {
        if stderr
            .lines()
            .any(|line| line.starts_with("Could not find service \""))
        {
            return Ok(ServiceState::Absent);
        }
        return Err(format!(
            "launchctl print exited {}: {}",
            output.status,
            tail(&stderr)
        ));
    }
    let state = stdout
        .lines()
        .find_map(|line| line.strip_prefix("\tstate = "));
    match state {
        Some("not running" | "exited" | "waiting") => Ok(ServiceState::Inactive),
        Some(_) => Ok(ServiceState::Active),
        None => Err("launchctl print omitted service state".into()),
    }
}

impl LaunchdJobRuntime {
    fn target(&self, label: &str) -> Result<String, String> {
        if let Some((domain, job)) = label.rsplit_once('/') {
            let uid = nix::unistd::Uid::current();
            if (domain == format!("gui/{uid}") || domain == format!("user/{uid}"))
                && valid_label(job)
            {
                return Ok(label.into());
            }
            return Err("invalid RSI job service target".into());
        }
        if !valid_label(label) {
            return Err("invalid RSI job service label".into());
        }
        Ok(format!("{}/{label}", self.domain))
    }

    fn control(&self, args: &[&str]) -> Result<Output, String> {
        Command::new(&self.launchctl)
            .args(args)
            .env_remove("RSI_SESSION_TOKEN")
            .env_remove(super::STRIP_NAMESPACE_ENV)
            .output()
            .map_err(|error| format!("cannot start launchctl: {error}"))
    }

    fn state(&self, target: &str) -> Result<ServiceState, String> {
        service_state(&self.control(&["print", target])?)
    }
}

impl JobRuntime for LaunchdJobRuntime {
    fn unit_name(&self, id: uuid::Uuid) -> String {
        format!("{}/rsi-job-{id}", self.domain)
    }
    fn validate(&self, params: &JobParams) -> Result<(), &'static str> {
        match params {
            JobParams::Build(_) => Ok(()),
            JobParams::Test(test) if test.shard.is_none() && test.candidate_receipt.is_none() => {
                Ok(())
            }
            _ => Err(JOB_PLATFORM_UNSUPPORTED),
        }
    }

    fn launch(&self, spec: &LaunchSpec) -> Result<(), String> {
        let target = self.target(&spec.unit_name)?;
        let domain = target
            .rsplit_once('/')
            .ok_or("job service has no domain")?
            .0;
        let path = spec.log_path.with_extension("plist");
        let parent = path.parent().ok_or("job plist has no parent")?;
        // Exclusive private temporary file + atomic rename, with literal argv
        // XML strings. A path or filter cannot turn into shell/launchctl code.
        let mut file = tempfile::NamedTempFile::new_in(parent).map_err(|e| e.to_string())?;
        file.write_all(plist(spec, &target).as_bytes())
            .map_err(|e| e.to_string())?;
        file.as_file().sync_all().map_err(|e| e.to_string())?;
        file.persist(&path).map_err(|e| e.to_string())?;
        let output = self.control(&["bootstrap", domain, &path.display().to_string()])?;
        if !output.status.success() {
            return Err(format!(
                "launchctl bootstrap exited {}: {}",
                output.status,
                tail(&String::from_utf8_lossy(&output.stderr))
            ));
        }
        // Explicitly start once: on current macOS RunAtLoad alone can leave a
        // newly bootstrapped service idle. With RunAtLoad=false there is no
        // race between automatic completion/bootout and this kickstart.
        let started = self.control(&["kickstart", &target]);
        match started {
            Ok(output) if output.status.success() => Ok(()),
            other => {
                self.stop_unit(&target)?;
                Err(match other {
                    Ok(output) => format!(
                        "launchctl kickstart exited {}: {}",
                        output.status,
                        tail(&String::from_utf8_lossy(&output.stderr))
                    ),
                    Err(error) => error,
                })
            }
        }
    }

    fn unit_active(&self, label: &str) -> bool {
        // Control errors are conservatively alive: never delete a live job's
        // scratch or settle it lost because launchctl was temporarily broken.
        self.target(label)
            .and_then(|target| self.state(&target))
            .map_or(true, |state| state == ServiceState::Active)
    }

    fn stop_unit(&self, label: &str) -> Result<(), String> {
        let target = self.target(label)?;
        if self.state(&target)? == ServiceState::Absent {
            return Ok(());
        }
        let output = self.control(&["bootout", &target])?;
        if self.state(&target)? == ServiceState::Absent {
            return Ok(());
        }
        Err(format!(
            "service {target} still registered after launchctl bootout ({}): {}",
            output.status,
            tail(&String::from_utf8_lossy(&output.stderr))
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::process::ExitStatusExt;

    fn output(code: i32, stdout: &str, stderr: &str) -> Output {
        Output {
            status: std::process::ExitStatus::from_raw(code << 8),
            stdout: stdout.as_bytes().to_vec(),
            stderr: stderr.as_bytes().to_vec(),
        }
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn launchd_liveness_requires_a_service_state_or_explicit_absence() {
        for state in ["running", "spawn scheduled", "spawned"] {
            assert_eq!(
                service_state(&output(
                    0,
                    &format!("service = {{\n\tstate = {state}\n}}"),
                    ""
                ))
                .unwrap(),
                ServiceState::Active
            );
        }
        assert_eq!(
            service_state(&output(0, "service = {\n\tstate = not running\n}", "")).unwrap(),
            ServiceState::Inactive
        );
        assert_eq!(
            service_state(&output(
                113,
                "",
                "Bad request.\nCould not find service \"rsi-job-id\" in domain for uid: 501\n"
            ))
            .unwrap(),
            ServiceState::Absent
        );
        assert!(service_state(&output(1, "", "permission denied")).is_err());
        assert!(
            service_state(&output(
                0,
                "service = {\n\tenvironment = {\n\t\tstate = not running\n\t}\n}",
                ""
            ))
            .is_err()
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn launchd_control_handles_are_canonical_job_labels() {
        let runtime = LaunchdJobRuntime::default();
        let label = format!("rsi-job-{}", uuid::Uuid::new_v4());
        assert_eq!(
            runtime.target(&label).unwrap(),
            format!("{}/{label}", runtime.domain)
        );
        let uid = nix::unistd::Uid::current().as_raw();
        let saved = format!("gui/{uid}/{label}");
        let after_restart = LaunchdJobRuntime {
            launchctl: PathBuf::from("/missing-launchctl"),
            domain: format!("user/{uid}"),
        };
        assert_eq!(
            after_restart.target(&saved).unwrap(),
            saved,
            "recovery uses the persisted domain even when discovery changed"
        );
        assert!(
            after_restart
                .target(&format!("gui/{}/{label}", uid + 1))
                .is_err()
        );
        for bad in [
            "1234",
            "user/501/other",
            "rsi-job-../other",
            "com.apple.Finder",
            "rsi-job-00000000000000000000000000000000",
        ] {
            assert!(runtime.target(bad).is_err());
            assert!(runtime.stop_unit(bad).is_err());
        }
        let unavailable = LaunchdJobRuntime {
            launchctl: PathBuf::from("/missing-launchctl"),
            domain: runtime.domain,
        };
        assert!(
            unavailable.unit_active(&label),
            "an unavailable controller conservatively keeps the job live"
        );
        assert!(unavailable.stop_unit(&label).is_err());
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn launchd_plist_escapes_literal_argv_and_uses_one_shot_group_custody() {
        let spec = LaunchSpec {
            unit_name: format!("rsi-job-{}", uuid::Uuid::new_v4()),
            cwd: PathBuf::from("/tmp/a & <b>"),
            log_path: PathBuf::from("/tmp/log"),
            status_path: PathBuf::from("/tmp/status"),
            command: super::super::JobCommand {
                argv: vec![
                    "/bin/echo".into(),
                    "literal & <tag> \"quote\" 'x' $HOME".into(),
                ],
                runtime_max_secs: 10,
                stop_timeout_secs: 2,
                log_max_bytes: 1024,
                memory_max_gib: 1,
                cpu_quota_percent: 100,
            },
            build_environment: Some(super::super::BuildEnvironment {
                tmp_dir: PathBuf::from("/tmp/private"),
                target_dir: PathBuf::from("/tmp/target"),
                artifact_lock: None,
            }),
        };
        let xml = plist(&spec, "user/501/rsi-job-example");
        assert!(xml.contains(
            "<string>literal &amp; &lt;tag&gt; &quot;quote&quot; &apos;x&apos; $HOME</string>"
        ));
        assert!(xml.contains("<key>RunAtLoad</key><false/>"));
        assert!(xml.contains("<key>KeepAlive</key><false/>"));
        assert!(xml.contains("<key>AbandonProcessGroup</key><false/>"));
        assert!(xml.contains("<key>CARGO_BUILD_JOBS</key><string>2</string>"));
        assert!(xml.contains("<string>-u</string><string>RSI_SESSION_TOKEN</string>"));
        assert!(wrapper().contains("exec /bin/launchctl bootout \"$target\""));
    }
}
