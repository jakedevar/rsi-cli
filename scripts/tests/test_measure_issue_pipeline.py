"""measure-issue-pipeline: known per-Issue turns/cost from synthetic sanitized exports (#2304 slice 5).

Every number here comes from the synthetic fixture below; none is a production
measurement. The tool reads supplied JSON only: no DB, network or cargo.
"""

import copy
from decimal import Decimal
import importlib.util
import json
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import unittest

ROOT = Path(__file__).resolve().parents[2]
SCRIPT = ROOT / "scripts/measure-issue-pipeline.py"

_spec = importlib.util.spec_from_file_location("measure_issue_pipeline", SCRIPT)
mip = importlib.util.module_from_spec(_spec)
sys.modules["measure_issue_pipeline"] = mip
_spec.loader.exec_module(mip)

BEFORE = "before=2026-10-01T00:00:00Z,2026-10-08T00:00:00Z"
AFTER = "after=2026-10-15T00:00:00Z,2026-10-22T00:00:00Z"
OPUS = "claude-opus-4-1"


def at(day, hms="00:00:00"):
    return f"2026-10-{day:02d}T{hms}Z"


class Export:
    """Builds one synthetic sanitized export."""

    def __init__(self, export_id="exp-1"):
        self.doc = {
            "schema_version": 1,
            "export": {"id": export_id, "generated_at": at(30), "sanitized": True},
            "issues": [], "lineages": [], "turns": [], "invocations": [], "fleet_totals": [],
        }

    def lineage(self, lineage_id, *sessions):
        self.doc["lineages"].append({"lineage_id": lineage_id, "session_ids": list(sessions)})

    def issue(self, issue_id, route, outcome, opened, lineage="L-hand", coverage="complete",
              **extra):
        row = {"issue_id": issue_id, "route": route, "outcome": outcome, "opened_at": opened,
               "observed_at": at(29), "manager_lineage_id": lineage,
               "coverage": {"turns": coverage, "invocations": coverage}}
        if route == "topology":
            row["route_name"] = "issue-pipeline"
            row["execution_id"] = f"exec-{issue_id.strip('#')}"
        if outcome == "landed":
            row["landed_sha"] = "a" * 40
        row.update(extra)
        self.doc["issues"].append(row)

    def turn(self, turn_id, issue_id, session, when, category):
        self.doc["turns"].append({"turn_id": turn_id, "issue_id": issue_id, "session_id": session,
                                  "at": when, "category": category})

    def invocation(self, inv_id, session, role, cost, issue_id=None, turn_id=None, model=OPUS,
                   when=None):
        row = {"invocation_id": inv_id, "session_id": session, "role": role, "model": model,
               "cost_usd": cost, "at": when or at(2)}
        if issue_id:
            row["issue_id"] = issue_id
        if turn_id:
            row["turn_id"] = turn_id
        self.doc["invocations"].append(row)

    def turns(self, issue_id, session, when, categories, prefix):
        for index, category in enumerate(categories):
            self.turn(f"{prefix}{index}", issue_id, session, when, category)


def known_export():
    """Hand: #101 landed (rotation m1->m1b), #102 failed. Topology: #201 landed, #202 blocked, #203 censored."""
    export = Export()
    export.lineage("L-hand", "m1", "m1b")
    export.lineage("L-topo", "g1")
    export.issue("#101", "hand", "landed", at(2), published_at=at(2, "02:00:00"))
    export.turn("t1", "#101", "m1", at(2), "setup")
    export.turn("t2", "#101", "m1", at(2), "setup")
    export.turn("t3", "#101", "m1", at(2), "mechanical")
    export.turn("t4", "#101", "m1b", at(2), "mechanical")  # after rotation: same owner lineage
    export.turn("t5", "#101", "m1b", at(2), "mechanical")
    export.turn("t6", "#101", "m1b", at(2), "real_gate")
    export.turn("t7", "#101", "m1b", at(2), "terminal_summary")
    export.invocation("i1", "m1", "manager", 1.25, turn_id="t1")
    export.invocation("i2", "m1b", "manager", 0.75, turn_id="t6")
    export.invocation("i3", "f1", "follow_up", 0.5, issue_id="#101")
    export.invocation("i4", "m1", "manager", 9, turn_id="t2", model="claude-sonnet-4-5")
    export.issue("#102", "hand", "failed", at(3), settled_at=at(3, "05:00:00"))
    export.turn("t8", "#102", "m1b", at(3), "setup")
    export.turn("t9", "#102", "m1b", at(3), "real_gate")
    export.invocation("i5", "m1b", "manager", 4.0, turn_id="t8")
    export.issue("#201", "topology", "landed", at(4), "L-topo", published_at=at(4, "01:00:00"))
    export.turn("t10", "#201", "g1", at(4), "setup")
    export.turn("t11", "#201", "g1", at(4), "terminal_summary")
    export.invocation("i6", "g1", "manager", 0.25, turn_id="t10")
    export.issue("#202", "topology", "blocked", at(5), "L-topo", settled_at=at(5, "03:00:00"),
                 gate_reason="review_blocked")
    export.turn("t12", "#202", "g1", at(5), "setup")
    export.turn("t13", "#202", "g1", at(5), "real_gate")
    export.invocation("i7", "g1", "manager", 1.0, turn_id="t12")
    export.issue("#203", "topology", "open", at(6), "L-topo")
    export.turn("t14", "#203", "g1", at(6), "setup")
    export.invocation("i8", "g1", "manager", 0.1, turn_id="t14")
    return export


