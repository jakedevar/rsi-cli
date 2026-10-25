//! Spend guard and cache-stat summary for the AWS gate ledger (Issue #1010).
//!
//! ```text
//! rsi-cloud spend check [--ledger PATH] [--caps PATH] [--today YYYY-MM-DD]
//! rsi-cloud spend reserve [--ledger PATH] [--caps PATH] --minutes N --rate USD_PER_H
//!                         [--token T] [--now YYYY-MM-DDTHH:MM:SSZ]
//! rsi-cloud spend spot-reason < `aws ec2 describe-instances` JSON
//! rsi-cloud spend cache [--json-out PATH] < `sccache --show-stats --stats-format json`
//! ```
//!
//! `check` exits 0 when the ledger's cumulative compute estimate is below the
//! stop line and today's (UTC) estimate is below the daily cap, and 4 when
//! either is reached or the ledger or caps file cannot be read: the guard
//! fails closed. The operator sets both caps through the rsid daemon settings
//! `cloud_spend_stop_line_usd` and `cloud_spend_daily_cap_usd`; rsid mirrors
//! them into `spend-caps.json` beside the ledger and this module reads that
//! file. Without the file the stop line falls back to the ledger header ("Stop
//! and report by $90 cumulative ...") and the daily cap to $15. The estimate is
//! the sum of every "est compute $X" figure in the ledger; a line counts toward
//! the UTC day of its "stop <timestamp>" (else its first timestamp), the same
//! rule as rsid's `GetCloudSpend`.
//!
//! `reserve` writes the worst-case cost of a run BEFORE it starts (#3460 S1):
//! exit 0 and print `reservation_id=<token>` only when
//! `spent + open reservations + minutes/60 x rate` stays within the stop line
//! and today's (UTC) spent plus open reservations that started today plus the
//! new reservation stays within the daily cap; otherwise exit 4 and write
//! nothing. The check and the append happen under an exclusive lock on
//! `<ledger>.lock`. A reservation is the ledger line
//! `Gate reservation <token>: reserved $X until <expiry> ...`; it is open until
//! another line names `reservation <token>` and either carries an
//! `est compute $X` figure (the stop line: the actual cost replaces the
//! reservation) or says `released` (the host never started). With no such line
//! (a hub crash) the reservation stands, so a lost run still counts against both
//! caps until the orphan sweep reconciles it with a closing line.
//!
//! AWS Budgets (rsi-cloud-us-west-1-daily, -monthly) stay the external backstop.

use std::collections::HashMap;
use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

use chrono::{Duration, Utc};
use regex::Regex;
use serde_json::Value;

use crate::args::{Kind, parse};
use crate::util::{
    FileLock, absolute, default_cloud_file, json_float, now_whole_seconds, parse_stamp, round_to,
    splitlines, stamp, with_suffix,
};

pub const DEFAULT_DAILY_CAP: f64 = 15.0;
pub const CAPS_FILE: &str = "spend-caps.json";
const NUM: &str = r"[0-9]+(?:\.[0-9]+)?";

fn regex(pattern: &str) -> Regex {
    // The patterns are literals in this file; a bad one fails every test.
    Regex::new(pattern).unwrap_or_else(|error| panic!("bad regex {pattern}: {error}"))
}

static GRANT_RE: LazyLock<Regex> =
    LazyLock::new(|| regex(&format!(r"Operator grant:\s*\$({NUM})")));
static STOP_RE: LazyLock<Regex> =
    LazyLock::new(|| regex(&format!(r"Stop and report by\s*\$({NUM})\s+cumulative")));
static EST_RE: LazyLock<Regex> = LazyLock::new(|| regex(&format!(r"est compute\s*\$({NUM})")));
static DATE_RE: LazyLock<Regex> = LazyLock::new(|| regex(r"(\d{4}-\d{2}-\d{2})[T ]"));
/// An S1 reservation line; the watchdog shares it to keep S1 and S2 apart.
pub static RESERVED_RE: LazyLock<Regex> = LazyLock::new(|| {
    regex(&format!(
        r"reservation\s+(res-[A-Za-z0-9-]+):\s*reserved\s*\$({NUM})\s+until\s+(\S+).*?\(start\s+(\d{{4}}-\d{{2}}-\d{{2}})T"
    ))
});
static TOKEN_RE: LazyLock<Regex> = LazyLock::new(|| regex(r"reservation\s+(res-[A-Za-z0-9-]+)"));
static TOKEN_VALID_RE: LazyLock<Regex> = LazyLock::new(|| regex(r"\Ares-[A-Za-z0-9-]{1,64}\z"));

