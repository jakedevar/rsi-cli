#!/usr/bin/env python3
"""Select and run the `desktop` test lane (hub #3001); the merge queue and scoped-test share it.

A diff that touches an input of the desktop suite selects the lane; a diff elsewhere pays nothing for it
(`desktop/README.md` documents it for the app's developers). The lane fails closed (#355): a
candidate without this script whose diff touches it (a delete or rename) or an input is `SelectorMissing`,
never an empty selection. The inputs are ONE list, `trigger_inputs`:
`desktop/`, the root `Cargo.toml` (a conservative whole-file trigger), the files its tests read outside it (`OUTSIDE_INPUTS`: `docs/keybindings.md` for the keymap and
parity tests, `crates/rsi/src` for the parity test's module listing) and the repository directories
`desktop/src-tauri/Cargo.toml` reaches through `path = ...` dependencies (read from the manifest, followed
through the dependencies' own manifests, so a new path dependency needs no edit here). The selector, scoped-test,
check-touched-shards and the merge queue all select with it. The inputs only decide WHETHER the lane runs; what
it runs on is the whole candidate tree (below), so they need not list what the build reads.
Selection (no flag but `--base`) prints a JSON list of argv lists, one per command to run, like
`python-test-gate.py`: `[]` when no input changed, else one command that runs the lane.
`--run` runs the lane against `--repo` and prints a final `DESKTOP_LANE_RESULT {json}` line.

The lane runs both of the desktop's own test sets on a private copy of the FULL candidate tree, never the
committed tree (`npm ci` would rewrite the tracked `node_modules`) and never a selection of it (#3129; the
selective copy failed twice, on the root `Cargo.toml` in #3114 and on an `include_str!` of
`docs/manual/rsi-manual.md`, and would fail again on the next `include_bytes!`, `build.rs` read or workspace
inheritance). The copy is every tracked or untracked-but-not-ignored file (`stage_tree`) minus build output,
`node_modules` and `.git`; the workspace root manifest, `.cargo/` and the toolchain pin come with it. Then:

1. `npm ci` then `npm test` in the copy's `desktop/` (the frontend build plus `node --test`);
2. `cargo test --locked` in the copy's `desktop/src-tauri`, a standalone Cargo workspace with its own
   target directory: cargo builds only that crate's dependency closure, never the repository workspace.
   A lock update refusal is `desktop_lock_stale`, naming the refresh command. Path-dependency edits
   already select this same lane in scoped-test and the lander, so they cannot publish a stale lock.

Private state: the copy and the npm cache live in a fresh temp directory removed afterwards (a caller
that trusts a warm cache names it in `$DESKTOP_LANE_NPM_CACHE`); the Cargo target is
`$DESKTOP_LANE_CARGO_TARGET`, else `$CARGO_TARGET_DIR/desktop-tauri` (inside the job's target, a
Cargo tree the sandbox reclaim recognizes by structure), else inside the copy's temp directory.
The caller's RUSTC_WRAPPER and toolchain pin are kept.
To reproduce a gate failure by hand run `python3 scripts/desktop-test-gate.py --run` from the repo root.

A missing runner (node, npm, cargo, or on Linux pkg-config and `webkit2gtk-4.1`) is the typed
`runner_unavailable` with `executed_tests: 0`, never green. Both test sets must execute at least
one test: a zero-test set is `no_tests_executed`, also red.

Each step has a wall-clock bound, including draining output and waiting for descendants:
`DESKTOP_LANE_NPM_CI_TIMEOUT_SECONDS` defaults to 900 (15 minutes),
`DESKTOP_LANE_NPM_TEST_TIMEOUT_SECONDS` to 600 (10 minutes, including `npm run build`), and
`DESKTOP_LANE_CARGO_TEST_TIMEOUT_SECONDS` to 1800 (30 minutes for a cold Cargo build).
Overrides must be positive finite seconds. Expiry is `desktop_lane_timed_out`, naming `failed_step`.
Unix steps own a session/process group; cleanup sends SIGTERM then SIGKILL after two seconds.
Windows uses a new process group and bounded `taskkill /T /F`, with direct termination as a fallback.
"""

