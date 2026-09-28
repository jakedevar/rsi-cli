"""Real temp-Git repositories; fake systemd scope and Python QA, never Cargo."""

import ast
import contextlib
import importlib.util
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import time
import unittest
from unittest import mock

sys.dont_write_bytecode = True
SCRIPT = Path(__file__).resolve().parents[1] / "agent-dev-queue.py"
SPEC = importlib.util.spec_from_file_location("agent_dev_queue", SCRIPT)
Q = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(Q)


class PilotTest(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix="rsi-batch-qa-test-")
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.repo = self.root / "repo"
        self.repo.mkdir()
        self.git("init", "-q")
        self.git("config", "user.name", "Pilot Test")
        self.git("config", "user.email", "pilot@example.invalid")
        (self.repo / "shared").write_text("base\n")
        (self.repo / "Cargo.lock").write_text("fixture only, never compiled\n")
        (self.repo / "rust-toolchain.toml").write_text('[toolchain]\nchannel = "fixture"\n')
        self.git("add", ".")
        self.git("commit", "-qm", "base")
        self.base = self.git("rev-parse", "HEAD")
        self.base_ref = self.git("symbolic-ref", "HEAD")
        self.bin = self.root / "bin"
        self.bin.mkdir()
        self.runner = self.bin / "systemd-run"
        self.runner.write_text(f'''#!{sys.executable}
import os, subprocess, sys
from pathlib import Path
with open(os.environ["RUNNER_CALLS"], "a") as log:
    log.write(repr(sys.argv[1:]) + "\\n")
assert any(arg.startswith("--property=MemoryHigh=") for arg in sys.argv)
assert any(arg.startswith("--property=MemoryMax=") for arg in sys.argv)
assert "--property=MemorySwapMax=0" in sys.argv
assert "--property=CPUWeight=50" in sys.argv
if os.environ.get("FAIL_RUNNER"):
    sys.exit(91)
sys.exit(subprocess.call(sys.argv[sys.argv.index("--") + 1:]))
''')
        self.runner.chmod(0o755)
        self.control = self.bin / "systemctl"
        self.control.write_text(f'''#!{sys.executable}
import os, signal, sys
from pathlib import Path
state = Path(os.environ["HOME"]).parent / "scope-active"
if "stop" in sys.argv:
    if os.environ.get("FAIL_SCOPE_STOP"):
        sys.exit(1)
    if state.exists():
        pid = int(state.read_text())
        if pid:
            try:
                os.killpg(pid, signal.SIGKILL)
            except ProcessLookupError:
                pass
        state.with_name("scope-stopped").write_text(str(pid))
        state.unlink()
    sys.exit(0)
print("LoadState=loaded")
print("ActiveState=" + ("active" if state.exists() else "inactive"))
''')
        self.control.chmod(0o755)
        self.qa = self.root / "qa.py"
        self.qa.write_text('''import os, sys
from pathlib import Path
assert os.environ["CARGO_BUILD_JOBS"] == "1"
assert os.environ["CARGO_PROFILE_DEV_DEBUG"] == "line-tables-only"
assert "RSI_SESSION_TOKEN" not in os.environ
with open(os.environ["QA_CALLS"], "a") as log:
    log.write("ran\\n")
cache = Path(os.environ["CARGO_TARGET_DIR"])
cache.mkdir(exist_ok=True)
(cache / "retained").write_text("warm cache")
print("fake QA completed")
sys.exit(int(os.environ.get("QA_EXIT", "0")))
''')
        self.calls = self.root / "calls"
        self.runner_calls = self.root / "runner-calls"
        self.config = {
            "base_ref": self.base_ref, "base": self.base, "qa_owner": "qa-custody-1",
            "workspace": str(self.root / "qa-worktree"), "max_wip": 4, "batch_size": 2,
            "oldest_seconds": 60, "max_batches": 4,
            "environment": {"PATH": str(self.bin) + os.pathsep + os.defpath,
                            "HOME": str(self.root / "home"), "QA_CALLS": str(self.calls), "RUNNER_CALLS": str(self.runner_calls)},
            "command": [sys.executable, str(self.qa)],
            "toolchain_command": [sys.executable, "--version"],
            "external_inputs": [str(self.qa)],
        }
        self.serial = 0

    def git(self, *args, repo=None):
        return subprocess.check_output(["git", "-C", str(repo or self.repo), *args],
                                       stderr=subprocess.PIPE, text=True).strip()

    @contextlib.contextmanager
    def queue(self):
        with Q.Queue(self.repo).locked() as queue:
            yield queue

    def init(self, **overrides):
        self.config.update(overrides)
        with self.queue() as queue:
            return queue.init(self.config)

    def source(self, name=None, contents="change\n", filename=None, base=None):
        self.serial += 1
        key = name or f"source-{self.serial}"
        path = self.root / key
        self.git("worktree", "add", "--detach", str(path), base or self.base)
        (path / (filename or key)).write_text(contents)
        self.git("add", ".", repo=path)
        self.git("commit", "-qm", key, repo=path)
        return {"key": key, "source": self.git("rev-parse", "HEAD", repo=path),
                "source_path": str(path), "owner": f"owner-{key}", "custody": f"custody-{key}",
                "checks": "python static checks declared", "tests": "regression declared; NOT run"}

    def intake(self, request):
        with self.queue() as queue:
            return queue.intake(request)

    def frozen(self):
        with self.queue() as queue:
            return queue.freeze(self.config["qa_owner"])

    def run_qa(self):
        with self.queue() as queue:
            return queue.run(self.config["qa_owner"])

    def prepare(self, **overrides):
        self.init(**overrides)
        one, two = self.source(), self.source()
        self.intake(one)
        self.intake(two)
        return one, two, self.frozen()

    def test_multi_source_success_exact_receipt_and_idempotent_retry(self):
        one, two, batch = self.prepare()
        initial = self.git("rev-parse", self.base_ref)
        receipt = self.run_qa()
        self.assertTrue(receipt["passed"])
        self.assertEqual(receipt["inputs"]["base"], self.base)
        self.assertEqual([m["request"]["source"] for m in receipt["inputs"]["members"]],
                         [one["source"], two["source"]])
        self.assertEqual(receipt["inputs"]["tree"], self.git("rev-parse", batch["candidate"] + "^{tree}"))
        self.assertIn("Python", receipt["terminal"]["toolchain"]["stdout"])
        self.assertEqual(self.run_qa(), receipt)
        self.assertEqual(self.calls.read_text(), "ran\n")
        self.assertEqual(len(self.runner_calls.read_text().splitlines()), 1)
        self.assertEqual(self.git("rev-parse", self.base_ref), initial)
        with self.queue() as queue:
            self.assertTrue(queue.eligible()["eligible_for_reviewed_admission"])
            self.assertEqual(queue.freeze(self.config["qa_owner"])["id"], batch["id"])
            self.assertFalse(queue.cleanup()["cleanup_authorized"])
        self.assertEqual(self.intake(one)["request"], one)

    def test_failed_qa_exposes_whole_batch_owners_and_repair_retains_sources(self):
        env = dict(self.config["environment"], QA_EXIT="7")
        one, two, _ = self.prepare(environment=env)
        receipt = self.run_qa()
        self.assertEqual(receipt["runner_exit"], 7)
        with self.queue() as queue:
            report = queue.eligible()
            self.assertFalse(report["eligible_for_reviewed_admission"])
            self.assertEqual([m["owner"] for m in report["repair_owners"]], [one["owner"], two["owner"]])
            retired = queue.retire(self.config["qa_owner"], "repair both sources")
            self.assertTrue(retired["retention_preserved"])
        self.assertEqual(self.git("rev-parse", Q.REF + "sources/" + one["key"]), one["source"])
        self.assertTrue(Path(two["source_path"]).is_dir())
        self.intake(self.source())

    def test_conflict_is_durable_restartable_and_no_source_worktree_changes(self):
        self.init()
        one = self.source(contents="one\n", filename="shared")
        two = self.source(contents="two\n", filename="shared")
        self.intake(one)
        self.intake(two)
        batch = self.frozen()
        self.assertEqual(batch["status"], "conflict")
        self.assertIn("shared", batch["conflict"])
        self.assertEqual(self.frozen(), batch)
        with self.assertRaisesRegex(Q.Refusal, "conflict"):
            self.run_qa()
        for request in (one, two):
            self.assertEqual(self.git("status", "--porcelain", repo=request["source_path"]), "")
            self.assertEqual(self.git("rev-parse", "HEAD", repo=request["source_path"]), request["source"])
        with self.queue() as queue:
            queue.retire(self.config["qa_owner"], "owners reconcile shared file in new commits")
            self.assertEqual(queue.state["active"], None)
            self.assertEqual(queue.state["batches"][0]["status"], "retired")

    def test_count_age_wip_and_batch_budget(self):
        self.init(max_wip=2, batch_size=2, max_batches=1)
        one, two = self.source(), self.source()
        self.intake(one)
        with self.queue() as queue:
            self.assertFalse(queue.ready()["ready"])
            with self.assertRaisesRegex(Q.Refusal, "readiness"):
                queue.freeze(self.config["qa_owner"])
            received = queue.state["members"][0]["received"]
            with mock.patch.object(Q.time, "time", return_value=received + 61):
                self.assertTrue(queue.ready()["ready"])
        self.intake(two)
        with self.assertRaisesRegex(Q.Refusal, "WIP"):
            self.intake(self.source())
        self.frozen()
        with self.queue() as queue:
            self.assertEqual(len(queue.batch()["members"]), 2)
        with self.assertRaisesRegex(Q.Refusal, "WIP|budget"):
            self.intake(self.source())

    def test_age_alone_freezes_partial_batch(self):
        self.init()
        self.intake(self.source())
        with self.queue() as queue:
            received = queue.state["members"][0]["received"]
            with mock.patch.object(Q.time, "time", return_value=received + 60):
                self.assertEqual(len(queue.freeze(self.config["qa_owner"])["members"]), 1)

    def test_batch_size_bounds_pending_queue(self):
        self.init(max_wip=4, batch_size=2)
        for _ in range(4):
            self.intake(self.source())
        self.assertEqual(len(self.frozen()["members"]), 2)
        with self.queue() as queue:
            self.assertEqual(queue.ready()["count"], 2)

    def test_lock_is_shared_across_linked_worktrees(self):
        self.init()
        request = self.source()
        with self.queue():
            with self.assertRaisesRegex(Q.Refusal, "busy"):
                with Q.Queue(request["source_path"]).locked():
                    self.fail("second lock acquired")

    def test_ref_cas_failure_cannot_partially_record_intake(self):
        self.init()
        request = self.source()
        conflicting = Q.REF + "sources/" + request["key"]
        self.git("update-ref", conflicting, self.base)
        with self.assertRaises(Q.Refusal):
            self.intake(request)
        with self.queue() as queue:
            self.assertEqual(queue.state["members"], [])
        self.assertEqual(self.git("rev-parse", conflicting), self.base)

    def test_journal_cas_failure_preserves_prior_state(self):
        self.init()
        with self.queue() as queue:
            original = queue.sha
            queue.sha = self.base
            queue.state["unexpected"] = "must not persist"
            with self.assertRaises(Q.Refusal):
                queue.save()
        self.assertEqual(self.git("rev-parse", Q.STATE), original)
        with self.queue() as queue:
            self.assertEqual(set(queue.state), {"version", "id", "config", "members", "batches", "active", "accepted_base"})

    def test_restart_after_committed_intake_does_not_double_add(self):
        self.init()
        request = self.source()
        self.intake(request)
        self.intake(request)
        with self.queue() as queue:
            self.assertEqual(len(queue.state["members"]), 1)
        changed = dict(request, custody="another-custody")
        with self.assertRaisesRegex(Q.Refusal, "different inputs"):
            self.intake(changed)
        with self.assertRaisesRegex(Q.Refusal, "already retained"):
            self.intake(dict(request, key="another-key"))

    def test_dirty_and_stale_source_intake_rejected(self):
        self.init()
        request = self.source()
        extra = Path(request["source_path"]) / "untracked"
        extra.write_text("preserve me")
        with self.assertRaisesRegex(Q.Refusal, "dirty"):
            self.intake(request)
        extra.unlink()
        with self.assertRaisesRegex(Q.Refusal, "stale HEAD"):
            self.intake(dict(request, source=self.base))

    def test_receipt_and_log_tampering_fail_closed(self):
        self.prepare()
        self.run_qa()
        with self.queue() as queue:
            directory = queue.directory / queue.batch()["id"]
            path = directory / "receipt.json"
            original = path.read_bytes()
            receipt = json.loads(original)
            receipt["runner_exit"] = 1
            Q.atomic_json(path, receipt)
            with self.assertRaisesRegex(Q.Refusal, "tampered"):
                queue.eligible()
            path.write_bytes(original)
            with (directory / "qa.log").open("a") as log:
                log.write("tampered")
            with self.assertRaisesRegex(Q.Refusal, "tampered"):
                queue.eligible()

    def test_modified_config_command_and_external_inputs_invalidate_receipt(self):
        self.prepare()
        self.run_qa()
        original = self.qa.read_bytes()
        self.qa.write_bytes(original + b"\n# edited\n")
        with self.queue() as queue:
            with self.assertRaisesRegex(Q.Refusal, "modified inputs"):
                queue.eligible()
        self.qa.write_bytes(original)
        self.git("config", "test.pilot", "changed")
        with self.queue() as queue:
            with self.assertRaisesRegex(Q.Refusal, "modified inputs"):
                queue.eligible()

    def test_dirty_qa_inputs_and_stale_base_fail_closed(self):
        self.prepare()
        self.run_qa()
        workspace = Path(self.config["workspace"])
        file = workspace / "Cargo.lock"
        original = file.read_text()
        file.write_text("changed lockfile")
        with self.queue() as queue:
            with self.assertRaisesRegex(Q.Refusal, "dirty"):
                queue.eligible()
        file.write_text(original)
        request = self.source()
        self.git("update-ref", self.base_ref, request["source"], self.base)
        with self.queue() as queue:
            with self.assertRaisesRegex(Q.Refusal, "stale input ref"):
                queue.eligible()

    def test_tampered_member_ref_blocks_run(self):
        one, _, _ = self.prepare()
        self.git("update-ref", Q.REF + "sources/" + one["key"], self.base)
        with self.assertRaisesRegex(Q.Refusal, "stale input ref"):
            self.run_qa()
        self.assertFalse(self.calls.exists())

    def test_runner_setup_failure_never_runs_unbounded_fallback(self):
        env = dict(self.config["environment"], FAIL_RUNNER="1")
        self.prepare(environment=env)
        receipt = self.run_qa()
        self.assertEqual(receipt["runner_exit"], 91)
        self.assertFalse(receipt["passed"])
        self.assertFalse(self.calls.exists())
        self.assertEqual(self.run_qa(), receipt)
        self.assertEqual(len(self.runner_calls.read_text().splitlines()), 1)
        with self.queue() as queue:
            self.assertEqual(len(queue.eligible()["repair_owners"]), 2)

    def test_crash_after_launch_intent_refuses_rerun(self):
        self.prepare()
        real_run = Q.subprocess.Popen
        def crash(args, **kwargs):
            if str(args[0]) == str(self.runner):
                raise KeyboardInterrupt("simulated process death before result")
            return real_run(args, **kwargs)
        with mock.patch.object(Q.subprocess, "Popen", side_effect=crash):
            with self.assertRaises(KeyboardInterrupt):
                self.run_qa()
        with self.assertRaisesRegex(Q.Refusal, "running; no automatic retry"):
            self.run_qa()
        with self.queue() as queue:
            self.assertEqual(queue.batch()["status"], "running")
            with self.assertRaisesRegex(Q.Refusal, "terminal failure"):
                queue.retire(self.config["qa_owner"], "cannot assert runner stopped")
        self.assertFalse(self.calls.exists())

    def test_crash_after_journal_before_receipt_mirror_recovers_without_rerun(self):
        self.prepare()
        original = Q.atomic_json
        def crash(path, value):
            if Path(path).name == "receipt.json":
                raise KeyboardInterrupt("crash before filesystem receipt")
            original(path, value)
        with mock.patch.object(Q, "atomic_json", side_effect=crash):
            with self.assertRaises(KeyboardInterrupt):
                self.run_qa()
        with self.queue() as queue:
            self.assertTrue(queue.recover(self.config["qa_owner"])["passed"])
            self.assertTrue(queue.eligible()["eligible_for_reviewed_admission"])
        self.assertTrue(self.run_qa()["passed"])
        self.assertEqual(self.calls.read_text(), "ran\n")

    def test_workspace_add_restart_reuses_claim_and_preserves_dirty_data(self):
        self.prepare()
        with self.queue() as queue:
            workspace = queue.workspace(queue.batch())
        with self.queue() as queue:
            self.assertEqual(queue.workspace(queue.batch()), workspace)
        (workspace / "precious").write_text("preserve")
        with self.assertRaisesRegex(Q.Refusal, "dirty"):
            self.run_qa()
        self.assertEqual((workspace / "precious").read_text(), "preserve")

    def test_successful_landing_observation_reuses_owner_workspace_and_cache(self):
        _, _, first = self.prepare()
        self.run_qa()
        with self.queue() as queue:
            with self.assertRaisesRegex(Q.Refusal, "not landed"):
                queue.landed(self.config["qa_owner"])
        # Simulate EXTERNAL reviewed admission in this throwaway repository only.
        self.git("update-ref", self.base_ref, first["candidate"], self.base)
        with self.queue() as queue:
            queue.landed(self.config["qa_owner"])
            self.assertTrue(all(m["candidate_for_daemon_custody_check"] for m in queue.cleanup()["members"]))
            self.assertFalse(queue.cleanup()["cleanup_authorized"])
            cache = queue.directory / "cache" / "retained"
            self.assertEqual(cache.read_text(), "warm cache")
        for _ in range(2):
            self.intake(self.source(base=first["candidate"]))
        second = self.frozen()
        self.assertEqual(second["base"], first["candidate"])
        self.run_qa()
        self.assertEqual(self.calls.read_text(), "ran\nran\n")
        self.assertEqual(self.git("rev-parse", "HEAD", repo=self.config["workspace"]), second["candidate"])
        self.assertEqual(cache.read_text(), "warm cache")

    def test_owner_and_initialization_are_explicit_and_immutable(self):
        self.init()
        with self.queue() as queue:
            with self.assertRaisesRegex(Q.Refusal, "owner"):
                queue.freeze("inferred-from-branch")
            with self.assertRaisesRegex(Q.Refusal, "different immutable"):
                queue.init(dict(self.config, qa_owner="replacement"))
        bad = dict(self.config, max_wip=0)
        with self.queue() as queue:
            with self.assertRaisesRegex(Q.Refusal, "max_wip"):
                queue.init(bad)

    def test_post_run_settlement_cas_failure_never_repeats_qa(self):
        self.prepare()
        original = Q.Queue.save
        def reject_terminal(queue, *args, **kwargs):
            if queue.batch()["status"] in ("passed", "failed"):
                raise Q.Refusal("simulated settlement CAS failure")
            return original(queue, *args, **kwargs)
        with mock.patch.object(Q.Queue, "save", reject_terminal):
            with self.assertRaisesRegex(Q.Refusal, "uncertain"):
                self.run_qa()
        with self.assertRaisesRegex(Q.Refusal, "running"):
            self.run_qa()
        with self.queue() as queue:
            with self.assertRaisesRegex(Q.Refusal, "no committed terminal"):
                queue.recover(self.config["qa_owner"])
        self.assertEqual(self.calls.read_text(), "ran\n")

    def test_new_ancestor_cargo_config_invalidates_receipt(self):
        self.prepare()
        self.run_qa()
        cargo = self.root / ".cargo"
        cargo.mkdir()
        (cargo / "config.toml").write_text('[build]\njobs = 99\n')
        with self.queue() as queue:
            with self.assertRaisesRegex(Q.Refusal, "modified inputs"):
                queue.eligible()

    def test_modified_environment_and_executable_invalidate_receipt(self):
        self.prepare()
        self.run_qa()
        with self.queue() as queue:
            queue.state["config"]["environment"]["NEW_VARIABLE"] = "changed"
            with self.assertRaisesRegex(Q.Refusal, "modified inputs"):
                queue.eligible()
        self.runner.write_text(self.runner.read_text() + "\n# changed binary\n")
        with self.queue() as queue:
            with self.assertRaisesRegex(Q.Refusal, "modified inputs"):
                queue.eligible()

    def test_input_modified_during_qa_remains_ineligible_and_preserved(self):
        self.qa.write_text(self.qa.read_text() + "\n")
        self.qa.write_text(self.qa.read_text().replace('print("fake QA completed")',
                          'Path("Cargo.lock").write_text("changed by QA")'))
        self.prepare()
        with self.assertRaisesRegex(Q.Refusal, "dirty input"):
            self.run_qa()
        self.assertEqual((Path(self.config["workspace"]) / "Cargo.lock").read_text(), "changed by QA")
        with self.assertRaisesRegex(Q.Refusal, "running"):
            self.run_qa()

    def test_exhausted_pilot_cannot_admit_more_after_retirement(self):
        env = dict(self.config["environment"], QA_EXIT="7")
        self.prepare(max_batches=1, environment=env)
        self.run_qa()
        with self.queue() as queue:
            queue.retire(self.config["qa_owner"], "failed batch repair")
        with self.assertRaisesRegex(Q.Refusal, "budget exhausted"):
            self.intake(self.source())
        with self.queue() as queue:
            with self.assertRaisesRegex(Q.Refusal, "budget exhausted"):
                queue.freeze(self.config["qa_owner"])

    def test_ignored_input_is_dirty_and_preserved(self):
        self.init()
        request = self.source()
        source = Path(request["source_path"])
        (source / ".gitignore").write_text("hidden\n")
        self.git("add", ".gitignore", repo=source)
        self.git("commit", "-qm", "ignore", repo=source)
        request["source"] = self.git("rev-parse", "HEAD", repo=source)
        (source / "hidden").write_text("input influencing build")
        with self.assertRaisesRegex(Q.Refusal, "dirty input"):
            self.intake(request)
        self.assertEqual((source / "hidden").read_text(), "input influencing build")

    def test_missing_runner_refuses_without_launch_and_can_retry_setup(self):
        self.prepare()
        self.runner.rename(self.bin / "saved-runner")
        # Keep only the fixture directory on PATH to avoid any host systemd-run.
        with self.queue() as queue:
            queue.state["config"]["environment"]["PATH"] = str(self.bin)
            with self.assertRaisesRegex(Q.Refusal, "unavailable"):
                queue.run(self.config["qa_owner"])
        with self.queue() as queue:
            self.assertEqual(queue.batch()["status"], "frozen")
        (self.bin / "saved-runner").rename(self.runner)
        self.assertTrue(self.run_qa()["passed"])

    def test_stale_base_before_freeze_is_rejected(self):
        self.init()
        one, two = self.source(), self.source()
        self.intake(one)
        self.intake(two)
        self.git("update-ref", self.base_ref, one["source"], self.base)
        with self.assertRaisesRegex(Q.Refusal, "stale accepted base"):
            self.frozen()

    def test_toolchain_failure_does_not_run_qa(self):
        self.prepare(toolchain_command=[sys.executable, "-c", "raise SystemExit(8)"])
        receipt = self.run_qa()
        self.assertEqual(receipt["terminal"]["toolchain_exit"], 8)
        self.assertFalse(receipt["passed"])
        self.assertFalse(self.calls.exists())

    def test_worker_success_without_terminal_evidence_is_failure(self):
        self.runner.write_text(f"#!{sys.executable}\nraise SystemExit(0)\n")
        self.prepare()
        receipt = self.run_qa()
        self.assertFalse(receipt["passed"])
        self.assertEqual(receipt["terminal"], None)
        self.assertFalse(self.calls.exists())

    def test_custody_marker_tampering_invalidates_success(self):
        self.prepare()
        self.run_qa()
        with self.queue() as queue:
            Q.atomic_json(queue.directory / "workspace.json", {"queue": "other", "workspace": self.config["workspace"]})
            with self.assertRaisesRegex(Q.Refusal, "custody changed"):
                queue.eligible()

    def test_prior_terminal_artifacts_are_not_adopted_as_new_execution(self):
        self.prepare()
        with self.queue() as queue:
            directory = queue.directory / queue.batch()["id"]
            directory.mkdir()
            Q.atomic_json(directory / "terminal.json", {"exit": 0})
        with self.assertRaisesRegex(Q.Refusal, "prior runner artifacts"):
            self.run_qa()
        self.assertFalse(self.calls.exists())

    def test_cli_failed_qa_exit_is_nonzero_and_names_owners(self):
        self.prepare(environment=dict(self.config["environment"], QA_EXIT="7"))
        result = subprocess.run([sys.executable, str(SCRIPT), "--repo", str(self.repo),
                                 "run", "--owner", self.config["qa_owner"]],
                                capture_output=True, text=True)
        self.assertEqual(result.returncode, 1, result.stderr)
        self.assertEqual(len(json.loads(result.stdout)["repair_owners"]), 2)

    def small_limits(self):
        return dict(probe_seconds=1, qa_seconds=1, runner_seconds=6,
                    probe_bytes=1024, qa_bytes=4096, log_bytes=8192)

    def assert_bounded_failure(self, receipt):
        self.assertFalse(receipt["passed"])
        self.assertTrue(receipt["scope_settlement"]["confirmed_inactive"])
        self.assertEqual(len(receipt["repair_owners"]), 2)
        with self.queue() as queue:
            directory = queue.directory / queue.batch()["id"]
            self.assertLessEqual((directory / "qa.log").stat().st_size,
                                 self.config["limits"]["log_bytes"])
            self.assertLessEqual((directory / "terminal.json").stat().st_size, Q.TERMINAL_BYTES)
            self.assertFalse(queue.eligible()["eligible_for_reviewed_admission"])
        self.assertEqual(self.run_qa(), receipt)
        self.assertEqual(len(self.runner_calls.read_text().splitlines()), 1)

    def test_hanging_qa_is_killed_reaped_and_never_repeated(self):
        self.qa.write_text("import os, time\nfrom pathlib import Path\n"
                           "Path(os.environ['QA_CALLS']).write_text(str(os.getpid()))\n"
                           "time.sleep(60)\n")
        self.prepare(limits=self.small_limits())
        started = time.monotonic()
        receipt = self.run_qa()
        self.assertLess(time.monotonic() - started, 5)
        self.assertEqual(receipt["terminal"]["qa_execution"]["limit"], "runtime_limit")
        self.assertTrue(receipt["terminal"]["qa_execution"]["reaped"])
        with self.assertRaises(ProcessLookupError):
            os.kill(int(self.calls.read_text()), 0)
        self.assert_bounded_failure(receipt)

    def test_hanging_probe_is_killed_without_starting_qa(self):
        probe = [sys.executable, "-c", "import time; time.sleep(60)"]
        self.prepare(limits=self.small_limits(), toolchain_command=probe)
        started = time.monotonic()
        receipt = self.run_qa()
        self.assertLess(time.monotonic() - started, 5)
        self.assertEqual(receipt["terminal"]["probe_execution"]["limit"], "runtime_limit")
        self.assertTrue(receipt["terminal"]["probe_execution"]["reaped"])
        self.assertEqual(receipt["terminal"]["qa_execution"], None)
        self.assertFalse(self.calls.exists())
        self.assert_bounded_failure(receipt)

    def test_noisy_qa_combined_stdout_stderr_are_capped(self):
        self.qa.write_text("import os\nwhile True:\n os.write(1, b'x' * 1000); os.write(2, b'y' * 1000)\n")
        self.prepare(limits=self.small_limits())
        receipt = self.run_qa()
        execution = receipt["terminal"]["qa_execution"]
        self.assertEqual(execution["limit"], "output_limit")
        self.assertEqual(execution["kept_bytes"], 4096)
        self.assert_bounded_failure(receipt)

    def test_noisy_probe_evidence_is_bounded_and_qa_not_started(self):
        probe = [sys.executable, "-c", "import os\nwhile True: os.write(2, b'x' * 1000)"]
        self.prepare(limits=self.small_limits(), toolchain_command=probe)
        receipt = self.run_qa()
        self.assertEqual(receipt["terminal"]["probe_execution"]["limit"], "output_limit")
        self.assertEqual(len(receipt["terminal"]["toolchain"]["stderr"]), 1024)
        self.assertFalse(self.calls.exists())
        self.assert_bounded_failure(receipt)

    def test_outer_runner_deadline_kills_its_group_and_stops_scope(self):
        # Escaped child models a scope member outside the launcher's process group.
        self.runner.write_text(f'''#!{sys.executable}
import os, subprocess, time
from pathlib import Path
child = subprocess.Popen([{sys.executable!r}, "-c", "import time; time.sleep(60)"], start_new_session=True)
Path(os.environ["HOME"]).parent.joinpath("scope-active").write_text(str(child.pid))
time.sleep(60)
''')
        selected = self.small_limits()
        selected["runner_seconds"] = 1
        self.prepare(limits=selected)
        receipt = self.run_qa()
        self.assertEqual(receipt["runner_execution"]["limit"], "runtime_limit")
        self.assertTrue(receipt["scope_settlement"]["confirmed_inactive"])
        self.assertTrue(receipt["scope_settlement"]["needed_stop"])
        self.assertFalse(receipt["passed"])
        self.assertEqual(self.run_qa(), receipt)
        self.assertFalse((self.root / "scope-active").exists())
        pid = int((self.root / "scope-stopped").read_text())
        stat = Path(f"/proc/{pid}/stat")
        if stat.exists():
            self.assertEqual(stat.read_text().split(") ", 1)[1].split()[0], "Z")

    def test_outer_noisy_runner_log_cannot_fill_disk(self):
        self.runner.write_text(f"#!{sys.executable}\nimport os\nwhile True: os.write(2, b'x' * 1000)\n")
        self.prepare(limits=self.small_limits())
        receipt = self.run_qa()
        self.assertEqual(receipt["runner_execution"]["limit"], "output_limit")
        self.assertFalse(receipt["passed"])
        with self.queue() as queue:
            self.assertEqual((queue.directory / queue.batch()["id"] / "qa.log").stat().st_size, 8192)
            self.assertFalse(queue.eligible()["eligible_for_reviewed_admission"])
        self.assertEqual(self.run_qa(), receipt)

    def test_unconfirmed_scope_shutdown_preserves_evidence_and_running_custody(self):
        self.prepare(limits=self.small_limits(),
                     environment=dict(self.config["environment"], FAIL_SCOPE_STOP="1"))
        (self.root / "scope-active").write_text("0")
        with self.assertRaisesRegex(Q.Refusal, "termination unconfirmed"):
            self.run_qa()
        with self.queue() as queue:
            batch = queue.batch()
            self.assertEqual(batch["status"], "running")
            self.assertFalse(batch["receipt"]["passed"])
            self.assertEqual(len(batch["receipt"]["repair_owners"]), 2)
            self.assertEqual(len(batch["receipt"]["scope_settlement"]["calls"]), 3)
            with self.assertRaisesRegex(Q.Refusal, "no terminal receipt"):
                queue.eligible()
            with self.assertRaisesRegex(Q.Refusal, "terminal failure"):
                queue.retire(self.config["qa_owner"], "cannot release active scope")
        with self.assertRaisesRegex(Q.Refusal, "no automatic retry"):
            self.run_qa()
        self.assertEqual(self.calls.read_text(), "ran\n")

    def test_exited_qa_parent_cannot_leave_a_child_holding_output_pipes(self):
        self.qa.write_text("import os, subprocess, sys\nfrom pathlib import Path\n"
                           "child = subprocess.Popen([sys.executable, '-c', 'import time; time.sleep(60)'])\n"
                           "Path(os.environ['QA_CALLS']).write_text(str(child.pid))\n")
        self.prepare(limits=self.small_limits())
        receipt = self.run_qa()
        self.assertEqual(receipt["terminal"]["qa_execution"]["exit"], 0)
        self.assertEqual(receipt["terminal"]["qa_execution"]["limit"], "runtime_limit")
        pid = int(self.calls.read_text())
        stat = Path(f"/proc/{pid}/stat")
        if stat.exists():
            self.assertEqual(stat.read_text().split(") ", 1)[1].split()[0], "Z")
        self.assert_bounded_failure(receipt)

    def test_hung_scope_control_is_bounded_and_never_confirms_shutdown(self):
        self.control.write_text(f"#!{sys.executable}\nimport time\ntime.sleep(60)\n")
        self.prepare(limits=self.small_limits())
        started = time.monotonic()
        with mock.patch.object(Q, "CONTROL_SECONDS", 1):
            with self.assertRaisesRegex(Q.Refusal, "termination unconfirmed"):
                self.run_qa()
        self.assertLess(time.monotonic() - started, 6)
        with self.queue() as queue:
            scope = queue.batch()["receipt"]["scope_settlement"]
            self.assertFalse(scope["confirmed_inactive"])
            self.assertEqual([c["result"]["limit"] for c in scope["calls"]], ["runtime_limit"] * 3)
            self.assertTrue(all(c["result"]["reaped"] for c in scope["calls"]))
        with self.assertRaisesRegex(Q.Refusal, "no automatic retry"):
            self.run_qa()

    def test_exact_output_ceiling_can_succeed(self):
        self.qa.write_text("import os\nos.write(1, b'x' * 4096)\n")
        self.prepare(limits=self.small_limits())
        receipt = self.run_qa()
        self.assertTrue(receipt["passed"])
        self.assertEqual(receipt["terminal"]["qa_execution"]["kept_bytes"], 4096)
        self.assertEqual(receipt["terminal"]["qa_execution"]["limit"], None)

    def test_successful_command_with_active_scope_cannot_promote(self):
        self.prepare(limits=self.small_limits())
        (self.root / "scope-active").write_text("0")
        receipt = self.run_qa()
        self.assertEqual(receipt["runner_exit"], 0)
        self.assertEqual(receipt["terminal"]["exit"], 0)
        self.assertTrue(receipt["scope_settlement"]["needed_stop"])
        self.assert_bounded_failure(receipt)

    def test_limits_are_validated_and_bound_into_receipts(self):
        for invalid in ({"qa_seconds": 0}, {"probe_bytes": -1}, {"runner_seconds": True},
                        {"log_bytes": Q.LIMITS["log_bytes"] + 1}, {"unknown": 1}):
            with self.subTest(invalid=invalid), self.queue() as queue:
                with self.assertRaises(Q.Refusal):
                    queue.init(dict(self.config, limits=invalid))
        self.prepare(limits=self.small_limits())
        receipt = self.run_qa()
        self.assertEqual(receipt["inputs"]["limits"], self.config["limits"])
        with self.queue() as queue:
            queue.state["config"]["limits"]["qa_seconds"] = 2
            with self.assertRaisesRegex(Q.Refusal, "modified inputs"):
                queue.eligible()

    def assert_memory_receipt(self, receipt, high_gib, max_gib):
        expected = {"MemoryHigh": high_gib * 1024 ** 3, "MemoryMax": max_gib * 1024 ** 3,
                    "MemorySwapMax": 0, "CPUWeight": 50}
        self.assertTrue(receipt["passed"])
        self.assertEqual(receipt["inputs"]["resources"], expected)
        self.assertEqual(receipt["terminal"]["resources"], expected)
        self.assertEqual(receipt["terminal"]["runner_argv"], receipt["runner_argv"])
        observed = ast.literal_eval(self.runner_calls.read_text().splitlines()[0])
        self.assertEqual(observed, receipt["runner_argv"][1:])
        for key, value in expected.items():
            self.assertEqual(observed.count(f"--property={key}={value}"), 1)
        with self.queue() as queue:
            self.assertEqual(queue.batch()["job"]["resources"], expected)
            self.assertEqual(receipt["terminal"]["job_sha256"], Q.digest(Q.encoded(queue.batch()["job"])))
            self.assertTrue(queue.eligible()["eligible_for_reviewed_admission"])

    def test_default_memory_6_8_is_bound_to_actual_argv_and_terminal(self):
        self.prepare()
        self.assert_memory_receipt(self.run_qa(), 6, 8)

    def test_measured_memory_8_10_is_bound_to_actual_argv_and_terminal(self):
        self.prepare(memory={"high_gib": 8, "max_gib": 10})
        receipt = self.run_qa()
        self.assert_memory_receipt(receipt, 8, 10)
        self.assertEqual(self.run_qa(), receipt)
        self.assertEqual(len(self.runner_calls.read_text().splitlines()), 1)
        with self.queue() as queue:
            with self.assertRaisesRegex(Q.Refusal, "different immutable config"):
                queue.init(dict(self.config, memory={"high_gib": 6, "max_gib": 8}))

    def test_memory_rejects_invalid_values_before_initialization(self):
        invalid = [None, [], "8G", {"unknown": 1}]
        for field in ("high_gib", "max_gib"):
            for value in (True, False, 0, -1, 1.5, "8", "infinity", None,
                          float("inf"), float("nan"), [], {}):
                invalid.append({field: value})
        invalid.extend([{"high_gib": 9, "max_gib": 10}, {"max_gib": 11},
                        {"high_gib": 8, "max_gib": 7}, {"max_gib": 5}])
        for memory in invalid:
            with self.subTest(memory=memory), self.queue() as queue:
                with self.assertRaisesRegex(Q.Refusal, "memory"):
                    queue.init(dict(self.config, memory=memory))
                self.assertEqual(queue.state, None)
                self.assertEqual(queue.sha, None)

    def test_memory_defaults_partial_values_and_equal_ordering(self):
        for selected, high, maximum in (({}, 6, 8), ({"max_gib": 10}, 6, 10),
                                        ({"high_gib": 8}, 8, 8),
                                        ({"high_gib": 1, "max_gib": 1}, 1, 1)):
            with self.subTest(memory=selected):
                effective = Q.resources({"memory": selected})
                self.assertEqual((effective["MemoryHigh"], effective["MemoryMax"]),
                                 (high * 1024 ** 3, maximum * 1024 ** 3))

    def test_changed_memory_inputs_or_terminal_receipt_fail_closed(self):
        self.prepare(memory={"high_gib": 8, "max_gib": 10})
        self.run_qa()
        with self.queue() as queue:
            queue.state["config"]["memory"]["high_gib"] = 7
            with self.assertRaisesRegex(Q.Refusal, "modified inputs"):
                queue.eligible()
        with self.queue() as queue:
            path = queue.directory / queue.batch()["id"] / "terminal.json"
            terminal = Q.terminal_json(path)
            terminal["resources"]["MemoryMax"] = 8 * 1024 ** 3
            Q.atomic_json(path, terminal)
            with self.assertRaisesRegex(Q.Refusal, "tampered/stale receipt"):
                queue.eligible()

    def test_cli_status_and_help(self):
        self.init()
        result = subprocess.run([sys.executable, str(SCRIPT), "--repo", str(self.repo), "status"],
                                capture_output=True, text=True)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(json.loads(result.stdout)["readiness"]["count"], 0)


if __name__ == "__main__":
    unittest.main()
