#!/usr/bin/env python3
"""Import passing shards from a complete sweep-report v1 into the shared base cache."""

import argparse
import fcntl
import hashlib
import json
import os
from pathlib import Path
import re
import subprocess
import tempfile


ROOT = Path(__file__).resolve().parents[1]
SHARDS = {
    "memory-01", "memory-02", "other-01", "other-02", "other-03",
    "other-04", "other-05", "session-01", "session-02", "session-03",
    "session-04", "session-05", "store-01", "store-02", "store-03",
    "store-04",
}
LANES = {"integrations", "bins", "rsid-doc", "rsi", "rsi-common"}
OID = re.compile(r"[0-9a-f]{40}\Z")
DIGEST = re.compile(r"[0-9a-f]{64}\Z")
TEST_NAME = re.compile(r"[A-Za-z0-9_:.-]+\Z")
ENV_KEY = re.compile(r"[A-Za-z_][A-Za-z0-9_]*\Z")
TMPDIR_CLASSES = {"tmpfs", "disk"}


class CacheConflict(ValueError):
    """A complete result disagrees with an existing exact-key cache entry."""


def load_report(path):
    content = path.read_text()
    if path.suffix == ".json":
        report = json.loads(content)
    else:
        match = re.search(r"```qa-sweep-v1\s*\n(.*?)\n```", content, re.DOTALL)
        if not match:
            raise ValueError("Markdown QA report needs a qa-sweep-v1 JSON block")
        report = json.loads(match.group(1))
    if report.get("schema_version") != 1 or report.get("complete") is not True:
        raise ValueError("QA cache import requires a complete sweep-report v1")
    if not OID.fullmatch(report.get("swept_sha", "")) or not OID.fullmatch(report.get("swept_tree", "")):
        raise ValueError("QA swept SHA/tree must be full lowercase Git OIDs")
    rows = report.get("shards", [])
    if len(rows) != 16 or {row.get("name") for row in rows} != SHARDS:
        raise ValueError("QA report must contain all 16 distinct shard results")
    lanes = report.get("lanes", [])
    if len(lanes) != 5 or {row.get("name") for row in lanes} != LANES:
        raise ValueError("QA report must contain all five companion lanes")
    provenance = report.get("provenance", {})
    if provenance.get("kind") != "qa_sweep" or not provenance.get("runner") or not provenance.get("report_ref"):
        raise ValueError("QA report needs sweep provenance")
    return report


def shard_env_class(spec_env, tmpdir_class):
    """Mirror of rsi-rolling-land `shard_env_class` (#988): every spec env
    entry except TMPDIR in key order, then the TMPDIR filesystem class."""
    canonical = "".join(f"{key}={value}\n" for key, value in sorted(spec_env.items()) if key != "TMPDIR")
    canonical += f"TMPDIR={tmpdir_class}\n"
    return "sha256:" + hashlib.sha256(canonical.encode()).hexdigest()


def shard_cache_key(fingerprint, env_class):
    """Mirror of rsi-rolling-land `shard_cache_key`: the shard fingerprint
    folded with the environment class, as the lander looks base entries up."""
    return "sha256:" + hashlib.sha256(f"{fingerprint}\n{env_class}".encode()).hexdigest()


def recorded_env_class(row, shard):
    """The environment class a shard ran under, or None when the report
    predates environment recording (#994) and cannot match any lander key."""
    environment = row.get("environment")
    if environment is None:
        return None
    spec_env = environment.get("spec_env") if isinstance(environment, dict) else None
    tmpdir_class = environment.get("tmpdir_class") if isinstance(environment, dict) else None
    if (not isinstance(spec_env, dict) or tmpdir_class not in TMPDIR_CLASSES
            or any(not isinstance(key, str) or not ENV_KEY.fullmatch(key)
                   or not isinstance(value, str) or "\n" in value
                   for key, value in spec_env.items())):
        raise ValueError(f"invalid QA environment for {shard}")
    return shard_env_class(spec_env, tmpdir_class)


def local_fingerprint(sha, shard, jobs):
    return json.loads(subprocess.check_output(
        ["python3", "scripts/rolling-shard-fingerprint.py", "--sha", sha,
         "--shard", shard, "--jobs", str(jobs), "--json"], cwd=ROOT, text=True
    ))


def local_tree(sha):
    return subprocess.check_output(
        ["git", "rev-parse", "--verify", f"{sha}^{{tree}}"], cwd=ROOT, text=True
    ).strip()


