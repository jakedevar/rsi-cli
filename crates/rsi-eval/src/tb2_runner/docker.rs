//! Remote Docker for one pinned Terminal-Bench 2 trial (#3679 R2, design #3628 "Harbor parity
//! contract" rows on image, Compose, limits, log binds and cleanup, and "Architecture and
//! transport" items 2-5).
//!
//! The hub runs `docker -H ssh://<host> ...`; the Docker daemon resolves bind sources on the
//! laptop. [`RemoteEnvironment`] implements R1's [`TrialEnvironment`] over it:
//!
//! * the `main` service runs the digest-pinned `repo@sha256:...` image (never a tag, never a
//!   build, never a silent pull) with `sh -c 'exec sleep infinity'`, exact CPU and memory limits and
//!   `/logs/{verifier,agent,artifacts}` bound to laptop directories made writable;
//! * readiness is checked, not assumed: exactly one running `main` container with the trial's
//!   Compose and `rsi.bench.*` labels, the planned image digest, the planned limits and binds;
//! * teardown is scoped to the trial's own Compose project (`down --volumes --remove-orphans`)
//!   and the trial's own log directory; there is no global prune and no image removal.

use std::{
    fs,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

use rsi_common::bench_runner::{
    CANCEL_SETTLE_TIMEOUT_SEC, CLEANUP_TIMEOUT_SEC, COPY_TIMEOUT_SEC, PlannedTrial, TrialIdentity,
};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use super::{
    transport::{
        CommandExecutor, CommandOutput, CommandSpec, ExecError, OpError, OpKind, Remote,
        posix_quote, validate_remote_path,
    },
    trial::{EnvError, EnvFacts, ExecSpec, TrialEnvironment},
};

/// The only Compose service.
pub const SERVICE: &str = "main";
pub const LABEL_PROJECT: &str = "com.docker.compose.project";
pub const LABEL_SERVICE: &str = "com.docker.compose.service";
pub const LABEL_RUN: &str = "rsi.bench.run";
pub const LABEL_TASK: &str = "rsi.bench.task";
pub const LABEL_SLOT: &str = "rsi.bench.slot";
pub const LABEL_RETRY: &str = "rsi.bench.retry";
/// Every pinned image runs linux/amd64.
pub const PLATFORM: &str = "linux/amd64";
/// Exit status the reader script uses for "no such file".
const MISSING_FILE_EXIT: i32 = 3;

// ---- naming --------------------------------------------------------------------------------

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write;
    bytes.iter().fold(String::new(), |mut text, byte| {
        let _ = write!(text, "{byte:02x}");
        text
    })
}

/// First 12 hex digits of the SHA-256 of the NUL-joined parts.
#[must_use]
pub fn short_hash(parts: &[&str]) -> String {
    let mut digest = Sha256::new();
    for part in parts {
        digest.update(part.as_bytes());
        digest.update([0]);
    }
    hex(&digest.finalize())[..12].to_string()
}

/// A stable directory key of one trial attempt (the shared definition, so the record layer's
/// join key can never drift from the directory name).
#[must_use]
pub fn trial_key(identity: &TrialIdentity) -> String {
    rsi_common::bench_runner::trial_key(identity)
}

/// The Compose project and laptop directories of one trial attempt. The project name is unique
/// per `(run, task, slot, retry)`, so teardown by project can never touch another trial.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrialNames {
    pub key: String,
    pub project: String,
    pub trial_dir: String,
    pub agent_dir: String,
    pub verifier_dir: String,
    pub artifacts_dir: String,
    /// The run-owned root every trial directory sits under; a removal is refused outside it.
    pub run_root: String,
}

impl TrialNames {
    /// # Errors
    /// [`OpKind::UnsafePath`] when `run_root` is not a safe absolute remote path.
    pub fn new(identity: &TrialIdentity, run_root: &str) -> Result<Self, OpError> {
        validate_remote_path(run_root)?;
        let key = trial_key(identity);
        let trial_dir = format!("{}/{key}", run_root.trim_end_matches('/'));
        validate_remote_path(&trial_dir)?;
        let slot = identity.slot.to_string();
        let retry = identity.retry.to_string();
        Ok(Self {
            project: format!(
                "rsi-tb2-{}",
                short_hash(&[&identity.run_id, &identity.task, &slot, &retry])
            ),
            agent_dir: format!("{trial_dir}/agent"),
            verifier_dir: format!("{trial_dir}/verifier"),
            artifacts_dir: format!("{trial_dir}/artifacts"),
            run_root: run_root.trim_end_matches('/').to_string(),
            trial_dir,
            key,
        })
    }
}

