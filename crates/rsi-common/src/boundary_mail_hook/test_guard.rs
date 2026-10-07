//! Cooperative shell guard, not a shell interpreter: like the kill guard,
//! inspect command text without executing binaries or following aliases.
//! Copies of libtest binaries retain their rsid-/rsid_store- name prefix.

use crate::kill_guard::{command_word, simple_commands};

pub(super) const REFUSAL: &str = "rsi refused an unscoped test run (#1339): \
derive touched-module filters with `scripts/check-touched-shards --base origin/rolling`, \
then use its suggested filters or `scripts/run-rsid-test-shards.sh shard SHARD --filterset 'test(MODULE)'`. \
Supply a nonempty test-name filter for rsid/rsid-store library tests and copied test binaries. \
Only the QA lane may opt out with explicit `RSI_FULL_SUITE=1`.";

pub(super) fn refusal(command: &str, full_suite: bool) -> Option<&'static str> {
    unscoped(command, full_suite, 0).then_some(REFUSAL)
}

fn unscoped(script: &str, full_suite: bool, depth: usize) -> bool {
    if depth > 4 {
        return false;
    }
    shell_commands(script).iter().any(|words| {
        let Some((program, args)) = command_word(words) else {
            return false;
        };
        let mut opted_out = full_suite;
        // Only assignments before the executable affect it. A quoted mention
        // in echo, or an assignment on a different command, is not an opt-out.
        for word in &words[..words.len() - args.len() - 1] {
            if let Some(value) = word.strip_prefix("RSI_FULL_SUITE=") {
                opted_out = value == "1";
            } else if word == "RSI_FULL_SUITE" || word == "--unset=RSI_FULL_SUITE" {
                opted_out = false;
            }
        }
        // Redirection paths are shell operands, not test-name filters.
        let mut skip_path = false;
        let args: Vec<String> = args
            .iter()
            .filter(|arg| {
                if skip_path {
                    skip_path = false;
                    return false;
                }
                let redirect = arg.trim_start_matches(|c: char| c.is_ascii_digit());
                if redirect.starts_with('>') || redirect.starts_with('<') {
                    skip_path = redirect.chars().all(|c| c == '>' || c == '<');
                    return false;
                }
                true
            })
            .cloned()
            .collect();
        inspect(program, &args, opted_out, depth)
    })
}

/// The shell lexer is intentionally small and does not parse here-documents.
/// Remove their bodies before inspecting command words so code passed to
/// `python3 - <<'EOF'` (or a script written with `cat <<'EOF'`) is not treated
/// as a sequence of shell commands.
fn shell_commands(script: &str) -> Vec<Vec<String>> {
    let mut shell_input = String::with_capacity(script.len());
    let mut heredocs = Vec::<(String, bool)>::new();
    for line in script.lines() {
        if !heredocs.is_empty() {
            let (delimiter, strip_tabs) = &heredocs[0];
            let terminator = if *strip_tabs {
                line.trim_start_matches('\t')
            } else {
                line
            };
            if terminator == delimiter {
                heredocs.remove(0);
            }
            shell_input.push('\n');
            continue;
        }

        let words = simple_commands(line);
        for command in &words {
            let mut index = 0;
            while index < command.len() {
                let word = &command[index];
                if let Some(delimiter) = word.strip_prefix("<<-") {
                    if !delimiter.is_empty() {
                        heredocs.push((delimiter.to_owned(), true));
                    } else if let Some(delimiter) = command.get(index + 1) {
                        heredocs.push((delimiter.clone(), true));
                        index += 1;
                    }
                } else if let Some(delimiter) = word.strip_prefix("<<") {
                    if !delimiter.is_empty() {
                        heredocs.push((delimiter.to_owned(), false));
                    } else if let Some(delimiter) = command.get(index + 1) {
                        heredocs.push((delimiter.clone(), false));
                        index += 1;
                    }
                }
                index += 1;
            }
        }
        shell_input.push_str(line);
        shell_input.push('\n');
    }
    simple_commands(&shell_input)
}

