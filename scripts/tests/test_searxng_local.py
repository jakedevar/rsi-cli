"""Tests for scripts/searxng-local.sh (#2315).

The script is exercised against a throwaway HOME with fake `podman`,
`systemctl` and `curl` executables first on PATH, so no service is started, no
container runs and nothing touches the network. The shims log every call; the
assertions read those logs.
"""

import os
import stat
import subprocess
import tempfile
import unittest
from pathlib import Path

SCRIPT = Path(__file__).resolve().parents[1] / "searxng-local.sh"

# `image inspect` succeeds once `pull` has run, as a real runtime would.
PODMAN = """#!/bin/sh
echo "$@" >> "$SHIM_LOG/podman.log"
case "$1 $2" in
  "image inspect") [ -f "$SHIM_LOG/pulled" ]; exit $? ;;
esac
case "$1" in
  pull) : > "$SHIM_LOG/pulled" ;;
esac
exit 0
"""

# Everything the script asks systemd must be a user-manager call. With
# SHIM_NO_SYSTEMD set the user manager is reported unavailable.
SYSTEMCTL = """#!/bin/sh
echo "$@" >> "$SHIM_LOG/systemctl.log"
if [ "$2" = "show-environment" ] && [ -n "$SHIM_NO_SYSTEMD" ]; then
  exit 1
fi
exit 0
"""

# Answers the health probe and the JSON probe instantly.
CURL = """#!/bin/sh
for arg in "$@"; do
  if [ "$arg" = "-w" ]; then
    printf 200
    exit 0
  fi
done
exit 0
"""

# A docker the user cannot reach: installed, but `info` fails like it does
# without access to the socket. Nothing else may run.
DOCKER_NO_SOCKET = """#!/bin/sh
echo "$@" >> "$SHIM_LOG/docker.log"
if [ "$1" = "info" ]; then
  echo "permission denied while trying to connect to the docker API at unix:///var/run/docker.sock" >&2
  exit 1
fi
exit 1
"""

# A podman that is present but broken, so a real one on the host cannot leak in.
PODMAN_BROKEN = """#!/bin/sh
echo "$@" >> "$SHIM_LOG/podman.log"
exit 1
"""

# `git clone --depth 1 URL DIR` makes a checkout with a requirements file.
GIT = """#!/bin/sh
echo "$@" >> "$SHIM_LOG/git.log"
if [ "$1" = "clone" ]; then
  for last; do :; done
  mkdir -p "$last/.git"
  printf '%s\\n' "msgspec==1" > "$last/requirements.txt"
fi
exit 0
"""

# `python3 -m venv DIR` makes a venv whose pip logs every call.
PYTHON3 = """#!/bin/sh
echo "$@" >> "$SHIM_LOG/python3.log"
if [ "$1 $2" = "-m venv" ]; then
  mkdir -p "$3/bin"
  printf '#!/bin/sh\\nexit 0\\n' > "$3/bin/python"
  printf '#!/bin/sh\\necho "$@" >> "$SHIM_LOG/pip.log"\\n' > "$3/bin/pip"
  chmod +x "$3/bin/python" "$3/bin/pip"
fi
exit 0
"""


