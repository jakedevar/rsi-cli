//! Opt-in SSH shard executor. Only full rsid shard commands use this path;
//! failure comparison and isolated retries remain in the local lander.

use rsid::integration::{GuardCommand, GuardCommandReport, GuardStatus};
use serde::Deserialize;
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

const MAX_OUTPUT_BYTES: usize = 8 * 1024 * 1024;

#[derive(Clone, Debug)]
pub(super) struct Config {
    pub target: String,
    pub workdir: String,
    pub identity: PathBuf,
    pub run_as: String,
}

impl Config {
    pub fn validate(&self) -> Result<(), String> {
        let (user, host) = self
            .target
            .split_once('@')
            .ok_or("remote_config_invalid: target must be user@host")?;
        if !valid_atom(user)
            || host.is_empty()
            || !host
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || b".-".contains(&byte))
        {
            return Err("remote_config_invalid: target must be user@host".into());
        }
        if !valid_atom(&self.run_as) {
            return Err("remote_config_invalid: run-as user is invalid".into());
        }
        let path = Path::new(&self.workdir);
        if !path.is_absolute()
            || path
                .components()
                .any(|part| matches!(part, std::path::Component::ParentDir))
            || !self
                .workdir
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || b"/._-".contains(&byte))
        {
            return Err("remote_config_invalid: workdir must be an absolute plain path without parent traversal".into());
        }
        if !self.identity.is_file() {
            return Err("remote_config_invalid: identity file is missing".into());
        }
        Ok(())
    }
}

fn valid_atom(value: &str) -> bool {
    !value.is_empty()
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"_-".contains(&byte))
}

