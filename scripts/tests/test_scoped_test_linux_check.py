"""#253: the Linux compile lane of scripts/scoped-test on a non-Linux host.

A Mac `scoped_test` compiles only the macOS target, so the Linux side of `cfg(target_os = ...)` code
was never built there. These tests cover the lane choice (a Linux host has none; a Mac with the
rustup target runs it; a Mac without it records `not_covered(linux_target_missing)`), the verdict
from cargo's output, and the `linux_check` entry of the typed receipt.
"""

import json
import os
from pathlib import Path
import runpy
import subprocess
import sys
import types
import unittest

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
from scratch_git import scratch_git, scratch_tempdir  # noqa: E402

SCRIPT = Path(__file__).resolve().parents[1] / "scoped-test"
RUNNER = runpy.run_path(str(SCRIPT))
TARGET = "x86_64-unknown-linux-gnu"
ZIG = {"kind": "zig", "zig": "/opt/homebrew/bin/zig"}


def compiler_error(message, file_name="crates/rsid/src/proc.rs", line=7, code="E0425"):
    return json.dumps({"reason": "compiler-message", "package_id": "rsid 0.1.0", "message": {
        "level": "error", "message": message, "code": {"code": code} if code else None,
        "rendered": f"error[{code}]: {message}\n",
        "spans": [{"is_primary": True, "file_name": file_name, "line_start": line}]}}) + "\n"


class LaneChoiceTest(unittest.TestCase):
    choice = staticmethod(RUNNER["linux_check_choice"])

    def test_a_linux_host_has_no_lane(self):
        self.assertIsNone(self.choice(True, ["rsid"], {TARGET}))

    def test_a_mac_with_the_target_runs_the_lane(self):
        entry = self.choice(False, ["rsid", "rsid-store"], {"aarch64-apple-darwin", TARGET}, ZIG)
        self.assertEqual(entry, {"target": TARGET, "packages": ["rsid", "rsid-store"], "status": "pending",
                                 "c_toolchain": ZIG})

    def test_a_mac_with_the_target_but_no_c_toolchain_is_not_covered_and_names_the_remedy(self):
        entry = self.choice(False, ["rsid"], {TARGET}, None)
        self.assertEqual((entry["status"], entry["reason"]), ("not_covered", "c_toolchain_missing"))
        self.assertEqual(entry["remedy"], RUNNER["LINUX_CHECK_C_REMEDY"])
        self.assertFalse(RUNNER["report_linux_check"](entry))

    def test_a_mac_without_the_target_is_not_covered_and_names_the_remedy(self):
        entry = self.choice(False, ["rsid"], {"aarch64-apple-darwin"}, ZIG)
        self.assertEqual((entry["status"], entry["reason"]), ("not_covered", "linux_target_missing"))
        self.assertEqual(entry["remedy"], f"rustup target add {TARGET}")
        self.assertEqual(RUNNER["linux_check_label"](entry), "not_covered(linux_target_missing)")

    def test_an_unreadable_rustup_is_not_covered(self):
        entry = self.choice(False, ["rsid"], None, ZIG)
        self.assertEqual((entry["status"], entry["reason"]), ("not_covered", "rustup_unavailable"))

    def test_no_touched_rust_package_needs_no_lane(self):
        entry = self.choice(False, [], {TARGET})
        self.assertEqual((entry["status"], entry["reason"]), ("not_applicable", "no_touched_rust_package"))

    def test_installed_targets_come_from_rustup_without_installing_anything(self):
        calls = []

        def run(command, **kwargs):
            calls.append(command)
            return types.SimpleNamespace(returncode=0, stdout=f"aarch64-apple-darwin\n{TARGET}\n")

        self.assertEqual(RUNNER["installed_rust_targets"]("/repo", run=run), {"aarch64-apple-darwin", TARGET})
        self.assertEqual(calls, [["rustup", "target", "list", "--installed"]])

    def test_a_failing_or_missing_rustup_cannot_say(self):
        failing = lambda command, **kwargs: types.SimpleNamespace(returncode=1, stdout="")  # noqa: E731

        def missing(command, **kwargs):
            raise FileNotFoundError("rustup")

        self.assertIsNone(RUNNER["installed_rust_targets"]("/repo", run=failing))
        self.assertIsNone(RUNNER["installed_rust_targets"]("/repo", run=missing))

    def test_the_command_checks_every_lane_package_for_linux_with_their_native_features(self):
        command = RUNNER["linux_check_command"](["cargo"], ["rsid", "rsid-store"], {"rsid": ["test-seam"]})
        self.assertEqual(command, ["cargo", "check", "--target", TARGET, "--all-targets", "--keep-going",
                                   "--message-format=json", "-p", "rsid", "-p", "rsid-store",
                                   "--features", "rsid/test-seam"])