import argparse
import json
import math
import os
import posixpath
from pathlib import Path
import queue
import re
import shutil
import signal
import subprocess
import sys
import tempfile
import threading
import time
import tomllib

LANE = "desktop"
PREFIX = "desktop/"
RUNNER_UNAVAILABLE = "runner_unavailable"
RESULT_MARK = "DESKTOP_LANE_RESULT "
TIMED_OUT = "desktop_lane_timed_out"
LOCK_STALE = "desktop_lock_stale"
LOCK_FIX = ("refresh desktop/src-tauri/Cargo.lock: run "
            "`cargo update --workspace --manifest-path desktop/src-tauri/Cargo.toml` "
            "and commit the refreshed lock with the dependency change")
STEP_TIMEOUTS = {"npm ci": ("DESKTOP_LANE_NPM_CI_TIMEOUT_SECONDS", 900),
                 "npm test": ("DESKTOP_LANE_NPM_TEST_TIMEOUT_SECONDS", 600),
                 "cargo test": ("DESKTOP_LANE_CARGO_TEST_TIMEOUT_SECONDS", 1800)}
TERMINATE_GRACE_SECONDS = 2
SCRIPT = "scripts/desktop-test-gate.py"
# Paths of the copy that are rebuilt (generated), relative to `desktop/`.
SKIPPED = ("dist", "src-tauri/gen")
# Directory names left out of the copy at any depth: build output, installed packages and git metadata.
STAGED_SKIPPED = ("target", ".git", "node_modules")
# Root Cargo.toml is deliberately a whole-file trigger: workspace inheritance and dependency edits
# can affect the desktop build. Keep selection conservative rather than diffing individual TOML tables.
# Repository paths outside `desktop/` that its own tests read (`scripts/parity.mjs` and the keymap test
# resolve them from the copy's parent). `crates/rsi/src` is the whole tree, not a narrowed subset: the parity
# test only lists the names under `ui/` and `ui/overlay/` and checks the module files `parity-lib.mjs` names
# exist, but a test that starts reading a source file's text would silently outgrow any narrower list.
OUTSIDE_INPUTS = ("Cargo.toml", "docs/keybindings.md", "crates/rsi/src")
MANIFEST = "desktop/src-tauri/Cargo.toml"
DEPENDENCY_TABLES = ("dependencies", "dev-dependencies", "build-dependencies")
NODE_TESTS = re.compile(r"^(?:# |ℹ )tests (\d+)\s*$")
CARGO_PASSED = re.compile(r"^test result: .*?(\d+) passed;")


def dependency_tables(manifest, strict=False):
    """Every table of a parsed manifest that maps dependency names to specs (target-specific and workspace too)."""
    tables = [manifest.get(name) for name in DEPENDENCY_TABLES]
    workspace = manifest.get("workspace")
    if workspace is not None:
        if strict and not isinstance(workspace, dict):
            raise ValueError("invalid workspace table")
        tables.append((workspace or {}).get("dependencies"))
    targets = manifest.get("target") or {}
    if strict and not isinstance(targets, dict):
        raise ValueError("invalid target table")
    for target in targets.values():
        if strict and not isinstance(target, dict):
            raise ValueError("invalid target dependency table")
        tables.extend(target.get(name) for name in DEPENDENCY_TABLES if isinstance(target, dict))
    patches = manifest.get("patch") or {}
    if strict and not isinstance(patches, dict):
        raise ValueError("invalid patch table")
    if isinstance(patches, dict):
        tables.extend(patches.values())
    tables.append(manifest.get("replace"))
    if strict and any(table is not None and not isinstance(table, dict) for table in tables):
        raise ValueError("invalid dependency table")
    return [table for table in tables if isinstance(table, dict)]