def after_export():
    export = Export("exp-2")
    export.lineage("L-hand", "m1", "m1b")
    export.lineage("L-topo", "g1")
    export.issue("#204", "topology", "landed", at(16), "L-topo", published_at=at(16, "01:00:00"))
    export.turns("#204", "g1", at(16), ["setup", "terminal_summary"], "a")
    export.invocation("i20", "g1", "manager", 0.2, turn_id="a0")
    export.issue("#205", "topology", "landed", at(17), "L-topo", published_at=at(17, "01:00:00"))
    export.turns("#205", "g1", at(17), ["setup", "mechanical", "terminal_summary"], "b")
    export.invocation("i21", "g1", "manager", 0.4, turn_id="b0")
    export.issue("#103", "hand", "landed", at(18), published_at=at(18, "04:00:00"))
    export.turns("#103", "m1b", at(18), ["setup", "mechanical", "mechanical", "real_gate", "terminal_summary"], "c")
    export.invocation("i22", "m1b", "manager", 1.5, turn_id="c0")
    return export


def load(*exports):
    return [mip.validate_document(json.loads(json.dumps(e.doc), parse_float=Decimal), "fixture")
            for e in exports]


def windows(*texts):
    return mip.resolve_windows(list(texts), None)


def report(*exports, window_texts=(BEFORE,), min_coverage=Decimal("0.8")):
    win, compare = windows(*window_texts)
    return mip.build_report(load(*exports), win, compare, min_coverage)


def issue_of(rep, issue_id, window=0):
    return next(row for row in rep["windows"][window]["issues"] if row["issue_id"] == issue_id)


class KnownTotalsTest(unittest.TestCase):
    def setUp(self):
        self.rep = report(known_export())

    def test_per_issue_turn_counts_by_category_across_rotation(self):
        turns = issue_of(self.rep, "#101")["manager"]
        self.assertEqual(turns["turns"], {"setup": 2, "mechanical": 3, "real_gate": 1,
                                          "terminal_summary": 1, "unclassified": 0, "total": 7})
        self.assertEqual((turns["lineage_id"], turns["sessions_seen"], turns["rotations"]), ("L-hand", 2, 1))
        self.assertTrue(turns["turns_complete"])

    def test_terminal_wake_turns_are_counted(self):
        self.assertEqual(issue_of(self.rep, "#201")["manager"]["turns"]["terminal_summary"], 1)
        self.assertEqual(issue_of(self.rep, "#201")["manager"]["turns"]["total"], 2)

    def test_opus_cost_is_per_role_and_excludes_other_models(self):
        cost = issue_of(self.rep, "#101")["opus_cost"]
        self.assertEqual(cost["manager"]["usd"], "2.000000")
        self.assertEqual(cost["manager"]["non_opus_invocations"], 1)
        self.assertEqual(cost["follow_up"]["usd"], "0.500000")
        self.assertEqual(cost["worker"]["usd"], "0.000000")  # complete evidence, truly none

    def test_publication_latency_and_review_fields(self):
        row = issue_of(self.rep, "#101")
        self.assertEqual(row["publication_latency_seconds"], "7200.000")
        self.assertEqual(row["review_status"], "unknown")
        self.assertEqual(issue_of(self.rep, "#202")["gate_reason"], "review_blocked")
        self.assertIsNone(issue_of(self.rep, "#202")["publication_latency_seconds"])

    def test_route_summaries_count_distinct_landed_and_means(self):
        routes = self.rep["windows"][0]["routes"]
        hand, topo = routes["hand"], routes["topology"]
        self.assertEqual((hand["eligible"], hand["distinct_landed"]), (2, 1))
        self.assertEqual((topo["eligible"], topo["distinct_landed"]), (3, 1))
        self.assertEqual(hand["by_state"]["landed"]["manager_turns_mean"], "7.0000")
        self.assertEqual(hand["all_eligible"]["manager_turns_mean"], "4.5000")
        self.assertEqual(topo["all_eligible"]["manager_turns_mean"], "1.6667")
        self.assertEqual(hand["by_state"]["landed"]["opus_usd_mean"]["manager"], "2.000000")
        self.assertEqual(hand["by_state"]["landed"]["opus_usd_mean"]["follow_up"], "0.500000")

    def test_failed_blocked_and_censored_work_is_exposed_not_hidden(self):
        hand = self.rep["windows"][0]["routes"]["hand"]["by_state"]
        topo = self.rep["windows"][0]["routes"]["topology"]["by_state"]
        self.assertEqual(hand["failed"]["issues"], 1)
        self.assertEqual(hand["failed"]["opus_usd_sum"]["manager"], "4.000000")  # the expensive failure
        self.assertEqual((topo["blocked"]["issues"], topo["censored"]["issues"]), (1, 1))
        self.assertEqual(topo["blocked"]["manager_turns_sum"]["total"], 2)
        self.assertEqual(topo["censored"]["manager_turns_sum"]["total"], 1)
        self.assertEqual(self.rep["windows"][0]["routes"]["hand"]["all_eligible"]["opus_usd_sum"]["manager"],
                         "6.000000")

    def test_strata_and_attribution_coverage(self):
        window = self.rep["windows"][0]
        self.assertEqual(sorted(window["strata"]), ["hand/unspecified", "topology/issue-pipeline"])
        attribution = window["summary"]["attribution"]
        self.assertEqual((attribution["turn_coverage"], attribution["cost_coverage"]), ("1.0000", "1.0000"))
        self.assertEqual(attribution["fully_attributed_issues"], 5)

    def test_report_is_deterministic(self):
        again = report(known_export())
        self.assertEqual(json.dumps(self.rep, sort_keys=True), json.dumps(again, sort_keys=True))


