import contextlib
import importlib.machinery
import importlib.util
import io
import json
from pathlib import Path
import subprocess
import tempfile
from types import SimpleNamespace
import unittest
from unittest.mock import patch


loader = importlib.machinery.SourceFileLoader(
    "qa_resume", str(Path(__file__).resolve().parents[1] / "qa-resume")
)
spec = importlib.util.spec_from_loader(loader.name, loader)
qa = importlib.util.module_from_spec(spec)
loader.exec_module(qa)


def artifact(kind, name, package_id="p", test=True):
    return json.dumps({
        "reason": "compiler-artifact", "package_id": package_id,
        "profile": {"test": test}, "target": {"kind": [kind], "name": name},
        "executable": "libexe" if kind == "lib" else name,
    })


class FakeRunner:
    def __init__(self, names, failure=None):
        self.names = names
        self.calls = []
        self.failure = failure or (lambda argv: False)
        self.omit_integration = False

    def run(self, argv, capture=False):
        self.calls.append(argv)
        output = ""
        if "metadata" in argv:
            output = json.dumps({"packages": [{"name": "rsid", "id": "p", "targets": [
                {"name": "rsid", "kind": ["lib"], "test": True},
                {"name": "tool", "kind": ["bin"], "test": True},
                {"name": "collision", "kind": ["test"], "test": True},
                {"name": "production-only", "kind": ["bin"], "test": False},
                {"name": "dev-only", "kind": ["bin"], "test": True,
                 "required-features": ["dev-fixtures"]},
            ]}]})
        elif "--no-run" in argv:
            output = "\n".join([
                artifact("lib", "rsid"), artifact("bin", "tool"),
                artifact("lib", "dependency", "other"),
                artifact("bin", "production-only", test=False),
            ] + ([] if self.omit_integration else [artifact("test", "collision")]))
        elif argv == ["libexe", "--list"]:
            output = "\n".join(name + ": test" for name in self.names)
        return SimpleNamespace(returncode=101 if self.failure(argv) else 0, stdout=output)


class QaResumeTests(unittest.TestCase):
    def run_resume(self, runner, completed):
        with contextlib.redirect_stdout(io.StringIO()):
            return qa.resume(runner, "rsid", completed)

    def test_prior_failures_are_isolated_and_completed_passes_are_skipped(self):
        runner = FakeRunner(["tests::done", "tests::bad1", "tests::bad2", "tests::new"])
        self.assertEqual(self.run_resume(runner, {
            "tests::done": "ok", "tests::bad1": "FAILED", "tests::bad2": "FAILED",
        }), 0)
        lib_calls = [argv for argv in runner.calls if "--lib" in argv]
        self.assertEqual([argv[argv.index("--") + 1:] for argv in lib_calls], [
            ["tests::bad1", "--exact"], ["tests::bad2", "--exact"], ["tests::new", "--exact"],
        ])
        # A nonlib target containing the same test name as a completed library
        # test still runs in full, without a substring filter on the library.
        self.assertTrue(any("--test" in argv and "collision" in argv for argv in runner.calls))
        self.assertEqual(sum("--doc" in argv for argv in runner.calls), 1)

    def test_failure_does_not_hide_later_targets_or_doctests(self):
        runner = FakeRunner(["tests::bad"], lambda argv: "--lib" in argv or "--bin" in argv)
        self.assertEqual(self.run_resume(runner, {"tests::bad": "FAILED"}), 101)
        self.assertTrue(any("--test" in argv for argv in runner.calls))
        self.assertIn("--doc", runner.calls[-1])

    def test_complete_library_never_gets_an_empty_filter_invocation(self):
        runner = FakeRunner(["tests::done"])
        self.assertEqual(self.run_resume(runner, {"tests::done": "ok"}), 0)
        self.assertEqual([argv for argv in runner.calls if "--lib" in argv], [])

    def test_mismatched_inventory_refuses_execution(self):
        runner = FakeRunner(["tests::actual"])
        with self.assertRaisesRegex(ValueError, "inventory"):
            self.run_resume(runner, {"tests::stale": "ok"})
        self.assertEqual([argv for argv in runner.calls if "--no-fail-fast" in argv], [])

    def test_missing_target_artifact_never_claims_complete_coverage(self):
        runner = FakeRunner(["tests::new"])
        runner.omit_integration = True
        with self.assertRaisesRegex(ValueError, "target inventory"):
            self.run_resume(runner, {})
        self.assertEqual([argv for argv in runner.calls if "--no-fail-fast" in argv], [])

    def test_completed_nonlib_and_doc_targets_are_retained_without_rerunning(self):
        runner = FakeRunner(["tests::done"])
        with contextlib.redirect_stdout(io.StringIO()):
            code = qa.resume(runner, "rsid", {"tests::done": "ok"}, {
                ("bin", "tool"): 101, ("doc", "rsid"): 0,
            })
        self.assertEqual(code, 101)
        self.assertEqual([argv for argv in runner.calls if "--bin" in argv or "--doc" in argv], [])
        self.assertTrue(any("--test" in argv for argv in runner.calls))

    def test_completed_isolated_retry_is_never_replayed(self):
        runner = FakeRunner(["tests::bad", "tests::new"])
        with contextlib.redirect_stdout(io.StringIO()):
            code = qa.resume(runner, "rsid", {"tests::bad": "FAILED"}, {("isolated", "tests::bad"): 101})
        self.assertEqual(code, 101)
        lib_calls = [argv for argv in runner.calls if "--lib" in argv]
        self.assertEqual([argv[argv.index("--") + 1:] for argv in lib_calls], [["tests::new", "--exact"]])

    def test_exact_batches_fit_portable_argv_budget_and_preserve_every_name(self):
        names = ["tests::" + "a" * 170 + str(i) for i in range(500)]
        chunks = list(qa.batches(names))
        self.assertGreater(len(chunks), 1)
        self.assertEqual([name for chunk in chunks for name in chunk], names)
        self.assertTrue(all(sum(len(name) + 4 for name in chunk) <= 20000 for chunk in chunks))
        with self.assertRaises(ValueError):
            list(qa.batches(["--ignored"]))

    def test_wrong_source_pin_refuses_before_any_mutation(self):
        with tempfile.TemporaryDirectory() as directory:
            repo = Path(directory)
            worktree = repo / "qa"
            worktree.mkdir()
            with patch.object(qa.subprocess, "check_output", side_effect=["common", "common", "b" * 40]), \
                    patch.object(qa.subprocess, "run") as process:
                with self.assertRaisesRegex(ValueError, "pin changed"):
                    qa.check_source(repo, worktree, "a" * 40)
                process.assert_not_called()

    def test_expired_budget_starts_no_process(self):
        runner = qa.Runner(Path("."), -1)
        with patch.object(qa.subprocess, "run") as process:
            with self.assertRaises(TimeoutError):
                runner.run(["cargo", "test"])
            process.assert_not_called()


if __name__ == "__main__":
    unittest.main()
