#!/usr/bin/env python3
"""Read-only exporter of #2304 slice 5 evidence from the rsi database.

Writes the sanitized JSON contract that `scripts/measure-issue-pipeline.py`
reads (schema_version 1). It opens the database with SQLite `mode=ro`, never
writes, never calls the daemon and exports no content: only ids, timestamps,
routes, outcomes, SHAs, models and costs.

Usage:
  export-issue-pipeline-evidence.py --db ~/.rsi/rsi.db --project <project-uuid> \\
      --from 2026-10-13T01:00:00Z --to 2026-10-15T01:00:00Z \\
      [--rolling-ref origin/rolling --repo .] [--assume-complete] > export.json

Attribution rules (deterministic, never guessed; see the adoption note):
  * route = "topology" if the Issue has any topology execution, else "hand".
  * outcome landed = a succeeded `land` node attempt, or a published rolling
    queue entry whose source session authored an Issue handoff. A Closed Issue
    without landing evidence is exported as `cancelled` with gate_reason
    `closed_without_landing_evidence`; Open/InProgress is `open` (censored).
  * manager lineage = a `continued_from` chain; an Issue's lineage is the session
    that requested its execution (topology) or last closed it as a manager (hand).
  * a manager turn is a `session.continue.resume` invocation; it is attributed to
    an Issue only when the daemon message that started it names exactly one Issue
    (an execution id or a `#NNNN` display number). Any other turn has issue_id
    null and is never split.
  * coverage.turns is always `partial` (turns the message does not name cannot be
    seen); coverage.invocations is `complete` for topology Issues (every node
    attempt session is exported) and `partial` for hand Issues. `--assume-complete`
    overrides both to `complete` for a labeled lower-bound sensitivity run.
"""
import argparse
import json
import re
import sqlite3
import subprocess
import sys
from datetime import datetime, timezone

UUID_RE = re.compile(r"[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}")
HASH_RE = re.compile(r"(?<![\w/])#(\d{2,6})\b")


def issue_refs(text, exec_issue, known_numbers):
    """Distinct Issue display numbers a daemon message names (execution ids and #N)."""
    found = set()
    for uid in UUID_RE.findall(text or ""):
        if uid in exec_issue:
            found.add(exec_issue[uid])
    for num in HASH_RE.findall(text or ""):
        if int(num) in known_numbers:
            found.add(int(num))
    return found


def attribute_message(text, exec_issue, known_numbers):
    """The single Issue a message names, or None when it names zero or several."""
    found = issue_refs(text, exec_issue, known_numbers)
    return next(iter(found)) if len(found) == 1 else None


def norm_ts(value):
    """Normalise a stored RFC3339 stamp to a `Z` form the measure tool accepts."""
    if value is None:
        return None
    return re.sub(r"\+00:00$", "Z", value)


def connect_ro(path):
    conn = sqlite3.connect(f"file:{path}?mode=ro", uri=True, timeout=30)
    conn.row_factory = sqlite3.Row
    return conn


def merged_to_ref(sha, repo, ref):
    if not (sha and repo and ref):
        return None
    proc = subprocess.run(["git", "-C", repo, "merge-base", "--is-ancestor", sha, ref],
                          capture_output=True)
    return proc.returncode == 0


def branch_tip(repo, session_id):
    """Tip of a worker's pushed `origin/rsi/<session>` branch, or None."""
    if not repo:
        return None
    proc = subprocess.run(["git", "-C", repo, "rev-parse", "--verify", "-q",
                           f"refs/remotes/origin/rsi/{session_id}"], capture_output=True, text=True)
    out = proc.stdout.strip()
    return out if proc.returncode == 0 and out else None


