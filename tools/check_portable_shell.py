#!/usr/bin/env python3
"""Fail on new non-portable shell in scripts/ (BSD-only and GNU-only forms).

Scripts under scripts/ run on the Linux hub and on the macOS satellite (bash 3.2,
BSD userland). A BSD-only form breaks Linux and a GNU-only form breaks the Mac
(#2936, local #322). This guard flags both, and every finding names the portable
alternative.

Flagged forms
  BSD-only: `mktemp -t PREFIX` (no X's), `sed -i ''`, `stat -f`, `date -j`/`-v`
  GNU-only: bare `sed -i`, `stat -c`, `date -d`, `readlink -f`, `grep -P`, `xargs -r`,
            bash 4+ (`declare -A`, `mapfile`, `${var,,}`, `wait -n`, `declare -n`)

Allowed
  * code inside an `if`/`case` whose condition probes the OS (`uname`, `$OSTYPE`, a
    variable assigned from `uname`, `is_linux`/`is_macos`-style helpers), or on a
    line that follows such a probe (`[ "$(uname)" = Linux ] && ...`);
  * a BSD form and a GNU form of one tool joined by `||` in one command list (the
    portable `stat -c ... || stat -f ...` fallback idiom, also across a line ending
    in `||` or `\\` followed by `||`); adjacent lines alone are two defects;
  * `# portability: linux-only` (exempts GNU-only forms) or
    `# portability: macos-only` (exempts BSD-only forms): in the file's leading
    comment block for the whole file, otherwise on the line itself or on the
    comment line directly above it.

Scope: tracked-style shell files under scripts/ (`*.sh`, `*.bash`, and extensionless
files with a shell shebang), minus scripts/tests/fixtures/. Heredoc bodies are
scanned when they feed a shell (`bash`, `sh`, `ssh`, `cat > x.sh`) and skipped when
they feed another language or plain text.

Baseline: existing uses are counted per file in tools/portable-shell-baseline.json.
The guard fails when a file has more findings than its baseline (new uses; any file,
always), when a count is stale (fewer findings than the baseline), and, with
--base REF, when the baseline raises any count over REF's baseline. The baseline may
only go down; --refresh refuses to raise it.

Stale counts are judged only for the files the change touches, so an unrelated
change is never blocked because a sibling landing already fixed a use: with
--base REF the touched files are the ones `git diff REF...HEAD` (plus the working
tree) names, with --touched-from FILE the NUL-separated paths in FILE (the pre-push
hook, whose exported tree has no .git), and with explicit paths those paths. A
plain full-tree run (no --base, no --touched-from) reports every stale count.

Usage: tools/check_portable_shell.py [--base REF] [--touched-from FILE]
                                     [--baseline PATH] [--root DIR]
                                     [--refresh | --list] [paths...]
With no paths the whole scripts/ tree is scanned. Exit 0 clean, 1 findings, 2 usage.
"""
import argparse
import bisect
import collections
import json
import os
import re
import subprocess
import sys
from pathlib import Path

BASELINE = "tools/portable-shell-baseline.json"
SCAN_ROOT = "scripts"
SKIP_PARTS = ("scripts/tests/fixtures/",)

Rule = collections.namedtuple("Rule", "side form message")

