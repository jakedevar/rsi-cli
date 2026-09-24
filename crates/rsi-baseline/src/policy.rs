//! Opaque, validated tool-launch policy values.
#![allow(
    clippy::expect_used,
    clippy::format_collect,
    clippy::missing_const_for_fn,
    clippy::missing_panics_doc
)]

use crate::bounds::{PolicyItems, StringBytes};
use crate::canonical_json::{self, Sink};
use crate::error::{PolicyBuildError, WitnessBuildError};
use chrono::{DateTime, SecondsFormat, Utc};
use serde::Serialize;
use uuid::Uuid;
macro_rules! scalar {
    ($name:ident,$validate:ident) => {
        #[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
        pub struct $name(String);
        impl $name {
            pub fn parse(value: impl Into<String>) -> Result<Self, PolicyBuildError> {
                let value = value.into();
                $validate(&value)?;
                Ok(Self(value))
            }
            #[must_use]
            pub fn as_str(&self) -> &str {
                &self.0
            }
        }
        impl Serialize for $name {
            fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
                s.serialize_str(&self.0)
            }
        }
    };
}
scalar!(CanonicalUuid, uuid_value);
scalar!(CanonicalAbsolutePath, path_value);
scalar!(Sha256Digest, sha_value);
scalar!(GitCommit, commit_value);
scalar!(RollingRef, rolling_value);
scalar!(CanonicalUtcTime, time_value);
scalar!(EnvironmentName, env_name);
scalar!(EnvironmentValue, text_value);
scalar!(ArgvAtom, text_value);
fn nonempty(v: &str, f: &'static str) -> Result<(), PolicyBuildError> {
    if v.is_empty() {
        Err(PolicyBuildError::Empty { field: f })
    } else if StringBytes::try_new(v.len() as u64).is_err() {
        Err(PolicyBuildError::TooLong { field: f })
    } else if v.contains('\0') {
        Err(PolicyBuildError::Nul { field: f })
    } else {
        Ok(())
    }
}
fn uuid_value(v: &str) -> Result<(), PolicyBuildError> {
    let p = Uuid::parse_str(v).map_err(|_| PolicyBuildError::Invalid { field: "uuid" })?;
    if v.len() == 36 && p.to_string() == v {
        Ok(())
    } else {
        Err(PolicyBuildError::Invalid { field: "uuid" })
    }
}
fn path_value(v: &str) -> Result<(), PolicyBuildError> {
    nonempty(v, "path")?;
    if !v.starts_with('/')
        || (v.len() > 1 && v.ends_with('/'))
        || v.split('/')
            .skip(1)
            .any(|p| p.is_empty() || p == "." || p == "..")
    {
        Err(PolicyBuildError::Invalid { field: "path" })
    } else {
        Ok(())
    }
}
fn hex(v: &str, n: usize, f: &'static str) -> Result<(), PolicyBuildError> {
    if v.len() == n
        && v.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    {
        Ok(())
    } else {
        Err(PolicyBuildError::Invalid { field: f })
    }
}
fn sha_value(v: &str) -> Result<(), PolicyBuildError> {
    hex(v, 64, "sha256")
}
fn commit_value(v: &str) -> Result<(), PolicyBuildError> {
    hex(v, 40, "git_commit")
}
fn rolling_value(v: &str) -> Result<(), PolicyBuildError> {
    if v == "refs/heads/rolling" {
        Ok(())
    } else {
        Err(PolicyBuildError::Invalid {
            field: "rolling_ref",
        })
    }
}
fn time_value(v: &str) -> Result<(), PolicyBuildError> {
    let p = DateTime::parse_from_rfc3339(v)
        .map_err(|_| PolicyBuildError::Invalid { field: "utc_time" })?;
    if p.offset().local_minus_utc() == 0
        && v.ends_with('Z')
        && p.with_timezone(&Utc)
            .to_rfc3339_opts(SecondsFormat::Nanos, true)
            == v
    {
        Ok(())
    } else {
        Err(PolicyBuildError::Invalid { field: "utc_time" })
    }
}
fn text_value(v: &str) -> Result<(), PolicyBuildError> {
    nonempty(v, "text")
}
fn env_name(v: &str) -> Result<(), PolicyBuildError> {
    const BASE: &[&str] = &[
        "HOME",
        "CARGO_HOME",
        "RUSTUP_HOME",
        "PATH",
        "LANG",
        "LC_ALL",
        "TZ",
        "TMPDIR",
        "CARGO_TARGET_DIR",
        "XDG_CONFIG_HOME",
        "NEXTEST_CONFIG_FILE",
    ];
    nonempty(v, "environment_name")?;
    if BASE.contains(&v) {
        return Err(PolicyBuildError::ReservedName {
            field: "environment_name",
        });
    }
    if v.bytes()
        .enumerate()
        .all(|(i, b)| b == b'_' || b.is_ascii_uppercase() || (i > 0 && b.is_ascii_digit()))
    {
        Ok(())
    } else {
        Err(PolicyBuildError::Invalid {
            field: "environment_name",
        })
    }
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValidatedToolPolicyV1 {
    environment: Vec<(EnvironmentName, EnvironmentValue)>,
    argv: Vec<ArgvAtom>,
}
impl ValidatedToolPolicyV1 {
    pub fn try_new(
        environment: Vec<(EnvironmentName, EnvironmentValue)>,
        argv: Vec<ArgvAtom>,
    ) -> Result<Self, PolicyBuildError> {
        if PolicyItems::try_new(environment.len() as u64).is_err()
            || PolicyItems::try_new(argv.len() as u64).is_err()
        {
            return Err(PolicyBuildError::TooMany { field: "policy" });
        }
        strictly(
            environment.iter().map(|(n, _)| n.as_str()),
            "additional_environment",
        )?;
        strictly(argv.iter().map(ArgvAtom::as_str), "allowed_argv_suffix")?;
        Ok(Self { environment, argv })
    }
    #[must_use]
    pub fn additional_environment(&self) -> &[(EnvironmentName, EnvironmentValue)] {
        &self.environment
    }
    #[must_use]
    pub fn allowed_argv_suffix(&self) -> &[ArgvAtom] {
        &self.argv
    }
}
pub(crate) fn stream_policy(
    policy: &ValidatedToolPolicyV1,
    sink: &mut Sink,
) -> Result<(), WitnessBuildError> {
    sink.write(b"{")?;
    let mut first = true;
    canonical_json::field(sink, "additional_environment", &mut first)?;
    sink.write(b"[")?;
    for (index, (name, value)) in policy.additional_environment().iter().enumerate() {
        if index != 0 {
            sink.write(b",")?;
        }
        sink.write(b"{")?;
        let mut binding_first = true;
        canonical_json::field(sink, "name", &mut binding_first)?;
        canonical_json::string(sink, name.as_str())?;
        canonical_json::field(sink, "value", &mut binding_first)?;
        canonical_json::string(sink, value.as_str())?;
        sink.write(b"}")?;
    }
    sink.write(b"]")?;
    canonical_json::field(sink, "allowed_argv_suffix", &mut first)?;
    sink.write(b"[")?;
    for (index, atom) in policy.allowed_argv_suffix().iter().enumerate() {
        if index != 0 {
            sink.write(b",")?;
        }
        canonical_json::string(sink, atom.as_str())?;
    }
    sink.write(b"]}")
}
fn strictly<'a>(
    mut values: impl Iterator<Item = &'a str>,
    field: &'static str,
) -> Result<(), PolicyBuildError> {
    let Some(mut prior) = values.next() else {
        return Ok(());
    };
    for value in values {
        if value == prior {
            return Err(PolicyBuildError::Duplicate { field });
        }
        if value.as_bytes() < prior.as_bytes() {
            return Err(PolicyBuildError::Unsorted { field });
        }
        prior = value;
    }
    Ok(())
}
/// A digest can only be minted from an opaque validated policy.
///
/// ```compile_fail
/// use rsi_baseline::PolicyDigestV1;
/// let _ = PolicyDigestV1::of(&());
/// ```
///
/// ```compile_fail
/// use rsi_baseline::ValidatedToolPolicyV1;
/// let _ = ValidatedToolPolicyV1 { environment: vec![], argv: vec![] };
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PolicyDigestV1([u8; 32]);
impl PolicyDigestV1 {
    #[must_use]
    pub fn of(policy: &ValidatedToolPolicyV1) -> Self {
        let mut count = Sink::counting(None);
        stream_policy(policy, &mut count).expect("validated policy");
        let mut prefix = b"rsi-baseline/tool-policy/v1\0".to_vec();
        prefix.extend_from_slice(&count.count().to_be_bytes());
        let mut hash = Sink::hashing_prefixed(&prefix);
        stream_policy(policy, &mut hash).expect("validated policy");
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
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValidatedLaunchPolicyV1 {
    cargo: ValidatedToolPolicyV1,
    git: ValidatedToolPolicyV1,
    make: ValidatedToolPolicyV1,
    nextest: ValidatedToolPolicyV1,
    rust: ValidatedToolPolicyV1,
}
impl ValidatedLaunchPolicyV1 {
    #[must_use]
    pub fn new(
        cargo: ValidatedToolPolicyV1,
        git: ValidatedToolPolicyV1,
        make: ValidatedToolPolicyV1,
        nextest: ValidatedToolPolicyV1,
        rust: ValidatedToolPolicyV1,
    ) -> Self {
        Self {
            cargo,
            git,
            make,
            nextest,
            rust,
        }
    }
    #[must_use]
    pub fn cargo(&self) -> &ValidatedToolPolicyV1 {
        &self.cargo
    }
    #[must_use]
    pub fn git(&self) -> &ValidatedToolPolicyV1 {
        &self.git
    }
    #[must_use]
    pub fn make(&self) -> &ValidatedToolPolicyV1 {
        &self.make
    }
    #[must_use]
    pub fn nextest(&self) -> &ValidatedToolPolicyV1 {
        &self.nextest
    }
    #[must_use]
    pub fn rust(&self) -> &ValidatedToolPolicyV1 {
        &self.rust
    }
}
