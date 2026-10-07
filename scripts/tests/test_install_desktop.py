"""Exercise desktop installer platform selection with isolated fake builds."""
import json
import os
from pathlib import Path
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
        self.root = self.base / "repo"
        self.fake_bin = self.base / "bin"
        self.fake_bin.mkdir()
        (self.root / "scripts").mkdir(parents=True)
        shutil.copy2(SCRIPT, self.root / "scripts/install-desktop.sh")
        self.desktop = self.root / "desktop"
        (self.desktop / "node_modules").mkdir(parents=True)
        (self.desktop / "package-lock.json").write_text("{}")
        (self.desktop / "packaging").mkdir()
        (self.desktop / "packaging/rsi-desktop.desktop").write_text("Exec=@BIN@\n")
        icons = self.desktop / "src-tauri/icons"
        icons.mkdir(parents=True)
        for name in ["32x32.png", "128x128.png", "icon.png"]:
            (icons / name).write_bytes(b"icon")
        self.target = self.base / "target"
        self.executable(self.target / "release/rsi-desktop", "exit 0")
        self.stub("node", "exit 0")
        self.stub("npm", 'echo "$*" >> "$BUILD_CALLS"')
        metadata = json.dumps({"target_directory": str(self.target)})
        self.stub("cargo", "case \"$1\" in metadata) echo '" + metadata + "' ;; *) echo \"$*\" >> \"$BUILD_CALLS\" ;; esac")
        self.stub("xcrun", "exit 0")
        self.stub("pkg-config", "exit 1")
        self.env = dict(os.environ, HOME=str(self.base / "home"),
                        PATH=str(self.fake_bin) + ":/usr/bin:/bin",
                        RSI_DESKTOP_INSTALL_DIR=str(self.base / "install"),
                        RSI_INSTALL_BIN_DIR=str(self.base / "installed-bin"),
                        XDG_DATA_HOME=str(self.base / "data"),
                        BUILD_CALLS=str(self.base / "calls"))

    def executable(self, path, body):
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text("#!/bin/sh\n" + body + "\n")
        path.chmod(0o755)

    def stub(self, name, body):
        self.executable(self.fake_bin / name, body)

    def run_install(self, platform, *flags):
        self.stub("uname", "echo " + platform)
        return subprocess.run(["bash", str(self.root / "scripts/install-desktop.sh"), *flags],
                              env=self.env, text=True, capture_output=True)

    def test_mac_installs_using_system_webkit(self):
        result = self.run_install("Darwin", "--required")
        self.assertEqual(result.returncode, 0, result.stderr)
        link = self.base / "installed-bin/rsi-desktop"
        self.assertEqual(link.resolve(), self.base / "install/rsi-desktop")
        self.assertTrue(os.access(link, os.X_OK))
        self.assertIn("build --release", (self.base / "calls").read_text())
        self.assertIn("Desktop UI installed:", result.stdout)

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


if __name__ == "__main__":
    unittest.main()