class VerdictTest(unittest.TestCase):
    classify = staticmethod(RUNNER["classify_linux_check"])

    def test_a_clean_check_passes(self):
        self.assertEqual(self.classify(0, ['{"reason":"build-finished","success":true}\n'])["status"], "passed")

    def test_a_rust_compile_error_is_a_red_naming_the_first_error(self):
        lines = [compiler_error("cannot find function `proc_seam` in this scope"),
                 compiler_error("mismatched types", line=9, code="E0308"),
                 compiler_error("aborting due to 2 previous errors", code=None)]
        verdict = self.classify(101, lines)
        self.assertEqual((verdict["status"], verdict["reason"], verdict["rust_errors"]),
                         ("failed", "linux_check_failed", 2))
        self.assertEqual(verdict["first_error"],
                         "error[E0425]: cannot find function `proc_seam` in this scope (crates/rsid/src/proc.rs:7)")

    def test_a_missing_c_cross_toolchain_is_not_covered_never_red(self):
        lines = ["error: failed to run custom build command for `ring v0.17.14`\n",
                 "  --- stderr\n",
                 "  error occurred in cc-rs: failed to find tool \"x86_64-linux-gnu-gcc\": No such file or directory\n"]
        verdict = self.classify(101, lines)
        self.assertEqual((verdict["status"], verdict["reason"]), ("not_covered", "c_toolchain_missing"))
        self.assertEqual(verdict["build_scripts"], ["ring v0.17.14"])

    def test_a_rust_error_beside_a_c_toolchain_failure_is_still_red(self):
        lines = ["error: failed to run custom build command for `zstd-sys v2.0.15`\n",
                 "  Is `x86_64-linux-gnu-gcc` installed?\n",
                 compiler_error("cannot find value `LIBC_SEAM` in this scope")]
        self.assertEqual(self.classify(101, lines)["status"], "failed")

    def test_another_build_script_failure_is_not_covered(self):
        verdict = self.classify(101, ["error: failed to run custom build command for `foo v1.0.0`\n",
                                      "  thread 'main' panicked at build.rs:3:5\n"])
        self.assertEqual((verdict["status"], verdict["reason"]), ("not_covered", "build_script_failed"))

    def test_a_timeout_or_other_cargo_failure_is_not_covered(self):
        self.assertEqual(self.classify(124, [], timed_out=True)["reason"], "timed_out")
        verdict = self.classify(101, ["error: no matching package named `gone` found\n"])
        self.assertEqual((verdict["status"], verdict["reason"], verdict["first_error"]),
                         ("not_covered", "cargo_failed", "error: no matching package named `gone` found"))

    def test_the_lane_runs_one_bounded_command_and_logs_it(self):
        seen = {}

        def fake_stream(command, emit, cwd=None, budget=None, env=None):
            seen.update(command=command, cwd=cwd, budget=budget, env=env)
            emit(compiler_error("cannot find function `proc_seam` in this scope"))
            return 101

        with scratch_tempdir(prefix="scoped-test-linux-log-") as log_dir:
            entry = {"target": TARGET, "packages": ["rsid"], "status": "pending"}
            RUNNER["run_linux_check"](entry, "/repo", ["cargo"], {}, log_dir, "BUDGET", run_stream=fake_stream)
            self.assertIn("cannot find function", Path(entry["log"]).read_text())
        self.assertEqual((seen["cwd"], seen["budget"]), ("/repo", "BUDGET"))
        self.assertEqual(seen["command"][:4], ["cargo", "check", "--target", TARGET])
        self.assertEqual((entry["status"], entry["exit_code"]), ("failed", 101))
        self.assertTrue(RUNNER["report_linux_check"](entry))

    def test_a_lane_timeout_is_not_covered(self):
        def slow_stream(command, emit, cwd=None, budget=None, env=None):
            raise subprocess.TimeoutExpired(command, 1)

        with scratch_tempdir(prefix="scoped-test-linux-log-") as log_dir:
            entry = {"target": TARGET, "packages": ["rsid"], "status": "pending"}
            RUNNER["run_linux_check"](entry, "/repo", ["cargo"], {}, log_dir, None, run_stream=slow_stream)
        self.assertEqual((entry["status"], entry["reason"]), ("not_covered", "timed_out"))
        self.assertFalse(RUNNER["report_linux_check"](entry))