fn valid_sha(value: &str) -> bool {
    value.len() == 40
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn valid_fingerprint(value: &str) -> bool {
    value.len() == 71
        && value.starts_with("sha256:")
        && value[7..]
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ShardFingerprint {
    digest: String,
    inputs: BTreeMap<String, Value>,
}

fn compare_host_fingerprints(
    base: &ShardFingerprint,
    candidate: &ShardFingerprint,
) -> Result<(), String> {
    for proof in [base, candidate] {
        if !valid_fingerprint(&format!("sha256:{}", proof.digest))
            || proof.inputs.len() != 11
            || !proof
                .inputs
                .get("host_class")
                .is_some_and(|value| value.as_str().is_some_and(|host| !host.is_empty()))
        {
            return Err("remote_fingerprint_mismatch: incomplete remote proof".into());
        }
        for key in ["runner_blob", "checker_blob", "nextest_config_blob"] {
            if !proof
                .inputs
                .get(key)
                .is_some_and(|value| value.as_str().is_some_and(valid_sha))
            {
                return Err("remote_fingerprint_mismatch: invalid runner blob".into());
            }
        }
    }
    // The candidate can change runner blobs. Everything describing the remote
    // executor must match the base proof computed on that same physical host.
    for key in [
        "rustc",
        "cargo",
        "nextest",
        "target_triple",
        "host_class",
        "feature",
        "jobs",
        "test_threads",
    ] {
        if base.inputs.get(key).is_none() || base.inputs.get(key) != candidate.inputs.get(key) {
            return Err(format!(
                "remote_fingerprint_mismatch: remote base and candidate differ at {key}"
            ));
        }
    }
    Ok(())
}

#[derive(Debug)]
pub(super) struct Executor {
    config: Config,
    private_repo: PathBuf,
    run_id: String,
    prepared: BTreeSet<String>,
    base_proofs: BTreeMap<(String, String, u32), ShardFingerprint>,
}

impl Executor {
    pub fn new(config: Config, private_repo: PathBuf) -> Result<Self, String> {
        config.validate()?;
        Ok(Self {
            config,
            private_repo,
            run_id: uuid::Uuid::new_v4().simple().to_string(),
            prepared: BTreeSet::new(),
            base_proofs: BTreeMap::new(),
        })
    }

    fn root(&self) -> String {
        format!(
            "{}/{}",
            self.config.workdir.trim_end_matches('/'),
            self.run_id
        )
    }

    fn ssh_script(&self, script: &str, reason: &str) -> Result<Output, String> {
        ssh_script(&self.config, script, reason)
    }

    pub fn prepare(&mut self, sha: &str) -> Result<(), String> {
        if !valid_sha(sha) {
            return Err("remote_sha_mismatch: expected a full commit SHA".into());
        }
        if !self.prepared.insert(sha.to_owned()) {
            return Ok(());
        }
        let result = self.prepare_inner(sha);
        if result.is_err() {
            self.prepared.remove(sha);
        }
        result
    }

    fn prepare_inner(&self, sha: &str) -> Result<(), String> {
        let bundle_dir =
            tempfile::tempdir().map_err(|error| format!("remote_bundle_failed: {error}"))?;
        let bundle = bundle_dir.path().join("source.bundle");
        let source_ref = format!("refs/rsi-remote-gate/{sha}");
        git(&self.private_repo, &["update-ref", &source_ref, sha])?;
        let bundle_arg = bundle
            .to_str()
            .ok_or("remote_bundle_failed: non-UTF8 path")?;
        git(
            &self.private_repo,
            &["bundle", "create", bundle_arg, &source_ref],
        )?;
        let root = self.root();
        let staging = format!("/tmp/rsi-gate-{}", self.run_id);
        let upload = format!("{staging}/{sha}.bundle");
        let incoming = format!("{root}/incoming/{sha}.bundle");
        let setup = format!(
            "set -euo pipefail\ntest -x /usr/bin/python3.11\ninstall -d -m 700 '{staging}'\nsudo -n install -d -m 700 -o {user} -g {user} '{base}' '{root}' '{root}/bin' '{root}/incoming' '{root}/repo' '{root}/worktrees' '{root}/targets'\nsudo -n -H -u {user} ln -sfn /usr/bin/python3.11 '{root}/bin/python3'\n",
            user = self.config.run_as,
            base = self.config.workdir,
            root = root,
            staging = staging
        );
        self.ssh_script(&setup, "remote_setup_failed")?;
        let mut scp = Command::new("/usr/bin/scp");
        scp.args([
            "-F",
            "/dev/null",
            "-o",
            "BatchMode=yes",
            "-o",
            "IdentitiesOnly=yes",
            "-o",
            "StrictHostKeyChecking=yes",
            "-o",
            "ConnectTimeout=10",
            "-i",
        ])
        .arg(&self.config.identity)
        .arg(&bundle)
        .arg(format!("{}:{upload}", self.config.target));
        scrub_transport_env(&mut scp);
        let copied = scp
            .output()
            .map_err(|error| format!("remote_unreachable: {error}"))?;
        if !copied.status.success() {
            return Err(format!(
                "remote_unreachable: bundle upload failed: {}",
                String::from_utf8_lossy(&copied.stderr)
            ));
        }
        let fetch = format!(
            "set -euo pipefail\nsudo -n install -m 600 -o {user} -g {user} '{upload}' '{incoming}'\nrm -f '{upload}'\nrmdir '{staging}'\nsudo -n -H -u {user} git -C '{root}/repo' init -q\nsudo -n -H -u {user} git -C '{root}/repo' fetch -q '{incoming}' '{source_ref}'\nactual=$(sudo -n -H -u {user} git -C '{root}/repo' rev-parse FETCH_HEAD)\nif [ \"$actual\" != '{sha}' ]; then echo remote_sha_mismatch >&2; exit 42; fi\nsudo -n -H -u {user} git -C '{root}/repo' worktree add --detach '{root}/worktrees/{sha}' '{sha}' >/dev/null\nactual=$(sudo -n -H -u {user} git -C '{root}/worktrees/{sha}' rev-parse HEAD)\nif [ \"$actual\" != '{sha}' ]; then echo remote_sha_mismatch >&2; exit 42; fi\nsudo -n -H -u {user} rm -f '{incoming}'\n",
            user = self.config.run_as,
            root = root,
            incoming = incoming,
            staging = staging
        );
        self.ssh_script(&fetch, "remote_sha_mismatch")?;
        Ok(())
    }

    pub fn run_full_shard(
        &mut self,
        sha: &str,
        fingerprint_source_sha: &str,
        base_sha: &str,
        shard: &str,
        jobs: u32,
        command: &GuardCommand,
    ) -> Result<GuardCommandReport, String> {
        self.plan_full_shard(sha, fingerprint_source_sha, base_sha, shard, jobs, command)?
            .execute()
    }

    /// Prepares both commits and proves the remote fingerprint under the
    /// executor lock. The returned run needs no executor state, so several
    /// can execute on the gate host at once (#970).
    pub fn plan_full_shard(
        &mut self,
        sha: &str,
        fingerprint_source_sha: &str,
        base_sha: &str,
        shard: &str,
        jobs: u32,
        command: &GuardCommand,
    ) -> Result<ShardRun, String> {
        if !valid_sha(sha)
            || !valid_sha(fingerprint_source_sha)
            || !valid_sha(base_sha)
            || !super::RSID_SHARDS.contains(&shard)
            || !(1..=6).contains(&jobs)
        {
            return Err("remote_config_invalid: invalid shard execution identity".into());
        }
        self.prepare(sha)?;
        // The rolling base can predate the fingerprint script. Use the
        // candidate's exact script, as the local lander does.
        self.prepare(fingerprint_source_sha)?;
        let root = self.root();
        let probe = format!(
            "set -euo pipefail\nhome=$(getent passwd {user} | cut -d: -f6)\nfor commit in '{sha}' '{fingerprint_source_sha}'; do\n  worktree='{root}/worktrees/'\"$commit\"\n  actual=$(sudo -n -H -u {user} git -C \"$worktree\" rev-parse HEAD)\n  if [ \"$actual\" != \"$commit\" ]; then echo remote_sha_mismatch >&2; exit 42; fi\n  if [ -n \"$(sudo -n -H -u {user} git -C \"$worktree\" status --porcelain=v1)\" ]; then echo remote_sha_mismatch >&2; exit 42; fi\ndone\nsudo -n -H -u {user} env PATH=\"{root}/bin:$home/.cargo/bin:/usr/local/bin:/usr/bin:/bin\" python3.11 '{root}/worktrees/{fingerprint_source_sha}/scripts/rolling-shard-fingerprint.py' --sha '{sha}' --shard '{shard}' --jobs '{jobs}' --json\n",
            user = self.config.run_as,
            root = root
        );
        let remote_output = self.ssh_script(&probe, "remote_fingerprint_mismatch")?;
        let proof: ShardFingerprint =
            serde_json::from_slice(&remote_output.stdout).map_err(|error| {
                format!("remote_fingerprint_mismatch: invalid remote proof: {error}")
            })?;
        let remote_fingerprint = format!("sha256:{}", proof.digest);
        if !valid_fingerprint(&remote_fingerprint) {
            return Err("remote_fingerprint_mismatch: invalid remote digest".into());
        }
        compare_host_fingerprints(&proof, &proof)?;
        let proof_key = (base_sha.to_owned(), shard.to_owned(), jobs);
        if sha == base_sha {
            // The probe proves the base executor; a candidate compares
            // against it before either side's shard run finishes.
            self.base_proofs.insert(proof_key, proof);
        } else {
            let base = self
                .base_proofs
                .get(&proof_key)
                .ok_or("remote_missing_evidence: no remote base fingerprint for candidate shard")?;
            compare_host_fingerprints(base, &proof)?;
        }
        Ok(ShardRun {
            config: self.config.clone(),
            root,
            sha: sha.to_owned(),
            fingerprint_source_sha: fingerprint_source_sha.to_owned(),
            shard: shard.to_owned(),
            jobs,
            fingerprint: remote_fingerprint,
            command: command.clone(),
        })
    }
}

/// One proven remote shard execution. It carries only transport settings and
/// the remote paths, so runs for different shards and sides may execute
/// concurrently on the gate host (#970). Concurrent runs of one commit share
/// its `CARGO_TARGET_DIR`: Cargo serializes their builds while the tests run
/// in parallel, and `REMOTE_RUNNER` keeps rsid artifacts so no run cleans another
/// run's binaries.
#[derive(Clone, Debug)]
pub(super) struct ShardRun {
    config: Config,
    root: String,
    sha: String,
    fingerprint_source_sha: String,
    shard: String,
    jobs: u32,
    fingerprint: String,
    command: GuardCommand,
}

impl ShardRun {
    pub fn sha(&self) -> &str {
        &self.sha
    }

    pub fn shard(&self) -> &str {
        &self.shard
    }

    pub fn execute(&self) -> Result<GuardCommandReport, String> {
        let script = format!(
            "set -euo pipefail\nhome=$(getent passwd {user} | cut -d: -f6)\nsudo -n -H -u {user} env PATH=\"{root}/bin:$home/.cargo/bin:/usr/local/bin:/usr/bin:/bin\" CARGO_TARGET_DIR='{root}/targets/{sha}' CARGO_BUILD_JOBS='{jobs}' CARGO_PROFILE_DEV_DEBUG=line-tables-only python3.11 - '{sha}' '{shard}' '{fingerprint}' '{root}/worktrees/{sha}' '{root}/worktrees/{fingerprint_source_sha}/scripts/rolling-shard-fingerprint.py' '{jobs}' '{timeout}' <<'PY'\n{runner}\nPY\n",
            user = self.config.run_as,
            root = self.root,
            sha = self.sha,
            shard = self.shard,
            fingerprint = self.fingerprint,
            fingerprint_source_sha = self.fingerprint_source_sha,
            jobs = self.jobs,
            timeout = self.command.timeout.as_secs(),
            runner = REMOTE_RUNNER,
        );
        let started = Instant::now();
        let output = ssh_script(&self.config, &script, "remote_missing_evidence")?;
        parse_result(
            &output.stdout,
            &self.sha,
            &self.shard,
            &self.fingerprint,
            &self.command,
            started.elapsed(),
        )
    }
}

impl Drop for Executor {
    fn drop(&mut self) {
        let root = self.root();
        let staging = format!("/tmp/rsi-gate-{}", self.run_id);
        let cleanup = format!(
            "sudo -n -H -u {user} rm -rf -- '{root}'\nrm -rf -- '{staging}'\n",
            user = self.config.run_as
        );
        let _ = self.ssh_script(&cleanup, "remote_cleanup_failed");
    }
}

fn ssh(config: &Config) -> Command {
    let mut cmd = Command::new("/usr/bin/ssh");
    cmd.args([
        "-F",
        "/dev/null",
        "-T",
        "-o",
        "BatchMode=yes",
        "-o",
        "IdentitiesOnly=yes",
        "-o",
        "StrictHostKeyChecking=yes",
        "-o",
        "ConnectTimeout=10",
        "-o",
        "ServerAliveInterval=15",
        "-o",
        "ServerAliveCountMax=2",
        "-o",
        "ForwardAgent=no",
        "-i",
    ])
    .arg(&config.identity)
    .arg(&config.target)
    .args(["/bin/bash", "-s"]);
    scrub_transport_env(&mut cmd);
    cmd
}

fn ssh_script(config: &Config, script: &str, reason: &str) -> Result<Output, String> {
    let mut child = ssh(config)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| format!("remote_unreachable: {error}"))?;
    child
        .stdin
        .take()
        .ok_or("remote_unreachable: SSH stdin unavailable")?
        .write_all(script.as_bytes())
        .map_err(|error| format!("remote_unreachable: {error}"))?;
    let output = child
        .wait_with_output()
        .map_err(|error| format!("remote_unreachable: {error}"))?;
    if !output.status.success() {
        let code = ssh_failure_code(reason, output.status.code());
        return Err(format!(
            "{code}: SSH exit {:?}: {}",
            output.status.code(),
            String::from_utf8_lossy(&output.stderr)
        ));
    }
    Ok(output)
}

fn ssh_failure_code<'a>(reason: &'a str, exit_code: Option<i32>) -> &'a str {
    if exit_code == Some(255) {
        "remote_unreachable"
    } else if reason == "remote_sha_mismatch" && exit_code == Some(42) {
        "remote_sha_mismatch"
    } else {
        reason
    }
}

