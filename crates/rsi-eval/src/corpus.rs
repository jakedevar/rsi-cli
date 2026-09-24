//! Corpus loader for the eval/replay harness.
//!
//! Walks `eval/corpus/<name>/` (or `eval/corpus/` when `name == "default"`),
//! parses each `<ticket-id>/expected.json`, reads `prompt.txt`, and optionally
//! reads `system_prompt.txt`. Errors fail closed — a single missing file
//! aborts the entire run. See `eval/README.md` for the schema contract.

use crate::errors::{EvalError, Result};
use rsi_common::types::SessionKind;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// Per-ticket expectation rows (matches `expected.json` schema).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CorpusExpected {
    pub kind: SessionKindLabel,
    pub expected_completion_status: String,
    pub expected_test_passed: Option<bool>,
    pub expected_clippy_passed: Option<bool>,
    pub expected_partial: bool,
}

/// String wrapper around `SessionKind` for the JSON wire format. The corpus
/// uses display-strings ("Bug", "Feature", …) rather than enum-discriminant
/// integers so corpus designers can hand-edit JSON without a serde lookup.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SessionKindLabel(pub String);

impl SessionKindLabel {
    /// Translate the corpus label into a real `SessionKind`. Unknown labels
    /// fall back to `Standard` with a warning logged via `tracing`.
    #[must_use]
    pub fn to_session_kind(&self) -> SessionKind {
        match self.0.as_str() {
            "Bug" => SessionKind::Bug,
            "Feature" => SessionKind::Feature,
            "Refactor" => SessionKind::Refactor,
            "Research" => SessionKind::Research,
            "Standard" => SessionKind::Standard,
            "TaskRabbit" => SessionKind::TaskRabbit,
            "Group" => SessionKind::Group,
            "Epic" => SessionKind::Epic,
            "Story" => SessionKind::Story,
            "Task" => SessionKind::Task,
            other => {
                tracing::warn!(
                    label = other,
                    "Unknown corpus kind label; falling back to Standard"
                );
                SessionKind::Standard
            }
        }
    }
}

#[derive(Debug, Clone)]
pub struct CorpusTicket {
    /// Directory name under `eval/corpus/`.
    pub id: String,
    /// `SessionKind` derived from `expected.json`.
    pub kind: SessionKind,
    /// Verbatim contents of `prompt.txt`.
    pub prompt: String,
    /// Verbatim contents of `system_prompt.txt`, or `None` if absent. The
    /// driver passes this verbatim through `LaunchSessionParams.system_prompt`
    /// when set, with `skip_context_pipeline=true` so the daemon does not
    /// alter it.
    pub system_prompt: Option<String>,
    /// Parsed `expected.json` row.
    pub expected: CorpusExpected,
}

/// Load every ticket directory under `<root>/<corpus_name>/` (or `<root>/`
/// when `corpus_name == "default"`). Returns tickets in alphabetical order
/// by directory name.
///
/// `root` is typically the workspace's `eval/corpus/` directory. Fails closed
/// on any missing required file, missing `expected.json` parse error, or
/// I/O error.
pub fn load_corpus(root: &Path, corpus_name: &str) -> Result<Vec<CorpusTicket>> {
    let target = if corpus_name == "default" {
        root.to_path_buf()
    } else {
        root.join(corpus_name)
    };

    if !target.is_dir() {
        return Err(EvalError::Corpus(format!(
            "corpus root {} does not exist",
            target.display()
        )));
    }

    let mut entries: Vec<PathBuf> = std::fs::read_dir(&target)?
        .filter_map(|res| res.ok().map(|e| e.path()))
        .filter(|p| p.is_dir())
        .collect();
    entries.sort();

    let mut tickets = Vec::with_capacity(entries.len());
    for dir in entries {
        let id = dir
            .file_name()
            .and_then(|s| s.to_str())
            .ok_or_else(|| EvalError::Corpus(format!("invalid path: {}", dir.display())))?
            .to_string();

        let expected = load_expected(&dir)?;
        let prompt = load_prompt(&dir)?;
        let system_prompt = load_system_prompt(&dir)?;

        let kind = expected.kind.to_session_kind();

        tickets.push(CorpusTicket {
            id,
            kind,
            prompt,
            system_prompt,
            expected,
        });
    }

    Ok(tickets)
}

fn load_expected(dir: &Path) -> Result<CorpusExpected> {
    let path = dir.join("expected.json");
    let raw = std::fs::read_to_string(&path).map_err(|e| {
        EvalError::Corpus(format!("missing or unreadable {}: {}", path.display(), e))
    })?;
    serde_json::from_str(&raw)
        .map_err(|e| EvalError::Corpus(format!("invalid JSON in {}: {}", path.display(), e)))
}