class DedupeTest(unittest.TestCase):
    def test_repeat_export_does_not_double_count_turns_or_cost(self):
        once = report(known_export())
        twice = report(known_export(), known_export())
        self.assertEqual(issue_of(twice, "#101"), issue_of(once, "#101"))
        self.assertEqual(twice["windows"][0]["summary"], once["windows"][0]["summary"])
        self.assertEqual(twice["input"]["duplicates_collapsed"],
                         {"issue_records": 5, "turns": 14, "invocations": 8})

    def test_conflicting_turn_id_is_ambiguous_and_never_counted_as_zero(self):
        export = known_export()
        export.turn("t3", "#101", "m1", at(2), "real_gate")  # same id, different category
        rep = report(export)
        row = issue_of(rep, "#101")
        self.assertEqual(row["manager"]["turns"]["total"], 6)
        self.assertFalse(row["manager"]["turns_complete"])
        self.assertIn("ambiguous_turns", row["manager"]["incomplete_reasons"])
        self.assertEqual(rep["unattributed"]["ambiguous_turns"]["rows"][0]["turn_id"], "t3")
        self.assertIsNone(rep["windows"][0]["routes"]["hand"]["by_state"]["landed"]["manager_turns_mean"])

    def test_conflicting_invocation_cost_stays_unknown(self):
        export = known_export()
        export.invocation("i2", "m1b", "manager", 99, turn_id="t6")  # i2 already recorded as 0.75
        row = issue_of(report(export), "#101")
        self.assertIsNone(row["opus_cost"]["manager"]["usd"])
        self.assertEqual(row["opus_cost"]["manager"]["known_usd"], "1.250000")
        self.assertFalse(row["cost_complete"])
        self.assertIn("ambiguous_invocations", row["cost_incomplete_reasons"])

    def test_rows_without_ids_are_quarantined_not_summed(self):
        export = known_export()
        export.doc["turns"].append({"issue_id": "#101", "session_id": "m1", "at": at(2), "category": "setup"})
        export.doc["invocations"].append({"role": "manager", "at": at(2), "model": OPUS, "cost_usd": 5,
                                          "issue_id": "#101", "session_id": "m1"})
        rep = report(export)
        row = issue_of(rep, "#101")
        self.assertEqual(row["manager"]["turns"]["total"], 7)
        self.assertEqual(row["opus_cost"]["manager"]["known_usd"], "2.000000")
        self.assertFalse(row["attribution_complete"])
        self.assertEqual(rep["input"]["unidentified"], {"issue_records": 0, "turns": 1, "invocations": 1})

    def test_open_record_yields_to_later_terminal_record(self):
        early = known_export()
        early.doc["issues"] = [dict(early.doc["issues"][0], outcome="open", observed_at=at(3))]
        early.doc["issues"][0].pop("published_at")
        late = known_export()
        rep = report(early, late)
        self.assertEqual(issue_of(rep, "#101")["state"], "landed")
        self.assertEqual(rep["input"]["ambiguous_issue_ids"], [])

    def test_conflicting_issue_records_are_ambiguous_and_excluded(self):
        other = known_export()
        other.doc["issues"][0]["outcome"] = "failed"
        rep = report(known_export(), other)
        self.assertEqual(rep["input"]["ambiguous_issue_ids"], ["#101"])
        self.assertEqual(rep["windows"][0]["routes"]["hand"]["eligible"], 1)
        self.assertEqual({row["issue_id"] for row in rep["windows"][0]["issues"]},
                         {"#102", "#201", "#202", "#203"})
        reasons = {row["turn_id"]: row["reason"] for row in rep["unattributed"]["turns"]["rows"]}
        self.assertEqual(reasons["t1"], "unknown_or_ambiguous_issue")

    def test_issue_without_id_is_quarantined(self):
        export = known_export()
        export.doc["issues"].append({"route": "hand", "outcome": "landed", "opened_at": at(2),
                                     "observed_at": at(3)})
        rep = report(export)
        self.assertEqual(rep["input"]["unidentified"]["issue_records"], 1)
        self.assertEqual(rep["windows"][0]["summary"]["eligible"], 5)


