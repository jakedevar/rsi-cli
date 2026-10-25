//! Fail on new non-portable shell in scripts/ (BSD-only and GNU-only forms).
//!
//! Scripts under scripts/ run on the Linux hub and on the macOS satellite (bash 3.2,
//! BSD userland). A BSD-only form breaks Linux and a GNU-only form breaks the Mac
//! (#2936, local #322). This guard flags both, and every finding names the portable
//! alternative.
//!
//! Flagged forms
//!   BSD-only: `mktemp -t PREFIX` (no X's), `sed -i ''`, `stat -f`, `date -j`/`-v`
//!   GNU-only: bare `sed -i`, `stat -c`, `date -d`, `readlink -f`, `grep -P`, `xargs -r`,
//!             bash 4+ (`declare -A`, `mapfile`, `${var,,}`, `wait -n`, `declare -n`)
//!
//! Allowed
//!   * code inside an `if`/`case` whose condition probes the OS (`uname`, `$OSTYPE`, a
//!     variable assigned from `uname`, `is_linux`/`is_macos`-style helpers), or on a
//!     line that follows such a probe (`[ "$(uname)" = Linux ] && ...`);
//!   * a BSD form and a GNU form of one tool joined by `||` in one command list (the
//!     portable `stat -c ... || stat -f ...` fallback idiom, also across a line ending
//!     in `||` or `\` followed by `||`); adjacent lines alone are two defects;
//!   * `# portability: linux-only` (exempts GNU-only forms) or
//!     `# portability: macos-only` (exempts BSD-only forms): in the file's leading
//!     comment block for the whole file, otherwise on the line itself or on the
//!     comment line directly above it.
//!
//! Scope: tracked-style shell files under scripts/ (`*.sh`, `*.bash`, and extensionless
//! files with a shell shebang), minus scripts/tests/fixtures/. Heredoc bodies are
//! scanned when they feed a shell (`bash`, `sh`, `ssh`, `cat > x.sh`) and skipped when
//! they feed another language or plain text.
//!
//! Baseline: existing uses are counted per file in tools/portable-shell-baseline.json.
//! The guard fails when a file has more findings than its baseline (new uses; any file,
//! always), when a count is stale (fewer findings than the baseline), and, with
//! --base REF, when the baseline raises any count over REF's baseline. The baseline may
//! only go down; --refresh refuses to raise it.
//!
//! Stale counts are judged only for the files the change touches, so an unrelated
//! change is never blocked because a sibling landing already fixed a use: with
//! --base REF the touched files are the ones `git diff REF...HEAD` (plus the working
//! tree) names, with --touched-from FILE the NUL-separated paths in FILE (the pre-push
//! hook, whose exported tree has no .git), and with explicit paths those paths. A
//! plain full-tree run (no --base, no --touched-from) reports every stale count.
//!
//! Usage: `rsi-repo-guard portable-shell [--base REF] [--touched-from FILE]
//! [--baseline PATH] [--root DIR] [--refresh | --list] [paths...]`.
//! With no paths the whole scripts/ tree is scanned. Exit 0 clean, 1 findings, 2 usage.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::fmt::Write as _;
use std::io::Write;
use std::path::{Path, PathBuf};

use crate::cli::{self, Opt};
use crate::re::{self, Lazy, Re, group};
use crate::{Ctx, py};

pub const BASELINE: &str = "tools/portable-shell-baseline.json";
const SCAN_ROOT: &str = "scripts";
const SKIP_PARTS: &[&str] = &["scripts/tests/fixtures/"];
const REFRESH: &str = "scripts/repo-guard portable-shell --refresh";

/// A rule: key, the side it breaks the other host on, the form, the portable alternative.
pub struct Rule {
    pub key: &'static str,
    pub side: &'static str,
    pub form: &'static str,
    pub message: &'static str,
}

pub const RULES: &[Rule] = &[
    Rule {
        key: "mktemp-t",
        side: "bsd",
        form: "`mktemp -t PREFIX` without X's",
        message: "GNU mktemp needs X's in the template; use `mktemp \"${TMPDIR:-/tmp}/PREFIX.XXXXXX\"`",
    },
    Rule {
        key: "sed-i-empty",
        side: "bsd",
        form: "`sed -i ''`",
        message: "GNU sed reads '' as the script; use `sed -i.bak ... file && rm -f file.bak`, \
                  or write a temp file and `mv` it over the original",
    },
    Rule {
        key: "sed-i-bare",
        side: "gnu",
        form: "bare `sed -i`",
        message: "BSD sed takes the next argument as the backup suffix; use \
                  `sed -i.bak ... file && rm -f file.bak`, or write a temp file and `mv` it",
    },
    Rule {
        key: "stat-f",
        side: "bsd",
        form: "`stat -f FORMAT`",
        message: "GNU `stat -f` prints filesystem status; use `wc -c < file` for size, or \
                  `python3 -c 'import os,sys; print(os.stat(sys.argv[1]).st_mtime)' file`",
    },
    Rule {
        key: "stat-c",
        side: "gnu",
        form: "`stat -c`/`--format`",
        message: "BSD stat has no -c; use `wc -c < file` for size, or \
                  `python3 -c 'import os,sys; print(os.stat(sys.argv[1]).st_mtime)' file`",
    },
    Rule {
        key: "date-j",
        side: "bsd",
        form: "`date -j`/`date -v`",
        message: "GNU date has neither; do date arithmetic with `date +%s` epoch seconds or \
                  `python3 -c 'import datetime ...'`",
    },
    Rule {
        key: "date-d",
        side: "gnu",
        form: "`date -d`/`--date`",
        message: "BSD date has no -d; parse and shift dates with `python3 -c 'import datetime ...'` \
                  or `date +%s` arithmetic",
    },
    Rule {
        key: "readlink-f",
        side: "gnu",
        form: "`readlink -f`/`-e`/`-m`",
        message: "older macOS readlink lacks it; use `(cd \"$(dirname \"$p\")\" && pwd -P)` for a \
                  directory, or `python3 -c 'import os,sys; print(os.path.realpath(sys.argv[1]))' \"$p\"`",
    },
    Rule {
        key: "grep-P",
        side: "gnu",
        form: "`grep -P`",
        message: "BSD grep has no PCRE; use `grep -E`, or `perl -ne`/`python3` for lookarounds",
    },
    Rule {
        key: "xargs-r",
        side: "gnu",
        form: "`xargs -r`",
        message: "BSD xargs rejects it (it already skips empty input); guard with \
                  `[ -n \"$list\" ]` or loop with `while IFS= read -r x; do ...; done`",
    },
    Rule {
        key: "declare-A",
        side: "gnu",
        form: "`declare -A` (bash 4)",
        message: "macOS ships bash 3.2; use parallel indexed arrays, a `case` lookup, or \
                  `key=value` lines read with `cut`/`grep`",
    },
    Rule {
        key: "mapfile",
        side: "gnu",
        form: "`mapfile`/`readarray` (bash 4)",
        message: "macOS ships bash 3.2; use `while IFS= read -r line; do arr+=(\"$line\"); done < <(cmd)`",
    },
    Rule {
        key: "case-mod",
        side: "gnu",
        form: "`${var,,}`/`${var^^}` (bash 4)",
        message: "macOS ships bash 3.2; use `$(printf '%s' \"$var\" | tr '[:upper:]' '[:lower:]')`",
    },
    Rule {
        key: "wait-n",
        side: "gnu",
        form: "`wait -n` (bash 4.3)",
        message: "macOS ships bash 3.2; wait for each pid (`wait \"$pid\"`) or poll with `kill -0`",
    },
    Rule {
        key: "nameref",
        side: "gnu",
        form: "`declare -n`/`local -n` (bash 4.3)",
        message: "macOS ships bash 3.2; pass values through stdout or use `eval` indirection",
    },
];

