//! Lexical custody scan of a shell command line for git (#2256, #2320).
//!
//! AGENTS.md hard rules 1, 2 and 9 forbid the git commands that break an RSI
//! sandbox's sharing of one repository: moving the sandbox branch (`checkout`,
//! `switch`, `reset --hard`), dropping its allocated source ancestry, rewriting remote history, clobbering another
//! agent's branch and using the shared stash. This module reads a command with
//! the #1227 kill-guard lexer ([`crate::kill_guard`]) and reports what it would
//! do. It has no I/O of its own beyond [`check_command_sync`], the blocking
//! settle the CLI provider hook uses; the Harness `shell`/`exec_command`/`git`
//! tools scan with the same code and settle with their own async probe
//! (`rsid::session::harness::tools::custody_guard`), so the rules cannot drift
//! between provider families.
//!
//! A `cd` into a scratch or temporary location followed by a git command that
//! changes a repository without naming one (#2571) is refused too: if the `cd`
//! (or the `mkdir`/`mktemp` before it) fails, the shell carries on and the git
//! command runs in the sandbox. Fixtures use `git -C <absolute dir>` instead.
//!
//! `git checkout|switch` in another worktree (a manager's detached deploy
//! worktree) is allowed whether it is named by `git -C <absolute dir>` or by a
//! straight-line `cd <absolute dir> && git checkout --detach <rev>` (#2573,
//! #3037); the hook probes the directory and refuses when it is the sandbox.
//!
//! The scan follows `bash -c '...'`, `eval` and the usual wrappers, refuses
//! aliases rather than resolving them, and fails closed on a script nested
//! deeper than [`MAX_DEPTH`]. It is a guard rail for cooperative agents, not a
//! security boundary: it does not follow scripts, functions, variables, brace
//! expansion, `env -S`, aliases written into config files or here-document
//! bodies.

#![allow(clippy::missing_const_for_fn)]

use crate::kill_guard::{Scoped, command_word, scoped_commands};
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// Nested `bash -c '... eval "..."'` levels followed.
pub const MAX_DEPTH: usize = 4;
/// Shell programs whose `-c` script is scanned too.
pub const SHELLS: &[&str] = &["sh", "bash", "dash", "zsh", "ksh", "fish"];
/// Dirty paths named in a refusal.
pub const MAX_PATHS_SHOWN: usize = 5;

#[derive(Default)]
pub struct Scan {
    /// First unconditional refusal found.
    pub refusal: Option<String>,
    /// Discards allowed only while the paths they touch are clean.
    pub discards: Vec<Discard>,
    /// The script changes directory or repository (`cd`, `env -C`, `git -C`,
    /// `--git-dir`, `--work-tree`, `GIT_DIR=`), so the probe cannot know which
    /// tree a discard hits.
    pub relocated: bool,
    /// A discard shares its line with other commands or a grouping, so the
    /// probe (which runs before the shell starts) cannot vouch for it (#2290).
    pub crowded: bool,
    /// Subcommands that are not built-in git commands: each could be a
    /// configured alias (#2287).
    pub alias_candidates: Vec<String>,
    /// The line reassigns where git reads its configuration (`GIT_CONFIG_GLOBAL=`,
    /// `HOME=`, ...), so the alias probe, which runs without that assignment,
    /// cannot see the aliases the command would (#2319).
    pub config_source_overridden: bool,
    /// An earlier `cd`/`pushd` of the line went to a scratch or temporary
    /// location (or to the result of `mktemp`); if it fails the shell carries on
    /// in the sandbox (#2571).
    pub scratch_cd: bool,
    /// A `mktemp` command runs somewhere in the line, so a `cd` to a variable
    /// or to nothing probably goes to its result.
    pub mktemp_seen: bool,
    /// Variables the line assigns a scratch-looking value.
    pub scratch_vars: Vec<String>,
    /// The `-C` chain of the git invocation being classified when it names its
    /// repository by a literal absolute directory and nothing else relocates it
    /// (#2573). `None` when the target worktree cannot be named lexically.
    pub invocation_directory: Option<Vec<String>>,
    /// Branch changes that are refused only when the worktree they name is this
    /// session's sandbox; the caller resolves them with [`branch_change_verdict`].
    pub pending_branch_changes: Vec<PendingBranchChange>,
    /// The literal absolute directory the shell reading the command is certain
    /// to be in after an earlier `cd /abs/dir` of a straight-line script (#3037);
    /// a git invocation with no `-C` of its own runs there. `None` when no `cd`
    /// happened or the `cd` is conditional, relative, dynamic or unreadable.
    pub cd_directory: Option<String>,
    /// The line assigns `GIT_DIR=` or `GIT_WORK_TREE=` somewhere, so a `cd` or
    /// `-C` no longer decides which repository git works on.
    pub repository_env_set: bool,
}

/// A branch change or ancestry rewrite in the worktree `git -C <directory>`
/// names (#2573).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingBranchChange {
    pub subcommand: &'static str,
    /// The `-C` values in order (the first is absolute).
    pub directory: Vec<String>,
}

/// The repository pins one git invocation gets from the assignments and
/// wrappers in front of it (`GIT_DIR=/abs GIT_WORK_TREE=/abs git ...`). They
/// belong to that invocation only: an earlier `export GIT_DIR=...` or an
/// assignment on another command pins nothing here (#2571).
#[derive(Clone, Copy, Default, Debug, PartialEq, Eq)]
pub struct EnvPins {
    pub git_dir: bool,
    pub work_tree: bool,
    /// A wrapper (`env -C DIR`, `env --chdir`) starts the command elsewhere, so
    /// an earlier `cd` does not tell where it runs (#3037).
    pub chdir: bool,
}

/// The pins in `prefix`, the words before the program of one command: the
/// effective environment after its assignments and its `env` options, in order.
/// The last assignment of a name wins, `env -u NAME` / `--unset` removes it and
/// `env -i` removes everything before it (#2580). `env -S` hides words from the
/// scan, so it pins nothing.
#[must_use]
pub fn env_pins(prefix: &[String]) -> EnvPins {
    fn unset(pins: &mut EnvPins, name: &str) {
        match name {
            "GIT_DIR" => pins.git_dir = false,
            "GIT_WORK_TREE" => pins.work_tree = false,
            _ => {}
        }
    }
    let mut pins = EnvPins::default();
    let mut index = 0;
    while let Some(word) = prefix.get(index).map(String::as_str) {
        index += 1;
        if let Some(value) = word.strip_prefix("GIT_DIR=") {
            pins.git_dir = pins_repository(value);
        } else if let Some(value) = word.strip_prefix("GIT_WORK_TREE=") {
            pins.work_tree = pins_repository(value);
        } else if word == "--ignore-environment"
            || word == "--split-string"
            || word.starts_with("--split-string=")
        {
            pins = EnvPins::default();
        } else if word == "--unset" {
            if let Some(name) = prefix.get(index) {
                unset(&mut pins, name);
            }
            index += 1;
        } else if let Some(name) = word.strip_prefix("--unset=") {
            unset(&mut pins, name);
        } else if word == "--chdir" {
            index += 1;
        } else if is_short_cluster(word) {
            let flags = &word[1..];
            if flags.contains(['i', 'S']) {
                pins = EnvPins::default();
            }
            if let Some(at) = flags.find('u') {
                let attached = &flags[at + 1..];
                if attached.is_empty() {
                    if let Some(name) = prefix.get(index) {
                        unset(&mut pins, name);
                    }
                    index += 1;
                } else {
                    unset(&mut pins, attached);
                }
            } else if flags.ends_with('C') {
                index += 1;
            }
        }
    }
    pins
}

impl Scan {
    pub fn refuse(&mut self, message: String) {
        self.refusal.get_or_insert(message);
    }