class CrossToolchainTest(unittest.TestCase):
    """#256: the C cross toolchain the lane's build scripts compile with, and its lane-only env."""

    detect = staticmethod(RUNNER["linux_cross_toolchain"])

    @staticmethod
    def which(found):
        return lambda name: found.get(name)

    def test_a_compiler_the_environment_names_for_the_target_wins(self):
        found = self.detect({"CC_x86_64_unknown_linux_gnu": "/opt/cross/cc"},
                            self.which({"zig": "/z", "x86_64-linux-gnu-gcc": "/g"}), windows=False)
        self.assertEqual(found, {"kind": "configured", "cc": "/opt/cross/cc",
                                 "variable": "CC_x86_64_unknown_linux_gnu"})

    def test_a_cross_gcc_on_path_comes_before_zig(self):
        found = self.detect({}, self.which({"zig": "/z", "x86_64-linux-gnu-gcc": "/usr/bin/x86_64-linux-gnu-gcc"}),
                            windows=False)
        self.assertEqual(found, {"kind": "cross_gcc", "cc": "/usr/bin/x86_64-linux-gnu-gcc"})

    def test_zig_on_path_is_the_mac_toolchain(self):
        self.assertEqual(self.detect({}, self.which({"zig": "/opt/homebrew/bin/zig"}), windows=False),
                         {"kind": "zig", "zig": "/opt/homebrew/bin/zig"})

    def test_no_toolchain_or_zig_on_windows_is_none(self):
        self.assertIsNone(self.detect({}, self.which({}), windows=False))
        self.assertIsNone(self.detect({}, self.which({"zig": "C:/zig/zig.exe"}), windows=True))

    def test_the_target_dir_follows_cargo_target_dir(self):
        self.assertEqual(RUNNER["lane_target_dir"]("/repo", {}), Path("/repo/target"))
        self.assertEqual(RUNNER["lane_target_dir"]("/repo", {"CARGO_TARGET_DIR": "/t"}), Path("/t"))
        self.assertEqual(RUNNER["lane_target_dir"]("/repo", {"CARGO_TARGET_DIR": "out"}), Path("/repo/out"))

    def test_a_configured_compiler_or_cross_gcc_adds_no_variables(self):
        with scratch_tempdir(prefix="scoped-test-linux-cc-") as target:
            self.assertEqual(RUNNER["linux_cross_env"]({"kind": "cross_gcc", "cc": "/g"}, target), {})
            self.assertEqual(RUNNER["linux_cross_env"](None, target), {})
            self.assertEqual(list(Path(target).iterdir()), [])

    def test_zig_wrappers_live_under_the_target_dir_and_stay_put_across_runs(self):
        with scratch_tempdir(prefix="scoped-test-linux-cc-") as target:
            env = RUNNER["linux_cross_env"](ZIG, target)
            root = Path(target) / TARGET / "rsi-zig-cc"
            self.assertEqual(env, {"CC_x86_64_unknown_linux_gnu": str(root / "cc"),
                                   "CXX_x86_64_unknown_linux_gnu": str(root / "c++"),
                                   "AR_x86_64_unknown_linux_gnu": str(root / "ar"),
                                   "ZIG_GLOBAL_CACHE_DIR": str(root / "zig-cache"),
                                   "ZIG_LOCAL_CACHE_DIR": str(root / "zig-cache")})
            cc = (root / "cc").read_text()
            self.assertIn("exec /opt/homebrew/bin/zig cc -target x86_64-linux-gnu", cc)
            self.assertIn('exec /opt/homebrew/bin/zig ar "$@"', (root / "ar").read_text())
            self.assertTrue(os.access(root / "cc", os.X_OK))
            stamp = (root / "cc").stat().st_mtime_ns
            self.assertEqual(RUNNER["linux_cross_env"](ZIG, target), env)
            self.assertEqual((root / "cc").stat().st_mtime_ns, stamp)

    @unittest.skipIf(os.name == "nt", "the zig wrapper is a POSIX shell script")
    def test_the_zig_wrapper_drops_the_clang_target_argument(self):
        with scratch_tempdir(prefix="scoped-test-linux-cc-") as target:
            fake = Path(target) / "fake-zig"
            fake.write_text('#!/bin/sh\nfor a do printf "%s\\n" "$a"; done\n')
            fake.chmod(0o755)
            env = RUNNER["linux_cross_env"]({"kind": "zig", "zig": str(fake)}, target)
            done = subprocess.run([env["CC_x86_64_unknown_linux_gnu"], "--target=x86_64-unknown-linux-gnu", "-O2",
                                   "-target", "x86_64-unknown-linux-gnu", "-c", "a b.c"],
                                  capture_output=True, text=True, check=True)
        self.assertEqual(done.stdout.splitlines(), ["cc", "-target", "x86_64-linux-gnu", "-O2", "-c", "a b.c"])

    def test_the_lane_env_reaches_only_the_lane_process(self):
        seen = {}

        def fake_stream(command, emit, cwd=None, budget=None, env=None):
            seen["env"] = env
            return 0

        with scratch_tempdir(prefix="scoped-test-linux-log-") as log_dir:
            entry = {"target": TARGET, "packages": ["rsid"], "status": "pending", "c_toolchain": dict(ZIG)}
            environ = {"PATH": "/usr/bin", "CARGO_TARGET_DIR": log_dir}
            RUNNER["run_linux_check"](entry, "/repo", ["cargo"], {}, log_dir, None, run_stream=fake_stream,
                                      environ=environ)
        self.assertEqual(entry["status"], "passed")
        self.assertEqual(seen["env"]["PATH"], "/usr/bin")
        self.assertTrue(seen["env"]["CC_x86_64_unknown_linux_gnu"].endswith("rsi-zig-cc/cc"))
        self.assertEqual(entry["c_toolchain"]["env"][0], "AR_x86_64_unknown_linux_gnu")
        self.assertNotIn("CC_x86_64_unknown_linux_gnu", environ)

    def test_a_toolchain_failure_after_the_run_carries_the_c_remedy(self):
        def failing_stream(command, emit, cwd=None, budget=None, env=None):
            emit("error: failed to run custom build command for `ring v0.17.14`\n")
            emit("  failed to find tool \"x86_64-linux-gnu-gcc\"\n")
            return 101

        with scratch_tempdir(prefix="scoped-test-linux-log-") as log_dir:
            entry = {"target": TARGET, "packages": ["rsid"], "status": "pending",
                     "c_toolchain": {"kind": "cross_gcc", "cc": "/g"}}
            RUNNER["run_linux_check"](entry, "/repo", ["cargo"], {}, log_dir, None, run_stream=failing_stream,
                                      environ={})
        self.assertEqual((entry["status"], entry["reason"]), ("not_covered", "c_toolchain_missing"))
        self.assertEqual(entry["remedy"], RUNNER["LINUX_CHECK_C_REMEDY"])
        self.assertFalse(RUNNER["report_linux_check"](entry))


