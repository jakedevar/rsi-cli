//! Shared types for the CompilePrompt RPC.
//!
//! Used by the daemon (producer) and TUI (consumer). Wire contract.

use serde::{Deserialize, Serialize};

/// The structured output of a prompt compilation pass.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompileResult {
    /// The compiled prompt text, with the contract line stripped.
    pub compiled: String,
    /// The parsed output contract from the final line.
    pub contract: OutputContract,
    /// Results of the per-layer validation pass.
    pub layer_validation: LayerValidation,
}

/// The output contract appended by the compiler to every compiled prompt.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
#[non_exhaustive]
pub enum OutputContract {
    Complete,
    Incomplete { criterion: String },
    Error { kind: String, message: String },
}

impl OutputContract {
    pub fn to_status_string(&self) -> String {
        match self {
            OutputContract::Complete => "complete".to_string(),
            OutputContract::Incomplete { criterion } => format!("incomplete:{criterion}"),
            OutputContract::Error { kind, message } => format!("error:{kind}:{message}"),
        }
    }
}

/// Which of the 5 linguistic layers are present in the compiled output.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LayerValidation {
    pub semantic: bool,
    pub syntactic: bool,
    pub deictic: bool,
    pub discourse: bool,
    pub pragmatic: bool,
}

impl LayerValidation {
    pub fn all_present(&self) -> bool {
        self.semantic && self.syntactic && self.deictic && self.discourse && self.pragmatic
    }

    pub fn missing(&self) -> Vec<&'static str> {
        let mut v = Vec::new();
        if !self.semantic {
            v.push("SEMANTIC");
        }
        if !self.syntactic {
            v.push("SYNTACTIC");
        }
        if !self.deictic {
            v.push("DEICTIC");
        }
        if !self.discourse {
            v.push("DISCOURSE");
        }
        if !self.pragmatic {
            v.push("PRAGMATIC");
        }
        v
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compile_result_round_trip() {
        let result = CompileResult {
            compiled: "Do X then Y.".to_string(),
            contract: OutputContract::Complete,
            layer_validation: LayerValidation {
                semantic: true,
                syntactic: true,
                deictic: true,
                discourse: true,
                pragmatic: true,
            },
        };
        let json = serde_json::to_string(&result).unwrap();
        let back: CompileResult = serde_json::from_str(&json).unwrap();
        assert_eq!(back.compiled, "Do X then Y.");
        assert_eq!(back.contract, OutputContract::Complete);
        assert!(back.layer_validation.all_present());
    }

    #[test]
    fn output_contract_incomplete_round_trip() {
        let c = OutputContract::Incomplete {
            criterion: "missing scope".to_string(),
        };
        let json = serde_json::to_string(&c).unwrap();
        let back: OutputContract = serde_json::from_str(&json).unwrap();
        assert_eq!(c, back);
    }

    #[test]
    fn output_contract_error_round_trip() {
        let c = OutputContract::Error {
            kind: "VALIDATION".to_string(),
            message: "field X".to_string(),
        };
        let json = serde_json::to_string(&c).unwrap();
        let back: OutputContract = serde_json::from_str(&json).unwrap();
        assert_eq!(c, back);
    }

    #[test]
    fn status_string_formatting() {
        assert_eq!(OutputContract::Complete.to_status_string(), "complete");
        assert_eq!(
            OutputContract::Incomplete {
                criterion: "x".into()
            }
            .to_status_string(),
            "incomplete:x"
        );
        assert_eq!(
            OutputContract::Error {
                kind: "K".into(),
                message: "M".into()
            }
            .to_status_string(),
            "error:K:M"
        );
    }

    #[test]
    fn missing_lists_absent_layers() {
        let v = LayerValidation {
            semantic: true,
            syntactic: false,
            deictic: true,
            discourse: false,
            pragmatic: true,
        };
        assert!(!v.all_present());
        assert_eq!(v.missing(), vec!["SYNTACTIC", "DISCOURSE"]);
    }
}
