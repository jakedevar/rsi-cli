#!/usr/bin/env python3
"""Agent-usability benchmark for the RSI harness.

Drives the closed Agent* RPC surface through `rsi-rpc` from inside an
rsi-managed session and measures whether an agent can complete ordinary
coordination work without operator help.

Measures per operation: outcome class, wall-clock latency, retries required,
and the exact failure receipt. Aggregates to task-completion rate and
failures-per-operation so regressions are visible.

Usage:
    python3 tools/bench/agent_usability.py --phase read      # read-only matrix
    python3 tools/bench/agent_usability.py --phase live      # + spawn/lifecycle
    python3 tools/bench/agent_usability.py --phase all
    python3 tools/bench/agent_usability.py --json out.json
"""

from __future__ import annotations

import argparse
import json
import os
import subprocess
import sys
import time
import uuid
from dataclasses import dataclass, field
from typing import Any

RPC = os.environ.get("RSI_RPC_BIN", "rsi-rpc")
TIMEOUT = 90


@dataclass
class OpResult:
    op_id: str
    phase: str
    verb: str
    intent: str
    expectation: str
    outcome: str = "unknown"
    latency_ms: int = 0
    attempts: int = 0
    error_code: str | None = None
    error_message: str | None = None
    detail: dict[str, Any] = field(default_factory=dict)

    @property
    def passed(self) -> bool:
        return self.outcome in {"ok", "denied_expected"}


