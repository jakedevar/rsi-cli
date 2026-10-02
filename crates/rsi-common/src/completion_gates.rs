//! Operator-configured completion gates for Harness sessions (#794).
//!
//! Gates are an execution surface: only `LaunchSessionParams` (an operator RPC)
//! carries them, and the daemon refuses them for providers that do not run the
//! Harness loop.

use serde::{Deserialize, Serialize};
use std::time::Duration;

pub const COMPLETION_GATES_INVALID: &str = "completion_gates_invalid";
pub const COMPLETION_GATES_UNSUPPORTED_PROVIDER: &str =
    "completion_gates_unsupported_provider";
pub const COMPLETION_GATES_MAX_GATES: usize = 8;
pub const COMPLETION_GATES_MAX_NAME_BYTES: usize = 64;
pub const COMPLETION_GATES_MAX_COMMAND_BYTES: usize = 4096;
pub const COMPLETION_GATES_MAX_ATTEMPTS: u32 = 10;
pub const COMPLETION_GATES_MAX_TIMEOUT_SECS: u64 = 1800;
pub const COMPLETION_GATES_DEFAULT_TIMEOUT_SECS: u64 = 300;
pub const COMPLETION_GATES_MAX_OUTPUT_BYTES: usize = 64 * 1024;
pub const COMPLETION_GATES_DEFAULT_OUTPUT_BYTES: usize = 16 * 1024;
pub const COMPLETION_GATES_DEFAULT_ATTEMPTS: u32 = 3;
pub const COMPLETION_GATES_GATE_TURN_BUDGET: u32 = 5;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CompletionGate {
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub command: String,
    #[serde(default = "default_timeout_secs")]
    pub timeout_secs: u64,
    #[serde(default = "default_max_output_bytes")]
    pub max_output_bytes: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CompletionGates {
    #[serde(default)]
    pub gates: Vec<CompletionGate>,
    #[serde(default = "default_max_attempts")]
    pub max_attempts: u32,
}

fn default_timeout_secs() -> u64 {
    COMPLETION_GATES_DEFAULT_TIMEOUT_SECS
}

fn default_max_output_bytes() -> usize {
    COMPLETION_GATES_DEFAULT_OUTPUT_BYTES
}

fn default_max_attempts() -> u32 {
    COMPLETION_GATES_DEFAULT_ATTEMPTS
}

impl Default for CompletionGates {
    fn default() -> Self {
        Self {
            gates: Vec::new(),
            max_attempts: COMPLETION_GATES_DEFAULT_ATTEMPTS,
        }
    }
}

impl CompletionGate {
    #[must_use]
    pub fn timeout(&self) -> Duration {
        Duration::from_secs(self.timeout_secs)
    }
}

impl CompletionGates {
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.gates.is_empty()
    }

    /// # Errors
    /// Stable `completion_gates_invalid` for a malformed or unbounded set.
    pub fn validate(&self) -> Result<(), &'static str> {
        if self.gates.len() > COMPLETION_GATES_MAX_GATES
            || self.max_attempts == 0
            || self.max_attempts > COMPLETION_GATES_MAX_ATTEMPTS
        {
            return Err(COMPLETION_GATES_INVALID);
        }
        let mut names = Vec::with_capacity(self.gates.len());
        for gate in &self.gates {
            let valid_name = !gate.name.is_empty()
                && gate.name.len() <= COMPLETION_GATES_MAX_NAME_BYTES
                && gate
                    .name
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_');
            if !valid_name
                || gate.command.is_empty()
                || gate.command.len() > COMPLETION_GATES_MAX_COMMAND_BYTES
                || gate.timeout_secs == 0
                || gate.timeout_secs > COMPLETION_GATES_MAX_TIMEOUT_SECS
                || gate.max_output_bytes == 0
                || gate.max_output_bytes > COMPLETION_GATES_MAX_OUTPUT_BYTES
            {
                return Err(COMPLETION_GATES_INVALID);
            }
            names.push(gate.name.clone());
        }
        names.sort_unstable();
        if names.windows(2).any(|pair| pair[0] == pair[1]) {
            return Err(COMPLETION_GATES_INVALID);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gate(name: &str, command: &str) -> CompletionGate {
        CompletionGate {
            name: name.into(),
            command: command.into(),
            timeout_secs: default_timeout_secs(),
            max_output_bytes: default_max_output_bytes(),
        }
    }

    #[test]
    fn empty_gates_are_valid_and_have_defaults() {
        let gates = CompletionGates::default();
        assert_eq!(gates.validate(), Ok(()));
        assert!(gates.is_empty());
        assert_eq!(gates.max_attempts, 3);
    }

    #[test]
    fn gate_bounds_are_enforced() {
        let mut gates = CompletionGates {
            gates: vec![gate("test", "true")],
            max_attempts: 10,
        };
        assert_eq!(gates.validate(), Ok(()));

        gates.gates[0].name = "Not A Gate".into();
        assert_eq!(gates.validate(), Err(COMPLETION_GATES_INVALID));
        gates.gates[0].name = String::new();
        assert_eq!(gates.validate(), Err(COMPLETION_GATES_INVALID));
        gates.gates[0].name = "test".into();
        gates.gates[0].command = String::new();
        assert_eq!(gates.validate(), Err(COMPLETION_GATES_INVALID));
        gates.gates[0].command = "true".into();
        gates.gates[0].timeout_secs = 1801;
        assert_eq!(gates.validate(), Err(COMPLETION_GATES_INVALID));
        gates.gates[0].timeout_secs = 300;
        gates.gates[0].max_output_bytes = COMPLETION_GATES_MAX_OUTPUT_BYTES + 1;
        assert_eq!(gates.validate(), Err(COMPLETION_GATES_INVALID));
        gates.gates[0].max_output_bytes = 16 * 1024;
        gates.max_attempts = 11;
        assert_eq!(gates.validate(), Err(COMPLETION_GATES_INVALID));
        gates.max_attempts = 3;
        gates.gates.push(gate("test", "false"));
        assert_eq!(gates.validate(), Err(COMPLETION_GATES_INVALID));
    }

    #[test]
    fn unknown_fields_are_refused() {
        assert!(
            serde_json::from_value::<CompletionGates>(serde_json::json!({"gates": []}))
                .unwrap()
                .is_empty()
        );
        assert!(serde_json::from_str::<CompletionGates>(
            r#"{"gates":[],"max_attempts":3,"extra":true}"#
        )
        .is_err());
    }
}