class SearxngLocalTest(unittest.TestCase):
    def setUp(self):
        self._tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self._tmp.cleanup)
        root = Path(self._tmp.name)
        self.home = root / "home"
        self.home.mkdir()
        self.shims = root / "shims"
        self.shims.mkdir()
        self.logs = root / "logs"
        self.logs.mkdir()
        for name, body in (
            ("podman", PODMAN),
            ("systemctl", SYSTEMCTL),
            ("curl", CURL),
            ("git", GIT),
            ("python3", PYTHON3),
        ):
            path = self.shims / name
            path.write_text(body)
            path.chmod(path.stat().st_mode | stat.S_IXUSR)

    def run_script(self, *args, extra_env=None):
        env = {
            "HOME": str(self.home),
            "PATH": f"{self.shims}:/usr/bin:/bin",
            "SHIM_LOG": str(self.logs),
            "SEARXNG_RUNTIME": "podman",
        }
        env.update(extra_env or {})
        return subprocess.run(
            ["bash", str(SCRIPT), *args],
            env=env,
            capture_output=True,
            text=True,
            timeout=60,
            check=False,
        )

    def log(self, name):
        path = self.logs / f"{name}.log"
        return path.read_text().splitlines() if path.exists() else []

    @property
    def data(self):
        return self.home / ".local/share/rsi/searxng"

    def test_help_documents_the_commands_and_the_loopback_guarantee(self):
        for flag in ("--help", "-h"):
            result = self.run_script(flag)
            self.assertEqual(result.returncode, 0, result.stderr)
            for needle in ("up", "down", "status", "url", "run", "--dry-run", "127.0.0.1 only"):
                self.assertIn(needle, result.stdout)

    def test_bad_arguments_are_refused(self):
        self.assertEqual(self.run_script().returncode, 2)
        self.assertEqual(self.run_script("frobnicate").returncode, 2)
        self.assertEqual(self.run_script("up", "down").returncode, 2)
        for port in ("80", "70000", "abc"):
            result = self.run_script("url", extra_env={"SEARXNG_PORT": port})
            self.assertEqual(result.returncode, 2, port)
            self.assertIn("SEARXNG_PORT", result.stderr)
        bad = self.run_script("--dry-run", "up", extra_env={"SEARXNG_RUNTIME": "lxc"})
        self.assertEqual(bad.returncode, 2)
        self.assertIn("SEARXNG_RUNTIME", bad.stderr)

    def test_url_is_always_loopback(self):
        self.assertEqual(self.run_script("url").stdout.strip(), "http://127.0.0.1:8888")
        custom = self.run_script("url", extra_env={"SEARXNG_PORT": "9123"})
        self.assertEqual(custom.stdout.strip(), "http://127.0.0.1:9123")

    def test_dry_run_changes_nothing(self):
        result = self.run_script("--dry-run", "up")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("would write", result.stdout)
        self.assertIn("would pull", result.stdout)
        self.assertEqual(list(self.home.rglob("*")), [], "dry-run wrote files")
        self.assertEqual(self.log("systemctl"), [], "dry-run called systemctl")
        self.assertNotIn("pull", " ".join(self.log("podman")))
        # The same flag after the command.
        again = self.run_script("up", "--dry-run")
        self.assertEqual(again.returncode, 0, again.stderr)
        self.assertEqual(list(self.home.rglob("*")), [])

    def test_up_installs_a_loopback_only_json_enabled_user_service(self):
        result = self.run_script("up")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("url:       http://127.0.0.1:8888", result.stdout)
        self.assertIn("harness_searxng_url", result.stdout)

        settings = (self.data / "config/settings.yml").read_text()
        self.assertIn("- json", settings)
        self.assertIn("- html", settings)
        # Inside the container it listens widely; the published port is loopback.
        self.assertIn('bind_address: "0.0.0.0"', settings)
        self.assertIn("limiter: false", settings)
        self.assertNotIn("ultrasecretkey", settings)
        secret = (self.data / "secret").read_text()
        self.assertEqual(len(secret), 64)
        self.assertIn(f'secret_key: "{secret}"', settings)
        for private in (self.data / "secret", self.data / "config/settings.yml"):
            self.assertEqual(stat.S_IMODE(private.stat().st_mode), 0o600, private)

        unit = (self.home / ".config/systemd/user/rsi-searxng.service").read_text()
        self.assertIn(f'ExecStart="{self.data}/searxng-local.sh" run', unit)
        self.assertIn('Environment="SEARXNG_PORT=8888"', unit)
        self.assertIn('Environment="SEARXNG_RUNTIME=podman"', unit)
        self.assertNotIn("0.0.0.0", unit)
        self.assertTrue(os.access(self.data / "searxng-local.sh", os.X_OK))

        systemctl = self.log("systemctl")
        self.assertTrue(systemctl, "no systemd call was made")
        self.assertTrue(
            all(line.startswith("--user ") for line in systemctl),
            f"every systemd call must be a user-manager call: {systemctl}",
        )
        self.assertIn("--user daemon-reload", systemctl)
        self.assertIn("--user restart rsi-searxng.service", systemctl)
        self.assertIn("pull docker.io/searxng/searxng:latest", self.log("podman"))

    def test_up_twice_changes_and_restarts_nothing_the_second_time(self):
        self.assertEqual(self.run_script("up").returncode, 0)
        files = {
            path: (path.read_bytes(), path.stat().st_mtime_ns)
            for path in self.home.rglob("*")
            if path.is_file()
        }
        systemctl_before = len(self.log("systemctl"))
        podman_before = len(self.log("podman"))

        second = self.run_script("up")
        self.assertEqual(second.returncode, 0, second.stderr)
        self.assertNotIn("wrote", second.stdout)
        after = {
            path: (path.read_bytes(), path.stat().st_mtime_ns)
            for path in self.home.rglob("*")
            if path.is_file()
        }
        self.assertEqual(after, files, "a second `up` rewrote a file")

        new_calls = self.log("systemctl")[systemctl_before:]
        self.assertTrue(new_calls)
        self.assertTrue(all(line.startswith("--user ") for line in new_calls), new_calls)
        for line in new_calls:
            self.assertNotIn("restart", line)
            self.assertNotIn("daemon-reload", line)
        self.assertIn("--user start rsi-searxng.service", new_calls)
        self.assertNotIn("pull", " ".join(self.log("podman")[podman_before:]))

    def test_a_changed_port_rewrites_the_unit_and_restarts_once(self):
        self.assertEqual(self.run_script("up").returncode, 0)
        before = len(self.log("systemctl"))
        moved = self.run_script("up", extra_env={"SEARXNG_PORT": "9123"})
        self.assertEqual(moved.returncode, 0, moved.stderr)
        self.assertIn("url:       http://127.0.0.1:9123", moved.stdout)
        unit = (self.home / ".config/systemd/user/rsi-searxng.service").read_text()
        self.assertIn('Environment="SEARXNG_PORT=9123"', unit)
        calls = self.log("systemctl")[before:]
        self.assertIn("--user daemon-reload", calls)
        self.assertIn("--user restart rsi-searxng.service", calls)

    def test_unusual_home_is_escaped_in_the_unit(self):
        weird = Path(self._tmp.name) / 'we"ird %h \\ $x'
        result = self.run_script("up", extra_env={"SEARXNG_HOME": str(weird)})
        self.assertEqual(result.returncode, 0, result.stderr)
        unit = (self.home / ".config/systemd/user/rsi-searxng.service").read_text()
        lines = unit.splitlines()
        base = str(weird)
        env_home = base.replace("\\", "\\\\").replace('"', '\\"').replace("%", "%%")
        self.assertIn(f'Environment="SEARXNG_HOME={env_home}"', lines)
        exec_path = env_home.replace("$", "$$") + "/searxng-local.sh"
        self.assertIn(f'ExecStart="{exec_path}" run', lines)
        # The unusual characters add no directive of their own.
        keys = [line.split("=", 1)[0] for line in lines if "=" in line and not line.startswith("#")]
        self.assertEqual(
            keys,
            ["Description", "After", "Type", "Environment", "Environment", "Environment",
             "Environment", "ExecStart", "Restart", "RestartSec", "WantedBy"],
        )

    def test_unusual_image_is_escaped_in_the_unit(self):
        result = self.run_script("up", extra_env={"SEARXNG_IMAGE": 'reg/img%n"x:1'})
        self.assertEqual(result.returncode, 0, result.stderr)
        unit = (self.home / ".config/systemd/user/rsi-searxng.service").read_text()
        self.assertIn('Environment="SEARXNG_IMAGE=reg/img%%n\\"x:1"', unit.splitlines())

    def test_line_breaks_and_control_characters_are_refused_without_writing(self):
        for name, value in (
            ("SEARXNG_HOME", "/tmp/x\nExecStartPre=/bin/evil"),
            ("SEARXNG_HOME", "/tmp/x\ty"),
            ("SEARXNG_IMAGE", "img\nExecStartPre=/bin/evil"),
            ("SEARXNG_IMAGE", "img\rx"),
        ):
            for args in (("up",), ("--dry-run", "up")):
                result = self.run_script(*args, extra_env={name: value})
                self.assertEqual(result.returncode, 2, (name, value, result.stdout))
                self.assertIn(name, result.stderr)
        self.assertEqual(list(self.home.rglob("*")), [], "a refused value wrote files")
        self.assertEqual(self.log("systemctl"), [])

    def test_relative_home_and_option_like_images_are_refused(self):
        relative = self.run_script("--dry-run", "up", extra_env={"SEARXNG_HOME": "rel/dir"})
        self.assertEqual(relative.returncode, 2)
        self.assertIn("SEARXNG_HOME", relative.stderr)
        for image in ("--privileged", "two words"):
            result = self.run_script("--dry-run", "up", extra_env={"SEARXNG_IMAGE": image})
            self.assertEqual(result.returncode, 2, image)
            self.assertIn("SEARXNG_IMAGE", result.stderr)

    def test_run_publishes_only_on_the_loopback_interface(self):
        result = self.run_script("run")
        self.assertEqual(result.returncode, 0, result.stderr)
        runs = [line for line in self.log("podman") if line.startswith("run ")]
        self.assertEqual(len(runs), 1, self.log("podman"))
        self.assertIn("-p 127.0.0.1:8888:8080", runs[0])
        self.assertIn(f"-v {self.data}/config:/etc/searxng:Z", runs[0])
        self.assertNotIn("0.0.0.0:", runs[0])

    def test_down_disables_the_user_service_and_keeps_the_data(self):
        self.assertEqual(self.run_script("up").returncode, 0)
        result = self.run_script("down")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("--user disable --now rsi-searxng.service", self.log("systemctl"))
        self.assertTrue((self.data / "config/settings.yml").exists())

    def test_up_without_systemd_user_services_is_an_explicit_error(self):
        result = self.run_script("up", extra_env={"SHIM_NO_SYSTEMD": "1"})
        self.assertEqual(result.returncode, 3, result.stdout + result.stderr)
        self.assertIn("systemd user services", result.stderr)
        self.assertEqual(list(self.home.rglob("*")), [], "nothing is installed without systemd")

    def broken_runtimes(self):
        """Auto-detect env (no SEARXNG_RUNTIME): podman broken, docker unreachable."""
        for name, body in (("podman", PODMAN_BROKEN), ("docker", DOCKER_NO_SOCKET)):
            path = self.shims / name
            path.write_text(body)
            path.chmod(path.stat().st_mode | stat.S_IXUSR)
        return {"SEARXNG_RUNTIME": ""}

    def test_a_docker_without_socket_access_falls_through_to_venv(self):
        result = self.run_script("--dry-run", "up", extra_env=self.broken_runtimes())
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("runtime: venv", result.stdout)
        self.assertIn("docker is installed but not usable", result.stderr)
        self.assertIn("permission denied", result.stderr)
        self.assertIn("podman is installed but not usable", result.stderr)
        self.assertEqual(self.log("docker"), ["info"], "only `docker info` may run")

    def test_a_usable_podman_is_chosen_by_auto_detect(self):
        result = self.run_script("--dry-run", "up", extra_env={"SEARXNG_RUNTIME": ""})
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("runtime: podman", result.stdout)

    def test_venv_dry_run_names_the_requirements_step(self):
        result = self.run_script("--dry-run", "up", extra_env={"SEARXNG_RUNTIME": "venv"})
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("install the requirements from", result.stdout)
        self.assertIn("requirements.txt", result.stdout)
        self.assertEqual(list(self.home.rglob("*")), [], "dry-run wrote files")

    def test_venv_installs_requirements_before_the_editable_install_and_is_idempotent(self):
        env = {"SEARXNG_RUNTIME": "venv"}
        first = self.run_script("up", extra_env=env)
        self.assertEqual(first.returncode, 0, first.stderr)
        pip = self.log("pip")
        req = f"install -r {self.data}/src/requirements.txt"
        self.assertIn(req, pip)
        editable = [i for i, line in enumerate(pip) if "-e ." in line]
        self.assertEqual(len(editable), 1, pip)
        self.assertLess(pip.index(req), editable[0])

        # Unchanged requirements: a second `up` installs nothing.
        count = len(pip)
        second = self.run_script("up", extra_env=env)
        self.assertEqual(second.returncode, 0, second.stderr)
        self.assertEqual(len(self.log("pip")), count, "a second `up` reinstalled")
        dry = self.run_script("--dry-run", "up", extra_env=env)
        self.assertNotIn("requirements", dry.stdout)

        # Changed requirements re-run the install, once.
        (self.data / "src/requirements.txt").write_text("msgspec==2\n")
        third = self.run_script("up", extra_env=env)
        self.assertEqual(third.returncode, 0, third.stderr)
        self.assertIn(req, self.log("pip")[count:])
        count = len(self.log("pip"))
        self.assertEqual(self.run_script("up", extra_env=env).returncode, 0)
        self.assertEqual(len(self.log("pip")), count)


if __name__ == "__main__":
    unittest.main()
