"""The spend guard reads the operator caps (or the ledger header) and fails closed."""

import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import unittest

ROOT = Path(__file__).resolve().parents[2]
SPEND = ROOT / "scripts/cloud-spend.py"
GATE = ROOT / "scripts/cloud-gate.sh"

HEADER = (
    "# AWS satellite spend ledger\n\n"
    "Operator grant: $100 starting 2026-09-27 22:48:22 UTC (direct operator message).\n"
    "Configured compute estimate: $3.5616/hour. Stop and report by $90 cumulative under this grant.\n"
)


def window(cost):
    return f"Gate window i-0abc: stop 2026-09-28T09:09:14Z, elapsed 1 s, est compute ${cost} at $1.7808/h; ok.\n"


class CloudSpendTest(unittest.TestCase):
    def setUp(self):
        self.tmp = Path(tempfile.mkdtemp())
        self.addCleanup(shutil.rmtree, self.tmp)
        self.ledger = self.tmp / "spend.md"

    def check(self, text, caps=None, today="2030-01-01"):
        self.ledger.write_text(text)
        caps_file = self.tmp / "spend-caps.json"
        if caps is not None:
            caps_file.write_text(caps if isinstance(caps, str) else json.dumps(caps))
        return subprocess.run([sys.executable, str(SPEND), "check", "--ledger", str(self.ledger), "--today", today],
                              capture_output=True, text=True)

    def test_below_stop_line_allows_a_run(self):
        result = self.check(HEADER + window("40.0000") + window("49.9999"))
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("of $100.00 grant, stop line $90.00", result.stderr)

    def test_at_the_stop_line_refuses(self):
        result = self.check(HEADER + window("45.0000") + window("45.0000"))
        self.assertEqual(result.returncode, 4)
        self.assertIn("stop line $90.00", result.stderr)

    def test_stop_line_comes_from_the_header_not_the_script(self):
        text = HEADER.replace("by $90 cumulative", "by $5 cumulative") + window("5.0000")
        self.assertEqual(self.check(text).returncode, 4)

    def test_operator_caps_file_overrides_the_header_stop_line(self):
        result = self.check(HEADER + window("6.0000"), caps={"stop_line_usd": 5, "daily_cap_usd": 100})
        self.assertEqual(result.returncode, 4)
        self.assertIn("stop line $5.00", result.stderr)
        raised = self.check(HEADER + window("95.0000"), caps={"stop_line_usd": 200, "daily_cap_usd": 100})
        self.assertEqual(raised.returncode, 0, raised.stderr)

    def test_daily_cap_refuses_only_for_the_current_utc_day(self):
        text = HEADER + window("9.0000") + window("7.0000")
        caps = {"stop_line_usd": 90, "daily_cap_usd": 15}
        on_the_day = self.check(text, caps=caps, today="2026-09-28")
        self.assertEqual(on_the_day.returncode, 4)
        self.assertIn("of $15.00 daily cap", on_the_day.stderr)
        self.assertEqual(self.check(text, caps=caps, today="2026-09-29").returncode, 0)
        raised = self.check(text, caps={"stop_line_usd": 90, "daily_cap_usd": 20}, today="2026-09-28")
        self.assertEqual(raised.returncode, 0, raised.stderr)

    def test_daily_cap_defaults_to_fifteen_without_a_caps_file(self):
        text = HEADER + window("15.0000")
        self.assertEqual(self.check(text, today="2026-09-28").returncode, 4)
        self.assertEqual(self.check(HEADER + window("14.9000"), today="2026-09-28").returncode, 0)

    def test_unreadable_caps_file_fails_closed(self):
        self.assertEqual(self.check(HEADER + window("1.0000"), caps="not json").returncode, 4)

    def test_unreadable_header_fails_closed(self):
        self.assertEqual(self.check("no header here\n" + window("1.0000")).returncode, 4)
        result = subprocess.run([sys.executable, str(SPEND), "check", "--ledger", str(self.tmp / "missing.md")],
                                capture_output=True, text=True)
        self.assertEqual(result.returncode, 4)

    def test_real_ledger_shape_sums_gate_and_window_lines(self):
        text = HEADER + "Window 1 stop confirmed: elapsed 533 seconds, estimated compute $0.5275 at configured $3.5616/h.\n"
        text += window("2.0000")
        result = self.check(text)
        self.assertEqual(result.returncode, 0)
        self.assertIn("cumulative estimate $2.00 of $100.00", result.stderr)

    def cache(self, stats):
        out = self.tmp / "cache.json"
        result = subprocess.run([sys.executable, str(SPEND), "cache", "--json-out", str(out)],
                                input=stats, capture_output=True, text=True)
        return result, out

    def test_cache_summary_reports_hits_misses_and_rate(self):
        stats = json.dumps({"stats": {
            "cache_hits": {"counts": {"Rust": 90, "C/C++": 10}},
            "cache_misses": {"counts": {"Rust": 25}},
        }})
        result, out = self.cache(stats)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(result.stdout.strip(), "sccache_hits=100 sccache_misses=25 sccache_hit_rate=80.0%")
        self.assertEqual(json.loads(out.read_text()), {"hits": 100, "misses": 25, "hit_rate_percent": 80.0})

    def test_cache_summary_of_a_cold_run_is_zero_not_an_error(self):
        result, _ = self.cache(json.dumps({"stats": {"cache_hits": {"counts": {}}, "cache_misses": {"counts": {}}}}))
        self.assertEqual(result.stdout.strip(), "sccache_hits=0 sccache_misses=0 sccache_hit_rate=0.0%")

    def test_cache_summary_rejects_garbage(self):
        self.assertEqual(self.cache("not json")[0].returncode, 1)

    @unittest.skipUnless(all(shutil.which(t) for t in ("terraform", "aws", "ssh", "ssh-keyscan", "ssh-keygen", "flock")),
                         "gate tooling not installed")
    def test_gate_wrapper_refuses_before_touching_aws_at_the_stop_line(self):
        cloud = self.tmp / "cloud"
        cloud.mkdir()
        (cloud / "gate.tfvars").write_text('ssh_key_name = "x"\n')
        (cloud / "id").write_text("k\n")
        (cloud / "spend.md").write_text(HEADER + window("90.0000"))
        env = dict(os.environ, RSI_CLOUD_DIR=str(cloud), RSI_GATE_IDENTITY=str(cloud / "id"),
                   RSI_GATE_LANDER="/bin/true", AWS_PROFILE="nonexistent-profile")
        result = subprocess.run(["bash", str(GATE), "--run", "--", "/bin/true"],
                                env=env, capture_output=True, text=True, timeout=60)
        self.assertEqual(result.returncode, 4, result.stderr)
        self.assertIn("refusing to start a remote run", result.stderr)
        self.assertFalse((cloud / "logs").exists(), "no terraform/apply work may start")


if __name__ == "__main__":
    unittest.main()
