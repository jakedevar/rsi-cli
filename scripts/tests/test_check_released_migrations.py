"""Hermetic refresh behavior tests for the released migration guard."""

import importlib.util
import json
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest

sys.dont_write_bytecode = True
ROOT = Path(__file__).resolve().parents[2]
SPEC = importlib.util.spec_from_file_location(
    "check_released_migrations", ROOT / "tools/check-released-migrations.py"
)
GUARD = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(GUARD)


class ReleasedMigrationRefreshTest(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix="rsi-released-migration-test-")
        self.addCleanup(self.temp.cleanup)
        self.repo = Path(self.temp.name) / "repo"
        self.repo.mkdir()
        self.git("init", "-q", "-b", "rolling")
        self.git("config", "user.name", "Migration Test")
        self.git("config", "user.email", "migration@example.invalid")
        self.write(GUARD.MIGRATION_PATH, self.store())
        self.write("crates/rsid/src/store/cohort_settlement.rs", "")
        self.write("crates/rsid/src/store/alpha.rs", self.section("alpha-catalog"))
        self.write("crates/rsid/src/store/beta.rs", self.section("beta-catalog"))
        manifest = GUARD.inventory({
            GUARD.MIGRATION_PATH: self.store(),
            "crates/rsid/src/store/cohort_settlement.rs": "",
            "crates/rsid/src/store/alpha.rs": self.section("alpha-catalog"),
            "crates/rsid/src/store/beta.rs": self.section("beta-catalog"),
        })
        self.write(GUARD.MANIFEST_PATH, json.dumps(manifest, indent=2) + "\n")
        self.git("add", ".")
        self.git("commit", "-q", "-m", "base V1")
        self.manifest = manifest

    @staticmethod
    def store():
        return (
            "pub const LATEST_SCHEMA_VERSION: i32 = 1;\n"
            "// V0: Original schema\n"
            "create_table();\n"
            "// V1: Session metadata columns\n"
            "if version < 1 {\n    migrate_v1();\n}\n"
        )

    @staticmethod
    def section(name):
        return (
            f"// RSI-RELEASED-MIGRATION-BEGIN: {name}\n"
            f"pub fn apply_{name.replace('-', '_')}() {{}}\n"
            f"// RSI-RELEASED-MIGRATION-END: {name}\n"
        )

    def git(self, *args):
        result = subprocess.run(
            ["git", "-C", str(self.repo), *args], capture_output=True, text=True, check=False
        )
        if result.returncode:
            raise AssertionError(f"git {' '.join(args)}: {result.stderr}")
        return result.stdout.strip()

    def write(self, name, value):
        target = self.repo / name
        target.parent.mkdir(parents=True, exist_ok=True)
        target.write_text(value)

    def refresh(self, *args):
        return subprocess.run(
            [sys.executable, str(ROOT / "tools/check-released-migrations.py"), "--refresh", *args],
            cwd=self.repo,
            capture_output=True,
            text=True,
            check=False,
        )

    def read_manifest(self):
        return (self.repo / GUARD.MANIFEST_PATH).read_text()

    def test_unchanged_refresh_preserves_manifest_bytes(self):
        before = self.read_manifest()
        invocation = self.refresh()
        self.assertEqual(invocation.returncode, 0, invocation.stderr + invocation.stdout)
        self.assertEqual(self.read_manifest(), before)

    def test_refresh_appends_new_section_and_migration_block(self):
        before = self.read_manifest()
        old = json.loads(before)
        helper = "crates/rsid/src/store/gamma.rs"
        store = self.store().replace("LATEST_SCHEMA_VERSION: i32 = 1", "LATEST_SCHEMA_VERSION: i32 = 2")
        store += "if version < 2 {\n    migrate_v2();\n}\n"
        self.write(GUARD.MIGRATION_PATH, store)
        self.write(helper, self.section("gamma-catalog"))
        invocation = self.refresh("--include-path", helper)
        self.assertEqual(invocation.returncode, 0, invocation.stderr + invocation.stdout)
        updated = json.loads(self.read_manifest())
        self.assertEqual(updated["latest_schema_version"], 2)
        self.assertEqual(list(updated["blocks"]), ["0", "1", "2"])
        self.assertIn("gamma-catalog", updated["protected_sections"])
        for name, value in old["blocks"].items():
            self.assertEqual(updated["blocks"][name], value)
        for name, value in old["protected_sections"].items():
            self.assertEqual(updated["protected_sections"][name], value)
        self.assertEqual(list(updated), list(old))

    def test_refresh_discovers_a_tracked_marked_rust_file_without_include_path(self):
        store = self.store().replace("LATEST_SCHEMA_VERSION: i32 = 1", "LATEST_SCHEMA_VERSION: i32 = 2")
        store += "if version < 2 {\n    migrate_v2();\n}\n"
        self.write(GUARD.MIGRATION_PATH, store)
        self.write("crates/rsid/src/store/delta.rs", self.section("delta-catalog"))
        self.git("add", "crates/rsid/src/store/delta.rs")
        invocation = self.refresh()
        self.assertEqual(invocation.returncode, 0, invocation.stderr + invocation.stdout)
        updated = json.loads(self.read_manifest())
        self.assertEqual(
            updated["protected_sections"]["delta-catalog"]["path"],
            "crates/rsid/src/store/delta.rs",
        )

    def test_markers_quoted_outside_rust_sources_do_not_affect_refresh(self):
        # Plans and fixtures quote real sections; only crates/**/*.rs pins count.
        before = self.read_manifest()
        self.write("thoughts/shared/plans/quote.md", self.section("alpha-catalog"))
        self.write("scripts/tests/fixture.py", self.section("zeta-catalog"))
        self.git("add", "thoughts/shared/plans/quote.md", "scripts/tests/fixture.py")
        invocation = self.refresh()
        self.assertEqual(invocation.returncode, 0, invocation.stderr + invocation.stdout)
        self.assertEqual(self.read_manifest(), before)

    def test_removed_pinned_section_refuses_and_preserves_file(self):
        before = self.read_manifest()
        self.write("crates/rsid/src/store/beta.rs", "")
        invocation = self.refresh()
        self.assertEqual(invocation.returncode, 1)
        self.assertIn("removed protected sections beta-catalog", invocation.stderr)
        self.assertEqual(self.read_manifest(), before)


