//! Command parser for the file viewer's `:` command line.
//!
//! Intentionally separate from `commands.rs` (global app commands) to avoid
//! collision: `:q` in the file viewer closes the viewer, not the whole app.

/// Result of parsing a file viewer command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FileViewerCommand {
    /// `:w` — save the file.
    Save,
    /// `:wq` or `:x` — save and close the viewer.
    SaveAndClose,
    /// `:w!` — force save, overwriting external changes.
    ForceSave,
    /// `:q` — close if not dirty, else show error.
    Close,
    /// `:q!` — close unconditionally, discarding changes.
    ForceClose,
    /// `:e!` — revert to disk version.
    Revert,
    /// `:{n}` — jump to line number (1-based).
    GoToLine(usize),
    /// `:set autopair` / `:set noautopair` — toggle bracket auto-pairing.
    SetAutoPair(bool),
    /// Command was not recognized or input was empty.
    Unknown(String),
}

/// One documented file-viewer ex form.
///
/// The spelling the operator types (after `:`), the command it parses to, and
/// what it does. The manual renders these;
/// `docs_round_trip_parser` feeds every example through the real parser.
pub struct FileViewerCommandDoc {
    pub example: &'static str,
    pub parses_to: FileViewerCommand,
    pub summary: &'static str,
}

const fn doc(
    example: &'static str,
    parses_to: FileViewerCommand,
    summary: &'static str,
) -> FileViewerCommandDoc {
    FileViewerCommandDoc {
        example,
        parses_to,
        summary,
    }
}

/// Every file-viewer ex form, in manual order.
pub static FILE_VIEWER_COMMAND_DOCS: &[FileViewerCommandDoc] = &[
    doc("w", FileViewerCommand::Save, "Save the file."),
    doc(
        "w!",
        FileViewerCommand::ForceSave,
        "Save, overwriting changes made on disk since the file was opened.",
    ),
    doc(
        "wq",
        FileViewerCommand::SaveAndClose,
        "Save and close the viewer.",
    ),
    doc(
        "x",
        FileViewerCommand::SaveAndClose,
        "Save and close the viewer (same as `:wq`).",
    ),
    doc(
        "q",
        FileViewerCommand::Close,
        "Close the viewer; refused while there are unsaved changes.",
    ),
    doc(
        "q!",
        FileViewerCommand::ForceClose,
        "Close the viewer, discarding unsaved changes.",
    ),
    doc(
        "quit!",
        FileViewerCommand::ForceClose,
        "Close the viewer, discarding unsaved changes.",
    ),
    doc(
        "e!",
        FileViewerCommand::Revert,
        "Revert the buffer to the version on disk.",
    ),
    doc(
        "42",
        FileViewerCommand::GoToLine(42),
        "Jump to a line number (1-based): `:<n>`.",
    ),
    doc(
        "set autopair",
        FileViewerCommand::SetAutoPair(true),
        "Turn bracket and quote auto-pairing on.",
    ),
    doc(
        "set noautopair",
        FileViewerCommand::SetAutoPair(false),
        "Turn bracket and quote auto-pairing off.",
    ),
];

/// Parse a command string (without the leading `:`).
pub fn parse_file_command(input: &str) -> FileViewerCommand {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return FileViewerCommand::Unknown(String::new());
    }

    // Line number: purely numeric input
    if let Ok(n) = trimmed.parse::<usize>() {
        return FileViewerCommand::GoToLine(n);
    }

    let (cmd, args) = match trimmed.split_once(char::is_whitespace) {
        Some((c, a)) => (c, Some(a.trim())),
        None => (trimmed, None),
    };

    match cmd {
        "w" => FileViewerCommand::Save,
        "w!" => FileViewerCommand::ForceSave,
        "wq" | "x" => FileViewerCommand::SaveAndClose,
        "q" => FileViewerCommand::Close,
        "q!" | "quit!" => FileViewerCommand::ForceClose,
        "e!" => FileViewerCommand::Revert,
        "set" => match args {
            Some("autopair") => FileViewerCommand::SetAutoPair(true),
            Some("noautopair") => FileViewerCommand::SetAutoPair(false),
            _ => FileViewerCommand::Unknown(trimmed.to_string()),
        },
        _ => FileViewerCommand::Unknown(trimmed.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// T9: every documented example parses to its documented command.
    #[test]
    fn docs_round_trip_parser() {
        for doc in FILE_VIEWER_COMMAND_DOCS {
            assert_eq!(
                parse_file_command(doc.example),
                doc.parses_to,
                ":{} parses to its documented command",
                doc.example
            );
            assert!(!doc.summary.is_empty());
        }
    }

    #[test]
    fn test_goto_line() {
        assert_eq!(parse_file_command("42"), FileViewerCommand::GoToLine(42));
        assert_eq!(parse_file_command("1"), FileViewerCommand::GoToLine(1));
        assert_eq!(parse_file_command("999"), FileViewerCommand::GoToLine(999));
    }

    #[test]
    fn test_save() {
        assert_eq!(parse_file_command("w"), FileViewerCommand::Save);
    }

    #[test]
    fn test_save_and_close() {
        assert_eq!(parse_file_command("wq"), FileViewerCommand::SaveAndClose);
        assert_eq!(parse_file_command("x"), FileViewerCommand::SaveAndClose);
    }

    #[test]
    fn test_force_save() {
        assert_eq!(parse_file_command("w!"), FileViewerCommand::ForceSave);
    }

    #[test]
    fn test_close() {
        assert_eq!(parse_file_command("q"), FileViewerCommand::Close);
    }

    #[test]
    fn test_force_close() {
        assert_eq!(parse_file_command("q!"), FileViewerCommand::ForceClose);
        assert_eq!(parse_file_command("quit!"), FileViewerCommand::ForceClose);
    }

    #[test]
    fn test_revert() {
        assert_eq!(parse_file_command("e!"), FileViewerCommand::Revert);
    }

    #[test]
    fn test_set_autopair() {
        assert_eq!(
            parse_file_command("set autopair"),
            FileViewerCommand::SetAutoPair(true)
        );
        assert_eq!(
            parse_file_command("set noautopair"),
            FileViewerCommand::SetAutoPair(false)
        );
    }

    #[test]
    fn test_whitespace_trimming() {
        assert_eq!(parse_file_command("  w  "), FileViewerCommand::Save);
        assert_eq!(
            parse_file_command("  42  "),
            FileViewerCommand::GoToLine(42)
        );
    }

    #[test]
    fn test_unknown() {
        assert_eq!(
            parse_file_command("foobar"),
            FileViewerCommand::Unknown("foobar".to_string())
        );
    }

    #[test]
    fn test_empty() {
        assert_eq!(
            parse_file_command(""),
            FileViewerCommand::Unknown(String::new())
        );
    }
}