/// `repo@sha256:...` for a pinned image: the tag is dropped, so only the digest selects bytes.
///
/// # Errors
/// [`OpKind::InvalidPlan`] for an empty image or a malformed digest.
pub fn pinned_ref(image: &str, digest: &str) -> Result<String, OpError> {
    let well_formed = digest.strip_prefix("sha256:").is_some_and(|h| {
        h.len() == 64
            && h.bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    });
    if !well_formed {
        return Err(OpError::new(
            OpKind::InvalidPlan,
            format!("image digest {digest:?} is not sha256:<64 lowercase hex>"),
        ));
    }
    let name = image.split('@').next().unwrap_or_default();
    let slash = name.rfind('/').map_or(0, |i| i + 1);
    let repo = name[slash..]
        .rfind(':')
        .map_or(name, |colon| &name[..slash + colon]);
    if repo.is_empty() || repo.contains(char::is_whitespace) {
        return Err(OpError::new(
            OpKind::InvalidPlan,
            format!("image {image:?} has no repository"),
        ));
    }
    Ok(format!("{repo}@{digest}"))
}

/// The Compose file of one trial, as JSON (a YAML subset). Exact limits, digest-pinned image,
/// no build, no pull, no GPU, the image entrypoint retained.
///
/// # Errors
/// [`OpKind::InvalidPlan`] for a bad image or digest.
pub fn compose_config(trial: &PlannedTrial, names: &TrialNames) -> Result<Value, OpError> {
    let id = &trial.identity;
    let bind =
        |source: &str, target: &str| json!({"type": "bind", "source": source, "target": target});
    Ok(json!({
        "name": names.project,
        "services": {
            SERVICE: {
                "image": pinned_ref(&trial.image, &trial.image_digest)?,
                "platform": PLATFORM,
                "pull_policy": "never",
                // A waiting PID-1 shell would exit when settlement kills its sleep child,
                // tearing down the PID namespace and SIGKILLing the settlement exec (#3894).
                "command": ["sh", "-c", "exec sleep infinity"],
                "labels": {
                    LABEL_RUN: id.run_id,
                    LABEL_TASK: id.task,
                    LABEL_SLOT: id.slot.to_string(),
                    LABEL_RETRY: id.retry.to_string(),
                },
                "deploy": {"resources": {"limits": {
                    "cpus": trial.cpus.to_string(),
                    "memory": format!("{}M", trial.memory_mb),
                }}},
                "volumes": [
                    bind(&names.verifier_dir, "/logs/verifier"),
                    bind(&names.agent_dir, "/logs/agent"),
                    bind(&names.artifacts_dir, "/logs/artifacts"),
                ],
            }
        }
    }))
}

// ---- docker client -------------------------------------------------------------------------

/// Stderr fragments that mean the daemon or the tunnel to it is gone, not that a command failed.
const UNREACHABLE: [&str; 8] = [
    "Cannot connect to the Docker daemon",
    "error during connect",
    "connection refused",
    "Connection closed",
    "Connection reset",
    "Broken pipe",
    "Host key verification failed",
    "dial-stdio",
];

fn is_unreachable(stderr: &[u8]) -> bool {
    let text = String::from_utf8_lossy(stderr);
    UNREACHABLE.iter().any(|needle| text.contains(needle))
}

/// A failed Docker command as a typed error: a lost daemon or tunnel is
/// [`OpKind::DockerUnreachable`], anything else `kind`.
fn failure(kind: OpKind, what: &str, output: &CommandOutput) -> OpError {
    let kind = if is_unreachable(&output.stderr) {
        OpKind::DockerUnreachable
    } else {
        kind
    };
    OpError::new(
        kind,
        format!(
            "{what} exited {}: {}",
            output
                .code
                .map_or_else(|| "by signal".to_string(), |c| c.to_string()),
            output.stderr_tail()
        ),
    )
}

/// Facts from `docker info`.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct DockerFacts {
    pub os_type: String,
    pub architecture: String,
    pub server_version: String,
    pub root_dir: String,
    pub compose_version: String,
}

/// `docker -H <host> ...` through a [`CommandExecutor`].
pub struct DockerClient<'a> {
    pub exec: &'a dyn CommandExecutor,
    pub host: &'a str,
}