class LanePackagesTest(unittest.TestCase):
    def setUp(self):
        self._temp = scratch_tempdir(prefix="scoped-test-linux-check-")
        self.repo = Path(self._temp.name) / "repo"
        (self.repo / "crates/alpha/src").mkdir(parents=True)
        (self.repo / "crates/beta/src").mkdir(parents=True)
        (self.repo / "Cargo.toml").write_text('[workspace]\nmembers = [\n    "crates/alpha",\n    "crates/beta",\n]\n')
        for name in ("alpha", "beta"):
            (self.repo / f"crates/{name}/Cargo.toml").write_text(
                f'[package]\nname = "{name}"\nversion = "0.1.0"\n\n[dependencies]\nserde = {{ version = "1" }}\n')
            (self.repo / f"crates/{name}/src/lib.rs").write_text("pub fn f() {}\n")
        scratch_git(self.repo, "init", "--quiet")
        scratch_git(self.repo, "config", "user.name", "Test")
        scratch_git(self.repo, "config", "user.email", "test@example.invalid")
        scratch_git(self.repo, "add", ".")
        scratch_git(self.repo, "commit", "--quiet", "-m", "base")
        self.base = self.head()

    def tearDown(self):
        self._temp.cleanup()

    def head(self):
        return scratch_git(self.repo, "rev-parse", "HEAD").stdout.strip()

    def commit(self, path, text):
        (self.repo / path).parent.mkdir(parents=True, exist_ok=True)
        (self.repo / path).write_text(text)
        scratch_git(self.repo, "add", path)
        scratch_git(self.repo, "commit", "--quiet", "-m", "change")
        return self.head()

    @staticmethod
    def git_out(repo, *args):
        done = scratch_git(repo, *args, check=False)
        if done.returncode:
            raise SystemExit(done.stderr)
        return done.stdout

    def packages(self, head, extra=()):
        return RUNNER["linux_check_packages"](self.repo, self.base, head, self.git_out, extra)

    def test_a_changed_rust_source_selects_its_package(self):
        head = self.commit("crates/alpha/src/seam.rs", '#[cfg(any(target_os = "macos", test))]\npub fn s() {}\n')
        self.assertEqual(self.packages(head), ["alpha"])

    def test_a_docs_only_change_inside_a_crate_selects_none(self):
        self.assertEqual(self.packages(self.commit("crates/beta/AGENTS.md", "notes\n")), [])

    def test_downstream_compile_packages_join_the_lane(self):
        head = self.commit("crates/alpha/src/lib.rs", "pub fn f() {}\npub fn g() {}\n")
        self.assertEqual(self.packages(head, extra=["beta"]), ["alpha", "beta"])

    def test_a_manifest_change_selects_its_package(self):
        head = self.commit("crates/beta/Cargo.toml", '[package]\nname = "beta"\nversion = "0.2.0"\n')
        self.assertEqual(self.packages(head), ["beta"])


