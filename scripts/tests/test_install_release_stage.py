"""#136: install-release.sh --stage-dir DIR stages the built set into DIR and
touches nothing live (no ~/.rsi/install|bin|lib, no ~/.local/bin links, no
supervisor, no restart), and prints a sha256 manifest. --no-restart still means
"install but do not restart".

#262 regressions (hub #2800, #2801, #2799, #2802): DIR is written as one fresh
tree, never through a symlink already in it; a staging build refuses while a
live link resolves into the cargo release directory; the staged names are
exactly the top-level files AgentRequestDeploy reads; no GNU-only readlink."""

import hashlib
import os
import re
import shutil
import subprocess
import tempfile
import unittest
from pathlib import Path

SCRIPTS = Path(__file__).resolve().parents[1]
REPO = SCRIPTS.parent
# The deploy consumer: AgentRequestDeploy's managed set and the loop that reads it.
AGENT_DEPLOY_RS = REPO / "crates" / "rsi-common" / "src" / "agent_deploy.rs"
DEPLOY_RS = REPO / "crates" / "rsid" / "src" / "deploy.rs"
# Members a cargo build produces; the rest of DEPLOY_BINARIES are repo scripts.
DEPLOY_SCRIPTS = {"rsid-supervisor.sh", "cargo-slot", "target-pool", "rsi-spill"}
RELEASE_BINS = [
    "rsi",
    "rsid",
    "rsi-rpc",
    "rsi-agent-mcp",
    "rsi-build-rustc",
    "rsi-contract-validate",
    "rsi-turn-shim",
    "rsi-socket-hold",
]


def write_executable(path, text):
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(text)
    path.chmod(0o755)


def consumer_deploy_binaries():
    """DEPLOY_BINARIES as rsi-common declares it: the names the deploy reads."""
    source = AGENT_DEPLOY_RS.read_text()
    block = re.search(r"pub const DEPLOY_BINARIES: \[&str; \d+\] = \[(.*?)\];", source, re.S)
    return re.findall(r'"([^"]+)"', block.group(1))


def snapshot(*roots):
    """Every path under the roots with its kind, mode, link target and content hash."""
    state = {}
    for root in roots:
        if not root.exists() and not root.is_symlink():
            state[str(root)] = None
            continue
        for path in [root, *sorted(root.rglob("*"))]:
            if path.is_symlink():
                state[str(path)] = ("link", os.readlink(path))
            elif path.is_file():
                stat = path.stat()
                state[str(path)] = (
                    "file",
                    hashlib.sha256(path.read_bytes()).hexdigest(),
                    stat.st_mtime_ns,
                    stat.st_mode,
                )
            else:
                state[str(path)] = ("dir", path.stat().st_mode)
    return state