class AttributionTest(unittest.TestCase):
    def test_turn_from_a_session_outside_the_issue_lineage_is_unattributed(self):
        export = known_export()
        export.turn("tx", "#101", "stranger", at(2), "mechanical")
        rep = report(export)
        row = issue_of(rep, "#101")
        self.assertEqual(row["manager"]["turns"]["total"], 7)
        self.assertFalse(row["manager"]["turns_complete"])
        self.assertIn({"turn_id": "tx", "reason": "session_not_in_lineage"},
                      rep["unattributed"]["turns"]["rows"])

    def test_session_in_two_lineages_is_ambiguous(self):
        export = known_export()
        export.lineage("L-other", "m1")
        rep = report(export)
        self.assertEqual(issue_of(rep, "#101")["manager"]["turns"]["total"], 4)  # only m1b turns remain
        reasons = {row["reason"] for row in rep["unattributed"]["turns"]["rows"]}
        self.assertEqual(reasons, {"session_in_multiple_lineages"})

    def test_turn_without_issue_is_unattributed(self):
        export = known_export()
        export.turn("shared", None, "m1", at(2), "mechanical")  # one turn serving several Issues
        rep = report(export)
        self.assertEqual(issue_of(rep, "#101")["manager"]["turns"]["total"], 7)
        self.assertIn({"turn_id": "shared", "reason": "no_issue"}, rep["unattributed"]["turns"]["rows"])

    def test_missing_category_is_unclassified_but_still_counted(self):
        export = known_export()
        export.turn("t15", "#203", "g1", at(6), None)
        row = issue_of(report(export), "#203")
        self.assertEqual((row["manager"]["turns"]["unclassified"], row["manager"]["turns"]["total"]), (1, 2))

    def test_invocation_links_through_its_turn_and_conflicts_are_ambiguous(self):
        export = known_export()
        export.invocation("i30", "m1b", "manager", 2.0, issue_id="#102", turn_id="t6")  # t6 belongs to #101
        rep = report(export)
        self.assertEqual(issue_of(rep, "#101")["opus_cost"]["manager"]["usd"], "2.000000")
        self.assertIn({"invocation_id": "i30", "reason": "turn_issue_conflict"},
                      rep["unattributed"]["invocations"]["rows"])
        self.assertFalse(issue_of(rep, "#102")["cost_complete"])

    def test_manager_invocation_from_foreign_session_is_not_an_issue_cost(self):
        export = known_export()
        export.invocation("i31", "stranger", "manager", 7.0, issue_id="#101")
        rep = report(export)
        self.assertEqual(issue_of(rep, "#101")["opus_cost"]["manager"]["known_usd"], "2.000000")
        self.assertEqual(rep["unattributed"]["invocation_cost"]["known_usd"], "7.000000")

    def test_follow_up_cost_never_merges_into_manager_cost(self):
        cost = issue_of(report(known_export()), "#101")["opus_cost"]
        self.assertEqual((cost["manager"]["usd"], cost["follow_up"]["usd"]), ("2.000000", "0.500000"))
        self.assertNotIn("total", cost)