impl DockerClient<'_> {
    /// The argv of one Docker command: the explicit host first, so no ambient context applies.
    #[must_use]
    pub fn spec(&self, args: &[&str], timeout: Duration) -> CommandSpec {
        let mut argv = vec!["-H".to_string(), self.host.to_string()];
        argv.extend(args.iter().map(ToString::to_string));
        CommandSpec::new("docker", argv, timeout)
    }

    /// Run one Docker command and return its output whatever its exit status.
    ///
    /// # Errors
    /// [`OpKind::Timeout`] after the command was killed, or [`OpKind::DockerFailed`] when
    /// `docker` cannot start.
    pub fn run(&self, args: &[&str], timeout: Duration) -> Result<CommandOutput, OpError> {
        self.exec
            .run(&self.spec(args, timeout))
            .map_err(|error| match error {
                ExecError::Timeout => OpError::new(
                    OpKind::Timeout,
                    format!(
                        "docker {} ran past {}s",
                        args.first().copied().unwrap_or_default(),
                        timeout.as_secs()
                    ),
                ),
                ExecError::Spawn(why) => OpError::new(OpKind::DockerFailed, why),
            })
    }

    /// [`DockerClient::run`], with a nonzero exit as an error of `kind`.
    ///
    /// # Errors
    /// As [`DockerClient::run`], or `kind` (or unreachable) for a nonzero exit.
    pub fn must(
        &self,
        args: &[&str],
        timeout: Duration,
        kind: OpKind,
    ) -> Result<CommandOutput, OpError> {
        let output = self.run(args, timeout)?;
        if output.success() {
            Ok(output)
        } else {
            Err(failure(
                kind,
                &format!("docker {}", args.first().copied().unwrap_or_default()),
                &output,
            ))
        }
    }

    /// Daemon facts and Compose v2 presence.
    ///
    /// # Errors
    /// Unreachable daemon, missing Compose v2, or an unparsable answer.
    pub fn facts(&self, timeout: Duration) -> Result<DockerFacts, OpError> {
        let info = self.must(
            &[
                "info",
                "--format",
                "{{.OSType}} {{.Architecture}} {{.ServerVersion}} {{.DockerRootDir}}",
            ],
            timeout,
            OpKind::DockerFailed,
        )?;
        let text = info.stdout_text();
        let mut fields = text.trim().splitn(4, ' ');
        let (Some(os_type), Some(architecture), Some(server_version), Some(root_dir)) =
            (fields.next(), fields.next(), fields.next(), fields.next())
        else {
            return Err(OpError::new(
                OpKind::DockerFailed,
                format!("unparsable docker info {:?}", text.trim()),
            ));
        };
        let compose = self
            .must(
                &["compose", "version", "--short"],
                timeout,
                OpKind::ComposeUnavailable,
            )
            .map_err(|mut error| {
                if error.kind == OpKind::DockerFailed {
                    error.kind = OpKind::ComposeUnavailable;
                }
                error
            })?;
        let version = compose
            .stdout_text()
            .trim()
            .trim_start_matches('v')
            .to_string();
        let major = version
            .split('.')
            .next()
            .and_then(|m| m.parse::<u32>().ok());
        if major.is_none_or(|m| m < 2) {
            return Err(OpError::new(
                OpKind::ComposeUnavailable,
                format!("docker compose {version:?} is not v2 or newer"),
            ));
        }
        Ok(DockerFacts {
            os_type: os_type.to_string(),
            architecture: architecture.to_string(),
            server_version: server_version.to_string(),
            root_dir: root_dir.to_string(),
            compose_version: version,
        })
    }

    /// `RepoDigests` of a local image; `None` when the image is not present.
    ///
    /// # Errors
    /// Docker failures other than a missing image.
    pub fn repo_digests(
        &self,
        reference: &str,
        timeout: Duration,
    ) -> Result<Option<Vec<String>>, OpError> {
        let output = self.run(
            &[
                "image",
                "inspect",
                "--format",
                "{{json .RepoDigests}}",
                reference,
            ],
            timeout,
        )?;
        crate::tb2::pin::parse_digests(
            reference,
            output.success(),
            &output.stdout_text(),
            &String::from_utf8_lossy(&output.stderr),
        )
        .map_err(|error| {
            if is_unreachable(&output.stderr) {
                OpError::new(OpKind::DockerUnreachable, error.to_string())
            } else {
                OpError::new(OpKind::DockerFailed, error.to_string())
            }
        })
    }

    /// Pull one pinned image by digest for linux/amd64, then prove the local copy is exactly
    /// that digest and platform. A failed pull never falls back to a build or a tag.
    ///
    /// # Errors
    /// [`OpKind::ImagePullFailed`] or [`OpKind::DigestMismatch`].
    pub fn pull_pinned(
        &self,
        reference: &str,
        digest: &str,
        timeout: Duration,
    ) -> Result<(), OpError> {
        self.must(
            &["pull", "--quiet", "--platform", PLATFORM, reference],
            timeout,
            OpKind::ImagePullFailed,
        )?;
        let digests = self.repo_digests(reference, timeout)?.ok_or_else(|| {
            OpError::new(
                OpKind::ImagePullFailed,
                format!("{reference} is absent after the pull"),
            )
        })?;
        require_digest(reference, digest, &digests)?;
        let platform = self.must(
            &[
                "image",
                "inspect",
                "--format",
                "{{.Os}}/{{.Architecture}}",
                reference,
            ],
            timeout,
            OpKind::DockerFailed,
        )?;
        if platform.stdout_text().trim() != PLATFORM {
            return Err(OpError::new(
                OpKind::UnsupportedPlatform,
                format!(
                    "{reference} is {} not {PLATFORM}",
                    platform.stdout_text().trim()
                ),
            ));
        }
        Ok(())
    }
}