    /// `git <subcommand>` moves HEAD of the worktree it runs in. Refused now
    /// unless the invocation names another worktree by a literal absolute `-C`;
    /// then the caller decides, because only it knows which worktree is the
    /// sandbox (#2573).
    pub fn branch_change(&mut self, subcommand: &'static str) {
        match self.invocation_directory.clone() {
            Some(directory) => self.pending_branch_changes.push(PendingBranchChange {
                subcommand,
                directory,
            }),
            None => self.refuse(branch_change(subcommand)),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ignored {
    No,
    Also,
    Only,
}

pub enum Discard {
    /// `checkout -- <path>`, `restore`: loses uncommitted changes to tracked
    /// files under `pathspecs` (empty = the whole tree).
    Tracked {
        command: &'static str,
        pathspecs: Vec<String>,
    },
    /// `clean -f`: deletes untracked (and maybe ignored) files.
    Untracked {
        pathspecs: Vec<String>,
        ignored: Ignored,
    },
}

impl Discard {
    #[must_use]
    pub fn command(&self) -> &'static str {
        match self {
            Self::Tracked { command, .. } => command,
            Self::Untracked { .. } => "git clean",
        }
    }
}

/// `alone`: nothing but this script runs in the line (the outer line is one
/// simple command, and so is every enclosing `bash -c`/`eval`).
pub fn scan_script(script: &str, depth: usize, alone: bool, scan: &mut Scan) {
    if depth > MAX_DEPTH {
        // Fail closed: a script this deeply nested is not read (#2319).
        scan.refuse(depth_refusal());
        return;
    }
    let items = scoped_commands(script);
    let command_count = items
        .iter()
        .filter(|item| matches!(item, Scoped::Command(_)))
        .count();
    let alone = alone && command_count == 1 && is_lone_command(script);
    // `cd $(mktemp -d)` splits into `cd` and `mktemp -d`, so look ahead.
    scan.mktemp_seen |= items.iter().any(|item| {
        matches!(item, Scoped::Command(words)
            if command_word(words).is_some_and(|(program, _)| program == "mktemp"))
    });
    // `scan.scratch_cd` is the directory risk of the shell running the command
    // being read. A subshell or command substitution starts with its parent's
    // value, and its own `cd` is forgotten when it ends (#2582).
    let mut parent_scratch_cd: Vec<(bool, Option<String>)> = Vec::new();
    // A `cd` is only certain to have happened for the commands after it when the
    // script is a straight line (#3037).
    let straight = is_straight_line(script) && !items.iter().any(starts_with_reserved_word);
    for item in items {
        let words = match item {
            Scoped::Enter => {
                parent_scratch_cd.push((scan.scratch_cd, scan.cd_directory.clone()));
                continue;
            }
            Scoped::Exit => {
                // An unmatched `)` (a `case` pattern) closes nothing.
                if let Some((scratch_cd, cd_directory)) = parent_scratch_cd.pop() {
                    scan.scratch_cd = scratch_cd;
                    scan.cd_directory = cd_directory;
                }
                continue;
            }
            Scoped::Command(words) => words,
        };
        scan_environment(&words, scan);
        note_scratch_assignments(&words, scan);
        let Some((program, args)) = command_word(&words) else {
            continue;
        };
        if matches!(
            program,
            "export" | "declare" | "typeset" | "local" | "readonly"
        ) {
            let skipped = args.iter().take_while(|arg| arg.starts_with('-')).count();
            note_scratch_assignments(&args[skipped..], scan);
        }
        let prefix = &words[..words.len() - args.len() - 1];
        if wrapper_changes_directory(prefix) {
            scan.relocated = true;
        }
        let discards_before = scan.discards.len();
        match program {
            "cd" | "pushd" | "popd" => {
                scan.relocated = true;
                note_directory_change(program, args, scan);
                scan.cd_directory = if straight && program == "cd" {
                    literal_cd_target(args)
                } else {
                    None
                };
            }
            "git" => scan_git_pinned(args, scan, command_pins(prefix)),
            // `eval` runs in the shell that reads it, so its `cd` stays.
            "eval" => {
                scan_script(&args.join(" "), depth + 1, alone, scan);
                if !straight {
                    // The eval may not have run, so its `cd` is not certain.
                    scan.cd_directory = None;
                }
            }
            program if SHELLS.contains(&program) => {
                if let Some(inner) = shell_script(args) {
                    // A new shell process: it inherits the risk, and its `cd`
                    // cannot move this shell.
                    let parent = (scan.scratch_cd, scan.cd_directory.clone());
                    scan_script(inner, depth + 1, alone, scan);
                    (scan.scratch_cd, scan.cd_directory) = parent;
                }
            }
            program => {
                // `git-stash` is the same program as `git stash`.
                if let Some(subcommand) = program.strip_prefix("git-") {
                    let mut git_args = vec![subcommand.to_owned()];
                    git_args.extend_from_slice(args);
                    scan_git_pinned(&git_args, scan, command_pins(prefix));
                }
            }
        }
        if scan.discards.len() > discards_before && !alone {
            scan.crowded = true;
        }
    }
    // An unterminated subshell still ends with the script.
    if let Some((scratch_cd, cd_directory)) = parent_scratch_cd.into_iter().next() {
        scan.scratch_cd = scratch_cd;
        scan.cd_directory = cd_directory;
    }
}

/// [`env_pins`] plus whether a wrapper moves the command to another directory.
fn command_pins(prefix: &[String]) -> EnvPins {
    EnvPins {
        chdir: wrapper_changes_directory(prefix),
        ..env_pins(prefix)
    }
}

/// Shell reserved words that start a compound command: a script using one is
/// not a straight line (the commands inside may not run).
fn starts_with_reserved_word(item: &Scoped) -> bool {
    matches!(item, Scoped::Command(words) if words.first().is_some_and(|word| matches!(
        word.as_str(),
        "if" | "then" | "else" | "elif" | "fi" | "for" | "while" | "until" | "do" | "done"
            | "case" | "esac" | "select" | "!" | "function"
    )))
}

/// True when every command of `script` runs, in order, in the same shell, and
/// either all are joined by `&&` (a later command runs only if every earlier one,
/// a `cd` included, succeeded) or none is (`;` and newline run on regardless, so
/// a `cd` before them is not conditional). No `||`, pipe, background `&`,
/// grouping or substitution (#3037).
fn is_straight_line(script: &str) -> bool {
    let script = script.trim().trim_end_matches(';').trim_end();
    let mut chars = script.chars().peekable();
    let (mut and_then, mut sequence) = (false, false);
    let mut word_start = true;
    while let Some(c) = chars.next() {
        match c {
            '\'' => {
                for inner in chars.by_ref() {
                    if inner == '\'' {
                        break;
                    }
                }
            }
            '"' => {
                while let Some(inner) = chars.next() {
                    match inner {
                        '"' => break,
                        '\\' => {
                            chars.next();
                        }
                        '`' => return false,
                        '$' if chars.peek() == Some(&'(') => return false,
                        _ => {}
                    }
                }
            }
            '\\' => {
                chars.next();
            }
            '&' if chars.peek() == Some(&'&') => {
                chars.next();
                and_then = true;
            }
            ';' | '\n' => sequence = true,
            '&' | '|' | '(' | ')' | '{' | '}' | '`' => return false,
            '$' if chars.peek() == Some(&'(') => return false,
            '#' if word_start => break,
            _ => {}
        }
        word_start = c.is_whitespace();
    }
    !(and_then && sequence)
}

/// The directory a `cd` is certain to enter: exactly one operand that is a
/// literal absolute path, after the options `-L`, `-P`, `-e`, `-@` and `--`.
fn literal_cd_target(args: &[String]) -> Option<String> {
    let mut operands = args
        .iter()
        .skip_while(|arg| matches!(arg.as_str(), "-L" | "-P" | "-e" | "-@" | "--"));
    let target = operands.next()?;
    (operands.next().is_none()
        && pins_repository(target)
        && !target.contains(['~', '*', '?', '[', '{']))
    .then(|| target.clone())
}

/// Environment variables that move where git reads its configuration from. A
/// command-line assignment reaches only the command it prefixes, so the alias
/// probe never sees it (#2319).
pub const CONFIG_SOURCE_VARIABLES: &[&str] = &[
    "GIT_CONFIG",
    "GIT_CONFIG_GLOBAL",
    "GIT_CONFIG_SYSTEM",
    "GIT_CONFIG_NOSYSTEM",
    "GIT_COMMON_DIR",
    "HOME",
    "XDG_CONFIG_HOME",
];

/// `NAME=value` words that relocate git or inject configuration. They count on
/// any command of the line: `export GIT_DIR=..` changes every later command.
pub fn scan_environment(words: &[String], scan: &mut Scan) {
    for word in words {
        if word.starts_with("GIT_DIR=") || word.starts_with("GIT_WORK_TREE=") {
            scan.relocated = true;
            scan.repository_env_set = true;
        } else if let Some((name, value)) = word.split_once('=') {
            // `NAME+=value` appends in bash.
            let name = name.strip_suffix('+').unwrap_or(name);
            if CONFIG_SOURCE_VARIABLES.contains(&name) {
                scan.config_source_overridden = true;
            }
            let value = value.to_ascii_lowercase();
            let injects = if name == "GIT_CONFIG_PARAMETERS" {
                value.contains("alias.") || value.contains("include.")
            } else if name.starts_with("GIT_CONFIG_KEY_") {
                is_alias_key(&value)
            } else {
                false
            };
            if injects {
                scan.refuse(ALIAS_REFUSAL.to_owned());
            }
        }
    }
}

/// `env -C DIR`, `env -iC DIR` and `env --chdir[=]DIR` start the command in
/// another directory, like `cd` (#2289). `prefix` is everything before the
/// program: assignments and wrappers with their options.
pub fn wrapper_changes_directory(prefix: &[String]) -> bool {
    prefix.iter().skip(1).any(|word| {
        word == "--chdir"
            || word.starts_with("--chdir=")
            || (word.starts_with('-') && !word.starts_with("--") && word[1..].contains('C'))
    })
}

/// True when `script` is one simple command: no unquoted `;`, `&`, `|`,
/// newline, parenthesis, brace, backtick or `$(` (a trailing newline and a
/// trailing comment are fine). `simple_commands` drops grouping, so
/// `(git restore .)` and `git restore . &` look like one command there; the
/// custody guard wants the stricter reading (#2290).
pub fn is_lone_command(script: &str) -> bool {
    let mut chars = script.trim().chars().peekable();
    let mut word_start = true;
    while let Some(c) = chars.next() {
        match c {
            '\'' => {
                for inner in chars.by_ref() {
                    if inner == '\'' {
                        break;
                    }
                }
            }
            '"' => {
                while let Some(inner) = chars.next() {
                    match inner {
                        '"' => break,
                        '\\' => {
                            chars.next();
                        }
                        '`' => return false,
                        '$' if chars.peek() == Some(&'(') => return false,
                        _ => {}
                    }
                }
            }
            '\\' => {
                chars.next();
            }
            ';' | '&' | '|' | '\n' | '(' | ')' | '{' | '}' | '`' => return false,
            // A comment runs to the end of the line and is not a command.
            '#' if word_start => break,
            _ => {}
        }
        word_start = c.is_whitespace();
    }
    true
}

/// The script after a `-c` (or a cluster such as `-lc`, `-ec`) of a shell.
pub fn shell_script(args: &[String]) -> Option<&str> {
    let position = args.iter().position(|arg| {
        is_short_cluster(arg) && arg[1..].contains('c') && arg[1..].chars().all(char::is_alphabetic)
    })?;
    args.get(position + 1).map(String::as_str)
}

pub fn is_short_cluster(arg: &str) -> bool {
    arg.len() > 1 && arg.starts_with('-') && !arg.starts_with("--")
}

/// Classify one `git` invocation; `args` are everything after `git`.
pub fn scan_git(args: &[String], scan: &mut Scan) {
    scan_git_pinned(args, scan, EnvPins::default());
}

/// [`scan_git`] for an invocation whose own environment prefix is `env`.
pub fn scan_git_pinned(args: &[String], scan: &mut Scan, env: EnvPins) {
    let mut index = 0;
    // What this invocation's `-C`, `--git-dir` and `--work-tree` name by a
    // literal absolute path (#2571).
    let mut pinned_directory = false;
    let mut pinned_git_dir = env.git_dir;
    let mut pinned_work_tree = env.work_tree;
    // The `-C` chain, and whether anything else (`--git-dir`, `--work-tree`,
    // `GIT_DIR=`) also decides the repository (#2573).
    let mut directories: Vec<String> = Vec::new();
    let mut repository_redirected = env.git_dir || env.work_tree || scan.repository_env_set;
    while let Some(arg) = args.get(index).map(String::as_str) {
        match arg {
            "-C" | "--git-dir" | "--work-tree" => {
                scan.relocated = true;
                let pins = args
                    .get(index + 1)
                    .is_some_and(|value| pins_repository(value));
                if arg == "-C" {
                    directories.extend(args.get(index + 1).cloned());
                } else {
                    repository_redirected = true;
                }
                match arg {
                    "-C" => pinned_directory |= pins,
                    "--git-dir" => pinned_git_dir |= pins,
                    _ => pinned_work_tree |= pins,
                }
                index += 2;
            }
            // `-c name=value` and `--config-env name=ENVVAR` set configuration
            // for this one invocation, aliases included.
            "-c" | "--config-env" => {
                if args.get(index + 1).is_some_and(|value| names_alias(value)) {
                    scan.refuse(ALIAS_REFUSAL.to_owned());
                }
                index += 2;
            }
            "--namespace" | "--super-prefix" => index += 2,
            _ if arg.starts_with("--config-env=") => {
                if names_alias(&arg["--config-env=".len()..]) {
                    scan.refuse(ALIAS_REFUSAL.to_owned());
                }
                index += 1;
            }
            _ if arg.starts_with("--git-dir=") || arg.starts_with("--work-tree=") => {
                scan.relocated = true;
                repository_redirected = true;
                let pins = arg
                    .split_once('=')
                    .is_some_and(|(_, value)| pins_repository(value));
                if arg.starts_with("--git-dir=") {
                    pinned_git_dir |= pins;
                } else {
                    pinned_work_tree |= pins;
                }
                index += 1;
            }
            _ if arg.starts_with('-') => index += 1,
            _ => break,
        }
    }
    let Some(subcommand) = args.get(index).map(String::as_str) else {
        return;
    };
    let rest = &args[index + 1..];
    // Only a chain that starts at a literal absolute directory and holds no
    // variable or substitution can be resolved without running the shell.
    // A `cd` to a literal absolute directory before it is where a `-C` chain
    // starts (#3037).
    let chain: Vec<String> = scan
        .cd_directory
        .clone()
        .filter(|_| !env.chdir)
        .into_iter()
        .chain(directories)
        .collect();
    scan.invocation_directory = (!repository_redirected
        && chain.first().is_some_and(|first| pins_repository(first))
        && chain.iter().all(|value| !value.contains(['$', '`', '~'])))
    .then_some(chain);
    // A built-in command cannot be shadowed by an alias, so only the rest can
    // hide one; `settle` checks them against the configured aliases.
    if !subcommand.is_empty() && !BUILTIN_SUBCOMMANDS.contains(&subcommand) {
        scan.alias_candidates.push(subcommand.to_owned());
    }
    // Preserve the actionable scratch-chain refusal before a newly guarded
    // rewrite queues a worktree probe. Only an explicit repository pin makes
    // a failed scratch `cd` safe for these commands.
    if scan.scratch_cd
        && !pinned_directory
        && !(pinned_git_dir && pinned_work_tree)
        && matches!(
            subcommand,
            "rebase" | "filter-branch" | "replace" | "update-ref" | "reset" | "pull"
        )
    {
        scan.refuse(scratch_cd_refusal(subcommand));
    }
    let pending_before = scan.pending_branch_changes.len();
    match subcommand {
        "config" => scan_config(rest, scan),
        "stash" => scan.refuse(STASH_REFUSAL.to_owned()),
        "switch" => scan.branch_change("switch"),
        "checkout" => scan_checkout(rest, scan),
        // The hook has no authenticated custody-source pin. Refuse rewrites
        // conservatively in managed worktrees; a separate fixture/deploy tree
        // is resolved by the existing worktree probe, just like checkout.
        "rebase" if rest != ["--abort"] => scan.branch_change("rebase"),
        "filter-branch" => scan.branch_change("filter-branch"),
        "replace" if !rest.is_empty() && !rest.iter().any(|arg| arg == "--list" || arg == "-l") => {
            scan.branch_change("replace");
        }
        "update-ref" => scan.branch_change("update-ref"),
        "symbolic-ref"
            if rest.iter().filter(|arg| !arg.starts_with('-')).count() > 1
                || rest.iter().any(|arg| arg == "--delete" || arg == "-d") =>
        {
            scan.branch_change("symbolic-ref");
        }
        "commit"
            if rest
                .iter()
                .any(|arg| "--amend".starts_with(arg.as_str()) && arg.len() >= 4) =>
        {
            scan.branch_change("commit --amend");
        }
        // Even a bare pull can rebase through local configuration.
        "pull"
            if !rest.iter().any(|arg| arg == "--no-rebase")
                || rest
                    .iter()
                    .any(|arg| arg == "-r" || arg.starts_with("--rebase")) =>
        {
            scan.branch_change("pull");
        }
        "reset" => {
            // git accepts any unambiguous prefix of a long option.
            if rest
                .iter()
                .any(|arg| arg.len() >= 4 && "--hard".starts_with(arg.as_str()))
            {
                scan.refuse(RESET_HARD_REFUSAL.to_owned());
            } else if !rest.iter().any(|arg| arg == "--")
                && rest
                    .iter()
                    .any(|arg| !arg.starts_with('-') && arg != "HEAD")
            {
                // Explicit path resets use `reset -- <paths>`; otherwise a
                // revision (including --soft/--mixed) can drop the source.
                scan.branch_change("reset <rev>");
            }
        }
        "restore" => scan_restore(rest, scan),
        "clean" => scan_clean(rest, scan),
        "push" => scan_push(rest, scan),
        "branch" => scan_branch(rest, scan),
        _ => {}
    }
    scan.invocation_directory = None;
    // After the match, so an older refusal keeps its own message.
    // A branch change queued for the caller's probe has its directory resolved
    // before the command runs, so a failed `cd` cannot send it to the sandbox
    // unnoticed (#3037).
    let probed = scan.pending_branch_changes.len() > pending_before;
    let pinned = pinned_directory || probed || (pinned_git_dir && pinned_work_tree);
    if scan.scratch_cd && !pinned && mutates_repository(subcommand, rest) {
        scan.refuse(scratch_cd_refusal(subcommand));
    }
}

/// Path components that mark a scratch or temporary directory.
fn component_is_scratch(component: &str) -> bool {
    let component = component.to_ascii_lowercase();
    matches!(component.as_str(), "tmp" | "temp" | "shm")
        || ["tmp.", "tmp-", "tmp_"]
            .iter()
            .any(|prefix| component.starts_with(prefix))
        || ["-tmp", ".tmp", "_tmp"]
            .iter()
            .any(|suffix| component.ends_with(suffix))
        || component.contains("scratch")
        || component.contains("mktemp")
}

/// Shell variables that conventionally hold a temporary directory.
fn variable_is_scratch(name: &str, scan: &Scan) -> bool {
    let lowercase = name.to_ascii_lowercase();
    matches!(lowercase.as_str(), "tmp" | "temp" | "tmpdir" | "tempdir")
        || lowercase.contains("scratch")
        || scan.scratch_vars.iter().any(|known| known == name)
}

/// The names of the `$NAME` / `${NAME` references in `word`.
fn variable_references(word: &str) -> Vec<&str> {
    word.split('$')
        .skip(1)
        .map(|tail| {
            let tail = tail.strip_prefix('{').unwrap_or(tail);
            let end = tail
                .find(|c: char| !(c == '_' || c.is_ascii_alphanumeric()))
                .unwrap_or(tail.len());
            &tail[..end]
        })
        .filter(|name| !name.is_empty())
        .collect()
}

/// True when `word` (a path, or a value that holds one) names a scratch or
/// temporary location: a `/tmp`-style path component, `mktemp`, or a variable
/// that holds one.
fn names_scratch(word: &str, scan: &Scan) -> bool {
    word.split('/').any(component_is_scratch)
        || variable_references(word)
            .into_iter()
            .any(|name| variable_is_scratch(name, scan))
}

/// `NAME=value` words at the start of `words` whose value names a scratch
/// location (`tmp=/tmp/x git ...`, `export dir=$TMPDIR/x`).
fn note_scratch_assignments(words: &[String], scan: &mut Scan) {
    for word in words {
        let Some((name, value)) = word.split_once('=') else {
            return;
        };
        let name = name.strip_suffix('+').unwrap_or(name);
        let valid = !name.is_empty()
            && name.chars().all(|c| c == '_' || c.is_ascii_alphanumeric())
            && !name.starts_with(|c: char| c.is_ascii_digit());
        if !valid {
            return;
        }
        if names_scratch(value, scan) && !scan.scratch_vars.iter().any(|known| known == name) {
            scan.scratch_vars.push(name.to_owned());
        }
    }
}

/// A `cd`, `pushd` or `popd` of the line: remember a move into scratch space.
fn note_directory_change(program: &str, args: &[String], scan: &mut Scan) {
    if program == "popd" {
        return;
    }
    let target = args
        .iter()
        .map(String::as_str)
        .find(|arg| *arg == "-" || !arg.starts_with('-'));
    let scratch = match target {
        // `cd` with no operand goes home, unless `cd $(mktemp -d)` split it.
        None => scan.mktemp_seen,
        Some(target) => {
            names_scratch(target, scan) || (scan.mktemp_seen && target.contains(['$', '`']))
        }
    };
    // Never cleared: a later `cd` to a sandbox path may fail just as well.
    if scratch {
        scan.scratch_cd = true;
    }
}

/// True when a `-C`/`--git-dir`/`--work-tree`/`GIT_DIR` value names a
/// repository by a literal absolute path. A relative path is resolved in the
/// directory a failed `cd` leaves behind, an empty value is ignored by git, and
/// a variable or command substitution (`"$dir"`, `$(mktemp -d)`) may expand to
/// nothing, so none of those pins anything.
pub fn pins_repository(value: &str) -> bool {
    value.starts_with('/') && !value.contains(['$', '`'])
}

/// True when `git <subcommand> <rest>` changes a repository: commits, ref and
/// index writes, working-tree rewrites, configuration, and the like. Reads
/// (`status`, `log`, `diff`, `branch --show-current`, `config --get`) are not.
pub fn mutates_repository(subcommand: &str, rest: &[String]) -> bool {
    let first = rest.first().map(String::as_str);
    // Bare `git branch` / `git tag` list; so do the read flags.
    let lists = |read_flags: &[&str]| {
        rest.is_empty() || rest.iter().any(|arg| read_flags.contains(&arg.as_str()))
    };
    match subcommand {
        "add" | "am" | "apply" | "checkout" | "cherry-pick" | "clean" | "commit" | "merge"
        | "mv" | "pull" | "push" | "rebase" | "reset" | "restore" | "revert" | "rm" | "stash"
        | "switch" | "symbolic-ref" | "update-index" | "update-ref" | "read-tree" | "replace"
        | "filter-branch" => true,
        "branch" => !lists(&[
            "--show-current",
            "--list",
            "-l",
            "-a",
            "-r",
            "-v",
            "-vv",
            "--contains",
            "--no-contains",
            "--merged",
            "--no-merged",
            "--points-at",
        ]),
        "tag" => !lists(&[
            "--list",
            "-l",
            "-n",
            "--contains",
            "--merged",
            "--points-at",
            "-v",
        ]),
        "config" => config_action(rest).action != ConfigAction::Read,
        "remote" => matches!(
            first,
            Some(
                "add"
                    | "remove"
                    | "rm"
                    | "rename"
                    | "set-url"
                    | "set-head"
                    | "set-branches"
                    | "prune"
            )
        ),
        "reflog" => matches!(first, Some("expire" | "delete")),
        "worktree" => !matches!(first, None | Some("list")),
        "submodule" => matches!(
            first,
            Some("add" | "update" | "init" | "deinit" | "sync" | "absorbgitdirs")
        ),
        _ => false,
    }
}

pub fn scan_checkout(rest: &[String], scan: &mut Scan) {
    let split = rest.iter().position(|arg| arg == "--");
    let (before, after) = match split {
        Some(position) => (&rest[..position], Some(&rest[position + 1..])),
        None => (rest, None),
    };
    let mut force = false;
    let mut operands: Vec<&str> = Vec::new();
    for arg in before {
        let arg = arg.as_str();
        if arg == "--force" {
            force = true;
        } else if arg == "--ignore-other-worktrees" {
            // It lets git move a branch another worktree has checked out, the
            // protection that makes a checkout elsewhere safe.
            return scan.refuse(branch_change("checkout"));
        } else if arg == "--detach"
            || arg == "--orphan"
            || arg.starts_with("--orphan=")
            || arg == "--track"
            || arg.starts_with("--track=")
        {
            return scan.branch_change("checkout");
        } else if is_short_cluster(arg) {
            // -b/-B create and switch, -d detaches, -t tracks and switches.
            if arg[1..].contains(['b', 'B', 'd', 't']) {
                return scan.branch_change("checkout");
            }
            force |= arg[1..].contains('f');
        } else if arg == "-" || !arg.starts_with('-') {
            operands.push(arg);
        }
    }
    match after {
        // `git checkout <ref> --` with no path switches to <ref>.
        Some(paths) if paths.is_empty() => {
            if !operands.is_empty() {
                scan.branch_change("checkout");
            }
        }
        Some(paths) => scan.discards.push(Discard::Tracked {
            command: "git checkout",
            pathspecs: paths.to_vec(),
        }),
        None => match operands.as_slice() {
            // Bare `git checkout` only prints status; `-f` resets the tree.
            [] if force => scan.discards.push(Discard::Tracked {
                command: "git checkout",
                pathspecs: Vec::new(),
            }),
            [] => {}
            [only] if matches!(*only, "." | "./" | ":/") => scan.discards.push(Discard::Tracked {
                command: "git checkout",
                pathspecs: vec![(*only).to_owned()],
            }),
            [_] => scan.branch_change("checkout"),
            [_, paths @ ..] => scan.discards.push(Discard::Tracked {
                command: "git checkout",
                pathspecs: paths.iter().map(|path| (*path).to_owned()).collect(),
            }),
        },
    }
}

pub fn scan_restore(rest: &[String], scan: &mut Scan) {
    let (mut staged, mut worktree) = (false, false);
    let mut pathspecs = Vec::new();
    let mut index = 0;
    while let Some(arg) = rest.get(index).map(String::as_str) {
        index += 1;
        match arg {
            "--" => {
                pathspecs.extend(rest[index..].iter().cloned());
                break;
            }
            "-s" | "--source" | "--pathspec-from-file" => index += 1,
            "--staged" => staged = true,
            "--worktree" => worktree = true,
            _ if arg.starts_with("--") => {}
            _ if is_short_cluster(arg) => {
                staged |= arg[1..].contains('S');
                worktree |= arg[1..].contains('W');
            }
            _ => pathspecs.push(arg.to_owned()),
        }
    }
    // `--staged` alone only unstages; any other form rewrites the worktree.
    if !(staged && !worktree) {
        scan.discards.push(Discard::Tracked {
            command: "git restore",
            pathspecs,
        });
    }
}

pub fn scan_clean(rest: &[String], scan: &mut Scan) {
    let (mut force, mut dry_run, mut all_ignored, mut only_ignored) = (false, false, false, false);
    let mut pathspecs = Vec::new();
    let mut index = 0;
    while let Some(arg) = rest.get(index).map(String::as_str) {
        index += 1;
        match arg {
            "--" => {
                pathspecs.extend(rest[index..].iter().cloned());
                break;
            }
            "--force" => force = true,
            "--dry-run" => dry_run = true,
            "-e" | "--exclude" => index += 1,
            _ if arg.starts_with("--") => {}
            _ if is_short_cluster(arg) => {
                for flag in arg[1..].chars() {
                    match flag {
                        'f' => force = true,
                        'n' => dry_run = true,
                        'x' => all_ignored = true,
                        'X' => only_ignored = true,
                        // The rest of a cluster after -e is its pattern.
                        'e' => break,
                        _ => {}
                    }
                }
            }
            _ => pathspecs.push(arg.to_owned()),
        }
    }
    // Without `-f` git refuses by itself; `-n` deletes nothing.
    if force && !dry_run {
        let ignored = match (all_ignored, only_ignored) {
            (_, true) => Ignored::Only,
            (true, false) => Ignored::Also,
            (false, false) => Ignored::No,
        };
        scan.discards
            .push(Discard::Untracked { pathspecs, ignored });
    }
}

pub fn scan_push(rest: &[String], scan: &mut Scan) {
    let mut index = 0;
    while let Some(arg) = rest.get(index).map(String::as_str) {
        index += 1;
        if arg.starts_with("--force") || arg.starts_with('+') {
            return scan.refuse(FORCE_PUSH_REFUSAL.to_owned());
        }
        if arg == "--push-option" || arg == "--receive-pack" || arg == "--exec" || arg == "--repo" {
            index += 1;
        } else if is_short_cluster(arg) {
            for flag in arg[1..].chars() {
                match flag {
                    'f' => return scan.refuse(FORCE_PUSH_REFUSAL.to_owned()),
                    // `-o <option>` takes the next word.
                    'o' => {
                        index += 1;
                        break;
                    }
                    _ => {}
                }
            }
        }
    }
}

pub fn scan_branch(rest: &[String], scan: &mut Scan) {
    let forced = rest.iter().any(|arg| {
        arg == "--force" || (is_short_cluster(arg) && arg[1..].contains(['f', 'D', 'M', 'C']))
    });
    if forced {
        scan.refuse(BRANCH_FORCE_REFUSAL.to_owned());
    }
}

/// What one `git config` invocation does to the configuration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfigAction {
    /// `--get*`, `--list`, `get`, `list`, or a bare key: nothing is written.
    Read,
    /// `--unset`, `--unset-all`, `unset`, `--remove-section`, `remove-section`.
    Unset,
    /// Anything else: a key and a value, `set`, `--add`, `--replace-all`,
    /// `--edit`, `rename-section`.
    Write,
}

/// [`ConfigAction`] with the operands (keys and values) after the options and
/// the action word.
pub struct ConfigInvocation<'a> {
    pub action: ConfigAction,
    pub operands: Vec<&'a str>,
}