RULES = {
    "mktemp-t": Rule(
        "bsd", "`mktemp -t PREFIX` without X's",
        "GNU mktemp needs X's in the template; use "
        "`mktemp \"${TMPDIR:-/tmp}/PREFIX.XXXXXX\"`"),
    "sed-i-empty": Rule(
        "bsd", "`sed -i ''`",
        "GNU sed reads '' as the script; use `sed -i.bak ... file && rm -f file.bak`, "
        "or write a temp file and `mv` it over the original"),
    "sed-i-bare": Rule(
        "gnu", "bare `sed -i`",
        "BSD sed takes the next argument as the backup suffix; use "
        "`sed -i.bak ... file && rm -f file.bak`, or write a temp file and `mv` it"),
    "stat-f": Rule(
        "bsd", "`stat -f FORMAT`",
        "GNU `stat -f` prints filesystem status; use `wc -c < file` for size, or "
        "`python3 -c 'import os,sys; print(os.stat(sys.argv[1]).st_mtime)' file`"),
    "stat-c": Rule(
        "gnu", "`stat -c`/`--format`",
        "BSD stat has no -c; use `wc -c < file` for size, or "
        "`python3 -c 'import os,sys; print(os.stat(sys.argv[1]).st_mtime)' file`"),
    "date-j": Rule(
        "bsd", "`date -j`/`date -v`",
        "GNU date has neither; do date arithmetic with `date +%s` epoch seconds or "
        "`python3 -c 'import datetime ...'`"),
    "date-d": Rule(
        "gnu", "`date -d`/`--date`",
        "BSD date has no -d; parse and shift dates with `python3 -c 'import datetime ...'` "
        "or `date +%s` arithmetic"),
    "readlink-f": Rule(
        "gnu", "`readlink -f`/`-e`/`-m`",
        "older macOS readlink lacks it; use `(cd \"$(dirname \"$p\")\" && pwd -P)` for a "
        "directory, or `python3 -c 'import os,sys; print(os.path.realpath(sys.argv[1]))' \"$p\"`"),
    "grep-P": Rule(
        "gnu", "`grep -P`",
        "BSD grep has no PCRE; use `grep -E`, or `perl -ne`/`python3` for lookarounds"),
    "xargs-r": Rule(
        "gnu", "`xargs -r`",
        "BSD xargs rejects it (it already skips empty input); guard with "
        "`[ -n \"$list\" ]` or loop with `while IFS= read -r x; do ...; done`"),
    "declare-A": Rule(
        "gnu", "`declare -A` (bash 4)",
        "macOS ships bash 3.2; use parallel indexed arrays, a `case` lookup, or "
        "`key=value` lines read with `cut`/`grep`"),
    "mapfile": Rule(
        "gnu", "`mapfile`/`readarray` (bash 4)",
        "macOS ships bash 3.2; use `while IFS= read -r line; do arr+=(\"$line\"); done < <(cmd)`"),
    "case-mod": Rule(
        "gnu", "`${var,,}`/`${var^^}` (bash 4)",
        "macOS ships bash 3.2; use `$(printf '%s' \"$var\" | tr '[:upper:]' '[:lower:]')`"),
    "wait-n": Rule(
        "gnu", "`wait -n` (bash 4.3)",
        "macOS ships bash 3.2; wait for each pid (`wait \"$pid\"`) or poll with `kill -0`"),
    "nameref": Rule(
        "gnu", "`declare -n`/`local -n` (bash 4.3)",
        "macOS ships bash 3.2; pass values through stdout or use `eval` indirection"),
}

# A marker exempts the forms that only break the *other* host.
MARKER_EXEMPTS = {"linux": "gnu", "macos": "bsd"}
MARKER = re.compile(r"#\s*portability:\s*(linux|macos)-only\b")

ASSIGN = re.compile(r"^([A-Za-z_][A-Za-z0-9_]*)\+?=")
LEADING_KEYWORDS = {"then", "else", "do", "{", "}", "!", "elif", "while", "until"}
OPENERS = {"if", "case"}
CLOSERS = {"fi", "esac"}
WRAPPERS = {"xargs", "sudo", "env", "command", "exec", "time", "nohup", "timeout", "nice",
            "ionice", "-exec", "-execdir", "-ok", "watch", "builtin", "ssh", "stdbuf"}
