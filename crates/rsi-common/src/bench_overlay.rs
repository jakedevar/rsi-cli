//! #3191 (program #3135, milestone M6): the bench run overlay.
//!
//! A Meta-Harness candidate is one committed overlay file (plus an optional
//! instructions file) under `eval/bench/tb2/candidates/<id>/`. A bench run
//! config names it with `overlay = "<repo path>"`; the bench job applies it to
//! every trial session of that run and to nothing else. This module holds the
//! schema, the validation and the identity. It does no I/O: the caller reads the
//! files and passes the bytes.
//!
//! What an overlay may change, all optional:
//!
//! * `instructions`: a file in the candidate directory that replaces the
//!   editable body of the bench prompt. The fixed header (the role, the
//!   container facts, the integrity rules and the critical rules) is always
//!   kept and cannot be overridden. At most 64 KiB of UTF-8.
//! * `[tool_policy] enabled_tools` / `denied_tools`: only narrow the bench
//!   allowlist. `web_access`, `egress`, `context_editing`, `budgets` and
//!   `cli_access` are refused.
//! * `[context] compact_budget_tokens` in `[32000, 2000000]`.
//! * `[loop] max_iterations` in `[10, 1000]`.
//! * `[output_spill.<tool>] max_bytes` in `[2048, 1048576]` and `max_lines` in
//!   `[10, 100000]` for `shell`, `exec_command` or `write_stdin` (#3173): when
//!   that command's output is replaced by a spill stub. Unset keeps the default
//!   (8 KiB / 200 lines).
//! * `[compaction]` (#3199) sets the Tier 1 compaction policy for the run:
//!   `message_trigger` in `[40, 1000]`, `keep_recent` in `[4, 100]` (below the
//!   trigger), `window_percent` in `[25, 95]`, `summary_tokens` in `[500, 16000]`
//!   and `summary_instruction` (at most 4 KiB). Unset keeps today's value.
//! * The reserved table `reasoning_replay` is refused with
//!   `overlay_field_not_supported` until it is wired.
//!
//! An out-of-range value is refused and never clamped. An overlay that sets
//! nothing resolves to no overlay, so "no overlay" and "empty overlay" have the
//! same identity. The overlay never writes a daemon setting or a stored session
//! policy: it rides on the per-launch [`crate::bench_profile::BenchLaunchProfile`].

use crate::bench_profile::LoopKnobs;
use crate::compaction_policy::{
    CompactionOverlay, CompactionPolicy, KEEP_RECENT_MAX, KEEP_RECENT_MIN, MESSAGE_TRIGGER_MAX,
    MESSAGE_TRIGGER_MIN, SUMMARY_INSTRUCTION_MAX_BYTES, SUMMARY_TOKENS_MAX, SUMMARY_TOKENS_MIN,
    WINDOW_PERCENT_MAX, WINDOW_PERCENT_MIN,
};
use crate::harness_tool_policy::{
    BENCH_ENABLED_TOOLS, HarnessToolPolicy, OUTPUT_SPILL_MAX_BYTES, OUTPUT_SPILL_MAX_LINES,
    OUTPUT_SPILL_MIN_BYTES, OUTPUT_SPILL_MIN_LINES, OutputSpill, SPILL_COMMAND_TOOLS,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;

/// The repository directory candidates live under.
pub const OVERLAY_CANDIDATES_DIR: &str = "eval/bench/tb2/candidates/";
/// The only file name an overlay may have inside its candidate directory.
pub const OVERLAY_FILE_NAME: &str = "overlay.toml";

/// Largest instructions file.
pub const OVERLAY_MAX_INSTRUCTIONS_BYTES: usize = 64 * 1024;
/// `context.compact_budget_tokens` range (the operator setting's own range).
pub const OVERLAY_COMPACT_BUDGET_MIN: u64 = 32_000;
pub const OVERLAY_COMPACT_BUDGET_MAX: u64 = 2_000_000;
/// `loop.max_iterations` range (the operator setting's own range).
pub const OVERLAY_MAX_ITERATIONS_MIN: u32 = 10;
pub const OVERLAY_MAX_ITERATIONS_MAX: u32 = 1000;

/// The overlay text is not valid TOML or sets a field that does not exist.
pub const OVERLAY_PARSE_FAILED: &str = "overlay_parse_failed";
/// A reserved table (`reasoning_replay`) that is not wired yet.
pub const OVERLAY_FIELD_NOT_SUPPORTED: &str = "overlay_field_not_supported";
/// A field that exists but an overlay may never set (`tool_policy.web_access`, ...).
pub const OVERLAY_FIELD_NOT_ALLOWED: &str = "overlay_field_not_allowed";
/// A value outside its range. Never clamped.
pub const OVERLAY_OUT_OF_RANGE: &str = "overlay_out_of_range";
/// An `enabled_tools` entry the bench allowlist does not hold.
pub const OVERLAY_TOOL_WIDENED: &str = "overlay_tool_widened";
/// A malformed or unknown tool name, or an empty `enabled_tools`.
pub const OVERLAY_TOOL_INVALID: &str = "overlay_tool_invalid";
/// The instructions file is missing, oversized, not UTF-8 or blank.
pub const OVERLAY_INSTRUCTIONS_INVALID: &str = "overlay_instructions_invalid";
/// A path that is not inside the candidate directory.
pub const OVERLAY_PATH_INVALID: &str = "overlay_path_invalid";
/// A carried overlay whose digest does not match its content.
pub const OVERLAY_DIGEST_MISMATCH: &str = "overlay_digest_mismatch";

/// Version of the canonical overlay layout the digest is taken over.
const OVERLAY_SCHEMA_VERSION: u32 = 1;

/// A typed overlay refusal: a stable `code` and the offending field.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OverlayError {
    pub code: &'static str,
    pub detail: String,
}

impl OverlayError {
    #[must_use]
    pub fn new(code: &'static str, detail: impl Into<String>) -> Self {
        Self {
            code,
            detail: detail.into(),
        }
    }
}

impl std::fmt::Display for OverlayError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.code, self.detail)
    }
}

impl std::error::Error for OverlayError {}