/// The UTC day a ledger line counts toward: the first date at or after
/// `stop `, else the first date anywhere on the line.
#[must_use]
pub fn line_date(line: &str) -> Option<String> {
    if let Some(at) = line.find("stop ")
        && let Some(found) = DATE_RE.captures(&line[at..])
    {
        return Some(found[1].to_string());
    }
    DATE_RE.captures(line).map(|found| found[1].to_string())
}

/// What a ledger says about money already spent.
#[derive(Debug, Clone, PartialEq)]
pub struct Ledger {
    pub grant: f64,
    pub header_stop: f64,
    pub spent: f64,
    pub by_day: HashMap<String, f64>,
}

impl Ledger {
    #[must_use]
    pub fn day(&self, day: &str) -> f64 {
        self.by_day.get(day).copied().unwrap_or(0.0)
    }
}

fn number(text: &str) -> f64 {
    text.parse().unwrap_or(0.0)
}

/// Parse ledger text: the header grant and stop line, the cumulative estimate
/// and the estimate by UTC day.
pub fn parse_ledger(text: &str) -> Result<Ledger, String> {
    let (Some(grant), Some(stop)) = (GRANT_RE.captures(text), STOP_RE.captures(text)) else {
        return Err(
            "ledger header lacks 'Operator grant: $N' or 'Stop and report by $N cumulative'"
                .to_string(),
        );
    };
    let mut spent = 0.0;
    let mut by_day: HashMap<String, f64> = HashMap::new();
    for line in splitlines(text) {
        if !EST_RE.is_match(line) {
            continue;
        }
        let est: f64 = EST_RE.captures_iter(line).map(|m| number(&m[1])).sum();
        spent += est;
        if let Some(day) = line_date(line) {
            *by_day.entry(day).or_insert(0.0) += est;
        }
    }
    Ok(Ledger {
        grant: number(&grant[1]),
        header_stop: number(&stop[1]),
        spent,
        by_day,
    })
}

/// Read and parse the ledger at `path`.
pub fn read_ledger(path: &Path) -> Result<Ledger, String> {
    let text = fs::read_to_string(path).map_err(|error| format!("{}: {error}", path.display()))?;
    parse_ledger(&text)
}

/// One open S1 reservation.
#[derive(Debug, Clone, PartialEq)]
pub struct Reservation {
    pub id: String,
    pub amount: f64,
    pub until: String,
    pub day: String,
    pub line: String,
}

/// Token-keyed S1 reservations not settled by a cost or release line, in the
/// order their token first appeared (a repeated token keeps its first place).
#[must_use]
pub fn open_reservation_records(text: &str) -> Vec<Reservation> {
    let mut reserved: Vec<Reservation> = Vec::new();
    let mut closed: Vec<String> = Vec::new();
    for line in splitlines(text) {
        if let Some(found) = RESERVED_RE.captures(line) {
            let record = Reservation {
                id: found[1].to_string(),
                amount: number(&found[2]),
                until: found[3].to_string(),
                day: found[4].to_string(),
                line: line.to_string(),
            };
            match reserved.iter_mut().find(|have| have.id == record.id) {
                Some(slot) => *slot = record,
                None => reserved.push(record),
            }
            continue;
        }
        if let Some(token) = TOKEN_RE.captures(line)
            && (EST_RE.is_match(line) || line.contains("released"))
        {
            closed.push(token[1].to_string());
        }
    }
    reserved
        .into_iter()
        .filter(|res| !closed.contains(&res.id))
        .collect()
}

/// Total of the open reservations and the total by the UTC day they started.
#[must_use]
pub fn open_reservations(text: &str) -> (f64, HashMap<String, f64>) {
    let mut total = 0.0;
    let mut by_day: HashMap<String, f64> = HashMap::new();
    for res in open_reservation_records(text) {
        total += res.amount;
        *by_day.entry(res.day).or_insert(0.0) += res.amount;
    }
    (total, by_day)
}

/// The caps the daemon wrote: `(stop line or None, daily cap)`. A missing file
/// is `(None, 15)`; an unreadable or malformed one is an error (fail closed).
pub fn read_caps(path: &Path) -> Result<(Option<f64>, f64), String> {
    let text = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok((None, DEFAULT_DAILY_CAP));
        }
        Err(error) => return Err(format!("unreadable {}: {error}", path.display())),
    };
    let unreadable = |why: &str| format!("unreadable {}: {why}", path.display());
    let caps: Value =
        serde_json::from_str(&text).map_err(|error| unreadable(&error.to_string()))?;
    let field = |key: &str| {
        caps.get(key)
            .and_then(json_float)
            .ok_or_else(|| unreadable(&format!("{key} is missing or not a finite number")))
    };
    Ok((Some(field("stop_line_usd")?), field("daily_cap_usd")?))
}