SHELLISH = {"bash", "sh", "zsh", "dash", "ksh", "eval", "ssh", "su"}
PROBE = re.compile(r"uname|OSTYPE|\bis_?(?:mac|macos|darwin|linux|bsd|gnu)\w*")
CASE_MOD = re.compile(r"\$\{[A-Za-z_][A-Za-z0-9_]*(?:,,?|\^\^?)[^}]*\}")
HEREDOC = re.compile(r"(?<!<)<<(?!<)(-?)\s*(?:'([^']+)'|\"([^\"]+)\"|\\?([A-Za-z_][A-Za-z0-9_]*))")
SHELL_SHEBANG = re.compile(r"^#!.*\b(?:ba|z|da|k)?sh\b")
SHELL_SUFFIXES = (".sh", ".bash")
NON_SHELL_SINKS = {"python", "python3", "perl", "ruby", "node", "awk", "sqlite3", "psql", "jq", "cat", "tee"}
SHELL_SINKS = {"bash", "sh", "zsh", "dash", "ksh", "ssh"}
MAX_DEPTH = 3

Word = collections.namedtuple("Word", "text raw expands")
Command = collections.namedtuple("Command", "line words raw start end")
Finding = collections.namedtuple("Finding", "path line rule snippet")


# --------------------------------------------------------------------------- lexer


class _Level:
    """The words and command being collected at one nesting level."""

    def __init__(self, parser):
        self.parser = parser
        self.words = []
        self.word = None
        self.first = None

    def in_word(self):
        return self.word is not None

    def begin(self, pos):
        if self.word is None:
            self.word = [pos, [], False]
        if self.first is None:
            self.first = pos

    def add(self, text, expands):
        self.word[1].append(text)
        self.word[2] = self.word[2] or expands

    def end_word(self, pos):
        if self.word is None:
            return
        start, parts, expands = self.word
        self.words.append(Word("".join(parts), self.parser.text[start:pos], expands))
        self.word = None

    def end_cmd(self, pos):
        self.end_word(pos)
        if not self.words:
            return
        text = self.parser.text
        self.parser.commands.append(Command(self.parser.line(self.first), self.words, text[self.first:pos], self.first, pos))
        self.words = []
        self.first = None


class _Parser:
    """A forgiving shell lexer: splits text into simple commands, quote and `$(...)` aware."""

    def __init__(self, text):
        self.text = text
        self.commands = []
        self.starts = [0] + [m.end() for m in re.finditer("\n", text)]

    def line(self, offset):
        return bisect.bisect_right(self.starts, offset)

    def run(self):
        self.parse(0, None)
        return self.commands

    def parse(self, i, closer):
        level = _Level(self)
        n = len(self.text)
        while i < n and self.text[i] != closer:
            i = self.step(level, i, self.text[i])
        level.end_cmd(i)
        return min(i + 1, n)

    def step(self, level, i, c):
        if c in " \t\r":
            level.end_word(i)
            return i + 1
        if c == "\n" or c in ";&|()":
            level.end_cmd(i)
            return i + 1
        if c == "\\":
            return self.backslash(level, i)
        if c == "#" and not level.in_word():
            end = self.text.find("\n", i)
            return len(self.text) if end < 0 else end
        if c == "'":
            return self.single(level, i)
        if c == '"':
            return self.double(level, i)
        if c == "$":
            return self.dollar(level, i)
        if c == "`":
            return self.backtick(level, i)
        level.begin(i)
        level.add(c, True)
        return i + 1

    def backslash(self, level, i):
        following = self.text[i + 1:i + 2]
        if following == "\n":
            return i + 2
        level.begin(i)
        level.add(following, True)
        return i + 2

    def single(self, level, i):
        end = self.text.find("'", i + 1)
        end = len(self.text) if end < 0 else end
        level.begin(i)
        level.add(self.text[i + 1:end], False)
        return end + 1

    def double(self, level, i):
        level.begin(i)
        n = len(self.text)
        j = i + 1
        while j < n and self.text[j] != '"':
            j = self.double_char(level, j)
        level.add("", True)
        return j + 1

    def double_char(self, level, j):
        c = self.text[j]
        if c == "\\":
            level.add(self.text[j + 1:j + 2], True)
            return j + 2
        if c == "$":
            return self.dollar(level, j)
        if c == "`":
            return self.backtick(level, j)
        level.add(c, True)
        return j + 1

    def dollar(self, level, i):
        level.begin(i)
        text = self.text
        if text.startswith("$((", i):
            end = self.match_paren(i + 1)
            level.add(text[i:end], True)
            return end
        if text.startswith("$(", i):
            end = self.parse(i + 2, ")")
            level.add(text[i:end], True)
            return end
        if text.startswith("${", i):
            end = self.match_brace(i + 1)
            self.substitutions_within(i, end)
            level.add(text[i:end], True)
            return end
        level.add("$", True)
        return i + 1

    def substitutions_within(self, start, end):
        """Parse the `$(...)` inside a `${...}` so a default value's command is seen."""
        at = self.text.find("$(", start + 2, end)
        while 0 <= at < end:
            at = self.text.find("$(", self.parse(at + 2, ")"), end)

    def backtick(self, level, i):
        level.begin(i)
        end = self.parse(i + 1, "`")
        level.add(self.text[i:end], True)
        return end

    def match_paren(self, i):
        return self.match_pair(i, "(", ")")

    def match_brace(self, i):
        return self.match_pair(i, "{", "}")

    def match_pair(self, i, opener, closer):
        depth = 0
        n = len(self.text)
        j = i
        while j < n:
            c = self.text[j]
            depth += (c == opener) - (c == closer)
            j += 2 if c == "\\" else 1
            if depth == 0:
                return j
        return n


