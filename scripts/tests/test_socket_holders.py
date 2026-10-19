"""#1647: rsid.socket and install-release.sh share a stable user-only front door."""
import os
from pathlib import Path
import subprocess
import tempfile
import unittest

SCRIPTS = Path(__file__).resolve().parents[1]


class SocketHoldersTest(unittest.TestCase):
    def test_managed_socket_is_user_only_and_is_retained_across_service_restarts(self):
        socket = (SCRIPTS / "systemd/rsid.socket").read_text()
        service = (SCRIPTS / "systemd/rsid.service").read_text()
        self.assertIn("ListenStream=%h/.rsi/daemon.sock", socket)
        self.assertIn("SocketMode=0600", socket)
        self.assertIn("DirectoryMode=0700", socket)
        self.assertIn("Accept=no", socket)
        self.assertIn("Service=rsid.service", socket)
        self.assertIn("WantedBy=sockets.target", socket)
        self.assertIn("Sockets=rsid.socket", service)
        self.assertIn("Requires=rsi-workers.slice rsid.socket", service)
        self.assertIn("ExecStart=%h/.rsi/install/rsid-supervisor.sh %h/.local/bin/rsid", service)
        self.assertIn("UMask=0077", service)

    def test_recovery_runbook_reuses_configured_socket_and_daemon_environment(self):
        runbook = (SCRIPTS / "systemd/README.md").read_text()
        self.assertIn('export RSI_DAEMON_SOCKET_PATH="$socket" RSI_SOCKET="$socket"', runbook)
        self.assertIn("stat -c '%i %a %U' \"$socket\"", runbook)
        self.assertIn('rsi-rpc --socket "$socket" GetHealthStatus', runbook)
        self.assertIn("source scripts/rsid-health.sh", runbook)
        self.assertIn('rsid_rpc_ready "$HOME/.rsi/install/rsi-rpc" "$socket" "$holder_pid"', runbook)
        self.assertIn("rsid_daemon_environment", runbook)
        self.assertIn('"${DAEMON_ENV_ARGS[@]}"', runbook)
        self.assertIn('--setenv="RSI_DAEMON_SOCKET_PATH=$socket"', runbook)

    def test_recovery_unit_carries_the_installer_resource_limits(self):
        runbook = (SCRIPTS / "systemd/README.md").read_text()
        installer = (SCRIPTS / "install-release.sh").read_text()
        recovery = runbook[runbook.index('unit="rsid-recover-'):runbook.index("launcher_pid=", runbook.index('unit="rsid-recover-'))]
        self.assertIn("source scripts/rsid-scope.sh", runbook)
        self.assertIn("load_rsid_scope_settings", runbook)
        for line in (
            '--property="MemoryHigh=${RSID_SCOPE_MEMORY_HIGH_MIB}M"',
            '--property="MemoryMax=${RSID_SCOPE_MEMORY_MAX_MIB}M"',
            '--property="MemorySwapMax=${RSID_SCOPE_MEMORY_SWAP_MAX_MIB}M"',
            '--property="CPUWeight=${RSID_SCOPE_CPU_WEIGHT}"',
        ):
            self.assertIn(line, installer)
            self.assertIn(line, recovery)

    def restart(self, platform, holder_exists=True):
        source = (SCRIPTS / "install-release.sh").read_text()
        health_helpers = (SCRIPTS / "rsid-health.sh").read_text()
        function = source[source.index("installer_daemon_environment() {"):source.index("# #2188: the links now point at INSTALL_DIR")]
        with tempfile.TemporaryDirectory(prefix="rsi-holder-install-") as temp:
            root = Path(temp)
            holder = root / "rsi-socket-hold"
            if holder_exists:
                holder.write_text("#!/bin/sh\nexit 0\n")
                holder.chmod(0o700)
            # All effects are mocked; no daemon, systemd unit or user file is touched.
            harness = r'''
set -euo pipefail
calls="$FIXTURE/calls"
record() { printf '%s\n' "$*" >> "$calls"; }
uname() { echo "$PLATFORM"; }
systemctl() {
    if [[ "$*" == *--property=MainPID* ]]; then echo 999996; else return 1; fi
}
rsid_running_pids() {
    if [[ ! -f "$FIXTURE/probed" ]]; then touch "$FIXTURE/probed"; echo 999999; fi
}
rsid_launcher_pids() { record "snapshot $*"; echo "999998 999997"; }
rsid_pids_still_running() {
    record "launcher-probe $*"
    if [[ ! -f "$FIXTURE/launcher-probed" ]]; then
        touch "$FIXTURE/launcher-probed"; echo 999997
    else
        record launcher-released
    fi
}
sleep() { record "wait $*"; }
kill() { record "kill $*"; }
load_rsid_scope_settings() {
    RSID_SCOPE_MEMORY_HIGH_MIB=6144 RSID_SCOPE_MEMORY_MAX_MIB=8192
    RSID_SCOPE_MEMORY_SWAP_MAX_MIB=0 RSID_SCOPE_CPU_WEIGHT=20
}
provision_worker_slice() { record provision; }
run_restart_drain_hook() { record drain; }
systemd-run() {
    record "systemd-run $*"
    local arg key
    for arg in "$@"; do
        if [[ "$arg" == --setenv=* && "${arg#--setenv=}" != *=* ]]; then
            key="${arg#--setenv=}"
            printf '%s=%s\n' "$key" "${!key}" >> "$FIXTURE/service-env"
        fi
    done
}
nohup() { record "nohup $*"; }
disown() { :; }
wait_for_rsid_health() { record "health $*"; }
verify_rsid_scope() { record "verify $*"; }
daemon_socket_path() { echo "$FIXTURE/custom.sock"; }
RSI_HOME_DIR="$FIXTURE/home"
INSTALL_DIR="$FIXTURE/install"
SOCKET_HOLD_BIN="$FIXTURE/rsi-socket-hold"
RSID_BIN="$INSTALL_DIR/rsid"
RSI_RPC_BIN="$INSTALL_DIR/rsi-rpc"
DAEMON_LOG="$FIXTURE/daemon.log"
'''
            # Give the mocked macOS nohup job time to write its arguments.
            result = subprocess.run(["bash", "-c", "umask 022\n" + health_helpers + harness + function + "\nrestart_rsid\nwait\nfor path in \"$RSI_HOME_DIR\" \"$DAEMON_LOG\"; do { stat -c '%a' \"$path\" 2>/dev/null || stat -f '%Lp' \"$path\"; } | tr '\\n' ' '; echo \"$path\"; done > \"$FIXTURE/modes\"\n"],
                env=dict(os.environ, FIXTURE=temp, PLATFORM=platform, RSI_MEMORY_ENABLED="false", XDG_CONFIG_HOME="/fixture/config with spaces", RSI_SANDBOX_BASE="/fixture/sandboxes", ANTHROPIC_API_KEY="fixture-provider-secret", RSI_SESSION_TOKEN="fixture-agent-secret"), capture_output=True,
                text=True, timeout=5)
            calls = (root / "calls").read_text() if (root / "calls").exists() else ""
            environment = (root / "service-env").read_text() if (root / "service-env").exists() else ""
            self.modes = (root / "modes").read_text() if (root / "modes").exists() else ""
            return result, calls, environment

    def test_linux_launcher_is_durable_and_wraps_the_supervisor_in_the_holder(self):
        result, calls, _ = self.restart("Linux")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("systemd-run --user --collect --service-type=exec", calls)
        self.assertIn(".service --slice=user.slice", calls)
        self.assertIn("MemoryHigh=6144M", calls)
        self.assertIn("MemoryMax=8192M", calls)
        self.assertIn("MemorySwapMax=0M", calls)
        self.assertIn("CPUWeight=20", calls)
        self.assertIn("--property=StandardOutput=append:", calls)
        self.assertIn("--setenv=RSI_SOCKET=", calls)
        self.assertRegex(calls, r"rsi-socket-hold \S+/custom.sock -- \S+/install/rsid-supervisor.sh \S+/install/rsid")
        self.assertIn("verify rsid-install-", calls)

    def test_log_and_data_dir_are_created_owner_only_before_output_is_redirected(self):
        # umask 022 is the hostile default: without the launcher's own umask 077
        # the redirect would create daemon.log 0644 in a 0755 directory (#1852).
        for platform in ("Linux", "Darwin"):
            with self.subTest(platform=platform):
                result, calls, _ = self.restart(platform)
                self.assertEqual(result.returncode, 0, result.stderr)
                modes = self.modes.splitlines()
                self.assertEqual(len(modes), 2, self.modes)
                self.assertTrue(modes[0].startswith("700 "), self.modes)
                self.assertTrue(modes[1].startswith("600 "), self.modes)

    def test_transient_service_receives_configuration_without_logging_secrets(self):
        result, calls, environment = self.restart("Linux")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("RSI_MEMORY_ENABLED=false\n", environment)
        self.assertIn("RSI_SANDBOX_BASE=/fixture/sandboxes\n", environment)
        self.assertIn("XDG_CONFIG_HOME=/fixture/config with spaces\n", environment)
        self.assertIn("--setenv=RSI_MEMORY_ENABLED", calls)
        for output in [result.stdout, result.stderr, calls, environment]:
            self.assertNotIn("fixture-provider-secret", output)
            self.assertNotIn("fixture-agent-secret", output)

    def test_replacement_waits_for_the_old_supervisor_and_holder(self):
        result, calls, _ = self.restart("Linux")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertLess(calls.index("snapshot 999999"), calls.index("kill -INT 999999"))
        self.assertLess(calls.index("wait 0.1"), calls.index("launcher-released"))
        self.assertLess(calls.index("launcher-released"), calls.index("systemd-run "))

    def test_launcher_snapshot_follows_only_known_parents_and_ignores_zombies(self):
        source = (SCRIPTS / "rsid-processes.sh").read_text()
        harness = r'''
ps() {
    case "$*" in
        '-o ppid= -p 10') echo 11 ;;
        '-o ppid= -p 11') echo 12 ;;
        '-o ppid= -p 12') echo 13 ;;
        '-o args= -p 11') echo '/bin/bash /fixture/rsid-supervisor.sh /fixture/rsid' ;;
        '-o args= -p 12') echo '/fixture/rsi-socket-hold /fixture/daemon.sock -- supervisor' ;;
        '-o args= -p 13') echo '/bin/bash' ;;
        '-o stat= -p 11') echo Z ;;
        '-o stat= -p 12') echo S ;;
    esac
}
rsid_launcher_pids 10
rsid_pids_still_running 11 12
'''
        result = subprocess.run(["bash", "-c", source + harness], capture_output=True, text=True, timeout=5)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(result.stdout.splitlines(), ["11", "12", "12"])

    def test_macos_uses_the_portable_holder_and_supervisor(self):
        result, calls, _ = self.restart("Darwin")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertRegex(calls, r"nohup \S+/rsi-socket-hold \S+/custom.sock -- \S+/install/rsid-supervisor.sh \S+/install/rsid")
        self.assertIn("health ", calls)

    def test_missing_holder_refuses_before_stopping_the_running_daemon(self):
        result, calls, _ = self.restart("Linux", holder_exists=False)
        self.assertEqual(result.returncode, 1)
        self.assertIn("refusing to start or restart without the front door", result.stderr)
        self.assertEqual(calls, "")


if __name__ == "__main__":
    unittest.main()
