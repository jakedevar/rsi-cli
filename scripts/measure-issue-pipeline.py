#!/usr/bin/env python3
"""Per-Issue manager-turn and Opus-cost evidence from a sanitized JSON export (#2304 slice 5).

Usage:
  measure-issue-pipeline.py --input EXPORT.json [--input MORE.json ...] \\
      --window before=2026-10-01T00:00:00Z,2026-10-08T00:00:00Z \\
      --window after=2026-10-15T00:00:00Z,2026-10-22T00:00:00Z \\
      [--compare before after] [--min-coverage 0.8] [--fail-on-incomparable]
      [--output REPORT.json] [--max-bytes N]

It reads ONLY supplied files (`-` is stdin). It never opens the daemon DB, never
touches the network and never polls. Output is deterministic JSON on stdout
(or --output). Exit 0 = report written, 2 = bad usage/contract, 3 =
--fail-on-incomparable and the comparison is not comparable.

INPUT CONTRACT (schema_version 1; an allow-list: any other field is refused so
content, usage tokens, credentials and personal metadata cannot ride along):

  {"schema_version": 1,
   "export": {"id": str, "generated_at": RFC3339, "sanitized": true},
   "issues": [{"issue_id", "route": "topology"|"hand", "route_name"?,
               "execution_id"?, "manager_lineage_id"?,
               "outcome": "landed"|"failed"|"blocked"|"cancelled"|"open",
               "opened_at", "observed_at", "published_at"?, "settled_at"?,
               "source_sha"?, "landed_sha"?, "gate_reason"?,
               "review_status"?: none_required|pending|accepted|findings|blocked|unknown,
               "coverage"?: {"turns"?, "invocations"?: complete|partial|missing}}],
   "lineages": [{"lineage_id", "session_ids": [str]}],
   "turns": [{"turn_id", "issue_id"?, "session_id", "at",
              "category"?: setup|mechanical|real_gate|terminal_summary}],
   "invocations": [{"invocation_id", "session_id"?, "turn_id"?, "issue_id"?, "at",
                    "model"?, "role": manager|follow_up|worker|review,
                    "cost_usd"?: number >= 0}],
   "fleet_totals": [{"window_start", "window_end", "model", "cost_usd",
                     "invocations"?, "source"?}]}

ATTRIBUTION RULES (never guessed):
  * Identity is the issue_id / turn_id / invocation_id. Repeated records with
    identical content collapse to one; same id with differing content is
    AMBIGUOUS: it is excluded from every count, listed under `unattributed`, and
    marks the affected Issue incomplete. An open Issue record is superseded by a
    later-`observed_at` terminal record of the same Issue; any other difference
    is ambiguous. Rows with no id are quarantined (counted, never summed).
  * A manager lineage is every session of one owner across rotations (the
    `lineages` list; a session in two lineages is ambiguous). A turn counts for
    an Issue only if its issue_id names it and its session is in that Issue's
    manager lineage; otherwise it is unattributed. A turn serving several Issues
    must be exported with issue_id null: it is never split.
  * Every attributed manager turn counts, terminal wake turns included. A turn
    with no/unknown `category` counts in `total` and `unclassified`.
  * Cost is Opus-only (`model` starts with `claude-opus`). `cost_usd` null/absent,
    unknown model or an ambiguous duplicate keeps that cost UNKNOWN (usd null,
    never 0). Roles manager/follow_up/worker/review are never summed together.
    An invocation links to an Issue by its issue_id or its turn's Issue (a
    conflict is ambiguous). `fleet_totals` and Issue-less invocations are echoed
    as context only and never assigned to an Issue.
  * `coverage.turns`/`coverage.invocations` must be `complete` (the exporter
    asserts it listed every row for that Issue) for a count/cost to be known;
    otherwise the numbers are lower bounds and the value is null.

WINDOWS: half-open [start, end). The eligible population of a window is every
non-ambiguous Issue (any outcome) with opened_at in [start, end), grouped by
route and route_name. Landed/failed/blocked/cancelled/censored (outcome open)
are reported side by side so successful-only sampling cannot hide expensive
failures. Turns and invocations are counted as exported (not cut at the window
end); `min_follow_up_seconds` shows how mature the cohort is. A before/after
comparison is `comparable` only with equal length, no overlap, ordered windows,
and per-route eligible/landed counts and attribution coverage at or above
--min-coverage in both windows. Fixture runs are observations of the supplied
file, not a claim about production or a deployment.
"""
import argparse
from datetime import datetime, timedelta, timezone
from decimal import Decimal, InvalidOperation
import json
import re
import sys

SCHEMA_VERSION = 1
REPORT_SCHEMA = "measure-issue-pipeline/1"
DEFAULT_MAX_BYTES = 16 * 1024 * 1024
MAX_ISSUES = 5_000
MAX_TURNS = 100_000
MAX_INVOCATIONS = 200_000
MAX_LINEAGES = 5_000
MAX_SESSIONS_PER_LINEAGE = 1_000
MAX_FLEET = 1_000
MAX_WINDOWS = 8
MAX_WINDOW_DAYS = 400
MAX_COST_USD = Decimal("1000000")
LIST_CAP = 100