/// The local `RepoDigests` must contain the pinned digest.
fn require_digest(reference: &str, digest: &str, digests: &[String]) -> Result<(), OpError> {
    let want = format!("@{digest}");
    if digests.iter().any(|d| d.ends_with(&want)) {
        Ok(())
    } else {
        Err(OpError::new(
            OpKind::DigestMismatch,
            format!("{reference}: pinned {digest}, local RepoDigests {digests:?}"),
        ))
    }
}

// ---- container verification ----------------------------------------------------------------

/// Check an `inspect` document of the running `main` container against the trial's plan:
/// running, the Compose and RSI identity labels, exact CPU and memory limits, and the three
/// log binds.
///
/// # Errors
/// [`OpKind::NotReady`], [`OpKind::LabelMismatch`] or [`OpKind::LimitMismatch`].
pub fn verify_container(
    inspect: &Value,
    trial: &PlannedTrial,
    names: &TrialNames,
) -> Result<(), OpError> {
    if inspect.pointer("/State/Running") != Some(&Value::Bool(true)) {
        return Err(OpError::new(
            OpKind::NotReady,
            "the main container is not running",
        ));
    }
    let id = &trial.identity;
    let expected = [
        (LABEL_PROJECT, names.project.clone()),
        (LABEL_SERVICE, SERVICE.to_string()),
        (LABEL_RUN, id.run_id.clone()),
        (LABEL_TASK, id.task.clone()),
        (LABEL_SLOT, id.slot.to_string()),
        (LABEL_RETRY, id.retry.to_string()),
    ];
    for (key, want) in &expected {
        let got = inspect
            .pointer("/Config/Labels")
            .and_then(|labels| labels.get(*key))
            .and_then(Value::as_str);
        if got != Some(want.as_str()) {
            return Err(OpError::new(
                OpKind::LabelMismatch,
                format!("label {key} is {got:?}, expected {want:?}"),
            ));
        }
    }
    let nano_cpus = trial.cpus.checked_mul(1_000_000_000);
    let memory = trial.memory_mb.checked_mul(1 << 20);
    for (what, got, want) in [
        (
            "NanoCpus",
            inspect
                .pointer("/HostConfig/NanoCpus")
                .and_then(Value::as_u64),
            nano_cpus,
        ),
        (
            "Memory",
            inspect
                .pointer("/HostConfig/Memory")
                .and_then(Value::as_u64),
            memory,
        ),
    ] {
        if got != want || want.is_none() {
            return Err(OpError::new(
                OpKind::LimitMismatch,
                format!("{what} is {got:?}, planned {want:?}"),
            ));
        }
    }
    let mounts = inspect
        .get("Mounts")
        .and_then(Value::as_array)
        .map_or(&[][..], Vec::as_slice);
    for (source, target) in [
        (&names.verifier_dir, "/logs/verifier"),
        (&names.agent_dir, "/logs/agent"),
        (&names.artifacts_dir, "/logs/artifacts"),
    ] {
        let bound = mounts.iter().any(|m| {
            m.get("Type").and_then(Value::as_str) == Some("bind")
                && m.get("Source").and_then(Value::as_str) == Some(source.as_str())
                && m.get("Destination").and_then(Value::as_str) == Some(target)
                && m.get("RW").and_then(Value::as_bool) != Some(false)
        });
        if !bound {
            return Err(OpError::new(
                OpKind::LimitMismatch,
                format!("{target} is not bound read-write to {source}"),
            ));
        }
    }
    Ok(())
}

/// Stop every process but PID 1 and this exec's ancestor chain, then prove none is left
/// (zombies excepted). The keepalive must exec its sleep so it cannot exit when a child dies.
/// Killing the `docker exec` client alone does not stop what it started in the container.
pub const SETTLE_SCRIPT: &str = r#"me=$$
keep=" 1 "
p=$me
while [ "$p" -gt 1 ] 2>/dev/null; do
  case "$keep" in *" $p "*) break;; esac
  keep="$keep$p "
  parent=0
  while read -r k v _; do
    if [ "$k" = "PPid:" ]; then parent=$v; break; fi
  done < "/proc/$p/status" 2>/dev/null
  p=$parent
done
i=0
while [ "$i" -lt 20 ]; do
  left=0
  for d in /proc/[0-9]*; do
    p=${d#/proc/}
    case "$keep" in *" $p "*) continue;; esac
    st=
    while read -r k v _; do
      if [ "$k" = "State:" ]; then st=$v; break; fi
    done < "$d/status" 2>/dev/null
    [ -z "$st" ] && continue
    [ "$st" = Z ] && continue
    kill -9 "$p" 2>/dev/null
    left=1
  done
  [ "$left" = 0 ] && exit 0
  i=$((i+1))
  sleep 0.2 2>/dev/null || sleep 1
