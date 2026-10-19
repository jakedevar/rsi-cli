"""The slow tier of released-migration history tests (#2270).

`.config/slow-tier.toml` names the tests, nextest's default-filter leaves them out of the
default profiles, and scripts/check-touched-shards decides per diff whether they run.
"""

import json
import re
import runpy
import subprocess
import sys
import tomllib
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
from scratch_git import scratch_git, scratch_tempdir  # noqa: E402

ROOT = Path(__file__).resolve().parents[2]
CHECKER_PATH = ROOT / "scripts" / "check-touched-shards"
CHECKER = runpy.run_path(str(CHECKER_PATH))

RUNNER_FIXTURE = """\
use super::*;

fn helper() -> u32 {
    1
}

#[test]
fn fast_one() {
    assert_eq!(helper(), 1);
}

#[cfg(any(
    not(feature = "x"),
    feature = "y"
))]
#[test]
fn slow_one() {
    assert_eq!(helper(), 1);
}
"""


def git(repo, *args):
    return scratch_git(repo, *args).stdout


class RepoTest(unittest.TestCase):
    """A throwaway repository with a base commit that carries a small slow-tier config."""

    CONFIG = (
        'package = "rsid-store"\n'
        'trigger_paths = ["crates/rsid-store/src/store/migrations/", "tools/released-migrations.json",'
        ' "crates/rsid-store/src/store/mod.rs", ".config/slow-tier.toml"]\n'
        'tests = ["store::tests::slow_one", "store::other::tests::slow_two"]\n'
    )

    def setUp(self):
        self.temp = scratch_tempdir(prefix="slow-tier-")
        self.addCleanup(self.temp.cleanup)
        self.repo = Path(self.temp.name)
        git(self.repo, "init", "-q", "-b", "main")
        git(self.repo, "config", "user.email", "t@example.invalid")
        git(self.repo, "config", "user.name", "t")
        self.write(".config/slow-tier.toml", self.CONFIG)
        self.write("crates/rsid-store/src/store/tests.rs", RUNNER_FIXTURE)
        self.write("crates/rsid-store/src/store/mod.rs", "pub fn runner() {}\n")
        self.write("crates/rsid-store/src/store/migrations/v1.rs", "// v1\n")
        self.write("crates/rsid-store/src/store/other.rs", "pub fn other() {}\n")
        self.write("tools/released-migrations.json", "{}\n")
        self.write("README.md", "docs\n")
        self.commit("base")
        self.base = git(self.repo, "rev-parse", "HEAD").strip()

    def write(self, relative, text):
        path = self.repo / relative
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(text)

    def commit(self, message):
        git(self.repo, "add", "-A")
        git(self.repo, "commit", "-q", "-m", message)

    def decide(self):
        head = git(self.repo, "rev-parse", "HEAD").strip()
        changed = git(self.repo, "diff", "--name-only", f"{self.base}..{head}").split()
        return CHECKER["slow_tier_decision"](self.repo, self.base, head, changed)