ROUTES = ("topology", "hand")
OUTCOMES = ("landed", "failed", "blocked", "cancelled", "open")
STATES = ("landed", "failed", "blocked", "cancelled", "censored")
CATEGORIES = ("setup", "mechanical", "real_gate", "terminal_summary")
ROLES = ("manager", "follow_up", "worker", "review")
REVIEW_STATUSES = ("none_required", "pending", "accepted", "findings", "blocked", "unknown")
COVERAGE = ("complete", "partial", "missing")

ID_RE = re.compile(r"^[A-Za-z0-9#][A-Za-z0-9_.:#/-]{0,127}$")
NAME_RE = re.compile(r"^[A-Za-z0-9][A-Za-z0-9_.-]{0,63}$")
CODE_RE = re.compile(r"^[a-z0-9][a-z0-9_.-]{0,63}$")
SHA_RE = re.compile(r"^[0-9a-f]{7,64}$")
TS_RE = re.compile(
    r"^(\d{4})-(\d{2})-(\d{2})T(\d{2}):(\d{2}):(\d{2})(?:\.(\d{1,9}))?(Z|[+-]\d{2}:\d{2})$")
EPOCH = datetime(1970, 1, 1, tzinfo=timezone.utc)
NS = 1_000_000_000

SAMPLING_RULES = [
    "cohort: Issues with opened_at in the half-open window [start, end), every outcome",
    "ambiguous Issue records are excluded from the cohort and counted separately",
    "turns/invocations are counted as exported, deduplicated by id, not cut at window end",
    "unknown cost/turns stay null; means use only Issues with complete evidence",
    "manager and follow_up costs are never summed; fleet totals are never Issue costs",
]


class ContractError(Exception):
    """The input or arguments violate the documented contract."""


# ---------------------------------------------------------------- validation

def fail(path, message):
    raise ContractError(f"{path}: {message}")


def parse_ts(value, path):
    """RFC3339 -> integer nanoseconds since the epoch (exact at the half-open boundary)."""
    match = TS_RE.match(value) if isinstance(value, str) else None
    if not match:
        fail(path, f"expected an RFC3339 timestamp with a zone, got {value!r}")
    year, month, day, hour, minute, second, frac, zone = match.groups()
    offset = timedelta(0)
    if zone != "Z":
        sign = 1 if zone[0] == "+" else -1
        offset = sign * timedelta(hours=int(zone[1:3]), minutes=int(zone[4:6]))
    try:
        moment = datetime(int(year), int(month), int(day), int(hour), int(minute), int(second),
                          tzinfo=timezone(offset))
    except ValueError as err:
        fail(path, f"invalid timestamp {value!r}: {err}")
    delta = moment - EPOCH
    seconds = delta.days * 86400 + delta.seconds
    return seconds * NS + int((frac or "").ljust(9, "0"))


def ts_text(ns):
    seconds, nanos = divmod(ns, NS)
    stamp = (EPOCH + timedelta(seconds=seconds)).strftime("%Y-%m-%dT%H:%M:%S")
    return f"{stamp}.{nanos:09d}Z" if nanos else f"{stamp}Z"


def check_object(value, path, required, optional):
    if not isinstance(value, dict):
        fail(path, "expected an object")
    unknown = sorted(set(value) - set(required) - set(optional))
    if unknown:
        fail(path, f"field(s) {unknown} are outside the sanitized contract; only "
                   f"{sorted(set(required) | set(optional))} are accepted (no content, tokens or credentials)")
    missing = [key for key in required if key not in value]
    if missing:
        fail(path, f"missing required field(s) {missing}")
    return value


def get_str(row, key, path, pattern, optional=True):
    value = row.get(key)
    if value is None:
        if optional:
            return None
        fail(f"{path}.{key}", "is required")
    if not isinstance(value, str) or not pattern.match(value):
        fail(f"{path}.{key}", f"invalid value {value!r} (expected {pattern.pattern})")
    return value


def get_enum(row, key, path, allowed, default=None):
    value = row.get(key)
    if value is None:
        if default is None:
            fail(f"{path}.{key}", f"is required; one of {list(allowed)}")
        return default
    if value not in allowed:
        fail(f"{path}.{key}", f"invalid value {value!r}; one of {list(allowed)}")
    return value


def get_ts(row, key, path, optional=False):
    value = row.get(key)
    if value is None:
        if optional:
            return None
        fail(f"{path}.{key}", "is required")
    return parse_ts(value, f"{path}.{key}")


def get_cost(row, key, path):
    value = row.get(key)
    if value is None:
        return None
    if isinstance(value, bool) or not isinstance(value, (int, float, Decimal)):
        fail(f"{path}.{key}", f"expected a number or null, got {value!r}")
    try:
        cost = Decimal(value)
    except InvalidOperation:
        fail(f"{path}.{key}", f"invalid number {value!r}")
    if not cost.is_finite() or cost < 0 or cost > MAX_COST_USD:
        fail(f"{path}.{key}", f"cost must be a finite number in [0, {MAX_COST_USD}], got {value!r}")
    return cost


def get_list(doc, key, limit):
    rows = doc.get(key, [])
    if not isinstance(rows, list):
        fail(key, "expected an array")
    if len(rows) > limit:
        fail(key, f"{len(rows)} rows exceeds the bound of {limit}; export a narrower window")
    return rows