fn rule(key: &str) -> &'static Rule {
    RULES
        .iter()
        .find(|rule| rule.key == key)
        .unwrap_or(&RULES[0])
}

static MARKER: Lazy = Lazy::new(|| Re::new(r"#\s*portability:\s*(linux|macos)-only\b"));
static ASSIGN: Lazy = Lazy::new(|| Re::new(r"^([A-Za-z_][A-Za-z0-9_]*)\+?="));
const LEADING_KEYWORDS: &[&str] = &[
    "then", "else", "do", "{", "}", "!", "elif", "while", "until",
];
const OPENERS: &[&str] = &["if", "case"];
const CLOSERS: &[&str] = &["fi", "esac"];
const WRAPPERS: &[&str] = &[
    "xargs", "sudo", "env", "command", "exec", "time", "nohup", "timeout", "nice", "ionice",
    "-exec", "-execdir", "-ok", "watch", "builtin", "ssh", "stdbuf",
];
const SHELLISH: &[&str] = &["bash", "sh", "zsh", "dash", "ksh", "eval", "ssh", "su"];
static PROBE: Lazy =
    Lazy::new(|| Re::new(r"uname|OSTYPE|\bis_?(?:mac|macos|darwin|linux|bsd|gnu)\w*"));
static CASE_MOD: Lazy = Lazy::new(|| Re::new(r"\$\{[A-Za-z_][A-Za-z0-9_]*(?:,,?|\^\^?)[^}]*\}"));
static HEREDOC: Lazy = Lazy::new(|| {
    Re::new(r#"(?<!<)<<(?!<)(-?)\s*(?:'([^']+)'|"([^"]+)"|\\?([A-Za-z_][A-Za-z0-9_]*))"#)
});
static SHELL_SHEBANG: Lazy = Lazy::new(|| Re::new(r"^#!.*\b(?:ba|z|da|k)?sh\b"));
static WRITES_SCRIPT: Lazy = Lazy::new(|| Re::new(r">>?\s*\S+\.(?:sh|bash)\b"));
static WHITESPACE: Lazy = Lazy::new(|| Re::new(r"\s"));
const NON_SHELL_SINKS: &[&str] = &[
    "python", "python3", "perl", "ruby", "node", "awk", "sqlite3", "psql", "jq", "cat", "tee",
];
const SHELL_SINKS: &[&str] = &["bash", "sh", "zsh", "dash", "ksh", "ssh"];
const MAX_DEPTH: usize = 3;

/// One shell word: its unquoted text, its source text, and whether it expands.
#[derive(Debug, Clone)]
struct Word {
    text: String,
    expands: bool,
}

/// One simple command: its line, words, source text and offsets.
#[derive(Debug, Clone)]
struct Command {
    line: usize,
    words: Vec<Word>,
    raw: String,
    start: usize,
    end: usize,
}

/// A finding: path, line, rule key, snippet.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Finding {
    pub path: String,
    pub line: usize,
    pub rule: &'static str,
    pub snippet: String,
}

// ---------------------------------------------------------------------- lexer

/// The words and command being collected at one nesting level.
#[derive(Default)]
struct Level {
    words: Vec<Word>,
    word: Option<(usize, String, bool)>,
    first: Option<usize>,
}

impl Level {
    const fn in_word(&self) -> bool {
        self.word.is_some()
    }

    fn begin(&mut self, pos: usize) {
        if self.word.is_none() {
            self.word = Some((pos, String::new(), false));
        }
        if self.first.is_none() {
            self.first = Some(pos);
        }
    }

    fn add(&mut self, text: &str, expands: bool) {
        if let Some((_, parts, word_expands)) = &mut self.word {
            parts.push_str(text);
            *word_expands = *word_expands || expands;
        }
    }
}

/// A forgiving shell lexer: splits text into simple commands, quote and `$(...)` aware.
struct Parser<'t> {
    text: &'t str,
    commands: Vec<Command>,
    starts: Vec<usize>,
}

impl<'t> Parser<'t> {
    fn new(text: &'t str) -> Self {
        let mut starts = vec![0];
        starts.extend(text.match_indices('\n').map(|(at, _)| at + 1));
        Self {
            text,
            commands: Vec::new(),
            starts,
        }
    }

    fn line(&self, offset: usize) -> usize {
        self.starts.partition_point(|start| *start <= offset)
    }