done
exit 1"#;

/// Reads `$1`: prints it, or exits with [`MISSING_FILE_EXIT`] when it is not a file.
const READ_SCRIPT: &str = r#"if [ -f "$1" ]; then cat -- "$1"; else exit 3; fi"#;

/// Empties the three mounted log directories (the mount points stay), as root, so the laptop
/// user can remove the run directory afterwards.
const EMPTY_LOGS_SCRIPT: &str =
    "find /logs/agent /logs/verifier /logs/artifacts -mindepth 1 -delete";

// ---- the environment -----------------------------------------------------------------------

/// Where one trial runs.
#[derive(Clone, Copy)]
pub struct EnvContext<'a> {
    pub exec: &'a dyn CommandExecutor,
    /// `ssh://<host>`.
    pub docker_host: &'a str,
    /// The ssh destination (`bench.laptop_host`).
    pub ssh_host: &'a str,
}

/// R1's [`TrialEnvironment`] over remote Docker.
pub struct RemoteEnvironment<'a> {
    ctx: EnvContext<'a>,
    trial: &'a PlannedTrial,
    names: TrialNames,
    compose_path: PathBuf,
    container: Option<String>,
    dirs_created: bool,
    compose_attempted: bool,
    collected: bool,
    /// Set when cleanup kept the laptop log directory because collection did not succeed.
    pub kept_evidence: Option<String>,
}

fn budget(deadline: Instant, cap: Duration) -> Result<Duration, OpError> {
    let left = deadline.saturating_duration_since(Instant::now());
    if left.is_zero() {
        Err(OpError::new(
            OpKind::Timeout,
            "the operation deadline passed",
        ))
    } else {
        Ok(left.min(cap))
    }
}

impl<'a> RemoteEnvironment<'a> {
    /// `compose_path` is where this trial's Compose file is written on the hub.
    ///
    /// # Errors
    /// [`OpKind::UnsafePath`] for an unusable remote run root.
    pub fn new(
        ctx: EnvContext<'a>,
        trial: &'a PlannedTrial,
        remote_run_root: &str,
        compose_path: PathBuf,
    ) -> Result<Self, OpError> {
        Ok(Self {
            names: TrialNames::new(&trial.identity, remote_run_root)?,
            ctx,
            trial,
            compose_path,
            container: None,
            dirs_created: false,
            compose_attempted: false,
            collected: false,
            kept_evidence: None,
        })
    }

    #[must_use]
    pub const fn names(&self) -> &TrialNames {
        &self.names
    }

    /// Take over what a previous runner process journaled (R5): the container, and whether the
    /// log directories, the project and the collected evidence exist. A restarted runner then
    /// collects from and cleans up exactly what the dead one created.
    pub fn adopt(&mut self, facts: &EnvFacts) {
        self.container.clone_from(&facts.container);
        self.dirs_created = facts.dirs_created;
        self.compose_attempted = facts.compose_attempted;
        self.collected = facts.collected;
    }

