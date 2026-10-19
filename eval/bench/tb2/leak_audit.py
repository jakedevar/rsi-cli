#!/usr/bin/env python3
"""Report held-out Terminal-Bench 2.0 content found in harness-editable text.

    leak_audit.py --checkout CHECKOUT (--diff FILE|- | --files PATH ...) [--exclude GLOB ...]
                  [--terms held_out|search|all]

Input is a unified diff (only added lines are audited) or a list of files (every line is). A
`--files` entry may be a candidate directory (`eval/bench/tb2/candidates/<id>/`): its overlay.toml,
the instructions file(s) and notes.md are audited.
Every path is normalised to repo-relative before the excludes apply, so a relative and an absolute
spelling of one file are excluded (or audited) alike. Only the files that list task names on purpose
(dataset.toml, split.json) are excluded by default; `candidates/` is always audited.
Terms come from the held-out tasks in split.json (`--terms held_out`, the default):
  task-name  the held-out task name, matched as a whole token (so `path-tracing` does not
             match the search task `path-tracing-reverse`);
  test-file  the name of a file under the task's tests/ that no other task has;
  literal    a line (>= 16 characters) of an expected-output string literal in the task's
             tests/*.py, or of a non-script text file under tests/, that no other task has.
Without --checkout only task names are audited. Prints `HIT path:line kind term (task)`,
exits 1 on any hit, 0 when clean, 2 on an input error. M6 runs it on every candidate.

`--terms search` is the generality mode: it flags search-split task names (whole token) and test file
names only one search task has, as `GENERALITY path:line kind term (task)`; such text overfits the
search split. It exits 1 like a held-out hit. `--terms all` runs both.
"""

import argparse
import ast
import collections
import fnmatch
import posixpath
import pathlib
import re
import sys
import warnings

import tb2lib

MIN_LITERAL = 16
MAX_TEXT_BYTES = 256 * 1024
REPO_ROOT = pathlib.Path(__file__).resolve().parents[3]
# The pin files list task names on purpose; they are not harness surfaces. Exact repo paths only.
DEFAULT_EXCLUDES = ["eval/bench/tb2/dataset.toml", "eval/bench/tb2/split.json"]
# Candidate files are always audited, whatever --exclude says.
CANDIDATES_DIR = "eval/bench/tb2/candidates/"
CANDIDATE_FILES = ["overlay.toml", "instructions.md", "notes.md"]


def _lines(text):
    return [ln.strip() for ln in text.splitlines() if len(ln.strip()) >= MIN_LITERAL]


def _py_literal_lines(source):
    try:
        with warnings.catch_warnings():
            warnings.simplefilter("ignore")  # task tests carry invalid escapes
            tree = ast.parse(source)
    except (SyntaxError, ValueError):
        return _lines(source)
    docstrings = set()
    for node in ast.walk(tree):
        if isinstance(node, (ast.Module, ast.FunctionDef, ast.AsyncFunctionDef, ast.ClassDef)):
            first = node.body[0] if node.body else None
            if isinstance(first, ast.Expr) and isinstance(first.value, ast.Constant):
                docstrings.add(id(first.value))
    out = []
    for node in ast.walk(tree):
        if isinstance(node, ast.Constant) and isinstance(node.value, str) and id(node) not in docstrings:
            out += _lines(node.value)
    return out


def task_test_terms(task_dir):
    """(test file names, literal lines) of one task's tests/ directory."""
    names, lits = set(), set()
    tests = pathlib.Path(task_dir) / "tests"
    if not tests.is_dir():
        return names, lits
    for p in sorted(tests.rglob("*")):
        if not p.is_file():
            continue
        names.add(p.name)
        if p.suffix == ".sh" or p.stat().st_size > MAX_TEXT_BYTES:
            continue
        try:
            text = p.read_text(encoding="utf-8")
        except (UnicodeDecodeError, OSError):
            continue
        lits.update(_py_literal_lines(text) if p.suffix == ".py" else _lines(text))
    return names, lits


def build_terms(checkout, tasks, held_out, literals=True):
    """List of (kind, term, task) for the named tasks (the held-out split, or the search split
    with `literals=False`). A test file name or literal counts only when no other task has it."""
    terms = [("task-name", n, n) for n in held_out]
    if checkout is None:
        return terms
    per_task = {t["name"]: task_test_terms(pathlib.Path(checkout) / t["name"]) for t in tasks}
    name_count = collections.Counter(n for names, _ in per_task.values() for n in names)
    lit_count = collections.Counter(s for _, lits in per_task.values() for s in lits)
    for task in held_out:
        names, lits = per_task.get(task, (set(), set()))
        terms += [("test-file", n, task) for n in sorted(names) if name_count[n] == 1]
        if literals:
            terms += [("literal", s, task) for s in sorted(lits) if lit_count[s] == 1]
    return terms


def _name_regex(name):
    return re.compile(r"(?<![A-Za-z0-9_-])" + re.escape(name) + r"(?![A-Za-z0-9_-])")


class Auditor:
    def __init__(self, terms):
        self.names = [(_name_regex(t), t, task) for kind, t, task in terms if kind == "task-name"]
        self.plain = [(kind, t, task) for kind, t, task in terms if kind != "task-name"]

    def scan_line(self, line):
        hits = [("task-name", term, task) for rx, term, task in self.names if rx.search(line)]
        hits += [(kind, term, task) for kind, term, task in self.plain if term in line]
        return hits


