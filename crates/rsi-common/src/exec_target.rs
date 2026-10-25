//! #3176 (program #3135 M3 S2): where a Harness session's shell tools run.
//!
//! A session either runs its tools on the daemon host ([`ExecTarget::Host`], the
//! default and every pre-existing session) or inside one validated benchmark
//! container on a docker host the operator allowed
//! ([`ExecTarget::Container`]). Only daemon code builds a container target:
//! `LaunchSessionParams` and every agent verb have no field for it. The target
//! is durable per session (`session_exec_targets`), so a continuation, a resume
//! after a daemon restart and a context rotation rebuild exactly the same
//! target, or refuse; a container-bound session never falls back to the host.
//!
//! This module holds the data, the syntactic validation and the operator
//! setting's normalizer. The live checks (`docker inspect` labels, the image
//! digest) are `rsid`'s `session::harness::tools::exec_target`.

use serde::{Deserialize, Serialize};

/// The operator setting naming the docker hosts a container target may use.
/// Empty (the default) refuses every container target. Operator-only.
pub const BENCH_DOCKER_HOSTS_FIELD: &str = "bench.docker_hosts";

/// Typed refusal prefix: the target is not acceptable (`exec_target_invalid:<reason>`).
pub const EXEC_TARGET_INVALID: &str = "exec_target_invalid";
/// Typed refusal: a container-bound session's target cannot be rebuilt.
pub const EXEC_TARGET_UNAVAILABLE: &str = "exec_target_unavailable";
/// Typed refusal of a host-touching tool call on a container-bound session.
pub const CONTAINER_TARGET_HOST_TOOL_REFUSED: &str = "container_target_host_tool_refused";

/// Default per-call ceiling of a container target's `shell` (seconds).
pub const DEFAULT_PER_CALL_CEILING_SECS: u64 = 900;
/// Largest accepted per-call ceiling (seconds).
pub const MAX_PER_CALL_CEILING_SECS: u64 = 3600;

/// Most entries of `bench.docker_hosts`.
pub const MAX_DOCKER_HOSTS: usize = 16;
/// Most pinned image digests on one target.
pub const MAX_PINNED_DIGESTS: usize = 64;
const MAX_FIELD_LEN: usize = 256;

/// Where a Harness session's `shell`, `exec_command` and `write_stdin` run.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ExecTarget {
    /// The daemon host (every ordinary session).
    #[default]
    Host,
    /// One container, via `docker -H <docker_host> exec`.
    Container(ContainerExecTarget),
}

/// A validated-on-use benchmark container (Harbor's compose service `main`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContainerExecTarget {
    /// The `docker -H` value, for example `ssh://arch-laptop`. Must be an entry
    /// of the operator setting [`BENCH_DOCKER_HOSTS_FIELD`].
    pub docker_host: String,
    /// The container name or id.
    pub container: String,
    /// The container's absolute working directory; a relative `workdir` of
    /// `exec_command` resolves against it, inside the container.
    pub workdir: String,
    /// Optional `docker exec -u` user.
    #[serde(default)]
    pub user: Option<String>,
    /// The registered compose project the container must belong to
    /// (`com.docker.compose.project`).
    pub compose_project: String,
    /// The caller's pinned image digests (`sha256:<64 hex>`); the container's
    /// image must carry one of them in its RepoDigests.
    pub pinned_digests: Vec<String>,
    /// The `shell` timeout for this target and the ceiling of any per-call
    /// `timeout_secs`.
    #[serde(default = "default_ceiling")]
    pub per_call_ceiling_secs: u64,
}

fn default_ceiling() -> u64 {
    DEFAULT_PER_CALL_CEILING_SECS
}

impl ExecTarget {
    /// The container target, if any.
    #[must_use]
    pub fn container(&self) -> Option<&ContainerExecTarget> {
        match self {
            Self::Host => None,
            Self::Container(target) => Some(target),
        }
    }

    /// `true` for a container target.
    #[must_use]
    pub fn is_container(&self) -> bool {
        matches!(self, Self::Container(_))
    }

    /// Syntax check of a container target; `Host` is always well formed.
    ///
    /// # Errors
    /// The `<reason>` of `exec_target_invalid:<reason>`.
    pub fn validate_shape(&self) -> Result<(), &'static str> {
        match self {
            Self::Host => Ok(()),
            Self::Container(target) => target.validate_shape(),
        }
    }
}

fn word_ok(text: &str, extra: &[char]) -> bool {
    !text.is_empty()
        && text.len() <= MAX_FIELD_LEN
        && !text.starts_with('-')
        && text
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.') || extra.contains(&c))
}

/// The digest part of a `repo@sha256:...` RepoDigest or a bare `sha256:...`.
#[must_use]
pub fn digest_of(reference: &str) -> &str {
    reference.rsplit_once('@').map_or(reference, |(_, d)| d)
}