def cache_root():
    return Path(os.environ.get("RSI_ROLLING_BASE_CACHE_DIR", Path.home() / ".rsi/cache/rolling-base-shards-v1"))


def import_report(path, root=None):
    report = load_report(path)
    tip = report["swept_sha"]
    if local_tree(tip) != report["swept_tree"]:
        raise ValueError("QA swept tree does not match the reported commit")
    root = root or cache_root()
    results = []
    provenance = report["provenance"]
    source = f"qa:{provenance['runner']}:{provenance['report_ref']}"
    prepared = []
    for row in sorted(report["shards"], key=lambda item: item["name"]):
        shard = row["name"]
        names = row.get("failing_tests")
        if not isinstance(names, list) or any(not isinstance(name, str) for name in names):
            raise ValueError(f"QA failing tests missing for {shard}")
        names = [name.removeprefix("rsid ") for name in names]
        if len(set(names)) != len(names) or any(not TEST_NAME.fullmatch(name) for name in names):
            raise ValueError(f"QA failure names invalid for {shard}")
        passed, failed, run = row.get("passed"), row.get("failed"), row.get("run")
        if any(not isinstance(value, int) or value < 0 for value in (passed, failed, run)) or run != passed + failed or failed != len(names):
            raise ValueError(f"QA test counts do not match failures for {shard}")
        if (failed == 0 and row.get("exit_code") != 0) or (failed > 0 and row.get("exit_code") != 100):
            results.append((shard, "abnormal_skipped"))
            continue
        fingerprint = row.get("fingerprint", {})
        inputs = fingerprint.get("inputs", {})
        digest = fingerprint.get("digest", "")
        if not DIGEST.fullmatch(digest):
            raise ValueError(f"invalid QA fingerprint for {shard}")
        canonical = json.dumps(inputs, sort_keys=True, separators=(",", ":")).encode()
        if hashlib.sha256(canonical).hexdigest() != digest or inputs.get("feature") != f"test-shard-{shard}":
            raise ValueError(f"QA fingerprint inputs do not match {shard}")
        jobs = inputs.get("jobs")
        if not isinstance(jobs, int) or not 1 <= jobs <= 64:
            raise ValueError(f"invalid QA jobs for {shard}")
        local = local_fingerprint(tip, shard, jobs)
        if inputs.get("host_class") != local.get("inputs", {}).get("host_class"):
            raise ValueError(f"QA host class does not match this host for {shard}")
        if local != fingerprint:
            raise ValueError(f"QA runner/toolchain fingerprint does not match this host for {shard}")
        env_class = recorded_env_class(row, shard)
        if env_class is None:
            results.append((shard, "environment_unrecorded"))
            continue
        prepared.append((shard, shard_cache_key("sha256:" + digest, env_class), sorted(names)))
    for shard, key, failures in prepared:
        directory = root / tip / key.removeprefix("sha256:")
        directory.mkdir(parents=True, exist_ok=True, mode=0o700)
        target = directory / f"{shard}.json"
        entry = {
            "schema_version": 1, "rolling_sha": tip, "shard": shard,
            "fingerprint": key, "failures": failures, "provenance": source,
        }
        with (directory / f"{shard}.lock").open("a+b") as lock:
            fcntl.flock(lock, fcntl.LOCK_EX)
            if target.exists():
                previous = json.loads(target.read_text())
                if (previous.get("rolling_sha"), previous.get("shard"), previous.get("fingerprint"), previous.get("failures")) != (tip, shard, entry["fingerprint"], failures):
                    raise CacheConflict(f"conflicting cached base result for {tip} {shard}")
                results.append((shard, "existing"))
                continue
            with tempfile.NamedTemporaryFile("w", dir=directory, delete=False) as temporary:
                temporary_name = temporary.name
                json.dump(entry, temporary, sort_keys=True)
                temporary.flush()
                os.fsync(temporary.fileno())
            try:
                os.replace(temporary_name, target)
            finally:
                if os.path.exists(temporary_name):
                    os.unlink(temporary_name)
            results.append((shard, "imported"))
    return tip, results


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("report", type=Path)
    args = parser.parse_args()
    tip, rows = import_report(args.report)
    for shard, state in rows:
        print(f"qa_cache_{state}={tip}:{shard}")


if __name__ == "__main__":
    main()
