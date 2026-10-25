"""Bounded runner checks with stub builds and an optional real systemd stop."""

import contextlib
from datetime import datetime, timedelta, timezone
import hashlib
import io
import json
import re
import os
from pathlib import Path
import runpy
import shutil
import signal
import subprocess
import sys
import tempfile
import time
import unittest
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
from scratch_git import scratch_git, scratch_tempdir  # noqa: E402

SCRIPT = Path(__file__).resolve().parents[1] / "scoped-test"
# #3478: a run probes and uses a PID namespace unless this is off. These tests pin the plain
# argv of every lane; the namespace tests below turn the probe on explicitly.
os.environ["RSI_PID_ISOLATION"] = "off"
RUNNER = runpy.run_path(str(SCRIPT))


def block_quota(path):
    """(hard limit, used) bytes of this user's block quota on `path`, or None."""
    import ctypes
    import struct
    try:
        fd = os.open(path, os.O_RDONLY | os.O_DIRECTORY)
    except OSError:
        return None
    try:
        dqblk = ctypes.create_string_buffer(72)
        libc = ctypes.CDLL(None, use_errno=True)
        if libc.syscall(443, fd, ctypes.c_uint((0x800007 << 8) | 0), ctypes.c_uint(os.geteuid()), dqblk) != 0:
            return None
    finally:
        os.close(fd)
    hard_kib, _soft, used = struct.unpack_from("=QQQ", dqblk.raw)
    return (hard_kib * 1024, used) if hard_kib else None


class ControlledClock:
    """A monotonic clock a fixture advances itself, so a test-time box expires by what the fake batches
    "cost" and never by how fast the host schedules the listing subprocesses (#2624). Everything but
    `monotonic` is the real `time` module's."""

    def __init__(self):
        self.now = 0.0

    def monotonic(self):
        return self.now

    def advance(self, seconds):
        self.now += seconds

    def __getattr__(self, name):
        return getattr(time, name)


class ReceiptEncodingTest(unittest.TestCase):
    def test_large_selection_keeps_progress_and_terminal_receipts_below_64_kib(self):
        filters = [f"rsid=shard:store-01:test(module_{n}::dependent_tests)" for n in range(7000)]
        entries = [{"package": "rsid", "module": f"module_{n}", "filter": value,
                    "path": f"crates/rsid/src/module_{n}.rs", "reason": "public library dependency"}
                   for n, value in enumerate(filters)]
        downstream = {"items": entries, "selections": entries, "filters": filters, "notes": [],
                      "changed_libraries": [{"package": "rsi-common", "paths": ["src/lib.rs"]}],
                      "packages": [{"package": "rsid", "status": "pending"}]}
        selection = {"filters": filters, "downstream_compile": downstream,
                     "downstream_tests": {key: downstream[key] for key in ("items", "selections", "filters", "notes")}}
        # Terminal fields read by agent_jobs.rs and rsi-common/src/agent_landing_candidate.rs.
        # The Rust parser accepts a JSON object, with no fixed deserialization schema.
        failure = {"package": "rsid", "kind": "lib", "target": "rsid", "test": "existing_red"}
        evidence = {"head": "a" * 40, "failure_class": "test_failure", "failures": [failure],
                    "baseline_reds": [failure], "new_failures": [], "plan": "targeted_subset",
                    "uncertain": "broad_observers", "stale_base": "base note", "gate_covers_rest": True,
                    "selected_tests": 7000, "run_tests": 100, "merges_cleanly_onto_rolling": False,
                    "conflict_paths": ["scripts/scoped-test"],
                    "tool": {"source": "rolling", "rev": "b" * 40, "blob": "c" * 40}}
        with scratch_tempdir(prefix="scoped-receipt-") as tmp, patch.dict(RUNNER["RECEIPT"], clear=True):
            receipt = RUNNER["RECEIPT"]
            receipt.update(schema=1, emit=True, packages=[], **selection, **evidence)
            RUNNER["persist_selection"](Path(tmp))
            path = Path(receipt["selection"]["path"])
            saved = path.read_bytes()
            self.assertEqual(json.loads(saved), selection)
            self.assertEqual(receipt["selection"]["sha256"], hashlib.sha256(saved).hexdigest())
            with patch.object(Path, "write_bytes", side_effect=AssertionError("selection must be written once")), \
                 contextlib.redirect_stdout(io.StringIO()) as out:
                for phase in ("building", "listed", "testing", "test_failed"):
                    RUNNER["snapshot_package"]({"package": "rsid", "status": "running", "phase": phase,
                                                "completed": 100})
                downstream["packages"][0]["status"] = "passed"
                receipt["packages"] = [{"package": "rsid", "status": "passed", "completed": 100}]
                RUNNER["emit_receipt"](0)
            lines = out.getvalue().splitlines()
            self.assertEqual(len(lines), 5)
            for line in lines:
                self.assertLess(len(line.encode()) + 1, 64 * 1024)
            parsed = [json.loads(line.removeprefix(RUNNER["RECEIPT_PREFIX"])) for line in lines]
            self.assertEqual([item["packages"][0]["phase"] for item in parsed[:-1]],
                             ["building", "listed", "testing", "test_failed"])
            self.assertTrue(all(item["partial"] and not item["ok"] for item in parsed[:-1]))
            terminal = parsed[-1]
            self.assertEqual((terminal["ok"], terminal["exit_code"]), (True, 0))
            self.assertEqual({key: terminal[key] for key in evidence}, evidence)
            self.assertEqual(terminal["packages"], receipt["packages"])
            self.assertEqual(terminal["filters"]["count"], 7000)
            self.assertEqual(terminal["downstream_tests"]["counts"]["selections"], 7000)
            self.assertEqual(terminal["downstream_compile"]["packages"][0]["status"], "passed")
            for key in selection:
                summary = terminal[key]
                self.assertEqual(summary["detail_path"], str(path))
                self.assertEqual(summary["sha256"],
                                 hashlib.sha256(RUNNER["json_bytes"](json.loads(saved)[key])).hexdigest())
            self.assertEqual(path.read_bytes(), saved)
            self.assertEqual(receipt["filters"], filters)

    def test_progress_cannot_be_green_even_after_baseline_verdict(self):
        with patch.dict(RUNNER["RECEIPT"], {"emit": True, "packages": [],
                                           "ok_reason": "baseline_known_failure"}, clear=True), \
             contextlib.redirect_stdout(io.StringIO()) as out:
            RUNNER["emit_receipt"](1, progress=True)
        receipt = json.loads(out.getvalue().removeprefix(RUNNER["RECEIPT_PREFIX"]))
        self.assertEqual((receipt["ok"], receipt["partial"]), (False, True))