fn inspect(program: &str, args: &[String], full_suite: bool, depth: usize) -> bool {
    if matches!(program, "sh" | "bash" | "dash" | "zsh" | "ksh" | "fish") {
        if let Some(index) = args
            .iter()
            .position(|arg| matches!(arg.as_str(), "-c" | "-lc"))
        {
            return args
                .get(index + 1)
                .is_some_and(|script| unscoped(script, full_suite, depth + 1));
        }
        // The shard wrapper is often invoked as `bash scripts/...`.
        if let Some(script) = args.first() {
            return inspect(
                script.rsplit('/').next().unwrap_or(script),
                &args[1..],
                full_suite,
                depth + 1,
            );
        }
    }
    if matches!(program, "cargo-slot" | "rsi-spill") {
        if args.first().is_some_and(|arg| arg == "--shell") {
            return args
                .get(1)
                .is_some_and(|script| unscoped(script, full_suite, depth + 1));
        }
        // Re-quote lexer words so filters containing spaces stay one operand.
        let args = if args.first().is_some_and(|arg| arg == "--") {
            &args[1..]
        } else {
            args
        };
        let command = args
            .iter()
            .map(|arg| format!("'{}'", arg.replace('\'', "'\\''")))
            .collect::<Vec<_>>()
            .join(" ");
        return unscoped(&command, full_suite, depth + 1);
    }
    if full_suite {
        return false;
    }
    if program == "run-rsid-test-shards.sh" {
        return matches!(args.first().map(String::as_str), Some("fast" | "full"))
            && !args.iter().any(|arg| arg == "--dry-run");
    }
    if program == "cargo" {
        return cargo_unscoped(args);
    }
    if program.starts_with("rsid-") || program.starts_with("rsid_store-") {
        return !listing_or_help(args) && !has_filter(args, false);
    }
    false
}

fn listing_or_help(args: &[String]) -> bool {
    args.iter()
        .any(|arg| matches!(arg.as_str(), "--list" | "--help" | "-h"))
}

fn cargo_unscoped(args: &[String]) -> bool {
    // Cargo's subcommand is the first positional argument after global flags
    // (and an optional `+toolchain` selector). Searching the whole argv lets
    // a path or an argument to another subcommand masquerade as `cargo test`.
    let mut command = 0;
    while let Some(arg) = args.get(command) {
        if matches!(arg.as_str(), "--color" | "--config" | "-Z") {
            command += 2;
        } else if arg.starts_with('-') || arg.starts_with('+') {
            command += 1;
        } else {
            break;
        }
    }
    if args.get(command).map(String::as_str) != Some("test") {
        return false;
    }
    let args = &args[command + 1..];
    let split = args
        .iter()
        .position(|arg| arg == "--")
        .unwrap_or(args.len());
    let cargo = &args[..split];
    let libtest = args.get(split + 1..).unwrap_or_default();
    if !cargo.iter().any(|arg| arg == "--lib")
        || cargo.iter().any(|arg| arg == "--no-run")
        || listing_or_help(cargo)
        || listing_or_help(libtest)
    {
        return false;
    }
    let protected_package = cargo.iter().enumerate().any(|(index, arg)| {
        let package = if matches!(arg.as_str(), "-p" | "--package") {
            cargo.get(index + 1).map(String::as_str)
        } else {
            arg.strip_prefix("--package=")
                .or_else(|| arg.strip_prefix("-p"))
        };
        matches!(package, Some("rsid" | "rsid-store"))
    });
    protected_package && !has_filter(cargo, true) && !has_filter(libtest, false)
}