/// `[tool_policy]`. The fields an overlay may never set are kept as raw values so
/// the refusal is typed (`overlay_field_not_allowed`) rather than a parse error.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawToolPolicy {
    #[serde(default)]
    pub enabled_tools: Option<Vec<String>>,
    #[serde(default)]
    pub denied_tools: Option<Vec<String>>,
    #[serde(default)]
    pub web_access: Option<serde_json::Value>,
    #[serde(default)]
    pub egress: Option<serde_json::Value>,
    #[serde(default)]
    pub context_editing: Option<serde_json::Value>,
    #[serde(default)]
    pub budgets: Option<serde_json::Value>,
    #[serde(default)]
    pub cli_access: Option<serde_json::Value>,
}

/// `[context]`. The value is signed so a negative number is a typed range error.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawContext {
    #[serde(default)]
    pub compact_budget_tokens: Option<i64>,
}

/// `[loop]`.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawLoop {
    #[serde(default)]
    pub max_iterations: Option<i64>,
}

/// `[output_spill.<tool>]`. Signed so a negative number is a typed range error.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawOutputSpill {
    #[serde(default)]
    pub max_bytes: Option<i64>,
    #[serde(default)]
    pub max_lines: Option<i64>,
}

/// `[compaction]` (#3199). Signed so a negative number is a typed range error.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawCompaction {
    #[serde(default)]
    pub message_trigger: Option<i64>,
    #[serde(default)]
    pub keep_recent: Option<i64>,
    #[serde(default)]
    pub window_percent: Option<i64>,
    #[serde(default)]
    pub summary_tokens: Option<i64>,
    #[serde(default)]
    pub summary_instruction: Option<String>,
}

/// `eval/bench/tb2/candidates/<id>/overlay.toml` as written. Unknown fields are
/// refused; every field is optional.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawOverlay {
    #[serde(default)]
    pub instructions: Option<String>,
    #[serde(default)]
    pub tool_policy: Option<RawToolPolicy>,
    #[serde(default)]
    pub context: Option<RawContext>,
    #[serde(default, rename = "loop")]
    pub loop_knobs: Option<RawLoop>,
    /// Per command tool spill thresholds (#3173).
    #[serde(default)]
    pub output_spill: Option<BTreeMap<String, RawOutputSpill>>,
    /// The compaction policy (#3199).
    #[serde(default)]
    pub compaction: Option<RawCompaction>,
    /// Reserved (#3170): any value is refused until it is wired.
    #[serde(default)]
    pub reasoning_replay: Option<serde_json::Value>,
}

/// The instructions file an overlay carries: where it was, what it said.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OverlayInstructions {
    /// The path as written, relative to the candidate directory.
    pub path: String,
    /// `sha256:<hex>` of the file's exact bytes.
    pub sha256: String,
    /// The editable prompt body: the file's text with `\n` line endings and no
    /// surrounding blank lines.
    pub text: String,
    /// `sha256:<hex>` of [`Self::text`].
    pub text_sha256: String,
}

/// A validated, non-empty overlay: what one run's sessions run with.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BenchOverlay {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instructions: Option<OverlayInstructions>,
    /// Sorted, deduplicated, a subset of the bench allowlist.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enabled_tools: Option<Vec<String>>,
    /// Sorted, deduplicated, each a tool of the bench allowlist.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub denied_tools: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compact_budget_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_iterations: Option<u32>,
    /// Spill thresholds by command tool (#3173); no entry keeps the default.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub output_spill: BTreeMap<String, OutputSpill>,
    /// The compaction policy (#3199); unset keeps today's behaviour.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compaction: Option<CompactionOverlay>,
    /// `sha256:<hex>` of the canonical form of every field above.
    pub digest: String,
}

/// The overlay part of a bench session's identity (`harness_build_digest`,
/// `observation_fingerprint`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OverlayIdentity {
    /// [`BenchOverlay::digest`].
    pub digest: String,
    /// The sha256 of each file the overlay references, by path.
    pub files: BTreeMap<String, String>,
    /// Digest of the effective rendered bench prompt base (hex, no prefix).
    pub rendered_prompt_digest: String,
}

fn sha256_prefixed(bytes: &[u8]) -> String {
    format!("sha256:{:x}", Sha256::digest(bytes))
}

/// The instructions file name, as the audit and the reader both see it: a plain
/// relative path of plain components under the candidate directory.
///
/// # Errors
/// `overlay_path_invalid` for an absolute path, a `..` or `.` component, a hidden
/// or empty component, a backslash or anything outside `[A-Za-z0-9._-]`.
pub fn check_instructions_path(path: &str) -> Result<(), OverlayError> {
    let invalid = |why: &str| {
        OverlayError::new(
            OVERLAY_PATH_INVALID,
            format!("instructions {path:?}: {why}"),
        )
    };
    if path.is_empty() || path.len() > 128 {
        return Err(invalid("must be 1..=128 bytes"));
    }
    if path.starts_with('/') {
        return Err(invalid("must be relative to the candidate directory"));
    }
    for component in path.split('/') {
        if component.is_empty() || component == "." || component == ".." {
            return Err(invalid("must stay inside the candidate directory"));
        }
        if component.starts_with(['.', '-']) {
            return Err(invalid("a component may not start with '.' or '-'"));
        }
        if !component
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
        {
            return Err(invalid(
                "only letters, digits, '.', '_' and '-' are allowed",
            ));
        }
    }
    Ok(())
}

/// Whether `path` is `eval/bench/tb2/candidates/<id>/overlay.toml` with a plain `<id>`.
#[must_use]
pub fn overlay_path_ok(path: &str) -> bool {
    candidate_dir(path).is_some()
}

/// The candidate directory (`eval/bench/tb2/candidates/<id>/`) of an overlay path.
#[must_use]
pub fn candidate_dir(overlay_path: &str) -> Option<&str> {
    let rest = overlay_path.strip_prefix(OVERLAY_CANDIDATES_DIR)?;
    let (id, file) = rest.split_once('/')?;
    let plain = !id.is_empty()
        && id.len() <= 100
        && !id.starts_with(['-', '.'])
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
        && !id.contains("..");
    (plain && file == OVERLAY_FILE_NAME)
        .then(|| &overlay_path[..OVERLAY_CANDIDATES_DIR.len() + id.len() + 1])
}