/// Parse `git config`'s arguments (everything after `config`) before judging
/// them: an operand is a key or a value, never an action, unless it is the
/// first operand of a subcommand form (`git config set KEY VALUE`). A value
/// that spells `get` or `list` is still a write (#2581). Unknown options fall
/// to `Write`, the safe side.
pub fn config_action(rest: &[String]) -> ConfigInvocation<'_> {
    let mut action = None;
    let mut operands: Vec<&str> = Vec::new();
    let mut index = 0;
    while let Some(arg) = rest.get(index).map(String::as_str) {
        index += 1;
        if arg == "--" {
            operands.extend(rest[index..].iter().map(String::as_str));
            break;
        }
        let option_action = match arg {
            "--list" | "-l" => Some(ConfigAction::Read),
            "--unset" | "--unset-all" | "--remove-section" => Some(ConfigAction::Unset),
            "--add" | "--replace-all" | "--edit" | "-e" | "--rename-section" => {
                Some(ConfigAction::Write)
            }
            _ if arg.starts_with("--get") => Some(ConfigAction::Read),
            _ => None,
        };
        if option_action.is_some() {
            action = action.or(option_action);
        } else if matches!(
            arg,
            "-f" | "--file" | "--blob" | "--default" | "-t" | "--type" | "--comment"
        ) {
            index += 1;
        } else if arg.starts_with("--") {
            // `--global`, `--file=X`, `--type=bool`, `--null`, ...
        } else if is_short_cluster(arg) {
            let flags = &arg[1..];
            if flags.contains('l') {
                action = action.or(Some(ConfigAction::Read));
            } else if flags.contains('e') {
                action = action.or(Some(ConfigAction::Write));
            }
            if flags.ends_with(['f', 't']) {
                index += 1;
            }
        } else {
            operands.push(arg);
        }
    }
    let action = action.unwrap_or_else(|| {
        let subcommand = match operands.first().copied() {
            Some("get" | "list") => {
                operands.remove(0);
                Some(ConfigAction::Read)
            }
            Some("unset" | "remove-section") => {
                operands.remove(0);
                Some(ConfigAction::Unset)
            }
            Some("set" | "rename-section" | "edit") => {
                operands.remove(0);
                Some(ConfigAction::Write)
            }
            _ => None,
        };
        subcommand.unwrap_or(if operands.len() >= 2 {
            ConfigAction::Write
        } else {
            ConfigAction::Read
        })
    });
    ConfigInvocation { action, operands }
}

