"""Hermetic refresh behavior tests for the released migration guard."""

import importlib.util
import json
import re
from pathlib import Path
import shutil
import subprocess
import sys
import unittest

sys.dont_write_bytecode = True
ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
from scratch_git import scratch_git, scratch_tempdir  # noqa: E402

SPEC = importlib.util.spec_from_file_location(
    "check_released_migrations", ROOT / "tools/check-released-migrations.py"
)
GUARD = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(GUARD)


class ReleasedMigrationRefreshTest(unittest.TestCase):
    def setUp(self):
        self.temp = scratch_tempdir("rsi-released-migration-test-")
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
        self.git("update-ref", "refs/remotes/origin/rolling", "HEAD")
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
        result = scratch_git(self.repo, *args, check=False)
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

    def append_unreleased(self):
        helper = f"{GUARD.MIGRATION_DIR}/v002.rs"
        self.write(
            helper, "if version < 2 {\n    migrate_v2();\n}\n" + self.section("v002-catalog")
        )
        invocation = self.refresh()
        self.assertEqual(invocation.returncode, 0, invocation.stderr)
        self.git("add", ".")
        self.git("commit", "-q", "-m", "unreleased V2")
        return helper

    def test_default_refresh_repins_edited_unreleased_block_and_section(self):
        helper = self.append_unreleased()
        before = json.loads(self.read_manifest())
        edited = (self.repo / helper).read_text().replace("migrate_v2", "changed_v2")
        self.write(helper, edited.replace("apply_v002", "changed_v002"))
        invocation = self.refresh()
        self.assertEqual(invocation.returncode, 0, invocation.stderr)
        after = json.loads(self.read_manifest())
        self.assertNotEqual(after["blocks"]["2"], before["blocks"]["2"])
        self.assertNotEqual(
            after["protected_sections"]["v002-catalog"],
            before["protected_sections"]["v002-catalog"],
        )
        for version, pin in self.manifest["blocks"].items():
            self.assertEqual(after["blocks"][version], pin)
        for name, section in self.manifest["protected_sections"].items():
            self.assertEqual(after["protected_sections"][name], section)

    def test_explicit_base_can_repin_an_unreleased_version(self):
        helper = self.append_unreleased()
        before = json.loads(self.read_manifest())
        self.write(helper, (self.repo / helper).read_text().replace("migrate_v2", "changed_v2"))
        invocation = self.refresh("HEAD^")
        self.assertEqual(invocation.returncode, 0, invocation.stderr)
        self.assertNotEqual(json.loads(self.read_manifest())["blocks"]["2"], before["blocks"]["2"])

    def test_explicit_base_keeps_a_published_version_immutable(self):
        helper = self.append_unreleased()
        self.git("update-ref", "refs/heads/published", "HEAD")
        before = self.read_manifest()
        self.write(helper, (self.repo / helper).read_text().replace("migrate_v2", "changed_v2"))
        invocation = self.refresh("published")
        self.assertEqual(invocation.returncode, 1)
        self.assertIn("changed migration blocks V2", invocation.stderr)
        self.assertEqual(self.read_manifest(), before)

    def test_refresh_refuses_changed_released_ddl_even_after_branch_pin_refresh(self):
        before = self.read_manifest()
        changed = self.store().replace("migrate_v1", "changed_v1")
        self.write(GUARD.MIGRATION_PATH, changed)
        # A branch's edited pin must not redefine what the base released.
        manifest = json.loads(before)
        manifest["blocks"]["1"] = GUARD.inventory({GUARD.MIGRATION_PATH: changed})["blocks"]["1"]
        self.write(GUARD.MANIFEST_PATH, json.dumps(manifest, indent=2) + "\n")
        edited = self.read_manifest()
        invocation = self.refresh()
        self.assertEqual(invocation.returncode, 1)
        self.assertIn("changed migration blocks V1", invocation.stderr)
        self.assertEqual(self.read_manifest(), edited)

    def test_refresh_refuses_changed_released_helper(self):
        before = self.read_manifest()
        self.write(
            "crates/rsid/src/store/alpha.rs",
            self.section("alpha-catalog").replace("apply_alpha", "changed_alpha"),
        )
        invocation = self.refresh()
        self.assertEqual(invocation.returncode, 1)
        self.assertIn("changed protected sections alpha-catalog", invocation.stderr)
        self.assertEqual(self.read_manifest(), before)

    def test_missing_base_refuses_before_writing(self):
        before = self.read_manifest()
        invocation = self.refresh("missing-base")
        self.assertEqual(invocation.returncode, 1)
        self.assertIn("cannot be resolved", invocation.stderr)
        self.assertEqual(self.read_manifest(), before)

    def test_deleted_worktree_manifest_does_not_allow_released_ddl_changes(self):
        (self.repo / GUARD.MANIFEST_PATH).unlink()
        self.write(GUARD.MIGRATION_PATH, self.store().replace("migrate_v1", "changed_v1"))
        invocation = self.refresh()
        self.assertEqual(invocation.returncode, 1)
        self.assertIn("changed migration blocks V1", invocation.stderr)
        self.assertFalse((self.repo / GUARD.MANIFEST_PATH).exists())

    def test_deleted_manifest_is_rebuilt_against_the_released_base(self):
        before = self.read_manifest()
        (self.repo / GUARD.MANIFEST_PATH).unlink()
        invocation = self.refresh()
        self.assertEqual(invocation.returncode, 0, invocation.stderr)
        self.assertEqual(self.read_manifest(), before)

    def test_invalid_base_pins_refuse_before_writing(self):
        before = self.read_manifest()
        self.write(GUARD.MIGRATION_PATH, self.store().replace("migrate_v1", "changed_v1"))
        self.git("add", GUARD.MIGRATION_PATH)
        self.git("commit", "-q", "-m", "base source without corresponding pins")
        invocation = self.refresh("HEAD")
        self.assertEqual(invocation.returncode, 1)
        self.assertIn("refresh base migration manifest does not match its source", invocation.stderr)
        self.assertEqual(self.read_manifest(), before)

    def test_refresh_refuses_noncontiguous_unreleased_versions(self):
        before = self.read_manifest()
        self.write(f"{GUARD.MIGRATION_DIR}/v003.rs", "if version < 3 {\n    migrate_v3();\n}\n")
        invocation = self.refresh()
        self.assertEqual(invocation.returncode, 1)
        self.assertIn("every new version", invocation.stderr)
        self.assertEqual(self.read_manifest(), before)

    def test_refresh_resolves_a_conflicted_manifest_from_both_sides(self):
        # Two landings that each append a pin at the same spot conflict on the
        # derived manifest; refresh keeps every pin of both sides, no hand edit.
        self.git("checkout", "-q", "-b", "side-gamma")
        for name in ("gamma", "delta"):
            if name == "delta":
                self.git("checkout", "-q", "rolling")
                self.git("checkout", "-q", "-b", "side-delta")
            path = f"crates/rsid/src/store/{name}.rs"
            self.write(path, self.section(f"{name}-catalog"))
            self.assertEqual(self.refresh("--include-path", path).returncode, 0)
            self.git("add", ".")
            self.git("commit", "-q", "-m", f"add {name}")
        merge = scratch_git(self.repo, "merge", "side-gamma", check=False)
        self.assertNotEqual(merge.returncode, 0, "the fixture must conflict on the manifest")
        self.assertIn("<<<<<<<", self.read_manifest())
        invocation = self.refresh()
        self.assertEqual(invocation.returncode, 0, invocation.stderr + invocation.stdout)
        sections = json.loads(self.read_manifest())["protected_sections"]
        self.assertEqual(
            sorted(sections), ["alpha-catalog", "beta-catalog", "delta-catalog", "gamma-catalog"]
        )

    def check(self, base="origin/rolling", head="HEAD"):
        return subprocess.run(
            [sys.executable, str(ROOT / "tools/check-released-migrations.py"), base, head],
            cwd=self.repo, capture_output=True, text=True, check=False,
        )

    def commit_v2_without_a_pin(self):
        store = self.store().replace("LATEST_SCHEMA_VERSION: i32 = 1", "LATEST_SCHEMA_VERSION: i32 = 2")
        self.write(GUARD.MIGRATION_PATH, store + "if version < 2 {\n    migrate_v2();\n}\n")
        self.git("add", ".")
        self.git("commit", "-q", "-m", "migration 2, no pin edit")

    def test_the_guard_defers_the_pin_of_a_source_that_left_the_pin_file_alone(self):
        # #1949: the lander derives the pin at publication, so a worker's
        # source carries the new migration and the base's pin file unchanged.
        self.commit_v2_without_a_pin()
        self.assertEqual(json.loads(self.read_manifest()), self.manifest)
        checked = self.check()
        self.assertEqual(checked.returncode, 0, checked.stderr + checked.stdout)
        self.assertIn("pin deferred to the lander", checked.stdout)

    def commit_provisional(self, version, declared=True):
        path = f"{GUARD.MIGRATION_DIR}/v{version:03d}.rs"
        source = SplitLayoutTest.step(version)
        self.write(path, source)
        if declared:
            self.write(f"{GUARD.DECLARATIONS_DIR}/issue-531-v{version}.json", json.dumps({
                "schema_version": 1,
                "version": version,
                "files": [{
                    "path": path,
                    "path_template": f"{GUARD.MIGRATION_DIR}/v${{VERSION}}.rs",
                    "source_blob": GUARD.digest(source),
                    "sites": [{
                        "anchor": f"if version < {version} {{",
                        "replacement": "if version < ${VERSION} {",
                        "scope": "unit",
                    }],
                }],
            }))
        self.git("add", ".")
        self.git("commit", "-q", "-m", f"provisional V{version}, no pin edit")
        return path

    def test_stacked_base_with_declared_provisional_block_passes(self):
        before = self.read_manifest()
        self.commit_provisional(2)
        base = self.git("rev-parse", "HEAD")
        self.write("README.md", "stacked worker changes no migrations\n")
        self.git("add", "README.md")
        self.git("commit", "-q", "-m", "stacked worker")
        checked = self.check(base)
        self.assertEqual(checked.returncode, 0, checked.stderr + checked.stdout)
        self.assertIn("V0-V2 immutable, head V2", checked.stdout)
        self.assertEqual(self.read_manifest(), before)

    def test_undeclared_extra_base_block_still_fails(self):
        self.commit_provisional(2)
        self.commit_provisional(3, declared=False)
        checked = self.check("HEAD", "HEAD")
        self.assertEqual(checked.returncode, 1)
        self.assertIn("base migration manifest does not match its source", checked.stderr)

    def test_391e970d6_zero_diff_reproduction_as_fixture(self):
        # #531: 391e970d6 has a released V188 pin and #519's declared,
        # unpublished V189. Recreate that inventory without a live git ref.
        files = SplitLayoutTest().files(188)
        self.write(GUARD.MIGRATION_PATH, "")
        for path, source in files.items():
            self.write(path, source)
        self.write(GUARD.MANIFEST_PATH, json.dumps(GUARD.inventory(files)))
        self.git("add", ".")
        self.git("commit", "-q", "-m", "published V188 fixture")
        before = self.read_manifest()
        self.commit_provisional(189)
        checked = self.check("HEAD", "HEAD")
        self.assertEqual(checked.returncode, 0, checked.stderr + checked.stdout)
        self.assertIn("V0-V189 immutable, head V189", checked.stdout)
        self.assertEqual(self.read_manifest(), before)

    def test_base_declaration_does_not_allow_changed_released_history(self):
        self.commit_provisional(2)
        for path, old, new in (
            (GUARD.MIGRATION_PATH, "migrate_v1", "changed_v1"),
            ("crates/rsid/src/store/alpha.rs", "apply_alpha", "changed_alpha"),
        ):
            with self.subTest(path=path):
                original = (self.repo / path).read_text()
                self.write(path, original.replace(old, new))
                self.git("add", path)
                self.git("commit", "-q", "-m", "change released history")
                base = self.git("rev-parse", "HEAD")
                self.write(path, original)
                self.git("add", path)
                self.git("commit", "-q", "-m", "restore released history in head")
                checked = self.check(base)
                self.assertEqual(checked.returncode, 1)
                self.assertIn("changed " + ("migration blocks V1" if old == "migrate_v1"
                                           else "protected sections alpha-catalog"), checked.stderr)

    def test_declaration_added_only_in_head_cannot_cover_base(self):
        self.commit_provisional(2, declared=False)
        base = self.git("rev-parse", "HEAD")
        self.commit_provisional(2)
        checked = self.check(base)
        self.assertEqual(checked.returncode, 1)
        self.assertIn("base migration manifest does not match its source", checked.stderr)

    def test_base_provisional_declaration_shape_is_validated(self):
        self.commit_provisional(2)
        path = f"{GUARD.DECLARATIONS_DIR}/issue-531-v2.json"
        declaration = json.loads((self.repo / path).read_text())
        del declaration["files"][0]["path_template"]
        self.write(path, json.dumps(declaration))
        self.git("add", path)
        self.git("commit", "-q", "-m", "malformed provisional declaration")
        checked = self.check("HEAD", "HEAD")
        self.assertEqual(checked.returncode, 1)
        self.assertIn("missing key `path_template`", checked.stderr)

    def test_provisional_base_block_is_immutable_in_the_stacked_head(self):
        path = self.commit_provisional(2)
        base = self.git("rev-parse", "HEAD")
        self.write(path, (self.repo / path).read_text().replace("migrate();", "changed();"))
        self.git("add", path)
        self.git("commit", "-q", "-m", "change provisional base block")
        checked = self.check(base)
        self.assertEqual(checked.returncode, 1)
        self.assertIn("released migration block V2 changed relative to base", checked.stderr)

    def test_a_bare_check_after_merging_rolling_names_the_real_base(self):
        # #151: a source with an unpinned migration merges origin/rolling (no
        # rebase); the bare guard's default base HEAD^ is then its own commit.
        self.git("checkout", "-q", "-b", "source")
        self.commit_v2_without_a_pin()
        self.git("checkout", "-q", "rolling")
        self.write("README.md", "rolling advanced\n")
        self.git("add", "README.md")
        self.git("commit", "-q", "-m", "rolling advances")
        self.git("update-ref", "refs/remotes/origin/rolling", "HEAD")
        self.git("checkout", "-q", "source")
        self.git("merge", "-q", "--no-edit", "--no-ff", "origin/rolling")
        bare = subprocess.run(
            [sys.executable, str(ROOT / "tools/check-released-migrations.py")],
            cwd=self.repo, capture_output=True, text=True, check=False,
        )
        self.assertEqual(bare.returncode, 1, bare.stderr + bare.stdout)
        self.assertIn("Rerun against the real base: "
                          "`python3 tools/check-released-migrations.py origin/rolling HEAD`", bare.stderr)
        checked = self.check()
        self.assertEqual(checked.returncode, 0, checked.stderr + checked.stdout)
        self.assertIn("pin deferred to the lander", checked.stdout)

    def test_the_derived_pin_passes_the_strict_guard(self):
        # What the lander commits (`--refresh` against the tip) is exactly the
        # pin the guard defers: the committed result validates without deferral.
        self.commit_v2_without_a_pin()
        self.assertEqual(self.refresh("origin/rolling").returncode, 0)
        self.git("add", GUARD.MANIFEST_PATH)
        self.git("commit", "-q", "-m", "derive pin")
        checked = self.check()
        self.assertEqual(checked.returncode, 0, checked.stderr + checked.stdout)
        self.assertNotIn("deferred", checked.stdout)
        self.assertEqual(json.loads(self.read_manifest())["blocks"]["2"],
                         GUARD.inventory({GUARD.MIGRATION_PATH: (self.repo / GUARD.MIGRATION_PATH).read_text(),
                                          "crates/rsid/src/store/cohort_settlement.rs": "",
                                          "crates/rsid/src/store/alpha.rs": self.section("alpha-catalog"),
                                          "crates/rsid/src/store/beta.rs": self.section("beta-catalog")})["blocks"]["2"])

    def test_a_deferred_pin_cannot_cover_a_changed_released_block(self):
        self.write(GUARD.MIGRATION_PATH, self.store().replace("migrate_v1", "migrate_v1_edited"))
        self.git("add", ".")
        self.git("commit", "-q", "-m", "edit a released block")
        refused = self.check()
        self.assertEqual(refused.returncode, 1)
        self.assertIn("does not match its source", refused.stderr)

    def test_a_hand_edited_partial_pin_is_still_judged_strictly(self):
        # Only a pin file left exactly at the base's content is deferred.
        self.commit_v2_without_a_pin()
        manifest = json.loads(self.read_manifest())
        manifest["latest_schema_version"] = 2
        self.write(GUARD.MANIFEST_PATH, json.dumps(manifest, indent=2) + "\n")
        self.git("add", ".")
        self.git("commit", "-q", "-m", "half a pin")
        refused = self.check()
        self.assertEqual(refused.returncode, 1)
        self.assertIn("does not match its source", refused.stderr)

    def test_a_store_moved_to_the_new_layout_still_validates_against_the_old_base(self):
        # #1021 S4: the store moved from crates/rsid to crates/rsid-store. The
        # base revision keeps the pre-split layout; the head is the new one.
        # Re-create the base in the old layout, then move the whole store tree.
        old_root = "crates/rsid/src/store"
        new_root = GUARD.STORE_ROOTS[0]
        self.git("rm", "-q", "-r", "--cached", ".")
        shutil.rmtree(self.repo / "crates")
        files = {
            f"{old_root}/mod.rs": self.store(),
            f"{old_root}/cohort_settlement.rs": "",
            f"{old_root}/alpha.rs": self.section("alpha-catalog"),
            f"{old_root}/beta.rs": self.section("beta-catalog"),
        }
        for name, value in files.items():
            self.write(name, value)
        legacy = GUARD.inventory(files)
        self.assertEqual(legacy["migration_file"], f"{old_root}/mod.rs")
        self.write(GUARD.MANIFEST_PATH, json.dumps(legacy, indent=2) + "\n")
        self.git("add", ".")
        self.git("commit", "-q", "-m", "pre-split layout")
        self.git("mv", "crates/rsid", "crates/rsid-store")
        moved = {name.replace("crates/rsid/", "crates/rsid-store/"): value for name, value in files.items()}
        self.write(GUARD.MANIFEST_PATH, json.dumps(GUARD.inventory(moved), indent=2) + "\n")
        self.git("add", ".")
        self.git("commit", "-q", "-m", "move the store")
        run = lambda: subprocess.run(
            [sys.executable, str(ROOT / "tools/check-released-migrations.py"), "HEAD^", "HEAD"],
            cwd=self.repo, capture_output=True, text=True, check=False,
        )
        passed = run()
        self.assertEqual(passed.returncode, 0, passed.stderr + passed.stdout)
        self.write(f"{new_root}/alpha.rs", self.section("alpha-catalog").replace("fn apply", "pub fn changed_apply"))
        self.write(GUARD.MANIFEST_PATH, json.dumps(GUARD.inventory({**moved, f"{new_root}/alpha.rs": (self.repo / f"{new_root}/alpha.rs").read_text()}), indent=2) + "\n")
        self.git("add", ".")
        self.git("commit", "-q", "-m", "edit a released section")
        refused = run()
        self.assertNotEqual(refused.returncode, 0)
        self.assertIn("alpha-catalog", refused.stderr)

    def test_a_revision_with_migrations_in_both_store_layouts_is_refused(self):
        # build.rs consumes only the new directory: an old-path unit (with a
        # refreshed manifest) would be a contiguous append that never runs.
        old_unit = "crates/rsid/src/store/migrations/v003.rs"
        new_unit = f"{GUARD.MIGRATION_DIR}/v002.rs"
        self.write(new_unit, "if version < 2 {\n    migrate_v2();\n}\n")
        self.write(old_unit, "if version < 3 {\n    migrate_v3();\n}\n")
        files = {
            GUARD.MIGRATION_PATH: self.store(),
            new_unit: (self.repo / new_unit).read_text(),
            old_unit: (self.repo / old_unit).read_text(),
        }
        with self.assertRaisesRegex(GUARD.GuardError, "more than one store layout"):
            GUARD.inventory(files)
        # The refresh path reads the worktree the same way and refuses too.
        refused = self.refresh()
        self.assertNotEqual(refused.returncode, 0)
        self.assertIn("more than one store layout", refused.stderr)

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

    def test_the_rewind_constant_must_equal_the_schema_head(self):
        """#2018: a new migration without the rewind fixture bump is refused."""
        tests = "const REWIND_TEARDOWN_COVERED_THROUGH: i32 = {};\n"
        GUARD.validate_rewind_teardown_matches_schema(self.files(2), tests.format(2))
        GUARD.validate_rewind_teardown_matches_schema(self.files(2), None)
        with self.assertRaisesRegex(GUARD.GuardError, r"\(1\) does not match the schema head \(V2\)"):
            GUARD.validate_rewind_teardown_matches_schema(self.files(2), tests.format(1))
        GUARD.validate_rewind_teardown_matches_schema(self.files(2), "fn unrelated() {}\n")

    def test_the_rewind_constant_is_found_in_this_checkout(self):
        """#2018: a rename of the constant must not switch the guard off silently."""
        text = (ROOT / GUARD.REWIND_TESTS_PATH).read_text()
        self.assertRegex(text, GUARD.REWIND_TEARDOWN_RE)

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

    def test_a_declared_pending_sibling_may_be_skipped_but_nothing_else(self):
        """#1285: v3 over base v1 passes only with v2 declared as a pending predecessor."""
        base = GUARD.inventory(self.files(1))
        gapped = self.files(3)
        del gapped[f"{GUARD.MIGRATION_DIR}/v002.rs"]
        head = GUARD.inventory(gapped)
        with self.assertRaises(GUARD.GuardError):
            GUARD.validate_append_only(base, head)
        GUARD.validate_append_only(base, head, [2])
        with self.assertRaises(GUARD.GuardError):
            GUARD.validate_append_only(base, GUARD.inventory(self.files(3)), [2])
        with self.assertRaises(GUARD.GuardError):
            GUARD.validate_append_only(base, head, [3])

    def test_pending_predecessors_parse_only_above_the_base_head(self):
        declaration = json.dumps({"schema_version": 1, "version": 5, "files": [],
                                  "pending_predecessors": [3, 4]})
        self.assertEqual(GUARD.parse_pending_predecessors([declaration], 2), [3, 4])
        self.assertEqual(GUARD.parse_pending_predecessors([declaration], 3), [4])
        self.assertEqual(GUARD.parse_pending_predecessors([declaration], 4), [])
        plain = json.dumps({"schema_version": 1, "version": 5, "files": []})
        self.assertEqual(GUARD.parse_pending_predecessors([plain], 2), [])

    def test_malformed_pending_predecessors_are_refused(self):
        for bad in ([], [5], [6], ["4"], [True], [4, 4], list(range(1, 20))):
            declaration = json.dumps({"schema_version": 1, "version": 5, "files": [],
                                      "pending_predecessors": bad})
            with self.assertRaises(GUARD.GuardError, msg=repr(bad)):
                GUARD.parse_pending_predecessors([declaration], 0)
        with self.assertRaises(GUARD.GuardError):
            GUARD.parse_pending_predecessors(["not json"], 0)

    def test_inline_and_split_layouts_share_block_digests(self):
        inline = "// V0: Original schema\n        create_table();\n\n// V1: Session metadata columns\n" + (
            "        if version < 1 {\n            migrate();\n        }\n"
        )
        legacy = GUARD.inventory({GUARD.MIGRATION_PATH: "pub const LATEST_SCHEMA_VERSION: i32 = 1;\n" + inline})
        split = GUARD.inventory(self.files(1))
        self.assertEqual(legacy["blocks"]["1"], split["blocks"]["1"])
        GUARD.validate_append_only(legacy, split)


