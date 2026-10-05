"""Intent checks for scripts/check-touched-shards (#1099): one typed receipt per candidate."""

import json
import os
from pathlib import Path
import shutil
import subprocess
import tempfile
import unittest


SCRIPT = Path(__file__).resolve().parents[1] / "check-touched-shards"
GATE = '#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-{shard}"))]\n#[test]\nfn {name}() {{}}\n'


class CheckTouchedShardsTest(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix="touched-shards-test-")
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.git("init", "-q", "-b", "rolling")
        self.git("config", "user.name", "T")
        self.git("config", "user.email", "t@example.invalid")
        self.write("crates/rsid/src/store/migrations/v150.rs", "// v150\n")
        self.write("crates/rsid/src/foo.rs", GATE.format(shard="store-02", name="a"))
        self.write("crates/rsid/src/bar.rs", "pub fn bar() {}\n")
        self.write("crates/rsid/src/bar/tests.rs", GATE.format(shard="other-03", name="b"))
        self.write("crates/rsi-common/src/types.rs", "pub fn t() {}\n#[cfg(test)]\nmod tests { #[test] fn a() {} }\n")
        self.write("crates/rsi-common/src/untested.rs", "pub fn u() {}\n")
        self.write("crates/rsid/src/bin/tool.rs", "fn main() {}\n#[test]\nfn t() {}\n")
        self.write("crates/rsid/tests/it.rs", "#[test]\nfn t() {}\n")
        self.git("add", ".")
        self.git("commit", "-q", "-m", "base")
        self.git("branch", "base")
        self.git("checkout", "-q", "-b", "feature")

    def git(self, *args):
        subprocess.run(["git", *args], cwd=self.root, check=True, capture_output=True)

    def write(self, rel, text):
        path = self.root / rel
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(text)

    def commit(self, msg="change"):
        self.git("add", ".")
        self.git("commit", "-q", "-m", msg)

    def check(self, *extra, cargo="true"):
        env = dict(os.environ, CHECK_TOUCHED_SHARDS_CARGO=cargo)
        result = subprocess.run(
            [str(SCRIPT), "--repo", str(self.root), "--base", "base", *extra],
            capture_output=True, text=True, env=env,
        )
        summary, _, rest = result.stdout.partition("\n")
        return result.returncode, summary, json.loads(rest)

    def test_maps_diff_to_shards_and_emits_one_line_and_receipt(self):
        self.write("crates/rsid/src/foo.rs", GATE.format(shard="store-02", name="a") + "// edit\n")
        self.write("crates/rsid/src/bar.rs", "pub fn bar() { }\n")
        self.write("crates/rsi-common/src/types.rs", "pub fn t() { }\n#[test]\nfn a() {}\n")
        self.commit()
        code, summary, receipt = self.check()
        self.assertEqual(code, 0, summary)
        self.assertTrue(summary.startswith("check-touched-shards OK"), summary)
        self.assertEqual(receipt["shards"]["compiled_ok"], ["other-03", "store-02"])
        self.assertTrue(receipt["merge_clean"])
        self.assertTrue(receipt["ok"])
        self.assertEqual(
            receipt["suggested_filters"],
            ["rsi-common=types", "rsid=shard:other-03:test(bar)", "rsid=shard:store-02:test(foo)"],
        )
        self.assertEqual(receipt["audits"]["diff_check"]["verdict"], "ok")

    def test_filters_only_name_targets_that_hold_a_test(self):
        # An untested rsi-common module, a bin target and an integration test.
        self.write("crates/rsi-common/src/untested.rs", "pub fn u() { }\n")
        self.write("crates/rsid/src/bin/tool.rs", "fn main() { }\n#[test]\nfn t() {}\n")
        self.write("crates/rsid/tests/it.rs", "#[test]\nfn t() { }\n")
        self.commit()
        code, summary, receipt = self.check()
        self.assertEqual(code, 0, summary)
        # bin and integration targets never map to a lib shard or compile one.
        self.assertEqual(receipt["shards"]["compiled_ok"], [])
        self.assertEqual(receipt["suggested_filters"], ["rsid=bin:tool", "rsid=test:it"])

    def test_failed_shard_compile_fails_the_receipt(self):
        self.write("crates/rsid/src/foo.rs", GATE.format(shard="store-02", name="a") + "// edit\n")
        self.commit()
        code, summary, receipt = self.check(cargo="false")
        self.assertEqual(code, 1)
        self.assertEqual(receipt["shards"]["compiled_failed"], ["store-02"])
        self.assertIn("FAIL", summary)

    def test_new_migration_numbering_and_dynamic_sql_are_reported(self):
        self.write("crates/rsid/src/store/migrations/v152.rs", "// skips 151\n")
        self.write("crates/rsid/src/foo.rs", GATE.format(shard="store-02", name="a") + 'fn q(t: &str) -> String { format!("SELECT * FROM {t}") }\n')
        self.commit()
        code, _, receipt = self.check("--no-compile")
        self.assertEqual(receipt["migrations"], {"new": [152], "base_head": 150, "numbering_conflict": True, "stray_pre_split": []})
        self.assertEqual(receipt["audits"]["dynamic_sql"]["verdict"], "flag")
        self.assertEqual(code, 1)

    def test_conflicting_candidate_is_not_merge_clean_and_pins_follow_touched_files(self):
        self.git("checkout", "-q", "base")
        self.write("crates/rsi/src/key_tables.rs", "// base\n")
        self.commit("base2")
        self.git("checkout", "-q", "feature")
        self.write("crates/rsi/src/key_tables.rs", "// feature\n")
        self.commit()
        code, _, receipt = self.check("--no-compile")
        self.assertFalse(receipt["merge_clean"])
        self.assertIn("rsi=manual::", receipt["suggested_filters"])
        self.assertEqual(code, 1)