class ScopedTest(unittest.TestCase):
    def setUp(self):
        # These fixtures execute stub binaries. Keep them on disk even when the
        # runner supplies a tmpfs TMPDIR (which may be mounted noexec).
        disk_parent = "/var/tmp" if sys.platform.startswith("linux") and Path("/var/tmp").is_dir() else None
        self.temp = tempfile.TemporaryDirectory(prefix="scoped-test-unit-", dir=disk_parent)
        self.addCleanup(self.temp.cleanup)
        # The tmpfs room floor (#2177) is a host property; fixtures that name a scratch parent
        # on a small disk must not depend on it. Tests of the floor set it themselves.
        floor = patch.dict(os.environ, RSI_SCOPED_TEST_TMPFS_MIN_GB="0")
        floor.start()
        self.addCleanup(floor.stop)
        # Resolve once: macOS's default TMPDIR (/var/folders) is a symlink to /private/var/folders,
        # and the script reports resolved paths (hub #2994).
        self.root = Path(self.temp.name).resolve()
        self.log = self.root / "commands.log"
        self.binary = self.executable("test-binary", '''
import sys
print('store::migration: test' if '--list' in sys.argv else 'test result: ok. 1 passed')
''')
        artifact = {"reason": "compiler-artifact", "profile": {"test": True},
                    "manifest_path": str(self.root / "Cargo.toml"),
                    "executable": str(self.binary), "target": {"kind": ["lib"], "name": "rsid_store"}}
        self.cargo = self.executable("cargo-stub", f"print({json.dumps(json.dumps(artifact))})\n")
        self.environment = patch.dict(os.environ, SCOPED_TEST_LOG=str(self.log), RSI_SCOPED_TEST_RUNNER="libtest")
        self.environment.start()
        # A recipe job exports these; the tests set their own (#1756).
        for name in ("RSI_SCOPED_TEST_FILTERS", "RSI_CARGO_SLOT_HELD"):
            os.environ.pop(name, None)
        self.addCleanup(self.environment.stop)

    def scratch_repo(self):
        """An empty directory for a fixture repository, outside every protected root.

        `self.root` stays on the disk directory the stub binaries need; fixture Git runs
        only through `scratch_git` in a directory `scratch_tempdir` allocated."""
        temp = scratch_tempdir(prefix="scoped-test-repo-")
        self.addCleanup(temp.cleanup)
        repo = Path(temp.name) / "repo"
        repo.mkdir()
        return repo

    def executable(self, name, body):
        path = self.root / name
        # The fakes name no ignored tests: `--list --ignored` (#2273) prints nothing and is not logged.
        path.write_text(f"#!{sys.executable}\nimport os, sys\nfrom pathlib import Path\n"
                        "if '--ignored' in sys.argv:\n    sys.exit(0)\n"
                        "with Path(os.environ['SCOPED_TEST_LOG']).open('a') as log:\n"
                        "    log.write(Path(sys.argv[0]).name + ' ' + ' '.join(sys.argv[1:]) + '\\n')\n" + body)
        path.chmod(0o755)
        return path

    # What `git rev-parse`/`git show` answer for the warning gate's base lookup (#1897): None is a
    # base that resolves but has no baseline file, a dict is the baseline document there.
    base_baseline = None

    def baseline_git(self, args):
        if args[:3] == ["rev-parse", "--verify", "--quiet"]:
            return subprocess.CompletedProcess(args, 1 if self.base_baseline == "unresolved" else 0, "", "")
        if args[0] == "merge-tree":
            # This fixture has no Git history. Its warning-baseline JSON is not a trial tree;
            # real trial merges and recent-deletion receipts have separate Git fixtures.
            return subprocess.CompletedProcess(args, 128, "", "not a Git history fixture")
        document = self.base_baseline if isinstance(self.base_baseline, dict) else None
        return subprocess.CompletedProcess(args, 0 if document else 1, json.dumps(document) if document else "", "")

    def plan(self):
        selections = RUNNER["selection_plan"]([
            "rsid=shard:store-01:test(store::migration)",
            "rsid=shard:store-01:test(migration)"], {"store-01": ["rsid-store"]})["rsid-store"]
        return {"repo": str(self.root), "package": "rsid-store", "selections": selections,
                "build": RUNNER["build_command"]("rsid-store", selections, [str(self.cargo)])}

    def test_downstream_compile_builds_test_targets_without_listing_or_running_them(self):
        plan = self.plan()
        plan.update(compile_only=True, selections=[],
                    build=RUNNER["build_command"]("rsid-store", [], [str(self.cargo)],
                                                   ["dev-fixtures"], all_tests=True))
        output = io.StringIO()
        with contextlib.chdir(self.root), contextlib.redirect_stdout(output):
            code = RUNNER["execute_package"](plan)
        self.assertEqual(code, 0, output.getvalue())
        self.assertEqual(self.log.read_text().splitlines(), [
            "cargo-stub test -p rsid-store --tests --features dev-fixtures --no-run --message-format=json"])
        self.assertIn("SCOPED_PHASE compiled", output.getvalue())
        self.assertIn("SCOPED_COMPLETE rsid-store 0", output.getvalue())

    def test_downstream_compile_failure_is_red(self):
        cargo = self.executable("broken-cargo", "print('error[E0308]: mismatched types')\nsys.exit(101)\n")
        plan = self.plan()
        plan.update(compile_only=True, selections=[], build=[str(cargo), "test", "--tests", "--no-run"])
        output = io.StringIO()
        with contextlib.chdir(self.root), contextlib.redirect_stdout(output):
            code = RUNNER["execute_package"](plan)
        self.assertEqual(code, 101)
        self.assertIn("SCOPED_PHASE build_failed", output.getvalue())
        self.assertIn("E0308", output.getvalue())

    def test_bounded_unit_builds_with_the_worker_shell_build_settings(self):
        # #1741: the systemd unit must build like the worker's shell, or it
        # writes a second copy of every artifact (another profile) and cannot
        # rebuild in a target seeded from the warm pool (no RUSTC_WRAPPER).
        shell = {"CARGO_TARGET_DIR": "/s/target", "CARGO_INCREMENTAL": "0",
                 "CARGO_PROFILE_DEV_DEBUG": "line-tables-only",
                 "RUSTC_WRAPPER": "/w/rsi-build-rustc", "RSI_BUILD_SCCACHE": "/usr/bin/sccache",
                 "SCCACHE_DIR": "/c", "SCCACHE_SERVER_UDS": "/c.sock", "SCCACHE_CLIENT_SIDE": "1",
                 "RSI_BUILD_SLOTS": "12", "RSI_BUILD_SLOT_ROOT": "/slots",
                 "SCCACHE_CACHE_SIZE": "50G", "RSI_SESSION_TOKEN": "secret"}
        with patch.dict(os.environ, shell):
            command, _unit = RUNNER["bounded_command"]({"repo": "/s"}, Path("/p"), 60, 200, True)
        for name, value in shell.items():
            if name == "RSI_SESSION_TOKEN":
                continue
            self.assertIn(f"--setenv={name}", command)
        self.assertEqual([arg for arg in command if "RSI_SESSION_TOKEN" in arg], [])

    def test_bounded_unit_runs_in_the_hub_build_slice(self):
        # #2119: every cargo build and test unit shares one CPU budget.
        with patch.dict(os.environ):
            os.environ.pop("RSI_BUILD_SLICE", None)
            command, unit = RUNNER["bounded_command"]({"repo": "/s"}, Path("/p"), 60, 200, True)
            self.assertIn("--slice=rsi-build.slice", command)
            self.assertIn(f"--unit={unit}", command)
            self.assertIn("CPUQuota=200%", command)
            # Off Linux there is no unit and no slice: the worker runs directly.
            plain, plain_unit = RUNNER["bounded_command"]({"repo": "/s"}, Path("/p"), 60, 200, False)
            self.assertIsNone(plain_unit)
            self.assertFalse([arg for arg in plain if arg.startswith("--slice")])
        # The name is overridable and an empty override opts out.
        with patch.dict(os.environ, {"RSI_BUILD_SLICE": "rsi-build-other.slice"}):
            other, _ = RUNNER["bounded_command"]({"repo": "/s"}, Path("/p"), 60, 200, True)
        self.assertIn("--slice=rsi-build-other.slice", other)
        with patch.dict(os.environ, {"RSI_BUILD_SLICE": ""}):
            outside, _ = RUNNER["bounded_command"]({"repo": "/s"}, Path("/p"), 60, 200, True)
        self.assertEqual([arg for arg in outside if arg.startswith("--slice")], [])

    def test_slice_budget_reads_the_live_build_slice_quota(self):
        # #2524: nested units are bounded by min(job ceiling, host build budget).
        from types import SimpleNamespace

        def shown(text, code=0):
            calls = []

            def run(argv, **_kwargs):
                calls.append(argv)
                return SimpleNamespace(stdout=text, returncode=code)
            return run, calls

        budget = RUNNER["slice_budget_percent"]
        with patch.dict(os.environ):
            os.environ.pop("RSI_BUILD_SLICE", None)
            run, calls = shown("8s\n")
            self.assertEqual(budget(True, run), 800.0)
            self.assertEqual(calls[0][:4], ["systemctl", "--user", "show", "rsi-build.slice"])
            self.assertEqual(budget(True, shown("1.5s")[0]), 150.0)
            self.assertEqual(budget(True, shown("500ms")[0]), 50.0)
            # No quota, a failed read, other platforms and the opt-out add no bound.
            self.assertIsNone(budget(True, shown("infinity")[0]))
            self.assertIsNone(budget(True, shown("", 1)[0]))
            self.assertIsNone(budget(False, shown("8s")[0]))
            self.assertIsNone(budget(True, lambda *_a, **_k: (_ for _ in ()).throw(OSError("no systemctl"))))
        with patch.dict(os.environ, {"RSI_BUILD_SLICE": ""}):
            self.assertIsNone(budget(True, shown("8s")[0]))

    def test_a_job_ceiling_above_the_build_slice_budget_is_lowered_to_it(self):
        # #2524: the nested units get min(job ceiling, host build budget).
        self.slice_quota_text = "2s\n"
        try:
            code, output = self.invoke(seconds="5", entrypoint=True, extra_args=("--cpu-quota", "400"))
        finally:
            del self.slice_quota_text
        self.assertEqual(code, 0, output)
        self.assertIn("exceeds the rsi-build.slice budget 200%", output)
        receipt = json.loads(output.splitlines()[-1].split(" ", 1)[1])
        self.assertEqual(receipt["cpu_quota"], 200)

    def test_bounded_unit_name_carries_the_daemon_job_id(self):
        # #2127: the CPU andon attributes the nested unit to its job by name.
        job = "00000000-0000-0000-0000-00000000002a"
        with patch.dict(os.environ, {"RSI_JOB_ID": job}):
            _command, unit = RUNNER["bounded_command"]({"repo": "/s"}, Path("/p"), 60, 200, True)
        self.assertRegex(unit, rf"^rsi-scoped-test-{job}-[0-9a-f]{{32}}$")
        # An operator run (no job id) or a malformed one keeps the plain name.
        for value in (None, "", "not-a-uuid", job + "; rm -rf /"):
            with patch.dict(os.environ):
                os.environ.pop("RSI_JOB_ID", None)
                if value is not None:
                    os.environ["RSI_JOB_ID"] = value
                _command, plain = RUNNER["bounded_command"]({"repo": "/s"}, Path("/p"), 60, 200, True)
            self.assertRegex(plain, r"^rsi-scoped-test-[0-9a-f]{32}$")

    def test_test_lanes_run_in_the_pid_namespace_the_run_probed(self):
        # #3478: the bounded cargo worker and the Python lane run inside `unshare --pid`, so a
        # test's host-wide `pgrep`/`kill` cannot reach the hub daemon.
        from types import SimpleNamespace

        namespaced = {"mode": "namespaced", "reason": "", "mapping": "current_user"}
        globals_ = RUNNER["bounded_command"].__globals__
        with patch.dict(os.environ):
            os.environ.pop("RSI_BUILD_SLICE", None)
            with patch.dict(globals_, PID_ISOLATION=None):
                plain, _ = RUNNER["bounded_command"]({"repo": "/s"}, Path("/p"), 60, 200, True)
            with patch.dict(globals_, PID_ISOLATION=namespaced):
                wrapped, unit = RUNNER["bounded_command"]({"repo": "/s"}, Path("/p"), 60, 200, True)
                off_linux, _ = RUNNER["bounded_command"]({"repo": "/s"}, Path("/p"), 60, 200, False)
        # The unit (cgroup, CPU quota, timeout) is unchanged; only its worker is wrapped.
        self.assertEqual(plain[-5], sys.executable)
        self.assertIn(f"--unit={unit}", wrapped)
        tail = wrapped[wrapped.index("unshare"):]
        self.assertEqual(tail[:2], ["unshare", "--user"])
        for flag in ("--pid", "--fork", "--kill-child", "--mount-proc"):
            self.assertIn(flag, tail)
        self.assertEqual(tail[tail.index("--") + 1:], plain[-5:])
        self.assertNotIn("unshare", off_linux)

        seen = []

        def stream(command, emit, **_kwargs):
            seen.append(list(command))
            emit("Ran 1 test in 0.1s\n")
            return 0

        args = SimpleNamespace(dry_run=False, runtime_max_sec=60, total_max_sec=None, repo=str(self.root))
        command = ["python3", "-m", "unittest", "-v", "scripts/tests/t.py"]
        for state, expected in ((namespaced, True), (None, False)):
            seen.clear()
            with patch.dict(RUNNER["run_python_tests"].__globals__, PID_ISOLATION=state, stream=stream,
                            RECEIPT={"python": []}), \
                    patch.object(RUNNER["PYTHON_GATE"], "runner_status", return_value=None), \
                    contextlib.redirect_stdout(io.StringIO()):
                self.assertIsNone(RUNNER["run_python_tests"]([command], args, self.root, time.monotonic(), {}))
            self.assertEqual(seen[0][0] == "unshare", expected, seen)
            self.assertEqual(seen[0][-5:], command)

    def test_the_receipt_records_whether_the_run_was_pid_isolated(self):
        # #3478: `pid_isolation` is `namespaced` or `unavailable` (with the reason), never absent.
        record = RUNNER["record_pid_isolation"]
        globals_ = record.__globals__
        receipt = globals_["RECEIPT"]
        isolate = RUNNER["PID_ISOLATE"]
        found = {"mode": "namespaced", "reason": "", "mapping": "current_user"}
        refused = {"mode": "unavailable", "reason": "canary_exit_1: unshare failed: Operation not permitted"}
        before = dict(receipt)
        try:
            with patch.dict(globals_, PID_ISOLATION=None), contextlib.redirect_stdout(io.StringIO()) as printed:
                with patch.object(isolate, "probe", return_value=found):
                    record()
                self.assertEqual(receipt["pid_isolation"], "namespaced")
                self.assertNotIn("pid_isolation_reason", receipt)
                self.assertEqual(globals_["PID_ISOLATION"], found)
                with patch.object(isolate, "probe", return_value=refused):
                    record()
                self.assertEqual(receipt["pid_isolation"], "unavailable")
                self.assertIn("Operation not permitted", receipt["pid_isolation_reason"])
                self.assertIsNone(globals_["PID_ISOLATION"])
            self.assertIn("pid_isolation=unavailable", printed.getvalue())
        finally:
            for key in ("pid_isolation", "pid_isolation_reason"):
                receipt.pop(key, None)
            receipt.update({key: before[key] for key in ("pid_isolation", "pid_isolation_reason") if key in before})

    def test_tmpfs_runs_selected_binaries_under_private_tmpdir_and_cleans_up(self):
        parent = self.root / "shm"
        parent.mkdir()
        binary = self.executable("tmpdir-binary", '''
if '--list' in sys.argv:
    print('store::migration: test')
else:
    with Path(os.environ['SCOPED_TEST_LOG']).open('a') as log:
        log.write('TMPDIR=' + os.environ['TMPDIR'] + '\\n')
''')
        artifact = {"reason": "compiler-artifact", "profile": {"test": True},
                    "manifest_path": str(self.root / "Cargo.toml"),
                    "executable": str(binary), "target": {"kind": ["lib"], "name": "rsid_store"}}
        cargo = self.executable("cargo-tmpdir", f"print({json.dumps(json.dumps(artifact))})\n")
        plan = self.plan()
        plan["build"] = RUNNER["build_command"]("rsid-store", plan["selections"], [str(cargo)])
        plan["tmpfs"] = str(parent)
        path = self.root / "plan.json"
        path.write_text(json.dumps(plan))
        result = subprocess.run([sys.executable, str(SCRIPT), "--execute-plan", str(path)],
                                capture_output=True, text=True)
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertRegex(result.stdout, r"TMPFS_PARITY tmpfs=" + re.escape(str(parent)))
        seen = [line for line in self.log.read_text().splitlines() if line.startswith("TMPDIR=")]
        self.assertEqual(len(seen), 1)
        self.assertTrue(seen[0].startswith("TMPDIR=" + str(parent) + "/rsi-scoped-test-tmpfs-"), seen)
        self.assertEqual(list(parent.iterdir()), [])

    def test_a_broad_selection_runs_in_one_libtest_invocation(self):
        """#1756: ~200 tests in one module used to cost one process (~2.5 s) each."""
        names = [f"session::lifecycle::tests::case_{n:03}" for n in range(200)]
        body = f"""
names = {names!r}
if '--list' in sys.argv:
    for name in names:
        print(name + ': test')
else:
    asked = [arg for arg in sys.argv[1:] if not arg.startswith('--')]
    assert asked == names, asked
    print('running %d tests' % len(asked))
    for name in asked:
        print('test ' + name + ' ... ', end='', flush=True)
        print('output from the test body', flush=True)
        print('ok', flush=True)
    print('test result: ok. %d passed; 0 failed' % len(asked))
"""
        self.binary = self.executable("test-binary", body)
        artifact = {"reason": "compiler-artifact", "profile": {"test": True},
                    "manifest_path": str(self.root / "Cargo.toml"),
                    "executable": str(self.binary), "target": {"kind": ["lib"], "name": "rsid"}}
        self.cargo = self.executable("cargo-stub", f"print({json.dumps(json.dumps(artifact))})\n")
        selections = RUNNER["selection_plan"](["rsid=session::lifecycle"], {})["rsid"]
        plan = {"repo": str(self.root), "package": "rsid", "selections": selections,
                "build": RUNNER["build_command"]("rsid", selections, [str(self.cargo)])}
        path = self.root / "plan.json"
        path.write_text(json.dumps(plan))
        result = subprocess.run([sys.executable, str(SCRIPT), "--execute-plan", str(path)],
                                capture_output=True, text=True)
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        runs = [line for line in self.log.read_text().splitlines()
                if line.startswith("test-binary ") and "--exact" in line]
        self.assertEqual(len(runs), 1, runs)
        self.assertIn("executed 200 tests", result.stdout)
        self.assertEqual(result.stdout.count("SCOPED_START "), 200)
        self.assertEqual(result.stdout.count("SCOPED_END "), 200)
        self.assertIn("SCOPED_COMPLETE rsid 200", result.stdout)

    def sharded_plan(self, body, names, shards):
        self.binary = self.executable("test-binary", body)
        artifact = {"reason": "compiler-artifact", "profile": {"test": True},
                    "manifest_path": str(self.root / "Cargo.toml"),
                    "executable": str(self.binary), "target": {"kind": ["lib"], "name": "rsid"}}
        self.cargo = self.executable("cargo-stub", f"print({json.dumps(json.dumps(artifact))})\n")
        selections = RUNNER["selection_plan"](["rsid=shardcase::"], {})["rsid"]
        plan = {"repo": str(self.root), "package": "rsid", "selections": selections, "test_shards": shards,
                "build": RUNNER["build_command"]("rsid", selections, [str(self.cargo)])}
        path = self.root / "plan.json"
        path.write_text(json.dumps(plan))
        return [sys.executable, str(SCRIPT), "--execute-plan", str(path)]

    SHARD_BODY = """
names = {names!r}
if '--list' in sys.argv:
    for name in names:
        print(name + ': test')
    sys.exit(0)
assert '--test-threads=1' in sys.argv
asked = [arg for arg in sys.argv[1:] if not arg.startswith('--')]
live = Path(os.environ['SCOPED_TEST_LOG']).with_name('live-' + str(os.getpid()))
live.write_text('')
{behaviour}
for name in asked:
    print('test ' + name + ' ... ', end='', flush=True)
    {hang}
    print('ok', flush=True)
print('test result: ok. %d passed; 0 failed' % len(asked))
"""

    def test_shards_run_concurrently_and_their_counts_merge(self):
        names = [f"shardcase::t{n}" for n in range(8)]
        # Each process waits for the other's marker, so serial execution would time out.
        behaviour = """
import time
deadline = time.monotonic() + 10
while len(list(live.parent.glob('live-*'))) < 2:
    if time.monotonic() > deadline:
        sys.exit(3)
    time.sleep(0.05)
"""
        command = self.sharded_plan(self.SHARD_BODY.format(names=names, behaviour=behaviour, hang="pass"),
                                    names, 2)
        result = subprocess.run(command, capture_output=True, text=True, timeout=60)
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        runs = [line.split() for line in self.log.read_text().splitlines()
                if line.startswith("test-binary ") and "--exact" in line]
        self.assertEqual(sorted(run[1:5] for run in runs), [names[0:7:2][:4], names[1:8:2][:4]])
        self.assertIn("executed 8 tests", result.stdout)
        self.assertEqual(result.stdout.count("SCOPED_START "), 8)
        self.assertEqual(result.stdout.count("SCOPED_END "), 8)
        self.assertIn("SCOPED_COMPLETE rsid 8", result.stdout)

    def test_a_hang_in_one_shard_names_that_test_while_the_other_shard_finishes(self):
        names = [f"shardcase::t{n}" for n in range(4)]
        hang = "time.sleep(60) if name == 'shardcase::t1' else None"
        command = self.sharded_plan(
            "import time\n" + self.SHARD_BODY.format(names=names, behaviour="", hang=hang), names, 2)
        seen, started = [], time.monotonic()
        # Own process group: the shard children keep writing `live-*` files into the fixture
        # directory after the runner dies, which raced its cleanup ("Directory not empty").
        with subprocess.Popen(command, stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True,
                              start_new_session=True) as process:
            try:
                for line in process.stdout:
                    seen.append(line)
                    if "SCOPED_END shardcase::t2" in line:
                        break
                    self.assertLess(time.monotonic() - started, 30, "".join(seen))
            finally:
                os.killpg(process.pid, signal.SIGKILL)
        # shard 0 (t0, t2) finished; shard 1 is stuck inside t1 and is the only open test.
        output = "".join(seen)
        open_tests = {line.split()[1] for line in output.splitlines() if line.startswith("SCOPED_START ")} - \
            {line.split()[1] for line in output.splitlines() if line.startswith("SCOPED_END ")}
        self.assertEqual(open_tests, {"shardcase::t1"}, output)

    def test_a_timeout_names_every_shards_running_test(self):
        code, output = self.invoke(systemd_body="""
print('SCOPED_MATCH rsid=shard:store-01:test(store::migration)')
print('SCOPED_START store::migration_one', flush=True)
print('SCOPED_START store::migration_two', flush=True)
import time
time.sleep(30)
""")
        self.assertEqual(code, 1, output)
        self.assertRegex(output, r"scoped_test_timeout: slowest still-running tests: .*migration_one.*migration_two")

    def test_adaptive_threads_follow_free_cpu_within_bounds(self):
        threads = RUNNER["adaptive_threads"]
        self.assertEqual(threads(32, 11.0), 10)
        self.assertEqual(threads(32, 40.0), 2)
        self.assertEqual(threads(64, 0.0), 12)
        self.assertEqual(threads(4, 8.0), 2)
        self.assertEqual(threads(8, None), 4)

    def test_packages_run_most_specific_first(self):
        plan = RUNNER["selection_plan"](["rsid=module::", "rsid=other::", "rsid-store=bin:tool"], {})
        plan["rsi-common"] = [{"filter": "rsi-common=x", "kind": "lib", "target": "", "substring": "",
                               "exact": "a::b", "regex": None}]
        self.assertEqual([package for package, _ in RUNNER["package_order"](plan)],
                         ["rsi-common", "rsid-store", "rsid"])

    def test_detect_runner_prefers_nextest_when_new_enough_and_falls_back_to_libtest(self):
        detect = RUNNER["detect_runner"]
        glue = detect.__globals__
        done = subprocess.CompletedProcess([], 0, "cargo-nextest 0.9.137 (abc 2026-05-26)\n", "")
        with patch.dict(os.environ, {"RSI_SCOPED_TEST_RUNNER": ""}):
            with patch.dict(glue, shutil=type("S", (), {"which": staticmethod(lambda _n: "/x/cargo-nextest")})), \
                 patch("subprocess.run", return_value=done):
                self.assertEqual(detect(self.root), ("nextest", "0.9.137"))
                (self.root / ".config").mkdir()
                (self.root / ".config" / "nextest.toml").write_text('nextest-version = { required = "0.9.999" }\n')
                self.assertEqual(detect(self.root), ("libtest", None))
                with self.assertRaises(ValueError):
                    detect(self.root, "nextest")
            with patch.dict(glue, shutil=type("S", (), {"which": staticmethod(lambda _n: None)})):
                self.assertEqual(detect(self.root), ("libtest", None))
        self.assertEqual(detect(self.root, "libtest"), ("libtest", None))

    def test_nextest_progress_turns_status_lines_into_the_start_end_protocol(self):
        out = []
        progress = RUNNER["NextestProgress"](["t::a", "t::b", "t::c", "t::d"], out.append)
        for line in (
            "        PASS [   0.002s] ( 1/4) p t::a\n",
            "        SLOW [>  1.000s] (───) p t::b\n",
            " TERMINATING [>  2.000s] (───) p t::b\n",
            "     TIMEOUT [   2.003s] ( 2/4) p t::b\n",
            "        FAIL [   0.002s] ( 3/4) p t::c\n",
            "  stdout ───\n",
            "    test t::c ... FAILED\n",
            "        SLOW [>  1.000s] (───) p t::d\n",
        ):
            progress.line(line)
        self.assertEqual([line.strip() for line in out if line.startswith("SCOPED_")],
                         ["SCOPED_START t::a", "SCOPED_END t::a", "SCOPED_START t::b", "SCOPED_END t::b",
                          "SCOPED_START t::c", "SCOPED_END t::c", "SCOPED_START t::d"])
        self.assertEqual((progress.ran, sorted(progress.failures), progress.missing()),
                         (3, ["t::b", "t::c"], ["t::d"]))

    NEXTEST_STUB = """
if sys.argv[1:2] != ['nextest']:
    print({artifact!r})
    sys.exit(0)
args = sys.argv[2:]
names = re.findall(r'test\\(=([^)]+)\\)', args[args.index('-E') + 1])
assert '--test-threads' in args and args[args.index('--test-threads') + 1] == '{threads}', args
for name in names:
    print('        %s [   0.001s] (  1/1) rsid %s' % ('FAIL' if name == '{fail}' else 'PASS', name), flush=True)
sys.exit(100 if '{fail}' in names else 0)
"""

    def nextest_plan(self, names, threads=3, fail="", box=None, features=(), filter_="rsid=shardcase::"):
        body = f"names = {names!r}\nif '--list' in sys.argv:\n    [print(n + ': test') for n in names]\n"
        self.binary = self.executable("test-binary", body)
        artifact = {"reason": "compiler-artifact", "profile": {"test": True},
                    "manifest_path": str(self.root / "Cargo.toml"),
                    "executable": str(self.binary), "target": {"kind": ["lib"], "name": "rsid"}}
        stub = self.executable("cargo-stub", "import re\n" + self.NEXTEST_STUB.format(
            artifact=json.dumps(artifact), threads=threads, fail=fail))
        selections = RUNNER["selection_plan"]([filter_], {})["rsid"]
        plan = {"repo": str(self.root), "package": "rsid", "selections": selections, "test_shards": threads,
                "test_threads": threads, "runner": "nextest", "nextest": [str(stub), "nextest"],
                "features": list(features),
                "time_box_sec": box, "build": RUNNER["build_command"]("rsid", selections, [str(stub)], features)}
        path = self.root / "plan.json"
        path.write_text(json.dumps(plan))
        return [sys.executable, str(SCRIPT), "--execute-plan", str(path)]

    def test_nextest_runs_exact_names_at_the_planned_thread_count(self):
        names = [f"shardcase::t{n}" for n in range(5)]
        result = subprocess.run(self.nextest_plan(names), capture_output=True, text=True, timeout=60)
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertIn("executed 5 tests", result.stdout)
        self.assertEqual(result.stdout.count("SCOPED_END "), 5)
        self.assertIn("SCOPED_COMPLETE rsid 5", result.stdout)
        runs = [line for line in self.log.read_text().splitlines() if line.startswith("cargo-stub nextest run")]
        self.assertEqual(len(runs), 1, runs)
        self.assertIn("--lib", runs[0])
        # The names are explicit and the slow tier is scoped-test's own decision (#2270): the
        # profile's default-filter must not drop a named test.
        self.assertIn("--ignore-default-filter", runs[0])
        self.assertIn("test(=shardcase::t0) | test(=shardcase::t1)", runs[0])

    def test_nextest_failures_are_named_and_fail_the_package(self):
        names = [f"shardcase::t{n}" for n in range(3)]
        result = subprocess.run(self.nextest_plan(names, fail="shardcase::t1"), capture_output=True, text=True,
                                timeout=60)
        self.assertEqual(result.returncode, 1, result.stdout + result.stderr)
        self.assertIn("SCOPED_FAILURE lib:rsid shardcase::t1", result.stdout)
        self.assertIn("SCOPED_PHASE test_failed", result.stdout)

    def test_required_features_reach_both_the_build_and_nextest(self):
        result = subprocess.run(self.nextest_plan(["recursive_dag::smoke_fixture::tests::one"],
                                                 features=["dev-fixtures"], filter_="rsid=recursive_dag::smoke_fixture"),
                                capture_output=True, text=True, timeout=60)
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertIn("SCOPED_COMPLETE rsid 1", result.stdout)
        commands = self.log.read_text().splitlines()
        build = next(line for line in commands if line.startswith("cargo-stub test"))
        run = next(line for line in commands if line.startswith("cargo-stub nextest run"))
        self.assertIn("--features dev-fixtures", build)
        self.assertIn("--features dev-fixtures", run)

    def test_a_spent_time_box_skips_the_remaining_batches_without_failing(self):
        out = io.StringIO()
        binary = self.executable("two-batches", "print('a::one: test\\na::two: test' if '--list' in sys.argv else '')\n")
        selections = RUNNER["selection_plan"](["rsid=a::one", "rsid=a::two"], {})["rsid"]
        artifacts = {str(binary): ({"kind": ["lib"], "name": "rsid"}, str(self.root))}
        calls = []

        clock = ControlledClock()

        def run_batch(_binary, names, *_args):
            calls.append(list(names))
            clock.advance(0.15)
            return len(names), 0, [], set(names)

        with patch.dict(RUNNER["run_selected"].__globals__, run_batch=run_batch, time=clock,
                        test_batches=lambda names, *_limit: [[name] for name in names]), \
             contextlib.redirect_stdout(out):
            result = RUNNER["run_selected"]({"package": "rsid", "selections": selections,
                                             "time_box_sec": 0.05}, {str(binary): artifacts[str(binary)]}, None)
        self.assertEqual((result, len(calls)), (0, 1))
        subset = json.loads(next(line for line in out.getvalue().splitlines()
                                 if line.startswith("SCOPED_SUBSET ")).split(" ", 1)[1])
        self.assertEqual((subset["selected_tests"], subset["run_tests"], subset["box_expired"]), (2, 1, True))
        self.assertEqual(sorted(subset["filters"].values()), [[1, 0], [1, 1]])
        self.assertIn("SCOPED_COMPLETE rsid 1", out.getvalue())

    def boxed_priority_run(self, failing=()):
        """Run priority filters first under a time box that expires before the broad tier starts."""
        out = io.StringIO()
        names = ["pri::a", "pri::b", "pri::c", "broad::one", "broad::two", "broad::three"]
        binary = self.executable("boxed", f"print({chr(10).join(n + ': test' for n in names)!r} if '--list' in sys.argv else '')\n")
        selections = RUNNER["selection_plan"](
            ["rsid=broad::"], {}, ["rsid=pri::a", "rsid=pri::b", "rsid=pri::c"])["rsid"]
        calls = []

        clock = ControlledClock()

        def run_batch(_binary, batch, *_args):
            calls.append(list(batch))
            clock.advance(0.15)
            return len(batch), int(bool(failing)), sorted(set(batch) & set(failing)), set(batch)

        with patch.dict(RUNNER["run_selected"].__globals__, run_batch=run_batch, time=clock,
                        test_batches=lambda batch, *_limit: [list(batch)]), contextlib.redirect_stdout(out):
            result = RUNNER["run_selected"]({"package": "rsid", "selections": selections, "time_box_sec": 0.05},
                                            {str(binary): ({"kind": ["lib"], "name": "rsid"}, str(self.root))}, None)
        subset = json.loads(next(line for line in out.getvalue().splitlines()
                                 if line.startswith("SCOPED_SUBSET ")).split(" ", 1)[1])
        return result, calls, subset, {"rsid": selections}

    def test_priority_filters_executed_before_the_box_expires_are_run_and_broad_is_partial(self):
        result, calls, subset, plan = self.boxed_priority_run()
        self.assertEqual((result, calls), (0, [["pri::a", "pri::b", "pri::c"]]))
        self.assertEqual(subset["filters"], {"rsid=pri::a": [1, 1], "rsid=pri::b": [1, 1],
                                             "rsid=pri::c": [1, 1], "rsid=broad::": [3, 0]})
        self.assertTrue(subset["box_expired"])
        coverage = RUNNER["filter_coverage"](plan, [subset])
        self.assertEqual(coverage["run"], ["rsid=pri::a", "rsid=pri::b", "rsid=pri::c"])
        self.assertEqual((coverage["partial"], coverage["not_run"]), ([], ["rsid=broad::"]))

    def test_a_failed_priority_test_counts_as_executed_without_turning_the_run_green(self):
        result, _calls, subset, plan = self.boxed_priority_run(failing=["pri::c"])
        self.assertEqual(result, 1)
        self.assertEqual(subset["filters"]["rsid=pri::c"], [1, 1])
        self.assertEqual(RUNNER["filter_coverage"](plan, [subset])["run"],
                         ["rsid=pri::a", "rsid=pri::b", "rsid=pri::c"])

    def boxed_changed_module_run(self, records, box=0.05):
        """The #1186 shape under a controlled time box: a broad `session::` filter whose unrelated
        tests sort before the changed module's, beside narrow exact observers of unrelated modules."""
        out = io.StringIO()
        unrelated = [f"provider_capabilities::legacy_{n}" for n in range(3)]
        earlier = [f"session::agent_{n:02}" for n in range(30)]
        rotation = ["session::rotation::fence_tests::hold", "session::rotation::recovery::tests::keeps_controller",
                    "session::rotation::tests::publishes", "session::rotation::zeta_tests::last"]
        untouched = ["session::rotation::abandon::tests::own_source"]
        names = unrelated + earlier + rotation + untouched
        binary = self.executable("changed-modules", f"print({chr(10).join(n + ': test' for n in names)!r} if '--list' in sys.argv else '')\n")
        selections = RUNNER["selection_plan"](
            ["rsid=shard:session-03:test(/^session::/)"] + [f"rsid=shard:other-01:test(={n})" for n in unrelated],
            {"session-03": ["rsid"], "other-01": ["rsid"]})["rsid"]
        calls = []

        clock = ControlledClock()

        def run_batch(_binary, batch, *_args):
            calls.append(list(batch))
            clock.advance(0.15)
            return len(batch), 0, [], set(batch)

        plan = {"package": "rsid", "selections": selections, "time_box_sec": box, "changed_modules": records}
        with patch.dict(RUNNER["run_selected"].__globals__, run_batch=run_batch, time=clock,
                        test_batches=lambda batch, *_limit: [list(batch)]), contextlib.redirect_stdout(out):
            result = RUNNER["run_selected"](plan, {str(binary): ({"kind": ["lib"], "name": "rsid"}, str(self.root))}, None)
        subset = json.loads(next(line for line in out.getvalue().splitlines()
                                 if line.startswith("SCOPED_SUBSET ")).split(" ", 1)[1])
        return result, calls, subset, {"rsid": selections}, rotation, untouched

    CHANGED_ROTATION = [
        {"package": "rsid", "module": "session::rotation", "path": "crates/rsid/src/session/rotation.rs",
         "excluded": ["session::rotation::abandon"], "tests": 1},
        {"package": "rsid", "module": "session::rotation::recovery", "path": "crates/rsid/src/session/rotation/recovery.rs",
         "excluded": [], "tests": 1},
    ]

    def test_a_changed_modules_tests_run_before_unrelated_observers_can_spend_the_time_box(self):
        # Without the receipt's changed modules the narrow unrelated observers fill the box first.
        result, calls, subset, _plan, rotation, _untouched = self.boxed_changed_module_run([])
        self.assertEqual((result, calls), (0, [["provider_capabilities::legacy_0", "provider_capabilities::legacy_1",
                                                "provider_capabilities::legacy_2"]]))
        self.assertEqual(subset["changed_module_tests"], [0, 0])
        # With them, a spent box never takes a changed module's own tests (#3252): the smallest module
        # (recovery) runs first, then the rest of the changed modules, never an unchanged child source's,
        # and only the tiers after them are left to the gate.
        result, calls, subset, plan, rotation, untouched = self.boxed_changed_module_run(self.CHANGED_ROTATION)
        self.assertEqual(result, 0)
        self.assertEqual(calls[0], ["session::rotation::recovery::tests::keeps_controller"])
        self.assertEqual(sorted(name for batch in calls for name in batch), sorted(rotation))
        self.assertNotIn(untouched[0], [name for batch in calls for name in batch])
        self.assertTrue(subset["box_expired"])
        self.assertEqual(subset["changed_module_tests"], [4, 4])
        self.assertEqual((subset["selected_tests"], subset["run_tests"]), (38, 4))
        coverage = RUNNER["filter_coverage"](plan, [subset])
        self.assertEqual(coverage["run"], [])
        self.assertIn({"filter": "rsid=shard:session-03:test(/^session::/)", "selected": 35, "executed": 4}, coverage["partial"])
        self.assertEqual(len(coverage["not_run"]), 3)

    def test_with_a_wider_box_every_changed_module_test_runs_in_size_order_before_the_rest(self):
        result, calls, subset, _plan, rotation, untouched = self.boxed_changed_module_run(self.CHANGED_ROTATION, box=60)
        self.assertEqual(result, 0)
        order = [name for batch in calls for name in batch]
        self.assertEqual(order[0], "session::rotation::recovery::tests::keeps_controller")
        self.assertEqual(sorted(order[1:4]), sorted(set(rotation) - {order[0]}))
        self.assertEqual(len(order), 38)
        self.assertEqual(len(set(order)), 38)
        self.assertEqual(subset["changed_module_tests"], [4, 4])
        self.assertFalse(subset["box_expired"])
        self.assertNotIn(untouched[0], order[:4])

    DIRECT_FILTERS = [
        {"filter": r"rsid=shard:session-03:test(/^session::zeta::(?:\w+::)*calls_the_changed_function$/)", "distance": 1},
        {"filter": r"rsid=shard:session-03:test(/^session::zeta::(?:\w+::)*calls_the_hub_method$/)", "distance": 2},
        {"filter": r"rsid=shard:session-03:test(/^session::beta::(?:\w+::)*calls_the_wrapper$/)", "distance": 3},
    ]
    DIRECT = ["session::zeta::tests::calls_the_changed_function", "session::zeta::tests::calls_the_hub_method",
              "session::beta::tests::calls_the_wrapper"]

    def boxed_hub_run(self, direct_filters, box=0.05):
        """The #2588 shape under a controlled time box: the broad `session::` filter of a stopped hub
        trace selects 30 far tests whose names sort before the direct observers'."""
        out = io.StringIO()
        far = [f"session::agent_{n:02}" for n in range(30)]
        names = far + self.DIRECT
        binary = self.executable("hub-run", f"print({chr(10).join(n + ': test' for n in names)!r} if '--list' in sys.argv else '')\n")
        shards = {"session-03": ["rsid"]}
        selections = RUNNER["selection_plan"](["rsid=shard:session-03:test(/^session::/)"], shards)["rsid"]
        with patch.dict(RUNNER["RECEIPT"]), contextlib.redirect_stdout(out):
            direct = RUNNER["direct_observer_plan"]({"direct_observer_filters": direct_filters,
                                                    "direct_observers": {"tests": len(direct_filters), "by_distance": {}}}, shards)
            direct_receipt = dict(RUNNER["RECEIPT"].get("direct_observers") or {})
        calls = []

        clock = ControlledClock()

        def run_batch(_binary, batch, *_args):
            calls.append(list(batch))
            clock.advance(0.15)
            return len(batch), 0, [], set(batch)

        plan = {"package": "rsid", "selections": selections, "time_box_sec": box,
                "direct_selections": direct.get("rsid", [])}
        with patch.dict(RUNNER["run_selected"].__globals__, run_batch=run_batch, time=clock,
                        test_batches=lambda batch, *_limit: [list(batch)]), contextlib.redirect_stdout(out):
            result = RUNNER["run_selected"](plan, {str(binary): ({"kind": ["lib"], "name": "rsid"}, str(self.root))}, None)
        subset = json.loads(next(line for line in out.getvalue().splitlines()
                                 if line.startswith("SCOPED_SUBSET ")).split(" ", 1)[1])
        return result, calls, subset, {"rsid": selections}, direct_receipt

    def test_the_direct_observers_of_a_stopped_hub_trace_run_before_the_far_observers_in_a_spent_box(self):
        # Without them the broad filter runs in name order: thirty far tests spend the box first.
        result, calls, subset, _plan, _direct = self.boxed_hub_run([])
        self.assertEqual((result, len(calls), calls[0][0]), (0, 1, "session::agent_00"))
        self.assertEqual(subset["direct_observer_tests"], [0, 0])
        # With them the one batch the box admits is the nearest direct observer, whose name sorts last.
        result, calls, subset, plan, direct = self.boxed_hub_run(self.DIRECT_FILTERS)
        self.assertEqual((result, calls), (0, [[self.DIRECT[0]]]))
        self.assertTrue(subset["box_expired"])
        self.assertEqual(subset["direct_observer_tests"], [3, 1])
        self.assertEqual(direct["filters"], 3)
        # The executed-identity accounting is the plan's own: the broad filter selected all 33, ran 1.
        self.assertEqual((subset["selected_tests"], subset["run_tests"]), (33, 1))
        coverage = RUNNER["filter_coverage"](plan, [subset])
        self.assertEqual(coverage["run"], [])
        self.assertEqual(coverage["partial"], [{"filter": "rsid=shard:session-03:test(/^session::/)",
                                                "selected": 33, "executed": 1}])
        self.assertEqual(subset["filters"], {"rsid=shard:session-03:test(/^session::/)": [33, 1]})

    def test_with_a_wider_box_every_direct_observer_runs_nearest_first_and_every_far_test_after_it(self):
        result, calls, subset, plan, _direct = self.boxed_hub_run(self.DIRECT_FILTERS, box=60)
        self.assertEqual(result, 0)
        self.assertEqual(calls[:3], [[name] for name in self.DIRECT])
        far = [name for batch in calls[3:] for name in batch]
        self.assertEqual(far, sorted(far))
        self.assertEqual(len(far), 30)
        self.assertEqual(sum(len(batch) for batch in calls), 33)
        self.assertEqual(subset["direct_observer_tests"], [3, 3])
        self.assertFalse(subset["box_expired"])
        self.assertEqual(RUNNER["filter_coverage"](plan, [subset])["run"], ["rsid=shard:session-03:test(/^session::/)"])

    EXACT_FAR = [f"provider_capabilities::legacy_{n}" for n in range(3)]

    def mixed_run(self, box, binaries=None):
        """Direct observers selected only by the broad `session::` filter beside unrelated far tests an
        exact filter selects (the narrowest specificity tier), an added test and a changed module."""
        out = io.StringIO()
        far = [f"session::agent_{n:02}" for n in range(30)]
        added = "session::added::tests::new_test"
        changed = "session::rotation::tests::own_test"
        names = far + self.DIRECT + self.EXACT_FAR + [added, changed]
        binary = self.executable("mixed-run", f"print({chr(10).join(n + ': test' for n in names)!r} if '--list' in sys.argv else '')\n")
        shards = {"session-03": ["rsid"], "other-01": ["rsid"]}
        added_filter = f"rsid=shard:session-03:test(={added})"
        selections = RUNNER["selection_plan"](
            ["rsid=shard:session-03:test(/^session::/)"] + [f"rsid=shard:other-01:test(={n})" for n in self.EXACT_FAR],
            shards, [added_filter])["rsid"]
        with patch.dict(RUNNER["RECEIPT"]), contextlib.redirect_stdout(out):
            direct = RUNNER["direct_observer_plan"]({"direct_observer_filters": self.DIRECT_FILTERS}, shards)
        calls = []

        clock = ControlledClock()

        def run_batch(_binary, batch, *_args):
            calls.append(list(batch))
            clock.advance(0.15)
            return len(batch), 0, [], set(batch)

        records = [{"package": "rsid", "module": "session::rotation", "path": "x", "excluded": [], "tests": 1}]
        plan = {"package": "rsid", "selections": selections, "time_box_sec": box, "changed_modules": records,
                "direct_selections": direct.get("rsid", [])}
        with patch.dict(RUNNER["run_selected"].__globals__, run_batch=run_batch, time=clock,
                        test_batches=lambda batch, *_limit: [list(batch)]), contextlib.redirect_stdout(out):
            result = RUNNER["run_selected"](plan, {str(binary): ({"kind": ["lib"], "name": "rsid"}, str(self.root))}, None)
        subset = json.loads(next(line for line in out.getvalue().splitlines()
                                 if line.startswith("SCOPED_SUBSET ")).split(" ", 1)[1])
        return result, calls, subset, {"rsid": selections}, added, changed

    def test_a_direct_observer_selected_only_by_a_broad_filter_runs_before_an_unrelated_exact_far_test(self):
        # The pure ranking: specificity no longer puts a one-test far filter ahead of a proven observer.
        self.assertEqual(RUNNER["specificity_tiers"](["far_exact", "near_broad"], {"far_exact": 1, "near_broad": 100},
                                                     direct_ranks={"near_broad": 1}), [["near_broad"], ["far_exact"]])
        # Added and changed-module tests still come first: the spent box admits the added test and the
        # changed module's own test (#3252), never a direct or far observer.
        result, calls, subset, plan, added, changed = self.mixed_run(0.05)
        self.assertEqual((result, calls), (0, [[added], [changed]]))
        self.assertTrue(subset["box_expired"])

    def test_the_box_admits_the_nearest_direct_observer_once_the_priority_tests_ran(self):
        # 0.4s admits the added test and the changed module's, and one more batch: the nearest direct observer.
        result, calls, subset, plan, added, changed = self.mixed_run(0.4)
        self.assertEqual(calls[:3], [[added], [changed], [self.DIRECT[0]]])
        self.assertTrue(subset["box_expired"])
        # The unrelated exact far tests stay selected and deferred; the plan's coverage is unchanged.
        coverage = RUNNER["filter_coverage"](plan, [subset])
        self.assertEqual(sorted(coverage["not_run"]), sorted(f"rsid=shard:other-01:test(={n})" for n in self.EXACT_FAR))
        self.assertEqual(subset["selected_tests"], 30 + 3 + 3 + 2)
        self.assertEqual(subset["direct_observer_tests"][0], 3)

    def test_with_a_wider_box_every_test_runs_priority_then_direct_then_the_far_ones_by_specificity(self):
        result, calls, subset, plan, added, changed = self.mixed_run(60)
        self.assertEqual(result, 0)
        order = [name for batch in calls for name in batch]
        self.assertEqual(order[:5], [added, changed, *self.DIRECT])
        self.assertEqual(order[5:8], self.EXACT_FAR)
        self.assertEqual(order[8:], sorted(order[8:]))
        self.assertEqual(len(order), 30 + 3 + 3 + 2)
        self.assertFalse(subset["box_expired"])
        self.assertEqual(subset["direct_observer_tests"], [3, 3])
        self.assertEqual(RUNNER["filter_coverage"](plan, [subset])["not_run"], [])

    def test_a_direct_observer_in_a_later_binary_runs_before_an_earlier_binarys_far_tests(self):
        out = io.StringIO()
        lib = self.executable("lib-run", f"print({chr(10).join(n + ': test' for n in self.EXACT_FAR)!r} if '--list' in sys.argv else '')\n")
        tool = self.executable("tool-run", "print('tool::calls_the_change: test' if '--list' in sys.argv else '')\n")
        shards = {"other-01": ["rsid"]}
        selections = RUNNER["selection_plan"](
            [f"rsid=shard:other-01:test(={n})" for n in self.EXACT_FAR] + ["rsid=bin:tool:test(/^tool::/)"], shards)["rsid"]
        with patch.dict(RUNNER["RECEIPT"]), contextlib.redirect_stdout(out):
            direct = RUNNER["direct_observer_plan"](
                {"direct_observer_filters": [{"filter": "rsid=bin:tool:test(/^tool::calls_the_change$/)", "distance": 1}]}, shards)
        calls = []

        clock = ControlledClock()

        def run_batch(binary, batch, *_args):
            calls.append((Path(binary).name, list(batch)))
            clock.advance(1.0)
            return len(batch), 0, [], set(batch)

        artifacts = {str(lib): ({"kind": ["lib"], "name": "rsid"}, str(self.root)),
                     str(tool): ({"kind": ["bin"], "name": "tool"}, str(self.root))}
        plan = {"package": "rsid", "selections": selections, "time_box_sec": 0.8, "direct_selections": direct["rsid"]}
        with patch.dict(RUNNER["run_selected"].__globals__, run_batch=run_batch, time=clock,
                        test_batches=lambda batch, *_limit: [list(batch)]), contextlib.redirect_stdout(out):
            self.assertEqual(RUNNER["run_selected"](plan, artifacts, None), 0)
        self.assertEqual(calls, [("tool-run", ["tool::calls_the_change"])])
        subset = json.loads(next(line for line in out.getvalue().splitlines()
                                 if line.startswith("SCOPED_SUBSET ")).split(" ", 1)[1])
        self.assertEqual((subset["selected_tests"], subset["run_tests"], subset["direct_observer_tests"]), (4, 1, [1, 1]))

    def test_packages_holding_a_direct_observer_run_before_the_other_packages(self):
        plan = RUNNER["selection_plan"](["rsid=module::"], {})
        plan["rsid-store"] = [{"filter": "rsid-store=exact::one", "kind": "lib", "target": "", "substring": "",
                               "exact": "exact::one", "regex": None}]
        order = RUNNER["package_order"]
        self.assertEqual([p for p, _ in order(plan)], ["rsid-store", "rsid"])
        self.assertEqual([p for p, _ in order(plan, direct_packages={"rsid"})], ["rsid", "rsid-store"])
        # Changed modules still outrank a direct observer elsewhere.
        self.assertEqual([p for p, _ in order(plan, {"rsid-store"}, {"rsid"})], ["rsid-store", "rsid"])

    BOUNDED_FILTER = "rsid=shard:session-03:test(/^session::/)"
    SCANNER = "session::scan::tests::catalog_pin"
    # A proven direct observer outside every plan filter's scope (#1186: scheduler, satellite, store tests).
    OUTSIDE = "scheduler::tests::calls_the_changed_function"

    def bounded_run(self, box, extra=(), records=None):
        """#2592: the module filter of a trace that stopped at a cap is bounded, not widened.

        The listing holds thirty far tests the module filter matches, the direct observers, a changed
        module's two tests, a scanner pin an absorbed named filter selects and three unrelated exact tests."""
        out = io.StringIO()
        far = [f"session::agent_{n:02}" for n in range(30)]
        own = ["session::rotation::tests::own_test", "session::rotation::tests::own_two"]
        names = far + self.DIRECT + own + [self.SCANNER, self.OUTSIDE, *self.EXACT_FAR, *extra]
        binary = self.executable("bounded-run", f"print({chr(10).join(n + ': test' for n in names)!r} if '--list' in sys.argv else '')\n")
        shards = {"session-03": ["rsid"], "other-01": ["rsid"]}
        selections = RUNNER["selection_plan"](
            [self.BOUNDED_FILTER, f"rsid=shard:session-03:test(={self.SCANNER})"]
            + [f"rsid=shard:other-01:test(={n})" for n in self.EXACT_FAR], shards)["rsid"]
        for selection in selections:
            selection["bounded"] = selection["filter"] == self.BOUNDED_FILTER
        outside = {"filter": f"rsid=shard:other-01:test(={self.OUTSIDE})", "distance": 4}
        with patch.dict(RUNNER["RECEIPT"]), contextlib.redirect_stdout(out):
            direct = RUNNER["direct_observer_plan"]({"direct_observer_filters": [*self.DIRECT_FILTERS, outside]}, shards)
        RUNNER["with_direct_observers"]({"rsid": selections}, direct)
        calls = []

        clock = ControlledClock()

        def run_batch(_binary, batch, *_args):
            calls.append(list(batch))
            clock.advance(0.15)
            return len(batch), 0, [], set(batch)

        records = records if records is not None else [
            {"package": "rsid", "module": "session::rotation", "path": "x", "excluded": [], "tests": 2}]
        plan = {"package": "rsid", "selections": selections, "time_box_sec": box, "changed_modules": records,
                "direct_selections": direct.get("rsid", [])}
        with patch.dict(RUNNER["run_selected"].__globals__, run_batch=run_batch, time=clock,
                        test_batches=lambda batch, *_limit: [list(batch)]), contextlib.redirect_stdout(out):
            result = RUNNER["run_selected"](plan, {str(binary): ({"kind": ["lib"], "name": "rsid"}, str(self.root))}, None)
        subset = json.loads(next(line for line in out.getvalue().splitlines()
                                 if line.startswith("SCOPED_SUBSET ")).split(" ", 1)[1])
        return result, calls, subset, {"rsid": selections}

    def test_a_bounded_module_filter_runs_only_its_changed_module_and_direct_tests_and_defers_the_far_ones(self):
        result, calls, subset, plan = self.bounded_run(60)
        self.assertEqual(result, 0)
        order = [name for batch in calls for name in batch]
        # Changed module first, then the direct observers nearest first, then the exact tests by specificity.
        self.assertEqual(order[:2], ["session::rotation::tests::own_test", "session::rotation::tests::own_two"])
        self.assertEqual(order[2:5], self.DIRECT)
        # The direct observer no plan filter matches is selected in its own right, after the nearer ones.
        self.assertEqual(order[5], self.OUTSIDE)
        self.assertEqual(sorted(order[6:]), sorted([self.SCANNER, *self.EXACT_FAR]))
        # Not one of the thirty far tests the module filter matches was selected, run or counted passed.
        self.assertEqual(len(order), 10)
        self.assertFalse([name for name in order if name.startswith("session::agent_")])
        self.assertEqual((subset["selected_tests"], subset["run_tests"], subset["deferred_tests"]), (10, 10, 30))
        self.assertEqual(subset["bounded"], {self.BOUNDED_FILTER: [5, 5, 30]})
        self.assertNotIn(self.BOUNDED_FILTER, subset["filters"])
        self.assertEqual(subset["direct_observer_tests"], [4, 4])
        self.assertFalse(subset["box_expired"])
        coverage = RUNNER["filter_coverage"](plan, [subset])
        self.assertEqual((coverage["partial"], coverage["not_run"], coverage["selected"]), ([], [], 4))
        self.assertNotIn(self.BOUNDED_FILTER, coverage["run"])
        self.assertEqual(coverage["bounded"], [{"filter": self.BOUNDED_FILTER, "kept": 5, "executed": 5, "deferred": 30}])
        self.assertEqual(coverage["deferred_tests"], 30)

    def test_a_spent_box_leaves_kept_tests_unrun_and_the_deferred_count_unchanged(self):
        # 0.4s admits the changed module's batch and two direct observers: the nearest first.
        result, calls, subset, plan = self.bounded_run(0.4)
        self.assertEqual(result, 0)
        self.assertEqual(calls[:3], [["session::rotation::tests::own_test", "session::rotation::tests::own_two"],
                                     [self.DIRECT[0]], [self.DIRECT[1]]])
        self.assertTrue(subset["box_expired"])
        # Kept 5, executed 4 (partial truth); the thirty far tests stay deferred, never "not run".
        self.assertEqual(subset["bounded"], {self.BOUNDED_FILTER: [5, 4, 30]})
        self.assertEqual(subset["deferred_tests"], 30)
        coverage = RUNNER["filter_coverage"](plan, [subset])
        self.assertEqual(coverage["bounded"], [{"filter": self.BOUNDED_FILTER, "kept": 5, "executed": 4, "deferred": 30}])
        self.assertEqual(coverage["run"], [])
        self.assertEqual(sorted(coverage["not_run"]), sorted(
            [f"rsid=shard:session-03:test(={self.SCANNER})", *(f"rsid=shard:other-01:test(={n})" for n in self.EXACT_FAR)]))

    def test_a_changed_module_over_the_limit_is_not_ranked_by_what_the_bounded_filter_kept(self):
        # 301 tests would make the module a broad filter. Restricting the plan must not shrink it
        # below the limit and rank its tests ahead of the nearer direct observers.
        big = [f"session::big::tests::case_{n:03}" for n in range(301)]
        records = [{"package": "rsid", "module": "session::big", "path": "x", "excluded": [], "tests": 301}]
        result, calls, subset, _plan = self.bounded_run(60, extra=big, records=records)
        self.assertEqual(result, 0)
        order = [name for batch in calls for name in batch]
        self.assertEqual(order[:3], self.DIRECT)
        self.assertFalse([name for name in order if "session::big::" in name])
        # Kept: the three direct observers. Deferred: thirty far tests, the 301 and the unranked rotation pair.
        self.assertEqual(subset["bounded"][self.BOUNDED_FILTER], [3, 3, 333])
        self.assertEqual(subset["deferred_tests"], 333)

    def test_a_plan_without_a_bounded_filter_keeps_its_far_tests_selected(self):
        # The policy is the derivation's flag only: a broad filter without it still selects every match.
        out = io.StringIO()
        far = [f"session::agent_{n:02}" for n in range(30)]
        binary = self.executable("unbounded-run", f"print({chr(10).join(n + ': test' for n in far)!r} if '--list' in sys.argv else '')\n")
        broad = RUNNER["selection_plan"]([self.BOUNDED_FILTER], {"session-03": ["rsid"]})["rsid"]
        calls = []

        def run_batch(_binary, batch, *_args):
            calls.append(list(batch))
            return len(batch), 0, [], set(batch)

        with patch.dict(RUNNER["run_selected"].__globals__, run_batch=run_batch,
                        test_batches=lambda batch, *_limit: [list(batch)]), contextlib.redirect_stdout(out):
            self.assertEqual(RUNNER["run_selected"]({"package": "rsid", "selections": broad, "time_box_sec": 60},
                                                    {str(binary): ({"kind": ["lib"], "name": "rsid"}, str(self.root))}, None), 0)
        self.assertEqual(sum(len(batch) for batch in calls), 30)
        subset = json.loads(next(line for line in out.getvalue().splitlines()
                                 if line.startswith("SCOPED_SUBSET ")).split(" ", 1)[1])
        self.assertEqual((subset["deferred_tests"], subset["bounded"]), (0, {}))

    def test_a_direct_observer_no_plan_filter_selects_is_never_added_to_the_run(self):
        ranks = RUNNER["direct_observer_ranks"]
        with patch.dict(RUNNER["RECEIPT"]), contextlib.redirect_stdout(io.StringIO()):
            selections = RUNNER["direct_observer_plan"](
                {"direct_observer_filters": self.DIRECT_FILTERS + [
                    {"filter": "rsid=shard:session-03:test(=session::elsewhere::lonely)", "distance": 1}]},
                {"session-03": ["rsid"]})["rsid"]
        # Only the names the plan selected are ranked; a hint for a test no filter selects adds nothing.
        self.assertEqual(ranks(set(self.DIRECT[:2]), selections),
                         {self.DIRECT[0]: 1, self.DIRECT[1]: 2})
        # The nearest distance wins when two hints name the same test.
        again = selections + [{**selections[0], "distance": 5}]
        self.assertEqual(ranks({self.DIRECT[0]}, again), {self.DIRECT[0]: 1})
        tiers = RUNNER["specificity_tiers"](["a", "b", "c", "d"], {"a": 30, "b": 30, "c": 30, "d": 70},
                                            direct_ranks={"b": 2, "c": 1})
        self.assertEqual(tiers, [["c"], ["b"], ["a"], ["d"]])

    def test_changed_module_ranks_hold_only_a_modules_own_tests_and_skip_a_module_that_selects_too_many(self):
        ranks = RUNNER["changed_module_ranks"]
        lib = {"kind": ["lib"], "name": "rsid"}
        names = ["a::tests::one", "a::tests::two", "a::child::tests::three", "a::child::deep::four", "b::five"]
        records = [{"package": "rsid", "module": "a", "excluded": ["a::child"]}]
        self.assertEqual(ranks(names, records, lib), {"a::tests::one": 2, "a::tests::two": 2})
        # A nested changed module is smaller, so it ranks before the module around it.
        nested = records + [{"package": "rsid", "module": "a::child", "excluded": []}]
        self.assertEqual(ranks(names, nested, lib)["a::child::deep::four"], 2)
        self.assertEqual(ranks(names, nested, lib)["a::tests::one"], 2)
        # More selected tests than the limit: a broad filter, not the changed module's own tests.
        self.assertEqual(ranks(names, records, lib, limit=1), {})
        # A changed integration test or binary ranks all of its tests, and only for that target.
        target = [{"package": "rsid", "kind": "test", "target": "e2e"}]
        self.assertEqual(ranks(["x", "y"], target, {"kind": ["test"], "name": "e2e"}), {"x": 2, "y": 2})
        self.assertEqual(ranks(["x"], target, {"kind": ["test"], "name": "other"}), {})
        self.assertEqual(ranks(["x"], records, {"kind": ["test"], "name": "e2e"}), {})

    def test_added_tests_still_precede_changed_module_tests_and_specificity_orders_the_rest(self):
        tiers = RUNNER["specificity_tiers"](
            ["p::new", "m::a", "m::b", "n::x", "n::y"], {"p::new": 50, "m::a": 50, "m::b": 50, "n::x": 1, "n::y": 70},
            priority_tests={"p::new"}, changed_ranks={"m::a": 2, "m::b": 2, "p::new": 2})
        self.assertEqual(tiers, [["p::new"], ["m::a", "m::b"], ["n::x"], ["n::y"]])
        self.assertEqual(RUNNER["specificity_tiers"](["a", "b"], {"a": 1, "b": 70}), [["a"], ["b"]])

    def test_packages_that_own_a_changed_module_run_after_added_tests_and_before_the_rest(self):
        plan = RUNNER["selection_plan"](["rsid=module::", "rsid-store=exact::one"], {}, ["rsid-store=exact::one"])
        plan["rsi-common"] = [{"filter": "rsi-common=x", "kind": "lib", "target": "", "substring": "",
                               "exact": "a::b", "regex": None}]
        self.assertEqual([p for p, _ in RUNNER["package_order"](plan)], ["rsid-store", "rsi-common", "rsid"])
        self.assertEqual([p for p, _ in RUNNER["package_order"](plan, {"rsid"})], ["rsid-store", "rsid", "rsi-common"])

    def test_filter_coverage_counts_part_run_filters_and_never_claims_an_unlisted_package(self):
        def selection(name):
            return {"filter": name, "kind": "lib", "target": "", "substring": name}
        # The priority filters are carried by both packages; the second never listed (box spent).
        plan = {"rsid-store": [selection("p1"), selection("p2"), selection("broad")],
                "rsid": [selection("p1"), selection("p2"), selection("untouched")]}
        store = {"package": "rsid-store", "selected_tests": 8, "run_tests": 4, "box_expired": True,
                 "filters": {"p1": [1, 1], "p2": [2, 2], "broad": [5, 1]}}
        coverage = RUNNER["filter_coverage"](plan, [store])
        self.assertEqual(coverage["run"], ["p1", "p2"])
        self.assertEqual(coverage["partial"], [{"filter": "broad", "selected": 5, "executed": 1}])
        self.assertEqual((coverage["not_run"], coverage["unlisted_packages"]), (["untouched"], ["rsid"]))
        # A listed package whose filter selects nothing runnable has nothing left to run.
        both = RUNNER["filter_coverage"](plan, [store, {"package": "rsid", "filters": {}}])
        self.assertEqual((both["run"], both["not_run"], both["unlisted_packages"]),
                         (["p1", "p2", "untouched"], [], []))

    def test_report_subset_writes_run_partial_and_not_run_filters_to_the_receipt(self):
        _result, _calls, subset, plan = self.boxed_priority_run()
        out = io.StringIO()
        with patch.dict(RUNNER["RECEIPT"]), contextlib.redirect_stdout(out):
            RUNNER["report_subset"](plan, [subset], set(), False)
            receipt = dict(RUNNER["RECEIPT"])
        self.assertEqual((receipt["selected_filters"], receipt["run_filters"], receipt["box_expired"]), (4, 3, True))
        self.assertEqual((receipt["filters_not_run"], receipt["filters_not_run_total"], receipt["filters_partial"]),
                         (["rsid=broad::"], 1, []))
        self.assertEqual((receipt["selected_tests"], receipt["run_tests"]), (6, 3))
        self.assertIn("ran 3 of 4 filters completely (0 partial, 1 not run", out.getvalue())

    def test_changed_modules_from_the_derivation_reach_the_unit_plan_and_the_receipt(self):
        listing = "\n".join([f"rotation::case{n}: test" for n in range(14)] + ["other::unrelated: test"])
        self.binary = self.executable("test-binary", f"print({listing!r} if '--list' in sys.argv else '')\n")
        artifact = {"reason": "compiler-artifact", "profile": {"test": True},
                    "manifest_path": str(self.root / "Cargo.toml"),
                    "executable": str(self.binary), "target": {"kind": ["lib"], "name": "rsid_store"}}
        self.cargo = self.executable("cargo-stub", f"print({json.dumps(json.dumps(artifact))})\n")
        filters = [f"rsid=shard:store-01:test(case{n})" for n in range(14)] + ["rsid=shard:store-01:test(other::unrelated)"]
        changed = {"package": "rsid-store", "module": "rotation", "path": "crates/rsid-store/src/rotation.rs",
                   "excluded": [], "tests": 14}
        code, output = self.invoke(linux=False, seconds="5", entrypoint=True, filters=filters,
                                   derived_extra={"changed_modules": [changed]})
        self.assertEqual(code, 0, output)
        self.assertIn("scoped-test: tests of a changed module run before unrelated observers, smallest module first: rotation", output)
        receipt = self.receipt_of(output)
        self.assertEqual(receipt["changed_modules"], [{key: changed[key] for key in ("package", "module", "path", "tests")}])
        self.assertEqual((receipt["changed_module_tests_selected"], receipt["changed_module_tests_run"]), (14, 14))
        self.assertEqual((receipt["selected_tests"], receipt["run_tests"]), (15, 15))
        unit = json.loads((self.root / "output" / "rsid-store.json").read_text())
        self.assertEqual(unit["changed_modules"], [changed])

    def test_a_targeted_subset_run_reports_selected_and_run_filters(self):
        listing = "\n".join(f"case{n}: test" for n in range(14))
        self.binary = self.executable("test-binary", f"print({listing!r} if '--list' in sys.argv else '')\n")
        artifact = {"reason": "compiler-artifact", "profile": {"test": True},
                    "manifest_path": str(self.root / "Cargo.toml"),
                    "executable": str(self.binary), "target": {"kind": ["lib"], "name": "rsid_store"}}
        self.cargo = self.executable("cargo-stub", f"print({json.dumps(json.dumps(artifact))})\n")
        filters = [f"rsid=shard:store-01:test(case{n})" for n in range(14)]
        # `case1` also selects case10..case13, so the exact count is not the test count.
        code, output = self.invoke(linux=False, seconds="5", entrypoint=True, filters=filters)
        self.assertEqual(code, 0, output)
        receipt = self.receipt_of(output)
        self.assertEqual((receipt["plan"], receipt["selected_filters"], receipt["run_filters"]),
                         ("targeted_subset", 14, 14))
        self.assertEqual((receipt["box_expired"], receipt["filters_not_run"]), (False, []))
        self.assertEqual(receipt["selected_tests"], receipt["run_tests"])
        self.assertIn("the whole plan fit the time box", output)
        # A box that is already spent runs nothing, exits green, and names what the gate must cover.
        self.log.write_text("")
        shutil.rmtree(self.root / "output")
        code, output = self.invoke(linux=False, seconds="5", entrypoint=True, filters=filters,
                                   extra_args=["--test-time-box-sec", "0.0001"])
        self.assertEqual(code, 0, output)
        receipt = self.receipt_of(output)
        self.assertEqual((receipt["run_filters"], receipt["run_tests"], receipt["box_expired"]), (0, 0, True))
        self.assertEqual(receipt["filters_not_run_total"], 14)
        self.assertIn("merge-queue gate covers the rest", output)

    def test_a_spent_box_still_runs_the_changed_module_tests_of_a_later_package(self):
        """#3252: `rsid` spends the whole box on its priority tests and far observers; the `rsi` package that
        owns the changed module `settings_registry` still runs that module's tests, not the gate first."""
        def binary(name, tests):
            listing = "\n".join(f"{test}: test" for test in tests)
            return self.executable(name, "import time\nif '--list' in sys.argv:\n    time.sleep(0.4)\n"
                                         f"print({listing!r} if '--list' in sys.argv else 'test result: ok. 1 passed')\n")
        rsid = binary("rsid-binary", ["far::priority", *[f"far::observer_{n}" for n in range(4)]])
        registry = ["settings_registry::tests::no_section_holds_more_than_a_dozen_rows",
                    "settings_registry::tests::rows_are_unique"]
        rsi = binary("rsi-binary", [*registry, "unrelated::tests::far_observer"])
        self.cargo = self.executable("cargo-two-packages", f"""
import json
package = sys.argv[sys.argv.index('-p') + 1]
binary = {str(rsid)!r} if package == 'rsid' else {str(rsi)!r}
print(json.dumps({{'reason': 'compiler-artifact', 'profile': {{'test': True}},
                  'manifest_path': {str(self.root / 'Cargo.toml')!r},
                  'executable': binary, 'target': {{'kind': ['lib'], 'name': package}}}}))
""")
        filters = ["rsid=shard:session-01:test(/^far::/)", "rsi=settings_registry", "rsi=unrelated"]
        changed = {"package": "rsi", "module": "settings_registry", "path": "crates/rsi/src/settings_registry.rs",
                   "excluded": [], "tests": 2}
        code, output = self.invoke(
            linux=False, seconds="20", entrypoint=True, filters=filters,
            shard_packages={"session-01": ["rsid"]},
            extra_args=["--filter-budget", "1", "--test-time-box-sec", "0.1"],
            derived_extra={"changed_modules": [changed], "priority_filters": ["rsid=shard:session-01:test(far::priority)"]})
        self.assertEqual(code, 0, output)
        self.assertIn("rsi still runs the tests of its changed modules", output)
        self.assertNotIn("rsi left to the merge-queue gate", output)
        receipt = self.receipt_of(output)
        self.assertEqual((receipt["changed_module_tests_selected"], receipt["changed_module_tests_run"]), (2, 2))
        log = (self.root / "output" / "rsi.log").read_text()
        for test in registry:
            self.assertIn(f"SCOPED_END {test}", log)
        self.assertNotIn("SCOPED_END unrelated::tests::far_observer", log)
        self.assertTrue(receipt["box_expired"])
        # A changed module's own filter is run, never named as left for the gate.
        self.assertNotIn("rsi=settings_registry", receipt["filters_not_run"])
        self.assertNotIn("rsi", receipt.get("unlisted_packages", []))

    def test_batches_are_bounded_by_count_and_argv_size(self):
        names = [f"t{n:04}" for n in range(10)]
        self.assertEqual(list(RUNNER["test_batches"](names, max_tests=4)),
                         [names[:4], names[4:8], names[8:]])
        self.assertEqual(list(RUNNER["test_batches"](names, max_bytes=15)),
                         [names[:2], names[2:4], names[4:6], names[6:8], names[8:]])
        self.assertEqual(list(RUNNER["test_batches"]([])), [])

    def test_batch_progress_follows_libtest_start_lines_through_test_output(self):
        out = []
        progress = RUNNER["BatchProgress"](["a::one", "a::two"], out.append)
        progress.begin("a::one")
        progress.line("test a::one ... partial output without a newline")
        progress.line("ok\n")
        progress.line("test a::two ... ok\n")
        progress.line("test result: ok. 2 passed; 0 failed\n")
        self.assertEqual(progress.ran, 2)
        self.assertEqual([line.strip() for line in out if line.startswith("SCOPED_")],
                         ["SCOPED_START a::one", "SCOPED_END a::one",
                          "SCOPED_START a::two", "SCOPED_END a::two"])

    def test_nested_child_echo_of_a_listed_name_is_not_double_counted(self):
        """#2079: 5212 lines for 5211 names must still be a complete, clean batch."""
        names = ["a::before", "a::parent", "a::fixture", "a::after"]
        out = []
        progress = RUNNER["BatchProgress"](names, out.append)
        progress.begin(names[0])
        for line in (
            "test a::before ... ok\n",
            "test a::parent ... ",
            "running 1 test\n",
            "test a::fixture ... ok\n",  # the nested libtest child, listed in this batch too
            "test result: ok. 1 passed; 0 failed\n",
            "ok\n",
            "test a::fixture ... ignored\n",  # the real line for the same name
            "test a::after ... ok\n",
            "test result: ok. 3 passed; 0 failed; 1 ignored\n",
        ):
            progress.line(line)
        progress.finish_active()
        self.assertEqual((progress.ran, progress.missing(), progress.failures), (4, [], set()))

    def test_unknown_nested_test_lines_are_ignored_once_and_failed_ones_fail(self):
        names = ["a::one", "a::two"]
        out = []
        progress = RUNNER["BatchProgress"](names, out.append)
        progress.begin("a::one")
        progress.line("test child::x ... ok\n")
        progress.line("test child::x ... ok\n")
        progress.line("test a::two ... ok\n")
        progress.line("test child::y ... FAILED\n")
        progress.finish_active()
        self.assertEqual((progress.ran, progress.missing()), (2, []))
        self.assertEqual(sum("unselected test child::x" in line for line in out), 1)
        self.assertEqual(progress.failures, {"child::y"})

    def test_whole_batch_with_a_nested_echo_passes_and_unknown_failure_fails(self):
        for verdict, code, expect in (("ok", 0, (2, 0, [])), ("FAILED", 101, (2, 101, ["child::y"]))):
            binary = self.executable("nested-" + verdict, f"""
if '--list' in sys.argv:
    print('a::one: test')
    print('a::two: test')
else:
    print('test a::one ... ')
    print('test child::y ... {verdict}')
    print('test result: {verdict}. 1 passed; 0 failed')
    print('test a::one ... ok')
    print('test a::two ... ok')
    sys.exit({code})
""")
            with contextlib.redirect_stdout(io.StringIO()):
                outcome = RUNNER["run_batch"](
                    str(binary), ["a::one", "a::two"], str(self.root), {"SCOPED_TEST_LOG": str(self.log)})
            self.assertEqual(outcome[:3], expect)

    def test_nocapture_failure_summary_reports_exact_failed_test(self):
        binary = self.executable("multiline-failure", """
if '--list' in sys.argv:
    print('a::one: test')
    print('a::two: test')
else:
    print('test a::one ... panic output')
    print('thread panicked at a failed assertion')
    print('FAILED')
    print('test a::two ... ok')
    print('failures:')
    print('')
    print('failures:')
    print('    a::one')
    print('test result: FAILED. 1 passed; 1 failed')
    sys.exit(101)
""")
        selections = RUNNER["selection_plan"](["rsid=a::"], {})["rsid"]
        plan = {"package": "rsid", "selections": selections}
        artifacts = {str(binary): ({"kind": ["lib"], "name": "rsid"}, str(self.root))}
        out = io.StringIO()
        with contextlib.redirect_stdout(out):
            result = RUNNER["run_selected"](plan, artifacts, {"SCOPED_TEST_LOG": str(self.log)})
        self.assertEqual(result, 1)
        self.assertIn("SCOPED_FAILURE lib:rsid a::one", out.getvalue())
        self.assertIn("scoped-test: rsid executed 2 tests", out.getvalue())

    def test_successful_binary_failure_looking_output_keeps_a_clean_result(self):
        binary = self.executable("failure-looking-output", """
print('test a::one ... diagnostic output')
print('FAILED')
print('failures:')
print('    a::one')
print('test result: ok. 1 passed; 0 failed')
""")
        out = io.StringIO()
        with contextlib.redirect_stdout(out):
            ran, result, failures, _done = RUNNER["run_batch"](
                str(binary), ["a::one"], str(self.root), {"SCOPED_TEST_LOG": str(self.log)})
        self.assertEqual((ran, result, failures), (1, 0, []))

    def test_unterminated_libtest_start_line_names_the_blocked_test(self):
        """#196: `test b ... ` has no newline until b finishes, so the hung test is b, not a."""
        release = self.root / "release-b"
        binary = self.executable("blocked-b", f"""
import time
if '--list' in sys.argv:
    print('a: test')
    print('b: test')
else:
    sys.stdout.write('test a ... ok\\n')
    sys.stdout.write('test b ... ')
    sys.stdout.flush()
    release = Path({str(release)!r})
    for _ in range(3000):
        if release.exists():
            break
        time.sleep(0.01)
    else:
        sys.exit(3)
    print('ok')
    print('test result: ok. 2 passed; 0 failed')
""")
        seen = []

        def emit(line):
            seen.append(line)
            if line == "SCOPED_START b\n":
                # The child is still blocked on `release`: b is the live test.
                self.assertFalse(release.exists())
                release.touch()
            print(line, end="")

        with patch.dict(RUNNER["run_batch"].__globals__, emit_line=emit), \
                contextlib.redirect_stdout(io.StringIO()):
            ran, result, failures, done = RUNNER["run_batch"](
                str(binary), ["a", "b"], str(self.root), {"SCOPED_TEST_LOG": str(self.log)})
        self.assertEqual((ran, result, failures, done), (2, 0, [], {"a", "b"}))
        self.assertEqual([line.strip() for line in seen if line.startswith("SCOPED_")],
                         ["SCOPED_START a", "SCOPED_END a", "SCOPED_START b", "SCOPED_END b"])
        self.assertEqual(seen.count("test b ... ok\n"), 1)

    def test_partial_line_start_is_parsed_without_a_second_emit_or_count(self):
        out = []
        progress = RUNNER["BatchProgress"](["a", "b"], out.append)
        progress.begin("a")
        progress.partial("test a ... ok\n")  # a is the active test: not a new start
        progress.partial("test x ... ")  # unselected names are ignored
        progress.partial("test b")  # too short to be a start line yet
        self.assertEqual((progress.active, progress.ran), ("a", 0))
        progress.partial("test b ... ")
        self.assertEqual((progress.active, progress.ran), ("b", 1))
        progress.line("test b ... ok\n")
        progress.partial("test b ... ")  # b is finished
        progress.finish_active()
        self.assertEqual((progress.ran, progress.missing()), (2, []))
        self.assertEqual(out.count("SCOPED_START b\n"), 1)
        self.assertEqual(out.count("SCOPED_END b\n"), 1)

    def slot_stub(self, delay, grant=True):
        grant_line = "print('rsi-phase 1 slot-granted: waited %ds (build); running' % DELAY, file=sys.stderr, flush=True)\n"
        return self.executable("cargo-slot", f"""
import time
DELAY = {delay}
time.sleep(DELAY)
{grant_line if grant else ''}os.environ['RSI_CARGO_SLOT_HELD'] = '1'
os.execv(sys.argv[1], sys.argv[1:])
""")

    def shared_slot_stub(self):
        return self.executable("cargo-slot", """
import subprocess
marker = Path(os.environ['SCOPED_TEST_LOG'] + '.held')
assert not marker.exists(), 'nested admission'
marker.touch()
print('rsi-phase 1 slot-granted: running', file=sys.stderr, flush=True)
try:
    env = dict(os.environ, RSI_CARGO_SLOT_HELD='1', CARGO_BUILD_JOBS='4',
               CARGO_PROFILE_DEV_DEBUG='line-tables-only')
    result = subprocess.run(sys.argv[1:], env=env)
finally:
    marker.unlink()
    with Path(os.environ['SCOPED_TEST_LOG']).open('a') as log:
        log.write('slot released\\n')
sys.exit(result.returncode)
""")

    def assert_shared_slot_released(self):
        commands = self.log.read_text().splitlines()
        self.assertEqual(sum(line.startswith('cargo-slot ') for line in commands), 1, commands)
        self.assertEqual(commands[-1], 'slot released')
        self.assertFalse(Path(str(self.log) + '.held').exists())
        self.assertNotIn('RSI_CARGO_SLOT_HELD', os.environ)

    def test_connected_packages_hold_one_admission_and_keep_bounded_units(self):
        artifact = {"reason": "compiler-artifact", "profile": {"test": True},
                    "manifest_path": str(self.root / "Cargo.toml"),
                    "executable": str(self.binary), "target": {"kind": ["lib"], "name": "rsid_store"}}
        self.cargo = self.executable("cargo-held", f"""
assert Path(os.environ['SCOPED_TEST_LOG'] + '.held').exists()
assert os.environ['RSI_CARGO_SLOT_HELD'] == '1'
assert os.environ['CARGO_BUILD_JOBS'] == '4'
assert os.environ['CARGO_PROFILE_DEV_DEBUG'] == 'line-tables-only'
print({json.dumps(json.dumps(artifact))})
""")
        systemd = """
assert Path(os.environ['SCOPED_TEST_LOG'] + '.held').exists()
assert '--slice=rsi-build.slice' in sys.argv
assert 'CPUQuota=75%' in sys.argv
assert 'RuntimeMaxSec=5' in sys.argv
assert '--setenv=RSI_CARGO_SLOT_HELD' in sys.argv
index = sys.argv.index('--execute-plan')
os.execv(sys.executable, [sys.executable, '-u', sys.argv[index - 1], *sys.argv[index:]])
"""
        code, output = self.invoke(seconds='5', slot=self.shared_slot_stub(), systemd_body=systemd,
                                   shard_packages={'store-01': ['rsid-store', 'rsid']},
                                   extra_args=['--job-max-sec', '3600'], entrypoint=True)
        self.assertEqual(code, 0, output)
        self.assert_shared_slot_released()
        receipt = self.receipt_of(output)
        self.assertEqual(receipt['admission']['count'], 1)
        self.assertEqual([(p['package'], p['status'], p['completed']) for p in receipt['packages']],
                         [('rsid-store', 'passed', 1), ('rsid', 'passed', 1)])
        self.assertGreater(receipt['packages'][0]['slot_wait_sec'], 0)
        self.assertEqual(receipt['packages'][1]['slot_wait_sec'], 0)
        self.assertTrue(all(p['run_sec'] > 0 for p in receipt['packages']))
        self.assertEqual(receipt['admission']['waited_sec'], receipt['packages'][0]['slot_wait_sec'])
        commands = self.log.read_text().splitlines()
        self.assertEqual(sum(line.startswith('systemd-run ') for line in commands), 2)

    def test_existing_outer_admission_runs_connected_packages_without_another_slot(self):
        slot = self.executable('cargo-slot', "raise AssertionError('nested admission')")
        code, output = self.invoke(linux=False, seconds='5', slot=slot, slot_held=True,
                                   shard_packages={'store-01': ['rsid-store', 'rsid']}, entrypoint=True)
        self.assertEqual(code, 0, output)
        self.assertEqual([(p['package'], p['completed']) for p in self.receipt_of(output)['packages']],
                         [('rsid-store', 1), ('rsid', 1)])
        self.assertEqual(sum(line.startswith('cargo-slot ') for line in self.log.read_text().splitlines()), 0)

    def test_connected_package_dry_run_takes_no_admission(self):
        self.log.write_text('')
        slot = self.executable('cargo-slot', "raise AssertionError('dry-run admission')")
        code, output = self.invoke(linux=False, seconds='5', slot=slot, dry_run=True,
                                   shard_packages={'store-01': ['rsid-store', 'rsid']})
        self.assertEqual(code, 0, output)
        self.assertIn('rsid filters:', output)
        self.assertEqual(sum(line.startswith('cargo-slot ') for line in self.log.read_text().splitlines()), 0)

    def test_admission_holder_reports_only_build_settings(self):
        result = subprocess.run([sys.executable, str(SCRIPT), '--hold-admission'], input='',
                                capture_output=True, text=True,
                                env=dict(os.environ, RSI_CARGO_SLOT_HELD='1', RSI_SESSION_TOKEN='test-only'))
        self.assertEqual(result.returncode, 0, result.stderr)
        settings = json.loads(result.stdout.removeprefix(RUNNER['BuildAdmission'].PREFIX))
        self.assertEqual(settings['RSI_CARGO_SLOT_HELD'], '1')
        self.assertNotIn('RSI_SESSION_TOKEN', settings)

    def test_shared_admission_releases_after_build_failure(self):
        self.cargo = self.executable('cargo-broken', 'sys.exit(1)')
        code, output = self.invoke(linux=False, seconds='5', slot=self.shared_slot_stub(),
                                   shard_packages={'store-01': ['rsid-store', 'rsid']}, entrypoint=True)
        self.assertEqual(code, 1, output)
        self.assert_shared_slot_released()
        self.assertEqual(self.receipt_of(output)['packages'][0]['phase'], 'build_failed')
        self.assertEqual(sum(line.startswith('cargo-broken ') for line in self.log.read_text().splitlines()), 1)

    def test_shared_admission_releases_on_parent_exception(self):
        admission = RUNNER['BuildAdmission']()
        budget = RUNNER['Budget'](5, queued=True)
        with self.assertRaisesRegex(RuntimeError, 'parent stopped'):
            with admission:
                self.assertEqual(admission.acquire([str(self.shared_slot_stub())], lambda line: None,
                                                   budget, 5, None), 0)
                raise RuntimeError('parent stopped')
        self.assert_shared_slot_released()

    def test_shared_admission_releases_after_package_timeout(self):
        self.binary = self.executable('test-binary', """
if '--list' in sys.argv:
    print('store::migration: test')
else:
    import time
    time.sleep(30)
""")
        artifact = {"reason": "compiler-artifact", "profile": {"test": True},
                    "manifest_path": str(self.root / "Cargo.toml"),
                    "executable": str(self.binary), "target": {"kind": ["lib"], "name": "rsid_store"}}
        self.cargo = self.executable('cargo-timeout', f'print({json.dumps(json.dumps(artifact))})\n')
        code, output = self.invoke(linux=False, seconds='1', slot=self.shared_slot_stub(), entrypoint=True)
        self.assertEqual(code, 1, output)
        self.assertIn('scoped_test_timeout:', output)
        self.assert_shared_slot_released()

    def test_time_queued_for_a_build_slot_does_not_count_against_the_budget(self):
        """#1756: 616 s of queueing used to be spent from the job's 20 minutes."""
        slot = self.slot_stub(2.5)
        code, output = self.invoke(linux=False, seconds="1.5", slot=slot)
        self.assertEqual(code, 0, output)
        self.assertRegex(output, r"rsid-store: waited [23]s for a build slot \(not counted against the budget\)")
        self.assertIn("rsid-store: passed (1 completed tests)", output)
        self.assertNotIn("scoped_test_timeout", output)

    def test_daemon_job_slot_wait_uses_its_separate_admission_allowance(self):
        slot = self.executable("cargo-slot", """
cap = int(os.environ['RSI_CARGO_ADMISSION_MAX_SEC'])
assert 3500 < cap <= 3600, cap
print('rsi-phase 1 slot-granted: running', file=sys.stderr, flush=True)
os.environ['RSI_CARGO_SLOT_HELD'] = '1'
os.execv(sys.argv[1], sys.argv[1:])
""")
        code, output = self.invoke(linux=False, seconds="5", slot=slot,
                                   extra_args=["--job-max-sec", "3600", "--slot-wait-max-sec", "3600"])
        self.assertEqual(code, 0, output)
        self.assertIn("rsid-store: passed (1 completed tests)", output)
        self.assertRegex(output, r"rsid-store: waited \d+s for a build slot \(not counted against the budget\)")

    def test_daemon_job_slot_wait_defaults_to_the_job_allowance(self):
        """#3284: a daemon that passes only --job-max-sec still gets the hour, not 600s."""
        slot = self.executable("cargo-slot", """
assert int(os.environ['RSI_CARGO_ADMISSION_MAX_SEC']) == 3600
print('rsi-phase 1 slot-granted: running', file=sys.stderr, flush=True)
os.environ['RSI_CARGO_SLOT_HELD'] = '1'
os.execv(sys.argv[1], sys.argv[1:])
""")
        code, output = self.invoke(linux=False, seconds="5", slot=slot, extra_args=["--job-max-sec", "3600"])
        self.assertEqual(code, 0, output)

    def test_daemon_admission_timeout_has_a_typed_phase_and_red_receipt(self):
        # The wrapper's lock wait leaves only one second of admission allowance.
        code, output = self.invoke(linux=False, seconds="5", slot=self.slot_stub(30, grant=False),
                                   extra_args=["--job-max-sec", "300", "--slot-wait-max-sec", "180"],
                                   entrypoint=True, job_started_at=str(time.time() - 179))
        self.assertEqual(code, 124, output)
        self.assertIn("admission-timeout: no build slot within separate admission allowance", output)
        receipt = self.receipt_of(output)
        self.assertEqual((receipt["ok"], receipt["exit_code"]), (False, 124))

    def test_granted_slot_gets_the_full_execution_budget_after_35_minutes_queued(self):
        with patch("time.monotonic", return_value=100):
            budget = RUNNER["Budget"](3600, queued=True, wait_cap=3600, deadline=3700)
        with patch("time.monotonic", return_value=2200):
            budget.observe("rsi-phase 2200 slot-granted: running")
            self.assertEqual(budget.waited, 2100)
            self.assertEqual(budget.remaining(), 3600)
        with patch("time.monotonic", return_value=5799):
            self.assertEqual(budget.remaining(), 1)
        with patch("time.monotonic", return_value=5800):
            self.assertEqual(budget.remaining(), 0)

    def test_slot_wait_is_excluded_from_the_job_deadline(self):
        """#3284: a 2736s queue used to leave 804s of a 3540s deadline; it now leaves all of it."""
        with patch("time.monotonic", return_value=100):
            budget = RUNNER["Budget"](4000, queued=True, wait_cap=3600, deadline=100 + 3540)
        with patch("time.monotonic", return_value=100 + 2736):
            # Still queued: only the wait cap bounds the queue, not the job deadline.
            self.assertEqual(budget.remaining(), 3600 - 2736)
            budget.observe("rsi-phase 2836 slot-granted: waited 2736s (build); running")
            self.assertEqual(budget.waited, 2736)
            self.assertEqual(budget.remaining(), 3540)
        with patch("time.monotonic", return_value=100 + 2736 + 3540):
            self.assertEqual(budget.remaining(), 0)
        with patch("time.monotonic", return_value=100 + 2736 + 3500):
            self.assertEqual(budget.remaining(), 40)

    def test_slot_wait_is_capped_as_a_whole_not_by_the_job_deadline(self):
        with patch("time.monotonic", return_value=100):
            budget = RUNNER["Budget"](3480, queued=True, wait_cap=600, deadline=100 + 300)
        with patch("time.monotonic", return_value=100 + 599):
            self.assertEqual(budget.remaining(), 1)
        with patch("time.monotonic", return_value=100 + 600):
            self.assertEqual(budget.remaining(), 0)
            self.assertFalse(budget.ran_out())

    def test_an_unqueued_budget_keeps_the_job_deadline_unshifted(self):
        with patch("time.monotonic", return_value=100):
            budget = RUNNER["Budget"](4000, deadline=100 + 3540)
        with patch("time.monotonic", return_value=100 + 3000):
            self.assertEqual(budget.remaining(), 540)

    def test_daemon_job_lock_and_slot_wait_leave_budget_for_all_packages_and_triage(self):
        clock = ControlledClock()
        acquire = RUNNER["BuildAdmission"].acquire
        compare = RUNNER["compare_failures_with_base"]
        comparison_budgets = []

        def held_admission(admission, *args):
            if admission.process is None:
                clock.advance(35 * 60)
            return acquire(admission, *args)

        def compare_after_wait(repo, base, failures, log_dir, seconds, *args):
            comparison_budgets.append(seconds)
            return compare(repo, base, failures, log_dir, seconds, *args)

        with patch.dict(RUNNER["main"].__globals__, time=clock,
                        compare_failures_with_base=compare_after_wait), \
             patch.object(RUNNER["BuildAdmission"], "acquire", held_admission):
            code, output = self.invoke(
                linux=False, seconds="3480", total="3540", slot=self.shared_slot_stub(),
                filters=["rsid-store=store::migration", "rsi-common=store::migration"],
                extra_args=["--job-max-sec", "3600", "--slot-wait-max-sec", "3600"],
                job_started_at=str(time.time() - 179))
        self.assertEqual(code, 0, output)
        receipt = RUNNER["RECEIPT"]
        self.assertEqual([item["status"] for item in receipt["packages"]], ["passed", "passed"])
        self.assertEqual(comparison_budgets, [3540])
        self.assertGreaterEqual(receipt["admission"]["startup_wait_sec"], 179)
        self.assertGreaterEqual(receipt["admission"]["waited_sec"], 2279)
        self.assertEqual(receipt["admission"]["excluded_from_execution"], True)

    def test_python_only_daemon_diff_runs_without_acquiring_a_slot(self):
        tests = self.root / "scripts/tests"
        tests.mkdir(parents=True)
        (tests / "test_widget.py").write_text(
            "import unittest\nclass Widget(unittest.TestCase):\n"
            "    def test_widget(self): self.assertEqual(1 + 1, 2)\n")
        slot = self.executable("cargo-slot", "raise AssertionError('unexpected slot acquisition')")
        self.log.write_text("")
        code, output = self.invoke(linux=False, filters=[], python_paths=["scripts/widget.py"],
                                   slot=slot, extra_args=["--job-max-sec", "3600"], entrypoint=True)
        self.assertEqual(code, 0, output)
        receipt = self.receipt_of(output)
        self.assertEqual(self.core_packages(receipt), [])
        self.assertEqual(receipt["python"], [{"test_file": "scripts/tests/test_widget.py", "status": "passed",
                                             "completed": True, "runner": "unittest", "executed_tests": 1}])
        commands = self.log.read_text()
        self.assertEqual([line for line in commands.splitlines() if line.startswith("cargo-slot ")], [])

    def test_the_budget_still_applies_once_the_slot_is_granted(self):
        self.binary = self.executable("test-binary", """
if '--list' in sys.argv:
    print('store::migration: test')
else:
    import time
    time.sleep(30)
""")
        # A 1s budget can expire before the python stub binary has listed and started its test on a
        # loaded host (the receipt then says "none (build/list/startup)"); 5s still times out the 30s sleep.
        code, output = self.invoke(linux=False, seconds="5", slot=self.slot_stub(0.5))
        self.assertEqual(code, 1, output)
        self.assertIn("scoped_test_timeout: slowest still-running tests: store::migration (", output)

    def test_default_admission_deadline_is_separate_from_execution(self):
        with patch("time.monotonic", return_value=100):
            budget = RUNNER["Budget"](60, queued=True)
        with patch("time.monotonic", return_value=699):
            self.assertEqual(budget.remaining(), 1)
            self.assertFalse(budget.ran_out())
            budget.observe("rsi-phase 699 slot-granted: waited 599s (build); running")
            self.assertEqual(budget.remaining(), 60)
        with patch("time.monotonic", return_value=759):
            self.assertTrue(budget.ran_out())
        with patch("time.monotonic", return_value=100):
            queued = RUNNER["Budget"](60, queued=True)
        with patch("time.monotonic", return_value=700):
            self.assertEqual(queued.remaining(), 0)
            self.assertFalse(queued.ran_out())

    def test_a_slot_that_is_never_granted_stops_at_the_wait_cap(self):
        code, output = self.invoke(linux=False, seconds="5", slot=self.slot_stub(30, grant=False),
                                   extra_args=["--slot-wait-max-sec", "1"])
        self.assertEqual(code, 1, output)
        self.assertIn("scoped_test_slot_wait_timeout: no build slot after 1s", output)
        self.assertNotIn("scoped_test_timeout: slowest", output)

    def test_e2e_target_runs_with_flag_set_after_prebuilding_rsid(self):
        """#1596: e2e_tui tests no-op without RSI_E2E=1 and a built rsid."""
        binary = self.executable("e2e-binary", '''
if '--list' in sys.argv:
    print('tests::settings_layout: test')
else:
    with Path(os.environ['SCOPED_TEST_LOG']).open('a') as log:
        log.write('RSI_E2E=' + os.environ.get('RSI_E2E', 'unset') + '\\n')
''')
        artifact = {"reason": "compiler-artifact", "profile": {"test": True},
                    "manifest_path": str(self.root / "Cargo.toml"),
                    "executable": str(binary), "target": {"kind": ["test"], "name": "e2e_tui"}}
        cargo = self.executable("cargo-e2e", f"print({json.dumps(json.dumps(artifact))})\n")
        selections = RUNNER["selection_plan"](["rsi=test:e2e_tui:test(=tests::settings_layout)"],
                                              {})["rsi"]
        self.assertTrue(RUNNER["selects_e2e"](selections))
        other = RUNNER["selection_plan"](["rsi=test:flow"], {})["rsi"]
        self.assertFalse(RUNNER["selects_e2e"](other))
        plan = {"repo": str(self.root), "package": "rsi", "selections": selections,
                "build": RUNNER["build_command"]("rsi", selections, [str(cargo)]),
                "e2e": {"prebuild": RUNNER["e2e_prebuild_command"]([str(cargo)])}}
        path = self.root / "plan.json"
        path.write_text(json.dumps(plan))
        env = {key: value for key, value in os.environ.items() if key != "RSI_E2E"}
        result = subprocess.run([sys.executable, str(SCRIPT), "--execute-plan", str(path)],
                                capture_output=True, text=True, env=env)
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        lines = self.log.read_text().splitlines()
        self.assertIn("cargo-e2e build -p rsid -p rsi-common -p rsi-turn-shim --bin rsid --bin rsi-agent-mcp --bin rsi-rpc --bin rsi-turn-shim", lines)
        self.assertIn("RSI_E2E=1", lines)

    def test_e2e_dry_run_reports_the_rsid_prebuild_for_e2e_filters(self):
        code, output = self.invoke(dry_run=True, explicit=["rsi=test:e2e_tui:test(=tests::one)"])
        self.assertEqual(code, 0, output)
        self.assertIn("e2e prebuild (RSI_E2E=1): ", output)
        self.assertIn("build -p rsid -p rsi-common -p rsi-turn-shim --bin rsid --bin rsi-agent-mcp", output)

    def test_non_e2e_dry_run_has_no_rsid_prebuild(self):
        code, output = self.invoke(dry_run=True, explicit=["rsi=test:flow"])
        self.assertEqual(code, 0, output)
        self.assertNotIn("e2e prebuild", output)

    def test_tmpfs_unavailable_is_reported_without_failing_or_building(self):
        result = subprocess.run([sys.executable, str(SCRIPT), "--tmpfs", "--dry-run",
                                 "--filter", "rsid-store=migration"],
                                capture_output=True, text=True,
                                env={**os.environ, "CHECK_TOUCHED_SHARDS_TMPDIR": str(self.root / "missing")})
        if sys.platform.startswith("linux"):
            self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
            self.assertIn("TMPFS_PARITY unavailable", result.stdout)
            self.assertNotIn("build once", result.stdout)

    def test_build_once_and_deduplicate_tests_across_filters(self):
        path = self.root / "plan.json"
        path.write_text(json.dumps(self.plan()))
        result = subprocess.run([sys.executable, str(SCRIPT), "--execute-plan", str(path)],
                                capture_output=True, text=True)
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertEqual(self.log.read_text().splitlines(), [
            'cargo-stub test -p rsid-store --lib --no-run --message-format=json',
            'test-binary --list --format=terse',
            'test-binary store::migration --exact --nocapture --test-threads=1'])
        self.assertIn('executed 1 tests', result.stdout)
        self.assertEqual(result.stdout.count('SCOPED_MATCH '), 2)

    def test_exact_failure_rerun_selects_only_the_named_candidate_failure(self):
        names = ['store::migration', 'store::unrelated']
        self.binary = self.executable("exact-binary", f"""
names = {names!r}
if '--list' in sys.argv:
    for name in names:
        print(name + ': test')
else:
    asked = [arg for arg in sys.argv[1:] if not arg.startswith('--')]
    assert asked == ['store::migration'], asked
    print('test store::migration ... ', end='', flush=True)
    print('FAILED')
    sys.exit(1)
""")
        artifact = {"reason": "compiler-artifact", "profile": {"test": True},
                    "manifest_path": str(self.root / "Cargo.toml"),
                    "executable": str(self.binary), "target": {"kind": ["lib"], "name": "rsid_store"}}
        self.cargo = self.executable("exact-cargo", f"print({json.dumps(json.dumps(artifact))})\n")
        plan = self.plan()
        plan["exact_tests"] = ["store::migration"]
        path = self.root / "baseline-plan.json"
        path.write_text(json.dumps(plan))
        result = subprocess.run([sys.executable, str(SCRIPT), "--execute-plan", str(path)],
                                capture_output=True, text=True)
        self.assertEqual(result.returncode, 1, result.stdout + result.stderr)
        self.assertIn("SCOPED_FAILURE lib:rsid_store store::migration", result.stdout)
        self.assertNotIn("store::unrelated", self.log.read_text())

    def test_base_comparison_reports_only_failures_reproduced_on_the_base(self):
        repo = self.scratch_repo()
        scratch_git(repo, "init", "--quiet")
        scratch_git(repo, "config", "user.name", "Test")
        scratch_git(repo, "config", "user.email", "test@example.invalid")
        (repo / "tracked").write_text("base\n")
        scratch_git(repo, "add", "tracked")
        scratch_git(repo, "commit", "--quiet", "-m", "base")
        base = scratch_git(repo, "rev-parse", "HEAD").stdout.strip()
        log_dir = self.root / "comparison-logs"
        log_dir.mkdir()
        def command(_plan, _path, _seconds, _quota, _linux, env=None):
            self.assertEqual(_plan["tmpfs"], str(self.root / "shm"))
            return [sys.executable], None

        def stream(_command, emit, **_kwargs):
            emit("SCOPED_FAILURE lib:rsid_store store::migration\n")
            emit("scoped-test: rsid-store executed 1 tests\n")
            return 1

        globals_ = RUNNER["compare_failures_with_base"].__globals__
        with patch.dict(os.environ, CARGO_TARGET_DIR=str(self.root / "target")), \
             patch.dict(globals_, bounded_command=command, stream=stream):
            result = RUNNER["compare_failures_with_base"](
                repo, base,
                {"rsid-store": [{"kind": "lib", "target": "rsid_store", "test": "store::migration"}]},
                log_dir, 30, 200, False, [str(self.cargo)], tmpfs=str(self.root / "shm"))
        self.assertEqual(result["status"], "complete")
        self.assertEqual(result["baseline_reds"], [{"package": "rsid-store", "kind": "lib",
                                                     "target": "rsid_store", "test": "store::migration"}])

    def test_base_comparison_plan_sets_up_e2e_for_e2e_tui_failures(self):
        """#3321: the base rerun of an e2e_tui failure needs RSI_E2E=1 and the rsid prebuild."""
        fn = RUNNER["compare_failures_with_base"]
        plans = []
        for target, expect_e2e in (("e2e_tui", True), ("flow", False)):
            log_dir = self.root / f"e2e-base-{target}"
            log_dir.mkdir()

            def git(command, **_kwargs):
                if command[:3] == ["git", "worktree", "add"]:
                    Path(command[-2]).mkdir()
                return subprocess.CompletedProcess(command, 0, "", "")

            def command(plan, _path, _seconds, _quota, _linux, env=None):
                plans.append(plan)
                return [sys.executable], None

            def stream(_command, emit, **_kwargs):
                emit("scoped-test: rsi executed 1 tests\n")
                return 0

            failure = {"kind": "test", "target": target, "test": "issue_jump::one"}
            with patch.dict(fn.__globals__, stream=stream, bounded_command=command,
                            seed_baseline_target=lambda *_: None), \
                 patch("subprocess.run", side_effect=git):
                fn(self.root, "base", {"rsi": [failure]}, log_dir, 30, 200, False,
                   [str(self.cargo)])
            plan = plans[-1]
            self.assertEqual("e2e" in plan, expect_e2e, target)
            if expect_e2e:
                self.assertEqual(plan["e2e"]["prebuild"],
                                 RUNNER["e2e_prebuild_command"]([str(self.cargo)]))

    def test_base_red_requires_a_failing_base_run_and_an_observed_named_failure(self):
        failure = {"kind": "lib", "target": "rsid_store", "test": "store::migration"}
        fn = RUNNER["compare_failures_with_base"]
        for code, lines in (
                (0, ["SCOPED_FAILURE lib:rsid_store store::migration\n"]),
                (1, []),
                (1, ["SCOPED_FAILURE lib:rsid_store store::unrelated\n"])):
            with self.subTest(code=code, lines=lines):
                log_dir = self.root / f"base-{len(list(self.root.glob('base-*')))}"
                log_dir.mkdir()

                def git(command, **_kwargs):
                    if command[:3] == ["git", "worktree", "add"]:
                        Path(command[-2]).mkdir()
                    return subprocess.CompletedProcess(command, 0, "", "")

                def stream(_command, emit, **_kwargs):
                    for line in [*lines, "scoped-test: rsid-store executed 1 tests\n"]:
                        emit(line)
                    return code

                with patch.dict(fn.__globals__, stream=stream, seed_baseline_target=lambda *_: None), \
                     patch("subprocess.run", side_effect=git):
                    result = fn(self.root, "base", {"rsid-store": [failure]}, log_dir,
                                30, 200, False, [str(self.cargo)])
                self.assertEqual(result["baseline_reds"], [])

    def test_baseline_red_without_known_failure_record_is_explicit_in_receipt(self):
        failure = {"package": "rsid-store", "kind": "lib", "target": "rsid_store",
                   "test": "store::migration"}
        comparison = {"status": "complete", "base": "origin/rolling", "baseline_reds": [failure]}
        fn = RUNNER["main"]
        with patch.dict(fn.__globals__,
                        compare_failures_with_base=lambda *_: comparison,
                        split_stale_base_failures=lambda *_: ([], {}),
                        query_failure_records=lambda *_: {"status": "unknown", "records": []}):
            code, output = self.invoke(systemd_body="""
print('SCOPED_MATCH rsid=shard:store-01:test(store::migration)')
print('SCOPED_START store::migration')
print('SCOPED_END store::migration')
print('SCOPED_FAILURE lib:rsid_store store::migration')
print('SCOPED_PHASE listed')
print('SCOPED_PHASE test_failed')
sys.exit(1)
""")
        self.assertEqual(code, 1, output)
        receipt = RUNNER["RECEIPT"]
        self.assertEqual(receipt["baseline_reds_without_record"], [failure])
        self.assertEqual(receipt["baseline_failure_records"], [
            {**failure, "status": "unknown", "records": []}])
        self.assertEqual(receipt["new_failures"], [])
        self.assertIn("baseline red without known-failure record: rsid-store store::migration", output)
        # A loaded snapshot with no record for the red keeps the run red, as before #183.
        self.assertFalse(self.emitted_receipt(code)["ok"])

    def emitted_receipt(self, code):
        with contextlib.redirect_stdout(io.StringIO()) as out:
            RUNNER["emit_receipt"](code)
        return json.loads(out.getvalue().splitlines()[-1].removeprefix("SCOPED_TEST_RECEIPT "))

    def run_with_baseline(self, comparison, record):
        fn = RUNNER["main"]
        with patch.dict(fn.__globals__,
                        compare_failures_with_base=lambda *_: comparison,
                        split_stale_base_failures=lambda *_: ([], {}),
                        query_failure_records=lambda *_: record):
            code, output = self.invoke(systemd_body="""
print('SCOPED_MATCH rsid=shard:store-01:test(store::migration)')
print('SCOPED_START store::migration')
print('SCOPED_END store::migration')
print('SCOPED_FAILURE lib:rsid_store store::migration')
print('SCOPED_PHASE listed')
print('SCOPED_PHASE test_failed')
sys.exit(1)
""")
        return code, output, self.emitted_receipt(code)

    def test_baseline_confirmed_red_without_snapshot_is_ok_with_its_reason(self):
        failure = {"package": "rsid-store", "kind": "lib", "target": "rsid_store", "test": "store::migration"}
        missing = {"status": "unavailable", "source": "snapshot",
                   "reason": "cannot read snapshot fixture/known-failures.v1.json: No such file or directory"}
        code, output, receipt = self.run_with_baseline(
            {"status": "complete", "base": "origin/rolling", "baseline_reds": [failure]}, missing)
        self.assertEqual(code, 1, output)
        self.assertEqual((receipt["exit_code"], receipt["ok"]), (1, True))
        self.assertEqual(receipt["ok_reason"], "baseline_confirmed_without_snapshot")
        self.assertEqual(receipt["new_failures"], [])
        self.assertEqual(receipt["baseline_failure_records"], [{**failure, **missing}])
        self.assertIn("scoped-test: ok: every failure is a baseline red (baseline_confirmed_without_snapshot)",
                      output)

    def test_baseline_red_with_an_active_record_is_ok_with_its_reason(self):
        failure = {"package": "rsid-store", "kind": "lib", "target": "rsid_store", "test": "store::migration"}
        record = {"issue": 183, "class": "regression", "test_id": "store::migration"}
        code, output, receipt = self.run_with_baseline(
            {"status": "complete", "base": "origin/rolling", "baseline_reds": [failure]},
            {"status": "recorded", "source": "snapshot", "records": [record]})
        self.assertEqual(code, 1, output)
        self.assertEqual((receipt["ok"], receipt["ok_reason"]), (True, "baseline_known_failure"))

    @staticmethod
    def core_packages(receipt):
        """The package entries without the per-package failure lists #199 added."""
        drop = ("failed_tests", "new_failures", "baseline_reds")
        return [{k: v for k, v in entry.items() if k not in drop} for entry in receipt["packages"]]

    def run_two_failures(self, reds, record):
        """A package where two tests fail and 7 ran, only 2 of them as SCOPED_START/END pairs (#199)."""
        fn = RUNNER["main"]
        # The module-level receipt outlives one run: keep an earlier test's unmatched filters out.
        with patch.dict(RUNNER["RECEIPT"]), patch.dict(
                fn.__globals__,
                compare_failures_with_base=lambda *_: {
                    "status": "complete", "base": "origin/rolling", "baseline_reds": reds},
                split_stale_base_failures=lambda *_: ([], {}),
                query_failure_records=lambda *_: record):
            RUNNER["RECEIPT"].pop("unmatched_filters", None)
            code, output = self.invoke(systemd_body="""
print('SCOPED_MATCH rsid=shard:store-01:test(store::migration)')
for name in ('store::migration', 'store::other'):
    print('SCOPED_START ' + name)
    print('SCOPED_END ' + name)
    print('SCOPED_FAILURE lib:rsid_store ' + name)
print('scoped-test: rsid-store executed 7 tests')
print('SCOPED_EXECUTED rsid-store 7')
print('SCOPED_PHASE listed')
print('SCOPED_PHASE test_failed')
sys.exit(1)
""")
            final = self.emitted_receipt(code)
        receipts = [json.loads(line.removeprefix("SCOPED_TEST_RECEIPT ")) for line in output.splitlines()
                    if line.startswith("SCOPED_TEST_RECEIPT ")]
        return code, output, receipts, final

    def test_only_baseline_reds_leave_a_final_package_verdict_split_by_origin(self):
        migration = {"package": "rsid-store", "kind": "lib", "target": "rsid_store", "test": "store::migration"}
        other = {**migration, "test": "store::other"}
        record = {"status": "recorded", "source": "snapshot", "records": [
            {"issue": 183, "class": "regression", "test_id": "store::migration"},
            {"issue": 184, "class": "regression", "test_id": "store::other"}]}
        code, output, receipts, final = self.run_two_failures([migration, other], record)
        self.assertEqual(code, 1, output)
        package = final["packages"][0]
        # A final status, the whole run's completed count and the failing ids split by origin.
        self.assertEqual((package["status"], package["phase"], package["completed"]),
                         ("failed", "test_failed", 7))
        self.assertEqual(package["failed_tests"], [migration, other])
        self.assertEqual((package["new_failures"], package["baseline_reds"]), ([], [migration, other]))
        # The package lists are the top-level lists, the ones AgentCheckLandingCandidate reads.
        self.assertEqual((package["new_failures"], package["baseline_reds"]),
                         (final["new_failures"], final["baseline_reds"]))
        self.assertEqual((final["ok"], final.get("ok_reason")), (True, "baseline_known_failure"), output)
        # No printed receipt after the package settled can read "running": a run cut off during the
        # base comparison leaves the last printed one (#199).
        settled = [r for r in receipts if "failed_tests" in (r["packages"] or [{}])[0]]
        self.assertTrue(settled, receipts)
        self.assertFalse(any(p["status"] == "running" for p in settled[-1]["packages"]))
        self.assertEqual(receipts[-1]["packages"][0]["baseline_reds"], [migration, other])

    def test_a_new_red_beside_a_baseline_red_is_split_per_package(self):
        migration = {"package": "rsid-store", "kind": "lib", "target": "rsid_store", "test": "store::migration"}
        other = {**migration, "test": "store::other"}
        code, output, _receipts, final = self.run_two_failures(
            [migration], {"status": "recorded", "source": "snapshot", "records": [
                {"issue": 183, "class": "regression", "test_id": "store::migration"}]})
        self.assertEqual(code, 1, output)
        package = final["packages"][0]
        self.assertEqual((package["new_failures"], package["baseline_reds"]), ([other], [migration]))
        self.assertEqual((final["new_failures"], final["baseline_reds"]), ([other], [migration]))
        self.assertFalse(final["ok"])

    def test_an_unavailable_base_comparison_leaves_package_new_failures_unknown(self):
        fn = RUNNER["split_package_failures"]
        failed = {"package": "p", "kind": "lib", "target": "t", "test": "a"}
        packages = [{"package": "p", "failed_tests": [failed]}]
        fn(packages, None, set(), False)
        self.assertEqual((packages[0]["new_failures"], packages[0]["baseline_reds"]), (None, []))

    def test_a_new_failure_is_never_ok_without_a_snapshot(self):
        failure = {"package": "rsid-store", "kind": "lib", "target": "rsid_store", "test": "store::migration"}
        code, output, receipt = self.run_with_baseline(
            {"status": "complete", "base": "origin/rolling", "baseline_reds": []},
            {"status": "unavailable", "source": "snapshot", "reason": "known-failure snapshot missing file"})
        self.assertEqual(code, 1, output)
        self.assertEqual(receipt["new_failures"], [failure])
        self.assertEqual((receipt["ok"], receipt.get("ok_reason")), (False, None))

    def test_baseline_only_ok_needs_a_clean_run_apart_from_baseline_reds(self):
        fn = RUNNER["baseline_only_ok_reason"]
        red = {"package": "rsid", "kind": "lib", "target": "rsid", "test": "a::b"}
        unavailable = {**red, "status": "unavailable", "reason": "known-failure snapshot missing file"}
        base = {"dry_run": False, "baseline_comparison": {"status": "complete"}, "failures": [red],
                "baseline_reds": [red], "new_failures": [], "baseline_failure_records": [unavailable],
                "packages": [{"package": "rsid", "status": "failed", "phase": "test_failed"},
                             {"package": "rsid-store", "status": "passed"}],
                "python": [{"status": "passed"}], "desktop": [], "warnings": {"status": "ok"}}
        self.assertEqual(fn(base), "baseline_confirmed_without_snapshot")
        for name, change in (
                ("comparison incomplete", {"baseline_comparison": {"status": "incomplete"}}),
                ("new failure", {"new_failures": [red]}),
                ("no comparison verdict", {"new_failures": None}),
                ("stale-base red only", {"baseline_reds": []}),
                ("timed out", {"packages": [{"package": "rsid", "status": "timed out"}]}),
                ("incomplete package", {"packages": [{"package": "rsid", "status": "failed",
                                                      "phase": "incomplete"}]}),
                ("build failed", {"packages": [{"package": "rsid", "status": "failed", "phase": "build_failed"}]}),
                ("unmatched filter", {"unmatched_filters": ["rsid=x"]}),
                ("warning gate", {"warnings": {"status": "new_warnings"}}),
                ("desktop lane", {"desktop": [{"status": "runner_unavailable"}]}),
                ("untracked red", {"baseline_failure_records": [{**red, "status": "unknown", "records": []}]}),
                ("unannotated red", {"baseline_failure_records": []}),
                ("dry run", {"dry_run": True}),
                ("no failures", {"failures": []})):
            with self.subTest(name=name):
                self.assertIsNone(fn({**base, **change}))

    def test_known_failure_record_lookup_preserves_unknown_and_unavailable(self):
        fn = RUNNER["query_failure_records"]
        record = {"issue": 397, "class": "env", "test_id": "store::migration"}
        for payload, expected in (
                ({"result": {"records": [record], "malformed_issues": []}},
                 {"status": "recorded", "records": [record]}),
                ({"result": {"records": [], "malformed_issues": []}},
                 {"status": "unknown", "records": []}),
                ({"result": {"records": [], "malformed_issues": [397]}},
                 {"status": "unavailable", "reason": "known-failure query found malformed Issues"}),
                ({"error": {"code": "unavailable"}},
                 {"status": "unavailable", "reason": "known-failure query did not return records"})):
            with self.subTest(payload=payload), patch.dict(os.environ, RSI_SESSION_TOKEN="fixture-token"), \
                    patch("subprocess.run", return_value=
                    subprocess.CompletedProcess([], 0, json.dumps(payload), "")) as run:
                self.assertEqual(fn(self.root, "store::migration", 2), expected)
                command = run.call_args.args[0]
                self.assertEqual(command[:3], ["rsi-rpc", "AgentQueryFailureSignatures", "--params"])
                self.assertEqual(json.loads(command[3]), {"test_id": "store::migration"})
                self.assertEqual(run.call_args.kwargs["timeout"], 2)
        for error in (FileNotFoundError(), subprocess.TimeoutExpired("rsi-rpc", 2)):
            with self.subTest(error=error), patch("subprocess.run", side_effect=error):
                self.assertEqual(fn(self.root, "store::migration", 2)["status"], "unavailable")

    def test_job_without_agent_authority_queries_the_shared_snapshot(self):
        fn = RUNNER["query_failure_records"]
        record = {"issue": 397, "test_id": "store::migration"}
        for output, stderr, code, status in (
                (json.dumps(record) + "\n", "", 0, "recorded"),
                ("", "", 0, "unknown"),
                ("", "warning: stale snapshot", 0, "unavailable"),
                ("", "cannot read snapshot", 2, "unavailable"),
                (json.dumps({**record, "test_id": "other"}), "", 0, "unavailable")):
            with self.subTest(output=output, stderr=stderr), \
                 patch.dict(os.environ, RSI_SESSION_TOKEN="", RSI_JOB_ID="", RSI_KNOWN_FAILURE_BIN="classifier"), \
                 patch("subprocess.run", return_value=
                       subprocess.CompletedProcess([], code, output, stderr)) as run:
                result = fn(self.root, "store::migration", 2)
                self.assertEqual((result["status"], result["source"]), (status, "snapshot"))
                self.assertEqual(run.call_args.args[0], ["classifier", "query", "store::migration"])
                if stderr:
                    self.assertEqual(result["reason"], stderr)

    def test_daemon_job_reads_project_snapshot_without_classifier_or_authority(self):
        fn = RUNNER["query_failure_records"]
        path = self.root / "target/.rsi-tmp/scoped-test/known-failures.v1.json"
        path.parent.mkdir(parents=True)
        record = {"schema_version": 1, "issue": 430, "test_id": "store::migration",
                  "class": "regression", "matcher": {"contains": ["boom"]}}
        export = {"schema_version": 1, "project_id": "00000000-0000-4000-8000-000000000430",
                  "snapshot": {"schema_version": 1, "exported_at": datetime.now(timezone.utc).isoformat(),
                               "records": [{"record": record, "issue_status": "InProgress"}]},
                  "malformed_issues": []}
        for name, records, errors, status, expected_records in (
                ("recorded", export["snapshot"]["records"], [], "recorded", [record]),
                ("different test", [{"record": {**record, "test_id": "another::test"},
                                     "issue_status": "Open"}], [], "unknown", []),
                ("closed owner", [{"record": record, "issue_status": "Closed"}], [], "unknown", []),
                ("empty export", [], [], "unknown", []),
                ("malformed source", [], ["issue #430: invalid JSON"], "unavailable", None),
                ("matching record despite other malformed source", export["snapshot"]["records"],
                 ["issue #431: invalid JSON"], "recorded", [record])):
            with self.subTest(name=name), \
                 patch.dict(os.environ, RSI_SESSION_TOKEN="", RSI_JOB_ID="job-fixture",
                            HOME=str(self.root / "different-home"), TMPDIR=str(self.root / "job-private-tmp"),
                            CARGO_TARGET_DIR="another-target"), \
                 patch("subprocess.run") as run:
                current = {**export, "snapshot": {**export["snapshot"], "records": records},
                           "malformed_issues": errors}
                path.write_text(json.dumps(current))
                result = fn(self.root, "store::migration", 2)
                self.assertEqual((result["status"], result["source"]), (status, "snapshot"))
                self.assertEqual(result["path"], str(path))
                if expected_records is not None:
                    self.assertEqual(result["records"], expected_records)
                    self.assertGreaterEqual(result["age_seconds"], 0)
                    if status == "unknown":
                        self.assertEqual(result["reason"], "snapshot loaded; no matching active record")
                else:
                    self.assertEqual(result["malformed_issues"], errors)
                run.assert_not_called()

    def test_daemon_snapshot_qa_issue_mentions_are_advisory_and_do_not_change_verdict(self):
        path = self.root / "target/.rsi-tmp/scoped-test/known-failures.v1.json"
        path.parent.mkdir(parents=True)
        export = {"schema_version": 1, "project_id": "00000000-0000-4000-8000-000000000430",
                  "snapshot": {"schema_version": 1, "exported_at": datetime.now(timezone.utc).isoformat(),
                               "records": []}, "malformed_issues": [],
                  "qa_advisories": {"store::migration": [430, 431], "store::migration_extra": [432]}}
        path.write_text(json.dumps(export))
        failure = {"package": "rsid-store", "kind": "lib", "target": "rsid_store",
                   "test": "store::migration"}
        with patch.dict(os.environ, RSI_SESSION_TOKEN="", RSI_JOB_ID="job-fixture"), \
             patch("subprocess.run") as run, contextlib.redirect_stdout(io.StringIO()) as out:
            annotation = RUNNER["annotate_baseline_reds"](self.root, [failure])
        expected = [{"issue": number, "message":
                     f"open qa Issue #{number} names this test (advisory, not a signature match)"}
                    for number in (430, 431)]
        record = annotation["baseline_failure_records"][0]
        self.assertEqual(record["status"], "unknown")
        self.assertEqual(record["records"], [])
        self.assertEqual(record["qa_issue_advisories"], expected)
        self.assertEqual(annotation["baseline_reds_without_record"], [failure])
        for advisory in expected:
            self.assertIn(advisory["message"], out.getvalue())
        receipt = {"baseline_comparison": {"status": "complete"}, "failures": [failure],
                   "baseline_reds": [failure], "new_failures": [], **annotation}
        self.assertIsNone(RUNNER["baseline_only_ok_reason"](receipt))
        run.assert_not_called()

    def test_daemon_snapshot_qa_advisory_is_exact_and_signature_records_win(self):
        path = self.root / "target/.rsi-tmp/scoped-test/known-failures.v1.json"
        path.parent.mkdir(parents=True)
        record = {"schema_version": 1, "issue": 433, "test_id": "store::migration",
                  "class": "regression", "matcher": {"contains": ["boom"]}}
        export = {"schema_version": 1, "project_id": "00000000-0000-4000-8000-000000000430",
                  "snapshot": {"schema_version": 1, "exported_at": datetime.now(timezone.utc).isoformat(),
                               "records": []}, "malformed_issues": []}
        fn = RUNNER["query_job_failure_snapshot"]
        for name, records, advisories, status, expected_issues in (
                ("prefix collision", [], {"store::migration_extra": [430]}, "unknown", []),
                ("signature wins", [{"record": record, "issue_status": "Open"}],
                 {"store::migration": [430]}, "recorded", [record]),
                ("malformed optional context", [], [430], "unknown", []),
                ("malformed optional Issue numbers", [], {"store::migration": [True]}, "unknown", []),
                ("old snapshot", [], None, "unknown", [])):
            with self.subTest(name=name):
                current = {**export, "snapshot": {**export["snapshot"], "records": records}}
                if advisories is not None:
                    current["qa_advisories"] = advisories
                path.write_text(json.dumps(current))
                result = fn(self.root, "store::migration")
                self.assertEqual((result["status"], result["records"]), (status, expected_issues))
                self.assertEqual(result.get("qa_issue_advisories", []), [])

    def test_daemon_snapshot_load_failures_report_concrete_causes_and_stale_age(self):
        fn = RUNNER["query_failure_records"]
        path = self.root / "target/.rsi-tmp/scoped-test/known-failures.v1.json"
        path.parent.mkdir(parents=True)
        with patch.dict(os.environ, RSI_SESSION_TOKEN="", RSI_JOB_ID="job-fixture"):
            missing = fn(self.root, "store::migration", 2)
            self.assertEqual(missing["status"], "unavailable")
            self.assertIn(f"missing file: {path}", missing["reason"])
            path.mkdir()
            unreadable = fn(self.root, "store::migration", 2)
            self.assertEqual(unreadable["status"], "unavailable")
            self.assertIn("read error:", unreadable["reason"])
            path.rmdir()
            export = {"schema_version": 1, "project_id": "00000000-0000-4000-8000-000000000430",
                      "snapshot": {"schema_version": 1, "exported_at": datetime.now(timezone.utc).isoformat(),
                                   "records": []}, "malformed_issues": []}
            for content, reason in (
                    ("not JSON", "parse error:"),
                    (json.dumps({**export, "schema_version": 2}), "unsupported export schema_version"),
                    (json.dumps({**export, "snapshot": {**export["snapshot"], "schema_version": 2}}),
                     "unsupported snapshot schema_version"),
                    (json.dumps({**export, "snapshot": {**export["snapshot"], "exported_at": "invalid"}}),
                     "exported_at must be RFC3339"),
                    (json.dumps({**export, "snapshot": {**export["snapshot"], "records": [{}]}}),
                     "parse error:"),
                    (json.dumps({**export, "snapshot": {**export["snapshot"], "records": {}}}),
                     "records and malformed_issues must be lists")):
                with self.subTest(reason=reason):
                    path.write_text(content)
                    result = fn(self.root, "store::migration", 2)
                    self.assertEqual(result["status"], "unavailable")
                    self.assertIn(reason, result["reason"])
            old = datetime.now(timezone.utc) - timedelta(hours=25, seconds=13)
            export["snapshot"]["exported_at"] = old.isoformat()
            path.write_text(json.dumps(export))
            stale = fn(self.root, "store::migration", 2)
            self.assertEqual(stale["status"], "unavailable")
            self.assertAlmostEqual(stale["age_seconds"], 90013, delta=2)
            self.assertIn(f"age={stale['age_seconds']:.3f}s", stale["reason"])
            self.assertIn("limit=86400s", stale["reason"])

    def test_baseline_record_annotation_deduplicates_queries_and_bounds_lookup_time(self):
        first = {"package": "rsid", "kind": "lib", "target": "rsid", "test": "one"}
        second = {**first, "target": "other"}
        third = {**first, "test": "two"}
        fn = RUNNER["annotate_baseline_reds"]
        clock = ControlledClock()

        def query(_repo, name, timeout):
            self.assertEqual((name, timeout), ("one", 5))
            clock.advance(10)
            return {"status": "recorded", "records": [{"issue": 397}]}

        with patch.dict(fn.__globals__, query_failure_records=query, time=clock), \
             contextlib.redirect_stdout(io.StringIO()):
            receipt = fn(self.root, [first, second, third])
        self.assertEqual(receipt["baseline_reds_without_record"], [])
        self.assertEqual([item["status"] for item in receipt["baseline_failure_records"]],
                         ["recorded", "recorded", "unavailable"])

    def _forked_repo(self):
        repo = self.scratch_repo()
        git = lambda *args: scratch_git(repo, *args).stdout.strip()
        git("init", "--quiet", "-b", "main")
        git("config", "user.name", "Test")
        git("config", "user.email", "test@example.invalid")
        (repo / "f").write_text("0\n")
        git("add", "f")
        git("commit", "--quiet", "-m", "fork point")
        fork = git("rev-parse", "HEAD")
        git("branch", "tip")
        (repo / "g").write_text("c\n")
        git("add", "g")
        git("commit", "--quiet", "-m", "candidate")
        git("checkout", "--quiet", "tip")
        (repo / "f").write_text("1\n")
        git("add", "f")
        git("commit", "--quiet", "-m", "fix on rolling")
        git("checkout", "--quiet", "main")
        return repo, fork

    def test_failure_red_at_fork_point_and_fixed_on_the_tip_is_stale_base_not_new(self):
        repo, fork = self._forked_repo()
        failures = {"rsid-store": [
            {"kind": "lib", "target": "t", "test": "inherited"},
            {"kind": "lib", "target": "t", "test": "mine"},
            {"kind": "lib", "target": "t", "test": "on_tip"}]}
        comparison = {"status": "complete", "base": "tip", "checked": [],
                      "baseline_reds": [{"package": "rsid-store", "kind": "lib", "target": "t",
                                         "test": "on_tip"}]}
        seen = []
        def compare(_repo, rev, packages, log_dir, *_rest):
            seen.append((rev, sorted(n["test"] for n in packages["rsid-store"]), Path(log_dir).name))
            reds = [{"package": "rsid-store", "kind": "lib", "target": "t", "test": "inherited"}]
            return {"status": "complete", "base": rev, "checked": [], "baseline_reds": reds}
        log_dir = self.root / "fork-logs"
        log_dir.mkdir()
        fn = RUNNER["split_stale_base_failures"]
        with patch.dict(fn.__globals__, compare_failures_with_base=compare):
            stale, detail = fn(repo, "main", "tip", failures, comparison, log_dir, 30, 200, False, ["cargo"])
        # Only failures the tip does not reproduce are re-run, at the merge-base.
        self.assertEqual(seen, [(fork, ["inherited", "mine"], "fork-point")])
        self.assertEqual([r["test"] for r in stale], ["inherited"])
        self.assertEqual(detail["merge_base"], fork)

    def test_fork_point_check_is_skipped_when_the_candidate_is_on_the_tip(self):
        repo, _fork = self._forked_repo()
        self.assertIsNone(RUNNER["fork_point"](repo, "tip", "tip"))
        self.assertIsNone(RUNNER["fork_point"](repo, "main", "no-such-ref"))
        comparison = {"status": "complete", "baseline_reds": []}
        fn = RUNNER["split_stale_base_failures"]
        def boom(*_a, **_k):
            raise AssertionError("no second comparison expected")
        failures = {"rsid-store": [{"kind": "lib", "target": "t", "test": "x"}]}
        with patch.dict(fn.__globals__, compare_failures_with_base=boom):
            self.assertEqual(fn(repo, "tip", "tip", failures, comparison, self.root, 30, 200, False, []),
                             ([], {}))
            self.assertEqual(fn(repo, "main", "tip", failures, {"status": "incomplete"}, self.root,
                                30, 200, False, []), ([], {}))

    def test_candidate_baseline_candidate_retry_keeps_candidate_dependencies(self):
        """An unchanged retry must compile after the baseline builds (#1963)."""
        repo = self.scratch_repo()
        git = lambda *args: scratch_git(repo, *args).stdout.strip()
        git("init", "--quiet")
        git("config", "user.name", "Test")
        git("config", "user.email", "test@example.invalid")
        (repo / "layout").write_text("base\n")
        git("add", "layout")
        git("commit", "--quiet", "-m", "base")
        base = git("rev-parse", "HEAD")
        (repo / "layout").write_text("candidate\n")
        git("add", "layout")
        git("commit", "--quiet", "-m", "candidate")
        candidate_target = self.root / "target"
        self.binary = self.executable("layout-binary", """
if '--list' in sys.argv:
    print('store::migration: test')
else:
    print('test store::migration ... ok')
""")
        artifact = {"reason": "compiler-artifact", "profile": {"test": True},
                    "manifest_path": str(self.root / "Cargo.toml"),
                    "executable": str(self.binary), "target": {"kind": ["lib"], "name": "rsid_store"}}
        # Cargo stub: the baseline can overwrite cached dependency artifacts,
        # but an unchanged candidate retry reuses them instead of rebuilding.
        self.cargo = self.executable("layout-cargo", f"""
layout = Path('layout').read_text()
marker = Path(os.environ['CARGO_TARGET_DIR']) / 'dep-layout'
if layout.strip() == 'candidate' and marker.exists() and marker.read_text() != layout:
    print('error[E0433]: candidate symbol missing from stale baseline dependency')
    sys.exit(1)
marker.parent.mkdir(parents=True, exist_ok=True)
marker.write_text(layout)
print({json.dumps(json.dumps(artifact))})
""")
        log_dir = self.root / "layout-logs"
        log_dir.mkdir()
        globals_ = RUNNER["compare_failures_with_base"].__globals__
        with patch.dict(os.environ, CARGO_TARGET_DIR=str(candidate_target)), \
             patch.dict(globals_, seed_baseline_target=lambda *_args: None):
            build = [str(self.cargo), "test", "--no-run"]
            first = subprocess.run(build, cwd=repo, capture_output=True, text=True)
            self.assertEqual(first.returncode, 0, first.stdout + first.stderr)
            result = RUNNER["compare_failures_with_base"](
                repo, base,
                {"rsid-store": [{"kind": "lib", "target": "rsid_store", "test": "store::migration"}]},
                log_dir, 60, 200, False, [str(self.cargo)])
            retry = subprocess.run(build, cwd=repo, capture_output=True, text=True)
            self.assertEqual(retry.returncode, 0, retry.stdout + retry.stderr)
        self.assertEqual(result["status"], "complete", (log_dir / "baseline-rsid-store.log").read_text())
        self.assertEqual((candidate_target / "dep-layout").read_text(), "candidate\n")
        self.assertEqual(list((candidate_target / "scoped-baselines").iterdir()), [])

    def test_baseline_target_stays_inside_the_assigned_candidate_target(self):
        for setting in (None, "relative-target", str(self.root / "absolute-target")):
            with self.subTest(setting=setting), patch.dict(os.environ):
                if setting is None:
                    os.environ.pop("CARGO_TARGET_DIR", None)
                else:
                    os.environ["CARGO_TARGET_DIR"] = setting
                candidate = self.root / (setting or "target")
                baseline = RUNNER["baseline_target_dir"](self.root, self.root / "logs")
                self.assertEqual(baseline.parent, candidate / "scoped-baselines")
                self.assertTrue(baseline.is_dir())
                self.assertTrue(baseline.name.startswith("run-"))

    def test_stale_baseline_targets_are_swept_but_fresh_and_candidate_stay(self):
        candidate = self.root / "target"
        stale = candidate / "scoped-baselines/run-old"
        fresh = candidate / "scoped-baselines/run-new"
        legacy = self.root / "target-baseline-old"
        for path in (stale, fresh, legacy):
            path.mkdir(parents=True)
        old = time.time() - 7 * 3600
        os.utime(stale, (old, old))
        os.utime(legacy, (old, old))
        with patch.dict(os.environ, CARGO_TARGET_DIR=str(candidate)):
            RUNNER["baseline_target_dir"](self.root, self.root / "logs")
        self.assertFalse(stale.exists())
        self.assertFalse(legacy.exists())
        self.assertTrue(fresh.is_dir())
        self.assertTrue(candidate.is_dir())

    def test_baseline_unit_is_told_its_own_target_not_the_candidates(self):
        plan = {"repo": str(self.root), "package": "rsid-store"}
        with patch.dict(os.environ, CARGO_TARGET_DIR=str(self.root / "target")):
            command, _unit = RUNNER["bounded_command"](
                plan, self.root / "plan.json", 30, 200, True, env={"CARGO_TARGET_DIR": "/baseline/target"})
        self.assertIn("--setenv=CARGO_TARGET_DIR=/baseline/target", command)
        self.assertNotIn("--setenv=CARGO_TARGET_DIR", command)

    def test_failures_survive_a_later_batch_that_times_out(self):
        """Printed as each batch settles, not at the end of the package (#1895)."""
        results = iter([(1, 1, ["a::one"])])

        def run_batch(*_args):
            try:
                return next(results)
            except StopIteration:
                raise subprocess.TimeoutExpired("batch", 1)

        binary = self.executable("two-batches", "print('a::one: test\\na::two: test' if '--list' in sys.argv else '')\n")
        selections = RUNNER["selection_plan"](["rsid=a::"], {})["rsid"]
        artifacts = {str(binary): ({"kind": ["lib"], "name": "rsid"}, str(self.root))}
        out = io.StringIO()
        with patch.dict(RUNNER["run_selected"].__globals__, run_batch=run_batch,
                        test_batches=lambda names: [[name] for name in names]), \
             contextlib.redirect_stdout(out), self.assertRaises(subprocess.TimeoutExpired):
            RUNNER["run_selected"]({"package": "rsid", "selections": selections}, artifacts, None)
        self.assertIn("SCOPED_FAILURE lib:rsid a::one", out.getvalue())
        self.assertEqual(RUNNER["failure_records"]({"rsid": [{"kind": "lib", "target": "rsid", "test": "a::one"}]}),
                         [{"package": "rsid", "kind": "lib", "target": "rsid", "test": "a::one"}])

    def test_tests_of_a_narrow_filter_run_before_those_only_a_broad_filter_selects(self):
        """A budget that stops the run must leave the targeted evidence behind (#1732)."""
        broad = [f"a::t{n:02d}" for n in range(70)]
        listing = "\n".join(f"{name}: test" for name in [*broad, "z::targeted"])
        binary = self.executable("two-filters", f"print({listing!r} if '--list' in sys.argv else '')\n")
        selections = RUNNER["selection_plan"](["rsid=a::", "rsid=z::targeted"], {})["rsid"]
        artifacts = {str(binary): ({"kind": ["lib"], "name": "rsid"}, str(self.root))}
        batches = []

        def run_batch(_binary, names, *_args):
            batches.append(list(names))
            return len(names), 0, []

        with patch.dict(RUNNER["run_selected"].__globals__, run_batch=run_batch), \
             contextlib.redirect_stdout(io.StringIO()):
            result = RUNNER["run_selected"]({"package": "rsid", "selections": selections}, artifacts, None)
        self.assertEqual(result, 0)
        self.assertEqual(batches, [["z::targeted"], broad])

    def slow_tier_run(self, plan_extra, filters=("rsid-store=store::tests",)):
        """Run `run_selected` over a store listing with one fast and one slow test."""
        listing = "\n".join(f"{name}: test" for name in
                            ["store::tests::fast_one", "store::tests::slow_one", "store::tests::fast_two"])
        binary = self.executable("slow-tier", f"print({listing!r} if '--list' in sys.argv else '')\n")
        selections = RUNNER["selection_plan"](list(filters), {})["rsid-store"]
        artifacts = {str(binary): ({"kind": ["lib"], "name": "rsid_store"}, str(self.root))}
        batches = []

        def run_batch(_binary, names, *_args):
            batches.append(sorted(names))
            return len(names), 0, []

        out = io.StringIO()
        with patch.dict(RUNNER["run_selected"].__globals__, run_batch=run_batch), contextlib.redirect_stdout(out):
            result = RUNNER["run_selected"](
                {"package": "rsid-store", "selections": selections, **plan_extra}, artifacts, None)
        return result, batches, out.getvalue()

    def test_an_excluded_slow_tier_is_left_out_of_a_module_selection_and_counted(self):
        """#2270: a derived plan whose diff needs no migration history skips the slow tier."""
        result, batches, out = self.slow_tier_run({"slow_tier_excluded": ["store::tests::slow_one"]})
        self.assertEqual(result, 0)
        self.assertEqual(batches, [["store::tests::fast_one", "store::tests::fast_two"]])
        self.assertIn("SCOPED_SLOW_TIER_SKIPPED 1", out)
        self.assertIn("SCOPED_MATCH rsid-store=store::tests", out)

    def test_a_filter_that_selects_only_slow_tier_tests_still_counts_as_matching(self):
        result, batches, out = self.slow_tier_run(
            {"slow_tier_excluded": ["store::tests::slow_one"]}, filters=("rsid-store=store::tests::slow_one",))
        self.assertEqual((result, batches), (0, []))
        self.assertIn("SCOPED_MATCH rsid-store=store::tests::slow_one", out)
        self.assertIn("SCOPED_SLOW_TIER_SKIPPED 1", out)

    def ignored_run(self, filters, exact_tests=None):
        """Run `run_selected` over a listing where `t::ignored_one` is #[ignore]d (#2273)."""
        listing = "\n".join(f"{name}: test" for name in ["t::live", "t::ignored_one"])
        ignored = "t::ignored_one: test"
        binary = self.root / "ignoring"
        binary.write_text(f"#!{sys.executable}\nimport sys\n"
                          f"print({ignored!r} if '--ignored' in sys.argv else {listing!r})\n")
        binary.chmod(0o755)
        selections = RUNNER["selection_plan"](list(filters), {})["rsid-store"]
        artifacts = {str(binary): ({"kind": ["lib"], "name": "rsid_store"}, str(self.root))}
        batches = []

        def run_batch(_binary, names, *_args):
            batches.append(sorted(names))
            return len(names), 0, [], set(names)

        out = io.StringIO()
        with patch.dict(RUNNER["run_selected"].__globals__, run_batch=run_batch), contextlib.redirect_stdout(out):
            result = RUNNER["run_selected"](
                {"package": "rsid-store", "selections": selections}, artifacts, None, exact_tests)
        return result, batches, out.getvalue()

    def test_an_ignored_test_is_left_out_of_a_selection_that_also_matches_a_live_one(self):
        result, batches, out = self.ignored_run(("rsid-store=t::",))
        self.assertEqual((result, batches), (0, [["t::live"]]))
        self.assertNotIn("SCOPED_IGNORED_ONLY", out)

    def test_a_filter_that_selects_only_ignored_tests_matches_runs_nothing_and_is_reported(self):
        result, batches, out = self.ignored_run(("rsid-store=t::ignored_one",))
        self.assertEqual((result, batches), (0, []))
        self.assertIn("SCOPED_MATCH rsid-store=t::ignored_one", out)
        self.assertIn("SCOPED_IGNORED_ONLY rsid-store=t::ignored_one", out)
        self.assertIn("SCOPED_COMPLETE rsid-store 0", out)

    def test_a_full_slow_tier_runs_every_selected_test(self):
        result, batches, out = self.slow_tier_run({})
        self.assertEqual(result, 0)
        self.assertEqual(batches, [["store::tests::fast_one", "store::tests::fast_two", "store::tests::slow_one"]])
        self.assertNotIn("SCOPED_SLOW_TIER_SKIPPED", out)

    def test_the_slow_tier_plan_follows_the_checker_decision_and_explicit_filters_run_as_named(self):
        config = {"package": "rsid-store", "tests": ["store::tests::slow_two", "store::tests::slow_one"]}
        checker = {"load_slow_tier": lambda repo, head: config}
        plan = RUNNER["slow_tier_plan"]
        excluded = {"slow_tier": {"mode": "excluded", "reason": "no_trigger"}}
        entry, names = plan(checker, self.root, "HEAD", excluded, explicit=False)
        self.assertEqual((entry["mode"], entry["package"], entry["skipped_tests"]), ("excluded", "rsid-store", 0))
        self.assertEqual(names, ["store::tests::slow_one", "store::tests::slow_two"])
        triggered = {"slow_tier": {"mode": "full", "reason": "trigger_path", "path": "tools/released-migrations.json"}}
        entry, names = plan(checker, self.root, "HEAD", triggered, explicit=False)
        self.assertEqual((entry["mode"], entry["path"], names), ("full", "tools/released-migrations.json", []))
        entry, names = plan(checker, self.root, "HEAD", excluded, explicit=True)
        self.assertEqual((entry["mode"], entry["reason"], names), ("full", "explicit_filters", []))
        entry, names = plan(checker, self.root, "HEAD", {}, explicit=False)
        self.assertEqual((entry["mode"], entry["reason"], names), ("full", "no_decision", []))
        entry, names = plan({"load_slow_tier": lambda repo, head: None}, self.root, "HEAD", excluded, explicit=False)
        self.assertEqual((entry["mode"], entry["reason"], names), ("full", "no_slow_tier_config", []))
        self.assertIn("left to the merge-queue full QA", RUNNER["slow_tier_summary"](
            {"mode": "excluded", "reason": "no_trigger", "tests": 2}))
        self.assertIn("runs in full (trigger_path at x)", RUNNER["slow_tier_summary"](
            {"mode": "full", "reason": "trigger_path", "path": "x"}))

    def test_specificity_tiers_split_at_the_narrow_and_module_sizes(self):
        names = {"b": 1, "a": 24, "d": 25, "c": 60, "e": 61}
        self.assertEqual(RUNNER["specificity_tiers"](list(names), names), [["a", "b"], ["c", "d"], ["e"]])

    def test_changed_tests_run_before_replay_pins_even_under_a_namespace_filter(self):
        filters = ["rsid=shard:store-01:test(a_replay)",
                   "rsid=shard:store-01:test(z_journal)"]
        priority = ["rsid=shard:store-01:test(/^z_journal::(?:changed|new)$/)"]
        selections = RUNNER["selection_plan"](filters, {"store-01": ["rsid"]}, priority)["rsid"]
        names = ["a_replay::pin", "z_journal::changed", "z_journal::new", "z_journal::old"]
        listing = "\n".join(f"{name}: test" for name in names)
        binary = self.executable("changed-tests", f"print({listing!r} if '--list' in sys.argv else '')\n")
        artifacts = {str(binary): ({"kind": ["lib"], "name": "rsid"}, str(self.root))}
        batches = []

        def run_batch(_binary, tests, *_args):
            batches.append(list(tests))
            return len(tests), 0, []

        with patch.dict(RUNNER["run_selected"].__globals__, run_batch=run_batch), \
             contextlib.redirect_stdout(io.StringIO()):
            result = RUNNER["run_selected"]({"package": "rsid", "selections": selections}, artifacts, None)
        self.assertEqual(result, 0)
        self.assertEqual(batches, [["z_journal::changed", "z_journal::new"],
                                   ["a_replay::pin", "z_journal::old"]])

    slice_quota_text = "infinity\n"
    selector_present = True

    def invoke(self, linux=True, dry_run=False, systemd_body=None, filters=None, seconds="5",
               shard_packages=None, explicit=None, head="candidate", worktree_head="candidate",
               reclaim_incremental=False, derive_code=0, audits=None, guard_failure=None,
               broad=False, total=None, env_filters=None, python_paths=(), slot=None, extra_args=(),
               entrypoint=False, job_started_at=None, broad_filters=None, derived_extra=None, slot_held=False,
               missing_guards=(), downstream=None):
        selected = filters if filters is not None else ["rsid=shard:store-01:test(store::migration)"]
        receipt = subprocess.CompletedProcess([], derive_code, json.dumps({
            "suggested_filters": selected, "head": "candidate", "broad": broad,
            "broad_filters": broad_filters or [], **(derived_extra or {}),
            "audits": audits if audits is not None else {
                "shard_gates": {"verdict": "ok"}, "released_migrations": {"verdict": "ok"}},
        }), "BROAD: split the run\n" if broad else "")
        self.guard_commands = []
        # The guard scripts and the Rust guard modules (#360) live in the candidate tree; a test
        # names the ones its candidate predates (#3126).
        for rel in ("scripts/check-rsid-test-shards.py", "tools/check-released-migrations.py",
                    "crates/rsi-repo-guards/src/starter_pins.rs", "crates/rsi-repo-guards/src/settings_preflight.rs", "crates/rsi-repo-guards/src/identity_assertions.rs", "scripts/check-target-reclaim.py", "scripts/check-guard-regressions.py", "scripts/check-store-boundary.py", "crates/rsi-repo-guards/src/test_env_writes.rs",
                    "crates/rsi-repo-guards/src/personal_paths.rs", "crates/rsi-repo-guards/src/agents_md_size.rs", "crates/rsi-repo-guards/src/portable_shell.rs"):
            if rel not in missing_guards:
                (self.root / rel).parent.mkdir(parents=True, exist_ok=True)
                (self.root / rel).write_text("")

        def run(command, **kwargs):
            if command[:2] == ["git", "-C"]:
                return self.baseline_git(command[3:])
            if command[:2] == ["git", "show"]:
                # The desktop lane reads its manifest at the candidate; the fixture has none. The
                # selector script is there unless a test removes it (#355).
                has_selector = command[2].endswith(":scripts/desktop-test-gate.py") and self.selector_present
                return subprocess.CompletedProcess(command, 0 if has_selector else 128, b"", b"")
            if command[:3] == ["git", "diff", "--check"]:
                self.guard_commands.append(command)
                failed = guard_failure == "diff"
                return subprocess.CompletedProcess(command, int(failed), "guard finding" if failed else "", "")
            if command[:2] == ["git", "diff"]:
                paths = b"".join(os.fsencode(path) + b"\0" for path in python_paths)
                return subprocess.CompletedProcess(command, 0, paths, b"")
            if command[:3] == ["systemctl", "--user", "stop"]:
                return subprocess.CompletedProcess(command, 0, "", "")
            if command[:3] == ["systemctl", "--user", "show"]:
                return subprocess.CompletedProcess(command, 0, self.slice_quota_text, "")
            if command[:3] == ["git", "worktree", "add"]:
                return subprocess.CompletedProcess(command, 1, "", "not a worktree fixture")
            if str(command[1]).endswith("check-touched-shards"):
                return receipt
            self.guard_commands.append(command)
            self.assertEqual(kwargs["cwd"], self.root)
            failed = command[1] == guard_failure
            return subprocess.CompletedProcess(command, int(failed), "guard finding" if failed else "", "")
        command = self.executable("systemd-run", systemd_body or '''
index = sys.argv.index('--execute-plan')
os.execv(sys.executable, [sys.executable, '-u', sys.argv[index - 1], *sys.argv[index:]])
''')
        log_dir = self.root / "output"
        log_dir.mkdir()
        checker = {"git_out": lambda *args: worktree_head if args[-1] == "HEAD" else head,
                   "feature_test_inventory": lambda *args: [],
                   "downstream_test_plan": lambda *args: (downstream if downstream is not None else
                                                           {"changed_libraries": [], "packages": []}),
                   "cargo_prefix": lambda: [*([str(slot)] if slot and not os.environ.get("RSI_CARGO_SLOT_HELD") else []),
                                            str(self.cargo)],
                   "shard_packages": lambda repo: (shard_packages if shard_packages is not None else
                                                    {"store-01": ["rsid-store"]})}
        argv = [str(SCRIPT), "--repo", str(self.root), "--head", head,
                "--runtime-max-sec", seconds, "--cpu-quota", "75"]
        if reclaim_incremental:
            argv.append("--reclaim-incremental")
        if total:
            argv.extend(["--total-max-sec", total])
        argv.extend(extra_args)
        for index, value in enumerate(explicit or []):
            argv.extend(["--filter" if index == 0 else "--test-filter", value])
        if dry_run:
            argv.append("--dry-run")
        output = io.StringIO()
        run_path = runpy.run_path
        real_mkdtemp = tempfile.mkdtemp
        # Build the environment patch: clear RSI_SCOPED_TEST_* variables unless explicitly set
        env_patch = {
            "PATH": str(self.root) + os.pathsep + os.environ.get("PATH", ""),
            "CARGO_TARGET_DIR": str(self.root / "target"),
        }
        if slot_held:
            env_patch["RSI_CARGO_SLOT_HELD"] = "1"
        if job_started_at is not None:
            env_patch["RSI_JOB_STARTED_AT"] = job_started_at
        # Only set RSI_SCOPED_TEST_FILTERS if env_filters is explicitly provided
        if env_filters:
            env_patch["RSI_SCOPED_TEST_FILTERS"] = env_filters

        # Context manager to clear RSI_SCOPED_TEST_* variables that a recipe job might have set
        @contextlib.contextmanager
        def isolated_env():
            saved = {}
            for name in ("RSI_SCOPED_TEST_FILTERS", "RSI_CARGO_SLOT_HELD", "RSI_JOB_STARTED_AT"):
                if name in os.environ:
                    saved[name] = os.environ.pop(name)
            try:
                yield
            finally:
                for name, value in saved.items():
                    os.environ[name] = value

        with patch.object(sys, "argv", argv), patch.object(sys, "platform", "linux" if linux else "darwin"), \
             patch.dict(RUNNER["run"].__globals__, **({"downstream_test_plan": lambda *_args: downstream}
                                                    if downstream is not None else {})), \
             patch.dict(RUNNER["run"].__globals__, installed_rust_targets=lambda *_args: set()), \
             isolated_env(), \
             patch.dict(os.environ, env_patch), \
             patch("runpy.run_path", return_value=checker), patch("subprocess.run", side_effect=run) as derive, \
             patch.object(RUNNER["PYTHON_GATE"], "changed_paths", return_value=python_paths), \
             patch("tempfile.mkdtemp", side_effect=lambda prefix="", dir=None, **kw: (
                 real_mkdtemp(prefix=prefix, dir=dir) if prefix.startswith("rsi-scoped-test-tmpfs-")
                 else str(log_dir))), \
             contextlib.redirect_stdout(output), contextlib.redirect_stderr(output):
            if entrypoint:
                # Exercise the real CLI shutdown, not just main(): it owns the
                # last-line receipt consumed by durable jobs (#1821).
                with self.assertRaises(SystemExit) as exited:
                    run_path(str(SCRIPT), run_name="__main__")
                code = exited.exception.code
            else:
                code = RUNNER["main"]()
        if explicit or env_filters:
            self.assertTrue(all(not str(call.args[0][1]).endswith("check-touched-shards")
                                for call in derive.call_args_list))
        else:
            self.assertIn("--filters-only", derive.call_args_list[0].args[0])
        return code, output.getvalue()

    def test_compile_only_lane_records_success_without_runtime_test_evidence(self):
        downstream = {"changed_libraries": [{"package": "rsi-common", "paths": ["src/lib.rs"]}],
                      "packages": [{"package": "rsid-store", "status": "pending"}]}
        code, output = self.invoke(linux=False, filters=[], downstream=downstream)
        self.assertEqual(code, 0, output)
        receipt = RUNNER["RECEIPT"]
        entry = receipt["downstream_compile"]["packages"][0]
        self.assertEqual((entry["status"], entry["execution"]), ("passed", "compile_only"))
        self.assertIn("--tests", entry["command"])
        self.assertEqual(self.core_packages(receipt), [{"package": "rsid-store", "status": "passed", "completed": 0}])
        self.assertEqual(receipt["compile"]["packages"], {"rsid-store": "executed"})
        self.assertEqual(len(self.log.read_text().splitlines()), 1)

    def test_compile_only_lane_keeps_a_downstream_type_error_red_in_the_receipt(self):
        self.cargo = self.executable("type-error-cargo", "print('error[E0308]: mismatched types')\nsys.exit(101)\n")
        downstream = {"changed_libraries": [{"package": "rsi-common", "paths": ["src/lib.rs"]}],
                      "packages": [{"package": "rsid-store", "status": "pending"}]}
        code, output = self.invoke(linux=False, filters=[], downstream=downstream)
        self.assertEqual(code, 1, output)
        receipt = RUNNER["RECEIPT"]
        self.assertEqual(receipt["downstream_compile"]["packages"][0]["status"], "failed")
        self.assertEqual(receipt["packages"][0]["phase"], "build_failed")
        self.assertEqual(receipt["compile"]["packages"], {"rsid-store": "failed"})

    def test_successful_package_run_ends_with_a_green_durable_receipt(self):
        code, output = self.invoke(linux=False, seconds="5", entrypoint=True)
        self.assertEqual(code, 0, output)
        lines = output.splitlines()
        self.assertIn("scoped-test: rsid-store: passed (1 completed tests)", lines)
        self.assertTrue(lines[-1].startswith("SCOPED_TEST_RECEIPT "), output)
        receipt = json.loads(lines[-1].split(" ", 1)[1])
        self.assertEqual((receipt["ok"], receipt["exit_code"]), (True, code))
        self.assertEqual(self.core_packages(receipt), [
            {"package": "rsid-store", "status": "passed", "completed": 1}])
        self.assertEqual(receipt["filters"], ["rsid=shard:store-01:test(store::migration)"])
        self.assertEqual(receipt["log_dir"], str(self.root / "output"))

    def test_planning_starts_no_cargo_and_the_admitted_run_schedules_the_required_build(self):
        # #2564: derivation only ran --filters-only (asserted by invoke); the build and the
        # tests run later in this job and the receipt labels what truly ran.
        code, output = self.invoke(linux=False, seconds="5", entrypoint=True)
        self.assertEqual(code, 0, output)
        receipt = self.receipt_of(output)
        self.assertEqual(receipt["compile"], {
            "planning": {"source": "filters_only", "cargo_invoked": False},
            "execution": "executed", "packages": {"rsid-store": "executed"}})
        commands = self.log.read_text().splitlines()
        self.assertEqual(len([c for c in commands if c.startswith("cargo-stub ")]), 1, commands)
        self.assertRegex(commands[0], r"^cargo-stub test -p rsid-store .*--no-run")
        self.assertFalse([c for c in self.guard_commands if "cargo" in Path(c[0]).name], self.guard_commands)

    def test_a_dry_run_plans_the_build_without_running_it_and_labels_it_skipped(self):
        code, output = self.invoke(dry_run=True, entrypoint=True)
        self.assertEqual(code, 0, output)
        self.assertIn("build once: " + str(self.cargo), output)
        self.assertFalse(self.log.exists(), "a dry run must not start the build")
        self.assertEqual(self.receipt_of(output)["compile"], {
            "planning": {"source": "filters_only", "cargo_invoked": False},
            "execution": "skipped_dry_run", "packages": {}})

    def test_explicit_filters_label_their_plan_and_still_build_inside_the_run(self):
        code, output = self.invoke(linux=False, seconds="5", entrypoint=True,
                                   explicit=["rsid=shard:store-01:test(store::migration)"])
        self.assertEqual(code, 0, output)
        compile_label = self.receipt_of(output)["compile"]
        self.assertEqual(compile_label["planning"], {"source": "explicit_filters", "cargo_invoked": False})
        self.assertEqual(compile_label["execution"], "executed")

    def test_a_failed_supervised_build_is_labelled_failed_and_keeps_the_gate_red(self):
        self.cargo = self.executable("cargo-stub", "print('error: could not compile `rsid-store`')\nsys.exit(101)\n")
        code, output = self.invoke(linux=False, seconds="5", entrypoint=True)
        self.assertEqual(code, 1, output)
        compile_label = self.receipt_of(output)["compile"]
        self.assertEqual((compile_label["execution"], compile_label["packages"]),
                         ("executed", {"rsid-store": "failed"}))

    def test_failed_package_run_ends_with_a_red_durable_receipt(self):
        self.binary = self.executable("test-binary", """
if '--list' in sys.argv:
    print('store::migration: test')
else:
    print('test store::migration ... FAILED')
    sys.exit(1)
""")
        code, output = self.invoke(linux=False, seconds="5", entrypoint=True)
        self.assertEqual(code, 1, output)
        lines = output.splitlines()
        self.assertIn("scoped-test: rsid-store: failed (1 completed tests)", lines)
        self.assertTrue(lines[-1].startswith("SCOPED_TEST_RECEIPT "), output)
        receipt = json.loads(lines[-1].split(" ", 1)[1])
        self.assertEqual((receipt["ok"], receipt["exit_code"]), (False, code))
        self.assertEqual(self.core_packages(receipt), [
            {"package": "rsid-store", "status": "failed", "completed": 1, "phase": "test_failed",
             "log": str(self.root / "output/rsid-store.log")}])
        self.assertEqual(receipt["failures"], [
            {"package": "rsid-store", "kind": "lib", "target": "rsid_store",
             "test": "store::migration"}])

    def receipt_of(self, output):
        return json.loads(output.splitlines()[-1].split(" ", 1)[1])

    def test_named_assertion_failure_survives_abort_before_summary_in_receipt(self):
        self.binary = self.executable("test-binary", """
if '--list' in sys.argv:
    for name in ('store::assertion', 'store::ok', 'store::stop'):
        print(name + ': test')
else:
    print('test store::assertion ... panic output')
    print("thread 'store::assertion' panicked at a failed assertion")
    print('FAILED')
    print('test store::ok ... ok')
    print('test store::stop ... process aborting', flush=True)
    os._exit(134)
""")
        code, output = self.invoke(linux=False, seconds="5", entrypoint=True,
                                   filters=["rsid-store=store::"])
        self.assertEqual(code, 1, output)
        receipt = self.receipt_of(output)
        self.assertEqual((receipt["ok"], receipt["exit_code"]), (False, code))
        expected = [{"package": "rsid-store", "kind": "lib", "target": "rsid_store",
                     "test": "store::assertion"}]
        self.assertEqual(receipt["failures"], expected)
        # invoke() deliberately makes baseline worktree creation unavailable;
        # failures remain named, but cannot be classified as new without it.
        self.assertEqual(receipt["baseline_comparison"]["status"], "unavailable")
        self.assertIsNone(receipt["new_failures"])
        self.assertEqual(receipt["packages"][0]["phase"], "test_failed")
        self.assertIn("SCOPED_FAILURE lib:rsid_store store::assertion",
                      Path(receipt["packages"][0]["log"]).read_text())

    def test_compile_error_is_the_primary_cause_not_unmatched_filters(self):
        error = {"reason": "compiler-message", "message": {
            "level": "error", "message": "couldn't read migrations/v9.rs",
            "spans": [{"is_primary": True, "file_name": "src/store.rs", "line_start": 7}],
            "rendered": "error: couldn't read migrations/v9.rs\n --> src/store.rs:7\n"}}
        self.cargo = self.executable("cargo-stub", f"""
print({json.dumps(json.dumps(error))})
print('error: could not compile `rsid-store`')
sys.exit(101)
""")
        code, output = self.invoke(linux=False, seconds="5", entrypoint=True)
        self.assertEqual(code, 1, output)
        self.assertNotIn("selected no tests", output)
        self.assertIn("rsid-store: build_failed: couldn't read migrations/v9.rs (src/store.rs:7)", output)
        entry = self.receipt_of(output)["packages"][0]
        self.assertEqual((entry["status"], entry["phase"]), ("failed", "build_failed"))
        self.assertEqual(entry["diagnostic"], "couldn't read migrations/v9.rs (src/store.rs:7)")
        self.assertIn("could not compile", Path(entry["log"]).read_text())

    def test_listing_failure_is_reported_as_list_failed(self):
        self.binary = self.executable("test-binary", "sys.exit(3)\n")
        code, output = self.invoke(linux=False, seconds="5", entrypoint=True)
        self.assertEqual(code, 1, output)
        self.assertNotIn("selected no tests", output)
        self.assertEqual(self.receipt_of(output)["packages"][0]["phase"], "list_failed")

    def test_genuinely_empty_filters_report_no_tests_selected(self):
        code, output = self.invoke(linux=False, seconds="5", entrypoint=True,
                                   filters=["rsid=shard:store-01:test(missing)"])
        self.assertEqual(code, 1, output)
        self.assertIn("filters selected no tests: rsid=shard:store-01:test(missing)", output)
        self.assertEqual(self.receipt_of(output)["packages"][0]["phase"], "no_tests_selected")

    def test_ninety_passed_tests_keep_passing_package_when_another_filter_is_empty(self):
        self.binary = self.executable("ninety-tests", """
if '--list' in sys.argv:
    for index in range(90):
        print(f'store::test_{index:02}: test')
else:
    for name in sys.argv[1:]:
        if name.startswith('store::'):
            print(f'test {name} ... ok')
""")
        self.shared_package_build("rsid-store")
        missing = "rsid=shard:store-01:test(missing)"
        code, output = self.invoke(linux=False, entrypoint=True,
                                   filters=["rsid=shard:store-01:test(store::)", missing])
        self.assertEqual(code, 1, output)  # The empty filter still needs attention.
        self.assertIn("SCOPED_COMPLETE rsid-store 90", output)
        receipt = self.receipt_of(output)
        self.assertEqual(self.core_packages(receipt), [
            {"package": "rsid-store", "status": "passed", "completed": 90}])
        self.assertEqual(receipt["unmatched_filters"], [missing])

    def test_mixed_packages_keep_the_failed_phase_and_the_passing_package(self):
        self.shared_package_build("rsid", "rsid-store")
        code, output = self.invoke(linux=False, seconds="5", entrypoint=True,
                                   shard_packages={"store-01": ["rsid", "rsid-store"]})
        self.assertEqual(code, 1, output)
        self.assertNotIn("selected no tests", output)
        phases = {item["package"]: item.get("phase") for item in self.receipt_of(output)["packages"]}
        self.assertEqual(phases, {"rsid": None, "rsid-store": "build_failed"})

    def test_build_failure_stops_before_building_the_next_package(self):
        self.shared_package_build(None, "rsid")
        code, output = self.invoke(linux=False, seconds="5", entrypoint=True,
                                   shard_packages={"store-01": ["rsid", "rsid-store"]})
        self.assertEqual(code, 1, output)
        packages = self.receipt_of(output)["packages"]
        self.assertEqual([item["package"] for item in packages], ["rsid"])
        self.assertEqual(packages[0]["phase"], "build_failed")
        commands = self.log.read_text()
        self.assertEqual(commands.count("cargo-shared-stub test -p"), 1, commands)
        self.assertIn("-p rsid ", commands)
        self.assertNotIn("-p rsid-store ", commands)

    def test_scripts_only_runs_matching_python_tests_without_cargo(self):
        test = self.root / "scripts/tests/test_widget.py"
        test.parent.mkdir(parents=True)
        test.write_text("import unittest\nclass Test(unittest.TestCase):\n    def test_widget(self): pass\n")
        code, output = self.invoke(filters=[], python_paths=["scripts/widget.py"])
        self.assertEqual(code, 0, output)
        self.assertIn("test_widget", output)
        self.assertEqual(list(self.root.glob("commands.log")), [])

    def test_tmpfs_runs_python_tests_under_the_private_tmpdir_and_cleans_up(self):
        parent = self.root / "shm"
        parent.mkdir()
        test = self.root / "scripts/tests/test_widget.py"
        test.parent.mkdir(parents=True)
        test.write_text("import os, unittest\nclass Test(unittest.TestCase):\n"
                        "    def test_widget(self): print('SEEN_TMPDIR=' + os.environ['TMPDIR'])\n")
        with patch.dict(os.environ, CHECK_TOUCHED_SHARDS_TMPDIR=str(parent)):
            code, output = self.invoke(filters=[], python_paths=["scripts/widget.py"], extra_args=["--tmpfs"])
        self.assertEqual(code, 0, output)
        self.assertRegex(output, r"TMPFS_PARITY python tests=1 tmpfs=" + re.escape(str(parent)))
        self.assertRegex(output, r"SEEN_TMPDIR=" + re.escape(str(parent)) + r"/rsi-scoped-test-tmpfs-python-")
        self.assertEqual(list(parent.iterdir()), [])

    def test_default_runs_python_suite_once_under_tmpfs_and_records_completion(self):
        parent = self.root / "shm"
        parent.mkdir()
        test = self.root / "scripts/tests/test_widget.py"
        test.parent.mkdir(parents=True)
        test.write_text("import os, unittest\nclass Test(unittest.TestCase):\n"
                        "    def test_widget(self): print('SEEN_TMPDIR=' + os.environ['TMPDIR'])\n")
        with patch.dict(os.environ, CHECK_TOUCHED_SHARDS_TMPDIR=str(parent)):
            code, output = self.invoke(filters=[], python_paths=["scripts/widget.py"], entrypoint=True)
        self.assertEqual(code, 0, output)
        self.assertEqual(output.count("SEEN_TMPDIR="), 1, output)
        self.assertRegex(output, r"SEEN_TMPDIR=" + re.escape(str(parent)) + r"/rsi-scoped-test-tmpfs-python-")
        receipt = json.loads(output.splitlines()[-1].split(" ", 1)[1])
        self.assertEqual(receipt["scratch_mode"], "tmpfs")
        self.assertEqual(receipt["python"], [{"test_file": "scripts/tests/test_widget.py", "status": "passed",
                                             "completed": True, "runner": "unittest", "executed_tests": 1}])
        self.assertEqual(list(parent.iterdir()), [])

    @unittest.skipUnless(sys.platform.startswith("linux"), "Linux tmpfs parity")
    def test_default_builds_once_and_runs_one_selected_test_under_linux_tmpfs(self):
        parent, reason = RUNNER["tmpfs_parent"]()
        if parent is None:
            self.skipTest(reason)
        self.executable("test-binary", '''
if '--list' in sys.argv:
    print('store::migration: test')
else:
    with Path(os.environ['SCOPED_TEST_LOG']).open('a') as log:
        log.write('SEEN_TMPDIR=' + os.environ['TMPDIR'] + '\\n')
    print('test store::migration ... ok')
''')
        code, output = self.invoke(seconds="5", entrypoint=True)
        self.assertEqual(code, 0, output)
        commands = self.log.read_text().splitlines()
        self.assertEqual(sum(line.startswith("cargo-stub test ") for line in commands), 1)
        self.assertEqual(sum(line.startswith("test-binary store::migration ") for line in commands), 1)
        scratch = next(line.split("=", 1)[1] for line in commands if line.startswith("SEEN_TMPDIR="))
        self.assertEqual(Path(scratch).parent, Path(parent))
        self.assertFalse(Path(scratch).exists())
        receipt = json.loads(output.splitlines()[-1].split(" ", 1)[1])
        self.assertEqual(receipt["scratch_mode"], "tmpfs")
        self.assertEqual(receipt["scratch_parent"], parent)
        self.assertEqual(self.core_packages(receipt), [{"package": "rsid-store", "status": "passed", "completed": 1}])

    def assert_default_fallback_runs_tests(self, linux):
        with patch.dict(os.environ, CHECK_TOUCHED_SHARDS_TMPDIR=str(self.root / "missing")):
            code, output = self.invoke(linux=linux, seconds="5", entrypoint=True)
        self.assertEqual(code, 0, output)
        self.assertIn("TMPFS_PARITY unavailable", output)
        receipt = json.loads(output.splitlines()[-1].split(" ", 1)[1])
        self.assertEqual(receipt["scratch_mode"], "host")
        self.assertTrue(receipt["parity_unavailable"])
        self.assertEqual(self.core_packages(receipt), [{"package": "rsid-store", "status": "passed", "completed": 1}])
        commands = self.log.read_text().splitlines()
        self.assertEqual(sum(line.startswith("cargo-stub test ") for line in commands), 1)
        self.assertEqual(sum(line.startswith("test-binary store::migration ") for line in commands), 1)

    def test_default_runs_tests_when_the_tmpfs_quota_is_below_the_floor(self):
        # #2177: a parent whose user room is under the floor is not used, and the selected tests
        # still run on the host TMPDIR instead of failing with "Disk quota exceeded".
        parent = self.root / "shm"
        parent.mkdir()
        with patch.dict(os.environ, CHECK_TOUCHED_SHARDS_TMPDIR=str(parent),
                        RSI_SCOPED_TEST_TMPFS_MIN_GB="1000000000"):
            code, output = self.invoke(seconds="5", entrypoint=True)
        self.assertEqual(code, 0, output)
        self.assertRegex(output, r"TMPFS_PARITY unavailable: .*quota included.*below 1000000000 GiB")
        receipt = json.loads(output.splitlines()[-1].split(" ", 1)[1])
        self.assertEqual(receipt["scratch_mode"], "host")
        self.assertEqual(self.core_packages(receipt), [{"package": "rsid-store", "status": "passed", "completed": 1}])
        self.assertEqual(list(parent.iterdir()), [])

    def test_tmpfs_parent_counts_the_user_quota_not_just_free_space(self):
        # The quota leaves 3 GiB although the filesystem has far more free: a 12 GiB floor refuses
        # the parent, a 3 GiB floor takes it.
        parent = self.root / "shm"
        parent.mkdir()
        scope = RUNNER["tmpfs_parent"].__globals__
        # tmpfs_parent returns "no tmpfs on <platform>" before any quota arithmetic off Linux, so
        # the quota logic is exercised with the platform faked (user_free_bytes is already faked
        # and `parent` is a real temp directory); the assertions are identical on every host.
        with patch.dict(os.environ, CHECK_TOUCHED_SHARDS_TMPDIR=str(parent)), \
                patch.dict(scope, user_free_bytes=lambda path: 3 << 30), \
                patch.object(sys, "platform", "linux"):
            with patch.dict(os.environ, RSI_SCOPED_TEST_TMPFS_MIN_GB="12"):
                chosen, reason = RUNNER["tmpfs_parent"]()
            self.assertIsNone(chosen)
            self.assertIn("leaves this user 3 GiB (quota included), below 12 GiB", reason)
            with patch.dict(os.environ, RSI_SCOPED_TEST_TMPFS_MIN_GB="3"):
                chosen, reason = RUNNER["tmpfs_parent"]()
            self.assertEqual((chosen, reason), (str(parent), ""))

    @unittest.skipUnless(sys.platform.startswith("linux") and os.path.isdir("/dev/shm"), "Linux /dev/shm")
    def test_user_free_bytes_is_capped_by_a_real_block_quota(self):
        quota = block_quota("/dev/shm")
        if quota is None:
            self.skipTest("no user block quota on /dev/shm")
        hard, used = quota
        stats = os.statvfs("/dev/shm")
        free = stats.f_bavail * stats.f_frsize
        room = RUNNER["user_free_bytes"]("/dev/shm")
        # Other users move free space and the quota meanwhile; allow a 1 GiB drift.
        self.assertLessEqual(room, min(free, max(0, hard - used)) + (1 << 30), (room, free, hard, used))

    def test_default_runs_tests_when_linux_tmpfs_is_unavailable(self):
        self.assert_default_fallback_runs_tests(linux=True)

    def test_default_runs_tests_on_non_linux_without_tmpfs(self):
        self.assert_default_fallback_runs_tests(linux=False)

    def test_python_failure_blocks_the_rust_build(self):
        test = self.root / "tools/tests/test_widget.py"
        test.parent.mkdir(parents=True)
        test.write_text("import unittest\nclass Test(unittest.TestCase):\n"
                        "    def test_widget(self): self.assertEqual(1, 2)\n")
        code, output = self.invoke(python_paths=["tools/widget.py"])
        self.assertEqual(code, 1, output)
        self.assertIn("Python test failed", output)
        self.assertEqual(list(self.root.glob("commands.log")), [])

    def test_python_missing_runner_is_a_typed_receipt_with_zero_tests_and_no_substitute(self):
        test = self.root / "tools/tests/test_widget.py"
        test.parent.mkdir(parents=True)
        test.write_text("def test_widget(): pass\n")
        unavailable = {"status": "runner_unavailable", "runner": "pytest", "python": sys.executable,
                       "test_file": "tools/tests/test_widget.py", "executed_tests": 0,
                       "reason": "No module named pytest"}
        with patch.object(RUNNER["PYTHON_GATE"], "runner_status", return_value=unavailable):
            code, output = self.invoke(python_paths=["tools/widget.py"])
        self.assertEqual(code, 1, output)
        self.assertIn("runner_unavailable runner=pytest executed_tests=0", output)
        self.assertNotIn("Python test failed", output)
        self.assertNotIn("-m unittest", output)
        self.assertEqual(list(self.root.glob("commands.log")), [])
        self.assertEqual(RUNNER["RECEIPT"]["python"], [{**unavailable, "completed": False}])

    def test_python_genuine_failure_stays_distinct_from_an_unavailable_runner(self):
        test = self.root / "tools/tests/test_widget.py"
        test.parent.mkdir(parents=True)
        test.write_text("import unittest\nclass Test(unittest.TestCase):\n"
                        "    def test_widget(self): self.assertEqual(1, 2)\n")
        code, output = self.invoke(python_paths=["tools/widget.py"], entrypoint=True)
        self.assertEqual(code, 1, output)
        self.assertNotIn("runner_unavailable", output)
        self.assertEqual(self.receipt_of(output)["python"], [{
            "test_file": "tools/tests/test_widget.py", "status": "failed", "completed": True,
            "runner": "unittest", "executed_tests": 1}])

    def test_python_suite_that_executes_zero_tests_is_not_green(self):
        test = self.root / "tools/tests/test_widget.py"
        test.parent.mkdir(parents=True)
        test.write_text("import unittest\n")
        with patch.object(RUNNER["PYTHON_GATE"], "test_commands", return_value=[
                [sys.executable, "-c", "print('Ran 0 tests in 0.000s')", "tools/tests/test_widget.py"]]), \
             patch.object(RUNNER["PYTHON_GATE"], "command_runner", return_value="unittest"):
            code, output = self.invoke(python_paths=["tools/widget.py"])
        self.assertEqual(code, 1, output)
        self.assertIn("executed 0 tests", output)
        self.assertEqual((RUNNER["RECEIPT"]["python"][0]["status"], RUNNER["RECEIPT"]["python"][0]["executed_tests"]),
                         ("no_tests_executed", 0))

    def test_python_dry_run_prints_selection_without_running_tests(self):
        test = self.root / "scripts/tests/test_widget.py"
        test.parent.mkdir(parents=True)
        test.write_text("import unittest\n")
        code, output = self.invoke(filters=[], dry_run=True, python_paths=["scripts/widget.py"])
        self.assertEqual(code, 0, output)
        self.assertIn("python test:", output)
        self.assertEqual(list(self.root.glob("commands.log")), [])

    def write_marking_python_test(self, name="test_widget.py"):
        """A selected unittest suite that leaves a marker file when it executes at all."""
        test = self.root / "scripts/tests" / name
        test.parent.mkdir(parents=True, exist_ok=True)
        test.write_text("import unittest\nfrom pathlib import Path\nclass Test(unittest.TestCase):\n"
                        f"    def test_widget(self): Path({str(self.root / 'python-ran.marker')!r}).write_text('ran')\n")

    def python_plan_lines(self, output):
        return [line for line in output.splitlines() if line.startswith("python test: ")]

    def test_dry_run_states_python_before_cargo_for_a_scripts_diff_and_starts_neither(self):
        # #2647: a scripts/ diff names its Python guards, and the order, before any job is submitted.
        self.write_marking_python_test("test_fixture_git_guard.py")
        self.write_marking_python_test("test_widget.py")
        code, output = self.invoke(dry_run=True, entrypoint=True,
                                   python_paths=["scripts/widget.py", "scripts/fixture-git-guard.py"])
        self.assertEqual(code, 0, output)
        lines = self.python_plan_lines(output)
        self.assertEqual(lines, [
            f"python test: {sys.executable} -m unittest -v scripts/tests/test_fixture_git_guard.py",
            f"python test: {sys.executable} -m unittest -v scripts/tests/test_widget.py"])
        order = next(line for line in output.splitlines() if line.startswith("scoped-test: execution order: "))
        self.assertRegex(order, r"Python first \(2 test file\(s\), fail-fast: .*before Cargo starts\), "
                                r"then Cargo second \(1 package\(s\): rsid-store\); this dry run executes no")
        # The order line precedes the Python commands, which precede the Cargo build plan.
        position = {name: output.index(name) for name in (order, lines[0], lines[1], "build once: ")}
        self.assertLess(position[order], position[lines[0]])
        self.assertLess(position[lines[1]], position["build once: "])
        # Neither the suites nor Cargo ran.
        self.assertFalse((self.root / "python-ran.marker").exists())
        self.assertFalse(self.log.exists(), "a dry run must not start Cargo")
        receipt = self.receipt_of(output)
        self.assertEqual(receipt["phase_order"], ["python", "cargo"])
        self.assertEqual(receipt["python"], [
            {"test_file": f"scripts/tests/{name}", "status": "planned", "completed": False,
             "runner": "unittest", "executed_tests": None}
            for name in ("test_fixture_git_guard.py", "test_widget.py")])
        self.assertEqual(receipt["compile"]["execution"], "skipped_dry_run")

    def test_dry_run_selection_matches_the_real_run_and_the_cargo_phase_follows_python(self):
        self.write_marking_python_test()
        code, dry_output = self.invoke(dry_run=True, entrypoint=True, python_paths=["scripts/widget.py"])
        self.assertEqual(code, 0, dry_output)
        self.assertFalse((self.root / "python-ran.marker").exists())
        # A second invocation in the same fixture: the real run, with stub Cargo and a stub unittest-free suite.
        shutil.rmtree(self.root / "output")
        code, real_output = self.invoke(linux=False, seconds="5", entrypoint=True, python_paths=["scripts/widget.py"])
        self.assertEqual(code, 0, real_output)
        self.assertEqual((self.root / "python-ran.marker").read_text(), "ran")
        self.assertEqual(self.python_plan_lines(dry_output), self.python_plan_lines(real_output))
        dry, real = self.receipt_of(dry_output), self.receipt_of(real_output)
        identity = lambda receipt: [(item["test_file"], item["runner"]) for item in receipt["python"]]
        self.assertEqual(identity(dry), identity(real))
        self.assertEqual(identity(dry), [("scripts/tests/test_widget.py", "unittest")])
        self.assertEqual((dry["phase_order"], real["phase_order"]), (["python", "cargo"], ["python", "cargo"]))
        self.assertEqual(real["python"][0]["status"], "passed")
        # The real run keeps the order: Python passed before the stub Cargo ran.
        self.assertRegex(self.log.read_text().splitlines()[0], r"^cargo-stub test -p rsid-store ")

    def test_dry_run_keeps_the_real_python_command_quoting(self):
        self.write_marking_python_test("test_my widget.py")
        code, output = self.invoke(filters=[], dry_run=True, python_paths=["scripts/my widget.py"])
        self.assertEqual(code, 0, output)
        self.assertEqual(self.python_plan_lines(output), [
            f"python test: {sys.executable} -m unittest -v 'scripts/tests/test_my widget.py'"])
        self.assertIn("Python first (1 test file(s)", output)
        self.assertIn("no Cargo package scheduled", output)
        self.assertEqual(RUNNER["RECEIPT"]["phase_order"], ["python"])

    def test_dry_run_reports_a_missing_runner_as_the_typed_entry_without_substituting_one(self):
        test = self.root / "tools/tests/test_widget.py"
        test.parent.mkdir(parents=True)
        test.write_text("def test_widget(): pass\n")
        unavailable = {"status": "runner_unavailable", "runner": "pytest", "python": sys.executable,
                       "test_file": "tools/tests/test_widget.py", "executed_tests": 0,
                       "reason": "No module named pytest"}
        with patch.object(RUNNER["PYTHON_GATE"], "runner_status", return_value=unavailable):
            code, output = self.invoke(filters=[], dry_run=True, python_paths=["tools/widget.py"])
        self.assertEqual(code, 0, output)
        self.assertIn("runner_unavailable runner=pytest executed_tests=0", output)
        self.assertIn("a real run stops here, before Cargo", output)
        self.assertNotIn("-m unittest", output)
        self.assertEqual(RUNNER["RECEIPT"]["python"], [{**unavailable, "completed": False}])
        self.assertEqual(list(self.root.glob("commands.log")), [])

    def test_dry_run_without_python_selection_orders_cargo_alone(self):
        code, output = self.invoke(dry_run=True)
        self.assertEqual(code, 0, output)
        self.assertEqual(self.python_plan_lines(output), [])
        self.assertIn("scoped-test: execution order: Cargo second (1 package(s): rsid-store); this dry run", output)
        self.assertEqual((RUNNER["RECEIPT"]["phase_order"], RUNNER["RECEIPT"]["python"]), (["cargo"], []))

    desktop_selected = {"desktop": {"selected": True, "total": 1, "paths": ["desktop/src/model.ts"]}}

    def desktop_lane_stream(self, code=0, result=None, error=None, after=None):
        """Replace the lane's child process: record the command and emit the lane's result line."""
        self.desktop_commands = []
        reported = {"status": "passed" if not code else "failed", "npm_tests": 31, "cargo_tests": 31,
                    "executed_tests": 62, **(result or {})}

        real = RUNNER["stream"]

        def fake(command, emit, **kwargs):
            if not any(str(part).endswith("desktop-test-gate.py") for part in command):
                return real(command, emit, **kwargs)
            self.desktop_commands.append(command)
            if error:
                raise error
            emit("npm test output\n")
            emit(f"DESKTOP_LANE_RESULT {json.dumps(reported)}\n")
            if after:
                after()
            return code
        return patch.dict(RUNNER["run_desktop_lane"].__globals__, stream=fake)

    def test_a_desktop_only_dry_run_selects_the_desktop_lane_plus_the_static_guards_and_nothing_else(self):
        with self.desktop_lane_stream():
            code, output = self.invoke(filters=[], dry_run=True, python_paths=["desktop/src/model.ts"],
                                       derived_extra=self.desktop_selected)
        self.assertEqual(code, 0, output)
        self.assertEqual(self.desktop_commands, [], "a dry run starts no test")
        receipt = RUNNER["RECEIPT"]
        self.assertEqual(receipt["phase_order"], ["desktop"])
        self.assertEqual(receipt["desktop"], [{"lane": "desktop", "status": "planned", "completed": False,
                                               "runner": "npm+cargo", "executed_tests": None}])
        self.assertEqual((receipt["python"], receipt["filters"], receipt["packages"]), ([], [], []))
        self.assertIn("scoped-test: execution order: the desktop lane", output)
        self.assertRegex(output, r"desktop lane: \S+ \S+desktop-test-gate\.py --run --repo ")
        for name in ("whitespace", "shard_gates", "released_migrations", "starter_pins", "settings_preflight", "identity_assertions", "target_reclaim", "store_boundary", "test_env_writes",
                     "personal_paths", "agents_md_size", "portable_shell"):
            self.assertIn(f"static guard {name}=ok", output)
        self.assertFalse(self.log.exists(), "a dry run must not start Cargo")

    def test_a_dry_run_outside_desktop_does_not_select_the_lane(self):
        code, output = self.invoke(dry_run=True, python_paths=["crates/rsid-store/src/lib.rs"],
                                   derived_extra={"desktop": {"selected": False, "total": 0, "paths": []}})
        self.assertEqual(code, 0, output)
        receipt = RUNNER["RECEIPT"]
        self.assertEqual((receipt["phase_order"], receipt["desktop"]), (["cargo"], []))
        self.assertNotIn("desktop lane", output)

    def test_without_a_derived_selection_the_lane_follows_the_diff_like_the_python_tests_do(self):
        code, output = self.invoke(filters=[], dry_run=True, python_paths=["desktop/package.json"], explicit=[
            "rsid=shard:store-01:test(store::migration)"])
        self.assertEqual(code, 0, output)
        self.assertEqual(RUNNER["RECEIPT"]["phase_order"], ["desktop", "cargo"])
        shutil.rmtree(self.root / "output")
        code, output = self.invoke(dry_run=True, python_paths=["docs/desktop/notes.md"], explicit=[
            "rsid=shard:store-01:test(store::migration)"])
        self.assertEqual(RUNNER["RECEIPT"]["desktop"], [])
        # #3042: the files the desktop suite reads outside desktop/ owe the lane too.
        for path in ("docs/keybindings.md", "crates/rsi/src/keybindings.rs"):
            shutil.rmtree(self.root / "output")
            code, output = self.invoke(filters=[], dry_run=True, python_paths=[path], explicit=[
                "rsid=shard:store-01:test(store::migration)"])
            self.assertEqual((code, RUNNER["RECEIPT"]["phase_order"]), (0, ["desktop", "cargo"]), (path, output))

    def test_a_candidate_without_the_desktop_selector_is_refused_when_its_diff_needs_it(self):
        # #355: the gate script is the selector; a tree without it must not select nothing.
        explicit = ["rsid=shard:store-01:test(store::migration)"]
        for path in ("scripts/desktop-test-gate.py", "desktop/src/model.ts", "docs/keybindings.md"):
            shutil.rmtree(self.root / "output", ignore_errors=True)
            with patch.object(type(self), "selector_present", False):
                code, output = self.invoke(filters=[], dry_run=True, python_paths=[path], explicit=explicit)
            self.assertEqual(code, 1, (path, output))
            self.assertIn("scripts/desktop-test-gate.py is missing from the candidate", output)
            self.assertEqual(RUNNER["RECEIPT"]["desktop"], [])

    def test_a_derived_desktop_error_refuses_the_run(self):
        error = {"desktop": {"selected": True, "total": 1, "paths": ["desktop/a"],
                             "error": "scripts/desktop-test-gate.py is missing from the candidate"}}
        code, output = self.invoke(filters=[], dry_run=True, python_paths=["desktop/a"], derived_extra=error)
        self.assertEqual(code, 1, output)
        self.assertIn("scripts/desktop-test-gate.py is missing", output)

    def test_a_dry_run_reports_a_missing_desktop_runner_as_the_typed_entry(self):
        missing = RUNNER["DESKTOP_GATE"].unavailable("webkit2gtk-4.1", "pkg-config --exists webkit2gtk-4.1 failed")
        with patch.object(RUNNER["DESKTOP_GATE"], "runner_status", return_value=missing):
            code, output = self.invoke(filters=[], dry_run=True, python_paths=["desktop/src/model.ts"],
                                       derived_extra=self.desktop_selected)
        self.assertEqual(code, 0, output)
        self.assertIn("runner_unavailable runner=webkit2gtk-4.1 executed_tests=0 lane=desktop", output)
        self.assertEqual(RUNNER["RECEIPT"]["desktop"], [{**missing, "completed": False}])

    def test_a_real_desktop_run_passes_both_test_sets_before_cargo_starts(self):
        with patch.object(RUNNER["DESKTOP_GATE"], "runner_status", return_value=None), self.desktop_lane_stream():
            code, output = self.invoke(linux=False, seconds="5", python_paths=["desktop/src/model.ts"],
                                       derived_extra=self.desktop_selected)
        self.assertEqual(code, 0, output)
        receipt = RUNNER["RECEIPT"]
        self.assertEqual(receipt["phase_order"], ["desktop", "cargo"])
        self.assertEqual(receipt["desktop"], [{
            "lane": "desktop", "status": "passed", "completed": True, "runner": "npm+cargo", "executed_tests": 62,
            "npm_tests": 31, "cargo_tests": 31, "lane_status": "passed"}])
        self.assertEqual(len(self.desktop_commands), 1)
        self.assertEqual(self.desktop_commands[0][1:3], [str(RUNNER["SCRIPT"].with_name("desktop-test-gate.py")), "--run"])
        self.assertRegex(self.log.read_text().splitlines()[0], r"^cargo-stub test -p rsid-store ")
        self.assertTrue((self.root / "output/desktop-0.log").is_file())

    def test_a_failing_desktop_test_keeps_the_run_red_and_cargo_still_runs(self):
        self.write_marking_python_test()
        with patch.object(RUNNER["DESKTOP_GATE"], "runner_status", return_value=None), \
             patch.dict(RUNNER["main"].__globals__, warning_gate=lambda *_: {"status": "ok", "packages": {}}), \
             self.desktop_lane_stream(1, {"status": "failed", "failed_step": "npm test", "cargo_tests": 0,
                                          "executed_tests": 0}):
            code, output = self.invoke(linux=False, seconds="5", python_paths=["desktop/src/model.ts", "scripts/widget.py"],
                                       filters=["rsid-store=store::migration", "rsi-common=store::migration"],
                                       derived_extra=self.desktop_selected)
        self.assertEqual(code, 1, output)
        self.assertIn("scoped-test: desktop lane failed", output)
        # Serialize the same durable receipt the CLI emits after main returns.
        serialized = io.StringIO()
        with contextlib.redirect_stdout(serialized):
            RUNNER["emit_receipt"](code)
        receipt = self.receipt_of(serialized.getvalue())
        entry = receipt["desktop"][0]
        self.assertEqual((entry["status"], entry["completed"], entry["failed_step"]), ("failed", True, "npm test"))
        self.assertEqual((receipt["ok"], receipt["exit_code"]), (False, 1))
        self.assertEqual(self.core_packages(receipt), [{"package": package, "status": "passed", "completed": 1}
                                              for package in ("rsid-store", "rsi-common")])
        self.assertEqual((receipt["python"][0]["status"], receipt["python"][0]["executed_tests"]), ("passed", 1))
        self.assertEqual(receipt["warnings"]["status"], "ok")
        failure = {"package": "desktop", "kind": "desktop", "target": "desktop", "test": "npm test"}
        self.assertEqual(receipt["failures"], [failure])
        self.assertEqual(receipt["new_failures"], [failure])
        self.assertEqual(receipt["baseline_reds"], [])
        self.assertEqual(receipt["baseline_comparison"]["status"], "not_needed")
        self.assertIn("cargo-stub test -p rsid-store", self.log.read_text())

    def test_a_desktop_failure_stays_new_when_rust_failures_reproduce_on_the_base(self):
        self.executable("test-binary", "print('store::migration: test' if '--list' in sys.argv "
                        "else 'test store::migration ... FAILED')\nsys.exit(0 if '--list' in sys.argv else 1)\n")
        rust_failure = {"package": "rsid-store", "kind": "lib", "target": "rsid_store", "test": "store::migration"}
        compared = []

        def compare(_repo, base, failures, *_args):
            compared.append(failures)
            return {"status": "complete", "base": base, "baseline_reds": [rust_failure]}

        with patch.dict(RUNNER["RECEIPT"], {"schema": 1, "emit": False, "packages": []}, clear=True), \
             patch.object(RUNNER["DESKTOP_GATE"], "runner_status", return_value=None), \
             patch.dict(RUNNER["main"].__globals__, compare_failures_with_base=compare,
                        annotate_baseline_reds=lambda *_: {}), \
             self.desktop_lane_stream(1, {"failed_step": "npm test"}):
            code, output = self.invoke(linux=False, python_paths=["desktop/src/model.ts"],
                                       derived_extra=self.desktop_selected)
            receipt = dict(RUNNER["RECEIPT"])
        self.assertEqual(code, 1, output)
        desktop_failure = {"package": "desktop", "kind": "desktop", "target": "desktop", "test": "npm test"}
        self.assertEqual(compared, [{"rsid-store": [{key: rust_failure[key] for key in ("kind", "target", "test")}]}])
        self.assertEqual(receipt["failures"], [rust_failure, desktop_failure])
        self.assertEqual(receipt["baseline_reds"], [rust_failure])
        self.assertEqual(receipt["new_failures"], [desktop_failure])

    def test_a_missing_desktop_runner_keeps_the_run_red_and_cargo_still_runs(self):
        missing = RUNNER["DESKTOP_GATE"].unavailable("node", "node is not installed")
        with patch.object(RUNNER["DESKTOP_GATE"], "runner_status", return_value=missing), self.desktop_lane_stream():
            code, output = self.invoke(linux=False, seconds="5", python_paths=["desktop/src/model.ts"],
                                       derived_extra=self.desktop_selected)
        self.assertEqual(code, 1, output)
        self.assertIn("runner_unavailable runner=node executed_tests=0 lane=desktop", output)
        self.assertEqual(self.desktop_commands, [])
        entry = RUNNER["RECEIPT"]["desktop"][0]
        self.assertEqual((entry["status"], entry["executed_tests"], entry["completed"]), ("runner_unavailable", 0, False))
        self.assertIn("cargo-stub test -p rsid-store", self.log.read_text())

    def test_a_desktop_step_timeout_is_preserved_in_the_receipt_and_stops_cargo(self):
        with patch.object(RUNNER["DESKTOP_GATE"], "runner_status", return_value=None), \
             self.desktop_lane_stream(1, {"status": "desktop_lane_timed_out", "failed_step": "npm test",
                                          "executed_tests": 0}):
            code, output = self.invoke(linux=False, seconds="5", python_paths=["desktop/src/model.ts"],
                                       derived_extra=self.desktop_selected)
        self.assertEqual(code, 1, output)
        entry = RUNNER["RECEIPT"]["desktop"][0]
        self.assertEqual((entry["status"], entry["lane_status"], entry["failed_step"], entry["completed"]),
                         ("failed", "desktop_lane_timed_out", "npm test", True))
        self.assertIn("scoped_test_timeout:", output)
        self.assertFalse(self.log.exists(), "a desktop timeout stops before Cargo")

    def test_a_desktop_process_timeout_stops_before_cargo(self):
        with patch.object(RUNNER["DESKTOP_GATE"], "runner_status", return_value=None), \
             self.desktop_lane_stream(error=subprocess.TimeoutExpired("desktop", 5)):
            code, output = self.invoke(linux=False, python_paths=["desktop/src/model.ts"],
                                       derived_extra=self.desktop_selected)
        self.assertEqual(code, 1, output)
        self.assertEqual(RUNNER["RECEIPT"]["desktop"][0]["status"], "timed out")
        self.assertIn("scoped_test_timeout:", output)
        self.assertFalse(self.log.exists(), "a desktop timeout stops before Cargo")

    def test_a_desktop_failure_that_spends_the_total_budget_stops_before_cargo(self):
        clock = ControlledClock()
        with patch.dict(RUNNER["main"].__globals__, time=clock), \
             patch.object(RUNNER["DESKTOP_GATE"], "runner_status", return_value=None), \
             self.desktop_lane_stream(1, after=lambda: clock.advance(10)):
            code, output = self.invoke(linux=False, total="10", python_paths=["desktop/src/model.ts"],
                                       derived_extra=self.desktop_selected)
        self.assertEqual(code, 1, output)
        self.assertEqual(RUNNER["RECEIPT"]["desktop"][0]["status"], "failed")
        self.assertIn("scoped_test_timeout: total budget exhausted before Cargo", output)
        self.assertFalse(self.log.exists(), "an exhausted total budget stops before Cargo")

    def test_a_stale_desktop_lock_is_preserved_in_the_receipt_and_cargo_still_runs(self):
        with patch.object(RUNNER["DESKTOP_GATE"], "runner_status", return_value=None), \
             self.desktop_lane_stream(1, {"status": "desktop_lock_stale", "failed_step": "cargo test",
                                          "reason": "refresh desktop/src-tauri/Cargo.lock", "cargo_tests": 0}):
            code, output = self.invoke(linux=False, seconds="5", python_paths=["crates/rsi-common/Cargo.toml"],
                                       derived_extra=self.desktop_selected)
        self.assertEqual(code, 1, output)
        entry = RUNNER["RECEIPT"]["desktop"][0]
        self.assertEqual((entry["status"], entry["lane_status"], entry["failed_step"], entry["completed"]),
                         ("failed", "desktop_lock_stale", "cargo test", True))
        self.assertIn("refresh desktop/src-tauri/Cargo.lock", output)
        self.assertIn("cargo-stub test -p rsid-store", self.log.read_text())

    def test_a_desktop_run_that_exits_zero_without_a_passed_verdict_is_not_green(self):
        with patch.object(RUNNER["DESKTOP_GATE"], "runner_status", return_value=None), \
             self.desktop_lane_stream(0, {"status": "no_tests_executed", "npm_tests": 0, "executed_tests": 31}):
            code, output = self.invoke(linux=False, seconds="5", python_paths=["desktop/src/model.ts"],
                                       derived_extra=self.desktop_selected)
        self.assertEqual(code, 1, output)
        self.assertEqual(RUNNER["RECEIPT"]["desktop"][0]["status"], "no_tests_executed")

    def test_broad_dry_run_reports_derived_guards_and_runs_identity_guard(self):
        code, output = self.invoke(dry_run=True, broad=True)
        self.assertEqual(code, 0, output)
        self.assertIn("BROAD", output)
        for name in ("whitespace", "shard_gates", "released_migrations", "starter_pins", "settings_preflight", "identity_assertions", "target_reclaim", "store_boundary", "test_env_writes",
                     "personal_paths", "agents_md_size", "portable_shell"):
            self.assertIn(f"static guard {name}=ok", output)
        self.assertEqual(self.guard_commands, [
            ["git", "diff", "--check", "origin/rolling", "candidate"],
            ["scripts/repo-guard", "starter-pins", "origin/rolling", "candidate"],
            ["scripts/repo-guard", "settings-preflight", "origin/rolling", "candidate"],
            ["scripts/repo-guard", "identity-assertions"],
            [sys.executable, "scripts/check-target-reclaim.py"],
            [sys.executable, "scripts/check-guard-regressions.py", "origin/rolling", "candidate"],
            [sys.executable, "scripts/check-store-boundary.py"],
            ["scripts/repo-guard", "test-env-writes"],
            ["scripts/repo-guard", "personal-paths"],
            ["scripts/repo-guard", "agents-md-size"],
            ["scripts/repo-guard", "portable-shell", "--base", "origin/rolling"],
        ])
        self.assertEqual(list(self.root.glob("commands.log")), [])

    def test_a_broad_observer_fallback_runs_a_bounded_subset_and_records_the_uncertainty(self):
        """#2105: a broad store:: observer trace is never refused; it runs boxed and says so."""
        broad = [{"filter": "rsid=shard:store-01:test(/^store::/)", "tests": 1954,
                  "reason": "observer trace exceeds 150 functions"},
                 {"filter": "rsid=shard:store-02:test(store::recursive_dag)", "tests": 198,
                  "reason": "observer trace selects 228 tests, exceeding the 24-test bound"}]
        code, output = self.invoke(broad=True, broad_filters=broad, seconds="5", entrypoint=True)
        self.assertEqual(code, 0, output)
        self.assertIn("rsid-store: passed (1 completed tests)", output)
        receipt = json.loads(output.splitlines()[-1].split(" ", 1)[1])
        self.assertEqual((receipt["ok"], receipt["plan"], receipt["uncertain"]),
                         (True, "targeted_subset", "broad_observers"))
        self.assertEqual((receipt["broad_filter_count"], receipt["broad_tests"], receipt["gate_covers_rest"]),
                         (2, 2152, True))
        self.assertEqual(receipt["broad_filters"], broad)
        self.assertEqual(receipt["test_time_box_sec"], 600)
        self.assertIn("uncertain: broad_observers", output)
        self.assertNotIn("*", json.dumps(receipt["filters"]))

    def test_one_expression_under_two_shard_labels_is_one_broad_selection(self):
        """#213: the shard label only routes a filter, so `/^store::/` under two labels is one selection."""
        broad = [{"filter": "rsid=shard:other-01:test(/^store::/)", "tests": 2189,
                  "reason": "observer trace exceeds 150 functions"},
                 {"filter": "rsid=shard:store-01:test(/^store::/)", "tests": 2189,
                  "reason": "observer trace exceeds 150 functions"},
                 {"filter": "rsid=shard:store-01:test(/^store::cold_archive::/)", "tests": 2189,
                  "reason": "a different expression with an equal count stays its own selection"}]
        code, output = self.invoke(broad=True, broad_filters=broad, seconds="5", entrypoint=True)
        self.assertEqual(code, 0, output)
        receipt = json.loads(output.splitlines()[-1].split(" ", 1)[1])
        self.assertEqual((receipt["broad_filter_count"], receipt["broad_filter_entries"], receipt["broad_tests"]),
                         (2, 3, 4378))
        self.assertEqual(receipt["broad_filters"], broad)
        self.assertIn("uncertain: broad_observers (2 filters, 4378 tests)", output)

    SESSION = "rsid=shard:session-01:test(/^session::/)"
    ROTATION = "rsid=shard:session-03:test(/^session::rotation::/)"

    def bounded_derivation(self, **overrides):
        entry = {"filter": self.SESSION, "tests": 2696, "path": "crates/rsid/src/session/mod.rs",
                 "reason": "observer trace exceeds 150 functions", "stop": "function bound", "worker_bounded": True,
                 "direct_tests": 245}
        derived = {
            "suggested_filters": [self.SESSION, "rsid=shard:store-02:test(store::sessions)"],
            "broad_filters": [entry, {**entry, "filter": self.ROTATION, "tests": 155}],
            "merged_filters": [
                {"filter": self.ROTATION, "covered_by": self.SESSION,
                 "reason": "same-shard module filter already selects every matching test"},
                {"filter": "rsid=shard:session-01:test(/^session::scan::(?:\\w+::)*catalog_pin$/)", "covered_by": self.SESSION,
                 "reason": "same-shard module filter already selects every matching test"},
                {"filter": "rsid=shard:session-05:test(/^session::/)", "covered_by": self.SESSION,
                 "reason": "identical expression selects the same unsharded library packages"},
                {"filter": "rsid=shard:session-05:test(/^session::scan::(?:\\w+::)*other_pin$/)",
                 "covered_by": "rsid=shard:session-05:test(/^session::/)",
                 "reason": "same-shard module filter already selects every matching test"}],
        }
        derived.update(overrides)
        return derived

    def test_a_cap_stopped_module_filter_is_bounded_and_the_named_filters_it_absorbed_return(self):
        bounded = RUNNER["bounded_worker_selection"](self.bounded_derivation())
        self.assertEqual(bounded["bounded"], [self.SESSION])
        # The absorbed scanner pins return, through the identical-expression twin too; the nested
        # broad filter (rotation) is covered by the bounded one and is not restored as a named test.
        self.assertEqual(sorted(bounded["restored"]), [
            "rsid=shard:session-01:test(/^session::scan::(?:\\w+::)*catalog_pin$/)",
            "rsid=shard:session-05:test(/^session::scan::(?:\\w+::)*other_pin$/)"])
        self.assertIn("rsid=shard:store-02:test(store::sessions)", bounded["filters"])
        self.assertEqual(bounded["broad_filters_left"], [])
        # One identity each: the anchored rotation filter sits inside the session filter.
        self.assertEqual(RUNNER["scope_tests_at_most"](bounded["entries"]), 2696)

    def test_an_ambiguity_or_an_untraceable_diff_keeps_the_broad_worker_filter(self):
        entry = self.bounded_derivation()["broad_filters"][0]
        for change in ({"stop": "unresolved helper identity", "worker_bounded": False},
                       {"stop": None, "worker_bounded": False}):
            derived = self.bounded_derivation(broad_filters=[{**entry, **change}])
            self.assertIsNone(RUNNER["bounded_worker_selection"](derived), change)
        # An older checker has no flag; one unbounded entry for the same filter keeps it broad.
        legacy = {key: value for key, value in entry.items() if key not in ("stop", "worker_bounded")}
        self.assertIsNone(RUNNER["bounded_worker_selection"](self.bounded_derivation(broad_filters=[legacy])))
        mixed = self.bounded_derivation(broad_filters=[entry, {**entry, "worker_bounded": False, "stop": None}])
        self.assertIsNone(RUNNER["bounded_worker_selection"](mixed))

    BOUNDED_STORE = "rsid=shard:store-01:test(/^store::/)"

    def bounded_invoke(self, **kwargs):
        self.binary = self.executable("test-binary", """
if '--list' in sys.argv:
    print('store::migration: test\\nstore::far_one: test\\nstore::far_two: test\\nscheduler::outside: test')
else:
    print('test result: ok. 1 passed')
""")
        entry = {"filter": self.BOUNDED_STORE, "tests": 1954, "reason": "observer trace exceeds 150 functions",
                 "stop": "function bound", "worker_bounded": True, "path": "crates/rsid-store/src/store/mod.rs"}
        extra = {"direct_observer_filters": [{"filter": "rsid=shard:store-01:test(=store::migration)", "distance": 1},
                                             {"filter": "rsid=shard:store-01:test(=scheduler::outside)", "distance": 2}],
                 "direct_observers": {"tests": 2, "by_distance": {"1": 1, "2": 1}}}
        return self.invoke(filters=[self.BOUNDED_STORE], broad=True, broad_filters=[entry], derived_extra=extra, **kwargs)

    def test_a_cap_overflow_run_names_its_deferred_scope_routes_and_pending_status(self):
        code, output = self.bounded_invoke(seconds="5", entrypoint=True)
        self.assertEqual(code, 0, output)
        receipt = self.receipt_of(output)
        self.assertEqual((receipt["plan"], receipt["worker_policy"], receipt["worker_broad_filters"], receipt["gate_covers_rest"]),
                         ("targeted_subset", "bounded_deferred", 0, True))
        deferred = receipt["deferred"]
        self.assertEqual((deferred["status"], deferred["routes"], deferred["candidate"]),
                         ("pending", ["merge_queue_gate", "pinned_full_qa"], "candidate"))
        # The two far tests the module filter matches were listed and neither run nor counted passed.
        self.assertEqual((deferred["tests"], receipt["deferred_tests"], deferred["scope_tests_at_most"]), (2, 2, 1954))
        self.assertEqual(receipt["filters_bounded"], [{"filter": self.BOUNDED_STORE, "kept": 1, "executed": 1, "deferred": 2}])
        # The direct observer outside the module filter is selected in its own right and run.
        self.assertEqual((receipt["selected_tests"], receipt["run_tests"]), (2, 2))
        self.assertEqual((receipt["direct_observer_tests_selected"], receipt["direct_observer_tests_run"]), (2, 2))
        self.assertEqual((receipt["selected_filters"], receipt["run_filters"]), (0, 0))
        self.assertNotIn("store::far_one", self.log.read_text())
        self.assertIn("scheduler::outside", self.log.read_text())
        self.assertIn("(bounded)", output)

    def test_a_cap_overflow_dry_run_names_the_deferred_scope_without_running_anything(self):
        code, output = self.bounded_invoke(dry_run=True, entrypoint=True)
        self.assertEqual(code, 0, output)
        receipt = self.receipt_of(output)
        deferred = receipt["deferred"]
        self.assertEqual((deferred["status"], deferred["tests"], deferred["scope_tests_at_most"]), ("pending", None, 1954))
        self.assertEqual(deferred["direct_observers"], {"tests": 2, "by_distance": {"1": 1, "2": 1}})
        self.assertEqual(receipt["worker_broad_filters"], 0)
        self.assertIn("are NOT widened", output)
        self.assertFalse(self.log.exists())

    def test_a_derivation_without_the_worker_flag_keeps_the_broad_fallback_unchanged(self):
        broad = [{"filter": "rsid=shard:store-01:test(/^store::/)", "tests": 1954, "reason": "observer trace exceeds 150 functions"}]
        code, output = self.invoke(filters=[self.BOUNDED_STORE], broad=True, broad_filters=broad, seconds="5", entrypoint=True)
        self.assertEqual(code, 0, output)
        receipt = self.receipt_of(output)
        self.assertNotIn("deferred", receipt)
        self.assertNotIn("worker_policy", receipt)

    def test_a_clean_plan_has_no_uncertainty(self):
        code, output = self.invoke(seconds="5", entrypoint=True)
        receipt = json.loads(output.splitlines()[-1].split(" ", 1)[1])
        self.assertEqual((code, receipt["plan"]), (0, "precise"))
        self.assertNotIn("uncertain", receipt)

    def test_a_derivation_failure_names_its_reason_in_the_receipt(self):
        code, output = self.invoke(derive_code=1, dry_run=True, entrypoint=True,
                                   derived_extra={"merge_clean": False, "ok": False})
        self.assertEqual(code, 1, output)
        receipt = json.loads(output.splitlines()[-1].split(" ", 1)[1])
        self.assertEqual(len(receipt["derive_failure"]), 1)
        self.assertIn("does not merge cleanly", receipt["derive_failure"][0])

    def test_declined_broad_plan_still_runs_and_reports_all_missing_guards(self):
        code, output = self.invoke(derive_code=2, broad=True, audits={},
                                   guard_failure="scripts/check-rsid-test-shards.py")
        self.assertEqual(code, 2, output)
        self.assertEqual(self.guard_commands, [
            ["git", "diff", "--check", "origin/rolling", "candidate"],
            [sys.executable, "scripts/check-rsid-test-shards.py", "--require-gates"],
            [sys.executable, "tools/check-released-migrations.py", "origin/rolling", "candidate"],
            ["scripts/repo-guard", "starter-pins", "origin/rolling", "candidate"],
            ["scripts/repo-guard", "settings-preflight", "origin/rolling", "candidate"],
            ["scripts/repo-guard", "identity-assertions"],
            [sys.executable, "scripts/check-target-reclaim.py"],
            [sys.executable, "scripts/check-guard-regressions.py", "origin/rolling", "candidate"],
            [sys.executable, "scripts/check-store-boundary.py"],
            ["scripts/repo-guard", "test-env-writes"],
            ["scripts/repo-guard", "personal-paths"],
            ["scripts/repo-guard", "agents-md-size"],
            ["scripts/repo-guard", "portable-shell", "--base", "origin/rolling"],
        ])
        self.assertIn("static guard shard_gates=fail: guard finding", output)
        self.assertIn("static guard released_migrations=ok", output)
        self.assertIn("static guard identity_assertions=ok", output)
        self.assertEqual(list(self.root.glob("commands.log")), [])

    def test_a_guard_script_missing_from_the_candidate_is_skipped_not_failed(self):
        # #3126: the tool copy is rolling's, the guard scripts are the candidate's. A stack that
        # predates a guard must still run its tests; the merge queue runs the guard on the merged tree.
        code, output = self.invoke(explicit=["rsid-store=migration"],
                                   missing_guards=("crates/rsi-repo-guards/src/starter_pins.rs",), entrypoint=True)
        self.assertEqual(code, 0, output)
        receipt = json.loads(output.splitlines()[-1].removeprefix("SCOPED_TEST_RECEIPT "))
        self.assertEqual(receipt["static_guards"]["starter_pins"], {
            "status": "skipped",
            "detail": "crates/rsi-repo-guards/src/starter_pins.rs not in candidate; merge queue runs it on the merged tree",
        })
        self.assertEqual(receipt["static_guards"]["identity_assertions"]["status"], "ok")
        self.assertIn("static guard starter_pins=skipped: crates/rsi-repo-guards/src/starter_pins.rs not in candidate", output)
        self.assertIn("static guard identity_assertions=ok", output)
        self.assertEqual(len(self.guard_commands), 12)  # thirteen guards, one missing
        self.assertNotIn(
            ["scripts/repo-guard", "starter-pins", "origin/rolling", "candidate"], self.guard_commands)

    def test_a_present_guard_that_fails_still_blocks_when_another_is_missing(self):
        code, output = self.invoke(explicit=["rsid-store=migration"], guard_failure="portable-shell",
                                   missing_guards=("crates/rsi-repo-guards/src/starter_pins.rs",), entrypoint=True)
        self.assertEqual(code, 1, output)
        receipt = json.loads(output.splitlines()[-1].removeprefix("SCOPED_TEST_RECEIPT "))
        self.assertEqual(receipt["static_guards"]["starter_pins"]["status"], "skipped")
        self.assertEqual(receipt["static_guards"]["portable_shell"], {
            "status": "fail", "detail": "guard finding",
        })
        self.assertIn("static guard starter_pins=skipped", output)
        self.assertIn("static guard portable_shell=fail: guard finding", output)

    def test_explicit_narrowing_runs_all_guards_and_each_failure_blocks_build(self):
        guards = (("diff", "whitespace"),
                  ("scripts/check-rsid-test-shards.py", "shard_gates"),
                  ("tools/check-released-migrations.py", "released_migrations"),
                  ("starter-pins", "starter_pins"),
                  ("settings-preflight", "settings_preflight"),
                  ("identity-assertions", "identity_assertions"),
                  ("scripts/check-target-reclaim.py", "target_reclaim"),
                  ("scripts/check-guard-regressions.py", "guard_regressions"),
                  ("scripts/check-store-boundary.py", "store_boundary"),
                  ("test-env-writes", "test_env_writes"),
                  ("personal-paths", "personal_paths"),
                  ("agents-md-size", "agents_md_size"),
                  ("portable-shell", "portable_shell"))
        for path, name in guards:
            with self.subTest(guard=name):
                code, output = self.invoke(explicit=["rsid-store=migration"], guard_failure=path)
                self.assertEqual(code, 1, output)
                self.assertEqual(len(self.guard_commands), len(guards))
                self.assertIn(f"static guard {name}=fail: guard finding", output)
                self.assertEqual(list(self.root.glob("commands.log")), [])
                shutil.rmtree(self.root / "output")

    def test_declined_derived_guard_failure_is_reported_and_identity_still_runs(self):
        code, output = self.invoke(derive_code=1, broad=True, audits={
            "shard_gates": {"verdict": "fail", "detail": "ungated test"},
            "released_migrations": {"verdict": "ok"},
        })
        self.assertEqual(code, 1, output)
        self.assertIn("static guard shard_gates=fail: ungated test", output)
        self.assertIn("static guard identity_assertions=ok", output)
        self.assertIn("static guard portable_shell=ok", output)
        self.assertEqual(self.guard_commands, [
            ["git", "diff", "--check", "origin/rolling", "candidate"],
            ["scripts/repo-guard", "starter-pins", "origin/rolling", "candidate"],
            ["scripts/repo-guard", "settings-preflight", "origin/rolling", "candidate"],
            ["scripts/repo-guard", "identity-assertions"],
            [sys.executable, "scripts/check-target-reclaim.py"],
            [sys.executable, "scripts/check-guard-regressions.py", "origin/rolling", "candidate"],
            [sys.executable, "scripts/check-store-boundary.py"],
            ["scripts/repo-guard", "test-env-writes"],
            ["scripts/repo-guard", "personal-paths"],
            ["scripts/repo-guard", "agents-md-size"],
            ["scripts/repo-guard", "portable-shell", "--base", "origin/rolling"],
        ])
        self.assertEqual(list(self.root.glob("commands.log")), [])

    def test_explicit_filters_reproduce_on_base_without_diff_derivation(self):
        code, output = self.invoke(filters=[], explicit=["rsid-store=store::migration"],
                                   head="base", worktree_head="base", seconds="5")
        self.assertEqual(code, 0, output)
        self.assertIn("explicit filters: head=base", output)
        self.assertIn("rsid-store: passed (1 completed tests)", output)
        commands = self.log.read_text()
        self.assertIn("RuntimeMaxSec=5", commands)
        self.assertIn("CPUQuota=75%", commands)
        self.assertIn("--user --collect --wait --pipe", commands)

    def test_repeated_explicit_filters_replace_derived_selection_and_deduplicate(self):
        code, output = self.invoke(filters=["rsid-store=missing"], seconds="5",
                                   explicit=["rsid-store=migration", "rsid-store=store::migration"])
        self.assertEqual(code, 0, output)
        self.assertEqual(output.count("SCOPED_MATCH "), 2)
        self.assertEqual(output.count("SCOPED_START "), 1)
        self.assertEqual(self.log.read_text().count("cargo-stub test"), 1)

    def test_recipe_filters_from_the_environment_replace_derived_selection(self):
        # #1584: the daemon hands a recipe's focused filters over as one
        # newline-separated environment entry.
        code, output = self.invoke(filters=["rsid=missing"], seconds="5",
                                   env_filters="rsid-store=migration\nrsid-store=store::migration\n")
        self.assertEqual(code, 0, output)
        self.assertIn("explicit filters:", output)
        self.assertEqual(output.count("SCOPED_MATCH "), 2)
        self.assertEqual(self.log.read_text().count("cargo-stub test"), 1)

    def test_explicit_filters_win_over_the_recipe_environment(self):
        code, output = self.invoke(seconds="5", explicit=["rsid-store=migration"],
                                   env_filters="rsid-store=missing")
        self.assertEqual(code, 0, output)
        self.assertNotIn("missing", output)

    def test_total_budget_caps_each_package_and_skips_those_it_cannot_afford(self):
        # #1584: sequential package budgets must fit inside the job's own cap.
        filters = ["rsid=migration", "rsid-store=migration"]
        clock = ControlledClock()
        stream = RUNNER["stream"]

        def run_package(*args, **kwargs):
            code = stream(*args, **kwargs)
            # #499: charge a fixed package cost, independent of host scheduling.
            clock.advance(1.3)
            return code

        with patch.dict(RUNNER["main"].__globals__, time=clock, stream=run_package):
            code, output = self.invoke(filters=filters, explicit=filters, seconds="5", total="2")
        self.assertEqual(code, 1, output)
        commands = self.log.read_text()
        self.assertEqual(re.findall(r"RuntimeMaxSec=(\S+) ", commands), ["2"], commands)
        self.assertEqual(commands.count("cargo-stub test"), 1)
        self.assertIn("scoped_test_timeout: total budget exhausted before rsid-store", output)
        self.assertIn("rsid: passed (1 completed tests)", output)
        self.assertIn("rsid-store: timed out (0 completed tests)", output)

    def test_makefile_scoped_test_budgets_fit_inside_the_default_test_job_cap(self):
        root = Path(__file__).resolve().parents[2]
        make = (root / "Makefile").read_text()
        for target in ("scoped-test", "scoped-test-tmpfs"):
            block = re.search(rf"^{target}:\n(?:\t.*\n)+", make, re.M)[0]
            total = int(re.search(r"--total-max-sec (\d+)", block)[1])
            per_package = int(re.search(r"--runtime-max-sec (\d+)", block)[1])
            cap = 20 * 60  # Default durable test-job timeout; recipes are retired (#1954).
            self.assertLessEqual(per_package, total, target)
            self.assertLess(total, cap, target)

    def test_explicit_dry_run_prints_caps_without_building(self):
        code, output = self.invoke(dry_run=True, explicit=["rsid-store=migration"],
                                   head="base", worktree_head="candidate")
        self.assertEqual(code, 0, output)
        self.assertIn("RuntimeMaxSec=5", output)
        self.assertIn("CPUQuota=75%", output)
        self.assertIn("rsid-store=migration", output)
        self.assertEqual(list(self.root.glob("commands.log")), [])

    def test_explicit_execution_requires_the_selected_worktree_head(self):
        with self.assertRaisesRegex(ValueError, "--head differs"):
            self.invoke(explicit=["rsid-store=migration"], head="base")
        self.assertEqual(list(self.root.glob("commands.log")), [])

    def test_explicit_zero_match_fails(self):
        code, output = self.invoke(explicit=["rsid-store=missing"], seconds="5")
        self.assertEqual(code, 1, output)
        self.assertIn("filters selected no tests: rsid-store=missing", output)

    def test_invalid_explicit_selection_fails_before_build(self):
        with self.assertRaisesRegex(ValueError, "unscoped filter"):
            self.invoke(explicit=["rsid="])
        self.assertEqual(list(self.root.glob("commands.log")), [])

    def test_default_empty_diff_still_runs_no_tests(self):
        code, output = self.invoke(filters=[])
        self.assertEqual(code, 0, output)
        self.assertIn("no test filters derived; no tests run", output)
        self.assertEqual(list(self.root.glob("commands.log")), [])

    def test_explicit_named_target_selects_exact_test_and_builds_target_once(self):
        for kind in ("bin", "test"):
            with self.subTest(kind=kind):
                self.binary = self.executable("test-binary", """
if '--list' in sys.argv:
    print('tests::one: test\\ntests::one_more: test\\ntests::other: test')
else:
    assert sys.argv[1:] == ['tests::one', '--exact', '--nocapture', '--test-threads=1']
    print('test result: ok. 1 passed')
""")
                artifact = {"reason": "compiler-artifact", "profile": {"test": True},
                            "manifest_path": str(self.root / "Cargo.toml"),
                            "executable": str(self.binary), "target": {"kind": [kind], "name": "tool"}}
                self.cargo = self.executable("cargo-stub", f"print({json.dumps(json.dumps(artifact))})\n")
                code, output = self.invoke(seconds="5", explicit=[f"rsid={kind}:tool:test(=tests::one)",
                                                                 f"rsid={kind}:tool:test(/^tests::one$/)"])
                self.assertEqual(code, 0, output)
                self.assertIn("rsid: passed (1 completed tests)", output)
                self.assertEqual(self.log.read_text().splitlines()[1:], [
                    f'cargo-stub test -p rsid --{kind} tool --no-run --message-format=json',
                    'test-binary --list --format=terse',
                    'test-binary tests::one --exact --nocapture --test-threads=1'])
                shutil.rmtree(self.root / "output")
                self.log.unlink()

    def test_dry_run_derives_filters_and_prints_caps_without_running_commands(self):
        code, output = self.invoke(dry_run=True)
        self.assertEqual(code, 0, output)
        self.assertIn("RuntimeMaxSec=5", output)
        self.assertIn("CPUQuota=75%", output)
        self.assertIn("test(store::migration)", output)
        self.assertEqual(list(self.root.glob("commands.log")), [])

    def test_unfiltered_leaf_derivation_still_runs_mandatory_token_scanners(self):
        names = ["store::cold_archive::read_classes::cold_archive_reader_classification",
                 "store::cold_archive::read_tests::cold_archive_reader_invariance"]
        pins = [f"rsid=shard:store-01:test(={name})" for name in names]
        code, output = self.invoke(dry_run=True, filters=[], derived_extra={
            "unfiltered_leaf_paths": ["crates/rsid-remote-read/src/history.rs"],
            "scanned_token_filters": pins,
        })
        self.assertEqual(code, 0, output)
        for name in names:
            self.assertIn(name, output)
        self.assertEqual(list(self.root.glob("commands.log")), [])

    def test_unfiltered_leaf_derivation_runs_the_leafs_own_tests(self):
        code, output = self.invoke(dry_run=True, filters=[], derived_extra={
            "unfiltered_leaf_paths": ["crates/rsid-remote-read/src/lib.rs"],
            "worker_filters": ["rsid-remote-read=history"],
        })
        self.assertEqual(code, 0, output)
        # invoke() captures plans in the fixture output directory; main() does
        # not emit the CLI entry point's durable receipt.
        plan = json.loads((self.root / "output" / "rsid-remote-read.json").read_text())
        self.assertEqual(plan["package"], "rsid-remote-read")
        self.assertEqual(plan["selections"][0]["substring"], "history")
        self.assertEqual(list(self.root.glob("commands.log")), [])

    def test_linux_runs_foreground_service_and_reports_passed(self):
        code, output = self.invoke()
        self.assertEqual(code, 0, output)
        commands = self.log.read_text()
        self.assertIn("--user --collect --wait --pipe", commands)
        self.assertIn("rsid-store: passed", output)
        self.assertIn("1 completed tests", output)

    def test_reclaim_incremental_between_successful_packages(self):
        cache = self.root / "target" / "debug" / "incremental"
        cache.mkdir(parents=True)
        (cache / "cache-file").write_bytes(b"disposable incremental state")
        retained = self.root / "target" / "debug" / "deps"
        retained.mkdir(parents=True)
        (retained / "compiled-dependency").write_bytes(b"keep")
        filters = ["rsid=migration", "rsid-store=migration"]

        code, output = self.invoke(filters=filters, explicit=filters, seconds="5",
                                   reclaim_incremental=True)

        self.assertEqual(code, 0, output)
        self.assertFalse(cache.exists())
        self.assertEqual((retained / "compiled-dependency").read_bytes(), b"keep")
        self.assertEqual(self.log.read_text().count("cargo-stub test"), 2)
        self.assertIn("reclaimed", output)

    def test_clean_client_exit_without_worker_completion_fails(self):
        code, output = self.invoke(systemd_body="""
print('SCOPED_MATCH rsid=shard:store-01:test(store::migration)', flush=True)
print('SCOPED_START store::migration', flush=True)
print('SCOPED_END store::migration', flush=True)
""")
        self.assertEqual(code, 1, output)
        self.assertIn("scoped_test_incomplete", output)
        self.assertIn("rsid-store: failed (1 completed tests)", output)

    def test_a_worker_killed_by_a_signal_is_named_with_its_unfinished_tests_not_as_a_time_box(self):
        # #146: a test that signals its own process group killed the worker after 617 tests on macOS;
        # the run read as a bare incomplete next to a 600 s box. The signal and the tests the worker
        # had started and not finished are named, in the message and in the package's receipt entry.
        code, output = self.invoke(entrypoint=True, systemd_body="""
import signal
print('SCOPED_MATCH rsid=shard:store-01:test(store::migration)', flush=True)
print('SCOPED_START store::migration::done', flush=True)
print('SCOPED_END store::migration::done', flush=True)
print('SCOPED_START store::migration::cut_a', flush=True)
print('SCOPED_START store::migration::cut_b', flush=True)
os.kill(os.getpid(), signal.SIGKILL)
""")
        self.assertEqual(code, 1, output)
        incomplete = next(line for line in output.splitlines() if line.startswith("scoped_test_incomplete"))
        self.assertIn("1 completed tests", incomplete)
        self.assertIn("killed by SIGKILL", incomplete)
        self.assertIn("not a time-box expiry", incomplete)
        self.assertIn("store::migration::cut_a, store::migration::cut_b", incomplete)
        self.assertNotIn("scoped_test_timeout", output)
        receipt = json.loads(output.splitlines()[-1].split(" ", 1)[1])
        entry = next(item for item in receipt["packages"] if item["package"] == "rsid-store")
        self.assertEqual(entry["status"], "failed")
        self.assertEqual(entry["worker_signal"], "SIGKILL")
        self.assertEqual(entry["unfinished_tests"], ["store::migration::cut_a", "store::migration::cut_b"])
        self.assertEqual(entry["unfinished_total"], 2)

    def test_worker_end_names_the_signal_and_bounds_the_unfinished_list(self):
        worker_end = RUNNER["worker_end"]
        self.assertEqual(worker_end(0, {}), ("", {}))
        self.assertEqual(worker_end(1, {}), ("", {}))
        suffix, fields = worker_end(-signal.SIGTERM, {})
        self.assertIn("killed by SIGTERM", suffix)
        self.assertEqual(fields, {"worker_signal": "SIGTERM"})
        suffix, fields = worker_end(0, {"b": 1.0, "a": 2.0})
        self.assertIn("started and not finished, not failures: a, b", suffix)
        self.assertEqual((fields["unfinished_tests"], fields["unfinished_total"]), (["a", "b"], 2))
        many = {f"t{index:03}": 0.0 for index in range(RUNNER["UNFINISHED_LIMIT"] + 5)}
        suffix, fields = worker_end(1, many)
        self.assertEqual(len(fields["unfinished_tests"]), RUNNER["UNFINISHED_LIMIT"])
        self.assertEqual(fields["unfinished_total"], RUNNER["UNFINISHED_LIMIT"] + 5)
        self.assertIn("(+5 more)", suffix)

    def test_termination_overrides_successful_completion_receipt(self):
        for outcome in ("code=killed, status=15/TERM", "code=dumped, status=11/SEGV",
                        "Finished with result: signal"):
            with self.subTest(outcome=outcome):
                code, output = self.invoke(systemd_body=f"""
print('SCOPED_MATCH rsid=shard:store-01:test(store::migration)')
print('SCOPED_START store::migration')
print('SCOPED_END store::migration')
print('SCOPED_COMPLETE rsid-store 1')
print({outcome!r})
""")
                self.assertEqual(code, 1, output)
                self.assertIn("scoped_test_incomplete", output)
                self.assertIn("rsid-store: failed (1 completed tests)", output)
                shutil.rmtree(self.root / "output")

    def test_completion_receipt_must_match_completed_count(self):
        code, output = self.invoke(systemd_body="""
print('SCOPED_MATCH rsid=shard:store-01:test(store::migration)')
print('SCOPED_START store::migration')
print('SCOPED_END store::migration')
print('SCOPED_COMPLETE rsid-store 2')
""")
        self.assertEqual(code, 1, output)
        self.assertIn("scoped_test_incomplete", output)
        self.assertIn("rsid-store: failed (1 completed tests)", output)

    def test_non_linux_success_requires_and_accepts_completion_receipt(self):
        code, output = self.invoke(linux=False)
        self.assertEqual(code, 0, output)
        self.assertIn("rsid-store: passed (1 completed tests)", output)

    def test_timeout_with_clean_client_exit_fails(self):
        code, output = self.invoke(systemd_body="""
print('SCOPED_MATCH rsid=shard:store-01:test(store::migration)')
print('SCOPED_START store::migration')
print('Finished with result: timeout')
""")
        self.assertEqual(code, 1, output)
        self.assertIn("scoped_test_timeout", output)
        self.assertIn("rsid-store: timed out (0 completed tests)", output)

    def test_timeout_retains_passed_priority_filter_coverage_and_a_red_receipt(self):
        # The worker is killed inside a batch, before it can emit SCOPED_SUBSET (#448).
        # Listing and verdicts must preserve the completed priority regex, the partial
        # broad observer, and the exact tests that never reached a verdict.
        priority = "rsid=shard:store-01:test(/^case::pri_[ab]$/)"
        broad = "rsid-store=case::"
        pending = ["rsid=shard:store-01:test(=case::hang)", "rsid=shard:store-01:test(=case::unrun)"]
        self.binary = self.executable("test-binary", """
import time
if '--list' in sys.argv:
    for name in ('case::pri_a', 'case::pri_b', 'case::hang', 'case::unrun'):
        print(name + ': test')
else:
    for name in sys.argv[1:]:
        if name.startswith('case::'):
            if name == 'case::hang':
                print('test ' + name + ' ... ', flush=True)
                time.sleep(60)
            print('test ' + name + ' ... ok', flush=True)
""")
        code, output = self.invoke(
            linux=False, seconds="2", entrypoint=True, filters=[broad, *pending],
            derived_extra={"priority_filters": [priority]},
            extra_args=["--filter-budget", "1", "--test-shards", "1"])
        receipt = self.receipt_of(output)
        self.assertEqual(code, 1, output)
        self.assertFalse(receipt["ok"])
        self.assertTrue(receipt["packages"], output)
        self.assertEqual(receipt["packages"][0]["status"], "timed out")
        self.assertEqual(receipt["packages"][0]["completed"], 2)
        self.assertIn("test case::pri_a ... ok", output)
        self.assertIn("test case::pri_b ... ok", output)
        self.assertEqual((receipt["selected_tests"], receipt["run_tests"]), (4, 2))
        self.assertEqual((receipt["selected_filters"], receipt["run_filters"]), (4, 1))
        self.assertEqual(receipt["filters_partial"], [{"filter": broad, "selected": 4, "executed": 2}])
        self.assertEqual(receipt["filters_not_run"], pending)
        self.assertEqual(receipt["unlisted_packages"], [])

    def test_interrupted_coverage_keeps_identical_names_in_different_binaries_separate(self):
        records = [{"selected": ["same::test"], "ran": ran, "by_filter": {"shared": ["same::test"]},
                    "changed": ["same::test"], "direct": [], "bounded": {}, "deferred": []}
                   for ran in (["same::test"], [])]
        subset = RUNNER["subset_progress"]("rsid", records, True, 1)
        self.assertEqual(subset["filters"], {"shared": [2, 1]})
        self.assertEqual(subset["changed_module_tests"], [2, 1])
        self.assertEqual((subset["selected_tests"], subset["run_tests"]), (2, 1))

    @unittest.skipUnless(sys.platform.startswith("linux") and shutil.which("systemd-run"),
                         "requires Linux systemd")
    def test_stopped_systemd_unit_fails_despite_clean_client_exit(self):
        if subprocess.run(["systemctl", "--user", "show-environment"],
                          capture_output=True).returncode:
            self.skipTest("requires a running systemd user manager")
        systemd_run = shutil.which("systemd-run")
        self.binary = self.executable("test-binary", """
if '--list' in sys.argv:
    print('store::migration: test')
else:
    import time
    time.sleep(30)
""")
        code, output = self.invoke(seconds="10", systemd_body=f"""
import subprocess
unit = next(arg.split('=', 1)[1] for arg in sys.argv if arg.startswith('--unit='))
client = subprocess.Popen([{systemd_run!r}, '--setenv=SCOPED_TEST_LOG=' + os.environ['SCOPED_TEST_LOG'],
                           *sys.argv[1:]], stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True)
try:
    for line in client.stdout:
        print(line, end='', flush=True)
        if line.startswith('SCOPED_START '):
            subprocess.run(['systemctl', '--user', 'stop', unit], check=True, timeout=5)
    result = client.wait(timeout=5)
    print('STOPPED_CLIENT_EXIT ' + str(result), flush=True)
    sys.exit(result)
finally:
    subprocess.run(['systemctl', '--user', 'stop', unit], check=False, capture_output=True, timeout=5)
""")
        self.assertIn("SCOPED_START store::migration", output)
        self.assertIn("code=killed", output)
        self.assertIn("STOPPED_CLIENT_EXIT 0", output)
        self.assertEqual(code, 1, output)
        self.assertIn("scoped_test_incomplete", output)
        self.assertIn("rsid-store: failed (0 completed tests)", output)

    def test_systemd_runtime_timeout_reports_active_test_and_nonzero(self):
        code, output = self.invoke(systemd_body="""
print('SCOPED_START store::slow', flush=True)
print('Finished with result: timeout', flush=True)
sys.exit(1)
""")
        self.assertEqual(code, 1, output)
        self.assertIn("scoped_test_timeout", output)
        self.assertIn("store::slow (", output)
        self.assertIn("rsid-store: timed out", output)
        self.assertIn("scoped_test_timeout", (self.root / "output/rsid-store.log").read_text())

    def test_non_linux_fallback_times_out_silent_build(self):
        self.cargo = self.executable("cargo-sleep", "import time\ntime.sleep(30)\n")
        code, output = self.invoke(linux=False)
        self.assertEqual(code, 1, output)
        self.assertIn("plain wall-clock timeout, no CPU cap", output)
        self.assertIn("scoped_test_timeout", output)
        self.assertIn("none (build/list/startup)", output)

    def test_forced_short_service_budget_times_out_an_active_test(self):
        self.binary = self.executable("test-binary", """
if '--list' in sys.argv:
    print('store::migration: test')
else:
    import time
    time.sleep(30)
""")
        code, output = self.invoke(seconds="1", systemd_body="""
import signal, subprocess
seconds = float(next(arg.split('=', 1)[1] for arg in sys.argv if arg.startswith('RuntimeMaxSec=')))
index = sys.argv.index('--execute-plan')
child = subprocess.Popen([sys.executable, '-u', sys.argv[index - 1], *sys.argv[index:]], start_new_session=True)
try:
    sys.exit(child.wait(timeout=seconds))
except subprocess.TimeoutExpired:
    os.killpg(child.pid, signal.SIGKILL)
    child.wait()
    print('Finished with result: timeout', flush=True)
    sys.exit(1)
""")
        self.assertEqual(code, 1, output)
        self.assertIn("scoped_test_timeout: slowest still-running tests: store::migration (", output)
        self.assertIn("rsid-store: timed out", output)

    def test_test_failure_is_reported_nonzero(self):
        self.binary = self.executable("test-binary", """
if '--list' in sys.argv:
    print('store::migration: test')
else:
    sys.exit(101)
""")
        code, output = self.invoke()
        self.assertEqual(code, 1, output)
        self.assertIn("rsid-store: failed", output)

    def test_worker_uses_the_artifact_package_directory(self):
        directory = self.root / "package"
        directory.mkdir()
        artifact = {"reason": "compiler-artifact", "profile": {"test": True},
                    "manifest_path": str(directory / "Cargo.toml"),
                    "executable": str(self.binary), "target": {"kind": ["lib"], "name": "rsid_store"}}
        self.cargo = self.executable("cargo-stub", f"print({json.dumps(json.dumps(artifact))})\n")
        self.binary = self.executable("test-binary", f"""
assert Path.cwd() == Path({str(directory)!r})
print('store::migration: test' if '--list' in sys.argv else 'test result: ok. 1 passed')
""")
        code, output = self.invoke()
        self.assertEqual(code, 0, output)

    def test_build_failure_is_not_reported_as_timeout(self):
        code, output = self.invoke(systemd_body="sys.exit(101)\n")
        self.assertEqual(code, 1, output)
        self.assertIn("rsid-store: failed", output)

    def test_zero_test_selection_fails(self):
        code, output = self.invoke(filters=["rsid=shard:store-01:test(missing)"])
        self.assertEqual(code, 1, output)
        self.assertIn("filters selected no tests", output)
        self.assertIn("rsid-store: failed", output)

    def shared_package_build(self, matching_package, failed_package=None):
        empty_binary = self.executable("empty-binary", "print('other::test: test')\n")
        self.cargo = self.executable("cargo-shared-stub", f"""
import json
package = sys.argv[sys.argv.index('-p') + 1]
if package == {failed_package!r}:
    sys.exit(101)
binary = {str(self.binary)!r} if package == {matching_package!r} else {str(empty_binary)!r}
print(json.dumps({{'reason': 'compiler-artifact', 'profile': {{'test': True}},
                  'manifest_path': {str(self.root / 'Cargo.toml')!r},
                  'executable': binary, 'target': {{'kind': ['lib'], 'name': package}}}}))
""")

    def test_shared_filter_accepts_empty_leg_when_either_package_matches(self):
        for matching in ("rsid", "rsid-store"):
            with self.subTest(matching=matching):
                self.shared_package_build(matching)
                code, output = self.invoke(seconds="5", shard_packages={"store-01": ["rsid", "rsid-store"]})
                self.assertEqual(code, 0, output)
                other = "rsid-store" if matching == "rsid" else "rsid"
                self.assertIn(f"{matching}: passed (1 completed tests)", output)
                self.assertIn(f"{other}: not applicable (0 completed tests)", output)
                self.assertIn(f"SCOPED_COMPLETE {other} 0", output)
                self.assertIn("not applicable", (self.root / f"output/{other}.log").read_text())
                shutil.rmtree(self.root / "output")

    def test_shared_filter_fails_when_neither_package_matches(self):
        self.shared_package_build(None)
        code, output = self.invoke(seconds="5", shard_packages={"store-01": ["rsid", "rsid-store"]})
        self.assertEqual(code, 1, output)
        self.assertIn("filters selected no tests", output)
        for package in ("rsid", "rsid-store"):
            self.assertIn(f"{package}: failed (0 completed tests)", output)

    def test_shared_filters_require_a_match_for_each_filter(self):
        self.shared_package_build("rsid")
        missing = "rsid=shard:store-01:test(missing)"
        code, output = self.invoke(seconds="5", shard_packages={"store-01": ["rsid", "rsid-store"]},
                                   filters=["rsid=shard:store-01:test(store::migration)", missing])
        self.assertEqual(code, 1, output)
        self.assertIn("filters selected no tests: " + missing, output)

    def test_shared_filter_does_not_mask_build_or_test_failures(self):
        for failure in ("build", "test"):
            with self.subTest(failure=failure):
                if failure == "test":
                    self.binary = self.executable("test-binary", """
if '--list' in sys.argv:
    print('store::migration: test')
else:
    sys.exit(101)
""")
                self.shared_package_build("rsid", "rsid-store" if failure == "build" else None)
                code, output = self.invoke(seconds="5", shard_packages={"store-01": ["rsid", "rsid-store"]})
                self.assertEqual(code, 1, output)
                failed = "rsid-store" if failure == "build" else "rsid"
                self.assertIn(f"{failed}: failed", output)
                shutil.rmtree(self.root / "output")

    def test_empty_leg_requires_successful_completion(self):
        for ending in ("", "print('SCOPED_COMPLETE rsid-store 0')\nprint('code=killed, status=15/TERM')"):
            with self.subTest(ending=ending):
                code, output = self.invoke(seconds="5", shard_packages={"store-01": ["rsid", "rsid-store"]},
                                           systemd_body="""
index = sys.argv.index('--execute-plan')
if Path(sys.argv[index + 1]).stem == 'rsid':
    os.execv(sys.executable, [sys.executable, '-u', sys.argv[index - 1], *sys.argv[index:]])
""" + ending)
                self.assertEqual(code, 1, output)
                self.assertIn("rsid: passed (1 completed tests)", output)
                self.assertIn("scoped_test_incomplete", output)
                self.assertIn("rsid-store: failed (0 completed tests)", output)
                shutil.rmtree(self.root / "output")

    def test_unscoped_and_unknown_filters_are_refused(self):
        for value in ("rsid=", "rsid-store=", "rsid=shard:store-01:all()", "rsi=all()",
                      "rsid=bin:tool:test()", "rsid=bin:tool:test(=)", "rsid=bin:tool:test(a b)",
                      "rsid=bin:tool:test(/(/)", "rsid=test:tool:test(/a/b/)", "rsid=bin:tool:all()"):
            with self.subTest(value=value), self.assertRaises((ValueError, re.error)):
                RUNNER["selection_plan"]([value], {"store-01": ["rsid-store"]})

    def test_exact_and_regex_atoms_select_their_own_tests_only(self):
        # #1440: check-touched-shards names exact module paths and tests, not prefixes.
        plan = RUNNER["selection_plan"]([
            "rsid=shard:store-01:test(/^store::tests::[A-Za-z0-9_]+$/)",
            "rsid=shard:store-01:test(/^store::(?:\\w+::)*(?:a|b)$/)",
            "rsid=shard:store-01:test(=store::tests::one)",
        ], {"store-01": ["rsid-store"]})["rsid-store"]
        self.assertEqual([(item["regex"], item["exact"]) for item in plan], [
            ("^store::tests::[A-Za-z0-9_]+$", None),
            ("^store::(?:\\w+::)*(?:a|b)$", None),
            (None, "store::tests::one"),
        ])
        for value in ("rsid=shard:store-01:test(/a/b/)", "rsid=shard:store-01:test(/(/)", "rsid=shard:store-01:test(a b)"):
            with self.subTest(value=value), self.assertRaises((ValueError, re.error)):
                RUNNER["selection_plan"]([value], {"store-01": ["rsid-store"]})

    def test_windows_build_does_not_require_unix_env(self):
        with patch("os.name", "nt"):
            command = RUNNER["build_command"]("rsi", [{"kind": "lib"}],
                                               ["env", "-u", "RSI_PROCESS_OWNERSHIP_NAMESPACE", "cargo"])
        self.assertEqual(command[:5], ["cargo", "test", "-p", "rsi", "--lib"])

    def test_fixture_isolates_scoped_test_filters_from_recipe_environment(self):
        """#1796: invoke() clears RSI_SCOPED_TEST_FILTERS so recipe job filters
        don't leak into fixture tests. Verify by setting a real shard filter and
        asserting the default test still runs."""
        # Set a real shard filter in the parent environment as a recipe job would
        with patch.dict(os.environ, {"RSI_SCOPED_TEST_FILTERS": "session-05"}):
            # The fixture should clear this and use stub derivation instead
            code, output = self.invoke(linux=False, seconds="5")
        self.assertEqual(code, 0, output)
        self.assertIn("rsid-store: passed (1 completed tests)", output)

    def test_named_targets_are_explicit_in_build(self):
        selections = RUNNER["selection_plan"](["rsi=bin:tool", "rsi=test:integration"], {})["rsi"]
        self.assertEqual(RUNNER["build_command"]("rsi", selections, ["cargo-stub"]),
                         ["cargo-stub", "test", "-p", "rsi", "--bin", "tool", "--test", "integration",
                          "--no-run", "--message-format=json"])

    # --- the warning gate inside a whole supervised run (#1897) ---

    def warning_run_with(self, *messages, baseline=None, base=None, env=None):
        lines = [json.dumps(message) for message in messages]
        self.cargo = self.executable("cargo-stub", "\n".join(f"print({json.dumps(line)})" for line in lines) + "\n")
        if baseline is not None:
            (self.root / "tools").mkdir(exist_ok=True)
            (self.root / "tools" / "warning-baseline.json").write_text(json.dumps(baseline))
        self.base_baseline = base
        with patch.dict(os.environ, env or {}):
            code, output = self.invoke(linux=False, seconds="5", entrypoint=True)
        receipt = json.loads(output.splitlines()[-1].split(" ", 1)[1])
        return code, output, receipt

    def warning_messages(self, *warnings):
        return [cargo_artifact(), *warnings, cargo_artifact(
            test=True, executable=str(self.binary)) | {"manifest_path": str(self.root / "Cargo.toml")}]

    def test_known_warnings_keep_the_run_green_and_reach_the_receipt(self):
        code, output, receipt = self.warning_run_with(*self.warning_messages(*known_warnings()), baseline=baseline_document())
        self.assertEqual(code, 0, output)
        self.assertIn("scoped-test: warnings rsid-store: 2 observed, baseline 2, 0 new", output)
        self.assertEqual(receipt["warnings"]["status"], "ok")
        self.assertTrue(receipt["ok"])

    def test_a_new_warning_makes_the_run_non_green(self):
        extra = cargo_diagnostic(f"{STORE}/mod.rs", 9, "unused variable: `stale`", code="unused_variables")
        code, output, receipt = self.warning_run_with(*self.warning_messages(*known_warnings(), extra), baseline=baseline_document())
        self.assertEqual(code, 1, output)
        self.assertIn("scoped-test: new warning x1: " + f"{STORE}/mod.rs|unused_variables|unused variable: `stale`", output)
        self.assertIn("scoped-test: warning_gate_failed: new_warnings", output)
        self.assertEqual(receipt["warnings"]["status"], "new_warnings")
        self.assertFalse(receipt["ok"])

    def test_suppressed_warnings_are_unavailable_evidence_not_a_clean_count(self):
        code, output, receipt = self.warning_run_with(*self.warning_messages(), baseline=baseline_document(),
                                              env={"RUSTFLAGS": "-A warnings"})
        self.assertEqual(code, 1, output)
        self.assertIn("scoped-test: warning_evidence_unavailable: compiler flags suppress warnings", output)
        self.assertEqual(receipt["warnings"]["status"], "unavailable")

    def test_a_raised_baseline_is_refused(self):
        raised = baseline_document({**KNOWN, f"{STORE}/mod.rs|dead_code|function `x` is never used": 1})
        extra = cargo_diagnostic(f"{STORE}/mod.rs", 9, "function `x` is never used")
        code, output, receipt = self.warning_run_with(*self.warning_messages(*known_warnings(), extra), baseline=raised,
                                              base=baseline_document())
        self.assertEqual(code, 1, output)
        self.assertEqual(receipt["warnings"]["status"], "baseline_raised")
        self.assertIn("scoped-test: warning_baseline_raised: rsid-store: baseline raised", output)
        self.assertEqual(receipt["reverts_recent_landing"]["status"], "skipped")

    def test_no_checked_in_baseline_leaves_the_run_and_its_receipt_alone(self):
        code, output, receipt = self.warning_run_with(*self.warning_messages(*known_warnings()))
        self.assertEqual(code, 0, output)
        self.assertNotIn("warnings", receipt)
        self.assertNotIn("warning_gate_failed", output)

    def test_a_failed_build_reports_no_warning_verdict(self):
        failing = self.executable("cargo-stub", "print(" + repr(json.dumps(cargo_artifact())) + ")\nsys.exit(101)\n")
        self.cargo = failing
        (self.root / "tools").mkdir(exist_ok=True)
        (self.root / "tools" / "warning-baseline.json").write_text(json.dumps(baseline_document()))
        code, output = self.invoke(linux=False, seconds="5", entrypoint=True)
        self.assertEqual(code, 1, output)
        self.assertNotIn("warning_gate_failed", output)


