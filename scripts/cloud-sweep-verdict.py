#!/usr/bin/env python3
"""Append a mechanical verdict to a collected cloud sweep's QA.md (#1016).

    cloud-sweep-verdict.py RESULT_DIR [--snapshot FILE] [--classifier BIN]

Runs after `cloud-sweep.sh collect`, on the host that ran it (the hub), where the known-failure
snapshot lives. Each failing test is classified with the same
`rsi-known-failure classify` CLI and snapshot as scripts/run-rolling-qa.py,
over the full lane logs collect brings back (failure-logs/LANE.log.gz, or the
LANE.txt excerpt when a full log is absent). The last line of QA.md is:

    VERDICT GREEN <sha> new=0                  every red is KNOWN
    VERDICT RED <sha> new=<n>                  at least one NEW red (n >= 1)
    VERDICT INCOMPLETE <sha> new=<n>           a lane never ran/built

That line is the machine grammar the daemon's cloud_sweep job parses word for
word (#1077); human notes such as "Known only: #a, #b" go on other lines.

A missing classifier or snapshot is stated and never crashes: reds stay
unclassified and the verdict is RED (unknown reds are never green).
"""

import argparse
import gzip
import os
from pathlib import Path
import re
import shutil
import subprocess
import sys
import tempfile

DEFAULT_SNAPSHOT = Path.home() / ".rsi/qa/known-failures.v1.json"
HARNESS = re.compile(r"^- `([^`]+):build_or_harness_failure`$", re.MULTILINE)
NAME = re.compile(r"^- `([^`]+)`$", re.MULTILINE)
# `- `name` [crash]` lines cloud-sweep-report.py writes for crashed (SIGABRT,
# SIGSEGV) and timed-out tests (#1120).
CLASS = re.compile(r"^- `([^`]+)` \[(crash|timeout)\]$", re.MULTILINE)


def find_classifier(explicit=None):
    """The shared classifier binary, or None. Never builds it."""
    candidates = [explicit, os.environ.get("RSI_KNOWN_FAILURE_BIN")]
    for path in candidates:
        if path:
            return Path(path) if Path(path).is_file() else None
    found = shutil.which("rsi-known-failure")
    if found:
        return Path(found)
    targets = [os.environ.get("CARGO_TARGET_DIR"),
               str(Path.home() / ".cargo/shared-target")]
    for target in filter(None, targets):
        binary = Path(target) / "debug/rsi-known-failure"
        if binary.is_file():
            return binary
    return None


def parse_classification(lines):
    """`KNOWN #n class test` / `KNOWN? #a,#b test` / `NEW test` per test."""
    matches = {}
    for line in lines:
        line = line.strip()
        if line.startswith("KNOWN?"):
            parts = line.split(None, 2)
            if len(parts) == 3:
                matches[parts[2]] = ("name-only", parts[1])
        elif line.startswith("KNOWN "):
            parts = line.split(None, 3)
            if len(parts) == 4:
                matches[parts[3]] = ("known", parts[1])
        elif line.startswith("NEW "):
            matches[line[4:].strip()] = ("new", "")
    return matches


def lane_logs(result):
    """Yield (lane, path) for each failed lane's log, full log preferred."""
    logs = result / "failure-logs"
    seen = {}
    for path in sorted(logs.glob("*.log.gz")):
        seen[path.name[: -len(".log.gz")]] = path
    for path in sorted(logs.glob("*.txt")):
        seen.setdefault(path.stem, path)
    return seen


def classify_logs(classifier, snapshot, result):
    """Classify every collected lane log. Raises OSError/CalledProcessError."""
    matches = {}
    with tempfile.TemporaryDirectory() as scratch:
        for lane, path in lane_logs(result).items():
            log = Path(scratch) / f"{lane}.log"
            if path.suffix == ".gz":
                with gzip.open(path, "rb") as source, open(log, "wb") as target:
                    shutil.copyfileobj(source, target)
            else:
                shutil.copyfile(path, log)
            output = subprocess.run(
                [str(classifier), "classify", "--log", str(log), "--snapshot", str(snapshot)],
                check=True, capture_output=True, text=True).stdout
            matches.update(parse_classification(output.splitlines()))
    return matches