class InstallReleaseStageTest(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix="rsi-install-stage-")
        self.addCleanup(self.temp.cleanup)
        self.base = Path(self.temp.name)
        self.home = self.base / "home"
        self.rsi_home = self.home / ".rsi"
        self.bin_dir = self.home / ".local" / "bin"
        self.root = self.base / "repo"
        self.target = self.base / "target"
        self.fakes = self.base / "fakes"
        self.called = self.base / "called"
        self.called.mkdir()
        # `cargo build` is recorded here (not in `called`, which must stay empty).
        self.cargo_builds = self.base / "cargo-builds"
        self.cargo_builds.mkdir()
        write_executable(
            self.fakes / "cargo",
            f'#!/bin/sh\n[ "$1" = build ] && touch "{self.cargo_builds}/build"\n'
            "printf '{\"target_directory\":\"%s\"}\\n' \"$CARGO_TARGET_DIR\"\n",
        )
        write_executable(self.fakes / "rustup", "#!/bin/sh\nexit 99\n")
        # A restart or supervisor start would go through one of these.
        for name in ("systemctl", "systemd-run", "nohup"):
            write_executable(
                self.fakes / name,
                f'#!/bin/sh\ntouch "{self.called}/{name}"\nexit 99\n',
            )
        shutil.copytree(SCRIPTS, self.root / "scripts", ignore=shutil.ignore_patterns("tests"))
        (self.root / "Cargo.toml").write_text(
            '[workspace]\n[package]\nname = "stub"\nversion = "0.0.0"\nedition = "2021"\n'
        )
        write_executable(self.root / "src" / "lib.rs", "")
        for name in RELEASE_BINS:
            write_executable(
                self.target / "release" / name,
                '#!/bin/sh\necho "fake-%s $*"\n' % name,
            )
        (self.target / "release" / ".rsi-release-source").write_text(
            f"worktree={self.root.resolve()}\nstate=built\nhead=unknown\n"
        )
        # A pre-existing live install the staging run must leave alone.
        write_executable(self.rsi_home / "install" / "rsid", "#!/bin/sh\necho live-rsid\n")
        write_executable(self.rsi_home / "install" / "rsid-supervisor.sh", "#!/bin/sh\necho live-supervisor\n")
        write_executable(self.rsi_home / "bin" / "cargo-slot", "#!/bin/sh\necho live-wrapper\n")
        write_executable(self.rsi_home / "bin" / "target-pool", "#!/bin/sh\necho live-pool\n")
        write_executable(self.rsi_home / "lib" / "cargo-slot", "#!/bin/sh\necho live-slot\n")
        self.bin_dir.mkdir(parents=True)
        (self.bin_dir / "rsid").symlink_to(self.rsi_home / "install" / "rsid")
        self.stage = self.base / "stage"

    def live_state(self):
        return snapshot(self.rsi_home, self.bin_dir, self.root / "target")

    def run_script(self, *args, check=True):
        env = dict(
            os.environ,
            HOME=str(self.home),
            CARGO_TARGET_DIR=str(self.target),
            PATH=f"{self.fakes}:{os.environ['PATH']}",
        )
        env.pop("RSI_INSTALL_BIN_DIR", None)
        return subprocess.run(
            ["bash", str(self.root / "scripts" / "install-release.sh"), *args],
            env=env,
            text=True,
            capture_output=True,
            timeout=30,
            check=check,
        )

    def test_stage_dir_writes_the_set_and_touches_nothing_live(self):
        before = self.live_state()
        result = self.run_script("--link-only", "--stage-dir", str(self.stage))
        self.assertEqual(self.live_state(), before)
        self.assertEqual(list(self.called.iterdir()), [])
        for name in RELEASE_BINS:
            staged = self.stage / name
            self.assertTrue(staged.is_file() and not staged.is_symlink(), name)
            self.assertTrue(os.access(staged, os.X_OK), name)
        self.assertEqual(
            (self.stage / "rsid-supervisor.sh").read_bytes(),
            (self.root / "scripts" / "rsid-supervisor.sh").read_bytes(),
        )
        self.assertEqual(
            (self.stage / "cargo-slot").read_bytes(),
            (self.root / "scripts" / "cargo-slot").read_bytes(),
        )
        self.assertEqual(
            (self.stage / "target-pool").read_bytes(),
            (self.root / "scripts" / "target-pool").read_bytes(),
        )
        # hub #2958: rsi-spill travels with the other copied tools.
        self.assertEqual(
            (self.stage / "rsi-spill").read_bytes(),
            (self.root / "scripts" / "rsi-spill").read_bytes(),
        )
        self.assertTrue(os.access(self.stage / "rsi-spill", os.X_OK))
        self.assertNotIn("Linked:", result.stdout)

    def test_stage_dir_prints_a_sha256_manifest_of_every_staged_file(self):
        result = self.run_script("--link-only", f"--stage-dir={self.stage}")
        staged = sorted(
            p.relative_to(self.stage).as_posix()
            for p in self.stage.rglob("*")
            if p.is_file() and p.name != "MANIFEST.sha256"
        )
        self.assertIn("rsid", staged)
        self.assertIn("rsid-supervisor.sh", staged)
        for rel in staged:
            digest = hashlib.sha256((self.stage / rel).read_bytes()).hexdigest()
            self.assertIn(f"{digest}  {rel}", result.stdout.splitlines(), rel)
        written = (self.stage / "MANIFEST.sha256").read_text().splitlines()
        self.assertEqual([line.split("  ", 1)[1] for line in written], staged)

    def test_stage_dir_refuses_a_live_path(self):
        before = self.live_state()
        for live in (
            self.rsi_home / "install",
            self.rsi_home / "bin",
            self.rsi_home / "lib",
            self.bin_dir,
            self.rsi_home,
            self.rsi_home / "install" / "nested",
        ):
            result = self.run_script("--link-only", "--stage-dir", str(live), check=False)
            self.assertNotEqual(result.returncode, 0, live)
            self.assertIn("overlaps the live path", result.stderr, live)
        self.assertEqual(self.live_state(), before)

    def test_stage_dir_cannot_combine_with_tui_only(self):
        result = self.run_script("--tui-only", "--stage-dir", str(self.stage), check=False)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("--tui-only", result.stderr)
        self.assertFalse(self.stage.exists())

    def test_stage_dir_needs_a_directory(self):
        result = self.run_script("--stage-dir", check=False)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("--stage-dir needs a directory", result.stderr)

    def test_help_says_no_restart_is_not_staging(self):
        result = self.run_script("--help")
        self.assertIn("--stage-dir DIR", result.stdout)
        no_restart = result.stdout.split("\n  --no-restart ", 1)[1].split("\n  --link-only ", 1)[0]
        self.assertIn("not staging", no_restart)
        self.assertIn("overwritten", no_restart)

    def test_no_restart_still_overwrites_the_live_install(self):
        # The documented meaning of --no-restart: install, do not restart.
        self.run_script("--link-only", "--no-restart")
        self.assertIn("fake-rsid", (self.rsi_home / "install" / "rsid").read_text())
        self.assertEqual(list(self.called.iterdir()), [])


    # --- #262 / hub #2800: never write through a path already inside DIR -----

    def test_stage_dir_never_writes_through_a_symlink_inside_dir(self):
        self.stage.mkdir()
        (self.stage / "tools").symlink_to(self.rsi_home / "bin")
        (self.stage / "MANIFEST.sha256").symlink_to(self.rsi_home / "lib" / "cargo-slot")
        before = self.live_state()
        planted = snapshot(self.stage)
        result = self.run_script("--link-only", "--stage-dir", str(self.stage), check=False)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("not an empty directory", result.stderr)
        self.assertEqual(self.live_state(), before)
        self.assertEqual(snapshot(self.stage), planted)

    def test_stage_dir_fills_an_empty_dir_as_one_fresh_tree(self):
        self.stage.mkdir()
        self.run_script("--link-only", "--stage-dir", str(self.stage))
        self.assertTrue((self.stage / "rsid").is_file())
        self.assertTrue((self.stage / "MANIFEST.sha256").is_file())
        # The private tree was renamed into place: the parent holds DIR alone.
        self.assertEqual(sorted(p.name for p in self.base.iterdir() if p.name.startswith(".")), [])
        self.assertEqual(self.stage.stat().st_mode & 0o777, 0o755)

    # --- #262 / hub #2801: a staging build never changes a live link's file --

    def test_stage_build_refuses_a_live_link_into_the_build_output(self):
        (self.bin_dir / "rsi").symlink_to(self.target / "release" / "rsi")
        before = self.live_state()
        release_before = snapshot(self.target / "release")
        result = self.run_script("--stage-dir", str(self.stage), check=False)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("resolve into the build output", result.stderr)
        self.assertIn(str(self.bin_dir / "rsi"), result.stderr)
        self.assertEqual(list(self.cargo_builds.iterdir()), [])
        self.assertEqual(snapshot(self.target / "release"), release_before)
        self.assertTrue(os.access(self.target / "release" / "rsi", os.X_OK))
        self.assertEqual(self.live_state(), before)
        self.assertFalse(self.stage.exists())

    def test_stage_build_runs_when_no_live_link_resolves_into_the_output(self):
        before = self.live_state()
        self.run_script("--stage-dir", str(self.stage))
        self.assertEqual([p.name for p in self.cargo_builds.iterdir()], ["build"])
        self.assertTrue((self.stage / "rsid").is_file())
        self.assertEqual(self.live_state(), before)
        self.assertEqual(list(self.called.iterdir()), [])

    # --- #262 / hub #2799: producer (stage) matches consumer (deploy) --------

    def test_staged_names_are_the_top_level_files_the_deploy_reads(self):
        expected = consumer_deploy_binaries()
        self.assertIn("cargo-slot", expected)
        self.assertIn("target-pool", expected)
        self.assertIn("rsi-spill", expected)
        # deploy.rs stage_all reads each member as `dir.join(name)`, top level.
        deploy = DEPLOY_RS.read_text()
        self.assertIn("for name in DEPLOY_BINARIES", deploy)
        self.assertIn("let source = dir.join(name);", deploy)
        for name in expected:
            if name not in DEPLOY_SCRIPTS:
                write_executable(self.target / "release" / name, f"#!/bin/sh\necho fake-{name}\n")
        self.run_script("--link-only", "--stage-dir", str(self.stage))
        self.assertEqual(
            sorted(p.name for p in self.stage.iterdir()),
            sorted([*expected, "MANIFEST.sha256"]),
        )
        for name in expected:
            staged = self.stage / name
            # The consumer requires a regular file (symlink_metadata is_file).
            self.assertTrue(staged.is_file() and not staged.is_symlink(), name)
            self.assertTrue(os.access(staged, os.X_OK), name)
        # cargo-slot is the full implementation, never the ~/.rsi/bin wrapper.
        self.assertEqual(
            (self.stage / "cargo-slot").read_bytes(),
            (self.root / "scripts" / "cargo-slot").read_bytes(),
        )
        self.assertEqual(
            (self.stage / "target-pool").read_bytes(),
            (self.root / "scripts" / "target-pool").read_bytes(),
        )
        # hub #2958: rsi-spill travels with the other copied tools.
        self.assertEqual(
            (self.stage / "rsi-spill").read_bytes(),
            (self.root / "scripts" / "rsi-spill").read_bytes(),
        )
        self.assertTrue(os.access(self.stage / "rsi-spill", os.X_OK))

    # --- #262 / hub #2802: no GNU-only readlink ------------------------------

    def test_stage_dir_works_with_a_readlink_lacking_gnu_flags(self):
        real = shutil.which("readlink") or "/usr/bin/readlink"
        write_executable(
            self.fakes / "readlink",
            "#!/bin/sh\n"
            'for a in "$@"; do case "$a" in -m|-e|--canonicalize*) '
            'echo "readlink: illegal option -- $a" >&2; exit 1;; esac; done\n'
            f'exec "{real}" "$@"\n',
        )
        # Overlap detection still resolves a symlinked path to the live one.
        alias = self.base / "alias-of-rsi-home"
        alias.symlink_to(self.rsi_home)
        before = self.live_state()
        result = self.run_script("--link-only", "--stage-dir", str(alias / "bin" / "new"), check=False)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("overlaps the live path", result.stderr)
        self.assertEqual(self.live_state(), before)
        result = self.run_script("--link-only", "--stage-dir", str(self.stage))
        self.assertNotIn("illegal option", result.stderr)
        self.assertTrue((self.stage / "rsid").is_file())
        self.assertEqual(self.live_state(), before)

if __name__ == "__main__":
    unittest.main()
