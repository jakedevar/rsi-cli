import json
import os
import sys
import subprocess
import tempfile
import unittest
from pathlib import Path

sys.path.insert(0, os.path.dirname(__file__))
import check_starter_pins as csp  # noqa: E402

ROOT = Path(__file__).resolve().parents[1]


def digest(char):
    return "sha256:" + char * 64


SOURCE = """\
pub(crate) const ISSUE_PIPELINE: &str = "issue-pipeline";
pub(crate) const ISSUE_PIPELINE_REVIEWED: &str = "issue-pipeline-reviewed";
static STARTERS: [Starter; 2] = [
    Starter {
        id: Uuid::nil(),
        name: ISSUE_PIPELINE,
        definition: include_str!("starters/issue-pipeline.json"),
    },
    Starter {
        id: Uuid::nil(),
        name: ISSUE_PIPELINE_REVIEWED,
        definition: include_str!("starters/issue-pipeline-reviewed.json"),
    },
];
pub(crate) const SHIPPED_BEFORE: &[(&str, &[&str])] = &[
    (
        ISSUE_PIPELINE,
        &[
%(pipeline)s
        ],
    ),
    (
        ISSUE_PIPELINE_REVIEWED,
        &["%(reviewed)s"],
    ),
];

fn shipped_before() {}
"""

TESTS = """\
const ISSUE_PIPELINE_DIGEST: &str =
    "%(pipeline)s";
const REVIEWED_DIGEST: &str =
    "%(reviewed)s";
"""


def sources(pipeline_pin, pipeline_history, reviewed_pin="b", reviewed_history=("e",),
            pipeline_json=None, reviewed_json=None):
    history = "\n".join(f'            "{digest(c)}",' for c in pipeline_history)
    return csp.Sources(
        SOURCE % {"pipeline": history, "reviewed": digest(reviewed_history[0])},
        TESTS % {"pipeline": digest(pipeline_pin), "reviewed": digest(reviewed_pin)},
        {"issue-pipeline.json": json.dumps(pipeline_json or {"nodes": ["a"]}),
         "issue-pipeline-reviewed.json": json.dumps(reviewed_json or {"nodes": ["r"]})},
    )


