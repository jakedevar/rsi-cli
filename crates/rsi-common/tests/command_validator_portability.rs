//! Contract regression for cross-repository validator availability.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::Value;

#[derive(Debug, Eq, PartialEq)]
enum Availability {
    Present,
    Buildable,
    Path,
    Unavailable,
    ProbeError,
}

#[derive(Debug, Eq, PartialEq)]
enum AvailabilityDecision {
    RequireValidator,
    SkipUnavailable,
    Fail,
}

#[derive(Debug, Eq, PartialEq)]
enum ValidatorDisposition {
    Accepted,
    MarkdownFallback,
    Blocker,
}

fn metadata(manifest_path: &Path) -> Value {
    let output = Command::new("cargo")
        .args([
            "metadata",
            "--no-deps",
            "--format-version",
            "1",
            "--manifest-path",
        ])
        .arg(manifest_path)
        .output()
        .expect("run cargo metadata");
    assert!(
        output.status.success(),
        "cargo metadata failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).expect("metadata is JSON")
}

fn has_target(metadata: &Value, validator: &str) -> bool {
    metadata["packages"].as_array().is_some_and(|packages| {
        packages.iter().any(|package| {
            package["name"] == "rsi-common"
                && package["targets"].as_array().is_some_and(|targets| {
                    targets.iter().any(|target| {
                        target["name"] == validator
                            && target["kind"]
                                .as_array()
                                .is_some_and(|kinds| kinds.iter().any(|kind| kind == "bin"))
                    })
                })
        })
    })
}

fn classify(
    repo_executable: bool,
    metadata: Result<&Value, ()>,
    path_executable: bool,
    validator: &str,
) -> Availability {
    if repo_executable {
        Availability::Present
    } else {
        match metadata {
            Ok(metadata) if has_target(metadata, validator) => Availability::Buildable,
            Ok(_) if path_executable => Availability::Path,
            Ok(_) => Availability::Unavailable,
            Err(()) if path_executable => Availability::Path,
            Err(()) => Availability::ProbeError,
        }
    }
}

fn availability_decision(
    availability: Availability,
    build_succeeded: bool,
) -> AvailabilityDecision {
    match availability {
        Availability::Present | Availability::Path => AvailabilityDecision::RequireValidator,
        Availability::Buildable if build_succeeded => AvailabilityDecision::RequireValidator,
        Availability::Buildable | Availability::ProbeError => AvailabilityDecision::Fail,
        Availability::Unavailable => AvailabilityDecision::SkipUnavailable,
    }
}

fn validator_disposition(resume: bool, exit_code: i32) -> ValidatorDisposition {
    match (resume, exit_code) {
        (_, 0) => ValidatorDisposition::Accepted,
        (false, 1 | 2) => ValidatorDisposition::MarkdownFallback,
        (true, 1 | 2) => ValidatorDisposition::Blocker,
        _ => ValidatorDisposition::Blocker,
    }
}

fn copy_dir(source: &Path, destination: &Path) {
    fs::create_dir_all(destination).expect("create fixture destination");
    for entry in fs::read_dir(source).expect("read fixture") {
        let entry = entry.expect("fixture entry");
        let destination = destination.join(entry.file_name());
        if entry.file_type().expect("fixture type").is_dir() {
            copy_dir(&entry.path(), &destination);
        } else {
            fs::copy(entry.path(), destination).expect("copy fixture file");
        }
    }
}

fn repository_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("workspace root")
        .to_path_buf()
}

