"""Unresolved Rust diagnostics make mixed panic/signal runs counted reds (#570)."""

import contextlib
import io
import json
from pathlib import Path
import runpy
import unittest
from unittest.mock import patch

from scripts.tests import test_scoped_test as fixtures


SCRIPTS = Path(__file__).resolve().parents[1]
FAILURE = runpy.run_path(str(SCRIPTS / "scoped-test-failure.py"))
RUNNER = runpy.run_path(str(SCRIPTS / "scoped-test"))
PANIC_SIGNAL = (Path(__file__).with_name("fixtures") / "scoped-test-failure"
                / "panic-then-signal.txt").read_text()


class PanicSignalTest(unittest.TestCase):
    def test_unresolved_diagnostics_veto_signal_only(self):
        cases = [PANIC_SIGNAL,
                 "thread 'tests::panic' panicked at src/lib.rs:1:1:\n",
                 "test tests::assertion ... assertion failed: value.is_ok()\n",
                 "test tests::assertion ... assertion `left != right` failed\n",
                 "assertion failed: value.is_ok()\n"]
        for diagnostic in cases:
            with self.subTest(diagnostic=diagnostic):
                self.assertFalse(FAILURE["signal_only"](
                    (diagnostic + "SCOPED_PROCESS_SIGNAL SIGTERM\n").splitlines()))

    def test_only_an_ok_verdict_resolves_a_panic(self):
        expected = ("test tests::expected - should panic ... \n"
                    "thread 'tests::expected' panicked at expected panic\n")
        for verdict, interrupted in (("ok\n", True), ("SCOPED_END tests::expected\n", False),
                                     ("test tests::other ... ok\n", False), ("", False)):
            with self.subTest(verdict=verdict):
                self.assertEqual(FAILURE["signal_only"](
                    (expected + verdict + "test tests::later ... \n"
                     "SCOPED_PROCESS_SIGNAL SIGTERM\n").splitlines()), interrupted)
        # A previous passing invocation cannot hide a new unresolved panic.
        self.assertFalse(FAILURE["signal_only"](
            ("test store::assertion ... ok\n" + PANIC_SIGNAL).splitlines()))

    def test_run_batch_counts_named_and_unattributed_diagnostics(self):
        for diagnostic, name in ((PANIC_SIGNAL, "store::assertion"),
                                 ("assertion failed: false\n", "store::assertion")):
            with self.subTest(diagnostic=diagnostic):
                def stream(_argv, emit, **_kwargs):
                    for line in diagnostic.splitlines(keepends=True):
                        emit(line)
                    return -15

                with patch.dict(RUNNER["run_batch"].__globals__, stream=stream), \
                     contextlib.redirect_stdout(io.StringIO()):
                    ran, code, failed, done = RUNNER["run_batch"](
                        "unused", ["store::assertion"], None, {})
                self.assertEqual((ran, code, failed, done),
                                 (1, -15, [name], {"store::assertion"}))
        evidence = FAILURE["PanicEvidence"]()
        evidence.line("assertion failed: false")
        self.assertEqual(evidence.unresolved, {evidence.UNKNOWN})

    def test_successful_binary_keeps_failure_looking_output_a_pass(self):
        def stream(_argv, emit, **_kwargs):
            for line in PANIC_SIGNAL.splitlines(keepends=True):
                emit(line)
            return 0

        with patch.dict(RUNNER["run_batch"].__globals__, stream=stream), \
             contextlib.redirect_stdout(io.StringIO()):
            ran, code, failed, _done = RUNNER["run_batch"](
                "unused", ["store::assertion"], None, {})
        self.assertEqual((ran, code, failed), (1, 0, []))

    def test_end_to_end_receipts_for_mixed_expected_and_plain_signals(self):
        for mode in ("panic", "assertion", "expected", "plain"):
            with self.subTest(mode=mode):
                fixture = fixtures.ScopedTest(methodName="runTest")
                fixture.setUp()
                try:
                    fixture.binary = fixture.executable("test-binary", f"""
import signal
if '--list' in sys.argv:
    print('store::assertion: test')
    print('store::later: test')
else:
    suffix = ' - should panic' if {mode!r} == 'expected' else ''
    print('test store::assertion' + suffix + ' ... ', flush=True)
    if {mode!r} in ('panic', 'expected'):
        print("thread 'store::assertion' (123) panicked at src/store.rs:42:9:", flush=True)
    if {mode!r} == 'assertion':
        print('assertion `left == right` failed\\n  left: 1\\n right: 2', flush=True)
    if {mode!r} == 'expected':
        print('ok\\ntest store::later ... ', flush=True)
    os.kill(os.getpid(), signal.SIGTERM)
""")
                    code, output = fixture.invoke(linux=False, entrypoint=True,
                                                  filters=["rsid-store=store::"])
                    receipt = json.loads(output.splitlines()[-1].split(" ", 1)[1])
                    self.assertEqual((code, receipt["ok"]), (1, False), output)
                    mixed = mode in ("panic", "assertion")
                    self.assertEqual(receipt.get("failure_class") == "infrastructure_interrupted",
                                     not mixed, output)
                    failures = [{"package": "rsid-store", "kind": "lib", "target": "rsid_store",
                                 "test": "store::assertion"}] if mixed else []
                    self.assertEqual(receipt["failures"], failures)
                    self.assertEqual(receipt["packages"][0]["failed_tests"], failures)
                    if mixed:
                        self.assertEqual(receipt["packages"][0]["completed"], 1)
                        log = Path(receipt["packages"][0]["log"]).read_text()
                        self.assertIn("SCOPED_FAILURE lib:rsid_store store::assertion", log)
                finally:
                    fixture.doCleanups()


if __name__ == "__main__":
    unittest.main()
