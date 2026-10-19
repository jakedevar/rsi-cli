#!/usr/bin/env python3
"""Select matching Python tests for changed files; never invoke Cargo.

Changed tests select themselves. Script stems match test_<stem>.py (hyphens
become underscores), including tools covered by scripts/tests. A test that
mentions a changed file's basename also observes it, covering config/fixtures
and scripts whose test filename differs. A path outside scripts/ and tools/ (a
committed manifest, lockfile or fixture a test reads) selects a test that names
its full repo-relative path, never its basename (`Cargo.toml` is everywhere).
Deleted sources still select tests present in the candidate. Docs/thoughts
paths do not select anything.

Runners: repo unittest suites run under stdlib `python3 -m unittest`, which every
host has. Function-style tools suites keep `python3 -m pytest`; `runner_status`
is the preflight that reports a missing interpreter or module as the typed
`runner_unavailable` (runner identity, `executed_tests: 0`) instead of a
generic exit 1, and the gate never substitutes the other runner.
"""

import argparse
import ast
import importlib.util
import json
from pathlib import Path
import subprocess
import sys

RUNNER_UNAVAILABLE = "runner_unavailable"
PROBE = "import importlib.util, sys; sys.exit(0 if importlib.util.find_spec(sys.argv[1]) else 3)"


SOURCE_ROOTS = ("scripts", "tools")
IGNORED_ROOTS = ("docs", "thoughts")


def changed_paths(repo, base, head):
    result = subprocess.run(
        ["git", "diff", "--name-only", "--no-renames", "-z", base, head],
        cwd=repo, check=True, capture_output=True,
    )
    return [Path(path.decode("utf-8", "surrogateescape"))
            for path in result.stdout.split(b"\0") if path]


def observes(test, text, path):
    """Whether `test` (with source `text`) covers the changed repo-relative `path`."""
    if path.parts[0] in SOURCE_ROOTS:
        return (path == test or test.stem == "test_" + path.stem.replace("-", "_")
                or path.name in text)
    return path.as_posix() in text


def selected_tests(repo, paths):
    paths = [Path(path) for path in paths if Path(path).parts and Path(path).parts[0] not in IGNORED_ROOTS]
    if not paths:
        return []
    tests = sorted(set(path.relative_to(repo) for root in ("scripts/tests", "tools/tests")
                       for path in (repo / root).rglob("test_*.py")))
    selected = []
    for test in tests:
        text = (repo / test).read_text(encoding="utf-8")
        if any(observes(test, text, path) for path in paths):
            selected.append(test.as_posix())
    return selected


def test_commands(repo, tests, python="python3"):
    commands = []
    for test in tests:
        tree = ast.parse((repo / test).read_text(encoding="utf-8"), filename=test)
        unittest = any((isinstance(node, ast.Import) and any(
            alias.name == "unittest" for alias in node.names))
            or (isinstance(node, ast.ImportFrom) and node.module == "unittest")
            for node in ast.walk(tree))
        # Use the stdlib runner for the repo's unittest suites. Pytest-style
        # tools tests require pytest; an unavailable runner is the typed
        # runner_unavailable (runner_status), never a silent unittest fallback.
        commands.append([python, "-m", "unittest" if unittest else "pytest",
                         "-v" if unittest else "-q", test])
    return commands


def command_runner(command):
    """The `-m <module>` runner a planned command names, else None."""
    if "-m" not in command[:-1]:
        return None
    return command[command.index("-m") + 1]


def runner_status(command, run=subprocess.run):
    """Preflight one planned command; None when its runner is available.

    The typed result for a missing interpreter or module never counts as a test
    result: it executed zero tests and cannot be green. A suite that exists but
    fails is the runner's own nonzero exit, not this status.
    """
    runner = command_runner(command)
    if runner is None:
        return None
    unavailable = {"status": RUNNER_UNAVAILABLE, "runner": runner, "python": command[0],
                   "test_file": command[-1], "executed_tests": 0}
    if command[0] == sys.executable:
        # The running interpreter is the runner's interpreter: no child process.
        found = importlib.util.find_spec(runner) is not None
        return None if found else {**unavailable, "reason": f"{command[0]} -m {runner}: No module named {runner}"}
    try:
        probe = run([command[0], "-I", "-c", PROBE, runner], capture_output=True, text=True, timeout=30)
    except (OSError, subprocess.SubprocessError) as error:
        return {**unavailable, "reason": f"interpreter {command[0]!r} cannot run: {error}"}
    if probe.returncode == 0:
        return None
    detail = f"No module named {runner}" if probe.returncode == 3 else f"probe exited {probe.returncode}"
    return {**unavailable, "reason": f"{command[0]} -m {runner}: {detail}"}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--repo", type=Path, default=Path("."))
    parser.add_argument("--base", required=True)
    parser.add_argument("--head", default="HEAD")
    args = parser.parse_args()
    repo = args.repo.resolve()
    tests = selected_tests(repo, changed_paths(repo, args.base, args.head))
    print(json.dumps(test_commands(repo, tests)))


if __name__ == "__main__":
    main()
