//! #1331: tool activity after a worker's `PIPELINE HANDOFF`.
//!
//! A worker that writes a valid handoff and then runs one more tool call
//! used to settle `Failed` (`terminal_handoff_superseded_by_tool`) even when
//! that call was a `git status` and its commit was complete. The handoff
//! still stands when every later call is read-only, or when the sandbox is
//! clean at the commit the handoff names. Anything else (a call that may have
//! changed the tree or the commit, with no clean-tree proof) keeps the
//! superseded-handoff failure.

use std::path::Path;
use std::sync::Arc;

use rsi_common::types::{ConversationEvent, EventType, Role};
use uuid::Uuid;

/// What followed the latest worker handoff of the current turn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PostHandoffTools {
    /// No handoff, or no tool activity after it.
    None,
    /// Every later tool call is read-only.
    ReadOnly,
    /// A later tool call may have changed the tree or the commit.
    Mutating,
}

fn handoff_first_line(content: &str) -> Option<&str> {
    let first_line = content.lines().find(|line| !line.trim().is_empty())?;
    first_line
        .trim_start_matches(|ch: char| ch == '#' || ch.is_whitespace())
        .starts_with("PIPELINE HANDOFF — ")
        .then_some(first_line)
}

/// Classify the tool activity after the current turn's latest handoff. A
/// malformed handoff only counts when no earlier handoff was seen; only a
/// contract-valid one can correct earlier post-handoff activity.
pub(crate) fn classify(events: &[ConversationEvent]) -> PostHandoffTools {
    classify_with_handoff(events).0
}

/// [`classify`] plus the index of the handoff message it measured from.
pub(crate) fn classify_with_handoff(events: &[ConversationEvent]) -> (PostHandoffTools, usize) {
    let mut handoff: Option<usize> = None;
    let mut state = PostHandoffTools::None;
    for (index, event) in events.iter().enumerate() {
        if event.event_type == EventType::Message && event.role == Some(Role::User) {
            handoff = None;
            state = PostHandoffTools::None;
        } else if event.event_type == EventType::Message && event.role == Some(Role::Assistant) {
            if let Some(first_line) = handoff_first_line(&event.content)
                && (handoff.is_none() || rsi_common::validate_first_line(first_line).is_ok())
            {
                handoff = Some(index);
                state = PostHandoffTools::None;
            }
        } else if let Some(start) = handoff
            && matches!(event.event_type, EventType::ToolUse | EventType::ToolResult)
            && state != PostHandoffTools::Mutating
        {
            let read_only = match event.event_type {
                EventType::ToolUse => tool_call_is_read_only(event),
                // A result is covered by its call when that call came after
                // the handoff; otherwise classify the call it answers.
                _ => result_call_is_read_only(events, start, event),
            };
            state = if read_only {
                PostHandoffTools::ReadOnly
            } else {
                PostHandoffTools::Mutating
            };
        }
    }
    (state, handoff.unwrap_or(0))
}

fn result_call_is_read_only(
    events: &[ConversationEvent],
    handoff: usize,
    result: &ConversationEvent,
) -> bool {
    let Some(id) = result.tool_use_id.as_deref() else {
        return false;
    };
    events
        .iter()
        .enumerate()
        .rev()
        .find(|(_, event)| {
            event.event_type == EventType::ToolUse && event.tool_use_id.as_deref() == Some(id)
        })
        .is_some_and(|(index, call)| index > handoff || tool_call_is_read_only(call))
}

/// Read-only Claude and rsi tools (by name; MCP names normalized).
const READ_ONLY_TOOLS: &[&str] = &[
    "Read",
    "Grep",
    "Glob",
    "LS",
    "NotebookRead",
    "ToolSearch",
    "WebFetch",
    "WebSearch",
];

const READ_ONLY_RSI_CONTROLS: &[&str] = &[
    "authority_catalog",
    "get_issue",
    "list_issues",
    "list_issue_events",
    "status",
    "read_session_events",
    "manager_inspect",
    "manager_overview",
    "manager_work_view",
    "manager_get_action",
    "global_overview",
    "query_failure_signatures",
    "topology_list",
    "topology_get_execution",
];