def norm_issue(row, path):
    row = check_object(row, path, ["route", "outcome", "opened_at", "observed_at"], [
        "issue_id", "route_name", "execution_id", "manager_lineage_id", "published_at",
        "settled_at", "source_sha", "landed_sha", "gate_reason", "review_status", "coverage"])
    coverage = row.get("coverage") or {}
    check_object(coverage, f"{path}.coverage", [], ["turns", "invocations"])
    return {
        "issue_id": get_str(row, "issue_id", path, ID_RE),
        "route": get_enum(row, "route", path, ROUTES),
        "route_name": get_str(row, "route_name", path, NAME_RE),
        "execution_id": get_str(row, "execution_id", path, ID_RE),
        "lineage_id": get_str(row, "manager_lineage_id", path, ID_RE),
        "outcome": get_enum(row, "outcome", path, OUTCOMES),
        "opened_ns": get_ts(row, "opened_at", path),
        "observed_ns": get_ts(row, "observed_at", path),
        "published_ns": get_ts(row, "published_at", path, optional=True),
        "settled_ns": get_ts(row, "settled_at", path, optional=True),
        "source_sha": get_str(row, "source_sha", path, SHA_RE),
        "landed_sha": get_str(row, "landed_sha", path, SHA_RE),
        "gate_reason": get_str(row, "gate_reason", path, CODE_RE),
        "review_status": get_enum(row, "review_status", path, REVIEW_STATUSES, "unknown"),
        "coverage_turns": get_enum(coverage, "turns", f"{path}.coverage", COVERAGE, "missing"),
        "coverage_invocations": get_enum(coverage, "invocations", f"{path}.coverage", COVERAGE, "missing"),
    }


def norm_turn(row, path):
    row = check_object(row, path, ["at"], ["turn_id", "issue_id", "session_id", "category"])
    category = row.get("category")
    if category is not None and category not in CATEGORIES:
        fail(f"{path}.category", f"invalid value {category!r}; one of {list(CATEGORIES)} or null")
    return {
        "turn_id": get_str(row, "turn_id", path, ID_RE),
        "issue_id": get_str(row, "issue_id", path, ID_RE),
        "session_id": get_str(row, "session_id", path, ID_RE),
        "at_ns": get_ts(row, "at", path),
        "category": category,
    }


def norm_invocation(row, path):
    row = check_object(row, path, ["at", "role"], [
        "invocation_id", "session_id", "turn_id", "issue_id", "model", "cost_usd"])
    model = row.get("model")
    if model is not None and (not isinstance(model, str) or not 0 < len(model) <= 128):
        fail(f"{path}.model", f"invalid model {model!r}")
    return {
        "invocation_id": get_str(row, "invocation_id", path, ID_RE),
        "session_id": get_str(row, "session_id", path, ID_RE),
        "turn_id": get_str(row, "turn_id", path, ID_RE),
        "issue_id": get_str(row, "issue_id", path, ID_RE),
        "at_ns": get_ts(row, "at", path),
        "model": model,
        "role": get_enum(row, "role", path, ROLES),
        "cost": get_cost(row, "cost_usd", path),
    }


def norm_lineage(row, path):
    row = check_object(row, path, ["lineage_id", "session_ids"], [])
    sessions = row["session_ids"]
    if not isinstance(sessions, list) or len(sessions) > MAX_SESSIONS_PER_LINEAGE:
        fail(f"{path}.session_ids", f"expected an array of at most {MAX_SESSIONS_PER_LINEAGE} ids")
    for index, session in enumerate(sessions):
        if not isinstance(session, str) or not ID_RE.match(session):
            fail(f"{path}.session_ids[{index}]", f"invalid session id {session!r}")
    return {"lineage_id": get_str(row, "lineage_id", path, ID_RE, optional=False),
            "sessions": frozenset(sessions)}


def norm_fleet(row, path):
    row = check_object(row, path, ["window_start", "window_end", "model", "cost_usd"],
                       ["invocations", "source"])
    invocations = row.get("invocations")
    if invocations is not None and (isinstance(invocations, bool) or not isinstance(invocations, int)
                                    or invocations < 0):
        fail(f"{path}.invocations", f"expected a non-negative integer, got {invocations!r}")
    cost = get_cost(row, "cost_usd", path)
    return {"window_start": ts_text(get_ts(row, "window_start", path)),
            "window_end": ts_text(get_ts(row, "window_end", path)),
            "model": get_str(row, "model", path, ID_RE, optional=False),
            "cost_usd": money(cost) if cost is not None else None,
            "invocations": invocations,
            "source": get_str(row, "source", path, NAME_RE),
            "attribution": "none: fleet aggregate, never an Issue cost"}


def validate_document(doc, source):
    if not isinstance(doc, dict):
        fail(source, "top level must be an object")
    check_object(doc, source, ["schema_version", "export"],
                 ["issues", "lineages", "turns", "invocations", "fleet_totals"])
    if doc["schema_version"] != SCHEMA_VERSION:
        fail(f"{source}.schema_version", f"unsupported {doc['schema_version']!r}; this tool reads {SCHEMA_VERSION}")
    export = check_object(doc["export"], f"{source}.export", ["id", "generated_at", "sanitized"], [])
    if export["sanitized"] is not True:
        fail(f"{source}.export.sanitized", "must be true: only sanitized exports are accepted")
    parsed = {"export": {"id": get_str(export, "id", f"{source}.export", ID_RE, optional=False),
                         "generated_ns": get_ts(export, "generated_at", f"{source}.export")}}
    for key, limit, norm in (("issues", MAX_ISSUES, norm_issue), ("lineages", MAX_LINEAGES, norm_lineage),
                             ("turns", MAX_TURNS, norm_turn), ("invocations", MAX_INVOCATIONS, norm_invocation),
                             ("fleet_totals", MAX_FLEET, norm_fleet)):
        parsed[key] = [norm(row, f"{source}.{key}[{index}]")
                       for index, row in enumerate(get_list(doc, key, limit))]
    return parsed