class UnknownEvidenceTest(unittest.TestCase):
    def test_missing_cost_is_unknown_never_zero(self):
        export = known_export()
        export.invocation("i40", "m1", "manager", None, turn_id="t3")
        row = issue_of(report(export), "#101")
        self.assertIsNone(row["opus_cost"]["manager"]["usd"])
        self.assertEqual(row["opus_cost"]["manager"]["unknown_cost_invocations"], 1)
        self.assertEqual(row["opus_cost"]["manager"]["known_usd"], "2.000000")
        self.assertIn("unknown_cost_manager", row["cost_incomplete_reasons"])

    def test_unknown_model_cannot_be_ruled_out_as_non_opus(self):
        export = known_export()
        export.invocation("i41", "m1", "manager", 3.0, turn_id="t3", model=None)
        row = issue_of(report(export), "#101")
        self.assertIsNone(row["opus_cost"]["manager"]["usd"])
        self.assertEqual(row["opus_cost"]["manager"]["unknown_model_invocations"], 1)

    def test_partial_or_missing_coverage_makes_counts_lower_bounds(self):
        export = known_export()
        export.doc["issues"][0]["coverage"] = {"turns": "partial"}
        rep = report(export)
        row = issue_of(rep, "#101")
        self.assertEqual(row["manager"]["turns"]["total"], 7)
        self.assertFalse(row["manager"]["turns_complete"])
        self.assertEqual(row["manager"]["incomplete_reasons"], ["turn_coverage_partial"])
        self.assertIsNone(row["opus_cost"]["manager"]["usd"])  # invocation coverage missing
        hand = rep["windows"][0]["routes"]["hand"]
        self.assertIsNone(hand["by_state"]["landed"]["manager_turns_mean"])
        self.assertEqual(hand["attribution"]["turn_coverage"], "0.5000")

    def test_landed_issue_missing_publication_evidence_is_reported(self):
        export = known_export()
        export.doc["issues"][0].pop("published_at")
        export.doc["issues"][0].pop("landed_sha")
        row = issue_of(report(export), "#101")
        self.assertEqual(row["missing"], ["landed_sha", "published_at"])
        self.assertIsNone(row["publication_latency_seconds"])
        self.assertFalse(row["attribution_complete"])

    def test_topology_issue_without_execution_or_lineage_is_incomplete(self):
        export = known_export()
        export.doc["issues"][2].pop("execution_id")
        export.doc["issues"][3]["manager_lineage_id"] = None
        rep = report(export)
        self.assertEqual(issue_of(rep, "#201")["missing"], ["execution_id"])
        row = issue_of(rep, "#202")
        self.assertIn("no_manager_lineage", row["manager"]["incomplete_reasons"])
        self.assertEqual(row["manager"]["turns"]["total"], 0)  # unattributed, not an Issue's zero

    def test_empty_but_complete_issue_has_known_zero_cost(self):
        export = Export()
        export.lineage("L-hand", "m1")
        export.issue("#9", "hand", "landed", at(2), published_at=at(2, "01:00:00"))
        row = issue_of(report(export), "#9")
        self.assertEqual(row["opus_cost"]["manager"]["usd"], "0.000000")
        self.assertEqual(row["manager"]["turns"]["total"], 0)


class FleetTotalsTest(unittest.TestCase):
    def test_fleet_totals_are_context_never_issue_cost(self):
        export = known_export()
        export.doc["fleet_totals"].append({"window_start": at(1), "window_end": at(8), "model": OPUS,
                                           "cost_usd": 500, "invocations": 900, "source": "efficiency-metrics"})
        export.issue("#300", "hand", "landed", at(7), "L-hand", coverage="missing",
                     published_at=at(7, "01:00:00"))
        rep = report(export)
        row = issue_of(rep, "#300")
        self.assertIsNone(row["opus_cost"]["manager"]["usd"])  # unknown, not a share of 500
        self.assertEqual(row["opus_cost"]["manager"]["known_usd"], "0.000000")
        hand = rep["windows"][0]["routes"]["hand"]
        self.assertEqual(hand["all_eligible"]["opus_usd_sum"]["manager"], "6.000000")
        fleet = rep["fleet_context"]["rows"][0]
        self.assertEqual(fleet["cost_usd"], "500.000000")
        self.assertTrue(fleet["attribution"].startswith("none"))

    def test_issueless_invocations_are_reported_apart_from_every_issue(self):
        export = known_export()
        export.invocation("fleet-1", "other", "manager", 123.0)
        export.invocation("fleet-2", "other", "manager", None)
        rep = report(export)
        bucket = rep["unattributed"]["invocation_cost"]
        self.assertEqual((bucket["invocations"], bucket["known_usd"], bucket["unknown_cost_invocations"]),
                         (2, "123.000000", 1))
        all_issue_usd = sum(Decimal(row["opus_cost"]["manager"]["usd"] or 0)
                            for row in rep["windows"][0]["issues"])
        self.assertEqual(all_issue_usd, Decimal("7.350000"))


