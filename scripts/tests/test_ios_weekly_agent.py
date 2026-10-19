"""Tests for the weekly iOS re-sign launchd agent (hub #2963, mac-mini #7).

Nothing here bootstraps a real launchd agent or runs xcodebuild: the installer
runs with --dry-run or with a stub launchctl, and weekly-resign.sh runs against
a stub dev-install.sh in a temp dir.
"""

import os
import plistlib
import re
import stat
import subprocess
import tempfile
import unittest
from pathlib import Path

IOS_DIR = Path(__file__).resolve().parents[1] / "ios"
INSTALL = IOS_DIR / "install-weekly-agent.sh"
RESIGN = IOS_DIR / "weekly-resign.sh"
LABEL = "dev.rsi.ios-resign"
NOW = 1_800_000_000  # fixed fake clock (epoch seconds)
DAY = 86400

LOG_LINE = re.compile(
    r"^\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}Z (ok|skip|failed) exit=(\d+) \S.*$"
)
# Shapes that must never appear in the plist: Apple team ids (10 uppercase
# alphanumerics), device UDIDs (8-16 hex groups) and e-mail addresses.
FORBIDDEN = [
    re.compile(r"\b[0-9A-Z]{10}\b"),
    re.compile(r"\b[0-9A-Fa-f]{8}-[0-9A-Fa-f]{16}\b"),
    re.compile(r"\b[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}\b"),
    re.compile(r"@[A-Za-z0-9.-]+\.[A-Za-z]{2,}"),
]


def write_exec(path, text):
    path.write_text(text)
    path.chmod(path.stat().st_mode | stat.S_IXUSR)


class TempHome(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix="ios-weekly-agent-")
        self.addCleanup(self.temp.cleanup)
        self.home = Path(self.temp.name) / "home"
        self.home.mkdir()
        self.agent_dir = self.home / "Library" / "LaunchAgents"
        self.env = {
            "PATH": os.environ.get("PATH", "/usr/bin:/bin"),
            "HOME": str(self.home),
            "RSI_IOS_AGENT_DIR": str(self.agent_dir),
            "RSI_IOS_UID": "501",
        }

    def run_script(self, script, *args, extra_env=None):
        env = dict(self.env)
        env.update(extra_env or {})
        return subprocess.run(
            ["/bin/bash", str(script), *args],
            env=env,
            capture_output=True,
            text=True,
            timeout=60,
        )


class InstallWeeklyAgentTest(TempHome):
    def dry_run_plist(self, *args):
        result = self.run_script(INSTALL, "--dry-run", *args)
        self.assertEqual(result.returncode, 0, result.stderr)
        match = re.search(
            r"--- plist begin ---\n(.*)--- plist end ---", result.stdout, re.S
        )
        self.assertIsNotNone(match, result.stdout)
        return result.stdout, plistlib.loads(match.group(1).encode())

    def test_dry_run_plist_is_valid_retrying_agent(self):
        stdout, plist = self.dry_run_plist()
        self.assertEqual(plist["Label"], LABEL)
        # Fires every 6 hours; weekly-resign.sh decides whether an install is due.
        self.assertEqual(plist["StartInterval"], 6 * 3600)
        self.assertNotIn("StartCalendarInterval", plist)
        self.assertIs(plist["RunAtLoad"], True)
        self.assertEqual(
            plist["ProgramArguments"], ["/bin/bash", str(RESIGN)]
        )
        self.assertIn("PATH", plist["EnvironmentVariables"])
        self.assertEqual(plist["EnvironmentVariables"]["HOME"], str(self.home))
        self.assertIn(f"bootstrap gui/501 {self.agent_dir / (LABEL + '.plist')}", stdout)
        self.assertIn(f"bootout gui/501/{LABEL}", stdout)

    def test_dry_run_touches_nothing(self):
        self.dry_run_plist()
        self.assertFalse(self.agent_dir.exists())
        self.assertFalse((self.home / ".rsi").exists())

    def test_plist_carries_no_device_team_or_apple_id(self):
        stdout, plist = self.dry_run_plist()
        # Even with such values in the environment, they never reach the plist.
        secret_env = {
            "RSI_IOS_DEVICE_ID": "00008110-001234567890ABCD",
            "RSI_IOS_TEAM_ID": "ABCDE12345",
            "RSI_IOS_APPLE_ID": "someone@example.invalid",
        }
        result = self.run_script(INSTALL, "--dry-run", extra_env=secret_env)
        self.assertEqual(result.returncode, 0, result.stderr)
        for value in secret_env.values():
            self.assertNotIn(value, result.stdout)
        keys = set(plist["EnvironmentVariables"])
        self.assertEqual(keys, {"PATH", "HOME"})
        # The temp home and the checkout path (a sandbox path may contain a
        # session UUID) are expected; scan the rest of the rendered text.
        scrubbed = stdout.replace(str(self.home), "").replace(str(RESIGN), "")
        for pattern in FORBIDDEN:
            self.assertIsNone(pattern.search(scrubbed), pattern.pattern)

    def test_custom_interval(self):
        _, plist = self.dry_run_plist("--interval-hours", "3")
        self.assertEqual(plist["StartInterval"], 3 * 3600)

    def test_rejects_bad_interval(self):
        for args in (
            ["--interval-hours", "25"],
            ["--interval-hours", "0"],
            ["--interval-hours", "x"],
            ["--interval-hours"],
            ["--weekday", "1"],
        ):
            result = self.run_script(INSTALL, "--dry-run", *args)
            self.assertEqual(result.returncode, 2, (args, result.stderr))
        result = self.run_script(INSTALL, "--bogus")
        self.assertEqual(result.returncode, 2)

    def test_uninstall_dry_run(self):
        result = self.run_script(INSTALL, "--dry-run", "--uninstall")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn(f"bootout gui/501/{LABEL}", result.stdout)
        self.assertNotIn("bootstrap", result.stdout)

    def stub_launchctl(self):
        calls = self.home / "launchctl.calls"
        stub = self.home / "launchctl-stub"
        write_exec(stub, f'#!/bin/bash\necho "$@" >> "{calls}"\nexit 0\n')
        return stub, calls

    def test_install_is_idempotent_and_uninstall_removes(self):
        stub, calls = self.stub_launchctl()
        env = {"RSI_IOS_LAUNCHCTL": str(stub)}
        plist_path = self.agent_dir / f"{LABEL}.plist"
        for _ in range(2):
            result = self.run_script(INSTALL, extra_env=env)
            self.assertEqual(result.returncode, 0, result.stderr)
            with open(plist_path, "rb") as handle:
                self.assertEqual(plistlib.load(handle)["Label"], LABEL)
        lines = calls.read_text().splitlines()
        # Each install is bootout (replace) then bootstrap, twice, in that order.
        self.assertEqual(
            lines,
            [f"bootout gui/501/{LABEL}", f"bootstrap gui/501 {plist_path}"] * 2,
        )
        result = self.run_script(INSTALL, "--uninstall", extra_env=env)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertFalse(plist_path.exists())
        self.assertEqual(calls.read_text().splitlines()[-1], f"bootout gui/501/{LABEL}")