def load_documents(paths, max_bytes):
    docs = []
    for path in paths:
        try:
            if path == "-":
                raw = sys.stdin.buffer.read(max_bytes + 1)
            else:
                with open(path, "rb") as handle:
                    raw = handle.read(max_bytes + 1)
        except OSError as err:
            fail(path, f"cannot read input: {err}")
        if len(raw) > max_bytes:
            fail(path, f"input exceeds the {max_bytes}-byte bound; export a narrower window")
        try:
            doc = json.loads(raw.decode("utf-8"), parse_float=Decimal, parse_constant=_reject_constant)
        except (UnicodeDecodeError, json.JSONDecodeError, ContractError) as err:
            fail(path, f"not valid JSON: {err}")
        docs.append(validate_document(doc, path))
    return docs


def _reject_constant(name):
    raise ContractError(f"non-finite number {name} is not allowed")


# ------------------------------------------------------------ deduplication

def dedupe(rows, id_key):
    """-> (ok {id: row}, ambiguous {id: [distinct rows]}, unidentified [rows], duplicate_count)."""
    seen, unidentified, duplicates = {}, [], 0
    for row in rows:
        identity = row[id_key]
        if identity is None:
            unidentified.append(row)
            continue
        variants = seen.setdefault(identity, [])
        if row in variants:
            duplicates += 1
            continue
        variants.append(row)
    ok = {identity: variants[0] for identity, variants in seen.items() if len(variants) == 1}
    ambiguous = {identity: variants for identity, variants in seen.items() if len(variants) > 1}
    return ok, ambiguous, unidentified, duplicates


def merge_issues(records):
    """Collapse repeated Issue records; an open record yields to a later terminal one."""
    ok, ambiguous, unidentified, duplicates = {}, {}, [], 0
    by_id = {}
    for record in records:
        if record["issue_id"] is None:
            unidentified.append(record)
            continue
        by_id.setdefault(record["issue_id"], []).append(record)
    for issue_id, variants in by_id.items():
        distinct = []
        for record in variants:
            if record in distinct:
                duplicates += 1
                continue
            distinct.append(record)
        resolved = resolve_progression(distinct)
        if resolved is None:
            ambiguous[issue_id] = distinct
            continue
        ok[issue_id] = resolved
    return ok, ambiguous, unidentified, duplicates


def resolve_progression(variants):
    if len(variants) == 1:
        return variants[0]
    terminal = [record for record in variants if record["outcome"] != "open"]
    if len(terminal) != 1:
        return None
    final = terminal[0]
    stale = [record for record in variants if record is not final]
    if all(record["observed_ns"] < final["observed_ns"] and same_identity(record, final) for record in stale):
        return final
    return None


def same_identity(left, right):
    keys = ("route", "route_name", "execution_id", "lineage_id", "opened_ns")
    return all(left[key] == right[key] for key in keys)


# -------------------------------------------------------------- attribution

