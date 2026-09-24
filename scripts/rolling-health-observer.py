#!/usr/bin/env python3
"""Append one exact-tip, per-crate rolling-health observation to a spool tree.

This observer never pushes. The lander publishes the resulting immutable run
directory to origin/rolling-health using the project's fast-forward policy.
"""

from __future__ import annotations

import argparse
import fcntl
import hashlib
import json
import os
import platform
import re
import subprocess
import sys
import uuid
import xml.etree.ElementTree as ET
from pathlib import Path


def run(command: list[str], *, cwd: Path, env: dict[str, str] | None = None) -> subprocess.CompletedProcess[str]:
    return subprocess.run(command, cwd=cwd, env=env, text=True, stdout=subprocess.PIPE, stderr=subprocess.STDOUT)


def validator(repo: Path, mode: str, data: object | bytes, extra: tuple[str, ...] = ()) -> bytes:
    payload = data if isinstance(data, bytes) else json.dumps(data, separators=(",", ":")).encode()
    completed = subprocess.run(
        ["cargo", "run", "--quiet", "-p", "rsi-common", "--bin", "rsi-rolling-health-validate", "--", mode, *extra],
        cwd=repo, input=payload, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
    )
    if completed.returncode:
        raise RuntimeError(completed.stderr.decode(errors="replace").strip())
    return completed.stdout.strip()


def digest(data: bytes) -> str:
    return "sha256:" + hashlib.sha256(data).hexdigest()


def normalize_failure(value: str, repo: Path, tmpdir: Path) -> str:
    value = value.replace(str(repo), "<REPO>").replace(str(tmpdir), "<TMPDIR>")
    value = re.sub(r"\b\d{4}-\d\d-\d\d[T ][0-9:.+Z-]+", "<TIMESTAMP>", value)
    value = re.sub(r"/tmp/(?:rustc|nextest)[^\s:]*(?:/[A-Za-z0-9_.-]+)*", "<TEMP_PATH>", value)
    return value.strip()


def cargo_packages(repo: Path) -> list[str]:
    result = run(["cargo", "metadata", "--no-deps", "--format-version", "1"], cwd=repo)
    if result.returncode:
        raise RuntimeError(result.stdout)
    metadata = json.loads(result.stdout)
    members = set(metadata["workspace_members"])
    return sorted(package["name"] for package in metadata["packages"] if package["id"] in members)


def test_id(event: dict) -> str:
    info = event.get("nextest") or {}
    name = event.get("name", "")
    if "$" not in name:
        raise ValueError("nextest event lacks stable package/target/test identity")
    if info.get("crate") and info.get("test_binary"):
        package, target = info["crate"], info["test_binary"]
    else:
        package_target = name.split("$", 1)[0]
        if "::" not in package_target:
            raise ValueError("nextest event lacks package and target identity")
        package, target = package_target.split("::", 1)
    return f"{package}::{target}::{name.split('$', 1)[1]}"


def read_junit(path: Path, repo: Path, tmpdir: Path) -> dict[str, str]:
    if not path.exists():
        return {}
    result: dict[str, str] = {}
    root = ET.parse(path).getroot()
    for case in root.iter("testcase"):
        errors = [*case.findall("failure"), *case.findall("error"), *case.findall("flakyFailure")]
        if not errors:
            continue
        detail = "\n".join(filter(None, [
            "\n".join("\n".join(filter(None, [error.get("type"), error.get("message"), error.text])) for error in errors),
            case.findtext("system-out"), case.findtext("system-err"),
        ]))
        result[case.get("name", "")] = normalize_failure(detail, repo, tmpdir)
    return result


