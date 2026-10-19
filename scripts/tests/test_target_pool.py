"""scripts/target-pool on a tiny Cargo workspace (#1741).

The clobber test is the acceptance check: two sandboxes on different commits,
seeded from one pool, build concurrently; each gets its own commit's program
and the pool's files keep their bytes. The wrapper variants need a built
`rsi-build-rustc` (RSI_TEST_BUILD_RUSTC, or target/debug under the repo's
CARGO_TARGET_DIR); they are skipped when none is available.
"""

import argparse
import contextlib
import hashlib
import io
import json
import os
from pathlib import Path
import runpy
import shutil
import subprocess
import sys
import unittest
from unittest import mock

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
from scratch_git import scratch_git, scratch_tempdir  # noqa: E402

SCRIPT = Path(__file__).resolve().parents[1] / "target-pool"
REPO = Path(__file__).resolve().parents[2]
POOL = runpy.run_path(str(SCRIPT))
# run_path returns a copy; the functions look names up in this live dict.
LIVE = POOL["gc"].__globals__
# Tests that go through the admission check state the floors and the disk they see, so
# neither the host's free space nor the caller's environment decides the outcome (#2773).
ADMISSION_ENV = {"RSI_TARGET_POOL_MIN_FREE_GIB": "40", "RSI_TARGET_POOL_WORKER_FLOOR_GIB": "30"}
PLENTY_GIB = 1000.0

LIB_BUILD = """fn main() {
    let out = std::env::var("OUT_DIR").unwrap();
    let value = std::fs::read_to_string("gen.txt").unwrap();
    std::fs::write(format!("{out}/gen.rs"), format!("pub const GEN: &str = {:?};", value.trim())).unwrap();
    println!("cargo:rerun-if-changed=gen.txt");
    // A watched directory: Cargo compares the directory's own mtime.
    println!("cargo:rerun-if-changed=data");
}
"""
LIB_SRC = """include!(concat!(env!("OUT_DIR"), "/gen.rs"));
pub fn value() -> String { format!("{GEN}-{}", VALUE) }
pub const VALUE: &str = "VALUE_PLACEHOLDER";
"""
APP_SRC = """fn main() { println!("{} {}", lib1::value(), baked::DIR); }
"""
BAKED_SRC = """pub const DIR: &str = env!("CARGO_MANIFEST_DIR");
"""


def wrapper_binary():
    configured = os.environ.get("RSI_TEST_BUILD_RUSTC")
    candidates = [Path(configured)] if configured else []
    target = os.environ.get("CARGO_TARGET_DIR")
    candidates += [Path(target) / "debug" / "rsi-build-rustc"] if target else []
    candidates.append(REPO / "target" / "debug" / "rsi-build-rustc")
    stamped = os.environ.get("RUSTC_WRAPPER")
    if stamped:
        candidates.append(Path(stamped))
    for candidate in candidates:
        if candidate.is_file() and os.access(candidate, os.X_OK):
            probe = subprocess.run([str(candidate), "--capabilities"], capture_output=True,
                                   text=True)
            if "unlink-outputs" in probe.stdout.split():
                return candidate
    return None


def digest_tree(root: Path) -> dict:
    digests = {}
    for path in sorted(root.rglob("*")):
        if path.is_file() and not path.is_symlink() and path.name != ".cargo-lock":
            digests[str(path.relative_to(root))] = hashlib.sha256(path.read_bytes()).hexdigest()
    return digests