fn caps_path(caps: Option<&str>, ledger: &Path) -> PathBuf {
    caps.map_or_else(
        || {
            absolute(ledger)
                .parent()
                .map_or_else(|| PathBuf::from(CAPS_FILE), |dir| dir.join(CAPS_FILE))
        },
        PathBuf::from,
    )
}

/// The outcome of a guard decision: the line for stderr and the exit code.
#[derive(Debug, PartialEq)]
pub struct Verdict {
    pub message: String,
    pub code: i32,
}

/// `check`: may a remote run start on `today` (UTC `YYYY-MM-DD`)?
#[must_use]
pub fn check_verdict(ledger: &Ledger, caps: (Option<f64>, f64), today: &str) -> Verdict {
    let (caps_stop, daily_cap) = caps;
    let stop = caps_stop.unwrap_or(ledger.header_stop);
    let day_spent = ledger.day(today);
    let line = format!(
        "cloud-spend: cumulative estimate ${:.2} of ${:.2} grant, stop line ${stop:.2}; {today} UTC ${day_spent:.2} of ${daily_cap:.2} daily cap",
        ledger.spent, ledger.grant
    );
    if ledger.spent >= stop {
        return Verdict {
            message: format!("{line}; at or above the stop line, refusing to start a remote run"),
            code: 4,
        };
    }
    if day_spent >= daily_cap {
        return Verdict {
            message: format!("{line}; at or above the daily cap, refusing to start a remote run"),
            code: 4,
        };
    }
    Verdict {
        message: line,
        code: 0,
    }
}

/// The worst-case cost of a run, rounded to the four places the ledger keeps.
#[must_use]
pub fn reservation_amount(minutes: i64, rate: f64) -> f64 {
    #[allow(clippy::cast_precision_loss)]
    round_to(minutes as f64 / 60.0 * rate, 4)
}

/// `reserve`: the guard decision for a new reservation of `amount` on `today`,
/// given the settled ledger, the open reservations and the caps.
#[must_use]
pub fn reserve_verdict(
    ledger: &Ledger,
    held: (f64, &HashMap<String, f64>),
    caps: (Option<f64>, f64),
    today: &str,
    request: (i64, f64, f64),
) -> Verdict {
    let (held_total, held_by_day) = held;
    let (minutes, rate, amount) = request;
    let (caps_stop, daily_cap) = caps;
    let stop = caps_stop.unwrap_or(ledger.header_stop);
    let day_spent = ledger.day(today);
    let day_held = held_by_day.get(today).copied().unwrap_or(0.0);
    let line = format!(
        "cloud-spend: reserving ${amount:.2} ({minutes} min at ${rate:.4}/h); cumulative ${:.2} + ${held_total:.2} reserved of stop line ${stop:.2}; {today} UTC ${day_spent:.2} + ${day_held:.2} reserved of ${daily_cap:.2} daily cap",
        ledger.spent
    );
    if ledger.spent + held_total + amount > stop {
        return Verdict {
            message: format!("{line}; would pass the stop line, refusing to start a remote run"),
            code: 4,
        };
    }
    if day_spent + day_held + amount > daily_cap {
        return Verdict {
            message: format!("{line}; would pass the daily cap, refusing to start a remote run"),
            code: 4,
        };
    }
    Verdict {
        message: line,
        code: 0,
    }
}

fn refuse(stderr: &mut dyn Write, why: &str) -> i32 {
    let _ = writeln!(stderr, "cloud-spend: refusing to start a remote run: {why}");
    4
}

fn usage(stderr: &mut dyn Write, why: &str) -> i32 {
    let _ = writeln!(stderr, "rsi-cloud spend: {why}");
    2
}

fn check(args: &[String], stderr: &mut dyn Write) -> i32 {
    let spec = [
        ("--ledger", Kind::Value),
        ("--caps", Kind::Value),
        ("--today", Kind::Value),
    ];
    let parsed = match parse(args, &spec) {
        Ok(parsed) if parsed.positionals.is_empty() => parsed,
        Ok(_) => return usage(stderr, "unexpected positional arguments"),
        Err(why) => return usage(stderr, &why),
    };
    let today = parsed
        .value("--today")
        .map_or_else(|| Utc::now().format("%Y-%m-%d").to_string(), str::to_string);
    let ledger_path = parsed
        .value("--ledger")
        .map_or_else(|| default_cloud_file("spend.md"), PathBuf::from);
    let caps = caps_path(parsed.value("--caps"), &ledger_path);
    let read = read_ledger(&ledger_path).and_then(|ledger| Ok((ledger, read_caps(&caps)?)));
    let (ledger, caps) = match read {
        Ok(read) => read,
        Err(why) => return refuse(stderr, &why),
    };
    let verdict = check_verdict(&ledger, caps, &today);
    let _ = writeln!(stderr, "{}", verdict.message);
    verdict.code
}