fn load_prompt(dir: &Path) -> Result<String> {
    let path = dir.join("prompt.txt");
    std::fs::read_to_string(&path)
        .map_err(|e| EvalError::Corpus(format!("missing or unreadable {}: {}", path.display(), e)))
}

fn load_system_prompt(dir: &Path) -> Result<Option<String>> {
    let path = dir.join("system_prompt.txt");
    if path.is_file() {
        Ok(Some(std::fs::read_to_string(&path)?))
    } else {
        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    fn write_ticket(
        root: &Path,
        id: &str,
        prompt: &str,
        expected: &CorpusExpected,
        system_prompt: Option<&str>,
    ) {
        let dir = root.join(id);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("prompt.txt"), prompt).unwrap();
        fs::write(
            dir.join("expected.json"),
            serde_json::to_string_pretty(expected).unwrap(),
        )
        .unwrap();
        if let Some(sp) = system_prompt {
            fs::write(dir.join("system_prompt.txt"), sp).unwrap();
        }
        fs::write(dir.join("ticket.md"), format!("# {id}\n")).unwrap();
    }

    fn impl_expected() -> CorpusExpected {
        CorpusExpected {
            kind: SessionKindLabel("Bug".to_string()),
            expected_completion_status: "Completed".to_string(),
            expected_test_passed: Some(true),
            expected_clippy_passed: Some(true),
            expected_partial: false,
        }
    }

    #[test]
    fn loads_two_tickets_in_alphabetical_order() {
        let dir = TempDir::new().unwrap();
        write_ticket(dir.path(), "b-second", "Q2", &impl_expected(), None);
        write_ticket(dir.path(), "a-first", "Q1", &impl_expected(), Some("SP1"));

        let tickets = load_corpus(dir.path(), "default").unwrap();
        assert_eq!(tickets.len(), 2);
        assert_eq!(tickets[0].id, "a-first");
        assert_eq!(tickets[0].prompt, "Q1");
        assert_eq!(tickets[0].system_prompt.as_deref(), Some("SP1"));
        assert_eq!(tickets[1].id, "b-second");
        assert!(tickets[1].system_prompt.is_none());
    }

    #[test]
    fn loads_real_workspace_corpus() {
        // Walk up from CARGO_MANIFEST_DIR (crates/rsi-eval) to the workspace
        // root, then look for eval/corpus/.
        let manifest = std::env::var("CARGO_MANIFEST_DIR").unwrap();
        let workspace_root = PathBuf::from(&manifest)
            .ancestors()
            .find(|p| p.join("eval/corpus").is_dir())
            .map(Path::to_path_buf)
            .expect("workspace eval/corpus must exist");
        let corpus_root = workspace_root.join("eval/corpus");
        let tickets = load_corpus(&corpus_root, "default").unwrap();
        assert_eq!(tickets.len(), 10, "expected 10 corpus tickets");

        // The representative ticket is fully populated.
        let representative = tickets
            .iter()
            .find(|t| t.id == "impl-001-trivial-bugfix")
            .expect("representative ticket must load");
        assert!(representative.system_prompt.is_some());
        assert_eq!(representative.kind, SessionKind::Bug);
        assert_eq!(representative.expected.expected_partial, false);
    }

    #[test]
    fn rejects_missing_prompt() {
        let dir = TempDir::new().unwrap();
        let ticket_dir = dir.path().join("missing-prompt");
        fs::create_dir_all(&ticket_dir).unwrap();
        fs::write(
            ticket_dir.join("expected.json"),
            serde_json::to_string(&impl_expected()).unwrap(),
        )
        .unwrap();
        // intentionally NOT writing prompt.txt
        fs::write(ticket_dir.join("ticket.md"), "# missing\n").unwrap();

        let result = load_corpus(dir.path(), "default");
        assert!(result.is_err());
        if let Err(EvalError::Corpus(msg)) = result {
            assert!(msg.contains("prompt.txt"), "unexpected error: {msg}");
        } else {
            panic!("expected Corpus error");
        }
    }

    #[test]
    fn rejects_invalid_expected_json() {
        let dir = TempDir::new().unwrap();
        let ticket_dir = dir.path().join("bad-json");
        fs::create_dir_all(&ticket_dir).unwrap();
        fs::write(ticket_dir.join("expected.json"), "{").unwrap();
        fs::write(ticket_dir.join("prompt.txt"), "Q").unwrap();

        let result = load_corpus(dir.path(), "default");
        assert!(matches!(result, Err(EvalError::Corpus(_))));
    }

    #[test]
    fn rejects_missing_corpus_root() {
        let dir = TempDir::new().unwrap();
        let result = load_corpus(&dir.path().join("nonexistent"), "default");
        assert!(matches!(result, Err(EvalError::Corpus(_))));
    }
}