impl RawOverlay {
    /// The instructions path, once its shape is checked.
    ///
    /// # Errors
    /// `overlay_path_invalid` for a path that is not a plain relative path.
    pub fn instructions_path(&self) -> Result<Option<&str>, OverlayError> {
        match self.instructions.as_deref() {
            None => Ok(None),
            Some(path) => check_instructions_path(path).map(|()| Some(path)),
        }
    }

    /// Validate and normalise. `instructions` is the content of the file
    /// [`Self::instructions_path`] names. An overlay that sets nothing resolves to
    /// `None`, the same as no overlay.
    ///
    /// # Errors
    /// The first refusal, typed by its `code`.
    pub fn resolve(
        self,
        instructions: Option<&[u8]>,
    ) -> Result<Option<BenchOverlay>, OverlayError> {
        if self.reasoning_replay.is_some() {
            return Err(OverlayError::new(
                OVERLAY_FIELD_NOT_SUPPORTED,
                "reasoning_replay is reserved and not wired yet",
            ));
        }
        let (mut enabled, mut denied) = (None, Vec::new());
        if let Some(policy) = &self.tool_policy {
            for (name, value) in [
                ("web_access", &policy.web_access),
                ("egress", &policy.egress),
                ("context_editing", &policy.context_editing),
                ("budgets", &policy.budgets),
                ("cli_access", &policy.cli_access),
            ] {
                if value.is_some() {
                    return Err(OverlayError::new(
                        OVERLAY_FIELD_NOT_ALLOWED,
                        format!("tool_policy.{name} can only be set by the bench profile"),
                    ));
                }
            }
            enabled = policy.enabled_tools.clone();
            denied = policy.denied_tools.clone().unwrap_or_default();
        }
        let enabled = enabled.map(narrow_enabled).transpose()?;
        let denied = narrow_denied(denied)?;
        let compact_budget_tokens = self
            .context
            .as_ref()
            .and_then(|context| context.compact_budget_tokens)
            .map(|value| {
                u64::try_from(value)
                    .ok()
                    .filter(|v| {
                        (OVERLAY_COMPACT_BUDGET_MIN..=OVERLAY_COMPACT_BUDGET_MAX).contains(v)
                    })
                    .ok_or_else(|| {
                        OverlayError::new(
                            OVERLAY_OUT_OF_RANGE,
                            format!(
                                "context.compact_budget_tokens {value} is outside \
                                 {OVERLAY_COMPACT_BUDGET_MIN}..={OVERLAY_COMPACT_BUDGET_MAX}"
                            ),
                        )
                    })
            })
            .transpose()?;
        let max_iterations = self
            .loop_knobs
            .as_ref()
            .and_then(|knobs| knobs.max_iterations)
            .map(|value| {
                u32::try_from(value)
                    .ok()
                    .filter(|v| {
                        (OVERLAY_MAX_ITERATIONS_MIN..=OVERLAY_MAX_ITERATIONS_MAX).contains(v)
                    })
                    .ok_or_else(|| {
                        OverlayError::new(
                            OVERLAY_OUT_OF_RANGE,
                            format!(
                                "loop.max_iterations {value} is outside \
                                 {OVERLAY_MAX_ITERATIONS_MIN}..={OVERLAY_MAX_ITERATIONS_MAX}"
                            ),
                        )
                    })
            })
            .transpose()?;
        let output_spill = narrow_output_spill(self.output_spill.as_ref())?;
        let compaction = narrow_compaction(self.compaction.as_ref())?;
        let instructions = match (self.instructions_path()?, instructions) {
            (None, _) => None,
            (Some(path), Some(bytes)) => Some(read_instructions(path, bytes)?),
            (Some(path), None) => {
                return Err(OverlayError::new(
                    OVERLAY_INSTRUCTIONS_INVALID,
                    format!("{path}: the file is missing"),
                ));
            }
        };
        if instructions.is_none()
            && enabled.is_none()
            && denied.is_empty()
            && compact_budget_tokens.is_none()
            && max_iterations.is_none()
            && output_spill.is_empty()
            && compaction.is_none()
        {
            return Ok(None);
        }
        let mut overlay = BenchOverlay {
            instructions,
            enabled_tools: enabled,
            denied_tools: denied,
            compact_budget_tokens,
            max_iterations,
            output_spill,
            compaction,
            digest: String::new(),
        };
        overlay.digest = overlay.compute_digest();
        Ok(Some(overlay))
    }
}

fn check_tool_names(names: &[String], field: &str) -> Result<(), OverlayError> {
    let policy = HarnessToolPolicy {
        denied_tools: names.to_vec(),
        ..HarnessToolPolicy::default()
    };
    policy.validate().map_err(|_| {
        OverlayError::new(
            OVERLAY_TOOL_INVALID,
            format!("{field}: malformed tool name"),
        )
    })
}

fn normalised(mut names: Vec<String>) -> Vec<String> {
    names.sort();
    names.dedup();
    names
}

/// `enabled_tools`: a non-empty subset of the bench allowlist.
fn narrow_enabled(names: Vec<String>) -> Result<Vec<String>, OverlayError> {
    check_tool_names(&names, "tool_policy.enabled_tools")?;
    if names.is_empty() {
        return Err(OverlayError::new(
            OVERLAY_TOOL_INVALID,
            "tool_policy.enabled_tools names no tool",
        ));
    }
    if let Some(wider) = names
        .iter()
        .find(|n| !BENCH_ENABLED_TOOLS.contains(&n.as_str()))
    {
        return Err(OverlayError::new(
            OVERLAY_TOOL_WIDENED,
            format!("tool_policy.enabled_tools: {wider} is not in the bench allowlist"),
        ));
    }
    Ok(normalised(names))
}

/// `denied_tools`: each a tool of the bench allowlist (a name outside it denies
/// nothing and is almost certainly a typo, so it is refused).
fn narrow_denied(names: Vec<String>) -> Result<Vec<String>, OverlayError> {
    check_tool_names(&names, "tool_policy.denied_tools")?;
    if let Some(unknown) = names
        .iter()
        .find(|n| !BENCH_ENABLED_TOOLS.contains(&n.as_str()))
    {
        return Err(OverlayError::new(
            OVERLAY_TOOL_INVALID,
            format!("tool_policy.denied_tools: {unknown} is not a bench tool"),
        ));
    }
    Ok(normalised(names))
}