def shard_is_complete(result: subprocess.CompletedProcess[str], suites_started: int,
                      suites_ended: int, tests: dict[str, dict]) -> bool:
    if suites_started > 0:
        return suites_started == suites_ended and result.returncode in (0, 100, 101)
    return (
        suites_ended == 0
        and not tests
        and result.returncode == 4
        and "error: no tests to run" in result.stdout
    )


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--repo", type=Path, default=Path.cwd())
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--attempt", type=int, default=1)
    args = parser.parse_args()
    repo = args.repo.resolve()
    args.output = args.output.resolve()
    observer_lock = Path("/tmp/rh-observer.lock").open("w", encoding="utf-8")
    fcntl.flock(observer_lock, fcntl.LOCK_EX)
    run_id = str(uuid.uuid4())
    if args.attempt < 1:
        parser.error("--attempt must be positive")

    fetched = run(["git", "fetch", "origin", "rolling"], cwd=repo)
    if fetched.returncode:
        print(fetched.stdout, file=sys.stderr)
        return 2
    tip = run(["git", "rev-parse", "origin/rolling"], cwd=repo).stdout.strip()
    head = run(["git", "rev-parse", "HEAD"], cwd=repo).stdout.strip()
    if head != tip:
        print(f"observer must execute from fetched origin/rolling tip: HEAD={head} tip={tip}", file=sys.stderr)
        return 2

    packages = cargo_packages(repo)
    profile = {
        "os_image": platform.system().lower(),
        "image_version": platform.release(),
        "architecture": platform.machine(),
        "rust_toolchain": run(["rustc", "--version"], cwd=repo).stdout.strip(),
        "test_command": "cargo nextest run -p <crate> --message-format libtest-json-plus --retries 2",
        "features": [],
        "tmpdir_policy": "short-deterministic",
    }
    profile_id = validator(repo, "--profile-id", profile).decode()
    run_dir = args.output / "observations" / tip / profile_id / f"{run_id}-{args.attempt}"
    run_dir.mkdir(parents=True, exist_ok=False)
    tmpdir = Path("/tmp/rh")
    tmpdir.mkdir(mode=0o700, exist_ok=True)
    shards = [{"crate": name, "state": "pending", "artifact_digest": digest(b"")} for name in packages]
    pending = {
        "schema_version": 1, "kind": "pending", "observed_commit": tip, "profile_id": profile_id,
        "run": {"workflow_ref": "host-observer", "run_id": run_id, "attempt": args.attempt},
        "profile": profile, "shards": shards, "tests": [],
    }
    pending_bytes = validator(repo, "--seal-record", pending)
    (run_dir / "pending.json").write_bytes(pending_bytes + b"\n")
    pending_digest = digest(pending_bytes + b"\n")

    nextest_config = run_dir / "nextest.toml"
    nextest_config.write_text(
        f'[profile.default]\nretries = 2\nfail-fast = false\n[profile.default.junit]\npath = "{run_dir}/junit.xml"\nstore-failure-output = true\n',
        encoding="utf-8",
    )
    env = os.environ.copy()
    env.update({"TMPDIR": str(tmpdir), "NEXTEST_EXPERIMENTAL_LIBTEST_JSON": "1"})
    terminal_shards: list[dict] = []
    all_tests: dict[str, dict] = {}
    complete = True
    for crate in packages:
        junit_path = run_dir / f"{crate}.junit.xml"
        config = nextest_config.read_text(encoding="utf-8").replace(str(run_dir / "junit.xml"), str(junit_path))
        crate_config = run_dir / f"{crate}.nextest.toml"
        crate_config.write_text(config, encoding="utf-8")
        result = run(["nice", "-n", "10", "cargo", "nextest", "run", "-p", crate,
                      "--config-file", str(crate_config), "--message-format", "libtest-json-plus",
                      "--message-format-version", "0.1"], cwd=repo, env=env)
        (run_dir / f"{crate}.log").write_text(result.stdout, encoding="utf-8")
        tests: dict[str, dict] = {}
        suites_started = suites_ended = 0
        for line in result.stdout.splitlines():
            if not line.startswith("{"):
                continue
            try:
                event = json.loads(line)
            except json.JSONDecodeError:
                continue
            if event.get("type") == "suite":
                if event.get("event") == "started":
                    suites_started += 1
                elif event.get("event") in {"ok", "failed"}:
                    suites_ended += 1
            if event.get("type") != "test" or event.get("event") not in {"ok", "failed"}:
                continue
            try:
                identity = test_id(event)
            except ValueError:
                complete = False
                continue
            test = tests.setdefault(identity, {"id": identity, "attempts": 0, "passes": 0,
                                               "failures": 0, "failure_signatures": []})
            test["attempts"] += 1
            if event["event"] == "ok":
                test["passes"] += 1
            else:
                test["failures"] += 1

        failures = read_junit(junit_path, repo, tmpdir)
        for identity, test in tests.items():
            if test["failures"]:
                name = identity.split("::", 2)[-1]
                detail = failures.get(name)
                if detail:
                    signature = digest(("panic\n" + detail).encode())
                    test["failure_signatures"] = [signature]
                else:
                    complete = False
            all_tests[identity] = test

        shard_complete = shard_is_complete(result, suites_started, suites_ended, tests)
        if not shard_complete:
            complete = False
        shard = {"crate": crate, "state": "complete" if shard_complete else "incomplete",
                 "artifact_digest": ""}
        shard_bytes = validator(repo, "--canonicalize", {"crate": crate, "tests": list(tests.values()),
                                                            "exit_code": result.returncode,
                                                            "log_digest": digest(result.stdout.encode())})
        shard_path = run_dir / "shards" / f"{crate}.json"
        shard_path.parent.mkdir(exist_ok=True)
        shard_path.write_bytes(shard_bytes + b"\n")
        shard["artifact_digest"] = digest(shard_bytes + b"\n")
        terminal_shards.append(shard)

    kind = "complete" if complete and all(shard["state"] == "complete" for shard in terminal_shards) else "incomplete"
    terminal = {
        "schema_version": 1, "kind": kind, "observed_commit": tip, "profile_id": profile_id,
        "run": {"workflow_ref": "host-observer", "run_id": run_id, "attempt": args.attempt},
        "profile": profile, "shards": terminal_shards,
        "tests": sorted(all_tests.values(), key=lambda test: test["id"]), "pending_digest": pending_digest,
    }
    sealed = validator(repo, "--seal-record", terminal)
    (run_dir / ("complete.json" if kind == "complete" else "incomplete.json")).write_bytes(sealed + b"\n")
    validator(repo, "--verify-artifacts", sealed, (str(run_dir),))
    print(f"{kind}: {run_dir}")
    return 0 if kind == "complete" else 1


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except (OSError, RuntimeError, ValueError, KeyError) as error:
        print(f"rolling-health observer failed: {error}", file=sys.stderr)
        raise SystemExit(2)