    fn docker(&self) -> DockerClient<'a> {
        DockerClient {
            exec: self.ctx.exec,
            host: self.ctx.docker_host,
        }
    }

    fn remote(&self) -> Remote<'a> {
        Remote {
            exec: self.ctx.exec,
            host: self.ctx.ssh_host,
        }
    }

    fn compose_file(&self) -> Result<&str, OpError> {
        self.compose_path
            .to_str()
            .ok_or_else(|| OpError::new(OpKind::Io, "the compose path is not valid UTF-8"))
    }

    fn container_id(&self) -> Result<&str, OpError> {
        self.container
            .as_deref()
            .ok_or_else(|| OpError::new(OpKind::NotReady, "no container is running"))
    }

    fn start_inner(&mut self, deadline: Instant) -> Result<(), OpError> {
        let names = self.names.clone();
        let quoted = |p: &str| posix_quote(p);
        let mkdir = format!(
            "mkdir -p -- {trial} && mkdir -p -- {a} {v} {x} && chmod 0777 -- {a} {v} {x}",
            trial = quoted(&names.trial_dir),
            a = quoted(&names.agent_dir),
            v = quoted(&names.verifier_dir),
            x = quoted(&names.artifacts_dir),
        );
        self.dirs_created = true;
        self.remote().must(
            &mkdir,
            budget(deadline, Duration::from_secs(COPY_TIMEOUT_SEC))?,
            OpKind::Io,
        )?;
        let config = compose_config(self.trial, &names)?;
        if let Some(parent) = self.compose_path.parent() {
            fs::create_dir_all(parent)
                .map_err(|e| OpError::new(OpKind::Io, format!("{}: {e}", parent.display())))?;
        }
        fs::write(
            &self.compose_path,
            serde_json::to_vec_pretty(&config)
                .map_err(|e| OpError::new(OpKind::Io, e.to_string()))?,
        )
        .map_err(|e| OpError::new(OpKind::Io, format!("{}: {e}", self.compose_path.display())))?;

        let left = budget(deadline, Duration::MAX)?;
        let wait = left.as_secs().max(1).to_string();
        let file = self.compose_file()?.to_string();
        self.compose_attempted = true;
        self.docker().must(
            &[
                "compose",
                "-p",
                &names.project,
                "-f",
                &file,
                "up",
                "--detach",
                "--wait",
                "--wait-timeout",
                &wait,
                "--no-build",
                "--pull",
                "never",
            ],
            left,
            OpKind::NotReady,
        )?;
        self.verify_ready(deadline)
    }

    fn verify_ready(&mut self, deadline: Instant) -> Result<(), OpError> {
        let docker = self.docker();
        let names = self.names.clone();
        let project_filter = format!("label={LABEL_PROJECT}={}", names.project);
        let service_filter = format!("label={LABEL_SERVICE}={SERVICE}");
        let listed = docker.must(
            &[
                "ps",
                "-q",
                "--no-trunc",
                "--filter",
                &project_filter,
                "--filter",
                &service_filter,
                "--filter",
                "status=running",
            ],
            budget(deadline, Duration::MAX)?,
            OpKind::DockerFailed,
        )?;
        let text = listed.stdout_text();
        let ids: Vec<&str> = text.split_whitespace().collect();
        let id = match ids.as_slice() {
            [] => {
                return Err(OpError::new(
                    OpKind::NotReady,
                    format!("no running {SERVICE} container for {}", names.project),
                ));
            }
            [one] if one.bytes().all(|b| b.is_ascii_hexdigit()) => (*one).to_string(),
            [_] => {
                return Err(OpError::new(
                    OpKind::LabelMismatch,
                    "docker ps returned a malformed container id",
                ));
            }
            many => {
                return Err(OpError::new(
                    OpKind::LabelMismatch,
                    format!(
                        "{} {SERVICE} containers match {}",
                        many.len(),
                        names.project
                    ),
                ));
            }
        };
        self.container = Some(id.clone());
        let inspected = docker.must(
            &["inspect", "--format", "{{json .}}", &id],
            budget(deadline, Duration::MAX)?,
            OpKind::DockerFailed,
        )?;
        let document: Value = serde_json::from_slice(&inspected.stdout).map_err(|e| {
            OpError::new(
                OpKind::DockerFailed,
                format!("unparsable docker inspect: {e}"),
            )
        })?;
        verify_container(&document, self.trial, &names)?;
        let image_id = document
            .get("Image")
            .and_then(Value::as_str)
            .ok_or_else(|| OpError::new(OpKind::DigestMismatch, "inspect names no image id"))?;
        let digests = docker
            .repo_digests(image_id, budget(deadline, Duration::MAX)?)?
            .unwrap_or_default();
        require_digest(&self.trial.image, &self.trial.image_digest, &digests)
    }

    /// Best-effort: stop every container process but PID 1 (used when an exec timed out).
    fn stop_processes(&self, timeout: Duration) -> Result<(), OpError> {
        let id = self.container_id()?.to_string();
        let output = self.docker().run(
            &["exec", "-u", "root", &id, "sh", "-c", SETTLE_SCRIPT],
            timeout,
        )?;
        if output.success() {
            Ok(())
        } else {
            Err(failure(OpKind::DockerFailed, "the settle script", &output))
        }
    }

    fn remove_remote_dir(&self, deadline: Instant) -> Result<(), OpError> {
        let dir = &self.names.trial_dir;
        // Defence in depth: only a directory strictly inside the run's own root.
        let inside = dir
            .strip_prefix(&format!("{}/", self.names.run_root))
            .is_some_and(|rest| !rest.is_empty() && !rest.contains('/'));
        if !inside {
            return Err(OpError::new(
                OpKind::UnsafePath,
                format!("refusing to remove {dir}: it is not a trial directory of this run"),
            ));
        }
        self.remote().must(
            &format!("rm -rf -- {}", posix_quote(dir)),
            budget(deadline, Duration::from_secs(CLEANUP_TIMEOUT_SEC))?,
            OpKind::Io,
        )?;
        Ok(())
    }

    fn cleanup_inner(&mut self, deadline: Instant) -> Result<(), OpError> {
        let mut errors: Vec<String> = Vec::new();
        let mut note = |result: Result<(), OpError>| {
            if let Err(error) = result {
                errors.push(error.to_string());
            }
        };
        if self.compose_attempted {
            let names = self.names.clone();
            if self.collected
                && let Some(id) = self.container.clone()
            {
                // Best effort: files written by the container's user cannot be removed by the
                // laptop user, so empty the mounts first, as root.
                let _ = budget(deadline, Duration::from_secs(5)).and_then(|t| {
                    self.docker().run(
                        &["exec", "-u", "root", &id, "sh", "-c", EMPTY_LOGS_SCRIPT],
                        t,
                    )
                });
            }
            note(self.compose_down(&names, deadline));
        }
        if self.dirs_created {
            if self.collected {
                note(self.remove_remote_dir(deadline));
            } else {
                self.kept_evidence = Some(self.names.trial_dir.clone());
            }
        }
        if errors.is_empty() {
            Ok(())
        } else {
            Err(OpError::new(OpKind::DockerFailed, errors.join("; ")))
        }
    }

    /// The Compose file is deterministic; write it again when a crash lost it, so `down` can
    /// still find the project.
    fn ensure_compose_file(&self, names: &TrialNames) -> Result<(), OpError> {
        if self.compose_path.exists() {
            return Ok(());
        }
        let config = compose_config(self.trial, names)?;
        if let Some(parent) = self.compose_path.parent() {
            fs::create_dir_all(parent)
                .map_err(|e| OpError::new(OpKind::Io, format!("{}: {e}", parent.display())))?;
        }
        fs::write(
            &self.compose_path,
            serde_json::to_vec_pretty(&config)
                .map_err(|e| OpError::new(OpKind::Io, e.to_string()))?,
        )
        .map_err(|e| OpError::new(OpKind::Io, format!("{}: {e}", self.compose_path.display())))
    }

    /// Give Compose at most a third of the remaining phase (capped at 30 seconds), reserving
    /// time for force removal and proof for this project's containers, networks and volumes.
    /// A recovered Compose failure is successful cleanup; no prune or image removal.
    fn compose_down(&self, names: &TrialNames, deadline: Instant) -> Result<(), OpError> {
        let docker = self.docker();
        let down = self.ensure_compose_file(names).and_then(|()| {
            let file = self.compose_file()?.to_string();
            let timeout = budget(deadline, Duration::from_secs(90))? / 3;
            docker
                .must(
                    &[
                        "compose",
                        "-p",
                        &names.project,
                        "-f",
                        &file,
                        "down",
                        "--volumes",
                        "--remove-orphans",
                        "--timeout",
                        "10",
                    ],
                    timeout,
                    OpKind::DockerFailed,
                )
                .map(|_| ())
        });
        let filter = format!("label={LABEL_PROJECT}={}", names.project);
        let mut errors = Vec::new();
        // Containers first: this also kills any surviving heavy verifier/agent process and
        // detaches its endpoints before the network removal. IDs come only from exact labels.
        for (kind, list, remove) in [
            (
                "containers",
                vec!["ps", "-aq"],
                vec!["rm", "-f", "--volumes"],
            ),
            (
                "networks",
                vec!["network", "ls", "-q"],
                vec!["network", "rm"],
            ),
            (
                "volumes",
                vec!["volume", "ls", "-q"],
                vec!["volume", "rm", "-f"],
            ),
        ] {
            let list_args: Vec<&str> = list.into_iter().chain(["--filter", &filter]).collect();
            let listed = || {
                docker.must(
                    &list_args,
                    budget(deadline, Duration::from_secs(10))?,
                    OpKind::DockerFailed,
                )
            };
            let result = (|| {
                let objects = listed()?.stdout_text();
                let ids: Vec<&str> = objects.split_whitespace().collect();
                if ids.is_empty() {
                    return Ok(());
                }
                // Fail closed on malformed output rather than interpreting an option.
                if ids.iter().any(|id| {
                    !id.as_bytes()[0].is_ascii_alphanumeric()
                        || !id
                            .bytes()
                            .all(|b| b.is_ascii_alphanumeric() || b"_.-".contains(&b))
                }) {
                    return Err(OpError::new(
                        OpKind::DockerFailed,
                        "malformed owned resource id",
                    ));
                }
                let args: Vec<&str> = remove.into_iter().chain(ids).collect();
                let removed = docker.must(
                    &args,
                    budget(deadline, Duration::from_secs(10))?,
                    OpKind::DockerFailed,
                );
                // Removal can race the timed-out Compose client. The postcondition, not that
                // client's status, decides success (including stopped containers).
                let left = listed()?;
                if !left.stdout_text().trim().is_empty() {
                    return Err(OpError::new(
                        OpKind::DockerFailed,
                        format!(
                            "{kind} of {} remain after cleanup: {}; removal: {removed:?}",
                            names.project,
                            left.stdout_text().trim(),
                        ),
                    ));
                }
                Ok(())
            })();
            if let Err(error) = result {
                errors.push(error.to_string());
            }
        }
        if errors.is_empty() {
            Ok(())
        } else {
            if let Err(error) = down {
                errors.insert(0, error.to_string());
            }
            Err(OpError::new(OpKind::DockerFailed, errors.join("; ")))
        }
    }
}

