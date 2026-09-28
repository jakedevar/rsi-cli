#!/usr/bin/env python3
"""Summarize cloud sweep lane logs as the hub-readable QA.md."""

import re
import sys
from datetime import datetime
from pathlib import Path

sha, started, finished, result_path, seed_path = sys.argv[1:]
result = Path(result_path)
seed_file = Path(seed_path)
seeds = set(seed_file.read_text().splitlines()) if seed_file.exists() else set()
statuses = sorted((result / "status").glob("*.tsv"))
failures = set()
flakes = set()
rows = []
for status_file in statuses:
    label, exit_code = status_file.read_text().strip().split("\t")
    log = (result / "logs" / f"{label}.log").read_text(errors="replace")
    nextest = re.findall(
        r"Summary \[[^\n]*\] (\d+) tests run: (\d+) passed[^\n]*?, (\d+) skipped",
        log,
    )
    if nextest:
        run, passed, skipped = map(int, nextest[-1])
        failed = run - passed
    else:
        counts = [tuple(map(int, match)) for match in re.findall(
            r"test result: (?:ok|FAILED)\. (\d+) passed; (\d+) failed; (\d+) ignored", log
        )]
        passed = sum(item[0] for item in counts)
        failed = sum(item[1] for item in counts)
        skipped = sum(item[2] for item in counts)
    names = re.findall(r"^test (.+?) \.\.\. FAILED$", log, re.MULTILINE)
    names += re.findall(r"^\s*(?:FAIL|TIMEOUT) \[[^\]]*\] \([^)]*\) \S+ (.+)$", log, re.MULTILINE)
    names = sorted(set(names))
    failures.update(names)
    if int(exit_code) and not names:
        failures.add(f"{label}:build_or_harness_failure")
    flakes.update(line.strip() for line in log.splitlines() if re.search(r"\bFLAKE\b|\bflaky\b", line, re.I))
    rows.append((label, passed, failed, skipped, exit_code))

elapsed = (datetime.fromisoformat(finished.replace("Z", "+00:00")) -
           datetime.fromisoformat(started.replace("Z", "+00:00"))).total_seconds() / 60
all_failures = sorted(failures)
print(f"# Cloud QA sweep — {sha}")
print()
print(f"Tip SHA: `{sha}`  ")
print(f"Started: {started}  ")
print(f"Finished: {finished}  ")
print(f"Wall: {elapsed:.1f} min  ")
print(f"Lanes: {len(rows)}  ")
print(f"Failing names needing signature review: {len(all_failures)}")
print()
print("| Lane | Pass | Fail | Skip | Exit |")
print("| --- | ---: | ---: | ---: | ---: |")
for label, passed, failed, skipped, exit_code in rows:
    print(f"| {label} | {passed} | {failed} | {skipped} | {exit_code} |")
print()
print("Seed set: " + (str(seed_file) if seed_file.exists() else "absent; classification unavailable"))
if seed_file.exists():
    print(f"Names absent from seed set: {len(failures - seeds)}; signatures still require review")
print("Unclassified failing names (seed names alone do not verify failure signatures):")
for name in all_failures:
    print(f"- `{name}`")
if not all_failures:
    print("- none")
print("FLAKE lines:")
for line in sorted(flakes):
    print(f"- `{line.replace('`', '')[:300]}`")
if not flakes:
    print("- none observed")