class StarterPinGuard(unittest.TestCase):
    def test_a_consistent_tree_passes(self):
        self.assertEqual(csp.check(None, sources("a", ["1", "2"])), [])

    def test_retired_literals_are_refused_anywhere_outside_starters_source(self):
        head = sources("a", ["1"])
        with tempfile.TemporaryDirectory() as directory:
            repo = Path(directory)
            files = {csp.SOURCE_PATH: head.source, csp.TESTS_PATH: head.tests,
                     **{f"{csp.STARTERS_DIR}/starters/{name}": text
                        for name, text in head.jsons.items()}}
            path = f"{csp.STARTERS_DIR}/nested/issue_close_tests.rs"
            files[path] = f'// regression\nconst PIN: &str = "{digest("1")}";\n'
            for name, text in files.items():
                target = repo / name
                target.parent.mkdir(parents=True, exist_ok=True)
                target.write_text(text)
            subprocess.run(["git", "init", "-q", str(repo)], check=True)
            subprocess.run(["git", "-C", str(repo), "add", *files], check=True)
            subprocess.run(["git", "-C", str(repo), "-c", "user.name=fixture",
                            "-c", "user.email=fixture@example.invalid", "-c", "core.hooksPath=/dev/null",
                            "commit", "-qm", "fixture"], check=True)
            for loaded in (csp.load_tree(repo), csp.load_ref(repo, "HEAD")):
                errors = csp.check(None, loaded)
                self.assertEqual(len(errors), 2, errors)
                self.assertIn(f"{path}:2: retired starter digest literal {digest('1')}", errors[0])
                self.assertIn("shared current fixture", errors[0])
                loaded.topology_texts[path] = f'const PIN: &str = "{digest("a")}";'
                self.assertEqual(csp.check(None, loaded), [])

    def test_the_repository_starters_are_consistent(self):
        self.assertEqual(csp.check(None, csp.load_tree(ROOT)), [])
        self.assertEqual(csp.main(["--tree"], repo=ROOT), 0)

    def test_every_repository_starter_has_a_pin_site(self):
        starters = csp.load_tree(ROOT).parse()
        self.assertTrue(starters)
        for const, starter in starters.items():
            self.assertIn(const, csp.PIN_SITES)
            self.assertIsNotNone(starter.pin, starter.name)
            self.assertIsNotNone(starter.history, starter.name)

    def test_a_changed_starter_with_both_pins_untouched_is_refused(self):
        # #3045: the JSON moved, the fixture and SHIPPED_BEFORE did not.
        base = sources("a", ["1"])
        head = sources("a", ["1"], pipeline_json={"nodes": ["a", "b"]})
        errors = csp.check(base, head)
        self.assertTrue(any("fixture still pins " + digest("a") in e for e in errors), errors)
        self.assertTrue(any("lacks the digest it replaced, " + digest("a") in e for e in errors), errors)
        self.assertEqual(errors[-1], csp.HOW_TO_MOVE)

    def test_a_moved_fixture_without_the_replaced_digest_is_refused(self):
        base = sources("a", ["1"])
        head = sources("c", ["1"], pipeline_json={"nodes": ["a", "b"]})
        errors = csp.check(base, head)
        self.assertEqual(len([e for e in errors if "lacks the digest it replaced" in e]), 1, errors)
        self.assertFalse(any("fixture still pins" in e for e in errors), errors)

    def test_a_replaced_digest_appended_without_moving_the_fixture_is_refused(self):
        base = sources("a", ["1"])
        head = sources("a", ["1", "a"], pipeline_json={"nodes": ["a", "b"]})
        errors = csp.check(base, head)
        self.assertTrue(any("lists as a definition it replaced" in e for e in errors), errors)
        self.assertTrue(any("fixture still pins" in e for e in errors), errors)

    def test_moving_both_pins_together_passes(self):
        base = sources("a", ["1"])
        head = sources("c", ["1", "a"], pipeline_json={"nodes": ["a", "b"]})
        self.assertEqual(csp.check(base, head), [])

    def test_reformatting_or_reordering_the_json_is_not_a_change(self):
        base = csp.Sources(*dataclass_like(sources("a", ["1"])))
        head = sources("a", ["1"])
        head.jsons["issue-pipeline.json"] = json.dumps({"nodes": ["a"]}, indent=4)
        base.jsons["issue-pipeline.json"] = json.dumps({"nodes": ["a"]})
        self.assertEqual(csp.check(base, head), [])
        base.jsons["issue-pipeline.json"] = '{"b": 1, "a": 2}'
        head.jsons["issue-pipeline.json"] = '{"a": 2,\n "b": 1}'
        self.assertEqual(csp.check(base, head), [])

    def test_only_the_changed_starter_is_judged(self):
        base = sources("a", ["1"])
        head = sources("c", ["1", "a"], pipeline_json={"nodes": ["a", "b"]})
        self.assertEqual(csp.check(base, head), [])
        stale = sources("a", ["1"], reviewed_json={"nodes": ["r", "s"]})
        errors = csp.check(base, stale)
        self.assertTrue(errors and all("issue-pipeline-reviewed" in e or e == csp.HOW_TO_MOVE
                                       for e in errors), errors)

    def test_history_is_append_only_and_unique(self):
        base = sources("a", ["1", "2"])
        errors = csp.check(base, sources("a", ["1"]))
        self.assertTrue(any("dropped " + digest("2") in e for e in errors), errors)
        errors = csp.check(None, sources("a", ["1", "1"]))
        self.assertTrue(any("repeats a digest" in e for e in errors), errors)

    def test_a_starter_without_a_pin_site_or_history_is_refused(self):
        head = sources("a", ["1"])
        head.source = head.source.replace("ISSUE_PIPELINE_REVIEWED,\n        &[", "ISSUE_UNKNOWN,\n        &[")
        errors = csp.check(None, head)
        self.assertTrue(any("no SHIPPED_BEFORE entry" in e for e in errors), errors)
        head = sources("a", ["1"])
        head.tests = head.tests.replace("REVIEWED_DIGEST", "SOMETHING_ELSE")
        errors = csp.check(None, head)
        self.assertTrue(any("digest fixture is missing" in e for e in errors), errors)
        head = sources("a", ["1"])
        head.source = head.source.replace("pub(crate) const ISSUE_PIPELINE_REVIEWED", "pub(crate) const ISSUE_NEW") \
            .replace("name: ISSUE_PIPELINE_REVIEWED,", "name: ISSUE_NEW,")
        errors = csp.check(None, head)
        self.assertTrue(any("no pin site" in e for e in errors), errors)

    def test_a_tree_without_starters_has_nothing_to_check(self):
        self.assertEqual(csp.check(None, csp.Sources(None, None, {})), [])
        self.assertEqual(csp.check(csp.Sources(None, None, {}), csp.Sources(None, None, {})), [])


def dataclass_like(sources_):
    return sources_.source, sources_.tests, dict(sources_.jsons)


if __name__ == "__main__":
    unittest.main()
