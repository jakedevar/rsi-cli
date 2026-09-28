#!/usr/bin/env python3
"""Run every QA lane against the exact fetched rolling tree; never publish refs."""

import argparse
from datetime import datetime, timezone
import json
from pathlib import Path
import platform
import re
import subprocess
import sys
import time


ROOT = Path(__file__).resolve().parents[1]
SLOT = Path.home() / ".rsi/bin/cargo-slot"
NEXTTEST_FAILURE = re.compile(
    r"^\s*(?:FAIL|SIG[A-Z0-9]+) \[[^]]+\] \([^)]*\) (.+)$", re.MULTILINE
)
CARGO_FAILURE = re.compile(r"^test (.+) \.\.\. FAILED$", re.MULTILINE)
SUMMARY = re.compile(r"^\s*Summary \[[^]]+\] .+$", re.MULTILINE)
SUMMARY_COUNTS = re.compile(r"^\s*Summary \[[^]]+\] (\d+) tests run: (.+)$", re.MULTILINE)


def failures(output):
    return sorted(set(NEXTTEST_FAILURE.findall(output) + CARGO_FAILURE.findall(output)))


def git(*args):
    return subprocess.check_output(["git", *args], cwd=ROOT, text=True).strip()


def shard_fingerprint(tip, jobs, shard="store-01"):
    return json.loads(subprocess.check_output(
        [sys.executable, "scripts/rolling-shard-fingerprint.py", "--sha", tip,
         "--shard", shard, "--jobs", str(jobs), "--json"], cwd=ROOT, text=True
    ))


def shard_counts(summary):
    match = SUMMARY_COUNTS.search("\n".join(summary))
    if not match:
        return None
    run = int(match.group(1))
    detail = match.group(2)
    def count(word):
        found = re.search(rf"(\d+) {word}", detail)
        return int(found.group(1)) if found else 0
    passed, failed, skipped = count("passed"), count("failed"), count("skipped")
    if run != passed + failed:
        return None
    return {"run": run, "passed": passed, "failed": failed, "skipped": skipped}


def rpc(method, params):
    output = subprocess.check_output(
        ["rsi-rpc", method, "--params", json.dumps(params)], cwd=ROOT, text=True
    )
    envelope = json.loads(output)
    if "error" in envelope:
        raise RuntimeError(f"{method}: {envelope['error']}")
    return envelope["result"]


def closed_issues_since(since):
    cutoff = datetime.fromisoformat(since.replace("Z", "+00:00")) if since else None
    cursor = None
    issues = []
    while True:
        params = {"status": "Closed", "archive": "All", "limit": 256}
        if cursor:
            params["cursor"] = cursor
        page = rpc("AgentListIssues", params)
        for row in page["issues"]:
            closed_at = row.get("closed_at")
            if not closed_at or (cutoff and datetime.fromisoformat(closed_at.replace("Z", "+00:00")) <= cutoff):
                continue
            detail = rpc("AgentGetIssue", {"issue_id": row["id"]})
            issues.append({"id": row["id"], "number": row["display_number"],
                           "title": row["title"], "closed_at": closed_at,
                           "body": detail["body"]})
        cursor = page.get("next_cursor")
        if not cursor:
            return issues


def run(label, argv, directory):
    log = directory / f"{label}.log"
    started = time.monotonic()
    print(f"running {label}: {' '.join(map(str, argv))}", flush=True)
    with log.open("w") as sink:
        process = subprocess.Popen(
            [str(part) for part in argv], cwd=ROOT, stdout=subprocess.PIPE,
            stderr=subprocess.STDOUT, text=True, bufsize=1,
        )
        assert process.stdout is not None
        for line in process.stdout:
            sink.write(line)
            sys.stdout.write(line)
        code = process.wait()
    output = log.read_text(errors="replace")
    result = {"label": label, "exit": code, "seconds": round(time.monotonic() - started, 3),
              "log": str(log.relative_to(ROOT)), "summary": SUMMARY.findall(output)[-1:] or [],
              "failures": failures(output)}
    print(f"finished {label}: exit={code} failures={len(result['failures'])}", flush=True)
    return result


