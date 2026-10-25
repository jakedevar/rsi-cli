//! Test fixture, never installed: one executable that plays `terraform`, `aws`,
//! `ssh` or `scp` for `scripts/cloud-pool.sh`, `scripts/cloud-gate.sh` and
//! `scripts/cloud-build.sh` under test, chosen by the name it is
//! invoked as (the tests symlink it as each tool). Every call is logged as one
//! JSON line to `$FAKE_LOG`. No real cloud tool is involved.

use std::collections::BTreeMap;
use std::io::Write;
use std::path::Path;

use serde_json::{Value, json};

fn log(entry: &Value) {
    let Ok(path) = std::env::var("FAKE_LOG") else {
        return;
    };
    if let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    {
        let _ = writeln!(file, "{entry}");
    }
}

fn read_json(path: &str) -> Option<Value> {
    serde_json::from_str(&std::fs::read_to_string(path).ok()?).ok()
}

fn terraform(args: &[String]) -> i32 {
    let mut state = String::new();
    let mut var_files = Vec::new();
    let mut rest: Vec<&str> = Vec::new();
    for arg in args {
        if arg.starts_with("-chdir=") {
            continue;
        }
        if let Some(path) = arg.strip_prefix("-state=") {
            state = path.to_string();
        } else if let Some(path) = arg.strip_prefix("-var-file=") {
            var_files.push(path.to_string());
        } else {
            rest.push(arg);
        }
    }
    let sub = rest.first().copied().unwrap_or("");
    let mut hosts = json!({});
    for path in &var_files {
        if path.ends_with(".json")
            && let Some(data) = read_json(path)
        {
            hosts = data.get("hosts").cloned().unwrap_or_else(|| json!({}));
        }
    }
    let ledger = std::env::var("FAKE_LEDGER")
        .ok()
        .and_then(|path| std::fs::read_to_string(path).ok())
        .unwrap_or_default();
    log(
        &json!({"tool": "terraform", "sub": sub, "state": state, "hosts": hosts, "ledger": ledger, "args": args}),
    );

    let make = |slot: &str, expiry: &Value| {
        let n: u32 = slot
            .rsplit('-')
            .next()
            .and_then(|n| n.parse().ok())
            .unwrap_or(0);
        json!({"instance_id": format!("i-0{n:016x}"), "public_ip": format!("192.0.2.{n}"), "expires_at": expiry})
    };
    let planned = || {
        let mut map = BTreeMap::new();
        if let Some(hosts) = hosts.as_object() {
            for (slot, expiry) in hosts {
                map.insert(slot.clone(), make(slot, expiry));
            }
        }
        json!({"hosts": map})
    };
    match sub {
        "init" => 0,
        "apply" => {
            if hosts.as_object().is_some_and(|hosts| hosts.len() > 6) {
                eprintln!("Error: The pool holds at most 6 hosts.");
                return 1;
            }
            let marker = format!("{state}.failed-once");
            if let Ok(mode) = std::env::var("FAKE_TF_FAIL_FIRST_APPLY")
                && !mode.is_empty()
                && !Path::new(&marker).exists()
            {
                let _ = std::fs::File::create(&marker);
                if mode == "partial" {
                    let _ = std::fs::write(&state, planned().to_string());
                }
                return 1;
            }
            let _ = std::fs::write(&state, planned().to_string());
            0
        }
        "destroy" => {
            let _ = std::fs::write(&state, json!({"hosts": {}}).to_string());
            0
        }
        "output" => {
            if rest.contains(&"-json") {
                let current = read_json(&state)
                    .and_then(|data| data.get("hosts").cloned())
                    .unwrap_or_else(|| json!({}));
                println!("{current}");
            } else if rest.contains(&"instance_type") {
                print!("c7i.8xlarge");
            } else if rest.contains(&"instance_id") {
                // The single gate host `scripts/cloud-gate.sh` applies.
                print!("i-0gatefake000001");
            } else if rest.contains(&"public_ip") {
                print!("192.0.2.1");
            } else if rest.contains(&"run_as") {
                print!("rsi");
            }
            0
        }
        _ => 2,
    }
}