fn reserve(args: &[String], stdout: &mut dyn Write, stderr: &mut dyn Write) -> i32 {
    let spec = [
        ("--ledger", Kind::Value),
        ("--caps", Kind::Value),
        ("--minutes", Kind::Value),
        ("--rate", Kind::Value),
        ("--token", Kind::Value),
        ("--now", Kind::Value),
    ];
    let parsed = match parse(args, &spec) {
        Ok(parsed) if parsed.positionals.is_empty() => parsed,
        Ok(_) => return usage(stderr, "unexpected positional arguments"),
        Err(why) => return usage(stderr, &why),
    };
    let (minutes, rate) = match (
        parsed.parsed::<i64>("--minutes"),
        parsed.parsed::<f64>("--rate"),
    ) {
        (Ok(Some(minutes)), Ok(Some(rate))) => (minutes, rate),
        (Err(why), _) | (_, Err(why)) => return usage(stderr, &why),
        _ => return usage(stderr, "--minutes and --rate are required"),
    };
    let now = match parsed.value("--now") {
        Some(text) => match parse_stamp(text) {
            Ok(now) => now,
            Err(why) => {
                let _ = writeln!(stderr, "rsi-cloud spend: {why}");
                return 1;
            }
        },
        None => now_whole_seconds(),
    };
    let today = now.format("%Y-%m-%d").to_string();
    let ledger_path = parsed
        .value("--ledger")
        .map_or_else(|| default_cloud_file("spend.md"), PathBuf::from);
    let caps = caps_path(parsed.value("--caps"), &ledger_path);
    if minutes <= 0 || !(rate.is_finite() && rate > 0.0) {
        return refuse(stderr, "--minutes and --rate must be positive");
    }
    let token = parsed.value("--token").map_or_else(
        || format!("res-{}-{}", now.timestamp(), std::process::id()),
        str::to_string,
    );
    if !TOKEN_VALID_RE.is_match(&token) {
        return refuse(stderr, "invalid reservation token");
    }
    let amount = reservation_amount(minutes, rate);
    let expiry = stamp(now + Duration::minutes(minutes));
    let done = (|| -> Result<Verdict, String> {
        let _lock = FileLock::acquire(&with_suffix(&ledger_path, ".lock"))
            .map_err(|error| error.to_string())?;
        let ledger = read_ledger(&ledger_path)?;
        let caps = read_caps(&caps)?;
        let text = fs::read_to_string(&ledger_path).map_err(|error| error.to_string())?;
        let (held, held_by_day) = open_reservations(&text);
        let verdict = reserve_verdict(
            &ledger,
            (held, &held_by_day),
            caps,
            &today,
            (minutes, rate, amount),
        );
        if verdict.code == 0 {
            let mut handle = fs::OpenOptions::new()
                .append(true)
                .open(&ledger_path)
                .map_err(|error| error.to_string())?;
            writeln!(
                handle,
                "Gate reservation {token}: reserved ${amount:.4} until {expiry} (start {}, {minutes} min at ${rate:.4}/h worst case).",
                stamp(now)
            )
            .map_err(|error| error.to_string())?;
        }
        Ok(verdict)
    })();
    match done {
        Ok(verdict) if verdict.code == 0 => {
            let _ = writeln!(stderr, "{}", verdict.message);
            let _ = writeln!(stdout, "reservation_id={token}");
            0
        }
        Ok(verdict) => {
            let _ = writeln!(stderr, "{}", verdict.message);
            verdict.code
        }
        Err(why) => refuse(stderr, &why),
    }
}

/// `spot-reason`: `spot_interrupted` when an instance's StateReason says AWS
/// reclaimed it. `None` means "not a spot interruption" (exit 1).
#[must_use]
pub fn spot_reason(describe_json: &str) -> Option<&'static str> {
    let data: Value = serde_json::from_str(describe_json).ok()?;
    let reservations = match data.get("Reservations") {
        Some(Value::Array(items)) => items.as_slice(),
        _ => return None,
    };
    reservations
        .iter()
        .filter_map(|reservation| reservation.get("Instances").and_then(Value::as_array))
        .flatten()
        .any(|instance| {
            instance
                .get("StateReason")
                .and_then(|reason| reason.get("Code"))
                .and_then(Value::as_str)
                .is_some_and(|code| code.contains("SpotInstanceTermination"))
        })
        .then_some("spot_interrupted")
}