fn scrub_transport_env(command: &mut Command) {
    let home = std::env::var_os("HOME");
    command.env_clear().env("PATH", "/usr/bin:/bin");
    if let Some(home) = home {
        command.env("HOME", home);
    }
}

fn git(repo: &Path, args: &[&str]) -> Result<(), String> {
    let output = Command::new("git")
        .args(args)
        .current_dir(repo)
        .output()
        .map_err(|error| format!("remote_bundle_failed: {error}"))?;
    if output.status.success() {
        Ok(())
    } else {
        Err(format!(
            "remote_bundle_failed: {}",
            String::from_utf8_lossy(&output.stderr)
        ))
    }
}

#[derive(Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum Wire {
    Result {
        sha: String,
        shard: String,
        fingerprint: String,
        exit_code: i32,
        stdout: String,
        stderr: String,
    },
    Error {
        code: String,
        #[serde(default)]
        actual: Option<String>,
    },
}

fn parse_result(
    bytes: &[u8],
    sha: &str,
    shard: &str,
    fingerprint: &str,
    command: &GuardCommand,
    duration: Duration,
) -> Result<GuardCommandReport, String> {
    if bytes.len() > 2 * MAX_OUTPUT_BYTES {
        return Err("remote_missing_evidence: wire result exceeded capture bound".into());
    }
    let wire: Wire = serde_json::from_slice(bytes)
        .map_err(|error| format!("remote_missing_evidence: invalid JSON: {error}"))?;
    let (got_sha, got_shard, got_fingerprint, code, stdout, stderr) = match wire {
        Wire::Error { code, actual } => {
            return Err(match code.as_str() {
                "sha_mismatch" => "remote_sha_mismatch: executor refused result".into(),
                "fingerprint_mismatch" => format!(
                    "remote_fingerprint_mismatch: executor digest {} differs from expected {}",
                    actual
                        .filter(|value| valid_fingerprint(value))
                        .unwrap_or_else(|| "unavailable".into()),
                    fingerprint
                ),
                "fingerprint_script_failed" => {
                    "remote_fingerprint_mismatch: executor fingerprint script failed".into()
                }
                "timeout" => "remote_timeout: executor timed out".into(),
                _ => "remote_missing_evidence: executor returned an unknown error".into(),
            });
        }
        Wire::Result {
            sha,
            shard,
            fingerprint,
            exit_code,
            stdout,
            stderr,
        } => (sha, shard, fingerprint, exit_code, stdout, stderr),
    };
    if got_sha != sha {
        return Err("remote_sha_mismatch: result commit differs from requested commit".into());
    }
    if got_shard != shard {
        return Err("remote_missing_evidence: result shard differs from requested shard".into());
    }
    if got_fingerprint != fingerprint {
        return Err(
            "remote_fingerprint_mismatch: result differs from remote probe fingerprint".into(),
        );
    }
    if !(0..=255).contains(&code) {
        return Err("remote_missing_evidence: shard ended without an exit code".into());
    }
    if stdout.len() > MAX_OUTPUT_BYTES || stderr.len() > MAX_OUTPUT_BYTES {
        return Err("remote_missing_evidence: shard output exceeded capture bound".into());
    }
    Ok(GuardCommandReport {
        program: command.program.clone(),
        args: command.args.clone(),
        status: if code == 0 {
            GuardStatus::Passed
        } else {
            GuardStatus::Failed { code: Some(code) }
        },
        stdout_tail: stdout,
        stderr_tail: stderr,
        output_truncated: false,
        duration,
    })
}