class Dataset:
    def __init__(self, docs):
        self.exports = [doc["export"] for doc in docs]
        flat = lambda key: [row for doc in docs for row in doc[key]]
        self.issues, self.ambiguous_issues, self.unidentified_issues, dup_issues = merge_issues(flat("issues"))
        self.turns, self.ambiguous_turns, self.unidentified_turns, dup_turns = dedupe(flat("turns"), "turn_id")
        self.invocations, self.ambiguous_invocations, self.unidentified_invocations, dup_inv = dedupe(
            flat("invocations"), "invocation_id")
        self.duplicates = {"issue_records": dup_issues, "turns": dup_turns, "invocations": dup_inv}
        self.fleet = sorted((row for doc in docs for row in doc["fleet_totals"]),
                            key=lambda row: json.dumps(row, sort_keys=True))
        self.session_lineage = self._map_sessions(flat("lineages"))
        self.acc = {issue_id: new_acc() for issue_id in self.issues}
        self.unattributed = {"turns": [], "invocations": [], "ambiguous_turns": [], "ambiguous_invocations": []}
        self.unattributed_cost = {"invocations": 0, "known_usd": Decimal(0), "unknown_cost_invocations": 0}
        self._attribute_turns()
        self._attribute_invocations()

    def _map_sessions(self, lineages):
        members = {}
        for lineage in lineages:
            for session in lineage["sessions"]:
                members.setdefault(session, set()).add(lineage["lineage_id"])
        self.lineage_sessions = {}
        for lineage in lineages:
            self.lineage_sessions.setdefault(lineage["lineage_id"], set()).update(lineage["sessions"])
        return members

    def _issue_known(self, issue_id):
        return issue_id in self.issues

    def _note(self, issue_id, key, value):
        if issue_id in self.acc:
            self.acc[issue_id][key].add(value)

    def _check_session(self, issue_id, session):
        """-> reason an owner session cannot be tied to the Issue's manager lineage, else None."""
        if session is None:
            return "missing_session"
        lineages = self.session_lineage.get(session, set())
        if len(lineages) > 1:
            return "session_in_multiple_lineages"
        issue_lineage = self.issues[issue_id]["lineage_id"]
        if issue_lineage is None:
            return "issue_without_lineage"
        if lineages != {issue_lineage}:
            return "session_not_in_lineage"
        return None

    def _turn_issue(self, turn):
        """-> (issue_id | None, reason | None)."""
        if turn["issue_id"] is None:
            return None, "no_issue"
        if not self._issue_known(turn["issue_id"]):
            return None, "unknown_or_ambiguous_issue"
        reason = self._check_session(turn["issue_id"], turn["session_id"])
        return (None, reason) if reason else (turn["issue_id"], None)

    def _attribute_turns(self):
        self.turn_issue = {}
        for turn_id, turn in sorted(self.turns.items()):
            issue_id, reason = self._turn_issue(turn)
            self.turn_issue[turn_id] = issue_id
            if issue_id is None:
                self._unattributed("turns", {"turn_id": turn_id, "reason": reason})
                self._note(turn["issue_id"], "unattributed_turns", turn_id)
                continue
            acc = self.acc[issue_id]
            acc["turns"][turn["category"] or "unclassified"] += 1
            acc["sessions"].add(turn["session_id"])
        for turn_id, variants in sorted(self.ambiguous_turns.items()):
            self._unattributed("ambiguous_turns", {"turn_id": turn_id, "reason": "conflicting_duplicate_turn_id"})
            for variant in variants:
                self._note(variant["issue_id"], "ambiguous_turns", turn_id)
        for turn in self.unidentified_turns:
            self._unattributed("turns", {"turn_id": None, "reason": "missing_turn_id"})
            self._note(turn["issue_id"], "unidentified_turns", id(turn))

    def _unattributed(self, key, row):
        self.unattributed[key].append(row)

    def _invocation_issue(self, inv):
        """-> (issue_id | None, reason | None)."""
        explicit, via_turn = inv["issue_id"], None
        if inv["turn_id"] is not None:
            via_turn = self.turn_issue.get(inv["turn_id"])
        if explicit and via_turn and explicit != via_turn:
            return None, "turn_issue_conflict"
        issue_id = explicit or via_turn
        if issue_id is None:
            return None, "turn_unresolved" if inv["turn_id"] else "no_issue"
        if not self._issue_known(issue_id):
            return None, "unknown_or_ambiguous_issue"
        if inv["role"] == "manager":
            reason = self._check_session(issue_id, inv["session_id"])
            if reason:
                return None, reason
        return issue_id, None

    def _attribute_invocations(self):
        for inv_id, inv in sorted(self.invocations.items()):
            issue_id, reason = self._invocation_issue(inv)
            if issue_id is None:
                self._unattributed("invocations", {"invocation_id": inv_id, "reason": reason})
                self._note(inv["issue_id"], "unattributed_invocations", inv_id)
                self._count_unattributed_cost(inv)
                continue
            self.acc[issue_id]["roles"][inv["role"]].append(inv)
        for inv_id, variants in sorted(self.ambiguous_invocations.items()):
            self._unattributed("ambiguous_invocations",
                               {"invocation_id": inv_id, "reason": "conflicting_duplicate_invocation_id"})
            for variant in variants:
                candidate = variant["issue_id"] or self.turn_issue.get(variant["turn_id"])
                self._note(candidate, "ambiguous_invocations", inv_id)
        for inv in self.unidentified_invocations:
            self._unattributed("invocations", {"invocation_id": None, "reason": "missing_invocation_id"})
            self._note(inv["issue_id"] or self.turn_issue.get(inv["turn_id"]), "unidentified_invocations", id(inv))

    def _count_unattributed_cost(self, inv):
        bucket = self.unattributed_cost
        bucket["invocations"] += 1
        if inv["cost"] is None:
            bucket["unknown_cost_invocations"] += 1
            return
        bucket["known_usd"] += inv["cost"]


def new_acc():
    return {"turns": dict.fromkeys(CATEGORIES + ("unclassified",), 0), "sessions": set(),
            "roles": {role: [] for role in ROLES},
            "unattributed_turns": set(), "ambiguous_turns": set(), "unidentified_turns": set(),
            "unattributed_invocations": set(), "ambiguous_invocations": set(),
            "unidentified_invocations": set()}


# ------------------------------------------------------------------- output

def money(value):
    return str(value.quantize(Decimal("0.000001")))


def ratio(numerator, denominator):
    if denominator == 0:
        return None
    return str((Decimal(numerator) / Decimal(denominator)).quantize(Decimal("0.0001")))


def mean(total, count, places="0.000001"):
    if count == 0:
        return None
    return str((Decimal(total) / Decimal(count)).quantize(Decimal(places)))


def is_opus(model):
    return model is not None and model.startswith("claude-opus")


