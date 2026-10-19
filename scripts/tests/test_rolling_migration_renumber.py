"""Hermetic proof and conflict tests for mechanical migration renumbering."""

import importlib.util
import json
from pathlib import Path
import subprocess
import sys
import unittest

sys.dont_write_bytecode = True
ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
from scratch_git import scratch_git, scratch_tempdir  # noqa: E402
SPEC = importlib.util.spec_from_file_location(
    "rolling_migration_renumber", ROOT / "tools/rolling-migration-renumber.py"
)
RENUMBER = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(RENUMBER)
GUARD_SPEC = importlib.util.spec_from_file_location(
    "rolling_landing_guard", ROOT / "scripts/rolling-landing-guard.py"
)
LANDING_GUARD = importlib.util.module_from_spec(GUARD_SPEC)
GUARD_SPEC.loader.exec_module(LANDING_GUARD)


class ProvisionalMigrationTest(unittest.TestCase):
    def setUp(self):
        self.temp = scratch_tempdir(prefix="rsi-renumber-test-")
        self.addCleanup(self.temp.cleanup)
        self.repo = Path(self.temp.name) / "repo"
        self.repo.mkdir()
        self.git(self.repo, "init", "-q", "-b", "rolling")
        self.git(self.repo, "config", "user.name", "Migration Test")
        self.git(self.repo, "config", "user.email", "migration@example.invalid")
        self.write(self.repo, "crates/rsid-store/src/store/mod.rs", self.base_store())
        self.write(self.repo, "crates/rsid/Cargo.toml", '[package]\nname = "rsid"\nversion = "0.1.0"\n')
        self.write(self.repo, "crates/rsid-store/Cargo.toml", '[package]\nname = "rsid-store"\nversion = "0.1.0"\n')
        self.write(self.repo, "crates/rsid-store/src/store/cohort_settlement.rs", "")
        self.write(self.repo, "crates/rsid-store/src/store/tests.rs", self.base_tests())
        inventory = RENUMBER.guard.inventory({
            RENUMBER.STORE: self.base_store(),
            "crates/rsid-store/src/store/cohort_settlement.rs": "",
            "crates/rsid-store/src/store/tests.rs": self.base_tests(),
        })
        self.write(self.repo, RENUMBER.MANIFEST, json.dumps(inventory, indent=2) + "\n")
        self.git(self.repo, "add", ".")
        self.git(self.repo, "commit", "-q", "-m", "base V129")
        self.base = self.git(self.repo, "rev-parse", "HEAD")
        self.completed = {}

    @staticmethod
    def base_store():
        return ("pub const LATEST_SCHEMA_VERSION: i32 = 129;\n"
                "// V0: Original schema\n"
                "// original\n"
                "// V1: Session metadata columns\n"
                "if version < 1 {\n    migrate_v1();\n}\n"
                "if version < 129 {\n    migrate_v129();\n}\n")

    @staticmethod
    def base_tests():
        return ("// RSI-RELEASED-MIGRATION-BEGIN: test-catalog\n"
                "// fixed test catalog\n"
                "// RSI-RELEASED-MIGRATION-END: test-catalog\n"
                "const REWIND: i32 = 129;\n")

    @staticmethod
    def git(repo, *args):
        result = scratch_git(repo, *args, check=False)
        if result.returncode:
            raise AssertionError(f"git {' '.join(args)}: {result.stderr}")
        return result.stdout.strip()

    @staticmethod
    def write(repo, name, value):
        target = repo / name
        target.parent.mkdir(parents=True, exist_ok=True)
        target.write_text(value)

    def source(self, label, extra="", released_edit=False, version=130, pending=None):
        worktree = Path(self.temp.name) / f"source-{label}"
        self.git(self.repo, "worktree", "add", "-q", "--detach", str(worktree), self.base)
        helper = f"crates/rsid-store/src/store/{label}_v{version}.rs"
        helper_text = (f"// RSI-RELEASED-MIGRATION-BEGIN: v{version}-{label}-migration\n"
                       f"pub fn apply_v{version}() {{}}\n"
                       f"// RSI-RELEASED-MIGRATION-END: v{version}-{label}-migration\n")
        self.write(worktree, helper, helper_text)
        store = self.base_store().replace("LATEST_SCHEMA_VERSION: i32 = 129",
                                          f"LATEST_SCHEMA_VERSION: i32 = {version}")
        store = store.replace("// V0: Original schema",
                              f"mod {label}_v{version};\n// V0: Original schema")
        store += (f"// V{version}: {label} migration\n"
                  f"if version < {version} {{\n    {label}_v{version}::apply_v{version}();\n}}\n")
        store += extra
        if released_edit:
            store = store.replace("migrate_v129();", "changed_v129();")
        self.write(worktree, RENUMBER.STORE, store)
        tests = self.base_tests().replace("REWIND: i32 = 129", f"REWIND: i32 = {version}")
        self.write(worktree, "crates/rsid-store/src/store/tests.rs", tests)
        base_manifest = RENUMBER.revision_inventory(self.repo, self.base)
        files = {name: (worktree / name).read_text()
                 for name in RENUMBER.guard.tracked_source_paths(base_manifest)}
        files.update({RENUMBER.STORE: store,
                      "crates/rsid-store/src/store/tests.rs": tests,
                      helper: helper_text})
        inventory = RENUMBER.guard.inventory(files)
        self.write(worktree, RENUMBER.MANIFEST, json.dumps(inventory, indent=2) + "\n")
        files = [
            (RENUMBER.STORE, store, RENUMBER.STORE, [
                (f"pub const LATEST_SCHEMA_VERSION: i32 = {version};",
                 "pub const LATEST_SCHEMA_VERSION: i32 = ${VERSION};", "head"),
                (f"mod {label}_v{version};", f"mod {label}_v${{VERSION}};", "unit"),
                (f"// V{version}: {label} migration", f"// V${{VERSION}}: {label} migration", "unit"),
                (f"if version < {version} {{", "if version < ${VERSION} {", "unit"),
                (f"{label}_v{version}::apply_v{version}();",
                 f"{label}_v${{VERSION}}::apply_v${{VERSION}}();", "unit"),
            ]),
            ("crates/rsid-store/src/store/tests.rs", tests,
             "crates/rsid-store/src/store/tests.rs", [
                 (f"const REWIND: i32 = {version};", "const REWIND: i32 = ${VERSION};", "head"),
             ]),
            (helper, helper_text, f"crates/rsid-store/src/store/{label}_v${{VERSION}}.rs", [
                (f"// RSI-RELEASED-MIGRATION-BEGIN: v{version}-{label}-migration",
                 f"// RSI-RELEASED-MIGRATION-BEGIN: v${{VERSION}}-{label}-migration", "unit"),
                (f"pub fn apply_v{version}() {{}}", "pub fn apply_v${VERSION}() {}", "unit"),
                (f"// RSI-RELEASED-MIGRATION-END: v{version}-{label}-migration",
                 f"// RSI-RELEASED-MIGRATION-END: v${{VERSION}}-{label}-migration", "unit"),
            ]),
        ]
        declaration = {"schema_version": 1, "version": version, "files": [
            {"path": name, "path_template": template,
             "source_blob": RENUMBER.sha(content.encode()),
             "sites": [{"anchor": before, "replacement": after, "scope": scope}
                       for before, after, scope in sites]}
            for name, content, template, sites in files
        ]}
        if pending:
            declaration["pending_predecessors"] = pending
        self.write(worktree, f"tools/provisional-migrations/{label}.json",
                   json.dumps(declaration, indent=2) + "\n")
        self.git(worktree, "add", ".")
        self.git(worktree, "commit", "-q", "-m", f"provisional V{version} {label}")
        return self.git(worktree, "rev-parse", "HEAD")

    def candidate(self, source, target):
        unit = RENUMBER.inspect(self.repo, self.base, source, target)
        unit.update(RENUMBER.transform(self.repo, unit))
        unit["prior_units"] = self.completed.get(target, [])
        scratch = Path(self.temp.name) / "scratch"
        scratch.mkdir(exist_ok=True)
        candidate = RENUMBER.build_candidate(self.repo, unit, scratch)["candidate"]
        self.completed[candidate] = unit["prior_units"] + [{
            "base": unit["base"], "source": unit["source"],
            "target": unit["target"], "unit_candidate": candidate,
        }]
        return unit, candidate

    def test_two_v130_sources_land_as_v130_v131_with_exact_parents_and_proofs(self):
        first_source = self.source("alpha")
        second_source = self.source("beta")
        first, first_candidate = self.candidate(first_source, self.base)
        second, final = self.candidate(second_source, first_candidate)
        self.assertEqual(self.git(self.repo, "show", "-s", "--format=%P", final),
                         f"{first_candidate} {second_source}")
        self.assertTrue(RENUMBER.is_ancestor(self.repo, first_source, final))
        self.assertTrue(RENUMBER.is_ancestor(self.repo, second_source, final))
        manifest = RENUMBER.revision_inventory(self.repo, final)
        self.assertEqual(manifest["latest_schema_version"], 131)
        self.assertEqual(manifest["blocks"]["130"], first["source_manifest"]["blocks"]["130"])
        released = subprocess.run(
            [sys.executable, str(ROOT / "tools/check-released-migrations.py"),
             self.base, final], cwd=self.repo, capture_output=True, text=True, check=False,
        )
        self.assertEqual(released.returncode, 0, released.stderr + released.stdout)
        for unit, intermediate in ((first, first_candidate), (second, final)):
            proof = RENUMBER.prove(self.repo, unit, intermediate, final)
            self.assertEqual(LANDING_GUARD.lost_hunks(
                self.repo, self.base, unit["source"], final, proof), [])
            proof_path = Path(self.temp.name) / f"proof-{intermediate}.json"
            proof_path.write_text(json.dumps(proof))
            self.assertEqual(LANDING_GUARD.validate_renumber_proof(
                self.repo, self.base, unit["source"], final, proof_path, self.base), proof)
            invocation = subprocess.run(
                [sys.executable, str(ROOT / "scripts/rolling-landing-guard.py"),
                 "--repo", str(self.repo), "--base", self.base,
                 "--source", unit["source"], "--target", self.base,
                 "--candidate", final, "--proof", str(proof_path), "--plan-only"],
                capture_output=True, text=True, check=False,
            )
            self.assertEqual(invocation.returncode, 0, invocation.stderr + invocation.stdout)
            with self.assertRaises(LANDING_GUARD.GuardError):
                LANDING_GUARD.validate_renumber_proof(
                    self.repo, self.base, unit["source"], final, proof_path, second_source)
            proof["sites"][0]["after"] = "tampered"
            proof_path.write_text(json.dumps(proof))
            with self.assertRaises(LANDING_GUARD.GuardError):
                LANDING_GUARD.validate_renumber_proof(
                    self.repo, self.base, unit["source"], final, proof_path)
        RENUMBER.guard.validate_append_only(RENUMBER.revision_inventory(self.repo, self.base),
                                            manifest)

    def test_a_slice_numbered_after_a_declared_pending_sibling_lands_after_it(self):
        """#1285: V131 over base V129 with V130 pending lands once the V130 sibling is on the target."""
        sibling = self.source("alpha")
        gapped = self.source("beta", version=131, pending=[130])
        with self.assertRaisesRegex(RENUMBER.Refusal, "pending predecessor"):
            RENUMBER.inspect(self.repo, self.base, gapped, self.base)
        guard_cli = [sys.executable, str(ROOT / "tools/check-released-migrations.py")]
        declared = subprocess.run([*guard_cli, self.base, gapped], cwd=self.repo,
                                  capture_output=True, text=True, check=False)
        self.assertEqual(declared.returncode, 0, declared.stderr + declared.stdout)
        undeclared = self.source("gamma", version=131)
        refused = subprocess.run([*guard_cli, self.base, undeclared], cwd=self.repo,
                                 capture_output=True, text=True, check=False)
        self.assertNotEqual(refused.returncode, 0)
        self.assertIn("pending_predecessors", refused.stderr)
        _, sibling_candidate = self.candidate(sibling, self.base)
        unit, final = self.candidate(gapped, sibling_candidate)
        self.assertEqual(unit["old_version"], 131)
        self.assertEqual(unit["new_version"], 131)
        manifest = RENUMBER.revision_inventory(self.repo, final)
        self.assertEqual(manifest["latest_schema_version"], 131)
        self.assertEqual(sorted(manifest["blocks"])[-2:], ["130", "131"])
        proof = RENUMBER.prove(self.repo, unit, final, final)
        self.assertEqual(LANDING_GUARD.lost_hunks(self.repo, self.base, gapped, final, proof), [])
        released = subprocess.run(
            [sys.executable, str(ROOT / "tools/check-released-migrations.py"), sibling_candidate, final],
            cwd=self.repo, capture_output=True, text=True, check=False,
        )
        self.assertEqual(released.returncode, 0, released.stderr + released.stdout)

    def test_a_gap_without_a_declaration_is_still_more_than_one_migration(self):
        undeclared = self.source("beta", version=131)
        with self.assertRaises(RENUMBER.Refusal):
            RENUMBER.inspect(self.repo, self.base, undeclared, self.base)

    def test_build_merge_uses_private_committer_identity(self):
        source = self.source("alpha")
        self.git(self.repo, "config", "--unset", "user.name")
        self.git(self.repo, "config", "--unset", "user.email")
        _, candidate = self.candidate(source, self.base)
        self.assertEqual(self.git(self.repo, "show", "-s", "--format=%cn <%ce>", candidate),
                         "rsi rolling landing <rsi-rolling-land@rsi.invalid>")

    def test_undeclared_version_site_refuses(self):
        source = self.source("alpha", "const HIDDEN_V130: i32 = 130;\n")
        with self.assertRaisesRegex(RENUMBER.Refusal, "undeclared version-bearing site"):
            RENUMBER.inspect(self.repo, self.base, source, self.base)

    def test_three_sources_keep_accepted_order_and_rebind_prior_proofs(self):
        sources = [self.source(label) for label in ("alpha", "beta", "gamma")]
        tip = self.base
        units = []
        for source in sources:
            unit, tip = self.candidate(source, tip)
            units.append((unit, tip))
        self.assertEqual([unit["new_version"] for unit, _ in units], [130, 131, 132])
        self.assertEqual(RENUMBER.revision_inventory(self.repo, tip)["latest_schema_version"], 132)
        for unit, intermediate in units:
            proof = RENUMBER.prove(self.repo, unit, intermediate, tip)
            self.assertEqual(LANDING_GUARD.lost_hunks(
                self.repo, self.base, unit["source"], tip, proof), [])

    def test_second_landing_discovers_proved_sites_from_published_target(self):
        first_source = self.source("alpha")
        second_source = self.source("beta")
        _, first_candidate = self.candidate(first_source, self.base)
        self.completed.clear()  # A new lander invocation has no in-memory proof list.
        second, final = self.candidate(second_source, first_candidate)
        self.assertEqual(second["new_version"], 131)
        self.assertEqual(RENUMBER.revision_inventory(self.repo, final)["latest_schema_version"], 131)

    def test_target_history_with_unproved_migration_merge_refuses(self):
        source = self.source("alpha")
        tree = self.git(self.repo, "rev-parse", f"{self.base}^{{tree}}")
        false_candidate = self.git(self.repo, "-c", "user.name=Migration Test",
                                   "-c", "user.email=migration@example.invalid",
                                   "commit-tree", tree, "-p", self.base, "-p", source,
                                   "-m", "unproved migration merge")
        with self.assertRaisesRegex(RENUMBER.Refusal, "released prefix|migration block"):
            RENUMBER.discover_target_provisional_units(self.repo, self.base, false_candidate)

    def test_nonmigration_source_hunk_survives_transform_and_guard(self):
        source = self.source("alpha")
        builder = Path(self.temp.name) / "source-alpha"
        self.write(builder, "docs/accepted-source.txt", "accepted source content\n")
        self.git(builder, "add", "docs/accepted-source.txt")
        self.git(builder, "commit", "-q", "-m", "accepted documentation")
        source = self.git(builder, "rev-parse", "HEAD")
        unit, candidate = self.candidate(source, self.base)
        proof = RENUMBER.prove(self.repo, unit, candidate, candidate)
        self.assertEqual(RENUMBER.show(self.repo, candidate, "docs/accepted-source.txt"),
                         b"accepted source content\n")
        self.assertEqual(LANDING_GUARD.lost_hunks(
            self.repo, self.base, source, candidate, proof), [])

    def test_rewound_descendant_refuses_proof_and_canary_guard(self):
        source = self.source("alpha")
        unit, candidate = self.candidate(source, self.base)
        self.git(self.repo, "checkout", "--detach", "-q", candidate)
        self.git(self.repo, "read-tree", "--reset", "-u", self.base)
        self.git(self.repo, "commit", "-q", "--allow-empty", "-m", "rewind candidate")
        rewound = self.git(self.repo, "rev-parse", "HEAD")
        self.assertTrue(RENUMBER.is_ancestor(self.repo, source, rewound))
        with self.assertRaises(RENUMBER.Refusal):
            RENUMBER.prove(self.repo, unit, candidate, rewound)
        proof = RENUMBER.prove(self.repo, unit, candidate, candidate)
        proof_path = Path(self.temp.name) / "canary-proof.json"
        proof_path.write_text(json.dumps(proof))
        with self.assertRaises(LANDING_GUARD.GuardError):
            LANDING_GUARD.validate_renumber_proof(
                self.repo, self.base, source, rewound, proof_path, self.base)

    def test_released_ddl_change_refuses_even_with_refreshed_manifest(self):
        source = self.source("alpha", released_edit=True)
        with self.assertRaisesRegex(RENUMBER.Refusal, "released prefix"):
            RENUMBER.inspect(self.repo, self.base, source, self.base)

    def test_nonmigration_conflict_refuses(self):
        first_source = self.source("alpha")
        second_source = self.source("beta", "semantic_change();\n")
        _, first_candidate = self.candidate(first_source, self.base)
        with self.assertRaisesRegex(RENUMBER.Refusal, "unproved migration conflict"):
            self.candidate(second_source, first_candidate)

    def test_target_semantic_neighbor_in_conflict_refuses(self):
        first_source = self.source("alpha", "semantic_change();\n")
        second_source = self.source("beta")
        _, first_candidate = self.candidate(first_source, self.base)
        with self.assertRaisesRegex(RENUMBER.Refusal, "unproved migration conflict"):
            self.candidate(second_source, first_candidate)

    def test_duplicate_gate_and_malformed_inventory_refuse(self):
        self.source("alpha")
        builder = Path(self.temp.name) / "source-alpha"
        store = (builder / RENUMBER.STORE).read_text()
        self.write(builder, RENUMBER.STORE,
                   store + "if version < 130 {\n    duplicate();\n}\n")
        self.git(builder, "add", RENUMBER.STORE)
        self.git(builder, "commit", "-q", "-m", "duplicate gate")
        duplicate = self.git(builder, "rev-parse", "HEAD")
        with self.assertRaisesRegex(RENUMBER.Refusal, "duplicate migration block"):
            RENUMBER.inspect(self.repo, self.base, duplicate, self.base)
        self.write(builder, RENUMBER.MANIFEST, "{malformed\n")
        self.git(builder, "add", RENUMBER.MANIFEST)
        self.git(builder, "commit", "-q", "-m", "malformed inventory")
        malformed = self.git(builder, "rev-parse", "HEAD")
        with self.assertRaisesRegex(RENUMBER.Refusal, "invalid migration inventory"):
            RENUMBER.inspect(self.repo, self.base, malformed, self.base)