const REMOTE_RUNNER: &str = r#"import json, os, subprocess, sys
sha, shard, expected, worktree, fingerprint_script, jobs, timeout = sys.argv[1:]
def emit(value):
    print(json.dumps(value, sort_keys=True))
def check_sha():
    return subprocess.check_output(['git', 'rev-parse', 'HEAD'], cwd=worktree, text=True).strip()
if check_sha() != sha or subprocess.check_output(['git', 'status', '--porcelain=v1'], cwd=worktree, text=True).strip():
    emit({'kind': 'error', 'code': 'sha_mismatch'})
    sys.exit(0)
actual = subprocess.run(['python3.11', fingerprint_script, '--sha', sha, '--shard', shard, '--jobs', jobs], cwd=worktree, text=True, capture_output=True)
if actual.returncode != 0:
    emit({'kind': 'error', 'code': 'fingerprint_script_failed'})
    sys.exit(0)
if actual.stdout.strip() != expected:
    emit({'kind': 'error', 'code': 'fingerprint_mismatch', 'actual': actual.stdout.strip()})
    sys.exit(0)
try:
    result = subprocess.run(['scripts/run-rsid-test-shards.sh', 'shard', shard, '--jobs', jobs, '--keep-rsid-artifacts'], cwd=worktree, text=True, errors='replace', capture_output=True, timeout=int(timeout))