def role_cost(invocations, evidence_ok):
    known, unknown_cost, unknown_model, non_opus, opus = Decimal(0), 0, 0, 0, 0
    for inv in invocations:
        if inv["model"] is None:
            unknown_model += 1
            continue
        if not is_opus(inv["model"]):
            non_opus += 1
            continue
        opus += 1
        if inv["cost"] is None:
            unknown_cost += 1
            continue
        known += inv["cost"]
    complete = evidence_ok and unknown_cost == 0 and unknown_model == 0
    return {"usd": money(known) if complete else None, "known_usd": money(known),
            "opus_invocations": opus, "unknown_cost_invocations": unknown_cost,
            "unknown_model_invocations": unknown_model, "non_opus_invocations": non_opus}


def issue_state(record):
    return "censored" if record["outcome"] == "open" else record["outcome"]


def issue_row(issue_id, record, acc, data):
    reasons_turns, reasons_cost, missing = [], [], []
    if record["coverage_turns"] != "complete":
        reasons_turns.append(f"turn_coverage_{record['coverage_turns']}")
    if record["lineage_id"] is None:
        reasons_turns.append("no_manager_lineage")
    for key, label in (("unattributed_turns", "unattributed_turns"), ("ambiguous_turns", "ambiguous_turns"),
                       ("unidentified_turns", "unidentified_turns")):
        if acc[key]:
            reasons_turns.append(label)
    if record["coverage_invocations"] != "complete":
        reasons_cost.append(f"invocation_coverage_{record['coverage_invocations']}")
    for key in ("unattributed_invocations", "ambiguous_invocations", "unidentified_invocations"):
        if acc[key]:
            reasons_cost.append(key)
    lineage_sessions = data.lineage_sessions.get(record["lineage_id"], set())
    cost = {role: role_cost(acc["roles"][role], not reasons_cost) for role in ROLES}
    for role in ROLES:
        if cost[role]["unknown_cost_invocations"]:
            reasons_cost.append(f"unknown_cost_{role}")
        if cost[role]["unknown_model_invocations"]:
            reasons_cost.append(f"unknown_model_{role}")
    if record["route"] == "topology" and record["execution_id"] is None:
        missing.append("execution_id")
    if record["outcome"] == "landed":
        for key, label in (("published_ns", "published_at"), ("landed_sha", "landed_sha")):
            if record[key] is None:
                missing.append(label)
    latency = None
    if record["published_ns"] is not None:
        if record["published_ns"] < record["opened_ns"]:
            missing.append("published_before_opened")
        else:
            latency = str((Decimal(record["published_ns"] - record["opened_ns"]) / NS).quantize(Decimal("0.001")))
    turns_total = sum(acc["turns"].values())
    turns_complete = not reasons_turns
    cost_complete = not reasons_cost
    return {
        "issue_id": issue_id, "route": record["route"], "route_name": record["route_name"],
        "execution_id": record["execution_id"], "outcome": record["outcome"], "state": issue_state(record),
        "opened_at": ts_text(record["opened_ns"]), "observed_at": ts_text(record["observed_ns"]),
        "published_at": ts_text(record["published_ns"]) if record["published_ns"] is not None else None,
        "settled_at": ts_text(record["settled_ns"]) if record["settled_ns"] is not None else None,
        "source_sha": record["source_sha"], "landed_sha": record["landed_sha"],
        "gate_reason": record["gate_reason"], "review_status": record["review_status"],
        "publication_latency_seconds": latency,
        "manager": {
            "lineage_id": record["lineage_id"],
            "sessions_seen": len(acc["sessions"]),
            "rotations": max(len(lineage_sessions) - 1, 0),
            "turns": {**acc["turns"], "total": turns_total},
            "turns_complete": turns_complete, "incomplete_reasons": sorted(set(reasons_turns)),
        },
        "opus_cost": cost, "cost_complete": cost_complete,
        "cost_incomplete_reasons": sorted(set(reasons_cost)),
        "missing": sorted(set(missing)),
        "attribution_complete": turns_complete and cost_complete and not missing,
        "_ns": {"opened": record["opened_ns"], "observed": record["observed_ns"]},
    }


def aggregate(rows):
    turns = {name: Decimal(0) for name in CATEGORIES + ("unclassified", "total")}
    turn_issues = 0
    usd = {role: Decimal(0) for role in ROLES}
    usd_issues = dict.fromkeys(ROLES, 0)
    latency, latency_n = Decimal(0), 0
    for row in rows:
        if row["manager"]["turns_complete"]:
            turn_issues += 1
            for name in turns:
                turns[name] += row["manager"]["turns"][name]
        for role in ROLES:
            value = row["opus_cost"][role]["usd"]
            if value is not None:
                usd_issues[role] += 1
                usd[role] += Decimal(value)
        if row["publication_latency_seconds"] is not None:
            latency += Decimal(row["publication_latency_seconds"])
            latency_n += 1
    return {
        "issues": len(rows),
        "turn_evidence_issues": turn_issues,
        "manager_turns_sum": {name: int(total) for name, total in turns.items()},
        "manager_turns_mean": mean(turns["total"], turn_issues, "0.0001"),
        "manager_turns_mean_by_category": {name: mean(turns[name], turn_issues, "0.0001")
                                           for name in CATEGORIES + ("unclassified",)},
        "cost_evidence_issues": usd_issues,
        "opus_usd_sum": {role: money(usd[role]) if usd_issues[role] else None for role in ROLES},
        "opus_usd_mean": {role: mean(usd[role], usd_issues[role]) for role in ROLES},
        "publication_latency_issues": latency_n,
        "publication_latency_mean_seconds": mean(latency, latency_n, "0.001"),
    }


