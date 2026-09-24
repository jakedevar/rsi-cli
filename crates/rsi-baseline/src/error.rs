//! Fail-closed errors for the validated baseline contract.

use thiserror::Error;

pub type Result<T> = std::result::Result<T, BaselineError>;

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum BaselineError {
    #[error("producer not assembled")]
    ProducerNotAssembled,
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum PolicyBuildError {
    #[error("{field} is empty")]
    Empty { field: &'static str },
    #[error("{field} contains a NUL byte")]
    Nul { field: &'static str },
    #[error("{field} exceeds its byte limit")]
    TooLong { field: &'static str },
    #[error("{field} is invalid")]
    Invalid { field: &'static str },
    #[error("{field} uses a reserved environment name")]
    ReservedName { field: &'static str },
    #[error("{field} must be strictly sorted")]
    Unsorted { field: &'static str },
    #[error("{field} contains a duplicate")]
    Duplicate { field: &'static str },
    #[error("{field} has too many entries")]
    TooMany { field: &'static str },
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum WitnessBuildError {
    #[error("invalid witness field {field}")]
    Invalid { field: &'static str },
    #[error("witness canonical JSON exceeds 65536 bytes")]
    TooLarge,
    #[error("checked arithmetic failed")]
    Arithmetic,
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum WitnessDecodeError {
    #[error("witness exceeds 65536 bytes")]
    TooLarge,
    #[error("witness is not UTF-8")]
    Utf8,
    #[error("witness JSON syntax is invalid")]
    Syntax,
    #[error("witness contains a duplicate JSON key")]
    DuplicateKey,
    #[error("witness contains a non-integer JSON number")]
    NonInteger,
    #[error("witness contains an unknown or missing field")]
    Shape,
    #[error("witness JSON is not canonical")]
    NonCanonical,
    #[error("unsupported witness version {found}")]
    UnsupportedWitnessVersion { found: u32 },
    #[error("witness contains an invalid value")]
    InvalidValue,
    #[error("a policy digest does not match its validated policy")]
    DigestMismatch,
}

/// Construction failures for the fixed terminal taxonomy.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum TerminalBuildError {
    #[error("terminal code is unknown")]
    UnknownCode,
    #[error("terminal boundary and phase are incompatible with the reason")]
    IncompatibleCode,
    #[error("terminal rejection reason is required")]
    MissingReason,
    #[error("active command facts are invalid")]
    InvalidActiveCommand,
    #[error("terminal finality is invalid")]
    InvalidFinality,
}

/// Checked construction failures for WJR2 values.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum WjrBuildError {
    #[error("WJR capacity is invalid")]
    Capacity,
    #[error("watch dictionary entry is invalid")]
    Dictionary,
    #[error("watch detail entry is invalid")]
    Detail,
    #[error("watch accounting equation is invalid")]
    Accounting,
    #[error("watch reference is invalid")]
    Reference,
    #[error("WJR digest is invalid")]
    Digest,
    #[error("checked arithmetic failed")]
    Arithmetic,
    #[error("terminal facts are incoherent")]
    TerminalCoherence,
}

/// Refusal reasons for an untrusted WJR2 envelope.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum WjrDecodeError {
    #[error("WJR length is invalid")]
    WrongLength,
    #[error("WJR magic is invalid")]
    Magic,
    #[error("WJR version is unsupported")]
    Version,
    #[error("WJR endian code is unsupported")]
    Endian,
    #[error("WJR schema is unsupported")]
    Schema,
    #[error("WJR capacity profile is unsupported")]
    UnsupportedCapacityProfile,
    #[error("WJR capacity digest is invalid")]
    CapacityDigest,
    #[error("WJR code is unknown")]
    UnknownCode,
    #[error("WJR reserved bytes are nonzero")]
    NonzeroReserved,
    #[error("WJR dictionary, detail, or footer shape is invalid")]
    Shape,
    #[error("WJR cross-field equation is invalid")]
    Equation,
    #[error("WJR reference is invalid")]
    Reference,
    #[error("WJR digest is invalid")]
    Digest,
    #[error("WJR terminal finality is invalid")]
    TerminalFinality,
}
