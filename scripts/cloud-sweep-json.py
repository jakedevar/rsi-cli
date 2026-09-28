#!/usr/bin/env python3
"""Write the v1 rolling QA artifact from exact cloud sweep logs."""

import hashlib
import json
import re
import socket
import subprocess
import sys
from pathlib import Path

from qa_host_class import host_class


def command(*args):
    return subprocess.check_output(args, text=True).strip()


def blob(path):
    return command("git", "rev-parse", f"HEAD:{path}")


def status_rows(result):
    rows = {}
    for path in (result / "status").glob("*.tsv"):
        label, code = path.read_text().strip().split("\t")
        rows[label] = int(code)
    return rows


def summary(log):
    matches = re.findall(
        r"Summary \[[^\n]*\] (\d+) tests run: (\d+) passed[^\n]*?, (\d+) skipped",
        log,
    )
    if not matches:
        return 0, 0, 0, 0
    run, passed, skipped = map(int, matches[-1])
    return run, passed, run - passed, skipped


def failures(log):
    return sorted(set(re.findall(
        r"^\s*(?:FAIL|TIMEOUT) \[[^\]]*\] \([^)]*\) \S+ (.+)$", log, re.MULTILINE
    )))


def main():
    sha, started, finished, result_name, runner = sys.argv[1:]
    result = Path(result_name)
    if runner not in {"nextest", "cargo-test"}:
        raise SystemExit("runner must be nextest or cargo-test")
    if runner == "nextest" and command("git", "rev-parse", "HEAD") != sha:
        raise SystemExit("report source HEAD differs from swept SHA")
    report = {
        "schema_version": 1,
        "swept_sha": sha,
        "swept_tree": command("git", "rev-parse", f"{sha}^{{tree}}"),
        "complete": False,
        "provenance": {
            "kind": "qa_sweep",
            "runner": ("scripts/cloud-sweep.sh -> scripts/run-rsid-test-shards.sh shard --jobs 4"
                       if runner == "nextest" else f"scripts/cloud-sweep.sh ({runner})"),
            "host": socket.gethostname(),
            "started_at": started,
            "finished_at": finished,
            "report_ref": str(result / "report.json"),
        },
        "shards": [],
        "lanes": [],
    }
    # Pre-schema cargo-test sweeps have useful QA.md logs but cannot supply a
    # Nextest base fingerprint. Preserve them as partial v1 artifacts only.
    if runner == "cargo-test":
        print(json.dumps(report, indent=2, sort_keys=True))
        return

    rustc = command("rustc", "-Vv")
    cargo = command("cargo", "-V")
    nextest = command("cargo", "nextest", "--version")
    target = next(line.split(": ", 1)[1] for line in rustc.splitlines() if line.startswith("host: "))
    shard_jobs = 4  # cloud-sweep.sh passes --jobs 4 to the shard runner.
    rows = status_rows(result)
    for shard in command(sys.executable, "scripts/check-rsid-test-shards.py", "--list-shards").splitlines():
        label = f"rsid-{shard}"
        if label not in rows:
            continue
        log = (result / "logs" / f"{label}.log").read_text(errors="replace")
        run, passed, failed, skipped = summary(log)
        inputs = {
            "rustc": rustc,
            "cargo": cargo,
            "nextest": nextest,
            "target_triple": target,
            "host_class": host_class(),
            "feature": f"test-shard-{shard}",
            "jobs": shard_jobs,
            "test_threads": shard_jobs,
            "runner_blob": blob("scripts/run-rsid-test-shards.sh"),
            "checker_blob": blob("scripts/check-rsid-test-shards.py"),
            "nextest_config_blob": blob(".config/nextest.toml"),
        }
        digest = hashlib.sha256(json.dumps(inputs, sort_keys=True, separators=(",", ":")).encode()).hexdigest()
        report["shards"].append({
            "name": shard,
            "run": run,
            "passed": passed,
            "failed": failed,
            "skipped": skipped,
            "exit_code": rows[label],
            "failing_tests": failures(log),
            "fingerprint": {"digest": digest, "inputs": inputs},
        })
    lane_labels = {
        "integrations": "rsid-integrations",
        "bins": "rsid-bins",
        "rsid-doc": "rsid-doctests",
        "rsi": "rsi",
        "rsi-common": "rsi-common",
    }
    for name, label in lane_labels.items():
        if label not in rows:
            continue
        log = (result / "logs" / f"{label}.log").read_text(errors="replace")
        names = failures(log)
        if name == "rsid-doc":
            names = sorted(set(names + re.findall(r"^---- ([^\n]+) stdout ----$", log, re.MULTILINE)))
        report["lanes"].append({"name": name, "exit_code": rows[label], "failing_tests": names})
    extra_labels = ("other-workspace", "other-doctests")
    report["extra_lanes"] = []
    for label in extra_labels:
        if label not in rows:
            continue
        log = (result / "logs" / f"{label}.log").read_text(errors="replace")
        names = failures(log)
        if label == "other-doctests":
            names = sorted(set(names + re.findall(r"^---- ([^\n]+) stdout ----$", log, re.MULTILINE)))
        report["extra_lanes"].append({"name": label, "exit_code": rows[label],
                                      "failing_tests": names})
    expected_labels = ({f"rsid-{row['name']}" for row in report["shards"]}
                       | set(lane_labels.values()) | set(extra_labels))
    report["complete"] = (len(report["shards"]) == 16 and len(report["lanes"]) == 5
                          and len(report["extra_lanes"]) == 2 and set(rows) == expected_labels
                          and all(row["run"] > 0 and row["run"] == row["passed"] + row["failed"]
                                  for row in report["shards"]))
    print(json.dumps(report, indent=2, sort_keys=True))


if __name__ == "__main__":
    main()
