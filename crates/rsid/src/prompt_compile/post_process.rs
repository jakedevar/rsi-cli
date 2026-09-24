//! Post-processing: parse contract, validate layers, strip think tags,
//! extract compile-error. Ported verbatim from
//! `crates/flywheel/src/prompt_processor.rs` (pre-migration) and re-homed here
//! so the daemon can produce `CompileResult`s directly.

use rsi_common::prompt_compile::{LayerValidation, OutputContract};

/// Extract `COMPILE_ERROR:AMBIGUOUS_INTENT:[desc]` if present anywhere in output.
pub fn extract_compile_error(s: &str) -> Option<String> {
    for line in s.lines() {
        if let Some(rest) = line.strip_prefix("COMPILE_ERROR:AMBIGUOUS_INTENT:") {
            return Some(rest.trim().to_string());
        }
    }
    None
}

/// Split the compiled body from the final contract line.
pub fn parse_contract(s: &str) -> (String, OutputContract) {
    let trimmed = s.trim();
    let mut lines: Vec<&str> = trimmed.lines().collect();

    while lines.last().is_some_and(|l| l.trim().is_empty()) {
        lines.pop();
    }

    let contract = match lines.last() {
        None => OutputContract::Incomplete {
            criterion: "no contract line".to_string(),
        },
        Some(&last) => {
            let parsed = parse_contract_line(last);
            if parsed
                != (OutputContract::Incomplete {
                    criterion: format!("malformed contract: {last}"),
                })
            {
                lines.pop();
            }
            parsed
        }
    };

    let body = lines.join("\n").trim_end().to_string();
    (body, contract)
}

pub fn parse_contract_line(line: &str) -> OutputContract {
    let line = line.trim();
    if line == "COMPLETE" {
        return OutputContract::Complete;
    }
    if let Some(rest) = line.strip_prefix("INCOMPLETE:") {
        return OutputContract::Incomplete {
            criterion: rest.to_string(),
        };
    }
    if let Some(rest) = line.strip_prefix("ERROR:")
        && let Some((kind, msg)) = rest.split_once(':')
    {
        return OutputContract::Error {
            kind: kind.to_string(),
            message: msg.to_string(),
        };
    }
    OutputContract::Incomplete {
        criterion: format!("malformed contract: {line}"),
    }
}

/// Heuristic layer validator. Each check is a keyword scan on the compiled text.
pub fn validate_layers(text: &str) -> LayerValidation {
    let lower = text.to_lowercase();

    const OPERATIONAL_VERBS: &[&str] = &[
        "extract",
        "classify",
        "return",
        "compare",
        "generate",
        "validate",
        "filter",
        "transform",
        "emit",
        "parse",
        "compute",
        "list",
        "output",
        "produce",
        "construct",
        "define",
        "assert",
        "enumerate",
        "serialize",
    ];
    let semantic = OPERATIONAL_VERBS.iter().any(|&v| contains_word(&lower, v));

    let syntactic = [
        "for each",
        "if ",
        "given ",
        "never ",
        "always ",
        "when ",
        "before ",
        "only proceed",
        "is valid if",
        "compare ",
    ]
    .iter()
    .any(|&p| lower.contains(p));

    let padded = format!(" {lower} ");
    let deictic = ![" it ", " they ", " them ", " this ", " those ", " these "]
        .iter()
        .any(|&p| padded.contains(p));

    let discourse = [
        "first,",
        "then,",
        "next,",
        "finally,",
        "if ",
        "unless ",
        "after ",
        "before ",
        "subsequently",
    ]
    .iter()
    .any(|&m| lower.contains(m));

    let first_line = text
        .lines()
        .find(|l| !l.trim().is_empty())
        .unwrap_or("")
        .to_lowercase();
    let pragmatic = [
        "machine parser",
        "specification",
        "executing",
        "you are",
        "as a ",
        "acting as",
        "structured",
        "analytical",
        "your output",
        "your task",
        "write naturally",
        "human reader",
        "evaluate evidence",
        "confidence level",
        "produce",
    ]
    .iter()
    .any(|&f| first_line.contains(f));

    LayerValidation {
        semantic,
        syntactic,
        deictic,
        discourse,
        pragmatic,
    }
}

/// Check if `word` appears in `text` bounded by non-alphanumeric characters
/// (or string edges). Both inputs are assumed already lowercased.
pub fn contains_word(text: &str, word: &str) -> bool {
    let text_bytes = text.as_bytes();
    let word_len = word.len();
    let mut start = 0;
    while let Some(pos) = text[start..].find(word) {
        let abs = start + pos;
        let before_ok = abs == 0 || !text_bytes[abs - 1].is_ascii_alphanumeric();
        let after_pos = abs + word_len;
        let after_ok =
            after_pos >= text_bytes.len() || !text_bytes[after_pos].is_ascii_alphanumeric();
        if before_ok && after_ok {
            return true;
        }
        start = abs + 1;
    }
    false
}

