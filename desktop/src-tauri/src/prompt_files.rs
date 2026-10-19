//! The prompt creator's files (hub #3040; desktop rows, local #88): the
//! daemon's `<data dir>/prompts/*.md` store, reached only through its four
//! operator methods (`ListPromptFiles`, `GetPromptFile`, `PutPromptFile`,
//! `DeletePromptFile`; wire types `rsi_common::prompt_files`). The desktop
//! never touches `~/.rsi/prompts` itself. Names are checked here with the
//! daemon's rules first, so a bad name never leaves the webview; the daemon
//! checks them again and owns containment, atomic writes and the trash.

use serde_json::{Value, json};
use tauri::State;

use crate::admin::{call, pick};
use crate::{AppState, CmdResult};

/// `MAX_PROMPT_NAME_BYTES` in `rsi_common::prompt_files`.
pub const MAX_PROMPT_NAME_BYTES: usize = 200;
/// `MAX_PROMPT_FILE_BYTES` (1 MiB).
pub const MAX_PROMPT_FILE_BYTES: usize = 1024 * 1024;
const ENTRY_KEYS: &[&str] = &["name", "first_line", "size", "mtime"];

/// One flat `*.md` file name, as the daemon's `validate_prompt_name` takes it.
pub fn check_prompt_name(name: &str) -> Result<(), String> {
    if name.is_empty() {
        return Err("the prompt name is empty".into());
    }
    if name.len() > MAX_PROMPT_NAME_BYTES {
        return Err(format!(
            "the prompt name is longer than {MAX_PROMPT_NAME_BYTES} bytes"
        ));
    }
    if name.trim() != name {
        return Err("the prompt name has surrounding whitespace".into());
    }
    if name.starts_with('.') {
        return Err("the prompt name starts with a dot".into());
    }
    if name.contains(['/', '\\', ':']) || name.contains("..") {
        return Err("the prompt name must be one flat file name".into());
    }
    if name.chars().any(char::is_control) {
        return Err("the prompt name contains a control character".into());
    }
    let stem = name.strip_suffix(".md").unwrap_or_default();
    if stem.is_empty() {
        return Err("the prompt name must end in .md".into());
    }
    Ok(())
}

pub fn check_prompt_content(content: &str) -> Result<(), String> {
    if content.len() > MAX_PROMPT_FILE_BYTES {
        return Err(format!(
            "the prompt is larger than {MAX_PROMPT_FILE_BYTES} bytes"
        ));
    }
    Ok(())
}

pub fn put_prompt_params(name: &str, content: &str, create_only: bool) -> Result<Value, String> {
    check_prompt_name(name)?;
    check_prompt_content(content)?;
    Ok(json!({ "name": name, "content": content, "create_only": create_only }))
}

pub fn name_params(name: &str) -> Result<Value, String> {
    check_prompt_name(name)?;
    Ok(json!({ "name": name }))
}

pub fn prompt_list(list: &Value) -> Value {
    let files = list
        .get("files")
        .and_then(Value::as_array)
        .map_or_else(Vec::new, |a| {
            a.iter().map(|f| pick(f, ENTRY_KEYS)).collect()
        });
    let mut out = pick(list, &["dir", "truncated"]);
    out["files"] = Value::Array(files);
    out
}

#[tauri::command]
pub async fn list_prompt_files(state: State<'_, AppState>) -> CmdResult {
    let list = call(&state.client, "ListPromptFiles", json!({})).await?;
    Ok(prompt_list(&list))
}

#[tauri::command]
pub async fn get_prompt_file(state: State<'_, AppState>, name: String) -> CmdResult {
    let file = call(&state.client, "GetPromptFile", name_params(&name)?).await?;
    Ok(pick(&file, &["name", "content", "size", "mtime"]))
}

/// Save (or, with `create_only`, create without clobbering) one prompt.
#[tauri::command]
pub async fn put_prompt_file(
    state: State<'_, AppState>,
    name: String,
    content: String,
    create_only: bool,
) -> CmdResult {
    let params = put_prompt_params(&name, &content, create_only)?;
    let out = call(&state.client, "PutPromptFile", params).await?;
    Ok(pick(&out, &["name", "size", "mtime", "created"]))
}

/// Move one prompt to the daemon's recoverable trash; the answer names it.
#[tauri::command]
pub async fn delete_prompt_file(state: State<'_, AppState>, name: String) -> CmdResult {
    let out = call(&state.client, "DeletePromptFile", name_params(&name)?).await?;
    Ok(pick(&out, &["name", "trash_path"]))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_plain_md_name_passes_and_escapes_are_refused() {
        for ok in [
            "plan.md",
            "prompt-20261018-101500.md",
            "a b.md",
            "notes.v2.md",
        ] {
            assert!(check_prompt_name(ok).is_ok(), "{ok}");
        }
        for bad in [
            "",
            ".md",
            ".hidden.md",
            "../x.md",
            "a..b.md",
            "dir/x.md",
            "dir\\x.md",
            "/abs.md",
            "C:x.md",
            " x.md",
            "x.md ",
            "x.txt",
            "x",
            "x\u{7}.md",
        ] {
            assert!(check_prompt_name(bad).is_err(), "{bad:?}");
        }
        assert!(check_prompt_name(&format!("{}.md", "a".repeat(MAX_PROMPT_NAME_BYTES))).is_err());
    }

    #[test]
    fn put_params_bound_the_content_and_keep_create_only() {
        assert_eq!(
            put_prompt_params("plan.md", "# Plan\n", true).unwrap(),
            json!({ "name": "plan.md", "content": "# Plan\n", "create_only": true })
        );
        assert!(
            put_prompt_params("plan.md", &"x".repeat(MAX_PROMPT_FILE_BYTES + 1), false).is_err()
        );
        assert!(put_prompt_params("../plan.md", "", false).is_err());
        assert_eq!(
            name_params("plan.md").unwrap(),
            json!({ "name": "plan.md" })
        );
    }

    #[test]
    fn the_listing_is_allowlisted() {
        let raw = json!({ "dir": "/home/op/.rsi/prompts", "truncated": false, "extra": 1,
            "files": [{ "name": "plan.md", "first_line": "Plan", "size": 7, "mtime": "2026-10-18T10:15:00Z", "content": "full" }] });
        assert_eq!(
            prompt_list(&raw),
            json!({ "dir": "/home/op/.rsi/prompts", "truncated": false,
                "files": [{ "name": "plan.md", "first_line": "Plan", "size": 7, "mtime": "2026-10-18T10:15:00Z" }] })
        );
        assert_eq!(prompt_list(&Value::Null), json!({ "files": [] }));
    }
}
