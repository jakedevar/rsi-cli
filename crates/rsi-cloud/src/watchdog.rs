//! Orphan sweeper for RSI-tagged AWS hosts (Issue #3465, plan #3460 S2).
//!
//! Run from a systemd --user timer every 5 minutes (`scripts/cloud-watchdog.timer`).
//! One pass:
//!
//! 1. Terminates every EC2 instance tagged `Service=rsi-*` whose `rsi:expires-at`
//!    tag is more than `--grace-minutes` in the past, or that has no usable
//!    expiry tag. Unattached (`available`) EBS volumes with the same tags get the
//!    same verdict and are deleted. Attached volumes die with their instance
//!    (`delete_on_termination`), so they are never touched directly. Services
//!    matching `--exempt-service` (default `*-rsid`, the persistent rsid host
//!    module) are never swept.
//! 2. Appends one `Gate window <id>: stop ...` line per terminated instance to the
//!    spend ledger (`~/.rsi/cloud/spend.md`) unless the ledger already has a stop
//!    line for it, so a sweep is never double counted.
//! 3. Reconciles reservations: S1 `Gate reservation res-<token>: reserved ...`
//!    lines close when a later line names the token and records `est compute $` or
//!    `released`, sharing the spend guard's parser. S2 `Gate window <id>:`
//!    reservations close by window stop line. Open reservations with an `until`
//!    more than the grace in the past are closed with a stop line. The cost is the
//!    instance's real launch-to-end time at the rate on its start line (else the
//!    reserve rate), never above the reservation; with no matching instance the
//!    full reservation is charged, an upper bound.
//! 4. Prints one summary line, including the ledger totals.
//!
//! `--dry-run` calls only the describe APIs and writes nothing. The existing
//! `rsi-cloud-watchdog` profile is used; the sweep needs no new IAM permission
//! beyond ec2 describe/terminate/delete-volume it already holds.

use std::collections::HashSet;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::LazyLock;

use chrono::{DateTime, Duration, Utc};
use regex::Regex;
use serde_json::Value;

use crate::args::{Kind, parse};
use crate::spend;
use crate::util::{
    FileLock, default_cloud_file, fnmatchcase, now_whole_seconds, parse_time, splitlines, stamp,
};

pub const REGION: &str = "us-west-1";
pub const PROFILE: &str = "rsi-cloud-watchdog";
pub const SERVICE_GLOB: &str = "rsi-*";
pub const EXEMPT_SERVICE_GLOB: &str = "*-rsid";
pub const EXPIRY_TAG: &str = "rsi:expires-at";
pub const RESERVE_RATE_USD: f64 = 0.7813;
const LIVE_STATES: &str = "pending,running,stopping,stopped";

fn regex(pattern: &str) -> Regex {
    Regex::new(pattern).unwrap_or_else(|error| panic!("bad regex {pattern}: {error}"))
}

static RESERVED_RE: LazyLock<Regex> =
    LazyLock::new(|| regex(r"reserved\s*\$([0-9]+(?:\.[0-9]+)?)\s+until\s+(\S+?)[.;,]?(?:\s|$)"));
static WINDOW_RE: LazyLock<Regex> = LazyLock::new(|| regex(r"^Gate window ([^\s:]+):"));
static STOP_RE: LazyLock<Regex> = LazyLock::new(|| regex(r"^Gate window ([^\s:]+): stop "));
static RATE_RE: LazyLock<Regex> = LazyLock::new(|| regex(r"est \$([0-9]+(?:\.[0-9]+)?)/h"));
static TRANSITION_RE: LazyLock<Regex> =
    LazyLock::new(|| regex(r"\((\d{4}-\d{2}-\d{2} \d{2}:\d{2}:\d{2}) GMT\)"));
static INSTANCE_ID_RE: LazyLock<Regex> = LazyLock::new(|| regex(r"\Ai-[0-9a-f]+\z"));

/// A failed AWS call.
#[derive(Debug)]
pub struct AwsError(pub String);

impl std::fmt::Display for AwsError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// The AWS CLI as the watchdog uses it. `command` is the part before the
/// region/profile/output flags, e.g. `["ec2", "describe-instances", "--filters", ...]`.
/// An empty response is `{}`.
pub trait Aws {
    fn call(&self, command: &[String]) -> Result<Value, AwsError>;
}

/// The real `aws` CLI.
pub struct CliAws {
    pub binary: String,
    pub region: String,
    pub profile: String,
}