    /// `text[a:b]` with Python's forgiving bounds.
    fn slice(&self, a: usize, b: usize) -> &'t str {
        let n = self.text.len();
        let (a, b) = (a.min(n), b.min(n));
        if a >= b { "" } else { &self.text[a..b] }
    }

    fn char_at(&self, i: usize) -> Option<char> {
        self.text.get(i..).and_then(|rest| rest.chars().next())
    }

    /// The offset after the character at `i` (Python's `i + 1`).
    fn after(&self, i: usize) -> usize {
        self.char_at(i).map_or(i + 1, |c| i + c.len_utf8())
    }

    fn run(mut self) -> Vec<Command> {
        self.parse(0, None);
        self.commands
    }

    fn end_word(&self, level: &mut Level, pos: usize) {
        if let Some((start, parts, expands)) = level.word.take() {
            let _ = self.slice(start, pos);
            level.words.push(Word {
                text: parts,
                expands,
            });
        }
    }

    fn end_cmd(&mut self, level: &mut Level, pos: usize) {
        self.end_word(level, pos);
        if level.words.is_empty() {
            return;
        }
        let first = level.first.unwrap_or(pos);
        let words = std::mem::take(&mut level.words);
        let raw = self.slice(first, pos).to_string();
        self.commands.push(Command {
            line: self.line(first),
            words,
            raw,
            start: first,
            end: pos,
        });
        level.first = None;
    }

    fn parse(&mut self, start: usize, closer: Option<char>) -> usize {
        let mut level = Level::default();
        let n = self.text.len();
        let mut i = start;
        while i < n {
            let Some(c) = self.char_at(i) else { break };
            if Some(c) == closer {
                break;
            }
            i = self.step(&mut level, i, c);
        }
        self.end_cmd(&mut level, i);
        (i + 1).min(n)
    }

    fn step(&mut self, level: &mut Level, i: usize, c: char) -> usize {
        match c {
            ' ' | '\t' | '\r' => {
                self.end_word(level, i);
                i + 1
            }
            '\n' | ';' | '&' | '|' | '(' | ')' => {
                self.end_cmd(level, i);
                i + 1
            }
            '\\' => self.backslash(level, i),
            '#' if !level.in_word() => self.text[i..]
                .find('\n')
                .map_or(self.text.len(), |at| i + at),
            '\'' => self.single(level, i),
            '"' => self.double(level, i),
            '$' => self.dollar(level, i),
            '`' => self.backtick(level, i),
            _ => {
                level.begin(i);
                let mut buf = [0; 4];
                level.add(c.encode_utf8(&mut buf), true);
                i + c.len_utf8()
            }
        }
    }

    fn backslash(&mut self, level: &mut Level, i: usize) -> usize {
        let following = self.char_at(i + 1);
        if following == Some('\n') {
            return i + 2;
        }
        level.begin(i);
        let text = following.map(String::from).unwrap_or_default();
        level.add(&text, true);
        i + 1 + following.map_or(1, char::len_utf8)
    }

    fn single(&mut self, level: &mut Level, i: usize) -> usize {
        let end = self
            .text
            .get(i + 1..)
            .and_then(|rest| rest.find('\''))
            .map_or(self.text.len(), |at| i + 1 + at);
        level.begin(i);
        let body = self.slice(i + 1, end);
        level.add(body, false);
        end + 1
    }

    fn double(&mut self, level: &mut Level, i: usize) -> usize {
        level.begin(i);
        let n = self.text.len();
        let mut j = i + 1;
        while j < n && self.char_at(j) != Some('"') {
            j = self.double_char(level, j);
        }
        level.add("", true);
        j + 1
    }

    fn double_char(&mut self, level: &mut Level, j: usize) -> usize {
        match self.char_at(j) {
            Some('\\') => {
                let following = self.char_at(j + 1);
                let text = following.map(String::from).unwrap_or_default();
                level.add(&text, true);
                j + 1 + following.map_or(1, char::len_utf8)
            }
            Some('$') => self.dollar(level, j),
            Some('`') => self.backtick(level, j),
            Some(c) => {
                let mut buf = [0; 4];
                level.add(c.encode_utf8(&mut buf), true);
                j + c.len_utf8()
            }
            None => j + 1,
        }
    }

    fn dollar(&mut self, level: &mut Level, i: usize) -> usize {
        level.begin(i);
        let rest = self.text.get(i..).unwrap_or("");
        if rest.starts_with("$((") {
            let end = self.match_pair(i + 1, '(', ')');
            let body = self.slice(i, end);
            level.add(body, true);
            return end;
        }
        if rest.starts_with("$(") {
            let end = self.parse(i + 2, Some(')'));
            let body = self.slice(i, end);
            level.add(body, true);
            return end;
        }
        if rest.starts_with("${") {
            let end = self.match_pair(i + 1, '{', '}');
            self.substitutions_within(i, end);
            let body = self.slice(i, end);
            level.add(body, true);
            return end;
        }
        level.add("$", true);
        i + 1
    }

    /// `text.find(sub, start, end)`.
    fn find_in(&self, sub: &str, start: usize, end: usize) -> Option<usize> {
        let end = end.min(self.text.len());
        if start > end {
            return None;
        }
        self.text.get(start..end)?.find(sub).map(|at| start + at)
    }

    /// Parse the `$(...)` inside a `${...}` so a default value's command is seen.
    fn substitutions_within(&mut self, start: usize, end: usize) {
        let mut at = self.find_in("$(", start + 2, end);
        while let Some(found) = at {
            if found >= end {
                break;
            }
            let resume = self.parse(found + 2, Some(')'));
            at = self.find_in("$(", resume, end);
        }
    }

    fn backtick(&mut self, level: &mut Level, i: usize) -> usize {
        level.begin(i);
        let end = self.parse(i + 1, Some('`'));
        let body = self.slice(i, end);
        level.add(body, true);
        end
    }

    fn match_pair(&self, i: usize, opener: char, closer: char) -> usize {
        let mut depth: i64 = 0;
        let n = self.text.len();
        let mut j = i;
        while j < n {
            let Some(c) = self.char_at(j) else { break };
            depth += i64::from(c == opener) - i64::from(c == closer);
            j = if c == '\\' {
                self.after(j + 1).max(j + 2)
            } else {
                j + c.len_utf8()
            };
            if depth == 0 {
                return j;
            }
        }
        n
    }
}

fn parse_commands(text: &str) -> Vec<Command> {
    Parser::new(text).run()
}

// ---------------------------------------------------------- heredocs/markers