def build_verdict(qa_text, sha, classify):
    """Return (section lines, verdict line). `classify` is ("ok", {name:
    (state, issue)}) or ("unavailable", reason)."""
    lanes = re.search(r"^Lanes: (\d+)", qa_text, re.MULTILINE)
    harness = HARNESS.findall(qa_text)
    section = ["", "## Verdict", ""]
    # Only the failing-names block: NAME would also match FLAKE lines.
    block = re.search(r"^Unclassified failing names[^\n]*\n(.*?)^FLAKE lines:",
                      qa_text, re.MULTILINE | re.DOTALL)
    names = [n for n in NAME.findall(block.group(1))
             if not n.endswith(":build_or_harness_failure") and n != "none"] if block else []
    incomplete = (not lanes or int(lanes.group(1)) == 0 or bool(harness))
    if incomplete:
        reasons = ["Lanes: 0" if not lanes or int(lanes.group(1)) == 0 else ""]
        reasons += [f"{lane}:build_or_harness_failure" for lane in harness]
        section.append("Incomplete: " + ", ".join(r for r in reasons if r))
    state, matches = classify
    if state != "ok":
        section.append(f"Classification unavailable: {matches}; reds are unclassified.")
        matches = {}
    classes = dict(CLASS.findall(qa_text))
    known, new = {}, []
    for name in names:
        tag = f" [{classes[name]}]" if name in classes else ""
        kind, issue = matches.get(name, ("new", ""))
        if kind == "known":
            known[name] = issue
            section.append(f"- KNOWN {issue} `{name}`{tag}")
        else:
            new.append(name)
            note = f" (name-only match {issue}; signature unverified)" if kind == "name-only" else ""
            section.append(f"- NEW `{name}`{tag}{note}")
    if not names:
        section.append("- no failing test names")
    crashed = [name for name in new if name in classes]
    if crashed:
        section.append(f"Crashed or timed out (NEW): {len(crashed)}; the verdict is RED, not INCOMPLETE.")
    if incomplete:
        verdict = f"VERDICT INCOMPLETE {sha} new={len(new)}"
    elif new:
        verdict = f"VERDICT RED {sha} new={len(new)}"
    else:
        issues = sorted(set(known.values()), key=lambda v: int(v.lstrip("#") or 0))
        if issues:
            section.append(f"Known only: {', '.join(issues)}")
        verdict = f"VERDICT GREEN {sha} new=0"
    return section, verdict


def main(argv=None):
    parser = argparse.ArgumentParser()
    parser.add_argument("result_dir", type=Path)
    parser.add_argument("--snapshot", type=Path, default=DEFAULT_SNAPSHOT)
    parser.add_argument("--classifier")
    args = parser.parse_args(argv)
    qa = args.result_dir / "QA.md"
    qa_text = qa.read_text(errors="replace") if qa.exists() else ""
    head = re.search(r"Tip SHA: `([^`]+)`", qa_text)
    sha = head.group(1) if head else args.result_dir.name
    classified = ("unavailable", "")
    try:
        classifier = find_classifier(args.classifier)
        if classifier is None:
            classified = ("unavailable", "rsi-known-failure CLI not found")
        elif not args.snapshot.is_file():
            classified = ("unavailable", f"snapshot {args.snapshot} missing")
        else:
            classified = ("ok", classify_logs(classifier, args.snapshot, args.result_dir))
    except (OSError, subprocess.CalledProcessError, gzip.BadGzipFile) as error:
        classified = ("unavailable", f"classifier failed: {error}")
    section, verdict = build_verdict(qa_text, sha, classified)
    with qa.open("a") as handle:
        handle.write("\n".join(section) + "\n\n" + verdict + "\n")
    print("\n".join(section) + "\n\n" + verdict)
    return 0


if __name__ == "__main__":
    sys.exit(main())