CHECKER = Path(__file__).resolve().parents[1] / "check-rsid-test-shards.py"
RSID_MANIFEST = (
    '[features]\ndefault = []\ntest-shard-mode = []\n'
    'test-shard-store-02 = ["test-shard-mode"]\ntest-shard-other-03 = ["test-shard-mode"]\n'
)
STORE_MANIFEST = (
    '[features]\ndefault = []\ntest-shard-mode = []\n'
    'test-shard-store-01 = ["test-shard-mode"]\ntest-shard-store-02 = ["test-shard-mode"]\n'
)


class SplitStoreReceiptTest(unittest.TestCase):
    """#1021 S4: rsid-store changes are mapped, compiled and reported like rsid's."""

    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix="touched-shards-split-")
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name) / "repo"
        self.root.mkdir()
        self.log = Path(self.temp.name) / "cargo.log"
        self.cargo = Path(self.temp.name) / "cargo.sh"
        self.cargo.write_text('#!/bin/sh\necho "$@" >> "$CARGO_LOG"\n')
        self.cargo.chmod(0o755)
        self.git("init", "-q", "-b", "rolling")
        self.git("config", "user.name", "T")
        self.git("config", "user.email", "t@example.invalid")
        (self.root / "scripts").mkdir()
        shutil.copy(CHECKER, self.root / "scripts/check-rsid-test-shards.py")
        self.write("crates/rsid/Cargo.toml", RSID_MANIFEST)
        self.write("crates/rsid-store/Cargo.toml", STORE_MANIFEST)
        self.write("crates/rsid/src/foo.rs", GATE.format(shard="store-02", name="a"))
        self.write("crates/rsid/src/bar.rs", "pub fn bar() {}\n")
        self.write("crates/rsid/src/bar/tests.rs", GATE.format(shard="other-03", name="b"))
        self.write("crates/rsid-store/src/store/q.rs", GATE.format(shard="store-01", name="c"))
        self.write("crates/rsid-store/src/store/r.rs", GATE.format(shard="store-02", name="d"))
        self.write("crates/rsid-store/src/store/migrations/v150.rs", "// v150\n")
        self.git("add", ".")
        self.git("commit", "-q", "-m", "base")
        self.git("branch", "base")
        self.git("checkout", "-q", "-b", "feature")

    def git(self, *args):
        subprocess.run(["git", *args], cwd=self.root, check=True, capture_output=True)

    def write(self, rel, text):
        path = self.root / rel
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(text)

    def commit(self):
        self.git("add", ".")
        self.git("commit", "-q", "-m", "change")

    def check(self):
        env = dict(os.environ, CHECK_TOUCHED_SHARDS_CARGO=str(self.cargo), CARGO_LOG=str(self.log))
        result = subprocess.run(
            [str(SCRIPT), "--repo", str(self.root), "--base", "base"],
            capture_output=True, text=True, env=env,
        )
        summary, _, rest = result.stdout.partition("\n")
        commands = self.log.read_text().splitlines() if self.log.exists() else []
        return result.returncode, summary, json.loads(rest), commands

    def test_an_rsid_store_source_change_compiles_its_shard_and_one_dependent_shard(self):
        self.write("crates/rsid-store/src/store/q.rs", GATE.format(shard="store-01", name="c") + "// edit\n")
        self.commit()
        code, summary, receipt, commands = self.check()
        self.assertEqual(code, 0, summary)
        # store-01 holds only rsid-store tests; other-03 is the rsid-only shard
        # that proves the daemon still compiles against the changed store.
        self.assertEqual(receipt["shards"]["compiled_ok"], ["other-03", "store-01"])
        self.assertIn(
            "check -p rsid-store --lib --tests --no-default-features --features test-shard-store-01",
            commands,
        )
        self.assertIn(
            "check -p rsid --lib --tests --no-default-features --features test-shard-other-03",
            commands,
        )
        self.assertEqual(receipt["suggested_filters"], ["rsid=shard:store-01:test(store::q)"])

    def test_a_shard_shared_by_both_packages_compiles_both(self):
        self.write("crates/rsid-store/src/store/r.rs", GATE.format(shard="store-02", name="d") + "// edit\n")
        self.commit()
        code, summary, receipt, commands = self.check()
        self.assertEqual(code, 0, summary)
        self.assertIn(
            "check -p rsid -p rsid-store --lib --tests --no-default-features --features test-shard-store-02",
            commands,
        )
        self.assertEqual(receipt["suggested_filters"], ["rsid=shard:store-02:test(store::r)"])

    def test_a_migration_only_store_change_compiles_a_fallback_shard_and_reports_the_number(self):
        self.write("crates/rsid-store/src/store/migrations/v151.rs", "// v151\n")
        self.commit()
        code, summary, receipt, commands = self.check()
        self.assertEqual(code, 0, summary)
        self.assertEqual(receipt["migrations"]["new"], [151])
        self.assertEqual(receipt["migrations"]["base_head"], 150)
        self.assertFalse(receipt["migrations"]["numbering_conflict"])
        self.assertEqual(receipt["shards"]["fallback"], "store-01")
        self.assertIn("check -p rsid-store --lib --tests --no-default-features --features test-shard-store-01", commands)
        self.assertIn("migrations=V151", summary)

    def test_a_migration_unit_added_to_the_pre_split_directory_fails_the_receipt(self):
        self.write("crates/rsid/src/store/migrations/v151.rs", "// v151 in the dead directory\n")
        self.commit()
        code, summary, receipt, _ = self.check()
        self.assertEqual(code, 1)
        self.assertEqual(receipt["migrations"]["stray_pre_split"], ["crates/rsid/src/store/migrations/v151.rs"])
        self.assertFalse(receipt["ok"])
        self.assertIn("STRAY_PRE_SPLIT_MIGRATION", summary)

    def test_a_pre_split_base_still_reports_its_head_from_the_old_directory(self):
        self.git("checkout", "-q", "base")
        self.git("rm", "-q", "-r", "crates/rsid-store/src/store/migrations")
        self.write("crates/rsid/src/store/migrations/v150.rs", "// v150\n")
        self.git("add", ".")
        self.git("commit", "-q", "-m", "pre-split base")
        self.git("checkout", "-q", "-b", "after-split")
        self.git("rm", "-q", "-r", "crates/rsid/src/store/migrations")
        self.write("crates/rsid-store/src/store/migrations/v150.rs", "// v150\n")
        self.write("crates/rsid-store/src/store/migrations/v151.rs", "// v151\n")
        self.commit()
        code, summary, receipt, _ = self.check()
        self.assertEqual(code, 0, summary)
        self.assertEqual(receipt["migrations"]["base_head"], 150)
        # The moved V150 unit is a rename, not a new migration.
        self.assertEqual(receipt["migrations"]["new"], [151])
        self.assertFalse(receipt["migrations"]["numbering_conflict"])
        self.assertEqual(receipt["migrations"]["stray_pre_split"], [])


if __name__ == "__main__":
    unittest.main()