def summarize(rows):
    states = {state: aggregate([row for row in rows if row["state"] == state]) for state in STATES}
    eligible = len(rows)
    turns_ok = sum(1 for row in rows if row["manager"]["turns_complete"])
    cost_ok = sum(1 for row in rows if row["cost_complete"])
    review = {}
    for row in rows:
        review[row["review_status"]] = review.get(row["review_status"], 0) + 1
    return {
        "eligible": eligible,
        "distinct_landed": states["landed"]["issues"],
        "by_state": {state: states[state] for state in STATES},
        "all_eligible": aggregate(rows),
        "review_status_counts": dict(sorted(review.items())),
        "attribution": {
            "turn_evidence_issues": turns_ok, "cost_evidence_issues": cost_ok,
            "fully_attributed_issues": sum(1 for row in rows if row["attribution_complete"]),
            "turn_coverage": ratio(turns_ok, eligible), "cost_coverage": ratio(cost_ok, eligible),
        },
    }


def group_rows(rows, key):
    groups = {}
    for row in rows:
        groups.setdefault(key(row), []).append(row)
    return {name: summarize(groups[name]) for name in sorted(groups)}


def window_report(name, start, end, issue_rows):
    rows = sorted((row for row in issue_rows if start <= row["_ns"]["opened"] < end),
                  key=lambda row: row["issue_id"])
    follow_up = [row["_ns"]["observed"] - end for row in rows]
    public_rows = [{key: value for key, value in row.items() if key != "_ns"} for row in rows]
    return {
        "name": name, "start": ts_text(start), "end": ts_text(end),
        "duration_seconds": (end - start) // NS, "_start": start, "_end": end,
        "min_follow_up_seconds": (str((Decimal(min(follow_up)) / NS).quantize(Decimal("0.001")))
                                  if follow_up else None),
        "summary": summarize(rows),
        "routes": group_rows(rows, lambda row: row["route"]),
        "strata": group_rows(rows, lambda row: f"{row['route']}/{row['route_name'] or 'unspecified'}"),
        "issues": public_rows,
    }


def compare_windows(before, after, min_coverage):
    reasons = []
    if before["_end"] - before["_start"] != after["_end"] - after["_start"]:
        reasons.append("unequal_window_length")
    if before["_start"] >= after["_start"]:
        reasons.append("windows_not_ordered")
    if before["_end"] > after["_start"] and after["_end"] > before["_start"]:
        reasons.append("windows_overlap")
    routes = {}
    for route in ROUTES:
        routes[route] = compare_route(before["routes"].get(route), after["routes"].get(route),
                                      min_coverage, not reasons)
    return {
        "before": before["name"], "after": after["name"],
        "window_seconds": {"before": before["duration_seconds"], "after": after["duration_seconds"]},
        "equal_window_length": "unequal_window_length" not in reasons,
        "comparable": not reasons, "reasons": reasons,
        "min_coverage": str(min_coverage), "routes": routes,
        "note": "descriptive arithmetic on the supplied export only; not a production, deployment or causal claim",
    }


def compare_route(before, after, min_coverage, windows_ok):
    reasons = [] if windows_ok else ["window_comparison_invalid"]
    side = {}
    for label, summary in (("before", before), ("after", after)):
        side[label] = None if summary is None else {
            "eligible": summary["eligible"], "distinct_landed": summary["distinct_landed"],
            "turn_coverage": summary["attribution"]["turn_coverage"],
            "cost_coverage": summary["attribution"]["cost_coverage"]}
        if summary is None or summary["eligible"] == 0:
            reasons.append(f"no_eligible_issues_{label}")
            continue
        if summary["distinct_landed"] == 0:
            reasons.append(f"no_landed_issues_{label}")
        for kind in ("turn_coverage", "cost_coverage"):
            if Decimal(summary["attribution"][kind]) < min_coverage:
                reasons.append(f"{kind}_below_minimum_{label}")
    result = {"comparable": not reasons, "reasons": reasons, **side, "delta": None}
    if reasons:
        return result
    result["delta"] = {
        "landed_manager_turns_mean": delta(before["by_state"]["landed"]["manager_turns_mean"],
                                           after["by_state"]["landed"]["manager_turns_mean"]),
        "landed_manager_opus_usd_mean": delta(before["by_state"]["landed"]["opus_usd_mean"]["manager"],
                                              after["by_state"]["landed"]["opus_usd_mean"]["manager"]),
        "landed_follow_up_opus_usd_mean": delta(before["by_state"]["landed"]["opus_usd_mean"]["follow_up"],
                                                after["by_state"]["landed"]["opus_usd_mean"]["follow_up"]),
        "all_eligible_manager_turns_mean": delta(before["all_eligible"]["manager_turns_mean"],
                                                 after["all_eligible"]["manager_turns_mean"]),
    }
    return result


def delta(before, after):
    if before is None or after is None:
        return None
    return str(Decimal(after) - Decimal(before))


def cap(rows):
    return {"total": len(rows), "truncated": len(rows) > LIST_CAP, "rows": rows[:LIST_CAP]}


