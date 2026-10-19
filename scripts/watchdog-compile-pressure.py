#!/usr/bin/env python3
"""Linux-only, opt-in experiment: AgentSubmitJob test recipe watchdog-pressure.

No daemon startup or cgroup writes. The job's CPU/memory policy is inherited.
Scratch targets stay inside the designated sandbox target; the stamped rustc
wrapper and sccache settings remain intact. Only rsid is cleaned between phases.
Results/logs go to /tmp, never target's top level. Total runtime is bounded to
18 minutes, leaving the ordinary 20-minute job time for teardown.
"""

import json
import math
import os
from pathlib import Path
import shutil
import signal
import subprocess
import sys
import tempfile
import time


def read(path):
    try:
        return Path(path).read_text().strip()
    except OSError:
        return None


def snapshot(group):
    return {name: read(group / name) for name in (
        "memory.current", "memory.peak", "memory.high", "memory.max",
        "memory.events", "memory.pressure", "cpu.max", "cpu.stat",
    )}


def percentile(values, p):
    return sorted(values)[max(0, math.ceil(len(values) * p) - 1)] if values else None


def summarize(rows):
    result = {}
    for phase in sorted({row["phase"] for row in rows}):
        selected = [row for row in rows if row["phase"] == phase]
        result[phase] = {"samples": len(selected)}
        for probe in ("rpc", "store"):
            values = [row[probe + "_ms"] for row in selected]
            result[phase][probe] = {
                "p50_ms": percentile(values, .50), "p95_ms": percentile(values, .95),
                "p99_ms": percentile(values, .99), "max_ms": max(values),
                "deadline_misses": sum(row[probe + "_ms"] >= row["deadline_ms"]
                                       or (probe == "store" and row["store_outcome"] == "TimedOut")
                                       for row in selected),
                "errors": sum(not row["rpc_ok"] if probe == "rpc"
                              else row["store_outcome"] != "Responsive" for row in selected),
            }
    return result


