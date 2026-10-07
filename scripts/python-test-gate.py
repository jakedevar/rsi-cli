#!/usr/bin/env python3
"""Select matching Python tests for scripts/tools changes; never invoke Cargo.

Changed tests select themselves. Script stems match test_<stem>.py (hyphens
become underscores), including tools covered by scripts/tests. A test that
mentions a changed file's basename also observes it, covering config/fixtures
and scripts whose test filename differs. Deleted sources still select tests
present in the candidate. Docs/thoughts paths do not select anything.
"""

import argparse
import ast
import json
from pathlib import Path
import subprocess


def changed_paths(repo, base, head):
    result = subprocess.run(
        ["git", "diff", "--name-only", "--no-renames", "-z", base, head, "--", "scripts/", "tools/"],
        cwd=repo, check=True, capture_output=True,
    )
    return [Path(path.decode("utf-8", "surrogateescape"))
            for path in result.stdout.split(b"\0") if path]


def selected_tests(repo, paths):
    paths = [Path(path) for path in paths if Path(path).parts[0] in ("scripts", "tools")]
    if not paths:
        return []
    tests = sorted(set(path.relative_to(repo) for root in ("scripts/tests", "tools/tests")
                       for path in (repo / root).rglob("test_*.py")))
    selected = []
    for test in tests:
        text = (repo / test).read_text(encoding="utf-8")
        if any(path == test or test.stem == "test_" + path.stem.replace("-", "_")
               or path.name in text for path in paths):
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
        # tools tests require pytest; an unavailable runner refuses the gate.
        commands.append([python, "-m", "unittest" if unittest else "pytest",
                         "-v" if unittest else "-q", test])
    return commands


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