class SplitLayoutTest(unittest.TestCase):
    """One file per version under the migrations directory."""

    @staticmethod
    def step(version, body="migrate();"):
        head = "// V0: Original schema\n        create_table();\n" if version == 0 else ""
        block = (
            f"        if version < {version} {{\n            {body}\n        }}\n"
            if version
            else ""
        )
        return (
            "impl Store {\n"
            f"    fn migrate_v{version:03d}(&self, version: i32) -> Result<()> {{\n"
            f"{head}{block}\n        Ok(())\n    }}\n}}\n"
        )

    def files(self, latest):
        return {
            f"{GUARD.MIGRATION_DIR}/v{version:03d}.rs": self.step(version)
            for version in range(latest + 1)
        }

    def test_head_is_the_highest_migration_file_and_blocks_are_per_file(self):
        actual = GUARD.inventory(self.files(2))
        self.assertEqual(actual["latest_schema_version"], 2)
        self.assertEqual(list(actual["blocks"]), ["0", "1", "2"])
        self.assertEqual(actual["migration_dir"], GUARD.MIGRATION_DIR)

    def test_v0_region_stops_at_the_step_functions_closing_ok(self):
        files = self.files(1)
        region = GUARD.migration_blocks(files)["0"]
        self.assertTrue(region.startswith("// V0: Original schema\n"))
        self.assertNotIn("Ok(())", region)

    def test_a_block_in_the_wrong_file_is_rejected(self):
        files = self.files(2)
        files[f"{GUARD.MIGRATION_DIR}/v001.rs"] = self.step(2)
        with self.assertRaises(GUARD.GuardError):
            GUARD.inventory(files)

    def test_a_misnamed_migration_file_is_rejected(self):
        files = self.files(1)
        files[f"{GUARD.MIGRATION_DIR}/v2.rs"] = self.step(2)
        with self.assertRaises(GUARD.GuardError):
            GUARD.inventory(files)

    def test_moving_a_pinned_section_between_files_keeps_its_digest(self):
        section = ReleasedMigrationRefreshTest.section("alpha-catalog")
        before = GUARD.inventory({**self.files(1), "crates/rsid/src/store/a.rs": section})
        after = GUARD.inventory({**self.files(1), "crates/rsid/src/store/b.rs": section})
        GUARD.validate_append_only(before, after)
        self.assertEqual(
            after["protected_sections"]["alpha-catalog"]["sha256"],
            before["protected_sections"]["alpha-catalog"]["sha256"],
        )

    def test_a_new_version_needs_one_new_file_and_nothing_else(self):
        base = GUARD.inventory(self.files(1))
        head = GUARD.inventory(self.files(2))
        GUARD.validate_append_only(base, head)
        skipped = self.files(3)
        del skipped[f"{GUARD.MIGRATION_DIR}/v002.rs"]
        with self.assertRaises(GUARD.GuardError):
            GUARD.validate_append_only(base, GUARD.inventory(skipped))

    def test_inline_and_split_layouts_share_block_digests(self):
        inline = "// V0: Original schema\n        create_table();\n\n// V1: Session metadata columns\n" + (
            "        if version < 1 {\n            migrate();\n        }\n"
        )
        legacy = GUARD.inventory({GUARD.MIGRATION_PATH: "pub const LATEST_SCHEMA_VERSION: i32 = 1;\n" + inline})
        split = GUARD.inventory(self.files(1))
        self.assertEqual(legacy["blocks"]["1"], split["blocks"]["1"])
        GUARD.validate_append_only(legacy, split)


if __name__ == "__main__":
    unittest.main()