/// `git config alias.NAME ...` (any scope, `set` form included) defines an
/// alias; reading, listing and unsetting one stays allowed.
pub fn scan_config(rest: &[String], scan: &mut Scan) {
    let invocation = config_action(rest);
    if invocation.action != ConfigAction::Write {
        return;
    }
    // The key is the first operand; `rename-section OLD NEW` names two sections.
    let names = invocation.operands.iter().take(
        if rest
            .iter()
            .any(|arg| arg == "rename-section" || arg == "--rename-section")
        {
            2
        } else {
            1
        },
    );
    if names
        .into_iter()
        .any(|name| names_alias(name) || names_alias(&format!("{name}.")))
    {
        scan.refuse(ALIAS_REFUSAL.to_owned());
    }
}

/// A `name[=value]` config setting (or a bare key) for an alias, or for an
/// include that can load aliases from a file. Config keys are case-insensitive.
pub fn names_alias(setting: &str) -> bool {
    let key = setting.split('=').next().unwrap_or(setting);
    is_alias_key(&key.to_ascii_lowercase())
}

pub fn is_alias_key(lowercase_key: &str) -> bool {
    ["alias.", "include.", "includeif."]
        .iter()
        .any(|prefix| lowercase_key.starts_with(prefix))
}

/// Built-in git commands and the scripts shipped with git. Git runs those before
/// it looks for an alias, so no alias can stand behind these names.
pub const BUILTIN_SUBCOMMANDS: &[&str] = &[
    "add",
    "am",
    "annotate",
    "apply",
    "archive",
    "bisect",
    "blame",
    "branch",
    "bundle",
    "cat-file",
    "check-attr",
    "check-ignore",
    "checkout",
    "checkout-index",
    "cherry",
    "cherry-pick",
    "clean",
    "clone",
    "commit",
    "commit-graph",
    "commit-tree",
    "config",
    "count-objects",
    "credential",
    "describe",
    "diff",
    "diff-files",
    "diff-index",
    "diff-tree",
    "difftool",
    "fast-export",
    "fast-import",
    "fetch",
    "filter-branch",
    "for-each-ref",
    "format-patch",
    "fsck",
    "gc",
    "grep",
    "hash-object",
    "help",
    "init",
    "interpret-trailers",
    "log",
    "ls-files",
    "ls-remote",
    "ls-tree",
    "maintenance",
    "merge",
    "merge-base",
    "merge-file",
    "merge-tree",
    "mergetool",
    "mv",
    "name-rev",
    "notes",
    "pack-refs",
    "prune",
    "pull",
    "push",
    "range-diff",
    "read-tree",
    "rebase",
    "reflog",
    "remote",
    "repack",
    "replace",
    "request-pull",
    "reset",
    "restore",
    "rev-list",
    "rev-parse",
    "revert",
    "rm",
    "send-email",
    "shortlog",
    "show",
    "show-branch",
    "show-ref",
    "sparse-checkout",
    "stage",
    "stash",
    "status",
    "submodule",
    "switch",
    "symbolic-ref",
    "tag",
    "update-index",
    "update-ref",
    "var",
    "verify-commit",
    "verify-tag",
    "version",
    "whatchanged",
    "worktree",
    "write-tree",
];

