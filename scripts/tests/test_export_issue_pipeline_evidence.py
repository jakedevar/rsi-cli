"""export-issue-pipeline-evidence: attribution and cost rules on a synthetic database (#2304 slice 6).

Every number comes from the in-memory fixture below; none is a production measurement.
"""

import importlib.util
import json
from pathlib import Path
import sqlite3
import sys
import unittest

ROOT = Path(__file__).resolve().parents[2]
SCRIPT = ROOT / "scripts/export-issue-pipeline-evidence.py"

_spec = importlib.util.spec_from_file_location("export_issue_pipeline_evidence", SCRIPT)
exp = importlib.util.module_from_spec(_spec)
sys.modules["export_issue_pipeline_evidence"] = exp
_spec.loader.exec_module(exp)

EXEC = "8a0693b2-20dc-4ea5-a71d-b552f0862da1"


class AttributionTest(unittest.TestCase):
    def test_execution_id_names_its_issue(self):
        text = f"Run 8a0693b2 settled. execution_id={EXEC} status=succeeded"
        self.assertEqual(exp.attribute_message(text, {EXEC: 2530}, {2530, 2573}), 2530)

    def test_display_number_names_its_issue(self):
        self.assertEqual(exp.attribute_message("Run (#2573) settled", {}, {2530, 2573}), 2573)

    def test_two_issues_are_never_split(self):
        self.assertIsNone(exp.attribute_message("#2530 and #2573 landed", {}, {2530, 2573}))

    def test_unknown_number_and_empty_text_are_unattributed(self):
        self.assertIsNone(exp.attribute_message("see #99999", {}, {2530}))
        self.assertIsNone(exp.attribute_message("", {}, {2530}))

    def test_timestamps_use_z(self):
        self.assertEqual(exp.norm_ts("2026-10-15T02:14:13.828573887+00:00"), "2026-10-15T02:14:13.828573887Z")


class CumulativeCostTest(unittest.TestCase):
    def setUp(self):
        self.conn = sqlite3.connect(":memory:")
        self.conn.row_factory = sqlite3.Row
        self.conn.execute(
            "create table model_invocations (id text, session_id text, purpose text, "
            "created_at text, model text, estimated_cost_usd real)")

    def add(self, i, purpose, at, cost, session="s1"):
        self.conn.execute("insert into model_invocations values (?,?,?,?,?,?)",
                          (i, session, purpose, at, "claude-opus-5-5", cost))

    def test_cumulative_snapshots_become_increments(self):
        self.add("a", "session.launch.fresh", "2026-10-15T00:00:00Z", 0.88)
        self.add("t", "session.title", "2026-10-15T00:00:01Z", 0.0)
        self.add("b", "session.continue.resume", "2026-10-15T00:01:00Z", 0.97)
        self.add("c", "session.continue.resume", "2026-10-15T00:02:00Z", 1.20)
        costs = [round(r["cost"], 2) for r in exp.session_invocations(self.conn, "s1")]
        self.assertEqual(costs, [0.88, 0.09, 0.23])
        self.assertAlmostEqual(sum(costs), 1.20, places=2)  # equals the session's final cost

    def test_a_drop_restarts_and_null_stays_unknown(self):
        self.add("a", "session.continue.resume", "2026-10-15T00:00:00Z", 5.0)
        self.add("b", "session.continue.resume", "2026-10-15T00:01:00Z", 1.0)
        self.add("c", "session.continue.resume", "2026-10-15T00:02:00Z", None)
        rows = exp.session_invocations(self.conn, "s1")
        self.assertEqual([r["cost"] for r in rows], [5.0, 1.0, None])


if __name__ == "__main__":
    unittest.main()