def first_rolling_merge(repo, tip, ref):
    """(sha, committer date) of the first-parent rolling commit that first contains `tip`."""
    if merged_to_ref(tip, repo, ref) is not True:
        return None
    proc = subprocess.run(["git", "-C", repo, "log", "--ancestry-path", "--first-parent", "--reverse",
                           "--format=%H %cI", f"{tip}..{ref}"], capture_output=True, text=True)
    line = proc.stdout.splitlines()[0] if proc.stdout.strip() else None
    if not line:  # `tip` is itself the rolling commit (fast-forward): use its own date
        own = subprocess.run(["git", "-C", repo, "log", "-1", "--format=%cI", tip],
                             capture_output=True, text=True).stdout.strip()
        return (tip, own or None)
    sha, date = line.split(" ", 1)
    return (sha, date)


def session_invocations(conn, sid):
    """A session's invocations with per-invocation incremental cost.

    `estimated_cost_usd` on `session.launch.fresh` / `session.continue.resume` rows is the
    session's CUMULATIVE cost at that moment (the last row equals `sessions.cost_usd`), so a
    plain sum overstates cost by an order of magnitude. The increment is the difference to the
    previous row; a drop (a reset) restarts from the row's own value; null stays unknown.
    """
    rows = conn.execute(
        "select id, purpose, created_at, model, estimated_cost_usd from model_invocations "
        "where session_id=? and purpose != 'session.title' order by created_at, id", (sid,)).fetchall()
    out, prev = [], 0.0
    for row in rows:
        cum, inc = row["estimated_cost_usd"], None
        if cum is not None:
            inc = cum - prev if cum >= prev else cum
            prev = cum
        out.append({"id": row["id"], "purpose": row["purpose"], "created_at": row["created_at"],
                    "model": row["model"], "cost": None if inc is None else max(inc, 0.0)})
    return out


def lineage_components(conn, project):
    parent = {}
    rows = conn.execute(
        "select id, continued_from from sessions where project_id=?", (project,)).fetchall()
    for row in rows:
        parent[row["id"]] = row["continued_from"]

    def root(sid):
        seen = set()
        while parent.get(sid) and parent[sid] in parent and sid not in seen:
            seen.add(sid)
            sid = parent[sid]
        return sid
    return root