pub const ALIAS_REFUSAL: &str = "rsi refused this command: a git alias (`git -c alias.NAME=...`, \
`--config-env` for an alias key, `-c include.*`, `GIT_CONFIG_KEY_n=alias.NAME`, \
`git config alias.NAME ...`) can run any command, including `git stash`, `git checkout` and a \
forced push, behind a name rsi cannot read. AGENTS.md hard rules 1, 2 and 9. Run the real git \
command instead of an alias. Nothing was run.";

pub fn alias_in_use(name: &str) -> String {
    format!(
        "rsi refused this command: `git {name}` is a git alias from your git configuration. An \
alias can run any command, including `git stash`, `git checkout` and a forced push, and rsi \
does not follow aliases. AGENTS.md hard rules 1, 2 and 9. Run the real git command instead. \
Nothing was run."
    )
}

pub fn alias_unverifiable(name: &str, why: &str) -> String {
    format!(
        "rsi refused this command: `git {name}` is not a built-in git command, so it may be a \
git alias that runs another command, and rsi cannot check that because {why}. AGENTS.md hard \
rules 1, 2 and 9. Run it from the working directory without relocating (no cd, no -C) and \
without reassigning git's configuration (no GIT_CONFIG_* or HOME), or use the built-in command \
it stands for. Nothing was run."
    )
}

/// A script nested deeper than `MAX_DEPTH` `bash -c`/`eval` levels (#2319).
pub fn depth_refusal() -> String {
    format!(
        "rsi refused this command: it nests shells (`bash -c`, `eval`) more than {MAX_DEPTH} \
levels deep, and rsi does not read scripts that deep, so it cannot check that nothing inside \
breaks AGENTS.md hard rules 1, 2 and 9. Flatten it: run each command as its own tool call, \
without the extra `bash -c` or `eval` layers. Nothing was run."
    )
}

pub const STASH_REFUSAL: &str = "rsi refused this command: `git stash` (every form) is not \
allowed in Harness tools. AGENTS.md hard rule 9: the git stash is shared by every worktree, so \
a stash can hide or pop another agent's work. To set work aside, commit it on your branch, or \
run `scripts/rsi-stash save|list|restore <sha>|drop <sha>` from the shell tool (it keeps stashes \
in a ref namespace owned by this worktree). Nothing was run.";

pub const RESET_HARD_REFUSAL: &str = "rsi refused this command: `git reset --hard` discards \
uncommitted work and moves the sandbox branch. AGENTS.md hard rule 2: work and commit in the \
sandbox on its assigned branch; do not checkout, switch or reset it (child launches pin to its \
commits). Use `git reset -- <path>` to unstage, or `git revert <rev>` to undo a commit. \
To integrate upstream, merge origin/rolling, never rebase; the lander integrates rolling. Nothing was run.";

pub const FORCE_PUSH_REFUSAL: &str = "rsi refused this command: a forced push (`--force`, \
`--force-with-lease`, `-f` or a `+refspec`) can rewrite shared history. AGENTS.md hard rule 1: \
never force; fetch, integrate and push again, because `rolling` only moves by fast-forward. \
Nothing was run.";

pub const BRANCH_FORCE_REFUSAL: &str = "rsi refused this command: a forced branch update, delete or \
rename (`git branch -f`, `-D`, `-M`, `-C`, `--force`) can rewrite the sandbox branch or the \
branch of another agent, and every branch in the shared repository may be some agent's sandbox. \
AGENTS.md hard rules 1 and 2. Use `git branch -d` (merged branches only) for a branch you \
created, or leave the branch alone. Nothing was run.";

/// #2571: a `cd` into a scratch location came before a repository-changing git
/// command that names no repository.
pub fn scratch_cd_refusal(subcommand: &str) -> String {
    format!(
        "rsi refused this command: it changes directory to a scratch or temporary location \
(`cd`, `pushd`, `mktemp`) and then runs `git {subcommand}`, which changes a repository without \
naming one by a literal absolute path. If the directory is missing or the `cd` fails, the shell carries on and the git \
command runs in this RSI-owned sandbox, where it can commit, move HEAD or change shared \
configuration (AGENTS.md hard rule 2). Create and check the directory first, then run \
`git -C <absolute scratch dir> {subcommand} ...` (or set `GIT_DIR` and `GIT_WORK_TREE`) and \
never `cd <dir> && git ...`. A variable or `$(...)` that may expand to nothing does not count as \
a path, and neither does a relative one. In Python fixtures use `scripts/scratch_git.py`. Nothing \
was run."
    )
}

fn rewrites_ancestry(subcommand: &str) -> bool {
    !matches!(subcommand, "checkout" | "switch")
}

fn ancestry_rewrite(subcommand: &str) -> String {
    format!(
        "rsi refused this command: `git {subcommand}` can drop the allocated custody source \
from HEAD's ancestry and strand a verified worker. Preserve the sandbox's source ancestry: \
merge origin/rolling, never rebase; the lander integrates rolling. Use `git revert` to undo \
commits and `git reset -- <paths>` to unstage. Nothing was run."
    )
}

pub fn branch_change(subcommand: &str) -> String {
    if rewrites_ancestry(subcommand) {
        return ancestry_rewrite(subcommand);
    }
    format!(
        "rsi refused this command: `git {subcommand}` would change the branch of this sandbox. \
AGENTS.md hard rule 2: work and commit in the sandbox on its assigned branch; do not checkout, \
switch or reset it (child launches pin to its commits). To read a file from another revision use \
`git show <rev>:<path>`; to restore a path use `git restore <path>` or `git checkout -- <path>` \
(allowed while that path has no uncommitted changes). Nothing was run."
    )
}

/// [`branch_change`] for a worktree resolved from `git -C` (#2573).
pub fn branch_change_at(subcommand: &str, worktree: &Path) -> String {
    if rewrites_ancestry(subcommand) {
        return ancestry_rewrite(subcommand);
    }
    format!(
        "rsi refused this command: `git {subcommand}` would change the branch of this sandbox \
(`cd` or `git -C` resolves to the sandbox worktree {}). AGENTS.md hard rule 2: work and commit in \
the sandbox on its assigned branch; do not checkout, switch or reset it (child launches pin to its \
commits). `git -C <another worktree> checkout --detach <sha>` (or `cd <absolute worktree> && git \
checkout --detach <sha>`) is allowed; to read a file from \
another revision use `git show <rev>:<path>`. Nothing was run.",
        worktree.display()
    )
}