def added_lines(diff_text):
    """Yield (path, line number in the new file, text) for each added line of a unified diff."""
    path, lineno = None, 0
    for raw in diff_text.splitlines():
        if raw.startswith("+++ "):
            tgt = raw[4:].split("\t")[0]
            path = None if tgt == "/dev/null" else (tgt[2:] if tgt.startswith("b/") else tgt)
        elif raw.startswith("@@"):
            m = re.match(r"@@ -\d+(?:,\d+)? \+(\d+)", raw)
            lineno = int(m.group(1)) if m else 0
        elif raw.startswith("---") and not raw.startswith("--- a/") and path is None:
            continue
        elif path is not None and raw.startswith("+"):
            yield path, lineno, raw[1:]
            lineno += 1
        elif path is not None and raw.startswith(" "):
            lineno += 1


def candidate_files(directory):
    """The audited files of a candidate directory: overlay, instructions file(s) and notes."""
    d = pathlib.Path(directory)
    names = list(CANDIDATE_FILES)
    overlay = d / "overlay.toml"
    if overlay.is_file():
        for m in re.finditer(r'^\s*instructions\s*=\s*"([^"]+)"', overlay.read_text(encoding="utf-8"), re.M):
            names.append(m.group(1))
    files = []
    for n in names:
        f = d / n
        if f.is_file() and f not in files:
            files.append(f)
    if not files:
        raise ValueError(f"{directory}: no candidate files (overlay.toml, instructions, notes.md)")
    return files


def expand_paths(paths):
    out = []
    for p in paths:
        out += candidate_files(p) if pathlib.Path(p).is_dir() else [pathlib.Path(p)]
    return out


def normalize_path(path, repo_root=REPO_ROOT):
    """Repo-relative POSIX spelling of `path`; a path outside the repo keeps its own spelling."""
    pp = pathlib.Path(path)
    if pp.is_absolute():
        try:
            return pp.resolve().relative_to(pathlib.Path(repo_root).resolve()).as_posix()
        except ValueError:
            return pp.as_posix()
    norm = posixpath.normpath(pp.as_posix())
    return norm


def file_lines(paths):
    for p in expand_paths(paths):
        try:
            text = p.read_text(encoding="utf-8")
        except UnicodeDecodeError:
            continue  # binary
        for i, line in enumerate(text.splitlines(), 1):
            yield str(p.resolve()), i, line


def excluded(path, patterns):
    """`path` is repo-relative. Candidate files are never excluded."""
    if path.startswith(CANDIDATES_DIR):
        return False
    return any(fnmatch.fnmatchcase(path, g) for g in patterns)


def audit(auditor, items, excludes=DEFAULT_EXCLUDES, repo_root=REPO_ROOT):
    found = []
    for path, lineno, text in items:
        path = normalize_path(path, repo_root)
        if excluded(path, excludes):
            continue
        for kind, term, task in auditor.scan_line(text):
            found.append((path, lineno, kind, term, task))
    return found


def main(argv=None):
    ap = argparse.ArgumentParser(description="Report held-out Terminal-Bench 2.0 strings in a diff or files.")
    ap.add_argument("--checkout", help="benchmark checkout; adds test file names and expected-output literals")
    src = ap.add_mutually_exclusive_group(required=True)
    src.add_argument("--diff", help="unified diff file, or - for stdin")
    src.add_argument("--files", nargs="+", help="files to audit in full")
    ap.add_argument("--exclude", action="append", default=None, help="glob of paths to skip (repeatable)")
    ap.add_argument("--terms", choices=["held_out", "search", "all"], default="held_out",
                    help="held_out (default): held-out content; search: search-split names (GENERALITY); all: both")
    ap.add_argument("--repo-root", default=str(REPO_ROOT), help=argparse.SUPPRESS)
    ap.add_argument("--dataset", default=str(tb2lib.DATASET))
    ap.add_argument("--split", default=str(tb2lib.SPLIT))
    args = ap.parse_args(argv)
    try:
        _, tasks, _ = tb2lib.load_dataset(args.dataset)
        split = tb2lib.load_split(args.split)
        passes = []  # (label, auditor)
        if args.terms in ("held_out", "all"):
            passes.append(("HIT", Auditor(build_terms(args.checkout, tasks, split["held_out"]))))
        if args.terms in ("search", "all"):
            passes.append(("GENERALITY", Auditor(build_terms(args.checkout, tasks, split["search"], literals=False))))
        if args.diff:
            text = sys.stdin.read() if args.diff == "-" else pathlib.Path(args.diff).read_text(encoding="utf-8")
            items = list(added_lines(text))
        else:
            items = list(file_lines(args.files))
        excludes = DEFAULT_EXCLUDES if args.exclude is None else args.exclude
        results = [(label, audit(a, items, excludes, args.repo_root)) for label, a in passes]
    except (OSError, KeyError, ValueError) as e:
        print(f"leak_audit error: {e}", file=sys.stderr)
        return 2
    total = 0
    for label, found in results:
        for path, lineno, kind, term, task in found:
            print(f"{label} {path}:{lineno} {kind} {term!r} ({task})")
        total += len(found)
        if found:
            what = "generality" if label == "GENERALITY" else "held-out"
            print(f"leak_audit: {len(found)} {what} hit(s)", file=sys.stderr)
    if total:
        return 1
    print("leak_audit: clean")
    return 0


if __name__ == "__main__":
    sys.exit(main())
