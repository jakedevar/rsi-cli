//! Lease state for the warm spot pool (#3460 S4b); the helper behind
//! `scripts/cloud-pool.sh`.
//!
//! The pool file (default `~/.rsi/cloud/pool.json`) maps a slot (`pool-1` ..
//! `pool-6`) to its lease: `slot`, `host` (`ec2-user@IP`), `instance_id`,
//! `public_ip`, `lease_no`, `lease_start`, `lease_expires_at`, `lease_minutes`,
//! `reservation_id`, `hourly_usd`, `last_job_at`, `state` (`starting|ready`),
//! `renewal` (`scheduled|refused`), `renewal_note`.
//!
//! `cloud-pool.sh` holds an exclusive lock around every command, so this module
//! only reads and atomically rewrites the file; it never touches AWS, the ledger
//! or terraform. Times are UTC `YYYY-MM-DDTHH:MM:SSZ`.
//!
//! ```text
//! rsi-cloud pool-state [--file F] [--now T] <command>
//!   slots --want N        print "have=K" and "new=<slots>"; exit 2 past the pool limit
//!   put SLOT K=V ...      create or update a lease (integers and floats are typed)
//!   get SLOT FIELD        print one field; exit 1 when the lease or field is absent
//!   list                  print the slot names
//!   drop SLOT ...         remove leases
//!   touch [SLOT]          record job activity now (every lease without SLOT)
//!   hosts-json --out P [--drop SLOT ...] [--set SLOT=EXPIRY ...]
//!                         write {"hosts": {slot: lease_expires_at}} for terraform
//!   due [--idle-minutes M] [--renew-window M]
//!                         print "<slot> expire|idle|renew" for leases needing action
//!   status [--json]       print the leases with remaining lease and idle time
//!   shift TIME MINUTES    print TIME + MINUTES (the next lease's expiry)
//!   until TIME            print the whole minutes (rounded up) from now to TIME
//!   output-field SLOT F   read `terraform output -json hosts` on stdin, print one field
//!   instance-state        read `aws ec2 describe-instances` JSON on stdin, print the state name
//!   cost SLOT END         print "<elapsed seconds> <cost USD>" from lease_start to END
//! ```

use std::fs;
use std::io::{Read, Write};
use std::path::PathBuf;

use chrono::{DateTime, Duration, Utc};
use serde_json::{Map, Value, json};

use crate::args::{Kind, parse};
use crate::util::{
    FileLock, default_cloud_file, json_float, now_whole_seconds, parse_stamp, pretty_sorted,
    py_display, stamp, with_suffix,
};

pub const MAX_HOSTS: usize = 6;

#[must_use]
pub fn slots() -> Vec<String> {
    (1..=MAX_HOSTS).map(|n| format!("pool-{n}")).collect()
}

type Leases = Map<String, Value>;

struct Pool {
    path: PathBuf,
    now: Option<DateTime<Utc>>,
}

impl Pool {
    fn now(&self) -> DateTime<Utc> {
        self.now.unwrap_or_else(now_whole_seconds)
    }

    fn load(&self) -> Result<Value, String> {
        let text = match fs::read_to_string(&self.path) {
            Ok(text) => text,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(json!({"version": 1, "leases": {}}));
            }
            Err(error) => return Err(format!("{}: {error}", self.path.display())),
        };
        let data: Value = serde_json::from_str(&text)
            .map_err(|error| format!("{}: {error}", self.path.display()))?;
        if data.get("leases").is_some_and(Value::is_object) {
            Ok(data)
        } else {
            Err(format!(
                "cloud-pool: {} is not a pool file",
                self.path.display()
            ))
        }
    }

    fn leases(&self) -> Result<Leases, String> {
        Ok(self
            .load()?
            .get("leases")
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default())
    }

    /// Replace the leases of the pool file atomically (temp file, then rename).
    fn save(&self, data: &Value) -> Result<(), String> {
        if let Some(dir) = crate::util::absolute(&self.path).parent() {
            fs::create_dir_all(dir).map_err(|error| error.to_string())?;
        }
        let tmp = with_suffix(&self.path, &format!(".tmp.{}", std::process::id()));
        fs::write(&tmp, format!("{}\n", pretty_sorted(data))).map_err(|error| error.to_string())?;
        fs::rename(&tmp, &self.path).map_err(|error| error.to_string())
    }

    /// Serialize read-modify-write cycles (touch must not wait for a long apply).
    fn locked(&self) -> Result<FileLock, String> {
        if let Some(dir) = crate::util::absolute(&self.path).parent() {
            fs::create_dir_all(dir).map_err(|error| error.to_string())?;
        }
        FileLock::acquire(&with_suffix(&self.path, ".lock")).map_err(|error| error.to_string())
    }

    /// Run `change` on the leases under the lock and save the result.
    fn update(&self, change: impl FnOnce(&mut Leases) -> Result<(), i32>) -> Result<i32, String> {
        let _lock = self.locked()?;
        let mut data = self.load()?;
        let Some(leases) = data.get_mut("leases").and_then(Value::as_object_mut) else {
            return Err(format!(
                "cloud-pool: {} is not a pool file",
                self.path.display()
            ));
        };
        if let Err(code) = change(leases) {
            return Ok(code);
        }
        self.save(&data)?;
        Ok(0)
    }
}