/// `git -C <directory> checkout|switch` could not be resolved to a worktree.
pub fn branch_change_unresolved(subcommand: &str, directory: &[String], why: &str) -> String {
    format!(
        "rsi refused this command: `git {subcommand}` changes HEAD and rsi cannot tell whether \
`git -C {}` (or the `cd` before it) is this sandbox, because {why}. AGENTS.md hard rule 2. Check the directory exists and \
is a git worktree, then retry.  Nothing was run.",
        directory.join(" -C ")
    )
}

pub fn dirty(command: &str, paths: &[String]) -> String {
    let shown: Vec<&str> = paths
        .iter()
        .take(MAX_PATHS_SHOWN)
        .map(String::as_str)
        .collect();
    let more = paths.len().saturating_sub(MAX_PATHS_SHOWN);
    let more = if more == 0 {
        String::new()
    } else {
        format!(" (+{more} more)")
    };
    format!(
        "rsi refused this command: `{command}` would discard uncommitted work in {}{more}. \
AGENTS.md hard rules 8 and 9: uncommitted sandbox work can be reclaimed and cannot be stashed \
here. Commit it first (a WIP commit is fine) or save it with `scripts/rsi-stash save`, then \
retry. Nothing was run.",
        shown.join(", ")
    )
}

pub fn unverifiable(command: &str, why: &str) -> String {
    format!(
        "rsi refused this command: `{command}` discards uncommitted changes and rsi cannot \
check that nothing would be lost, because {why}. AGENTS.md hard rules 8 and 9. Commit your work \
and run the command alone, as the only command of its own tool call, from the working directory \
(no cd, no -C or env -C, nothing before or after it). Nothing was run."
    )
}

/// `git status --porcelain` arguments limited to what `discard` would destroy.
#[must_use]
pub fn status_args(discard: &Discard) -> Vec<String> {
    let mut args = vec!["status".to_owned(), "--porcelain".to_owned()];
    let pathspecs = match discard {
        Discard::Tracked { pathspecs, .. } => {
            args.push("--untracked-files=no".to_owned());
            pathspecs
        }
        Discard::Untracked { pathspecs, ignored } => {
            args.push("--untracked-files=all".to_owned());
            if *ignored != Ignored::No {
                args.push("--ignored".to_owned());
            }
            pathspecs
        }
    };
    if !pathspecs.is_empty() {
        args.push("--".to_owned());
        args.extend(pathspecs.iter().cloned());
    }
    args
}

/// The paths in `git status --porcelain` output that `discard` would destroy.
#[must_use]
pub fn paths_at_risk_from_status(discard: &Discard, stdout: &str) -> Vec<String> {
    let (tracked, untracked, ignored) = match discard {
        Discard::Tracked { .. } => (true, false, false),
        Discard::Untracked { ignored, .. } => {
            (false, *ignored != Ignored::Only, *ignored != Ignored::No)
        }
    };
    stdout
        .lines()
        .filter(|line| line.len() > 3 && line.is_char_boundary(3))
        .filter(|line| match &line[..2] {
            "??" => untracked,
            "!!" => ignored,
            _ => tracked,
        })
        .map(|line| line[3..].to_owned())
        .collect()
}

/// `git config --get-regexp` arguments listing every configured alias name.
pub const ALIAS_PROBE_ARGS: [&str; 4] = ["config", "--name-only", "--get-regexp", "^alias\\."];

/// Lowercase alias names from [`ALIAS_PROBE_ARGS`] output.
#[must_use]
pub fn alias_names_from(stdout: &str) -> Vec<String> {
    stdout
        .lines()
        .filter_map(|line| line.trim().strip_prefix("alias."))
        .map(str::to_ascii_lowercase)
        .collect()
}

/// Probe arguments identifying the worktree `git -C <directory>...` runs in:
/// its absolute git dir and top level, one per line (#2573).
#[must_use]
pub fn worktree_identity_args(directory: &[String]) -> Vec<String> {
    let mut args = Vec::new();
    for value in directory {
        args.push("-C".to_owned());
        args.push(value.clone());
    }
    args.extend(["rev-parse", "--absolute-git-dir", "--show-toplevel"].map(str::to_owned));
    args
}

/// `(git dir, top level)` from [`worktree_identity_args`] output.
fn parse_worktree_identity(stdout: &str) -> Option<(std::path::PathBuf, std::path::PathBuf)> {
    let mut lines = stdout
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty());
    let git_dir = lines.next()?;
    let top_level = lines.next()?;
    Some((git_dir.into(), top_level.into()))
}

/// Settle one [`PendingBranchChange`]: `Some(refusal)` when the worktree it
/// names is this session's sandbox (the worktree the command runs from, or any
/// worktree under the sandbox base), cannot be resolved, or is not a worktree.
/// A detached deploy worktree elsewhere passes: git itself refuses to move a
/// branch another worktree has checked out (#2573).
///
/// `target` and `own` are the stdout of [`worktree_identity_args`] run for the
/// pending directory and for the session's own working directory.
#[must_use]
pub fn branch_change_verdict(
    pending: &PendingBranchChange,
    target: Result<&str, &str>,
    own: Option<&str>,
    sandbox_base: &Path,
) -> Option<String> {
    let unresolved =
        |why: &str| branch_change_unresolved(pending.subcommand, &pending.directory, why);
    let stdout = match target {
        Ok(stdout) => stdout,
        Err(error) => return Some(unresolved(&format!("git could not read it: {error}"))),
    };
    let Some((git_dir, top_level)) = parse_worktree_identity(stdout) else {
        return Some(unresolved("it is not a git worktree"));
    };
    let canonical = |path: &Path| path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    let own_git_dir = own
        .and_then(parse_worktree_identity)
        .map(|(git_dir, _)| canonical(&git_dir));
    let is_own = own_git_dir.is_some_and(|own| own == canonical(&git_dir));
    let under_base = !sandbox_base.as_os_str().is_empty()
        && canonical(&top_level).starts_with(canonical(sandbox_base));
    (is_own || under_base).then(|| branch_change_at(pending.subcommand, &top_level))
}

/// How long one blocking `git` probe may run.
const SYNC_PROBE_TIMEOUT: Duration = Duration::from_secs(10);

/// Run a read-only `git` probe in `cwd`, bounded in time. `Ok((exit, stdout))`.
fn probe_git(args: &[String], cwd: &Path) -> Result<(Option<i32>, String), String> {
    let mut child = Command::new("git")
        .args(args)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| error.to_string())?;
    let deadline = Instant::now() + SYNC_PROBE_TIMEOUT;
    // Drain stdout on a thread so a full pipe cannot stall the child.
    let mut stdout = child.stdout.take();
    let reader = std::thread::spawn(move || {
        let mut text = String::new();
        if let Some(pipe) = stdout.as_mut() {
            let mut bytes = Vec::new();
            let _ = std::io::Read::read_to_end(pipe, &mut bytes);
            text = String::from_utf8_lossy(&bytes).into_owned();
        }
        text
    });
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                return Err("the git probe timed out".to_owned());
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(10)),
            Err(error) => return Err(error.to_string()),
        }
    };
    let text = reader.join().unwrap_or_default();
    Ok((status.code(), text))
}

/// Blocking custody check of a CLI provider shell command that runs in `cwd`
/// (the hook's counterpart of the Harness tools' async settle). `Some(message)`
/// is the refusal the agent is told; nothing was run.
#[must_use]
pub fn check_command_sync(command: &str, cwd: &Path) -> Option<String> {
    check_command_sync_in(command, cwd, &crate::boundary_mail_hook::sandbox_base())
}

/// [`check_command_sync`] with an explicit sandbox base (#2573).
#[must_use]
pub fn check_command_sync_in(command: &str, cwd: &Path, base: &Path) -> Option<String> {
    let mut scan = Scan::default();
    scan_script(command, 0, true, &mut scan);
    if let Some(message) = scan.refusal.take() {
        return Some(message);
    }
    if !scan.pending_branch_changes.is_empty() {
        let own = probe_git(&worktree_identity_args(&[]), cwd)
            .ok()
            .filter(|(code, _)| *code == Some(0))
            .map(|(_, stdout)| stdout);
        for pending in &scan.pending_branch_changes {
            let target = match probe_git(&worktree_identity_args(&pending.directory), cwd) {
                Ok((Some(0), stdout)) => Ok(stdout),
                Ok((code, _)) => Err(format!(
                    "`git rev-parse` exited with {}",
                    code.unwrap_or(-1)
                )),
                Err(error) => Err(error),
            };
            if let Some(message) = branch_change_verdict(
                pending,
                target.as_deref().map_err(String::as_str),
                own.as_deref(),
                base,
            ) {
                return Some(message);
            }
        }
    }
    if let Some(first) = scan.alias_candidates.first() {
        if scan.relocated {
            return Some(alias_unverifiable(
                first,
                "it changes directory or repository first (cd, env -C, git -C, --git-dir, --work-tree or GIT_DIR)",
            ));
        }
        if scan.config_source_overridden {
            return Some(alias_unverifiable(
                first,
                "the line points git at other configuration (GIT_CONFIG_GLOBAL, GIT_CONFIG_SYSTEM, GIT_CONFIG, GIT_CONFIG_NOSYSTEM, GIT_COMMON_DIR, HOME or XDG_CONFIG_HOME), which the alias check does not see",
            ));
        }
        let args = ALIAS_PROBE_ARGS.map(str::to_owned);
        match probe_git(&args, cwd) {
            // Exit 1 is "no such key": no alias is defined.
            Ok((Some(0 | 1), stdout)) => {
                let aliases = alias_names_from(&stdout);
                if let Some(name) = scan
                    .alias_candidates
                    .iter()
                    .find(|name| aliases.contains(&name.to_ascii_lowercase()))
                {
                    return Some(alias_in_use(name));
                }
            }
            Ok((code, _)) => {
                return Some(alias_unverifiable(
                    first,
                    &format!("`git config` exited with {}", code.unwrap_or(-1)),
                ));
            }
            Err(error) => {
                return Some(alias_unverifiable(
                    first,
                    &format!("`git config` failed: {error}"),
                ));
            }
        }
    }
    let first = scan.discards.first()?;
    if scan.relocated {
        return Some(unverifiable(
            first.command(),
            "it changes directory or repository first (cd, env -C, git -C, --git-dir, --work-tree or GIT_DIR)",
        ));
    }
    if scan.crowded {
        return Some(unverifiable(
            first.command(),
            "it shares its line with other commands or a grouping, and the check runs before the shell starts, so an earlier command could change the paths first",
        ));
    }
    for discard in &scan.discards {
        match probe_git(&status_args(discard), cwd) {
            Ok((Some(0), stdout)) => {
                let paths = paths_at_risk_from_status(discard, &stdout);
                if !paths.is_empty() {
                    return Some(dirty(discard.command(), &paths));
                }
            }
            Ok((code, _)) => {
                return Some(unverifiable(
                    discard.command(),
                    &format!("`git status` exited with {}", code.unwrap_or(-1)),
                ));
            }
            Err(error) => {
                return Some(unverifiable(
                    discard.command(),
                    &format!("`git status` failed: {error}"),
                ));
            }
        }
    }
    None
}