def path_dependencies(text, manifest_dir, workspace_paths=None):
    """Repository-relative directories the manifest text reaches through `path = ...` dependencies.

    A dependency written `name.workspace = true` resolves through `workspace_paths` (name -> repository-relative
    directory, from the root manifest's `workspace.dependencies`). A path leaving the repository, or naming
    its root, is not a trigger input and is dropped."""
    try:
        manifest = tomllib.loads(text)
    except tomllib.TOMLDecodeError:
        return []
    found = []
    for table in dependency_tables(manifest):
        for name, spec in table.items():
            if isinstance(spec, dict) and spec.get("workspace") is True and name in (workspace_paths or {}):
                directory = workspace_paths[name]
            else:
                path = spec.get("path") if isinstance(spec, dict) else None
                if not isinstance(path, str):
                    continue
                directory = posixpath.normpath(posixpath.join(manifest_dir, path))
            if directory in (".", "") or directory.startswith(("..", "/")):
                continue
            found.append(directory)
    return found


def parse_root(read):
    """The repository root manifest as a dict (empty when missing or unparsable)."""
    text = read("Cargo.toml")
    try:
        return tomllib.loads(text) if text is not None else {}
    except tomllib.TOMLDecodeError:
        return {}


def workspace_dependency_paths(root):
    """name -> repository-relative directory for the root manifest's `workspace.dependencies` with a `path`."""
    found = {}
    for name, spec in ((root.get("workspace") or {}).get("dependencies") or {}).items():
        path = spec.get("path") if isinstance(spec, dict) else None
        if isinstance(path, str):
            found[name] = posixpath.normpath(path)
    return found


def outside_inputs(read):
    """Repository paths outside `desktop/` the suite reads: `OUTSIDE_INPUTS` and the path-dependency directories.

    `read(relative_path)` returns a file's text or None; the dependency manifests are followed transitively."""
    inputs = list(OUTSIDE_INPUTS)
    workspace_paths = workspace_dependency_paths(parse_root(read))
    seen = set()
    queue = [MANIFEST]
    while queue:
        manifest = queue.pop()
        if manifest in seen:
            continue
        seen.add(manifest)
        text = read(manifest)
        if text is None:
            continue
        for directory in path_dependencies(text, posixpath.dirname(manifest), workspace_paths):
            queue.append(directory + "/Cargo.toml")
            if directory != "desktop" and not directory.startswith(PREFIX):
                inputs.append(directory)
    return sorted(set(inputs))


def trigger_inputs(read):
    """Every repository path whose change selects the lane: `desktop`, then `outside_inputs(read)`."""
    return ["desktop", *outside_inputs(read)]


def fs_reader(repo):
    """A `read` over the working tree at `repo`."""
    def read(relative):
        try:
            return (Path(repo) / relative).read_text(encoding="utf-8")
        except (OSError, UnicodeDecodeError):
            return None
    return read


def git_reader(repo, revision):
    """A `read` over the tree of `revision` in the repository at `repo`."""
    def read(relative):
        result = subprocess.run(["git", "show", f"{revision}:{relative}"], cwd=repo, capture_output=True)
        if result.returncode != 0:
            return None
        text = result.stdout
        return text if isinstance(text, str) else text.decode("utf-8", "replace")
    return read


def static_inputs():
    """The inputs that need no manifest: what selects the lane when the dependency list is unknown."""
    return ["desktop", *OUTSIDE_INPUTS]


def under(path, inputs):
    """Whether `path` is an input or inside an input directory."""
    path = str(path).replace("\\", "/")
    return any(path == item or path.startswith(item + "/") for item in inputs)


def changed_paths(repo, base, head, inputs=None):
    """Paths of the diff base..head that are lane inputs (the manifest is read at `head` unless `inputs` is given)."""
    inputs = trigger_inputs(git_reader(repo, head)) if inputs is None else inputs
    result = subprocess.run(
        ["git", "diff", "--name-only", "--no-renames", "-z", base, head, "--", *inputs],
        cwd=repo, check=True, capture_output=True,
    )
    return [path.decode("utf-8", "surrogateescape") for path in result.stdout.split(b"\0") if path]


def selected(paths, inputs=None):
    """Whether any changed path is a lane input (`inputs` defaults to the manifest-free static list)."""
    inputs = static_inputs() if inputs is None else inputs
    return any(under(path, inputs) for path in paths)


class SelectorMissing(RuntimeError):
    """The candidate has no `SCRIPT` but its diff needs it: the lane cannot be skipped (#355)."""


