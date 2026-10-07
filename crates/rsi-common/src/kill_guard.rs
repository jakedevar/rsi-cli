//! #1227: refuse agent shell commands that signal processes by name or
//! pattern.
//!
//! On 2026-10-05 an agent ran `pkill -f "run-rsid-test-shards.sh shard"` to
//! stop its own test run. The pattern also matched another worker's detached
//! `systemd-run` test unit and the manager's lander shard, and SIGTERM killed
//! all three in the same second. `pkill`, `killall`, `kill -1` and a `kill`
//! fed by `pgrep` reach every process this user owns. Every agent shares that
//! user, so these commands are refused before they run: the Claude
//! `PreToolUse` hook and the Harness shell tools both call
//! [`broad_kill_refusal`].
//!
//! It is a guard rail for cooperative agents, not a security boundary: it
//! reads the command text with a small shell lexer and does not follow
//! scripts, aliases or variables.

/// What a refused command is told.
pub const BROAD_KILL_REFUSAL: &str = "rsi refused this command (#1227): \
`pkill`, `killall`, `kill -1` and a `kill` fed by `pgrep` signal every process \
whose name matches, and every agent, test unit and lander runs as the same user, \
so the command can kill other agents' work. Signal only what you started: kill \
the exact PID you launched (`$!`, or one PID read from `pgrep -af` and checked), \
or stop your own unit with `systemctl --user stop <your-unit>`.";

/// Shell programs whose `-c` script is checked too.
const SHELLS: &[&str] = &["sh", "bash", "dash", "zsh", "ksh", "fish"];
/// Programs that run the rest of the line as a command.
const PASS_THROUGH: &[&str] = &[
    "sudo", "doas", "env", "command", "exec", "nohup", "setsid", "time", "nice", "ionice",
    "timeout", "xargs", "chrt", "taskset", "stdbuf", "builtin", "!",
];
/// Programs that signal processes chosen by name or pattern.
const PATTERN_KILLERS: &[&str] = &["pkill", "killall", "killall5"];

/// `Some(reason)` when `command` signals processes by name or pattern, or
/// every process (`kill -1`); `None` otherwise.
#[must_use]
pub fn broad_kill_refusal(command: &str) -> Option<&'static str> {
    is_broad_kill(command, 0).then_some(BROAD_KILL_REFUSAL)
}

fn is_broad_kill(script: &str, depth: usize) -> bool {
    if depth > 4 {
        return false;
    }
    let commands = simple_commands(script);
    let mut pgrep = false;
    let mut kill = false;
    for words in &commands {
        let Some((program, args)) = command_word(words) else {
            continue;
        };
        if PATTERN_KILLERS.contains(&program) {
            return true;
        }
        if SHELLS.contains(&program)
            && let Some(position) = args.iter().position(|arg| arg == "-c")
            && let Some(inner) = args.get(position + 1)
            && is_broad_kill(inner, depth + 1)
        {
            return true;
        }
        match program {
            "pgrep" | "pidof" => pgrep = true,
            "kill" => {
                if kills_every_process(args) {
                    return true;
                }
                kill = true;
            }
            _ => {}
        }
    }
    // `kill $(pgrep -f X)` and `pgrep -f X | xargs kill` are `pkill -f X`.
    pgrep && kill
}

/// The program (basename) and its arguments after leading `NAME=value`
/// assignments and pass-through wrappers.
pub(crate) fn command_word(words: &[String]) -> Option<(&str, &[String])> {
    let mut index = 0;
    while index < words.len() {
        let word = words[index].as_str();
        let base = word.rsplit('/').next().unwrap_or(word);
        if is_assignment(word) {
            index += 1;
            continue;
        }
        if word.starts_with('-') && index > 0 {
            // `env -u NAME cmd` must reach cmd, not mistake NAME for it.
            if matches!(word, "-u" | "--unset" | "-C" | "--chdir") {
                index += 2;
                continue;
            }
            // A wrapper option; a numeric value (`nice -n 5`) is its argument,
            // never a program.
            index += 1;
            if words
                .get(index)
                .is_some_and(|next| next.chars().all(|c| c.is_ascii_digit()))
            {
                index += 1;
            }
            continue;
        }
        if PASS_THROUGH.contains(&base) {
            index += 1;
            // `timeout 5 cmd`, `nice -n 5 cmd`: skip a duration or a niceness.
            if matches!(base, "timeout" | "nice" | "chrt" | "taskset")
                && words
                    .get(index)
                    .is_some_and(|next| next.starts_with(|c: char| c.is_ascii_digit()))
            {
                index += 1;
            }
            continue;
        }
        return Some((base, &words[index + 1..]));
    }
    None
}

fn is_assignment(word: &str) -> bool {
    word.split_once('=').is_some_and(|(name, _)| {
        !name.is_empty()
            && name.chars().all(|c| c == '_' || c.is_ascii_alphanumeric())
            && !name.starts_with(|c: char| c.is_ascii_digit())
    })
}