/// sccache hit and miss totals.
#[derive(Debug, PartialEq)]
pub struct CacheSummary {
    pub hits: i64,
    pub misses: i64,
    pub hit_rate_percent: f64,
}

fn count_total(body: &Value, key: &str) -> Result<i64, String> {
    let counts = match body.get(key) {
        None | Some(Value::Null) => return Ok(0),
        Some(section) => match section.get("counts") {
            None | Some(Value::Null) => return Ok(0),
            Some(Value::Object(counts)) => counts,
            Some(_) => return Err(format!("{key}.counts is not an object")),
        },
    };
    let mut total = 0_i64;
    for value in counts.values() {
        let count = match value {
            Value::Number(n) => n
                .as_i64()
                .or_else(|| {
                    #[allow(clippy::cast_possible_truncation)]
                    n.as_f64().map(|f| f.trunc() as i64)
                })
                .ok_or_else(|| format!("{key}: bad count"))?,
            Value::String(text) => text
                .trim()
                .parse::<i64>()
                .map_err(|_| format!("{key}: bad count"))?,
            Value::Bool(flag) => i64::from(*flag),
            _ => return Err(format!("{key}: bad count")),
        };
        total += count;
    }
    Ok(total)
}

/// Summarise `sccache --show-stats --stats-format json`.
pub fn summarize_cache(stats_json: &str) -> Result<CacheSummary, String> {
    let stats: Value = serde_json::from_str(stats_json).map_err(|error| error.to_string())?;
    if !stats.is_object() {
        return Err("stats is not an object".to_string());
    }
    let body = stats.get("stats").unwrap_or(&stats);
    if !body.is_object() {
        return Err("stats is not an object".to_string());
    }
    let hits = count_total(body, "cache_hits")?;
    let misses = count_total(body, "cache_misses")?;
    #[allow(clippy::cast_precision_loss)]
    let rate = if hits + misses == 0 {
        0.0
    } else {
        100.0 * hits as f64 / (hits + misses) as f64
    };
    Ok(CacheSummary {
        hits,
        misses,
        hit_rate_percent: round_to(rate, 1),
    })
}

fn cache(
    args: &[String],
    stdin: &mut dyn Read,
    stdout: &mut dyn Write,
    stderr: &mut dyn Write,
) -> i32 {
    let parsed = match parse(args, &[("--json-out", Kind::Value)]) {
        Ok(parsed) if parsed.positionals.is_empty() => parsed,
        Ok(_) => return usage(stderr, "unexpected positional arguments"),
        Err(why) => return usage(stderr, &why),
    };
    let mut input = String::new();
    let summary = stdin
        .read_to_string(&mut input)
        .map_err(|error| error.to_string())
        .and_then(|_| summarize_cache(&input));
    let summary = match summary {
        Ok(summary) => summary,
        Err(why) => {
            let _ = writeln!(stderr, "cloud-spend: unreadable sccache stats: {why}");
            return 1;
        }
    };
    if let Some(path) = parsed.value("--json-out") {
        let body = format!(
            "{{\"hits\": {}, \"misses\": {}, \"hit_rate_percent\": {:?}}}\n",
            summary.hits, summary.misses, summary.hit_rate_percent
        );
        if let Err(error) = fs::write(path, body) {
            let _ = writeln!(stderr, "cloud-spend: cannot write {path}: {error}");
            return 1;
        }
    }
    let _ = writeln!(
        stdout,
        "sccache_hits={} sccache_misses={} sccache_hit_rate={:?}%",
        summary.hits, summary.misses, summary.hit_rate_percent
    );
    0
}

/// `rsi-cloud spend <check|reserve|spot-reason|cache> ...`
pub fn run(
    args: &[String],
    stdin: &mut dyn Read,
    stdout: &mut dyn Write,
    stderr: &mut dyn Write,
) -> i32 {
    let Some((command, rest)) = args.split_first() else {
        return usage(stderr, "expected check, reserve, spot-reason or cache");
    };
    match command.as_str() {
        "check" => check(rest, stderr),
        "reserve" => reserve(rest, stdout, stderr),
        "spot-reason" => {
            let mut input = String::new();
            if stdin.read_to_string(&mut input).is_err() {
                return 1;
            }
            match spot_reason(&input) {
                Some(reason) => {
                    let _ = writeln!(stdout, "{reason}");
                    0
                }
                None => 1,
            }
        }
        "cache" => cache(rest, stdin, stdout, stderr),
        other => usage(stderr, &format!("unknown command {other}")),
    }
}