def build_report(docs, windows, compare=None, min_coverage=Decimal("0.8")):
    data = Dataset(docs)
    issue_rows = [issue_row(issue_id, record, data.acc[issue_id], data)
                  for issue_id, record in sorted(data.issues.items())]
    reports = [window_report(name, start, end, issue_rows) for name, start, end in windows]
    comparison = None
    if compare:
        by_name = {report["name"]: report for report in reports}
        comparison = compare_windows(by_name[compare[0]], by_name[compare[1]], min_coverage)
    for report in reports:
        del report["_start"], report["_end"]
    return {
        "schema": REPORT_SCHEMA,
        "input": {
            "exports": [{"id": export["id"], "generated_at": ts_text(export["generated_ns"])}
                        for export in data.exports],
            "issues": len(data.issues), "duplicates_collapsed": data.duplicates,
            "ambiguous_issue_ids": sorted(data.ambiguous_issues),
            "unidentified": {"issue_records": len(data.unidentified_issues),
                             "turns": len(data.unidentified_turns),
                             "invocations": len(data.unidentified_invocations)},
        },
        "sampling_rules": SAMPLING_RULES,
        "windows": reports,
        "comparison": comparison,
        "unattributed": {
            **{key: cap(rows) for key, rows in data.unattributed.items()},
            "invocation_cost": {
                "invocations": data.unattributed_cost["invocations"],
                "known_usd": money(data.unattributed_cost["known_usd"]),
                "unknown_cost_invocations": data.unattributed_cost["unknown_cost_invocations"],
                "note": "not assignable to any Issue; excluded from every per-Issue cost"},
        },
        "fleet_context": {"rows": data.fleet,
                          "note": "fleet aggregates are context only and are never attributed to an Issue"},
        "claims": "observations of the supplied export only; no production improvement is claimed",
    }


# ---------------------------------------------------------------------- CLI

def parse_window(text):
    name, _, span = text.partition("=")
    start_text, _, end_text = span.partition(",")
    if not NAME_RE.match(name) or not start_text or not end_text:
        fail("--window", f"expected NAME=START,END (RFC3339), got {text!r}")
    start, end = parse_ts(start_text, f"--window {name} start"), parse_ts(end_text, f"--window {name} end")
    if end <= start:
        fail("--window", f"{name}: end must be after start (half-open [start, end))")
    if end - start > MAX_WINDOW_DAYS * 86400 * NS:
        fail("--window", f"{name}: longer than {MAX_WINDOW_DAYS} days")
    return name, start, end


def resolve_windows(texts, compare):
    if not texts:
        fail("--window", "at least one --window NAME=START,END is required")
    if len(texts) > MAX_WINDOWS:
        fail("--window", f"at most {MAX_WINDOWS} windows")
    windows = [parse_window(text) for text in texts]
    names = [name for name, _, _ in windows]
    if len(set(names)) != len(names):
        fail("--window", "window names must be unique")
    if compare is None and {"before", "after"} <= set(names):
        compare = ["before", "after"]
    unknown = [name for name in (compare or []) if name not in names]
    if unknown:
        fail("--compare", f"unknown window name(s) {unknown}; defined: {names}")
    return windows, compare


def parse_args(argv):
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--input", action="append", required=True, metavar="FILE",
                        help="sanitized export (repeatable; '-' is stdin); repeats are deduplicated")
    parser.add_argument("--window", action="append", metavar="NAME=START,END",
                        help="half-open RFC3339 window (repeatable)")
    parser.add_argument("--compare", nargs=2, metavar=("BEFORE", "AFTER"),
                        help="compare two named windows (default: before/after when both exist)")
    parser.add_argument("--min-coverage", default="0.8",
                        help="minimum attribution coverage (0-1) for a comparable route (default 0.8)")
    parser.add_argument("--max-bytes", type=int, default=DEFAULT_MAX_BYTES,
                        help=f"per-input size bound (default {DEFAULT_MAX_BYTES})")
    parser.add_argument("--output", metavar="FILE", help="write the report here instead of stdout")
    parser.add_argument("--fail-on-incomparable", action="store_true",
                        help="exit 3 when the requested comparison is not comparable")
    return parser.parse_args(argv)


def run(args):
    if args.max_bytes < 1:
        fail("--max-bytes", "must be positive")
    try:
        min_coverage = Decimal(args.min_coverage)
    except InvalidOperation:
        fail("--min-coverage", f"not a number: {args.min_coverage!r}")
    if not min_coverage.is_finite() or not 0 <= min_coverage <= 1:
        fail("--min-coverage", "must be between 0 and 1")
    windows, compare = resolve_windows(args.window, args.compare)
    docs = load_documents(args.input, args.max_bytes)
    return build_report(docs, windows, compare, min_coverage)


def main(argv=None):
    args = parse_args(argv)
    try:
        report = run(args)
    except ContractError as err:
        print(f"measure-issue-pipeline: {err}", file=sys.stderr)
        return 2
    text = json.dumps(report, indent=2, sort_keys=True) + "\n"
    if args.output:
        with open(args.output, "w", encoding="utf-8") as handle:
            handle.write(text)
    else:
        sys.stdout.write(text)
    comparison = report["comparison"]
    if args.fail_on_incomparable and (comparison is None or not comparison["comparable"]):
        print("measure-issue-pipeline: comparison is not comparable: "
              + (", ".join(comparison["reasons"]) if comparison else "no comparison requested"),
              file=sys.stderr)
        return 3
    return 0


if __name__ == "__main__":
    sys.exit(main())
