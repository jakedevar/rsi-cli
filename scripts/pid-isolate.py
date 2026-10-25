#!/usr/bin/env python3
"""Run a test command in its own PID namespace so it cannot see or signal host processes (#3478).

A test that runs a host-wide `pgrep -x rsid`, `pkill` or `kill -1` finds the real hub daemon
when it shares the host PID namespace (#3476 killed it ten times from the merge-queue gate).
On Linux this wraps the command in `unshare --user --pid --fork --mount-proc`: the user
namespace maps the caller's own uid/gid (a single id, so files keep their owner), the PID
namespace plus a fresh /proc leave `pgrep`, `kill` and `/proc` seeing only the command's own
tree, and `--kill-child` takes the whole namespace down when the wrapper is killed.

Usage: scripts/pid-isolate.py [--require] -- COMMAND [ARG...]
       scripts/pid-isolate.py --status      (print the probe result as JSON)

The probe is a canary, not a version check: it starts a real namespace and fails unless the
child is PID 1 and cannot see the probing process or its parent. Where isolation cannot be had
(not Linux, no `unshare`, unprivileged user namespaces refused, `RSI_PID_ISOLATION=off`) the
mode is `unavailable` with a reason; COMMAND then runs unwrapped, as before, unless
`--require` makes that an error (exit 3). The chosen mode is printed on stderr as
`pid-isolate: pid_isolation=<mode> ...` so a gate log records it.
"""

import json
import os
import shutil
import subprocess
import sys

ENV_SWITCH = "RSI_PID_ISOLATION"  # "off" opts out (the receipt says `disabled`)
NAMESPACED = "namespaced"
UNAVAILABLE = "unavailable"
PROBE_TIMEOUT_SEC = 20
UNAVAILABLE_EXIT = 3

# Runs inside the namespace. It must be PID 1 of a PID namespace whose /proc does not show the
# probing process or its parent (host pids passed as arguments; low pids are skipped because
# the namespace's own init legitimately owns them).
CANARY = (
    "import os, sys\n"
    "if os.getpid() != 1:\n"
    "    sys.exit(11)\n"
    "for pid in sys.argv[1:]:\n"
    "    if int(pid) > 100 and os.path.exists('/proc/' + pid):\n"
    "        sys.exit(12)\n"
)

_cached = None


def unshare_prefix(mapping, uid=None, gid=None):
    """The `unshare ...` argv that precedes the command, for one of the id mappings."""
    base = ["unshare", "--user"]
    if mapping == "root":
        base.append("--map-root-user")
    else:
        base += [f"--map-user={os.getuid() if uid is None else uid}",
                 f"--map-group={os.getgid() if gid is None else gid}"]
    return base + ["--pid", "--fork", "--kill-child", "--mount-proc", "--"]


def probe(run=subprocess.run, platform=sys.platform, which=shutil.which, environ=None):
    """Start a real namespace and report `{"mode": ..., "reason": ..., "mapping": ...}`."""
    environ = os.environ if environ is None else environ
    if environ.get(ENV_SWITCH, "").lower() in ("off", "0", "false", "no"):
        return {"mode": UNAVAILABLE, "reason": "disabled"}
    if not platform.startswith("linux"):
        return {"mode": UNAVAILABLE, "reason": "not_linux"}
    if which("unshare") is None:
        return {"mode": UNAVAILABLE, "reason": "unshare_missing"}
    reason = "unknown"
    # The caller's own uid first: root inside the namespace would break the user bus and
    # make every file look root-owned. `--map-root-user` is the fallback for an older unshare.
    for mapping in ("current_user", "root"):
        argv = unshare_prefix(mapping) + [sys.executable, "-I", "-c", CANARY,
                                          str(os.getpid()), str(os.getppid())]
        try:
            done = run(argv, capture_output=True, text=True, timeout=PROBE_TIMEOUT_SEC, check=False)
        except (OSError, subprocess.SubprocessError) as error:
            reason = f"probe_failed:{type(error).__name__}"
            continue
        if done.returncode == 0:
            return {"mode": NAMESPACED, "reason": "", "mapping": mapping}
        if done.returncode == 12:
            return {"mode": UNAVAILABLE, "reason": "host_pids_visible"}
        lines = (done.stderr or "").strip().splitlines()
        detail = lines[-1][:160] if lines else ""
        reason = f"canary_exit_{done.returncode}" + (f": {detail}" if detail else "")
    return {"mode": UNAVAILABLE, "reason": reason}


def status():
    """The memoized probe result for this process."""
    global _cached
    if _cached is None:
        _cached = probe()
    return _cached


def is_namespaced(result=None):
    return (result or status()).get("mode") == NAMESPACED


def wrap(argv, result=None):
    """`argv` inside the namespace, or unchanged where isolation is unavailable."""
    result = result or status()
    if not is_namespaced(result):
        return list(argv)
    return unshare_prefix(result.get("mapping", "current_user")) + list(argv)


def describe(result):
    text = f"pid_isolation={result['mode']}"
    if result.get("mapping"):
        text += f" mapping={result['mapping']}"
    if result.get("reason"):
        text += f" reason={result['reason']}"
    return text


def main(argv):
    args = argv[1:]
    if args == ["--status"]:
        result = probe()
        print(json.dumps(result, sort_keys=True))
        return 0 if is_namespaced(result) else UNAVAILABLE_EXIT
    require = False
    while args and args[0] == "--require":
        require = True
        args = args[1:]
    if args[:1] == ["--"]:
        args = args[1:]
    if not args:
        print(__doc__, file=sys.stderr)
        return 2
    result = probe()
    print("pid-isolate: " + describe(result), file=sys.stderr, flush=True)
    if require and not is_namespaced(result):
        print("pid-isolate: --require: refusing to run without a PID namespace", file=sys.stderr)
        return UNAVAILABLE_EXIT
    command = wrap(args, result)
    try:
        os.execvp(command[0], command)
    except OSError as error:
        print(f"pid-isolate: cannot run {command[0]}: {error}", file=sys.stderr)
        return 127


if __name__ == "__main__":
    sys.exit(main(sys.argv))
