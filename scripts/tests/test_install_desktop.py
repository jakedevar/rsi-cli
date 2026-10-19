"""Exercise desktop installer platform selection with isolated fake builds."""
import json
import os
from pathlib import Path
import plistlib
import shutil
import subprocess
import tempfile
import unittest

SCRIPT = Path(__file__).resolve().parents[1] / "install-desktop.sh"


class InstallDesktopTest(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix="rsi-desktop-install-")
        self.addCleanup(self.temp.cleanup)
        self.base = Path(self.temp.name)
        self.fake_bin = self.base / "bin"
        self.fake_bin.mkdir()
        self.root = self.make_root("repo")
        self.target = self.base / "target"
        self.executable(self.target / "release/rsi-desktop", "exit 0")
        self.stub("node", "exit 0")
        self.stub("npm", 'echo "$*" >> "$BUILD_CALLS"')
        metadata = json.dumps({"target_directory": str(self.target), "packages": [{"name": "rsi-desktop"}]})
        self.stub("cargo", "case \"$1\" in metadata) echo '" + metadata + "' ;; *) echo \"$*\" >> \"$BUILD_CALLS\" ;; esac")
        self.stub("xcrun", "exit 0")
        self.stub("pkg-config", "exit 1")
        self.env = dict(os.environ, HOME=str(self.base / "home"),
                        PATH=str(self.fake_bin) + ":/usr/bin:/bin",
                        RSI_DESKTOP_INSTALL_DIR=str(self.base / "install"),
                        RSI_INSTALL_BIN_DIR=str(self.base / "installed-bin"),
                        RSI_DESKTOP_APP_DIR=str(self.base / "Applications"),
                        XDG_DATA_HOME=str(self.base / "data"),
                        BUILD_CALLS=str(self.base / "calls"))

    def make_root(self, name):
        """A checkout of the installer and the desktop files it needs, as one worktree."""
        root = self.base / name
        (root / "scripts").mkdir(parents=True)
        shutil.copy2(SCRIPT, root / "scripts/install-desktop.sh")
        shutil.copy2(SCRIPT.with_name("release-target-guard.sh"), root / "scripts/release-target-guard.sh")
        desktop = root / "desktop"
        (desktop / "node_modules").mkdir(parents=True)
        (desktop / "package-lock.json").write_text("{}")
        (desktop / "packaging").mkdir()
        (desktop / "packaging/rsi-desktop.desktop").write_text("Exec=@BIN@\n")
        icons = desktop / "src-tauri/icons"
        icons.mkdir(parents=True)
        for icon in ["32x32.png", "128x128.png", "icon.png", "icon.icns"]:
            (icons / icon).write_bytes(b"icon:" + icon.encode())
        # The macOS bundle step reads the app name, identifier and version from here.
        (desktop / "src-tauri/tauri.conf.json").write_text(json.dumps(
            {"productName": "RSI Desktop", "identifier": "dev.rsi.desktop", "version": "0.1.0"}))
        return root

    def executable(self, path, body):
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text("#!/bin/sh\n" + body + "\n")
        path.chmod(0o755)

    def stub(self, name, body):
        self.executable(self.fake_bin / name, body)

    def run_install(self, platform, *flags, root=None):
        self.stub("uname", "echo " + platform)
        root = root or self.root
        return subprocess.run(["bash", str(root / "scripts/install-desktop.sh"), *flags],
                              env=self.env, text=True, capture_output=True)

    def test_mac_installs_using_system_webkit(self):
        result = self.run_install("Darwin", "--required")
        self.assertEqual(result.returncode, 0, result.stderr)
        link = self.base / "installed-bin/rsi-desktop"
        self.assertEqual(link.resolve(), self.base / "install/rsi-desktop")
        self.assertTrue(os.access(link, os.X_OK))
        self.assertIn("build --release", (self.base / "calls").read_text())
        self.assertIn("Desktop UI installed:", result.stdout)
        app = self.base / "Applications/RSI Desktop.app/Contents"
        self.assertEqual((app / "MacOS/rsi-desktop").read_text(), "#!/bin/sh\nexit 0\n")
        self.assertTrue(os.access(app / "MacOS/rsi-desktop", os.X_OK))
        self.assertEqual((app / "Resources/icon.icns").read_bytes(), b"icon:icon.icns")
        with open(app / "Info.plist", "rb") as fh:
            info = plistlib.load(fh)
        self.assertEqual(info["CFBundleName"], "RSI Desktop")
        self.assertEqual(info["CFBundleIdentifier"], "dev.rsi.desktop")
        self.assertEqual(info["CFBundleShortVersionString"], "0.1.0")
        self.assertEqual(info["CFBundleExecutable"], "rsi-desktop")
        self.assertEqual(info["CFBundleIconFile"], "icon.icns")
        self.assertIn(f"{self.base}/Applications/RSI Desktop.app", result.stdout)

    def test_mac_build_only_leaves_the_bundle_in_the_target(self):
        result = self.run_install("Darwin", "--build-only")
        self.assertEqual(result.returncode, 0, result.stderr)
        built = self.target / "release/bundle/macos/RSI Desktop.app/Contents"
        self.assertEqual((built / "Resources/icon.icns").read_bytes(), b"icon:icon.icns")
        with open(built / "Info.plist", "rb") as fh:
            self.assertEqual(plistlib.load(fh)["CFBundleIdentifier"], "dev.rsi.desktop")
        self.assertIn(f"built: {built.parent}", result.stdout)
        self.assertFalse((self.base / "Applications").exists(), "--build-only installs nothing")

    def test_mac_reports_missing_xcode(self):
        self.stub("xcrun", "exit 1")
        result = self.run_install("Darwin", "--required")
        self.assertEqual(result.returncode, 1)
        self.assertIn("xcode-select --install", result.stderr)

    def test_linux_requires_webkitgtk(self):
        result = self.run_install("Linux", "--required")
        self.assertEqual(result.returncode, 1)
        self.assertIn("webkit2gtk-4.1", result.stderr)

    def test_linux_installs_menu_and_icons(self):
        self.stub("pkg-config", "exit 0")
        result = self.run_install("Linux", "--required")
        self.assertEqual(result.returncode, 0, result.stderr)
        entry = self.base / "data/applications/rsi-desktop.desktop"
        self.assertEqual(entry.read_text(), f"Exec={self.base}/installed-bin/rsi-desktop\n")
        for size in ["32x32", "128x128", "512x512"]:
            self.assertTrue((self.base / f"data/icons/hicolor/{size}/apps/rsi-desktop.png").is_file())

    def build_calls(self):
        path = self.base / "calls"
        return path.read_text().splitlines() if path.exists() else []

    def clean_calls(self):
        return [call for call in self.build_calls() if call.startswith("clean ")]

    def marker(self):
        return (self.target / "release/.rsi-desktop-source").read_text()

    def test_a_second_worktree_cleans_the_desktop_crate_the_first_built_in_the_shared_target(self):
        # #2177 (#938): two worktrees, one target. Cargo's mtime freshness would otherwise link
        # the first tree's rsi-desktop into the second tree's binary.
        self.stub("pkg-config", "exit 0")
        other = self.make_root("other-worktree")
        (self.target / "release/.fingerprint").mkdir(parents=True)

        first = self.run_install("Linux", "--build-only", root=self.root)
        self.assertEqual(first.returncode, 0, first.stderr)
        self.assertIn(f"worktree={self.root.resolve()}\n", self.marker())
        self.assertIn("state=built\n", self.marker())
        # The first build found artifacts with no record: its crate is cleaned once.
        self.assertEqual(len(self.clean_calls()), 1, self.build_calls())

        again = self.run_install("Linux", "--build-only", root=self.root)
        self.assertEqual(again.returncode, 0, again.stderr)
        self.assertEqual(len(self.clean_calls()), 1, "the same worktree reuses its own build")

        switched = self.run_install("Linux", "--build-only", root=other)
        self.assertEqual(switched.returncode, 0, switched.stderr)
        self.assertIn(f"was last built from {self.root.resolve()}", switched.stderr)
        calls = self.build_calls()
        self.assertEqual(len(self.clean_calls()), 2, calls)
        self.assertRegex(self.clean_calls()[-1],
                         r"^clean --release --manifest-path \S*other-worktree/desktop/src-tauri/Cargo.toml -p rsi-desktop$")
        # The clean precedes the other worktree's build.
        self.assertLess(max(i for i, call in enumerate(calls) if call.startswith("clean ")),
                        max(i for i, call in enumerate(calls) if call.startswith("build ")))
        self.assertIn(f"worktree={other.resolve()}\n", self.marker())
        self.assertIn("state=built\n", self.marker())

    def test_a_fresh_target_is_not_cleaned_and_records_its_builder(self):
        self.stub("pkg-config", "exit 0")
        result = self.run_install("Linux", "--build-only")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(self.clean_calls(), [])
        self.assertIn(f"worktree={self.root.resolve()}\n", self.marker())

    def test_the_desktop_record_does_not_disturb_the_release_install_record(self):
        # install-release.sh keeps `.rsi-release-source`; the desktop build uses its own key.
        self.stub("pkg-config", "exit 0")
        release = self.target / "release"
        (release / ".fingerprint").mkdir(parents=True)
        (release / ".rsi-release-source").write_text("worktree=/elsewhere\nstate=built\n")
        result = self.run_install("Linux", "--build-only")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual((release / ".rsi-release-source").read_text(), "worktree=/elsewhere\nstate=built\n")


if __name__ == "__main__":
    unittest.main()