fn digest_ok(digest: &str) -> bool {
    digest.strip_prefix("sha256:").is_some_and(|hex| {
        hex.len() == 64 && hex.chars().all(|c| matches!(c, '0'..='9' | 'a'..='f'))
    })
}

/// Docker host syntax: a scheme-qualified endpoint such as `ssh://host`,
/// `tcp://host:2375` or `unix:///var/run/docker.sock`. No whitespace, no
/// leading `-` (it is passed as the value of `-H`).
#[must_use]
pub fn docker_host_syntax_ok(host: &str) -> bool {
    host.len() <= MAX_FIELD_LEN
        && !host.starts_with('-')
        && host.split_once("://").is_some_and(|(scheme, rest)| {
            matches!(scheme, "ssh" | "tcp" | "unix") && !rest.is_empty()
        })
        && host.chars().all(|c| {
            c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | ':' | '/' | '@' | '-' | '[' | ']')
        })
}

impl ContainerExecTarget {
    /// # Errors
    /// The `<reason>` of `exec_target_invalid:<reason>`.
    pub fn validate_shape(&self) -> Result<(), &'static str> {
        if !docker_host_syntax_ok(&self.docker_host) {
            return Err("docker_host_syntax");
        }
        if !word_ok(&self.container, &['-']) {
            return Err("container_syntax");
        }
        if !self.workdir.starts_with('/')
            || self.workdir.len() > MAX_FIELD_LEN
            || self.workdir.chars().any(|c| c.is_control())
        {
            return Err("workdir_syntax");
        }
        if let Some(user) = &self.user
            && !word_ok(user, &['-', ':'])
        {
            return Err("user_syntax");
        }
        if !word_ok(&self.compose_project, &['-']) {
            return Err("project_syntax");
        }
        if self.pinned_digests.is_empty() {
            return Err("no_pinned_digests");
        }
        if self.pinned_digests.len() > MAX_PINNED_DIGESTS
            || !self.pinned_digests.iter().all(|d| digest_ok(d))
        {
            return Err("digest_syntax");
        }
        if !(1..=MAX_PER_CALL_CEILING_SECS).contains(&self.per_call_ceiling_secs) {
            return Err("ceiling_out_of_range");
        }
        Ok(())
    }

    /// Whether `repo_digests` (an image's RepoDigests) carry a pinned digest.
    #[must_use]
    pub fn digest_is_pinned<S: AsRef<str>>(&self, repo_digests: &[S]) -> bool {
        repo_digests.iter().any(|d| {
            self.pinned_digests
                .iter()
                .any(|p| p == digest_of(d.as_ref()))
        })
    }
}

/// Normalize the operator setting [`BENCH_DOCKER_HOSTS_FIELD`]: the comma or
/// whitespace separated text the TUI writes, or a JSON array. Empty, `null` and
/// `[]` clear it (every container target is then refused).
///
/// # Errors
/// An entry is not a docker host endpoint, is listed twice, or there are more
/// than [`MAX_DOCKER_HOSTS`].
pub fn normalize_bench_docker_hosts(value: &serde_json::Value) -> Result<Vec<String>, String> {
    let raw: Vec<String> = match value {
        serde_json::Value::Null => Vec::new(),
        serde_json::Value::String(text) => text
            .split(|c: char| c == ',' || c.is_whitespace())
            .map(str::to_string)
            .collect(),
        serde_json::Value::Array(items) => items
            .iter()
            .map(|item| {
                item.as_str().map(str::to_string).ok_or_else(|| {
                    "expected a comma-separated string or an array of strings".to_string()
                })
            })
            .collect::<Result<_, _>>()?,
        _ => return Err("expected a comma-separated string or an array of strings".into()),
    };
    let mut hosts: Vec<String> = Vec::new();
    for host in raw {
        let host = host.trim();
        if host.is_empty() {
            continue;
        }
        if !docker_host_syntax_ok(host) {
            return Err(format!(
                "invalid docker host {host:?}: use ssh://host, tcp://host:port or unix:///path"
            ));
        }
        if hosts.iter().any(|existing| existing == host) {
            return Err(format!("docker host {host:?} is listed twice"));
        }
        hosts.push(host.to_string());
    }
    if hosts.len() > MAX_DOCKER_HOSTS {
        return Err(format!(
            "at most {MAX_DOCKER_HOSTS} docker hosts (got {})",
            hosts.len()
        ));
    }
    Ok(hosts)
}

#[cfg(test)]
mod tests {
    use super::*;

