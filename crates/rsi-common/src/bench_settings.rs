//! #3181 (program #3135 M3 S5): the operator settings of the benchmark job.
//!
//! `bench.enabled` (default off) is the master switch of the `bench` job kind,
//! `bench.max_concurrency` (default 4, at most 8) bounds Harbor's concurrent
//! trials on the laptop, and `bench.laptop_host` is the ssh host that runs
//! Harbor. `bench.docker_hosts` (#3176) is in [`crate::exec_target`].
//! Operator-only: reachable through `GetDaemonConfig` / `UpdateDaemonConfig`,
//! never an agent verb.

pub const BENCH_ENABLED_FIELD: &str = "bench.enabled";
pub const BENCH_MAX_CONCURRENCY_FIELD: &str = "bench.max_concurrency";
pub const BENCH_LAPTOP_HOST_FIELD: &str = "bench.laptop_host";

/// Harbor's default concurrency and the setting's default.
pub const DEFAULT_BENCH_MAX_CONCURRENCY: u64 = 4;
/// The most concurrent trials the setting accepts (the laptop's memory bound,
/// plan D4).
pub const MAX_BENCH_MAX_CONCURRENCY: u64 = 8;
/// The ssh host the laptop is reached by until the operator sets another.
pub const DEFAULT_BENCH_LAPTOP_HOST: &str = "arch-laptop";
/// Longest accepted ssh host name.
pub const MAX_BENCH_LAPTOP_HOST_BYTES: usize = 253;

/// The most concurrent trials: an integer (or its text) in `1..=8`.
///
/// # Errors
/// The value is not an integer in range.
pub fn parse_bench_max_concurrency(value: &serde_json::Value) -> Result<u64, String> {
    let number = match value {
        serde_json::Value::Number(n) => n.as_u64(),
        serde_json::Value::String(text) => text.trim().parse::<u64>().ok(),
        _ => None,
    }
    .ok_or("expected an integer")?;
    if !(1..=MAX_BENCH_MAX_CONCURRENCY).contains(&number) {
        return Err(format!(
            "expected an integer in 1..={MAX_BENCH_MAX_CONCURRENCY}"
        ));
    }
    Ok(number)
}

/// Whether `host` is a plain ssh host name or alias, with an optional
/// `user@`: letters, digits, `.`, `_`, `-`, never leading with `-` or `.`.
/// No option, port, URL or shell syntax can pass.
#[must_use]
pub fn laptop_host_syntax_ok(host: &str) -> bool {
    let (user, name) = host.split_once('@').map_or(("", host), |(u, n)| (u, n));
    let part_ok = |part: &str| {
        !part.is_empty()
            && !part.starts_with(['-', '.'])
            && part
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
    };
    host.len() <= MAX_BENCH_LAPTOP_HOST_BYTES
        && part_ok(name)
        && (user.is_empty() && !host.contains('@') || part_ok(user))
}

/// The laptop's ssh host from the text the TUI writes. Empty or `null` restores
/// the default.
///
/// # Errors
/// The text is not a plain ssh host.
pub fn normalize_bench_laptop_host(value: &serde_json::Value) -> Result<String, String> {
    let text = match value {
        serde_json::Value::Null => "",
        serde_json::Value::String(text) => text.trim(),
        _ => return Err("expected a host name".into()),
    };
    if text.is_empty() {
        return Ok(DEFAULT_BENCH_LAPTOP_HOST.to_string());
    }
    if !laptop_host_syntax_ok(text) {
        return Err(format!(
            "invalid laptop host {text:?}: use an ssh host name or alias such as arch-laptop"
        ));
    }
    Ok(text.to_string())
}

/// The docker host a run config must name for `laptop_host`: `ssh://<host>`.
#[must_use]
pub fn laptop_docker_host(laptop_host: &str) -> String {
    format!("ssh://{laptop_host}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn concurrency_accepts_one_to_eight_as_a_number_or_text() {
        for ok in 1..=8_u64 {
            assert_eq!(parse_bench_max_concurrency(&json!(ok)), Ok(ok));
            assert_eq!(parse_bench_max_concurrency(&json!(ok.to_string())), Ok(ok));
        }
        for bad in [
            json!(0),
            json!(9),
            json!(-1),
            json!(2.5),
            json!("x"),
            json!(null),
            json!(true),
        ] {
            assert!(parse_bench_max_concurrency(&bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn the_laptop_host_is_a_plain_ssh_name_and_empty_restores_the_default() {
        assert_eq!(
            normalize_bench_laptop_host(&json!("")),
            Ok(DEFAULT_BENCH_LAPTOP_HOST.to_string())
        );
        assert_eq!(
            normalize_bench_laptop_host(&json!(null)),
            Ok(DEFAULT_BENCH_LAPTOP_HOST.to_string())
        );
        assert_eq!(
            normalize_bench_laptop_host(&json!(" me@box.lan ")),
            Ok("me@box.lan".into())
        );
        for bad in [
            "-oProxyCommand=x",
            "a b",
            "ssh://host",
            "host:22",
            "a;b",
            "$(x)",
            ".hidden",
            "@host",
            "a@b@c",
            "user@",
        ] {
            assert!(normalize_bench_laptop_host(&json!(bad)).is_err(), "{bad}");
        }
        assert!(normalize_bench_laptop_host(&json!(7)).is_err());
        assert_eq!(laptop_docker_host("arch-laptop"), "ssh://arch-laptop");
    }
}
