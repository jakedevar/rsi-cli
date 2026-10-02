#!/usr/bin/env python3
"""Print a base shard fingerprint, or its sweep-report v1 JSON object."""

import argparse
import hashlib
import json
import os
from pathlib import Path
import re
import subprocess

from qa_host_class import host_class


ROOT = Path(__file__).resolve().parents[1]
SHARD = re.compile(r"^(store|session|memory|other)-[0-9]{2}$")


def output(*command):
    result = subprocess.run(command, cwd=ROOT, text=True, capture_output=True, check=True)
    # Version probes only: rustup-style progress and warnings go to stderr, and on
    # hosts where one is emitted the fingerprint must still match a clean host.
    return result.stdout.strip()


def fingerprint(sha, shard, jobs):
    rustc = output("rustc", "-vV")
    target = next((line[6:].strip() for line in rustc.splitlines() if line.startswith("host: ")), None)
    if not target:
        raise ValueError("rustc -vV did not report a host target triple")
    def blob(path):
        return output("git", "rev-parse", "--verify", f"{sha}:{path}")
    inputs = {
        "rustc": rustc,
        "cargo": output("cargo", "--version"),
        "nextest": output("cargo", "nextest", "--version"),
        "target_triple": os.environ.get("CARGO_BUILD_TARGET") or target,
        "host_class": host_class(),
        "feature": f"test-shard-{shard}",
        "jobs": jobs,
        # The shard runner passes -j jobs to Nextest, overriding its profile default.
        "test_threads": jobs,
        "runner_blob": blob("scripts/run-rsid-test-shards.sh"),
        "checker_blob": blob("scripts/check-rsid-test-shards.py"),
        "nextest_config_blob": blob(".config/nextest.toml"),
    }
    canonical = json.dumps(inputs, sort_keys=True, separators=(",", ":")).encode()
    return {"digest": hashlib.sha256(canonical).hexdigest(), "inputs": inputs}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--sha", required=True)
    parser.add_argument("--shard", required=True)
    parser.add_argument("--jobs", type=int, required=True)
    parser.add_argument("--json", action="store_true")
    args = parser.parse_args()
    if not re.fullmatch(r"[0-9a-f]{40}", args.sha):
        parser.error("--sha must be a full lowercase commit ID")
    if not SHARD.fullmatch(args.shard):
        parser.error("--shard must be a library shard name")
    if not 1 <= args.jobs <= 64:
        parser.error("--jobs must be 1..64")
    result = fingerprint(args.sha, args.shard, args.jobs)
    print(json.dumps(result, sort_keys=True) if args.json else "sha256:" + result["digest"])


if __name__ == "__main__":
    main()
