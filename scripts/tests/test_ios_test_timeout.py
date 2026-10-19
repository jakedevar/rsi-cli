"""scripts/ios/test.sh: #11 watchdog for a hung xcodebuild, mac-mini #22 per-run simulator."""
import os
import stat
import subprocess
import tempfile
import time
import unittest
from pathlib import Path

SCRIPT = Path(__file__).resolve().parents[1] / "ios" / "test.sh"

OK_XCODEBUILD = """#!/bin/bash
echo "Test Suite 'All tests' passed"
echo "	 Executed 12 tests, with 0 failures (0 unexpected) in 0.5 (0.6) seconds"
"""

# Prints the summary, then hangs like the observed simulator stall. The marker
# file lets the test confirm the process (and its sleep child) are gone.
HUNG_XCODEBUILD = """#!/bin/bash
echo "	 Executed 39 tests, with 0 failures (0 unexpected) in 0.5 (0.6) seconds"
echo $$ > "$STUB_PID_FILE"
sleep 1000
"""

# Stateful xcrun stub: answers the simctl queries test.sh makes, hands out a new
# udid per `create` (none when STUB_NO_CREATE is set) and logs every call.
CLONE_XCRUN = """#!/bin/bash
echo "$*" >> "$STUB_XCRUN_LOG"
[ "$1" = simctl ] || exit 0
case "$2" in
    list)
        case "$3" in
            devicetypes) echo "iPhone 17 Pro (com.apple.CoreSimulator.SimDeviceType.iPhone-17-Pro)"
                echo "iPhone 17 Pro Max (com.apple.CoreSimulator.SimDeviceType.iPhone-17-Pro-Max)" ;;
            runtimes) echo "iOS 26.5 (26.5 - 23F77) - com.apple.CoreSimulator.SimRuntime.iOS-26-5" ;;
            devices) echo "    iPhone 17 Pro (995FC5B4-40A7-4D35-BD8A-27832523127D) (Booted)" ;;
        esac ;;
    create)
        [ -z "$STUB_NO_CREATE" ] || exit 1
        n=$(cat "$STUB_COUNTER" 2>/dev/null || echo 0)
        n=$((n + 1))
        echo "$n" > "$STUB_COUNTER"
        printf 'AAAAAAAA-0000-0000-0000-%012d\\n' "$n" ;;
esac
exit 0
"""

# Logs its -destination, fails once with the preflight Busy error when asked.
RECORDING_XCODEBUILD = """#!/bin/bash
for a in "$@"; do echo "$a" >> "$STUB_DEST_LOG"; done
n=$(cat "$STUB_XB_COUNTER" 2>/dev/null || echo 0)
n=$((n + 1))
echo "$n" > "$STUB_XB_COUNTER"
if [ "$STUB_XB_MODE" = preflight_once ] && [ "$n" = 1 ]; then
    echo "Application failed preflight checks: SBMainWorkspace Busy"
    exit 65
fi
if [ "$STUB_XB_MODE" = fail ]; then
    echo "error: build broke"
    exit 65
fi
if [ "$STUB_XB_MODE" = hang ]; then
    sleep 1000
fi
echo "	 Executed 7 tests, with 0 failures (0 unexpected) in 0.5 (0.6) seconds"
"""