/// True when the heredoc opened at `start` feeds a shell (or writes a shell script).
fn should_scan_heredoc(line: &str, start: usize) -> bool {
    if WRITES_SCRIPT.is_match(line) {
        return true;
    }
    let names: HashSet<String> = parse_commands(&line[..start])
        .iter()
        .flat_map(|command| {
            command
                .words
                .iter()
                .map(|word| py::basename(&word.text).to_string())
        })
        .collect();
    if NON_SHELL_SINKS.iter().any(|sink| names.contains(*sink)) {
        return false;
    }
    SHELL_SINKS.iter().any(|sink| names.contains(*sink))
}

/// (delimiter, strips tabs, scan) for each heredoc a line opens, in order.
fn heredocs_opened(line: &str) -> Vec<(String, bool, bool)> {
    if py::lstrip(line).starts_with('#') {
        return Vec::new();
    }
    HEREDOC
        .find_all(line)
        .iter()
        .map(|caps| {
            let delim = [2, 3, 4]
                .iter()
                .map(|g| group(caps, *g))
                .find(|d| !d.is_empty())
                .unwrap_or("");
            (
                delim.to_string(),
                !group(caps, 1).is_empty(),
                should_scan_heredoc(line, re::start(caps)),
            )
        })
        .collect()
}

/// The text with heredoc bodies that do not feed a shell blanked (line numbers kept).
fn strip_heredocs(text: &str) -> String {
    let mut out: Vec<&str> = Vec::new();
    let mut pending: Vec<(String, bool, bool)> = Vec::new();
    for line in text.split('\n') {
        if pending.is_empty() {
            out.push(line);
            pending = heredocs_opened(line);
            continue;
        }
        let (delim, tabs, scan) = &pending[0];
        let candidate = if *tabs {
            line.trim_start_matches('\t')
        } else {
            line
        };
        if candidate.trim_end_matches('\r') == delim {
            pending.remove(0);
            out.push("");
            continue;
        }
        out.push(if *scan { line } else { "" });
    }
    out.join("\n")
}

/// (file kinds, {line: kinds}) from `# portability: linux-only|macos-only` markers.
fn collect_markers(text: &str) -> (BTreeSet<String>, HashMap<usize, BTreeSet<String>>) {
    let mut file_kinds = BTreeSet::new();
    let mut per_line = HashMap::new();
    let mut pending: BTreeSet<String> = BTreeSet::new();
    let mut header = true;
    for (index, line) in text.split('\n').enumerate() {
        let stripped = py::strip(line);
        let kinds: BTreeSet<String> = MARKER
            .search(line)
            .map(|caps| group(&caps, 1).to_string())
            .into_iter()
            .collect();
        if stripped.is_empty() {
            continue;
        }
        if stripped.starts_with('#') {
            if header {
                file_kinds.extend(kinds);
            } else {
                pending.extend(kinds);
            }
            continue;
        }
        header = false;
        let applies: BTreeSet<String> = kinds.union(&pending).cloned().collect();
        pending.clear();
        if !applies.is_empty() {
            per_line.insert(index + 1, applies);
        }
    }
    (file_kinds, per_line)
}

// --------------------------------------------------------------- detection

/// `-[A-Za-z]*[letters][A-Za-z]*` as a full match.
fn option_cluster(arg: &str, letters: &str) -> bool {
    let Some(rest) = arg.strip_prefix('-') else {
        return false;
    };
    !rest.is_empty()
        && rest.chars().all(|c| c.is_ascii_alphabetic())
        && rest.chars().any(|c| letters.contains(c))
}

fn hits_mktemp(args: &[&str]) -> Vec<&'static str> {
    let has_template = args
        .iter()
        .any(|a| !a.starts_with('-') && a.contains("XXX"));
    let uses_t = args.iter().any(|a| option_cluster(a, "t"));
    if uses_t && !has_template {
        vec!["mktemp-t"]
    } else {
        Vec::new()
    }
}

fn hits_sed(args: &[&str]) -> Vec<&'static str> {
    let mut hits = Vec::new();
    for (index, arg) in args.iter().enumerate() {
        if *arg == "--in-place" {
            hits.push("sed-i-bare");
        } else if *arg == "-i" {
            hits.push(if args.get(index + 1) == Some(&"") {
                "sed-i-empty"
            } else {
                "sed-i-bare"
            });
        }
    }
    hits
}

static FORMAT_OPTION: Lazy = Lazy::new(|| Re::new(r"^--(format|printf)\b"));

fn hits_stat(args: &[&str]) -> Vec<&'static str> {
    let gnu = args
        .iter()
        .any(|a| *a == "-c" || a.starts_with("-c%") || FORMAT_OPTION.is_match(a));
    let bsd = args.iter().any(|a| {
        *a == "-f" || a.starts_with("-f%") || a.starts_with("-f\"") || a.starts_with("-f'")
    });
    if gnu {
        vec!["stat-c"]
    } else if bsd {
        vec!["stat-f"]
    } else {
        Vec::new()
    }
}

static DATE_J: Lazy = Lazy::new(|| Re::new(r"^(?:-j[A-Za-z]*|-v[+-]?\d\w*)$"));
static DATE_D: Lazy = Lazy::new(|| Re::new(r"^(?:-d.*|--date(=.*)?)$"));

fn hits_date(args: &[&str]) -> Vec<&'static str> {
    let mut hits = Vec::new();
    if args.iter().any(|a| DATE_J.is_match(a)) {
        hits.push("date-j");
    }
    if args.iter().any(|a| DATE_D.is_match(a)) {
        hits.push("date-d");
    }
    hits
}

fn hits_readlink(args: &[&str]) -> Vec<&'static str> {
    if args
        .iter()
        .any(|a| option_cluster(a, "fem") || a.starts_with("--canonicalize"))
    {
        vec!["readlink-f"]
    } else {
        Vec::new()
    }
}

fn hits_grep(args: &[&str]) -> Vec<&'static str> {
    if args
        .iter()
        .any(|a| option_cluster(a, "P") || *a == "--perl-regexp")
    {
        vec!["grep-P"]
    } else {
        Vec::new()
    }
}

const XARGS_VALUE_OPTIONS: &[&str] = &[
    "-n", "-I", "-P", "-L", "-s", "-E", "-d", "-a", "-J", "-R", "-S",
];

fn hits_xargs(args: &[&str]) -> Vec<&'static str> {
    let mut skip = false;
    for arg in args {
        if skip {
            skip = false;
            continue;
        }
        if !arg.starts_with('-') {
            return Vec::new();
        }
        let cluster = arg.len() > 1
            && arg[1..].chars().all(|c| c.is_ascii_alphanumeric())
            && arg[1..].contains('r');
        if *arg == "--no-run-if-empty" || cluster {
            return vec!["xargs-r"];
        }
        skip = XARGS_VALUE_OPTIONS.contains(arg);
    }
    Vec::new()
}