    pub(crate) fn sample() -> ContainerExecTarget {
        ContainerExecTarget {
            docker_host: "ssh://arch-laptop".into(),
            container: "tb2-fix-git-main-1".into(),
            workdir: "/app".into(),
            user: Some("agent".into()),
            compose_project: "tb2-fix-git-trial-1".into(),
            pinned_digests: vec![format!("sha256:{}", "a".repeat(64))],
            per_call_ceiling_secs: DEFAULT_PER_CALL_CEILING_SECS,
        }
    }

    #[test]
    fn a_well_formed_container_target_validates_and_round_trips() {
        let target = ExecTarget::Container(sample());
        assert_eq!(target.validate_shape(), Ok(()));
        let json = serde_json::to_string(&target).unwrap();
        assert!(json.contains("\"kind\":\"container\""), "{json}");
        assert_eq!(serde_json::from_str::<ExecTarget>(&json).unwrap(), target);
        assert_eq!(ExecTarget::default(), ExecTarget::Host);
        assert_eq!(ExecTarget::Host.validate_shape(), Ok(()));
    }

    #[test]
    fn every_malformed_field_is_refused_with_its_reason() {
        let cases: Vec<(&str, Box<dyn Fn(&mut ContainerExecTarget)>)> = vec![
            (
                "docker_host_syntax",
                Box::new(|t| t.docker_host = "-H".into()),
            ),
            (
                "docker_host_syntax",
                Box::new(|t| t.docker_host = "arch-laptop".into()),
            ),
            (
                "docker_host_syntax",
                Box::new(|t| t.docker_host = "ssh://a b".into()),
            ),
            (
                "docker_host_syntax",
                Box::new(|t| t.docker_host = "ftp://host".into()),
            ),
            (
                "container_syntax",
                Box::new(|t| t.container = "--privileged".into()),
            ),
            ("container_syntax", Box::new(|t| t.container = "a b".into())),
            (
                "container_syntax",
                Box::new(|t| t.container = String::new()),
            ),
            ("workdir_syntax", Box::new(|t| t.workdir = "app".into())),
            ("workdir_syntax", Box::new(|t| t.workdir = "/a\nb".into())),
            ("user_syntax", Box::new(|t| t.user = Some("-u".into()))),
            ("user_syntax", Box::new(|t| t.user = Some("a;b".into()))),
            (
                "project_syntax",
                Box::new(|t| t.compose_project = "a b".into()),
            ),
            ("no_pinned_digests", Box::new(|t| t.pinned_digests.clear())),
            (
                "digest_syntax",
                Box::new(|t| t.pinned_digests = vec!["sha256:abc".into()]),
            ),
            (
                "digest_syntax",
                Box::new(|t| t.pinned_digests = vec![format!("sha256:{}", "A".repeat(64))]),
            ),
            (
                "ceiling_out_of_range",
                Box::new(|t| t.per_call_ceiling_secs = 0),
            ),
            (
                "ceiling_out_of_range",
                Box::new(|t| t.per_call_ceiling_secs = 3601),
            ),
        ];
        for (reason, mutate) in cases {
            let mut target = sample();
            mutate(&mut target);
            assert_eq!(target.validate_shape(), Err(reason), "{target:?}");
        }
    }

    #[test]
    fn a_repo_digest_matches_by_its_digest_part() {
        let target = sample();
        let pinned = format!("sha256:{}", "a".repeat(64));
        assert!(target.digest_is_pinned(&[format!("registry.example/tb2/fix-git@{pinned}")]));
        assert!(target.digest_is_pinned(&[pinned.clone()]));
        assert!(
            !target.digest_is_pinned(&[format!("registry.example/x@sha256:{}", "b".repeat(64))])
        );
        assert!(!target.digest_is_pinned::<String>(&[]));
    }

    #[test]
    fn the_docker_host_setting_normalizes_and_refuses_bad_entries() {
        use serde_json::json;
        assert_eq!(
            normalize_bench_docker_hosts(&json!("ssh://arch-laptop, tcp://10.0.0.2:2375")).unwrap(),
            ["ssh://arch-laptop", "tcp://10.0.0.2:2375"]
        );
        assert_eq!(
            normalize_bench_docker_hosts(&json!(["ssh://a"])).unwrap(),
            ["ssh://a"]
        );
        assert!(normalize_bench_docker_hosts(&json!("")).unwrap().is_empty());
        assert!(
            normalize_bench_docker_hosts(&json!(null))
                .unwrap()
                .is_empty()
        );
        for bad in [json!("laptop"), json!("-H ssh://a"), json!(5), json!([1])] {
            assert!(normalize_bench_docker_hosts(&bad).is_err(), "{bad}");
        }
        assert!(normalize_bench_docker_hosts(&json!("ssh://a,ssh://a")).is_err());
        let many: Vec<String> = (0..17).map(|i| format!("ssh://h{i}")).collect();
        assert!(normalize_bench_docker_hosts(&json!(many)).is_err());
    }
}
