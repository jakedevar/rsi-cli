"""#1137: install-release.sh links scripts/rsi-spill next to rsi-rpc, and the
link runs as printed (the spill stub says a bare `rsi-spill show ...`)."""

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

    def install(self, *flags):
        env = dict(os.environ, HOME=str(self.home), CARGO_TARGET_DIR=str(self.target))
        env.pop("RSI_INSTALL_BIN_DIR", None)
        return subprocess.run(
            ["bash", str(self.root / "scripts" / "install-release.sh"), "--link-only", "--no-restart", *flags],
            env=env,
            text=True,
            capture_output=True,
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
        )

    def test_link_only_links_rsi_spill_next_to_rsi_rpc(self):
        out = self.install().stdout
        link = self.bin_dir / "rsi-spill"
        self.assertTrue((self.bin_dir / "rsi-rpc").is_symlink())
        self.assertTrue(link.is_symlink())
        self.assertEqual(link.resolve(), (self.root / "scripts" / "rsi-spill").resolve())
        self.assertIn(f"{link} ->", out)

    def test_link_only_installs_rsid_under_rsi_install_and_links_there(self):
        # #1164: the supervisor, the links and a deploy share one rsid path.
        self.install()
        installed = self.home / ".rsi" / "install"
        for name in RELEASE_BINS:
            copy = installed / name
            self.assertTrue(copy.is_file() and not copy.is_symlink(), name)
            self.assertTrue(os.access(copy, os.X_OK), name)
        self.assertEqual((self.bin_dir / "rsid").resolve(), (installed / "rsid").resolve())
        self.assertNotEqual(
            (self.bin_dir / "rsid").resolve(), (self.target / "release" / "rsid").resolve()
        )
        # Reinstalling replaces the file atomically with the new build.
        write_executable(self.target / "release" / "rsid", '#!/bin/sh\necho "fake-rsid-v2"\n')
        self.install()
        self.assertIn("fake-rsid-v2", (installed / "rsid").read_text())

    def test_linked_rsi_spill_runs_from_another_cwd_via_path_rsi_rpc(self):
        self.install()
        elsewhere = self.base / "elsewhere"
        elsewhere.mkdir()
        result = self.run_linked(elsewhere, f"{self.bin_dir}:/usr/bin:/bin")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(result.stdout.strip(), "fake-rsi-rpc spill show s/1 --range 1:3")

    def test_linked_rsi_spill_resolves_its_own_location_through_the_link(self):
        # No rsi-rpc on PATH: the script must find ../target/debug/rsi-rpc
        # relative to the real script, not to the symlink in ~/.local/bin.
        self.install()
        (self.bin_dir / "rsi-rpc").unlink()
        write_executable(
            self.root / "target" / "debug" / "rsi-rpc",
            '#!/bin/sh\necho "debug-rsi-rpc $*"\n',
        )
        elsewhere = self.base / "elsewhere"
        elsewhere.mkdir()
        result = self.run_linked(elsewhere, f"{self.bin_dir}:/usr/bin:/bin")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(result.stdout.strip(), "debug-rsi-rpc spill show s/1 --range 1:3")


if __name__ == "__main__":
    unittest.main()