class WindowTest(unittest.TestCase):
    def one_issue_export(self, opened):
        export = Export()
        export.lineage("L-hand", "m1")
        export.issue("#1", "hand", "landed", opened, published_at=at(30))
        return export

    def eligible(self, opened, window):
        rep = report(self.one_issue_export(opened), window_texts=(window,))
        return rep["windows"][0]["summary"]["eligible"]

    def test_window_is_half_open(self):
        self.assertEqual(self.eligible("2026-10-01T00:00:00Z", BEFORE), 1)  # start is inclusive
        self.assertEqual(self.eligible("2026-10-07T23:59:59.999999999Z", BEFORE), 1)
        self.assertEqual(self.eligible("2026-10-08T00:00:00Z", BEFORE), 0)  # end is exclusive
        self.assertEqual(self.eligible("2026-09-30T23:59:59Z", BEFORE), 0)

    def test_offset_timestamps_are_normalised(self):
        self.assertEqual(self.eligible("2026-10-07T20:00:00-04:00", BEFORE), 0)  # == 10-08T00:00Z
        self.assertEqual(self.eligible("2026-10-01T02:00:00+02:00", BEFORE), 1)  # == 10-01T00:00Z

    def test_adjacent_windows_partition_the_cohort_and_overlapping_ones_share_it(self):
        export = self.one_issue_export("2026-10-08T00:00:00Z")
        adjacent = report(export, window_texts=(BEFORE, "next=2026-10-08T00:00:00Z,2026-10-15T00:00:00Z"))
        self.assertEqual([w["summary"]["eligible"] for w in adjacent["windows"]], [0, 1])
        overlap = report(export, window_texts=("a=2026-10-01T00:00:00Z,2026-10-09T00:00:00Z",
                                               "b=2026-10-08T00:00:00Z,2026-10-16T00:00:00Z"))
        self.assertEqual([w["summary"]["eligible"] for w in overlap["windows"]], [1, 1])

    def test_window_validation(self):
        for bad in ("x=2026-10-02T00:00:00Z,2026-10-01T00:00:00Z", "x=2026-10-01T00:00:00Z,2026-10-01T00:00:00Z",
                    "x=2026-10-01,2026-10-02", "nameonly", "x=2024-01-01T00:00:00Z,2026-01-01T00:00:00Z",
                    "bad name=2026-10-01T00:00:00Z,2026-10-02T00:00:00Z"):
            with self.assertRaises(mip.ContractError, msg=bad):
                windows(bad)
        with self.assertRaises(mip.ContractError):
            windows(BEFORE, BEFORE)
        with self.assertRaises(mip.ContractError):
            mip.resolve_windows([BEFORE], ["before", "nope"])