/// Remove `<think>...</think>` sections emitted by Qwen3 in reasoning mode.
pub fn strip_think_tags(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut depth = 0usize;
    let mut i = 0;
    let bytes = s.as_bytes();
    while i < bytes.len() {
        if bytes[i..].starts_with(b"<think>") {
            depth += 1;
            i += 7;
        } else if bytes[i..].starts_with(b"</think>") {
            depth = depth.saturating_sub(1);
            i += 8;
        } else {
            if depth == 0 {
                out.push(bytes[i] as char);
            }
            i += 1;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strip_think_tags_removes_thinking_blocks() {
        let input = "<think>reasoning here</think>Rewritten prompt.";
        assert_eq!(strip_think_tags(input), "Rewritten prompt.");
    }

    #[test]
    fn strip_think_tags_noop_on_clean_text() {
        let input = "Just a normal prompt.";
        assert_eq!(strip_think_tags(input), "Just a normal prompt.");
    }

    #[test]
    fn strip_think_tags_nested() {
        let input = "<think>outer <think>inner</think> still outer</think>Result.";
        assert_eq!(strip_think_tags(input), "Result.");
    }

    #[test]
    fn extract_compile_error_found() {
        let input = "COMPILE_ERROR:AMBIGUOUS_INTENT:unclear target file";
        assert_eq!(
            extract_compile_error(input),
            Some("unclear target file".to_string())
        );
    }

    #[test]
    fn extract_compile_error_none() {
        let input = "For each file in the directory, return the name.\nCOMPLETE";
        assert_eq!(extract_compile_error(input), None);
    }

    #[test]
    fn parse_contract_complete() {
        let input = "Some compiled prompt text.\nCOMPLETE";
        let (body, contract) = parse_contract(input);
        assert_eq!(body, "Some compiled prompt text.");
        assert_eq!(contract, OutputContract::Complete);
    }

    #[test]
    fn parse_contract_incomplete() {
        let input = "Prompt body.\nINCOMPLETE:missing scope";
        let (body, contract) = parse_contract(input);
        assert_eq!(body, "Prompt body.");
        assert_eq!(
            contract,
            OutputContract::Incomplete {
                criterion: "missing scope".to_string()
            }
        );
    }

    #[test]
    fn parse_contract_error() {
        let input = "Prompt body.\nERROR:VALIDATION:field X is missing";
        let (body, contract) = parse_contract(input);
        assert_eq!(body, "Prompt body.");
        assert_eq!(
            contract,
            OutputContract::Error {
                kind: "VALIDATION".to_string(),
                message: "field X is missing".to_string()
            }
        );
    }

    #[test]
    fn parse_contract_no_contract_line() {
        let input = "Just a prompt with no contract.";
        let (body, contract) = parse_contract(input);
        assert_eq!(body, "Just a prompt with no contract.");
        assert!(matches!(contract, OutputContract::Incomplete { .. }));
    }

    #[test]
    fn validate_layers_full() {
        let text = "You are investigating a production defect. Treat the following as a specification.\n\
                     First, extract all error-level entries from the application error logs.\n\
                     Then, for each extracted error entry, classify the entry by root cause category.\n\
                     Next, compare the timestamps and stack traces against the commit history.\n\
                     Finally, return a verdict: whether the memory leak correlates with the cache changes.";
        let v = validate_layers(text);
        assert!(v.semantic);
        assert!(v.syntactic);
        assert!(v.deictic);
        assert!(v.discourse);
        assert!(v.pragmatic);
        assert!(v.all_present());
    }

    #[test]
    fn validate_layers_missing_pragmatic() {
        let text = "Extract all names from the list.\nReturn them as JSON.";
        let v = validate_layers(text);
        assert!(v.semantic);
        assert!(!v.pragmatic);
        assert!(!v.all_present());
        assert!(v.missing().contains(&"PRAGMATIC"));
    }

    #[test]
    fn validate_layers_creative_frame() {
        let text = "Write naturally for a human reader.\nFirst, generate three title options.";
        let v = validate_layers(text);
        assert!(v.pragmatic);
        assert!(v.semantic);
        assert!(v.discourse);
    }

    #[test]
    fn validate_layers_research_frame() {
        let text =
            "Evaluate evidence and state confidence levels explicitly.\nFirst, extract all claims.";
        let v = validate_layers(text);
        assert!(v.pragmatic);
    }

    #[test]
    fn validate_layers_detects_pronouns() {
        let text = "Your output is structured.\nExtract it from the file.";
        let v = validate_layers(text);
        assert!(!v.deictic);
        assert!(v.missing().contains(&"DEICTIC"));
    }

    #[test]
    fn contains_word_basic() {
        assert!(contains_word("extract all items", "extract"));
        assert!(contains_word("then extract, filter", "extract"));
        assert!(!contains_word("do not invalidate", "validate"));
        assert!(contains_word("validate the input", "validate"));
    }

    #[test]
    fn contains_word_edges() {
        assert!(contains_word("extract", "extract"));
        assert!(contains_word("extract the data", "extract"));
        assert!(contains_word("then extract", "extract"));
        assert!(!contains_word("reextract", "extract"));
    }

    #[test]
    fn discourse_no_false_positive_first_principles() {
        let text =
            "Your output is structured.\nApply first principles thinking to extract the answer.";
        let v = validate_layers(text);
        assert!(!v.discourse);
    }
}