def missing_selector_inputs(read):
    """Conservative manifest closure when the candidate cannot provide its selector (#3136).

    Every path dependency (including optional, target, build and dev dependencies) is followed. Inherited
    dependencies resolve in the declaring crate's nearest workspace, or its explicit package.workspace.
    Missing/malformed reachable manifests or unresolvable paths refuse instead of dropping an edge.
    A tree without a desktop manifest has only the static inputs. Rust's fallback follows the same rules;
    scripts/tests/fixtures/desktop-selection.json exercises both implementations.
    """
    inputs = static_inputs()
    if read(MANIFEST) is None:
        return inputs
    cache = {}

    def manifest_at(path):
        if path not in cache:
            text = read(path)
            if text is None:
                raise ValueError(f"cannot read {path}")
            try:
                cache[path] = tomllib.loads(text)
            except tomllib.TOMLDecodeError as error:
                raise ValueError(f"cannot parse {path}: {error}") from error
        return cache[path]

    def directory_at(parent, path, allow_root=False):
        # Reject platform-dependent paths too, so Linux and Windows make the same decision.
        if not isinstance(path, str) or "\\" in path or ":" in path:
            raise ValueError(f"unresolvable dependency path {path!r}")
        directory = posixpath.normpath(posixpath.join(parent, path))
        if allow_root and directory in (".", ""):
            return ""
        if directory in (".", "") or directory == ".." or directory.startswith(("../", "/")):
            raise ValueError(f"dependency path leaves the repository or names its root: {path!r}")
        return directory

    def workspace_for(path, manifest):
        directory = posixpath.dirname(path)
        explicit = manifest.get("package", {}).get("workspace")
        if explicit is not None:
            directory = directory_at(directory, explicit, allow_root=True)
        while True:
            workspace_path = posixpath.join(directory, "Cargo.toml")
            if read(workspace_path) is not None:
                workspace = manifest_at(workspace_path).get("workspace")
                if isinstance(workspace, dict):
                    dependencies = workspace.get("dependencies", {})
                    if not isinstance(dependencies, dict):
                        raise ValueError("invalid workspace.dependencies")
                    return directory, dependencies
            if explicit is not None or not directory:
                raise ValueError(f"cannot resolve workspace for {path}")
            directory = posixpath.dirname(directory)

    seen = set()
    queue = [MANIFEST]
    while queue:
        path = queue.pop()
        if path in seen:
            continue
        seen.add(path)
        manifest = manifest_at(path)
        for table in dependency_tables(manifest, strict=True):
            for name, spec in table.items():
                parent = posixpath.dirname(path)
                if isinstance(spec, dict) and spec.get("workspace") is True:
                    parent, workspace = workspace_for(path, manifest)
                    if name not in workspace:
                        raise ValueError(f"cannot resolve workspace dependency {name} in {path}")
                    spec = workspace[name]
                if not isinstance(spec, (str, dict)):
                    raise ValueError(f"invalid dependency {name} in {path}")
                if isinstance(spec, dict) and "path" in spec:
                    directory = directory_at(parent, spec["path"])
                    inputs.append(directory)
                    queue.append(directory + "/Cargo.toml")
    return sorted(set(inputs))


def require_selector(repo, base, head):
    """Refuse a missing selector when the diff touches it or the conservative manifest closure.

    Static inputs are checked first so delete/rename refusals never depend on parsing manifests. When a
    closure cannot be established, refuse naming the selector rather than silently omitting the lane.
    """
    read = git_reader(repo, head)
    if read(SCRIPT) is not None:
        return
    changed = changed_paths(repo, base, head, [])
    if not changed:
        return
    touched = [path for path in changed if under(path, [SCRIPT, *static_inputs()])]
    if not touched:
        try:
            inputs = missing_selector_inputs(read)
        except (ValueError, TypeError, AttributeError) as error:
            raise SelectorMissing(f"{SCRIPT} is missing from the candidate; cannot establish desktop "
                                  f"dependency closure: {error}") from error
        touched = [path for path in changed if under(path, inputs)]
    if touched:
        raise SelectorMissing(f"{SCRIPT} is missing from the candidate but the diff touches {touched[0]} "
                              f"({len(touched)} path(s)); restore the script or land the change without it")