class ReceiptTest(unittest.TestCase):
    def test_a_red_linux_check_keeps_a_baseline_only_run_red(self):
        receipt = {"baseline_comparison": {"status": "complete"},
                   "failures": [{"package": "rsid", "kind": "lib", "target": "rsid", "test": "t"}],
                   "baseline_reds": [{"package": "rsid", "kind": "lib", "target": "rsid", "test": "t"}],
                   "new_failures": [], "packages": [{"package": "rsid", "status": "failed", "phase": "test_failed"}],
                   "baseline_failure_records": [{"status": "recorded"}]}
        self.assertEqual(RUNNER["baseline_only_ok_reason"](receipt), "baseline_known_failure")
        receipt["linux_check"] = {"status": "failed", "reason": "linux_check_failed"}
        self.assertIsNone(RUNNER["baseline_only_ok_reason"](receipt))
        receipt["linux_check"] = {"status": "not_covered", "reason": "linux_target_missing"}
        self.assertEqual(RUNNER["baseline_only_ok_reason"](receipt), "baseline_known_failure")

    def test_a_dry_run_receipt_carries_the_lane_on_a_non_linux_host_only(self):
        temp = scratch_tempdir(prefix="scoped-test-linux-receipt-")
        self.addCleanup(temp.cleanup)
        repo = Path(temp.name) / "repo"
        (repo / "crates/alpha/src").mkdir(parents=True)
        (repo / "Cargo.toml").write_text('[workspace]\nmembers = ["crates/alpha"]\n')
        (repo / "crates/alpha/Cargo.toml").write_text('[package]\nname = "alpha"\nversion = "0.1.0"\n')
        (repo / "crates/alpha/src/lib.rs").write_text("pub fn f() {}\n")
        scratch_git(repo, "init", "--quiet")
        scratch_git(repo, "config", "user.name", "Test")
        scratch_git(repo, "config", "user.email", "test@example.invalid")
        scratch_git(repo, "add", ".")
        scratch_git(repo, "commit", "--quiet", "-m", "base")
        (repo / "crates/alpha/src/lib.rs").write_text("pub fn f() {}\npub fn g() {}\n")
        scratch_git(repo, "commit", "--quiet", "-am", "change")
        leaked = {"RSI_SCOPED_TEST_FILTERS", "RSI_SCOPED_TEST_TOOL_SOURCE", "RSI_SCOPED_TEST_TOOL_REV",
                  "RSI_SCOPED_TEST_TOOL_FILES"}
        env = {k: v for k, v in os.environ.items() if k not in leaked}
        env["RSI_SCOPED_TEST_RUNNER"] = "libtest"
        done = subprocess.run([sys.executable, str(SCRIPT), "--repo", str(repo), "--dry-run", "--base", "HEAD~1",
                               "--filter", "alpha=f"], cwd=SCRIPT.parents[1], text=True, capture_output=True, env=env)
        lines = done.stdout.splitlines()
        self.assertTrue(lines and lines[-1].startswith("SCOPED_TEST_RECEIPT "), done.stdout + done.stderr)
        receipt = json.loads(lines[-1].split(" ", 1)[1])
        if sys.platform.startswith("linux"):
            self.assertNotIn("linux_check", receipt)
            return
        lane = receipt["linux_check"]
        self.assertEqual((lane["target"], lane["packages"]), (TARGET, ["alpha"]))
        if lane["status"] == "planned":
            command = lane["command"]
            self.assertEqual(command[command.index("check"):], ["check", "--target", TARGET, "--all-targets",
                                                                "--keep-going", "--message-format=json", "-p", "alpha"])
            self.assertIn(lane["c_toolchain"]["kind"], ("configured", "cross_gcc", "zig"))
        else:
            self.assertEqual(lane["status"], "not_covered")
            self.assertIn(lane["reason"], ("linux_target_missing", "rustup_unavailable", "c_toolchain_missing"))
            self.assertIn(lane["remedy"], done.stdout)


if __name__ == "__main__":
    unittest.main()
