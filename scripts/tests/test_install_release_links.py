"""#1137: install-release.sh links rsi-spill next to rsi-rpc, and the link runs
as printed (the spill stub says a bare `rsi-spill show ...`). hub #2958: the link
resolves to a stable copy under ~/.rsi/bin, never into the installing checkout."""

import os
import shutil
import subprocess
import tempfile
import unittest
from pathlib import Path

SCRIPTS = Path(__file__).resolve().parents[1]
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


class InstallReleaseLinksTest(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix="rsi-install-links-")
        self.addCleanup(self.temp.cleanup)
        self.base = Path(self.temp.name)
        self.home = self.base / "home"
        self.bin_dir = self.home / ".local" / "bin"
        self.root = self.base / "repo"
        self.target = self.base / "target"
        self.fakes = self.base / "fakes"
        write_executable(
            self.fakes / "cargo",
            "#!/bin/sh\nprintf '{\"target_directory\":\"%s\"}\\n' \"$CARGO_TARGET_DIR\"\n",
        )
        write_executable(self.fakes / "rustup", '#!/bin/sh\nexit 99\n')
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
        # #938: --link-only links only what this checkout built.
        (self.target / "release" / ".rsi-release-source").write_text(
            f"worktree={self.root.resolve()}\nstate=built\nhead=unknown\n"
        )

    def install(self, *flags):
        env = dict(os.environ, HOME=str(self.home), CARGO_TARGET_DIR=str(self.target), PATH=f"{self.fakes}:{os.environ['PATH']}")
        env.pop("RSI_INSTALL_BIN_DIR", None)
        return subprocess.run(
            ["bash", str(self.root / "scripts" / "install-release.sh"), "--link-only", "--no-restart", *flags],
            env=env,
            text=True,
            capture_output=True,
            timeout=10,
            check=True,
        )

    def run_linked(self, cwd, path_env):
        env = {"HOME": str(self.home), "PATH": path_env}
        return subprocess.run(
            ["rsi-spill", "show", "s/1", "--range", "1:3"],
            cwd=cwd,
            env=env,
            text=True,
            capture_output=True,
            timeout=10,
        )

    def test_link_only_links_rsi_spill_next_to_rsi_rpc(self):
        out = self.install().stdout
        link = self.bin_dir / "rsi-spill"
        self.assertTrue((self.bin_dir / "rsi-rpc").is_symlink())
        self.assertTrue(link.is_symlink())
        stable = self.home / ".rsi" / "bin" / "rsi-spill"
        self.assertEqual(link.resolve(), stable.resolve())
        self.assertIn(f"{link} ->", out)
        self.assertEqual(stable.read_bytes(), (self.root / "scripts" / "rsi-spill").read_bytes())
        self.assertTrue(stable.is_file() and not stable.is_symlink())
        self.assertEqual(stable.stat().st_mode & 0o777, 0o755)
        self.assertEqual(list(stable.parent.glob(".rsi-spill.*")), [])

    def test_rsi_spill_link_never_resolves_into_the_checkout(self):
        self.install()
        resolved = (self.bin_dir / "rsi-spill").resolve()
        checkout = self.root.resolve()
        self.assertNotIn(checkout, resolved.parents)

    def test_linked_rsi_spill_survives_removal_of_the_checkout_copy(self):
        self.install()
        shutil.rmtree(self.root)
        link = self.bin_dir / "rsi-spill"
        self.assertTrue(link.exists(), "link dangles after the checkout is removed")
        elsewhere = self.base / "elsewhere"
        elsewhere.mkdir()
        result = self.run_linked(elsewhere, f"{self.bin_dir}:/usr/bin:/bin")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(result.stdout.strip(), "fake-rsi-rpc spill show s/1 --range 1:3")

    def test_rsi_spill_is_atomically_refreshed_and_repoints_an_old_checkout_link(self):
        stable = self.home / ".rsi" / "bin" / "rsi-spill"
        # An older install linked the checkout script directly.
        self.bin_dir.mkdir(parents=True)
        (self.bin_dir / "rsi-spill").symlink_to(self.root / "scripts" / "rsi-spill")
        self.install()
        self.assertEqual((self.bin_dir / "rsi-spill").resolve(), stable.resolve())
        source = self.root / "scripts" / "rsi-spill"
        original = stable.read_bytes()
        with stable.open("rb") as old_copy:
            source.write_bytes(original + b"\n# updated release\n")
            self.install()
            self.assertEqual(old_copy.read(), original)
        self.assertEqual(stable.read_bytes(), source.read_bytes())
        self.assertEqual(list(stable.parent.glob(".rsi-spill.*")), [])

    def test_link_only_installs_rsid_under_rsi_install_and_links_there(self):
        # #1164: the supervisor, the links and a deploy share one rsid path.
        self.install()
        installed = self.home / ".rsi" / "install"
        for name in RELEASE_BINS:
            copy = installed / name
            self.assertTrue(copy.is_file() and not copy.is_symlink(), name)
            self.assertTrue(os.access(copy, os.X_OK), name)
        self.assertEqual((self.bin_dir / "rsi-socket-hold").resolve(), (installed / "rsi-socket-hold").resolve())
        self.assertEqual((self.bin_dir / "rsi-turn-shim").resolve(), (installed / "rsi-turn-shim").resolve())
        self.assertEqual((self.bin_dir / "rsid").resolve(), (installed / "rsid").resolve())
        self.assertNotEqual(
            (self.bin_dir / "rsid").resolve(), (self.target / "release" / "rsid").resolve()
        )
        # Reinstalling replaces the file atomically with the new build.
        write_executable(self.target / "release" / "rsid", '#!/bin/sh\necho "fake-rsid-v2"\n')
        self.install()
        self.assertIn("fake-rsid-v2", (installed / "rsid").read_text())

    def test_cargo_slot_is_installed_and_atomically_refreshed(self):
        source = self.root / "scripts" / "cargo-slot"
        installed = self.home / ".rsi" / "bin" / "cargo-slot"
        implementation = self.home / ".rsi" / "lib" / "cargo-slot"
        wrapper = self.root / "scripts" / "cargo-slot-wrapper"
        self.install()
        self.assertEqual(installed.read_bytes(), wrapper.read_bytes())
        self.assertEqual(implementation.read_bytes(), source.read_bytes())
        self.assertEqual(installed.stat().st_mode & 0o777, 0o755)
        self.assertEqual(implementation.stat().st_mode & 0o777, 0o755)

        # An open reader keeps the old implementation while install refreshes it.
        original = implementation.read_bytes()
        with implementation.open("rb") as old_copy:
            source.write_bytes(original + b"\n# updated release\n")
            source.chmod(0o600)
            self.install()
            self.assertEqual(old_copy.read(), original)
        self.assertEqual(implementation.read_bytes(), source.read_bytes())
        self.assertEqual(installed.read_bytes(), wrapper.read_bytes())
        self.assertEqual(implementation.stat().st_mode & 0o777, 0o755)
        self.assertEqual(list(installed.parent.glob(".cargo-slot.*")), [])
        self.assertEqual(list(implementation.parent.glob(".cargo-slot.*")), [])

    def test_target_pool_is_installed_for_the_daemon_seed(self):
        # #1741: the daemon runs ~/.rsi/bin/target-pool to seed worker targets.
        source = self.root / "scripts" / "target-pool"
        installed = self.home / ".rsi" / "bin" / "target-pool"
        self.install()
        self.assertEqual(installed.read_bytes(), source.read_bytes())
        self.assertEqual(installed.stat().st_mode & 0o777, 0o755)
        self.assertEqual(list(installed.parent.glob(".target-pool.*")), [])

    def test_linked_rsi_spill_runs_from_another_cwd_via_path_rsi_rpc(self):
        self.install()
        elsewhere = self.base / "elsewhere"
        elsewhere.mkdir()
        result = self.run_linked(elsewhere, f"{self.bin_dir}:/usr/bin:/bin")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(result.stdout.strip(), "fake-rsi-rpc spill show s/1 --range 1:3")

    def test_linked_rsi_spill_falls_back_to_the_installed_rsi_rpc(self):
        # No rsi-rpc on PATH: the stable copy has no checkout beside it, so it
        # finds the installed ~/.rsi/install/rsi-rpc instead.
        self.install()
        (self.bin_dir / "rsi-rpc").unlink()
        write_executable(
            self.home / ".rsi" / "install" / "rsi-rpc",
            '#!/bin/sh\necho "debug-rsi-rpc $*"\n',
        )
        elsewhere = self.base / "elsewhere"
        elsewhere.mkdir()
        result = self.run_linked(elsewhere, f"{self.bin_dir}:/usr/bin:/bin")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(result.stdout.strip(), "debug-rsi-rpc spill show s/1 --range 1:3")


if __name__ == "__main__":
    unittest.main()
