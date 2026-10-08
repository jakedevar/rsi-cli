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
        self.assertIn("ExecStart=%h/.local/bin/rsid", service)
        self.assertIn("UMask=0077", service)

    def restart(self, platform, holder_exists=True):
        source = (SCRIPTS / "install-release.sh").read_text()
        function = source[source.index("restart_rsid() {"):source.index("load_rsid_scope_settings() {")]
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
systemctl() { return 1; }
rsid_running_pids() {
    if [[ ! -f "$FIXTURE/probed" ]]; then touch "$FIXTURE/probed"; echo 999999; fi
}
kill() { record "kill $*"; }
load_rsid_scope_settings() {
    RSID_SCOPE_MEMORY_HIGH_MIB=6144 RSID_SCOPE_MEMORY_MAX_MIB=8192
    RSID_SCOPE_MEMORY_SWAP_MAX_MIB=0 RSID_SCOPE_CPU_WEIGHT=20
}
provision_worker_slice() { record provision; }
run_restart_drain_hook() { record drain; }
systemd-run() { record "systemd-run $*"; }
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
            result = subprocess.run(["bash", "-c", harness + function + "\nrestart_rsid\nwait\n"],
                env=dict(os.environ, FIXTURE=temp, PLATFORM=platform), capture_output=True,
                text=True, timeout=5)
            calls = (root / "calls").read_text() if (root / "calls").exists() else ""
            return result, calls

    def test_linux_launcher_is_durable_and_wraps_the_supervisor_in_the_holder(self):
        result, calls = self.restart("Linux")
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

    def test_macos_uses_the_portable_holder_and_supervisor(self):
        result, calls = self.restart("Darwin")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertRegex(calls, r"nohup \S+/rsi-socket-hold \S+/custom.sock -- \S+/install/rsid-supervisor.sh \S+/install/rsid")
        self.assertIn("health ", calls)

    def test_missing_holder_refuses_before_stopping_the_running_daemon(self):
        result, calls = self.restart("Linux", holder_exists=False)
        self.assertEqual(result.returncode, 1)
        self.assertIn("refusing a restart without the front door", result.stderr)
        self.assertEqual(calls, "")


if __name__ == "__main__":
    unittest.main()