class DecisionTest(RepoTest):
    def test_a_diff_that_touches_no_trigger_excludes_the_tier(self):
        self.write("README.md", "more docs\n")
        self.write("crates/rsid-store/src/store/other.rs", "pub fn other() { /* fix */ }\n")
        self.commit("unrelated")
        self.assertEqual(self.decide(), {"mode": "excluded", "reason": "no_trigger", "tests": 2})

    def test_each_trigger_path_runs_the_tier(self):
        for relative in ("crates/rsid-store/src/store/migrations/v2.rs", "tools/released-migrations.json",
                         "crates/rsid-store/src/store/mod.rs", ".config/slow-tier.toml"):
            with self.subTest(relative=relative):
                git(self.repo, "reset", "-q", "--hard", self.base)
                self.write(relative, ("// changed\n" if relative.endswith(".rs") else
                                      "{\"x\": 1}\n" if relative.endswith(".json") else self.CONFIG + "# note\n"))
                self.commit("trigger")
                decision = self.decide()
                self.assertEqual((decision["mode"], decision["reason"], decision["path"]),
                                 ("full", "trigger_path", relative))

    def test_a_new_fast_test_and_an_edit_inside_a_fast_test_stay_excluded(self):
        text = RUNNER_FIXTURE.replace("assert_eq!(helper(), 1);\n}\n\n#[cfg", "assert_eq!(helper() + 0, 1);\n}\n\n#[cfg", 1)
        text += "\n#[test]\nfn brand_new() {\n    assert!(true);\n}\n"
        self.write("crates/rsid-store/src/store/tests.rs", text)
        self.commit("fast tests")
        self.assertEqual(self.decide()["mode"], "excluded")

    def test_an_edit_inside_a_slow_test_runs_the_tier(self):
        text = RUNNER_FIXTURE.replace("fn slow_one() {\n    assert_eq!(helper(), 1);", "fn slow_one() {\n    assert_eq!(helper(), 2 - 1);", 1)
        self.write("crates/rsid-store/src/store/tests.rs", text)
        self.commit("slow body")
        decision = self.decide()
        self.assertEqual((decision["mode"], decision["reason"]), ("full", "slow_test_source"))
        self.assertEqual(decision["path"], "crates/rsid-store/src/store/tests.rs")

    def test_a_shared_helper_or_a_use_line_runs_the_tier(self):
        for old, new in (("    1\n}\n", "    2\n}\n"), ("use super::*;", "use super::*;\nuse std::fmt;")):
            with self.subTest(old=old):
                git(self.repo, "reset", "-q", "--hard", self.base)
                self.write("crates/rsid-store/src/store/tests.rs", RUNNER_FIXTURE.replace(old, new, 1))
                self.commit("helper")
                self.assertEqual(self.decide()["reason"], "slow_test_source")

    def test_deleting_a_slow_test_runs_the_tier(self):
        self.write("crates/rsid-store/src/store/tests.rs", RUNNER_FIXTURE.split("#[cfg(any(")[0])
        self.commit("delete the slow test")
        self.assertEqual(self.decide()["reason"], "slow_test_source")

    def test_no_config_or_a_malformed_config_fails_closed(self):
        self.write(".config/slow-tier.toml", 'package = "rsid-store"\ntests = ["not a name!"]\ntrigger_paths = []\n')
        self.commit("bad config")
        self.assertEqual(self.decide()["mode"], "full")
        self.assertTrue(self.decide()["reason"].startswith("decision_error"))
        git(self.repo, "rm", "-q", ".config/slow-tier.toml")
        self.commit("no config")
        self.assertEqual(self.decide()["reason"], "no_slow_tier_config")

    def test_the_cli_prints_the_decision_for_the_lander(self):
        self.write("README.md", "more docs\n")
        self.commit("docs")
        result = subprocess.run(
            [sys.executable, str(CHECKER_PATH), "--slow-tier-only", "--repo", str(self.repo),
             "--base", self.base, "--head", "HEAD"], text=True, capture_output=True)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(json.loads(result.stdout)["mode"], "excluded")
        self.write("crates/rsid-store/src/store/migrations/v2.rs", "// v2\n")
        self.commit("migration")
        result = subprocess.run(
            [sys.executable, str(CHECKER_PATH), "--slow-tier-only", "--repo", str(self.repo),
             "--base", self.base, "--head", "HEAD"], text=True, capture_output=True)
        self.assertEqual(json.loads(result.stdout)["mode"], "full")


class SpanTest(unittest.TestCase):
    def test_spans_cover_attributes_and_bodies_of_test_fns_only(self):
        spans = CHECKER["test_fn_spans"](RUNNER_FIXTURE.splitlines())
        self.assertEqual([name for _, _, name in spans], ["fast_one", "slow_one"])
        first, last, _ = spans[1]
        lines = RUNNER_FIXTURE.splitlines()
        self.assertTrue(lines[first - 1].startswith("#[cfg(any("))
        self.assertEqual(lines[last - 1], "}")

    def test_a_layout_rustfmt_would_not_produce_is_refused(self):
        text = "#[test]\nfn a() {\n    1;\n  }\n\n#[test]\nfn b() {\n}\n"
        self.assertIsNone(CHECKER["test_fn_spans"](text.splitlines()))