/// `kill [-SIG | -s SIG | -n NUM] [--] -1` targets every process.
fn kills_every_process(args: &[String]) -> bool {
    let mut operands = false;
    let mut index = 0;
    while index < args.len() {
        let arg = args[index].as_str();
        if operands {
            if arg == "-1" {
                return true;
            }
        } else if arg == "--" {
            operands = true;
        } else if matches!(arg, "-s" | "-n" | "--signal") {
            index += 1;
        } else if index == 0 && arg.starts_with('-') {
            // The signal spec (`-9`, `-TERM`, `-1` as SIGHUP).
        } else if arg.starts_with('-') && arg != "-1" {
            // Another option, such as `-l`.
        } else {
            operands = true;
            if arg == "-1" {
                return true;
            }
        }
        index += 1;
    }
    false
}

/// Split `script` into simple commands (word lists) at unquoted `;`, `&`,
/// `|`, newlines, parentheses, braces, backticks and `$(`. Quotes are removed
/// from words; separators inside quotes do not split.
pub(crate) fn simple_commands(script: &str) -> Vec<Vec<String>> {
    let mut commands = Vec::new();
    let mut words: Vec<String> = Vec::new();
    let mut word = String::new();
    let mut in_word = false;
    let mut chars = script.chars().peekable();
    let finish_word = |words: &mut Vec<String>, word: &mut String, in_word: &mut bool| {
        if *in_word {
            words.push(std::mem::take(word));
            *in_word = false;
        }
    };
    while let Some(c) = chars.next() {
        match c {
            '\'' => {
                in_word = true;
                for inner in chars.by_ref() {
                    if inner == '\'' {
                        break;
                    }
                    word.push(inner);
                }
            }
            '"' => {
                in_word = true;
                while let Some(inner) = chars.next() {
                    match inner {
                        '"' => break,
                        '\\' => {
                            if let Some(escaped) = chars.next() {
                                if escaped != '\n' {
                                    word.push(escaped);
                                }
                            }
                        }
                        _ => word.push(inner),
                    }
                }
            }
            '\\' => {
                in_word = true;
                if let Some(escaped) = chars.next() {
                    if escaped != '\n' {
                        word.push(escaped);
                    }
                }
            }
            '$' if chars.peek() == Some(&'(') => {
                chars.next();
                finish_word(&mut words, &mut word, &mut in_word);
                commands.push(std::mem::take(&mut words));
            }
            ';' | '&' | '|' | '\n' | '(' | ')' | '{' | '}' | '`' => {
                finish_word(&mut words, &mut word, &mut in_word);
                commands.push(std::mem::take(&mut words));
            }
            c if c.is_whitespace() => finish_word(&mut words, &mut word, &mut in_word),
            '#' if !in_word => {
                // A comment runs to the end of the line.
                for inner in chars.by_ref() {
                    if inner == '\n' {
                        break;
                    }
                }
                commands.push(std::mem::take(&mut words));
            }
            _ => {
                in_word = true;
                word.push(c);
            }
        }
    }
    finish_word(&mut words, &mut word, &mut in_word);
    commands.push(words);
    commands.retain(|words| !words.is_empty());
    commands
}

#[cfg(test)]
mod tests {
    use super::*;

    fn refused(command: &str) -> bool {
        broad_kill_refusal(command).is_some()
    }

    #[test]
    fn the_1227_command_is_refused() {
        assert!(refused(
            r#"tail -5 /tmp/w1226/shard-other-04.log; pkill -f "run-rsid-test-shards.sh shard" ; sleep 2; systemd-run --user --collect --unit w1226-shards bash -c 'true'"#
        ));
    }

    #[test]
    fn pattern_and_name_killers_are_refused_in_any_command_position() {
        for command in [
            "pkill cargo",
            "pkill -9 -f rsi-rolling-land",
            "/usr/bin/pkill -u $USER rsid",
            "killall cargo",
            "killall5 -9",
            "sudo pkill x",
            "FOO=1 timeout 5 pkill x",
            "nice -n 5 killall x",
            "true && pkill x",
            "true || pkill x",
            "(pkill x)",
            "echo $(pkill x)",
            "echo `pkill x`",
            "bash -c 'pkill -f shard'",
            "sh -c \"sleep 1; killall cargo\"",
            "foo | xargs pkill",
        ] {
            assert!(refused(command), "{command}");
        }
    }

    #[test]
    fn kill_of_every_process_or_fed_by_pgrep_is_refused() {
        for command in [
            "kill -9 -1",
            "kill -TERM -1",
            "kill -s KILL -1",
            "kill -- -1",
            "kill $(pgrep -f run-rsid-test-shards)",
            "pgrep -f cargo | xargs kill",
            "pgrep -f cargo | xargs -r kill -9",
            "kill -9 `pidof cargo`",
        ] {
            assert!(refused(command), "{command}");
        }
    }

    #[test]
    fn exact_pid_kills_and_pattern_text_are_allowed() {
        for command in [
            "kill 12345",
            "kill -9 12345",
            "kill -1 12345",
            "kill -TERM $child",
            "kill %1",
            "sleep 100 & pid=$!; kill $pid",
            "systemctl --user stop w1227-tests.service",
            "pgrep -af run-rsid-test-shards",
            "grep -n pkill scripts/cargo-slot",
            "echo 'do not pkill -f anything'",
            "git log --grep=\"killall\"",
            "rg 'kill -9 -1' docs/",
            "cargo test -p rsi-common kill_guard",
            "# pkill is refused\necho ok",
        ] {
            assert!(!refused(command), "{command}");
        }
    }
}