def lane_selected(repo, base, head):
    """Whether the diff base..head touches a lane input (the manifest is read at `head`).

    Raises `SelectorMissing` (see `require_selector`) rather than answering False without the script."""
    require_selector(repo, base, head)
    inputs = trigger_inputs(git_reader(repo, head))
    return selected(changed_paths(repo, base, head, inputs), inputs)


def lane_command(python, script=SCRIPT, repo="."):
    """The one argv the lane runs as."""
    return [str(python), str(script), "--run", "--repo", str(repo)]


def lane_commands(paths, python="python3", inputs=None):
    """The commands a changed-path list selects: the lane, or nothing."""
    return [lane_command(python)] if selected(paths, inputs) else []


def unavailable(runner, reason):
    return {"status": RUNNER_UNAVAILABLE, "lane": LANE, "runner": runner, "reason": reason,
            "executed_tests": 0}


def runner_status(platform=sys.platform, which=shutil.which, run=subprocess.run):
    """None when every runner the lane needs exists, else the typed `runner_unavailable` entry."""
    for tool in ("node", "npm", "cargo"):
        if not which(tool):
            return unavailable(tool, f"{tool} is not installed (the desktop lane needs node, npm and cargo)")
    if not platform.startswith("linux"):
        # macOS and Windows build against the platform webview, which needs no probe.
        return None
    if not which("pkg-config"):
        return unavailable("pkg-config", "pkg-config is not installed (needed to find webkit2gtk-4.1)")
    try:
        found = run(["pkg-config", "--exists", "webkit2gtk-4.1"], capture_output=True, timeout=30)
    except (OSError, subprocess.SubprocessError) as error:
        return unavailable("webkit2gtk-4.1", f"pkg-config cannot run: {error}")
    if found.returncode:
        return unavailable("webkit2gtk-4.1", "pkg-config --exists webkit2gtk-4.1 failed "
                           "(the Tauri cargo tests need the webkit2gtk-4.1 development files)")
    return None


def candidate_files(repo):
    """Repository-relative paths of the candidate tree at `repo`: tracked plus untracked-but-not-ignored files.

    A git work tree lists itself (so `.gitignore`d build output and machine-local state never come along); a
    directory that is not one is walked."""
    try:
        listed = subprocess.run(["git", "ls-files", "-z", "--cached", "--others", "--exclude-standard"], cwd=repo,
                                check=True, capture_output=True)
        return sorted({name.decode("utf-8", "surrogateescape") for name in listed.stdout.split(b"\0") if name})
    except (OSError, subprocess.CalledProcessError):
        found = []
        for directory, names, files in os.walk(repo):
            names[:] = [name for name in names if name not in STAGED_SKIPPED]
            linked = [name for name in names if (Path(directory) / name).is_symlink()]
            found.extend((Path(directory) / name).relative_to(repo).as_posix() for name in [*files, *linked])
        return sorted(found)


def left_out(relative):
    """Whether a candidate path is rebuilt or machine-local: build output, `.git`, `node_modules`, the desktop's skips."""
    parts = relative.split("/")
    return (any(part in STAGED_SKIPPED for part in parts)
            or any(relative.startswith(PREFIX + name + "/") for name in SKIPPED))


def stage_tree(repo, stage):
    """Copy the whole candidate tree (minus rebuilt or machine-local paths) into `stage`.

    The lane runs cargo against a full checkout, never a selection of it (#3129): a compile-time
    `include_str!`/`include_bytes!` or `build.rs` read, or a path dependency inheriting the root manifest's
    `workspace.package`, resolves exactly as in the repository, whatever the app starts reading next."""
    for relative in candidate_files(repo):
        origin = Path(repo) / relative
        if left_out(relative) or not (origin.is_file() or origin.is_symlink()):
            continue
        target = Path(stage) / relative
        target.parent.mkdir(parents=True, exist_ok=True)
        if origin.is_symlink():
            target.symlink_to(os.readlink(origin))
        else:
            shutil.copy2(origin, target)


