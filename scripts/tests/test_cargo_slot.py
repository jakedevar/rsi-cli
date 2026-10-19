"""Admission deadlines and recovery against a real Unix-socket governor stub."""

import json
import os
from pathlib import Path
import shutil
import socket
import subprocess
import tempfile
import threading
import time
import unittest

SCRIPT = Path(__file__).resolve().parents[1] / "cargo-slot"


@unittest.skipUnless(os.uname().sysname == "Linux" if hasattr(os, "uname") else False,
                     "cargo-slot's local fallback uses Linux resource probes")
class CargoSlot(unittest.TestCase):
    def setUp(self):
        if any(shutil.which(tool) is None for tool in ("bash", "flock", "ionice")):
            self.skipTest("cargo-slot prerequisites unavailable")
        # Unix socket paths are limited to 108 bytes; worker TMPDIRs can exceed that.
        self.temp = tempfile.TemporaryDirectory(prefix="cargo-slot-test-", dir="/tmp")
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.home = self.root / "home"
        self.env = dict(os.environ, HOME=str(self.home), RSI_CARGO_SLOT_POLL_SEC="1",
                        RSI_CARGO_MIN_FREE_GB="0", RSI_CARGO_MAX_LOAD="1000000",
                        RSI_CARGO_MIN_AVAIL_GB="0", RSI_CARGO_MAX_SLICE_GB="1000000",
                        RSI_CARGO_ADMISSION_MAX_SEC="1", RSI_CARGO_SLOT_LOCAL="1")

    def invoke(self, script=SCRIPT):
        return subprocess.run(["bash", str(script), "/bin/sh", "-c", "printf command-ran"],
                              env=self.env, text=True, capture_output=True, timeout=25)

    def test_local_gates_stop_admission_and_name_the_phase(self):
        for setting, value, phase in [
            ("RSI_CARGO_MAX_LOAD", "0", "local load wait"),
            ("RSI_CARGO_MIN_AVAIL_GB", "1000000", "memory wait (local)"),
            ("RSI_CARGO_MIN_FREE_GB", "1000000", "local disk wait"),
            ("RSI_CARGO_SLOTS", "0", "local slot wait"),
        ]:
            with self.subTest(phase=phase):
                previous = self.env.get(setting)
                self.env[setting] = value
                started = time.monotonic()
                result = self.invoke()
                elapsed = time.monotonic() - started
                if previous is None:
                    self.env.pop(setting)
                else:
                    self.env[setting] = previous
                self.assertEqual(result.returncode, 124, result.stderr)
                self.assertIn("admission-timeout: " + phase, result.stderr)
                self.assertLess(elapsed, 5)
                self.assertEqual(result.stdout, "")

    def reap(self, process):
        def stop():
            process.kill()
            process.wait()
        self.addCleanup(stop)

    def wait_for(self, predicate, message, timeout=5):
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            if predicate():
                return
            time.sleep(0.02)
        self.fail(f"timed out waiting for {message}")

    def test_local_slot_tickets_are_served_in_fifo_order(self):
        home = self.root / "fifo-home"
        release = self.root / "release-first"
        order = self.root / "run-order"
        env = dict(self.env, HOME=str(home), RSI_CARGO_SLOTS="1", RSI_CARGO_ADMISSION_MAX_SEC="30")
        holder = subprocess.Popen(
            ["bash", str(SCRIPT), "/bin/sh", "-c",
             f"printf '%s\\n' first >> {order}; while [ ! -e {release} ]; do sleep .02; done"],
            env=env, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
        queue = home / ".rsi/cargo-slot-queue/build/tickets"
        processes = [holder]
        try:
            self.wait_for(lambda: order.exists(), "first holder")
            first_waiter = subprocess.Popen(
                ["bash", str(SCRIPT), "/bin/sh", "-c", f"printf '%s\\n' second >> {order}"],
                env=env, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
            processes.append(first_waiter)
            self.wait_for(lambda: queue.exists() and len(list(queue.iterdir())) == 1, "first ticket")
            second_waiter = subprocess.Popen(
                ["bash", str(SCRIPT), "/bin/sh", "-c", f"printf '%s\\n' third >> {order}"],
                env=env, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
            processes.append(second_waiter)
            self.wait_for(lambda: len(list(queue.iterdir())) == 2, "second ticket")
            release.touch()
            for process in processes:
                out, err = process.communicate(timeout=10)
                self.assertEqual(process.returncode, 0, (out, err))
        finally:
            release.touch()
            for process in processes:
                if process.poll() is None:
                    process.kill()
                process.communicate(timeout=5)
        self.assertEqual(order.read_text().splitlines(), ["first", "second", "third"])
        self.assertEqual(list(queue.iterdir()), [])

    def test_local_queue_removes_a_dead_stale_pid_ticket(self):
        home = self.root / "stale-home"
        tickets = home / ".rsi/cargo-slot-queue/build/tickets"
        tickets.mkdir(parents=True)
        max_pid = int(Path("/proc/sys/kernel/pid_max").read_text())
        active = {int(path.name) for path in Path("/proc").iterdir() if path.name.isdecimal()}
        dead_pid = next(pid for pid in range(max_pid, 1, -1) if pid not in active)
        stale = tickets / f"00000000000000000001.{dead_pid}"
        stale.write_text(f"{dead_pid} 0\n")
        env = dict(self.env, HOME=str(home), RSI_CARGO_SLOTS="1")
        result = subprocess.run(["bash", str(SCRIPT), "/bin/sh", "-c", "printf ran"],
                                env=env, text=True, capture_output=True, timeout=10)
        self.assertEqual((result.returncode, result.stdout), (0, "ran"), result.stderr)
        self.assertFalse(stale.exists())

    def live_ticket(self, home, name_seq, pid, pool="build"):
        tickets = home / f".rsi/cargo-slot-queue/{pool}/tickets"
        tickets.mkdir(parents=True, exist_ok=True)
        ticket = tickets / f"{name_seq}.{pid}"
        ticket.write_text(f"{pid} 0\n")
        return ticket

    def test_older_live_ticket_blocks_a_free_slot_until_it_goes_away(self):
        # Deterministic FIFO: a free slot is not taken past an older live ticket.
        home = self.root / "blocked-home"
        older = subprocess.Popen(["sleep", "60"])
        self.reap(older)
        ticket = self.live_ticket(home, f"0{1:020d}", older.pid)
        env = dict(self.env, HOME=str(home), RSI_CARGO_SLOTS="2", RSI_CARGO_ADMISSION_MAX_SEC="2")
        blocked = subprocess.run(["bash", str(SCRIPT), "/bin/sh", "-c", "printf ran"],
                                 env=env, text=True, capture_output=True, timeout=15)
        self.assertEqual((blocked.returncode, blocked.stdout), (124, ""), blocked.stderr)
        self.assertIn("older ticket", blocked.stderr)
        self.assertTrue(ticket.exists())
        # The timed-out waiter removed its own ticket; the older one is untouched.
        self.assertEqual([p.name for p in ticket.parent.iterdir()], [ticket.name])
        older.kill(); older.wait()
        env["RSI_CARGO_ADMISSION_MAX_SEC"] = "10"
        freed = subprocess.run(["bash", str(SCRIPT), "/bin/sh", "-c", "printf ran"],
                               env=env, text=True, capture_output=True, timeout=15)
        self.assertEqual((freed.returncode, freed.stdout), (0, "ran"), freed.stderr)
        self.assertFalse(ticket.exists())

    def test_higher_priority_ticket_goes_ahead_of_an_older_normal_ticket(self):
        home = self.root / "priority-home"
        older = subprocess.Popen(["sleep", "60"])
        self.reap(older)
        ticket = self.live_ticket(home, f"9{1:020d}", older.pid)  # priority 0
        env = dict(self.env, HOME=str(home), RSI_CARGO_SLOTS="2",
                   RSI_CARGO_SLOT_PRIORITY="5", RSI_CARGO_ADMISSION_MAX_SEC="10")
        result = subprocess.run(["bash", str(SCRIPT), "/bin/sh", "-c", "printf ran"],
                                env=env, text=True, capture_output=True, timeout=15)
        self.assertEqual((result.returncode, result.stdout), (0, "ran"), result.stderr)
        self.assertTrue(ticket.exists())

    def test_local_disk_guard_wait_keeps_a_ticket_and_cleans_it_on_timeout(self):
        home = self.root / "disk-home"
        env = dict(self.env, HOME=str(home), RSI_CARGO_MIN_FREE_GB="1000000")
        result = subprocess.run(["bash", str(SCRIPT), "/bin/sh", "-c", "printf ran"],
                                env=env, text=True, capture_output=True, timeout=15)
        self.assertEqual((result.returncode, result.stdout), (124, ""), result.stderr)
        self.assertIn("admission-timeout: local disk wait", result.stderr)
        tickets = home / ".rsi/cargo-slot-queue/build/tickets"
        self.assertEqual(list(tickets.iterdir()), [])

    def test_local_status_lists_holders_and_the_ordered_queue_without_stale_tickets(self):
        home = self.root / "status-home"
        first = subprocess.Popen(["sleep", "60"]); self.reap(first)
        second = subprocess.Popen(["sleep", "60"]); self.reap(second)
        self.live_ticket(home, f"0{2:020d}", second.pid)
        self.live_ticket(home, f"0{1:020d}", first.pid)
        max_pid = int(Path("/proc/sys/kernel/pid_max").read_text())
        active = {int(path.name) for path in Path("/proc").iterdir() if path.name.isdecimal()}
        dead_pid = next(pid for pid in range(max_pid, 1, -1) if pid not in active)
        self.live_ticket(home, f"0{3:020d}", dead_pid)
        lock = home / ".rsi/build-slot-0.lock"
        (home / ".rsi").mkdir(parents=True, exist_ok=True)
        holder = subprocess.Popen(["flock", str(lock), "sleep", "60"])
        self.reap(holder)
        (home / ".rsi/build-slot-0.lock.holder").write_text("pid 1: a build\n")
        self.wait_for(lambda: subprocess.run(["flock", "-n", str(lock), "true"]).returncode != 0,
                      "slot holder lock")
        env = dict(self.env, HOME=str(home), RSI_DAEMON_SOCKET_PATH=str(self.root / "none.sock"))
        result = subprocess.run(["bash", str(SCRIPT), "--status"], env=env, text=True,
                                capture_output=True, timeout=10)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(result.stdout.splitlines(), [
            result.stdout.splitlines()[0],
            "holder build-slot-0.lock: pid 1: a build",
            f"queue build position 0 pid {first.pid}",
            f"queue build position 1 pid {second.pid}",
        ])

    def invoke_check(self, **env):
        """Run through cargo-slot with `cargo check` on the command line (the
        stand-in shell ignores the words, cargo-slot only classifies them)."""
        home = self.root / "home"
        (home / ".rsi").mkdir(parents=True, exist_ok=True)
        run_env = dict(self.env, HOME=str(home), **env)
        return subprocess.run(
            ["bash", str(SCRIPT), "/bin/sh", "-c", "printf command-ran", "cargo", "check", "-p", "x"],
            env=run_env, text=True, capture_output=True, timeout=25)

    def test_cargo_check_uses_its_own_local_pool_and_ignores_the_load_gate(self):
        # #2430: a closed build pool and a closed load gate hold a build but not a check.
        busy = {"RSI_CARGO_SLOTS": "0", "RSI_CARGO_MAX_LOAD": "0"}
        result = self.invoke_check(**busy)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(result.stdout, "command-ran")
        # The check pool is a pool of its own: with zero check slots it times out as a slot wait.
        result = self.invoke_check(RSI_CHECK_SLOTS="0")
        self.assertEqual(result.returncode, 124, result.stderr)
        self.assertIn("admission-timeout: local slot wait", result.stderr)
        self.assertIn("check slots busy", result.stderr)
        # A non-check cargo run is still gated by the same closed load gate.
        self.env["RSI_CARGO_MAX_LOAD"] = "0"
        blocked = self.invoke()
        self.assertEqual(blocked.returncode, 124, blocked.stderr)

    def test_cargo_check_asks_the_governor_for_the_check_class(self):
        ticket = "00000000-0000-0000-0000-000000000009"
        acquires = []

        def reply(request):
            if request["method"] == "ReleaseAdmission":
                return {"released": True}
            acquires.append(request["params"])
            return {"status": "granted", "lease_id": ticket}

        self.serve(reply)
        for words, expected in [(["cargo", "check", "-p", "x"], "check"),
                                (["cargo", "test", "-p", "x"], "build"),
                                (["cargo", "build", "--check"], "build")]:
            acquires.clear()
            result = subprocess.run(["bash", str(SCRIPT), "/bin/sh", "-c", "printf ran", *words],
                                    env=self.env, text=True, capture_output=True, timeout=25)
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertEqual([p["class"] for p in acquires][:1], [expected], words)

    def fake_pool_tool(self):
        """A stand-in target-pool that counts how often cargo-slot asks it to reclaim."""
        tool = self.root / "target-pool"
        calls = self.root / "reclaim-calls"
        tool.write_text(f'#!/bin/sh\necho "$@" >> {calls}\n'
                        'echo \'{"removed": ["pool-a"], "freed_bytes": 100}\'\n')
        tool.chmod(0o755)
        self.env["RSI_TARGET_POOL_TOOL"] = str(tool)
        return calls

    def test_a_worker_waiting_on_the_disk_floor_reclaims_the_idle_pool_once(self):
        # #2773: the governor reports the disk floor; cargo-slot evicts idle target pools once
        # (bounded), records it as a phase line, and keeps waiting until the governor grants.
        ticket = "00000000-0000-0000-0000-000000000011"
        acquires = []

        def reply(request):
            if request["method"] == "ReleaseAdmission":
                return {"released": True}
            acquires.append(request["params"])
            if len(acquires) <= 3:
                return {"status": "queued", "ticket_id": ticket, "position": 1,
                        "message": "only 29 GB free on / (< 30); waiting for space"}
            return {"status": "granted", "lease_id": ticket}

        calls = self.fake_pool_tool()
        self.serve(reply)
        self.env["RSI_CARGO_ADMISSION_MAX_SEC"] = "30"
        result = self.invoke()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(result.stdout, "command-ran")
        self.assertGreaterEqual(len(acquires), 4)
        self.assertEqual(calls.read_text().split("\n")[:-1], ["reclaim-idle"])
        self.assertIn("slot-wait: disk floor: reclaimed idle target pools:", result.stderr)
        self.assertIn("pool-a", result.stderr)

    def test_other_waits_and_pool_builds_do_not_reclaim_the_pool(self):
        ticket = "00000000-0000-0000-0000-000000000012"

        def reply(request):
            if request["method"] == "ReleaseAdmission":
                return {"released": True}
            return {"status": "queued", "ticket_id": ticket, "position": 2,
                    "message": "all 4 build slots busy"}

        calls = self.fake_pool_tool()
        self.serve(reply)
        self.assertEqual(self.invoke().returncode, 124)
        self.assertFalse(calls.exists(), "a slot wait must leave the warm pool alone")

    def test_the_local_disk_wait_reclaims_once_and_a_pool_build_never_does(self):
        calls = self.fake_pool_tool()
        self.env["RSI_CARGO_MIN_FREE_GB"] = "1000000"
        self.env["RSI_CARGO_ADMISSION_MAX_SEC"] = "3"
        result = self.invoke()
        self.assertEqual(result.returncode, 124, result.stderr)
        self.assertIn("admission-timeout: local disk wait", result.stderr)
        self.assertEqual(calls.read_text().split("\n")[:-1], ["reclaim-idle"])
        calls.unlink()
        self.env["RSI_CARGO_NO_POOL_RECLAIM"] = "1"
        self.assertEqual(self.invoke().returncode, 124)
        self.assertFalse(calls.exists(), "a pool build must not evict its own ancestors")

    def test_status_lists_current_governor_holders_and_queue_order(self):
        def reply(_request):
            return {
                "leases": [{"class": "build", "pid": 123, "label": "test holder",
                            "granted_at": "2026-01-01T00:00:00Z"}],
                "queue": [
                    {"class": "build", "position": 0, "pid": 456, "label": "older"},
                    {"class": "build", "position": 1, "pid": 789, "label": "newer"},
                ],
            }

        self.serve(reply)
        result = subprocess.run(["bash", str(SCRIPT), "--status"], env=self.env,
                                text=True, capture_output=True, timeout=10)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(result.stdout.splitlines(), [
            "holder build pid 123: test holder",
            "queue build position 0 pid 456: older",
            "queue build position 1 pid 789: newer",
        ])

    def local_holder_and_waiter(self, home):
        """A live local slot holder and one live local ticket under `home`."""
        waiter = subprocess.Popen(["sleep", "60"]); self.reap(waiter)
        self.live_ticket(home, f"0{1:020d}", waiter.pid)
        lock = home / ".rsi/build-slot-0.lock"
        holder = subprocess.Popen(["flock", str(lock), "sleep", "60"]); self.reap(holder)
        (home / ".rsi/build-slot-0.lock.holder").write_text("pid 1: a local build\n")
        self.wait_for(lambda: subprocess.run(["flock", "-n", str(lock), "true"]).returncode != 0,
                      "slot holder lock")
        return waiter

    def test_status_shows_live_local_holders_and_waiters_beside_a_reachable_daemon(self):
        # #2778: a healthy daemon with an empty queue must not hide local fallback runs.
        self.serve(lambda _request: {"leases": [], "queue": []})
        home = self.root / "both-home"
        waiter = self.local_holder_and_waiter(home)
        env = dict(self.env, HOME=str(home))
        result = subprocess.run(["bash", str(SCRIPT), "--status"], env=env, text=True,
                                capture_output=True, timeout=10)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(result.stdout.splitlines(), [
            "local fallback (live local holders and waiters beside the daemon)",
            "holder build-slot-0.lock: pid 1: a local build",
            f"queue build position 0 pid {waiter.pid}",
        ])

    def test_status_lists_daemon_state_first_then_the_labelled_local_state(self):
        self.serve(lambda _request: {
            "leases": [{"class": "build", "pid": 123, "label": "daemon holder",
                        "granted_at": "2026-01-01T00:00:00Z"}],
            "queue": [{"class": "build", "position": 0, "pid": 456, "label": "daemon waiter"}],
        })
        home = self.root / "both-order-home"
        waiter = self.local_holder_and_waiter(home)
        env = dict(self.env, HOME=str(home))
        result = subprocess.run(["bash", str(SCRIPT), "--status"], env=env, text=True,
                                capture_output=True, timeout=10)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(result.stdout.splitlines(), [
            "holder build pid 123: daemon holder",
            "queue build position 0 pid 456: daemon waiter",
            "local fallback (live local holders and waiters beside the daemon)",
            "holder build-slot-0.lock: pid 1: a local build",
            f"queue build position 0 pid {waiter.pid}",
        ])

    def test_forced_local_status_reports_the_local_backend_and_skips_the_daemon(self):
        requests = self.serve(lambda _request: {
            "leases": [{"class": "build", "pid": 123, "label": "daemon holder",
                        "granted_at": "2026-01-01T00:00:00Z"}],
            "queue": [],
        })
        home = self.root / "forced-home"
        waiter = self.local_holder_and_waiter(home)
        env = dict(self.env, HOME=str(home), RSI_CARGO_SLOT_LOCAL="1")
        result = subprocess.run(["bash", str(SCRIPT), "--status"], env=env, text=True,
                                capture_output=True, timeout=10)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(result.stdout.splitlines(), [
            "local fallback (RSI_CARGO_SLOT_LOCAL=1: daemon skipped)",
            "holder build-slot-0.lock: pid 1: a local build",
            f"queue build position 0 pid {waiter.pid}",
        ])
        self.assertEqual(requests, [])

    def serve(self, handler):
        listener = socket.socket(socket.AF_UNIX)
        path = str(self.root / "governor.sock")
        listener.bind(path)
        listener.listen()
        listener.settimeout(0.1)
        self.env.pop("RSI_CARGO_SLOT_LOCAL")
        self.env["RSI_DAEMON_SOCKET_PATH"] = path
        stopped = threading.Event()
        errors, requests = [], []

        def run():
            while not stopped.is_set():
                try:
                    connection, _ = listener.accept()
                except socket.timeout:
                    continue
                with connection:
                    connection.settimeout(2)
                    try:
                        with connection.makefile("rb") as source:
                            request = json.loads(source.readline())
                        requests.append(request)
                        result = handler(request)
                        if result is not None:
                            reply = {"jsonrpc": "2.0", "id": 1, "result": result}
                            connection.sendall((json.dumps(reply) + "\n").encode())
                    except BrokenPipeError:
                        pass  # the deliberately slow reply outlives its client
                    except Exception as error:
                        errors.append(error)

        thread = threading.Thread(target=run)
        thread.start()

        def stop():
            stopped.set()
            thread.join(timeout=10)
            listener.close()
            self.assertFalse(thread.is_alive())
            self.assertEqual(errors, [])

        self.addCleanup(stop)
        return requests

    def test_memory_wait_survives_a_transient_governor_timeout_with_the_same_ticket(self):
        ticket = "00000000-0000-0000-0000-000000000001"
        acquires = []

        def reply(request):
            if request["method"] == "ReleaseAdmission":
                return {"released": True}
            acquires.append(request["params"])
            if len(acquires) == 1:
                return {"status": "queued", "ticket_id": ticket, "position": 2,
                        "message": "memory low (available 15 GB < 16); waiting"}
            if len(acquires) == 2:
                time.sleep(5.2)  # original incident: daemon stalled longer than the 5 s RPC cap
            return {"status": "granted", "lease_id": ticket}

        requests = self.serve(reply)
        self.env["RSI_CARGO_ADMISSION_MAX_SEC"] = "20"
        # Local fallback would stall; successful execution must use the governor.
        self.env["RSI_CARGO_MAX_LOAD"] = "0"
        result = self.invoke()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(result.stdout, "command-ran")
        self.assertIn("memory wait (governor)", result.stderr)
        self.assertIn("transient RPC failure: TimeoutError", result.stderr)
        self.assertGreaterEqual(len(acquires), 3)
        self.assertTrue(all(p["ticket_id"] == ticket for p in acquires[1:]))
        self.assertEqual(requests[-1]["method"], "ReleaseAdmission")
        self.assertEqual(requests[-1]["params"]["lease_id"], ticket)

    def test_governor_queue_is_bounded_and_cancels_its_ticket(self):
        ticket = "00000000-0000-0000-0000-000000000002"

        def reply(request):
            if request["method"] == "ReleaseAdmission":
                return {"released": True}
            return {"status": "queued", "ticket_id": ticket, "position": 2,
                    "message": "all 4 build slots busy"}

        requests = self.serve(reply)
        result = self.invoke()
        self.assertEqual(result.returncode, 124, result.stderr)
        self.assertEqual(result.stdout, "")
        self.assertIn("admission-timeout: governor wait", result.stderr)
        self.assertEqual(requests[-1]["method"], "ReleaseAdmission")
        self.assertEqual(requests[-1]["params"]["lease_id"], ticket)

    def test_budgeted_caller_can_extend_the_interactive_admission_limit(self):
        # Accelerate the 600s interactive default to 1s; the override must still
        # grant after 2s instead of clamping a daemon job back to that default.
        script = self.root / "cargo-slot"
        script.write_text(SCRIPT.read_text().replace("admission_max=600", "admission_max=1"))
        ticket = "00000000-0000-0000-0000-000000000003"
        acquires = []

        def reply(request):
            if request["method"] == "ReleaseAdmission":
                return {"released": True}
            acquires.append(request)
            if len(acquires) == 1:
                return {"status": "queued", "ticket_id": ticket, "position": 1,
                        "message": "all 4 build slots busy"}
            return {"status": "granted", "lease_id": ticket}

        requests = self.serve(reply)
        self.env["RSI_CARGO_ADMISSION_MAX_SEC"] = "10"
        result = self.invoke(script)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(result.stdout, "command-ran")
        self.assertGreaterEqual(len(acquires), 2)
        self.assertEqual(requests[-1]["method"], "ReleaseAdmission")

    def test_unavailable_governor_records_the_error_before_local_admission_timeout(self):
        self.env.pop("RSI_CARGO_SLOT_LOCAL")
        self.env["RSI_DAEMON_SOCKET_PATH"] = str(self.root / "missing.sock")
        self.env["RSI_CARGO_MAX_LOAD"] = "0"
        result = self.invoke()
        self.assertEqual(result.returncode, 124, result.stderr)
        self.assertIn("governor-error: unreachable FileNotFoundError", result.stderr)
        self.assertIn("admission-timeout: local load wait", result.stderr)

    def run_lander(self, **env):
        """Run a stand-in `rsi-rolling-land` through cargo-slot's local lander slots
        (a private HOME holds the slot locks); return its TMPDIR, its
        CARGO_TARGET_DIR and cargo-slot's stderr."""
        lander = self.root / "rsi-rolling-land"
        lander.write_text('#!/bin/sh\nprintf "%s\\n%s\\n" "$TMPDIR" "${CARGO_TARGET_DIR-unset}"\n')
        lander.chmod(0o755)
        home = self.root / "home"
        (home / ".rsi").mkdir(parents=True, exist_ok=True)
        run_env = dict(self.env, HOME=str(home), RSI_CARGO_ADMISSION_MAX_SEC="20",
                       TMPDIR=str(self.root), **env)
        result = subprocess.run(["bash", str(SCRIPT), str(lander)], env=run_env,
                                text=True, capture_output=True, timeout=25)
        self.assertEqual(result.returncode, 0, result.stderr)
        tmpdir, target = result.stdout.splitlines()
        return tmpdir, target, result.stderr

    def test_cargo_target_dir_reaches_the_command_unchanged(self):
        # #938: cargo-slot never overrides an explicit target (a lander side's).
        side = str(self.root / "candidate-cargo-target")
        self.env["CARGO_TARGET_DIR"] = side
        result = subprocess.run(["bash", str(SCRIPT), "/bin/sh", "-c", 'printf %s "$CARGO_TARGET_DIR"'],
                                env=self.env, text=True, capture_output=True, timeout=25)
        self.assertEqual((result.returncode, result.stdout), (0, side), result.stderr)
        self.assertEqual(self.run_lander()[1], side)
        self.env.pop("CARGO_TARGET_DIR")
        self.assertEqual(self.run_lander()[1], "unset")

    @unittest.skipUnless(os.path.isdir("/dev/shm") and os.access("/dev/shm", os.W_OK), "no writable /dev/shm")
    def test_lander_tmpdir_goes_to_tmpfs_only_with_room_for_this_user(self):
        tmpdir, _, _ = self.run_lander(RSI_LANDER_TMPFS_MIN_GB="0")
        self.assertTrue(tmpdir.startswith("/dev/shm/rsi-lander-"), tmpdir)
        self.assertFalse(os.path.exists(tmpdir))  # removed after the run
        tmpdir, _, stderr = self.run_lander(RSI_LANDER_TMPFS_MIN_GB="1000000")
        self.assertEqual(tmpdir, str(self.root))
        self.assertIn("lander TMPDIR stays", stderr)

    @unittest.skipUnless(os.path.isdir("/dev/shm") and os.access("/dev/shm", os.W_OK), "no writable /dev/shm")
    def test_lander_tmpdir_counts_the_user_quota_not_just_free_space(self):
        # #938: systemd's per-user tmpfs quota (80% of the mount) runs out before
        # df's free space; a lander TMPDIR sized by df failed with EDQUOT.
        stats = os.statvfs("/dev/shm")
        free_gib = stats.f_bavail * stats.f_frsize // (1 << 30)
        quota = user_block_quota("/dev/shm")
        if quota is None:
            self.skipTest("no user block quota on /dev/shm")
        hard, used = quota
        room_gib = max(0, hard - used) // (1 << 30)
        if free_gib - room_gib < 4:
            self.skipTest(f"quota room {room_gib} GiB is not well below free space {free_gib} GiB")
        # More than the quota leaves, less than df reports: free space alone
        # would pick tmpfs, the quota must not.
        wanted = (room_gib + free_gib) // 2
        tmpdir, _, stderr = self.run_lander(RSI_LANDER_TMPFS_MIN_GB=str(wanted))
        if tmpdir == str(self.root):
            self.assertIn("quota included", stderr)
            return
        # Other users of /dev/shm may have freed gigabytes meanwhile; a tmpfs
        # pick is right only if the quota room really grew.
        hard, used = user_block_quota("/dev/shm")
        self.assertGreaterEqual(max(0, hard - used) // (1 << 30) + 1, wanted, (tmpdir, stderr))


def user_block_quota(path):
    """(hard limit, used) bytes of this user's block quota on `path`, or None."""
    import ctypes
    import struct
    try:
        fd = os.open(path, os.O_RDONLY | os.O_DIRECTORY)
    except OSError:
        return None
    try:
        dqblk = ctypes.create_string_buffer(72)
        libc = ctypes.CDLL(None, use_errno=True)
        if libc.syscall(443, fd, ctypes.c_uint((0x800007 << 8) | 0), ctypes.c_uint(os.geteuid()), dqblk) != 0:
            return None
    finally:
        os.close(fd)
    hard_kib, _soft, used = struct.unpack_from("=QQQ", dqblk.raw)
    return (hard_kib * 1024, used) if hard_kib else None


if __name__ == "__main__":
    unittest.main()