class WhitespaceGuardTest(unittest.TestCase):
    """#3060: scoped-test runs the lander's `git diff --check` so a stack the worker saw green is not
    refused for whitespace before any test runs."""

    def run_guards(self, tail):
        temp = scratch_tempdir(prefix="scoped-test-whitespace-")
        self.addCleanup(temp.cleanup)
        repo = Path(temp.name) / "repo"
        repo.mkdir()
        scratch_git(repo, "init", "--quiet")
        scratch_git(repo, "config", "user.name", "Test")
        scratch_git(repo, "config", "user.email", "test@example.invalid")
        (repo / "graphs.ts").write_text("export const a = 1;\n")
        scratch_git(repo, "add", "graphs.ts")
        scratch_git(repo, "commit", "--quiet", "-m", "base")
        base = scratch_git(repo, "rev-parse", "HEAD").stdout.strip()
        (repo / "graphs.ts").write_text("export const a = 1;\nexport const b = 2;\n" + tail)
        scratch_git(repo, "commit", "--quiet", "-am", "candidate")
        head = scratch_git(repo, "rev-parse", "HEAD").stdout.strip()
        real_run = subprocess.run

        def run(command, **kwargs):
            if command[:3] == ["git", "diff", "--check"]:
                return real_run(command, **kwargs)
            return subprocess.CompletedProcess(command, 0, "", "")
        audits = {name: {"verdict": "ok"} for name in
                  ("shard_gates", "released_migrations", "identity_assertions", "target_reclaim", "portable_shell")}
        out = io.StringIO()
        with patch.object(subprocess, "run", run), contextlib.redirect_stdout(out):
            failed = RUNNER["static_guards"](repo, base, head, audits)
        return failed, out.getvalue()

    def test_a_blank_line_at_eof_fails_the_whitespace_guard(self):
        failed, output = self.run_guards("\n")
        self.assertTrue(failed, output)
        self.assertIn("static guard whitespace=fail", output)
        self.assertIn("new blank line at EOF", output)

    def test_a_clean_candidate_passes_the_whitespace_guard(self):
        failed, output = self.run_guards("")
        self.assertFalse(failed, output)
        self.assertIn("static guard whitespace=ok", output)


