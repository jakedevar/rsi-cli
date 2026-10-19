//! Operator prompt-file RPCs (hub #3040, local #74): the TUI prompt creator's
//! `~/.rsi/prompts/*.md` store, served by `rsid` so the desktop can use it
//! without touching the disk itself.
//!
//! - [`METHOD_LIST`]: every `*.md` prompt, newest first.
//! - [`METHOD_GET`]: one prompt's text.
//! - [`METHOD_PUT`]: write one prompt atomically (temp file + rename in the
//!   prompts directory), optionally only when it does not exist yet.
//! - [`METHOD_DELETE`]: move one prompt to the recoverable trash
//!   `~/.rsi/trash/prompts/<UTC timestamp>-<name>`; nothing is unlinked.
//!
//! A prompt is named by a flat file name ([`validate_prompt_name`]): no
//! separator, no `..`, not absolute, ending in `.md`. A symlinked prompt is
//! refused. The directory is resolved under the rsi data directory
//! (`rsi_common::identity::data_dir()`), never a hard-coded home path. None of
//! these methods is in the attributed verb registry (operator-only, AGENTS.md
//! rule 10) and none is on the rsi-remote gateway.

use serde::{Deserialize, Serialize};

pub const METHOD_LIST: &str = "ListPromptFiles";
pub const METHOD_GET: &str = "GetPromptFile";
pub const METHOD_PUT: &str = "PutPromptFile";
pub const METHOD_DELETE: &str = "DeletePromptFile";

/// Every prompt-file method, for catalog and fence tests.
pub const PROMPT_FILE_METHODS: &[&str] = &[METHOD_LIST, METHOD_GET, METHOD_PUT, METHOD_DELETE];

/// Prompts directory, relative to the rsi data directory.
pub const PROMPTS_DIR_NAME: &str = "prompts";
/// Trash directory of deleted prompts, relative to the rsi data directory.
pub const PROMPT_TRASH_DIR: [&str; 2] = ["trash", "prompts"];

/// Largest prompt a get returns or a put writes.
pub const MAX_PROMPT_FILE_BYTES: usize = 1 << 20;
/// Longest prompt name, in bytes.
pub const MAX_PROMPT_NAME_BYTES: usize = 200;
/// Most prompts one listing returns (then `truncated`).
pub const MAX_LISTED_PROMPTS: usize = 2_000;
/// Directory entries one listing reads before it stops (then `truncated`).
pub const PROMPT_SCAN_LIMIT: usize = 20_000;
/// Bytes read from each prompt to find its first line.
pub const FIRST_LINE_SCAN_BYTES: usize = 4 << 10;
/// Longest `first_line`, in characters.
pub const FIRST_LINE_MAX_CHARS: usize = 200;

/// Refusal codes (the `InvalidParam` message prefix).
pub const CODE_INVALID_REQUEST: &str = "prompt_file_invalid_request";
pub const CODE_INVALID_NAME: &str = "prompt_file_invalid_name";
pub const CODE_NOT_FOUND: &str = "prompt_file_not_found";
pub const CODE_SYMLINK: &str = "prompt_file_symlink";
pub const CODE_NOT_A_FILE: &str = "prompt_file_not_a_file";
pub const CODE_TOO_LARGE: &str = "prompt_file_too_large";
pub const CODE_NOT_UTF8: &str = "prompt_file_not_utf8";
pub const CODE_EXISTS: &str = "prompt_file_exists";
pub const CODE_OUTSIDE_ROOT: &str = "prompt_file_outside_root";