class TargetPool(unittest.TestCase):
    def setUp(self):
        self.temp = scratch_tempdir(prefix="target-pool-")
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.env = {name: value for name, value in os.environ.items()
                    if not name.startswith(("CARGO_", "RSI_", "SCCACHE_", "RUSTC_"))}
        self.env.update({"RSI_TARGET_POOL_ROOT": str(self.root / "pools"),
                         "RSI_CARGO_SLOT": str(self.root / "no-cargo-slot"),
                         # No test here depends on the host's free space (#2773): the
                         # admission check is off unless a test stubs the disk itself.
                         "RSI_TARGET_POOL_MIN_FREE_GIB": "0",
                         "CARGO_INCREMENTAL": "0",
                         "CARGO_PROFILE_DEV_DEBUG": "line-tables-only",
                         "CARGO_HOME": os.environ.get("CARGO_HOME",
                                                      str(Path.home() / ".cargo")),
                         "CARGO_NET_OFFLINE": "true"})
        self.origin = self.root / "origin"
        self.make_workspace()

    def git(self, cwd, *args):
        return scratch_git(cwd, *args, env=self.env).stdout.strip()

    def write(self, relative, text, root=None):
        path = (root or self.origin) / relative
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(text)

    def make_workspace(self):
        self.origin.mkdir()
        self.git(self.origin, "init", "-q", "-b", "main")
        self.git(self.origin, "config", "user.email", "t@example.invalid")
        self.git(self.origin, "config", "user.name", "t")
        self.write("Cargo.toml", '[workspace]\nmembers = ["lib1", "baked", "app", "stable"]\n'
                                 'resolver = "2"\n')
        self.write(".gitignore", "target/\n")
        for name, extra in (("lib1", 'build = "build.rs"\n'), ("baked", ""),
                            ("app", ""), ("stable", "")):
            deps = ('[dependencies]\nlib1 = { path = "../lib1" }\nbaked = { path = "../baked" }\n'
                    if name == "app" else "")
            self.write(f"{name}/Cargo.toml", f'[package]\nname = "{name}"\nversion = "0.1.0"\n'
                                             f'edition = "2021"\n{extra}{deps}')
        self.write("lib1/build.rs", LIB_BUILD)
        self.write("lib1/gen.txt", "gen\n")
        self.write("lib1/data/notes.txt", "data\n")
        self.write("lib1/src/lib.rs", LIB_SRC.replace("VALUE_PLACEHOLDER", "one"))
        self.write("baked/src/lib.rs", BAKED_SRC)
        self.write("stable/src/lib.rs", 'pub const VALUE: &str = "stable";\n')
        self.write("app/src/main.rs", APP_SRC)
        self.write("app/tests/it.rs", "#[test]\nfn t() { assert!(!lib1::value().is_empty()); }\n")
        self.git(self.origin, "add", "-A")
        self.git(self.origin, "commit", "-qm", "one")
        self.one = self.git(self.origin, "rev-parse", "HEAD")
        self.write("lib1/src/lib.rs", LIB_SRC.replace("VALUE_PLACEHOLDER", "two"))
        self.git(self.origin, "commit", "-qam", "two")
        self.two = self.git(self.origin, "rev-parse", "HEAD")

    def sandbox(self, name, sha):
        path = self.root / name
        self.git(self.origin, "worktree", "add", "-q", "--detach", str(path), sha)
        (path / "target").mkdir(mode=0o700)
        return path

    def pool(self, env=None):
        result = subprocess.run([sys.executable, str(SCRIPT), "build", "--repo", str(self.origin),
                                 "--sha", self.one, "--min-free-gib", "0"],
                                env=env or self.env, capture_output=True, text=True)
        self.assertEqual(result.returncode, 0, result.stderr[-3000:])
        markers = list((self.root / "pools").glob("*/*.json"))
        self.assertEqual(len(markers), 1)
        record = json.loads(markers[0].read_text())
        self.assertEqual(record["state"], "ready")
        return Path(record["target"])

    def seed(self, sandbox, env=None):
        result = subprocess.run([sys.executable, str(SCRIPT), "seed", "--sandbox", str(sandbox)],
                                env=env or self.env, capture_output=True, text=True)
        self.assertEqual(result.returncode, 0, result.stderr)
        return json.loads(result.stdout)

    def cargo(self, sandbox, *args, env=None):
        environment = dict(env or self.env, CARGO_TARGET_DIR=str(sandbox / "target"))
        return subprocess.Popen(["cargo", *args], cwd=sandbox, env=environment,
                                stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True)

    def finish(self, process):
        output, _ = process.communicate(timeout=600)
        return process.returncode, output

    def run_app(self, sandbox):
        return subprocess.run([str(sandbox / "target" / "debug" / "app")], capture_output=True,
                              text=True, check=True).stdout.strip()

    def test_same_commit_seed_is_fresh_and_relocates_baked_paths(self):
        pool_target = self.pool()
        sandbox = self.sandbox("s1", self.one)
        outcome = self.seed(sandbox)
        self.assertEqual(outcome["result"], "seeded")
        self.assertEqual(outcome["distance"], 0)
        code, output = self.finish(self.cargo(sandbox, "build"))
        self.assertEqual(code, 0, output)
        compiled = [line for line in output.splitlines() if "Compiling" in line]
        # Only the crate that bakes CARGO_MANIFEST_DIR (and its dependent) rebuild.
        self.assertEqual(sorted(line.split()[1] for line in compiled), ["app", "baked"])
        # The rebuilt program reports this sandbox's own directory.
        self.assertEqual(self.run_app(sandbox), f"gen-one {sandbox / 'baked'}")
        self.assertTrue(pool_target.is_dir())

    def build_descendant(self, sha, env=None, *options):
        result = subprocess.run([sys.executable, str(SCRIPT), "build", "--repo", str(self.origin),
                                 "--sha", sha, "--min-free-gib", "0", "--keep", "4", *options],
                                env=env or self.env, capture_output=True, text=True)
        self.assertEqual(result.returncode, 0, result.stderr[-3000:])
        directory = next((self.root / "pools").iterdir())
        record = json.loads(next(directory.glob(f"{sha}-*.json")).read_text())
        self.assertEqual(record["state"], "ready")
        return record

    def incremental_pool_build(self, env):
        ancestor = self.pool(env)
        before = digest_tree(ancestor)
        record = self.build_descendant(self.two, env)
        self.assertEqual(record["seed"]["pool_sha"], self.one)
        self.assertEqual(record["seed"]["distance"], 1)
        self.assertGreater(record["seed"]["linked"], 0)
        self.assertTrue(any(unit.startswith("baked-") for unit in record["seed"]["rebuilt_units"]))
        compiled = [line.split()[1] for line in Path(record["log_path"]).read_text().splitlines()
                    if "Compiling" in line]
        self.assertEqual(sorted(compiled), ["app", "baked", "lib1"])
        target = Path(record["target"])
        stable = next((ancestor / "debug/deps").glob("libstable-*.rlib"))
        self.assertEqual(stable.stat().st_ino,
                         (target / stable.relative_to(ancestor)).stat().st_ino)
        self.assertEqual(digest_tree(ancestor), before)
        code, output = self.finish(self.cargo(Path(record["worktree"]), "build", env=env))
        self.assertEqual(code, 0, output)
        self.assertEqual(self.run_app(Path(record["worktree"])),
                         f"gen-two {Path(record['worktree']) / 'baked'}")
        return target

    def test_pool_build_reuses_ancestor_and_preserves_its_artifacts(self):
        self.incremental_pool_build(self.env)

    def test_pool_build_reuses_shared_executables_with_wrapper(self):
        wrapper = wrapper_binary()
        if wrapper is None:
            self.skipTest("no rsi-build-rustc with unlink-outputs built")
        self.incremental_pool_build(dict(self.env, RUSTC_WRAPPER=str(wrapper)))

    def test_pool_build_chooses_nearest_ready_ancestor(self):
        self.pool()
        second = self.build_descendant(self.two)
        self.write("notes.txt", "third commit\n")
        self.git(self.origin, "add", "notes.txt")
        self.git(self.origin, "commit", "-qm", "three")
        third = self.build_descendant(self.git(self.origin, "rev-parse", "HEAD"))
        self.assertEqual(third["seed"]["pool"], second["name"])
        self.assertEqual(third["seed"]["distance"], 1)
        compiled = [line.split()[1] for line in Path(third["log_path"]).read_text().splitlines()
                    if "Compiling" in line]
        self.assertEqual(sorted(compiled), ["app", "baked"])

    def test_pool_build_is_cold_without_a_compatible_nearby_ancestor(self):
        self.pool()
        for environment, options in ((self.env, ("--max-distance", "0")),
                                     (dict(self.env, CARGO_PROFILE_DEV_DEBUG="full"), ())):
            with self.subTest(options=options):
                # The two builds have distinct keys; each starts without a usable ancestor.
                result = subprocess.run([sys.executable, str(SCRIPT), "build", "--repo",
                                         str(self.origin), "--sha", self.two,
                                         "--min-free-gib", "0", "--keep", "4", *options],
                                        env=environment, capture_output=True, text=True)
                self.assertEqual(result.returncode, 0, result.stderr[-3000:])
                with mock.patch.dict(os.environ, environment, clear=True):
                    key = POOL["build_key"](self.origin)
                directory = next((self.root / "pools").iterdir())
                record = json.loads((directory / f"{self.two}-{key}.json").read_text())
                self.assertIsNone(record["seed"])
                compiled = [line.split()[1]
                            for line in Path(record["log_path"]).read_text().splitlines()
                            if "Compiling" in line]
                self.assertEqual(sorted(compiled), ["app", "baked", "lib1", "stable"])

    def test_pool_build_is_cold_when_ready_pool_is_a_descendant(self):
        self.build_descendant(self.two)
        record = self.build_descendant(self.one)
        self.assertIsNone(record["seed"])
        compiled = [line.split()[1] for line in Path(record["log_path"]).read_text().splitlines()
                    if "Compiling" in line]
        self.assertEqual(sorted(compiled), ["app", "baked", "lib1", "stable"])

    def test_key_mismatch_and_non_empty_target_are_not_seeded(self):
        self.pool()
        sandbox = self.sandbox("s1", self.one)
        other = dict(self.env, CARGO_PROFILE_DEV_DEBUG="full")
        self.assertEqual(self.seed(sandbox, env=other)["result"], "no pool")
        (sandbox / "target" / "debug").mkdir()
        self.assertEqual(self.seed(sandbox)["result"], "skipped: target is not empty")
        (sandbox / "Cargo.toml").unlink()
        self.assertEqual(self.seed(sandbox)["result"], "skipped: not a Cargo workspace")

    def test_seed_from_another_host_copies_the_newest_matching_pool(self):
        """#1884: a peer's pool arrives as tar|zstd and then seeds a sandbox."""
        remote_target = self.pool()
        remote_root = self.root / "pools"
        local_root = self.root / "peer-pools"
        shim = self.root / "fake-ssh"
        shim.write_text(f'#!/bin/sh\nshift\nexport RSI_TARGET_POOL_ROOT={remote_root}\n'
                        'exec sh -c "$1"\n')
        shim.chmod(0o755)
        peer = dict(self.env, RSI_TARGET_POOL_ROOT=str(local_root),
                    RSI_TARGET_POOL_SSH=str(shim))
        command = [sys.executable, str(SCRIPT), "seed-from", "peer", "--repo", str(self.origin),
                   "--remote-tool", f"{sys.executable} {SCRIPT}", "--min-free-gib", "0"]
        dry = subprocess.run([*command, "--dry-run"], env=peer, capture_output=True, text=True)
        self.assertEqual(dry.returncode, 0, dry.stderr)
        self.assertEqual(json.loads(dry.stdout)["result"], "dry run")
        self.assertFalse(local_root.exists() and any(local_root.glob("*/*.json")))
        result = subprocess.run(command, env=peer, capture_output=True, text=True)
        self.assertEqual(result.returncode, 0, result.stderr)
        outcome = json.loads(result.stdout)
        self.assertEqual(outcome["result"], "seeded")
        self.assertEqual(outcome["sha"], self.one)
        record = json.loads(next(local_root.glob("*/*.json")).read_text())
        self.assertEqual(record["state"], "ready")
        self.assertEqual(record["seeded_from"]["host"], "peer")
        self.assertEqual(digest_tree(Path(record["target"])), digest_tree(remote_target))
        again = subprocess.run(command, env=peer, capture_output=True, text=True)
        self.assertEqual(json.loads(again.stdout)["result"], "already present")
        sandbox = self.sandbox("s1", self.two)
        seeded = self.seed(sandbox, env=peer)
        self.assertEqual(seeded["result"], "seeded")
        self.assertEqual(seeded["pool_sha"], self.one)
        # A host that built differently offers nothing usable.
        other = dict(peer, CARGO_PROFILE_DEV_DEBUG="full")
        refused = subprocess.run(command, env=other, capture_output=True, text=True)
        self.assertEqual(refused.returncode, 2, refused.stderr)

    def test_gc_racing_a_seed_from_cannot_remove_the_half_copied_pool(self):
        """#1884/#1882: gc runs while the transfer is in flight, with a stale
        ready marker of the very pool being seeded in its plan; the seed's use
        lock makes gc skip it, the seed publishes a whole pool, and a later gc
        counts the seeded pool like any other."""
        remote_target = self.pool()
        remote = json.loads(next((self.root / "pools").glob("*/*.json")).read_text())
        name = remote["name"]
        local_root = self.root / "peer-pools"
        directory = local_root / next((self.root / "pools").glob("*")).name
        directory.mkdir(parents=True)
        stale = {"name": name, "sha": remote["sha"], "key": remote["key"], "state": "ready",
                 "target": str(directory / name / "target"), "built_at": 1}
        newer = {**stale, "name": "newer-pool", "target": str(directory / "newer" / "t"),
                 "built_at": 999}
        (directory / f"{name}.json").write_text(json.dumps(stale))
        (directory / "newer-pool.json").write_text(json.dumps(newer))
        raced = self.root / "gc-during-transfer.json"
        peer = dict(self.env, RSI_TARGET_POOL_ROOT=str(local_root))
        hook = self.root / "gc-hook"
        hook.write_text(f'#!/bin/sh\n{sys.executable} {SCRIPT} gc --keep 1 --json > {raced}\n')
        hook.chmod(0o755)
        shim = self.root / "fake-ssh"
        shim.write_text(f'#!/bin/sh\nshift\ncase "$1" in "tar -C"*) RSI_TARGET_POOL_ROOT='
                        f'{local_root} {hook};; esac\nexport RSI_TARGET_POOL_ROOT='
                        f'{self.root / "pools"}\nexec sh -c "$1"\n')
        shim.chmod(0o755)
        peer["RSI_TARGET_POOL_SSH"] = str(shim)
        result = subprocess.run(
            [sys.executable, str(SCRIPT), "seed-from", "peer", "--repo", str(self.origin),
             "--remote-tool", f"{sys.executable} {SCRIPT}", "--min-free-gib", "0",
             "--keep", "5"], env=peer, capture_output=True, text=True)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(json.loads(result.stdout)["result"], "seeded")
        during = json.loads(raced.read_text())
        self.assertEqual(during["skipped_in_use"], [name])
        self.assertEqual(during["removed"], [])
        record = json.loads((directory / f"{name}.json").read_text())
        self.assertEqual(record["seeded_from"]["host"], "peer")
        self.assertEqual(digest_tree(Path(record["target"])), digest_tree(remote_target))
        self.assertGreater(record["size_bytes"], 0)
        self.assertEqual(list(directory.glob("*.tmp")) + list(directory.glob(".seed-*")), [])
        later = subprocess.run([sys.executable, str(SCRIPT), "gc", "--keep", "1", "--json"],
                               env=peer, capture_output=True, text=True)
        self.assertEqual(json.loads(later.stdout),
                         {"removed": ["newer-pool"], "skipped_in_use": []})
        self.assertTrue(Path(record["target"]).is_dir())

    def test_pool_floor_is_capped_at_a_quarter_of_the_disk(self):
        total = shutil.disk_usage(self.root).total / 2**30
        floor = POOL["scaled_floor_gib"](self.root, 10 * total)
        self.assertAlmostEqual(floor, total / 4, places=3)
        self.assertEqual(POOL["scaled_floor_gib"](self.root, 0), 0)

    def test_seed_keys_and_requests_the_sandbox_head_despite_inherited_git_context(self):
        sandbox = self.sandbox("s1", self.one)
        # The shared checkout is at `two`; the worker's pinned HEAD is `one`.
        environment = dict(self.env, GIT_DIR=str(self.origin / ".git"),
                           GIT_WORK_TREE=str(self.origin),
                           GIT_INDEX_FILE=str(self.origin / ".git/index"))
        self.assertEqual(self.seed(sandbox, environment)["head"], self.one)
        args = argparse.Namespace(sandbox=str(sandbox), target=None,
                                  build_missing=True, max_distance=200)
        with mock.patch.dict(os.environ, environment, clear=True), \
                mock.patch.dict(POOL["cmd_seed"].__globals__,
                                request_build=mock.Mock(return_value="started")):
            output = io.StringIO()
            with contextlib.redirect_stdout(output):
                self.assertEqual(POOL["cmd_seed"](args), 0)
            POOL["cmd_seed"].__globals__["request_build"].assert_called_once_with(
                sandbox, self.one)
            self.assertEqual(json.loads(output.getvalue())["head"], self.one)

    def test_pool_unit_drops_launch_scratch_and_keeps_cache_settings(self):
        environment = dict(self.env, TMPDIR="/sandbox/" + "long/" * 30,
                           TMP="/sandbox/temp", TEMP="/sandbox/temp",
                           SCCACHE_SERVER_UDS="/var/tmp/cache.sock", SCCACHE_CLIENT_SIDE="1",
                           RSI_CARGO_SLOT_HELD="1", **ADMISSION_ENV)
        with mock.patch.dict(os.environ, environment, clear=True), \
                mock.patch.dict(LIVE, free_gib=lambda path: PLENTY_GIB), \
                mock.patch.object(sys, "platform", "linux"), \
                mock.patch.object(shutil, "which", return_value="/usr/bin/systemd-run"), \
                mock.patch.object(subprocess, "run") as run:
            run.return_value = subprocess.CompletedProcess([], 0, "", "")
            self.assertTrue(POOL["request_build"](self.origin, self.one).startswith("started "))
        command = run.call_args.args[0]
        options = command[:command.index("--")]
        # Only options systemd-run accepts (it has no --unsetenv), and the launch
        # scratch, token and slot marker are never passed into the unit.
        allowed = ("--user", "--collect", "--quiet", "--unit=", "--property=", "--setenv=",
                   "--slice=")
        self.assertEqual([arg for arg in options[1:] if not arg.startswith(allowed)], [])
        passed = {arg.split("=", 2)[1] for arg in options if arg.startswith("--setenv=")}
        self.assertEqual(passed & {"TMPDIR", "TMP", "TEMP", "RSI_CARGO_SLOT_HELD",
                                   "RSI_SESSION_TOKEN"}, set())
        self.assertIn("--setenv=SCCACHE_SERVER_UDS=/var/tmp/cache.sock", command)
        self.assertIn("--setenv=SCCACHE_CLIENT_SIDE=1", command)
        self.assertEqual(command[-4:], ["--repo", str(self.origin), "--sha", self.one])
        # The detached build runs under the hub build budget, with a CPU cap and a
        # bounded runtime (#2707, #2715).
        self.assertIn("--slice=rsi-build.slice", options)
        self.assertIn("--property=CPUQuota=400%", options)
        self.assertIn("--property=RuntimeMaxSec=7200", options)

    def test_pool_unit_limits_follow_the_slice_budget_and_settings(self):
        def shown(text):
            return mock.Mock(return_value=subprocess.CompletedProcess([], 0, text, ""))

        environment = {"RSI_TARGET_POOL_CPU_PERCENT": "800", "RSI_TARGET_POOL_MAX_SEC": "900"}
        with mock.patch.dict(os.environ, environment, clear=True), \
                mock.patch.object(subprocess, "run", shown("2.5s\n")):
            # The slice's live quota (250% of one CPU) caps the configured 800%.
            self.assertEqual(POOL["pool_unit_limits"]("rsi-build.slice"), (250.0, 900.0))
        with mock.patch.dict(os.environ, {}, clear=True), \
                mock.patch.object(subprocess, "run", shown("infinity\n")):
            self.assertEqual(POOL["pool_unit_limits"]("rsi-build.slice"), (400.0, 7200.0))
        with mock.patch.dict(os.environ, {"RSI_TARGET_POOL_MAX_SEC": "junk"}, clear=True), \
                mock.patch.object(subprocess, "run", side_effect=OSError):
            self.assertEqual(POOL["pool_unit_limits"]("rsi-build.slice"), (400.0, 7200.0))

    def test_pool_unit_without_systemd_runs_niced(self):
        real = subprocess.Popen
        started = []

        def fake(command, *args, **kwargs):
            if command and command[0] in ("git", "rustc"):  # rustc: the admission's build key
                return real(command, *args, **kwargs)
            started.append(kwargs)
            return mock.Mock()

        with mock.patch.dict(os.environ, dict(self.env, **ADMISSION_ENV), clear=True), \
                mock.patch.dict(LIVE, free_gib=lambda path: PLENTY_GIB), \
                mock.patch.object(sys, "platform", "linux"), \
                mock.patch.object(shutil, "which", return_value=None), \
                mock.patch.object(subprocess, "Popen", side_effect=fake):
            self.assertEqual(POOL["request_build"](self.origin, self.one), "started")
        self.assertEqual(len(started), 1)
        self.assertTrue(callable(started[0]["preexec_fn"]))

    def test_pool_environment_starts_sccache_through_wrapper_with_long_pool_path(self):
        wrapper = wrapper_binary()
        cache = shutil.which("sccache")
        if wrapper is None or cache is None or os.name == "nt":
            self.skipTest("requires Unix, sccache and rsi-build-rustc")
        # Pin the actual startup path, not merely the configured server socket:
        # a missing server makes sccache bind TMPDIR/sccache*/sock first.
        long_pool = self.root / ("long-pool-" + "x" * 100)
        long_pool.mkdir()
        with POOL["pool_temporary_directory"](long_pool) as temporary:
            temporary = Path(temporary)
            environment = dict(self.env, TMPDIR=str(long_pool), TMP=str(long_pool),
                               TEMP=str(long_pool), RSI_BUILD_SCCACHE=cache,
                               RUSTC_WRAPPER=str(wrapper), SCCACHE_CLIENT_SIDE="1",
                               SCCACHE_SERVER_UDS=str(temporary / "cache.sock"),
                               SCCACHE_DIR=str(temporary / "cache"),
                               RSI_BUILD_SLOT_ROOT=str(temporary / "slots"))
            with mock.patch.dict(os.environ, environment, clear=True):
                unit_env = POOL["pool_build_environment"](long_pool / "target", temporary)
            self.assertLess(len(os.fsencode(unit_env["TMPDIR"])) + 24, 104)
            self.assertEqual(unit_env["TMP"], str(temporary))
            self.assertEqual(unit_env["TEMP"], str(temporary))
            try:
                result = subprocess.run([str(wrapper), "rustc", "-vV"], env=unit_env,
                                        capture_output=True, text=True, timeout=30)
                self.assertEqual(result.returncode, 0, result.stderr)
                self.assertIn("rustc ", result.stdout)
                self.pool(unit_env)
                sandbox = self.sandbox("s1", self.one)
                outcome = self.seed(sandbox, unit_env)
                self.assertEqual(outcome["result"], "seeded")
                self.assertEqual(outcome["head"], self.one)
                self.assertEqual(outcome["pool_sha"], self.one)
            finally:
                # Stop only this test's private cache server.
                subprocess.run([cache, "--stop-server"], env=unit_env,
                               capture_output=True, timeout=15)

    def test_failed_build_diagnostics_are_visible_in_status_and_next_seed(self):
        failing_slot = self.root / "failing-slot"
        failing_slot.write_text("#!/bin/sh\necho controlled-pool-build-failure\nexit 9\n")
        failing_slot.chmod(0o700)
        environment = dict(self.env, RSI_CARGO_SLOT=str(failing_slot))
        result = subprocess.run([sys.executable, str(SCRIPT), "build", "--repo", str(self.origin),
                                 "--sha", self.one, "--min-free-gib", "0"],
                                env=environment, capture_output=True, text=True)
        self.assertEqual(result.returncode, 9, result.stderr)
        status = subprocess.run([sys.executable, str(SCRIPT), "status", "--repo", str(self.origin)],
                                env=environment, capture_output=True, text=True, check=True)
        record = json.loads(status.stdout)
        self.assertEqual(record["state"], "failed")
        self.assertEqual(record["exit_code"], 9)
        self.assertIn("controlled-pool-build-failure", record["error_tail"])
        self.assertTrue(Path(record["log_path"]).is_file())
        sandbox = self.sandbox("s1", self.one)
        outcome = self.seed(sandbox, environment)
        self.assertEqual(outcome["build_failure"], record)
        self.assertEqual(outcome["result"], "no pool")

    def break_the_tests(self):
        self.write("app/tests/it.rs", "#[test]\nfn t() { this_does_not_compile(); }\n")
        self.git(self.origin, "commit", "-qam", "three: tests do not compile")
        return self.git(self.origin, "rev-parse", "HEAD")

    def test_a_tip_whose_tests_do_not_compile_still_gets_a_pool(self):
        self.pool()
        broken = self.break_the_tests()
        result = subprocess.run([sys.executable, str(SCRIPT), "build", "--repo", str(self.origin),
                                 "--sha", broken, "--min-free-gib", "0", "--keep", "4"],
                                env=self.env, capture_output=True, text=True)
        self.assertEqual(result.returncode, 0, result.stderr[-3000:])
        directory = next((self.root / "pools").iterdir())
        record = json.loads(next(directory.glob(f"{broken}-*.json")).read_text())
        self.assertEqual(record["state"], "ready")
        self.assertTrue(record["degraded"])
        self.assertEqual(record["scope"], "libraries")
        self.assertNotEqual(record["test_exit_code"], 0)
        self.assertIn("this_does_not_compile", record["test_error_tail"])
        self.assertGreater(record["size_bytes"], 0)
        # The libraries and binaries of that commit are in the pool.
        target = Path(record["target"])
        self.assertTrue(list((target / "debug/deps").glob("liblib1-*.rlib")))
        self.assertTrue((target / "debug" / "app").is_file())
        # A sandbox at the broken tip seeds from it, not from the older pool.
        sandbox = self.sandbox("s3", broken)
        outcome = self.seed(sandbox)
        self.assertEqual(outcome["result"], "seeded")
        self.assertEqual(outcome["pool_sha"], broken)
        self.assertEqual(outcome["distance"], 0)
        self.assertTrue(outcome["degraded"])
        summary = subprocess.run([sys.executable, str(SCRIPT), "summary", "--repo",
                                  str(self.origin), "--ref", broken], env=self.env,
                                 capture_output=True, text=True, check=True)
        newest = json.loads(summary.stdout)["newest"]
        self.assertEqual(newest["sha"], broken)
        self.assertEqual(newest["age_commits"], 0)

    def test_a_real_build_break_still_fails_the_pool(self):
        self.write("lib1/src/lib.rs", "this is not rust\n")
        self.git(self.origin, "commit", "-qam", "three: libraries do not compile")
        broken = self.git(self.origin, "rev-parse", "HEAD")
        result = subprocess.run([sys.executable, str(SCRIPT), "build", "--repo", str(self.origin),
                                 "--sha", broken, "--min-free-gib", "0"],
                                env=self.env, capture_output=True, text=True)
        self.assertNotEqual(result.returncode, 0)
        directory = next((self.root / "pools").iterdir())
        record = json.loads(next(directory.glob(f"{broken}-*.json")).read_text())
        self.assertEqual(record["state"], "failed")

    def concurrent_different_commits(self, env):
        pool_target = self.pool(env)
        before = digest_tree(pool_target)
        first = self.sandbox("s1", self.one)
        second = self.sandbox("s2", self.two)
        self.assertEqual(self.seed(first, env)["distance"], 0)
        self.assertEqual(self.seed(second, env)["distance"], 1)
        builds = [self.cargo(sandbox, "test", "--workspace", "--no-run", env=env)
                  for sandbox in (first, second)]
        for process in builds:
            code, output = self.finish(process)
            self.assertEqual(code, 0, output[-4000:])
        builds = [self.cargo(sandbox, "build", env=env) for sandbox in (first, second)]
        for process in builds:
            code, output = self.finish(process)
            self.assertEqual(code, 0, output[-4000:])
        self.assertEqual(self.run_app(first), f"gen-one {first / 'baked'}")
        self.assertEqual(self.run_app(second), f"gen-two {second / 'baked'}")
        self.assertEqual(digest_tree(pool_target), before, "the pool's files kept their bytes")
        return first, second

    def test_clobber_two_sandboxes_on_different_commits_build_concurrently(self):
        self.concurrent_different_commits(self.env)

    def test_clobber_with_shared_executables_behind_the_wrapper(self):
        wrapper = wrapper_binary()
        if wrapper is None:
            self.skipTest("no rsi-build-rustc with unlink-outputs built")
        env = dict(self.env, RUSTC_WRAPPER=str(wrapper))
        first, _second = self.concurrent_different_commits(env)
        # lib1's unchanged test executable is still one inode with the pool's.
        shared = [path for path in (first / "target/debug/deps").iterdir()
                  if path.name.startswith("lib1-") and "." not in path.name]
        self.assertTrue(shared and all(path.stat().st_nlink > 1 for path in shared))

    def test_shared_executables_fail_closed_without_the_wrapper(self):
        wrapper = wrapper_binary()
        if wrapper is None:
            self.skipTest("no rsi-build-rustc with unlink-outputs built")
        env = dict(self.env, RUSTC_WRAPPER=str(wrapper))
        pool_target = self.pool(env)
        before = digest_tree(pool_target)
        second = self.sandbox("s2", self.two)
        self.seed(second, env)
        code, output = self.finish(self.cargo(second, "test", "--workspace", "--no-run"))
        # rustc refuses the read-only output ("not writeable") or LLVM cannot
        # open a read-only split-debuginfo file; either way the build stops
        # and the shared files keep their bytes.
        self.assertNotEqual(code, 0, output)
        self.assertTrue("not writeable" in output or "IO failure on output stream" in output,
                        output[-3000:])
        self.assertEqual(digest_tree(pool_target), before)