#[cfg(test)]
mod scratch_cd_tests {
    use super::*;

    fn refusal(command: &str) -> Option<String> {
        let mut scan = Scan::default();
        scan_script(command, 0, true, &mut scan);
        scan.refusal
    }

    fn scratch_refused(command: &str) -> bool {
        refusal(command).is_some_and(|message| message.contains("scratch or temporary"))
    }

    /// #2582: a child shell's `cd` cannot move the shell that started it.
    #[test]
    fn a_child_shell_cd_does_not_leak_into_the_parent_commit() {
        for command in [
            "bash -c 'cd /tmp; ls'; git commit -m fix",
            "sh -lc 'cd /tmp/f && ls' && git add a.txt && git commit -m fix",
            "(cd /tmp && ls); git commit -m fix",
            "(cd /tmp/f; ls)\ngit add -A\ngit commit -m fix",
            "x=$(cd /tmp/f && pwd); git commit -m fix",
            "echo \"$(cd /tmp/f; pwd)\"; git commit -m fix",
            "echo `cd /tmp/f; pwd`; git commit -m fix",
            "(bash -c 'cd /tmp/f; ls'); git commit -m fix",
        ] {
            assert_eq!(refusal(command), None, "{command:?}");
        }
    }

    /// The child still reads its own cd, and inherits the parent's.
    #[test]
    fn a_cd_stays_dangerous_in_its_own_scope_and_in_children() {
        for command in [
            "(cd /tmp/f; git commit -m x)",
            "bash -c 'cd /tmp/f; git commit -m x'",
            "cd /tmp/f; (git commit -m x)",
            "cd /tmp/f; bash -c 'git commit -m x'",
            "cd /tmp/f; (ls); git commit -m x",
            "cd /tmp/f; bash -c 'ls'; git commit -m x",
            "cd /tmp/f; (cd /home/a; ls); git commit -m x",
            "(cd /tmp/f; ls); cd /tmp/g; git commit -m x",
            "eval 'cd /tmp/f'; git commit -m x",
            "bash -c 'eval \"cd /tmp/f\"; git commit -m x'",
            "cd $(mktemp -d); git commit -m x",
            "cd \"$(mktemp -d)\" && git commit -m x",
            "(cd /tmp/f; (ls); git commit -m x)",
        ] {
            assert!(
                scratch_refused(command),
                "{command:?}: {:?}",
                refusal(command)
            );
        }
    }

    /// #2580: `env -u` and `env -i` remove the pins an assignment gave.
    #[test]
    fn env_unset_and_clear_remove_repository_pins() {
        for command in [
            "cd /tmp/missing; GIT_DIR=/tmp/f/.git GIT_WORK_TREE=/tmp/f env -u GIT_DIR -u GIT_WORK_TREE git commit -m x",
            "cd /tmp/f; GIT_DIR=/tmp/f/.git GIT_WORK_TREE=/tmp/f env --unset GIT_DIR git commit -m x",
            "cd /tmp/f; GIT_DIR=/tmp/f/.git GIT_WORK_TREE=/tmp/f env --unset=GIT_WORK_TREE git commit -m x",
            "cd /tmp/f; GIT_DIR=/tmp/f/.git GIT_WORK_TREE=/tmp/f env -uGIT_DIR git commit -m x",
            "cd /tmp/f; GIT_DIR=/tmp/f/.git GIT_WORK_TREE=/tmp/f env -i git commit -m x",
            "cd /tmp/f; GIT_DIR=/tmp/f/.git GIT_WORK_TREE=/tmp/f env --ignore-environment git commit -m x",
            "cd /tmp/f; env GIT_DIR=/tmp/f/.git GIT_WORK_TREE=/tmp/f env -u GIT_DIR git commit -m x",
            // The last assignment wins.
            "cd /tmp/f; GIT_DIR=/tmp/f/.git GIT_WORK_TREE=/tmp/f GIT_DIR=$d git commit -m x",
            "cd /tmp/f; GIT_DIR=/tmp/f/.git GIT_WORK_TREE=/tmp/f GIT_WORK_TREE= git commit -m x",
        ] {
            assert!(
                scratch_refused(command),
                "{command:?}: {:?}",
                refusal(command)
            );
        }
        for command in [
            "cd /tmp/f; GIT_DIR=/tmp/f/.git GIT_WORK_TREE=/tmp/f git commit -m x",
            "cd /tmp/f; env -u GIT_DIR GIT_DIR=/tmp/f/.git GIT_WORK_TREE=/tmp/f git commit -m x",
            "cd /tmp/f; env -i GIT_DIR=/tmp/f/.git GIT_WORK_TREE=/tmp/f git commit -m x",
            "cd /tmp/f; env -u HOME GIT_DIR=/tmp/f/.git GIT_WORK_TREE=/tmp/f git commit -m x",
            "cd /tmp/f; GIT_DIR=$d GIT_DIR=/tmp/f/.git GIT_WORK_TREE=/tmp/f git commit -m x",
            "cd /tmp/f; GIT_DIR=/tmp/f/.git GIT_WORK_TREE=/tmp/f env -u PATH git commit -m x",
        ] {
            assert_eq!(refusal(command), None, "{command:?}");
        }
    }

    /// #2581: a config value spelled like an action is still a write.
    #[test]
    fn a_config_value_spelled_get_or_list_is_still_a_write() {
        for command in [
            "cd /tmp/missing; git config rsi.fixture list",
            "cd /tmp/missing; git config rsi.fixture get",
            "cd /tmp/missing; git config set rsi.fixture list",
            "cd /tmp/missing; git config --local rsi.fixture list",
            "cd /tmp/missing; git config --replace-all rsi.fixture get",
            "cd /tmp/missing; git config unknown-option-x rsi.fixture list",
            "cd /tmp/missing; git config --unset rsi.fixture",
        ] {
            assert!(scratch_refused(command), "{command:?}");
        }
        for command in [
            "cd /tmp/f; git config --get user.name",
            "cd /tmp/f; git config --list",
            "cd /tmp/f; git config -l",
            "cd /tmp/f; git config --get-regexp 'user.*'",
            "cd /tmp/f; git config list",
            "cd /tmp/f; git config get user.name",
            "cd /tmp/f; git config --global get user.name",
            "cd /tmp/f; git config --file cfg get user.name",
            "cd /tmp/f; git config --show-origin --list",
            "cd /tmp/f; git config user.name",
            "cd /tmp/f; git config --type=bool core.bare",
        ] {
            assert_eq!(refusal(command), None, "{command:?}");
        }
    }

    #[test]
    fn config_alias_writes_are_refused_even_with_read_looking_values() {
        for command in [
            "git config alias.x list",
            "git config set alias.x get",
            "git config --global alias.x stash",
            "git config --file cfg include.path list",
        ] {
            assert_eq!(
                refusal(command).as_deref(),
                Some(ALIAS_REFUSAL),
                "{command:?}"
            );
        }
        for command in [
            "git config --get alias.x",
            "git config alias.x",
            "git config get alias.x",
            "git config --unset alias.x",
            "git config unset alias.x",
            "git config --list",
            "git config user.name alias.fake",
        ] {
            assert_eq!(refusal(command), None, "{command:?}");
        }
    }
}

/// #2573: `git -C <another worktree> checkout --detach` is not a change to
/// the sandbox branch.
#[cfg(test)]
mod other_worktree_tests {
    use super::*;

    fn git(dir: &Path, args: &[&str]) -> String {
        let output = Command::new("git")
            .args(args)
            .current_dir(dir)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .output()
            .expect("run git");
        assert!(
            output.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).trim().to_owned()
    }

    struct Fixture {
        _root: tempfile::TempDir,
        sandbox: std::path::PathBuf,
        deploy: std::path::PathBuf,
        sha: String,
    }

    /// A repository with a sandbox worktree on a branch and a detached deploy
    /// worktree beside it.
    fn fixture() -> Fixture {
        let root = tempfile::tempdir().expect("tempdir");
        let base = root.path().canonicalize().expect("canonical");
        let main = base.join("main");
        std::fs::create_dir(&main).unwrap();
        git(&main, &["init", "-q", "-b", "main"]);
        git(&main, &["config", "gc.auto", "0"]);
        std::fs::write(main.join("a.txt"), "one\n").unwrap();
        git(&main, &["add", "a.txt"]);
        git(
            &main,
            &[
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@test.invalid",
                "commit",
                "-q",
                "-m",
                "init",
            ],
        );
        let sha = git(&main, &["rev-parse", "HEAD"]);
        let sandbox = base.join("sandbox");
        let deploy = base.join("deploy-wt");
        git(
            &main,
            &["worktree", "add", "-q", "-b", "sandbox-branch"]
                .into_iter()
                .chain([sandbox.to_str().unwrap()])
                .collect::<Vec<_>>(),
        );
        git(
            &main,
            &[
                "worktree",
                "add",
                "-q",
                "--detach",
                deploy.to_str().unwrap(),
            ],
        );
        Fixture {
            _root: root,
            sandbox,
            deploy,
            sha,
        }
    }

