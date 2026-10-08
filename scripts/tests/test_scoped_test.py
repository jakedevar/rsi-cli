"""Bounded runner checks with stub builds and an optional real systemd stop."""

import contextlib
import io
import json
import re
import os
from pathlib import Path
import runpy
import shutil
import subprocess
import sys
import tempfile
import unittest
from unittest.mock import patch

SCRIPT = Path(__file__).resolve().parents[1] / "scoped-test"
RUNNER = runpy.run_path(str(SCRIPT))


class ScopedTest(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix="scoped-test-unit-")
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.log = self.root / "commands.log"
        self.binary = self.executable("test-binary", '''
import sys
print('store::migration: test' if '--list' in sys.argv else 'test result: ok. 1 passed')
''')
        artifact = {"reason": "compiler-artifact", "profile": {"test": True},
                    "manifest_path": str(self.root / "Cargo.toml"),
                    "executable": str(self.binary), "target": {"kind": ["lib"], "name": "rsid_store"}}
        self.cargo = self.executable("cargo-stub", f"print({json.dumps(json.dumps(artifact))})\n")
        self.environment = patch.dict(os.environ, SCOPED_TEST_LOG=str(self.log))
        self.environment.start()
        self.addCleanup(self.environment.stop)

    def executable(self, name, body):
        path = self.root / name
        path.write_text(f"#!{sys.executable}\nimport os, sys\nfrom pathlib import Path\n"
                        "with Path(os.environ['SCOPED_TEST_LOG']).open('a') as log:\n"
                        "    log.write(Path(sys.argv[0]).name + ' ' + ' '.join(sys.argv[1:]) + '\\n')\n" + body)
        path.chmod(0o755)
        return path

    def plan(self):
        selections = RUNNER["selection_plan"]([
            "rsid=shard:store-01:test(store::migration)",
            "rsid=shard:store-01:test(migration)"], {"store-01": ["rsid-store"]})["rsid-store"]
        return {"repo": str(self.root), "package": "rsid-store", "selections": selections,
                "build": RUNNER["build_command"]("rsid-store", selections, [str(self.cargo)])}

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

    def invoke(self, linux=True, dry_run=False, systemd_body=None, filters=None, seconds="0.2",
               shard_packages=None, explicit=None, head="candidate", worktree_head="candidate",
               reclaim_incremental=False, derive_code=0, audits=None, guard_failure=None,
               broad=False, total=None, env_filters=None, python_paths=()):
        selected = filters if filters is not None else ["rsid=shard:store-01:test(store::migration)"]
        receipt = subprocess.CompletedProcess([], derive_code, json.dumps({
            "suggested_filters": selected, "head": "candidate", "broad": broad,
            "audits": audits if audits is not None else {
                "shard_gates": {"verdict": "ok"}, "released_migrations": {"verdict": "ok"}},
        }), "BROAD: split the run\n" if broad else "")
        self.guard_commands = []

        def run(command, **kwargs):
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
                   "cargo_prefix": lambda: [str(self.cargo)],
                   "shard_packages": lambda repo: (shard_packages if shard_packages is not None else
                                                    {"store-01": ["rsid-store"]})}
        argv = [str(SCRIPT), "--repo", str(self.root), "--head", head,
                "--runtime-max-sec", seconds, "--cpu-quota", "75"]
        if reclaim_incremental:
            argv.append("--reclaim-incremental")
        if total:
            argv.extend(["--total-max-sec", total])
        for index, value in enumerate(explicit or []):
            argv.extend(["--filter" if index == 0 else "--test-filter", value])
        if dry_run:
            argv.append("--dry-run")
        output = io.StringIO()
        with patch.object(sys, "argv", argv), patch.object(sys, "platform", "linux" if linux else "darwin"), \
             patch.dict(os.environ, PATH=str(self.root) + os.pathsep + os.environ.get("PATH", ""),
                        CARGO_TARGET_DIR=str(self.root / "target"),
                        **({"RSI_SCOPED_TEST_FILTERS": env_filters} if env_filters else {})), \
             patch("runpy.run_path", return_value=checker), patch("subprocess.run", side_effect=run) as derive, \
             patch.object(RUNNER["PYTHON_GATE"], "changed_paths", return_value=python_paths), \
             patch("tempfile.mkdtemp", return_value=str(log_dir)), \
             contextlib.redirect_stdout(output), contextlib.redirect_stderr(output):
            code = RUNNER["main"]()
        if explicit or env_filters:
            self.assertTrue(all(not str(call.args[0][1]).endswith("check-touched-shards")
                                for call in derive.call_args_list))
        else:
            self.assertIn("--no-compile", derive.call_args_list[0].args[0])
        return code, output.getvalue()

    def test_scripts_only_runs_matching_python_tests_without_cargo(self):
        test = self.root / "scripts/tests/test_widget.py"
        test.parent.mkdir(parents=True)
        test.write_text("import unittest\nclass Test(unittest.TestCase):\n    def test_widget(self): pass\n")
        code, output = self.invoke(filters=[], python_paths=["scripts/widget.py"])
        self.assertEqual(code, 0, output)
        self.assertIn("test_widget", output)
        self.assertEqual(list(self.root.glob("commands.log")), [])

    def test_python_failure_blocks_the_rust_build(self):
        test = self.root / "tools/tests/test_widget.py"
        test.parent.mkdir(parents=True)
        test.write_text("import unittest\nclass Test(unittest.TestCase):\n"
                        "    def test_widget(self): self.assertEqual(1, 2)\n")
        code, output = self.invoke(python_paths=["tools/widget.py"])
        self.assertEqual(code, 1, output)
        self.assertIn("Python test failed", output)
        self.assertEqual(list(self.root.glob("commands.log")), [])

    def test_python_dry_run_prints_selection_without_running_tests(self):
        test = self.root / "scripts/tests/test_widget.py"
        test.parent.mkdir(parents=True)
        test.write_text("import unittest\n")
        code, output = self.invoke(filters=[], dry_run=True, python_paths=["scripts/widget.py"])
        self.assertEqual(code, 0, output)
        self.assertIn("python test:", output)
        self.assertEqual(list(self.root.glob("commands.log")), [])

    def test_broad_dry_run_reports_derived_guards_and_runs_identity_guard(self):
        code, output = self.invoke(dry_run=True, broad=True)
        self.assertEqual(code, 0, output)
        self.assertIn("BROAD", output)
        for name in ("shard_gates", "released_migrations", "identity_assertions"):
            self.assertIn(f"static guard {name}=ok", output)
        self.assertEqual(self.guard_commands, [[sys.executable, "tools/check_identity_assertions.py"]])
        self.assertEqual(list(self.root.glob("commands.log")), [])

    def test_declined_broad_plan_still_runs_and_reports_all_missing_guards(self):
        code, output = self.invoke(derive_code=2, broad=True, audits={},
                                   guard_failure="scripts/check-rsid-test-shards.py")
        self.assertEqual(code, 2, output)
        self.assertEqual(self.guard_commands, [
            [sys.executable, "scripts/check-rsid-test-shards.py", "--require-gates"],
            [sys.executable, "tools/check-released-migrations.py", "origin/rolling", "candidate"],
            [sys.executable, "tools/check_identity_assertions.py"],
        ])
        self.assertIn("static guard shard_gates=fail: guard finding", output)
        self.assertIn("static guard released_migrations=ok", output)
        self.assertIn("static guard identity_assertions=ok", output)
        self.assertEqual(list(self.root.glob("commands.log")), [])

    def test_explicit_narrowing_runs_all_guards_and_each_failure_blocks_build(self):
        for path, name in (("scripts/check-rsid-test-shards.py", "shard_gates"),
                           ("tools/check-released-migrations.py", "released_migrations"),
                           ("tools/check_identity_assertions.py", "identity_assertions")):
            with self.subTest(guard=name):
                code, output = self.invoke(explicit=["rsid-store=migration"], guard_failure=path)
                self.assertEqual(code, 1, output)
                self.assertEqual(len(self.guard_commands), 3)
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
        self.assertEqual(self.guard_commands, [[sys.executable, "tools/check_identity_assertions.py"]])
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
        code, output = self.invoke(filters=filters, explicit=filters, seconds="5", total="2",
                                   systemd_body="""
import time
time.sleep(1.3)
index = sys.argv.index('--execute-plan')
os.execv(sys.executable, [sys.executable, '-u', sys.argv[index - 1], *sys.argv[index:]])
""")
        self.assertEqual(code, 1, output)
        commands = self.log.read_text()
        self.assertEqual(len(re.findall(r"RuntimeMaxSec=(?:1\.9\d*|2) ", commands)), 1, commands)
        self.assertEqual(commands.count("cargo-stub test"), 1)
        self.assertIn("scoped_test_timeout: total budget exhausted before rsid-store", output)
        self.assertIn("rsid: passed (1 completed tests)", output)
        self.assertIn("rsid-store: timed out (0 completed tests)", output)

    def test_makefile_scoped_test_budgets_fit_inside_the_recipe_cap(self):
        root = Path(__file__).resolve().parents[2]
        make, manifest = (root / "Makefile").read_text(), (root / ".rsi/jobs.toml").read_text()
        for target in ("scoped-test", "scoped-test-tmpfs"):
            block = re.search(rf"^{target}:\n(?:\t.*\n)+", make, re.M)[0]
            total = int(re.search(r"--total-max-sec (\d+)", block)[1])
            per_package = int(re.search(r"--runtime-max-sec (\d+)", block)[1])
            recipe = re.search(rf'\[recipes\.{target}\]\n(?:.*\n)+?\n', manifest + "\n")[0]
            cap = int(re.search(r"timeout_minutes = (\d+)", recipe)[1]) * 60
            self.assertLessEqual(per_package, total, target)
            self.assertLess(total, cap, target)

    def test_explicit_dry_run_prints_caps_without_building(self):
        code, output = self.invoke(dry_run=True, explicit=["rsid-store=migration"],
                                   head="base", worktree_head="candidate")
        self.assertEqual(code, 0, output)
        self.assertIn("RuntimeMaxSec=0.2", output)
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
        self.assertIn("RuntimeMaxSec=0.2", output)
        self.assertIn("CPUQuota=75%", output)
        self.assertIn("test(store::migration)", output)
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

    def test_named_targets_are_explicit_in_build(self):
        selections = RUNNER["selection_plan"](["rsi=bin:tool", "rsi=test:integration"], {})["rsi"]
        self.assertEqual(RUNNER["build_command"]("rsi", selections, ["cargo-stub"]),
                         ["cargo-stub", "test", "-p", "rsi", "--bin", "tool", "--test", "integration",
                          "--no-run", "--message-format=json"])


