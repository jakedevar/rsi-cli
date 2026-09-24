#!/usr/bin/env python3
"""Check reviewer-calibration results for finding/case misalignment (Issue #615).

Usage: reviewer-calibration-validate.py <packet-dir> <results.json>...

For every `defect` verdict, the finding's `location` must name a file that the
keyed case's diff actually touches (path-suffix match on component boundaries). A mismatch
means the reviewer filed the finding under the wrong case. Also checks that
each results file covers every case exactly once. Exit 1 on any problem.
"""
import json
import re
import sys
from pathlib import Path


def case_files(packet):
    files = {}
    for diff in sorted(Path(packet).glob("case-*.diff")):
        paths = re.findall(r"^\+\+\+ b/(\S+)$", diff.read_text(), re.M)
        files[diff.stem] = set(paths)
    return files


def _same(token, path):
    """A location token names a case file when one path is a suffix of the
    other on a component boundary (`store/mod.rs` != `app/mod.rs`)."""
    a, b = token.strip("./"), path
    return a == b or b.endswith("/" + a) or a.endswith("/" + b)


def main():
    packet, results = sys.argv[1], sys.argv[2:]
    expected = case_files(packet)
    problems = 0
    for path in results:
        rows = json.loads(Path(path).read_text())
        seen = [r["case"] for r in rows]
        missing = sorted(set(expected) - set(seen))
        dupes = sorted({c for c in seen if seen.count(c) > 1})
        for c in missing:
            print(f"{path}: {c}: no verdict")
        for c in dupes:
            print(f"{path}: {c}: more than one verdict")
        problems += len(missing) + len(dupes)
        for r in rows:
            if r.get("verdict") != "defect":
                continue
            loc = r.get("location") or ""
            named = set(re.findall(r"[\w./-]+\.rs", loc))
            if not named:
                print(f"{path}: {r['case']}: defect without a file location")
                problems += 1
            elif not any(_same(t, p) for t in named for p in expected.get(r["case"], set())):
                print(f"{path}: {r['case']}: location {sorted(named)} not in case files "
                      f"{sorted(expected.get(r['case'], set()))}")
                problems += 1
    print(f"{problems} problem(s)")
    sys.exit(1 if problems else 0)


if __name__ == "__main__":
    main()
