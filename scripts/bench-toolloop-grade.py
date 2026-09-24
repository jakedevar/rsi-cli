#!/usr/bin/env python3
"""Grade tool-loop fidelity (benchmark #694, task T2) results against a private key.

Usage: bench-toolloop-grade.py <key.json> <results.json>...

key.json: {"pin": "<sha>", "answers": {"Q1": "...", ...}} (kept out of the
repository until grading). Each results file is the arm's
`[{"q":"Q1","answer":"..."}, ...]`. Matching is exact after trimming
whitespace and one layer of surrounding backticks or quotes; integers compare
as strings. An answer listing two names (Q8) matches in either order when
comma-separated. Prints one row per file and exits 1 if any file is malformed.
"""
import json
import sys
from pathlib import Path


def norm(value):
    s = str(value).strip()
    while len(s) >= 2 and s[0] == s[-1] and s[0] in "`'\"":
        s = s[1:-1].strip()
    return s


def matches(got, want):
    g, w = norm(got), norm(want)
    if g == w:
        return True
    if "," in w:
        split = lambda x: sorted(norm(p) for p in x.split(","))
        return split(g) == split(w)
    return False


def grade(key, path):
    rows = json.loads(Path(path).read_text())
    if not isinstance(rows, list):
        raise ValueError("results must be a JSON array")
    got = {}
    for row in rows:
        q = row.get("q")
        if q in got:
            raise ValueError(f"duplicate {q}")
        got[q] = row.get("answer")
    missing = [q for q in key if q not in got]
    extra = [q for q in got if q not in key]
    wrong = [q for q in key if q in got and not matches(got[q], key[q])]
    return len(key) - len(missing) - len(wrong), missing, extra, wrong


def main(argv):
    if len(argv) < 3:
        print(__doc__, file=sys.stderr)
        return 2
    key = json.loads(Path(argv[1]).read_text())["answers"]
    bad = 0
    print("file|correct|total|wrong|missing|extra")
    for path in argv[2:]:
        try:
            ok, missing, extra, wrong = grade(key, path)
        except (ValueError, json.JSONDecodeError, OSError, AttributeError) as err:
            print(f"{path}|MALFORMED|{len(key)}|{err}||")
            bad += 1
            continue
        print(f"{path}|{ok}|{len(key)}|{','.join(wrong)}|{','.join(missing)}|{','.join(extra)}")
    return 1 if bad else 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