/// `output_spill`: a command tool of the bench allowlist, each value in range.
/// An entry that sets nothing is dropped, so it is not a difference.
fn narrow_output_spill(
    raw: Option<&BTreeMap<String, RawOutputSpill>>,
) -> Result<BTreeMap<String, OutputSpill>, OverlayError> {
    let mut out = BTreeMap::new();
    for (tool, entry) in raw.into_iter().flatten() {
        if !SPILL_COMMAND_TOOLS.contains(&tool.as_str())
            || !BENCH_ENABLED_TOOLS.contains(&tool.as_str())
        {
            return Err(OverlayError::new(
                OVERLAY_TOOL_INVALID,
                format!("output_spill.{tool}: not a bench command tool"),
            ));
        }
        let out_of_range = |field: &str, value: i64, min: u64, max: u64| {
            OverlayError::new(
                OVERLAY_OUT_OF_RANGE,
                format!("output_spill.{tool}.{field} {value} is outside {min}..={max}"),
            )
        };
        let max_bytes = entry
            .max_bytes
            .map(|value| {
                u64::try_from(value)
                    .ok()
                    .filter(|v| (OUTPUT_SPILL_MIN_BYTES..=OUTPUT_SPILL_MAX_BYTES).contains(v))
                    .ok_or_else(|| {
                        out_of_range(
                            "max_bytes",
                            value,
                            OUTPUT_SPILL_MIN_BYTES,
                            OUTPUT_SPILL_MAX_BYTES,
                        )
                    })
            })
            .transpose()?;
        let max_lines = entry
            .max_lines
            .map(|value| {
                u32::try_from(value)
                    .ok()
                    .filter(|v| (OUTPUT_SPILL_MIN_LINES..=OUTPUT_SPILL_MAX_LINES).contains(v))
                    .ok_or_else(|| {
                        out_of_range(
                            "max_lines",
                            value,
                            u64::from(OUTPUT_SPILL_MIN_LINES),
                            u64::from(OUTPUT_SPILL_MAX_LINES),
                        )
                    })
            })
            .transpose()?;
        let spill = OutputSpill {
            max_bytes,
            max_lines,
        };
        if !spill.is_unset() {
            out.insert(tool.clone(), spill);
        }
    }
    Ok(out)
}

/// `compaction`: each value in range and `keep_recent` below the effective
/// `message_trigger`. A table that sets nothing is dropped, so it is not a
/// difference.
fn narrow_compaction(
    raw: Option<&RawCompaction>,
) -> Result<Option<CompactionOverlay>, OverlayError> {
    let Some(raw) = raw else {
        return Ok(None);
    };
    let out_of_range = |field: &str, value: i64, min: u64, max: u64| {
        OverlayError::new(
            OVERLAY_OUT_OF_RANGE,
            format!("compaction.{field} {value} is outside {min}..={max}"),
        )
    };
    let bounded = |field: &str, value: Option<i64>, min: u64, max: u64| {
        value
            .map(|value| {
                u64::try_from(value)
                    .ok()
                    .filter(|v| (min..=max).contains(v))
                    .ok_or_else(|| out_of_range(field, value, min, max))
            })
            .transpose()
    };
    let overlay = CompactionOverlay {
        message_trigger: bounded(
            "message_trigger",
            raw.message_trigger,
            MESSAGE_TRIGGER_MIN as u64,
            MESSAGE_TRIGGER_MAX as u64,
        )?
        .map(|v| v as usize),
        keep_recent: bounded(
            "keep_recent",
            raw.keep_recent,
            KEEP_RECENT_MIN as u64,
            KEEP_RECENT_MAX as u64,
        )?
        .map(|v| v as usize),
        window_percent: bounded(
            "window_percent",
            raw.window_percent,
            u64::from(WINDOW_PERCENT_MIN),
            u64::from(WINDOW_PERCENT_MAX),
        )?
        .map(|v| v as u32),
        summary_tokens: bounded(
            "summary_tokens",
            raw.summary_tokens,
            u64::from(SUMMARY_TOKENS_MIN),
            u64::from(SUMMARY_TOKENS_MAX),
        )?
        .map(|v| v as u32),
        summary_instruction: raw.summary_instruction.clone(),
    };
    if let Some(text) = &overlay.summary_instruction
        && (text.trim().is_empty() || text.len() > SUMMARY_INSTRUCTION_MAX_BYTES)
    {
        return Err(OverlayError::new(
            OVERLAY_OUT_OF_RANGE,
            format!(
                "compaction.summary_instruction must be 1..={SUMMARY_INSTRUCTION_MAX_BYTES} \
                 bytes and not blank"
            ),
        ));
    }
    overlay
        .policy()
        .validate()
        .map_err(|error| OverlayError::new(OVERLAY_OUT_OF_RANGE, error.to_string()))?;
    Ok((!overlay.is_unset()).then_some(overlay))
}

fn read_instructions(path: &str, bytes: &[u8]) -> Result<OverlayInstructions, OverlayError> {
    let invalid =
        |why: String| OverlayError::new(OVERLAY_INSTRUCTIONS_INVALID, format!("{path}: {why}"));
    if bytes.len() > OVERLAY_MAX_INSTRUCTIONS_BYTES {
        return Err(invalid(format!(
            "{} bytes is larger than {OVERLAY_MAX_INSTRUCTIONS_BYTES}",
            bytes.len()
        )));
    }
    let raw = std::str::from_utf8(bytes).map_err(|_| invalid("not valid UTF-8".into()))?;
    let text = raw.replace("\r\n", "\n").trim().to_string();
    if text.is_empty() {
        return Err(invalid("the file is blank".into()));
    }
    Ok(OverlayInstructions {
        path: path.to_string(),
        sha256: sha256_prefixed(bytes),
        text_sha256: sha256_prefixed(text.as_bytes()),
        text,
    })
}

/// The digest of the empty overlay: the identity of a candidate that sets nothing (the seed
/// `c00`). An overlay that sets nothing resolves to `None` and so carries no digest of its own;
/// the candidate store (#3194) names it with this one.
#[must_use]
pub fn empty_overlay_digest() -> String {
    let empty = BenchOverlay {
        instructions: None,
        enabled_tools: None,
        denied_tools: Vec::new(),
        compact_budget_tokens: None,
        max_iterations: None,
        output_spill: BTreeMap::new(),
        compaction: None,
        digest: String::new(),
    };
    empty.compute_digest()
}