class AlteredObjectsTest(unittest.TestCase):
    """#2064: a new migration that alters an existing object needs one table entry."""

    TABLE = (
        "pub(crate) const MIGRATION_ALTERED_OBJECTS: &[(i32, &[&str])] = &[\n"
        "    // #1: why\n"
        "{entries}"
        "];\n"
    )

    def table(self, *entries):
        return self.TABLE.format(entries="".join(f"    {entry}\n" for entry in entries))

    def migration(self, version, sql):
        return {f"{GUARD.MIGRATION_DIR}/v{version:03d}.rs": f'execute_batch("{sql}");\n'}

    def check(self, version, sql, *entries):
        GUARD.validate_altered_objects(self.migration(version, sql), version - 1, self.table(*entries))

    def test_an_alter_without_an_entry_is_refused_and_names_the_entry(self):
        with self.assertRaises(GUARD.GuardError) as caught:
            self.check(7, "ALTER TABLE things ADD COLUMN extra TEXT;")
        message = str(caught.exception)
        self.assertIn('(7, &["things"]),', message)
        self.assertIn("false refusal", message)
        self.assertIn(GUARD.ALTERED_OBJECTS_PATH, message)

    def test_rustfmt_multiline_and_trailing_comma_entries_are_parsed(self):
        sql = "ALTER TABLE things ADD COLUMN extra TEXT; DROP TRIGGER IF EXISTS guard_a;"
        multiline = '(\n        7,\n        &[\n            "things",\n            "guard_a",\n        ],\n    ),'
        self.check(7, sql, multiline)
        self.check(7, sql, '(7, &["things", "guard_a",],),')
        self.check(7, sql, '(7, &["things"],),\n    (7, &["guard_a"]),')

    def test_an_unparseable_entry_is_a_hard_error_naming_the_line(self):
        with self.assertRaises(GUARD.GuardError) as caught:
            self.check(7, "ALTER TABLE things ADD COLUMN extra TEXT;", '(7, ["things"]),')
        self.assertIn("unparseable", str(caught.exception))
        self.assertIn("line 2", str(caught.exception))

    def test_the_listed_object_passes_and_a_partial_entry_names_the_union(self):
        self.check(7, "ALTER TABLE things ADD COLUMN extra TEXT;", '(7, &["things"]),')
        with self.assertRaises(GUARD.GuardError) as caught:
            self.check(
                7,
                "ALTER TABLE things ADD COLUMN extra TEXT; DROP TRIGGER IF EXISTS guard_a;",
                '(7, &["things"]),',
            )
        self.assertIn('(7, &["guard_a", "things"]),', str(caught.exception))

    def test_a_rebuild_is_seen_as_drop_and_rename_and_scratch_is_not_required(self):
        rebuild = (
            "CREATE TABLE things_new (id TEXT); INSERT INTO things_new SELECT id FROM things; "
            "DROP TABLE things; ALTER TABLE things_new RENAME TO things;"
        )
        with self.assertRaises(GUARD.GuardError) as caught:
            self.check(7, rebuild)
        self.assertIn('(7, &["things"]),', str(caught.exception))
        self.check(7, rebuild, '(7, &["things"]),')

    def test_new_objects_comments_and_other_versions_do_not_need_entries(self):
        self.check(7, "CREATE TABLE brand_new (id TEXT); CREATE TRIGGER t_a BEFORE UPDATE ON brand_new BEGIN SELECT 1; END;")
        files = {f"{GUARD.MIGRATION_DIR}/v007.rs": "// ALTER TABLE old ADD COLUMN x TEXT\n/// DROP TABLE old\n"}
        GUARD.validate_altered_objects(files, 6, self.table())
        # Released versions are immutable and not rescanned.
        GUARD.validate_altered_objects(self.migration(7, "ALTER TABLE things ADD COLUMN x;"), 7, self.table())
        # A tree without the table or a base skips the check.
        GUARD.validate_altered_objects(self.migration(7, "ALTER TABLE things ADD COLUMN x;"), 6, None)
        GUARD.validate_altered_objects(self.migration(7, "ALTER TABLE things ADD COLUMN x;"), None, self.table())

    def test_a_rebuild_helper_call_needs_some_entry(self):
        files = {f"{GUARD.MIGRATION_DIR}/v008.rs": "cohort::rebuild_status_constraint(&tx)?;\n"}
        with self.assertRaisesRegex(GUARD.GuardError, r"rebuild_\* helper.*\(8, &\[\"<object"):
            GUARD.validate_altered_objects(files, 7, self.table())
        GUARD.validate_altered_objects(files, 7, self.table('(8, &["status_items"]),'))

    def test_the_altered_objects_table_is_found_in_this_checkout(self):
        """A rename of the table must not switch the guard off silently."""
        text = (ROOT / GUARD.ALTERED_OBJECTS_PATH).read_text()
        table = GUARD.altered_objects_table(text)
        self.assertEqual(table[170], {"manager_portfolio_appointments"})

    def test_the_backfilled_table_covers_every_checked_in_migration_above_its_floor(self):
        """The recorded versions are what the guard would demand of those migrations today."""
        text = (ROOT / GUARD.ALTERED_OBJECTS_PATH).read_text()
        floor = int(re.search(r"COMPLETE_ABOVE: i32 = (\d+);", text).group(1))
        files = {}
        for path in sorted((ROOT / GUARD.MIGRATION_DIR).glob("v*.rs")):
            files[f"{GUARD.MIGRATION_DIR}/{path.name}"] = path.read_text()
        GUARD.validate_altered_objects(files, floor, text)


if __name__ == "__main__":
    unittest.main()