impl Aws for CliAws {
    fn call(&self, command: &[String]) -> Result<Value, AwsError> {
        let output = Command::new(&self.binary)
            .args(command)
            .args([
                "--region",
                &self.region,
                "--profile",
                &self.profile,
                "--output",
                "json",
            ])
            .output()
            .map_err(|error| {
                AwsError(format!(
                    "aws {} failed: {}: {error}",
                    command
                        .iter()
                        .take(2)
                        .cloned()
                        .collect::<Vec<_>>()
                        .join(" "),
                    self.binary
                ))
            })?;
        if !output.status.success() {
            return Err(AwsError(format!(
                "aws {} failed: {}",
                command
                    .iter()
                    .take(2)
                    .cloned()
                    .collect::<Vec<_>>()
                    .join(" "),
                String::from_utf8_lossy(&output.stderr).trim()
            )));
        }
        let stdout = String::from_utf8_lossy(&output.stdout);
        if stdout.trim().is_empty() {
            return Ok(Value::Object(serde_json::Map::new()));
        }
        serde_json::from_str(&stdout).map_err(|error| AwsError(error.to_string()))
    }
}

/// What a pass may sweep and when.
#[derive(Debug, Clone)]
pub struct Options {
    pub dry_run: bool,
    pub ledger: PathBuf,
    pub service_glob: String,
    /// Empty means no exemption.
    pub exempt_service: String,
    pub grace_minutes: f64,
}

impl Options {
    fn grace(&self) -> Duration {
        #[allow(clippy::cast_possible_truncation)]
        Duration::microseconds((self.grace_minutes * 60_000_000.0) as i64)
    }
}

fn tag_map(resource: &Value) -> std::collections::HashMap<String, String> {
    resource
        .get("Tags")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|tag| {
            Some((
                tag.get("Key")?.as_str()?.to_string(),
                tag.get("Value")?.as_str()?.to_string(),
            ))
        })
        .collect()
}

/// `None` keeps a resource; otherwise the reason it must be swept.
#[must_use]
pub fn verdict(resource: &Value, now: DateTime<Utc>, options: &Options) -> Option<&'static str> {
    let tags = tag_map(resource);
    let service = tags.get("Service").map_or("", String::as_str);
    if !fnmatchcase(service, &options.service_glob) {
        return None;
    }
    if !options.exempt_service.is_empty() && fnmatchcase(service, &options.exempt_service) {
        return None;
    }
    let Some(raw) = tags.get(EXPIRY_TAG).filter(|raw| !raw.trim().is_empty()) else {
        return Some("missing_expiry");
    };
    let Some(expires) = parse_time(raw) else {
        return Some("missing_expiry");
    };
    (now > expires + options.grace()).then_some("expired")
}

fn strings(items: &[&str]) -> Vec<String> {
    items.iter().map(|item| (*item).to_string()).collect()
}

fn list_instances(aws: &dyn Aws, options: &Options) -> Result<Vec<Value>, AwsError> {
    let mut command = strings(&["ec2", "describe-instances", "--filters"]);
    command.push(format!("Name=tag:Service,Values={}", options.service_glob));
    command.push(format!("Name=instance-state-name,Values={LIVE_STATES}"));
    let response = aws.call(&command)?;
    Ok(response
        .get("Reservations")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|reservation| reservation.get("Instances").and_then(Value::as_array))
        .flatten()
        .cloned()
        .collect())
}

fn list_volumes(aws: &dyn Aws, options: &Options) -> Result<Vec<Value>, AwsError> {
    let mut command = strings(&["ec2", "describe-volumes", "--filters"]);
    command.push(format!("Name=tag:Service,Values={}", options.service_glob));
    command.push("Name=status,Values=available".to_string());
    let response = aws.call(&command)?;
    Ok(response
        .get("Volumes")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default())
}

/// The instance record (any state, terminated included) or `None`.
fn lookup_instance(aws: &dyn Aws, instance_id: &str) -> Option<Value> {
    if !INSTANCE_ID_RE.is_match(instance_id) {
        return None;
    }
    let command = strings(&["ec2", "describe-instances", "--instance-ids", instance_id]);
    let response = aws.call(&command).ok()?;
    response
        .get("Reservations")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|reservation| reservation.get("Instances").and_then(Value::as_array))
        .flatten()
        .find(|instance| instance.get("InstanceId").and_then(Value::as_str) == Some(instance_id))
        .cloned()
}

fn ledger_text(path: &Path) -> Result<String, String> {
    match fs::read_to_string(path) {
        Ok(text) => Ok(text),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(String::new()),
        Err(error) => Err(format!("{}: {error}", path.display())),
    }
}