/// Option values (threads, skip patterns, features, paths, etc.) are not
/// positive test-name filters. Accept both Cargo's positional TESTNAME and
/// libtest's FILTER after `--`.
fn has_filter(args: &[String], cargo: bool) -> bool {
    let mut index = 0;
    while let Some(arg) = args.get(index) {
        let takes_value = if cargo {
            matches!(
                arg.as_str(),
                "-p" | "--package"
                    | "--exclude"
                    | "--features"
                    | "-F"
                    | "--manifest-path"
                    | "--target"
                    | "--target-dir"
                    | "--profile"
                    | "-j"
                    | "--jobs"
                    | "--color"
                    | "--message-format"
                    | "--config"
                    | "--test"
                    | "--bin"
                    | "--bench"
                    | "--example"
                    | "-Z"
            )
        } else {
            matches!(
                arg.as_str(),
                "--test-threads"
                    | "--skip"
                    | "--format"
                    | "--color"
                    | "--logfile"
                    | "--shuffle-seed"
                    | "-Z"
            )
        };
        if takes_value {
            index += 2;
            continue;
        }
        if !arg.is_empty() && !arg.starts_with('-') {
            return true;
        }
        index += 1;
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};

    fn decision(command: &str, full_suite: bool) -> Option<Value> {
        let event = json!({"tool_name": "Bash", "tool_input": {"command": command}});
        super::super::render_shell_denial(&event, full_suite)
            .map(|output| serde_json::from_str(&output).unwrap())
    }

    #[test]
    fn unfiltered_run_is_denied_even_when_spill_is_disabled() {
        let event = json!({"hook_event_name": "PreToolUse", "tool_name": "Bash",
            "tool_input": {"command": "RSI_FULL_SUITE=0 cargo test -p rsid-store --lib"}});
        let tmp = tempfile::tempdir().unwrap();
        let cfg = crate::spill::SpillConfig {
            root: tmp.path().to_path_buf(),
            max_bytes: crate::spill::DEFAULT_MAX_BYTES,
            max_lines: crate::spill::DEFAULT_MAX_LINES,
            disabled: true,
        };
        let mut output = Vec::new();
        assert_eq!(
            super::super::run_hook_with_input(
                &event.to_string(),
                &cfg,
                "rsi-rpc",
                &mut output,
                |_, _, _, _| panic!("PreToolUse must not call the daemon"),
            ),
            0
        );
        let output: Value = serde_json::from_slice(&output).unwrap();
        assert_eq!(output["hookSpecificOutput"]["permissionDecision"], "deny");
        assert_eq!(
            output["hookSpecificOutput"]["permissionDecisionReason"],
            REFUSAL
        );
    }

    #[test]
    fn hook_refuses_unfiltered_library_runs_and_copies() {
        for command in [
            "cargo test -p rsid --lib",
            "cargo test -p rsid \\\n --lib",
            "cargo test -p rsid-store --lib -- --nocapture",
            "cargo test --package=rsid-store --lib -- --test-threads 1 --skip slow",
            "cargo test -prsid --features feature --lib '' -- --format json",
            "cargo +stable test --lib --package rsid -- --exact",
            "cargo test -p rsid --lib > /tmp/test.log",
            "env -u RSI_PROCESS_OWNERSHIP_NAMESPACE cargo test -p rsid --lib | tee log",
            "~/.rsi/bin/cargo-slot env -u RSI_PROCESS_OWNERSHIP_NAMESPACE cargo test -p rsid --lib",
            "bash -lc 'cargo test -p rsid-store --lib'",
            "scripts/rsi-spill --shell 'cargo test -p rsid-store --lib'",
            "scripts/rsi-spill -- cargo test -p rsid-store --lib -- --test-threads 1",
            "target/debug/deps/rsid-01234 --nocapture",
            "target/release/deps/rsid_store-abcdef --test-threads 4",
            "/tmp/rsid_store-copy",
            "/tmp/rsid-copy --skip slow",
            "scripts/run-rsid-test-shards.sh fast",
            "bash scripts/run-rsid-test-shards.sh full",
            "echo RSI_FULL_SUITE=1; cargo test -p rsid --lib",
            "RSI_FULL_SUITE=0 cargo test -p rsid --lib",
        ] {
            let output = decision(command, false).unwrap_or_else(|| panic!("allowed: {command}"));
            assert_eq!(
                output["hookSpecificOutput"]["permissionDecision"], "deny",
                "{command}"
            );
            assert_eq!(
                output["hookSpecificOutput"]["permissionDecisionReason"],
                REFUSAL
            );
        }
    }

    #[test]
    fn hook_allows_scoped_runs_integration_targets_and_inspection() {
        for command in [
            "cargo test -p rsid --lib -- store::tests",
            "cargo test -p rsid-store store::tests --lib",
            "cargo test -p rsid --test rpc_integration",
            "cargo test -p rsid --lib --no-run",
            "cargo test -p rsid --lib -- --list",
            "cargo test -p rsi-common --lib",
            "cargo run -p rsid --lib -- test",
            "cargo metadata --manifest-path /tmp/rsid-store-test/Cargo.toml",
            "target/debug/deps/rsid-01234 store::tests --test-threads 1",
            "/tmp/rsid_store-copy --list",
            "/tmp/rsid-copy store::tests",
            "scripts/run-rsid-test-shards.sh shard other-01",
            "scripts/run-rsid-test-shards.sh list",
            "scripts/run-rsid-test-shards.sh warmup",
            "scripts/run-rsid-test-shards.sh full --dry-run",
            "echo 'cargo test -p rsid --lib'",
        ] {
            assert!(decision(command, false).is_none(), "{command}");
        }
    }

    #[test]
    fn hook_ignores_test_like_text_in_heredocs() {
        for command in [
            "python3 - <<'EOF'\nfrom glob import glob\npaths = glob('/tmp/rsi-scoped-test-*/rsid-store.json')\n# Example command text: cargo test -p rsid-store --lib\nEOF",
            "cat > /tmp/inspect.py <<'EOF'\nprint('cargo test -p rsid --lib')\nEOF",
            "cat > /tmp/inspect.py <<'EOF'\nprint('rsid-store test output')\nEOF\npython3 /tmp/inspect.py",
        ] {
            assert!(decision(command, false).is_none(), "{command}");
        }
    }

    #[test]
    fn qa_opt_out_is_explicit_and_keeps_the_kill_guard() {
        for command in [
            "cargo test -p rsid --lib",
            "/tmp/rsid_store-copy",
            "scripts/run-rsid-test-shards.sh full",
        ] {
            assert!(decision(command, true).is_none(), "{command}");
            assert!(decision(&format!("RSI_FULL_SUITE=1 {command}"), false).is_none());
            assert!(decision(&format!("env RSI_FULL_SUITE=1 {command}"), false).is_none());
        }
        assert!(decision("env -u RSI_FULL_SUITE cargo test -p rsid --lib", true).is_some());
        assert!(decision("RSI_FULL_SUITE=1 pkill cargo", true).is_some());
    }
}