class ScopedTestReceipt(unittest.TestCase):
    """#1638: the daemon's scoped-test job reads the last-line JSON receipt.

    #3256: the dry runs target an empty fixture repository, never this checkout. The real checkout's
    static guards (a deferred migration pin, a dirty tool tree) would otherwise decide these
    receipt tests; a fixture without the guard scripts reports every guard as skipped."""

    @classmethod
    def setUpClass(cls):
        cls._fixture_temp = scratch_tempdir(prefix="scoped-test-receipt-")
        cls.fixture = Path(cls._fixture_temp.name) / "repo"
        cls.fixture.mkdir()
        scratch_git(cls.fixture, "init", "--quiet")
        scratch_git(cls.fixture, "config", "user.name", "Test")
        scratch_git(cls.fixture, "config", "user.email", "test@example.invalid")
        scratch_git(cls.fixture, "commit", "--quiet", "--allow-empty", "-m", "base")

    @classmethod
    def tearDownClass(cls):
        cls._fixture_temp.cleanup()

    def run_script(self, *args, extra_env=None):
        # A recipe job exports focused filters that would replace the derived plan, and a
        # scoped_test job exports the tool's source and rev (#1938); neither may leak in.
        leaked = {"RSI_SCOPED_TEST_FILTERS", "RSI_SCOPED_TEST_TOOL_SOURCE", "RSI_SCOPED_TEST_TOOL_REV",
                  "RSI_SCOPED_TEST_TOOL_FILES"}
        env = {k: v for k, v in os.environ.items() if k not in leaked}
        env["RSI_SCOPED_TEST_RUNNER"] = "libtest"
        env.update(extra_env or {})
        done = subprocess.run([sys.executable, str(SCRIPT), "--repo", str(self.fixture), *args], cwd=SCRIPT.parents[1],
                              text=True, capture_output=True, env=env)
        lines = done.stdout.splitlines()
        self.assertTrue(lines and lines[-1].startswith("SCOPED_TEST_RECEIPT "), done.stdout + done.stderr)
        return done.returncode, json.loads(lines[-1].split(" ", 1)[1])

    def test_a_dry_run_ends_with_the_typed_receipt(self):
        code, receipt = self.run_script("--dry-run", "--base", "HEAD", "--filter", "rsid-core=error")
        self.assertEqual(code, 0)
        self.assertEqual((receipt["ok"], receipt["exit_code"], receipt["schema"]), (True, 0, 1))
        self.assertEqual(receipt["base"], "HEAD")
        self.assertEqual(receipt["filters"], ["rsid-core=error"])
        self.assertRegex(receipt["head"], r"^[0-9a-f]{40}$")
        self.assertTrue(receipt["log_dir"].startswith("/tmp/") or Path(receipt["log_dir"]).is_dir())

    def many_filters(self, count=14):
        args = []
        for index in range(count):
            args += ["--filter", f"rsid-core=name_{index}"]
        return args

    def test_a_plan_within_the_filter_budget_is_precise(self):
        code, receipt = self.run_script("--dry-run", "--base", "HEAD", "--filter", "rsid-core=error")
        self.assertEqual(code, 0)
        self.assertEqual(receipt["plan"], "precise")
        self.assertNotIn("broadened", receipt)

    def test_a_plan_over_the_filter_budget_is_a_targeted_subset_never_whole_packages(self):
        code, receipt = self.run_script("--dry-run", "--base", "HEAD", *self.many_filters())
        self.assertEqual(code, 0)
        self.assertEqual((receipt["plan"], receipt["filter_budget"], receipt["test_time_box_sec"]),
                         ("targeted_subset", 12, 600))
        self.assertEqual((receipt["selected_filters"], receipt["gate_covers_rest"]), (14, True))
        self.assertEqual(len(receipt["filters"]), 14)
        self.assertNotIn("broadened", receipt)
        self.assertNotIn("rsid-core=*", json.dumps(receipt))
        self.assertEqual(receipt["packages"], [])

    def test_a_tiny_runtime_budget_no_longer_refuses_a_targeted_subset(self):
        code, receipt = self.run_script("--dry-run", "--base", "HEAD", "--runtime-max-sec", "10",
                                        *self.many_filters())
        self.assertEqual((code, receipt["plan"], receipt["ok"]), (0, "targeted_subset", True))

    def test_the_receipt_records_runner_threads_and_quota_within_the_job_quota(self):
        code, receipt = self.run_script("--dry-run", "--base", "HEAD", "--cpu-quota", "300",
                                        "--filter", "rsid-core=error")
        self.assertEqual(code, 0)
        self.assertEqual(receipt["runner"], "libtest")
        self.assertTrue(2 <= receipt["adaptive_threads"] <= 12, receipt)
        self.assertEqual(receipt["test_threads"], min(receipt["adaptive_threads"], 3))
        self.assertEqual(receipt["test_shards"], receipt["test_threads"])
        self.assertLessEqual(receipt["cpu_quota"], 300)
        self.assertEqual(receipt["cpu_quota"], receipt["test_threads"] * 100)
        self.assertEqual(receipt["job_cpu_quota"], 300)
        self.assertGreaterEqual(receipt["host_threads"], 1)

    def test_a_job_quota_below_two_cores_caps_the_adaptive_threads(self):
        _code, receipt = self.run_script("--dry-run", "--base", "HEAD", "--cpu-quota", "100",
                                         "--filter", "rsid-core=error")
        self.assertEqual((receipt["test_threads"], receipt["cpu_quota"]), (1, 100))

    def test_an_explicit_runner_that_is_unavailable_fails_loudly(self):
        code, receipt = self.run_script("--dry-run", "--base", "HEAD", "--runner", "nextest",
                                        "--filter", "rsid-core=error",
                                        extra_env={"PATH": "/nonexistent", "RSI_SCOPED_TEST_RUNNER": ""})
        # PATH is gone, so the script is run by an absolute interpreter and finds no cargo-nextest.
        self.assertEqual(code, 1)
        self.assertFalse(receipt["ok"])

    def test_filter_budget_zero_keeps_the_precise_plan(self):
        code, receipt = self.run_script("--dry-run", "--base", "HEAD", "--filter-budget", "0",
                                        *self.many_filters())
        self.assertEqual((code, receipt["plan"]), (0, "precise"))

    def test_the_receipt_names_the_copy_that_ran(self):
        """#1938: path and git blob SHA of the running script, plus the daemon's source and rev."""
        # Read-only query about a tracked file: explicit `-C`, never the caller's cwd.
        blob = subprocess.run(["git", "-C", str(SCRIPT.parent), "hash-object", str(SCRIPT)], text=True,
                              capture_output=True, check=True).stdout.strip()
        args = ("--dry-run", "--base", "HEAD", "--filter", "rsid=shard:other-01:test(x)")
        _, receipt = self.run_script(*args)
        self.assertEqual(receipt["tool"], {"path": str(SCRIPT), "blob": blob,
                                           "source": "worktree", "rev": None})
        rev = "0123456789abcdef0123456789abcdef01234567"
        _, receipt = self.run_script(*args, extra_env={"RSI_SCOPED_TEST_TOOL_SOURCE": "rolling",
                                                       "RSI_SCOPED_TEST_TOOL_REV": rev})
        self.assertEqual((receipt["tool"]["source"], receipt["tool"]["rev"]), ("rolling", rev))
        self.assertEqual(receipt["tool"]["blob"], blob)
        self.assertNotIn("files", receipt["tool"])

    def test_the_receipt_names_each_helper_file_beside_the_script(self):
        """Hub #3024: the daemon's file list is echoed with each helper's blob; a missing one is null."""
        helper = SCRIPT.parent / "python-test-gate.py"
        blob = subprocess.run(["git", "-C", str(SCRIPT.parent), "hash-object", str(helper)], text=True,
                              capture_output=True, check=True).stdout.strip()
        args = ("--dry-run", "--base", "HEAD", "--filter", "rsid=shard:other-01:test(x)")
        names = "python-test-gate.py,not-a-helper-for-test.py,../escape"
        _, receipt = self.run_script(*args, extra_env={"RSI_SCOPED_TEST_TOOL_SOURCE": "candidate",
                                                       "RSI_SCOPED_TEST_TOOL_FILES": names})
        self.assertEqual(receipt["tool"]["files"], {"python-test-gate.py": blob,
                                                    "not-a-helper-for-test.py": None,
                                                    "../escape": None})

    def test_a_failed_run_still_ends_with_a_red_receipt(self):
        code, receipt = self.run_script("--base", "no-such-ref-for-receipt-test")
        self.assertNotEqual(code, 0)
        self.assertEqual((receipt["ok"], receipt["exit_code"]), (False, code))
        self.assertEqual(receipt["base"], "no-such-ref-for-receipt-test")

    def test_the_package_executor_prints_no_receipt(self):
        done = subprocess.run([sys.executable, str(SCRIPT), "--execute-plan", "/nonexistent"],
                              text=True, capture_output=True)
        self.assertNotIn("SCOPED_TEST_RECEIPT", done.stdout)