except subprocess.TimeoutExpired:
    emit({'kind': 'error', 'code': 'timeout'})
    sys.exit(0)
if check_sha() != sha:
    emit({'kind': 'error', 'code': 'sha_mismatch'})
else:
    emit({'kind': 'result', 'sha': sha, 'shard': shard, 'fingerprint': actual.stdout.strip(), 'exit_code': result.returncode, 'stdout': result.stdout, 'stderr': result.stderr})"#;

#[cfg(test)]
mod tests {
    use super::*;

    fn command() -> GuardCommand {
        GuardCommand {
            program: "scripts/run-rsid-test-shards.sh".into(),
            args: vec![
                "shard".into(),
                "store-01".into(),
                "--jobs".into(),
                "4".into(),
            ],
            timeout: Duration::from_secs(1200),
        }
    }

    fn config() -> Config {
        Config {
            target: "ec2-user@example.com".into(),
            workdir: "/srv/rsi/gates".into(),
            identity: std::env::current_exe().expect("test executable exists"),
            run_as: "rsi".into(),
        }
    }

    fn proof(host: &str) -> ShardFingerprint {
        serde_json::from_value(serde_json::json!({
            "digest": "a".repeat(64),
            "inputs": {
                "rustc": "rustc 1.94.1", "cargo": "cargo 1.94.1", "nextest": "nextest 0.9.137",
                "target_triple": "x86_64-unknown-linux-gnu", "host_class": host,
                "feature": "test-shard-store-01", "jobs": 4, "test_threads": 4,
                "runner_blob": "b".repeat(40), "checker_blob": "c".repeat(40),
                "nextest_config_blob": "d".repeat(40)
            }
        }))
        .unwrap()
    }