fn aws(args: &[String]) -> i32 {
    log(&json!({"tool": "aws", "args": args}));
    match (
        args.first().map(String::as_str),
        args.get(1).map(String::as_str),
    ) {
        (Some("ec2"), Some("describe-instances")) => {
            if std::env::var_os("FAKE_AWS_ERROR").is_some() {
                // Partial output must be discarded by the shell on error.
                println!("i-partial");
                return 1;
            }
            if let Some(at) = args.iter().position(|arg| arg == "--instance-ids") {
                let id = args.get(at + 1).cloned().unwrap_or_default();
                let states = std::env::var("FAKE_AWS_STATES")
                    .ok()
                    .and_then(|path| read_json(&path))
                    .unwrap_or_else(|| json!({}));
                let entry = states
                    .get(&id)
                    .cloned()
                    .unwrap_or_else(|| json!({"state": "running"}));
                let mut instance = json!({"InstanceId": id, "State": {"Name": entry["state"]}});
                if let Some(reason) = entry.get("reason").and_then(Value::as_str)
                    && !reason.is_empty()
                {
                    instance["StateReason"] = json!({"Code": reason});
                }
                println!("{}", json!({"Reservations": [{"Instances": [instance]}]}));
            } else {
                println!(
                    "{}",
                    std::env::var("FAKE_AWS_LEFTOVER").unwrap_or_else(|_| "None".into())
                );
            }
        }
        (Some("ec2"), Some("get-console-output")) => {
            println!("ssh-ed25519 256 SHA256:fakefingerprint root@host (ED25519)");
        }
        _ => {}
    }
    0
}

fn ssh(args: &[String]) -> i32 {
    log(&json!({"tool": "ssh", "args": args}));
    if std::env::var("FAKE_SSH_FAIL_SHUTDOWN").is_ok_and(|v| !v.is_empty())
        && args.iter().any(|arg| arg.contains("shutdown"))
    {
        return 255;
    }
    // The cloud build lane pipes its remote script to `bash -s`: play that run
    // by printing the canned output and exiting with the canned code.
    if args.iter().any(|arg| arg.contains("bash -s")) {
        if let Some(text) = std::env::var("FAKE_SSH_OUTPUT")
            .ok()
            .and_then(|path| std::fs::read_to_string(path).ok())
        {
            print!("{text}");
        }
        return std::env::var("FAKE_SSH_EXIT")
            .ok()
            .and_then(|code| code.parse().ok())
            .unwrap_or(0);
    }
    0
}

fn scp(args: &[String]) -> i32 {
    log(&json!({"tool": "scp", "args": args}));
    0
}

fn main() {
    let argv: Vec<String> = std::env::args().collect();
    let name = argv
        .first()
        .and_then(|path| Path::new(path).file_name())
        .and_then(|name| name.to_str())
        .unwrap_or("")
        .to_string();
    let args = &argv[1..];
    let code = match name.as_str() {
        "rsi-cloud" => {
            if args.first().is_some_and(|arg| arg == "aws") {
                aws(&args[1..])
            } else {
                std::process::Command::new(
                    std::env::var_os("RSI_CLOUD_REAL").expect("test real binary"),
                )
                .args(args)
                .status()
                .expect("test rsi-cloud")
                .code()
                .unwrap_or(1)
            }
        }
        "terraform" => terraform(args),
        "aws" => aws(args),
        "ssh" => ssh(args),
        "scp" => scp(args),
        other => {
            eprintln!(
                "rsi-cloud-fakes: invoked as {other}; symlink it as terraform, aws, ssh or scp"
            );
            2
        }
    };
    std::process::exit(code);
}