def parse_commands(text):
    return _Parser(text).run()


# ------------------------------------------------------------------- heredocs/markers


def should_scan_heredoc(line, start):
    """True when the heredoc opened at `start` feeds a shell (or writes a shell script)."""
    if re.search(r">>?\s*\S+\.(?:sh|bash)\b", line):
        return True
    names = {os.path.basename(w.text) for c in parse_commands(line[:start]) for w in c.words}
    if names & NON_SHELL_SINKS:
        return False
    return bool(names & SHELL_SINKS)


def heredocs_opened(line):
    """(delimiter, strips_tabs, scan) for each heredoc a line opens, in order."""
    if line.lstrip().startswith("#"):
        return []
    opened = []
    for m in HEREDOC.finditer(line):
        delim = m.group(2) or m.group(3) or m.group(4)
        opened.append((delim, bool(m.group(1)), should_scan_heredoc(line, m.start())))
    return opened


def strip_heredocs(text):
    """The text with heredoc bodies that do not feed a shell blanked (line numbers kept)."""
    out = []
    pending = []
    for line in text.split("\n"):
        if not pending:
            out.append(line)
            pending = heredocs_opened(line)
            continue
        delim, tabs, scan = pending[0]
        candidate = (line.lstrip("\t") if tabs else line).rstrip("\r")
        if candidate == delim:
            pending.pop(0)
            out.append("")
            continue
        out.append(line if scan else "")
    return "\n".join(out)


def collect_markers(text):
    """(file_kinds, {line: kinds}) from `# portability: linux-only|macos-only` markers."""
    file_kinds = set()
    per_line = {}
    pending = set()
    header = True
    for number, line in enumerate(text.split("\n"), 1):
        stripped = line.strip()
        marker = MARKER.search(line)
        kinds = {marker.group(1)} if marker else set()
        if not stripped:
            continue
        if stripped.startswith("#"):
            file_kinds |= kinds if header else set()
            pending |= kinds if not header else set()
            continue
        header = False
        applies = kinds | pending
        pending = set()
        if applies:
            per_line[number] = applies
    return file_kinds, per_line


# ------------------------------------------------------------------------- detection


def basename(word):
    return os.path.basename(word.text)


def option_cluster(arg, letters):
    return re.fullmatch(r"-[A-Za-z]*[" + letters + r"][A-Za-z]*", arg) is not None


def hits_mktemp(args):
    has_template = any("XXX" in a for a in args if not a.startswith("-"))
    uses_t = any(option_cluster(a, "t") for a in args)
    return ["mktemp-t"] if uses_t and not has_template else []


