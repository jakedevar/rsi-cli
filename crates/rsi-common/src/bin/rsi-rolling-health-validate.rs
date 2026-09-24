//! Validate and canonicalize one rolling-health v1 record from stdin.

use rsi_common::rolling_health::{
    ComparisonRequest, Profile, Record, compare, jcs, profile_id, record_digest,
    shard_artifact_name, validate, validate_shard_artifacts,
};
use std::collections::BTreeMap;
use std::io::Read as _;
use std::process::ExitCode;

fn main() -> ExitCode {
    let mode = std::env::args().nth(1).unwrap_or_default();
    let mut input = Vec::new();
    if let Err(error) = std::io::stdin().read_to_end(&mut input) {
        eprintln!("I/O error: {error}");
        return ExitCode::from(1);
    }
    if mode == "--profile-id" {
        let profile: Profile = match serde_json::from_slice(&input) {
            Ok(profile) => profile,
            Err(error) => {
                eprintln!("invalid profile: {error}");
                return ExitCode::from(2);
            }
        };
        return match profile_id(&profile) {
            Ok(id) => {
                println!("{id}");
                ExitCode::SUCCESS
            }
            Err(error) => {
                eprintln!("invalid profile: {error}");
                ExitCode::from(2)
            }
        };
    }
    if mode == "--seal-record" {
        let mut value: serde_json::Value = match serde_json::from_slice(&input) {
            Ok(value) => value,
            Err(error) => {
                eprintln!("invalid rolling-health record: {error}");
                return ExitCode::from(2);
            }
        };
        let Some(object) = value.as_object_mut() else {
            eprintln!("invalid rolling-health record: expected object");
            return ExitCode::from(2);
        };
        object.remove("record_digest");
        let record: Record = match serde_json::from_value(value) {
            Ok(record) => record,
            Err(error) => {
                eprintln!("invalid rolling-health record: {error}");
                return ExitCode::from(2);
            }
        };
        let digest = match record_digest(&record) {
            Ok(digest) => digest,
            Err(error) => {
                eprintln!("cannot digest record: {error}");
                return ExitCode::from(2);
            }
        };
        let mut sealed = record;
        sealed.record_digest = Some(digest);
        if let Err(error) = validate(&sealed) {
            eprintln!("invalid rolling-health record: {error}");
            return ExitCode::from(2);
        }
        return match jcs(&sealed) {
            Ok(bytes) => {
                println!("{}", String::from_utf8_lossy(&bytes));
                ExitCode::SUCCESS
            }
            Err(error) => {
                eprintln!("canonicalization failed: {error}");
                ExitCode::from(2)
            }
        };
    }
    if mode == "--record-digest" {
        let record: Record = match serde_json::from_slice(&input) {
            Ok(record) => record,
            Err(error) => {
                eprintln!("invalid rolling-health record: {error}");
                return ExitCode::from(2);
            }
        };
        return match record_digest(&record) {
            Ok(digest) => {
                println!("{digest}");
                ExitCode::SUCCESS
            }
            Err(error) => {
                eprintln!("cannot digest record: {error}");
                ExitCode::from(2)
            }
        };
    }
    if mode == "--canonicalize" {
        let value: serde_json::Value = match serde_json::from_slice(&input) {
            Ok(value) => value,
            Err(error) => {
                eprintln!("invalid JSON: {error}");
                return ExitCode::from(2);
            }
        };
        return match jcs(&value) {
            Ok(bytes) => {
                println!("{}", String::from_utf8_lossy(&bytes));
                ExitCode::SUCCESS
            }
            Err(error) => {
                eprintln!("canonicalization failed: {error}");
                ExitCode::from(2)
            }
        };
    }
    if mode == "--verify-artifacts" {
        let Some(root) = std::env::args().nth(2).map(std::path::PathBuf::from) else {
            eprintln!("--verify-artifacts requires a run directory");
            return ExitCode::from(1);
        };
        let record: Record = match serde_json::from_slice(&input) {
            Ok(record) => record,
            Err(error) => {
                eprintln!("invalid rolling-health record: {error}");
                return ExitCode::from(2);
            }
        };
        let mut artifacts = BTreeMap::new();
        let shard_root = root.join("shards");
        if std::fs::symlink_metadata(&shard_root)
            .is_ok_and(|metadata| metadata.file_type().is_symlink())
        {
            eprintln!("shard artifact directory must not be a symlink");
            return ExitCode::from(2);
        }
        for shard in &record.shards {
            let relative = match shard_artifact_name(&shard.crate_name) {
                Ok(path) => path,
                Err(error) => {
                    eprintln!("invalid shard path: {error}");
                    return ExitCode::from(2);
                }
            };
            let path = root.join(relative);
            if std::fs::symlink_metadata(&path)
                .is_ok_and(|metadata| metadata.file_type().is_symlink() || !metadata.is_file())
            {
                eprintln!("shard artifact must be a regular file: {}", path.display());
                return ExitCode::from(2);
            }
            match std::fs::read(&path) {
                Ok(bytes) => {
                    artifacts.insert(format!("shards/{}.json", shard.crate_name), bytes);
                }
                Err(error) => {
                    eprintln!("cannot read shard artifact {}: {error}", path.display());
                    return ExitCode::from(2);
                }
            }
        }
        return match validate_shard_artifacts(&record, &artifacts) {
            Ok(()) => {
                println!("valid");
                ExitCode::SUCCESS
            }
            Err(error) => {
                eprintln!("invalid rolling-health artifacts: {error}");
                ExitCode::from(2)
            }
        };
    }
    if mode == "--compare" {
        let request: ComparisonRequest = match serde_json::from_slice(&input) {
            Ok(request) => request,
            Err(error) => {
                eprintln!("invalid comparison request: {error}");
                return ExitCode::from(2);
            }
        };
        let comparison = match compare(&request) {
            Ok(comparison) => comparison,
            Err(error) => {
                eprintln!("comparison unavailable: {error}");
                return ExitCode::from(3);
            }
        };
        return match jcs(&comparison) {
            Ok(bytes) => {
                println!("{}", String::from_utf8_lossy(&bytes));
                ExitCode::SUCCESS
            }
            Err(error) => {
                eprintln!("comparison serialization failed: {error}");
                ExitCode::from(2)
            }
        };
    }
    let record: Record = match serde_json::from_slice(&input) {
        Ok(record) => record,
        Err(error) => {
            eprintln!("invalid rolling-health record: {error}");
            return ExitCode::from(2);
        }
    };
    if let Err(error) = validate(&record) {
        eprintln!("invalid rolling-health record: {error}");
        return ExitCode::from(2);
    }
    match jcs(&record) {
        Ok(bytes) => {
            println!("{}", String::from_utf8_lossy(&bytes));
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("canonicalization failed: {error}");
            ExitCode::from(2)
        }
    }
}