/// A command-line value as the number it looks like, else the text (`typed`).
#[must_use]
pub fn typed(value: &str) -> Value {
    if let Ok(int) = value.trim().parse::<i64>() {
        return json!(int);
    }
    if let Ok(float) = value.trim().parse::<f64>()
        && float.is_finite()
    {
        return json!(float);
    }
    json!(value)
}

fn text<'a>(lease: &'a Value, field: &str) -> Result<&'a str, String> {
    lease
        .get(field)
        .and_then(Value::as_str)
        .ok_or_else(|| format!("lease has no text field {field}"))
}

fn time_field(lease: &Value, field: &str) -> Result<DateTime<Utc>, String> {
    parse_stamp(text(lease, field)?)
}

/// The latest of the last job and the lease start (a lease is "active" from its start).
fn active_since(lease: &Value) -> Result<DateTime<Utc>, String> {
    let start = time_field(lease, "lease_start")?;
    let last = match lease.get("last_job_at") {
        Some(Value::String(last)) if !last.is_empty() => parse_stamp(last)?,
        _ => start,
    };
    Ok(last.max(start))
}

fn truthy(value: Option<&Value>) -> bool {
    match value {
        None | Some(Value::Null) => false,
        Some(Value::Bool(flag)) => *flag,
        Some(Value::String(text)) => !text.is_empty(),
        Some(Value::Number(number)) => number.as_f64().is_some_and(|n| n != 0.0),
        Some(Value::Array(items)) => !items.is_empty(),
        Some(Value::Object(map)) => !map.is_empty(),
    }
}

fn shown(lease: &Value, field: &str, default: &str) -> String {
    match lease.get(field) {
        Some(value) if truthy(Some(value)) => py_display(value),
        _ => default.to_string(),
    }
}

fn shown_or_missing(lease: &Value, field: &str, default: &str) -> String {
    lease
        .get(field)
        .map_or_else(|| default.to_string(), py_display)
}

fn floor_minutes(duration: Duration) -> i64 {
    duration.num_seconds().div_euclid(60)
}