    fn verdict(command: &str, cwd: &Path) -> Option<String> {
        // The fixture lives under a real sandbox base when run in a sandbox, so
        // name a base that holds none of it: only the worktree identity counts.
        check_command_sync_in(command, cwd, Path::new("/nonexistent-sandbox-base"))
    }

    #[test]
    fn source_ancestry_rewrites_are_refused_only_in_managed_worktrees() {
        let fixture = fixture();
        let base = fixture.sandbox.parent().unwrap().join("managed-base");
        let head = git(&fixture.sandbox, &["rev-parse", "HEAD"]);
        for command in [
            "git rebase origin/rolling",
            "git rebase --onto origin/rolling HEAD~2",
            "git pull --rebase",
            "git -c pull.rebase=true pull",
            "git pull --no-rebase --rebase=merges",
            "git filter-branch -- --all",
            "git reset --soft HEAD~1",
            "git reset --mixed origin/rolling",
            "git commit --amend --no-edit",
            "git update-ref HEAD origin/rolling",
            "git replace HEAD origin/rolling",
            "bash -c 'git rebase origin/rolling'",
        ] {
            for script in [
                command.to_owned(),
                format!(
                    "git -C {} {}",
                    fixture.sandbox.display(),
                    command
                        .strip_prefix("git ")
                        .unwrap_or("rebase origin/rolling")
                ),
            ] {
                let refusal = check_command_sync_in(&script, &fixture.sandbox, &base)
                    .unwrap_or_else(|| panic!("must refuse {script}"));
                assert!(
                    refusal.contains("merge origin/rolling, never rebase"),
                    "{script}: {refusal}"
                );
            }
        }
        for command in [
            "git merge origin/rolling",
            "git pull --no-rebase",
            "git rebase --abort",
            "git reset -- file",
            "git commit -m fix",
        ] {
            assert_eq!(
                check_command_sync_in(command, &fixture.sandbox, &base),
                None,
                "{command}"
            );
        }
        for operation in [
            "rebase origin/rolling",
            "reset --mixed HEAD~1",
            "commit --amend --no-edit",
        ] {
            let script = format!("git -C {} {operation}", fixture.deploy.display());
            assert_eq!(
                check_command_sync_in(&script, &fixture.sandbox, &base),
                None,
                "{script}"
            );
        }
        assert_eq!(git(&fixture.sandbox, &["rev-parse", "HEAD"]), head);
    }

    #[test]
    fn a_sandbox_checkout_is_refused() {
        let fixture = fixture();
        for command in [
            "git checkout --detach HEAD",
            "git checkout main",
            "git switch main",
            "git checkout -b other",
        ] {
            let message = verdict(command, &fixture.sandbox).expect(command);
            assert!(message.contains("hard rule 2"), "{command}: {message}");
        }
    }

    #[test]
    fn a_detached_deploy_worktree_checkout_via_dash_c_is_allowed() {
        let fixture = fixture();
        let deploy = fixture.deploy.to_str().unwrap();
        for command in [
            format!("git -C {deploy} checkout -q --detach {}", fixture.sha),
            format!("git -C {deploy} checkout --detach {}", fixture.sha),
            format!("git -C {deploy} switch --detach {}", fixture.sha),
        ] {
            assert_eq!(verdict(&command, &fixture.sandbox), None, "{command}");
        }
    }

    #[test]
    fn dash_c_into_the_sandbox_itself_is_refused_and_names_it() {
        let fixture = fixture();
        let sandbox = fixture.sandbox.to_str().unwrap();
        let deploy = fixture.deploy.to_str().unwrap();
        for (command, cwd) in [
            (
                format!("git -C {sandbox} checkout --detach {}", fixture.sha),
                &fixture.sandbox,
            ),
            // Another `-C` walks back into the sandbox.
            (
                format!("git -C {deploy} -C ../sandbox switch main"),
                &fixture.sandbox,
            ),
        ] {
            let message = verdict(&command, cwd).expect(&command);
            assert!(message.contains("hard rule 2"), "{command}: {message}");
            assert!(
                message.contains(sandbox),
                "{command} must name the resolved worktree: {message}"
            );
        }
    }

    #[test]
    fn an_unresolvable_or_unnameable_target_stays_refused() {
        let fixture = fixture();
        let missing = fixture.deploy.join("missing");
        for command in [
            format!("git -C {} checkout --detach HEAD", missing.display()),
            "git -C ../deploy-wt checkout --detach HEAD".to_owned(),
            "git -C \"$DEPLOY\" checkout --detach HEAD".to_owned(),
            format!(
                "git -C {} --git-dir=.git checkout --detach HEAD",
                fixture.deploy.display()
            ),
            format!(
                "git -C {} checkout --ignore-other-worktrees sandbox-branch",
                fixture.deploy.display()
            ),
        ] {
            assert!(verdict(&command, &fixture.sandbox).is_some(), "{command}");
        }
    }

    /// #3037: the manager's own `cd <deploy worktree> && git checkout --detach`.
    #[test]
    fn a_detached_deploy_worktree_checkout_after_cd_is_allowed() {
        let fixture = fixture();
        let deploy = fixture.deploy.to_str().unwrap();
        let sha = &fixture.sha;
        for command in [
            format!("cd {deploy} && git checkout -q --detach {sha}"),
            format!("cd {deploy}; git checkout --detach {sha}"),
            format!("cd {deploy}\ngit switch --detach {sha}\n"),
            format!("cd -P -- {deploy} && git checkout --detach {sha}"),
            format!("cd {deploy} && git fetch -q origin && git checkout --detach {sha}"),
            format!("cd {deploy} && bash -c 'git checkout --detach {sha}'"),
            // A relative `-C` after the `cd` stays in the deploy worktree.
            format!("cd {deploy} && git -C . checkout --detach {sha}"),
        ] {
            assert_eq!(verdict(&command, &fixture.sandbox), None, "{command}");
        }
    }

    #[test]
    fn cd_into_the_sandbox_itself_is_still_refused_and_names_it() {
        let fixture = fixture();
        let sandbox = fixture.sandbox.to_str().unwrap();
        let deploy = fixture.deploy.to_str().unwrap();
        let sha = &fixture.sha;
        for command in [
            format!("cd {sandbox} && git checkout --detach {sha}"),
            format!("cd {sandbox}; git switch main"),
            // Walking back into the sandbox with a relative `-C`.
            format!("cd {deploy} && git -C ../sandbox switch main"),
        ] {
            let message = verdict(&command, &fixture.sandbox).expect(&command);
            assert!(message.contains("hard rule 2"), "{command}: {message}");
            assert!(
                message.contains(sandbox),
                "{command} must name the resolved worktree: {message}"
            );
        }
    }

    #[test]
    fn a_cd_that_may_not_have_happened_or_went_elsewhere_stays_refused() {
        let fixture = fixture();
        let deploy = fixture.deploy.to_str().unwrap();
        let missing = fixture.deploy.join("missing");
        let sha = &fixture.sha;
        for command in [
            // Conditional `cd` followed by an unconditional git.
            format!("false && cd {deploy}; git checkout --detach {sha}"),
            format!("cd {deploy} || true; git checkout --detach {sha}"),
            format!("[ -d {deploy} ] && cd {deploy}\ngit checkout --detach {sha}"),
            format!("if true; then cd {deploy}; fi; git checkout --detach {sha}"),
            format!("(cd {deploy}); git checkout --detach {sha}"),
            format!("cd {deploy} & git checkout --detach {sha}"),
            format!("true | cd {deploy}; git checkout --detach {sha}"),
            format!("bash -c 'cd {deploy}'; git checkout --detach {sha}"),
            // A later `cd` leaves the named directory.
            format!("cd {deploy} && cd .. && git checkout --detach {sha}"),
            format!("cd {deploy} && cd sub && git checkout --detach {sha}"),
            // Targets that cannot be named or resolved.
            format!("cd {} && git checkout --detach {sha}", missing.display()),
            format!("cd ../deploy-wt && git checkout --detach {sha}"),
            format!("cd \"$DEPLOY\" && git checkout --detach {sha}"),
            format!("cd ~ && git checkout --detach {sha}"),
            format!("cd - && git checkout --detach {sha}"),
            // Something else decides where git runs.
            format!("cd {deploy} && env -C . git checkout --detach {sha}"),
            format!("cd {deploy} && GIT_DIR=.git git checkout --detach {sha}"),
            format!("export GIT_DIR=/x/.git; cd {deploy}; git checkout --detach {sha}"),
            format!("cd {deploy} && git checkout --ignore-other-worktrees sandbox-branch"),
            // No `cd` at all.
            format!("git checkout --detach {sha}"),
        ] {
            assert!(verdict(&command, &fixture.sandbox).is_some(), "{command}");
        }
    }

    #[test]
    fn straight_lines_are_all_and_then_or_all_sequenced() {
        for script in [
            "cd /a && git status",
            "cd /a; git status",
            "cd /a\ngit status\n",
            "cd /a && echo 'x; y | z' && git status",
            "cd /a && git status # a | comment",
            "cd /a && git status;",
        ] {
            assert!(is_straight_line(script), "{script:?}");
        }
        for script in [
            "a && b; c",
            "a || b",
            "a | b",
            "a &",
            "(a)",
            "{ a; }",
            "a $(b)",
            "a `b`",
            "a \"$(b)\"",
        ] {
            assert!(!is_straight_line(script), "{script:?}");
        }
    }

    #[test]
    fn a_worktree_under_the_sandbox_base_counts_as_a_sandbox() {
        let pending = PendingBranchChange {
            subcommand: "checkout",
            directory: vec!["/base/other-agent".to_owned()],
        };
        let target = "/repo/.git/worktrees/other-agent\n/base/other-agent\n";
        let own = "/repo/.git/worktrees/mine\n/base/mine\n";
        let message = branch_change_verdict(&pending, Ok(target), Some(own), Path::new("/base"))
            .expect("another sandbox is refused");
        assert!(message.contains("/base/other-agent"), "{message}");
        assert_eq!(
            branch_change_verdict(&pending, Ok(target), Some(own), Path::new("/elsewhere")),
            None
        );
    }
}