class ScopedTestPlanningNeverCompilesTest(unittest.TestCase):
    """#2564: the real CLI plans (derivation and --dry-run) with a cargo that refuses to compile.

    The scratch repository sits outside this sandbox and every Git call goes through `scratch_git`.
    """

    def setUp(self):
        sandbox = SCRIPT.parents[1]
        self.temp = scratch_tempdir(prefix="scoped-planning-")
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name).resolve()
        self.assertFalse(self.root.is_relative_to(sandbox))
        self.repo, self.bin, self.log = self.root / "repo", self.root / "bin", self.root / "cargo.log"
        self.repo.mkdir()
        self.bin.mkdir()
        stub = ("#!/bin/sh\n"
                'echo "$*" >> "$CARGO_LOG"\n'
                'echo "stub cargo: refusing $*" >&2\n'
                "exit 97\n")
        for name in ("cargo", "cargo-stub"):
            (self.bin / name).write_text(stub)
            (self.bin / name).chmod(0o755)
        guard = "import sys\nsys.exit(0)\n"
        # The Rust guards (#360) are absent here, so they are skipped and start no Cargo build.
        for rel in ("tools/check-released-migrations.py", "scripts/check-target-reclaim.py", "scripts/check-guard-regressions.py",
                    "scripts/check-store-boundary.py"):
            self.write(rel, guard)
        self.write("scripts/check-rsid-test-shards.py",
                   "import sys\nif '--list-shard-packages' in sys.argv:\n    print('store-02 rsid')\n")
        gate = '#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-02"))]\n#[test]\nfn a() {}\n'
        self.write("crates/rsid/src/foo.rs", gate)
        self.git("init", "-q", "-b", "rolling")
        self.git("config", "user.name", "T")
        self.git("config", "user.email", "t@example.invalid")
        self.git("add", ".")
        self.git("commit", "-q", "-m", "base")
        self.git("branch", "base")
        self.write("crates/rsid/src/foo.rs", gate + "pub fn changed() {}\n")
        self.git("add", "crates")
        self.git("commit", "-q", "-m", "change")

    def git(self, *args):
        scratch_git(self.repo, *args)

    def write(self, rel, text):
        path = self.repo / rel
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(text)

    def scoped(self, *args):
        leaked = {"RSI_SCOPED_TEST_FILTERS", "RSI_SCOPED_TEST_TOOL_SOURCE", "RSI_SCOPED_TEST_TOOL_REV",
                  "RSI_SCOPED_TEST_TOOL_FILES", "CARGO_TARGET_DIR", "RSI_CARGO_SLOT_HELD"}
        env = {k: v for k, v in os.environ.items() if k not in leaked}
        env.update(RSI_SCOPED_TEST_RUNNER="libtest", CHECK_TOUCHED_SHARDS_CARGO=str(self.bin / "cargo-stub"),
                   CARGO_LOG=str(self.log), HOME=str(self.root),
                   PATH=str(self.bin) + os.pathsep + env.get("PATH", ""))
        done = subprocess.run([sys.executable, str(SCRIPT), "--repo", str(self.repo), "--base", "base", *args],
                              cwd=self.root, text=True, capture_output=True, env=env)
        lines = done.stdout.splitlines()
        self.assertTrue(lines and lines[-1].startswith("SCOPED_TEST_RECEIPT "), done.stdout + done.stderr)
        return done, json.loads(lines[-1].split(" ", 1)[1])

    def test_dry_run_derivation_and_plan_never_start_cargo(self):
        done, receipt = self.scoped("--dry-run")
        self.assertEqual(done.returncode, 0, done.stdout + done.stderr)
        self.assertIn("--filters-only", done.stdout)
        self.assertFalse(self.log.exists(), self.log.read_text() if self.log.exists() else "")
        self.assertTrue(receipt["filters"], receipt)
        self.assertIn("build once: " + str(self.bin / "cargo-stub"), done.stdout)
        self.assertEqual(receipt["compile"], {"planning": {"source": "filters_only", "cargo_invoked": False},
                                              "execution": "skipped_dry_run", "packages": {}})

    def downstream_fixture(self):
        self.write("Cargo.toml", '[workspace]\nmembers = ["crates/*"]\n'
                   '[workspace.dependencies]\ncommon = { package = "rsi-common", path = "crates/rsi-common" }\n')
        for name, deps in (("rsi-common", ""), ("rsid", '[dev-dependencies]\ncommon.workspace = true\n'),
                           ("rsi", '[target.\'cfg(unix)\'.dependencies]\nrsid = { path = "../rsid" }\n'),
                           ("unrelated", "")):
            self.write(f"crates/{name}/Cargo.toml", f'[package]\nname = "{name}"\nversion = "0.1.0"\n' + deps)
            self.write(f"crates/{name}/src/lib.rs", "pub fn unchanged() {}\n")
        # This is the #441 shape: inferred registry iteration changes across the crate boundary.
        self.write("crates/rsi-common/src/lib.rs", "pub mod health_anomaly;\n")
        self.write("crates/rsi-common/src/health_anomaly.rs", 'pub const ALL_KINDS: [&str; 1] = ["one"];\n')
        self.git("add", "Cargo.toml", "crates")
        self.git("commit", "-q", "-m", "workspace fixture")

    def downstream_change(self, source, path="crates/rsi-common/src/health_anomaly.rs"):
        self.write(path, source)
        self.git("add", path)
        self.git("commit", "-q", "-m", "API change")

    def test_public_common_api_diff_plans_transitive_dependent_test_compilation(self):
        self.downstream_fixture()
        self.downstream_change('pub const ALL_KINDS: &[&str] = &["one"];\n')
        done, receipt = self.scoped("--dry-run", "--base", "HEAD~1")
        self.assertEqual(done.returncode, 0, done.stdout + done.stderr)
        downstream = receipt["downstream_compile"]
        self.assertEqual(downstream["changed_libraries"], [{"package": "rsi-common", "paths": [
            "crates/rsi-common/src/health_anomaly.rs"]}])
        self.assertEqual([p["package"] for p in downstream["packages"]], ["rsi", "rsid"])
        for entry in downstream["packages"]:
            self.assertEqual((entry["status"], entry["execution"]), ("planned", "compile_only"))
            self.assertIn("--tests", entry["command"])
            self.assertIn("--no-run", entry["command"])
        self.assertFalse(self.log.exists())

    def test_public_item_consumers_become_runtime_selections_in_shared_dry_run(self):
        self.downstream_fixture()
        self.write("crates/rsid/src/lib.rs", "mod consumer;\n")
        self.write("crates/rsid/src/consumer.rs", "use common::health_anomaly::ALL_KINDS;\n"
                   "#[cfg(test)]\nmod tests {\n"
                   '    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-02"))]\n'
                   "    #[test]\n    fn registry_shape() {\n        assert_eq!(ALL_KINDS.len(), 1);\n    }\n}\n")
        self.git("add", "crates")
        self.git("commit", "-q", "-m", "consumer fixture")
        self.downstream_change('pub const ALL_KINDS: &[&str] = &["one"];\n')
        done, receipt = self.scoped("--dry-run", "--base", "HEAD~1")
        self.assertEqual(done.returncode, 0, done.stdout + done.stderr)
        selected = "rsid=shard:store-02:test(=consumer::tests::registry_shape)"
        self.assertEqual(receipt["filters"], [selected])
        self.assertEqual(receipt["downstream_tests"]["filters"], [selected])
        entry = next(p for p in receipt["downstream_compile"]["packages"] if p["package"] == "rsid")
        self.assertEqual(entry["execution"], "selected_tests_also_run")
        self.assertEqual(receipt["downstream_tests"]["selections"][0]["module"], "consumer")
        self.assertFalse(self.log.exists())

    def test_downstream_compile_unions_fixture_features_and_preserves_selected_filters(self):
        self.downstream_fixture()
        self.feature_fixture()
        self.downstream_change('pub const ALL_KINDS: &[&str] = &["one"];\n')
        selected = "rsid=shard:store-02:test(recursive_dag::smoke_fixture)"
        done, receipt = self.scoped("--dry-run", "--base", "HEAD~1", "--filter", selected)
        self.assertEqual(done.returncode, 0, done.stdout + done.stderr)
        entry = next(p for p in receipt["downstream_compile"]["packages"] if p["package"] == "rsid")
        self.assertEqual((entry["execution"], entry["features"]), ("selected_tests_also_run", ["dev-fixtures"]))
        self.assertEqual(receipt["filters"], [selected])
        self.assertEqual(receipt["features"]["rsid"], ["dev-fixtures"])
        self.assertEqual(entry["command"].count("--no-run"), 1)
        self.assertFalse(self.log.exists())

    def test_private_function_and_test_only_edits_keep_downstream_compile_empty(self):
        self.downstream_fixture()
        path = "crates/rsi-common/src/health_anomaly.rs"
        source = ('pub const ALL_KINDS: &[&str] = &["one"];\n'
                  'fn helper() -> u8 {\n    1\n}\n'
                  '#[cfg(test)]\nmod tests {\n    #[test]\n    fn works() { assert_eq!(1, 1); }\n}\n')
        self.downstream_change(source)
        checker = runpy.run_path(str(SCRIPT.with_name("check-touched-shards")))
        for before, after in ((source, source.replace("    1\n", "    2\n")),
                              (source, source.replace("assert_eq!(1, 1)", "assert_eq!(2, 2)")),
                              (source, source + "// documentation only\n")):
            with self.subTest(after=after):
                self.downstream_change(before + "\n")
                self.downstream_change(after)
                plan = RUNNER["downstream_test_plan"](checker, self.repo, "HEAD~1", "HEAD")
                self.assertEqual(plan["changed_libraries"], [])
                self.assertEqual(plan["packages"], [])

    def test_removed_public_item_and_multiline_type_member_select_downstream(self):
        self.downstream_fixture()
        checker = runpy.run_path(str(SCRIPT.with_name("check-touched-shards")))
        for source in ('pub struct Record {\n    pub value: u8,\n}\n',
                       'pub struct Record {\n    pub value: u16,\n}\n', ""):
            self.downstream_change(source)
            plan = RUNNER["downstream_test_plan"](checker, self.repo, "HEAD~1", "HEAD")
            self.assertEqual([p["package"] for p in plan["packages"]], ["rsi", "rsid"])

    def test_binary_only_and_unmounted_sources_do_not_trigger_library_compilation(self):
        self.downstream_fixture()
        checker = runpy.run_path(str(SCRIPT.with_name("check-touched-shards")))
        for path in ("crates/rsi-common/src/bin/helper.rs", "crates/rsi-common/src/unmounted.rs"):
            self.downstream_change("pub fn helper() {}\n", path)
            plan = RUNNER["downstream_test_plan"](checker, self.repo, "HEAD~1", "HEAD")
            self.assertEqual(plan["packages"], [])

    def feature_fixture(self):
        self.write("crates/rsid/src/lib.rs", "pub mod recursive_dag;\n")
        self.write("crates/rsid/src/recursive_dag.rs",
                   '#[cfg(feature = "dev-fixtures")]\npub mod smoke_fixture;\n')
        path = "crates/rsid/src/recursive_dag/smoke_fixture.rs"
        source = '''pub fn fixture_value() -> u8 {
    1
}
#[cfg(test)]
mod tests {
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-02"))]
    #[test]
    fn fixture_readback() {
        assert_eq!(super::fixture_value(), 1);
    }
}
'''
        self.write(path, source)
        self.git("add", "crates")
        self.git("commit", "-q", "-m", "fixture base")
        self.write(path, source.replace("    1\n", "    2\n"))
        self.git("add", path)
        self.git("commit", "-q", "-m", "change fixture implementation")

    def test_feature_gated_module_diff_enables_its_feature_without_cargo(self):
        self.feature_fixture()
        done, receipt = self.scoped("--dry-run", "--base", "HEAD~1")
        self.assertEqual(done.returncode, 0, done.stdout + done.stderr)
        self.assertTrue(any("recursive_dag::smoke_fixture" in f for f in receipt["filters"]), receipt)
        self.assertEqual(receipt["features"], {"rsid": ["dev-fixtures"]})
        self.assertIn("--lib --features dev-fixtures --no-run", done.stdout)
        self.assertFalse(self.log.exists())

    def test_feature_inventory_matches_exact_regex_and_ancestor_filters(self):
        self.feature_fixture()
        checker = runpy.run_path(str(SCRIPT.with_name("check-touched-shards")))
        name = "recursive_dag::smoke_fixture::tests::fixture_readback"
        for atom in ("=" + name, r"/^recursive_dag::(?:\w+::)*fixture_readback$/", "recursive_dag"):
            with self.subTest(atom=atom):
                selections = RUNNER["selection_plan"]([f"rsid=shard:store-02:test({atom})"],
                                                      {"store-02": ["rsid"]})["rsid"]
                self.assertEqual(RUNNER["required_test_features"](checker, self.repo, "HEAD", "rsid", selections),
                                 ["dev-fixtures"])
        selections = RUNNER["selection_plan"](["rsid=shard:store-02:test(unrelated)"],
                                              {"store-02": ["rsid"]})["rsid"]
        self.assertEqual(RUNNER["required_test_features"](checker, self.repo, "HEAD", "rsid", selections), [])

    def test_module_without_inline_tests_selects_sibling_rpc_behaviour_tests(self):
        self.write("crates/rsid/src/lib.rs", "pub mod operator_config;\npub mod unrelated;\n")
        self.write("crates/rsid/src/operator_config/mod.rs",
                   "mod custom_providers;\n#[cfg(test)]\nmod tests;\n#[cfg(test)]\nmod legacy_import_tests;\n")
        path = "crates/rsid/src/operator_config/custom_providers.rs"
        source = "pub fn saved_provider() -> u8 {\n    1\n}\n"
        self.write(path, source)
        gate = '#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-02"))]\n#[test]\n'
        for module, test in (("operator_config/tests", "provider_rpc_roundtrip"),
                             ("operator_config/legacy_import_tests", "legacy_provider_roundtrip"),
                             ("unrelated", "unrelated_behaviour")):
            # RPC behaviour does not name saved_provider directly.
            self.write(f"crates/rsid/src/{module}.rs", gate + f"fn {test}() {{}}\n")
        self.git("add", "crates")
        self.git("commit", "-q", "-m", "RPC fixture base")
        self.write(path, source.replace("    1\n", "    2\n"))
        self.git("add", path)
        self.git("commit", "-q", "-m", "change provider persistence")
        done, receipt = self.scoped("--dry-run", "--base", "HEAD~1")
        self.assertEqual(done.returncode, 0, done.stdout + done.stderr)
        self.assertEqual(set(receipt["filters"]), {
            "rsid=shard:store-02:test(/^operator_config::tests::/)",
            "rsid=shard:store-02:test(/^operator_config::legacy_import_tests::/)",
        })
        self.assertFalse(self.log.exists())


