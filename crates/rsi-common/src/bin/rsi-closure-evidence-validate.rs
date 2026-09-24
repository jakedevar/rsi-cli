//! Same-parser pre-import validator for Closure review/manifest evidence.

use std::io::Write as _;
use std::process::ExitCode;

use rsi_common::{
    ClosureEvidenceExpectationV1, ClosureGitShaV1, Sha256Digest, parse_canonical_uuid,
    validate_closure_evidence_bundle_v1,
};

fn usage() {
    eprintln!(
        "usage: rsi-closure-evidence-validate \\\n         --source-head <sha> --reviewer-session-id <uuid> \\\n         --model-invocation-id <uuid> --review-policy-digest <sha256:digest> \\\n         --review <review-v1.json> --manifest <manifest-v2.md>"
    );
}

fn main() -> ExitCode {
    let mut values = std::collections::BTreeMap::<String, String>::new();
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        if matches!(arg.as_str(), "-h" | "--help") {
            usage();
            return ExitCode::SUCCESS;
        }
        if !arg.starts_with("--") {
            eprintln!("unexpected positional argument `{arg}`");
            usage();
            return ExitCode::from(1);
        }
        let Some(value) = args.next() else {
            eprintln!("{arg} requires a value");
            return ExitCode::from(1);
        };
        if values.insert(arg.clone(), value).is_some() {
            eprintln!("duplicate argument `{arg}`");
            return ExitCode::from(1);
        }
    }

    let required = |name: &str| -> Result<&str, String> {
        values
            .get(name)
            .map(String::as_str)
            .ok_or_else(|| format!("missing required argument `{name}`"))
    };
    let expectation = (|| {
        Ok::<_, String>(ClosureEvidenceExpectationV1 {
            source_head: ClosureGitShaV1::parse(required("--source-head")?)?,
            reviewer_session_id: parse_canonical_uuid(required("--reviewer-session-id")?)?,
            model_invocation_id: parse_canonical_uuid(required("--model-invocation-id")?)?,
            review_policy_digest: Sha256Digest::parse(required("--review-policy-digest")?)?,
        })
    })();
    let expectation = match expectation {
        Ok(value) => value,
        Err(error) => {
            eprintln!("argument error: {error}");
            usage();
            return ExitCode::from(1);
        }
    };
    let review_path = match required("--review") {
        Ok(path) => path,
        Err(error) => {
            eprintln!("{error}");
            return ExitCode::from(1);
        }
    };
    let manifest_path = match required("--manifest") {
        Ok(path) => path,
        Err(error) => {
            eprintln!("{error}");
            return ExitCode::from(1);
        }
    };
    let review = match std::fs::read_to_string(review_path) {
        Ok(value) => value,
        Err(error) => {
            eprintln!("cannot read review `{review_path}`: {error}");
            return ExitCode::from(1);
        }
    };
    let manifest = match std::fs::read_to_string(manifest_path) {
        Ok(value) => value,
        Err(error) => {
            eprintln!("cannot read manifest `{manifest_path}`: {error}");
            return ExitCode::from(1);
        }
    };

    let result = validate_closure_evidence_bundle_v1(&review, &manifest, &expectation);
    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    match result {
        Ok(validated) => {
            if serde_json::to_writer_pretty(&mut out, &validated).is_err() {
                return ExitCode::from(1);
            }
            let _ = out.write_all(b"\n");
            ExitCode::SUCCESS
        }
        Err(error) => {
            let _ = serde_json::to_writer_pretty(&mut out, &error);
            let _ = out.write_all(b"\n");
            eprintln!("Closure evidence invalid: {error}");
            ExitCode::from(2)
        }
    }
}