def export(args):
    conn = connect_ro(args.db)
    project = args.project
    start, end = args.start, args.end
    exported_at = datetime.now(timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")

    issues_rows = conn.execute(
        "select id, display_number, status, created_at, closed_at from issues "
        "where project_id=? and created_at >= ? and created_at < ? order by display_number",
        (project, start, end)).fetchall()
    known_numbers = {r["display_number"] for r in conn.execute(
        "select display_number from issues where project_id=?", (project,))}
    by_number = {r["display_number"]: r for r in issues_rows}

    # Topology executions per Issue number.
    execs = {}
    exec_issue = {}
    for row in conn.execute(
            "select id, topology_name_snapshot, status, input_json, requested_by_session_id, "
            "created_at, finished_at from topology_executions where project_id=? "
            "and topology_name_snapshot like 'issue-%' order by created_at", (project,)):
        try:
            num = json.loads(row["input_json"] or "{}").get("issue", {}).get("display_number")
        except ValueError:
            num = None
        if isinstance(num, int):
            execs.setdefault(num, []).append(row)
            exec_issue[row["id"]] = num

    root = lineage_components(conn, project)

    issues, worker_sessions, review_sessions = [], {}, {}
    lineage_members = {}
    for ir in issues_rows:
        num = ir["display_number"]
        iid = f"#{num}"
        ex = execs.get(num, [])
        sessions, reviews = set(), set()
        landed_sha = published_at = None
        for e in ex:
            for att in conn.execute(
                    "select node_id, node_kind, status, session_id, result_commit, finished_at, "
                    "review_assignment_id from topology_node_attempts where execution_id=?", (e["id"],)):
                if att["session_id"] and att["node_kind"] == "session":
                    sessions.add(att["session_id"])
                if att["node_kind"] == "review" and att["review_assignment_id"]:
                    # the attempt's own session_id is synthetic; the reviewer is on the assignment
                    rv = conn.execute("select reviewer_session_id from manager_review_assignments "
                                      "where assignment_id=?", (att["review_assignment_id"],)).fetchone()
                    if rv and rv[0]:
                        reviews.add(rv[0])
                if att["node_id"] == "land" and att["status"] == "succeeded" and att["result_commit"]:
                    landed_sha, published_at = att["result_commit"], att["finished_at"]
        source_sha = None
        for h in conn.execute("select distinct author_session_id from issue_handoffs "
                              "where issue_id=?", (ir["id"],)):
            tip = branch_tip(args.repo, h[0]) if h[0] else None
            if tip:  # a worker session: its pushed branch exists
                sessions.add(h[0])
                if not landed_sha:
                    hit = first_rolling_merge(args.repo, tip, args.rolling_ref)
                    if hit:
                        source_sha, (landed_sha, published_at) = tip, hit
        # Manager lineage owner.
        owner = None
        if ex:
            owner = ex[-1]["requested_by_session_id"]
        else:
            ev = conn.execute(
                "select actor_session_id from issue_events where issue_id=? and operation="
                "'status_updated' and actor_kind='manager' order by sequence desc limit 1",
                (ir["id"],)).fetchone()
            owner = ev[0] if ev else None
        lineage = f"lin-{root(owner)[:8]}" if owner else None
        if owner:
            lineage_members.setdefault(lineage, set()).add(owner)

        status, gate = ir["status"], None
        if landed_sha:
            outcome = "landed"
        elif status == "Closed":
            outcome, gate = "cancelled", "closed_without_landing_evidence"
        elif status == "Cancelled":
            outcome, gate = "cancelled", "issue_cancelled"
        elif ex and ex[-1]["status"] == "failed":
            outcome, gate = "failed", "execution_failed"
        elif ex and ex[-1]["status"] == "cancelled":
            outcome, gate = "cancelled", "execution_cancelled"
        else:
            outcome = "open"
        rec = {"issue_id": iid, "route": "topology" if ex else "hand",
               "route_name": ex[-1]["topology_name_snapshot"] if ex else "hand",
               "outcome": outcome, "opened_at": norm_ts(ir["created_at"]), "observed_at": exported_at}
        if ex:
            rec["execution_id"] = ex[-1]["id"]
        if lineage:
            rec["manager_lineage_id"] = lineage
        if outcome == "landed":
            if published_at:
                rec["published_at"] = norm_ts(published_at)
            rec["landed_sha"] = landed_sha
            if source_sha:
                rec["source_sha"] = source_sha
            rec["settled_at"] = norm_ts(ir["closed_at"] or published_at)
            if args.repo and merged_to_ref(landed_sha, args.repo, args.rolling_ref) is False:
                rec["gate_reason"] = "landed_sha_not_ancestor_of_rolling"
        elif outcome != "open":
            rec["settled_at"] = norm_ts(ir["closed_at"] or (ex[-1]["finished_at"] if ex else None) or ir["created_at"])
        if gate:
            rec["gate_reason"] = gate
        rec["coverage"] = {"turns": "complete" if args.assume_complete else "partial",
                           "invocations": "complete" if (ex or args.assume_complete) else "partial"}
        issues.append(rec)
        worker_sessions[iid] = sessions
        review_sessions[iid] = reviews

    # Manager turns per lineage session.
    owner_sessions = set()
    for members in lineage_members.values():
        owner_sessions |= members
    lineage_sessions = {}
    for row in conn.execute("select id from sessions where project_id=?", (project,)):
        r = root(row["id"])
        lin = f"lin-{r[:8]}"
        if lin in lineage_members:
            lineage_sessions.setdefault(lin, []).append(row["id"])
    lineages = [{"lineage_id": lin, "session_ids": sorted(ids)} for lin, ids in sorted(lineage_sessions.items())]

    turns, invocations = [], []
    for lin, ids in lineage_sessions.items():
        for sid in ids:
            for inv in session_invocations(conn, sid):
                if inv["purpose"] != "session.continue.resume" or not (args.turn_from <= inv["created_at"] < args.turn_to):
                    continue
                ev = conn.execute(
                    "select content from conversation_events where session_id=? and role='User' "
                    "and event_type='Message' and julianday(created_at) >= julianday(?) - 1.0/86400 "
                    "and julianday(created_at) <= julianday(?) + 10.0/86400 order by created_at limit 1",
                    (sid, inv["created_at"], inv["created_at"])).fetchone()
                text = ev["content"] if ev else ""
                num = attribute_message(text, exec_issue, known_numbers) if "rsid-daemon-message" in text else None
                turn = {"turn_id": f"t-{inv['id']}", "session_id": sid, "at": norm_ts(inv["created_at"])}
                if num is not None and num in by_number:
                    turn["issue_id"] = f"#{num}"
                if "topology_execution_settled" in text:
                    turn["category"] = "terminal_summary"
                turns.append(turn)
                invocations.append({"invocation_id": f"i-{inv['id']}", "session_id": sid,
                                    "turn_id": turn["turn_id"], "at": norm_ts(inv["created_at"]),
                                    "model": inv["model"], "role": "manager",
                                    **({"issue_id": turn["issue_id"]} if "issue_id" in turn else {}),
                                    **({"cost_usd": inv["cost"]} if inv["cost"] is not None else {})})

    for iid, role_map in (("worker", worker_sessions), ("review", review_sessions)):
        for issue_id, sessions in role_map.items():
            for sid in sorted(sessions):
                for inv in session_invocations(conn, sid):
                    invocations.append({"invocation_id": f"i-{inv['id']}", "session_id": sid,
                                        "issue_id": issue_id, "at": norm_ts(inv["created_at"]),
                                        "model": inv["model"], "role": iid,
                                        **({"cost_usd": inv["cost"]} if inv["cost"] is not None else {})})
    # De-duplicate: the same session can serve as worker for one Issue only.
    seen, uniq = set(), []
    for inv in invocations:
        if inv["invocation_id"] in seen:
            continue
        seen.add(inv["invocation_id"])
        uniq.append(inv)

    fleet = []
    for w in args.fleet_windows:
        ws, we = w
        for row in conn.execute(
                "select model, count(*) n, sum(case when p is null then c when c >= p then c - p else c end) cost "
                "from (select model, created_at, estimated_cost_usd c, lag(estimated_cost_usd) over "
                "(partition by session_id order by created_at, id) p from model_invocations "
                "where project_id=? and purpose != 'session.title') "
                "where created_at >= ? and created_at < ? and model like 'claude-opus%' "
                "and c is not null group by model", (project, ws, we)):
            fleet.append({"window_start": ws, "window_end": we, "model": row["model"],
                          "cost_usd": row["cost"] or 0, "invocations": row["n"], "source": "model_invocations"})
    return {"schema_version": 1,
            "export": {"id": f"rsi-db-{exported_at}", "generated_at": exported_at, "sanitized": True},
            "issues": issues, "lineages": lineages, "turns": turns,
            "invocations": uniq, "fleet_totals": fleet}


def main(argv=None):
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--db", required=True)
    ap.add_argument("--project", required=True)
    ap.add_argument("--from", dest="start", required=True, help="cohort opened_at start (RFC3339)")
    ap.add_argument("--to", dest="end", required=True, help="cohort opened_at end (exclusive)")
    ap.add_argument("--turn-from", default=None, help="manager-turn range start (default: --from)")
    ap.add_argument("--turn-to", default="9999-12-31T00:00:00Z")
    ap.add_argument("--fleet-window", action="append", default=[], metavar="START,END")
    ap.add_argument("--repo")
    ap.add_argument("--rolling-ref", default="origin/rolling")
    ap.add_argument("--assume-complete", action="store_true")
    args = ap.parse_args(argv)
    args.turn_from = args.turn_from or args.start
    args.fleet_windows = [tuple(w.split(",", 1)) for w in args.fleet_window]
    json.dump(export(args), sys.stdout, sort_keys=True)
    sys.stdout.write("\n")


if __name__ == "__main__":
    main()