    /// #970: concurrent shard runs of one commit share its target dir, so
    /// the remote runner must never clean rsid artifacts another run uses.
    #[test]
    fn remote_runner_keeps_rsid_artifacts_for_concurrent_shards() {
        assert!(REMOTE_RUNNER.contains("'--keep-rsid-artifacts'"));
    }

    #[test]
    fn remote_base_proof_accepts_changed_runner_blob_on_same_host() {
        let base = proof("ec2-host");
        let mut candidate = proof("ec2-host");
        candidate
            .inputs
            .insert("runner_blob".into(), Value::String("e".repeat(40)));
        assert!(compare_host_fingerprints(&base, &candidate).is_ok());
    }

    #[test]
    fn remote_base_proof_refuses_host_or_toolchain_drift() {
        let base = proof("ec2-host");
        let changed_host = proof("another-host");
        assert!(
            compare_host_fingerprints(&base, &changed_host)
                .unwrap_err()
                .contains("host_class")
        );
        let mut changed_rustc = proof("ec2-host");
        changed_rustc
            .inputs
            .insert("rustc".into(), Value::String("rustc 1.98.0".into()));
        assert!(
            compare_host_fingerprints(&base, &changed_rustc)
                .unwrap_err()
                .contains("rustc")
        );
        let mut missing_blob = proof("ec2-host");
        missing_blob.inputs.remove("runner_blob");
        assert!(compare_host_fingerprints(&base, &missing_blob).is_err());
    }

    #[test]
    fn config_rejects_shell_metacharacters_and_parent_traversal() {
        assert!(config().validate().is_ok());
        let mut bad = config();
        bad.target = "ec2-user@host;true".into();
        assert!(bad.validate().is_err());
        let mut bad = config();
        bad.workdir = "/srv/rsi/../tmp".into();
        assert!(bad.validate().is_err());
    }