def cargo_diagnostic(file, line, text, code="dead_code", package="rsid-store", level="warning", column=5):
    """One `compiler-message` line as Cargo prints it with --message-format=json."""
    return {"reason": "compiler-message", "package_id": f"path+file:///w/crates/{package}#0.1.0",
            "target": {"kind": ["lib"], "name": package.replace("-", "_")},
            "message": {"message": text, "code": {"code": code} if code else None, "level": level,
                        "spans": [{"file_name": file, "line_start": line, "column_start": column,
                                   "is_primary": True}],
                        "rendered": f"{level}: {text}\n"}}


def cargo_artifact(package="rsid-store", test=False, executable=None):
    return {"reason": "compiler-artifact", "package_id": f"path+file:///w/crates/{package}#0.1.0",
            "target": {"kind": ["lib"], "name": package.replace("-", "_")}, "profile": {"test": test},
            "executable": executable}


STORE = "crates/rsid-store/src/store"
KNOWN = {f"{STORE}/agent_deploys_operator.rs|dead_code|constant `COPY_COLUMNS` is never used": 1,
         f"{STORE}/migrations/support.rs|dead_code|constant `V113_FINGERPRINT` is never used": 1}


def known_warnings(package="rsid-store"):
    return [cargo_diagnostic(f"{STORE}/agent_deploys_operator.rs", 21,
                             "constant `COPY_COLUMNS` is never used", package=package),
            cargo_diagnostic(f"{STORE}/migrations/support.rs", 172,
                             "constant `V113_FINGERPRINT` is never used", package=package)]