def fake_pool(directory: Path, name: str, built_at: float, **extra) -> dict:
    """A ready pool record with a target directory, no cargo build needed."""
    worktree = directory / name
    (worktree / "target").mkdir(parents=True)
    (worktree / "target" / "blob").write_bytes(b"x" * 100)
    record = {"name": name, "sha": name.split("-")[0], "key": "k", "state": "ready",
              "target": str(worktree / "target"), "worktree": str(worktree),
              "built_at": built_at, **extra}
    (directory / f"{name}.json").write_text(json.dumps(record))
    return record


class PoolLifecycle(unittest.TestCase):
    """Pool GC selection, the in-use guard and the operator summary (#1882)."""

    def setUp(self):
        self.temp = scratch_tempdir(prefix="target-pool-gc-")
        self.addCleanup(self.temp.cleanup)
        self.directory = Path(self.temp.name)

    def names(self):
        return sorted(path.stem for path in self.directory.glob("*.json"))

    def test_gc_keeps_the_newest_n_ready_pools(self):
        for number in range(4):
            fake_pool(self.directory, f"s{number}-k", 100 + number)
        removed = POOL["gc"](self.directory, 2, None)
        self.assertEqual(sorted(removed), ["s0-k", "s1-k"])
        self.assertEqual(self.names(), ["s2-k", "s3-k"])
        self.assertFalse((self.directory / "s0-k").exists())

    def test_gc_pressure_keeps_only_the_newest_pool(self):
        for number in range(3):
            fake_pool(self.directory, f"s{number}-k", 100 + number)
        removed = POOL["gc"](self.directory, 2, None, pressure=True)
        self.assertEqual(sorted(removed), ["s0-k", "s1-k"])
        self.assertEqual(self.names(), ["s2-k"])

    def test_gc_keeps_the_newest_complete_pool_behind_degraded_ones(self):
        fake_pool(self.directory, "s0-k", 100)
        fake_pool(self.directory, "s1-k", 101, degraded=True)
        fake_pool(self.directory, "s2-k", 102, degraded=True)
        POOL["gc"](self.directory, 2, None)
        self.assertEqual(self.names(), ["s0-k", "s1-k", "s2-k"])
        # Pressure still never evicts the last complete pool (the test-compile
        # fallback): the newest degraded pool and the newest complete one stay.
        removed = POOL["gc"](self.directory, 2, None, pressure=True)
        self.assertEqual(removed, ["s1-k"])
        self.assertEqual(self.names(), ["s0-k", "s2-k"])

    def test_gc_pressure_keeps_the_last_complete_pool_under_a_newer_degraded_one(self):
        fake_pool(self.directory, "s0-k", 100)
        fake_pool(self.directory, "s1-k", 101, degraded=True)
        self.assertEqual(POOL["gc"](self.directory, 2, None, pressure=True), [])
        self.assertEqual(self.names(), ["s0-k", "s1-k"])
        self.assertTrue((self.directory / "s0-k" / "target" / "blob").is_file())

    def test_a_stale_gc_cannot_delete_a_pool_rebuilt_and_seeded_meanwhile(self):
        """Review #1891 (1): GC2 removes pool A, a build recreates A and a seed
        locks it, then GC1 (planned before all that) resumes. The lock file keeps
        its inode across removals, so GC1 sees the seed's lock and skips A."""
        for number in range(3):
            fake_pool(self.directory, f"s{number}-k", 100 + number)
        real_use_lock, real_gc = POOL["use_lock"], POOL["gc"]
        state = {"seed": None, "running": False}

        def interleaved(directory, name, exclusive):
            if exclusive and name == "s0-k" and not state["running"]:
                state["running"] = True
                import fcntl
                opened = open(directory / "s0-k.use", "a+")           # GC1 opens the lock file
                real_gc(directory, 1, None)                             # GC2 removes A
                fake_pool(directory, "s0-k", 500)                       # a build recreates A
                state["seed"] = real_use_lock(directory, "s0-k", exclusive=False)
                try:                                                    # GC1 resumes and locks
                    fcntl.flock(opened.fileno(), fcntl.LOCK_EX | fcntl.LOCK_NB)
                except OSError:
                    opened.close()
                    return None
                return opened
            return real_use_lock(directory, name, exclusive)

        skipped = []
        with mock.patch.dict(LIVE, {"use_lock": interleaved}):
            real_gc(self.directory, 1, None, skipped=skipped)
        self.addCleanup(lambda: state["seed"] and state["seed"].close())
        self.assertIsNotNone(state["seed"])
        self.assertTrue((self.directory / "s0-k" / "target" / "blob").is_file())
        self.assertEqual(json.loads((self.directory / "s0-k.json").read_text())["built_at"], 500)

    def test_a_stale_gc_does_not_remove_a_pool_rebuilt_since_its_plan(self):
        for number in range(3):
            fake_pool(self.directory, f"s{number}-k", 100 + number)
        real_use_lock = POOL["use_lock"]
        state = {"done": False}

        def rebuilt(directory, name, exclusive):
            if exclusive and name == "s0-k" and not state["done"]:
                state["done"] = True
                shutil.rmtree(directory / "s0-k")
                fake_pool(directory, "s0-k", 500)  # same name, new generation
            return real_use_lock(directory, name, exclusive)

        with mock.patch.dict(LIVE, {"use_lock": rebuilt}):
            POOL["gc"](self.directory, 1, None)
        self.assertTrue((self.directory / "s0-k" / "target" / "blob").is_file())
        self.assertEqual(json.loads((self.directory / "s0-k.json").read_text())["state"], "ready")

    def test_gc_ignores_failed_markers_and_counts_only_ready_pools(self):
        fake_pool(self.directory, "s0-k", 100)
        (self.directory / "s9-k.json").write_text(json.dumps(
            {"name": "s9-k", "state": "failed", "built_at": 900}))
        self.assertEqual(POOL["gc"](self.directory, 1, None), [])
        self.assertEqual(self.names(), ["s0-k", "s9-k"])

    def test_gc_never_removes_a_pool_a_seed_is_reading(self):
        for number in range(3):
            fake_pool(self.directory, f"s{number}-k", 100 + number)
        reader = POOL["use_lock"](self.directory, "s0-k", exclusive=False)
        self.assertIsNotNone(reader)
        skipped = []
        removed = POOL["gc"](self.directory, 1, None, skipped=skipped)
        self.assertEqual(removed, ["s1-k"])
        self.assertEqual(skipped, ["s0-k"])
        self.assertTrue((self.directory / "s0-k" / "target" / "blob").is_file())
        reader.close()
        self.assertEqual(POOL["gc"](self.directory, 1, None), ["s0-k"])

    def test_a_pool_gc_is_removing_is_not_offered_to_a_seed(self):
        old = fake_pool(self.directory, "s0-k", 100)
        new = fake_pool(self.directory, "s1-k", 101)
        removing = POOL["use_lock"](self.directory, "s1-k", exclusive=True)
        self.assertIsNotNone(removing)
        newest = lambda left: left[0] if left else None  # noqa: E731
        chosen, handle = POOL["claim_pool"](self.directory, [new, old], newest)
        self.assertEqual(chosen["name"], "s0-k")
        handle.close()
        removing.close()

    def test_keep_comes_from_the_operator_setting(self):
        with mock.patch.dict(os.environ, {"RSI_TARGET_POOL_KEEP": "3"}):
            self.assertEqual(POOL["default_keep"](), 3)
        with mock.patch.dict(os.environ, {"RSI_TARGET_POOL_KEEP": "junk"}):
            self.assertEqual(POOL["default_keep"](), 2)
        with mock.patch.dict(os.environ, {"RSI_TARGET_POOL_KEEP": "0"}):
            self.assertEqual(POOL["default_keep"](), 1)

    def test_gc_command_reports_removed_and_in_use_pools_as_json(self):
        # The CLI scans every directory under RSI_TARGET_POOL_ROOT, so give it a
        # private root holding only this test's pool directory, never the shared
        # temporary parent (sibling tests' pools live there).
        root = scratch_tempdir(prefix="target-pool-gc-root-")
        self.addCleanup(root.cleanup)
        pool = Path(root.name) / "repo"
        pool.mkdir()
        for number in range(3):
            fake_pool(pool, f"s{number}-k", 100 + number)
        reader = POOL["use_lock"](pool, "s0-k", exclusive=False)
        self.addCleanup(reader.close)
        env = dict(os.environ, RSI_TARGET_POOL_ROOT=root.name)
        result = subprocess.run([sys.executable, str(SCRIPT), "gc", "--keep", "1", "--json"],
                                env=env, capture_output=True, text=True, check=True,
                                cwd=pool)
        report = json.loads(result.stdout.strip().splitlines()[-1])
        self.assertEqual(report["removed"], ["s1-k"])
        self.assertEqual(report["skipped_in_use"].count("s0-k"), 1)

    def test_a_stale_pool_is_doomed_even_when_it_is_the_newest_and_the_only_one(self):
        records = [fake_pool(self.directory, "s0-k", 100), fake_pool(self.directory, "s1-k", 101)]
        kept, doomed = POOL["gc_plan"](records, 2, False, {"s1-k"})
        self.assertEqual([r["name"] for r in kept], ["s0-k"])
        self.assertEqual([r["name"] for r in doomed], ["s1-k"])
        kept, doomed = POOL["gc_plan"](records[1:], 2, False, {"s1-k"})
        self.assertEqual((kept, [r["name"] for r in doomed]), ([], ["s1-k"]))

    def test_a_stale_pool_does_not_use_up_the_keep_count(self):
        records = [fake_pool(self.directory, f"s{n}-k", 100 + n) for n in range(3)]
        kept, doomed = POOL["gc_plan"](records, 1, False, {"s2-k"})
        self.assertEqual([r["name"] for r in kept], ["s1-k"])
        self.assertEqual(sorted(r["name"] for r in doomed), ["s0-k", "s2-k"])

    def test_gc_collects_pools_beyond_the_seed_distance_of_the_tip(self):
        """#2707: the only pool, 841 commits behind, was kept forever and held 26 GB."""
        fake_pool(self.directory, "old-k", 100)
        ages = {"old-k": 841}
        age_of = lambda record, ref, repo=None: ages[record["name"]]  # noqa: E731
        with mock.patch.dict(LIVE, {"pool_age_commits": age_of}):
            self.assertEqual(POOL["gc"](self.directory, 2, None), [])  # no ref: unchanged
            self.assertEqual(POOL["gc"](self.directory, 2, None, ref="origin/rolling"),
                             ["old-k"])
        self.assertEqual(self.names(), [])
        self.assertFalse((self.directory / "old-k").exists())

    def test_gc_keeps_a_pool_within_the_limit_and_one_whose_age_is_unknown(self):
        fake_pool(self.directory, "near-k", 100)
        fake_pool(self.directory, "limit-k", 101)
        fake_pool(self.directory, "unknown-k", 102)
        ages = {"near-k": 10, "limit-k": 200, "unknown-k": None}
        age_of = lambda record, ref, repo=None: ages[record["name"]]  # noqa: E731
        with mock.patch.dict(LIVE, {"pool_age_commits": age_of}):
            self.assertEqual(POOL["gc"](self.directory, 3, None, ref="origin/rolling",
                                        max_distance=200), [])
            self.assertEqual(POOL["gc"](self.directory, 3, None, ref="origin/rolling",
                                        max_distance=100), ["limit-k"])
        self.assertEqual(self.names(), ["near-k", "unknown-k"])

    def test_gc_never_removes_a_stale_pool_a_seed_is_reading(self):
        fake_pool(self.directory, "old-k", 100)
        reader = POOL["use_lock"](self.directory, "old-k", exclusive=False)
        self.addCleanup(reader.close)
        skipped = []
        with mock.patch.dict(LIVE, {"pool_age_commits": lambda record, ref, repo=None: 841}):
            removed = POOL["gc"](self.directory, 2, None, skipped=skipped, ref="origin/rolling")
        self.assertEqual((removed, skipped), ([], ["old-k"]))
        self.assertTrue((self.directory / "old-k" / "target" / "blob").is_file())

    def test_gc_command_collects_a_stale_pool_unless_the_ref_is_emptied(self):
        root = scratch_tempdir(prefix="target-pool-stale-root-")
        self.addCleanup(root.cleanup)
        pool = Path(root.name) / "repo"
        pool.mkdir()
        # A real worktree so the CLI can count its distance from the tip.
        origin = Path(root.name) / "origin"
        origin.mkdir()
        env = dict(os.environ, RSI_TARGET_POOL_ROOT=root.name, GIT_AUTHOR_NAME="t",
                   GIT_AUTHOR_EMAIL="t@example.invalid", GIT_COMMITTER_NAME="t",
                   GIT_COMMITTER_EMAIL="t@example.invalid")
        git = lambda *a: scratch_git(origin, *a, env=env).stdout.strip()  # noqa: E731
        git("init", "-q", "-b", "main")
        (origin / "a").write_text("a")
        git("add", "-A")
        git("commit", "-qm", "a")
        first = git("rev-parse", "HEAD")
        for name in ("b", "c", "d"):
            (origin / name).write_text(name)
            git("add", "-A")
            git("commit", "-qm", name)
        fake_pool(pool, f"{first}-k", 100, worktree=str(origin))
        args = [sys.executable, str(SCRIPT), "gc", "--json", "--ref", "main", "--max-distance"]
        collected = subprocess.run(args + ["2"], env=env, capture_output=True, text=True,
                              check=True, cwd=pool)
        self.assertEqual(json.loads(collected.stdout)["removed"], [f"{first}-k"])
        fake_pool(pool, f"{first}-k", 100, worktree=str(origin))
        emptied = subprocess.run([sys.executable, str(SCRIPT), "gc", "--json", "--ref", "",
                                  "--max-distance", "1"], env=env, capture_output=True,
                                 text=True, check=True, cwd=pool)
        self.assertEqual(json.loads(emptied.stdout)["removed"], [])

    def test_the_nearest_pool_triggers_a_refresh_past_half_the_distance_limit(self):
        needed = POOL["refresh_needed"]
        self.assertEqual([needed(None, 200), needed(0, 200), needed(100, 200), needed(101, 200)],
                         [True, False, False, True])

    def test_gc_explicit_zero_evicts_the_last_complete_pool_only_after_its_reader_finishes(self):
        for pressure in (False, True):
            with self.subTest(pressure=pressure), scratch_tempdir(prefix="target-pool-zero-") as root:
                repo = Path(root) / "source"
                repo.mkdir()
                scratch_git(repo, "init", "--quiet")
                env = dict(os.environ, RSI_TARGET_POOL_ROOT=str(Path(root) / "pools"))
                with mock.patch.dict(os.environ, env):
                    pool = POOL["repo_dir"](repo)
                pool.mkdir(parents=True)
                fake_pool(pool, "complete-k", 100)
                fake_pool(pool, "degraded-k", 200, degraded=True)
                (pool / "failed-k.json").write_text(json.dumps(
                    {"name": "failed-k", "state": "failed", "built_at": 300}))
                other = Path(env["RSI_TARGET_POOL_ROOT"]) / "other-repo"
                other.mkdir()
                fake_pool(other, "other-k", 400)
                reader = POOL["use_lock"](pool, "complete-k", exclusive=False)
                self.assertIsNotNone(reader)
                command = [sys.executable, str(SCRIPT), "gc", "--repo", str(repo),
                           "--keep", "0", "--json", *(["--pressure"] if pressure else [])]
                try:
                    result = subprocess.run(command, env=env, capture_output=True, text=True,
                                            check=True)
                    report = json.loads(result.stdout)
                    self.assertEqual(report["removed"], ["degraded-k"])
                    self.assertEqual(report["skipped_in_use"], ["complete-k"])
                    self.assertTrue((pool / "complete-k" / "target" / "blob").is_file())
                finally:
                    reader.close()
                result = subprocess.run(command, env=env, capture_output=True, text=True,
                                        check=True)
                self.assertEqual(json.loads(result.stdout),
                                 {"removed": ["complete-k"], "skipped_in_use": []})
                self.assertEqual(sorted(path.stem for path in pool.glob("*.json")),
                                 ["failed-k"])
                self.assertTrue((other / "other-k" / "target" / "blob").is_file())

    def test_summary_names_the_newest_good_pool_its_size_and_a_newer_failure(self):
        fake_pool(self.directory, "aaa-k", 100, size_bytes=300)
        fake_pool(self.directory, "bbb-k", 200, degraded=True, size_bytes=500)
        (self.directory / "ccc-k.json").write_text(json.dumps(
            {"name": "ccc-k", "sha": "ccc", "state": "failed", "built_at": 300}))
        summary = POOL["summarise"]([self.directory], "origin/rolling")
        self.assertEqual(summary["pools"], 2)
        self.assertEqual(summary["total_bytes"], 800)
        self.assertEqual(summary["newest"]["sha"], "bbb")
        self.assertTrue(summary["newest"]["degraded"])
        self.assertEqual(summary["failed_newer"], 1)
        self.assertEqual(summary["failed_latest"]["sha"], "ccc")

    def test_summary_counts_only_idle_pools_on_the_requested_filesystem(self):
        fake_pool(self.directory, "idle-k", 100, size_bytes=200)
        fake_pool(self.directory, "busy-k", 200, size_bytes=300)
        reader = POOL["use_lock"](self.directory, "busy-k", exclusive=False)
        self.assertIsNotNone(reader)
        try:
            summary = POOL["summarise"]([self.directory], "main", self.directory)
            self.assertEqual(summary["total_bytes"], 500)
            self.assertEqual(summary["reclaimable_bytes"], 200)
            # A different filesystem must count neither pool and reclaim neither.
            with mock.patch.dict(LIVE, same_filesystem=lambda record, path: False):
                summary = POOL["summarise"]([self.directory], "main", self.directory)
                self.assertEqual(summary["reclaimable_bytes"], 0)
                self.assertEqual(POOL["gc"](self.directory, 0, None,
                                            filesystem=self.directory), [])
            self.assertTrue((self.directory / "idle-k/target/blob").is_file())
            self.assertTrue((self.directory / "busy-k/target/blob").is_file())
        finally:
            reader.close()
        self.assertEqual(POOL["summarise"]([self.directory], "main")["reclaimable_bytes"], 500)

    def test_summary_sizes_an_old_pool_once_and_remembers_it(self):
        fake_pool(self.directory, "aaa-k", 100)
        summary = POOL["summarise"]([self.directory], "origin/rolling")
        self.assertEqual(summary["total_bytes"], 100)
        remembered = json.loads((self.directory / "aaa-k.json").read_text())
        self.assertEqual(remembered["size_bytes"], 100)

    def test_summary_cannot_resurrect_a_marker_gc_removed_during_the_size_walk(self):
        """Review #1891 (2): a pre-upgrade marker has no size; the walk races a GC."""
        fake_pool(self.directory, "aaa-k", 100)
        fake_pool(self.directory, "bbb-k", 200)
        real_tree_size = POOL["tree_size"]

        def racing(path):
            size = real_tree_size(path)
            if Path(path).parent.name == "aaa-k":
                POOL["gc"](self.directory, 1, None)  # held shared: gc must skip aaa-k
            return size

        with mock.patch.dict(LIVE, {"tree_size": racing}):
            POOL["summarise"]([self.directory], "origin/rolling")
        # The pool is either fully there or fully gone, never a ready marker
        # without a target.
        marker = self.directory / "aaa-k.json"
        self.assertEqual(marker.exists(), (self.directory / "aaa-k" / "target").is_dir())

    def test_size_cache_does_not_rewrite_a_marker_that_was_removed(self):
        fake_pool(self.directory, "aaa-k", 100)
        real_tree_size = POOL["tree_size"]

        def removing(path):
            size = real_tree_size(path)
            (self.directory / "aaa-k.json").unlink()
            shutil.rmtree(self.directory / "aaa-k")
            return size

        record = json.loads((self.directory / "aaa-k.json").read_text())
        with mock.patch.dict(LIVE, {"tree_size": removing}):
            POOL["pool_size"](self.directory, record)
        self.assertFalse((self.directory / "aaa-k.json").exists())

    def test_a_ready_marker_without_a_target_is_not_ready_and_not_counted(self):
        fake_pool(self.directory, "aaa-k", 100, size_bytes=5)
        shutil.rmtree(self.directory / "aaa-k")
        record = json.loads((self.directory / "aaa-k.json").read_text())
        self.assertFalse(POOL["is_ready"](record))
        self.assertEqual(POOL["ready_pools"](self.directory, "k"), [])
        self.assertEqual(POOL["summarise"]([self.directory], "origin/rolling")["pools"], 0)