fn tool_rule(name: &str) -> Option<fn(&[&str]) -> Vec<&'static str>> {
    Some(match name {
        "mktemp" => hits_mktemp,
        "sed" => hits_sed,
        "stat" => hits_stat,
        "date" => hits_date,
        "readlink" => hits_readlink,
        "grep" => hits_grep,
        "xargs" => hits_xargs,
        _ => return None,
    })
}

fn tool_hits(words: &[Word]) -> Vec<&'static str> {
    let names: Vec<&str> = words.iter().map(|w| py::basename(&w.text)).collect();
    let mut hits = Vec::new();
    for (index, name) in names.iter().enumerate() {
        let Some(rule) = tool_rule(name) else {
            continue;
        };
        if index != 0 && !names[..index].iter().any(|n| WRAPPERS.contains(n)) {
            continue;
        }
        let args: Vec<&str> = words[index + 1..].iter().map(|w| w.text.as_str()).collect();
        hits.extend(rule(&args));
    }
    hits
}

fn bash4_hits(words: &[Word]) -> Vec<&'static str> {
    let name = py::basename(&words[0].text);
    let args: Vec<&str> = words[1..].iter().map(|w| w.text.as_str()).collect();
    let mut hits = Vec::new();
    if words
        .iter()
        .any(|w| w.expands && CASE_MOD.is_match(&w.text))
    {
        hits.push("case-mod");
    }
    if ["declare", "local", "typeset", "readonly"].contains(&name) {
        if args.iter().any(|a| option_cluster(a, "A")) {
            hits.push("declare-A");
        }
        if args.iter().any(|a| option_cluster(a, "n")) {
            hits.push("nameref");
        }
    }
    if name == "mapfile" || name == "readarray" {
        hits.push("mapfile");
    }
    if name == "wait" && args.iter().any(|a| option_cluster(a, "n")) {
        hits.push("wait-n");
    }
    hits
}

fn strip_assignments(words: &[Word]) -> &[Word] {
    let mut index = 0;
    while index + 1 < words.len() && ASSIGN.is_match(&words[index].text) {
        index += 1;
    }
    &words[index..]
}

fn command_hits(words: &[Word], depth: usize) -> Vec<&'static str> {
    let words = strip_assignments(words);
    if words.is_empty() {
        return Vec::new();
    }
    let mut hits = tool_hits(words);
    hits.extend(bash4_hits(words));
    if depth >= MAX_DEPTH || !SHELLISH.contains(&py::basename(&words[0].text)) {
        return hits;
    }
    for word in &words[1..] {
        hits.extend(nested_hits(word, depth));
    }
    hits
}

/// Hits in a quoted command string handed to `bash -c`, `ssh host '...'`, `eval`.
fn nested_hits(word: &Word, depth: usize) -> Vec<&'static str> {
    if !WHITESPACE.is_match(&word.text) {
        return Vec::new();
    }
    parse_commands(&word.text)
        .iter()
        .flat_map(|command| command_hits(&command.words, depth + 1))
        .collect()
}

/// (rest, opened, closed): leading shell keywords stripped from a command.
fn split_head(words: &[Word]) -> (&[Word], usize, usize) {
    let (mut opened, mut closed, mut index) = (0, 0, 0);
    while index < words.len() {
        let text = words[index].text.as_str();
        opened += usize::from(OPENERS.contains(&text));
        closed += usize::from(CLOSERS.contains(&text));
        if !LEADING_KEYWORDS.contains(&text) && !OPENERS.contains(&text) && !CLOSERS.contains(&text)
        {
            break;
        }
        index += 1;
    }
    (&words[index..], opened, closed)
}

type FindingKey = (usize, &'static str, String);

/// Walks commands in order, tracking which are under an OS probe.
struct State {
    file_kinds: BTreeSet<String>,
    line_kinds: HashMap<usize, BTreeSet<String>>,
    stack: Vec<bool>,
    probe_vars: BTreeSet<String>,
    probe_lines: HashSet<usize>,
    findings: Vec<FindingKey>,
    spans: HashMap<FindingKey, (usize, usize)>,
}

impl State {
    fn is_probe(&self, raw: &str) -> bool {
        PROBE.is_match(raw)
            || self
                .probe_vars
                .iter()
                .any(|var| Re::new(&format!(r"\$\{{?{}\b", re::escape(var))).is_match(raw))
    }

    fn track_assignment(&mut self, words: &[Word]) {
        if let Some(caps) = ASSIGN.search(&words[0].text)
            && PROBE.is_match(&words[0].text)
        {
            self.probe_vars.insert(group(&caps, 1).to_string());
        }
    }

    fn exempt(&self, key: &str, line: usize) -> bool {
        if self.stack.iter().any(|probe| *probe) || self.probe_lines.contains(&line) {
            return true;
        }
        let side = rule(key).side;
        self.file_kinds
            .iter()
            .chain(self.line_kinds.get(&line).into_iter().flatten())
            .any(|kind| {
                let exempts = if kind == "linux" { "gnu" } else { "bsd" };
                exempts == side
            })
    }

    fn visit(&mut self, command: &Command) {
        let (rest, opened, closed) = split_head(&command.words);
        let keep = self.stack.len().saturating_sub(closed);
        self.stack.truncate(keep);
        let probe = self.is_probe(&command.raw);
        if probe {
            self.probe_lines.insert(command.line);
        }
        self.record(command, rest);
        self.stack.extend(std::iter::repeat_n(probe, opened));
        if !rest.is_empty() {
            self.track_assignment(rest);
        }
    }

    fn record(&mut self, command: &Command, rest: &[Word]) {
        if rest.is_empty() {
            return;
        }
        for key in command_hits(rest, 0) {
            if self.exempt(key, command.line) {
                continue;
            }
            let first_line = command.raw.split('\n').next().unwrap_or("");
            let snippet: String = py::strip(first_line).chars().take(80).collect();
            let finding = (command.line, key, snippet);
            self.spans
                .insert(finding.clone(), (command.start, command.end));
            self.findings.push(finding);
        }
    }
}

// A BSD form and a GNU form of one tool joined by `||` are the portable
// `stat -c ... || stat -f ...` fallback idiom, not two defects. Proximity alone is
// not a fallback: two independent statements each fail on one platform.
fn family(key: &str) -> Option<&'static str> {
    match key {
        "stat-c" | "stat-f" => Some("stat"),
        "sed-i-bare" | "sed-i-empty" => Some("sed"),
        "date-j" | "date-d" => Some("date"),
        _ => None,
    }
}