/// True when one recorded tool call cannot change the tree or the commit.
/// Unknown tools, and shell commands outside a small read-only grammar, are
/// treated as mutating.
pub(crate) fn tool_call_is_read_only(call: &ConversationEvent) -> bool {
    let Some(name) = call.tool_name.as_deref() else {
        return false;
    };
    if READ_ONLY_TOOLS.contains(&name) {
        return true;
    }
    if let Some(control) = name.split_once("rsi_control_").map(|(_, verb)| verb) {
        return READ_ONLY_RSI_CONTROLS.contains(&control);
    }
    // Recorded as a JSON object; tolerate one encoded as a JSON string.
    let input = match call.tool_input.as_deref() {
        Some(serde_json::Value::String(raw)) => {
            serde_json::from_str(raw).unwrap_or(serde_json::Value::Null)
        }
        Some(value) => value.clone(),
        None => serde_json::Value::Null,
    };
    match name {
        "Bash" | "bash" | "shell" | "exec_command" | "local_shell" => {
            let command = input
                .get("command")
                .or_else(|| input.get("cmd"))
                .map(|command| match command {
                    serde_json::Value::String(text) => Some(text.clone()),
                    serde_json::Value::Array(argv) => argv
                        .iter()
                        .map(|arg| arg.as_str().map(shell_quote))
                        .collect::<Option<Vec<_>>>()
                        .map(|argv| argv.join(" ")),
                    _ => None,
                });
            command
                .flatten()
                .is_some_and(|command| shell_command_is_read_only(&command))
        }
        _ => false,
    }
}

fn shell_quote(arg: &str) -> String {
    if !arg.is_empty()
        && arg
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "-_./=:@%+,".contains(c))
    {
        arg.to_string()
    } else {
        format!("'{}'", arg.replace('\'', "'\\''"))
    }
}

/// Split `text` into words, honouring single quotes, double quotes and
/// backslashes. `None` for an unterminated quote.
fn shell_words(text: &str) -> Option<Vec<String>> {
    let mut words = Vec::new();
    let mut word = String::new();
    let mut in_word = false;
    let mut chars = text.chars();
    while let Some(c) = chars.next() {
        match c {
            '\'' => {
                in_word = true;
                loop {
                    match chars.next()? {
                        '\'' => break,
                        other => word.push(other),
                    }
                }
            }
            '"' => {
                in_word = true;
                loop {
                    match chars.next()? {
                        '"' => break,
                        '\\' => {
                            let escaped = chars.next()?;
                            if !matches!(escaped, '"' | '\\' | '$' | '`' | '\n') {
                                word.push('\\');
                            }
                            word.push(escaped);
                        }
                        other => word.push(other),
                    }
                }
            }
            '\\' => {
                in_word = true;
                word.push(chars.next()?);
            }
            c if c.is_whitespace() => {
                if in_word {
                    words.push(std::mem::take(&mut word));
                    in_word = false;
                }
            }
            other => {
                in_word = true;
                word.push(other);
            }
        }
    }
    if in_word {
        words.push(word);
    }
    Some(words)
}

fn basename(word: &str) -> &str {
    word.rsplit('/').next().unwrap_or(word)
}

/// A conservative read-only shell grammar: segments joined by `;`, `&&`,
/// `||`, `|` or newlines, each an allowed read-only command; no command
/// substitution, process substitution, backgrounding or output redirection
/// (other than to `/dev/null` or `2>&1`). A `bash -c`/`-lc` wrapper is
/// unwrapped once.
pub(crate) fn shell_command_is_read_only(command: &str) -> bool {
    if let Some(words) = shell_words(command)
        && words.len() == 3
        && matches!(basename(&words[0]), "bash" | "sh" | "zsh" | "dash")
        && matches!(words[1].as_str(), "-c" | "-lc" | "-cl")
    {
        return script_is_read_only(&words[2]);
    }
    script_is_read_only(command)
}