    #[test]
    fn remote_result_requires_exact_sha_and_fingerprint() {
        let sha = "a".repeat(40);
        let fingerprint = format!("sha256:{}", "b".repeat(64));
        let result = serde_json::json!({"kind":"result","sha":sha,"shard":"store-01","fingerprint":fingerprint,"exit_code":100,"stdout":"FAIL [  1.00s] rsid test_name","stderr":""});
        let bytes = serde_json::to_vec(&result).unwrap();
        let report = parse_result(
            &bytes,
            &sha,
            "store-01",
            &fingerprint,
            &command(),
            Duration::from_secs(1),
        )
        .unwrap();
        assert_eq!(report.status, GuardStatus::Failed { code: Some(100) });
        assert!(
            parse_result(
                &bytes,
                &"c".repeat(40),
                "store-01",
                &fingerprint,
                &command(),
                Duration::ZERO
            )
            .unwrap_err()
            .contains("remote_sha_mismatch")
        );
        assert!(
            parse_result(
                &bytes,
                &sha,
                "store-01",
                &format!("sha256:{}", "d".repeat(64)),
                &command(),
                Duration::ZERO
            )
            .unwrap_err()
            .contains("remote_fingerprint_mismatch")
        );
    }

    #[test]
    fn remote_missing_or_unavailable_result_refuses_publication_evidence() {
        let sha = "a".repeat(40);
        let fingerprint = format!("sha256:{}", "b".repeat(64));
        assert!(
            parse_result(
                b"",
                &sha,
                "store-01",
                &fingerprint,
                &command(),
                Duration::ZERO
            )
            .unwrap_err()
            .contains("remote_missing_evidence")
        );
        let denied = br#"{"kind":"error","code":"timeout"}"#;
        assert!(
            parse_result(
                denied,
                &sha,
                "store-01",
                &fingerprint,
                &command(),
                Duration::ZERO
            )
            .unwrap_err()
            .contains("remote_timeout")
        );
        assert_eq!(
            ssh_failure_code("remote_setup_failed", Some(255)),
            "remote_unreachable"
        );
    }

    #[test]
    #[ignore = "requires an explicitly configured SSH executor"]
    fn exact_bundle_remote_shard_smoke() {
        let host = std::env::var("RSI_REMOTE_GATE_SMOKE_HOST").expect("smoke host");
        let identity = PathBuf::from(
            std::env::var("RSI_REMOTE_GATE_SMOKE_IDENTITY").expect("smoke SSH identity"),
        );
        let repo = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
        let sha = std::env::var("RSI_REMOTE_GATE_SMOKE_SHA").expect("exact smoke SHA");
        let shard = "other-01";
        let jobs = 4;
        let tmp = tempfile::tempdir().unwrap();
        let private_repo = tmp.path().join("repo");
        let cloned = Command::new("git")
            .args(["clone", "--quiet", "--no-checkout", "--no-hardlinks"])
            .arg(&repo)
            .arg(&private_repo)
            .status()
            .expect("private clone");
        assert!(cloned.success());
        let mut executor = Executor::new(
            Config {
                target: host,
                workdir: "/srv/rsi/remote-gate-smoke".into(),
                identity,
                run_as: "rsi".into(),
            },
            private_repo,
        )
        .expect("valid executor");
        let report = executor
            .run_full_shard(
                &sha,
                &sha,
                &sha,
                shard,
                jobs,
                &GuardCommand {
                    program: "scripts/run-rsid-test-shards.sh".into(),
                    args: vec!["shard".into(), shard.into(), "--jobs".into(), "4".into()],
                    timeout: Duration::from_secs(1800),
                },
            )
            .expect("exact remote result");
        let _failures = crate::observed_test_failures(&report).expect("remote test evidence");
        assert_eq!(report.program, "scripts/run-rsid-test-shards.sh");
        assert!(matches!(
            report.status,
            GuardStatus::Passed | GuardStatus::Failed { .. }
        ));
    }
}