static COMMENT: Lazy = Lazy::new(|| Re::new(r"(^|\s)#[^\n]*"));
static CONTINUATION: Lazy = Lazy::new(|| Re::new(r"\\\n|\|\|\s*\n"));
static SEPARATOR: Lazy = Lazy::new(|| Re::new(r";|&&|\n|(?<![|&])&(?![&>])"));

/// True when only `||` (and its continuation) joins the commands ending/starting at the spans.
fn or_linked(text: &str, first: (usize, usize), second: (usize, usize)) -> bool {
    let (earlier, later) = if first <= second {
        (first, second)
    } else {
        (second, first)
    };
    let (end, start) = (earlier.1.min(text.len()), later.0.min(text.len()));
    let between = if end < start { &text[end..start] } else { "" };
    let between = COMMENT.sub_with(between, &|caps| group(caps, 1).to_string());
    if !between.contains("||") {
        return false;
    }
    let flat = CONTINUATION.sub(&between, " ");
    !SEPARATOR.is_match(&flat)
}

fn drop_fallback_pairs(
    found: &[FindingKey],
    spans: &HashMap<FindingKey, (usize, usize)>,
    text: &str,
) -> Vec<FindingKey> {
    found
        .iter()
        .filter(|item| {
            let Some(fam) = family(item.1) else {
                return true;
            };
            !found.iter().any(|other| {
                family(other.1) == Some(fam)
                    && rule(other.1).side != rule(item.1).side
                    && or_linked(text, spans[*item], spans[other])
            })
        })
        .cloned()
        .collect()
}

/// Findings (in line order) for one shell file's text.
#[must_use]
pub fn scan_text(text: &str, path: &str) -> Vec<Finding> {
    let (file_kinds, line_kinds) = collect_markers(text);
    let mut state = State {
        file_kinds,
        line_kinds,
        stack: Vec::new(),
        probe_vars: BTreeSet::new(),
        probe_lines: HashSet::new(),
        findings: Vec::new(),
        spans: HashMap::new(),
    };
    let text = strip_heredocs(text);
    for command in parse_commands(&text) {
        state.visit(&command);
    }
    let mut found = drop_fallback_pairs(&state.findings, &state.spans, &text);
    found.sort();
    found
        .into_iter()
        .map(|(line, rule, snippet)| Finding {
            path: path.to_string(),
            line,
            rule,
            snippet,
        })
        .collect()
}

// ---------------------------------------------------------------- the tree

fn is_shell_file(root: &Path, rel: &str) -> bool {
    if SKIP_PARTS.iter().any(|part| rel.starts_with(part)) {
        return false;
    }
    let path = root.join(rel);
    if rel.ends_with(".sh") || rel.ends_with(".bash") {
        return path.is_file();
    }
    if !py::splitext_ext(rel).is_empty() || !path.is_file() {
        return false;
    }
    let Ok(file) = std::fs::File::open(&path) else {
        return false;
    };
    let mut head = Vec::new();
    let _ = std::io::Read::read_to_end(&mut std::io::Read::take(file, 200), &mut head);
    if let Some(newline) = head.iter().position(|b| *b == b'\n') {
        head.truncate(newline + 1);
    }
    SHELL_SHEBANG.is_match(&String::from_utf8_lossy(&head))
}

fn walk_scripts(root: &Path) -> Vec<String> {
    fn walk(dir: &Path, root: &Path, found: &mut Vec<String>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let is_link = entry.file_type().is_ok_and(|kind| kind.is_symlink());
            if path.is_dir() {
                if !is_link {
                    walk(&path, root, found);
                }
            } else {
                found.push(py::as_posix(path.strip_prefix(root).unwrap_or(&path)));
            }
        }
    }
    let mut found = Vec::new();
    walk(&root.join(SCAN_ROOT), root, &mut found);
    found.sort();
    found
}

fn relative_arg(root: &Path, arg: &str) -> String {
    let path = Path::new(arg);
    if path.is_absolute() {
        return path
            .strip_prefix(root)
            .map(py::as_posix)
            .unwrap_or_default();
    }
    py::path_str(&py::as_posix(path))
}

fn candidate_files(root: &Path, paths: &[String]) -> Vec<String> {
    let names: Vec<String> = if paths.is_empty() {
        walk_scripts(root)
    } else {
        paths
            .iter()
            .map(|p| relative_arg(root, p))
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect()
    };
    names
        .into_iter()
        .filter(|n| n.starts_with("scripts/") && is_shell_file(root, n))
        .collect()
}

fn scan_files(root: &Path, rels: &[String]) -> Result<BTreeMap<String, Vec<Finding>>, String> {
    let mut findings = BTreeMap::new();
    for rel in rels {
        let text =
            py::read_text_lossy(&root.join(rel)).map_err(|error| format!("{rel}: {error}"))?;
        findings.insert(rel.clone(), scan_text(&text, rel));
    }
    Ok(findings)
}

// ------------------------------------------------------------ the baseline

fn load_counts(text: &str) -> Result<BTreeMap<String, i64>, String> {
    let data: serde_json::Value =
        serde_json::from_str(text).map_err(|error| format!("baseline: {error}"))?;
    let Some(object) = data.as_object() else {
        return Ok(BTreeMap::new());
    };
    let files = match object.get("files") {
        None => return Ok(BTreeMap::new()),
        Some(serde_json::Value::Object(files)) => files,
        Some(other) => return Err(format!("baseline: \"files\" is not an object: {other}")),
    };
    let mut counts = BTreeMap::new();
    for (key, value) in files {
        #[allow(clippy::cast_possible_truncation)]
        let count = match value {
            serde_json::Value::Number(n) => {
                n.as_i64().or_else(|| n.as_f64().map(|f| f.trunc() as i64))
            }
            serde_json::Value::Bool(b) => Some(i64::from(*b)),
            serde_json::Value::String(s) => py::strip(s).parse().ok(),
            _ => None,
        };
        let count =
            count.ok_or_else(|| format!("baseline: count for {key} is not an integer: {value}"))?;
        counts.insert(key.clone(), count);
    }
    Ok(counts)
}