fn stopped_ids(text: &str) -> HashSet<String> {
    splitlines(text)
        .into_iter()
        .filter_map(|line| STOP_RE.captures(line).map(|found| found[1].to_string()))
        .collect()
}

fn start_rate(text: &str, instance_id: &str) -> f64 {
    for line in splitlines(text) {
        if let Some(found) = WINDOW_RE.captures(line)
            && &found[1] == instance_id
            && line.contains(" start ")
            && let Some(rate) = RATE_RE.captures(line)
        {
            return rate[1].parse().unwrap_or(RESERVE_RATE_USD);
        }
    }
    RESERVE_RATE_USD
}

/// An open reservation, S1 (token-keyed) or legacy S2 (window-keyed).
#[derive(Debug, Clone, PartialEq)]
pub struct OpenReservation {
    pub id: Option<String>,
    pub amount: f64,
    pub until: Option<DateTime<Utc>>,
    pub token: Option<String>,
}

/// Open S1 tokens and legacy S2 windows, once per identity.
#[must_use]
pub fn open_reservations(text: &str) -> Vec<OpenReservation> {
    let mut found: Vec<OpenReservation> = spend::open_reservation_records(text)
        .into_iter()
        .map(|res| OpenReservation {
            id: Some(res.id.clone()),
            amount: res.amount,
            until: parse_time(&res.until),
            token: Some(res.id),
        })
        .collect();
    let stops = stopped_ids(text);
    let mut seen: HashSet<String> = HashSet::new();
    for line in splitlines(text) {
        // Settled S1 records must never fall back to anonymous S2 reservations.
        if spend::RESERVED_RE.is_match(line) {
            continue;
        }
        let Some(found_line) = RESERVED_RE.captures(line) else {
            continue;
        };
        let id = WINDOW_RE.captures(line).map(|window| window[1].to_string());
        if let Some(id) = &id {
            if stops.contains(id) || seen.contains(id) {
                continue;
            }
            seen.insert(id.clone());
        }
        found.push(OpenReservation {
            id,
            amount: found_line[1].parse().unwrap_or(0.0),
            until: parse_time(&found_line[2]),
            token: None,
        });
    }
    found
}

fn stop_line(
    instance_id: &str,
    start: Option<DateTime<Utc>>,
    end: DateTime<Utc>,
    rate: f64,
    cost: f64,
    note: &str,
) -> String {
    let elapsed = start.map_or(0, |start| (end - start).num_seconds().max(0));
    #[allow(clippy::cast_precision_loss)]
    let hours = elapsed as f64 / 3600.0;
    format!(
        "Gate window {instance_id}: stop {}, elapsed {elapsed} s = {hours:.4} h, est compute ${cost:.4} at ${rate:.4}/h; {note}\n",
        stamp(end)
    )
}

fn append_ledger(path: &Path, lines: &[String]) -> Result<(), String> {
    if lines.is_empty() {
        return Ok(());
    }
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|error| error.to_string())?;
    }
    let mut handle = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .map_err(|error| format!("{}: {error}", path.display()))?;
    handle
        .write_all(lines.concat().as_bytes())
        .map_err(|error| format!("{}: {error}", path.display()))
}

/// Best known end of a (possibly terminated) instance's billing.
fn end_of_instance(instance: &Value, fallback: DateTime<Utc>) -> DateTime<Utc> {
    let state = instance
        .get("State")
        .and_then(|state| state.get("Name"))
        .and_then(Value::as_str);
    if matches!(state, Some("running" | "pending" | "stopping")) {
        return fallback;
    }
    instance
        .get("StateTransitionReason")
        .and_then(Value::as_str)
        .and_then(|reason| TRANSITION_RE.captures(reason))
        .and_then(|found| parse_time(&format!("{}Z", found[1].replace(' ', "T"))))
        .unwrap_or(fallback)
}

fn seconds_between(later: DateTime<Utc>, earlier: DateTime<Utc>) -> f64 {
    #[allow(clippy::cast_precision_loss)]
    let micros = (later - earlier).num_microseconds().unwrap_or(0) as f64;
    micros / 1e6
}

