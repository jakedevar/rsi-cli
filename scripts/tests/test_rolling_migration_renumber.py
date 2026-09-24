"""Hermetic proof and conflict tests for mechanical migration renumbering."""

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
        self.temp = tempfile.TemporaryDirectory(prefix="rsi-renumber-test-")
        self.addCleanup(self.temp.cleanup)
        self.repo = Path(self.temp.name) / "repo"
        self.repo.mkdir()
        self.git(self.repo, "init", "-q", "-b", "rolling")
        self.git(self.repo, "config", "user.name", "Migration Test")
        self.git(self.repo, "config", "user.email", "migration@example.invalid")
        self.write(self.repo, "crates/rsid/src/store/mod.rs", self.base_store())
        self.write(self.repo, "crates/rsid/Cargo.toml", '[package]\nname = "rsid"\nversion = "0.1.0"\n')
        self.write(self.repo, "crates/rsid/src/store/cohort_settlement.rs", "")
        self.write(self.repo, "crates/rsid/src/store/tests.rs", self.base_tests())
        inventory = RENUMBER.guard.inventory({
            RENUMBER.STORE: self.base_store(),
            "crates/rsid/src/store/cohort_settlement.rs": "",
            "crates/rsid/src/store/tests.rs": self.base_tests(),
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
        result = subprocess.run(["git", "-C", str(repo), *args], capture_output=True,
                                text=True, check=False)
        if result.returncode:
            raise AssertionError(f"git {' '.join(args)}: {result.stderr}")
        return result.stdout.strip()

    @staticmethod
    def write(repo, name, value):
        target = repo / name
        target.parent.mkdir(parents=True, exist_ok=True)
        target.write_text(value)

    def source(self, label, extra="", released_edit=False):
        worktree = Path(self.temp.name) / f"source-{label}"
        self.git(self.repo, "worktree", "add", "-q", "--detach", str(worktree), self.base)
        helper = f"crates/rsid/src/store/{label}_v130.rs"
        helper_text = (f"// RSI-RELEASED-MIGRATION-BEGIN: v130-{label}-migration\n"
                       f"pub fn apply_v130() {{}}\n"
                       f"// RSI-RELEASED-MIGRATION-END: v130-{label}-migration\n")
        self.write(worktree, helper, helper_text)
        store = self.base_store().replace("LATEST_SCHEMA_VERSION: i32 = 129",
                                          "LATEST_SCHEMA_VERSION: i32 = 130")
        store = store.replace("// V0: Original schema",
                              f"mod {label}_v130;\n// V0: Original schema")
        store += (f"// V130: {label} migration\n"
                  f"if version < 130 {{\n    {label}_v130::apply_v130();\n}}\n")
        store += extra
        if released_edit:
            store = store.replace("migrate_v129();", "changed_v129();")
        self.write(worktree, RENUMBER.STORE, store)
        tests = self.base_tests().replace("REWIND: i32 = 129", "REWIND: i32 = 130")
        self.write(worktree, "crates/rsid/src/store/tests.rs", tests)
        inventory = RENUMBER.guard.inventory({
            RENUMBER.STORE: store,
            "crates/rsid/src/store/cohort_settlement.rs": "",
            "crates/rsid/src/store/tests.rs": tests,
            helper: helper_text,
        })
        self.write(worktree, RENUMBER.MANIFEST, json.dumps(inventory, indent=2) + "\n")
        files = [
            (RENUMBER.STORE, store, RENUMBER.STORE, [
                ("pub const LATEST_SCHEMA_VERSION: i32 = 130;",
                 "pub const LATEST_SCHEMA_VERSION: i32 = ${VERSION};", "head"),
                (f"mod {label}_v130;", f"mod {label}_v${{VERSION}};", "unit"),
                (f"// V130: {label} migration", f"// V${{VERSION}}: {label} migration", "unit"),
                ("if version < 130 {", "if version < ${VERSION} {", "unit"),
                (f"{label}_v130::apply_v130();",
                 f"{label}_v${{VERSION}}::apply_v${{VERSION}}();", "unit"),
            ]),
            ("crates/rsid/src/store/tests.rs", tests,
             "crates/rsid/src/store/tests.rs", [
                 ("const REWIND: i32 = 130;", "const REWIND: i32 = ${VERSION};", "head"),
             ]),
            (helper, helper_text, f"crates/rsid/src/store/{label}_v${{VERSION}}.rs", [
                (f"// RSI-RELEASED-MIGRATION-BEGIN: v130-{label}-migration",
                 f"// RSI-RELEASED-MIGRATION-BEGIN: v${{VERSION}}-{label}-migration", "unit"),
                ("pub fn apply_v130() {}", "pub fn apply_v${VERSION}() {}", "unit"),
                (f"// RSI-RELEASED-MIGRATION-END: v130-{label}-migration",
                 f"// RSI-RELEASED-MIGRATION-END: v${{VERSION}}-{label}-migration", "unit"),
            ]),
        ]
        declaration = {"schema_version": 1, "version": 130, "files": [
            {"path": name, "path_template": template,
             "source_blob": RENUMBER.sha(content.encode()),
             "sites": [{"anchor": before, "replacement": after, "scope": scope}
                       for before, after, scope in sites]}
            for name, content, template, sites in files
        ]}
        self.write(worktree, f"tools/provisional-migrations/{label}.json",
                   json.dumps(declaration, indent=2) + "\n")
        self.git(worktree, "add", ".")
        self.git(worktree, "commit", "-q", "-m", f"provisional V130 {label}")
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


if __name__ == "__main__":
    unittest.main()