/// The committed counts, or `None` when the file does not exist.
fn read_baseline(path: &Path) -> Result<Option<BTreeMap<String, i64>>, String> {
    if !path.is_file() {
        return Ok(None);
    }
    let text = py::read_text(path).map_err(|error| error.to_string())?;
    load_counts(&text).map(Some)
}

/// The counts at git ref `reference`, or `None` when the ref or the file is not readable.
fn read_base_baseline(
    ctx: &Ctx,
    root: &Path,
    reference: &str,
    baseline_rel: &str,
) -> Result<Option<BTreeMap<String, i64>>, String> {
    let done = ctx
        .git()
        .arg("-C")
        .arg(root)
        .args(["show", &format!("{reference}:{baseline_rel}")])
        .output()
        .map_err(|error| format!("git show: {error}"))?;
    if !done.status.success() {
        return Ok(None);
    }
    let text = String::from_utf8(done.stdout)
        .map_err(|error| format!("{reference}:{baseline_rel}: {error}"))?;
    load_counts(&py::universal_newlines(&text)).map(Some)
}

/// The NUL-separated paths `git diff --name-only` prints, or `None` when git fails.
fn git_names(ctx: &Ctx, root: &Path, args: &[&str]) -> Option<BTreeSet<String>> {
    let done = ctx
        .git()
        .arg("-C")
        .arg(root)
        .args(["diff", "--name-only", "--no-renames", "-z"])
        .args(args)
        .output()
        .ok()?;
    if !done.status.success() {
        return None;
    }
    Some(
        done.stdout
            .split(|b| *b == 0)
            .filter(|name| !name.is_empty())
            .map(|name| String::from_utf8_lossy(name).into_owned())
            .collect(),
    )
}

/// Files changed on this branch since it left `reference` (and in the working tree), else `None`.
fn changed_since(ctx: &Ctx, root: &Path, reference: &str) -> Option<BTreeSet<String>> {
    let committed = git_names(ctx, root, &[&format!("{reference}...HEAD")])
        .or_else(|| git_names(ctx, root, &[reference, "HEAD"]));
    let pending = git_names(ctx, root, &["HEAD"]);
    Some(committed?.union(&pending?).cloned().collect())
}

struct Args {
    paths: Vec<String>,
    root: PathBuf,
    baseline: String,
    base: String,
    touched_from: String,
    refresh: bool,
    list: bool,
}

/// The files whose stale counts are judged, or `None` to judge every count.
fn touched_files(ctx: &Ctx, root: &Path, args: &Args) -> Result<Option<BTreeSet<String>>, String> {
    if !args.touched_from.is_empty() {
        let path = ctx.path(&args.touched_from);
        let data = std::fs::read(&path).map_err(|error| format!("{}: {error}", path.display()))?;
        return Ok(Some(
            data.split(|b| *b == 0)
                .filter(|name| !name.is_empty())
                .map(|name| String::from_utf8_lossy(name).into_owned())
                .collect(),
        ));
    }
    if !args.paths.is_empty() {
        return Ok((!args.base.is_empty())
            .then(|| args.paths.iter().map(|p| relative_arg(root, p)).collect()));
    }
    if !args.base.is_empty() {
        return Ok(changed_since(ctx, root, &args.base));
    }
    Ok(None)
}

