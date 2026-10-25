//! The runner build stamp (R3a): the source manifest `build.rs` embedded, and the evaluator
//! identity derived from it. `rsi-eval bench tb2 stamp` prints it so the daemon (R3b) can verify
//! the pinned binary at admission instead of trusting whichever `rsi-eval` is on `PATH`.
//!
//! The digests are `rsi_common::bench_runner`'s, the same functions the daemon's record
//! assembler uses, so the stamp and the record can never disagree about what changed.

use std::io;

use rsi_common::bench_runner::{RUNNER_VERSION_DISCRIMINATOR, RunnerSourceManifest};
use serde::Serialize;

/// `path=hex;path=hex`, embedded by `build.rs`.
pub const SOURCE_MANIFEST: &str = env!("RSI_EVAL_RUNNER_SOURCE_MANIFEST");

/// What `bench tb2 stamp` prints.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RunnerStamp {
    pub runner_version: &'static str,
    pub source_digest: String,
    /// The value the record carries as `adapter_tree_hash`.
    pub evaluator_digest: String,
    pub source_manifest: String,
    /// Sources absent at build time; a stamp with any is not admissible.
    pub missing: Vec<String>,
}

/// The stamp of a manifest text.
///
/// # Errors
/// A malformed manifest.
pub fn stamp_of(manifest_text: &str) -> Result<RunnerStamp, String> {
    let manifest = RunnerSourceManifest::parse(manifest_text)?;
    Ok(RunnerStamp {
        runner_version: RUNNER_VERSION_DISCRIMINATOR,
        source_digest: manifest.source_digest(),
        evaluator_digest: manifest.evaluator_digest(),
        source_manifest: manifest.to_text(),
        missing: manifest.missing().into_iter().map(str::to_string).collect(),
    })
}

/// This binary's stamp.
///
/// # Errors
/// A malformed embedded manifest.
pub fn stamp() -> Result<RunnerStamp, String> {
    stamp_of(SOURCE_MANIFEST)
}

/// `rsi-eval bench tb2 stamp`: print the stamp as JSON; exit 0, or 1 when the stamp is unusable.
pub fn stamp_command(out: &mut dyn io::Write, err: &mut dyn io::Write) -> u8 {
    match stamp() {
        Ok(stamp) => {
            let text = serde_json::to_string_pretty(&stamp).unwrap_or_default();
            if writeln!(out, "{text}").is_err() {
                return 1;
            }
            u8::from(!stamp.missing.is_empty())
        }
        Err(error) => {
            let _ = writeln!(err, "tb2_runner error [stamp]: {error}");
            1
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_build_stamps_every_evaluator_source_and_none_is_missing() {
        let stamp = stamp().unwrap_or_else(|e| panic!("{e}"));
        assert!(stamp.missing.is_empty(), "missing: {:?}", stamp.missing);
        let manifest =
            RunnerSourceManifest::parse(SOURCE_MANIFEST).unwrap_or_else(|e| panic!("{e}"));
        for needed in [
            "Cargo.lock",
            "crates/rsi-eval/Cargo.toml",
            "crates/rsi-eval/src/tb2_runner/trial.rs",
            "crates/rsi-eval/src/tb2_runner/stamp.rs",
            "crates/rsi-eval/src/tb2/dataset.rs",
            "crates/rsi-common/src/bench_runner.rs",
            "crates/rsid/src/agent_jobs/bench/record.rs",
        ] {
            assert!(manifest.entries().contains_key(needed), "{needed}");
        }
        // A test edit is not an evaluator change.
        assert!(
            !manifest
                .entries()
                .keys()
                .any(|path| path.ends_with("tests.rs"))
        );
        assert_eq!(stamp.evaluator_digest, manifest.evaluator_digest());
        assert!(stamp.evaluator_digest.starts_with("sha256:"));
        assert_eq!(stamp.runner_version, RUNNER_VERSION_DISCRIMINATOR);
    }

    #[test]
    fn a_stamp_reports_a_missing_source_and_refuses_a_malformed_manifest() {
        let digest = "a".repeat(64);
        let stamp =
            stamp_of(&format!("x.rs={digest};y.rs=missing")).unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(stamp.missing, ["y.rs"]);
        assert!(stamp_of("garbage").is_err());
    }

    #[test]
    fn the_stamp_command_prints_json_and_exits_zero_for_a_complete_build() {
        let (mut out, mut err) = (Vec::new(), Vec::new());
        assert_eq!(stamp_command(&mut out, &mut err), 0);
        let value: serde_json::Value =
            serde_json::from_slice(&out).unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(value["runner_version"], RUNNER_VERSION_DISCRIMINATOR);
        assert!(err.is_empty());
    }
}