impl BenchOverlay {
    fn canonical(&self) -> serde_json::Value {
        let mut canonical = serde_json::json!({
            "schema_version": OVERLAY_SCHEMA_VERSION,
            "instructions": self.instructions.as_ref().map(|i| serde_json::json!({
                "path": i.path,
                "sha256": i.sha256,
                "text_sha256": i.text_sha256,
            })),
            "enabled_tools": self.enabled_tools,
            "denied_tools": self.denied_tools,
            "compact_budget_tokens": self.compact_budget_tokens,
            "max_iterations": self.max_iterations,
        });
        // An overlay without it keeps the digest it had before #3173.
        if !self.output_spill.is_empty()
            && let Some(object) = canonical.as_object_mut()
        {
            object.insert(
                "output_spill".to_string(),
                serde_json::to_value(&self.output_spill).unwrap_or_default(),
            );
        }
        // Likewise for #3199.
        if let Some(compaction) = &self.compaction
            && let Some(object) = canonical.as_object_mut()
        {
            object.insert(
                "compaction".to_string(),
                serde_json::to_value(compaction).unwrap_or_default(),
            );
        }
        canonical
    }

    fn compute_digest(&self) -> String {
        // Plain strings, numbers and arrays cannot fail to serialise; an empty
        // digest would never match.
        let bytes = serde_jcs::to_vec(&self.canonical()).unwrap_or_default();
        sha256_prefixed(&bytes)
    }

    /// Re-check a carried overlay: every limit again, and that the digest and the
    /// text hashes match the content. A carried overlay that fails is never run.
    ///
    /// # Errors
    /// The refusal for the first failing check.
    pub fn verify(&self) -> Result<(), OverlayError> {
        if let Some(enabled) = &self.enabled_tools {
            narrow_enabled(enabled.clone())?;
        }
        narrow_denied(self.denied_tools.clone())?;
        let budget_ok = self
            .compact_budget_tokens
            .is_none_or(|v| (OVERLAY_COMPACT_BUDGET_MIN..=OVERLAY_COMPACT_BUDGET_MAX).contains(&v));
        let cap_ok = self
            .max_iterations
            .is_none_or(|v| (OVERLAY_MAX_ITERATIONS_MIN..=OVERLAY_MAX_ITERATIONS_MAX).contains(&v));
        let spill_ok = self.output_spill.iter().all(|(tool, spill)| {
            SPILL_COMMAND_TOOLS.contains(&tool.as_str())
                && BENCH_ENABLED_TOOLS.contains(&tool.as_str())
                && !spill.is_unset()
                && spill.validate().is_ok()
        });
        let compaction_ok = self
            .compaction
            .as_ref()
            .is_none_or(|c| !c.is_unset() && c.policy().validate().is_ok());
        if !budget_ok || !cap_ok || !spill_ok || !compaction_ok {
            return Err(OverlayError::new(
                OVERLAY_OUT_OF_RANGE,
                "a carried knob is outside its range",
            ));
        }
        if let Some(instructions) = &self.instructions {
            check_instructions_path(&instructions.path)?;
            if instructions.text.len() > OVERLAY_MAX_INSTRUCTIONS_BYTES
                || instructions.text.trim().is_empty()
                || instructions.text_sha256 != sha256_prefixed(instructions.text.as_bytes())
            {
                return Err(OverlayError::new(
                    OVERLAY_DIGEST_MISMATCH,
                    "the carried instructions do not match their hash",
                ));
            }
        }
        if self.digest != self.compute_digest() {
            return Err(OverlayError::new(
                OVERLAY_DIGEST_MISMATCH,
                "the overlay does not match its digest",
            ));
        }
        Ok(())
    }

    /// The sha256 of each file the overlay references, by path.
    #[must_use]
    pub fn files(&self) -> BTreeMap<String, String> {
        self.instructions
            .iter()
            .map(|i| (i.path.clone(), i.sha256.clone()))
            .collect()
    }

    /// The editable prompt body, when the overlay replaces it.
    #[must_use]
    pub fn instructions_text(&self) -> Option<&str> {
        self.instructions.as_ref().map(|i| i.text.as_str())
    }

    /// The bench tool policy under this overlay: the bench preset, narrowed.
    #[must_use]
    pub fn tool_policy(&self) -> HarnessToolPolicy {
        let mut policy = HarnessToolPolicy::bench();
        if let Some(enabled) = &self.enabled_tools {
            policy.enabled_tools = Some(enabled.clone());
        }
        policy.denied_tools = self.denied_tools.clone();
        policy.output_spill = self.output_spill.clone();
        policy
    }

    /// The compaction policy under this overlay: today's defaults with the
    /// overlay's values on top.
    #[must_use]
    pub fn compaction_policy(&self) -> CompactionPolicy {
        self.compaction
            .as_ref()
            .map_or_else(CompactionPolicy::default, CompactionOverlay::policy)
    }

    /// `knobs` with this overlay's values on top: the overlay wins where it sets
    /// a value, and `rotation_off` is never the overlay's to change.
    #[must_use]
    pub fn apply_knobs(&self, knobs: LoopKnobs) -> LoopKnobs {
        LoopKnobs {
            max_iterations: self.max_iterations.or(knobs.max_iterations),
            compaction_budget_tokens: self
                .compact_budget_tokens
                .or(knobs.compaction_budget_tokens),
            rotation_off: knobs.rotation_off,
            recovery_max_output_tokens: knobs.recovery_max_output_tokens,
        }
    }

