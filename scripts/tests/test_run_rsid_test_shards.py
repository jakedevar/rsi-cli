"""Exercise the real runner's artifact locks with deterministic Cargo stand-ins."""

import os
from pathlib import Path
import shutil
import subprocess
import tempfile
import time
import unittest


RUNNER = Path(__file__).resolve().parents[1] / "run-rsid-test-shards.sh"
SHARDS = [f"store-{i:02}" for i in range(1, 17)]

FAKE_CARGO = r'''#!/usr/bin/env python3
import json
import os
from pathlib import Path
import sys
import time

args = sys.argv[1:]
target = Path(os.environ.get("CARGO_TARGET_DIR", os.environ["FAKE_TARGET_DIRECTORY"]))
control = Path(os.environ["FAKE_CONTROL_DIRECTORY"])
name = os.environ["FAKE_RUN_NAME"]

def wait_for(path):
    deadline = time.monotonic() + 15
    while not path.exists():
        if time.monotonic() >= deadline:
            raise RuntimeError(f"timed out waiting for {path}")
        time.sleep(0.01)

if args[0] == "metadata":
    (control / f"metadata-{name}").touch()
    print(json.dumps({"target_directory": str(target)}))
elif args[:2] == ["nextest", "list"]:
    deps = target / "debug/deps"
    deps.mkdir(parents=True, exist_ok=True)
    binary = deps / f"rsid-{name}"
    binary.write_text("#!/bin/sh\nexit 0\n")
    binary.chmod(0o755)
    (control / f"listed-{name}").touch()
    if os.environ.get("FAKE_HOLD_LIST"):
        wait_for(control / f"release-{name}")
    print(json.dumps({"binary": str(binary)}))
elif args[:2] == ["nextest", "run"]:
    # Cargo rebuilds a harness after an earlier shard clean in fast/full mode.
    binary = target / "debug/deps" / f"rsid-{name}"
    if not binary.exists():
        binary.write_text("#!/bin/sh\nexit 0\n")
        binary.chmod(0o755)
    (control / f"running-{name}").touch()
    (control / f"env-{name}").write_text(
        "".join(f"{key}\n" for key in sorted(os.environ) if key.startswith("RSI_"))
    )
    if os.environ.get("FAKE_HOLD_RUN"):
        wait_for(control / f"release-{name}")
    # Exec the feature-hashed harness after the peer has attempted cleanup.
    subprocess_status = os.spawnv(os.P_WAIT, str(target / "debug/deps" / f"rsid-{name}"), [name])
    sys.exit(subprocess_status or int(os.environ.get("FAKE_TEST_STATUS", "0")))
elif args[0] == "clean":
    (control / f"cleaning-{name}").touch()
    with (control / f"cleans-{name}").open("a") as log:
        log.write("clean\n")
    if os.environ.get("FAKE_HOLD_CLEAN"):
        wait_for(control / f"release-clean-{name}")
    for artifact in (target / "debug/deps").glob("rsid-*"):
        artifact.unlink()
    print("Removed fake rsid artifacts")
elif args[0] == "test" and "--doc" in args:
    pass
else:
    raise RuntimeError(f"unexpected Cargo args: {args}")
'''