impl TrialEnvironment for RemoteEnvironment<'_> {
    fn start(&mut self, timeout: Duration) -> Result<(), EnvError> {
        self.start_inner(Instant::now() + timeout)
            .map_err(Into::into)
    }

    fn upload_dir(
        &mut self,
        local: &Path,
        remote: &str,
        timeout: Duration,
    ) -> Result<(), EnvError> {
        validate_remote_path(remote).map_err(EnvError::from)?;
        let source = local
            .to_str()
            .filter(|_| local.is_dir())
            .ok_or_else(|| EnvError::Failed(format!("{} is not a directory", local.display())))?;
        let id = self.container_id().map_err(EnvError::from)?.to_string();
        self.docker()
            .must(
                &["cp", &format!("{source}/."), &format!("{id}:{remote}")],
                timeout,
                OpKind::DockerFailed,
            )
            .map(|_| ())
            .map_err(Into::into)
    }

    fn exec(&mut self, spec: &ExecSpec<'_>) -> Result<i32, EnvError> {
        let id = self.container_id().map_err(EnvError::from)?.to_string();
        let mut args: Vec<String> = vec!["exec".into()];
        if let Some(user) = spec.user {
            args.extend(["-u".into(), user.into()]);
        }
        for (key, value) in spec.env {
            args.extend(["-e".into(), format!("{key}={value}")]);
        }
        args.extend([id, "bash".into(), "-c".into(), spec.script.into()]);
        let refs: Vec<&str> = args.iter().map(String::as_str).collect();
        match self.docker().run(&refs, spec.timeout) {
            Err(error) if error.kind == OpKind::Timeout => {
                // The client is gone; what it started in the container is not.
                let _ = self.stop_processes(Duration::from_secs(CANCEL_SETTLE_TIMEOUT_SEC));
                Err(EnvError::Timeout)
            }
            Err(error) => Err(error.into()),
            Ok(output) => match output.code {
                None => Err(EnvError::Infra("docker exec ended by a signal".to_string())),
                Some(code) if code != 0 && is_unreachable(&output.stderr) => Err(EnvError::Infra(
                    failure(OpKind::DockerUnreachable, "docker exec", &output).to_string(),
                )),
                Some(code) => Ok(code),
            },
        }
    }

    fn read_file(&mut self, remote: &str, timeout: Duration) -> Result<Option<Vec<u8>>, EnvError> {
        validate_remote_path(remote).map_err(EnvError::from)?;
        let id = self.container_id().map_err(EnvError::from)?.to_string();
        let output = self
            .docker()
            .run(
                &[
                    "exec",
                    "-u",
                    "root",
                    &id,
                    "sh",
                    "-c",
                    READ_SCRIPT,
                    "sh",
                    remote,
                ],
                timeout,
            )
            .map_err(EnvError::from)?;
        match output.code {
            Some(0) if output.truncated => Err(EnvError::Failed(format!(
                "{remote} is larger than the read limit"
            ))),
            Some(0) => Ok(Some(output.stdout)),
            Some(MISSING_FILE_EXIT) => Ok(None),
            _ => Err(failure(OpKind::DockerFailed, "reading a container file", &output).into()),
        }
    }

    fn settle_execs(&mut self, timeout: Duration) -> Result<(), EnvError> {
        self.stop_processes(timeout).map_err(Into::into)
    }

    fn collect(&mut self, dest: &Path, timeout: Duration) -> Result<(), EnvError> {
        let id = self.container_id().map_err(EnvError::from)?.to_string();
        fs::create_dir_all(dest)
            .map_err(|e| EnvError::Failed(format!("{}: {e}", dest.display())))?;
        let target = dest
            .to_str()
            .ok_or_else(|| EnvError::Failed("the collection path is not valid UTF-8".into()))?;
        self.docker()
            .must(
                &["cp", &format!("{id}:/logs/."), target],
                timeout,
                OpKind::DockerFailed,
            )
            .map_err(EnvError::from)?;
        self.collected = true;
        Ok(())
    }

    fn cleanup(&mut self, timeout: Duration) -> Result<(), EnvError> {
        self.cleanup_inner(Instant::now() + timeout)
            .map_err(Into::into)
    }

    fn facts(&self) -> Option<EnvFacts> {
        Some(EnvFacts {
            project: self.names.project.clone(),
            container: self.container.clone(),
            dirs_created: self.dirs_created,
            compose_attempted: self.compose_attempted,
            collected: self.collected,
        })
    }
}