/// Check a prompt name: one flat `*.md` file name. Returns the reason a name
/// is refused (the caller prefixes [`CODE_INVALID_NAME`]).
pub fn validate_prompt_name(name: &str) -> Result<(), &'static str> {
    if name.is_empty() {
        return Err("the name is empty");
    }
    if name.len() > MAX_PROMPT_NAME_BYTES {
        return Err("the name is longer than 200 bytes");
    }
    if name.contains('/') || name.contains('\\') {
        return Err("the name contains a path separator; name a flat file");
    }
    if name.contains("..") {
        return Err("the name contains '..'");
    }
    // ':' is a Windows drive prefix or alternate data stream.
    if name.contains(':') {
        return Err("the name contains ':'");
    }
    if name.chars().any(char::is_control) {
        return Err("the name contains a control character");
    }
    if name.starts_with('.') {
        return Err("the name starts with '.'");
    }
    if name.trim() != name {
        return Err("the name starts or ends with whitespace");
    }
    let Some(stem) = name.strip_suffix(".md") else {
        return Err("the name does not end in .md");
    };
    if stem.is_empty() {
        return Err("the name has no stem before .md");
    }
    if std::path::Path::new(name).is_absolute() {
        return Err("the name is an absolute path");
    }
    Ok(())
}

/// `ListPromptFiles` request (params may be omitted).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ListPromptFilesRequestV1 {}

/// One prompt in a listing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PromptFileEntryV1 {
    pub name: String,
    /// First line of the file without leading `#` and whitespace, at most
    /// [`FIRST_LINE_MAX_CHARS`] characters.
    pub first_line: String,
    pub size: u64,
    /// RFC3339 UTC with nanoseconds; absent when the platform cannot say.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mtime: Option<String>,
}

/// `ListPromptFiles` response: newest first, then by name. A missing
/// prompts directory is an empty listing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ListPromptFilesResponseV1 {
    /// Absolute prompts directory.
    pub dir: String,
    pub files: Vec<PromptFileEntryV1>,
    pub truncated: bool,
}

/// `GetPromptFile` request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GetPromptFileRequestV1 {
    pub name: String,
}

/// `GetPromptFile` response.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GetPromptFileResponseV1 {
    pub name: String,
    pub content: String,
    pub size: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mtime: Option<String>,
}

/// `PutPromptFile` request. `create_only` refuses an existing name
/// ([`CODE_EXISTS`]) instead of replacing it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PutPromptFileRequestV1 {
    pub name: String,
    pub content: String,
    #[serde(default)]
    pub create_only: bool,
}

/// `PutPromptFile` response. `created` is false when an existing prompt was
/// replaced.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PutPromptFileResponseV1 {
    pub name: String,
    pub size: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mtime: Option<String>,
    pub created: bool,
}

/// `DeletePromptFile` request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeletePromptFileRequestV1 {
    pub name: String,
}

/// `DeletePromptFile` response: where the prompt now is. Moving that file
/// back into the prompts directory restores it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeletePromptFileResponseV1 {
    pub name: String,
    pub trash_path: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_plain_md_name_is_accepted() {
        for name in ["a.md", "prompt-20261018-101500.md", "my notes.md", "x.y.md"] {
            assert_eq!(validate_prompt_name(name), Ok(()), "{name}");
        }
    }

    #[test]
    fn escaping_or_non_md_names_are_refused() {
        for name in [
            "",
            "..",
            "../a.md",
            "a..md",
            "/etc/a.md",
            "dir/a.md",
            "dir\\a.md",
            "C:\\a.md",
            "C:a.md",
            ".md",
            ".hidden.md",
            "a.txt",
            "a.md.txt",
            "a.MD",
            "a\n.md",
            " a.md",
        ] {
            assert!(validate_prompt_name(name).is_err(), "{name:?}");
        }
        assert!(validate_prompt_name(&format!("{}.md", "a".repeat(200))).is_err());
    }

    #[test]
    fn requests_refuse_unknown_fields() {
        let bad = serde_json::json!({"name": "a.md", "path": "/etc"});
        assert!(serde_json::from_value::<GetPromptFileRequestV1>(bad).is_err());
        let put: PutPromptFileRequestV1 =
            serde_json::from_value(serde_json::json!({"name": "a.md", "content": "x"})).unwrap();
        assert!(!put.create_only);
        assert!(
            serde_json::from_value::<ListPromptFilesRequestV1>(serde_json::json!({"x": 1}))
                .is_err()
        );
    }
}
