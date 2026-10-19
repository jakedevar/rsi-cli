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
or stop your own unit with `systemctl --user stop <your-unit>`. A \
`systemctl stop`/`kill`/`restart` with a glob (`rsi-job-*`) or an `rsi*.slice` stops \
every agent's job units and is refused too (#2121).";

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
        if program == "systemctl" && stops_unit_glob(args) {
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

/// #2121: `systemctl stop|kill|restart` of a glob (`rsi-job-*`) or an rsi
/// slice reaches every agent's job unit. On 2026-10-13 three concurrent
/// `scoped_test` units stopped in one millisecond with no daemon stop logged.
/// Naming exact units stays allowed.
fn stops_unit_glob(args: &[String]) -> bool {
    // Option values (`-t service`, `-s SIGKILL`) sit before the verb, so find
    // the verb among all non-option words rather than assuming the first.
    let mut operands = args.iter().filter(|arg| !arg.starts_with('-'));
    operands.any(|word| {
        matches!(
            word.as_str(),
            "stop" | "kill" | "restart" | "try-restart" | "reload-or-restart"
        )
    }) && operands.any(|unit| {
        unit.contains(['*', '?', '[']) || (unit.starts_with("rsi") && unit.ends_with(".slice"))
    })
}

/// The program (basename) and its arguments after leading `NAME=value`
/// assignments and pass-through wrappers.
pub fn command_word(words: &[String]) -> Option<(&str, &[String])> {
    let mut index = 0;
    while index < words.len() {
        let word = words[index].as_str();
        let base = word.rsplit('/').next().unwrap_or(word);
        if is_assignment(word) {
            index += 1;
            continue;
        }
        if word.starts_with('-') && index > 0 {
            // `env -u NAME cmd` must reach cmd, not mistake NAME for it; the
            // same holds for a cluster whose last flag takes the value
            // (`env -iC DIR cmd`, `sudo -iu USER cmd`).
            if matches!(word, "-u" | "--unset" | "-C" | "--chdir") || ends_in_value_flag(word) {
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

/// A short flag cluster (`-iC`, `-iu`) whose last flag takes the next word.
fn ends_in_value_flag(word: &str) -> bool {
    word.len() > 2
        && word.starts_with('-')
        && !word.starts_with("--")
        && word.chars().skip(1).all(|c| c.is_ascii_alphabetic())
        && word.ends_with(['u', 'C'])
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
/// `|`, newlines, parentheses, standalone braces, backticks and `$(`. Quotes are removed
/// from words; separators inside quotes do not split.
pub fn simple_commands(script: &str) -> Vec<Vec<String>> {
    scoped_commands(script)
        .into_iter()
        .filter_map(|item| match item {
            Scoped::Command(words) => Some(words),
            Scoped::Enter | Scoped::Exit => None,
        })
        .collect()
}

/// One item of [`scoped_commands`]: a simple command, or the edge of a child
/// shell execution scope (a parenthesized subshell or a command substitution).
/// A child's `cd` never reaches the shell that started it (#2582).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Scoped {
    Command(Vec<String>),
    Enter,
    Exit,
}

/// [`simple_commands`] with the subshell and command-substitution boundaries
/// kept as `Enter`/`Exit` items. A stray `)` (a `case` pattern) is an unmatched
/// `Exit`; readers ignore it.
pub fn scoped_commands(script: &str) -> Vec<Scoped> {
    let mut commands: Vec<Scoped> = Vec::new();
    let mut backtick_open = false;
    let mut words: Vec<String> = Vec::new();
    let mut word = String::new();
    let mut in_word = false;
    let mut chars = script.chars().peekable();
    let mut heredocs = Vec::new();
    let finish_word = |words: &mut Vec<String>, word: &mut String, in_word: &mut bool| {
        if *in_word {
            words.push(std::mem::take(word));
            *in_word = false;
        }
    };
    while let Some(c) = chars.next() {
        match c {
            '<' if chars.peek() == Some(&'<') => {
                chars.next();
                finish_word(&mut words, &mut word, &mut in_word);
                if chars.peek() == Some(&'<') {
                    // A here-string is one argument, not a following body.
                    chars.next();
                    heredoc_word(&mut chars);
                } else {
                    let strip_tabs = chars.peek() == Some(&'-');
                    if strip_tabs {
                        chars.next();
                    }
                    let (delimiter, quoted) = heredoc_word(&mut chars);
                    if !delimiter.is_empty() || quoted {
                        heredocs.push((delimiter, strip_tabs, quoted));
                    }
                }
            }
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
                        // #2288: Bash runs `$(..)` and backticks inside double
                        // quotes. The text stays part of the word and its commands
                        // are scanned with this same lexer, recursively.
                        '$' if chars.peek() == Some(&'(') => {
                            chars.next();
                            let body = read_substitution(&mut chars, ')');
                            word.push_str("$(");
                            word.push_str(&body);
                            word.push(')');
                            commands.push(Scoped::Enter);
                            commands.extend(scoped_commands(&body));
                            commands.push(Scoped::Exit);
                        }
                        '`' => {
                            let body = read_substitution(&mut chars, '`');
                            word.push('`');
                            word.push_str(&body);
                            word.push('`');
                            commands.push(Scoped::Enter);
                            commands.extend(scoped_commands(&body));
                            commands.push(Scoped::Exit);
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
                commands.push(Scoped::Command(std::mem::take(&mut words)));
                commands.push(Scoped::Enter);
            }
            // Braces inside words are operands (brace/parameter expansion),
            // not command boundaries. Splitting a log path such as
            // `/tmp/{rsid-store,rsid}.log` invents an executable named
            // `rsid-store,rsid`, which the test guard mistakes for libtest.
            '{' | '}'
                if in_word
                    || chars.peek().is_some_and(|c| {
                        !c.is_whitespace() && !matches!(c, ';' | '&' | '|' | '(' | ')')
                    }) =>
            {
                in_word = true;
                word.push(c);
            }
            ';' | '&' | '|' | '\n' | '(' | ')' | '{' | '}' | '`' => {
                finish_word(&mut words, &mut word, &mut in_word);
                commands.push(Scoped::Command(std::mem::take(&mut words)));
                match c {
                    '(' => commands.push(Scoped::Enter),
                    ')' => commands.push(Scoped::Exit),
                    '`' => {
                        commands.push(if backtick_open {
                            Scoped::Exit
                        } else {
                            Scoped::Enter
                        });
                        backtick_open = !backtick_open;
                    }
                    '\n' => consume_heredocs(&mut chars, &mut heredocs, &mut commands),
                    _ => {}
                }
            }
            c if c.is_whitespace() => finish_word(&mut words, &mut word, &mut in_word),
            '#' if !in_word => {
                // A comment runs to the end of the line.
                for inner in chars.by_ref() {
                    if inner == '\n' {
                        break;
                    }
                }
                commands.push(Scoped::Command(std::mem::take(&mut words)));
                consume_heredocs(&mut chars, &mut heredocs, &mut commands);
            }
            _ => {
                in_word = true;
                word.push(c);
            }
        }
    }
    finish_word(&mut words, &mut word, &mut in_word);
    commands.push(Scoped::Command(words));
    commands.retain(|item| !matches!(item, Scoped::Command(words) if words.is_empty()));
    commands
}

/// The body of a command substitution whose opener (`$(` or a backtick) was
/// just consumed, up to its closing `)` or backtick. Quotes, escapes and nested
/// parentheses inside `$(..)` are honoured, so a `"` inside the body does not end
/// the double-quoted word that holds the substitution. An unterminated
/// substitution runs to the end of the text.
fn read_substitution(
    chars: &mut std::iter::Peekable<std::str::Chars<'_>>,
    closing: char,
) -> String {
    let mut body = String::new();
    let mut depth = 1usize;
    while let Some(c) = chars.next() {
        match c {
            '\\' => {
                let Some(escaped) = chars.next() else {
                    body.push(c);
                    break;
                };
                // Inside backticks `\`` , `\\` and `\$` are the escapes of the
                // enclosing substitution; the body is what the inner shell reads.
                if closing == '`' && matches!(escaped, '`' | '\\' | '$') {
                    body.push(escaped);
                } else {
                    body.push(c);
                    body.push(escaped);
                }
            }
            '\'' if closing == ')' => {
                body.push(c);
                for inner in chars.by_ref() {
                    body.push(inner);
                    if inner == '\'' {
                        break;
                    }
                }
            }
            '"' if closing == ')' => {
                body.push(c);
                copy_double_quoted(chars, &mut body);
            }
            '(' if closing == ')' => {
                depth += 1;
                body.push(c);
            }
            c if c == closing => {
                depth -= 1;
                if depth == 0 {
                    break;
                }
                body.push(c);
            }
            _ => body.push(c),
        }
    }
    body
}

/// Copy the rest of a double-quoted string, closing quote included, nested
/// `$(..)` and backtick substitutions included verbatim.
fn copy_double_quoted(chars: &mut std::iter::Peekable<std::str::Chars<'_>>, out: &mut String) {
    while let Some(c) = chars.next() {
        out.push(c);
        match c {
            '"' => return,
            '\\' => {
                if let Some(escaped) = chars.next() {
                    out.push(escaped);
                }
            }
            '$' if chars.peek() == Some(&'(') => {
                chars.next();
                out.push('(');
                out.push_str(&read_substitution(chars, ')'));
                out.push(')');
            }
            '`' => {
                while let Some(inner) = chars.next() {
                    out.push(inner);
                    if inner == '\\' {
                        if let Some(escaped) = chars.next() {
                            out.push(escaped);
                        }
                    } else if inner == '`' {
                        break;
                    }
                }
            }
            _ => {}
        }
    }
}

/// Quote removal on the redirection word; quoting any part suppresses body
/// expansion. Delimiters are collected before bodies begin at the next newline.
fn heredoc_word(chars: &mut std::iter::Peekable<std::str::Chars<'_>>) -> (String, bool) {
    while chars.peek().is_some_and(|c| matches!(*c, ' ' | '\t')) {
        chars.next();
    }
    let mut delimiter = String::new();
    let mut quoted = false;
    while let Some(&c) = chars.peek() {
        if c.is_whitespace() || matches!(c, ';' | '&' | '|' | '<' | '>' | '(' | ')') {
            break;
        }
        chars.next();
        match c {
            '\'' | '"' => {
                quoted = true;
                while let Some(inner) = chars.next() {
                    if inner == c {
                        break;
                    }
                    if c == '"' && inner == '\\' {
                        if let Some(escaped) = chars.next() {
                            if !matches!(escaped, '$' | '`' | '"' | '\\' | '\n') {
                                delimiter.push('\\');
                            }
                            if escaped != '\n' {
                                delimiter.push(escaped);
                            }
                        }
                    } else {
                        delimiter.push(inner);
                    }
                }
            }
            '\\' => {
                quoted = true;
                if let Some(escaped) = chars.next() {
                    delimiter.push(escaped);
                }
            }
            _ => delimiter.push(c),
        }
    }
    (delimiter, quoted)
}

fn consume_heredocs(
    chars: &mut std::iter::Peekable<std::str::Chars<'_>>,
    heredocs: &mut Vec<(String, bool, bool)>,
    commands: &mut Vec<Scoped>,
) {
    for (delimiter, strip_tabs, quoted) in heredocs.drain(..) {
        let mut body = String::new();
        loop {
            let mut line = String::new();
            for c in chars.by_ref() {
                if c == '\n' {
                    break;
                }
                line.push(c);
            }
            let line = if strip_tabs {
                line.trim_start_matches('\t')
            } else {
                &line
            };
            if line == delimiter {
                break;
            }
            body.push_str(line);
            body.push('\n');
            if chars.peek().is_none() {
                break;
            }
        }
        if !quoted {
            // Plain body text is data, but Bash executes command substitutions
            // in unquoted heredocs. Keep checking only those executable parts.
            commands.extend(
                heredoc_substitutions(&body)
                    .into_iter()
                    .map(Scoped::Command),
            );
        }
    }
}

fn heredoc_substitutions(body: &str) -> Vec<Vec<String>> {
    let mut commands = Vec::new();
    let mut chars = body.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\\' {
            chars.next();
            continue;
        }
        let closing = match c {
            '$' if chars.peek() == Some(&'(') => {
                chars.next();
                ')'
            }
            '`' => '`',
            _ => continue,
        };
        let arithmetic = closing == ')' && chars.peek() == Some(&'(');
        let mut inner = String::new();
        let mut nesting = 1;
        let mut quote = None;
        while let Some(c) = chars.next() {
            if c == '\\' && quote != Some('\'') {
                inner.push(c);
                if let Some(escaped) = chars.next() {
                    inner.push(escaped);
                }
                continue;
            }
            if let Some(quoted) = quote {
                if c == quoted {
                    quote = None;
                }
                inner.push(c);
                continue;
            }
            if matches!(c, '\'' | '"') {
                quote = Some(c);
                inner.push(c);
                continue;
            }
            if closing == ')' && c == '(' {
                nesting += 1;
            } else if c == closing {
                nesting -= 1;
                if nesting == 0 {
                    break;
                }
            }
            inner.push(c);
        }
        if arithmetic {
            commands.extend(heredoc_substitutions(&inner));
        } else {
            commands.extend(simple_commands(&inner));
        }
    }
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
            "{ pkill x; }",
            "(pkill x)",
            "echo $(pkill x)",
            "echo `pkill x`",
            "bash -c 'pkill -f shard'",
            "sh -c \"sleep 1; killall cargo\"",
            "foo | xargs pkill",
            // A flag cluster ending in a value flag must not hide the program.
            "env -iu NAME pkill x",
            "env -iC /tmp pkill x",
            "sudo -iu root killall x",
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
    fn systemctl_glob_or_slice_stops_are_refused_but_exact_units_are_allowed() {
        for command in [
            "systemctl --user stop 'rsi-job-*'",
            "systemctl --user stop rsi-job-*.service",
            "systemctl --user -t service stop 'rsi-scoped-test-*'",
            "systemctl --user kill --signal=KILL 'rsi-job-?'",
            "systemctl --user stop rsi-build.slice",
            "sudo systemctl --user restart 'rsi-*'",
        ] {
            assert!(refused(command), "{command}");
        }
        for command in [
            "systemctl --user stop rsi-job-0123.service",
            "systemctl --user stop rsi-job-0123 rsi-scoped-test-0123",
            "systemctl --user status 'rsi-job-*'",
            "systemctl --user list-units 'rsi-job-*'",
        ] {
            assert!(!refused(command), "{command}");
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

    /// #2288: Bash runs `$(..)` and backticks inside double quotes.
    #[test]
    fn substitutions_inside_double_quotes_are_scanned_recursively() {
        for command in [
            r#"echo "$(pkill x)""#,
            r#"printf '%s' "$(pkill -f shard)""#,
            r#"echo "`killall cargo`""#,
            r#"echo "a $(echo "b $(pkill x)") c""#,
            r#"echo "$(echo "x")"; pkill y"#,
            // A quote inside the body must not end the outer word early.
            r#"echo "$(echo '"')"; pkill y"#,
            r#"echo "$(echo "a)" && pkill x)""#,
            r#"echo "`echo \`pkill x\``""#,
            r#"kill "$(pgrep -f x)""#,
            r#"echo "$( (pkill x) )""#,
            r#"bash -c 'echo "$(pkill x)"'"#,
            r#"FOO="$(pkill x)" true"#,
        ] {
            assert!(refused(command), "{command}");
        }
    }

    #[test]
    fn quoted_text_that_only_looks_like_a_substitution_is_allowed() {
        for command in [
            r#"echo "pkill x""#,
            r#"echo "(pkill x)""#,
            r#"echo "\$(pkill x)""#,
            r#"echo "\`pkill x\`""#,
            r#"echo '$(pkill x)'"#,
            r#"echo "$(date +%F)" "$((1 + 2))" "`hostname`""#,
            r#"git commit -m "fix it's $(date)" && echo done"#,
            r#"echo "$(echo "a")" | wc -c"#,
        ] {
            assert!(!refused(command), "{command}");
        }
    }

    #[test]
    fn heredoc_data_is_allowed_with_shell_quote_removal_and_multiple_bodies() {
        for command in [
            "python3 <<'PY'\npkill = 'comment text'\nPY",
            "cat <<EOF\npkill x\nkillall y\nEOF\necho done",
            "cat <<\\EOF\n$(pkill x)\nEOF",
            "cat <<E'OF' # pkill comment\n$(pkill x)\nEOF",
            "cat <<-EOF\n\tpkill x\n\tEOF\necho done",
            "cat <<A <<'B B'\npkill x\nA\nkillall y\nB B",
            "cat <<EOF\n\\$(pkill x)\nEOF",
            "cat <<EOF\n$(printf '(')\npkill is data\nEOF",
            "cat <<''\npkill is data\n\necho done",
            "cat <<EOF\n$(( pkill ))\nEOF",
            "cat <<<'pkill x'",
        ] {
            assert!(!refused(command), "{command}");
        }
    }

    #[test]
    fn executable_commands_after_or_inside_unquoted_heredocs_are_refused() {
        for command in [
            "cat <<'EOF'\npkill is data\nEOF\npkill x",
            "cat <<-EOF\n\tpkill is data\n\tEOF\nsudo pkill x",
            "cat <<EOF; pkill x\ntext\nEOF",
            "cat <<EOF\n$(pkill x)\nEOF",
            "cat <<EOF\n`killall x`\nEOF",
            "cat <<EOF\n$(echo $(pkill x))\nEOF",
            "cat <<EOF\n$(( $(pkill x) + 1 ))\nEOF",
        ] {
            assert!(refused(command), "{command}");
        }
    }
}