fn policy_block(command: &str, validator: &str) -> String {
    let path = repository_root().join(".claude/commands").join(command);
    let text = fs::read_to_string(path).expect("read canonical command");
    let start = text.find("**Present/repo-local:**").expect("policy starts");
    let end = text[start..]
        .find("When a validator is selected")
        .or_else(|| text[start..].find("2. Run the selected"))
        .map(|offset| start + offset)
        .expect("policy ends");
    let normalized = text[start..end]
        .replace(validator, "<validator>")
        .lines()
        .map(|line| {
            let line = line.trim();
            if line.starts_with("4. **Unavailable:**") {
                "4. **Unavailable:** <command-specific safe continuation>".to_string()
            } else {
                line.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join("\n");
    normalized.trim().to_string()
}

#[test]
fn consumer_fixture_is_unavailable_without_build_or_path() {
    let fixture = repository_root()
        .join("crates/rsi-common/tests/fixtures/validator_portability/consumer_no_rsi_common");
    let temp = tempfile::tempdir().expect("temporary fixture directory");
    let copied = temp.path().join("consumer");
    copy_dir(&fixture, &copied);
    let metadata = metadata(&copied.join("Cargo.toml"));

    for validator in ["rsi-research-validate", "rsi-handoff-validate"] {
        assert!(
            !has_target(&metadata, validator),
            "fixture must not expose {validator}"
        );
        assert_eq!(
            classify(false, Ok(&metadata), false, validator),
            Availability::Unavailable,
            "consumer must not attempt a validator build"
        );
    }
}

#[test]
fn rsi_workspace_requires_buildable_validators_and_preserves_failures() {
    let root = repository_root();
    let metadata = metadata(&root.join("Cargo.toml"));

    for validator in ["rsi-research-validate", "rsi-handoff-validate"] {
        assert!(
            has_target(&metadata, validator),
            "workspace must expose {validator}"
        );
        assert_eq!(
            classify(false, Ok(&metadata), false, validator),
            Availability::Buildable
        );
        assert_eq!(
            classify(true, Ok(&metadata), false, validator),
            Availability::Present
        );
    }

    assert_eq!(
        classify(false, Err(()), false, "rsi-research-validate"),
        Availability::ProbeError
    );
    assert_eq!(
        classify(false, Err(()), true, "rsi-research-validate"),
        Availability::Path
    );
}

#[test]
fn canonical_commands_share_order_and_preserve_continuation_boundaries() {
    let research = policy_block("research.md", "rsi-research-validate");
    let team = policy_block("team_research.md", "rsi-research-validate");
    let resume = policy_block("resume_handoff.md", "rsi-handoff-validate");
    assert_eq!(
        research, team,
        "research commands must share availability policy"
    );
    assert_eq!(
        research, resume,
        "all three commands must share the same five-state availability machine"
    );

    for policy in [&research, &team, &resume] {
        let mut position = 0;
        for state in [
            "Present/repo-local",
            "Buildable/repo-local",
            "Present/PATH",
            "Unavailable",
            "Probe error",
        ] {
            let found = policy[position..]
                .find(state)
                .expect("ordered availability state");
            position += found + state.len();
        }
        assert!(
            policy.contains(
                "build failure remains a required-validator failure; it is never `SKIPPED`"
            )
        );
        assert!(policy.contains("capability-probe error as a failure. It is never `SKIPPED`"));
    }

    let research_source =
        fs::read_to_string(repository_root().join(".claude/commands/research.md")).unwrap();
    let team_source =
        fs::read_to_string(repository_root().join(".claude/commands/team_research.md")).unwrap();
    let resume_source =
        fs::read_to_string(repository_root().join(".claude/commands/resume_handoff.md")).unwrap();
    for source in [&research_source, &team_source] {
        assert!(source.contains("Do not create a new JSON sidecar"));
        assert!(
            source.contains("do not overwrite, validate, trust, or delete a pre-existing sidecar")
        );
        assert!(source.contains("json_companion_status: invalid"));
        assert!(source.contains("same fallback as exit 2"));
        assert!(source.contains("Validator exit failures are never `SKIPPED`"));
        assert!(source.contains("markdown"));
    }
    assert!(resume_source.contains(
        "Do not write a `VALIDATOR_REJECTED` handoff; proceed immediately to Step 1 extraction."
    ));
    assert!(resume_source.contains("write a blocker handoff"));
    assert!(resume_source.contains("standard blocker (I/O error or argument issue). Halt"));
    assert!(resume_source.contains("Validator exit failures are never `SKIPPED`"));

    for availability in [
        Availability::Present,
        Availability::Buildable,
        Availability::Path,
    ] {
        assert_eq!(
            availability_decision(availability, true),
            AvailabilityDecision::RequireValidator
        );
    }
    assert_eq!(
        availability_decision(Availability::Unavailable, true),
        AvailabilityDecision::SkipUnavailable
    );
    assert_eq!(
        availability_decision(Availability::ProbeError, true),
        AvailabilityDecision::Fail
    );
    assert_eq!(
        availability_decision(Availability::Buildable, false),
        AvailabilityDecision::Fail,
        "a failed RSI validator build must never be treated as unavailable"
    );
    for exit_code in [1, 2] {
        assert_eq!(
            validator_disposition(false, exit_code),
            ValidatorDisposition::MarkdownFallback
        );
        assert_eq!(
            validator_disposition(true, exit_code),
            ValidatorDisposition::Blocker
        );
    }
    assert_eq!(
        validator_disposition(false, 0),
        ValidatorDisposition::Accepted
    );
}