class ScopedTestReceipt(unittest.TestCase):
    """#1638: the daemon's scoped-test job reads the last-line JSON receipt."""

    def run_script(self, *args):
        done = subprocess.run([sys.executable, str(SCRIPT), *args], cwd=SCRIPT.parents[1],
                              text=True, capture_output=True)
        lines = done.stdout.splitlines()
        self.assertTrue(lines and lines[-1].startswith("SCOPED_TEST_RECEIPT "), done.stdout + done.stderr)
        return done.returncode, json.loads(lines[-1].split(" ", 1)[1])

    def test_a_dry_run_ends_with_the_typed_receipt(self):
        code, receipt = self.run_script("--dry-run", "--base", "HEAD", "--filter", "rsid=shard:other-01:test(x)")
        self.assertEqual(code, 0)
        self.assertEqual((receipt["ok"], receipt["exit_code"], receipt["schema"]), (True, 0, 1))
        self.assertEqual(receipt["base"], "HEAD")
        self.assertEqual(receipt["filters"], ["rsid=shard:other-01:test(x)"])
        self.assertRegex(receipt["head"], r"^[0-9a-f]{40}$")
        self.assertTrue(receipt["log_dir"].startswith("/tmp/") or Path(receipt["log_dir"]).is_dir())

    def test_a_failed_run_still_ends_with_a_red_receipt(self):
        code, receipt = self.run_script("--base", "no-such-ref-for-receipt-test")
        self.assertNotEqual(code, 0)
        self.assertEqual((receipt["ok"], receipt["exit_code"]), (False, code))
        self.assertEqual(receipt["base"], "no-such-ref-for-receipt-test")

    def test_the_package_executor_prints_no_receipt(self):
        done = subprocess.run([sys.executable, str(SCRIPT), "--execute-plan", "/nonexistent"],
                              text=True, capture_output=True)
        self.assertNotIn("SCOPED_TEST_RECEIPT", done.stdout)


if __name__ == "__main__":
    unittest.main()