def lane_env(base_env, scratch):
    """The child environment: private npm cache, own Cargo target, the caller's RUSTC_WRAPPER."""
    env = dict(base_env)
    # Per-run unless the caller names a cache it trusts: a fixed name in a shared temp directory could be
    # pre-seeded by another user or candidate.
    cache = env.get("DESKTOP_LANE_NPM_CACHE") or str(Path(scratch) / "npm-cache")
    target = env.get("DESKTOP_LANE_CARGO_TARGET")
    if not target and env.get("CARGO_TARGET_DIR"):
        target = str(Path(env["CARGO_TARGET_DIR"]) / "desktop-tauri")
    env.update(npm_config_cache=cache, npm_config_audit="false", npm_config_fund="false",
               npm_config_update_notifier="false", CARGO_TARGET_DIR=target or str(Path(scratch) / "cargo-target"))
    env.setdefault("CARGO_INCREMENTAL", "0")
    env.pop("RSI_PROCESS_OWNERSHIP_NAMESPACE", None)
    return env


def count(lines, pattern):
    """Sum of the first group of `pattern` over `lines`."""
    return sum(int(found.group(1)) for found in (pattern.match(line.strip("\r\n")) for line in lines) if found)


class StepTimedOut(Exception):
    def __init__(self, name, seconds):
        self.name = name
        self.seconds = seconds
        super().__init__(f"{TIMED_OUT}: {name} exceeded {seconds:g} seconds")


def stop_step(process):
    """Terminate the owned group even if its leader already exited, then reap the leader."""
    if os.name == "posix":
        try:
            os.killpg(process.pid, signal.SIGTERM)
        except (ProcessLookupError, PermissionError):
            # macOS answers EPERM (not ESRCH) for a group whose members have all exited but are not
            # yet reaped (#138); there is nothing left to signal either way.
            pass
        # The leader exiting says nothing about grandchildren ignoring SIGTERM.
        time.sleep(TERMINATE_GRACE_SECONDS)
        try:
            os.killpg(process.pid, signal.SIGKILL)
        except (ProcessLookupError, PermissionError):
            pass
    else:
        # Windows has no killpg. taskkill reaches node/browser descendants as well as the leader.
        try:
            subprocess.run(["taskkill", "/PID", str(process.pid), "/T", "/F"],
                           stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
                           timeout=TERMINATE_GRACE_SECONDS, check=False)
        except (OSError, subprocess.TimeoutExpired):
            pass
        if process.poll() is None:
            process.kill()
    try:
        process.wait(timeout=TERMINATE_GRACE_SECONDS)
    except subprocess.TimeoutExpired:
        pass


def run_step(command, cwd, env, name):
    """Run one bounded step, teeing output; (exit code, lines), or StepTimedOut after cleanup."""
    variable, default = STEP_TIMEOUTS[name]
    seconds = float(env.get(variable, default))
    if not math.isfinite(seconds) or seconds <= 0:
        raise ValueError(f"{variable} must be positive finite seconds")
    print(f"desktop lane: {name}: {' '.join(map(str, command))}", flush=True)
    options = {"start_new_session": True} if os.name == "posix" else {
        "creationflags": getattr(subprocess, "CREATE_NEW_PROCESS_GROUP", 0)}
    # Chrome normally owns a separate group. Inside a bounded Unix step it must inherit ours.
    child_env = {**env, "DESKTOP_LANE_PROCESS_GROUP": "1"} if os.name == "posix" else env
    deadline = time.monotonic() + seconds
    process = subprocess.Popen(command, cwd=cwd, env=child_env, stdout=subprocess.PIPE, stderr=subprocess.STDOUT,
                               text=True, errors="replace", **options)
    lines = []
    output = queue.Queue()

    def read_output():
        try:
            for line in process.stdout:
                output.put(line)
        finally:
            output.put(None)

    # Reading a silent child (or an inherited stdout after the leader exits) must not block the clock.
    reader = threading.Thread(target=read_output, daemon=True)
    reader.start()
    try:
        while True:
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                raise queue.Empty
            line = output.get(timeout=remaining)
            if line is None:
                break
            print(line, end="", flush=True)
            lines.append(line)
        return process.wait(timeout=max(0, deadline - time.monotonic())), lines
    except (queue.Empty, subprocess.TimeoutExpired):
        stop_step(process)
        raise StepTimedOut(name, seconds) from None
    except BaseException:
        stop_step(process)
        raise
    finally:
        reader.join(timeout=TERMINATE_GRACE_SECONDS)
        if not reader.is_alive():
            process.stdout.close()