/// A JSON string as Python's `json.dumps` writes it (`ensure_ascii`).
fn json_str(text: &str) -> String {
    let mut out = String::from("\"");
    for c in text.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            c if (c as u32) < 0x20 || (c as u32) > 0x7e => {
                let mut units = [0_u16; 2];
                for unit in c.encode_utf16(&mut units) {
                    let _ = write!(out, "\\u{unit:04x}");
                }
            }
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

fn write_baseline(path: &Path, counts: &BTreeMap<String, usize>) -> Result<(), String> {
    let about = format!(
        "Accepted non-portable shell forms per file under scripts/ (local #322, hub #2978). \
         The portable-shell guard (crates/rsi-repo-guards, run as scripts/repo-guard portable-shell) fails a file \
         with more findings than its count here, a stale count, or a baseline above the merge base's. Lower counts \
         as forms are fixed or marked; never raise them. Regenerate (lowering only) with {REFRESH}."
    );
    let files: Vec<String> = counts
        .iter()
        .filter(|(_, count)| **count > 0)
        .map(|(file, count)| format!("    {}: {count}", json_str(file)))
        .collect();
    let files = if files.is_empty() {
        "{}".to_string()
    } else {
        format!("{{\n{}\n  }}", files.join(",\n"))
    };
    let body = format!(
        "{{\n  \"schema\": 1,\n  \"about\": {},\n  \"files\": {files}\n}}\n",
        json_str(&about)
    );
    std::fs::write(path, body).map_err(|error| format!("{}: {error}", path.display()))
}

fn actual_counts(findings: &BTreeMap<String, Vec<Finding>>) -> BTreeMap<String, usize> {
    findings
        .iter()
        .filter(|(_, items)| !items.is_empty())
        .map(|(rel, items)| (rel.clone(), items.len()))
        .collect()
}

/// Problems for files whose count differs from the baseline, with their findings.
///
/// `touched` is `None` for a strict run (every stale count is a problem) or the set of
/// files the change touches (only those may be stale; new uses fail for every file).
fn judge_files(
    findings: &BTreeMap<String, Vec<Finding>>,
    baseline: &BTreeMap<String, i64>,
    whole: bool,
    touched: Option<&BTreeSet<String>>,
) -> Vec<String> {
    let mut names: BTreeSet<&String> = findings.keys().collect();
    if whole {
        names.extend(baseline.keys());
    }
    if let Some(touched) = touched {
        names.extend(baseline.keys().filter(|rel| touched.contains(*rel)));
    }
    let mut problems = Vec::new();
    for rel in names {
        let items = findings.get(rel).map_or(&[][..], Vec::as_slice);
        let allowed = baseline.get(rel).copied().unwrap_or(0);
        let count = i64::try_from(items.len()).unwrap_or(i64::MAX);
        if count < allowed && touched.is_some_and(|touched| !touched.contains(rel)) {
            continue;
        }
        problems.extend(file_problem(rel, items, allowed));
    }
    problems
}

fn file_problem(rel: &str, items: &[Finding], allowed: i64) -> Vec<String> {
    let count = i64::try_from(items.len()).unwrap_or(i64::MAX);
    if count == allowed {
        return Vec::new();
    }
    if count < allowed {
        return vec![format!(
            "{rel}: baseline allows {allowed} but only {count} remain; lower the baseline for {rel} to {count} (`{REFRESH}`)"
        )];
    }
    let mut lines = vec![format!(
        "{rel}: {count} non-portable form(s), baseline allows {allowed}"
    )];
    lines.extend(items.iter().map(format_finding));
    lines
}

/// The printed form of one finding.
#[must_use]
pub fn format_finding(finding: &Finding) -> String {
    let rule = rule(finding.rule);
    format!(
        "  {}:{}: [{}] {} is {}-only: {}\n      > {}",
        finding.path,
        finding.line,
        finding.rule,
        rule.form,
        rule.side.to_uppercase(),
        rule.message,
        finding.snippet
    )
}

fn judge_raises(
    baseline: &BTreeMap<String, i64>,
    base_counts: Option<&BTreeMap<String, i64>>,
) -> Vec<String> {
    let Some(base) = base_counts else {
        return Vec::new();
    };
    baseline
        .iter()
        .filter(|(rel, count)| **count > base.get(*rel).copied().unwrap_or(0))
        .map(|(rel, count)| {
            format!(
                "{rel}: baseline raised from {} to {count}; the baseline may only go down",
                base.get(rel).copied().unwrap_or(0)
            )
        })
        .collect()
}

const HINT: &str = concat!(
    "fix each form with the named portable alternative, put it under a `uname` branch, or mark it ",
    "`# portability: linux-only` / `# portability: macos-only` (file header, same line or line above). ",
    "The baseline (tools/portable-shell-baseline.json) may only go down."
);

fn do_refresh(
    baseline_path: &Path,
    findings: &BTreeMap<String, Vec<Finding>>,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> Result<i32, String> {
    let counts = actual_counts(findings);
    let old = read_baseline(baseline_path)?;
    let raised: Vec<String> = old.map_or_else(Vec::new, |old| {
        counts
            .iter()
            .filter(|(rel, count)| {
                i64::try_from(**count).unwrap_or(i64::MAX) > old.get(*rel).copied().unwrap_or(0)
            })
            .map(|(rel, count)| {
                format!(
                    "{rel}: {count} > baseline {}",
                    old.get(rel).copied().unwrap_or(0)
                )
            })
            .collect()
    });
    if !raised.is_empty() {
        let _ = writeln!(
            err,
            "check-portable-shell: --refresh never raises the baseline; fix or mark these first:"
        );
        let _ = writeln!(err, "{}", raised.join("\n"));
        return Ok(1);
    }
    write_baseline(baseline_path, &counts)?;
    let _ = writeln!(
        out,
        "check-portable-shell: wrote {} ({} use(s) in {} file(s))",
        baseline_path.display(),
        counts.values().sum::<usize>(),
        counts.len()
    );
    Ok(0)
}

fn run(ctx: &Ctx, args: &Args, out: &mut dyn Write, err: &mut dyn Write) -> Result<i32, String> {
    let root = py::resolve(&args.root);
    let baseline_path = root.join(&args.baseline);
    let rels = candidate_files(&root, &args.paths);
    let findings = scan_files(&root, &rels)?;
    if args.refresh {
        return do_refresh(&baseline_path, &findings, out, err);
    }
    if args.list {
        let lines: Vec<String> = findings.values().flatten().map(format_finding).collect();
        let _ = writeln!(out, "{}", lines.join("\n"));
        return Ok(0);
    }
    let baseline = read_baseline(&baseline_path)?.unwrap_or_default();
    let touched = touched_files(ctx, &root, args)?;
    let mut problems = judge_files(
        &findings,
        &baseline,
        args.paths.is_empty(),
        touched.as_ref(),
    );
    if !args.base.is_empty() {
        let base_counts = read_base_baseline(ctx, &root, &args.base, &args.baseline)?;
        problems.extend(judge_raises(&baseline, base_counts.as_ref()));
    }
    if problems.is_empty() {
        return Ok(0);
    }
    let _ = writeln!(err, "check-portable-shell: {HINT}");
    let _ = writeln!(err, "{}", problems.join("\n"));
    Ok(1)
}

/// Entry point: `portable-shell [--base REF] [--touched-from FILE] [--baseline PATH]
/// [--root DIR] [--refresh | --list] [paths...]`.
pub fn main(args: &[String], ctx: &Ctx, out: &mut dyn Write, err: &mut dyn Write) -> i32 {
    let opts = [
        Opt {
            name: "--root",
            value: true,
        },
        Opt {
            name: "--baseline",
            value: true,
        },
        Opt {
            name: "--base",
            value: true,
        },
        Opt {
            name: "--touched-from",
            value: true,
        },
        Opt {
            name: "--refresh",
            value: false,
        },
        Opt {
            name: "--list",
            value: false,
        },
    ];
    let usage = "[-h] [--root ROOT] [--baseline BASELINE] [--base BASE] [--touched-from TOUCHED_FROM] \
                 [--refresh | --list] [paths ...]";
    let parsed = match cli::parse(
        "check_portable_shell",
        usage,
        args,
        &opts,
        &[&["--refresh", "--list"]],
        None,
        err,
    ) {
        Ok(parsed) => parsed,
        Err(exit) => return exit.code(),
    };
    let args = Args {
        paths: parsed.positionals.clone(),
        root: parsed
            .value("--root")
            .map_or_else(|| ctx.cwd.clone(), |root| ctx.path(root)),
        baseline: parsed.value("--baseline").unwrap_or(BASELINE).to_string(),
        base: parsed.value("--base").unwrap_or("").to_string(),
        touched_from: parsed.value("--touched-from").unwrap_or("").to_string(),
        refresh: parsed.flag("--refresh"),
        list: parsed.flag("--list"),
    };
    match run(ctx, &args, out, err) {
        Ok(code) => code,
        Err(error) => {
            let _ = writeln!(err, "check-portable-shell: {error}");
            1
        }
    }
}

#[cfg(test)]
#[path = "portable_shell_tests.rs"]
mod tests;