class ComparisonTest(unittest.TestCase):
    def compare(self, *exports, window_texts=(BEFORE, AFTER), **kwargs):
        return report(*exports, window_texts=window_texts, **kwargs)["comparison"]

    def test_equal_windows_state_counts_coverage_and_deltas(self):
        comparison = self.compare(known_export(), after_export())
        self.assertTrue(comparison["comparable"], comparison["reasons"])
        self.assertTrue(comparison["equal_window_length"])
        self.assertEqual(comparison["window_seconds"], {"before": 604800, "after": 604800})
        topo = comparison["routes"]["topology"]
        self.assertEqual((topo["before"]["eligible"], topo["after"]["eligible"]), (3, 2))
        self.assertEqual((topo["before"]["distinct_landed"], topo["after"]["distinct_landed"]), (1, 2))
        self.assertEqual((topo["after"]["turn_coverage"], topo["after"]["cost_coverage"]), ("1.0000", "1.0000"))
        self.assertEqual(topo["delta"]["landed_manager_turns_mean"], "0.5000")  # 2.5 vs 2
        self.assertEqual(topo["delta"]["landed_manager_opus_usd_mean"], "0.050000")  # 0.30 vs 0.25
        self.assertEqual(topo["delta"]["landed_follow_up_opus_usd_mean"], "0.000000")
        hand = comparison["routes"]["hand"]
        self.assertEqual(hand["delta"]["landed_manager_turns_mean"], "-2.0000")  # 5 vs 7
        self.assertEqual(hand["delta"]["landed_manager_opus_usd_mean"], "-0.500000")
        self.assertEqual(hand["delta"]["landed_follow_up_opus_usd_mean"], "-0.500000")
        self.assertIn("not a production", comparison["note"])

    def test_report_makes_no_production_claim_and_lists_sampling_rules(self):
        rep = report(known_export(), after_export(), window_texts=(BEFORE, AFTER))
        self.assertIn("no production improvement is claimed", rep["claims"])
        self.assertTrue(any("half-open" in rule for rule in rep["sampling_rules"]))

    def test_unequal_window_lengths_are_not_comparable(self):
        comparison = self.compare(known_export(), after_export(),
                                  window_texts=(BEFORE, "after=2026-10-15T00:00:00Z,2026-10-23T00:00:00Z"))
        self.assertFalse(comparison["comparable"])
        self.assertFalse(comparison["equal_window_length"])
        self.assertIn("unequal_window_length", comparison["reasons"])
        self.assertIsNone(comparison["routes"]["topology"]["delta"])

    def test_overlapping_or_misordered_windows_are_not_comparable(self):
        overlap = self.compare(known_export(), window_texts=(BEFORE, "after=2026-10-05T00:00:00Z,2026-10-12T00:00:00Z"))
        self.assertIn("windows_overlap", overlap["reasons"])
        ordered = mip.build_report(load(known_export(), after_export()),
                                   *mip.resolve_windows([AFTER.replace("after", "before"),
                                                         BEFORE.replace("before", "after")], None))
        self.assertIn("windows_not_ordered", ordered["comparison"]["reasons"])

    def test_empty_or_low_coverage_route_is_not_comparable(self):
        export = after_export()
        export.doc["issues"] = [row for row in export.doc["issues"] if row["route"] == "topology"]
        comparison = self.compare(known_export(), export)
        self.assertFalse(comparison["routes"]["hand"]["comparable"])
        self.assertIn("no_eligible_issues_after", comparison["routes"]["hand"]["reasons"])
        self.assertTrue(comparison["routes"]["topology"]["comparable"])
        low = after_export()
        for row in low.doc["issues"]:
            row["coverage"]["invocations"] = "partial"
        comparison = self.compare(known_export(), low)
        self.assertIn("cost_coverage_below_minimum_after", comparison["routes"]["topology"]["reasons"])
        relaxed = self.compare(known_export(), low, min_coverage=Decimal("0"))
        self.assertTrue(relaxed["routes"]["topology"]["comparable"])

    def test_no_landed_issue_in_a_window_is_not_comparable(self):
        export = after_export()
        for row in export.doc["issues"]:
            row["outcome"] = "failed"
        comparison = self.compare(known_export(), export)
        self.assertIn("no_landed_issues_after", comparison["routes"]["topology"]["reasons"])


class ContractTest(unittest.TestCase):
    def check_rejected(self, mutate, expect):
        export = known_export()
        mutate(export.doc)
        with self.assertRaises(mip.ContractError) as ctx:
            load(export)
        self.assertIn(expect, str(ctx.exception))

    def test_content_and_credential_fields_are_refused(self):
        self.check_rejected(lambda d: d["turns"][0].update(content="hello"), "outside the sanitized contract")
        self.check_rejected(lambda d: d["invocations"][0].update(input_tokens=5), "invocations[0]")
        self.check_rejected(lambda d: d["issues"][0].update(api_key="x"), "issues[0]")
        self.check_rejected(lambda d: d.update(prompt="x"), "outside the sanitized contract")

    def test_export_must_declare_sanitized_and_a_known_version(self):
        self.check_rejected(lambda d: d["export"].update(sanitized=False), "must be true")
        self.check_rejected(lambda d: d.update(schema_version=2), "unsupported")

    def test_bad_values_name_the_offending_path(self):
        self.check_rejected(lambda d: d["turns"][1].update(category="chatter"), "turns[1].category")
        self.check_rejected(lambda d: d["turns"][2].update(at="yesterday"), "turns[2].at")
        self.check_rejected(lambda d: d["invocations"][0].update(cost_usd=-1), "invocations[0].cost_usd")
        self.check_rejected(lambda d: d["invocations"][0].update(cost_usd="3"), "invocations[0].cost_usd")
        self.check_rejected(lambda d: d["invocations"][0].update(cost_usd=True), "invocations[0].cost_usd")
        self.check_rejected(lambda d: d["invocations"][0].update(role="boss"), "invocations[0].role")
        self.check_rejected(lambda d: d["issues"][0].update(route="manual"), "issues[0].route")
        self.check_rejected(lambda d: d["issues"][0].update(landed_sha="XYZ"), "issues[0].landed_sha")
        self.check_rejected(lambda d: d["issues"][0].pop("opened_at"), "missing required field")
        self.check_rejected(lambda d: d["lineages"][0].update(session_ids="m1"), "lineages[0].session_ids")
        self.check_rejected(lambda d: d.update(issues={}), "issues: expected an array")

    def test_row_bounds_are_enforced(self):
        original = mip.MAX_TURNS
        self.addCleanup(setattr, mip, "MAX_TURNS", original)
        mip.MAX_TURNS = 3
        with self.assertRaises(mip.ContractError) as ctx:
            load(known_export())
        self.assertIn("exceeds the bound of 3", str(ctx.exception))