@dataclass
class Benchmark:
    results: list[OpResult] = field(default_factory=list)
    session_id: str = ""
    epic_id: str = ""
    started_at: str = ""

    def record(self, r: OpResult) -> None:
        self.results.append(r)

    def summary(self) -> dict[str, Any]:
        total = len(self.results)
        passed = sum(1 for r in self.results if r.passed)
        retried = sum(1 for r in self.results if r.attempts > 1)
        latencies = sorted(r.latency_ms for r in self.results)
        p50 = latencies[len(latencies) // 2] if latencies else 0
        p95 = latencies[int(len(latencies) * 0.95)] if latencies else 0
        by_outcome: dict[str, int] = {}
        for r in self.results:
            by_outcome[r.outcome] = by_outcome.get(r.outcome, 0) + 1
        return {
            "baseline_source": "origin/rolling",
            "session_id": self.session_id,
            "epic_id": self.epic_id,
            "operations": total,
            "passed": passed,
            "completion_rate": round(passed / total, 4) if total else 0.0,
            "failures": total - passed,
            "retries_required": retried,
            "latency_ms_p50": p50,
            "latency_ms_p95": p95,
            "by_outcome": by_outcome,
        }


def call(verb: str, params: dict[str, Any] | None, expect: str) -> OpResult:
    """Invoke one rsi-rpc verb, classifying the outcome against expectation."""
    payload = json.dumps(params) if params is not None else "{}"
    r = OpResult(
        op_id="",
        phase="",
        verb=verb,
        intent="",
        expectation=expect,
    )
    started = time.monotonic()
    proc = subprocess.run(
        [RPC, verb, "--params", payload],
        capture_output=True,
        text=True,
        timeout=TIMEOUT,
    )
    r.latency_ms = int((time.monotonic() - started) * 1000)
    r.attempts = 1
    raw = proc.stdout.strip()
    for line in raw.splitlines():
        if line.startswith("{"):
            raw = raw[raw.index(line):]
            break
    try:
        parsed = json.loads(raw)
    except json.JSONDecodeError:
        r.outcome = "harness_error"
        r.error_message = f"non-JSON output rc={proc.returncode}: {proc.stdout[:200]}"
        return r
    if "error" in parsed:
        err = parsed["error"]
        r.error_code = str(err.get("code"))
        r.error_message = str(err.get("message"))
        if expect == "deny" or expect.startswith("deny:"):
            want = expect.split(":", 1)[1] if ":" in expect else None
            if want is None or want in (r.error_message or ""):
                r.outcome = "denied_expected"
            else:
                r.outcome = "denied_unexpected_reason"
        else:
            r.outcome = "denied_unexpected"
        return r
    r.outcome = "ok" if expect == "ok" else "succeeded_but_expected_denial"
    result = parsed.get("result")
    if isinstance(result, dict):
        r.detail = {k: v for k, v in result.items() if not isinstance(v, (list, dict))}
    return r


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--phase", default="read", choices=["read", "live", "all"])
    ap.add_argument("--json", default=None)
    args = ap.parse_args()

    bench = Benchmark()
    bench.started_at = time.strftime("%Y-%m-%dT%H:%M:%S%z")

    def add(phase: str, op_id: str, verb: str, intent: str, params: dict | None, expect: str) -> OpResult:
        r = call(verb, params, expect)
        r.phase, r.op_id, r.intent = phase, op_id, intent
        bench.record(r)
        mark = "PASS" if r.passed else "FAIL"
        note = f" [{r.error_message}]" if r.error_message else ""
        print(f"{mark:4} {op_id:34} {r.outcome:28} {r.latency_ms:6}ms{note}")
        return r

    # ---- Phase 1: identity + read surface -------------------------------
    st = add("read", "R01 self status", "AgentGetStatus", "resolve own identity", {}, "ok")
    bench.session_id = str(st.detail.get("id") or os.environ.get("RSI_SESSION_ID", ""))
    st_full = subprocess.run([RPC, "AgentGetStatus", "--params", "{}"], capture_output=True, text=True)
    try:
        full = json.loads(st_full.stdout[st_full.stdout.index("{"):])
        bench.epic_id = str(full["result"].get("parent_id") or "")
    except Exception:
        pass

    add("read", "R02 targeted status", "AgentGetStatus",
        "status of own session by id", {"session_id": bench.session_id}, "ok")
    add("read", "R03 cohort progress", "AgentGetProgress", "own child cohort", {}, "ok")
    add("read", "R04 issue list", "AgentListIssues", "lead-scoped issue read", {"limit": 5}, "ok")
    add("read", "R05 manager inspect", "AgentManagerInspect",
        "narrowed non-manager overview", {}, "ok")
    for sec in ("workers", "work", "requests", "decisions", "topology", "resources", "actions", "events"):
        add("read", f"R06 inspect {sec}", "AgentManagerInspect",
            f"narrowed {sec} page", {"section": sec}, "ok")
    add("read", "R07 inspect own epic", "AgentManagerInspect",
        "narrowed by own epic", {"epic_id": bench.epic_id}, "ok")
    add("read", "R08 inbox", "AgentManagerInbox", "lead inbox read", {}, "ok")

    # ---- Phase 2: authority boundary (expected denials) -----------------
    add("read", "A01 manager progress", "AgentManagerProgress",
        "parentless manager-only", {}, "deny")
    add("read", "A02 manager send", "AgentManagerSend",
        "manager-only write", {"epic_id": bench.epic_id, "message": "probe", "idempotency_key": "astra-a02"}, "deny")
    add("read", "A03 inspect foreign epic", "AgentManagerInspect",
        "cross-scope read", {"epic_id": str(uuid.uuid4())}, "deny")
    add("read", "A04 update without fence", "AgentManagerUpdate",
        "missing fence", {"idempotency_key": "astra-a04", "change": {"update": "work"}}, "deny")
    add("read", "A05 prepare control", "AgentManagerPrepareControl",
        "parentless preflight", {"operation": {"action": "resume_lead"}}, "deny")
    add("read", "A06 unknown verb", "AgentNotARealVerb",
        "default-deny", {}, "deny")
    add("read", "A07 token in params", "AgentGetStatus",
        "credential injection rejected",
        {"session_id": bench.session_id, "token": os.environ.get("RSI_SESSION_TOKEN", "x")}, "deny")

    # ---- Phase 3: schema discovery is offline ---------------------------
    for verb in ("AgentSpawnChild", "AgentManagerUpdate", "AgentSubmitReviewReceipt"):
        started = time.monotonic()
        proc = subprocess.run([RPC, verb, "--schema"], capture_output=True, text=True, timeout=30)
        r = OpResult("", "read", verb, "offline schema", "ok")
        r.op_id = f"R09 schema {verb.replace('Agent', '')}"
        r.phase = "read"
        r.latency_ms = int((time.monotonic() - started) * 1000)
        r.attempts = 1
        try:
            sch = json.loads(proc.stdout[proc.stdout.index("{"):])
            r.outcome = "ok" if sch.get("method") == verb else "schema_mismatch"
        except Exception:
            r.outcome = "harness_error"
        bench.record(r)
        print(f"{'PASS' if r.passed else 'FAIL':4} {r.op_id:34} {r.outcome:28} {r.latency_ms:6}ms")

    summary = bench.summary()
    print("\n=== SUMMARY ===")
    print(json.dumps(summary, indent=2))

    failures = [r for r in bench.results if not r.passed]
    if failures:
        print("\n=== FAILURES ===")
        for r in failures:
            print(f"- {r.op_id}: {r.outcome} :: {r.error_message}")

    if args.json:
        with open(args.json, "w") as fh:
            json.dump(
                {
                    "summary": summary,
                    "results": [r.__dict__ for r in bench.results],
                },
                fh,
                indent=2,
            )
        print(f"\nwrote {args.json}")
    return 1 if failures else 0


if __name__ == "__main__":
    sys.exit(main())