def main():
    if sys.platform != "linux":
        raise SystemExit("unsupported: cgroup-v2 measurement requires Linux")
    root = Path(__file__).resolve().parent.parent
    relative = read("/proc/self/cgroup").split("0::", 1)[1].strip()
    if "rsi-build.slice/" not in relative or "rsi-job-" not in relative:
        raise SystemExit("run only inside AgentSubmitJob recipe watchdog-pressure")
    group = Path("/sys/fs/cgroup") / relative.lstrip("/")
    slice_group = next(p for p in group.parents if p.name == "rsi-build.slice")
    target = Path(os.environ["CARGO_TARGET_DIR"]).resolve()
    if not target.is_relative_to(root) or target == root:
        raise SystemExit("designated target must be inside this sandbox")
    wrapper = os.environ.get("RUSTC_WRAPPER")
    if not wrapper or "unlink-outputs" not in subprocess.check_output(
            [wrapper, "--capabilities"], text=True):
        raise SystemExit("hardlink seeds require the stamped unlink-outputs wrapper")
    out = Path(tempfile.mkdtemp(prefix="rsi-watchdog-pressure-", dir="/tmp"))
    scratch = Path(tempfile.mkdtemp(prefix="watchdog-pressure-", dir=target))
    print(f"PRESSURE_RESULTS {out}", flush=True)
    started = time.monotonic()
    deadline = started + 1080
    children = []
    handles = []
    samples = []
    phases = []
    result = {
        "source_head": subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=root, text=True).strip(),
        "host_meminfo_before": read("/proc/meminfo"),
        "cgroup": relative, "before": snapshot(group), "slice_before": snapshot(slice_group),
        "ancestor_limits": {str(p): {key: read(p / key) for key in ("memory.high", "memory.max")}
                            for p in [group, *group.parents] if (p / "memory.max").exists()},
        "cargo_build_jobs": os.environ.get("CARGO_BUILD_JOBS"),
        "rustc_wrapper_slots": os.environ.get("RSI_BUILD_SLOTS"),
        "phases": phases, "fixture": "production watchdog probes; warm temporary Store; minimal RPC listener",
        "scratch": str(scratch), "logs": str(out),
        "fixture_database_filesystem": subprocess.check_output(
            ["stat", "-f", "-c", "%T", str(scratch)], text=True).strip(),
    }
    (out / "started.json").write_text(json.dumps(result, indent=2) + "\n")

    def launch(argv, log, env=None):
        handle = open(out / log, "w")
        handles.append(handle)
        child = subprocess.Popen(argv, cwd=root, env=env, stdout=handle, stderr=subprocess.STDOUT,
                                 start_new_session=True)
        children.append(child)
        return child

    def tick():
        if time.monotonic() >= deadline:
            raise TimeoutError("18-minute experiment budget exhausted")
        procs = (read(group / "cgroup.procs") or "").split()
        rustc = []
        for pid in procs:
            cmd = read(f"/proc/{pid}/cmdline") or ""
            if Path(cmd.split("\0")[0]).name == "rustc":
                status = read(f"/proc/{pid}/status") or ""
                rss = next((int(line.split()[1]) * 1024 for line in status.splitlines()
                            if line.startswith("VmRSS:")), 0)
                rustc.append({"pid": int(pid), "rss_bytes": rss,
                              "rsid": "--crate-name\0rsid\0" in cmd})
        samples.append({"elapsed_s": time.monotonic() - started,
                        "phase": read(out / "phase"), "rustc": rustc,
                        "memory_current": int(read(group / "memory.current")),
                        "slice_memory_current": int(read(slice_group / "memory.current"))})
        with (out / "memory.jsonl").open("a") as memory_log:
            memory_log.write(json.dumps(samples[-1]) + "\n")
        time.sleep(1)

    cargo = ["cargo", "test", "--locked", "-p", "rsid", "--lib", "--no-run",
             "--features", "test-shard-other-03"]
    fixture = None
    try:
        (out / "phase").write_text("preparation")
        prep = launch(cargo + ["--message-format=json"], "prepare.log")
        while prep.poll() is None:
            tick()
        if prep.returncode:
            raise RuntimeError(f"fixture compile exit {prep.returncode}")
        binaries = []
        for line in (out / "prepare.log").read_text().splitlines():
            if line.startswith("{"):
                message = json.loads(line)
                if message.get("executable") and message.get("target", {}).get("name") == "rsid":
                    binaries.append(message["executable"])
        fixture_env = dict(os.environ, RSI_PRESSURE_FIXTURE=str(out),
                           RSI_PRESSURE_DATABASE=str(scratch / "probe.sqlite"))
        fixture = launch([binaries[-1], "--exact", "watchdog::tests::compile_pressure_probe_fixture",
                          "--ignored", "--nocapture"], "fixture.log", fixture_env)
        while not (out / "ready").exists():
            if fixture.poll() is not None:
                raise RuntimeError("fixture exited before ready")
            tick()
        (out / "phase").write_text("baseline")
        for _ in range(10):
            tick()
        for concurrency in (1, 2):
            targets = []
            for index in range(concurrency):
                dest = scratch / f"phase-{concurrency}-{index}"
                dest.mkdir()
                # Hardlinks are safe only behind the wrapper capability checked above.
                subprocess.run(["cp", "-al", str(target / "debug"), str(dest / "debug")], check=True,
                               timeout=max(1, deadline - time.monotonic()))
                # Cargo flocks this inode. A hardlinked lock serializes otherwise
                # independent targets and invalidates the concurrency experiment.
                (dest / "debug" / ".cargo-lock").unlink(missing_ok=True)
                subprocess.run(["cargo", "clean", "-p", "rsid", "--target-dir", str(dest)],
                               cwd=root, check=True, timeout=max(1, deadline - time.monotonic()),
                               stdout=subprocess.DEVNULL)
                targets.append(dest)
            name = f"compile-{concurrency}"
            (out / "phase").write_text(name)
            phase = {"name": name, "concurrency": concurrency, "start_s": time.monotonic() - started}
            phases.append(phase)
            builds = [launch(cargo + ["--target-dir", str(dest)], f"{name}-{index}.log")
                      for index, dest in enumerate(targets)]
            while any(child.poll() is None for child in builds):
                if fixture.poll() is not None:
                    raise RuntimeError("probe fixture exited during compiles")
                tick()
            phase.update(exit_codes=[child.returncode for child in builds],
                         duration_s=time.monotonic() - started - phase["start_s"])
            if any(phase["exit_codes"]):
                break
            # Unlink only this experiment's targets, never the designated target.
            for dest in targets:
                shutil.rmtree(dest)
        result["status"] = "complete"
    except (Exception, KeyboardInterrupt) as error:
        result["status"] = "incomplete"
        result["error"] = str(error)
    finally:
        (out / "stop").touch()
        if fixture is not None:
            try:
                fixture.wait(timeout=12)
            except subprocess.TimeoutExpired:
                pass
        for child in children:
            if child.poll() is None:
                os.killpg(child.pid, signal.SIGTERM)
        for child in children:
            try:
                child.wait(timeout=5)
            except subprocess.TimeoutExpired:
                os.killpg(child.pid, signal.SIGKILL)
                child.wait()
        for handle in handles:
            handle.close()
        rows = [json.loads(line) for line in (read(out / "probes.jsonl") or "").splitlines()]
        result.update(after=snapshot(group), slice_after=snapshot(slice_group),
                      duration_s=time.monotonic() - started, probes=summarize(rows),
                      peak_sampled_job_memory=max((s["memory_current"] for s in samples), default=0),
                      peak_sampled_rustc_rss=max((sum(p["rss_bytes"] for p in s["rustc"])
                                                  for s in samples), default=0),
                      max_simultaneous_rsid_rustc=max((sum(p["rsid"] for p in s["rustc"])
                                                     for s in samples), default=0))
        (out / "memory.jsonl").write_text("".join(json.dumps(s) + "\n" for s in samples))
        (out / "result.json").write_text(json.dumps(result, indent=2) + "\n")
        print("PRESSURE_RESULT " + json.dumps(result), flush=True)
        shutil.rmtree(scratch)
    return 0 if result["status"] == "complete" else 1


if __name__ == "__main__":
    sys.exit(main())