def baseline_document(warnings=None, package="rsid-store", configs=("lib", "lib test")):
    return {"schema": 1, "packages": {package: {"configs": list(configs),
                                                "warnings": dict(KNOWN if warnings is None else warnings)}}}


class WarningGateTest(unittest.TestCase):
    """#1897: a crate with a checked-in baseline may not gain compiler warnings, judged from the
    diagnostics of the build scoped-test already supervises. No test compiles anything."""

    def collect(self, *messages, env=None, text=()):
        collector = RUNNER["WarningCollector"]("/w", env={} if env is None else env)
        for message in messages:
            (collector.artifact if message["reason"] == "compiler-artifact" else collector.diagnostic)(message)
        for line in text:
            collector.text(line)
        return collector.payload()

    def known(self, package="rsid-store"):
        return known_warnings(package)

    def verdict(self, payload, baseline=None, base=None, state="present"):
        return RUNNER["evaluate_warning_gate"](
            RUNNER["merge_warning_payloads"]([payload]), baseline or baseline_document(), base, state)

    def built(self, *warnings, package="rsid-store"):
        return [cargo_artifact(package), cargo_artifact(package, test=True), *warnings]

    def test_package_names_come_from_both_cargo_id_forms(self):
        name = RUNNER["warning_package_name"]
        self.assertEqual(name("path+file:///w/crates/rsid-store#0.1.0"), "rsid-store")
        self.assertEqual(name("path+file:///w/crates/store-dir#rsid-store@0.1.0"), "rsid-store")
        self.assertEqual(name("rsid-store 0.1.0 (path+file:///w/crates/rsid-store)"), "rsid-store")
        self.assertIsNone(name(None))
        self.assertIsNone(name(""))

    def test_known_warnings_are_accepted_at_the_declared_baseline(self):
        verdict = self.verdict(self.collect(*self.built(*self.known())))
        self.assertEqual(verdict["status"], "ok", verdict)
        store = verdict["packages"]["rsid-store"]
        self.assertEqual((store["observed"], store["baseline"], store["new_total"]), (2, 2, 0))
        self.assertEqual(store["configs"], ["lib", "lib test"])

    def test_a_new_warning_fails_the_gate_and_names_the_key(self):
        extra = cargo_diagnostic(f"{STORE}/mod.rs", 9, "unused variable: `stale`", code="unused_variables")
        verdict = self.verdict(self.collect(*self.built(*self.known(), extra)))
        self.assertEqual(verdict["status"], "new_warnings")
        self.assertEqual(verdict["packages"]["rsid-store"]["new"],
                         [{"key": f"{STORE}/mod.rs|unused_variables|unused variable: `stale`", "count": 1}])

    def test_a_second_instance_of_a_known_warning_is_new(self):
        again = cargo_diagnostic(f"{STORE}/migrations/support.rs", 400, "constant `V113_FINGERPRINT` is never used")
        verdict = self.verdict(self.collect(*self.built(*self.known(), again)))
        self.assertEqual(verdict["status"], "new_warnings")
        self.assertEqual(verdict["packages"]["rsid-store"]["new_total"], 1)

    def test_the_same_warning_reported_by_the_lib_and_lib_test_units_counts_once(self):
        # Both units report the same position; line numbers only tell two reports apart.
        verdict = self.verdict(self.collect(*self.built(*self.known(), *self.known())))
        self.assertEqual(verdict["status"], "ok", verdict)
        self.assertEqual(verdict["packages"]["rsid-store"]["observed"], 2)

    def test_a_line_shift_is_not_a_new_warning(self):
        moved = [cargo_diagnostic(f"{STORE}/agent_deploys_operator.rs", 99,
                                  "constant `COPY_COLUMNS` is never used"), self.known()[1]]
        self.assertEqual(self.verdict(self.collect(*self.built(*moved)))["status"], "ok")

    def test_fixed_warnings_are_reported_for_a_complete_measurement_only(self):
        gone = self.collect(*self.built(self.known()[0]))
        verdict = self.verdict(gone)
        self.assertEqual(verdict["status"], "ok")
        self.assertEqual(verdict["packages"]["rsid-store"]["fixed_total"], 1)
        # Only the lib unit was built (as another crate's dependency): absence proves nothing.
        partial = self.collect(cargo_artifact(), self.known()[0])
        verdict = self.verdict(partial)
        self.assertEqual(verdict["status"], "ok")
        self.assertFalse(verdict["packages"]["rsid-store"]["complete"])
        self.assertEqual(verdict["packages"]["rsid-store"]["fixed"], [])

    def test_another_crates_warning_is_not_attributed_to_the_store(self):
        elsewhere = cargo_diagnostic("crates/rsid/src/rpc.rs", 3, "unused import: `Foo`", code="unused_imports",
                                     package="rsid")
        payload = self.collect(*self.built(*self.known(), cargo_artifact("rsid"), elsewhere))
        self.assertEqual(self.verdict(payload)["status"], "ok")
        both = baseline_document()
        both["packages"]["rsid"] = {"configs": ["lib"], "warnings": {}}
        verdict = self.verdict(payload, both)
        self.assertEqual(verdict["status"], "new_warnings")
        self.assertEqual(verdict["packages"]["rsid-store"]["status"], "ok")
        self.assertEqual(verdict["packages"]["rsid"]["new_total"], 1)

    def test_a_package_the_run_did_not_build_is_never_read_as_clean(self):
        verdict = self.verdict(self.collect(*self.built(package="rsi-common")))
        self.assertEqual(verdict["status"], "not_measured")
        self.assertEqual(verdict["packages"]["rsid-store"]["status"], "not_measured")
        self.assertNotIn("observed", verdict["packages"]["rsid-store"])

    def test_non_warning_diagnostics_and_rustc_summaries_are_not_warnings(self):
        payload = self.collect(*self.built(
            *self.known(),
            cargo_diagnostic(f"{STORE}/mod.rs", 1, "mismatched types", code="E0308", level="error"),
            cargo_diagnostic(f"{STORE}/mod.rs", 1, "2 warnings emitted", code=None),
            cargo_diagnostic(f"{STORE}/mod.rs", 1, "aborting due to 1 previous error", code=None, level="error")))
        self.assertEqual(self.verdict(payload)["status"], "ok")

    def test_evidence_that_cannot_be_trusted_fails_closed_instead_of_counting_clean(self):
        built = self.built()
        for label, payload in {
            "flags": self.collect(*built, env={"RUSTFLAGS": "-C opt-level=1 -A warnings"}),
            "encoded flags": self.collect(*built, env={"CARGO_ENCODED_RUSTFLAGS": "-Adead_code\x1f-Cdebuginfo=0"}),
            "cap lints": self.collect(*built, env={"CARGO_BUILD_RUSTFLAGS": "--cap-lints=allow"}),
            "prose": self.collect(*built, text=["   Compiling rsid-store v0.1.0", "  --> crates/x.rs:3:4"]),
            "no package": self.collect(*built, {"reason": "compiler-message", "message": {
                "message": "unused", "code": {"code": "unused"}, "level": "warning", "spans": []}}),
        }.items():
            verdict = self.verdict(payload)
            self.assertEqual(verdict["status"], "unavailable", (label, verdict))
            self.assertTrue(verdict["unavailable"], label)
        # Cargo's own prose notes are not diagnostics.
        notes = self.collect(*self.built(*self.known()), text=["warning: unused manifest key: x\n", "   Compiling a\n"])
        self.assertEqual(self.verdict(notes)["status"], "ok")
        # Allowing a different lint (not a warning) does not hide anything.
        other = self.collect(*self.built(*self.known()), env={"RUSTFLAGS": "-A clippy::pedantic -D warnings"})
        self.assertEqual(self.verdict(other)["status"], "ok")

    def test_unavailable_evidence_does_not_fail_a_run_that_built_no_baselined_package(self):
        payload = self.collect(*self.built(package="rsi-common"), env={"RUSTFLAGS": "-A warnings"})
        self.assertEqual(self.verdict(payload)["status"], "not_measured")

    def test_a_baseline_above_the_base_or_missing_a_package_is_refused(self):
        payload = self.collect(*self.built(*self.known()))
        base = baseline_document()
        raised = baseline_document({**KNOWN, f"{STORE}/mod.rs|dead_code|function `x` is never used": 1})
        verdict = self.verdict(payload, raised, base)
        self.assertEqual(verdict["status"], "baseline_raised")
        self.assertIn("function `x` is never used", verdict["refusals"][0])
        counted_up = baseline_document({key: 2 for key in KNOWN})
        self.assertEqual(self.verdict(payload, counted_up, base)["status"], "baseline_raised")
        dropped = {"schema": 1, "packages": {"rsid": {"configs": ["lib"], "warnings": {}}}}
        verdict = self.verdict(payload, dropped, base)
        self.assertEqual(verdict["status"], "baseline_raised")
        self.assertIn("rsid-store: its baseline was removed", verdict["refusals"][0])
        # Lowering a count, or adding a package the base does not have, is fine.
        lowered = baseline_document({next(iter(KNOWN)): 1})
        self.assertEqual(self.verdict(self.collect(*self.built(self.known()[0])), lowered, base)["status"], "ok")
        extra = baseline_document()
        extra["packages"]["rsid"] = {"configs": ["lib"], "warnings": {"a|b|c": 4}}
        self.assertEqual(self.verdict(payload, extra, base)["status"], "ok")

    def test_raising_the_baseline_does_not_hide_a_new_warning(self):
        extra = cargo_diagnostic(f"{STORE}/mod.rs", 9, "unused variable: `stale`", code="unused_variables")
        key = f"{STORE}/mod.rs|unused_variables|unused variable: `stale`"
        verdict = self.verdict(self.collect(*self.built(*self.known(), extra)),
                               baseline_document({**KNOWN, key: 1}), baseline_document())
        self.assertEqual(verdict["status"], "baseline_raised")

    def test_baseline_documents_are_validated(self):
        read = RUNNER["read_warning_baseline"]
        self.assertEqual(read(json.dumps(baseline_document()))["schema"], 1)
        for broken in ("[]", "{}", '{"schema": 2, "packages": {}}',
                       json.dumps({"schema": 1, "packages": {"p": {"configs": [], "warnings": {}}}}),
                       json.dumps({"schema": 1, "packages": {"p": {"configs": ["lib"], "warnings": {"k": 0}}}}),
                       json.dumps({"schema": 1, "packages": {"p": {"configs": ["lib"], "warnings": {"k": True}}}})):
            with self.assertRaises(ValueError):
                read(broken)

    def test_a_missing_baseline_disables_nothing_and_a_broken_one_fails_closed(self):
        with scratch_tempdir(prefix="warning-gate-") as tmp:
            self.assertIsNone(RUNNER["warning_gate"](tmp, "base", []))
            (Path(tmp) / "tools").mkdir()
            (Path(tmp) / RUNNER["WARNING_BASELINE"]).write_text("{not json")
            verdict = RUNNER["warning_gate"](tmp, "base", [])
            self.assertEqual(verdict["status"], "unavailable")
            self.assertTrue(RUNNER["report_warning_gate"](verdict))

    def test_base_baseline_states_come_from_git(self):
        with scratch_tempdir(prefix="warning-gate-") as tmp:
            def git(*args):
                return scratch_git(tmp, "-c", "user.email=a@b", "-c", "user.name=a", *args).stdout.strip()
            git("init", "-q", "-b", "rolling")
            Path(tmp, "x").write_text("x")
            git("add", "x")
            git("commit", "-qm", "root")
            (Path(tmp) / "tools").mkdir()
            Path(tmp, RUNNER["WARNING_BASELINE"]).write_text(json.dumps(baseline_document()))
            git("add", RUNNER["WARNING_BASELINE"])
            git("commit", "-qm", "baseline")
            base = RUNNER["base_warning_baseline"]
            document, state = base(tmp, "HEAD")
            self.assertEqual((state, document["packages"]["rsid-store"]["warnings"]), ("present", KNOWN))
            self.assertEqual(base(tmp, "HEAD~1"), (None, "absent"))
            self.assertEqual(base(tmp, "no-such-ref"), (None, "unresolved"))
            Path(tmp, RUNNER["WARNING_BASELINE"]).write_text("{not json")
            git("commit", "-qam", "broken")
            self.assertEqual(base(tmp, "HEAD"), (None, "unreadable"))

    def test_the_checked_in_baseline_is_valid_and_names_real_files(self):
        root = SCRIPT.parents[1]
        document = RUNNER["read_warning_baseline"]((root / RUNNER["WARNING_BASELINE"]).read_text())
        self.assertIn("rsid-store", document["packages"])
        for package, entry in document["packages"].items():
            for key in entry["warnings"]:
                path, code, message = key.split("|", 2)
                self.assertTrue((root / path).is_file(), f"{package}: {path} does not exist")
                self.assertTrue(code and message, key)

    def test_a_job_log_regenerates_the_baseline_packages(self):
        payload = self.collect(*self.built(*self.known(), self.known()[0]))
        log = ["noise\n", "SCOPED_WARNINGS " + json.dumps(payload) + "\n", "SCOPED_WARNINGS " + json.dumps(payload) + "\n"]
        packages = RUNNER["baseline_from_log"](log)["packages"]
        self.assertEqual(packages["rsid-store"], {"configs": ["lib", "lib test"], "warnings": KNOWN})
        self.assertEqual(RUNNER["read_warning_baseline"](json.dumps(RUNNER["baseline_from_log"](log)))["schema"], 1)