class ConfigTest(unittest.TestCase):
    """The checked-in tier: the config, the generated filter and the tests it names."""

    @classmethod
    def setUpClass(cls):
        cls.config = CHECKER["parse_slow_tier"]((ROOT / ".config/slow-tier.toml").read_text())
        cls.nextest = tomllib.loads((ROOT / ".config/nextest.toml").read_text())["profile"]

    def test_the_default_filter_is_generated_from_the_config(self):
        self.assertEqual(
            self.nextest["default"]["default-filter"], CHECKER["slow_tier_filter"](self.config),
            "run: python3 scripts/check-touched-shards --write-slow-tier-filter")

    def test_the_full_profiles_run_every_test_and_the_gate_profiles_inherit_the_tier(self):
        for name in ("ci-full", "rsid-qa"):
            self.assertEqual(self.nextest[name]["default-filter"], "all()", name)
        for name in ("rsid-fast", "test-timings"):
            self.assertNotIn("default-filter", self.nextest[name], name)
            self.assertEqual(self.nextest[name]["inherits"], "default", name)

    def test_the_write_command_is_idempotent_on_the_checked_in_files(self):
        with scratch_tempdir(prefix="slow-tier-write-") as temp:
            root = Path(temp)
            (root / ".config").mkdir()
            for name in ("slow-tier.toml", "nextest.toml"):
                (root / ".config" / name).write_text((ROOT / ".config" / name).read_text())
            self.assertEqual(CHECKER["write_slow_tier_filter"](root), 0)
            self.assertEqual((root / ".config/nextest.toml").read_text(), (ROOT / ".config/nextest.toml").read_text())

    def test_every_named_test_is_a_test_fn_declared_exactly_once_in_the_store_sources(self):
        declared = {}
        for path in sorted((ROOT / "crates" / self.config["package"] / "src").rglob("*.rs")):
            lines = path.read_text().splitlines()
            spans = CHECKER["test_fn_spans"](lines)
            if spans is None:
                # An unusual layout: fall back to any `fn name`.
                spans = [(0, 0, m.group(1)) for line in lines if (m := re.match(r"\s*fn\s+(\w+)", line))]
            for _, _, name in spans:
                declared.setdefault(name, []).append(path.name)
        for name in self.config["tests"]:
            leaf = name.rsplit("::", 1)[-1]
            self.assertEqual(len(declared.get(leaf, [])), 1,
                             f"{name}: expected one #[test] fn {leaf} in the store sources, found {declared.get(leaf)}")

    def test_the_tier_names_the_migration_history_it_exists_for(self):
        tests = set(self.config["tests"])
        for name in (
            "store::tests::h1_rb_full_v84_graph_is_nonempty_rewindable_and_replayable",
            "store::tests::d05_v79_fresh_v77_v76_chain_failpoints_reopen_and_fingerprint",
            "store::tests::issue_v97_every_fault_rolls_back_and_repairs_fresh_copied_and_large_fixtures",
            "store::tests::v112_v113_convergence_tests::v112_identity_faults_roll_back_and_retry",
            "store::tests::v95_deployed_v94_v95_failpoints_roll_back_exactly_then_retry",
        ):
            self.assertIn(name, tests)
        # The current-behaviour tests next to them stay in the default tier.
        for name in ("store::tests::test_store_open_creates_schema", "store::tests::test_insert_and_load_session"):
            self.assertNotIn(name, tests)

    # #2298: tests that open the CURRENT schema and assert current behaviour. A version
    # number or the word "replay" in their names once put them in the tier; their subject is
    # not a rewound, upgraded or replayed historical schema, so a non-migration change to
    # what they observe must run them (the tier is excluded from every other diff).
    CURRENT_BEHAVIOUR = (
        "store::tests::closure_operator_same_key_replays_and_different_payload_refuses_without_effect",
        "store::tests::d02_idea_event_cas_tests::d02_idea_event_cas_two_file_connections_have_one_cas_winner",
        "store::tests::v96_tool_use_id_and_metadata_roundtrip",
        "store::tests::h1_v85_fresh_catalog_is_private_and_idempotent",
        "store::tests::h1_v85_request_key_grammar_and_claim_family_coupling_are_exhaustive",
        "store::tests::h1_v87_session_fence_finalize_requires_complete_terminal_bundle",
        "store::tests::h1_v87_session_fence_initial_bind_requires_exact_active_authority",
        "store::tests::h1_v87_session_fence_ordered_transactions_commit_or_rollback_atomically",
        "store::tests::h1_v87_session_fence_provider_live_writes_are_exact_next_sequence",
        "store::tests::h1_v88_static_state_machine_production_affinity_predicate_owners_are_exact",
        "store::tests::tag_default_empty_after_v43_upgrade",
        "store::tests::test_schema_migration_idempotent",
        "store::tests::test_v48_daemon_settings_table_created",
        "store::tests::v95_reopen_rejects_latest_run_pointer_catalog_drift",
    )

    def test_current_behaviour_tests_run_in_the_default_tier(self):
        tier = set(self.config["tests"])
        stayed = [name for name in self.CURRENT_BEHAVIOUR if name in tier]
        self.assertEqual(stayed, [], "current-behaviour tests belong to the default tier (#2298)")
        # The list must not go stale: each name is still defined in the store sources.
        source = "\n".join(path.read_text()
                           for path in (ROOT / "crates" / self.config["package"] / "src").rglob("*.rs"))
        for name in self.CURRENT_BEHAVIOUR:
            self.assertRegex(source, rf"\bfn {name.rsplit('::', 1)[-1]}\b", name)

    def test_every_trigger_path_exists(self):
        for trigger in self.config["trigger_paths"]:
            path = ROOT / trigger
            self.assertTrue(path.is_dir() if trigger.endswith("/") else path.is_file(), trigger)
        for required in ("crates/rsid-store/src/store/migrations/", "tools/released-migrations.json"):
            self.assertIn(required, self.config["trigger_paths"])

    def test_the_slow_tier_is_not_gitignored(self):
        ignored = subprocess.run(["git", "-C", str(ROOT), "check-ignore", "-q", ".config/slow-tier.toml"])
        self.assertEqual(ignored.returncode, 1, ".config/slow-tier.toml is ignored by .gitignore")


if __name__ == "__main__":
    unittest.main()