fn cmd_slots(
    pool: &Pool,
    args: &[String],
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> Result<i32, String> {
    let parsed = parse(args, &[("--want", Kind::Value)])?;
    let want = parsed
        .parsed::<i64>("--want")?
        .ok_or("argument --want is required")?;
    let leases = pool.leases()?;
    if want < 1 || usize::try_from(want).map_or(true, |want| want > MAX_HOSTS) {
        let _ = writeln!(
            err,
            "cloud-pool: the pool holds 1 to {MAX_HOSTS} hosts, not {want}"
        );
        return Ok(2);
    }
    let want = usize::try_from(want).unwrap_or(MAX_HOSTS);
    let free: Vec<String> = slots()
        .into_iter()
        .filter(|slot| !leases.contains_key(slot))
        .collect();
    let take = want.saturating_sub(leases.len());
    let new: Vec<String> = free.into_iter().take(take).collect();
    let _ = writeln!(out, "have={}", leases.len());
    let _ = writeln!(out, "new={}", new.join(" "));
    Ok(0)
}

fn cmd_put(pool: &Pool, args: &[String], err: &mut dyn Write) -> Result<i32, String> {
    let parsed = parse(args, &[])?;
    let Some((slot, pairs)) = parsed.positionals.split_first() else {
        return Err("put needs SLOT and at least one K=V".to_string());
    };
    if pairs.is_empty() {
        return Err("put needs SLOT and at least one K=V".to_string());
    }
    if !slots().contains(slot) {
        let _ = writeln!(err, "cloud-pool: unknown slot {slot}");
        return Ok(2);
    }
    pool.update(|leases| {
        let lease = leases
            .entry(slot.clone())
            .or_insert_with(|| json!({"slot": slot}));
        if let Some(lease) = lease.as_object_mut() {
            for pair in pairs {
                let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
                lease.insert(key.to_string(), typed(value));
            }
        }
        Ok(())
    })
}

fn cmd_get(pool: &Pool, args: &[String], out: &mut dyn Write) -> Result<i32, String> {
    let parsed = parse(args, &[])?;
    let [slot, field] = parsed.positionals.as_slice() else {
        return Err("get needs SLOT FIELD".to_string());
    };
    let leases = pool.leases()?;
    match leases.get(slot).and_then(|lease| lease.get(field)) {
        Some(value) => {
            let _ = writeln!(out, "{}", py_display(value));
            Ok(0)
        }
        None => Ok(1),
    }
}

fn cmd_list(pool: &Pool, out: &mut dyn Write) -> Result<i32, String> {
    let leases = pool.leases()?;
    for slot in slots() {
        if leases.contains_key(&slot) {
            let _ = writeln!(out, "{slot}");
        }
    }
    Ok(0)
}

fn cmd_drop(pool: &Pool, args: &[String]) -> Result<i32, String> {
    let parsed = parse(args, &[])?;
    if parsed.positionals.is_empty() {
        return Err("drop needs at least one SLOT".to_string());
    }
    pool.update(|leases| {
        for slot in &parsed.positionals {
            leases.remove(slot);
        }
        Ok(())
    })
}

fn cmd_touch(pool: &Pool, args: &[String], err: &mut dyn Write) -> Result<i32, String> {
    let parsed = parse(args, &[])?;
    let slot = match parsed.positionals.as_slice() {
        [] => None,
        [slot] => Some(slot.clone()),
        _ => return Err("touch takes at most one SLOT".to_string()),
    };
    let now = stamp(pool.now());
    pool.update(|leases| {
        if let Some(slot) = &slot
            && !leases.contains_key(slot)
        {
            let _ = writeln!(err, "cloud-pool: no lease for {slot}");
            return Err(1);
        }
        for (name, lease) in leases.iter_mut() {
            if slot.as_ref().is_none_or(|slot| slot == name)
                && let Some(lease) = lease.as_object_mut()
            {
                lease.insert("last_job_at".to_string(), json!(now));
            }
        }
        Ok(())
    })
}

fn cmd_hosts_json(pool: &Pool, args: &[String]) -> Result<i32, String> {
    let parsed = parse(
        args,
        &[
            ("--out", Kind::Value),
            ("--drop", Kind::Multi),
            ("--set", Kind::Multi),
        ],
    )?;
    let out = parsed.value("--out").ok_or("argument --out is required")?;
    let dropped = parsed.multi("--drop");
    let mut hosts = Map::new();
    for (slot, lease) in pool.leases()? {
        if dropped.contains(&slot) {
            continue;
        }
        let expiry = lease
            .get("lease_expires_at")
            .ok_or_else(|| format!("lease {slot} has no lease_expires_at"))?;
        hosts.insert(slot, expiry.clone());
    }
    for pair in parsed.multi("--set") {
        let (slot, expiry) = pair.split_once('=').unwrap_or((&pair, ""));
        hosts.insert(slot.to_string(), json!(expiry));
    }
    let body = pretty_sorted(&json!({"hosts": Value::Object(hosts)}));
    fs::write(out, format!("{body}\n")).map_err(|error| format!("{out}: {error}"))?;
    Ok(0)
}

fn minutes_duration(minutes: f64) -> Duration {
    #[allow(clippy::cast_possible_truncation)]
    Duration::microseconds((minutes * 60_000_000.0) as i64)
}

fn cmd_due(pool: &Pool, args: &[String], out: &mut dyn Write) -> Result<i32, String> {
    let parsed = parse(
        args,
        &[
            ("--idle-minutes", Kind::Value),
            ("--renew-window", Kind::Value),
        ],
    )?;
    let idle = parsed.parsed::<f64>("--idle-minutes")?.unwrap_or(30.0);
    let window = parsed.parsed::<f64>("--renew-window")?.unwrap_or(60.0);
    let now = pool.now();
    let mut leases: Vec<(String, Value)> = pool.leases()?.into_iter().collect();
    leases.sort_by(|a, b| a.0.cmp(&b.0));
    for (slot, lease) in leases {
        let expires = time_field(&lease, "lease_expires_at")?;
        let active = active_since(&lease)?;
        if now >= expires {
            let _ = writeln!(out, "{slot} expire");
        } else if now - active >= minutes_duration(idle) {
            let _ = writeln!(out, "{slot} idle");
        } else if expires - now <= minutes_duration(window)
            && lease.get("renewal").and_then(Value::as_str) != Some("refused")
        {
            let _ = writeln!(out, "{slot} renew");
        }
    }
    Ok(0)
}

fn cmd_status(pool: &Pool, args: &[String], out: &mut dyn Write) -> Result<i32, String> {
    let parsed = parse(args, &[("--json", Kind::Flag)])?;
    let now = pool.now();
    if parsed.flag("--json") {
        let _ = writeln!(out, "{}", pretty_sorted(&pool.load()?));
        return Ok(0);
    }
    let mut leases: Vec<(String, Value)> = pool.leases()?.into_iter().collect();
    if leases.is_empty() {
        let _ = writeln!(out, "pool: no hosts");
        return Ok(0);
    }
    leases.sort_by(|a, b| a.0.cmp(&b.0));
    let _ = writeln!(out, "pool: {} host(s) at {}", leases.len(), stamp(now));
    for (slot, lease) in leases {
        let left = floor_minutes(time_field(&lease, "lease_expires_at")? - now);
        let idle = floor_minutes(now - active_since(&lease)?);
        let renewal = shown_or_missing(&lease, "renewal", "scheduled");
        let note = if truthy(lease.get("renewal_note")) {
            format!(" ({})", shown(&lease, "renewal_note", ""))
        } else {
            String::new()
        };
        let _ = writeln!(
            out,
            "{slot} {} {} {} lease {} until {} ({left} min left) idle {idle} min renewal {renewal}{note} reservation {}",
            shown(&lease, "instance_id", "-"),
            shown(&lease, "host", "-"),
            shown_or_missing(&lease, "state", "ready"),
            shown_or_missing(&lease, "lease_no", "1"),
            text(&lease, "lease_expires_at")?,
            shown_or_missing(&lease, "reservation_id", "-"),
        );
    }
    Ok(0)
}

fn cmd_shift(args: &[String], out: &mut dyn Write) -> Result<i32, String> {
    let parsed = parse(args, &[])?;
    let [time, minutes] = parsed.positionals.as_slice() else {
        return Err("shift needs TIME MINUTES".to_string());
    };
    let minutes: i64 = minutes
        .parse()
        .map_err(|_| format!("invalid minutes: '{minutes}'"))?;
    let _ = writeln!(
        out,
        "{}",
        stamp(parse_stamp(time)? + Duration::minutes(minutes))
    );
    Ok(0)
}

fn cmd_until(pool: &Pool, args: &[String], out: &mut dyn Write) -> Result<i32, String> {
    let parsed = parse(args, &[])?;
    let [time] = parsed.positionals.as_slice() else {
        return Err("until needs TIME".to_string());
    };
    let seconds = (parse_stamp(time)? - pool.now()).num_seconds();
    let _ = writeln!(out, "{}", (seconds + 59).div_euclid(60).max(1));
    Ok(0)
}

fn cmd_output_field(
    args: &[String],
    stdin: &mut dyn Read,
    out: &mut dyn Write,
) -> Result<i32, String> {
    let parsed = parse(args, &[])?;
    let [slot, field] = parsed.positionals.as_slice() else {
        return Err("output-field needs SLOT FIELD".to_string());
    };
    let mut input = String::new();
    if stdin.read_to_string(&mut input).is_err() {
        return Ok(1);
    }
    let value = serde_json::from_str::<Value>(&input)
        .ok()
        .and_then(|data| data.get(slot)?.get(field).cloned());
    match value {
        Some(value) => {
            let _ = writeln!(out, "{}", py_display(&value));
            Ok(0)
        }
        None => Ok(1),
    }
}

fn cmd_instance_state(stdin: &mut dyn Read, out: &mut dyn Write) -> i32 {
    let mut input = String::new();
    let state = stdin
        .read_to_string(&mut input)
        .ok()
        .and_then(|_| serde_json::from_str::<Value>(&input).ok())
        .and_then(|data| {
            data.get("Reservations")?
                .as_array()?
                .iter()
                .filter_map(|reservation| reservation.get("Instances")?.as_array())
                .flatten()
                .next()?
                .get("State")?
                .get("Name")
                .map(py_display)
        });
    let _ = writeln!(out, "{}", state.unwrap_or_else(|| "unknown".to_string()));
    0
}

fn cmd_cost(pool: &Pool, args: &[String], out: &mut dyn Write) -> Result<i32, String> {
    let parsed = parse(args, &[])?;
    let [slot, end] = parsed.positionals.as_slice() else {
        return Err("cost needs SLOT END".to_string());
    };
    let leases = pool.leases()?;
    let lease = leases
        .get(slot)
        .ok_or_else(|| format!("no lease for {slot}"))?;
    let elapsed = (parse_stamp(end)? - time_field(lease, "lease_start")?)
        .num_seconds()
        .max(0);
    let hourly = lease
        .get("hourly_usd")
        .and_then(json_float)
        .ok_or("lease has no numeric hourly_usd")?;
    #[allow(clippy::cast_precision_loss)]
    let cost = elapsed as f64 / 3600.0 * hourly;
    let _ = writeln!(out, "{elapsed} {cost:.4}");
    Ok(0)
}

/// `rsi-cloud pool-state [--file F] [--now T] <command> ...`
pub fn run(
    args: &[String],
    stdin: &mut dyn Read,
    stdout: &mut dyn Write,
    stderr: &mut dyn Write,
) -> i32 {
    // The global options come before the command, as with argparse.
    let mut file: Option<PathBuf> = None;
    let mut now: Option<String> = None;
    let mut index = 0;
    while index < args.len() {
        let arg = args[index].as_str();
        let (name, inline) = arg
            .split_once('=')
            .map_or((arg, None), |(n, v)| (n, Some(v)));
        match name {
            "--file" | "--now" => {
                let value = if let Some(value) = inline {
                    index += 1;
                    value.to_string()
                } else if let Some(value) = args.get(index + 1) {
                    index += 2;
                    value.clone()
                } else {
                    let _ = writeln!(
                        stderr,
                        "rsi-cloud pool-state: argument {name}: expected one argument"
                    );
                    return 2;
                };
                if name == "--file" {
                    file = Some(PathBuf::from(value));
                } else {
                    now = Some(value);
                }
            }
            _ => break,
        }
    }
    let Some(command) = args.get(index) else {
        let _ = writeln!(stderr, "rsi-cloud pool-state: a command is required");
        return 2;
    };
    let rest = &args[index + 1..];
    let now = match now.as_deref().map(parse_stamp).transpose() {
        Ok(now) => now,
        Err(why) => {
            let _ = writeln!(stderr, "rsi-cloud pool-state: {why}");
            return 1;
        }
    };
    let pool = Pool {
        path: file.unwrap_or_else(|| default_cloud_file("pool.json")),
        now,
    };
    let result = match command.as_str() {
        "slots" => cmd_slots(&pool, rest, stdout, stderr),
        "put" => cmd_put(&pool, rest, stderr),
        "get" => cmd_get(&pool, rest, stdout),
        "list" => cmd_list(&pool, stdout),
        "drop" => cmd_drop(&pool, rest),
        "touch" => cmd_touch(&pool, rest, stderr),
        "hosts-json" => cmd_hosts_json(&pool, rest),
        "due" => cmd_due(&pool, rest, stdout),
        "status" => cmd_status(&pool, rest, stdout),
        "shift" => cmd_shift(rest, stdout),
        "until" => cmd_until(&pool, rest, stdout),
        "output-field" => cmd_output_field(rest, stdin, stdout),
        "instance-state" => Ok(cmd_instance_state(stdin, stdout)),
        "cost" => cmd_cost(&pool, rest, stdout),
        other => {
            let _ = writeln!(stderr, "rsi-cloud pool-state: unknown command {other}");
            return 2;
        }
    };
    match result {
        Ok(code) => code,
        Err(why) => {
            let _ = writeln!(stderr, "rsi-cloud pool-state: {why}");
            1
        }
    }
}
