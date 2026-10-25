//! Credential failures are exercised in isolated child environments, never
//! against a real profile, endpoint or AWS account.
use std::process::Command;

#[test]
fn credential_errors_are_nonzero_with_empty_stdout_and_static_diagnostics() {
    let home = tempfile::tempdir().unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_rsi-cloud"))
        .args([
            "aws",
            "ec2",
            "describe-instances",
            "--region",
            "us-west-1",
            "--filters",
            "Name=tag:Service,Values=rsi-pool",
            "--query",
            "Reservations[].Instances[].InstanceId",
            "--output",
            "text",
        ])
        .env_clear()
        .env("HOME", home.path())
        .env("AWS_ACCESS_KEY_ID", "test-partial-access-value")
        .env("AWS_SESSION_TOKEN", "test-session-token-value")
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(output.stdout, b"");
    assert_eq!(
        output.stderr,
        b"rsi-cloud: incomplete AWS environment credentials\n"
    );
}