MIGRATIONS = "crates/rsid-store/src/store/migrations"


class PerFileProvisionalMigrationTest(unittest.TestCase):
    """The per-version layout: a provisional migration is one new ``vNNN.rs``."""

    git = staticmethod(ProvisionalMigrationTest.git)
    write = staticmethod(ProvisionalMigrationTest.write)
    candidate = ProvisionalMigrationTest.candidate

    V0 = ("impl Store {\n    fn migrate_v000(&self, version: i32) -> Result<()> {\n"
          "        // V0: Original schema\n        // original\n        Ok(())\n    }\n}\n")
    V129 = ("impl Store {\n    fn migrate_v129(&self, version: i32) -> Result<()> {\n"
            "        if version < 129 {\n            settled();\n        }\n"
            "        Ok(())\n    }\n}\n")
    TESTS = ("// RSI-RELEASED-MIGRATION-BEGIN: test-catalog\n// fixed\n"
             "// RSI-RELEASED-MIGRATION-END: test-catalog\nconst REWIND: i32 = 129;\n"
             # An unrelated literal equal to the provisional number (#1655).
             "fn pagination() {\n    assert_eq!(second.events.len(), 130);\n}\n")

    def setUp(self):
        self.temp = scratch_tempdir(prefix="rsi-renumber-dir-test-")
        self.addCleanup(self.temp.cleanup)
        self.repo = Path(self.temp.name) / "repo"
        self.repo.mkdir()
        self.git(self.repo, "init", "-q", "-b", "rolling")
        self.git(self.repo, "config", "user.name", "Migration Test")
        self.git(self.repo, "config", "user.email", "migration@example.invalid")
        files = {
            RENUMBER.STORE: "// runner only\n",
            "crates/rsid/Cargo.toml": '[package]\nname = "rsid"\nversion = "0.1.0"\n',
            "crates/rsid-store/Cargo.toml": '[package]\nname = "rsid-store"\nversion = "0.1.0"\n',
            "crates/rsid-store/src/store/cohort_settlement.rs": "",
            "crates/rsid-store/src/store/tests.rs": self.TESTS,
            f"{MIGRATIONS}/v000.rs": self.V0,
            f"{MIGRATIONS}/v129.rs": self.V129,
        }
        for name, value in files.items():
            self.write(self.repo, name, value)
        inventory = RENUMBER.guard.inventory(files)
        self.write(self.repo, RENUMBER.MANIFEST, json.dumps(inventory, indent=2) + "\n")
        self.git(self.repo, "add", ".")
        self.git(self.repo, "commit", "-q", "-m", "base V129")
        self.base = self.git(self.repo, "rev-parse", "HEAD")
        self.completed = {}

    def source(self, label, deferred_pin=False):
        worktree = Path(self.temp.name) / f"source-{label}"
        self.git(self.repo, "worktree", "add", "-q", "--detach", str(worktree), self.base)
        unit = (f"impl Store {{\n    fn migrate_v130(&self, version: i32) -> Result<()> {{\n"
                f"        if version < 130 {{\n            {label}_apply();\n        }}\n"
                f"        Ok(())\n    }}\n}}\n")
        tests = self.TESTS.replace("REWIND: i32 = 129", "REWIND: i32 = 130")
        self.write(worktree, f"{MIGRATIONS}/v130.rs", unit)
        self.write(worktree, "crates/rsid-store/src/store/tests.rs", tests)
        files = {name: (worktree / name).read_text() for name in (
            RENUMBER.STORE, "crates/rsid-store/src/store/cohort_settlement.rs",
            "crates/rsid-store/src/store/tests.rs", f"{MIGRATIONS}/v000.rs",
            f"{MIGRATIONS}/v129.rs", f"{MIGRATIONS}/v130.rs")}
        inventory = RENUMBER.guard.inventory(files)
        if not deferred_pin:
            self.write(worktree, RENUMBER.MANIFEST, json.dumps(inventory, indent=2) + "\n")
        declared = [
            (f"{MIGRATIONS}/v130.rs", unit, f"{MIGRATIONS}/v${{VERSION}}.rs", [
                ("fn migrate_v130(", "fn migrate_v${VERSION}(", "unit"),
                ("if version < 130 {", "if version < ${VERSION} {", "unit"),
            ]),
            ("crates/rsid-store/src/store/tests.rs", tests, "crates/rsid-store/src/store/tests.rs", [
                ("const REWIND: i32 = 130;", "const REWIND: i32 = ${VERSION};", "head"),
            ]),
        ]
        declaration = {"schema_version": 1, "version": 130, "files": [
            {"path": name, "path_template": template,
             "source_blob": RENUMBER.sha(content.encode()),
             "sites": [{"anchor": before, "replacement": after, "scope": scope}
                       for before, after, scope in sites]}
            for name, content, template, sites in declared]}
        self.write(worktree, f"tools/provisional-migrations/{label}.json",
                   json.dumps(declaration, indent=2) + "\n")
        self.git(worktree, "add", ".")
        self.git(worktree, "commit", "-q", "-m", f"provisional V130 {label}")
        return self.git(worktree, "rev-parse", "HEAD")

    def test_deferred_pin_passes_guard_and_inspects_next_number(self):
        first_source = self.source("alpha")
        _first, target = self.candidate(first_source, self.base)
        source = self.source("beta", deferred_pin=True)
        checked = subprocess.run(
            [sys.executable, str(ROOT / "tools/check-released-migrations.py"),
             self.base, source], cwd=self.repo, capture_output=True, text=True, check=False)
        self.assertEqual(checked.returncode, 0, checked.stderr + checked.stdout)
        self.assertIn("pin deferred to the lander", checked.stdout)
        for rolling, expected in ((self.base, 130), (target, 131)):
            with self.subTest(next_version=expected):
                inspected = subprocess.run(
                    [sys.executable, str(ROOT / "tools/rolling-migration-renumber.py"),
                     "--repo", str(self.repo), "--base", self.base,
                     "--source", source, "--target", rolling],
                    capture_output=True, text=True, check=False)
                self.assertEqual(inspected.returncode, 0, inspected.stderr)
                unit = json.loads(inspected.stdout)
                self.assertEqual(unit["old_version"], 130)
                self.assertEqual(unit["new_version"], expected)
                self.assertIn("130", unit["source_manifest"]["blocks"])
        _second, candidate = self.candidate(source, target)
        self.assertEqual(RENUMBER.revision_inventory(self.repo, candidate)["latest_schema_version"], 131)
        # Inspection and private candidate construction leave publication to the lander.
        self.assertEqual(RENUMBER.show(self.repo, source, RENUMBER.MANIFEST),
                         RENUMBER.show(self.repo, self.base, RENUMBER.MANIFEST))

    def test_hand_edited_mismatched_pin_is_still_refused(self):
        self.source("alpha", deferred_pin=True)
        manifest = json.loads(RENUMBER.show(self.repo, self.base, RENUMBER.MANIFEST))
        manifest["latest_schema_version"] = 130
        source = self.commit_extra("alpha", RENUMBER.MANIFEST, json.dumps(manifest) + "\n")
        with self.assertRaisesRegex(RENUMBER.Refusal, "provisional inventory does not match its source"):
            RENUMBER.inspect(self.repo, self.base, source, self.base)

    def test_deferred_pin_with_changed_released_migration_is_refused(self):
        self.source("alpha", deferred_pin=True)
        source = self.commit_extra("alpha", f"{MIGRATIONS}/v129.rs",
                                   self.V129.replace("settled();", "changed();"))
        with self.assertRaisesRegex(RENUMBER.Refusal, "provisional inventory does not match its source"):
            RENUMBER.inspect(self.repo, self.base, source, self.base)

    def test_two_v130_files_land_as_v130_v131_without_touching_a_shared_file(self):
        first_source = self.source("alpha")
        second_source = self.source("beta")
        _first, first_candidate = self.candidate(first_source, self.base)
        _second, final = self.candidate(second_source, first_candidate)
        manifest = RENUMBER.revision_inventory(self.repo, final)
        self.assertEqual(manifest["latest_schema_version"], 131)
        self.assertEqual(manifest["migration_dir"], MIGRATIONS)
        listing = self.git(self.repo, "ls-tree", "-r", "--name-only", final, "--", MIGRATIONS)
        self.assertEqual(listing.splitlines(),
                         [f"{MIGRATIONS}/v000.rs", f"{MIGRATIONS}/v129.rs",
                          f"{MIGRATIONS}/v130.rs", f"{MIGRATIONS}/v131.rs"])
        second_text = self.git(self.repo, "show", f"{final}:{MIGRATIONS}/v131.rs")
        self.assertIn("if version < 131 {", second_text)
        self.assertIn("beta_apply();", second_text)
        self.assertIn("fn migrate_v131(", second_text)
        self.assertEqual(self.git(self.repo, "show", f"{final}:{RENUMBER.STORE}"),
                         "// runner only")
        released = subprocess.run(
            [sys.executable, str(ROOT / "tools/check-released-migrations.py"),
             self.base, final], cwd=self.repo, capture_output=True, text=True, check=False)
        self.assertEqual(released.returncode, 0, released.stderr + released.stdout)

    def test_undeclared_gate_site_refuses(self):
        source = self.source("alpha")
        worktree = Path(self.temp.name) / "source-alpha"
        declaration = worktree / "tools/provisional-migrations/alpha.json"
        data = json.loads(declaration.read_text())
        data["files"][0]["sites"] = data["files"][0]["sites"][:1]
        declaration.write_text(json.dumps(data))
        self.git(worktree, "commit", "-qam", "drop gate site")
        with self.assertRaises(RENUMBER.Refusal):
            RENUMBER.inspect(self.repo, self.base,
                             self.git(worktree, "rev-parse", "HEAD"), self.base)
        self.assertTrue(source)

    def commit_extra(self, label, name, text):
        worktree = Path(self.temp.name) / f"source-{label}"
        self.write(worktree, name, text)
        self.git(worktree, "add", name)
        self.git(worktree, "commit", "-q", "-m", f"extra {name}")
        return self.git(worktree, "rev-parse", "HEAD")

    def test_unchanged_literal_and_cargo_lock_do_not_block_a_renumbered_landing(self):
        first_source = self.source("alpha")
        second_source = self.commit_extra(
            self.source("beta") and "beta", "Cargo.lock",
            '[[package]]\nname = "uuid"\nchecksum = "ab130cd"\n')
        _first, first_candidate = self.candidate(first_source, self.base)
        _second, final = self.candidate(second_source, first_candidate)
        manifest = RENUMBER.revision_inventory(self.repo, final)
        self.assertEqual(manifest["latest_schema_version"], 131)
        tests = self.git(self.repo, "show", f"{final}:crates/rsid-store/src/store/tests.rs")
        self.assertIn("second.events.len(), 130)", tests)
        self.assertIn("const REWIND: i32 = 131;", tests)

    def test_declared_site_on_unchanged_base_line_refuses(self):
        source = self.source("alpha")
        worktree = Path(self.temp.name) / "source-alpha"
        decl = worktree / "tools/provisional-migrations/alpha.json"
        data = json.loads(decl.read_text())
        tests = next(f for f in data["files"] if f["path"].endswith("tests.rs"))
        tests["sites"].append({"anchor": "second.events.len(), 130)",
                               "replacement": "second.events.len(), ${VERSION})",
                               "scope": "head"})
        decl.write_text(json.dumps(data, indent=2) + "\n")
        self.git(worktree, "add", ".")
        self.git(worktree, "commit", "-q", "-m", "declare unchanged line")
        source = self.git(worktree, "rev-parse", "HEAD")
        with self.assertRaisesRegex(RENUMBER.Refusal, "unchanged base line"):
            RENUMBER.inspect(self.repo, self.base, source, self.base)

    def test_real_undeclared_schema_site_still_refuses(self):
        first_source = self.source("alpha")
        second_source = self.commit_extra(
            self.source("beta") and "beta", "crates/rsid-store/src/store/extra.rs",
            "const SCHEMA: i32 = 130;\n")
        _first, first_candidate = self.candidate(first_source, self.base)
        with self.assertRaisesRegex(RENUMBER.Refusal, "undeclared version-bearing"):
            RENUMBER.inspect(self.repo, self.base, second_source, first_candidate)

    def test_migration_already_at_head_plus_one_is_not_renumbered(self):
        source = self.commit_extra(
            self.source("alpha") and "alpha", "Cargo.lock", 'checksum = "x130y"\n')
        unit = RENUMBER.inspect(self.repo, self.base, source, self.base, allow_noop=True)
        self.assertTrue(unit["renumber_noop"])
        self.assertEqual(unit["old_version"], unit["new_version"])
        result = subprocess.run(
            [sys.executable, str(ROOT / "tools/rolling-migration-renumber.py"),
             "--repo", str(self.repo), "--transform", "--base", self.base,
             "--source", source, "--target", self.base],
            capture_output=True, text=True, check=False)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertTrue(json.loads(result.stdout)["renumber_noop"])

    def stacked_source_and_landed_target(self, differing_content=False, advance=False):
        migration = self.source("alpha")
        target_tree = self.git(self.repo, "rev-parse", f"{migration}^{{tree}}")
        # The migration lands through another source, so the stacked source's
        # merge-base with rolling still predates its declaration.
        target = self.git(self.repo, "commit-tree", target_tree, "-p", self.base,
                          "-m", "migration landed through another source")
        target_worktree = Path(self.temp.name) / "target"
        self.git(self.repo, "worktree", "add", "-q", "--detach", str(target_worktree), target)
        if differing_content:
            filename = f"{MIGRATIONS}/v130.rs"
            # Even a change outside the hashed migration gate must refuse.
            self.write(target_worktree, filename,
                       (target_worktree / filename).read_text() + "// landed fix\n")
            declaration_path = "tools/provisional-migrations/alpha.json"
            declaration = json.loads((target_worktree / declaration_path).read_text())
            declaration["files"][0]["source_blob"] = RENUMBER.sha(
                (target_worktree / filename).read_bytes())
            self.write(target_worktree, declaration_path, json.dumps(declaration, indent=2) + "\n")
            self.git(target_worktree, "add", filename, declaration_path)
            self.git(target_worktree, "commit", "-q", "-m", "landed migration differs")
        elif advance:
            filename = f"{MIGRATIONS}/v131.rs"
            self.write(target_worktree, filename, self.V129.replace("129", "131"))
            inventory = RENUMBER.guard.inventory({
                name: (target_worktree / name).read_text()
                for name in RENUMBER.protected_paths_at(self.repo, target,
                                                       RENUMBER.revision_inventory(self.repo, target))
            } | {filename: (target_worktree / filename).read_text()})
            self.write(target_worktree, RENUMBER.MANIFEST, json.dumps(inventory, indent=2) + "\n")
            self.git(target_worktree, "add", filename, RENUMBER.MANIFEST)
            self.git(target_worktree, "commit", "-q", "-m", "later migration")
        target = self.git(target_worktree, "rev-parse", "HEAD")
        source = self.commit_extra("alpha", "docs/contract.md", "V130 contract for the next slice\n")
        # Later slices may also edit a declared file beyond its original blob.
        source = self.commit_extra(
            "alpha", "crates/rsid-store/src/store/tests.rs", self.TESTS.replace(
                "REWIND: i32 = 129", "REWIND: i32 = 130") + "// later slice\n")
        self.assertEqual(self.git(self.repo, "merge-base", source, target), self.base)
        return source, target, target_worktree

    def transform_result(self, source, target):
        return subprocess.run(
            [sys.executable, str(ROOT / "tools/rolling-migration-renumber.py"),
             "--repo", str(self.repo), "--transform", "--base", self.base,
             "--source", source, "--target", target],
            capture_output=True, text=True, check=False)

    def test_identical_already_landed_migration_merges_stacked_slice_as_is(self):
        source, target, worktree = self.stacked_source_and_landed_target()
        result = self.transform_result(source, target)
        self.assertEqual(result.returncode, 0, result.stderr)
        unit = json.loads(result.stdout)
        self.assertTrue(unit["renumber_noop"])
        self.assertTrue(unit["migration_already_landed"])
        self.git(worktree, "merge", "--no-ff", "-q", source, "-m", "plain stacked merge")
        candidate = self.git(worktree, "rev-parse", "HEAD")
        self.assertEqual(self.git(self.repo, "show", "-s", "--format=%P", candidate),
                         f"{target} {source}")
        self.assertEqual(RENUMBER.revision_inventory(self.repo, candidate)["latest_schema_version"], 130)
        self.assertEqual(RENUMBER.show(self.repo, candidate, f"{MIGRATIONS}/v130.rs"),
                         RENUMBER.show(self.repo, target, f"{MIGRATIONS}/v130.rs"))
        self.assertEqual(self.git(self.repo, "show", f"{candidate}:docs/contract.md"),
                         "V130 contract for the next slice")
        guard = subprocess.run(
            [sys.executable, str(ROOT / "scripts/rolling-landing-guard.py"),
             "--repo", str(self.repo), "--base", self.base, "--source", source,
             "--target", target, "--candidate", candidate, "--plan-only"],
            capture_output=True, text=True, check=False)
        self.assertEqual(guard.returncode, 0, guard.stderr + guard.stdout)

    def test_identical_already_landed_migration_is_noop_after_schema_head_advances(self):
        source, target, _ = self.stacked_source_and_landed_target(advance=True)
        result = self.transform_result(source, target)
        self.assertEqual(result.returncode, 0, result.stderr)
        unit = json.loads(result.stdout)
        self.assertTrue(unit["renumber_noop"])
        self.assertTrue(unit["migration_already_landed"])
        self.assertEqual(unit["old_version"], 130)
        self.assertEqual(unit["new_version"], 130)
        self.assertEqual(RENUMBER.revision_inventory(self.repo, target)["latest_schema_version"], 131)

    def test_already_landed_migration_with_differing_content_names_remerge_remedy(self):
        source, target, _ = self.stacked_source_and_landed_target(differing_content=True)
        result = self.transform_result(source, target)
        self.assertEqual(result.returncode, 2)
        self.assertIn(f"declared migration {MIGRATIONS}/v130.rs", result.stderr)
        self.assertIn("already on rolling with differing content", result.stderr)
        self.assertIn("re-merge the source onto origin/rolling", result.stderr)
        self.assertIn("retain rolling's released migration", result.stderr)
        self.assertIn("re-submit the merged commit", result.stderr)


if __name__ == "__main__":
    unittest.main()