def hits_sed(args):
    hits = []
    for index, arg in enumerate(args):
        hits += sed_inplace_hit(arg, args[index + 1:index + 2])
    return hits


def sed_inplace_hit(arg, following):
    if arg == "--in-place":
        return ["sed-i-bare"]
    if arg != "-i":
        return []
    return ["sed-i-empty"] if following == [""] else ["sed-i-bare"]


def hits_stat(args):
    gnu = any(a == "-c" or re.match(r"-c%", a) or re.match(r"--(format|printf)\b", a) for a in args)
    bsd = any(a == "-f" or re.match(r"-f[%\"']", a) for a in args)
    if gnu:
        return ["stat-c"]
    return ["stat-f"] if bsd else []


def hits_date(args):
    hits = []
    if any(re.fullmatch(r"-j[A-Za-z]*|-v[+-]?\d\w*", a) for a in args):
        hits.append("date-j")
    if any(re.fullmatch(r"-d.*|--date(=.*)?", a) for a in args):
        hits.append("date-d")
    return hits


def hits_readlink(args):
    return ["readlink-f"] if any(option_cluster(a, "fem") or a.startswith("--canonicalize") for a in args) else []


def hits_grep(args):
    return ["grep-P"] if any(option_cluster(a, "P") or a == "--perl-regexp" for a in args) else []


XARGS_VALUE_OPTIONS = {"-n", "-I", "-P", "-L", "-s", "-E", "-d", "-a", "-J", "-R", "-S"}


def hits_xargs(args):
    skip = False
    for arg in args:
        if skip:
            skip = False
            continue
        if not arg.startswith("-"):
            return []
        if arg == "--no-run-if-empty" or re.fullmatch(r"-[A-Za-z0-9]*r[A-Za-z0-9]*", arg):
            return ["xargs-r"]
        skip = arg in XARGS_VALUE_OPTIONS
    return []


TOOL_RULES = {
    "mktemp": hits_mktemp, "sed": hits_sed, "stat": hits_stat, "date": hits_date,
    "readlink": hits_readlink, "grep": hits_grep, "xargs": hits_xargs,
}


def is_command_position(names, index):
    return index == 0 or any(n in WRAPPERS for n in names[:index])


def tool_hits(words):
    names = [basename(w) for w in words]
    hits = []
    for index, name in enumerate(names):
        rule = TOOL_RULES.get(name)
        if rule is None or not is_command_position(names, index):
            continue
        hits += rule([w.text for w in words[index + 1:]])
    return hits


def bash4_hits(words):
    name = basename(words[0])
    args = [w.text for w in words[1:]]
    hits = ["case-mod"] if any(w.expands and CASE_MOD.search(w.text) for w in words) else []
    if name in ("declare", "local", "typeset", "readonly"):
        hits += ["declare-A"] if any(option_cluster(a, "A") for a in args) else []
        hits += ["nameref"] if any(option_cluster(a, "n") for a in args) else []
    if name in ("mapfile", "readarray"):
        hits.append("mapfile")
    if name == "wait" and any(option_cluster(a, "n") for a in args):
        hits.append("wait-n")
    return hits


def strip_assignments(words):
    index = 0
    while index < len(words) - 1 and ASSIGN.match(words[index].text):
        index += 1
    return words[index:]


def command_hits(words, depth=0):
    words = strip_assignments(words)
    hits = tool_hits(words) + bash4_hits(words)
    if depth >= MAX_DEPTH or basename(words[0]) not in SHELLISH:
        return hits
    for word in words[1:]:
        hits += nested_hits(word, depth)
    return hits


def nested_hits(word, depth):
    """Hits in a quoted command string handed to `bash -c`, `ssh host '...'`, `eval`."""
    if not re.search(r"\s", word.text):
        return []
    hits = []
    for command in parse_commands(word.text):
        hits += command_hits(command.words, depth + 1)
    return hits