class IntegratedBaseTest(unittest.TestCase):
    def test_launch_pin_diff_excludes_merged_rolling_work(self):
        with scratch_tempdir(prefix="integrated-base-") as tmp:
            def git(*args):
                return scratch_git(tmp, "-c", "user.email=a@b", "-c", "user.name=a", *args).stdout.strip()
            def commit(name):
                Path(tmp, name).write_text(name)
                git("add", name)
                git("commit", "-qm", name)
            git("init", "-q", "-b", "rolling")
            commit("root")
            git("checkout", "-q", "-b", "pin")
            commit("pin")
            pin = git("rev-parse", "HEAD")
            git("checkout", "-q", "rolling")
            commit("rolled")
            git("update-ref", "refs/remotes/origin/rolling", "rolling")
            git("checkout", "-q", "pin")
            git("merge", "-q", "--no-edit", "rolling")
            commit("worker")
            base = RUNNER["integrated_base"](tmp, pin, "HEAD")
            self.assertNotEqual(base, pin)
            self.assertEqual(git("diff", "--name-only", base, "HEAD").split(), ["worker"])
            self.assertEqual(RUNNER["integrated_base"](tmp, "origin/rolling", "HEAD"), "origin/rolling")


class MergeOntoRollingTest(unittest.TestCase):
    def setup_repo(self, tmp, worker_file, worker_text):
        def git(*args):
            return scratch_git(tmp, "-c", "user.email=a@b", "-c", "user.name=a", *args).stdout.strip()
        def commit(name, text):
            Path(tmp, name).write_text(text)
            git("add", name)
            git("commit", "-qm", name)
        git("init", "-q", "-b", "rolling")
        commit("shared.txt", "base\n")
        git("checkout", "-q", "-b", "worker")
        commit(worker_file, worker_text)
        git("checkout", "-q", "rolling")
        commit("shared.txt", "rolled\n")
        git("update-ref", "refs/remotes/origin/rolling", "rolling")
        git("checkout", "-q", "worker")
        return git

    def test_clean_merge_records_true_without_paths(self):
        with scratch_tempdir(prefix="merge-clean-") as tmp:
            self.setup_repo(tmp, "other.txt", "x\n")
            self.assertEqual(RUNNER["merge_onto_rolling"](tmp, "HEAD"),
                             {"merges_cleanly_onto_rolling": True, "conflict_paths": []})

    def test_conflicting_merge_records_false_and_paths(self):
        with scratch_tempdir(prefix="merge-conflict-") as tmp:
            self.setup_repo(tmp, "shared.txt", "worker\n")
            result = RUNNER["merge_onto_rolling"](tmp, "HEAD")
            self.assertIs(result["merges_cleanly_onto_rolling"], False)
            self.assertEqual(result["conflict_paths"], ["shared.txt"])

    def test_conflict_paths_are_bounded(self):
        with scratch_tempdir(prefix="merge-bound-") as tmp:
            def git(*args):
                return scratch_git(tmp, "-c", "user.email=a@b", "-c", "user.name=a", *args).stdout.strip()
            def commit_all(text):
                for index in range(40):
                    Path(tmp, f"f{index:02}.txt").write_text(text)
                git("add", ".")
                git("commit", "-qm", text)
            git("init", "-q", "-b", "rolling")
            commit_all("base")
            git("checkout", "-q", "-b", "worker")
            commit_all("worker")
            git("checkout", "-q", "rolling")
            commit_all("rolled")
            git("update-ref", "refs/remotes/origin/rolling", "rolling")
            git("checkout", "-q", "worker")
            result = RUNNER["merge_onto_rolling"](tmp, "HEAD")
            self.assertEqual(len(result["conflict_paths"]), 32)
            self.assertEqual(result["conflict_paths_total"], 40)

    def test_unresolvable_rolling_records_null_with_reason(self):
        with scratch_tempdir(prefix="merge-null-") as tmp:
            git = self.setup_repo(tmp, "other.txt", "x\n")
            git("update-ref", "-d", "refs/remotes/origin/rolling")
            result = RUNNER["merge_onto_rolling"](tmp, "HEAD")
            self.assertIsNone(result["merges_cleanly_onto_rolling"])
            self.assertIn("origin/rolling", result["merge_check_reason"])

    def test_record_leaves_exit_code_and_dry_run_alone(self):
        with scratch_tempdir(prefix="merge-record-") as tmp:
            self.setup_repo(tmp, "shared.txt", "worker\n")
            receipt = RUNNER["RECEIPT"]
            saved = dict(receipt)
            try:
                receipt.update(head="HEAD", candidate_repo=tmp, dry_run=False, emit=True)
                RUNNER["record_merge_onto_rolling"]()
                self.assertIs(receipt["merges_cleanly_onto_rolling"], False)
                self.assertNotIn("candidate_repo", receipt)
            finally:
                receipt.clear()
                receipt.update(saved)


class StopProcessGroupTest(unittest.TestCase):
    """A timed-out bounded run must not make the next package's run fail with EPERM (#100).

    macOS killpg(2) returns EPERM for a group whose only members are zombies; that
    PermissionError used to replace the TimeoutExpired and surface as
    "unable to start bounded run: [Errno 1] Operation not permitted".
    """

    @staticmethod
    def zombie_leader():
        process = subprocess.Popen(["sh", "-c", "exit 0"], start_new_session=True)
        if hasattr(os, "waitid") and hasattr(os, "WNOWAIT"):
            os.waitid(os.P_PID, process.pid, os.WEXITED | os.WNOWAIT)  # exited, not reaped
        else:
            time.sleep(0.5)
        return process

    def test_stop_process_tolerates_an_all_zombie_group(self):
        process = self.zombie_leader()
        if sys.platform == "darwin":  # the host behaviour this regression is about
            with self.assertRaises(PermissionError):
                os.killpg(process.pid, signal.SIGTERM)
        RUNNER["stop_process"](process)
        self.assertEqual(process.returncode, 0)

    def test_signal_group_names_step_and_errno_when_a_live_leader_refuses(self):
        process = subprocess.Popen(["sleep", "30"], start_new_session=True)
        try:
            refused = PermissionError(1, "Operation not permitted")
            with patch.object(os, "killpg", side_effect=refused), \
                    patch.object(process, "send_signal", side_effect=refused):
                with self.assertRaises(PermissionError) as raised:
                    RUNNER["signal_group"](process, signal.SIGTERM)
            self.assertEqual(raised.exception.errno, 1)
            self.assertIn("process group", str(raised.exception))
            self.assertIn("SIGTERM", str(raised.exception))
        finally:
            os.killpg(process.pid, signal.SIGKILL)
            process.wait()

    def test_timed_out_run_then_a_fresh_run_starts_independently(self):
        stream, Budget = RUNNER["stream"], RUNNER["Budget"]
        seen = []
        real_killpg = os.killpg

        def refuse_like_macos(pgid, sig):
            raise PermissionError(1, "Operation not permitted")

        # First package: runs out of budget while its worker is still going.
        with patch.object(os, "killpg", side_effect=refuse_like_macos):
            with self.assertRaises(subprocess.TimeoutExpired):
                stream(["sleep", "30"], seen.append, budget=Budget(0.3))
        # Next package: a fresh bounded run starts and completes normally.
        self.assertTrue(real_killpg is os.killpg)
        out = []
        code = stream([sys.executable, "-c", "print('rsid ran')"], out.append, budget=Budget(30))
        self.assertEqual(code, 0)
        self.assertEqual(out, ["rsid ran\n"])


class RenumberDeclaredCandidateTest(unittest.TestCase):
    """#3339: a declared provisional migration whose number rolling took is tested renumbered."""

    @classmethod
    def setUpClass(cls):
        import importlib.util
        path = Path(__file__).with_name("test_rolling_migration_renumber.py")
        spec = importlib.util.spec_from_file_location("renumber_fixture_for_scoped_test", path)
        cls.fixtures = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(cls.fixtures)
        cls.tools = Path(__file__).resolve().parents[2] / "tools"

    def setUp(self):
        self.fixture = self.fixtures.ProvisionalMigrationTest()
        self.fixture.setUp()
        self.addCleanup(self.fixture.doCleanups)
        self.repo = self.fixture.repo
        self.git = self.fixture.git
        self.renumber = RUNNER["renumber_declared_candidate"]

    def landed_alpha(self):
        """Rolling after another source took v130."""
        _, tip = self.fixture.candidate(self.fixture.source("alpha"), self.fixture.base)
        return tip

    def test_a_taken_number_yields_the_landers_renumbered_candidate_to_test(self):
        tip = self.landed_alpha()
        beta = self.fixture.source("beta")
        plain = RUNNER["merge_onto_rolling"](self.repo, beta, rolling=tip)
        self.assertFalse(plain["merges_cleanly_onto_rolling"], "the premise: v130 collides")
        result = self.renumber(self.repo, beta, tip, tool_dir=self.tools)
        self.assertEqual(result["status"], "renumbered", result)
        self.assertEqual((result["old_version"], result["new_version"]), (130, 131))
        candidate = result["candidate"]
        self.assertEqual(self.git(self.repo, "show", "-s", "--format=%P", candidate), f"{tip} {beta}")
        manifest = self.fixtures.RENUMBER.revision_inventory(self.repo, candidate)
        self.assertEqual(manifest["latest_schema_version"], 131)
        self.assertEqual(sorted(manifest["blocks"])[-2:], ["130", "131"])
        trial = RUNNER["merge_onto_rolling"](self.repo, candidate, rolling=tip)
        self.assertTrue(trial["merges_cleanly_onto_rolling"])
        self.assertTrue(self.fixtures.RENUMBER.is_ancestor(self.repo, beta, candidate))

    def test_a_declared_source_at_rolling_head_plus_one_needs_no_renumber(self):
        beta = self.fixture.source("beta")
        self.assertIsNone(self.renumber(self.repo, beta, self.fixture.base, tool_dir=self.tools))

    def test_a_free_version_with_an_undeclared_nonmigration_literal_is_refused(self):
        self.fixture.source("beta")
        hidden = self.fixture.commit_extra(
            "beta", "crates/rsi-common/src/satellite_intake.rs",
            'const FIXTURE: &str = "migrations/v130.rs";\n')
        # The source contains the target, but must still be validated (#3397).
        result = self.renumber(self.repo, hidden, self.fixture.base, tool_dir=self.tools)
        self.assertEqual(result["status"], "refused", result)
        self.assertIn("undeclared version-bearing file or site: crates/rsi-common/src/satellite_intake.rs:35",
                      result["reason"])

    def test_a_free_version_with_an_undeclared_site_in_a_declared_file_is_refused(self):
        hidden = self.fixture.source("beta", "const HIDDEN_V130: i32 = 130;\n")
        result = self.renumber(self.repo, hidden, self.fixture.base, tool_dir=self.tools)
        self.assertEqual(result["status"], "refused", result)
        self.assertIn("undeclared version-bearing site: crates/rsid-store/src/store/mod.rs:",
                      result["reason"])

    def test_a_provisional_refusal_stops_the_run_before_derivation_and_builds(self):
        self.fixture.source("beta")
        hidden = self.fixture.commit_extra("beta", "extra.rs", "const SCHEMA: i32 = 130;\n")
        receipt = RUNNER["RECEIPT"]
        saved = dict(receipt)
        try:
            run = RUNNER["run"]
            with patch.object(sys, "argv", [str(SCRIPT), "--repo", str(self.repo),
                                           "--base", self.fixture.base, "--head", hidden, "--dry-run"]), \
                    patch.dict(run.__globals__, integrated_base=lambda *_: self.fail("derived refused source")), \
                    contextlib.redirect_stdout(io.StringIO()):
                code = run(None)
            self.assertEqual(code, 2)
            self.assertEqual(receipt["head"], hidden)
            self.assertEqual(receipt["renumber"]["status"], "refused")
            self.assertIn("extra.rs", receipt["renumber"]["reason"])
        finally:
            RUNNER["cleanup_renumbered"]()
            receipt.clear()
            receipt.update(saved)

    def test_a_source_without_a_declaration_needs_no_renumber(self):
        tip = self.landed_alpha()
        worktree = Path(self.fixture.temp.name) / "plain"
        self.git(self.repo, "worktree", "add", "-q", "--detach", str(worktree), self.fixture.base)
        (worktree / "note.txt").write_text("plain\n")
        self.git(worktree, "add", "note.txt")
        self.git(worktree, "-c", "user.name=a", "-c", "user.email=a@b", "commit", "-qm", "plain")
        head = self.git(worktree, "rev-parse", "HEAD")
        self.assertIsNone(self.renumber(self.repo, head, tip, tool_dir=self.tools))

    def test_a_source_that_already_contains_rolling_needs_no_renumber(self):
        tip = self.landed_alpha()
        self.assertIsNone(self.renumber(self.repo, tip, tip, tool_dir=self.tools))

    def test_a_refused_renumber_reports_why_and_changes_nothing(self):
        tip = self.landed_alpha()
        hidden = self.fixture.source("beta", "const HIDDEN_V130: i32 = 130;\n")
        result = self.renumber(self.repo, hidden, tip, tool_dir=self.tools)
        self.assertEqual(result["status"], "refused", result)
        self.assertIn("undeclared version-bearing site", result["reason"])
        self.assertNotIn("candidate", result)

    def test_the_renumbered_candidate_checks_out_and_is_removed(self):
        tip = self.landed_alpha()
        result = self.renumber(self.repo, self.fixture.source("beta"), tip, tool_dir=self.tools)
        tree = RUNNER["checkout_renumbered"](self.repo, result["candidate"])
        try:
            head = subprocess.run(["git", "-C", str(tree), "rev-parse", "HEAD"], capture_output=True,
                                  text=True, check=True).stdout.strip()
            self.assertEqual(head, result["candidate"])
            self.assertIn("131", (tree / self.fixtures.RENUMBER.STORE).read_text())
        finally:
            RUNNER["cleanup_renumbered"]()
        self.assertFalse(tree.exists())


    def test_the_receipt_keeps_the_committed_head_and_merges_the_renumbered_candidate(self):
        tip = self.landed_alpha()
        beta = self.fixture.source("beta")
        result = self.renumber(self.repo, beta, tip, tool_dir=self.tools)
        self.git(self.repo, "update-ref", "refs/remotes/origin/rolling", tip)
        receipt = RUNNER["RECEIPT"]
        saved = dict(receipt)
        try:
            receipt.update(head=beta, candidate_repo=str(self.repo), dry_run=False, emit=True,
                           renumber={"status": "renumbered", "candidate": result["candidate"]})
            RUNNER["record_merge_onto_rolling"]()
            self.assertEqual(receipt["head"], beta)
            self.assertIs(receipt["merges_cleanly_onto_rolling"], True)
            self.assertEqual(receipt["conflict_paths"], [])
        finally:
            receipt.clear()
            receipt.update(saved)


if __name__ == "__main__":
    unittest.main()