class NoPoolReason(unittest.TestCase):
    """Why a seed found no pool, and a build that would stop at the floor (#178).
    Fake pool records over a three-commit git repo: no Cargo build, no real pool."""

    def setUp(self):
        self.temp = scratch_tempdir(prefix="target-pool-reason-")
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.env = {name: value for name, value in os.environ.items()
                    if not name.startswith(("CARGO_", "RSI_", "SCCACHE_", "RUSTC_"))}
        self.env["RSI_TARGET_POOL_ROOT"] = str(self.root / "pools")
        self.repo = self.root / "repo"
        self.repo.mkdir()
        self.git("init", "-q", "-b", "main")
        self.git("config", "user.email", "t@example.invalid")
        self.git("config", "user.name", "t")
        (self.repo / "Cargo.toml").write_text("[workspace]\nmembers = []\n")
        self.shas = []
        for name in ("a", "b", "c"):
            (self.repo / f"{name}.txt").write_text(name)
            self.git("add", "-A")
            self.git("commit", "-qm", name)
            self.shas.append(self.git("rev-parse", "HEAD"))
        # A commit on another line of history: never an ancestor of HEAD.
        self.side = self.git("commit-tree", f"{self.shas[0]}^{{tree}}", "-p", self.shas[0],
                             "-m", "side")
        self.head = self.shas[2]

    def git(self, *args):
        return scratch_git(self.repo, *args, env=self.env).stdout.strip()

    def pools(self, *shas):
        return [{"sha": sha} for sha in shas]

    def test_no_ready_pool_for_the_key_is_named(self):
        reason = POOL["explain_no_pool"](self.repo, self.head, [], 200)
        self.assertEqual(reason, {"reason": "no ready pool has this build key"})

    def test_a_pool_that_is_not_an_ancestor_is_named(self):
        pools = self.pools(self.side)
        self.assertIsNone(POOL["choose_pool"](self.repo, self.head, pools, 200))
        reason = POOL["explain_no_pool"](self.repo, self.head, pools, 200)
        self.assertEqual(reason, {"reason": "none of 1 ready pool(s) is an ancestor of HEAD"})

    def test_an_ancestor_beyond_the_limit_is_refused_and_its_distance_reported(self):
        pools = self.pools(self.side, self.shas[0])
        self.assertIsNone(POOL["choose_pool"](self.repo, self.head, pools, 1))
        self.assertEqual(POOL["explain_no_pool"](self.repo, self.head, pools, 1), {
            "reason": "nearest ancestor pool is 2 commits behind (limit 1)",
            "nearest_pool": {"sha": self.shas[0], "distance": 2}})
        chosen = POOL["choose_pool"](self.repo, self.head, pools, 2)
        self.assertEqual((chosen["sha"], chosen["distance"]), (self.shas[0], 2))

    def test_the_nearest_ancestor_wins_and_an_exact_pool_is_distance_zero(self):
        pools = self.pools(self.shas[0], self.shas[1])
        self.assertEqual(POOL["choose_pool"](self.repo, self.head, pools, 200)["distance"], 1)
        pools.append({"sha": self.head})
        self.assertEqual(POOL["choose_pool"](self.repo, self.head, pools, 200)["distance"], 0)

    def test_seed_reports_the_reason_next_to_no_pool(self):
        with mock.patch.dict(os.environ, self.env, clear=True):
            key = POOL["build_key"](self.repo)
            directory = POOL["repo_dir"](self.repo)
            directory.mkdir(parents=True)
            fake_pool(directory, f"{self.shas[0]}-{key}", 100, key=key)
            args = argparse.Namespace(sandbox=str(self.repo), target=None,
                                      build_missing=False, max_distance=1)
            output = io.StringIO()
            with contextlib.redirect_stdout(output):
                self.assertEqual(POOL["cmd_seed"](args), 0)
        outcome = json.loads(output.getvalue())
        self.assertEqual(outcome["result"], "no pool")
        self.assertEqual(outcome["reason"], "nearest ancestor pool is 2 commits behind (limit 1)")
        self.assertEqual(outcome["nearest_pool"], {"sha": self.shas[0], "distance": 2})

    def pool_in(self, directory, key, sha, built_at):
        return fake_pool(directory, f"{sha}-{key}", built_at, key=key, sha=sha,
                         worktree=str(self.repo))

    def test_stale_pools_are_those_more_than_the_limit_behind_the_tip(self):
        records = [{"name": "far", "state": "ready", "sha": self.shas[0],
                    "worktree": str(self.repo)},
                   {"name": "near", "state": "ready", "sha": self.shas[2],
                    "worktree": str(self.repo)},
                   {"name": "gone", "state": "ready", "sha": self.shas[0],
                    "worktree": str(self.root / "missing")}]
        with mock.patch.dict(os.environ, self.env, clear=True):
            self.assertEqual(POOL["stale_pool_names"](records, "main", 1), {"far"})
            self.assertEqual(POOL["stale_pool_names"](records, "main", 2), set())
            self.assertEqual(POOL["stale_pool_names"](records, "nope", 0), set())

    def test_a_pool_directory_remembers_its_checkout_before_the_last_pool_goes(self):
        with mock.patch.dict(os.environ, self.env, clear=True):
            key = POOL["build_key"](self.repo)
            directory = POOL["repo_dir"](self.repo)
            directory.mkdir(parents=True)
            self.pool_in(directory, key, self.shas[0], 100)
            self.assertIsNone(POOL["known_repo"](directory))
            removed = POOL["gc"](directory, 2, None, ref="main", max_distance=1)
            self.assertEqual(removed, [f"{self.shas[0]}-{key}"])
            self.assertEqual(POOL["known_repo"](directory), self.repo)

    def test_refresh_builds_at_the_tip_when_the_only_pool_was_collected_as_stale(self):
        """#2707 end to end: collect the stale pool, then rebuild at the tip."""
        with mock.patch.dict(os.environ, self.env, clear=True):
            key = POOL["build_key"](self.repo)
            directory = POOL["repo_dir"](self.repo)
            directory.mkdir(parents=True)
            self.pool_in(directory, key, self.shas[0], 100)
            POOL["gc"](directory, 2, None, ref="main", max_distance=1)
            self.assertEqual(POOL["ready_pools"](directory, key), [])
            requested = mock.Mock(return_value="started unit")
            with mock.patch.dict(LIVE, {"request_build": requested}):
                outcome = POOL["refresh_directory"](directory, None, "main", 1, 3600)
        requested.assert_called_once_with(self.repo, self.head)
        self.assertEqual((outcome["result"], outcome["nearest"]), ("started unit", None))

    def imported_pool(self, directory, key, sha, built_at):
        """A pool `seed-from` made: a target directory with no Git worktree around it."""
        return fake_pool(directory, f"{sha}-{key}", built_at, key=key, sha=sha,
                         seeded_from={"host": "peer"})

    def test_an_imported_pool_is_aged_through_the_checkout_its_directory_belongs_to(self):
        """#2717: the pool's own directory is not a repository, so Git cannot count it."""
        with mock.patch.dict(os.environ, self.env, clear=True):
            key = POOL["build_key"](self.repo)
            directory = POOL["repo_dir"](self.repo)
            directory.mkdir(parents=True)
            record = self.imported_pool(directory, key, self.shas[0], 100)
            self.assertIsNone(POOL["pool_age_commits"](record, "main"))
            self.assertEqual(POOL["pool_age_commits"](record, "main", self.repo), 2)
            self.assertEqual(POOL["stale_pool_names"]([record], "main", 1), set())
            self.assertEqual(POOL["stale_pool_names"]([record], "main", 1, self.repo),
                             {record["name"]})

    def test_seed_from_records_the_checkout_of_the_pool_it_imports(self):
        """#2717: the import itself writes the association; no local build is needed."""
        with mock.patch.dict(os.environ, self.env, clear=True):
            key = POOL["build_key"](self.repo)
            directory = POOL["repo_dir"](self.repo)
            directory.mkdir(parents=True)
            self.assertIsNone(POOL["known_repo"](directory))
            name = f"{self.shas[0]}-{key}"
            remote = {"name": name, "sha": self.shas[0], "target": "/remote/target",
                      "built_at": 100}

            def fake_stream(host, target, destination, level):
                (destination / "target").mkdir()
                (destination / "target" / "blob").write_bytes(b"x" * 10)

            args = argparse.Namespace(repo=str(self.repo), host="peer", remote_tool="t",
                                      sha=None, min_free_gib=0, min_headroom_gib=0,
                                      dry_run=False, level=1, keep=5)
            patched = {"remote_pools": lambda host, tool: [remote],
                       "choose_remote_pool": lambda repo, key, records, sha: remote,
                       "remote_size_bytes": lambda host, target: 10,
                       "stream_target": fake_stream}
            with mock.patch.dict(LIVE, patched), contextlib.redirect_stdout(io.StringIO()):
                self.assertEqual(POOL["cmd_seed_from"](args), 0)
            self.assertEqual(POOL["known_repo"](directory), self.repo)
            self.assertFalse((directory / name / ".git").exists())

    def test_a_stale_imported_pool_is_collected_then_refreshed_without_a_repo(self):
        """#2717 end to end after an upgrade: the pool was imported before the checkout
        was recorded, a worker seed records it, gc collects the stale pool and the
        reclaim-triggered refresh (no --repo) rebuilds at the tip."""
        with mock.patch.dict(os.environ, self.env, clear=True):
            key = POOL["build_key"](self.repo)
            directory = POOL["repo_dir"](self.repo)
            directory.mkdir(parents=True)
            record = self.imported_pool(directory, key, self.shas[0], 100)
            self.assertIsNone(POOL["known_repo"](directory))
            # Before the upgrade path ran: nothing is collectable, nothing refreshes.
            self.assertEqual(POOL["gc"](directory, 2, None, ref="main", max_distance=1), [])
            skipped = POOL["refresh_directory"](directory, None, "main", 1, 3600)
            self.assertTrue(skipped["result"].startswith("skipped: no checkout"), skipped)
            # A worker seed in a sandbox of the repository records the checkout.
            args = argparse.Namespace(sandbox=str(self.repo), target=None,
                                      build_missing=False, max_distance=1)
            with contextlib.redirect_stdout(io.StringIO()):
                self.assertEqual(POOL["cmd_seed"](args), 0)
            self.assertEqual(POOL["known_repo"](directory), self.repo)
            removed = POOL["gc"](directory, 2, None, ref="main", max_distance=1)
            self.assertEqual(removed, [record["name"]])
            self.assertFalse((directory / record["name"]).exists())
            self.assertEqual(POOL["ready_pools"](directory, key), [])
            requested = mock.Mock(return_value="started unit")
            with mock.patch.dict(LIVE, {"request_build": requested}):
                outcome = POOL["refresh_directory"](directory, None, "main", 1, 3600)
        requested.assert_called_once_with(self.repo, self.head)
        self.assertEqual((outcome["result"], outcome["nearest"]), ("started unit", None))

    def test_gc_with_a_repo_records_the_checkout_of_an_imported_pool_directory(self):
        with mock.patch.dict(os.environ, self.env, clear=True):
            key = POOL["build_key"](self.repo)
            directory = POOL["repo_dir"](self.repo)
            directory.mkdir(parents=True)
            self.imported_pool(directory, key, self.shas[1], 100)
            args = argparse.Namespace(repo=str(self.repo), keep=2, pressure=False, json=True,
                                      ref="main", max_distance=1)
            with contextlib.redirect_stdout(io.StringIO()):
                self.assertEqual(POOL["cmd_gc"](args), 0)
            self.assertEqual(POOL["known_repo"](directory), self.repo)

    def test_refresh_leaves_a_fresh_pool_and_a_recent_failure_alone(self):
        with mock.patch.dict(os.environ, self.env, clear=True):
            key = POOL["build_key"](self.repo)
            directory = POOL["repo_dir"](self.repo)
            directory.mkdir(parents=True)
            self.pool_in(directory, key, self.shas[1], 100)  # one behind: within half of 4
            requested = mock.Mock(return_value="started unit")
            with mock.patch.dict(LIVE, {"request_build": requested}):
                fresh = POOL["refresh_directory"](directory, self.repo, "main", 4, 3600)
                self.assertEqual((fresh["result"], fresh["nearest"]), ("fresh", 1))
                requested.assert_not_called()
                # Past half the limit (2 > 1) a build is wanted, unless this tip just failed.
                (directory / f"{self.head}-{key}.json").write_text(json.dumps(
                    {"state": "failed", "built_at": POOL["time"].time()}))
                failed = POOL["refresh_directory"](directory, self.repo, "main", 1, 3600)
                self.assertTrue(failed["result"].startswith("skipped: the build"), failed)
                requested.assert_not_called()
                (directory / f"{self.head}-{key}.json").unlink()
                POOL["refresh_directory"](directory, self.repo, "main", 1, 3600)
        requested.assert_called_once_with(self.repo, self.head)

    def test_refresh_without_a_known_checkout_or_ref_says_so_and_builds_nothing(self):
        with mock.patch.dict(os.environ, self.env, clear=True):
            directory = POOL["repo_dir"](self.repo)
            directory.mkdir(parents=True)
            requested = mock.Mock()
            with mock.patch.dict(LIVE, {"request_build": requested}):
                unknown = POOL["refresh_directory"](directory, None, "main", 200, 3600)
                missing = POOL["refresh_directory"](directory, self.repo, "nope", 200, 3600)
        self.assertTrue(unknown["result"].startswith("skipped: no checkout"), unknown)
        self.assertTrue(missing["result"].startswith("skipped: nope does not resolve"), missing)
        requested.assert_not_called()

    def test_a_build_request_below_the_free_space_floor_says_so_and_starts_nothing(self):
        with mock.patch.dict(os.environ, dict(self.env, RSI_TARGET_POOL_MIN_FREE_GIB="1",
                                              RSI_TARGET_POOL_WORKER_FLOOR_GIB="1"),
                             clear=True), \
                mock.patch.dict(LIVE, free_gib=lambda path: 0.5):
            result = POOL["request_build"](self.repo, self.head)
        self.assertTrue(result.startswith("skipped: pool_skipped_disk_headroom: only 0.5 GiB free"),
                        result)
        self.assertIn("needs 17.0 GiB (floor 1.0 + expected pool 16.0, default)", result)


