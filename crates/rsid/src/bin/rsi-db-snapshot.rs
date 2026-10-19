//! `rsi-db-snapshot`: take a consistent copy of the daemon database, read-only,
//! and print how long it took (#2530).
//!
//! The schema's CHECK constraints call SQL functions only the daemon registers,
//! so `sqlite3 ... "VACUUM INTO ..."` fails with `no such function`, and the
//! CLI `.backup` restarts on every write by a live daemon. This tool registers
//! the functions, opens the source read-only and copies it in one read
//! transaction, so it is safe against a running daemon and writes nothing next
//! to the source.

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use rsid::store::migration_backup::{DatabaseSnapshot, DatabaseSnapshotMode, snapshot_database};

const USAGE: &str = "\
rsi-db-snapshot --out <PATH> [--source <DB>] [--vacuum] [--control]

Take a consistent, read-only copy of the rsid database and print one JSON line
with the method, page counts, bytes and elapsed milliseconds. Never writes next
to the source and never overwrites <PATH>.

  --out <PATH>     Destination file; must not exist.
  --source <DB>    Database to copy. Default: rsi.db next to the daemon socket
                   ($RSI_DAEMON_SOCKET_PATH, else ~/.rsi/daemon.sock). A
                   daemon keeps its database NEXT TO ITS SOCKET.
  --vacuum         Copy with VACUUM INTO instead of the page copy.
  --control        Also time VACUUM INTO into a scratch file beside <PATH>
                   (deleted afterwards): the #2384 control measurement.
";

struct Args {
    out: PathBuf,
    source: PathBuf,
    mode: DatabaseSnapshotMode,
    control: bool,
}

fn default_source() -> PathBuf {
    rsi_common::identity::resolve_socket_path()
        .parent()
        .map_or_else(|| PathBuf::from("."), Path::to_path_buf)
        .join(rsi_common::identity::DB_FILENAME)
}

fn parse(args: &[String]) -> Result<Option<Args>, String> {
    let mut out = None;
    let mut source = None;
    let mut mode = DatabaseSnapshotMode::PageCopy;
    let mut control = false;
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "-h" | "--help" => return Ok(None),
            "--out" => out = Some(PathBuf::from(value(&mut iter, "--out")?)),
            "--source" => source = Some(PathBuf::from(value(&mut iter, "--source")?)),
            "--vacuum" => mode = DatabaseSnapshotMode::VacuumInto,
            "--control" => control = true,
            other => return Err(format!("unknown argument `{other}`")),
        }
    }
    Ok(Some(Args {
        out: out.ok_or("--out <PATH> is required")?,
        source: source.unwrap_or_else(default_source),
        mode,
        control,
    }))
}

fn value<'a>(iter: &mut std::slice::Iter<'a, String>, flag: &str) -> Result<&'a String, String> {
    iter.next()
        .ok_or_else(|| format!("{flag} requires a value"))
}

fn report(snapshot: &DatabaseSnapshot) -> serde_json::Value {
    serde_json::json!({
        "method": snapshot.method,
        "page_count": snapshot.page_count,
        "freelist_count": snapshot.freelist_count,
        "bytes": snapshot.bytes,
        "elapsed_ms": u64::try_from(snapshot.elapsed.as_millis()).unwrap_or(u64::MAX),
    })
}

fn run(args: &Args) -> Result<serde_json::Value, String> {
    let copy = snapshot_database(&args.source, &args.out, args.mode)
        .map_err(|error| format!("snapshot failed: {error}"))?;
    let mut line = serde_json::json!({
        "source": args.source,
        "out": args.out,
        "snapshot": report(&copy),
    });
    if args.control {
        let mut scratch = args.out.as_os_str().to_os_string();
        scratch.push(".vacuum-control");
        let scratch = PathBuf::from(scratch);
        let control = snapshot_database(&args.source, &scratch, DatabaseSnapshotMode::VacuumInto);
        let _ = std::fs::remove_file(&scratch);
        let control = control.map_err(|error| format!("VACUUM INTO control failed: {error}"))?;
        line["vacuum_into_control"] = report(&control);
    }
    Ok(line)
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let parsed = match parse(&args) {
        Ok(Some(parsed)) => parsed,
        Ok(None) => {
            print!("{USAGE}");
            return ExitCode::SUCCESS;
        }
        Err(error) => {
            eprintln!("{error}\n\n{USAGE}");
            return ExitCode::from(2);
        }
    };
    match run(&parsed) {
        Ok(line) => {
            println!("{line}");
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("{error}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn strings(items: &[&str]) -> Vec<String> {
        items.iter().map(|item| (*item).to_string()).collect()
    }

    #[test]
    fn out_is_required_and_flags_parse() {
        assert!(parse(&strings(&["--vacuum"])).is_err());
        assert!(parse(&strings(&["--out"])).is_err());
        assert!(parse(&strings(&["--bogus"])).is_err());
        assert!(parse(&strings(&["--help"])).unwrap().is_none());
        let args = parse(&strings(&[
            "--out",
            "/x/o.db",
            "--source",
            "/y/rsi.db",
            "--vacuum",
            "--control",
        ]))
        .unwrap()
        .unwrap();
        assert_eq!(args.out, PathBuf::from("/x/o.db"));
        assert_eq!(args.source, PathBuf::from("/y/rsi.db"));
        assert_eq!(args.mode, DatabaseSnapshotMode::VacuumInto);
        assert!(args.control);
    }

    #[test]
    fn run_with_control_leaves_only_the_output() {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("rsi.db");
        {
            let conn = rusqlite::Connection::open(&source).unwrap();
            conn.execute_batch("CREATE TABLE t (x); INSERT INTO t VALUES (1);")
                .unwrap();
        }
        let out = temp.path().join("copy.db");
        let args = Args {
            out: out.clone(),
            source,
            mode: DatabaseSnapshotMode::PageCopy,
            control: true,
        };
        let line = run(&args).unwrap();
        assert_eq!(line["snapshot"]["method"], "online_backup");
        assert_eq!(line["vacuum_into_control"]["method"], "vacuum_into");
        assert!(out.exists());
        assert!(!temp.path().join("copy.db.vacuum-control").exists());
    }
}