def split_head(words):
    """(rest, opened, closed): leading shell keywords stripped from a command."""
    opened, closed, index = 0, 0, 0
    while index < len(words):
        text = words[index].text
        opened += text in OPENERS
        closed += text in CLOSERS
        if text not in LEADING_KEYWORDS | OPENERS | CLOSERS:
            break
        index += 1
    return words[index:], opened, closed


class _State:
    """Walks commands in order, tracking which are under an OS probe."""

    def __init__(self, file_kinds, line_kinds):
        self.file_kinds = file_kinds
        self.line_kinds = line_kinds
        self.stack = []
        self.probe_vars = set()
        self.probe_lines = set()
        self.findings = []
        self.spans = {}

    def is_probe(self, raw):
        if PROBE.search(raw):
            return True
        return any(re.search(r"\$\{?" + re.escape(v) + r"\b", raw) for v in self.probe_vars)

    def track_assignment(self, words):
        match = ASSIGN.match(words[0].text)
        if match and PROBE.search(words[0].text):
            self.probe_vars.add(match.group(1))

    def exempt(self, rule, line):
        if any(self.stack) or line in self.probe_lines:
            return True
        kinds = self.file_kinds | self.line_kinds.get(line, set())
        return any(MARKER_EXEMPTS[k] == RULES[rule].side for k in kinds)

    def visit(self, command):
        rest, opened, closed = split_head(command.words)
        del self.stack[max(0, len(self.stack) - closed):]
        probe = self.is_probe(command.raw)
        if probe:
            self.probe_lines.add(command.line)
        self.record(command, rest)
        self.stack.extend([probe] * opened)
        if rest:
            self.track_assignment(rest)

    def record(self, command, rest):
        if not rest:
            return
        for rule in command_hits(rest):
            if self.exempt(rule, command.line):
                continue
            finding = (command.line, rule, command.raw.split("\n")[0].strip()[:80])
            self.spans[finding] = (command.start, command.end)
            self.findings.append(finding)


# A BSD form and a GNU form of one tool joined by `||` are the portable
# `stat -c ... || stat -f ...` fallback idiom, not two defects. Proximity alone is
# not a fallback: two independent statements each fail on one platform.
FAMILY = {"stat-c": "stat", "stat-f": "stat", "sed-i-bare": "sed", "sed-i-empty": "sed",
          "date-j": "date", "date-d": "date"}
COMMENT = re.compile(r"(^|\s)#[^\n]*")


def or_linked(text, first, second):
    """True when only `||` (and its continuation) joins the commands ending/starting at the spans."""
    earlier, later = sorted([first, second])
    end, start = earlier[1], later[0]
    between = COMMENT.sub(r"\1", text[end:start])
    if "||" not in between:
        return False
    flat = re.sub(r"\\\n|\|\|\s*\n", " ", between)
    return not re.search(r";|&&|\n|(?<![|&])&(?![&>])", flat)


def drop_fallback_pairs(found, spans, text):
    """Remove findings whose opposite-side form of the same tool is `||`-linked to them."""
    kept = []
    for item in found:
        family = FAMILY.get(item[1])
        partners = [other for other in found
                    if family and FAMILY.get(other[1]) == family
                    and RULES[other[1]].side != RULES[item[1]].side
                    and or_linked(text, spans[item], spans[other])]
        if not partners:
            kept.append(item)
    return kept


def scan_text(text, path="<text>"):
    """Findings (Finding tuples, in line order) for one shell file's text."""
    file_kinds, line_kinds = collect_markers(text)
    state = _State(file_kinds, line_kinds)
    text = strip_heredocs(text)
    for command in parse_commands(text):
        state.visit(command)
    found = drop_fallback_pairs(state.findings, state.spans, text)
    return [Finding(path, line, rule, snippet) for line, rule, snippet in sorted(found)]


# ------------------------------------------------------------------------- the tree


