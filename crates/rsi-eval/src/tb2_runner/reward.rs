//! Verifier reward reading (design "Harbor parity contract", rows "Verifier exit code is not
//! itself the grade" and "Reward text is one finite float"; Harbor 0.24.0 `verifier/verifier.py`).
//!
//! * Reward JSON takes precedence over reward text. Its presence precludes the text fallback,
//!   even when the JSON is invalid.
//! * Text is one finite float with surrounding whitespace allowed. Empty, non-numeric and
//!   non-finite text are typed errors.
//! * JSON is an object of finite numbers; a JSON bool is normalized to 0 or 1 (Python accepts
//!   bool as a number). An empty object is accepted and yields no scalar reward.
//! * The scalar is the `reward` key, else the lexicographically first key's value.
//! * A missing or invalid reward is never synthesized as a valid zero.

use std::collections::BTreeMap;

use rsi_common::bench_runner::TrialExceptionKind;
use serde_json::Value;

/// Reward JSON path inside the task container.
pub const REWARD_JSON_PATH: &str = "/logs/verifier/reward.json";
/// Reward text path inside the task container.
pub const REWARD_TEXT_PATH: &str = "/logs/verifier/reward.txt";

/// A parsed reward: the raw map and the selected scalar.
#[derive(Debug, Clone, PartialEq)]
pub struct Reward {
    pub map: BTreeMap<String, f64>,
    /// `None` only for an empty JSON map.
    pub scalar: Option<f64>,
}

/// Why no reward could be read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RewardError {
    /// Neither reward file exists.
    Missing,
    /// The text file is blank.
    EmptyText,
    /// Not UTF-8, not a number, not JSON, or not an object of numbers.
    Parse(String),
    /// A float that is NaN or infinite.
    NonFinite(String),
}

impl RewardError {
    /// The exception kind this failure is recorded as.
    #[must_use]
    pub const fn kind(&self) -> TrialExceptionKind {
        match self {
            Self::Missing => TrialExceptionKind::RewardMissing,
            Self::EmptyText => TrialExceptionKind::RewardEmpty,
            Self::Parse(_) => TrialExceptionKind::RewardParse,
            Self::NonFinite(_) => TrialExceptionKind::RewardNonFinite,
        }
    }
}

impl std::fmt::Display for RewardError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Missing => write!(f, "no reward file"),
            Self::EmptyText => write!(f, "reward text is empty"),
            Self::Parse(why) => write!(f, "unreadable reward: {why}"),
            Self::NonFinite(text) => write!(f, "non-finite reward {text:?}"),
        }
    }
}

impl std::error::Error for RewardError {}

/// Read the reward from whichever files exist. JSON wins; see the module notes.
///
/// # Errors
/// [`RewardError`] when neither file exists or the chosen file is not a valid reward.
pub fn read_reward(json: Option<&[u8]>, text: Option<&[u8]>) -> Result<Reward, RewardError> {
    match (json, text) {
        (Some(json), _) => parse_json(json),
        (None, Some(text)) => parse_text(text),
        (None, None) => Err(RewardError::Missing),
    }
}

fn parse_text(bytes: &[u8]) -> Result<Reward, RewardError> {
    let text = std::str::from_utf8(bytes)
        .map_err(|e| RewardError::Parse(format!("reward text is not utf-8: {e}")))?
        .trim();
    if text.is_empty() {
        return Err(RewardError::EmptyText);
    }
    let value: f64 = text
        .parse()
        .map_err(|_| RewardError::Parse(format!("reward text {text:?} is not a number")))?;
    if !value.is_finite() {
        return Err(RewardError::NonFinite(text.to_string()));
    }
    Ok(Reward {
        map: BTreeMap::from([("reward".to_string(), value)]),
        scalar: Some(value),
    })
}

fn parse_json(bytes: &[u8]) -> Result<Reward, RewardError> {
    let value: Value = serde_json::from_slice(bytes)
        .map_err(|e| RewardError::Parse(format!("reward json: {e}")))?;
    let Value::Object(object) = value else {
        return Err(RewardError::Parse(
            "reward json is not an object".to_string(),
        ));
    };
    let mut map = BTreeMap::new();
    for (key, entry) in object {
        let number = match entry {
            Value::Bool(flag) => f64::from(u8::from(flag)),
            Value::Number(n) => n.as_f64().ok_or_else(|| {
                RewardError::Parse(format!("reward json value for {key:?} is not a float"))
            })?,
            _ => {
                return Err(RewardError::Parse(format!(
                    "reward json value for {key:?} is not a number"
                )));
            }
        };
        if !number.is_finite() {
            return Err(RewardError::NonFinite(key));
        }
        map.insert(key, number);
    }
    let scalar = map.get("reward").or_else(|| map.values().next()).copied();
    Ok(Reward { map, scalar })
}
