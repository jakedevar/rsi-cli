"""Satellite unit templates retry forever, and install-direct.sh protects other instances."""
import configparser
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest

SATELLITE = Path(__file__).resolve().parents[1] / "satellite"
INSTALL = SATELLITE / "install-direct.sh"
DIRECT = SATELLITE / "rsi-satellite-direct@.service"
DIAL_HOME = SATELLITE / "rsi-satellite@.service"
UNIT_NAME = "rsi-satellite-direct@.service"


def parse_unit(path):
    parser = configparser.ConfigParser(strict=False, interpolation=None, comment_prefixes=("#",))
    parser.optionxform = str
    parser.read_string(path.read_text(encoding="utf-8"))
    return parser


class TemplateContentTest(unittest.TestCase):
    def test_templates_retry_forever_with_backoff(self):
        for path in (DIRECT, DIAL_HOME):
            with self.subTest(template=path.name):
                unit = parse_unit(path)
                self.assertEqual(unit["Unit"]["StartLimitIntervalSec"], "0")
                self.assertNotIn("StartLimitBurst", unit["Unit"])
                service = unit["Service"]
                self.assertEqual(service["Restart"], "always")
                self.assertEqual(service["RestartSec"], "20s")
                self.assertEqual(service["RestartSteps"], "6")
                self.assertEqual(service["RestartMaxDelaySec"], "5min")

    def test_templates_document_the_old_systemd_fallback(self):
        # Keys unknown to systemd < 254 are ignored, leaving the fixed RestartSec retry.
        for path in (DIRECT, DIAL_HOME):
            with self.subTest(template=path.name):
                text = path.read_text(encoding="utf-8")
                self.assertIn("systemd >= 254", text)
                self.assertLess(text.index("RestartSec=20s"), text.index("RestartSteps=6"))


@unittest.skipUnless(
    sys.platform.startswith("linux"),
    "install-direct.sh is a Linux systemd installer (needs GNU timeout and stat -c)",
)
class InstallDirectOverwriteTest(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix="rsi-idt-", dir="/tmp")  # short: Unix socket paths cap at 107 bytes
        self.addCleanup(self.temp.cleanup)
        base = Path(self.temp.name)
        self.home = base / "home"
        self.bin = base / "bin"
        self.bin.mkdir()
        self.unit_dir = self.home / ".config/systemd/user"
        self.config_dir = self.home / ".config/rsi/satellites"
        self.unit_dir.mkdir(parents=True)
        self.config_dir.mkdir(parents=True)
        self.systemctl_log = base / "systemctl.log"
        self.systemctl_log.write_text("")
        self.stub("ssh", 'printf "RSI_HOME=%s\\n" "/home/peer"')
        self.stub("rsi-rpc", "exit 0")
        self.stub("journalctl", "exit 0")
        self.stub("systemctl", '''echo "$*" >> "$FAKE_SYSTEMCTL_LOG"
if [[ "$*" == *restart* ]]; then
  python3 -c 'import os, socket, sys
os.makedirs(os.path.dirname(sys.argv[1]), mode=0o700, exist_ok=True)
s = socket.socket(socket.AF_UNIX); s.bind(sys.argv[1]); s.close()' "$HOME/.rsi/satellites/fresh-direct.sock"
fi''')
        self.env = dict(os.environ, HOME=str(self.home), PATH=f"{self.bin}:{os.environ['PATH']}",
                        FAKE_SYSTEMCTL_LOG=str(self.systemctl_log))
        self.installed = self.unit_dir / UNIT_NAME

    def stub(self, name, body):
        path = self.bin / name
        path.write_text("#!/usr/bin/env bash\n" + body + "\n")
        path.chmod(0o755)

    def install(self, *extra):
        return subprocess.run([str(INSTALL), "fresh", "peer-ssh", *extra], env=self.env,
                              capture_output=True, text=True, timeout=60)

    def old_template(self):
        self.installed.write_text("[Service]\nExecStart=/bin/true\n")

    def add_instance(self, name, env_file):
        wants = self.unit_dir / "default.target.wants"
        wants.mkdir(exist_ok=True)
        (wants / f"rsi-satellite-direct@{name}.service").symlink_to(self.installed)
        if env_file:
            (self.config_dir / f"{name}-direct.env").write_text("RSI_SATELLITE_TARGET=x\n")

    def test_fresh_install_writes_the_repo_template(self):
        result = self.install()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(self.installed.read_text(), DIRECT.read_text())

    def test_refuses_differing_template_when_another_instance_has_no_env_file(self):
        self.old_template()
        self.add_instance("arch-laptop", env_file=False)
        result = self.install()
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("refusing to overwrite", result.stderr)
        self.assertIn("arch-laptop", result.stderr)
        self.assertEqual(self.installed.read_text(), "[Service]\nExecStart=/bin/true\n")
        self.assertFalse((self.config_dir / "fresh-direct.env").exists())
        self.assertEqual(self.systemctl_log.read_text(), "")

    def test_refusal_also_finds_instances_by_drop_in_directory(self):
        self.old_template()
        (self.unit_dir / "rsi-satellite-direct@mac-mini.service.d").mkdir()
        result = self.install()
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("mac-mini", result.stderr)

    def test_overwrites_when_other_instances_have_env_files(self):
        self.old_template()
        self.add_instance("arch-laptop", env_file=True)
        result = self.install()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(self.installed.read_text(), DIRECT.read_text())

    def test_overwrites_when_the_only_instance_is_the_one_being_installed(self):
        self.old_template()
        self.add_instance("fresh", env_file=False)
        result = self.install()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(self.installed.read_text(), DIRECT.read_text())

    def test_replace_template_overrides_the_refusal(self):
        self.old_template()
        self.add_instance("arch-laptop", env_file=False)
        result = self.install("--replace-template")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(self.installed.read_text(), DIRECT.read_text())

    def test_identical_template_is_never_refused(self):
        self.installed.write_text(DIRECT.read_text())
        self.add_instance("arch-laptop", env_file=False)
        result = self.install()
        self.assertEqual(result.returncode, 0, result.stderr)


if __name__ == "__main__":
    unittest.main()