@unittest.skipUnless(Path("/bin/bash").exists(), "needs bash")
class IosTestTimeoutTest(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.bin = Path(self.tmp.name)
        self.pid_file = self.bin / "xcodebuild.pid"
        self.write("xcodegen", "#!/bin/bash\nexit 0\n")
        self.write("xcrun", "#!/bin/bash\nexit 0\n")

    def write(self, name, text):
        path = self.bin / name
        path.write_text(text)
        path.chmod(path.stat().st_mode | stat.S_IXUSR)

    def run_script(self, *args, env_extra=None):
        env = dict(os.environ)
        env["PATH"] = f"{self.bin}:/usr/bin:/bin"
        env["STUB_PID_FILE"] = str(self.pid_file)
        env.update(env_extra or {})
        start = time.monotonic()
        proc = subprocess.run(
            ["/bin/bash", str(SCRIPT), *args],
            capture_output=True,
            text=True,
            env=env,
            timeout=60,
        )
        return proc, time.monotonic() - start

    def test_success_prints_ok_receipt(self):
        self.write("xcodebuild", OK_XCODEBUILD)
        proc, _ = self.run_script()
        self.assertEqual(proc.returncode, 0, proc.stderr)
        self.assertIn("ios-test: ok tests=12 failures=0 ", proc.stdout)
        self.assertNotIn("reason=timeout", proc.stdout)

    def test_hung_xcodebuild_times_out_with_receipt(self):
        self.write("xcodebuild", HUNG_XCODEBUILD)
        proc, elapsed = self.run_script("--timeout", "2")
        self.assertEqual(proc.returncode, 124, proc.stderr)
        self.assertIn("ios-test: failed tests=39 failures=0 ", proc.stdout)
        self.assertTrue(proc.stdout.strip().endswith("reason=timeout"), proc.stdout)
        self.assertLess(elapsed, 20)
        pid = int(self.pid_file.read_text())
        with self.assertRaises(ProcessLookupError):
            os.kill(pid, 0)

    def test_env_timeout_applies(self):
        self.write("xcodebuild", HUNG_XCODEBUILD)
        proc, _ = self.run_script(env_extra={"RSI_IOS_TEST_TIMEOUT": "2"})
        self.assertEqual(proc.returncode, 124, proc.stderr)
        self.assertIn("reason=timeout", proc.stdout)

    def test_rejects_bad_timeout(self):
        self.write("xcodebuild", OK_XCODEBUILD)
        proc, _ = self.run_script("--timeout", "abc")
        self.assertEqual(proc.returncode, 2)


@unittest.skipUnless(Path("/bin/bash").exists(), "needs bash")
class IosTestCloneTest(unittest.TestCase):
    """mac-mini #22: every run owns a simulator clone and always deletes it."""

    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.dir = Path(self.tmp.name)
        self.bin = self.dir / "bin"
        self.bin.mkdir()
        self.xcrun_log = self.dir / "xcrun.log"
        self.dest_log = self.dir / "dest.log"
        for name, text in (
            ("xcodegen", "#!/bin/bash\nexit 0\n"),
            ("xcrun", CLONE_XCRUN),
            ("xcodebuild", RECORDING_XCODEBUILD),
        ):
            path = self.bin / name
            path.write_text(text)
            path.chmod(path.stat().st_mode | stat.S_IXUSR)

    def run_script(self, *args, mode="ok", no_create=False):
        env = dict(os.environ)
        env["PATH"] = f"{self.bin}:/usr/bin:/bin"
        env["TMPDIR"] = str(self.dir)
        env["STUB_XCRUN_LOG"] = str(self.xcrun_log)
        env["STUB_DEST_LOG"] = str(self.dest_log)
        env["STUB_COUNTER"] = str(self.dir / "create.count")
        env["STUB_XB_COUNTER"] = str(self.dir / "xb.count")
        env["STUB_XB_MODE"] = mode
        if no_create:
            env["STUB_NO_CREATE"] = "1"
        return subprocess.run(
            ["/bin/bash", str(SCRIPT), *args],
            capture_output=True,
            text=True,
            env=env,
            timeout=60,
        )

    def xcrun_calls(self, verb):
        lines = self.xcrun_log.read_text().splitlines()
        return [l.split() for l in lines if l.startswith(f"simctl {verb} ")]

    def destinations(self):
        return [l for l in self.dest_log.read_text().splitlines() if l.startswith(("id=", "platform="))]

    def assert_all_clones_deleted(self):
        created = len(self.xcrun_calls("create"))
        deleted = [c[2] for c in self.xcrun_calls("delete")]
        self.assertEqual(len(deleted), created, self.xcrun_log.read_text())
        for n in range(1, created + 1):
            self.assertIn("AAAAAAAA-0000-0000-0000-%012d" % n, deleted)
        # never the pre-existing booted simulator
        self.assertNotIn("995FC5B4-40A7-4D35-BD8A-27832523127D", deleted)

    def test_run_uses_own_clone_and_deletes_it(self):
        proc = self.run_script()
        self.assertEqual(proc.returncode, 0, proc.stderr)
        self.assertIn("ios-test: ok tests=7 failures=0 os=26.5 device=iPhone 17 Pro ", proc.stdout)
        create = self.xcrun_calls("create")[0]
        self.assertTrue(create[2].startswith("rsi-ios-test-"), create)
        self.assertEqual(create[3], "com.apple.CoreSimulator.SimDeviceType.iPhone-17-Pro")
        self.assertEqual(create[4], "com.apple.CoreSimulator.SimRuntime.iOS-26-5")
        self.assertEqual(self.destinations(), ["id=AAAAAAAA-0000-0000-0000-000000000001"])
        self.assert_all_clones_deleted()

    def test_clone_deleted_after_build_failure(self):
        proc = self.run_script(mode="fail")
        self.assertEqual(proc.returncode, 65, proc.stderr)
        self.assertIn("ios-test: failed ", proc.stdout)
        self.assertNotIn("reason=", proc.stdout)
        self.assertEqual(len(self.xcrun_calls("create")), 1)
        self.assert_all_clones_deleted()

    def test_clone_deleted_after_timeout(self):
        proc = self.run_script("--timeout", "2", mode="hang")
        self.assertEqual(proc.returncode, 124, proc.stderr)
        self.assertTrue(proc.stdout.strip().endswith("reason=timeout"), proc.stdout)
        self.assert_all_clones_deleted()

    def test_preflight_busy_retries_once_on_fresh_clone(self):
        proc = self.run_script(mode="preflight_once")
        self.assertEqual(proc.returncode, 0, proc.stderr)
        self.assertIn("ios-test: ok tests=7 ", proc.stdout)
        self.assertTrue(proc.stdout.strip().endswith("reason=sim-preflight"), proc.stdout)
        self.assertEqual(
            self.destinations(),
            ["id=AAAAAAAA-0000-0000-0000-000000000001", "id=AAAAAAAA-0000-0000-0000-000000000002"],
        )
        self.assert_all_clones_deleted()

    def test_falls_back_to_shared_simulator_when_create_fails(self):
        proc = self.run_script(no_create=True)
        self.assertEqual(proc.returncode, 0, proc.stderr)
        self.assertIn("using the shared one", proc.stderr)
        self.assertEqual(self.destinations(), ["platform=iOS Simulator,OS=26.5,name=iPhone 17 Pro"])
        self.assertEqual(self.xcrun_calls("delete"), [])


if __name__ == "__main__":
    unittest.main()