/// One sweep: terminate expired or untagged instances, delete such volumes.
/// Returns `("id:reason" terminated, "id:reason" deleted)`.
pub fn sweep(
    aws: &dyn Aws,
    options: &Options,
    now: DateTime<Utc>,
) -> Result<(Vec<String>, Vec<String>), String> {
    let text = ledger_text(&options.ledger)?;
    let mut stopped = stopped_ids(&text);
    let mut victims: Vec<(Value, &'static str)> = Vec::new();
    for instance in list_instances(aws, options).map_err(|e| e.to_string())? {
        if let Some(reason) = verdict(&instance, now, options) {
            victims.push((instance, reason));
        }
    }
    let mut volumes: Vec<(Value, &'static str)> = Vec::new();
    for volume in list_volumes(aws, options).map_err(|e| e.to_string())? {
        if let Some(reason) = verdict(&volume, now, options) {
            volumes.push((volume, reason));
        }
    }

    let ids: Vec<&str> = victims
        .iter()
        .filter_map(|(instance, _)| instance.get("InstanceId").and_then(Value::as_str))
        .collect();
    if !ids.is_empty() && !options.dry_run {
        let mut command = strings(&["ec2", "terminate-instances", "--instance-ids"]);
        command.extend(ids.iter().map(|id| (*id).to_string()));
        aws.call(&command).map_err(|e| e.to_string())?;
    }
    let mut terminated = Vec::new();
    let mut new_lines = Vec::new();
    for (instance, reason) in &victims {
        let Some(instance_id) = instance.get("InstanceId").and_then(Value::as_str) else {
            continue;
        };
        terminated.push(format!("{instance_id}:{reason}"));
        if options.dry_run || stopped.contains(instance_id) {
            continue;
        }
        let launch = instance
            .get("LaunchTime")
            .and_then(Value::as_str)
            .and_then(parse_time)
            .unwrap_or(now);
        let rate = start_rate(&text, instance_id);
        let cost = seconds_between(now, launch).max(0.0) / 3600.0 * rate;
        new_lines.push(stop_line(
            instance_id,
            Some(launch),
            now,
            rate,
            cost,
            &format!("terminated by orphan sweep ({reason}); sccache: n/a."),
        ));
        stopped.insert(instance_id.to_string());
    }
    // The instances are already terminating: record their cost before any
    // later AWS call can fail the pass, or the next pass (which no longer
    // sees them) would never charge them.
    if !options.dry_run {
        append_ledger(&options.ledger, &new_lines)?;
    }
    let mut deleted = Vec::new();
    for (volume, reason) in &volumes {
        let Some(volume_id) = volume.get("VolumeId").and_then(Value::as_str) else {
            continue;
        };
        if !options.dry_run {
            let command = strings(&["ec2", "delete-volume", "--volume-id", volume_id]);
            aws.call(&command).map_err(|e| e.to_string())?;
        }
        deleted.push(format!("{volume_id}:{reason}"));
    }
    Ok((terminated, deleted))
}

/// Close stale reservations. Returns (closed `id:$cost`, still-open reserved USD).
pub fn reconcile(
    aws: &dyn Aws,
    options: &Options,
    now: DateTime<Utc>,
) -> Result<(Vec<String>, f64), String> {
    let text = ledger_text(&options.ledger)?;
    let grace = options.grace();
    let mut closed = Vec::new();
    let mut new_lines = Vec::new();
    let mut still_open = 0.0;
    for res in open_reservations(&text) {
        if let Some(until) = res.until
            && now <= until + grace
        {
            still_open += res.amount;
            continue;
        }
        let rid = res.id.clone().unwrap_or_else(|| "unmatched".to_string());
        let until_text = res.until.map_or_else(|| "?".to_string(), stamp);
        let instance = res.id.as_deref().and_then(|id| lookup_instance(aws, id));
        let (cost, rate, start, end, mut note) = if let Some(instance) = instance {
            let start = instance
                .get("LaunchTime")
                .and_then(Value::as_str)
                .and_then(parse_time)
                .unwrap_or(now);
            let end = end_of_instance(&instance, now);
            let rate = start_rate(&text, &rid);
            let cost = res
                .amount
                .min(seconds_between(end, start).max(0.0) / 3600.0 * rate);
            (
                cost,
                rate,
                Some(start),
                end,
                format!(
                    "reconciled by orphan sweep (reservation ${:.4} until {until_text}); sccache: n/a.",
                    res.amount
                ),
            )
        } else {
            (
                res.amount,
                RESERVE_RATE_USD,
                None,
                now,
                format!(
                    "reconciled by orphan sweep (reservation ${:.4} until {until_text}, no instance found, charged in full); sccache: n/a.",
                    res.amount
                ),
            )
        };
        closed.push(format!("{rid}:${cost:.4}"));
        if let Some(token) = &res.token {
            note.push_str(&format!(" reservation {token}."));
        }
        new_lines.push(stop_line(&rid, start, end, rate, cost, &note));
    }
    if !options.dry_run {
        append_ledger(&options.ledger, &new_lines)?;
    }
    Ok((closed, still_open))
}

fn spend_summary(options: &Options, now: DateTime<Utc>) -> String {
    match spend::read_ledger(&options.ledger) {
        Ok(ledger) => format!(
            "ledger_cumulative_usd={:.2} ledger_today_usd={:.2}",
            ledger.spent,
            ledger.day(&now.format("%Y-%m-%d").to_string())
        ),
        Err(error) => format!("spend=unreadable({error})"),
    }
}

fn dash(items: &[String]) -> String {
    if items.is_empty() {
        "-".to_string()
    } else {
        items.join(",")
    }
}

/// One full pass under the watchdog lock; returns the summary line.
pub fn pass(aws: &dyn Aws, options: &Options, now: DateTime<Utc>) -> Result<String, String> {
    if let Some(dir) = options.ledger.parent() {
        fs::create_dir_all(dir).map_err(|error| error.to_string())?;
    }
    let lock_path = options.ledger.with_file_name("watchdog.lock");
    let _lease = FileLock::acquire(&lock_path).map_err(|error| error.to_string())?;
    let (terminated, deleted) = sweep(aws, options, now)?;
    let (closed, reserved_open) = reconcile(aws, options, now)?;
    Ok(format!(
        "{} dry_run={} terminated={} deleted_volumes={} reconciled={} open_reserved_usd={reserved_open:.2} {}",
        stamp(now),
        if options.dry_run { "True" } else { "False" },
        dash(&terminated),
        dash(&deleted),
        dash(&closed),
        spend_summary(options, now)
    ))
}

/// `rsi-cloud watchdog [--dry-run] [--ledger P] [--region R] [--profile P] [--aws BIN]
/// [--service-glob G] [--exempt-service G] [--grace-minutes M] [--now T]`
pub fn run(args: &[String], stdout: &mut dyn Write, stderr: &mut dyn Write) -> i32 {
    let spec = [
        ("--dry-run", Kind::Flag),
        ("--ledger", Kind::Value),
        ("--region", Kind::Value),
        ("--profile", Kind::Value),
        ("--aws", Kind::Value),
        ("--service-glob", Kind::Value),
        ("--exempt-service", Kind::Value),
        ("--grace-minutes", Kind::Value),
        ("--now", Kind::Value),
    ];
    let usage = |stderr: &mut dyn Write, why: &str| {
        let _ = writeln!(stderr, "rsi-cloud watchdog: error: {why}");
        2
    };
    let parsed = match parse(args, &spec) {
        Ok(parsed) if parsed.positionals.is_empty() => parsed,
        Ok(_) => return usage(stderr, "unexpected positional arguments"),
        Err(why) => return usage(stderr, &why),
    };
    let grace_minutes = match parsed.parsed::<f64>("--grace-minutes") {
        Ok(value) => value.unwrap_or(10.0),
        Err(why) => return usage(stderr, &why),
    };
    if !grace_minutes.is_finite() || grace_minutes < 0.0 {
        return usage(stderr, "grace must be nonnegative");
    }
    let now = match parsed.value("--now") {
        Some(text) => match parse_time(text) {
            Some(now) => now,
            None => return usage(stderr, "--now is not a timestamp"),
        },
        None => now_whole_seconds(),
    };
    let options = Options {
        dry_run: parsed.flag("--dry-run"),
        ledger: parsed
            .value("--ledger")
            .map_or_else(|| default_cloud_file("spend.md"), PathBuf::from),
        service_glob: parsed
            .value("--service-glob")
            .unwrap_or(SERVICE_GLOB)
            .to_string(),
        exempt_service: parsed
            .value("--exempt-service")
            .unwrap_or(EXEMPT_SERVICE_GLOB)
            .to_string(),
        grace_minutes,
    };
    let aws = CliAws {
        binary: parsed.value("--aws").unwrap_or("aws").to_string(),
        region: parsed.value("--region").unwrap_or(REGION).to_string(),
        profile: parsed.value("--profile").unwrap_or(PROFILE).to_string(),
    };
    match pass(&aws, &options, now) {
        Ok(line) => {
            let _ = writeln!(stdout, "{line}");
            let _ = stdout.flush();
            0
        }
        Err(error) => {
            let _ = writeln!(stderr, "cloud-watchdog failed: {error}");
            1
        }
    }
}
