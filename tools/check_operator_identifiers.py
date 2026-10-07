#!/usr/bin/env python3
"""Refuse added lines that contain the operator's personal identifiers (#1454).

Handoffs and Issue bodies sometimes quote the operator verbatim, and once a
commit reaches `rolling` and the GitHub mirror, agents cannot rewrite history.
This guard reads the operator's identifiers (e-mail, phone, name, address) from
a private local list and fails when a staged diff, or a commit range about to
be pushed, ADDS a line containing one. It scans `thoughts/` too, which is where
the first leak happened.

The list is operator-owned local config, never committed:
  $RSI_OPERATOR_IDENTIFIERS_FILE, else ~/.rsi/operator-identifiers
One identifier per line, matched case-insensitively as a literal substring;
blank lines and `#` comments are ignored. No file (or an empty one) means the
guard is a no-op, so clones without the list are unaffected.

Findings never echo the identifier itself (that would re-leak it into logs and
commit output); they name the file, line number and list entry number.

Usage:
  tools/check_operator_identifiers.py --staged          # pre-commit
  tools/check_operator_identifiers.py --range A..B      # pre-push / lander
Exit 1 on any finding.
"""
import argparse
import os
import re
import subprocess
import sys
from pathlib import Path

HUNK = re.compile(r"^@@ -\S+ \+(\d+)(?:,\d+)? @@")


def identifiers_path():
    override = os.environ.get("RSI_OPERATOR_IDENTIFIERS_FILE")
    if override:
        return Path(override)
    return Path.home() / ".rsi" / "operator-identifiers"


def load_identifiers(path):
    try:
        text = path.read_text(encoding="utf-8")
    except OSError:
        return []
    out = []
    for raw in text.splitlines():
        line = raw.strip()
        if line and not line.startswith("#"):
            out.append(line.lower())
    return out


def added_lines(diff_text):
    """Yield (path, lineno, text) for every added line of a unified diff."""
    path = None
    lineno = 0
    for line in diff_text.splitlines():
        if line.startswith("+++ "):
            target = line[4:]
            path = None if target == "/dev/null" else target[2:] if target.startswith("b/") else target
            continue
        m = HUNK.match(line)
        if m:
            lineno = int(m.group(1))
            continue
        if path is None or line.startswith(("--- ", "diff ", "index ")):
            continue
        if line.startswith("+"):
            yield path, lineno, line[1:]
            lineno += 1
        elif not line.startswith("-"):
            lineno += 1


def scan(diff_text, identifiers):
    findings = []
    for path, lineno, text in added_lines(diff_text):
        low = text.lower()
        for idx, ident in enumerate(identifiers, 1):
            if ident in low:
                findings.append((path, lineno, idx))
    return findings


def git_diff(args):
    return subprocess.run(
        ["git", "diff", "--no-color", "--no-ext-diff", "--unified=0", *args],
        check=True, capture_output=True,
    ).stdout.decode("utf-8", "replace")


def main(argv):
    ap = argparse.ArgumentParser(description=__doc__.split("\n")[0])
    group = ap.add_mutually_exclusive_group(required=True)
    group.add_argument("--staged", action="store_true")
    group.add_argument("--range", dest="rev_range")
    args = ap.parse_args(argv[1:])

    identifiers = load_identifiers(identifiers_path())
    if not identifiers:
        return 0
    try:
        diff = git_diff(["--cached"] if args.staged else [args.rev_range])
    except subprocess.CalledProcessError:
        # An unreadable range (e.g. remote tip not fetched) must not block work.
        return 0
    findings = scan(diff, identifiers)
    for path, lineno, idx in findings:
        print(f"{path}:{lineno}: operator identifier #{idx} from the local identifiers list")
    if not findings:
        return 0
    print()
    print(f"check-operator-identifiers: {len(findings)} finding(s).")
    print("Do not commit operator personal data. Refer to it as \"the operator's")
    print("<purpose> contact (local config)\" and keep the value out of git, Issue")
    print("bodies and handoffs. Edit the list at", identifiers_path())
    return 1


if __name__ == "__main__":
    sys.exit(main(sys.argv))
