#!/usr/bin/env python3
"""Build a blind reviewer-calibration packet (H3 Layer B / Issue #581).

Usage: reviewer-calibration-packet.py <private-manifest.json> <out-dir>

The manifest (kept OUTSIDE the repo until grading, it is the answer key) is
a list of {"case": "...", "sha": "<commit>", "mode": "seeded" | "control"}.

- seeded:  the commit is a landed bug fix; the packet diff is the REVERSE of
           its production hunks (fixed -> buggy), i.e. it re-introduces the bug.
- control: the commit is an assumed-clean change; the packet diff is its
           forward production hunks.

Test files, lines inside inline `#[cfg(test)]` modules, and changed
comment-only lines are dropped, so neither tests nor the fix's own
explanation can reveal the mode. `index` lines (blob ids) are
stripped. Cases are written in a seeded-random order as case-NN.diff; the
mapping NN -> manifest case is written to <private-manifest>.order.json next
to the manifest, never to <out-dir>.
"""
import json
import os
import random
import re
import subprocess
import sys
from pathlib import Path

CONTEXT = 25
TEST_FILE = re.compile(r"tests?\.rs$|/tests/")
HUNK = re.compile(r"^@@ -(\d+)(?:,\d+)? \+(\d+)(?:,\d+)? @@")


def git(*args):
    return subprocess.run(["git", *args], check=True, capture_output=True, text=True).stdout


def test_ranges(rev, path):
    """1-based inclusive line ranges of inline `#[cfg(test)] mod x { ... }`
    blocks in path at rev (brace-matched; approximate for braces in strings)."""
    try:
        lines = git("show", f"{rev}:{path}").splitlines()
    except subprocess.CalledProcessError:
        return []
    ranges, i = [], 0
    while i < len(lines):
        if lines[i].strip() == "#[cfg(test)]":
            j = next((k for k in range(i + 1, len(lines))
                      if lines[k].strip() and not lines[k].strip().startswith("#[")), None)
            if j is not None and re.match(r"(pub(\(\w+\))? )?mod \w+ \{", lines[j].strip()):
                depth, k = 0, j
                while k < len(lines):
                    depth += lines[k].count("{") - lines[k].count("}")
                    if depth <= 0 and k > j or (depth == 0 and "}" in lines[k]):
                        break
                    k += 1
                ranges.append((i + 1, k + 1))
                i = k + 1
                continue
        i += 1
    return ranges


def prod_files(sha):
    names = git("show", "--format=", "--name-only", sha).split()
    return [f for f in names if f.endswith(".rs") and not TEST_FILE.search(f)]


def filtered_diff(old, new, files, test_rev):
    """Diff old->new over files, dropping every line that falls inside an
    inline test module on the test_rev side, and hunks left with no change.
    Hunk headers keep their original numbers (readable, not git-applicable)."""
    test_is_old = test_rev == old
    out = []
    for path in files:
        raw = git("diff", f"-U{CONTEXT}", old, new, "--", path)
        if not raw.strip():
            continue
        ranges = test_ranges(test_rev, path)
        in_test = lambda n: any(a <= n <= b for a, b in ranges)
        header, hunks, cur = [], [], None
        o = n = 0
        for line in raw.splitlines():
            if line.startswith("index "):
                continue
            m = HUNK.match(line)
            if m:
                o, n = int(m.group(1)), int(m.group(2))
                cur = [line]
                hunks.append(cur)
                continue
            if cur is None:
                header.append(line)
                continue
            tag = line[:1]
            pos = o if test_is_old else n
            # Changed comment-only lines are dropped in every mode: a fix's
            # own explanation, removed by the seeded reversal, would name the
            # defect and inflate recall.
            comment = tag in "+-" and line[1:].lstrip().startswith("//")
            if not in_test(pos) and not comment:
                cur.append(line)
            if tag in (" ", "-"):
                o += 1
            if tag in (" ", "+"):
                n += 1
        keep = [h for h in hunks if any(l[:1] in "+-" for l in h[1:])]
        if keep:
            out.extend(header)
            for body in keep:
                out.extend(body)
    return "\n".join(out) + "\n"


def main():
    manifest_path, out_dir = Path(sys.argv[1]), Path(sys.argv[2])
    cases = json.loads(manifest_path.read_text())
    order = list(range(len(cases)))
    # CAL_SEED varies the case order between packets (run 1 used the default).
    random.Random(int(os.environ.get("CAL_SEED", "20260922"))).shuffle(order)
    out_dir.mkdir(parents=True, exist_ok=True)
    mapping = {}
    for n, idx in enumerate(order, 1):
        c = cases[idx]
        sha = git("rev-parse", c["sha"]).strip()
        files = prod_files(sha)
        if c["mode"] == "seeded":
            diff = filtered_diff(sha, f"{sha}^", files, sha)
        else:
            diff = filtered_diff(f"{sha}^", sha, files, sha)
        name = f"case-{n:02d}"
        (out_dir / f"{name}.diff").write_text(diff)
        mapping[name] = {**c, "sha": sha, "files": files, "diff_lines": diff.count("\n")}
    manifest_path.with_suffix(".order.json").write_text(json.dumps(mapping, indent=2) + "\n")
    print(f"wrote {len(cases)} cases to {out_dir}")


if __name__ == "__main__":
    main()