def is_shell_file(root, rel):
    if any(rel.startswith(part) for part in SKIP_PARTS):
        return False
    path = root / rel
    if rel.endswith(SHELL_SUFFIXES):
        return path.is_file()
    if os.path.splitext(rel)[1] or not path.is_file():
        return False
    try:
        with open(path, "rb") as handle:
            first = handle.readline(200).decode("utf-8", "replace")
    except OSError:
        return False
    return SHELL_SHEBANG.match(first) is not None


def walk_scripts(root):
    base = root / SCAN_ROOT
    found = []
    for directory, _dirs, names in os.walk(base):
        found += [Path(directory, n).relative_to(root).as_posix() for n in names]
    return sorted(found)


def relative_arg(root, arg):
    path = Path(arg)
    if path.is_absolute():
        try:
            path = path.relative_to(root)
        except ValueError:
            return ""
    return path.as_posix()


def candidate_files(root, paths):
    names = walk_scripts(root) if not paths else sorted({relative_arg(root, p) for p in paths})
    names = [n for n in names if n.startswith(SCAN_ROOT + "/")]
    return [n for n in names if is_shell_file(root, n)]


def scan_files(root, rels):
    findings = {}
    for rel in rels:
        text = (root / rel).read_text(encoding="utf-8", errors="replace")
        findings[rel] = scan_text(text, rel)
    return findings


# ---------------------------------------------------------------------- the baseline


def load_counts(text):
    data = json.loads(text)
    files = data.get("files", {}) if isinstance(data, dict) else {}
    return {str(k): int(v) for k, v in files.items()}


def read_baseline(path):
    """The committed counts, or None when the file does not exist."""
    if not path.is_file():
        return None
    return load_counts(path.read_text(encoding="utf-8"))


def read_base_baseline(root, ref, baseline_rel):
    """The counts at git ref `ref`, or None when the ref or the file is not readable."""
    done = subprocess.run(["git", "-C", str(root), "show", f"{ref}:{baseline_rel}"],
                          capture_output=True, text=True)
    if done.returncode != 0:
        return None
    return load_counts(done.stdout)


def git_names(root, *args):
    """The NUL-separated paths `git diff --name-only` prints, or None when git fails."""
    done = subprocess.run(["git", "-C", str(root), "diff", "--name-only", "--no-renames", "-z", *args],
                          capture_output=True)
    if done.returncode != 0:
        return None
    return {n.decode("utf-8", "surrogateescape") for n in done.stdout.split(b"\0") if n}


def changed_since(root, ref):
    """Files changed on this branch since it left `ref` (and in the working tree), else None."""
    committed = git_names(root, f"{ref}...HEAD")
    if committed is None:
        committed = git_names(root, ref, "HEAD")
    pending = git_names(root, "HEAD")
    if committed is None or pending is None:
        return None
    return committed | pending


def read_touched_list(path):
    data = Path(path).read_bytes()
    return {n.decode("utf-8", "surrogateescape") for n in data.split(b"\0") if n}


def touched_files(root, args):
    """The files whose stale counts are judged, or None to judge every count."""
    if args.touched_from:
        return read_touched_list(args.touched_from)
    if args.paths:
        return {relative_arg(root, p) for p in args.paths} if args.base else None
    if args.base:
        return changed_since(root, args.base)
    return None


def write_baseline(path, counts):
    body = {
        "schema": 1,
        "about": ("Accepted non-portable shell forms per file under scripts/ (local #322, hub #2978). "
                  "tools/check_portable_shell.py fails a file with more findings than its count here, a "
                  "stale count, or a baseline above the merge base's. Lower counts as forms are fixed or "
                  "marked; never raise them. Regenerate (lowering only) with "
                  "python3 tools/check_portable_shell.py --refresh."),
        "files": {k: counts[k] for k in sorted(counts) if counts[k] > 0},
    }
    path.write_text(json.dumps(body, indent=2) + "\n", encoding="utf-8")


def actual_counts(findings):
    return {rel: len(items) for rel, items in findings.items() if items}