def run_lane(repo, base_env=None, step=run_step, status=runner_status):
    """Run both test sets; (exit code, result dict)."""
    missing = status()
    if missing:
        return 1, missing
    scratch = tempfile.mkdtemp(prefix="rsi-desktop-lane-")
    result = {"status": "failed", "lane": LANE, "runner": "npm+cargo", "npm_tests": 0, "cargo_tests": 0,
              "executed_tests": 0}
    try:
        env = lane_env(os.environ if base_env is None else base_env, scratch)
        stage_tree(repo, scratch)
        print(f"desktop lane: npm cache {env['npm_config_cache']}; cargo target {env['CARGO_TARGET_DIR']}", flush=True)
        npm = shutil.which("npm") or "npm"
        frontend = Path(scratch) / "desktop"
        code, _ = step([npm, "ci"], frontend, env, "npm ci")
        if code:
            return 1, {**result, "failed_step": "npm ci", "exit_code": code}
        code, lines = step([npm, "test"], frontend, env, "npm test")
        result["npm_tests"] = count(lines, NODE_TESTS)
        if code:
            return 1, {**result, "failed_step": "npm test", "exit_code": code}
        code, lines = step(["cargo", "test", "--locked"], frontend / "src-tauri", env, "cargo test")
        result["cargo_tests"] = count(lines, CARGO_PASSED)
        if code:
            # Cargo has used both phrasings. Require its locked-update diagnostic, rather than
            # mislabelling compiler/test errors, an unavailable registry or a cache miss as stale.
            output = "".join(lines)
            if re.search(r"(?:cannot update the lock file|lock file .* needs to be updated).*"
                         r"(?:because|but) --locked was passed", output):
                print(f"desktop lane: {LOCK_STALE}: {LOCK_FIX}", flush=True)
                return 1, {**result, "status": LOCK_STALE, "failed_step": "cargo test",
                           "exit_code": code, "reason": LOCK_FIX,
                           "executed_tests": result["npm_tests"] + result["cargo_tests"]}
            return 1, {**result, "failed_step": "cargo test", "exit_code": code}
    except StepTimedOut as error:
        print(str(error), flush=True)
        return 1, {**result, "status": TIMED_OUT, "failed_step": error.name,
                   "timeout_seconds": error.seconds,
                   "executed_tests": result["npm_tests"] + result["cargo_tests"]}
    finally:
        shutil.rmtree(scratch, ignore_errors=True)
    result["executed_tests"] = result["npm_tests"] + result["cargo_tests"]
    if not result["npm_tests"] or not result["cargo_tests"]:
        # A set that ran nothing is no evidence, whatever its exit code was.
        return 1, {**result, "status": "no_tests_executed"}
    return 0, {**result, "status": "passed"}


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--repo", type=Path, default=Path("."))
    parser.add_argument("--base")
    parser.add_argument("--head", default="HEAD")
    parser.add_argument("--run", action="store_true", help="run the lane instead of selecting it")
    args = parser.parse_args()
    repo = args.repo.resolve()
    if args.run:
        # Steps have their own sessions: an outer job cancellation must also run their cleanup.
        def terminate(signum, _frame):
            raise SystemExit(128 + signum)
        previous = signal.signal(signal.SIGTERM, terminate)
        try:
            code, result = run_lane(repo)
        finally:
            signal.signal(signal.SIGTERM, previous)
        print(RESULT_MARK + json.dumps(result, sort_keys=True), flush=True)
        return code
    if not args.base:
        parser.error("--base is required unless --run is given")
    try:
        selected_lane = lane_selected(repo, args.base, args.head)
    except SelectorMissing as error:
        print(str(error), file=sys.stderr)
        return 1
    print(json.dumps([lane_command("python3")] if selected_lane else []))
    return 0


if __name__ == "__main__":
    sys.exit(main())