class WeeklyResignTest(TempHome):
    def stub_dev_install(self, body):
        stub = self.home / "dev-install-stub.sh"
        write_exec(stub, "#!/bin/bash\n" + body)
        return {"RSI_IOS_DEV_INSTALL": str(stub)}

    @property
    def log_file(self):
        return self.home / ".rsi" / "logs" / "ios-resign.log"

    @property
    def last_log(self):
        return self.home / ".rsi" / "logs" / "ios-resign.last.log"

    def log_lines(self):
        return self.log_file.read_text().splitlines()

    def stub_clock(self, now):
        return {"RSI_IOS_RESIGN_NOW": str(now)}

    def env_for(self, body, now=NOW):
        env = self.stub_dev_install(body)
        env.update(self.stub_clock(now))
        return env

    @property
    def state_file(self):
        return self.home / ".rsi" / "ios-resign.state"

    def write_state(self, epoch):
        self.state_file.parent.mkdir(parents=True, exist_ok=True)
        self.state_file.write_text(f"{epoch}\n")

    def state_epoch(self):
        return int(self.state_file.read_text().strip())

    def test_success_logs_one_ok_line_and_passes_flag(self):
        argv = self.home / "argv"
        env = self.env_for(f'echo "$@" > "{argv}"\necho built\nexit 0\n')
        result = self.run_script(RESIGN, extra_env=env)
        self.assertEqual(result.returncode, 0, result.stderr)
        lines = self.log_lines()
        self.assertEqual(len(lines), 1, lines)
        self.assertRegex(lines[0], LOG_LINE)
        self.assertIn(" ok exit=0 ", lines[0])
        self.assertEqual(argv.read_text().strip(), "--network-first")
        self.assertFalse(self.last_log.exists())

    def test_missing_state_is_due_now_and_success_seeds_clock(self):
        counter = self.home / "count"
        env = self.env_for(f'echo x >> "{counter}"\nexit 0\n')
        self.assertFalse(self.state_file.exists())
        result = self.run_script(RESIGN, extra_env=env)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(len(counter.read_text().splitlines()), 1)
        self.assertEqual(self.state_epoch(), NOW)
        self.assertIn("no-state", self.log_lines()[0])

    def test_before_day_5_is_a_skip(self):
        counter = self.home / "count"
        env = self.env_for(f'echo x >> "{counter}"\nexit 0\n')
        last_ok = NOW - (4 * DAY + 23 * 3600)  # 4d23h old
        self.write_state(last_ok)
        result = self.run_script(RESIGN, extra_env=env)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertFalse(counter.exists())
        self.assertEqual(self.state_epoch(), last_ok)
        lines = self.log_lines()
        self.assertEqual(len(lines), 1, lines)
        self.assertRegex(lines[0], LOG_LINE)
        self.assertIn(" skip exit=0 ", lines[0])
        self.assertIn("age=4d23h", lines[0])

    def test_skip_does_not_touch_last_failure_log(self):
        self.run_script(RESIGN, extra_env=self.env_for("exit 3\n"))
        self.assertTrue(self.last_log.exists())
        self.write_state(NOW - DAY)
        self.run_script(RESIGN, extra_env=self.env_for("exit 0\n"))
        self.assertTrue(self.last_log.exists())

    def test_from_day_5_it_runs_and_success_resets_clock(self):
        counter = self.home / "count"
        env = self.env_for(f'echo x >> "{counter}"\nexit 0\n')
        self.write_state(NOW - 5 * DAY)
        result = self.run_script(RESIGN, extra_env=env)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(len(counter.read_text().splitlines()), 1)
        self.assertEqual(self.state_epoch(), NOW)
        self.assertIn(" ok exit=0 ", self.log_lines()[0])
        # Clock reset: an hour later it is a skip again.
        later = self.env_for(f'echo x >> "{counter}"\nexit 0\n', now=NOW + 3600)
        self.run_script(RESIGN, extra_env=later)
        self.assertEqual(len(counter.read_text().splitlines()), 1)
        self.assertIn(" skip exit=0 ", self.log_lines()[1])

    def test_failure_keeps_state_and_next_run_retries(self):
        counter = self.home / "count"
        last_ok = NOW - 6 * DAY
        self.write_state(last_ok)
        fail = self.env_for(f'echo x >> "{counter}"\nexit 3\n')
        result = self.run_script(RESIGN, extra_env=fail)
        self.assertEqual(result.returncode, 3)
        self.assertEqual(self.state_epoch(), last_ok)
        # Six hours later the phone is back: the retry runs and succeeds.
        ok = self.env_for(f'echo x >> "{counter}"\nexit 0\n', now=NOW + 6 * 3600)
        result = self.run_script(RESIGN, extra_env=ok)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(len(counter.read_text().splitlines()), 2)
        self.assertEqual(self.state_epoch(), NOW + 6 * 3600)
        lines = self.log_lines()
        self.assertEqual(len(lines), 2, lines)
        self.assertIn(" failed exit=3 ", lines[0])
        self.assertIn(" ok exit=0 ", lines[1])
        self.assertFalse(self.last_log.exists())

    def test_no_device_exit_3_reads_needs_phone(self):
        env = self.env_for('echo "no paired device"\nexit 3\n')
        result = self.run_script(RESIGN, extra_env=env)
        self.assertEqual(result.returncode, 3)
        lines = self.log_lines()
        self.assertEqual(len(lines), 1, lines)
        self.assertRegex(lines[0], LOG_LINE)
        self.assertIn(" failed exit=3 needs-phone ", lines[0])
        self.assertIn("no paired device", self.last_log.read_text())
        self.assertFalse(self.state_file.exists())

    def test_build_failure_keeps_full_output_and_one_line(self):
        env = self.env_for(
            'echo first line\necho "error: signing\tfailed"\necho\nexit 65\n'
        )
        result = self.run_script(RESIGN, extra_env=env)
        self.assertEqual(result.returncode, 65)
        lines = self.log_lines()
        self.assertEqual(len(lines), 1, lines)
        self.assertRegex(lines[0], LOG_LINE)
        self.assertIn(" failed exit=65 error: signing failed", lines[0])
        self.assertEqual(
            self.last_log.read_text(), "first line\nerror: signing\tfailed\n\n"
        )

    def test_unreadable_or_future_state_is_due_now(self):
        for content in ("garbage\n", f"{NOW + 10 * DAY}\n", ""):
            counter = self.home / "count"
            if counter.exists():
                counter.unlink()
            self.state_file.parent.mkdir(parents=True, exist_ok=True)
            self.state_file.write_text(content)
            env = self.env_for(f'echo x >> "{counter}"\nexit 3\n')
            self.run_script(RESIGN, extra_env=env)
            self.assertEqual(len(counter.read_text().splitlines()), 1, content)
            self.assertEqual(self.state_file.read_text(), content)

    def test_success_after_failure_clears_last_log(self):
        self.run_script(RESIGN, extra_env=self.env_for("exit 3\n"))
        self.assertTrue(self.last_log.exists())
        self.run_script(RESIGN, extra_env=self.env_for("exit 0\n"))
        self.assertFalse(self.last_log.exists())
        self.assertEqual(len(self.log_lines()), 2)

    def test_missing_dev_install_is_a_logged_failure(self):
        env = {
            "RSI_IOS_DEV_INSTALL": str(self.home / "does-not-exist.sh"),
            **self.stub_clock(NOW),
        }
        result = self.run_script(RESIGN, extra_env=env)
        self.assertEqual(result.returncode, 127)
        lines = self.log_lines()
        self.assertEqual(len(lines), 1, lines)
        self.assertRegex(lines[0], LOG_LINE)
        self.assertIn(" failed exit=127 ", lines[0])
        self.assertFalse(self.state_file.exists())

    def test_no_bsd_only_flags_in_scripts(self):
        for script in (INSTALL, RESIGN):
            text = script.read_text()
            for pattern in (r"sed -i ''", r"stat -f", r"date -j", r"mktemp -t\b", r"mapfile", r"declare -A"):
                self.assertIsNone(re.search(pattern, text), (script.name, pattern))


if __name__ == "__main__":
    unittest.main()