class CliTest(unittest.TestCase):
    def setUp(self):
        self.tmp = Path(tempfile.mkdtemp())
        self.addCleanup(shutil.rmtree, self.tmp)

    def write(self, name, export):
        path = self.tmp / name
        path.write_text(json.dumps(export.doc))
        return str(path)

    def run_cli(self, *args):
        return subprocess.run([sys.executable, "-I", str(SCRIPT), *args], capture_output=True, text=True)

    def test_cli_emits_the_report_for_before_and_after_exports(self):
        result = self.run_cli("--input", self.write("a.json", known_export()),
                              "--input", self.write("b.json", after_export()),
                              "--window", BEFORE, "--window", AFTER)
        self.assertEqual(result.returncode, 0, result.stderr)
        rep = json.loads(result.stdout)
        self.assertEqual(rep["schema"], "measure-issue-pipeline/1")
        self.assertTrue(rep["comparison"]["comparable"])
        self.assertEqual(issue_of(rep, "#101")["manager"]["turns"]["total"], 7)
        self.assertEqual(rep["windows"][1]["routes"]["topology"]["eligible"], 2)

    def test_cli_output_file_matches_in_process_report(self):
        out = self.tmp / "report.json"
        result = self.run_cli("--input", self.write("a.json", known_export()), "--window", BEFORE,
                              "--output", str(out))
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(result.stdout, "")
        self.assertEqual(json.loads(out.read_text()), json.loads(json.dumps(report(known_export()))))

    def test_cli_fail_on_incomparable_exits_three(self):
        args = ["--input", self.write("a.json", known_export()), "--window", BEFORE,
                "--window", "after=2026-10-15T00:00:00Z,2026-10-16T00:00:00Z"]
        self.assertEqual(self.run_cli(*args).returncode, 0)
        result = self.run_cli(*args, "--fail-on-incomparable")
        self.assertEqual(result.returncode, 3)
        self.assertIn("unequal_window_length", result.stderr)

    def test_cli_contract_violations_exit_two_with_a_clear_message(self):
        bad = known_export()
        bad.doc["turns"][0]["content"] = "secret transcript"
        result = self.run_cli("--input", self.write("bad.json", bad), "--window", BEFORE)
        self.assertEqual(result.returncode, 2)
        self.assertIn("measure-issue-pipeline:", result.stderr)
        self.assertIn("turns[0]", result.stderr)
        self.assertNotIn("secret transcript", result.stdout + result.stderr)
        self.assertEqual(result.stdout, "")

    def test_cli_input_size_and_json_errors(self):
        path = self.write("a.json", known_export())
        small = self.run_cli("--input", path, "--window", BEFORE, "--max-bytes", "50")
        self.assertEqual(small.returncode, 2)
        self.assertIn("exceeds the 50-byte bound", small.stderr)
        broken = self.tmp / "broken.json"
        broken.write_text("{nope")
        self.assertEqual(self.run_cli("--input", str(broken), "--window", BEFORE).returncode, 2)
        self.assertEqual(self.run_cli("--input", str(self.tmp / "missing.json"), "--window", BEFORE).returncode, 2)
        nan = self.tmp / "nan.json"
        nan.write_text('{"schema_version": 1, "cost": NaN}')
        self.assertEqual(self.run_cli("--input", str(nan), "--window", BEFORE).returncode, 2)

    def test_cli_requires_a_window_and_a_valid_coverage(self):
        path = self.write("a.json", known_export())
        self.assertEqual(self.run_cli("--input", path).returncode, 2)
        self.assertEqual(self.run_cli("--input", path, "--window", BEFORE, "--min-coverage", "2").returncode, 2)

    def test_help_documents_the_contract(self):
        result = self.run_cli("--help")
        self.assertEqual(result.returncode, 0)
        for text in ("INPUT CONTRACT", "ATTRIBUTION RULES", "half-open", "fleet_totals"):
            self.assertIn(text, result.stdout)


if __name__ == "__main__":
    unittest.main()
