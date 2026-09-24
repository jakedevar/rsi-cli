//! Opaque validated launch witness and exact canonical wire codec.
#![allow(
    clippy::expect_used,
    clippy::format_collect,
    clippy::missing_const_for_fn,
    clippy::missing_panics_doc,
    clippy::too_many_lines
)]

use crate::bounds::WitnessBytes;
use crate::canonical_json::{self, Sink};
use crate::error::{PolicyBuildError, WitnessBuildError, WitnessDecodeError};
use crate::policy::{
    ArgvAtom, CanonicalAbsolutePath, CanonicalUtcTime, CanonicalUuid, EnvironmentName,
    EnvironmentValue, GitCommit, PolicyDigestV1, RollingRef, Sha256Digest, ValidatedLaunchPolicyV1,
    ValidatedToolPolicyV1, stream_policy,
};
use serde::Deserialize;
pub const LAUNCH_WITNESS_SCHEMA_VERSION: u32 = 1;
pub const LAUNCH_WITNESS_TICKET: &str = "PERF-Z-BASELINE";
const FAILURE_RECORD: &str = "b3c15be2ebfa7bf7ed9f66288e16742e6721dc99";
const OUTPUT_LEAF: &str = concat!("metrics/", "test-suite-", "baseline.json");
/// Untrusted consumed construction input. It has no digest API.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WitnessDraftV1 {
    pub accepted_implementation_commit: GitCommit,
    pub accepted_review_commit: GitCommit,
    pub accepted_verification_manifest_commit: GitCommit,
    pub active_ref_sha256: Sha256Digest,
    pub authorization_time: CanonicalUtcTime,
    pub cargo_config_sha256: Sha256Digest,
    pub failure_record_commit: GitCommit,
    pub index_sha256: Sha256Digest,
    pub make_sha256: Sha256Digest,
    pub nextest_config_sha256: Sha256Digest,
    pub output_leaf: String,
    pub policy: ValidatedLaunchPolicyV1,
    pub producer_sha256: Sha256Digest,
    pub repository_dev: u64,
    pub repository_ino: u64,
    pub repository_path: CanonicalAbsolutePath,
    pub run_nonce: CanonicalUuid,
    pub rust_toolchain_sha256: Sha256Digest,
    pub source_commit: GitCommit,
    pub source_inventory_sha256: Sha256Digest,
    pub symbolic_head: RollingRef,
    pub tracked_stage_set_sha256: Sha256Digest,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValidatedLaunchWitnessV1 {
    draft: WitnessDraftV1,
}
impl ValidatedLaunchWitnessV1 {
    pub fn try_new(draft: WitnessDraftV1) -> Result<Self, WitnessBuildError> {
        validate(&draft)?;
        let mut sink = Sink::counting(Some(WitnessBytes::MAXIMUM));
        stream_witness(&draft, &mut sink)?;
        Ok(Self { draft })
    }
    #[must_use]
    pub fn policy(&self) -> &ValidatedLaunchPolicyV1 {
        &self.draft.policy
    }
    #[must_use]
    pub fn accepted_implementation_commit(&self) -> &GitCommit {
        &self.draft.accepted_implementation_commit
    }
    #[must_use]
    pub fn accepted_review_commit(&self) -> &GitCommit {
        &self.draft.accepted_review_commit
    }
    #[must_use]
    pub fn accepted_verification_manifest_commit(&self) -> &GitCommit {
        &self.draft.accepted_verification_manifest_commit
    }
    #[must_use]
    pub fn active_ref_sha256(&self) -> &Sha256Digest {
        &self.draft.active_ref_sha256
    }
    #[must_use]
    pub fn authorization_time(&self) -> &CanonicalUtcTime {
        &self.draft.authorization_time
    }
    #[must_use]
    pub fn cargo_config_sha256(&self) -> &Sha256Digest {
        &self.draft.cargo_config_sha256
    }
    #[must_use]
    pub fn cargo_policy_sha256(&self) -> PolicyDigestV1 {
        PolicyDigestV1::of(self.draft.policy.cargo())
    }
    #[must_use]
    pub fn failure_record_commit(&self) -> &GitCommit {
        &self.draft.failure_record_commit
    }
    #[must_use]
    pub fn git_policy_sha256(&self) -> PolicyDigestV1 {
        PolicyDigestV1::of(self.draft.policy.git())
    }
    #[must_use]
    pub fn index_sha256(&self) -> &Sha256Digest {
        &self.draft.index_sha256
    }
    #[must_use]
    pub fn make_sha256(&self) -> &Sha256Digest {
        &self.draft.make_sha256
    }
    #[must_use]
    pub fn make_policy_sha256(&self) -> PolicyDigestV1 {
        PolicyDigestV1::of(self.draft.policy.make())
    }
    #[must_use]
    pub fn nextest_config_sha256(&self) -> &Sha256Digest {
        &self.draft.nextest_config_sha256
    }
    #[must_use]
    pub fn nextest_policy_sha256(&self) -> PolicyDigestV1 {
        PolicyDigestV1::of(self.draft.policy.nextest())
    }
    #[must_use]
    pub const fn output_absent_at_launch(&self) -> bool {
        true
    }
    #[must_use]
    pub fn output_leaf(&self) -> &str {
        &self.draft.output_leaf
    }
    #[must_use]
    pub fn producer_sha256(&self) -> &Sha256Digest {
        &self.draft.producer_sha256
    }
    #[must_use]
    pub const fn repository_dev(&self) -> u64 {
        self.draft.repository_dev
    }
    #[must_use]
    pub const fn repository_ino(&self) -> u64 {
        self.draft.repository_ino
    }
    #[must_use]
    pub fn repository_path(&self) -> &CanonicalAbsolutePath {
        &self.draft.repository_path
    }
    #[must_use]
    pub fn run_nonce(&self) -> &CanonicalUuid {
        &self.draft.run_nonce
    }
    #[must_use]
    pub fn rust_toolchain_sha256(&self) -> &Sha256Digest {
        &self.draft.rust_toolchain_sha256
    }
    #[must_use]
    pub fn rust_policy_sha256(&self) -> PolicyDigestV1 {
        PolicyDigestV1::of(self.draft.policy.rust())
    }
    #[must_use]
    pub const fn schema_version(&self) -> u32 {
        LAUNCH_WITNESS_SCHEMA_VERSION
    }
    #[must_use]
    pub const fn single_invocation(&self) -> bool {
        true
    }
    #[must_use]
    pub fn source_commit(&self) -> &GitCommit {
        &self.draft.source_commit
    }
    #[must_use]
    pub fn source_inventory_sha256(&self) -> &Sha256Digest {
        &self.draft.source_inventory_sha256
    }
    #[must_use]
    pub fn symbolic_head(&self) -> &RollingRef {
        &self.draft.symbolic_head
    }
    #[must_use]
    pub const fn ticket(&self) -> &'static str {
        LAUNCH_WITNESS_TICKET
    }
    #[must_use]
    pub fn tracked_stage_set_sha256(&self) -> &Sha256Digest {
        &self.draft.tracked_stage_set_sha256
    }
    #[must_use]
    pub fn encode_canonical(&self) -> Vec<u8> {
        let mut sink = Sink::bytes(WitnessBytes::MAXIMUM);
        stream_witness(&self.draft, &mut sink).expect("validated witness JSON");
        sink.finish_bytes()
    }
    pub fn decode_canonical(input: &[u8]) -> Result<Self, WitnessDecodeError> {
        let value = canonical_json::parse(input)?;
        let raw: RawWitness =
            serde_json::from_value(value).map_err(|_| WitnessDecodeError::Shape)?;
        if raw.schema_version != 1 {
            return Err(WitnessDecodeError::UnsupportedWitnessVersion {
                found: raw.schema_version,
            });
        }
        let witness =
            Self::try_new(raw.into_draft()?).map_err(|_| WitnessDecodeError::InvalidValue)?;
        if witness.encode_canonical() != input {
            return Err(WitnessDecodeError::NonCanonical);
        }
        Ok(witness)
    }
}
fn text_field(
    sink: &mut Sink,
    first: &mut bool,
    name: &str,
    value: &str,
) -> Result<(), WitnessBuildError> {
    canonical_json::field(sink, name, first)?;
    canonical_json::string(sink, value)
}
fn number_field(
    sink: &mut Sink,
    first: &mut bool,
    name: &str,
    value: u64,
) -> Result<(), WitnessBuildError> {
    canonical_json::field(sink, name, first)?;
    sink.write(value.to_string().as_bytes())
}
fn bool_field(
    sink: &mut Sink,
    first: &mut bool,
    name: &str,
    value: bool,
) -> Result<(), WitnessBuildError> {
    canonical_json::field(sink, name, first)?;
    sink.write(if value { b"true" } else { b"false" })
}
fn stream_launch_policy(
    policy: &ValidatedLaunchPolicyV1,
    sink: &mut Sink,
) -> Result<(), WitnessBuildError> {
    sink.write(b"{")?;
    let mut first = true;
    for (name, policy) in [
        ("cargo", policy.cargo()),
        ("git", policy.git()),
        ("make", policy.make()),
        ("nextest", policy.nextest()),
        ("rust", policy.rust()),
    ] {
        canonical_json::field(sink, name, &mut first)?;
        stream_policy(policy, sink)?;
    }
    sink.write(b"}")
}
fn stream_witness(d: &WitnessDraftV1, sink: &mut Sink) -> Result<(), WitnessBuildError> {
    sink.write(b"{")?;
    let mut first = true;
    text_field(
        sink,
        &mut first,
        "accepted_implementation_commit",
        d.accepted_implementation_commit.as_str(),
    )?;
    text_field(
        sink,
        &mut first,
        "accepted_review_commit",
        d.accepted_review_commit.as_str(),
    )?;
    text_field(
        sink,
        &mut first,
        "accepted_verification_manifest_commit",
        d.accepted_verification_manifest_commit.as_str(),
    )?;
    text_field(
        sink,
        &mut first,
        "active_ref_sha256",
        d.active_ref_sha256.as_str(),
    )?;
    text_field(
        sink,
        &mut first,
        "authorization_time",
        d.authorization_time.as_str(),
    )?;
    text_field(
        sink,
        &mut first,
        "cargo_config_sha256",
        d.cargo_config_sha256.as_str(),
    )?;
    text_field(
        sink,
        &mut first,
        "cargo_policy_sha256",
        &PolicyDigestV1::of(d.policy.cargo()).to_hex(),
    )?;
    text_field(
        sink,
        &mut first,
        "failure_record_commit",
        d.failure_record_commit.as_str(),
    )?;
    text_field(
        sink,
        &mut first,
        "git_policy_sha256",
        &PolicyDigestV1::of(d.policy.git()).to_hex(),
    )?;
    text_field(sink, &mut first, "index_sha256", d.index_sha256.as_str())?;
    text_field(
        sink,
        &mut first,
        "make_policy_sha256",
        &PolicyDigestV1::of(d.policy.make()).to_hex(),
    )?;
    text_field(sink, &mut first, "make_sha256", d.make_sha256.as_str())?;
    text_field(
        sink,
        &mut first,
        "nextest_config_sha256",
        d.nextest_config_sha256.as_str(),
    )?;
    text_field(
        sink,
        &mut first,
        "nextest_policy_sha256",
        &PolicyDigestV1::of(d.policy.nextest()).to_hex(),
    )?;
    bool_field(sink, &mut first, "output_absent_at_launch", true)?;
    text_field(sink, &mut first, "output_leaf", &d.output_leaf)?;
    canonical_json::field(sink, "policy", &mut first)?;
    stream_launch_policy(&d.policy, sink)?;
    text_field(
        sink,
        &mut first,
        "producer_sha256",
        d.producer_sha256.as_str(),
    )?;
    number_field(sink, &mut first, "repository_dev", d.repository_dev)?;
    number_field(sink, &mut first, "repository_ino", d.repository_ino)?;
    text_field(
        sink,
        &mut first,
        "repository_path",
        d.repository_path.as_str(),
    )?;
    text_field(sink, &mut first, "run_nonce", d.run_nonce.as_str())?;
    text_field(
        sink,
        &mut first,
        "rust_policy_sha256",
        &PolicyDigestV1::of(d.policy.rust()).to_hex(),
    )?;
    text_field(
        sink,
        &mut first,
        "rust_toolchain_sha256",
        d.rust_toolchain_sha256.as_str(),
    )?;
    number_field(sink, &mut first, "schema_version", 1)?;
    bool_field(sink, &mut first, "single_invocation", true)?;
    text_field(sink, &mut first, "source_commit", d.source_commit.as_str())?;
    text_field(
        sink,
        &mut first,
        "source_inventory_sha256",
        d.source_inventory_sha256.as_str(),
    )?;
    text_field(sink, &mut first, "symbolic_head", d.symbolic_head.as_str())?;
    text_field(sink, &mut first, "ticket", LAUNCH_WITNESS_TICKET)?;
    text_field(
        sink,
        &mut first,
        "tracked_stage_set_sha256",
        d.tracked_stage_set_sha256.as_str(),
    )?;
    sink.write(b"}")
}
fn validate(d: &WitnessDraftV1) -> Result<(), WitnessBuildError> {
    if d.repository_dev == 0
        || d.repository_ino == 0
        || d.output_leaf != OUTPUT_LEAF
        || d.failure_record_commit.as_str() != FAILURE_RECORD
    {
        Err(WitnessBuildError::Invalid {
            field: "witness identity",
        })
    } else {
        Ok(())
    }
}
/// A witness digest can only be minted from an opaque validated witness.
///
/// ```compile_fail
/// use rsi_baseline::{WitnessDigestV1, WitnessDraftV1};
/// let _: fn(&WitnessDraftV1) -> WitnessDigestV1 = WitnessDigestV1::of;
/// ```
///
/// ```compile_fail
/// use rsi_baseline::ValidatedLaunchWitnessV1;
/// let _ = ValidatedLaunchWitnessV1 { draft: () };
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WitnessDigestV1([u8; 32]);
impl WitnessDigestV1 {
    #[must_use]
    pub fn of(w: &ValidatedLaunchWitnessV1) -> Self {
        let mut count = Sink::counting(Some(WitnessBytes::MAXIMUM));
        stream_witness(&w.draft, &mut count).expect("validated witness");
        let mut prefix = b"rsi-baseline/launch-witness/v1\0".to_vec();
        prefix.extend_from_slice(&count.count().to_be_bytes());
        let mut hash = Sink::hashing_prefixed(&prefix);
        stream_witness(&w.draft, &mut hash).expect("validated witness");
        Self(hash.finish_hash())
    }
    #[must_use]
    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
    #[must_use]
    pub fn to_hex(self) -> String {
        self.0.iter().map(|b| format!("{b:02x}")).collect()
    }
}
#[must_use]
pub fn expected_output_leaf() -> &'static str {
    OUTPUT_LEAF
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawBinding {
    name: String,
    value: String,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawTool {
    additional_environment: Vec<RawBinding>,
    allowed_argv_suffix: Vec<String>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawLaunch {
    cargo: RawTool,
    git: RawTool,
    make: RawTool,
    nextest: RawTool,
    rust: RawTool,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawWitness {
    accepted_implementation_commit: String,
    accepted_review_commit: String,
    accepted_verification_manifest_commit: String,
    active_ref_sha256: String,
    authorization_time: String,
    cargo_config_sha256: String,
    cargo_policy_sha256: String,
    failure_record_commit: String,
    git_policy_sha256: String,
    index_sha256: String,
    make_sha256: String,
    make_policy_sha256: String,
    nextest_config_sha256: String,
    nextest_policy_sha256: String,
    output_absent_at_launch: bool,
    output_leaf: String,
    policy: RawLaunch,
    producer_sha256: String,
    repository_dev: u64,
    repository_ino: u64,
    repository_path: String,
    run_nonce: String,
    rust_policy_sha256: String,
    rust_toolchain_sha256: String,
    schema_version: u32,
    single_invocation: bool,
    source_commit: String,
    source_inventory_sha256: String,
    symbolic_head: String,
    ticket: String,
    tracked_stage_set_sha256: String,
}
impl RawWitness {
    fn into_draft(self) -> Result<WitnessDraftV1, WitnessDecodeError> {
        if !self.output_absent_at_launch
            || !self.single_invocation
            || self.ticket != LAUNCH_WITNESS_TICKET
        {
            return Err(WitnessDecodeError::InvalidValue);
        }
        let policy = raw_launch(self.policy)?;
        let seen = [
            self.cargo_policy_sha256,
            self.git_policy_sha256,
            self.make_policy_sha256,
            self.nextest_policy_sha256,
            self.rust_policy_sha256,
        ];
        let expected = [
            PolicyDigestV1::of(policy.cargo()).to_hex(),
            PolicyDigestV1::of(policy.git()).to_hex(),
            PolicyDigestV1::of(policy.make()).to_hex(),
            PolicyDigestV1::of(policy.nextest()).to_hex(),
            PolicyDigestV1::of(policy.rust()).to_hex(),
        ];
        if seen != expected {
            return Err(WitnessDecodeError::DigestMismatch);
        }
        Ok(WitnessDraftV1 {
            accepted_implementation_commit: git(self.accepted_implementation_commit)?,
            accepted_review_commit: git(self.accepted_review_commit)?,
            accepted_verification_manifest_commit: git(self.accepted_verification_manifest_commit)?,
            active_ref_sha256: sha(self.active_ref_sha256)?,
            authorization_time: time(self.authorization_time)?,
            cargo_config_sha256: sha(self.cargo_config_sha256)?,
            failure_record_commit: git(self.failure_record_commit)?,
            index_sha256: sha(self.index_sha256)?,
            make_sha256: sha(self.make_sha256)?,
            nextest_config_sha256: sha(self.nextest_config_sha256)?,
            output_leaf: self.output_leaf,
            policy,
            producer_sha256: sha(self.producer_sha256)?,
            repository_dev: self.repository_dev,
            repository_ino: self.repository_ino,
            repository_path: path(self.repository_path)?,
            run_nonce: uuid(self.run_nonce)?,
            rust_toolchain_sha256: sha(self.rust_toolchain_sha256)?,
            source_commit: git(self.source_commit)?,
            source_inventory_sha256: sha(self.source_inventory_sha256)?,
            symbolic_head: rolling(self.symbolic_head)?,
            tracked_stage_set_sha256: sha(self.tracked_stage_set_sha256)?,
        })
    }
}
fn raw_tool(r: RawTool) -> Result<ValidatedToolPolicyV1, WitnessDecodeError> {
    let env = r
        .additional_environment
        .into_iter()
        .map(|x| Ok((name(x.name)?, value(x.value)?)))
        .collect::<Result<_, WitnessDecodeError>>()?;
    let argv = r
        .allowed_argv_suffix
        .into_iter()
        .map(atom)
        .collect::<Result<_, _>>()?;
    ValidatedToolPolicyV1::try_new(env, argv).map_err(|_| WitnessDecodeError::InvalidValue)
}
fn raw_launch(r: RawLaunch) -> Result<ValidatedLaunchPolicyV1, WitnessDecodeError> {
    Ok(ValidatedLaunchPolicyV1::new(
        raw_tool(r.cargo)?,
        raw_tool(r.git)?,
        raw_tool(r.make)?,
        raw_tool(r.nextest)?,
        raw_tool(r.rust)?,
    ))
}
fn scalar<T>(v: Result<T, PolicyBuildError>) -> Result<T, WitnessDecodeError> {
    v.map_err(|_| WitnessDecodeError::InvalidValue)
}
fn git(v: String) -> Result<GitCommit, WitnessDecodeError> {
    scalar(GitCommit::parse(v))
}
fn sha(v: String) -> Result<Sha256Digest, WitnessDecodeError> {
    scalar(Sha256Digest::parse(v))
}
fn time(v: String) -> Result<CanonicalUtcTime, WitnessDecodeError> {
    scalar(CanonicalUtcTime::parse(v))
}
fn path(v: String) -> Result<CanonicalAbsolutePath, WitnessDecodeError> {
    scalar(CanonicalAbsolutePath::parse(v))
}
fn uuid(v: String) -> Result<CanonicalUuid, WitnessDecodeError> {
    scalar(CanonicalUuid::parse(v))
}
fn rolling(v: String) -> Result<RollingRef, WitnessDecodeError> {
    scalar(RollingRef::parse(v))
}
fn name(v: String) -> Result<EnvironmentName, WitnessDecodeError> {
    scalar(EnvironmentName::parse(v))
}
fn value(v: String) -> Result<EnvironmentValue, WitnessDecodeError> {
    scalar(EnvironmentValue::parse(v))
}
fn atom(v: String) -> Result<ArgvAtom, WitnessDecodeError> {
    scalar(ArgvAtom::parse(v))
}
