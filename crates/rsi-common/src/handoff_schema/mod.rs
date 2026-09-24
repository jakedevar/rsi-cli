//! Machine-verifiable validation of handoff documents (RSI-013 v1).
//!
//! Public API:
//! - [`validate`] — main entry point; takes raw markdown content + a
//!   [`ValidationMode`] (`Strict` or `Lenient`) and returns a [`Validation`].
//! - [`HANDOFF_SCHEMA_VERSION`] — bump on schema changes.
//! - [`HandoffFrontmatter`] — typed frontmatter struct.
//!
//! See `.claude/commands/create_handoff.md` for the field contract this
//! module enforces. v1 is frozen — adding/removing fields requires bumping
//! the version constant.

pub mod body;
pub mod error;
pub mod frontmatter;
pub mod rules;

pub use body::{
    CONTRACT_SUBSECTIONS, ContractBlock, ContractProblem, STAGE_CONTRACT_HEADING,
    scan_contract_block, scan_sections,
};
pub use error::{Validation, ValidationError, ValidationMode};
pub use frontmatter::{HandoffFrontmatter, HandoffStatus};
pub use rules::{
    Field, FieldSource, HANDOFF_SCHEMA_VERSION, HANDOFF_V1_SCHEMA, Modes, Rule, blocker_template,
    validate,
};