fn script_is_read_only(script: &str) -> bool {
    if script.contains('`') || script.contains("$(") || script.contains("<(") {
        return false;
    }
    let mut cleaned = script.to_string();
    for allowed in ["2>&1", "&>/dev/null", "2>/dev/null", ">/dev/null"] {
        cleaned = cleaned.replace(allowed, " ");
    }
    if cleaned.contains('>') {
        return false;
    }
    let cleaned = cleaned
        .replace("&&", "\n")
        .replace("||", "\n")
        .replace([';', '|'], "\n");
    if cleaned.contains('&') {
        return false;
    }
    let mut any = false;
    for segment in cleaned.lines() {
        let Some(words) = shell_words(segment) else {
            return false;
        };
        let words: Vec<&str> = words
            .iter()
            .map(String::as_str)
            .skip_while(|word| {
                word.split_once('=').is_some_and(|(name, _)| {
                    !name.is_empty() && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
                })
            })
            .collect();
        let Some((program, args)) = words.split_first() else {
            continue;
        };
        if !segment_is_read_only(basename(program), args) {
            return false;
        }
        any = true;
    }
    any
}

fn segment_is_read_only(program: &str, args: &[&str]) -> bool {
    match program {
        "git" => git_is_read_only(args),
        // Only the print form `sed -n 'A,Bp' FILE...`.
        "sed" => {
            args.contains(&"-n")
                && args
                    .iter()
                    .find(|arg| !arg.starts_with('-'))
                    .is_some_and(|script| {
                        script.strip_suffix('p').is_some_and(|range| {
                            range
                                .chars()
                                .all(|c| c.is_ascii_digit() || c == ',' || c == '$')
                        })
                    })
        }
        "find" => !args.iter().any(|arg| {
            matches!(
                *arg,
                "-delete"
                    | "-exec"
                    | "-execdir"
                    | "-ok"
                    | "-okdir"
                    | "-fprint"
                    | "-fprint0"
                    | "-fprintf"
                    | "-fls"
            )
        }),
        "sort" => !args
            .iter()
            .any(|arg| arg.starts_with("-o") || arg.starts_with("--output")),
        "cat" | "head" | "tail" | "wc" | "ls" | "pwd" | "echo" | "printf" | "rg" | "grep"
        | "egrep" | "fgrep" | "diff" | "cmp" | "stat" | "file" | "true" | "test" | "[" | "date"
        | "which" | "uniq" | "cut" | "tr" | "jq" | "sha256sum" | "sha1sum" | "md5sum" | "du"
        | "df" | "readlink" | "realpath" | "basename" | "dirname" | "tree" | "cd" | "set"
        | "nl" | "column" => true,
        _ => false,
    }
}

fn git_is_read_only(args: &[&str]) -> bool {
    let mut rest = args;
    // Global options before the subcommand.
    loop {
        match rest.first().copied() {
            Some("-C" | "-c") => rest = rest.get(2..).unwrap_or(&[]),
            Some(option)
                if option == "--no-pager"
                    || option.starts_with("--git-dir=")
                    || option.starts_with("--work-tree=") =>
            {
                rest = &rest[1..]
            }
            _ => break,
        }
    }
    let Some((subcommand, args)) = rest.split_first() else {
        return false;
    };
    if args.iter().any(|arg| arg.starts_with("--output")) {
        return false;
    }
    match *subcommand {
        "status" | "diff" | "log" | "show" | "rev-parse" | "merge-base" | "ls-files"
        | "ls-tree" | "cat-file" | "describe" | "blame" | "shortlog" | "grep" | "rev-list"
        | "name-rev" | "for-each-ref" | "show-ref" | "whatchanged" => true,
        "branch" => args.iter().all(|arg| {
            matches!(
                *arg,
                "--show-current" | "--list" | "-a" | "-r" | "-v" | "-vv" | "--all" | "--remotes"
            )
        }),
        "remote" => args.iter().all(|arg| matches!(*arg, "-v" | "--verbose")),
        "stash" => matches!(args.first().copied(), Some("list" | "show")),
        "reflog" => matches!(args.first().copied(), None | Some("show")),
        // Fetch moves remote-tracking refs only; a `src:dst` refspec could
        // move a local branch.
        "fetch" => !args.iter().any(|arg| arg.contains(':')),
        _ => false,
    }
}

/// Hex tokens (7 to 40 chars, at least one digit) the handoff names: on its
/// first line, or on a line that mentions a commit, SHA or RESULT.
fn recorded_shas(handoff: &str) -> Vec<String> {
    let mut shas = Vec::new();
    for (index, line) in handoff.lines().enumerate() {
        let lower = line.to_ascii_lowercase();
        if index > 0
            && !(lower.contains("commit") || lower.contains("sha") || lower.contains("result"))
        {
            continue;
        }
        for token in line.split(|c: char| !c.is_ascii_hexdigit()) {
            if (7..=40).contains(&token.len()) && token.chars().any(|c| c.is_ascii_digit()) {
                shas.push(token.to_ascii_lowercase());
            }
        }
    }
    shas
}