def judge_files(findings, baseline, whole, touched=None):
    """Problems for files whose count differs from the baseline, with their findings.

    `touched` is None for a strict run (every stale count is a problem) or the set of
    files the change touches (only those may be stale; new uses fail for every file).
    """
    problems = []
    names = set(findings) | (set(baseline) if whole else set())
    if touched is not None:
        names |= set(baseline) & touched
    for rel in sorted(names):
        items = findings.get(rel, [])
        allowed = baseline.get(rel, 0)
        if len(items) < allowed and touched is not None and rel not in touched:
            continue
        problems += file_problem(rel, items, allowed)
    return problems


def file_problem(rel, items, allowed):
    if len(items) == allowed:
        return []
    if len(items) < allowed:
        return [f"{rel}: baseline allows {allowed} but only {len(items)} remain; "
                f"lower the baseline for {rel} to {len(items)} "
                f"(`python3 tools/check_portable_shell.py --refresh`)"]
    lines = [f"{rel}: {len(items)} non-portable form(s), baseline allows {allowed}"]
    lines += [format_finding(f) for f in items]
    return lines


def format_finding(finding):
    rule = RULES[finding.rule]
    return (f"  {finding.path}:{finding.line}: [{finding.rule}] {rule.form} is {rule.side.upper()}-only: "
            f"{rule.message}\n      > {finding.snippet}")


def judge_raises(baseline, base_counts):
    if base_counts is None:
        return []
    return [f"{rel}: baseline raised from {base_counts.get(rel, 0)} to {count}; the baseline may only go down"
            for rel, count in sorted(baseline.items()) if count > base_counts.get(rel, 0)]


HINT = ("fix each form with the named portable alternative, put it under a `uname` branch, or mark it "
        "`# portability: linux-only` / `# portability: macos-only` (file header, same line or line above). "
        f"The baseline ({BASELINE}) may only go down.")


def do_refresh(root, baseline_path, findings):
    counts = actual_counts(findings)
    old = read_baseline(baseline_path)
    raised = [] if old is None else [f"{r}: {c} > baseline {old.get(r, 0)}" for r, c in sorted(counts.items())
                                       if c > old.get(r, 0)]
    if raised:
        print("check-portable-shell: --refresh never raises the baseline; fix or mark these first:", file=sys.stderr)
        print("\n".join(raised), file=sys.stderr)
        return 1
    write_baseline(baseline_path, counts)
    print(f"check-portable-shell: wrote {baseline_path} ({sum(counts.values())} use(s) in {len(counts)} file(s))")
    return 0


def run(args):
    root = Path(args.root).resolve()
    baseline_path = root / args.baseline
    rels = candidate_files(root, args.paths)
    findings = scan_files(root, rels)
    if args.refresh:
        return do_refresh(root, baseline_path, findings)
    if args.list:
        print("\n".join(format_finding(f) for items in findings.values() for f in items))
        return 0
    baseline = read_baseline(baseline_path) or {}
    problems = judge_files(findings, baseline, whole=not args.paths, touched=touched_files(root, args))
    if args.base:
        problems += judge_raises(baseline, read_base_baseline(root, args.base, args.baseline))
    if not problems:
        return 0
    print("check-portable-shell: " + HINT, file=sys.stderr)
    print("\n".join(problems), file=sys.stderr)
    return 1


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__.split("\n", 1)[0])
    parser.add_argument("paths", nargs="*", help="files to check (default: the whole scripts/ tree)")
    parser.add_argument("--root", default=os.getcwd(), help="repository root (default: cwd)")
    parser.add_argument("--baseline", default=BASELINE, help="baseline path relative to the root")
    parser.add_argument("--base", default="", help="git ref whose baseline the committed one may not exceed")
    parser.add_argument("--touched-from", default="",
                        help="file of NUL-separated paths the change touches (only those may be stale)")
    mode = parser.add_mutually_exclusive_group()
    mode.add_argument("--refresh", action="store_true", help="rewrite the baseline (lowering only)")
    mode.add_argument("--list", action="store_true", help="print every finding and exit 0")
    return run(parser.parse_args(argv))


if __name__ == "__main__":
    sys.exit(main())