def save_state(directory, state):
    pending = directory / "state.json.pending"
    pending.write_text(json.dumps(state, indent=2) + "\n")
    pending.replace(directory / "state.json")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--evidence-dir", type=Path, required=True)
    parser.add_argument("--jobs", type=int, default=4)
    parser.add_argument("--closed-since", help="RFC3339 time of previous QA sweep")
    parser.add_argument("--resume", action="store_true", help="reuse completed lanes after interruption")
    args = parser.parse_args()
    if not 1 <= args.jobs <= 4:
        parser.error("--jobs must be 1..4")
    directory = (ROOT / args.evidence_dir).resolve()
    if not directory.is_relative_to(ROOT / "target"):
        parser.error("evidence directory must be under this sandbox's target/")
    git("fetch", "origin")
    if args.resume:
        state = json.loads((directory / "state.json").read_text())
        if state["jobs"] != args.jobs or state["closed_since"] != args.closed_since:
            parser.error("resume arguments differ from recorded sweep")
        tip = state["tip"]
        tree = state["tree"]
        if state.get("fingerprint") != shard_fingerprint(tip, args.jobs):
            parser.error("runner/toolchain fingerprint differs from recorded sweep")
    else:
        tip = git("rev-parse", "origin/rolling")
        tree = git("rev-parse", "origin/rolling^{tree}")
        if git("rev-parse", "HEAD^{tree}") != tree:
            parser.error("HEAD tree differs from fetched origin/rolling; integrate first")
        directory.mkdir(parents=True, exist_ok=False)
        state = {"tip": tip, "tree": tree, "jobs": args.jobs,
                 "started_at": datetime.now(timezone.utc).isoformat(),
                 "fingerprint": shard_fingerprint(tip, args.jobs),
                 "closed_since": args.closed_since, "results": []}
        save_state(directory, state)
    if git("rev-parse", "HEAD^{tree}") != tree:
        parser.error("HEAD tree differs from recorded sweep; restore that exact tree to resume")
    if subprocess.call(["git", "diff", "--quiet"], cwd=ROOT) or subprocess.call(
        ["git", "diff", "--cached", "--quiet"], cwd=ROOT
    ):
        parser.error("tracked worktree changes would contaminate the QA sweep")
    results = state["results"]

    def record(label, command):
        prior = next((row for row in results if row["label"] == label), None)
        if prior:
            print(f"reusing completed {label}: exit={prior['exit']}", flush=True)
            return prior
        result = run(label, command, directory)
        results.append(result)
        save_state(directory, state)
        return result

    shards = []
    static = record("static", [sys.executable, "scripts/check-rsid-test-shards.py", "--require-gates"])
    if static["exit"] != 0:
        print("static gate failed; shard execution is unsafe", file=sys.stderr)
    else:
        shards = subprocess.check_output(
            [sys.executable, "scripts/check-rsid-test-shards.py", "--list-shards"],
            cwd=ROOT, text=True,
        ).splitlines()
        if len(shards) != 16:
            raise RuntimeError(f"expected 16 shards, got {len(shards)}")
        for shard in shards:
            shard_dir = directory / shard
            if shard_dir.exists() and not any(row["label"] == shard for row in results):
                shard_dir = directory / f"{shard}-retry-{time.time_ns()}"
            record(shard, [SLOT, "scripts/run-rsid-test-shards.sh", "shard", shard,
                   "--jobs", args.jobs, "--evidence-dir", shard_dir])
        integrations = sorted(path.stem for path in (ROOT / "crates/rsid/tests").glob("*.rs"))
        record("integrations", [SLOT, "cargo", "nextest", "run", "--profile", "rsid-fast",
               "-p", "rsid", *(part for name in integrations for part in ("--test", name)),
               "--status-level", "all", "--final-status-level", "all", "-j", args.jobs])
        record("bins", [SLOT, "cargo", "nextest", "run", "--profile", "rsid-fast",
               "-p", "rsid", "--bins", "--status-level", "all", "--final-status-level", "all",
               "-j", args.jobs])
        for label, command in [
            ("rsid-doc", ["cargo", "test", "-p", "rsid", "--doc", "--", f"--test-threads={args.jobs}"]),
            ("rsi", ["cargo", "test", "-p", "rsi", "--lib", "--", f"--test-threads={args.jobs}"]),
            ("rsi-common", ["cargo", "test", "-p", "rsi-common", "--", f"--test-threads={args.jobs}"]),
        ]:
            record(label, [SLOT, *command])
    git("fetch", "origin")
    final_tip = git("rev-parse", "origin/rolling")
    report = {"tip": tip, "final_rolling": final_tip, "tip_stable": tip == final_tip,
              "jobs": args.jobs, "fingerprint": state["fingerprint"],
              "sweep_finished_at": datetime.now(timezone.utc).isoformat(),
              "closed_since": args.closed_since, "closed_issues": closed_issues_since(args.closed_since),
              "results": results}
    (directory / "report.json").write_text(json.dumps(report, indent=2) + "\n")
    shard_rows = []
    for row in results:
        if row["label"] not in shards:
            continue
        counts = shard_counts(row["summary"])
        shard_rows.append({"name": row["label"], **(counts or {"run": 0, "passed": 0, "failed": 0, "skipped": 0}),
                           "exit_code": row["exit"], "failing_tests": row["failures"],
                           "fingerprint": shard_fingerprint(tip, args.jobs, row["label"])})
    lane_names = {"integrations", "bins", "rsid-doc", "rsi", "rsi-common"}
    lane_rows = [{"name": row["label"], "exit_code": row["exit"],
                  "failing_tests": row["failures"]}
                 for row in results if row["label"] in lane_names]
    complete = (static["exit"] == 0 and len(shard_rows) == 16 and len(lane_rows) == 5
                and all(shard_counts(row["summary"]) is not None
                        for row in results if row["label"] in shards))
    sweep_report = {
        "schema_version": 1, "swept_sha": tip, "swept_tree": tree,
        "complete": complete,
        "provenance": {"kind": "qa_sweep", "runner": "scripts/run-rolling-qa.py",
                       "host": platform.node(), "started_at": state["started_at"],
                       "finished_at": report["sweep_finished_at"],
                       "report_ref": str((directory / "sweep-report.json").relative_to(ROOT))},
        "shards": shard_rows, "lanes": lane_rows,
    }
    (directory / "sweep-report.json").write_text(json.dumps(sweep_report, indent=2) + "\n")
    print(f"report: {directory / 'report.json'}")
    return 0 if all(row["exit"] == 0 for row in results) else 1


if __name__ == "__main__":
    sys.exit(main())