/// The sandbox at `root` is clean and its HEAD is a commit the handoff names.
pub(crate) fn tree_clean_at_handoff(root: &Path, handoff: &str) -> bool {
    let shas = recorded_shas(handoff);
    if shas.is_empty() {
        return false;
    }
    match crate::sandbox::git_worktree::observe_clean_head_bounded(root) {
        Ok((true, head)) => {
            let head = head.trim().to_ascii_lowercase();
            shas.iter().any(|sha| head.starts_with(sha.as_str()))
        }
        _ => false,
    }
}

type ActiveMap =
    Arc<tokio::sync::RwLock<std::collections::HashMap<Uuid, super::types::TrackedSession>>>;
type BoxFut<'a, T> = std::pin::Pin<Box<dyn std::future::Future<Output = T> + Send + 'a>>;

/// For a turn whose post-handoff activity is [`PostHandoffTools::Mutating`],
/// prove the handoff still stands: the sandbox is clean at the commit the
/// handoff names. Boxed so the monitor future stays small.
#[inline(never)]
pub(crate) fn tree_unchanged_proof_boxed(active: &ActiveMap, session_id: Uuid) -> BoxFut<'_, bool> {
    Box::pin(async move {
        let probe = {
            let guard = active.read().await;
            let Some(tracked) = guard.get(&session_id) else {
                return false;
            };
            let (state, handoff) = classify_with_handoff(&tracked.events);
            if state != PostHandoffTools::Mutating {
                return false;
            }
            let Some(root) = tracked.session.sandbox_root.clone() else {
                return false;
            };
            let Some(text) = tracked
                .events
                .get(handoff)
                .map(|event| event.content.clone())
            else {
                return false;
            };
            (root, text)
        };
        let (root, text) = probe;
        tokio::task::spawn_blocking(move || tree_clean_at_handoff(Path::new(&root), &text))
            .await
            .unwrap_or(false)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event(
        event_type: EventType,
        role: Option<Role>,
        content: &str,
        tool: Option<(&str, serde_json::Value, &str)>,
    ) -> ConversationEvent {
        ConversationEvent {
            id: 0,
            session_id: Uuid::nil(),
            sequence: 0,
            event_type,
            role,
            content: content.to_string(),
            tool_name: tool.as_ref().map(|(name, _, _)| name.to_string()),
            tool_input: tool.as_ref().map(|(_, input, _)| input.clone().into()),
            created_at: chrono::Utc::now(),
            offload_id: None,
            tool_use_id: tool.as_ref().map(|(_, _, id)| id.to_string()),
            metadata: None,
        }
    }

    fn handoff(text: &str) -> ConversationEvent {
        event(EventType::Message, Some(Role::Assistant), text, None)
    }

    fn call(name: &str, input: serde_json::Value, id: &str) -> Vec<ConversationEvent> {
        vec![
            event(EventType::ToolUse, None, "", Some((name, input, id))),
            event(
                EventType::ToolResult,
                None,
                "ok",
                Some((name, serde_json::Value::Null, id)),
            ),
        ]
    }

    fn after_handoff(calls: Vec<Vec<ConversationEvent>>) -> Vec<ConversationEvent> {
        let mut events = vec![handoff(
            "PIPELINE HANDOFF — IMPLEMENTATION:\nRESULT commit=abc1234 status=green",
        )];
        events.extend(calls.into_iter().flatten());
        events
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
    #[test]
    fn read_only_calls_after_a_handoff_leave_it_standing() {
        let events = after_handoff(vec![
            call(
                "Bash",
                serde_json::json!({"command": "git status --short && git diff origin/rolling --stat"}),
                "a",
            ),
            call(
                "shell",
                serde_json::json!({"command": "/usr/bin/bash -lc 'git status --short; git rev-parse HEAD; git log -3 --oneline 2>&1 | head -5'"}),
                "b",
            ),
            call("Read", serde_json::json!({"file_path": "/tmp/x"}), "c"),
            call(
                "mcp__rsi-agent__rsi_control_get_issue",
                serde_json::json!({}),
                "d",
            ),
            call(
                "exec_command",
                serde_json::json!({"cmd": ["git", "diff", "--stat"]}),
                "e",
            ),
        ]);
        assert_eq!(classify(&events), PostHandoffTools::ReadOnly);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
    #[test]
    fn mutating_or_unknown_calls_after_a_handoff_supersede_it() {
        for (name, input) in [
            (
                "Bash",
                serde_json::json!({"command": "git commit -am fixup"}),
            ),
            (
                "Bash",
                serde_json::json!({"command": "git status > status.txt"}),
            ),
            (
                "Bash",
                serde_json::json!({"command": "git status; rm -rf target"}),
            ),
            (
                "Bash",
                serde_json::json!({"command": "git diff $(touch x)"}),
            ),
            ("Bash", serde_json::json!({"command": "sed -i s/a/b/ f.rs"})),
            (
                "Bash",
                serde_json::json!({"command": "git branch new-branch"}),
            ),
            (
                "Bash",
                serde_json::json!({"command": "git fetch origin rolling:rolling"}),
            ),
            ("Bash", serde_json::json!({"command": "find . -delete"})),
            (
                "shell",
                serde_json::json!({"command": "/usr/bin/bash -lc 'git status; git add -A'"}),
            ),
            ("Edit", serde_json::json!({"file_path": "f.rs"})),
            (
                "exec",
                serde_json::json!({"code": "tools.exec_command({cmd:'git status'})"}),
            ),
            (
                "mcp__rsi-agent__rsi_control_update_issue",
                serde_json::json!({}),
            ),
        ] {
            let events = after_handoff(vec![
                call("Bash", serde_json::json!({"command": "git status"}), "r"),
                call(name, input.clone(), "m"),
            ]);
            assert_eq!(
                classify(&events),
                PostHandoffTools::Mutating,
                "{name} {input}"
            );
        }
        // A call without a recorded name is unknown, hence mutating.
        let mut events = after_handoff(Vec::new());
        events.push(event(EventType::ToolUse, None, "", None));
        assert_eq!(classify(&events), PostHandoffTools::Mutating);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
    #[test]
    fn a_new_user_turn_resets_and_no_tools_is_none() {
        let mut events = after_handoff(Vec::new());
        assert_eq!(classify(&events), PostHandoffTools::None);
        events.extend(call("Edit", serde_json::json!({}), "m"));
        events.push(event(EventType::Message, Some(Role::User), "next", None));
        events.extend(call("Edit", serde_json::json!({}), "n"));
        assert_eq!(classify(&events), PostHandoffTools::None);
    }

    fn git(dir: &Path, args: &[&str]) -> String {
        let output = std::process::Command::new("git")
            .arg("-C")
            .arg(dir)
            .args([
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@t",
                "-c",
                "commit.gpgsign=false",
            ])
            .args(args)
            .output()
            .expect("git runs");
        assert!(output.status.success(), "git {args:?}: {output:?}");
        String::from_utf8_lossy(&output.stdout).trim().to_string()
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
    #[test]
    fn clean_tree_at_the_recorded_commit_is_proof() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        git(root, &["init", "-q"]);
        std::fs::write(root.join("a.txt"), "a\n").unwrap();
        git(root, &["add", "a.txt"]);
        git(root, &["commit", "-q", "-m", "a"]);
        let head = git(root, &["rev-parse", "HEAD"]);

        let first_line = format!("PIPELINE HANDOFF — {head}\n\nFixed it.");
        assert!(tree_clean_at_handoff(root, &first_line));
        let short = format!("PIPELINE HANDOFF — IMPLEMENTATION:\ncommit: {}", &head[..9]);
        assert!(tree_clean_at_handoff(root, &short));
        assert!(!tree_clean_at_handoff(
            root,
            "PIPELINE HANDOFF — IMPLEMENTATION:\ncommit: 0123456789abcdef"
        ));
        assert!(!tree_clean_at_handoff(
            root,
            "PIPELINE HANDOFF — IMPLEMENTATION:\nno sha"
        ));

        // A later call left the tree dirty: no proof.
        std::fs::write(root.join("b.txt"), "b\n").unwrap();
        assert!(!tree_clean_at_handoff(root, &first_line));
    }
}
