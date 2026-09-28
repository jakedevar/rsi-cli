"""The QA importer admits exact, complete shard evidence, including known reds."""

from concurrent.futures import ThreadPoolExecutor
import hashlib
import importlib.util
import json
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch


SCRIPT = Path(__file__).resolve().parents[1] / "import-rolling-qa-cache.py"
SPEC = importlib.util.spec_from_file_location("rolling_qa_cache", SCRIPT)
MODULE = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(MODULE)
SHA = "a" * 40


class RollingQaCacheImportTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.cache = self.root / "cache"

    def fingerprint(self, shard):
        inputs = {"feature": f"test-shard-{shard}", "jobs": 4,
                  "rustc": "test rustc", "target_triple": "x86_64-unknown-linux-gnu",
                  "host_class": "linux-x86_64@desktop:0123456789abcdef", "test_threads": 4}
        canonical = json.dumps(inputs, sort_keys=True, separators=(",", ":")).encode()
        return {"digest": hashlib.sha256(canonical).hexdigest(), "inputs": inputs}

    def report(self, red=False):
        rows = [{"name": shard, "run": 1, "passed": 1, "failed": 0, "skipped": 0,
                 "exit_code": 0, "failing_tests": [], "fingerprint": self.fingerprint(shard)}
                for shard in sorted(MODULE.SHARDS)]
        if red:
            rows[0].update({"passed": 0, "failed": 1, "exit_code": 100,
                            "failing_tests": ["memory::tests::known_red"]})
        lanes = [{"name": lane, "exit_code": 0, "failing_tests": []}
                 for lane in sorted(MODULE.LANES)]
        payload = {"schema_version": 1, "swept_sha": SHA, "swept_tree": "b" * 40,
                   "complete": True,
                   "provenance": {"kind": "qa_sweep", "runner": "laptop", "host": "laptop",
                                  "started_at": "2026-09-27T09:00:00Z",
                                  "finished_at": "2026-09-27T10:00:00Z", "report_ref": "QA.md"},
                   "shards": rows, "lanes": lanes}
        path = self.root / "report.json"
        path.write_text(json.dumps(payload))
        return path, payload

    def matching_local(self):
        return patch.object(MODULE, "local_fingerprint",
                            side_effect=lambda sha, shard, jobs: self.fingerprint(shard))

    def test_json_import_reuses_complete_reds_and_records_provenance(self):
        path, _ = self.report(red=True)
        with self.matching_local(), patch.object(MODULE, "local_tree", return_value="b" * 40):
            tip, first = MODULE.import_report(path, self.cache)
            _, second = MODULE.import_report(path, self.cache)
        self.assertEqual(tip, SHA)
        self.assertEqual({state for _, state in first}, {"imported"})
        self.assertEqual({state for _, state in second}, {"existing"})
        fingerprint = self.fingerprint("store-01")
        entry = json.loads((self.cache / SHA / fingerprint["digest"] / "store-01.json").read_text())
        self.assertEqual(entry["failures"], [])
        self.assertEqual(entry["provenance"], "qa:laptop:QA.md")
        red = json.loads((self.cache / SHA / self.fingerprint("memory-01")["digest"] / "memory-01.json").read_text())
        self.assertEqual(red["failures"], ["memory::tests::known_red"])

    def test_fingerprint_mismatch_refuses_entry(self):
        path, _ = self.report()
        different = self.fingerprint("store-01")
        different["digest"] = "c" * 64
        with patch.object(MODULE, "local_fingerprint", return_value=different), patch.object(MODULE, "local_tree", return_value="b" * 40):
            with self.assertRaisesRegex(ValueError, "fingerprint does not match"):
                MODULE.import_report(path, self.cache)
        self.assertFalse(any(self.cache.rglob("*.json")))

    def test_cross_host_class_refuses_even_with_same_source_and_runner(self):
        path, _ = self.report()
        def cloud_fingerprint(_sha, shard, _jobs):
            local = self.fingerprint(shard)
            local["inputs"]["host_class"] = "linux-x86_64@cloud:fedcba9876543210"
            return local
        with patch.object(MODULE, "local_fingerprint", side_effect=cloud_fingerprint), \
             patch.object(MODULE, "local_tree", return_value="b" * 40):
            with self.assertRaisesRegex(ValueError, "host class does not match"):
                MODULE.import_report(path, self.cache)
        self.assertFalse(any(self.cache.rglob("*.json")))

    def test_exact_key_conflict_is_typed_and_preserves_first_result(self):
        path, payload = self.report()
        with self.matching_local(), patch.object(MODULE, "local_tree", return_value="b" * 40):
            MODULE.import_report(path, self.cache)
            payload["shards"][0].update({"passed": 0, "failed": 1, "exit_code": 100,
                                          "failing_tests": ["memory::tests::red"]})
            path.write_text(json.dumps(payload))
            with self.assertRaises(MODULE.CacheConflict):
                MODULE.import_report(path, self.cache)
        shard = payload["shards"][0]["name"]
        digest = self.fingerprint(shard)["digest"]
        cached = json.loads((self.cache / SHA / digest / f"{shard}.json").read_text())
        self.assertEqual(cached["failures"], [])

    def test_concurrent_writers_leave_complete_matching_entries(self):
        path, _ = self.report()
        with self.matching_local(), patch.object(MODULE, "local_tree", return_value="b" * 40):
            with ThreadPoolExecutor(max_workers=4) as pool:
                results = list(pool.map(lambda _: MODULE.import_report(path, self.cache), range(4)))
        self.assertEqual(len(results), 4)
        for shard in MODULE.SHARDS:
            fingerprint = self.fingerprint(shard)
            entry = json.loads((self.cache / SHA / fingerprint["digest"] / f"{shard}.json").read_text())
            self.assertEqual(entry["rolling_sha"], SHA)
            self.assertEqual(entry["shard"], shard)
            self.assertEqual(entry["fingerprint"], "sha256:" + fingerprint["digest"])

    def test_markdown_machine_block_can_seed_a_qa_entry(self):
        _, payload = self.report()
        path = self.root / "QA.md"
        path.write_text("# QA result\n\n```qa-sweep-v1\n" + json.dumps(payload) + "\n```\n")
        with self.matching_local(), patch.object(MODULE, "local_tree", return_value="b" * 40):
            MODULE.import_report(path, self.cache)
        digest = self.fingerprint("store-01")["digest"]
        entry = json.loads((self.cache / SHA / digest / "store-01.json").read_text())
        self.assertEqual(entry["provenance"], "qa:laptop:QA.md")

    def test_incomplete_report_is_refused(self):
        path, payload = self.report()
        payload["complete"] = False
        path.write_text(json.dumps(payload))
        with self.assertRaisesRegex(ValueError, "complete sweep"):
            MODULE.import_report(path, self.cache)


if __name__ == "__main__":
    unittest.main()