    /// The identity the build digest and the fingerprint fold in.
    #[must_use]
    pub fn identity(&self, rendered_prompt_digest: &str) -> OverlayIdentity {
        OverlayIdentity {
            digest: self.digest.clone(),
            files: self.files(),
            rendered_prompt_digest: rendered_prompt_digest.to_string(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn raw(value: serde_json::Value) -> RawOverlay {
        serde_json::from_value(value).expect("a well-formed overlay")
    }

    fn resolve(value: serde_json::Value) -> Result<Option<BenchOverlay>, OverlayError> {
        raw(value).resolve(Some(b"Work carefully.\r\n"))
    }

    fn code(value: serde_json::Value) -> &'static str {
        resolve(value).expect_err("refused").code
    }

    #[test]
    fn unknown_fields_are_refused_at_every_level() {
        for bad in [
            json!({"surprise": 1}),
            json!({"tool_policy": {"surprise": 1}}),
            json!({"context": {"surprise": 1}}),
            json!({"loop": {"surprise": 1}}),
        ] {
            assert!(serde_json::from_value::<RawOverlay>(bad).is_err());
        }
    }

    #[test]
    fn an_empty_overlay_resolves_to_no_overlay() {
        for empty in [
            json!({}),
            json!({"tool_policy": {}, "context": {}, "loop": {}}),
            json!({"tool_policy": {"denied_tools": []}}),
        ] {
            assert_eq!(resolve(empty).unwrap(), None);
        }
    }

    #[test]
    fn the_empty_overlay_has_one_stable_digest_unlike_any_overlay_that_sets_something() {
        let empty = empty_overlay_digest();
        assert!(empty.starts_with("sha256:") && empty.len() == "sha256:".len() + 64);
        assert_eq!(empty, empty_overlay_digest());
        let set = resolve(json!({"loop": {"max_iterations": 100}}))
            .unwrap()
            .expect("sets a value");
        assert_ne!(set.digest, empty);
    }

    #[test]
    fn out_of_range_values_are_refused_never_clamped() {
        for bad in [
            json!({"context": {"compact_budget_tokens": 31_999}}),
            json!({"context": {"compact_budget_tokens": 2_000_001}}),
            json!({"context": {"compact_budget_tokens": -5}}),
            json!({"loop": {"max_iterations": 9}}),
            json!({"loop": {"max_iterations": 1001}}),
            json!({"loop": {"max_iterations": -1}}),
        ] {
            assert_eq!(code(bad), OVERLAY_OUT_OF_RANGE);
        }
        let edges = resolve(json!({
            "context": {"compact_budget_tokens": 32_000},
            "loop": {"max_iterations": 1000},
        }))
        .unwrap()
        .unwrap();
        assert_eq!(edges.compact_budget_tokens, Some(32_000));
        assert_eq!(edges.max_iterations, Some(1000));
        let top = resolve(json!({"context": {"compact_budget_tokens": 2_000_000}, "loop": {"max_iterations": 10}}))
            .unwrap()
            .unwrap();
        assert_eq!(
            (top.compact_budget_tokens, top.max_iterations),
            (Some(2_000_000), Some(10))
        );
    }

    #[test]
    fn a_tool_list_can_only_narrow_the_bench_allowlist() {
        assert_eq!(
            code(json!({"tool_policy": {"enabled_tools": ["shell", "web_search"]}})),
            OVERLAY_TOOL_WIDENED
        );
        assert_eq!(
            code(json!({"tool_policy": {"enabled_tools": ["read_file"]}})),
            OVERLAY_TOOL_WIDENED
        );
        assert_eq!(
            code(json!({"tool_policy": {"enabled_tools": []}})),
            OVERLAY_TOOL_INVALID
        );
        assert_eq!(
            code(json!({"tool_policy": {"denied_tools": ["update_plans"]}})),
            OVERLAY_TOOL_INVALID
        );
        assert_eq!(
            code(json!({"tool_policy": {"denied_tools": ["Shell!"]}})),
            OVERLAY_TOOL_INVALID
        );
        let narrowed = resolve(json!({"tool_policy": {
            "enabled_tools": ["shell", "exec_command", "shell", "compact"],
            "denied_tools": ["update_plan"],
        }}))
        .unwrap()
        .unwrap();
        let policy = narrowed.tool_policy();
        assert_eq!(
            policy.enabled_tools.as_deref(),
            Some(
                &[
                    "compact".to_string(),
                    "exec_command".to_string(),
                    "shell".to_string()
                ][..]
            )
        );
        assert_eq!(policy.denied_tools, ["update_plan"]);
        assert_eq!(policy.web_access, HarnessToolPolicy::bench().web_access);
        // Deny alone narrows the full allowlist.
        let deny_only = resolve(json!({"tool_policy": {"denied_tools": ["compact"]}}))
            .unwrap()
            .unwrap();
        assert_eq!(
            deny_only.tool_policy().enabled_tools,
            HarnessToolPolicy::bench().enabled_tools
        );
        assert_eq!(deny_only.tool_policy().denied_tools, ["compact"]);
    }

    #[test]
    fn the_fields_the_profile_owns_are_refused() {
        for field in [
            "web_access",
            "egress",
            "context_editing",
            "budgets",
            "cli_access",
        ] {
            let bad = json!({"tool_policy": {field: "enabled"}});
            assert_eq!(code(bad), OVERLAY_FIELD_NOT_ALLOWED, "{field}");
        }
    }

    #[test]
    fn the_reserved_table_is_refused_as_not_supported() {
        assert_eq!(
            code(json!({"reasoning_replay": {"mode": "all"}})),
            OVERLAY_FIELD_NOT_SUPPORTED
        );
    }

    /// #3199: `[compaction]` is validated, reaches the policy and is part of the
    /// overlay's identity.
    #[test]
    fn compaction_values_are_validated_and_reach_the_policy() {
        let overlay = resolve(json!({"compaction": {
            "message_trigger": 100,
            "keep_recent": 10,
            "window_percent": 50,
            "summary_tokens": 5000,
            "summary_instruction": "Summarize in {max_tokens} tokens.",
        }}))
        .unwrap()
        .unwrap();
        let policy = overlay.compaction_policy();
        assert_eq!(policy.message_trigger, 100);
        assert_eq!(policy.keep_recent, 10);
        assert_eq!(policy.window_percent, 50);
        assert_eq!(policy.summary_tokens, Some(5000));
        assert_eq!(
            policy.summary_instruction.as_deref(),
            Some("Summarize in {max_tokens} tokens.")
        );
        assert_eq!(overlay.verify(), Ok(()));
        // An unset field keeps today's value.
        let partial = resolve(json!({"compaction": {"keep_recent": 8}}))
            .unwrap()
            .unwrap();
        assert_eq!(
            partial.compaction_policy(),
            CompactionPolicy {
                keep_recent: 8,
                ..CompactionPolicy::default()
            }
        );
        // The policy is in the digest; an overlay without it keeps the digest it had.
        let plain = resolve(json!({"loop": {"max_iterations": 40}}))
            .unwrap()
            .unwrap();
        let with =
            resolve(json!({"loop": {"max_iterations": 40}, "compaction": {"keep_recent": 8}}))
                .unwrap()
                .unwrap();
        assert_ne!(plain.digest, with.digest);
        assert_eq!(plain.compaction_policy(), CompactionPolicy::default());
        let skipped = serde_json::to_value(&plain).unwrap();
        assert!(skipped.get("compaction").is_none());
        // A table that sets nothing is no overlay.
        assert_eq!(resolve(json!({"compaction": {}})).unwrap(), None);
    }

    #[test]
    fn out_of_range_compaction_values_are_refused_never_clamped() {
        for bad in [
            json!({"compaction": {"message_trigger": 39}}),
            json!({"compaction": {"message_trigger": 1001}}),
            json!({"compaction": {"message_trigger": -1}}),
            json!({"compaction": {"keep_recent": 3}}),
            json!({"compaction": {"keep_recent": 101}}),
            json!({"compaction": {"window_percent": 24}}),
            json!({"compaction": {"window_percent": 96}}),
            json!({"compaction": {"summary_tokens": 499}}),
            json!({"compaction": {"summary_tokens": 16001}}),
            json!({"compaction": {"summary_instruction": "   "}}),
            json!({"compaction": {"summary_instruction": "x".repeat(4097)}}),
            // keep_recent must stay below the effective trigger.
            json!({"compaction": {"message_trigger": 40, "keep_recent": 40}}),
        ] {
            assert_eq!(code(bad.clone()), OVERLAY_OUT_OF_RANGE, "{bad}");
        }
        let edges = resolve(json!({"compaction": {
            "message_trigger": 1000,
            "keep_recent": 4,
            "window_percent": 95,
            "summary_tokens": 16000,
        }}))
        .unwrap()
        .unwrap();
        assert_eq!(edges.compaction_policy().message_trigger, 1000);
        assert!(
            serde_json::from_value::<RawOverlay>(json!({"compaction": {"surprise": 1}})).is_err()
        );
    }

    #[test]
    fn a_tampered_compaction_policy_fails_verification() {
        let mut overlay = resolve(json!({"compaction": {"keep_recent": 8}}))
            .unwrap()
            .unwrap();
        overlay.compaction.as_mut().unwrap().keep_recent = Some(9);
        assert_eq!(overlay.verify().unwrap_err().code, OVERLAY_DIGEST_MISMATCH);
        overlay.compaction.as_mut().unwrap().keep_recent = Some(1);
        assert_eq!(overlay.verify().unwrap_err().code, OVERLAY_OUT_OF_RANGE);
    }

    #[test]
    fn instructions_are_bounded_utf8_inside_the_candidate_directory() {
        let with = |bytes: &[u8]| {
            raw(json!({"instructions": "instructions.md"}))
                .resolve(Some(bytes))
                .map(|o| o.map(|o| o.digest))
        };
        assert_eq!(
            with(&[0xff, 0xfe]).unwrap_err().code,
            OVERLAY_INSTRUCTIONS_INVALID
        );
        assert_eq!(
            with(b"  \r\n\n").unwrap_err().code,
            OVERLAY_INSTRUCTIONS_INVALID
        );
        let big = vec![b'a'; OVERLAY_MAX_INSTRUCTIONS_BYTES + 1];
        assert_eq!(with(&big).unwrap_err().code, OVERLAY_INSTRUCTIONS_INVALID);
        assert!(
            with(&big[..OVERLAY_MAX_INSTRUCTIONS_BYTES])
                .unwrap()
                .is_some()
        );
        // A missing file.
        assert_eq!(
            raw(json!({"instructions": "instructions.md"}))
                .resolve(None)
                .unwrap_err()
                .code,
            OVERLAY_INSTRUCTIONS_INVALID
        );
        for escaping in [
            "../instructions.md",
            "/etc/passwd",
            "a/../../b.md",
            "./x.md",
            "a//b.md",
            "..",
            "x\\y.md",
            ".hidden.md",
            "-x.md",
            "sub dir/x.md",
            "",
            "dir/",
        ] {
            let err = raw(json!({"instructions": escaping}))
                .resolve(Some(b"text"))
                .unwrap_err();
            assert_eq!(err.code, OVERLAY_PATH_INVALID, "{escaping:?}");
        }
        assert!(check_instructions_path("sub/instructions-2.md").is_ok());
    }

    #[test]
    fn instructions_normalise_and_hash_the_file_and_the_text() {
        let overlay = resolve(json!({"instructions": "instructions.md"}))
            .unwrap()
            .unwrap();
        let instructions = overlay.instructions.as_ref().unwrap();
        assert_eq!(instructions.text, "Work carefully.");
        assert_eq!(instructions.sha256, sha256_prefixed(b"Work carefully.\r\n"));
        assert_eq!(
            instructions.text_sha256,
            sha256_prefixed(b"Work carefully.")
        );
        assert_eq!(
            overlay.files(),
            BTreeMap::from([("instructions.md".to_string(), instructions.sha256.clone())])
        );
        assert_eq!(overlay.instructions_text(), Some("Work carefully."));
        assert!(overlay.verify().is_ok());
    }

    #[test]
    fn the_digest_names_the_content_and_ignores_spelling() {
        let a = resolve(json!({
            "tool_policy": {"denied_tools": ["compact", "update_plan"]},
            "loop": {"max_iterations": 200},
        }))
        .unwrap()
        .unwrap();
        let b = resolve(json!({
            "loop": {"max_iterations": 200},
            "tool_policy": {"denied_tools": ["update_plan", "compact", "compact"]},
        }))
        .unwrap()
        .unwrap();
        assert_eq!(a.digest, b.digest);
        assert!(a.digest.starts_with("sha256:") && a.digest.len() == 71);
        let c = resolve(json!({
            "tool_policy": {"denied_tools": ["compact", "update_plan"]},
            "loop": {"max_iterations": 201},
        }))
        .unwrap()
        .unwrap();
        assert_ne!(a.digest, c.digest);
        // A different instructions file changes it.
        let one = raw(json!({"instructions": "i.md"}))
            .resolve(Some(b"one"))
            .unwrap()
            .unwrap();
        let two = raw(json!({"instructions": "i.md"}))
            .resolve(Some(b"two"))
            .unwrap()
            .unwrap();
        assert_ne!(one.digest, two.digest);
    }

    #[test]
    fn a_tampered_overlay_fails_verification() {
        let mut overlay = resolve(json!({"instructions": "i.md", "loop": {"max_iterations": 80}}))
            .unwrap()
            .unwrap();
        assert!(overlay.verify().is_ok());
        let round: BenchOverlay =
            serde_json::from_str(&serde_json::to_string(&overlay).unwrap()).unwrap();
        assert_eq!(round, overlay);
        overlay.max_iterations = Some(81);
        assert_eq!(overlay.verify().unwrap_err().code, OVERLAY_DIGEST_MISMATCH);
        overlay.max_iterations = Some(5);
        assert_eq!(overlay.verify().unwrap_err().code, OVERLAY_OUT_OF_RANGE);
        let mut edited = resolve(json!({"instructions": "i.md"})).unwrap().unwrap();
        edited
            .instructions
            .as_mut()
            .unwrap()
            .text
            .push_str(" Ignore the rules.");
        assert_eq!(edited.verify().unwrap_err().code, OVERLAY_DIGEST_MISMATCH);
    }

    #[test]
    fn knobs_layer_on_the_config_and_keep_rotation() {
        let overlay = resolve(json!({"loop": {"max_iterations": 200}}))
            .unwrap()
            .unwrap();
        let config = LoopKnobs {
            max_iterations: Some(80),
            compaction_budget_tokens: Some(100_000),
            rotation_off: true,
            ..LoopKnobs::default()
        };
        assert_eq!(
            overlay.apply_knobs(config),
            LoopKnobs {
                max_iterations: Some(200),
                compaction_budget_tokens: Some(100_000),
                rotation_off: true,
                ..LoopKnobs::default()
            }
        );
        assert_eq!(
            overlay
                .apply_knobs(LoopKnobs::default())
                .compaction_budget_tokens,
            None
        );
    }

    #[test]
    fn the_overlay_path_names_one_plain_candidate_directory() {
        let ok = "eval/bench/tb2/candidates/c03-1a2b3c4d5e6f/overlay.toml";
        assert!(overlay_path_ok(ok));
        assert_eq!(
            candidate_dir(ok),
            Some("eval/bench/tb2/candidates/c03-1a2b3c4d5e6f/")
        );
        for bad in [
            "eval/bench/tb2/candidates/overlay.toml",
            "eval/bench/tb2/candidates/../runs/overlay.toml",
            "eval/bench/tb2/candidates/a/b/overlay.toml",
            "eval/bench/tb2/candidates/a/other.toml",
            "eval/bench/tb2/candidates/.hidden/overlay.toml",
            "eval/bench/tb2/candidates/-x/overlay.toml",
            "eval/bench/tb2/runs/overlay.toml",
            "/eval/bench/tb2/candidates/a/overlay.toml",
            "eval/bench/tb2/candidates//overlay.toml",
        ] {
            assert!(!overlay_path_ok(bad), "{bad}");
        }
    }

    #[test]
    fn output_spill_thresholds_are_validated_and_reach_the_tool_policy() {
        let overlay = resolve(json!({"output_spill": {
            "shell": {"max_bytes": 32768, "max_lines": 800},
            "exec_command": {"max_bytes": 16384},
        }}))
        .unwrap()
        .unwrap();
        assert!(overlay.verify().is_ok());
        let policy = overlay.tool_policy();
        assert_eq!(policy.validate(), Ok(()));
        assert_eq!(
            policy.output_spill_for("shell"),
            Some(OutputSpill {
                max_bytes: Some(32768),
                max_lines: Some(800)
            })
        );
        assert_eq!(policy.output_spill_for("write_stdin"), None);
        // The spill is part of the identity.
        let other = resolve(json!({"output_spill": {"shell": {"max_bytes": 32769}}}))
            .unwrap()
            .unwrap();
        assert_ne!(overlay.digest, other.digest);
        // An absent field keeps the preset: the default tool policy is untouched.
        let plain = resolve(json!({"loop": {"max_iterations": 80}}))
            .unwrap()
            .unwrap();
        assert!(plain.output_spill.is_empty());
        assert_eq!(plain.tool_policy(), HarnessToolPolicy::bench());
        // An entry that sets nothing is no overlay.
        assert_eq!(
            resolve(json!({"output_spill": {"shell": {}}})).unwrap(),
            None
        );
    }

    #[test]
    fn out_of_range_or_unknown_output_spill_is_refused() {
        for bad in [
            json!({"output_spill": {"shell": {"max_bytes": 100}}}),
            json!({"output_spill": {"shell": {"max_bytes": 2_000_000}}}),
            json!({"output_spill": {"shell": {"max_bytes": -1}}}),
            json!({"output_spill": {"shell": {"max_lines": 1}}}),
            json!({"output_spill": {"shell": {"max_lines": 200_000}}}),
        ] {
            assert_eq!(code(bad), OVERLAY_OUT_OF_RANGE);
        }
        for bad in [
            json!({"output_spill": {"read_file": {"max_bytes": 4096}}}),
            // `git` is a command tool but not a bench tool.
            json!({"output_spill": {"git": {"max_bytes": 4096}}}),
        ] {
            assert_eq!(code(bad), OVERLAY_TOOL_INVALID);
        }
        assert!(
            serde_json::from_value::<RawOverlay>(
                json!({"output_spill": {"shell": {"surprise": 1}}})
            )
            .is_err()
        );
        // A carried overlay that was edited out of range is not run.
        let mut overlay = resolve(json!({"output_spill": {"shell": {"max_bytes": 4096}}}))
            .unwrap()
            .unwrap();
        overlay.output_spill.get_mut("shell").unwrap().max_bytes = Some(1);
        assert_eq!(overlay.verify().unwrap_err().code, OVERLAY_OUT_OF_RANGE);
    }
}