GIB = 2**30


class PoolDiskFloor(unittest.TestCase):
    """A cache build never pushes the host below the floor its workers need (#2773)."""

    def setUp(self):
        self.temp = scratch_tempdir(prefix="target-pool-floor-")
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.env = {name: value for name, value in os.environ.items()
                    if not name.startswith(("CARGO_", "RSI_", "SCCACHE_", "RUSTC_"))}
        self.env["RSI_TARGET_POOL_ROOT"] = str(self.root / "pools")
        self.env["RSI_CARGO_SLOT_HELD"] = "1"  # the stand-in build needs no governor slot
        self.repo = self.root / "repo"
        self.repo.mkdir()
        self.git("init", "-q", "-b", "main")
        self.git("config", "user.email", "t@example.invalid")
        self.git("config", "user.name", "t")
        (self.repo / "Cargo.toml").write_text("[workspace]\nmembers = []\n")
        self.shas = []
        for name in ("a", "b"):
            (self.repo / f"{name}.txt").write_text(name)
            self.git("add", "-A")
            self.git("commit", "-qm", name)
            self.shas.append(self.git("rev-parse", "HEAD"))
        self.head = self.shas[-1]
        # The floors cap at a quarter of the disk; these numbers must not depend on the host's.
        patch = mock.patch.dict(LIVE, scaled_floor_gib=lambda path, gib: gib)
        patch.start()
        self.addCleanup(patch.stop)

    def git(self, *args):
        return scratch_git(self.repo, *args, env=self.env).stdout.strip()

    def setup_directory(self):
        key = POOL["build_key"](self.repo)
        directory = POOL["repo_dir"](self.repo)
        directory.mkdir(parents=True)
        return key, directory

    def sized_pool(self, directory, key, sha, built_at, size_gib):
        return fake_pool(directory, f"{sha}-{key}", built_at, key=key, sha=sha,
                         size_bytes=int(size_gib * GIB))

    # -- 1. admission: floor + expected size, typed reason naming both numbers

    def test_expected_size_is_the_nearest_ancestor_then_the_newest_pool_then_the_last_then_default(self):
        with mock.patch.dict(os.environ, self.env, clear=True):
            key, directory = self.setup_directory()
            expected = POOL["expected_pool_size"]
            self.assertEqual(expected(directory, self.repo, self.head, key),
                             (16 * GIB, "default"))
            POOL["record_event"](directory, "last_pool", {"size_bytes": 11 * GIB})
            self.assertEqual(expected(directory, self.repo, self.head, key),
                             (11 * GIB, "last pool built"))
            self.sized_pool(directory, key, self.shas[0], 100, 14)
            self.assertEqual(expected(directory, self.repo, self.head, key),
                             (14 * GIB, "nearest ready ancestor"))
            # A pool that is not an ancestor still gives the newest pool's measured size.
            side = self.git("commit-tree", f"{self.shas[0]}^{{tree}}", "-m", "side")
            self.assertEqual(expected(directory, self.repo, side, key)[1], "newest ready pool")
        with mock.patch.dict(os.environ, dict(self.env, RSI_TARGET_POOL_EXPECTED_GIB="9"),
                             clear=True):
            self.assertEqual(expected(self.root / "empty", self.repo, self.head, key)[0], 9 * GIB)

    def test_admission_needs_the_floor_plus_the_expected_pool_size(self):
        env = dict(self.env, RSI_TARGET_POOL_WORKER_FLOOR_GIB="1")
        with mock.patch.dict(os.environ, env, clear=True):
            key, directory = self.setup_directory()
            self.sized_pool(directory, key, self.shas[0], 100, 16)
            skip = lambda free: POOL["admission_skip"](directory, self.repo, self.head, key, 2)
            # The incident: admitted at 36.9 GiB free for a 16 GiB pool, finishing below the floor.
            with mock.patch.dict(LIVE, free_gib=lambda path: 17.9):
                refused = skip(17.9)
            self.assertEqual(refused["reason"], "pool_skipped_disk_headroom")
            self.assertEqual((refused["free_gib"], refused["floor_gib"], refused["expected_gib"],
                              refused["needed_gib"]), (17.9, 2.0, 16.0, 18.0))
            self.assertEqual(refused["expected_source"], "nearest ready ancestor")
            for number in ("17.9", "18.0", "16.0"):
                self.assertIn(number, refused["detail"])
            with mock.patch.dict(LIVE, free_gib=lambda path: 18.0):
                self.assertIsNone(skip(18.0))

    def test_the_workers_floor_counts_when_it_is_above_the_pool_floor(self):
        env = dict(self.env, RSI_TARGET_POOL_WORKER_FLOOR_GIB="5", RSI_TARGET_POOL_EXPECTED_GIB="1")
        with mock.patch.dict(os.environ, env, clear=True):
            key, directory = self.setup_directory()
            with mock.patch.dict(LIVE, free_gib=lambda path: 5.5):
                refused = POOL["admission_skip"](directory, self.repo, self.head, key, 2)
            self.assertEqual((refused["floor_gib"], refused["needed_gib"]), (5.0, 6.0))
            # A floor of zero turns the check off (the other tests build on small hosts).
            with mock.patch.dict(LIVE, free_gib=lambda path: 0.0):
                self.assertIsNone(POOL["admission_skip"](directory, self.repo, self.head, key, 0))

    def test_a_skipped_request_records_the_reason_for_status_and_a_started_one_clears_it(self):
        env = dict(self.env, RSI_TARGET_POOL_WORKER_FLOOR_GIB="1",
                   RSI_TARGET_POOL_MIN_FREE_GIB="2")
        with mock.patch.dict(os.environ, env, clear=True):
            key, directory = self.setup_directory()
            with mock.patch.dict(LIVE, free_gib=lambda path: 5.0):
                result = POOL["request_build"](self.repo, self.head)
            self.assertTrue(result.startswith("skipped: pool_skipped_disk_headroom"), result)
            summary = POOL["summarise"]([directory], "main")
            self.assertEqual(summary["last_skip"]["reason"], "pool_skipped_disk_headroom")
            self.assertEqual(summary["last_skip"]["needed_gib"], 18.0)
            self.assertEqual(summary["last_skip"]["free_gib"], 5.0)
            # Once a newer pool exists the skip is history, not the reason there is no pool.
            self.sized_pool(directory, key, self.shas[0], POOL["time"].time() + 10, 1)
            self.assertIsNone(POOL["summarise"]([directory], "main")["last_skip"])

    # -- 2. a build that crosses the workers' floor stops and leaves nothing behind

    def test_a_guarded_build_is_stopped_the_moment_free_space_crosses_the_floor(self):
        started = POOL["time"].time()
        sleeper = [sys.executable, "-c", "import time; time.sleep(120)"]
        guard = POOL["DiskGuard"](self.root, 30.0, 0.05)
        with mock.patch.dict(LIVE, free_gib=lambda path: 29.0), \
                (self.root / "build.log").open("w") as output:
            with self.assertRaises(POOL["DiskFloorAbort"]) as caught:
                POOL["run_logged"](sleeper, self.root, dict(os.environ), output, guard)
        self.assertEqual((caught.exception.free, caught.exception.floor), (29.0, 30.0))
        self.assertLess(POOL["time"].time() - started, 60)
        # Above the floor the same guard lets the build finish and report its own exit code.
        with mock.patch.dict(LIVE, free_gib=lambda path: 31.0), \
                (self.root / "build.log").open("w") as output:
            self.assertEqual(POOL["run_logged"](
                [sys.executable, "-c", "import sys; sys.exit(3)"], self.root,
                dict(os.environ), output, guard), 3)

    def test_an_aborted_pool_build_removes_its_partial_output_and_reports_the_reason(self):
        writer = ("import os, time; deps = os.environ['CARGO_TARGET_DIR'] + '/debug/deps'; "
                  "os.makedirs(deps, exist_ok=True); open(deps + '/partial.rlib', 'w').write('x'); "
                  "time.sleep(120)")
        env = dict(self.env, RSI_TARGET_POOL_WORKER_FLOOR_GIB="30",
                   RSI_TARGET_POOL_WATCH_SEC="0.05", RSI_TARGET_POOL_EXPECTED_GIB="1")
        calls = []

        def draining(path):
            calls.append(path)
            return 400.0 if len(calls) <= 2 else 1.0  # admitted, then the build eats the disk

        with mock.patch.dict(os.environ, env, clear=True), \
                mock.patch.dict(LIVE, free_gib=draining,
                                BUILD_COMMAND=[sys.executable, "-c", writer]):
            directory = POOL["repo_dir"](self.repo)
            directory.mkdir(parents=True)
            args = argparse.Namespace(repo=str(self.repo), sha=self.head, keep=2,
                                      min_free_gib=2.0, retry_after=3600, max_distance=200)
            started = POOL["time"].time()
            self.assertEqual(POOL["cmd_build"](args), 1)
            self.assertLess(POOL["time"].time() - started, 90)
            key = POOL["build_key"](self.repo)
            name = f"{self.head}-{key}"
            self.assertFalse((directory / name).exists(), "the partial worktree must be gone")
            marker = json.loads((directory / f"{name}.json").read_text())
            self.assertEqual((marker["state"], marker["reason"]), ("failed", "pool_aborted_disk_floor"))
            self.assertEqual(marker["floor_gib"], 30.0)
            self.assertEqual(POOL["ready_pools"](directory, key), [])
            self.assertEqual(self.git("worktree", "list").count("\n"), 0, "no pool worktree left")
            summary = POOL["summarise"]([directory], "main")
            self.assertEqual(summary["failed_latest"]["reason"], "pool_aborted_disk_floor")
            # The retry window applies: the next build of this tip does nothing.
            self.assertEqual(POOL["cmd_build"](args), 0)

    # -- 3. a worker waiting on the disk floor may take an idle pool

    def reclaim_args(self, interval=600.0):
        return argparse.Namespace(min_interval=interval)

    def run_reclaim(self, interval=600.0):
        output = io.StringIO()
        with contextlib.redirect_stdout(output):
            self.assertEqual(POOL["cmd_reclaim_idle"](self.reclaim_args(interval)), 0)
        return json.loads(output.getvalue())

    def test_reclaim_idle_evicts_unleased_pools_skips_a_leased_one_and_is_bounded(self):
        with mock.patch.dict(os.environ, self.env, clear=True):
            key, directory = self.setup_directory()
            idle = self.sized_pool(directory, key, self.shas[0], 100, 2)
            busy = self.sized_pool(directory, key, self.shas[1], 200, 3)
            lease = POOL["use_lock"](directory, busy["name"], exclusive=False)
            self.assertIsNotNone(lease)
            report = self.run_reclaim()
            lease.close()
            self.assertEqual(report["reason"], "pool_reclaimed_for_disk_floor")
            self.assertEqual(report["removed"], [idle["name"]])
            self.assertEqual(report["skipped_in_use"], [busy["name"]])
            self.assertEqual(report["freed_bytes"], 2 * GIB)
            self.assertTrue((directory / f"{busy['name']}.json").exists())
            self.assertFalse((directory / f"{idle['name']}.json").exists())
            summary = POOL["summarise"]([directory], "main")
            self.assertEqual(summary["last_reclaim"]["reason"], "pool_reclaimed_for_disk_floor")
            self.assertEqual(summary["last_reclaim"]["removed"], 1)
            # Bounded: the directory was just reclaimed, so a second call leaves the rest alone.
            again = self.run_reclaim()
            self.assertEqual((again["removed"], again["throttled"]), ([], [directory.name]))
            self.assertTrue((directory / f"{busy['name']}.json").exists())
            # Past the interval the now idle pool goes too.
            self.assertEqual(self.run_reclaim(interval=0.0)["removed"], [busy["name"]])

    def test_the_last_pool_size_survives_a_reclaim_for_the_next_admission(self):
        with mock.patch.dict(os.environ, self.env, clear=True):
            key, directory = self.setup_directory()
            self.sized_pool(directory, key, self.shas[0], 100, 12)
            POOL["record_event"](directory, "last_pool", {"size_bytes": 12 * GIB})
            self.run_reclaim()
            self.assertEqual(POOL["ready_pools"](directory, key), [])
            self.assertEqual(POOL["expected_pool_size"](directory, self.repo, self.head, key),
                             (12 * GIB, "last pool built"))

    def test_seed_from_leaves_the_workers_their_floor_too(self):
        # The transfer is refused when it would end below the workers' floor, with the reason.
        env = dict(self.env, RSI_TARGET_POOL_WORKER_FLOOR_GIB="30")
        with mock.patch.dict(os.environ, env, clear=True):
            key, directory = self.setup_directory()
            remote = {"name": f"{self.shas[0]}-{key}", "sha": self.shas[0], "key": key,
                      "state": "ready", "target": "/remote/target", "built_at": 1}
            args = argparse.Namespace(host="h", repo=str(self.repo), sha=None, dry_run=False,
                                      keep=None, level=3, remote_tool="t", min_free_gib=2.0,
                                      min_headroom_gib=0)
            with mock.patch.dict(LIVE, remote_pools=lambda host, tool: [remote],
                                 remote_size_bytes=lambda host, target: 10 * GIB,
                                 free_gib=lambda path: 35.0):
                self.assertEqual(POOL["cmd_seed_from"](args), 3)
            skip = POOL["summarise"]([directory], "main")["last_skip"]
            self.assertEqual(skip["reason"], "pool_skipped_disk_headroom")
            self.assertEqual((skip["free_gib"], skip["expected_gib"]), (35.0, 10.0))


if __name__ == "__main__":
    unittest.main()