@unittest.skipUnless(shutil.which("flock"), "requires flock")
class ArtifactLockTests(unittest.TestCase):
    def setUp(self):
        scratch = tempfile.TemporaryDirectory()
        self.addCleanup(scratch.cleanup)
        self.root = Path(scratch.name)
        self.target = self.root / "shared target"
        self.control = self.root / "control"
        self.control.mkdir()
        self.bin = self.root / "bin"
        self.bin.mkdir()
        cargo = self.bin / "cargo"
        cargo.write_text(FAKE_CARGO)
        cargo.chmod(0o755)
        self.runners = []
        # Cleanup children before deleting their temporary worktrees.
        self.addCleanup(self.stop_runners)

    def stop_runners(self):
        for process in self.runners:
            if process.poll() is None:
                process.kill()
            process.communicate(timeout=20)

    def start(self, name, *, mode="shard", keep=False, dry_run=False, env_target=False, extra_env=None, **flags):
        # Separate worktrees sharing metadata's resolved (e.g. global-config)
        # target directory, rather than accidentally locking each repo/target.
        repo = self.root / name
        scripts = repo / "scripts"
        scripts.mkdir(parents=True)
        shutil.copyfile(RUNNER, scripts / RUNNER.name)
        packages = chr(10).join(f"{shard} rsid rsid-store" for shard in SHARDS)
        (scripts / "check-rsid-test-shards.py").write_text(
            "import sys\nif '--list-shard-packages' in sys.argv:\n"
            f"    print({packages!r})\n"
            "elif '--list-shards' in sys.argv:\n"
            f"    print({chr(10).join(SHARDS)!r})\n"
        )
        integrations = repo / "crates/rsid/tests"
        integrations.mkdir(parents=True)
        (integrations / "fixture.rs").touch()
        env = os.environ.copy()
        env.pop("CARGO_TARGET_DIR", None)
        env.update(
            PATH=f"{self.bin}:{env['PATH']}",
            FAKE_TARGET_DIRECTORY=str(self.target),
            FAKE_CONTROL_DIRECTORY=str(self.control),
            FAKE_RUN_NAME=name,
        )
        env.update({f"FAKE_{key.upper()}": str(value) for key, value in flags.items()})
        if env_target:
            env["CARGO_TARGET_DIR"] = str(self.target)
        if extra_env:
            env.update(extra_env)
        command = ["bash", str(scripts / RUNNER.name), mode]
        if mode == "shard":
            command.append("store-01")
        command.extend(["--jobs", "4"])
        if keep:
            command.append("--keep-rsid-artifacts")
        if dry_run:
            command.append("--dry-run")
        process = subprocess.Popen(command, env=env, stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True)
        self.runners.append(process)
        return process

    def wait_for(self, name):
        deadline = time.monotonic() + 10
        while not (self.control / name).exists():
            if time.monotonic() >= deadline:
                self.fail(f"timed out waiting for {name}")
            time.sleep(0.01)

    def finish(self, process, expected_status=0):
        output, _ = process.communicate(timeout=20)
        self.assertEqual(process.returncode, expected_status, output)
        return output

    def test_peer_binary_survives_cleanup_during_build_and_execution(self):
        for phase in ("list", "run"):
            with self.subTest(phase=phase):
                peer = self.start(f"peer-{phase}", **{f"hold_{phase}": 1})
                self.wait_for(f"{'listed' if phase == 'list' else 'running'}-peer-{phase}")
                first = self.start(f"first-{phase}")
                output = self.finish(first)
                self.assertIn("skipping clean-store-01: another rsid shard runner", output)
                binary = self.target / f"debug/deps/rsid-peer-{phase}"
                self.assertTrue(binary.is_file())
                self.assertEqual(subprocess.run([str(binary)], check=False).returncode, 0)
                (self.control / f"release-peer-{phase}").touch()
                output = self.finish(peer)
                self.assertIn("running clean-store-01", output)
                self.assertEqual(list((self.target / "debug/deps").glob("rsid-*")), [])

    def test_new_build_waits_until_exclusive_cleanup_finishes(self):
        first = self.start("first", hold_clean=1)
        self.wait_for("cleaning-first")
        second = self.start("second")
        self.wait_for("metadata-second")
        # Its list must wait until the first runner releases exclusive.
        time.sleep(0.2)
        self.assertIsNone(second.poll())
        self.assertFalse((self.control / "listed-second").exists())
        (self.control / "release-clean-first").touch()
        self.finish(first)
        self.finish(second)

    def test_alone_run_cleans_environment_selected_target(self):
        output = self.finish(self.start("alone", env_target=True))
        self.assertIn("running clean-store-01", output)
        self.assertTrue((self.control / "cleaning-alone").is_file())
        self.assertEqual(list((self.target / "debug/deps").glob("rsid-*")), [])

    def test_shards_run_with_only_build_and_lander_rsi_variables(self):
        # #1163: a worker shell inherits the daemon's RSI_* configuration and the
        # merge-queue lander does not; tests must see the same environment in both.
        inherited = {
            "RSI_CONTEXT_ROTATION_ENABLED": "true",
            "RSI_SANDBOX_BASE": str(self.root / "daemon-sandboxes"),
            "RSI_SESSION_ID": "00000000-0000-0000-0000-000000000000",
            "RSI_BUILD_SLOTS": "2",
            "RSI_JOB_CARGO_SLOT": "1",
            "RSI_LANDER_PREBUILD": "1",
        }
        self.finish(self.start("environment", extra_env=inherited))
        seen = set((self.control / "env-environment").read_text().split())
        environment = {**os.environ, **inherited}
        expected = {
            key
            for key in environment
            if key.startswith(("RSI_BUILD_", "RSI_JOB_", "RSI_LANDER_"))
        }
        self.assertEqual(seen, expected)

    def test_keep_artifacts_retains_binary(self):
        self.finish(self.start("keep", keep=True))
        self.assertTrue((self.target / "debug/deps/rsid-keep").is_file())

    def test_keep_run_protects_its_binary_from_an_active_peer(self):
        kept = self.start("kept", keep=True, hold_run=1)
        self.wait_for("running-kept")
        output = self.finish(self.start("peer"))
        self.assertIn("skipping clean-store-01", output)
        (self.control / "release-kept").touch()
        self.finish(kept)
        self.assertTrue((self.target / "debug/deps/rsid-kept").is_file())

    def test_fast_mode_cleans_between_shards_and_at_exit(self):
        output = self.finish(self.start("fast", mode="fast"))
        self.assertIn("running clean-store-01", output)
        self.assertIn("running clean-final", output)
        self.assertEqual((self.control / "cleans-fast").read_text().splitlines(), ["clean"] * 17)
        self.assertEqual(list((self.target / "debug/deps").glob("rsid-*")), [])

    def test_multi_shard_run_restores_shared_lock_after_skipped_clean(self):
        peer = self.start("held", hold_run=1)
        self.wait_for("running-held")
        output = self.finish(self.start("fast", mode="fast"))
        self.assertIn("skipping clean-store-01", output)
        self.assertIn("skipping clean-final", output)
        self.assertTrue((self.target / "debug/deps/rsid-held").is_file())
        (self.control / "release-held").touch()
        self.finish(peer)
        self.assertEqual(list((self.target / "debug/deps").glob("rsid-*")), [])

    def test_failed_tests_clean_and_preserve_exit_status(self):
        self.finish(self.start("failed", test_status=42), expected_status=42)
        self.assertTrue((self.control / "cleaning-failed").is_file())
        self.assertEqual(list((self.target / "debug/deps").glob("rsid-*")), [])

    def test_dry_run_does_not_invoke_cargo_or_create_lock(self):
        output = self.finish(self.start("dry", dry_run=True))
        self.assertIn("static plan:", output)
        self.assertEqual(list(self.control.iterdir()), [])
        self.assertFalse(self.target.exists())

    def test_a_shard_selects_every_package_that_declares_it(self):
        # #1021 S4: the store tests live in rsid-store, the rest in rsid; one
        # shard run lists, runs and cleans both.
        output = self.finish(self.start("pair", dry_run=True))
        self.assertIn("-p rsid -p rsid-store --lib --no-default-features --features test-shard-store-01", output)
        self.assertIn("cargo clean -p rsid -p rsid-store --profile test", output)


if __name__ == "__main__":
    unittest.main()
