#!/usr/bin/env bash
# Reproducible, custody-bound warm test-suite measurements.

# Authority is never installed into a shell that sourced this file.  This guard
# deliberately precedes set, readonly state, functions, traps, and dispatch.
if [[ ${BASH_SOURCE[0]} != "$0" ]]; then
    return 2 2>/dev/null || exit 2
fi
set -euo pipefail

readonly SCRIPT_NAME="${0##*/}"
readonly OUTPUT_ROOT="target/test-suite-benchmark"
readonly SCHEMA_VERSION=2
readonly RETAINED_CALIBRATION_CASES=141
readonly CUSTODY_THREAT_STATEMENT="Acceptance proves exact bytes and uninterrupted in-process descriptor custody against hostile inherited environment and concurrent same-UID pathname or configuration mutation after a conforming sterile launch. It does not authenticate the launching principal, resist same-UID ptrace, process-memory, signal/control, namespace-control, or inherited-open-fd compromise, and does not authenticate alteration or forgery after the producer's final reproof and exit. It is not a signature or cryptographic origin attestation."
REPO_ROOT="" OUTPUT_ROOT_ABS="" CAPTURE_OUT_ABS=""
CAPTURE_GIT_HEAD="" CAPTURE_GIT_BRANCH=""
EXTERNAL_MODE=0 EXTERNAL_ROOT="" EXTERNAL_PARENT="" EXTERNAL_OUT_LEAF=""
EXTERNAL_ROOT_FD="" EXTERNAL_PARENT_FD="" EXTERNAL_ROOT_DEV="" EXTERNAL_ROOT_INO=""
EXTERNAL_PARENT_DEV="" EXTERNAL_PARENT_INO="" EXTERNAL_WORKSPACE_LEAF="" EXTERNAL_PRIVATE_LEAF=""
EXTERNAL_WORKSPACE_DEV="" EXTERNAL_WORKSPACE_INO="" EXTERNAL_PRIVATE_DEV="" EXTERNAL_PRIVATE_INO=""
EXTERNAL_LOGS_LEAF="" EXTERNAL_SAMPLES_LEAF="" EXTERNAL_STAGING_ACTIVE=0
CAPTURE_STOP_ON_FIRST_RED=0

die() { echo "${SCRIPT_NAME}: $*" >&2; exit 2; }

usage() {
    cat <<'EOF'
Usage:
  /usr/bin/env -i HOME=/home/jakedevar CARGO_HOME=/home/jakedevar/.cargo RUSTUP_HOME=/home/jakedevar/.rustup PATH=/home/jakedevar/.rustup/toolchains/1.94.1-x86_64-unknown-linux-gnu/bin:/home/jakedevar/.cargo/bin:/usr/bin:/bin LANG=C LC_ALL=C TZ=UTC TMPDIR=<repo>/target/test-suite-benchmark/tmp CARGO_TARGET_DIR=<repo>/target XDG_CONFIG_HOME=<repo>/target/test-suite-benchmark/xdg-empty NEXTEST_CONFIG_FILE=<repo>/.config/nextest.toml /usr/bin/bash --noprofile --norc -p <repo>/scripts/test-suite-benchmark.sh calibrate-baseline
  scripts/test-suite-benchmark.sh capture --label LABEL --probe PROBE --repeat N --threads N|auto --out target/test-suite-benchmark/FILE.json
  scripts/test-suite-benchmark.sh capture --label LABEL --probe PROBE --repeat N --threads N|auto --stop-on-first-red --out target/test-suite-benchmark/FILE.json
  scripts/test-suite-benchmark.sh capture --label LABEL --probe PROBE --repeat N --threads N|auto --external-output-root ABSOLUTE_ROOT --out ABSOLUTE_ROOT/FILE.json
  scripts/test-suite-benchmark.sh attest-preflight --calibration-root target/test-suite-benchmark/z-baseline-calibration/SOURCE_HEAD --threads N
  scripts/test-suite-benchmark.sh generate-baseline --calibration-root target/test-suite-benchmark/z-baseline-calibration/SOURCE_HEAD --out target/test-suite-benchmark/z-baseline-calibration/SOURCE_HEAD/test-suite-baseline.candidate.json
  scripts/test-suite-benchmark.sh compare --baseline target/test-suite-benchmark/BASELINE.json --candidate target/test-suite-benchmark/CANDIDATE.json
  scripts/test-suite-benchmark.sh check --baseline metrics/test-suite-baseline.json --measurement target/test-suite-benchmark/MEASUREMENT.json

Probes:
  store-fixture      one representative fresh Store fixture test
  v87-matrices       exactly three V87 session-fence predicate matrices
  source-scanner     the protected-DML source scanner test
  nextest-fast       rsid library tests through the rsid-fast Nextest profile
  nextest-full       all workspace tests through cargo-nextest
  rsid-serial        all rsid library tests with one libtest thread
  workspace-doctests all workspace doctests

capture refuses a dirty Git tree. Each capture retains a unique private
workspace. By default it is below the repository's target/test-suite-benchmark/
directory. The capture-only external option requires Linux and an existing,
canonical, current-user-owned mode-0700 root outside every Git worktree.

calibrate-baseline is the sole accepted baseline producer. It owns one live,
resident descriptor-custody lifetime spanning preflight, enumeration, the fast
and full lanes, store/scanner captures, the 1/8/16/32 thread sweep, selection,
generation, held-out checks, exclusive publication, and identity-only cleanup.
It accepts no arguments and will run real tests; do not invoke it accidentally.

attest-preflight is diagnostic-only and produces accepted=false evidence. It runs
the fixed fact, enumeration, fast-lane, and full-lane commands once, retains
their complete raw stdout/stderr, snapshots the authenticated repository
Nextest TOML, derives normalized component evidence, and publishes the private
source-keyed root atomically. It stops after the first nonzero command.

generate-baseline is diagnostic-only and cannot publish
metrics/test-suite-baseline.json. It performs no measurement and reads only preflight.json,
nextest-fast-list.json, nextest-full-list.json, store.json, scanner.json,
fast.json, and sweep-t{1,8,16,32}.json below the source-keyed calibration root.
It validates every capture and referenced log, then publishes without overwrite.
The generator's conservative workspace-size bounds are: 4 MiB preflight JSON;
64 MiB per enumeration, capture, or raw log; 1 GiB aggregate evidence;
128 MiB generated output; JSON depth 64; 512 suites; 100000 tests or
identities; 5 samples per capture; and 1 MiB per JSON string. Evidence is read
through no-follow descriptors before whole-file parsing and raw text is scanned
in bounded chunks.

Acceptance proves exact bytes and uninterrupted in-process descriptor custody against hostile inherited environment and concurrent same-UID pathname or configuration mutation after a conforming sterile launch. It does not authenticate the launching principal, resist same-UID ptrace, process-memory, signal/control, namespace-control, or inherited-open-fd compromise, and does not authenticate alteration or forgery after the producer's final reproof and exit. It is not a signature or cryptographic origin attestation.
The pathname/configuration claim protects the target root and its ancestry, but
deliberately excludes target contents that Cargo and Nextest are expected to
create or mutate while lanes run; those bytes are not accepted input evidence.
EOF
}

need_command() { command -v "$1" >/dev/null 2>&1 || die "required command not found: $1"; }
require_jq() { need_command jq; }
positive_integer() { [[ "$1" =~ ^[1-9][0-9]*$ ]]; }

require_clean_tree() {
    git rev-parse --is-inside-work-tree >/dev/null 2>&1 || die "must run inside a Git worktree"
    [ -z "$(git status --porcelain)" ] || die "refusing capture from a dirty Git tree; commit, stash, or remove local changes first"
}

capture_source_state() {
    CAPTURE_GIT_HEAD="$(git rev-parse HEAD)"
    CAPTURE_GIT_BRANCH="$(git branch --show-current)"
    [ -n "$CAPTURE_GIT_HEAD" ] || die "cannot determine capture source HEAD"
}

require_unchanged_capture_source() {
    [ "$(git rev-parse HEAD)" = "$CAPTURE_GIT_HEAD" ] || die "refusing publication: source HEAD changed during capture"
    [ "$(git branch --show-current)" = "$CAPTURE_GIT_BRANCH" ] || die "refusing publication: source branch changed during capture"
    [ -z "$(git status --porcelain)" ] || die "refusing publication: source worktree changed during capture"
}

publish_capture_json() {
    local out="$1" tmp_out="$2"
    if [ "$EXTERNAL_MODE" -eq 1 ]; then
        external_publish_capture_json "$out" "$tmp_out"
        return
    fi
    # Metadata and JSON construction may invoke external tools. Fence both sides
    # of publication so a source change cannot leave successful evidence behind.
    require_unchanged_capture_source
    ln "$tmp_out" "$out" || die "capture output appeared concurrently: $out"
    if ! [ "$(git rev-parse HEAD)" = "$CAPTURE_GIT_HEAD" ] || ! [ "$(git branch --show-current)" = "$CAPTURE_GIT_BRANCH" ] || ! [ -z "$(git status --porcelain)" ]; then
        rm -f "$out" "$tmp_out"
        die "refusing publication: source changed at publication boundary"
    fi
    rm "$tmp_out"
}

initialize_repo_root() {
    REPO_ROOT="$(git rev-parse --show-toplevel)" || die "must run inside a Git worktree"
    REPO_ROOT="$(cd "$REPO_ROOT" && pwd -P)"
    OUTPUT_ROOT_ABS="$REPO_ROOT/$OUTPUT_ROOT"
}

validate_output_path() {
    local path="$1"
    case "$path" in "$OUTPUT_ROOT"/*) ;; *) die "output path must be below $OUTPUT_ROOT/: $path" ;; esac
    case "/$path/" in */../*|*/./*) die "output path must not contain . or .. components: $path" ;; esac
}

# Create only missing components, rejecting every existing or introduced symlink.
ensure_relative_directory() {
    local relative="$1" current="$REPO_ROOT" component
    local -a components
    IFS=/ read -r -a components <<<"$relative"
    for component in "${components[@]}"; do
        [ -n "$component" ] || die "invalid empty output component"
        current="$current/$component"
        [ ! -L "$current" ] || die "output path contains a symlink: $current"
        [ -e "$current" ] || mkdir "$current" || die "cannot create output directory: $current"
        [ -d "$current" ] && [ ! -L "$current" ] || die "output path component is not a directory: $current"
    done
}

prepare_output_destination() {
    local out="$1" parent_relative root_real parent_real
    validate_output_path "$out"; initialize_repo_root
    ensure_relative_directory "$OUTPUT_ROOT"
    parent_relative="${out%/*}"; ensure_relative_directory "$parent_relative"
    CAPTURE_OUT_ABS="$REPO_ROOT/$out"
    [ ! -e "$CAPTURE_OUT_ABS" ] && [ ! -L "$CAPTURE_OUT_ABS" ] || die "refusing to overwrite capture output: $out"
    root_real="$(realpath -e "$OUTPUT_ROOT_ABS")"
    parent_real="$(realpath -e "$(dirname "$CAPTURE_OUT_ABS")")"
    case "$parent_real/" in "$root_real/"*) ;; *) die "output path escapes $OUTPUT_ROOT/" ;; esac
}

validate_absolute_single_path() {
    local name="$1" path="$2" component remainder
    [ -n "$path" ] || die "$name requires a nonempty absolute path"
    case "$path" in /*) ;; *) die "$name must be absolute: $path" ;; esac
    [ "$path" = / ] || [ "${path%/}" = "$path" ] || die "$name must not have a trailing slash: $path"
    case "$path" in *//* ) die "$name must not contain empty components: $path" ;; esac
    case "$path" in *$'\t'*|*$'\r'*|*$'\n'*) die "$name must not contain tab, CR, or LF: $path" ;; esac
    remainder="${path#/}"
    while [ -n "$remainder" ]; do
        component="${remainder%%/*}"
        [ "$component" != . ] && [ "$component" != .. ] || die "$name must not contain . or .. components: $path"
        if [ "$remainder" = "$component" ]; then remainder=""; else remainder="${remainder#*/}"; fi
    done
}

external_test_fault() {
    [ "${S1Q_SELF_TEST_MODE:-}" = 1 ] && [ "${S1Q_TEST_FAULT:-}" = "$1" ]
}

external_capability_preflight() {
    external_test_fault platform && die "external output requires Linux"
    [ "$(uname -s)" = Linux ] || die "external output requires Linux"
    external_test_fault python3 && die "required command not found: python3"
    need_command python3
    external_test_fault flock && die "required command not found: flock"
    need_command flock
    S1Q_CAPABILITY_FAULT="${S1Q_TEST_FAULT:-}" python3 - <<'PY' || die "external output platform capabilities are unavailable"
import inspect
import os
import pwd
import shutil
import stat

fault = os.environ.get("S1Q_CAPABILITY_FAULT") if os.environ.get("S1Q_SELF_TEST_MODE") == "1" else ""

def require(condition, name):
    if fault == name or not condition:
        raise RuntimeError(name)

require(hasattr(os, "O_DIRECTORY"), "O_DIRECTORY")
require(hasattr(os, "O_NOFOLLOW"), "O_NOFOLLOW")
require(hasattr(os, "O_CLOEXEC"), "O_CLOEXEC")
require(os.link in os.supports_dir_fd, "link-dir-fd")
require(os.stat in os.supports_dir_fd, "stat-dir-fd")
require(os.unlink in os.supports_dir_fd, "unlink-dir-fd")
require(os.link in os.supports_follow_symlinks, "link-follow-symlinks")
require(os.stat in os.supports_follow_symlinks, "stat-follow-symlinks")
require(bool(getattr(shutil.rmtree, "avoids_symlink_attacks", False)), "rmtree-symlink-safe")
require("dir_fd" in inspect.signature(shutil.rmtree).parameters, "rmtree-dir-fd")
probe_fd = os.open(".", os.O_RDONLY | os.O_DIRECTORY | os.O_CLOEXEC)
try:
    require(fault != "procfs", "procfs")
    duplicate = os.open(f"/proc/{os.getpid()}/fd/{probe_fd}", os.O_RDONLY | os.O_DIRECTORY | os.O_CLOEXEC)
    try:
        original = os.fstat(probe_fd)
        copied = os.fstat(duplicate)
        require(stat.S_ISDIR(copied.st_mode) and (original.st_dev, original.st_ino) == (copied.st_dev, copied.st_ino), "procfs")
    finally:
        os.close(duplicate)
finally:
    os.close(probe_fd)
PY
}

external_fs_helper() {
    local action="$1"
    shift
    python3 - "$action" "$BASHPID" "$@" <<'PY'
import os
import shutil
import stat
import subprocess
import sys
import tempfile
import tomllib

action = sys.argv[1]
shell_pid = int(sys.argv[2])
args = sys.argv[3:]
uid = os.geteuid()
gid = os.getegid()

def fail(message):
    raise RuntimeError(message)

def mode_bits(st):
    return stat.S_IMODE(st.st_mode)

def duplicate_directory(fd_text):
    fd = os.open(
        f"/proc/{shell_pid}/fd/{int(fd_text)}",
        os.O_RDONLY | os.O_DIRECTORY | os.O_CLOEXEC,
    )
    if not stat.S_ISDIR(os.fstat(fd).st_mode):
        os.close(fd)
        fail("saved descriptor is not a directory")
    return fd

def require_owned_directory(st, label):
    if not stat.S_ISDIR(st.st_mode) or st.st_uid != uid or st.st_gid != gid or mode_bits(st) != 0o700:
        fail(f"{label} is not a current-user mode-0700 directory")

def require_owned_file(st, label, links):
    if not stat.S_ISREG(st.st_mode) or st.st_uid != uid or st.st_gid != gid or mode_bits(st) != 0o600 or st.st_nlink != links:
        fail(f"{label} is not a current-user mode-0600 regular file with link count {links}")

def authenticate_path(fd, path, expected, label):
    fd_st = os.fstat(fd)
    path_st = os.stat(path, follow_symlinks=False)
    require_owned_directory(fd_st, f"saved {label}")
    require_owned_directory(path_st, label)
    identity = (fd_st.st_dev, fd_st.st_ino)
    if identity != (path_st.st_dev, path_st.st_ino) or (expected and identity != expected):
        fail(f"{label} descriptor identity changed")
    if os.path.realpath(path) != path:
        fail(f"{label} is not canonically spelled")
    return identity

def stat_leaf(fd, leaf):
    return os.stat(leaf, dir_fd=fd, follow_symlinks=False)

def require_absent(fd, leaf, label):
    try:
        stat_leaf(fd, leaf)
    except FileNotFoundError:
        return
    fail(f"{label} already exists")

def identity_matches(st, identity):
    return (st.st_dev, st.st_ino) == identity

def open_child_directory(parent_fd, leaf):
    return os.open(leaf, os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW | os.O_CLOEXEC, dir_fd=parent_fd)

def validate_workspace(parent_fd, workspace_leaf, workspace_identity, logs_leaf, samples_leaf):
    workspace_st = stat_leaf(parent_fd, workspace_leaf)
    require_owned_directory(workspace_st, "workspace")
    if not identity_matches(workspace_st, workspace_identity):
        fail("workspace identity changed")
    workspace_fd = open_child_directory(parent_fd, workspace_leaf)
    try:
        def walk(directory_fd):
            for leaf in os.listdir(directory_fd):
                child = stat_leaf(directory_fd, leaf)
                if stat.S_ISDIR(child.st_mode):
                    require_owned_directory(child, "workspace descendant")
                    child_fd = open_child_directory(directory_fd, leaf)
                    try:
                        walk(child_fd)
                    finally:
                        os.close(child_fd)
                else:
                    require_owned_file(child, "workspace descendant", 1)
        for directory_leaf in (logs_leaf, samples_leaf):
            directory_st = stat_leaf(workspace_fd, directory_leaf)
            require_owned_directory(directory_st, directory_leaf)
        walk(workspace_fd)
    finally:
        os.close(workspace_fd)

def source_state(repo_root, head, branch):
    def git(*command):
        return subprocess.run(
            ["git", "-C", repo_root, *command],
            check=True,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
        ).stdout.rstrip("\n")
    if git("rev-parse", "HEAD") != head or git("branch", "--show-current") != branch or git("status", "--porcelain"):
        fail("source state changed during capture")

def test_hook(stage):
    if os.environ.get("S1Q_SELF_TEST_MODE") != "1" or os.environ.get("S1Q_TEST_HOOK_STAGE") != stage:
        return
    stage_fifo = os.environ.get("S1Q_TEST_STAGE_FIFO", "")
    continue_fifo = os.environ.get("S1Q_TEST_CONTINUE_FIFO", "")
    if not stage_fifo or not continue_fifo:
        fail("invalid self-test hook")
    with open(stage_fifo, "w", encoding="utf-8") as stream:
        stream.write(stage + "\n")
    with open(continue_fifo, "r", encoding="utf-8") as stream:
        if stream.readline().rstrip("\n") != stage:
            fail("invalid self-test continuation")

def cleanup(parent_fd, out_leaf, private_leaf, private_identity, workspace_leaf, workspace_identity):
    for leaf in (out_leaf, private_leaf):
        try:
            leaf_st = stat_leaf(parent_fd, leaf)
        except FileNotFoundError:
            continue
        if identity_matches(leaf_st, private_identity):
            os.unlink(leaf, dir_fd=parent_fd)
    try:
        workspace_st = stat_leaf(parent_fd, workspace_leaf)
    except FileNotFoundError:
        workspace_st = None
    if workspace_st is not None and identity_matches(workspace_st, workspace_identity):
        shutil.rmtree(workspace_leaf, dir_fd=parent_fd)
    for leaf, identity in ((out_leaf, private_identity), (private_leaf, private_identity), (workspace_leaf, workspace_identity)):
        try:
            remaining = stat_leaf(parent_fd, leaf)
        except FileNotFoundError:
            continue
        if identity_matches(remaining, identity):
            fail(f"runner identity remains at {leaf}")

try:
    if action == "prepare-parent":
        root_fd_text, root_path, parent_path, suffix = args
        root_fd = duplicate_directory(root_fd_text)
        try:
            authenticate_path(root_fd, root_path, None, "external root")
            current_fd = os.dup(root_fd)
            try:
                if suffix:
                    for component in suffix.split("/"):
                        try:
                            child_st = stat_leaf(current_fd, component)
                        except FileNotFoundError:
                            os.mkdir(component, 0o700, dir_fd=current_fd)
                            child_st = stat_leaf(current_fd, component)
                        require_owned_directory(child_st, "output parent component")
                        next_fd = open_child_directory(current_fd, component)
                        os.close(current_fd)
                        current_fd = next_fd
                if os.path.realpath(parent_path) != parent_path:
                    fail("output parent is not canonically spelled")
            finally:
                os.close(current_fd)
        finally:
            os.close(root_fd)
    elif action == "authenticate":
        root_fd_text, parent_fd_text, root_path, parent_path, out_leaf = args
        root_fd = duplicate_directory(root_fd_text)
        parent_fd = duplicate_directory(parent_fd_text)
        try:
            root_identity = authenticate_path(root_fd, root_path, None, "external root")
            parent_identity = authenticate_path(parent_fd, parent_path, None, "output parent")
            if parent_path != root_path and not parent_path.startswith(root_path + "/"):
                fail("output parent escapes external root")
            require_absent(parent_fd, out_leaf, "capture output")
            print(*root_identity, *parent_identity, sep="\t")
        finally:
            os.close(parent_fd)
            os.close(root_fd)
    elif action == "revalidate":
        root_fd_text, parent_fd_text, root_path, parent_path, root_dev, root_ino, parent_dev, parent_ino, out_leaf = args
        root_fd = duplicate_directory(root_fd_text)
        parent_fd = duplicate_directory(parent_fd_text)
        try:
            authenticate_path(root_fd, root_path, (int(root_dev), int(root_ino)), "external root")
            authenticate_path(parent_fd, parent_path, (int(parent_dev), int(parent_ino)), "output parent")
            require_absent(parent_fd, out_leaf, "capture output")
        finally:
            os.close(parent_fd)
            os.close(root_fd)
    elif action == "setup":
        parent_fd_text, workspace_leaf, private_leaf, logs_leaf, samples_leaf = args
        parent_fd = duplicate_directory(parent_fd_text)
        made_workspace = False
        made_private = False
        made_workspace_identity = None
        made_private_identity = None
        try:
            try:
                os.mkdir(workspace_leaf, 0o700, dir_fd=parent_fd)
                made_workspace = True
                created_workspace_st = stat_leaf(parent_fd, workspace_leaf)
                made_workspace_identity = (created_workspace_st.st_dev, created_workspace_st.st_ino)
                workspace_fd = open_child_directory(parent_fd, workspace_leaf)
                try:
                    os.mkdir(logs_leaf, 0o700, dir_fd=workspace_fd)
                    os.mkdir(samples_leaf, 0o700, dir_fd=workspace_fd)
                finally:
                    os.close(workspace_fd)
                private_fd = os.open(
                    private_leaf,
                    os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW | os.O_CLOEXEC,
                    0o600,
                    dir_fd=parent_fd,
                )
                made_private = True
                os.close(private_fd)
                created_private_st = stat_leaf(parent_fd, private_leaf)
                made_private_identity = (created_private_st.st_dev, created_private_st.st_ino)
                workspace_st = stat_leaf(parent_fd, workspace_leaf)
                private_st = stat_leaf(parent_fd, private_leaf)
                require_owned_directory(workspace_st, "workspace")
                require_owned_file(private_st, "private JSON", 1)
                print(workspace_st.st_dev, workspace_st.st_ino, private_st.st_dev, private_st.st_ino, sep="\t")
            except Exception:
                if made_private:
                    try:
                        current_private_st = stat_leaf(parent_fd, private_leaf)
                        if made_private_identity is not None and identity_matches(current_private_st, made_private_identity):
                            os.unlink(private_leaf, dir_fd=parent_fd)
                    except FileNotFoundError:
                        pass
                if made_workspace:
                    try:
                        current_workspace_st = stat_leaf(parent_fd, workspace_leaf)
                        if made_workspace_identity is not None and identity_matches(current_workspace_st, made_workspace_identity):
                            shutil.rmtree(workspace_leaf, dir_fd=parent_fd)
                    except FileNotFoundError:
                        pass
                raise
        finally:
            os.close(parent_fd)
    elif action == "validate-staging":
        (root_fd_text, parent_fd_text, root_path, parent_path, root_dev, root_ino, parent_dev, parent_ino,
         out_leaf, workspace_leaf, workspace_dev, workspace_ino, private_leaf, private_dev, private_ino,
         logs_leaf, samples_leaf) = args
        root_fd = duplicate_directory(root_fd_text)
        parent_fd = duplicate_directory(parent_fd_text)
        try:
            authenticate_path(root_fd, root_path, (int(root_dev), int(root_ino)), "external root")
            authenticate_path(parent_fd, parent_path, (int(parent_dev), int(parent_ino)), "output parent")
            require_absent(parent_fd, out_leaf, "capture output")
            private_st = stat_leaf(parent_fd, private_leaf)
            require_owned_file(private_st, "private JSON", 1)
            if not identity_matches(private_st, (int(private_dev), int(private_ino))):
                fail("private JSON identity changed")
            validate_workspace(parent_fd, workspace_leaf, (int(workspace_dev), int(workspace_ino)), logs_leaf, samples_leaf)
        finally:
            os.close(parent_fd)
            os.close(root_fd)
    elif action == "cleanup":
        parent_fd_text, parent_dev, parent_ino, out_leaf, private_leaf, private_dev, private_ino, workspace_leaf, workspace_dev, workspace_ino = args
        parent_fd = duplicate_directory(parent_fd_text)
        try:
            parent_st = os.fstat(parent_fd)
            if (parent_st.st_dev, parent_st.st_ino) != (int(parent_dev), int(parent_ino)):
                fail("saved output parent identity changed")
            cleanup(
                parent_fd,
                out_leaf,
                private_leaf,
                (int(private_dev), int(private_ino)),
                workspace_leaf,
                (int(workspace_dev), int(workspace_ino)),
            )
        finally:
            os.close(parent_fd)
    elif action == "publish":
        (root_fd_text, parent_fd_text, root_path, parent_path, root_dev, root_ino, parent_dev, parent_ino,
         out_leaf, private_leaf, private_dev, private_ino, workspace_leaf, workspace_dev, workspace_ino,
         logs_leaf, samples_leaf, repo_root, head, branch) = args
        root_fd = duplicate_directory(root_fd_text)
        parent_fd = duplicate_directory(parent_fd_text)
        root_identity = (int(root_dev), int(root_ino))
        parent_identity = (int(parent_dev), int(parent_ino))
        private_identity = (int(private_dev), int(private_ino))
        workspace_identity = (int(workspace_dev), int(workspace_ino))
        try:
            authenticate_path(root_fd, root_path, root_identity, "external root")
            authenticate_path(parent_fd, parent_path, parent_identity, "output parent")
            source_state(repo_root, head, branch)
            validate_workspace(parent_fd, workspace_leaf, workspace_identity, logs_leaf, samples_leaf)
            private_st = stat_leaf(parent_fd, private_leaf)
            require_owned_file(private_st, "private JSON", 1)
            if not identity_matches(private_st, private_identity):
                fail("private JSON identity changed before publication")
            os.link(private_leaf, out_leaf, src_dir_fd=parent_fd, dst_dir_fd=parent_fd, follow_symlinks=False)
            private_st = stat_leaf(parent_fd, private_leaf)
            final_st = stat_leaf(parent_fd, out_leaf)
            require_owned_file(private_st, "private JSON", 2)
            require_owned_file(final_st, "final JSON", 2)
            if not identity_matches(private_st, private_identity) or not identity_matches(final_st, private_identity):
                fail("publication identity mismatch")
            test_hook("after-link")
            source_state(repo_root, head, branch)
            authenticate_path(root_fd, root_path, root_identity, "external root")
            authenticate_path(parent_fd, parent_path, parent_identity, "output parent")
            private_st = stat_leaf(parent_fd, private_leaf)
            final_st = stat_leaf(parent_fd, out_leaf)
            require_owned_file(private_st, "private JSON", 2)
            require_owned_file(final_st, "final JSON", 2)
            if not identity_matches(private_st, private_identity) or not identity_matches(final_st, private_identity):
                fail("post-link publication identity mismatch")
            os.unlink(private_leaf, dir_fd=parent_fd)
            final_st = stat_leaf(parent_fd, out_leaf)
            require_owned_file(final_st, "final JSON", 1)
            if not identity_matches(final_st, private_identity):
                fail("final JSON identity changed")
            source_state(repo_root, head, branch)
            authenticate_path(root_fd, root_path, root_identity, "external root")
            authenticate_path(parent_fd, parent_path, parent_identity, "output parent")
            validate_workspace(parent_fd, workspace_leaf, workspace_identity, logs_leaf, samples_leaf)
        finally:
            os.close(parent_fd)
            os.close(root_fd)
    else:
        fail("unknown external helper action")
except Exception as error:
    print(f"external output: {error}", file=sys.stderr)
    sys.exit(2)
PY
}

external_test_hook() {
    local stage="$1" response
    [ "${S1Q_SELF_TEST_MODE:-}" = 1 ] || return 0
    [ "${S1Q_TEST_HOOK_STAGE:-}" = "$stage" ] || return 0
    [ -p "${S1Q_TEST_STAGE_FIFO:-}" ] && [ -p "${S1Q_TEST_CONTINUE_FIFO:-}" ] || die "invalid external self-test hook"
    printf '%s\n' "$stage" >"$S1Q_TEST_STAGE_FIFO"
    IFS= read -r response <"$S1Q_TEST_CONTINUE_FIFO"
    [ "$response" = "$stage" ] || die "invalid external self-test continuation"
}

external_close_descriptors() {
    if [ -n "$EXTERNAL_PARENT_FD" ]; then exec {EXTERNAL_PARENT_FD}<&-; EXTERNAL_PARENT_FD=""; fi
    if [ -n "$EXTERNAL_ROOT_FD" ]; then exec {EXTERNAL_ROOT_FD}<&-; EXTERNAL_ROOT_FD=""; fi
}

external_cleanup_staging() {
    [ "$EXTERNAL_STAGING_ACTIVE" -eq 1 ] || return 0
    external_fs_helper cleanup "$EXTERNAL_PARENT_FD" "$EXTERNAL_PARENT_DEV" "$EXTERNAL_PARENT_INO" \
        "$EXTERNAL_OUT_LEAF" "$EXTERNAL_PRIVATE_LEAF" "$EXTERNAL_PRIVATE_DEV" "$EXTERNAL_PRIVATE_INO" \
        "$EXTERNAL_WORKSPACE_LEAF" "$EXTERNAL_WORKSPACE_DEV" "$EXTERNAL_WORKSPACE_INO"
}

external_exit_cleanup() {
    local status=2
    trap - EXIT
    external_cleanup_staging || status=2
    external_close_descriptors
    exit "$status"
}

external_revalidate() {
    external_fs_helper revalidate "$EXTERNAL_ROOT_FD" "$EXTERNAL_PARENT_FD" "$EXTERNAL_ROOT" "$EXTERNAL_PARENT" \
        "$EXTERNAL_ROOT_DEV" "$EXTERNAL_ROOT_INO" "$EXTERNAL_PARENT_DEV" "$EXTERNAL_PARENT_INO" "$EXTERNAL_OUT_LEAF"
}

prepare_external_output_destination() {
    local root="$1" out="$2" worktree_text line worktree worktree_real suffix identities tokens staging
    validate_absolute_single_path "--external-output-root" "$root"
    [ "$root" != / ] || die "--external-output-root must not be /"
    validate_absolute_single_path "--out" "$out"
    case "$out" in "$root"/*) ;; *) die "external --out must be a strict descendant of --external-output-root" ;; esac
    external_capability_preflight
    initialize_repo_root
    [ -e "$root" ] && [ -d "$root" ] && [ ! -L "$root" ] || die "external output root must be an existing non-symlink directory: $root"
    [ "$(realpath -e -- "$root")" = "$root" ] || die "external output root must use its canonical spelling: $root"
    worktree_text="$(git worktree list --porcelain)" || die "cannot enumerate Git worktrees"
    while IFS= read -r line; do
        case "$line" in
            "worktree "*)
                worktree="${line#worktree }"
                worktree_real="$(realpath -e -- "$worktree")" || die "cannot canonicalize Git worktree: $worktree"
                case "$root" in "$worktree_real"|"$worktree_real"/*) die "external output root must be outside every Git worktree: $root" ;; esac
                ;;
        esac
    done <<<"$worktree_text"
    EXTERNAL_ROOT="$root"; CAPTURE_OUT_ABS="$out"; EXTERNAL_PARENT="${out%/*}"; EXTERNAL_OUT_LEAF="${out##*/}"
    [ -n "$EXTERNAL_OUT_LEAF" ] && [ "$EXTERNAL_OUT_LEAF" != . ] && [ "$EXTERNAL_OUT_LEAF" != .. ] && [[ "$EXTERNAL_OUT_LEAF" != */* ]] || die "invalid external output basename"
    if [ "$EXTERNAL_PARENT" = "$root" ]; then suffix=""; else suffix="${EXTERNAL_PARENT#"$root"/}"; fi
    exec {EXTERNAL_ROOT_FD}<"$root" || die "cannot open external output root: $root"
    external_fs_helper prepare-parent "$EXTERNAL_ROOT_FD" "$root" "$EXTERNAL_PARENT" "$suffix" || die "cannot prepare external output parent"
    # This is cooperative overlap detection for conforming writers. Neither
    # flock nor mode 0700 protects staging from a hostile same-UID process.
    flock --exclusive --nonblock "$EXTERNAL_ROOT_FD" || die "external output root is locked by another conforming writer: $root"
    exec {EXTERNAL_PARENT_FD}<"$EXTERNAL_PARENT" || die "cannot open external output parent: $EXTERNAL_PARENT"
    identities="$(external_fs_helper authenticate "$EXTERNAL_ROOT_FD" "$EXTERNAL_PARENT_FD" "$root" "$EXTERNAL_PARENT" "$EXTERNAL_OUT_LEAF")" || die "cannot authenticate external output descriptors"
    IFS=$'\t' read -r EXTERNAL_ROOT_DEV EXTERNAL_ROOT_INO EXTERNAL_PARENT_DEV EXTERNAL_PARENT_INO <<<"$identities"
    external_test_hook after-fds
    external_revalidate || die "external output descriptors changed before staging"
    tokens="$(python3 - <<'PY'
import secrets
print(
    ".capture-workspace." + secrets.token_hex(16),
    ".capture-json." + secrets.token_hex(16),
    "logs-" + secrets.token_hex(16),
    "samples-" + secrets.token_hex(16),
    sep="\t",
)
PY
)" || die "cannot allocate external staging names"
    IFS=$'\t' read -r EXTERNAL_WORKSPACE_LEAF EXTERNAL_PRIVATE_LEAF EXTERNAL_LOGS_LEAF EXTERNAL_SAMPLES_LEAF <<<"$tokens"
    EXTERNAL_STAGING_ACTIVE=1
    trap external_exit_cleanup EXIT
    staging="$(external_fs_helper setup "$EXTERNAL_PARENT_FD" "$EXTERNAL_WORKSPACE_LEAF" "$EXTERNAL_PRIVATE_LEAF" "$EXTERNAL_LOGS_LEAF" "$EXTERNAL_SAMPLES_LEAF")" || die "cannot create external staging"
    IFS=$'\t' read -r EXTERNAL_WORKSPACE_DEV EXTERNAL_WORKSPACE_INO EXTERNAL_PRIVATE_DEV EXTERNAL_PRIVATE_INO <<<"$staging"
    flock --exclusive --nonblock "$EXTERNAL_ROOT_FD" || die "external output root lock was lost before work"
}

validate_external_staging_before_work() {
    external_test_hook before-work
    external_fs_helper validate-staging "$EXTERNAL_ROOT_FD" "$EXTERNAL_PARENT_FD" "$EXTERNAL_ROOT" "$EXTERNAL_PARENT" \
        "$EXTERNAL_ROOT_DEV" "$EXTERNAL_ROOT_INO" "$EXTERNAL_PARENT_DEV" "$EXTERNAL_PARENT_INO" "$EXTERNAL_OUT_LEAF" \
        "$EXTERNAL_WORKSPACE_LEAF" "$EXTERNAL_WORKSPACE_DEV" "$EXTERNAL_WORKSPACE_INO" "$EXTERNAL_PRIVATE_LEAF" \
        "$EXTERNAL_PRIVATE_DEV" "$EXTERNAL_PRIVATE_INO" "$EXTERNAL_LOGS_LEAF" "$EXTERNAL_SAMPLES_LEAF" \
        || die "external staging changed before work"
    flock --exclusive --nonblock "$EXTERNAL_ROOT_FD" || die "external output root lock was lost before work"
}

external_publish_capture_json() {
    local out="$1" tmp_out="$2" expected_tmp
    expected_tmp="$EXTERNAL_PARENT/$EXTERNAL_PRIVATE_LEAF"
    [ "$out" = "$CAPTURE_OUT_ABS" ] && [ "$tmp_out" = "$expected_tmp" ] || die "external publication path mismatch"
    external_test_hook before-publish
    external_fs_helper publish "$EXTERNAL_ROOT_FD" "$EXTERNAL_PARENT_FD" "$EXTERNAL_ROOT" "$EXTERNAL_PARENT" \
        "$EXTERNAL_ROOT_DEV" "$EXTERNAL_ROOT_INO" "$EXTERNAL_PARENT_DEV" "$EXTERNAL_PARENT_INO" "$EXTERNAL_OUT_LEAF" \
        "$EXTERNAL_PRIVATE_LEAF" "$EXTERNAL_PRIVATE_DEV" "$EXTERNAL_PRIVATE_INO" "$EXTERNAL_WORKSPACE_LEAF" \
        "$EXTERNAL_WORKSPACE_DEV" "$EXTERNAL_WORKSPACE_INO" "$EXTERNAL_LOGS_LEAF" "$EXTERNAL_SAMPLES_LEAF" \
        "$REPO_ROOT" "$CAPTURE_GIT_HEAD" "$CAPTURE_GIT_BRANCH" || die "external capture publication failed"
    EXTERNAL_STAGING_ACTIVE=0
    trap - EXIT
    external_close_descriptors
}

logical_cpus() {
    if command -v nproc >/dev/null 2>&1; then nproc
    elif getconf _NPROCESSORS_ONLN >/dev/null 2>&1; then getconf _NPROCESSORS_ONLN
    elif command -v sysctl >/dev/null 2>&1; then sysctl -n hw.ncpu
    else echo 1; fi
}

resolve_threads() {
    local requested="$1"
    if [ "$requested" = auto ]; then logical_cpus
    elif positive_integer "$requested"; then echo "$requested"
    else die "--threads must be a positive integer or auto: $requested"; fi
}

json_string_array() { jq -Rsc 'split("\n") | map(select(length > 0))'; }
join_command() { local escaped=() arg; for arg in "$@"; do printf -v arg '%q' "$arg"; escaped+=("$arg"); done; local IFS=' '; printf '%s' "${escaped[*]}"; }
sha256_file() { sha256sum -- "$1" | awk '{print $1}'; }
sha256_text() { printf '%s' "$1" | sha256sum | awk '{print $1}'; }
capture_evidence_path() {
    local path="$1"
    if [ "$EXTERNAL_MODE" -eq 0 ] && [ "$CAPTURE_STOP_ON_FIRST_RED" -eq 1 ]; then
        realpath --relative-to="$(dirname "$CAPTURE_OUT_ABS")" -- "$path"
    else
        printf '%s\n' "$path"
    fi
}
canonical_target_path() {
    local target="${CARGO_TARGET_DIR:-$REPO_ROOT/target}"
    case "$target" in /*) ;; *) target="$REPO_ROOT/$target" ;; esac
    realpath -m -- "$target"
}
cpu_model() {
    local model=""
    if [ -r /proc/cpuinfo ]; then model="$(sed -n 's/^model name[[:space:]]*:[[:space:]]*//p' /proc/cpuinfo | head -n 1)"; fi
    if [ -z "$model" ] && command -v sysctl >/dev/null 2>&1; then model="$(sysctl -n machdep.cpu.brand_string 2>/dev/null || true)"; fi
    printf '%s' "${model:-unknown}"
}
time_mode() { if [ -x /usr/bin/time ] && /usr/bin/time -v true >/dev/null 2>&1; then echo gnu_verbose; else echo bash_portable; fi; }
duration_seconds() { awk -v value="$1" 'BEGIN { split(value, p, ":"); if (length(p) == 1) print p[1]; else if (length(p) == 2) print (p[1] * 60) + p[2]; else print (p[1] * 3600) + (p[2] * 60) + p[3] }'; }

TIME_WALL=0 TIME_USER=0 TIME_SYSTEM=0 TIME_RSS=null TIME_BACKEND=""
RUN_STATUS=0 RUN_STDOUT="" RUN_STDERR="" RUN_TIME="" RUN_STATUS_LOG="" RUN_STARTED_AT="" RUN_FINISHED_AT=""

parse_time_file() {
    local mode="$1" file="$2" wall
    TIME_BACKEND="$mode"; TIME_RSS=null
    if [ "$mode" = gnu_verbose ]; then
        wall="$(sed -n 's/^[[:space:]]*Elapsed (wall clock) time (h:mm:ss or m:ss):[[:space:]]*//p' "$file")"
        TIME_WALL="$(duration_seconds "${wall:-0}")"
        TIME_USER="$(sed -n 's/^[[:space:]]*User time (seconds):[[:space:]]*//p' "$file")"
        TIME_SYSTEM="$(sed -n 's/^[[:space:]]*System time (seconds):[[:space:]]*//p' "$file")"
        TIME_RSS="$(sed -n 's/^[[:space:]]*Maximum resident set size (kbytes):[[:space:]]*//p' "$file")"
    else
        TIME_WALL="$(sed -n 's/^real[[:space:]]*//p' "$file")"; TIME_USER="$(sed -n 's/^user[[:space:]]*//p' "$file")"; TIME_SYSTEM="$(sed -n 's/^sys[[:space:]]*//p' "$file")"
    fi
    TIME_WALL="${TIME_WALL:-0}"; TIME_USER="${TIME_USER:-0}"; TIME_SYSTEM="${TIME_SYSTEM:-0}"; TIME_RSS="${TIME_RSS:-null}"
}

run_timed() {
    local label="$1" logs_dir="$2" mode status
    shift 2
    RUN_STDOUT="$logs_dir/${label}.stdout.log"; RUN_STDERR="$logs_dir/${label}.stderr.log"; RUN_TIME="$logs_dir/${label}.time.log"; RUN_STATUS_LOG="$logs_dir/${label}.status.json"; mode="$(time_mode)"
    RUN_STARTED_AT="$(date -u +%Y-%m-%dT%H:%M:%SZ)"
    set +e
    if [ "$mode" = gnu_verbose ]; then /usr/bin/time -v -o "$RUN_TIME" -- "$@" >"$RUN_STDOUT" 2>"$RUN_STDERR"; status=$?
    else TIMEFORMAT=$'real %3R\nuser %3U\nsys %3S'; { time "$@" >"$RUN_STDOUT" 2>"$RUN_STDERR"; } 2>"$RUN_TIME"; status=$?; fi
    set -e
    RUN_FINISHED_AT="$(date -u +%Y-%m-%dT%H:%M:%SZ)"
    jq -n --arg label "$label" --arg started_at "$RUN_STARTED_AT" --arg finished_at "$RUN_FINISHED_AT" --argjson exit_status "$status" '{schema_version:1,label:$label,started_at:$started_at,finished_at:$finished_at,exit_status:$exit_status}' >"$RUN_STATUS_LOG"
    chmod 0600 "$RUN_STDOUT" "$RUN_STDERR" "$RUN_TIME" "$RUN_STATUS_LOG"
    parse_time_file "$mode" "$RUN_TIME"; RUN_STATUS="$status"
}

PROBE_COMMAND=() PROBE_WARMUP_COMMAND=() PROBE_WARMUP_KIND=execution PROBE_TEST_IDENTITY=() PROBE_IDENTITY_MODE=descriptive PROBE_RESULT_FORMAT=libtest PROBE_PROFILE=""
probe_command() {
    local probe="$1" threads="$2"
    PROBE_TEST_IDENTITY=(); PROBE_WARMUP_COMMAND=(); PROBE_IDENTITY_MODE=descriptive; PROBE_WARMUP_KIND=execution; PROBE_RESULT_FORMAT=libtest; PROBE_PROFILE=""
    case "$probe" in
        store-fixture)
            PROBE_IDENTITY_MODE=exact; PROBE_TEST_IDENTITY=("store::tests::load_sessions_survives_legacy_comma_fraction_timestamp")
            PROBE_COMMAND=(cargo test -p rsid --lib "${PROBE_TEST_IDENTITY[0]}" -- --exact --test-threads "$threads") ;;
        v87-matrices)
            PROBE_IDENTITY_MODE=exact; PROBE_TEST_IDENTITY=("store::tests::h1_v87_session_fence_initial_bind_requires_exact_active_authority" "store::tests::h1_v87_session_fence_provider_live_writes_are_exact_next_sequence" "store::tests::h1_v87_session_fence_finalize_requires_complete_terminal_bundle")
            PROBE_COMMAND=(bash -c 'set -e; threads=$1; shift; for test_id; do cargo test -p rsid --lib "$test_id" -- --exact --test-threads "$threads"; done' benchmark-v87 "$threads" "${PROBE_TEST_IDENTITY[@]}") ;;
        source-scanner)
            PROBE_IDENTITY_MODE=exact; PROBE_TEST_IDENTITY=("session::issue21_phase2_tests::p2_07_gate_permit_spine::exactly_one_production_writer_of_gate_closing_and_effect_permits")
            PROBE_COMMAND=(cargo test -p rsid --lib "${PROBE_TEST_IDENTITY[0]}" -- --exact --test-threads "$threads") ;;
        nextest-fast)
            PROBE_RESULT_FORMAT=nextest; PROBE_PROFILE=rsid-fast; PROBE_TEST_IDENTITY=("rsid library nextest fast profile"); PROBE_COMMAND=(cargo nextest run --profile rsid-fast -p rsid --lib --status-level all --final-status-level all -j "$threads"); PROBE_WARMUP_COMMAND=(cargo nextest run --profile rsid-fast -p rsid --lib --no-run); PROBE_WARMUP_KIND=artifact-build ;;
        nextest-full)
            PROBE_RESULT_FORMAT=nextest; PROBE_PROFILE=ci-full; PROBE_TEST_IDENTITY=("workspace nextest full profile"); PROBE_COMMAND=(cargo nextest run --profile ci-full --workspace --status-level all --final-status-level all -j "$threads"); PROBE_WARMUP_COMMAND=(cargo nextest run --profile ci-full --workspace --no-run); PROBE_WARMUP_KIND=artifact-build ;;
        rsid-serial)
            [ "$threads" = 1 ] || die "rsid-serial always executes with one thread; use --threads 1"; PROBE_TEST_IDENTITY=("rsid library test binary"); PROBE_COMMAND=(cargo test -p rsid --lib -- --test-threads 1); PROBE_WARMUP_COMMAND=(cargo test -p rsid --lib --no-run); PROBE_WARMUP_KIND=artifact-build ;;
        workspace-doctests)
            PROBE_TEST_IDENTITY=("workspace doctests"); PROBE_COMMAND=(cargo test --workspace --doc -- --test-threads "$threads") ;;
        *) die "unsupported probe: $probe" ;;
    esac
    [ "${#PROBE_WARMUP_COMMAND[@]}" -gt 0 ] || PROBE_WARMUP_COMMAND=("${PROBE_COMMAND[@]}")
}

# Emit unique normalized identities for failure and exact-identity evidence.
# Cargo/libtest aggregate totals intentionally use complete harness summaries
# below: status records can be split by output from a test subprocess.
observed_records() {
    local stdout_path="$1" stderr_path="$2"
    {
        sed -nE 's/^[[:space:]]*test[[:space:]]+(.+)[[:space:]]+\.\.\.[[:space:]]+ok[[:space:]]*$/passed\t\1/p' "$stdout_path" "$stderr_path"
        sed -nE 's/^[[:space:]]*test[[:space:]]+(.+)[[:space:]]+\.\.\.[[:space:]]+FAILED[[:space:]]*$/failed\t\1/p' "$stdout_path" "$stderr_path"
        sed -nE 's/^[[:space:]]*test[[:space:]]+(.+)[[:space:]]+\.\.\.[[:space:]]+ignored(,.*)?[[:space:]]*$/ignored\t\1/p' "$stdout_path" "$stderr_path"
        sed -nE 's/^[[:space:]]*PASS[[:space:]]+\[[^]]*\][[:space:]]+(\(([0-9]+\/[0-9]+|(─)+)\)[[:space:]]+)?([^[:space:]]+)[[:space:]]+(.+)$/passed\t\4::\5/p' "$stdout_path" "$stderr_path"
        sed -nE 's/^[[:space:]]*FAIL[[:space:]]+\[[^]]*\][[:space:]]+(\(([0-9]+\/[0-9]+|(─)+)\)[[:space:]]+)?([^[:space:]]+)[[:space:]]+(.+)$/failed\t\4::\5/p' "$stdout_path" "$stderr_path"
        sed -nE 's/^[[:space:]]*(SKIP|IGNORED)[[:space:]]+\[[^]]*\][[:space:]]+(\(([0-9]+\/[0-9]+|(─)+)\)[[:space:]]+)?([^[:space:]]+)[[:space:]]+(.+)$/ignored\t\5::\6/p' "$stdout_path" "$stderr_path"
    } | sort -u
}

libtest_summary_count() {
    local kind="$1" stdout_path="$2" stderr_path="$3" field
    case "$kind" in
        passed) field=1 ;;
        failed) field=2 ;;
        ignored) field=3 ;;
        *) die "unsupported observed count: $kind" ;;
    esac
    sed -nE 's/^[[:space:]]*test result:[[:space:]]+(ok|FAILED)\.[[:space:]]+([0-9]+)[[:space:]]+passed;[[:space:]]+([0-9]+)[[:space:]]+failed;[[:space:]]+([0-9]+)[[:space:]]+ignored;[[:space:]]+[0-9]+[[:space:]]+measured;[[:space:]]+[0-9]+[[:space:]]+filtered out;[[:space:]]+finished in .+$/\2\t\3\t\4/p' "$stdout_path" "$stderr_path" | awk -F '\t' -v field="$field" '{ total += $field } END { print total + 0 }'
}

executed_test_names() {
    local stdout_path="$1" stderr_path="$2"
    observed_records "$stdout_path" "$stderr_path" | awk -F '\t' '{ print $2 }' | sort -u | json_string_array
}
executed_runnable_test_names() {
    local stdout_path="$1" stderr_path="$2"
    observed_records "$stdout_path" "$stderr_path" | awk -F '\t' '$1 != "ignored" { print $2 }' | sort -u | json_string_array
}
ignored_test_names() {
    local stdout_path="$1" stderr_path="$2"
    observed_records "$stdout_path" "$stderr_path" | awk -F '\t' '$1 == "ignored" { print $2 }' | sort -u | json_string_array
}
failure_names() {
    local stdout_path="$1" stderr_path="$2"
    observed_records "$stdout_path" "$stderr_path" | awk -F '\t' '$1 == "failed" { print $2 }' | json_string_array
}
observed_count() {
    local kind="$1" stdout_path="$2" stderr_path="$3"
    observed_records "$stdout_path" "$stderr_path" | awk -F '\t' -v kind="$kind" '$1 == kind { n++ } END { print n + 0 }'
}
aggregate_count() {
    local kind="$1" stdout_path="$2" stderr_path="$3"
    if [ "$PROBE_RESULT_FORMAT" = libtest ]; then
        libtest_summary_count "$kind" "$stdout_path" "$stderr_path"
    else
        observed_count "$kind" "$stdout_path" "$stderr_path"
    fi
}
identity_proof_file() {
    local declared_file="$1" executed_file="$2" output_file="$3"
    jq -n --arg mode "$PROBE_IDENTITY_MODE" --slurpfile declared "$declared_file" --slurpfile executed "$executed_file" '{mode: $mode, declared: $declared[0], executed: $executed[0], verified: (if $mode == "exact" then ($declared[0] | length) > 0 and (($declared[0] | sort) == ($executed[0] | sort)) else true end)}' >"$output_file"
}

write_capture_json() {
    local out="$1" tmp_out="$2" label="$3" probe="$4" repeat="$5" requested_threads="$6" resolved_threads="$7" workspace="$8" warmup_status="$9" samples_file="${10}" identity_file="${11}"
    local host_class command_text host_os host_kernel host_arch host_cpu host_fields host_fingerprint
    local target_path target_device target_inode target_fields target_fingerprint warmup_rss_disposition
    local evidence_workspace evidence_warmup_stdout evidence_warmup_stderr evidence_warmup_time evidence_warmup_status
    require_unchanged_capture_source
    host_os="$(uname -s)"; host_kernel="$(uname -r)"; host_arch="$(uname -m)"; host_cpu="$(cpu_model)"
    host_class="$host_os-$host_arch-$(logical_cpus)cpu"; host_fields="$host_class|$host_os|$host_kernel|$host_arch|$host_cpu|$(logical_cpus)"; host_fingerprint="$(sha256_text "$host_fields")"
    target_path="$(canonical_target_path)"; [ -d "$target_path" ] || die "canonical Cargo target directory does not exist: $target_path"
    target_device="$(stat -Lc %d -- "$target_path")"; target_inode="$(stat -Lc %i -- "$target_path")"; target_fields="$target_path|$target_device|$target_inode"; target_fingerprint="$(sha256_text "$target_fields")"
    command_text="$(join_command "${PROBE_COMMAND[@]}")"
    evidence_workspace="$(capture_evidence_path "$workspace")"
    evidence_warmup_stdout="$(capture_evidence_path "$WARMUP_STDOUT")"; evidence_warmup_stderr="$(capture_evidence_path "$WARMUP_STDERR")"; evidence_warmup_time="$(capture_evidence_path "$WARMUP_TIME")"; evidence_warmup_status="$(capture_evidence_path "$WARMUP_STATUS_LOG")"
    if [ "$WARMUP_RSS" = null ]; then warmup_rss_disposition=unavailable_portable_backend; else warmup_rss_disposition=measured; fi
    jq -n --argjson schema_version "$SCHEMA_VERSION" --arg captured_at "$(date -u +%Y-%m-%dT%H:%M:%SZ)" --arg label "$label" --arg probe "$probe" --arg profile "$PROBE_PROFILE" --arg head "$CAPTURE_GIT_HEAD" --arg branch "$CAPTURE_GIT_BRANCH" --arg host_class "$host_class" --arg os "$host_os" --arg kernel "$host_kernel" --arg architecture "$host_arch" --arg cpu_model "$host_cpu" --argjson logical_cpus "$(logical_cpus)" --arg host_fingerprint "$host_fingerprint" --arg cargo_target_dir "$target_path" --arg target_device "$target_device" --arg target_inode "$target_inode" --arg target_fingerprint "$target_fingerprint" --arg cargo_version "$(cargo --version)" --arg rustc_version "$(rustc --version)" --arg nextest_version "$(cargo nextest --version)" --arg command "$command_text" --slurpfile identity "$identity_file" --arg identity_mode "$PROBE_IDENTITY_MODE" --argjson requested_repeat "$repeat" --argjson requested_threads "$requested_threads" --argjson resolved_threads "$resolved_threads" --argjson stop_on_first_red "$CAPTURE_STOP_ON_FIRST_RED" --arg workspace "$evidence_workspace" --argjson warmup_status "$warmup_status" --argjson warmup_wall "$WARMUP_WALL" --argjson warmup_user "$WARMUP_USER" --argjson warmup_system "$WARMUP_SYSTEM" --argjson warmup_rss "$WARMUP_RSS" --arg warmup_rss_disposition "$warmup_rss_disposition" --arg warmup_backend "$WARMUP_BACKEND" --arg warmup_kind "$PROBE_WARMUP_KIND" --arg warmup_command "$WARMUP_COMMAND" --arg warmup_stdout "$evidence_warmup_stdout" --arg warmup_stderr "$evidence_warmup_stderr" --arg warmup_time "$evidence_warmup_time" --arg warmup_stdout_hash "$(sha256_file "$WARMUP_STDOUT")" --arg warmup_stderr_hash "$(sha256_file "$WARMUP_STDERR")" --arg warmup_time_hash "$(sha256_file "$WARMUP_TIME")" --slurpfile samples "$samples_file" '{schema_version: $schema_version, capture: {captured_at: $captured_at, label: $label, probe: $probe, profile: $profile, source: {head: $head, branch: $branch, clean_tree: true}, host: {class: $host_class, os: $os, kernel: $kernel, architecture: $architecture, cpu_model: $cpu_model, logical_cpus: $logical_cpus, fingerprint_sha256: $host_fingerprint}, toolchain: {cargo: $cargo_version, rustc: $rustc_version, cargo_nextest: $nextest_version, cargo_target_dir: $cargo_target_dir}, target: {canonical_path: $cargo_target_dir, device: $target_device, inode: $target_inode, fingerprint_sha256: $target_fingerprint}, requested: {repeat: $requested_repeat, threads: $requested_threads, resolved_threads: $resolved_threads, stop_on_first_red: ($stop_on_first_red == 1)}, workspace: $workspace, warmup: {kind: $warmup_kind, command: $warmup_command, exit_status: $warmup_status, timing: {backend: $warmup_backend, wall_seconds: $warmup_wall, user_seconds: $warmup_user, system_seconds: $warmup_system, max_rss_kib: $warmup_rss, max_rss_disposition: $warmup_rss_disposition}, logs: {stdout: $warmup_stdout, stderr: $warmup_stderr, time: $warmup_time}, log_sha256: {stdout: $warmup_stdout_hash, stderr: $warmup_stderr_hash, time: $warmup_time_hash}}, execution: {command: $command, profile: $profile, test_identity: $identity[0], identity_mode: $identity_mode}}, samples: $samples[0]}' >"$tmp_out"
    jq --arg captured_at "$CAPTURE_COMPLETED_AT" --arg status "$evidence_warmup_status" --arg status_hash "$(sha256_file "$WARMUP_STATUS_LOG")" '.capture.captured_at=$captured_at | .capture.warmup.logs.status=$status | .capture.warmup.log_sha256.status=$status_hash' "$tmp_out" >"$tmp_out.status"
    cp "$tmp_out.status" "$tmp_out"; rm "$tmp_out.status"; chmod 0600 "$tmp_out"
    publish_capture_json "$out" "$tmp_out"
}

capture() {
    local label="" probe="" repeat="" threads="" out="" external_root="" external_root_count=0 saw_out=0 stop_on_first_red=0
    while [ "$#" -gt 0 ]; do
        case "$1" in
            --label) [ "$#" -ge 2 ] || die "--label requires a value"; label="$2"; shift 2 ;;
            --probe) [ "$#" -ge 2 ] || die "--probe requires a value"; probe="$2"; shift 2 ;;
            --repeat) [ "$#" -ge 2 ] || die "--repeat requires a value"; repeat="$2"; shift 2 ;;
            --threads) [ "$#" -ge 2 ] || die "--threads requires a value"; threads="$2"; shift 2 ;;
            --stop-on-first-red) stop_on_first_red=1; shift ;;
            --external-output-root)
                [ "$#" -ge 2 ] && [[ "$2" != --* ]] || die "--external-output-root requires a value"
                [ "$saw_out" -eq 0 ] || die "--external-output-root must precede --out"
                external_root_count=$((external_root_count + 1))
                [ "$external_root_count" -eq 1 ] || die "--external-output-root may be supplied only once"
                external_root="$2"; shift 2
                ;;
            --out) [ "$#" -ge 2 ] || die "--out requires a value"; out="$2"; saw_out=1; shift 2 ;;
            *) die "unknown capture argument: $1" ;;
        esac
    done
    [ -n "$label" ] && [ -n "$probe" ] && [ -n "$repeat" ] && [ -n "$threads" ] && [ -n "$out" ] || die "capture requires --label, --probe, --repeat, --threads, and --out"
    CAPTURE_STOP_ON_FIRST_RED="$stop_on_first_red"
    [[ "$label" =~ ^[A-Za-z0-9][A-Za-z0-9._-]*$ ]] || die "--label may contain only letters, digits, dot, underscore, and dash"; positive_integer "$repeat" || die "--repeat must be a positive integer: $repeat"
    local out_dir workspace logs_dir samples_dir tmp_out identity_file samples_file
    local resolved_threads
    if [ "$external_root_count" -eq 1 ]; then
        EXTERNAL_MODE=1
        require_jq; need_command git; need_command cargo; need_command awk; need_command sed; need_command realpath; need_command sha256sum; need_command stat
        require_clean_tree; capture_source_state
        resolved_threads="$(resolve_threads "$threads")"; probe_command "$probe" "$resolved_threads"
        umask 077
        prepare_external_output_destination "$external_root" "$out"
        out_dir="$EXTERNAL_PARENT"; workspace="$EXTERNAL_PARENT/$EXTERNAL_WORKSPACE_LEAF"
        logs_dir="$workspace/$EXTERNAL_LOGS_LEAF"; samples_dir="$workspace/$EXTERNAL_SAMPLES_LEAF"
        tmp_out="$EXTERNAL_PARENT/$EXTERNAL_PRIVATE_LEAF"; identity_file="$workspace/identity.json"; samples_file="$workspace/samples.json"
        printf '%s\n' "${PROBE_TEST_IDENTITY[@]}" | json_string_array >"$identity_file"; printf '[]\n' >"$samples_file"
        validate_external_staging_before_work
        cargo nextest --version >/dev/null 2>&1 || die "cargo-nextest is required; install it with: cargo install cargo-nextest"
    else
        validate_output_path "$out"; require_jq; need_command git; need_command cargo; need_command awk; need_command sed; need_command realpath; need_command sha256sum; need_command stat; require_clean_tree; capture_source_state; cargo nextest --version >/dev/null 2>&1 || die "cargo-nextest is required; install it with: cargo install cargo-nextest"
        resolved_threads="$(resolve_threads "$threads")"; probe_command "$probe" "$resolved_threads"; prepare_output_destination "$out"
        out_dir="$(dirname "$CAPTURE_OUT_ABS")"; umask 077; workspace="$(mktemp -d "$out_dir/.${label}-${probe}.capture.XXXXXX")"; logs_dir="$workspace/logs"; samples_dir="$workspace/samples"; mkdir "$logs_dir" "$samples_dir"; tmp_out="$(mktemp "$out_dir/.capture-json.XXXXXX")"; identity_file="$workspace/identity.json"; samples_file="$workspace/samples.json"; printf '%s\n' "${PROBE_TEST_IDENTITY[@]}" | json_string_array >"$identity_file"; printf '[]\n' >"$samples_file"
    fi
    if [ "$PROBE_RESULT_FORMAT" = nextest ]; then
        local XDG_CONFIG_HOME="$workspace" NEXTEST_CONFIG_FILE="$REPO_ROOT/.config/nextest.toml"
        export XDG_CONFIG_HOME NEXTEST_CONFIG_FILE
    fi
    run_timed warmup "$logs_dir" "${PROBE_WARMUP_COMMAND[@]}"
    WARMUP_WALL="$TIME_WALL" WARMUP_USER="$TIME_USER" WARMUP_SYSTEM="$TIME_SYSTEM" WARMUP_RSS="$TIME_RSS" WARMUP_BACKEND="$TIME_BACKEND" WARMUP_COMMAND="$(join_command "${PROBE_WARMUP_COMMAND[@]}")" WARMUP_STDOUT="$RUN_STDOUT" WARMUP_STDERR="$RUN_STDERR" WARMUP_TIME="$RUN_TIME" WARMUP_STATUS_LOG="$RUN_STATUS_LOG" CAPTURE_COMPLETED_AT="$RUN_FINISHED_AT"
    if [ "$RUN_STATUS" -ne 0 ]; then write_capture_json "$CAPTURE_OUT_ABS" "$tmp_out" "$label" "$probe" "$repeat" "$( [ "$threads" = auto ] && printf null || printf '%s' "$threads" )" "$resolved_threads" "$workspace" "$RUN_STATUS" "$samples_file" "$identity_file"; echo "${SCRIPT_NAME}: warmup failed; evidence written to $out" >&2; return "$RUN_STATUS"; fi
    local index overall_status=0
    for ((index = 1; index <= repeat; index++)); do
        local failures_file executed_file runnable_file ignored_file proof_file tests_passed tests_failed tests_ignored evidence_valid command_text rss_disposition
        failures_file="$workspace/$index.failures.json"; executed_file="$workspace/$index.executed.json"; runnable_file="$workspace/$index.runnable.json"; ignored_file="$workspace/$index.ignored.json"; proof_file="$workspace/$index.proof.json"
        run_timed "sample-${index}" "$logs_dir" "${PROBE_COMMAND[@]}"; failure_names "$RUN_STDOUT" "$RUN_STDERR" >"$failures_file"; executed_test_names "$RUN_STDOUT" "$RUN_STDERR" >"$executed_file"; executed_runnable_test_names "$RUN_STDOUT" "$RUN_STDERR" >"$runnable_file"; ignored_test_names "$RUN_STDOUT" "$RUN_STDERR" >"$ignored_file"; tests_passed="$(aggregate_count passed "$RUN_STDOUT" "$RUN_STDERR")"; tests_failed="$(aggregate_count failed "$RUN_STDOUT" "$RUN_STDERR")"; tests_ignored="$(aggregate_count ignored "$RUN_STDOUT" "$RUN_STDERR")"; identity_proof_file "$identity_file" "$executed_file" "$proof_file"; command_text="$(join_command "${PROBE_COMMAND[@]}")"; evidence_valid=false
        if [ "$RUN_STATUS" -eq 0 ] && [ "$tests_failed" -eq 0 ] && [ "$tests_passed" -gt 0 ] && [ "$(jq -r .verified "$proof_file")" = true ]; then evidence_valid=true; else overall_status=1; fi
        if [ "$TIME_RSS" = null ]; then rss_disposition=unavailable_portable_backend; else rss_disposition=measured; fi
        jq -n --argjson index "$index" --arg command "$command_text" --argjson exit_status "$RUN_STATUS" --argjson wall "$TIME_WALL" --argjson user "$TIME_USER" --argjson system "$TIME_SYSTEM" --argjson max_rss "$TIME_RSS" --arg rss_disposition "$rss_disposition" --arg time_backend "$TIME_BACKEND" --arg stdout "$(capture_evidence_path "$RUN_STDOUT")" --arg stderr "$(capture_evidence_path "$RUN_STDERR")" --arg time_log "$(capture_evidence_path "$RUN_TIME")" --arg stdout_hash "$(sha256_file "$RUN_STDOUT")" --arg stderr_hash "$(sha256_file "$RUN_STDERR")" --arg time_hash "$(sha256_file "$RUN_TIME")" --slurpfile failures "$failures_file" --slurpfile executed "$executed_file" --slurpfile runnable "$runnable_file" --slurpfile ignored_names "$ignored_file" --argjson passed "$tests_passed" --argjson failed "$tests_failed" --argjson ignored "$tests_ignored" --slurpfile proof "$proof_file" --argjson evidence_valid "$evidence_valid" '{index: $index, execution: {command: $command, exit_status: $exit_status, evidence_valid: $evidence_valid, timing: {backend: $time_backend, wall_seconds: $wall, user_seconds: $user, system_seconds: $system, max_rss_kib: $max_rss, max_rss_disposition: $rss_disposition}, logs: {stdout: $stdout, stderr: $stderr, time: $time_log}, log_sha256: {stdout: $stdout_hash, stderr: $stderr_hash, time: $time_hash}}, observed: {passed_lines: $passed, failed_lines: $failed, ignored_lines: $ignored, failure_names: $failures[0], executed_test_names: $executed[0], executed_runnable_test_names: $runnable[0], ignored_test_names: $ignored_names[0], identity_proof: $proof[0]}}' >"$samples_dir/$index.json"
        jq --arg status "$(capture_evidence_path "$RUN_STATUS_LOG")" --arg status_hash "$(sha256_file "$RUN_STATUS_LOG")" '.execution.logs.status=$status | .execution.log_sha256.status=$status_hash' "$samples_dir/$index.json" >"$samples_dir/$index.status.json"
        mv "$samples_dir/$index.status.json" "$samples_dir/$index.json"; chmod 0600 "$samples_dir/$index.json"; CAPTURE_COMPLETED_AT="$RUN_FINISHED_AT"
        [ "$RUN_STATUS" -eq 0 ] || overall_status="$RUN_STATUS"
        if [ "$stop_on_first_red" -eq 1 ] && [ "$evidence_valid" != true ]; then break; fi
    done
    jq -s 'sort_by(.index)' "$samples_dir"/*.json >"$samples_file"; write_capture_json "$CAPTURE_OUT_ABS" "$tmp_out" "$label" "$probe" "$repeat" "$( [ "$threads" = auto ] && printf null || printf '%s' "$threads" )" "$resolved_threads" "$workspace" 0 "$samples_file" "$identity_file"
    if [ "$overall_status" -ne 0 ]; then echo "${SCRIPT_NAME}: one or more samples were incomplete or failed; evidence written to $out" >&2; return "$overall_status"; fi
}

generator_override_names() {
    printf '%s\n' \
        S1Q_TEST_CURRENT_FACTS_JSON S1Q_TEST_GENERATE_HOOK_STAGE S1Q_TEST_STAGE_FIFO S1Q_TEST_CONTINUE_FIFO \
        S1Q_TEST_MAX_PREFLIGHT_BYTES S1Q_TEST_MAX_ENUM_BYTES S1Q_TEST_MAX_CAPTURE_BYTES S1Q_TEST_MAX_RAW_BYTES \
        S1Q_TEST_MAX_AGGREGATE_BYTES S1Q_TEST_MAX_OUTPUT_BYTES S1Q_TEST_MAX_JSON_DEPTH S1Q_TEST_MAX_SUITES \
        S1Q_TEST_MAX_TESTS S1Q_TEST_MAX_SAMPLES S1Q_TEST_MAX_IDENTITIES S1Q_TEST_MAX_STRING_BYTES
}

reject_public_fixture_environment() {
    local name
    while IFS= read -r name; do
        case "$name" in
            FAKE_*|S1Q_TEST_*|S1Q_INTERNAL_*|S1Q_PRIVATE_*|S1Q_SELF_TEST_MODE)
                die "public calibration operation forbids fixture environment: $name"
                ;;
        esac
    done < <(compgen -e)
}

# This is intentionally a builtins-only launch gate.  It runs before repository,
# evidence, configuration, or output paths are opened and before a public
# operation executes any external program.
validate_sterile_launch() {
    local operation="$1"; shift
    local script_abs="$0" repo expected_path name expected_value index
    local -a allowed=(HOME CARGO_HOME RUSTUP_HOME PATH LANG LC_ALL TZ TMPDIR CARGO_TARGET_DIR XDG_CONFIG_HOME NEXTEST_CONFIG_FILE PWD SHLVL _)
    local -a actual_cmdline expected_cmdline

    reject_public_fixture_environment
    [[ "$script_abs" = /*/scripts/test-suite-benchmark.sh ]] || die "$operation requires the absolute canonical script path"
    repo="${script_abs%/scripts/test-suite-benchmark.sh}"
    [[ "$repo" = /* && "$repo" != *'/../'* && "$repo" != *'/./'* && "$repo" != *'//' ]] || die "$operation rejects a noncanonical repository path"
    [[ -f "$script_abs" && ! -L "$script_abs" ]] || die "$operation rejects a replaced or symlinked script"

    expected_path="/home/jakedevar/.rustup/toolchains/1.94.1-x86_64-unknown-linux-gnu/bin:/home/jakedevar/.cargo/bin:/usr/bin:/bin"
    [[ ${PATH-} = "$expected_path" ]] || die "public calibration operation rejects PATH replacement"
    [[ /proc/$$/exe -ef /usr/bin/bash && "$-" = *p* && ":$SHELLOPTS:" = *:privileged:* ]] || die "$operation requires /usr/bin/bash privileged mode"
    [[ ${HOME-} = /home/jakedevar ]] || die "$operation rejects HOME"
    [[ ${CARGO_HOME-} = /home/jakedevar/.cargo ]] || die "$operation rejects CARGO_HOME"
    [[ ${RUSTUP_HOME-} = /home/jakedevar/.rustup ]] || die "$operation rejects RUSTUP_HOME"
    [[ ${LANG-} = C && ${LC_ALL-} = C && ${TZ-} = UTC ]] || die "$operation requires LANG=C LC_ALL=C TZ=UTC"
    [[ ${TMPDIR-} = "$repo/$OUTPUT_ROOT/tmp" ]] || die "$operation rejects TMPDIR"
    [[ ${CARGO_TARGET_DIR-} = "$repo/target" ]] || die "$operation rejects CARGO_TARGET_DIR"
    [[ ${XDG_CONFIG_HOME-} = "$repo/$OUTPUT_ROOT/xdg-empty" ]] || die "$operation rejects XDG_CONFIG_HOME"
    [[ ${NEXTEST_CONFIG_FILE-} = "$repo/.config/nextest.toml" ]] || die "$operation rejects NEXTEST_CONFIG_FILE"
    [[ "$PWD" = "$repo" && -d "$repo" && ! -L "$repo" ]] || die "$operation requires the canonical repository as cwd"

    while IFS= read -r name; do
        case " ${allowed[*]} " in *" $name "*) ;; *) die "$operation rejects inherited environment variable: $name" ;; esac
    done < <(compgen -e)

    mapfile -d '' -t actual_cmdline </proc/$$/cmdline
    expected_cmdline=(/usr/bin/bash --noprofile --norc -p "$script_abs" "$operation" "$@")
    [[ ${#actual_cmdline[@]} -eq ${#expected_cmdline[@]} ]] || die "$operation rejects the process command line"
    for ((index = 0; index < ${#expected_cmdline[@]}; index++)); do
        [[ ${actual_cmdline[index]} = "${expected_cmdline[index]}" ]] || die "$operation rejects the process command line"
    done
}

authenticate_public_tool_path() {
    local name="$1" expected="$2" actual
    [ -x "$expected" ] && [ ! -L "$expected" ] || {
        [ -L "$expected" ] && [ -x "$expected" ] || die "trusted executable is unavailable: $expected"
    }
    actual="$(command -v "$name" 2>/dev/null || true)"
    [ "$actual" = "$expected" ] || die "public calibration operation rejects PATH replacement for $name"
}

authenticate_public_environment() {
    local account_home
    reject_public_fixture_environment
    account_home="$(/usr/bin/getent passwd "$(/usr/bin/id -u)" | /usr/bin/awk -F: '{print $6}')"
    [ -n "$account_home" ] || die "cannot resolve account home for trusted toolchain"
    authenticate_public_tool_path git /usr/bin/git
    authenticate_public_tool_path cargo "$account_home/.cargo/bin/cargo"
    authenticate_public_tool_path rustc "$account_home/.cargo/bin/rustc"
    authenticate_public_tool_path nproc /usr/bin/nproc
    authenticate_public_tool_path uname /usr/bin/uname
    authenticate_public_tool_path lscpu /usr/bin/lscpu
    authenticate_public_tool_path make /usr/bin/make
    authenticate_public_tool_path jq /usr/bin/jq
    PATH="$account_home/.cargo/bin:/usr/bin:/bin"; export PATH
    hash -r
}

attest_execute_one() {
    local mode="$1" name="$2" threads="$3" isolated_config="$4"
    if [ "$mode" = private-self-test ]; then
        private_attest_fixture_command "$name"
        return
    fi
    case "$name" in
        git-head) /usr/bin/git rev-parse HEAD ;;
        git-branch) /usr/bin/git branch --show-current ;;
        git-status) /usr/bin/git status --porcelain=v1 ;;
        git-diff-check) /usr/bin/git diff --check ;;
        snapshots) /usr/bin/find . -type f -name '*.snap.new' -print ;;
        nproc) /usr/bin/nproc ;;
        uname) /usr/bin/uname -a ;;
        lscpu) /usr/bin/lscpu ;;
        cargo-version) cargo --version ;;
        rustc-version) rustc --version ;;
        nextest-version) cargo nextest --version ;;
        cargo-metadata) cargo metadata --format-version 1 --no-deps ;;
        nextest-config) XDG_CONFIG_HOME="$isolated_config" NEXTEST_CONFIG_FILE="$REPO_ROOT/.config/nextest.toml" cargo nextest show-config test-groups ;;
        nextest-fast-list) XDG_CONFIG_HOME="$isolated_config" NEXTEST_CONFIG_FILE="$REPO_ROOT/.config/nextest.toml" cargo nextest list --profile rsid-fast -p rsid --lib --message-format json ;;
        nextest-full-list) XDG_CONFIG_HOME="$isolated_config" NEXTEST_CONFIG_FILE="$REPO_ROOT/.config/nextest.toml" cargo nextest list --profile ci-full --workspace --message-format json ;;
        make-test-fast) XDG_CONFIG_HOME="$isolated_config" NEXTEST_CONFIG_FILE="$REPO_ROOT/.config/nextest.toml" /usr/bin/make test-fast "NEXTEST_JOBS=$threads" ;;
        make-test-full) XDG_CONFIG_HOME="$isolated_config" NEXTEST_CONFIG_FILE="$REPO_ROOT/.config/nextest.toml" /usr/bin/make test-full "NEXTEST_JOBS=$threads" ;;
        *) die "unsupported preflight command: $name" ;;
    esac
}

attest_preflight_impl() {
    local mode="$1" calibration_root="" threads="" root_head root_abs parent_abs staging isolated_config config_source
    shift
    while [ "$#" -gt 0 ]; do
        case "$1" in
            --calibration-root) [ "$#" -ge 2 ] || die "--calibration-root requires a value"; calibration_root="$2"; shift 2 ;;
            --threads) [ "$#" -ge 2 ] || die "--threads requires a value"; threads="$2"; shift 2 ;;
            *) die "unknown attest-preflight argument: $1" ;;
        esac
    done
    [ -n "$calibration_root" ] && positive_integer "$threads" || die "attest-preflight requires --calibration-root and positive --threads"
    if [ "$mode" = public ]; then authenticate_public_environment; fi
    initialize_repo_root
    validate_output_path "$calibration_root"
    case "$calibration_root" in "$OUTPUT_ROOT/z-baseline-calibration/"*) ;; *) die "calibration root must be source-keyed below $OUTPUT_ROOT/z-baseline-calibration/" ;; esac
    root_head="${calibration_root##*/}"; [[ "$root_head" =~ ^[0-9a-f]{40,64}$ ]] || die "calibration root must be keyed by a lowercase source commit"
    root_abs="$REPO_ROOT/$calibration_root"; parent_abs="$(dirname "$root_abs")"
    [ ! -e "$root_abs" ] && [ ! -L "$root_abs" ] || die "refusing to overwrite calibration root"
    umask 077; mkdir -p "$parent_abs"; chmod 0700 "$REPO_ROOT/$OUTPUT_ROOT" "$REPO_ROOT/$OUTPUT_ROOT/z-baseline-calibration" 2>/dev/null || true
    staging="$(mktemp -d "$parent_abs/.preflight-attestation.XXXXXX")"; chmod 0700 "$staging"
    mkdir -m 0700 "$staging/preflight" "$staging/.isolated-nextest-config" "$staging/.meta"
    isolated_config="$staging/.isolated-nextest-config"
    config_source="$REPO_ROOT/.config/nextest.toml"
    [ -f "$config_source" ] && [ ! -L "$config_source" ] || die "authenticated repository Nextest config is unavailable"
    install -m 0600 "$config_source" "$staging/preflight/nextest-config.toml"

    local -a names=(git-head git-branch git-status git-diff-check snapshots nproc uname lscpu cargo-version rustc-version nextest-version cargo-metadata nextest-config nextest-fast-list nextest-full-list make-test-fast make-test-full)
    local sequence=0 name stdout stderr started finished status overall=0 out_fd err_fd evidence_bytes=0 node_bytes
    for name in "${names[@]}"; do
        sequence=$((sequence + 1))
        if [ "$name" = nextest-fast-list ]; then stdout="$staging/nextest-fast-list.json"
        elif [ "$name" = nextest-full-list ]; then stdout="$staging/nextest-full-list.json"
        else stdout="$staging/preflight/$name.stdout"; fi
        stderr="$staging/preflight/$name.stderr"
        : >"$stdout"; : >"$stderr"; chmod 0600 "$stdout" "$stderr"
        exec {out_fd}>>"$stdout"; exec {err_fd}>>"$stderr"
        started="$(date -u +%Y-%m-%dT%H:%M:%SZ)"
        set +e; (ulimit -f 65536; attest_execute_one "$mode" "$name" "$threads" "$isolated_config") >&$out_fd 2>&$err_fd; status=$?; set -e
        finished="$(date -u +%Y-%m-%dT%H:%M:%SZ)"
        exec {out_fd}>&-; exec {err_fd}>&-
        for node_bytes in "$(/usr/bin/stat -Lc %s "$stdout")" "$(/usr/bin/stat -Lc %s "$stderr")"; do
            [ "$node_bytes" -le $((64 * 1024 * 1024)) ] || { rm -rf -- "$staging"; die "preflight raw output exceeded immutable byte bound: $name"; }
            evidence_bytes=$((evidence_bytes + node_bytes))
            [ "$evidence_bytes" -le $((1024 * 1024 * 1024)) ] || { rm -rf -- "$staging"; die "preflight aggregate output exceeded immutable byte bound"; }
        done
        jq -n --argjson sequence "$sequence" --arg name "$name" --arg started_at "$started" --arg finished_at "$finished" --argjson exit_status "$status" '{sequence:$sequence,name:$name,started_at:$started_at,finished_at:$finished_at,exit_status:$exit_status}' >"$staging/.meta/$sequence.json"
        chmod 0600 "$staging/.meta/$sequence.json"
        if [ "$status" -ne 0 ]; then overall="$status"; break; fi
    done

    [ "$overall" -eq 0 ] && [ "$sequence" -eq "${#names[@]}" ] || { rm -rf -- "$staging"; return "${overall:-2}"; }
    CAPTURE_GIT_HEAD="$(/usr/bin/sed -n '1p' "$staging/preflight/git-head.stdout")"
    CAPTURE_GIT_BRANCH="$(/usr/bin/sed -n '1p' "$staging/preflight/git-branch.stdout")"
    [ "$root_head" = "$CAPTURE_GIT_HEAD" ] || { rm -rf -- "$staging"; die "calibration root source key differs from attested HEAD"; }

    if ! /usr/bin/python3 - "$staging" "$REPO_ROOT" "$threads" "$CAPTURE_GIT_HEAD" "$CAPTURE_GIT_BRANCH" "$CUSTODY_THREAT_STATEMENT" <<'PY'
import hashlib, json, os, re, stat, sys, tomllib
root, repo, threads_text, head, branch, threat_statement = sys.argv[1:]
threads = int(threads_text)
names = ["git-head","git-branch","git-status","git-diff-check","snapshots","nproc","uname","lscpu","cargo-version","rustc-version","nextest-version","cargo-metadata","nextest-config","nextest-fast-list","nextest-full-list","make-test-fast","make-test-full"]
commands = {
"git-head":"git rev-parse HEAD","git-branch":"git branch --show-current","git-status":"git status --porcelain=v1","git-diff-check":"git diff --check","snapshots":"find . -type f -name '*.snap.new' -print","nproc":"nproc","uname":"uname -a","lscpu":"lscpu","cargo-version":"cargo --version","rustc-version":"rustc --version","nextest-version":"cargo nextest --version","cargo-metadata":"cargo metadata --format-version 1 --no-deps","nextest-config":"cargo nextest show-config test-groups","nextest-fast-list":"cargo nextest list --profile rsid-fast -p rsid --lib --message-format json","nextest-full-list":"cargo nextest list --profile ci-full --workspace --message-format json","make-test-fast":f"make test-fast NEXTEST_JOBS={threads}","make-test-full":f"make test-full NEXTEST_JOBS={threads}"}
def read(path):
    with open(path, "rb") as stream: return stream.read()
def sha(path): return hashlib.sha256(read(path)).hexdigest()
def write_json(path, value):
    temporary = path + ".private"
    with open(temporary, "w", encoding="utf-8", newline="\n") as stream: json.dump(value, stream, sort_keys=True, separators=(",", ":")); stream.write("\n")
    os.chmod(temporary, 0o600); os.link(temporary, path); os.unlink(temporary)
def paths(name):
    stdout = "nextest-fast-list.json" if name == "nextest-fast-list" else "nextest-full-list.json" if name == "nextest-full-list" else f"preflight/{name}.stdout"
    return stdout, f"preflight/{name}.stderr"
def nextest_summary(text, label):
    values = re.findall(r"^Summary \[[^]]+\] ([0-9]+) tests? run: ([0-9]+) passed, ([0-9]+) skipped$", text, re.M)
    if len(values) != 1: raise RuntimeError(f"{label} missing or duplicate Nextest ending")
    total, passed, skipped = map(int, values[0]); failed = total - passed - skipped
    if failed != 0 or passed <= 0: raise RuntimeError(f"{label} is red or empty")
    return {"name":"nextest","exit_status":0,"passed":passed,"failed":failed,"skipped":skipped}
def components(name, stdout, stderr, exit_status):
    if not name.startswith("make-test-"): return []
    if exit_status != 0: raise RuntimeError(f"{name} is red")
    if re.search(r"S1Q-(?:COMPONENT|LANE)-END", stdout + stderr): raise RuntimeError(f"{name} contains obsolete marker-only evidence")
    result = [nextest_summary(stderr, name)]
    if name == "make-test-full":
        summaries = re.findall(r"^test result: (ok|FAILED)\. ([0-9]+) passed; ([0-9]+) failed; ([0-9]+) ignored; [0-9]+ measured; [0-9]+ filtered out; finished in .+$", stdout, re.M)
        if not summaries: raise RuntimeError("make-test-full missing doctest ending")
        passed = sum(int(value[1]) for value in summaries); failed = sum(int(value[2]) for value in summaries); skipped = sum(int(value[3]) for value in summaries)
        if any(value[0] != "ok" for value in summaries) or failed != 0: raise RuntimeError("make-test-full doctests are red")
        result.append({"name":"doctests","exit_status":0,"passed":passed,"failed":failed,"skipped":skipped})
        running = re.findall(r"^\s*Running .+rsi-model-control-validate(?:\s+--offline)?`?\s*$", stderr, re.M)
        provider_running = re.findall(r"^\s*Running .+rsi-provider-capability-validate(?:\s+--offline)?`?\s*$", stderr, re.M)
        if len(running) != 1: raise RuntimeError("make-test-full missing or duplicate model-control invocation ending")
        if len(provider_running) != 1: raise RuntimeError("make-test-full missing or duplicate provider-capability invocation ending")
        if not (stderr.find("Summary [") < stderr.find("rsi-model-control-validate") < stderr.find("rsi-provider-capability-validate")): raise RuntimeError("make-test-full component endings are reordered")
        result.append({"name":"model-control-validator","exit_status":0,"passed":1,"failed":0,"skipped":0})
        result.append({"name":"provider-capability-validator","exit_status":0,"passed":1,"failed":0,"skipped":0})
    return result

records=[]
for sequence, name in enumerate(names, 1):
    meta_path=os.path.join(root,".meta",f"{sequence}.json")
    if not os.path.exists(meta_path): break
    meta=json.loads(read(meta_path)); stdout_rel,stderr_rel=paths(name); stdout_path=os.path.join(root,stdout_rel); stderr_path=os.path.join(root,stderr_rel)
    stdout=read(stdout_path).decode(); stderr=read(stderr_path).decode(); normalized=components(name,stdout,stderr,meta["exit_status"])
    status_rel=f"preflight/{name}.status.json"; normalized_rel=f"preflight/{name}.components.json"
    write_json(os.path.join(root,status_rel),{"schema_version":1,**meta})
    write_json(os.path.join(root,normalized_rel),{"schema_version":1,"name":name,"components":normalized,"stdout_sha256":sha(stdout_path),"stderr_sha256":sha(stderr_path)})
    records.append({"name":name,"command":commands[name],"exit_status":meta["exit_status"],"stdout":stdout_rel,"stderr":stderr_rel,"status":status_rel,"normalized":normalized_rel,"stdout_sha256":sha(stdout_path),"stderr_sha256":sha(stderr_path),"status_sha256":sha(os.path.join(root,status_rel)),"normalized_sha256":sha(os.path.join(root,normalized_rel))})

def text(name):
    stdout,_=paths(name); return read(os.path.join(root,stdout)).decode()
if text("git-head") != head + "\n" or text("git-branch") != branch + "\n": raise RuntimeError("attested source identity output differs")
if any(text(name) != "" for name in ("git-status","git-diff-check","snapshots")): raise RuntimeError("attested source is dirty or has new snapshots")
metadata=json.loads(text("cargo-metadata")); target_path=os.path.realpath(metadata["target_directory"]); node=os.stat(target_path)
logical=int(text("nproc").strip()); uname=os.uname(); cpu_match=re.search(r"^Model name:\s*(.+)$",text("lscpu"),re.M)
host={"class":f"{uname.sysname}-{uname.machine}-{logical}cpu","os":uname.sysname,"kernel":uname.release,"architecture":uname.machine,"cpu_model":cpu_match.group(1).strip(),"logical_cpus":logical}
host["fingerprint_sha256"]=hashlib.sha256("|".join(str(host[key]) for key in ("class","os","kernel","architecture","cpu_model","logical_cpus")).encode()).hexdigest()
target={"canonical_path":target_path,"device":str(node.st_dev),"inode":str(node.st_ino)}; target["fingerprint_sha256"]=hashlib.sha256("|".join(str(target[key]) for key in ("canonical_path","device","inode")).encode()).hexdigest()
config_rel="preflight/nextest-config.toml"; config_bytes=read(os.path.join(root,config_rel)); config=tomllib.loads(config_bytes.decode())
profiles=config.get("profile",{})
def resolve(name, seen=()):
    if name in seen or name not in profiles: raise RuntimeError(f"invalid Nextest profile inheritance: {name}")
    own=dict(profiles[name]); parent=own.pop("inherits",None); return ({**resolve(parent,seen+(name,)),**own} if parent else own)
normalized_profiles={}
for name in ("rsid-fast","ci-full"):
    value=resolve(name)
    if value.get("retries") != 0 or value.get("overrides") not in (None,[]) or any("filter" in key or "quarantine" in key for key in value): raise RuntimeError(f"unsafe Nextest profile: {name}")
    normalized_profiles[name]={"retries":0,"filters":[],"quarantine":[]}
preflight={"schema_version":3,"origin":{"kind":"unverified-durable-input","acceptance_operation":None,"accepted":False},"threat_boundary":threat_statement,"source":{"head":head,"branch":branch,"clean_tree":True},"host":host,"toolchain":{"cargo":text("cargo-version").rstrip("\n"),"rustc":text("rustc-version").rstrip("\n"),"cargo_nextest":text("nextest-version").rstrip("\n"),"cargo_target_dir":target_path},"target":target,"nextest_config":{"source":".config/nextest.toml","path":config_rel,"sha256":hashlib.sha256(config_bytes).hexdigest(),"profiles":normalized_profiles},"retry_policy":{key:value["retries"] for key,value in normalized_profiles.items()},"filters":{key:value["filters"] for key,value in normalized_profiles.items()},"quarantine":[],"snapshot_candidates":text("snapshots").splitlines(),"commands":records}
write_json(os.path.join(root,"preflight.json"),preflight)
for current, directories, files in os.walk(root):
    os.chmod(current,0o700)
    for filename in files: os.chmod(os.path.join(current,filename),0o600)
PY
    then
        rm -rf -- "$staging"
        return 2
    fi
    rm -rf -- "$staging/.meta" "$staging/.isolated-nextest-config"
    [ "$(/usr/bin/git rev-parse --verify 'HEAD^{commit}')" = "$CAPTURE_GIT_HEAD" ] || die "refusing preflight publication: source HEAD changed"
    [ "$(/usr/bin/git symbolic-ref --short HEAD)" = "$CAPTURE_GIT_BRANCH" ] || die "refusing preflight publication: source branch changed"
    /usr/bin/git diff-index --quiet HEAD -- || die "refusing preflight publication: tracked source changed"
    [ -z "$(/usr/bin/git ls-files --others --exclude-standard)" ] || die "refusing preflight publication: untracked source changed"
    [ ! -e "$root_abs" ] && [ ! -L "$root_abs" ] || die "calibration root appeared concurrently"
    if ! /usr/bin/python3 - "$parent_abs" "${staging##*/}" "${root_abs##*/}" <<'PY'
import ctypes, errno, os, stat, sys
parent, source, destination = sys.argv[1:]
flags = os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW | os.O_CLOEXEC
parent_fd = os.open(parent, flags)
try:
    parent_before = os.fstat(parent_fd)
    if parent_before.st_uid != os.geteuid() or parent_before.st_gid != os.getegid() or stat.S_IMODE(parent_before.st_mode) != 0o700:
        raise RuntimeError("preflight output parent lost private custody")
    source_before = os.stat(source, dir_fd=parent_fd, follow_symlinks=False)
    if not stat.S_ISDIR(source_before.st_mode) or source_before.st_uid != os.geteuid() or source_before.st_gid != os.getegid() or stat.S_IMODE(source_before.st_mode) != 0o700:
        raise RuntimeError("preflight staging root lost private custody")
    try: os.stat(destination, dir_fd=parent_fd, follow_symlinks=False)
    except FileNotFoundError: pass
    else: raise RuntimeError("preflight destination appeared concurrently")
    libc = ctypes.CDLL(None, use_errno=True)
    result = libc.renameat2(parent_fd, source.encode(), parent_fd, destination.encode(), 1)
    if result != 0:
        code = ctypes.get_errno()
        raise RuntimeError(f"exclusive preflight publication failed: {os.strerror(code)}")
    destination_after = os.stat(destination, dir_fd=parent_fd, follow_symlinks=False)
    if (destination_after.st_dev, destination_after.st_ino) != (source_before.st_dev, source_before.st_ino):
        raise RuntimeError("published preflight identity differs")
    os.fsync(parent_fd)
    parent_after = os.stat(parent, follow_symlinks=False)
    if (parent_after.st_dev, parent_after.st_ino) != (parent_before.st_dev, parent_before.st_ino):
        raise RuntimeError("preflight output parent was replaced")
finally:
    os.close(parent_fd)
PY
    then
        [ ! -e "$staging" ] || rm -rf -- "$staging"
        return 2
    fi
    staging=""
    [ "$overall" -eq 0 ] || return "$overall"
}

attest_preflight() {
    reject_public_fixture_environment
    local -a probe=("$@"); local index=0
    while [ "$index" -lt "${#probe[@]}" ]; do
        case "${probe[index]}" in
            --calibration-root|--threads) index=$((index + 2)) ;;
            *) die "unknown attest-preflight argument: ${probe[index]}" ;;
        esac
    done
    validate_sterile_launch attest-preflight "$@"
    attest_preflight_impl public "$@"
}

generate_baseline_impl() {
    local generator_mode="$1" calibration_root="" out="" root_head output_relative
    shift
    while [ "$#" -gt 0 ]; do
        case "$1" in
            --calibration-root) [ "$#" -ge 2 ] || die "--calibration-root requires a value"; calibration_root="$2"; shift 2 ;;
            --out) [ "$#" -ge 2 ] || die "--out requires a value"; out="$2"; shift 2 ;;
            *) die "unknown generate-baseline argument: $1" ;;
        esac
    done
    [ -n "$calibration_root" ] && [ -n "$out" ] || die "generate-baseline requires --calibration-root and --out"
    need_command git; need_command python3
    initialize_repo_root; require_clean_tree; capture_source_state
    validate_output_path "$calibration_root"; validate_output_path "$out"
    case "$calibration_root" in "$OUTPUT_ROOT/z-baseline-calibration/"*) ;; *) die "calibration root must be below $OUTPUT_ROOT/z-baseline-calibration/" ;; esac
    case "$out" in "$calibration_root"/*) ;; *) die "baseline output must be below the calibration root" ;; esac
    root_head="${calibration_root##*/}"; [[ "$root_head" =~ ^[0-9a-f]{40,64}$ ]] || die "calibration root must be keyed by a lowercase source commit"
    [ "$root_head" = "$CAPTURE_GIT_HEAD" ] || die "calibration root source key differs from current HEAD"
    output_relative="${out#"$calibration_root"/}"
    case "/$output_relative/" in */../*|*/./*|*//*) die "baseline output has a noncanonical relative path" ;; esac
    /usr/bin/python3 - "$REPO_ROOT" "$calibration_root" "$output_relative" "$CAPTURE_GIT_HEAD" "$CAPTURE_GIT_BRANCH" "$generator_mode" <<'PY'
import atexit
import codecs
import datetime
import hashlib
import json
import math
import os
import pwd
import re
import secrets
import stat
import subprocess
import sys
import tomllib

repo_path, root_relative, output_relative, current_head, current_branch, generator_mode = sys.argv[1:]
require_internal_overrides = generator_mode == "private-self-test"
uid = os.geteuid()
gid = os.getegid()
DIR_FLAGS = os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW | os.O_CLOEXEC
FILE_FLAGS = os.O_RDONLY | os.O_NOFOLLOW | os.O_CLOEXEC
def configured_bound(name, default):
    if require_internal_overrides and os.environ.get(name):
        value = int(os.environ[name])
        if value <= 0:
            raise RuntimeError(f"invalid self-test bound: {name}")
        return value
    return default
MAX_PREFLIGHT_BYTES = configured_bound("S1Q_TEST_MAX_PREFLIGHT_BYTES", 4 * 1024 * 1024)
MAX_ENUM_BYTES = configured_bound("S1Q_TEST_MAX_ENUM_BYTES", 64 * 1024 * 1024)
MAX_CAPTURE_BYTES = configured_bound("S1Q_TEST_MAX_CAPTURE_BYTES", 64 * 1024 * 1024)
MAX_RAW_BYTES = configured_bound("S1Q_TEST_MAX_RAW_BYTES", 64 * 1024 * 1024)
MAX_AGGREGATE_BYTES = configured_bound("S1Q_TEST_MAX_AGGREGATE_BYTES", 1024 * 1024 * 1024)
MAX_OUTPUT_BYTES = configured_bound("S1Q_TEST_MAX_OUTPUT_BYTES", 128 * 1024 * 1024)
MAX_JSON_DEPTH = configured_bound("S1Q_TEST_MAX_JSON_DEPTH", 64)
MAX_SUITES = configured_bound("S1Q_TEST_MAX_SUITES", 512)
MAX_TESTS = configured_bound("S1Q_TEST_MAX_TESTS", 100000)
MAX_SAMPLES = configured_bound("S1Q_TEST_MAX_SAMPLES", 5)
MAX_IDENTITIES = configured_bound("S1Q_TEST_MAX_IDENTITIES", 100000)
MAX_STRING_BYTES = configured_bound("S1Q_TEST_MAX_STRING_BYTES", 1024 * 1024)
capture_names = {
    "store": "store.json",
    "scanner": "scanner.json",
    "fast": "fast.json",
    "sweep-1": "sweep-t1.json",
    "sweep-8": "sweep-t8.json",
    "sweep-16": "sweep-t16.json",
    "sweep-32": "sweep-t32.json",
}
fixed_inputs = ["preflight.json", "nextest-fast-list.json", "nextest-full-list.json", *capture_names.values()]
held_files = {}
held_directories = []
inode_paths = {}
aggregate_bytes = 0
inventory = {}
private_leaf = None
private_identity = None
published_identity = None
root_fd = parent_fd = repo_fd = None

def fail(message):
    raise RuntimeError(message)

def require(condition, message):
    if not condition:
        fail(message)

def exact_keys(value, keys, label):
    require(isinstance(value, dict) and set(value) == set(keys), f"invalid {label} schema")

def strict_int(value, label, minimum=0):
    require(isinstance(value, int) and not isinstance(value, bool) and value >= minimum, f"invalid {label}")
    return value

def bounded_string(value, label, allow_empty=False):
    require(isinstance(value, str) and (allow_empty or value), f"invalid {label}")
    require(len(value.encode("utf-8")) <= MAX_STRING_BYTES, f"{label} exceeds string bound")
    return value

def relative_path(value, label):
    bounded_string(value, label)
    require(not os.path.isabs(value), f"absolute {label} is forbidden")
    require(value == os.path.normpath(value) and value not in (".", "..") and not value.startswith("../"), f"escaping or noncanonical {label}")
    require("\x00" not in value, f"invalid {label}")
    return value

def require_private_directory(node, label):
    require(stat.S_ISDIR(node.st_mode) and node.st_uid == uid and node.st_gid == gid and stat.S_IMODE(node.st_mode) == 0o700, f"{label} must be current-user/current-group mode-0700")

def open_dir_path(base_fd, relative, label, private_components=True):
    relative_path(relative, label)
    current = os.dup(base_fd)
    try:
        for component in relative.split("/"):
            next_fd = os.open(component, DIR_FLAGS, dir_fd=current)
            os.close(current)
            current = next_fd
            node = os.fstat(current)
            if private_components:
                require_private_directory(node, label)
            else:
                require(stat.S_ISDIR(node.st_mode) and node.st_uid == uid and node.st_gid == gid, f"invalid ownership for {label}")
                require(stat.S_IMODE(node.st_mode) & 0o022 == 0, f"writable ancestor of {label} is forbidden")
        return current
    except Exception:
        os.close(current)
        raise

def stat_path_from(base_fd, relative):
    components = relative.split("/")
    directory = os.dup(base_fd)
    try:
        for component in components[:-1]:
            next_fd = os.open(component, DIR_FLAGS, dir_fd=directory)
            os.close(directory)
            directory = next_fd
        return os.stat(components[-1], dir_fd=directory, follow_symlinks=False)
    finally:
        os.close(directory)

def authenticate_directory(base_fd, relative, saved_fd, label, private_components=True):
    reopened = open_dir_path(base_fd, relative, label, private_components=private_components)
    try:
        saved = os.fstat(saved_fd)
        current = os.fstat(reopened)
        require_private_directory(saved, label)
        require_private_directory(current, label)
        saved_facts = (saved.st_dev, saved.st_ino, saved.st_uid, saved.st_gid, stat.S_IMODE(saved.st_mode), saved.st_nlink)
        current_facts = (current.st_dev, current.st_ino, current.st_uid, current.st_gid, stat.S_IMODE(current.st_mode), current.st_nlink)
        require(saved_facts == current_facts, f"{label} identity facts drifted")
    finally:
        os.close(reopened)

def file_record(relative, node, digest):
    return {
        "path": relative,
        "sha256": digest,
        "bytes": node.st_size,
        "device": node.st_dev,
        "inode": node.st_ino,
        "uid": node.st_uid,
        "gid": node.st_gid,
        "mode": format(stat.S_IMODE(node.st_mode), "04o"),
        "link_count": node.st_nlink,
    }

def open_evidence(relative, limit=MAX_RAW_BYTES):
    global aggregate_bytes
    relative_path(relative, "evidence path")
    if relative in held_files:
        return held_files[relative]
    components = relative.split("/")
    directory = os.dup(root_fd)
    try:
        for component in components[:-1]:
            next_fd = os.open(component, DIR_FLAGS, dir_fd=directory)
            os.close(directory)
            directory = next_fd
            dnode = os.fstat(directory)
            require_private_directory(dnode, f"evidence directory for {relative}")
        fd = os.open(components[-1], FILE_FLAGS, dir_fd=directory)
    finally:
        os.close(directory)
    node = os.fstat(fd)
    require(stat.S_ISREG(node.st_mode), f"evidence is not regular: {relative}")
    require(node.st_uid == uid and node.st_gid == gid and stat.S_IMODE(node.st_mode) == 0o600, f"evidence must be current-user/current-group mode-0600: {relative}")
    require(node.st_nlink == 1, f"hardlinked evidence is forbidden: {relative}")
    require(node.st_size <= limit, f"evidence exceeds byte bound: {relative}")
    aggregate_bytes += node.st_size
    require(aggregate_bytes <= MAX_AGGREGATE_BYTES, "aggregate evidence exceeds byte bound")
    identity = (node.st_dev, node.st_ino)
    require(identity not in inode_paths, f"aliased evidence files: {inode_paths.get(identity)} and {relative}")
    inode_paths[identity] = relative
    held_files[relative] = (fd, node)
    return fd, node

def canonical_file(relative, limit=MAX_RAW_BYTES):
    fd, _ = open_evidence(relative, limit)
    return f"/proc/{os.getpid()}/fd/{fd}"

def digest_path(path):
    digest = hashlib.sha256()
    with open(path, "rb") as stream:
        for chunk in iter(lambda: stream.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()

def read_bytes(relative, limit=MAX_RAW_BYTES):
    fd, before = open_evidence(relative, limit)
    os.lseek(fd, 0, os.SEEK_SET)
    chunks = []
    remaining = before.st_size
    digest = hashlib.sha256()
    while remaining:
        chunk = os.read(fd, min(1024 * 1024, remaining))
        require(chunk, f"truncated evidence: {relative}")
        chunks.append(chunk)
        digest.update(chunk)
        remaining -= len(chunk)
    require(os.read(fd, 1) == b"", f"evidence grew while reading: {relative}")
    after = os.fstat(fd)
    require((before.st_dev, before.st_ino, before.st_size, before.st_mtime_ns, before.st_ctime_ns, before.st_nlink) == (after.st_dev, after.st_ino, after.st_size, after.st_mtime_ns, after.st_ctime_ns, after.st_nlink), f"evidence changed while reading: {relative}")
    path_node = stat_path_from(root_fd, relative)
    require((after.st_dev, after.st_ino) == (path_node.st_dev, path_node.st_ino), f"evidence path was replaced: {relative}")
    inventory[relative] = file_record(relative, after, digest.hexdigest())
    return b"".join(chunks)

def json_depth(value, depth=0):
    require(depth <= MAX_JSON_DEPTH, "JSON depth exceeds bound")
    if isinstance(value, dict):
        for key, child in value.items():
            bounded_string(key, "JSON key")
            json_depth(child, depth + 1)
    elif isinstance(value, list):
        for child in value:
            json_depth(child, depth + 1)
    elif isinstance(value, str):
        bounded_string(value, "JSON string", allow_empty=True)

def read_json(relative, limit):
    try:
        value = json.loads(read_bytes(relative, limit).decode("utf-8"))
    except (UnicodeDecodeError, json.JSONDecodeError) as error:
        fail(f"malformed JSON: {relative}: {error}")
    json_depth(value)
    return value

def leaf_stat(directory_fd, leaf):
    return os.stat(leaf, dir_fd=directory_fd, follow_symlinks=False)

def leaf_identity(directory_fd, leaf):
    node = leaf_stat(directory_fd, leaf)
    return node, (node.st_dev, node.st_ino)

def source_state():
    def git(*arguments):
        return subprocess.run(["/usr/bin/git", "-C", repo_path, *arguments], check=True, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True).stdout.rstrip("\n")
    require(git("rev-parse", "HEAD") == current_head, "source HEAD changed")
    require(git("branch", "--show-current") == current_branch, "source branch changed")
    require(git("status", "--porcelain") == "", "source worktree changed")

def test_hook(stage):
    if not require_internal_overrides or os.environ.get("S1Q_TEST_GENERATE_HOOK_STAGE") != stage:
        return
    stage_fifo = os.environ.get("S1Q_TEST_STAGE_FIFO", "")
    continue_fifo = os.environ.get("S1Q_TEST_CONTINUE_FIFO", "")
    require(stage_fifo and continue_fifo, "incomplete generator test hook")
    with open(stage_fifo, "w", encoding="utf-8") as stream:
        stream.write(stage + "\n")
    with open(continue_fifo, "r", encoding="utf-8") as stream:
        require(stream.readline().rstrip("\n") == stage, "invalid generator hook continuation")

def authenticate_all():
    authenticate_directory(repo_fd, root_relative, root_fd, "calibration root", private_components=False)
    parent_relative = os.path.dirname(output_relative)
    if parent_relative:
        authenticate_directory(root_fd, parent_relative, parent_fd, "output parent")
    else:
        saved = os.fstat(parent_fd)
        current = os.fstat(root_fd)
        require((saved.st_dev, saved.st_ino, saved.st_uid, saved.st_gid, stat.S_IMODE(saved.st_mode), saved.st_nlink) == (current.st_dev, current.st_ino, current.st_uid, current.st_gid, stat.S_IMODE(current.st_mode), current.st_nlink), "output parent identity facts drifted")
    for relative, (fd, before) in held_files.items():
        after = os.fstat(fd)
        require((before.st_dev, before.st_ino, before.st_size, before.st_mtime_ns, before.st_ctime_ns, before.st_nlink) == (after.st_dev, after.st_ino, after.st_size, after.st_mtime_ns, after.st_ctime_ns, after.st_nlink), f"evidence drifted: {relative}")
        current = stat_path_from(root_fd, relative)
        require((after.st_dev, after.st_ino) == (current.st_dev, current.st_ino), f"evidence was replaced: {relative}")
    if "target" in globals():
        current_target = os.stat(target["canonical_path"], follow_symlinks=False)
        require((str(current_target.st_dev), str(current_target.st_ino)) == (target["device"], target["inode"]), "target identity drifted")

def cleanup_owned():
    if parent_fd is None or private_identity is None:
        return
    for leaf in (os.path.basename(output_relative), private_leaf):
        if not leaf:
            continue
        try:
            _, identity = leaf_identity(parent_fd, leaf)
        except FileNotFoundError:
            continue
        if identity == private_identity:
            os.unlink(leaf, dir_fd=parent_fd)

atexit.register(cleanup_owned)

require(os.path.realpath(repo_path) == repo_path and os.path.isabs(repo_path), "repository root is not canonical")
relative_path(root_relative, "calibration root")
relative_path(output_relative, "baseline output")
repo_fd = os.open(repo_path, DIR_FLAGS)
root_fd = open_dir_path(repo_fd, root_relative, "calibration root", private_components=False)
root_node = os.fstat(root_fd)
require_private_directory(root_node, "calibration root")
parent_relative = os.path.dirname(output_relative)
parent_fd = open_dir_path(root_fd, parent_relative, "output parent") if parent_relative else os.dup(root_fd)
output_leaf = os.path.basename(output_relative)
try:
    leaf_stat(parent_fd, output_leaf)
except FileNotFoundError:
    pass
else:
    fail("refusing to overwrite baseline output")
source_state()
test_hook("before-read")
authenticate_all()

def sha_text(value):
    return hashlib.sha256(value.encode()).hexdigest()

def number(value, label):
    require(isinstance(value, (int, float)) and not isinstance(value, bool) and math.isfinite(value) and value >= 0, f"invalid {label}")
    return value

def median(values):
    require(values, "median requires a nonempty array")
    ordered = sorted(values)
    middle = len(ordered) // 2
    return ordered[middle] if len(ordered) % 2 else (ordered[middle - 1] + ordered[middle]) / 2

def statistics(values):
    med = median(values)
    mad = median([abs(value - med) for value in values])
    return {"samples": values, "median": med, "mad": mad}

def normalized_binary_id(value):
    require(isinstance(value, str) and value and not any(ch.isspace() for ch in value), f"invalid Nextest binary id: {value!r}")
    return value

def parse_testcases(binary_id, testcases, runnable, ignored):
    binary_id = normalized_binary_id(binary_id)
    require(isinstance(testcases, dict), "Nextest suite has no exact testcase map")
    require(len(testcases) <= MAX_TESTS, "Nextest testcase count exceeds bound")
    items = testcases.items()
    for name, metadata in items:
        bounded_string(name, "Nextest testcase name")
        exact_keys(metadata, {"ignored", "filter-match"}, "Nextest testcase")
        require(isinstance(metadata["ignored"], bool), "Nextest ignored status must be boolean")
        exact_keys(metadata["filter-match"], {"status"}, "Nextest filter match")
        identity = f"{binary_id}::{name}"
        is_ignored = metadata["ignored"]
        require(metadata["filter-match"]["status"] == "matches", f"filter expansion or mismatch in enumeration: {identity}")
        (ignored if is_ignored else runnable).append(identity)

def enumeration(relative):
    document = read_json(relative, MAX_ENUM_BYTES)
    exact_keys(document, {"rust-suites"}, "Nextest enumeration")
    suites = document["rust-suites"]
    require(isinstance(suites, dict) and len(suites) <= MAX_SUITES, "invalid or excessive Nextest suite map")
    runnable, ignored = [], []
    for key, suite in suites.items():
        exact_keys(suite, {"binary-id", "testcases"}, "Nextest suite")
        require(suite["binary-id"] == key, "Nextest suite key/binary id mismatch")
        parse_testcases(suite["binary-id"], suite["testcases"], runnable, ignored)
    require(len(runnable) + len(ignored) <= MAX_IDENTITIES, "Nextest identity count exceeds bound")
    require(len(runnable) == len(set(runnable)) and len(ignored) == len(set(ignored)), f"duplicate Nextest identities in {relative}")
    require(not set(runnable) & set(ignored), f"Nextest runnable/ignored overlap in {relative}")
    runnable.sort(); ignored.sort()
    return {
        "runnable": runnable,
        "ignored": ignored,
        "runnable_count": len(runnable),
        "ignored_count": len(ignored),
        "runnable_sha256": sha_text("\n".join(runnable) + ("\n" if runnable else "")),
        "ignored_sha256": sha_text("\n".join(ignored) + ("\n" if ignored else "")),
    }

verified_evidence = {}

MAX_RAW_LINE_BYTES = 1024 * 1024
MAX_RAW_LINES = 4 * MAX_IDENTITIES + 4096

def scan_evidence(path, expected_hash, label, handler):
    relative = relative_path(path, f"{label} log path")
    require(relative not in fixed_inputs and not relative.startswith("preflight/") and re.fullmatch(r".+/(warmup|sample-[1-5])\.(stdout|stderr|time)\.log|.+/(warmup|sample-[1-5])\.status\.json", relative) is not None, f"invalid raw evidence label: {relative}")
    require(relative not in verified_evidence, f"reused evidence log: {relative}")
    require(re.fullmatch(r"[0-9a-f]{64}", expected_hash or "") is not None, f"invalid {label} log hash")
    fd, before = open_evidence(relative, MAX_RAW_BYTES)
    os.lseek(fd, 0, os.SEEK_SET)
    digest = hashlib.sha256()
    decoder = codecs.getincrementaldecoder("utf-8")("strict")
    pending = ""
    lines = 0
    try:
        remaining = before.st_size
        while remaining:
            chunk = os.read(fd, min(64 * 1024, remaining))
            require(chunk, f"truncated evidence: {relative}")
            digest.update(chunk)
            remaining -= len(chunk)
            pending += decoder.decode(chunk)
            while "\n" in pending:
                line, pending = pending.split("\n", 1)
                require(len(line.encode("utf-8")) <= MAX_RAW_LINE_BYTES, f"raw evidence line exceeds bound: {label}")
                lines += 1
                require(lines <= MAX_RAW_LINES, f"raw evidence line count exceeds bound: {label}")
                handler(line)
            require(len(pending.encode("utf-8")) <= MAX_RAW_LINE_BYTES, f"raw evidence line exceeds bound: {label}")
        pending += decoder.decode(b"", final=True)
    except UnicodeDecodeError:
        fail(f"non-UTF-8 raw evidence in {label}")
    if pending:
        lines += 1
        require(lines <= MAX_RAW_LINES, f"raw evidence line count exceeds bound: {label}")
        handler(pending)
    require(os.read(fd, 1) == b"", f"evidence grew while reading: {relative}")
    after = os.fstat(fd)
    require((before.st_dev, before.st_ino, before.st_size, before.st_mtime_ns, before.st_ctime_ns, before.st_nlink) == (after.st_dev, after.st_ino, after.st_size, after.st_mtime_ns, after.st_ctime_ns, after.st_nlink), f"evidence changed while reading: {relative}")
    path_node = stat_path_from(root_fd, relative)
    require((after.st_dev, after.st_ino) == (path_node.st_dev, path_node.st_ino), f"evidence path was replaced: {relative}")
    actual_hash = digest.hexdigest()
    require(actual_hash == expected_hash, f"{label} log hash mismatch")
    inventory[relative] = file_record(relative, after, actual_hash)
    verified_evidence[relative] = actual_hash

def validate_timing(timing, label):
    exact_keys(timing, {"backend", "wall_seconds", "user_seconds", "system_seconds", "max_rss_kib", "max_rss_disposition"}, f"{label} timing")
    for field in ("wall_seconds", "user_seconds", "system_seconds"):
        number(timing.get(field), f"{label} {field}")
    backend = timing.get("backend")
    require(backend in ("gnu_verbose", "bash_portable"), f"invalid {label} timing backend")
    rss = timing.get("max_rss_kib")
    disposition = timing.get("max_rss_disposition")
    if backend == "bash_portable":
        require(rss is None and disposition == "unavailable_portable_backend", f"portable {label} RSS must be null/unavailable")
    else:
        number(rss, f"{label} max RSS")
        require(disposition == "measured", f"GNU {label} RSS disposition differs")

def raw_state(probe, label):
    return {"probe": probe, "label": label, "passed": [], "failed": [], "ignored": [], "seen": set(), "summaries": [], "artifact": False, "red": False}

def scan_status_line(state, line):
    stripped = line.strip()
    if re.search(r"\b(RETRY|error)\b", line, re.IGNORECASE) or re.match(r"^\s*FAIL\b", line) or re.match(r"^test result: FAILED\.", line):
        state["red"] = True
    if re.search(r"\b(artifacts ready|Finished|Compiling|Building)\b", line):
        state["artifact"] = True
    if state["probe"] in ("store-fixture", "source-scanner"):
        match = re.fullmatch(r"test (.+) \.\.\. (ok|FAILED|ignored(?:,.*)?)", stripped)
        if match:
            identity, status_value = match.groups()
            status_name = "passed" if status_value == "ok" else "failed" if status_value == "FAILED" else "ignored"
            require(identity not in state["seen"], f"duplicate raw status identity: {state['label']}: {identity}")
            state["seen"].add(identity); state[status_name].append(identity)
        summary = re.fullmatch(r"test result: (ok|FAILED)\. ([0-9]+) passed; ([0-9]+) failed; ([0-9]+) ignored; [0-9]+ measured; [0-9]+ filtered out; finished in .+", stripped)
        if summary:
            state["summaries"].append(summary.groups())
    else:
        match = re.fullmatch(r"\s*(PASS|FAIL|SKIP|IGNORED)\s+\[[^]]*\]\s+(?:\((?:[0-9]+/[0-9]+|─+)\)\s+)?(\S+)\s+(.+)", line)
        if match:
            status_value, binary_id, name = match.groups()
            identity = f"{binary_id}::{name}"
            require(identity not in state["seen"], f"duplicate raw status identity: {state['label']}: {identity}")
            state["seen"].add(identity)
            state["passed" if status_value == "PASS" else "failed" if status_value == "FAIL" else "ignored"].append(identity)
        summary = re.fullmatch(r"Summary \[[^]]+\] ([0-9]+) tests? run: ([0-9]+) passed, ([0-9]+) skipped", stripped)
        if summary:
            state["summaries"].append(summary.groups())

def finish_raw_state(state):
    require(not state["red"], f"red raw evidence: {state['label']}")
    require(len(state["summaries"]) == 1, f"missing or duplicate test summary: {state['label']}")
    if state["probe"] in ("store-fixture", "source-scanner"):
        status_value, passed, failed, ignored = state["summaries"][0]
        require(status_value == "ok" and (len(state["passed"]), len(state["failed"]), len(state["ignored"])) == (int(passed), int(failed), int(ignored)), f"libtest summary differs from status records: {state['label']}")
    else:
        total, passed, ignored = map(int, state["summaries"][0])
        require(state["passed"] and not state["failed"] and (total, passed, ignored) == (len(state["seen"]), len(state["passed"]), len(state["ignored"])), f"Nextest summary differs from status records: {state['label']}")
    state["passed"].sort(); state["failed"].sort(); state["ignored"].sort()
    return state

def parse_raw_status(container, label, expected_label):
    values = []
    def status_line(line):
        require(not values, f"status evidence has multiple lines: {label}")
        values.append(line)
    scan_evidence(container["logs"]["status"], container["log_sha256"]["status"], f"{label} status", status_line)
    require(len(values) == 1, f"status evidence must be one canonical JSON line: {label}")
    try:
        record = json.loads(values[0])
    except json.JSONDecodeError as error:
        fail(f"malformed status evidence: {label}: {error}")
    exact_keys(record, {"schema_version", "label", "started_at", "finished_at", "exit_status"}, f"{label} status")
    require(record["schema_version"] == 1 and record["label"] == expected_label, f"status label differs: {label}")
    strict_int(record["exit_status"], f"{label} status exit")
    for field in ("started_at", "finished_at"):
        require(re.fullmatch(r"[0-9]{4}-[0-9]{2}-[0-9]{2}T[0-9]{2}:[0-9]{2}:[0-9]{2}Z", record[field] or "") is not None, f"noncanonical status timestamp: {label}")
        try: datetime.datetime.strptime(record[field], "%Y-%m-%dT%H:%M:%SZ")
        except ValueError: fail(f"invalid status timestamp: {label}")
    require(record["started_at"] <= record["finished_at"] and record["exit_status"] == container["exit_status"], f"status ordering or exit differs: {label}")
    return record

def verify_logs(container, label, probe, expected_status_label, parse_tests=True):
    logs = container.get("logs")
    hashes = container.get("log_sha256")
    exact_keys(logs, {"stdout", "stderr", "time", "status"}, f"{label} logs")
    exact_keys(hashes, {"stdout", "stderr", "time", "status"}, f"{label} log hashes")
    state = raw_state(probe, label)
    for field in ("stdout", "stderr"):
        scan_evidence(logs[field], hashes[field], f"{label} {field}", lambda line: scan_status_line(state, line))
    time_lines = []
    def timing_line(line):
        require(len(time_lines) < 32, f"timing evidence line count exceeds bound: {label}")
        time_lines.append(line)
    scan_evidence(logs["time"], hashes["time"], f"{label} time", timing_line)
    status_record = parse_raw_status(container, label, expected_status_label)
    timing = container["timing"]
    if timing["backend"] == "bash_portable":
        match = re.fullmatch(r"real ([0-9]+(?:\.[0-9]+)?)\nuser ([0-9]+(?:\.[0-9]+)?)\nsys ([0-9]+(?:\.[0-9]+)?)", "\n".join(time_lines))
        require(match is not None, f"malformed portable timing log: {label}")
        actual = [float(value) for value in match.groups()]
        claimed = [timing["wall_seconds"], timing["user_seconds"], timing["system_seconds"]]
        require(all(math.isclose(left, right, rel_tol=0, abs_tol=1e-9) for left, right in zip(actual, claimed)), f"timing claim differs from raw log: {label}")
    else:
        patterns = {
            "wall_seconds": r"^\s*Elapsed \(wall clock\) time \(h:mm:ss or m:ss\):\s*(\S+)\s*$",
            "user_seconds": r"^\s*User time \(seconds\):\s*([0-9]+(?:\.[0-9]+)?)\s*$",
            "system_seconds": r"^\s*System time \(seconds\):\s*([0-9]+(?:\.[0-9]+)?)\s*$",
            "max_rss_kib": r"^\s*Maximum resident set size \(kbytes\):\s*([0-9]+)\s*$",
        }
        values = {}
        for field, pattern in patterns.items():
            found = [match.group(1) for line in time_lines if (match := re.fullmatch(pattern, line))]
            require(len(found) == 1, f"malformed GNU timing field {field}: {label}")
            values[field] = found[0]
        parts = [float(part) for part in values["wall_seconds"].split(":")]
        wall = parts[0] if len(parts) == 1 else parts[0] * 60 + parts[1] if len(parts) == 2 else parts[0] * 3600 + parts[1] * 60 + parts[2]
        require(math.isclose(wall, timing["wall_seconds"], rel_tol=0, abs_tol=1e-9), f"GNU wall claim differs: {label}")
        require(math.isclose(float(values["user_seconds"]), timing["user_seconds"], rel_tol=0, abs_tol=1e-9), f"GNU user claim differs: {label}")
        require(math.isclose(float(values["system_seconds"]), timing["system_seconds"], rel_tol=0, abs_tol=1e-9), f"GNU system claim differs: {label}")
        require(int(values["max_rss_kib"]) == timing["max_rss_kib"], f"GNU RSS claim differs: {label}")
    return (finish_raw_state(state) if parse_tests else state), status_record

def expected_command(probe, threads):
    if probe == "store-fixture":
        return "cargo test -p rsid --lib store::tests::load_sessions_survives_legacy_comma_fraction_timestamp -- --exact --test-threads 1", "", "execution"
    if probe == "source-scanner":
        return "cargo test -p rsid --lib session::issue21_phase2_tests::p2_07_gate_permit_spine::exactly_one_production_writer_of_gate_closing_and_effect_permits -- --exact --test-threads 1", "", "execution"
    if probe == "nextest-fast":
        return f"cargo nextest run --profile rsid-fast -p rsid --lib --status-level all --final-status-level all -j {threads}", "rsid-fast", "artifact-build"
    if probe == "nextest-full":
        return f"cargo nextest run --profile ci-full --workspace --status-level all --final-status-level all -j {threads}", "ci-full", "artifact-build"
    fail(f"unsupported calibration probe: {probe}")

def expected_warmup(probe):
    if probe == "nextest-fast":
        return "cargo nextest run --profile rsid-fast -p rsid --lib --no-run"
    if probe == "nextest-full":
        return "cargo nextest run --profile ci-full --workspace --no-run"
    return None

for relative in fixed_inputs:
    limit = MAX_PREFLIGHT_BYTES if relative == "preflight.json" else MAX_ENUM_BYTES if relative.endswith("-list.json") else MAX_CAPTURE_BYTES
    read_bytes(relative, limit)
initial_input_inventory = {relative: inventory[relative]["sha256"] for relative in fixed_inputs}
preflight = read_json("preflight.json", MAX_PREFLIGHT_BYTES)
exact_keys(preflight, {"schema_version", "origin", "threat_boundary", "source", "host", "toolchain", "target", "nextest_config", "retry_policy", "filters", "quarantine", "snapshot_candidates", "commands"}, "preflight")
require(preflight["schema_version"] == 3, "preflight schema must be version 3")
exact_keys(preflight["origin"], {"kind", "acceptance_operation", "accepted"}, "preflight origin")
require(preflight["origin"] == {"kind":"unverified-durable-input","acceptance_operation":None,"accepted":False}, "diagnostic preflight cannot claim acceptance")
require(preflight["threat_boundary"] == "Acceptance proves exact bytes and uninterrupted in-process descriptor custody against hostile inherited environment and concurrent same-UID pathname or configuration mutation after a conforming sterile launch. It does not authenticate the launching principal, resist same-UID ptrace, process-memory, signal/control, namespace-control, or inherited-open-fd compromise, and does not authenticate alteration or forgery after the producer's final reproof and exit. It is not a signature or cryptographic origin attestation.", "preflight threat boundary differs")
source = preflight["source"]
exact_keys(source, {"head", "branch", "clean_tree"}, "source")
require(source == {"head": current_head, "branch": current_branch, "clean_tree": True}, "preflight source differs from current clean source")
host = preflight["host"]
toolchain = preflight["toolchain"]
target = preflight["target"]
exact_keys(host, {"class", "os", "kernel", "architecture", "cpu_model", "logical_cpus", "fingerprint_sha256"}, "host")
exact_keys(toolchain, {"cargo", "rustc", "cargo_nextest", "cargo_target_dir"}, "toolchain")
exact_keys(target, {"canonical_path", "device", "inode", "fingerprint_sha256"}, "target")
strict_int(host["logical_cpus"], "logical CPU count", 1)
host_fields = "|".join(str(host[field]) for field in ("class", "os", "kernel", "architecture", "cpu_model", "logical_cpus"))
require(host["fingerprint_sha256"] == sha_text(host_fields), "host fingerprint mismatch")
target_fields = "|".join(str(target[field]) for field in ("canonical_path", "device", "inode"))
require(os.path.isabs(target["canonical_path"]) and os.path.realpath(target["canonical_path"]) == target["canonical_path"], "target path is not canonical")
target_node = os.stat(target["canonical_path"], follow_symlinks=False)
require(stat.S_ISDIR(target_node.st_mode), "target is not a directory")
require((str(target_node.st_dev), str(target_node.st_ino)) == (target["device"], target["inode"]), "target identity differs from current target")
require(target["fingerprint_sha256"] == sha_text(target_fields), "target fingerprint mismatch")
require(toolchain["cargo_target_dir"] == target["canonical_path"], "toolchain target path differs")
require(preflight["retry_policy"] == {"rsid-fast": 0, "ci-full": 0}, "calibration requires retries=0")
require(preflight["filters"] == {"rsid-fast": [], "ci-full": []}, "filter expansion is forbidden")
require(preflight["quarantine"] == [] and preflight["snapshot_candidates"] == [], "quarantine or snapshot evidence is forbidden")

commands = preflight.get("commands")
require(isinstance(commands, list) and len(commands) == 17, "preflight commands must be the exact bounded array")
for item in commands:
    exact_keys(item, {"name", "command", "exit_status", "stdout", "stderr", "status", "normalized", "stdout_sha256", "stderr_sha256", "status_sha256", "normalized_sha256"}, "preflight command")
by_name = {item.get("name"): item for item in commands if isinstance(item, dict)}
require(len(by_name) == len(commands), "duplicate or malformed preflight command record")
expected_static = {
    "git-head": "git rev-parse HEAD",
    "git-branch": "git branch --show-current",
    "git-status": "git status --porcelain=v1",
    "git-diff-check": "git diff --check",
    "snapshots": "find . -type f -name '*.snap.new' -print",
    "nproc": "nproc",
    "uname": "uname -a",
    "lscpu": "lscpu",
    "cargo-version": "cargo --version",
    "rustc-version": "rustc --version",
    "nextest-version": "cargo nextest --version",
    "cargo-metadata": "cargo metadata --format-version 1 --no-deps",
    "nextest-config": "cargo nextest show-config test-groups",
    "nextest-fast-list": "cargo nextest list --profile rsid-fast -p rsid --lib --message-format json",
    "nextest-full-list": "cargo nextest list --profile ci-full --workspace --message-format json",
}
require(set(by_name) == set(expected_static) | {"make-test-fast", "make-test-full"}, "preflight command set differs")
require([item["name"] for item in commands] == [*expected_static, "make-test-fast", "make-test-full"], "preflight command order differs")
for name, command in expected_static.items():
    require(by_name[name].get("command") == command, f"preflight command differs: {name}")
previous_finished = None
status_paths = set()
status_by_name = {}
normalized_by_name = {}
for sequence, item in enumerate(commands, 1):
    strict_int(item["exit_status"], f"{item['name']} exit status")
    require(item["exit_status"] == 0, f"nonzero preflight command: {item['name']}")
    for field in ("stdout", "stderr"):
        relative = relative_path(item[field], f"preflight {item['name']} {field}")
        expected_path = ("nextest-fast-list.json" if field == "stdout" and item["name"] == "nextest-fast-list" else "nextest-full-list.json" if field == "stdout" and item["name"] == "nextest-full-list" else f"preflight/{item['name']}.{field}")
        require(relative == expected_path, f"wrong preflight evidence label: {item['name']} {field}")
        actual_hash = inventory[relative]["sha256"] if relative in inventory else hashlib.sha256(read_bytes(relative, MAX_RAW_BYTES)).hexdigest()
        require(actual_hash == item[field + "_sha256"] and re.fullmatch(r"[0-9a-f]{64}", item[field + "_sha256"] or ""), f"preflight hash mismatch: {item['name']} {field}")
        if relative not in ("nextest-fast-list.json", "nextest-full-list.json"):
            require(relative not in verified_evidence, f"duplicate preflight evidence reference: {relative}")
            verified_evidence[relative] = item[field + "_sha256"]
    status_relative = relative_path(item["status"], f"preflight {item['name']} status")
    require(status_relative == f"preflight/{item['name']}.status.json" and status_relative not in status_paths, f"wrong or duplicate preflight status label: {item['name']}")
    status_paths.add(status_relative)
    status_record = read_json(status_relative, MAX_RAW_BYTES)
    require(inventory[status_relative]["sha256"] == item["status_sha256"] and re.fullmatch(r"[0-9a-f]{64}", item["status_sha256"] or ""), f"preflight status hash mismatch: {item['name']}")
    exact_keys(status_record, {"schema_version", "sequence", "name", "started_at", "finished_at", "exit_status"}, f"preflight {item['name']} status")
    strict_int(status_record["sequence"], f"preflight {item['name']} sequence", 1)
    strict_int(status_record["exit_status"], f"preflight {item['name']} raw exit")
    require(status_record["schema_version"] == 1 and status_record["sequence"] == sequence and status_record["name"] == item["name"] and status_record["exit_status"] == item["exit_status"], f"preflight raw status differs: {item['name']}")
    for field in ("started_at", "finished_at"):
        require(re.fullmatch(r"[0-9]{4}-[0-9]{2}-[0-9]{2}T[0-9]{2}:[0-9]{2}:[0-9]{2}Z", status_record[field] or ""), f"preflight timestamp is not canonical: {item['name']}")
        try: datetime.datetime.strptime(status_record[field], "%Y-%m-%dT%H:%M:%SZ")
        except ValueError: fail(f"preflight timestamp is invalid: {item['name']}")
    require(status_record["started_at"] <= status_record["finished_at"] and (previous_finished is None or previous_finished <= status_record["started_at"]), f"preflight status ordering differs: {item['name']}")
    previous_finished = status_record["finished_at"]
    normalized_relative = relative_path(item["normalized"], f"preflight {item['name']} normalized components")
    require(normalized_relative == f"preflight/{item['name']}.components.json", f"wrong normalized component label: {item['name']}")
    normalized_record = read_json(normalized_relative, MAX_RAW_BYTES)
    require(inventory[normalized_relative]["sha256"] == item["normalized_sha256"] and re.fullmatch(r"[0-9a-f]{64}", item["normalized_sha256"] or ""), f"preflight normalized hash mismatch: {item['name']}")
    exact_keys(normalized_record, {"schema_version", "name", "components", "stdout_sha256", "stderr_sha256"}, f"preflight {item['name']} normalized components")
    require(normalized_record["schema_version"] == 1 and normalized_record["name"] == item["name"] and normalized_record["stdout_sha256"] == item["stdout_sha256"] and normalized_record["stderr_sha256"] == item["stderr_sha256"], f"normalized component provenance differs: {item['name']}")
    status_by_name[item["name"]] = status_record
    normalized_by_name[item["name"]] = normalized_record
for empty_name in ("git-status", "git-diff-check", "snapshots"):
    require(os.path.getsize(canonical_file(by_name[empty_name]["stdout"])) == 0, f"preflight {empty_name} must be empty")
require(by_name["nextest-fast-list"]["stdout"] == "nextest-fast-list.json", "fast enumeration path differs")
require(by_name["nextest-full-list"]["stdout"] == "nextest-full-list.json", "full enumeration path differs")

def command_output(name):
    return read_bytes(by_name[name]["stdout"], MAX_RAW_BYTES).decode("utf-8")

def parse_cargo_metadata(text, label):
    try: value = json.loads(text)
    except json.JSONDecodeError as error: fail(f"malformed Cargo metadata: {label}: {error}")
    require(isinstance(value, dict), f"Cargo metadata is not an object: {label}")
    target_directory = value.get("target_directory")
    workspace_root = value.get("workspace_root")
    require(isinstance(target_directory, str) and os.path.isabs(target_directory) and os.path.realpath(target_directory) == target_directory, f"Cargo metadata target is not canonical: {label}")
    require(isinstance(workspace_root, str) and os.path.realpath(workspace_root) == repo_path, f"Cargo metadata workspace differs: {label}")
    return target_directory

def parse_nextest_toml(data, label):
    try: value = tomllib.loads(data.decode("utf-8"))
    except (UnicodeDecodeError, tomllib.TOMLDecodeError) as error: fail(f"malformed Nextest TOML: {label}: {error}")
    profiles = value.get("profile")
    require(isinstance(profiles, dict), f"{label} has no Nextest profiles")
    def resolve(profile, seen=()):
        require(profile in profiles and profile not in seen, f"invalid Nextest inheritance: {label}/{profile}")
        own = dict(profiles[profile]); parent = own.pop("inherits", None)
        return {**resolve(parent, seen + (profile,)), **own} if parent else own
    normalized = {}
    for profile in ("rsid-fast", "ci-full"):
        config = resolve(profile)
        strict_int(config.get("retries"), f"{label} {profile} retries")
        require(config["retries"] == 0 and config.get("overrides") in (None, []) and not any("filter" in key or "quarantine" in key for key in config), f"{label} {profile} violates zero-retry/no-filter/no-quarantine contract")
        normalized[profile] = {"retries": 0, "filters": [], "quarantine": []}
    return normalized

def validate_lane(name, lane, expected_components):
    record = normalized_by_name[name]
    require([component.get("name") for component in record["components"] if isinstance(component, dict)] == expected_components, f"incomplete composite status: {name}")
    for component in record["components"]:
        exact_keys(component, {"name", "exit_status", "passed", "failed", "skipped"}, f"{name} component")
        for field in ("exit_status", "passed", "failed", "skipped"):
            strict_int(component[field], f"{name} component {field}")
        require(component["exit_status"] == 0 and component["failed"] == 0 and component["passed"] > 0, f"red or empty component: {name}/{component['name']}")
    stdout = command_output(name); stderr = read_bytes(by_name[name]["stderr"], MAX_RAW_BYTES).decode("utf-8")
    require(re.search(r"S1Q-(?:COMPONENT|LANE)-END", stdout + stderr) is None, f"obsolete marker-only lane evidence: {name}")
    summaries = re.findall(r"^Summary \[[^]]+\] ([0-9]+) tests? run: ([0-9]+) passed, ([0-9]+) skipped$", stderr, re.MULTILINE)
    require(len(summaries) == 1, f"missing or duplicate Nextest component ending: {name}")
    total, passed, skipped = map(int, summaries[0]); derived = [{"name": "nextest", "exit_status": 0, "passed": passed, "failed": total - passed - skipped, "skipped": skipped}]
    if name == "make-test-full":
        doctest = re.findall(r"^test result: (ok|FAILED)\. ([0-9]+) passed; ([0-9]+) failed; ([0-9]+) ignored; [0-9]+ measured; [0-9]+ filtered out; finished in .+$", stdout, re.MULTILINE)
        require(doctest and all(value[0] == "ok" for value in doctest), "missing or red doctest component ending")
        derived.append({"name": "doctests", "exit_status": 0, "passed": sum(int(value[1]) for value in doctest), "failed": sum(int(value[2]) for value in doctest), "skipped": sum(int(value[3]) for value in doctest)})
        validator = re.findall(r"^\s*Running .+rsi-model-control-validate(?:\s+--offline)?`?\s*$", stderr, re.MULTILINE)
        provider_validator = re.findall(r"^\s*Running .+rsi-provider-capability-validate(?:\s+--offline)?`?\s*$", stderr, re.MULTILINE)
        require(len(validator) == 1, "missing or duplicate model-control component ending")
        require(len(provider_validator) == 1, "missing or duplicate provider-capability component ending")
        require(stderr.find("Summary [") < stderr.find("rsi-model-control-validate") < stderr.find("rsi-provider-capability-validate"), "full lane component endings are reordered")
        derived.append({"name": "model-control-validator", "exit_status": 0, "passed": 1, "failed": 0, "skipped": 0})
        derived.append({"name": "provider-capability-validator", "exit_status": 0, "passed": 1, "failed": 0, "skipped": 0})
    require(derived == record["components"], f"normalized components differ from complete raw lane bytes: {name}")

validate_lane("make-test-fast", "fast", ["nextest"])
validate_lane("make-test-full", "full", ["nextest", "doctests", "model-control-validator", "provider-capability-validator"])
recorded_target = parse_cargo_metadata(command_output("cargo-metadata"), "recorded")
show_config_stdout = command_output("nextest-config")
require(show_config_stdout.strip() and not show_config_stdout.lstrip().startswith("{"), "recorded show-config output is synthetic or empty")
nextest_config = preflight["nextest_config"]
exact_keys(nextest_config, {"source", "path", "sha256", "profiles"}, "Nextest config attestation")
require(nextest_config["source"] == ".config/nextest.toml" and nextest_config["path"] == "preflight/nextest-config.toml", "Nextest config source differs")
recorded_config_bytes = read_bytes(nextest_config["path"], MAX_RAW_BYTES)
require(inventory[nextest_config["path"]]["sha256"] == nextest_config["sha256"], "Nextest config snapshot hash differs")
recorded_config = parse_nextest_toml(recorded_config_bytes, "recorded")
require(nextest_config["profiles"] == recorded_config, "Nextest normalized profile claims differ")
require(recorded_target == target["canonical_path"], "recorded Cargo metadata target differs")
require(preflight["retry_policy"] == {profile: recorded_config[profile]["retries"] for profile in recorded_config}, "preflight retry claims differ from recorded config")
require(preflight["filters"] == {profile: recorded_config[profile]["filters"] for profile in recorded_config}, "preflight filter claims differ from recorded config")
require(preflight["quarantine"] == [], "preflight quarantine claim differs from recorded config")
current_config_path = os.path.join(repo_path, ".config", "nextest.toml")

def live_facts():
    if require_internal_overrides and os.environ.get("S1Q_TEST_CURRENT_FACTS_JSON"):
        value = json.loads(os.environ["S1Q_TEST_CURRENT_FACTS_JSON"])
        exact_keys(value, {"host", "toolchain", "stdout"}, "current-fact seam")
        return value
    def output(*arguments, env=None):
        return subprocess.run(arguments, check=True, cwd=repo_path, env=env, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True).stdout
    account_home = pwd.getpwuid(uid).pw_dir
    cargo_path = os.path.join(account_home, ".cargo", "bin", "cargo")
    rustc_path = os.path.join(account_home, ".cargo", "bin", "rustc")
    uname_stdout = output("/usr/bin/uname", "-a")
    lscpu_stdout = output("/usr/bin/lscpu")
    nproc_stdout = output("/usr/bin/nproc")
    cargo_stdout = output(cargo_path, "--version")
    rustc_stdout = output(rustc_path, "--version")
    nextest_stdout = output(cargo_path, "nextest", "--version")
    metadata_stdout = output(cargo_path, "metadata", "--format-version", "1", "--no-deps")
    with tempfile.TemporaryDirectory(prefix="s1q-nextest-config-") as isolated_config:
        isolated_environment = os.environ.copy()
        isolated_environment.update({"XDG_CONFIG_HOME": isolated_config, "NEXTEST_CONFIG_FILE": current_config_path})
        config_stdout = output(cargo_path, "nextest", "show-config", "test-groups", env=isolated_environment)
    uname = os.uname()
    cpu_match = re.search(r"^Model name:\s*(.+)$", lscpu_stdout, re.MULTILINE)
    require(cpu_match is not None, "cannot derive current CPU model")
    logical = int(nproc_stdout.strip())
    current_host = {"class": f"{uname.sysname}-{uname.machine}-{logical}cpu", "os": uname.sysname, "kernel": uname.release, "architecture": uname.machine, "cpu_model": cpu_match.group(1).strip(), "logical_cpus": logical}
    current_host["fingerprint_sha256"] = sha_text("|".join(str(current_host[field]) for field in ("class", "os", "kernel", "architecture", "cpu_model", "logical_cpus")))
    current_target = parse_cargo_metadata(metadata_stdout, "current")
    return {"host": current_host, "toolchain": {"cargo": cargo_stdout.rstrip("\n"), "rustc": rustc_stdout.rstrip("\n"), "cargo_nextest": nextest_stdout.rstrip("\n"), "cargo_target_dir": current_target}, "stdout": {"nproc": nproc_stdout, "uname": uname_stdout, "lscpu": lscpu_stdout, "cargo-version": cargo_stdout, "rustc-version": rustc_stdout, "nextest-version": nextest_stdout, "cargo-metadata": metadata_stdout, "nextest-config": config_stdout}}

facts = live_facts()
exact_keys(facts["host"], {"class", "os", "kernel", "architecture", "cpu_model", "logical_cpus", "fingerprint_sha256"}, "current host facts")
exact_keys(facts["toolchain"], {"cargo", "rustc", "cargo_nextest", "cargo_target_dir"}, "current toolchain facts")
exact_keys(facts["stdout"], {"nproc", "uname", "lscpu", "cargo-version", "rustc-version", "nextest-version", "cargo-metadata", "nextest-config"}, "current fact stdout")
require(facts["host"] == host, "preflight host differs from current canonical facts")
require(facts["toolchain"] == toolchain, "preflight toolchain differs from current canonical facts")
for name in ("nproc", "uname", "lscpu", "cargo-version", "rustc-version", "nextest-version"):
    require(command_output(name) == facts["stdout"][name], f"preflight stdout differs from current command: {name}")
require(parse_cargo_metadata(facts["stdout"]["cargo-metadata"], "current facts") == recorded_target, "recorded/current Cargo metadata targets differ")
if require_internal_overrides:
    current_config_bytes = recorded_config_bytes
else:
    current_config_node = os.lstat(current_config_path)
    require(stat.S_ISREG(current_config_node.st_mode) and not stat.S_ISLNK(current_config_node.st_mode), "current Nextest config is not an authenticated regular file")
    with open(current_config_path, "rb") as current_config_stream: current_config_bytes = current_config_stream.read(MAX_RAW_BYTES + 1)
require(len(current_config_bytes) <= MAX_RAW_BYTES and current_config_bytes == recorded_config_bytes and parse_nextest_toml(current_config_bytes, "current") == recorded_config, "recorded/current Nextest config differs")
require(command_output("cargo-metadata") == facts["stdout"]["cargo-metadata"], "recorded/current Cargo metadata stdout differs")
require(command_output("nextest-config") == facts["stdout"]["nextest-config"], "recorded/current Nextest config stdout differs")
require(command_output("git-head") == current_head + "\n", "git-head stdout differs")
require(command_output("git-branch") == current_branch + "\n", "git-branch stdout differs")
for empty_name in ("git-status", "git-diff-check", "snapshots"):
    require(command_output(empty_name) == "", f"preflight {empty_name} must be empty")
for current_directory, directory_names, file_names in os.walk(repo_path, followlinks=False):
    directory_names[:] = [name for name in directory_names if name != ".git"]
    require(not any(name.endswith(".snap.new") for name in file_names), "current source contains a snapshot candidate")
commands = sorted(commands, key=lambda item: item["name"])

fast_enum = enumeration("nextest-fast-list.json")
full_enum = enumeration("nextest-full-list.json")
require(set(fast_enum["runnable"]).issubset(full_enum["runnable"]), "fast runnable set is not a subset of full")
require(set(fast_enum["ignored"]).issubset(full_enum["ignored"]), "fast ignored set is not a subset of full")

captures = {}
for logical_name, relative in capture_names.items():
    capture = read_json(relative, MAX_CAPTURE_BYTES)
    exact_keys(capture, {"schema_version", "capture", "samples"}, f"{logical_name} capture")
    require(capture["schema_version"] == 2, f"{logical_name} capture schema differs")
    metadata = capture["capture"]
    exact_keys(metadata, {"captured_at", "label", "probe", "profile", "source", "host", "toolchain", "target", "requested", "workspace", "warmup", "execution"}, f"{logical_name} metadata")
    require(re.fullmatch(r"[0-9]{4}-[0-9]{2}-[0-9]{2}T[0-9]{2}:[0-9]{2}:[0-9]{2}Z", metadata["captured_at"] or "") is not None, f"{logical_name} timestamp is not canonical UTC")
    try:
        datetime.datetime.strptime(metadata["captured_at"], "%Y-%m-%dT%H:%M:%SZ")
    except ValueError:
        fail(f"{logical_name} timestamp is invalid")
    expected_label = "z-baseline-store" if logical_name == "store" else "z-baseline-scanner" if logical_name == "scanner" else "z-baseline-fast" if logical_name == "fast" else f"z-baseline-sweep-t{logical_name.split('-')[1]}"
    require(metadata["label"] == expected_label, f"{logical_name} label differs")
    require(metadata.get("source") == source and metadata.get("host") == host and metadata.get("toolchain") == toolchain and metadata.get("target") == target, f"{logical_name} provenance mismatch")
    probe = "store-fixture" if logical_name == "store" else "source-scanner" if logical_name == "scanner" else "nextest-fast" if logical_name == "fast" else "nextest-full"
    threads = 1 if logical_name in ("store", "scanner") else int(logical_name.split("-")[1]) if logical_name.startswith("sweep-") else metadata.get("requested", {}).get("resolved_threads")
    repeat = 5 if logical_name in ("store", "scanner") else 3
    require(metadata.get("probe") == probe, f"{logical_name} probe differs")
    requested = metadata.get("requested", {})
    exact_keys(requested, {"repeat", "threads", "resolved_threads", "stop_on_first_red"}, f"{logical_name} requested")
    strict_int(requested["repeat"], f"{logical_name} repeat", 1)
    strict_int(requested["threads"], f"{logical_name} requested threads", 1)
    strict_int(requested["resolved_threads"], f"{logical_name} resolved threads", 1)
    require(requested["repeat"] == repeat and requested["threads"] == requested["resolved_threads"] == threads and requested["stop_on_first_red"] is True, f"{logical_name} repeat/thread/stop contract differs")
    expected_workspace = relative_path(metadata["workspace"], f"{logical_name} workspace")
    command, profile, warmup_kind = expected_command(probe, threads)
    exact_keys(metadata["execution"], {"command", "profile", "test_identity", "identity_mode"}, f"{logical_name} execution metadata")
    require(metadata["execution"]["identity_mode"] in ("exact", "descriptive"), f"{logical_name} identity mode differs")
    require(isinstance(metadata["execution"]["test_identity"], list) and 0 < len(metadata["execution"]["test_identity"]) <= MAX_IDENTITIES, f"{logical_name} declared identity count differs")
    for identity in metadata["execution"]["test_identity"]:
        bounded_string(identity, f"{logical_name} declared identity")
    require(len(metadata["execution"]["test_identity"]) == len(set(metadata["execution"]["test_identity"])), f"{logical_name} duplicate declared identity")
    require(metadata.get("profile") == profile and metadata.get("execution", {}).get("profile") == profile, f"{logical_name} profile differs")
    require(metadata.get("execution", {}).get("command") == command, f"{logical_name} command differs")
    warmup = metadata.get("warmup", {})
    exact_keys(warmup, {"kind", "command", "exit_status", "timing", "logs", "log_sha256"}, f"{logical_name} warmup")
    expected_warmup_command = expected_warmup(probe) or command
    strict_int(warmup["exit_status"], f"{logical_name} warmup exit status")
    require(warmup["kind"] == warmup_kind and warmup["command"] == expected_warmup_command and warmup["exit_status"] == 0, f"{logical_name} warmup differs or is red")
    validate_timing(warmup["timing"], f"{logical_name} warmup")
    require(all(path.startswith(expected_workspace + "/logs/") for path in warmup["logs"].values()), f"{logical_name} warmup logs escape workspace")
    warmup_state, warmup_status_record = verify_logs(warmup, f"{logical_name} warmup", probe, "warmup", parse_tests=not bool(profile))
    if not profile:
        require(warmup_state["passed"] == metadata["execution"]["test_identity"] and not warmup_state["failed"] and not warmup_state["ignored"], f"warmup identity differs: {logical_name}")
    else:
        require(not warmup_state["red"] and warmup_state["artifact"], f"unrelated artifact warmup output: {logical_name}")
    samples = capture.get("samples")
    require(isinstance(samples, list) and len(samples) == repeat and len(samples) <= MAX_SAMPLES, f"{logical_name} capture is partial or excessive")
    require(sorted(sample.get("index") for sample in samples) == list(range(1, repeat + 1)), f"{logical_name} sample indexes differ or duplicate")
    enum = fast_enum if probe == "nextest-fast" else full_enum if probe == "nextest-full" else None
    for sample in samples:
        exact_keys(sample, {"index", "execution", "observed"}, f"{logical_name} sample")
        execution = sample.get("execution", {}); observed = sample.get("observed", {})
        exact_keys(execution, {"command", "exit_status", "evidence_valid", "timing", "logs", "log_sha256"}, f"{logical_name} sample execution")
        exact_keys(observed, {"passed_lines", "failed_lines", "ignored_lines", "failure_names", "executed_test_names", "executed_runnable_test_names", "ignored_test_names", "identity_proof"}, f"{logical_name} observations")
        strict_int(sample["index"], f"{logical_name} sample index", 1)
        strict_int(execution["exit_status"], f"{logical_name} sample exit status")
        for count_name in ("passed_lines", "failed_lines", "ignored_lines"):
            strict_int(observed[count_name], f"{logical_name} {count_name}")
        require(execution["command"] == command and execution["exit_status"] == 0 and execution["evidence_valid"] is True, f"{logical_name} sample is red or incompatible")
        validate_timing(execution["timing"], f"{logical_name} sample {sample['index']}")
        require(all(path.startswith(expected_workspace + "/logs/") for path in execution["logs"].values()), f"{logical_name} sample logs escape workspace")
        sample_state, sample_status_record = verify_logs(execution, f"{logical_name} sample {sample['index']}", probe, f"sample-{sample['index']}")
        require(warmup_status_record["finished_at"] <= sample_status_record["started_at"], f"raw invocation ordering differs: {logical_name}")
        warmup_status_record = sample_status_record
        passed, failed, ignored_names = sample_state["passed"], sample_state["failed"], sample_state["ignored"]
        executed = sorted(passed + failed + ignored_names)
        runnable_names = sorted(passed + failed)
        require(observed["passed_lines"] == len(passed) and observed["failed_lines"] == len(failed) and observed["ignored_lines"] == len(ignored_names), f"{logical_name} raw counts differ")
        require(observed["failure_names"] == failed and observed["executed_test_names"] == executed and observed["executed_runnable_test_names"] == runnable_names and observed["ignored_test_names"] == ignored_names, f"{logical_name} raw identity union differs")
        proof = observed["identity_proof"]
        exact_keys(proof, {"mode", "declared", "executed", "verified"}, f"{logical_name} identity proof")
        expected_mode = metadata["execution"]["identity_mode"]
        verified = bool(metadata["execution"]["test_identity"]) and sorted(metadata["execution"]["test_identity"]) == executed if expected_mode == "exact" else True
        require(proof == {"mode": expected_mode, "declared": metadata["execution"]["test_identity"], "executed": executed, "verified": verified} and verified, f"{logical_name} recomputed identity proof differs")
        require(not failed, f"{logical_name} sample failure evidence")
        if enum is not None:
            require(runnable_names == enum["runnable"], f"{logical_name} runnable identities differ from enumeration")
            require(ignored_names == enum["ignored"], f"{logical_name} ignored identities differ from enumeration")
            require(len(passed) == enum["runnable_count"] and len(ignored_names) == enum["ignored_count"], f"{logical_name} aggregate counts differ from enumeration")
    require(metadata["captured_at"] == warmup_status_record["finished_at"], f"capture timestamp differs from final raw status: {logical_name}")
    captures[logical_name] = capture

candidates = sorted({1, host["logical_cpus"] // 4, host["logical_cpus"] // 2, host["logical_cpus"]})
require(candidates == [1, 8, 16, 32], "thread candidate set differs from derived 1/quarter/half/all contract")
candidate_statistics = {}
for threads in candidates:
    sample_timings = [sample["execution"]["timing"] for sample in captures[f"sweep-{threads}"]["samples"]]
    metrics = {}
    for field in ("wall_seconds", "user_seconds", "system_seconds"):
        metrics[field] = statistics([timing[field] for timing in sample_timings])
    rss = [timing["max_rss_kib"] for timing in sample_timings]
    require(all(value is None for value in rss) or all(value is not None for value in rss), f"mixed RSS availability for thread candidate {threads}")
    metrics["max_rss_kib"] = {"samples": rss, "median": None, "mad": None, "disposition": "unavailable_portable_backend"} if all(value is None for value in rss) else {**statistics(rss), "disposition": "measured"}
    warmup = captures[f"sweep-{threads}"]["capture"]["warmup"]
    metrics["warmup"] = {"kind": warmup["kind"], "command": warmup["command"], "exit_status": warmup["exit_status"], "timing": warmup["timing"]}
    candidate_statistics[str(threads)] = metrics
fastest = min(candidates, key=lambda value: (candidate_statistics[str(value)]["wall_seconds"]["median"], value))
fastest_wall = candidate_statistics[str(fastest)]["wall_seconds"]
frontier = [value for value in candidates if candidate_statistics[str(value)]["wall_seconds"]["median"] <= fastest_wall["median"] + fastest_wall["mad"]]
selected = min(frontier)
require(captures["fast"]["capture"]["requested"]["resolved_threads"] == selected, "fast capture threads differ from selected count")
require(by_name.get("make-test-fast", {}).get("command") == f"make test-fast NEXTEST_JOBS={selected}", "fast composite preflight command differs")
require(by_name.get("make-test-full", {}).get("command") == f"make test-full NEXTEST_JOBS={selected}", "full composite preflight command differs")

def capture_stats(capture):
    result = {}
    timings = [sample["execution"]["timing"] for sample in capture["samples"]]
    for field in ("wall_seconds", "user_seconds", "system_seconds"):
        result[field] = statistics([timing[field] for timing in timings])
    rss = [timing["max_rss_kib"] for timing in timings]
    require(all(value is None for value in rss) or all(value is not None for value in rss), f"mixed RSS availability for {capture['capture']['probe']}")
    result["max_rss_kib"] = {"samples": rss, "median": None, "mad": None, "disposition": "unavailable_portable_backend"} if all(value is None for value in rss) else {**statistics(rss), "disposition": "measured"}
    return result

def identity_digest(values):
    return sha_text("\n".join(values) + ("\n" if values else ""))

def capture_summary(capture):
    metadata = capture["capture"]
    warmup = metadata["warmup"]
    summary = capture_stats(capture)
    summary.update({
        "probe": metadata["probe"],
        "profile": metadata["profile"],
        "command": metadata["execution"]["command"],
        "test_identity": metadata["execution"]["test_identity"],
        "repeat": metadata["requested"]["repeat"],
        "resolved_threads": metadata["requested"]["resolved_threads"],
        "warmup": {"kind": warmup["kind"], "command": warmup["command"], "exit_status": warmup["exit_status"], "timing": warmup["timing"]},
        "sample_signatures": [{
            "index": sample["index"],
            "passed": sample["observed"]["passed_lines"],
            "failed": sample["observed"]["failed_lines"],
            "ignored": sample["observed"]["ignored_lines"],
            "runnable_identity_sha256": identity_digest(sample["observed"].get("executed_runnable_test_names", [])),
            "ignored_identity_sha256": identity_digest(sample["observed"].get("ignored_test_names", [])),
        } for sample in sorted(capture["samples"], key=lambda item: item["index"])],
    })
    return summary

selected_full = captures[f"sweep-{selected}"]
lane_captures = {"nextest-fast": captures["fast"], "nextest-full": selected_full}
budgets = []
for probe in ("nextest-fast", "nextest-full"):
    capture = lane_captures[probe]
    wall = capture_stats(capture)["wall_seconds"]
    signatures = [{
        "passed_lines": sample["observed"]["passed_lines"],
        "failed_lines": sample["observed"]["failed_lines"],
        "ignored_lines": sample["observed"]["ignored_lines"],
        "failure_names": sample["observed"]["failure_names"],
        "identity_proof": sample["observed"]["identity_proof"],
    } for sample in sorted(capture["samples"], key=lambda item: item["index"])]
    budgets.append({
        "probe": probe,
        "host_class": host["class"],
        "expected_repeat": 3,
        "median_wall_seconds": wall["median"],
        "mad_wall_seconds": wall["mad"],
        "limit_wall_seconds": wall["median"] + 3 * wall["mad"],
        "command": capture["capture"]["execution"]["command"],
        "test_identity": capture["capture"]["execution"]["test_identity"],
        "resolved_threads": selected,
        "observed_samples": signatures,
    })

full_minus_fast = sorted(set(full_enum["runnable"]) - set(fast_enum["runnable"]))
full_minus_fast_ignored = sorted(set(full_enum["ignored"]) - set(fast_enum["ignored"]))
input_inventory = {relative: inventory[relative]["sha256"] for relative in fixed_inputs}
require(input_inventory == initial_input_inventory, "calibration input changed during generation")
for relative, expected_hash in verified_evidence.items():
    require(inventory[relative]["sha256"] == expected_hash, f"raw evidence changed during generation: {relative}")
result = {
    "schema_version": 2,
    "origin": {"kind":"unverified-durable-input","acceptance_operation":None,"accepted":False},
    "threat_boundary": "Acceptance proves exact bytes and uninterrupted in-process descriptor custody against hostile inherited environment and concurrent same-UID pathname or configuration mutation after a conforming sterile launch. It does not authenticate the launching principal, resist same-UID ptrace, process-memory, signal/control, namespace-control, or inherited-open-fd compromise, and does not authenticate alteration or forgery after the producer's final reproof and exit. It is not a signature or cryptographic origin attestation.",
    "formula_version": "median-mad-v1",
    "source": source,
    "host": host,
    "toolchain": toolchain,
    "target": target,
    "lanes": {
        "nextest-fast": {"profile": "rsid-fast", "command": captures["fast"]["capture"]["execution"]["command"], "warmup_command": expected_warmup("nextest-fast")},
        "nextest-full": {"profile": "ci-full", "command": selected_full["capture"]["execution"]["command"], "warmup_command": expected_warmup("nextest-full")},
        "composite_preflight": {"fast": by_name["make-test-fast"], "full": by_name["make-test-full"]},
    },
    "enumeration": {"fast": fast_enum, "full": full_enum},
    "scope_difference": {
        "label": "full-minus-fast scope difference",
        "identities": full_minus_fast,
        "count": len(full_minus_fast),
        "sha256": sha_text("\n".join(full_minus_fast) + ("\n" if full_minus_fast else "")),
        "ignored_identities": full_minus_fast_ignored,
        "ignored_count": len(full_minus_fast_ignored),
        "ignored_sha256": sha_text("\n".join(full_minus_fast_ignored) + ("\n" if full_minus_fast_ignored else "")),
        "fast_exclusions": [],
        "full_exclusions": [],
    },
    "measurements": {
        "store-fixture": capture_summary(captures["store"]),
        "source-scanner": capture_summary(captures["scanner"]),
        "nextest-fast": capture_summary(captures["fast"]),
        "nextest-full": capture_summary(selected_full),
        "thread_candidates": candidate_statistics,
    },
    "thread_selection": {
        "logical_cpus": host["logical_cpus"],
        "candidates": candidates,
        "fastest_candidate": fastest,
        "fastest_median_wall_seconds": fastest_wall["median"],
        "fastest_mad_wall_seconds": fastest_wall["mad"],
        "eligible_frontier": frontier,
        "selected_threads": selected,
        "rule": "smallest candidate with median <= fastest median + fastest MAD",
    },
    "budgets": budgets,
    "provenance": {
        "inputs_sha256": input_inventory,
        "preflight_commands": commands,
        "inventory": [inventory[path] for path in sorted(inventory)],
        "inventory_total_bytes": sum(record["bytes"] for record in inventory.values()),
        "bounds": {
            "preflight_bytes": MAX_PREFLIGHT_BYTES,
            "enumeration_bytes": MAX_ENUM_BYTES,
            "capture_bytes": MAX_CAPTURE_BYTES,
            "raw_log_bytes": MAX_RAW_BYTES,
            "aggregate_bytes": MAX_AGGREGATE_BYTES,
            "output_bytes": MAX_OUTPUT_BYTES,
            "json_depth": MAX_JSON_DEPTH,
            "suites": MAX_SUITES,
            "tests": MAX_TESTS,
            "samples": MAX_SAMPLES,
            "identities": MAX_IDENTITIES,
            "string_bytes": MAX_STRING_BYTES,
        },
    },
}
output_bytes = (json.dumps(result, sort_keys=True, indent=2, ensure_ascii=True, allow_nan=False) + "\n").encode("ascii")
require(len(output_bytes) <= MAX_OUTPUT_BYTES, "generated baseline exceeds output byte bound")
authenticate_all()
source_state()
for _ in range(128):
    candidate = f".baseline-json.{secrets.token_hex(16)}"
    try:
        private_fd = os.open(candidate, os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW | os.O_CLOEXEC, 0o600, dir_fd=parent_fd)
        private_leaf = candidate
        break
    except FileExistsError:
        continue
else:
    fail("cannot allocate private baseline inode")
offset = 0
while offset < len(output_bytes):
    offset += os.write(private_fd, output_bytes[offset:])
os.fsync(private_fd)
private_node = os.fstat(private_fd)
private_identity = (private_node.st_dev, private_node.st_ino)
require(stat.S_ISREG(private_node.st_mode) and private_node.st_uid == uid and private_node.st_gid == gid and stat.S_IMODE(private_node.st_mode) == 0o600 and private_node.st_nlink == 1, "invalid private baseline inode")
test_hook("before-publish")
authenticate_all()
source_state()
try:
    os.link(private_leaf, output_leaf, src_dir_fd=parent_fd, dst_dir_fd=parent_fd, follow_symlinks=False)
except FileExistsError:
    fail("baseline output appeared concurrently")
private_after, private_after_identity = leaf_identity(parent_fd, private_leaf)
final_after, final_after_identity = leaf_identity(parent_fd, output_leaf)
require(private_after_identity == final_after_identity == private_identity and private_after.st_nlink == final_after.st_nlink == 2, "publication link identity differs")
test_hook("after-link")
authenticate_all()
source_state()
private_after, private_after_identity = leaf_identity(parent_fd, private_leaf)
final_after, final_after_identity = leaf_identity(parent_fd, output_leaf)
require(private_after_identity == final_after_identity == private_identity and private_after.st_nlink == final_after.st_nlink == 2, "published baseline was replaced")
os.unlink(private_leaf, dir_fd=parent_fd)
final_node, final_identity = leaf_identity(parent_fd, output_leaf)
require(final_identity == private_identity and final_node.st_nlink == 1 and stat.S_IMODE(final_node.st_mode) == 0o600, "final baseline inode is not private")
private_identity = None
PY
}

generate_baseline() {
    reject_public_fixture_environment
    local -a probe=("$@"); local index=0
    while [ "$index" -lt "${#probe[@]}" ]; do
        case "${probe[index]}" in
            --calibration-root|--out) index=$((index + 2)) ;;
            *) die "unknown generate-baseline argument: ${probe[index]}" ;;
        esac
    done
    validate_sterile_launch generate-baseline "$@"
    authenticate_public_environment
    generate_baseline_impl public "$@"
}

calibrate_baseline() {
    [ "$#" -eq 0 ] || die "calibrate-baseline accepts no arguments"
    validate_sterile_launch calibrate-baseline
    local repo="$PWD" env_fd bash_fd python_fd script_fd repo_fd
    [[ -L /usr/bin/python3 && "$(/usr/bin/readlink /usr/bin/python3)" = python3.14 && /usr/bin/python3 -ef /usr/bin/python3.14 && -x /usr/bin/python3.14 && ! -L /usr/bin/python3.14 ]] || die "calibrate-baseline cannot authenticate /usr/bin/python3"
    exec {env_fd}</usr/bin/env {bash_fd}</usr/bin/bash {python_fd}</usr/bin/python3.14 {script_fd}<"$0" {repo_fd}<"$repo"
    "/proc/self/fd/$env_fd" -i \
        HOME=/home/jakedevar CARGO_HOME=/home/jakedevar/.cargo RUSTUP_HOME=/home/jakedevar/.rustup \
        PATH=/home/jakedevar/.rustup/toolchains/1.94.1-x86_64-unknown-linux-gnu/bin:/home/jakedevar/.cargo/bin:/usr/bin:/bin \
        LANG=C LC_ALL=C TZ=UTC TMPDIR="$repo/$OUTPUT_ROOT/tmp" CARGO_TARGET_DIR="$repo/target" \
        XDG_CONFIG_HOME="$repo/$OUTPUT_ROOT/xdg-empty" NEXTEST_CONFIG_FILE="$repo/.config/nextest.toml" \
        "/proc/self/fd/$python_fd" - "$repo" "$0" "$CUSTODY_THREAT_STATEMENT" "$env_fd" "$bash_fd" "$python_fd" "$script_fd" "$repo_fd" <<'PY'
import ctypes, dataclasses, datetime, errno, hashlib, json, math, os, re, resource, selectors, secrets, signal, stat, struct, sys, time, tomllib

REPO, SCRIPT, THREAT = sys.argv[1:4]
INHERITED_FDS={name:int(value) for name,value in zip(("env","bash","python3","script","repository"),sys.argv[4:9])}
UID, GID = os.getuid(), os.getgid()
RAW_LIMIT = 64 * 1024 * 1024
TOTAL_LIMIT = 1024 * 1024 * 1024
CHUNK = 64 * 1024
EXPECTED_ENV = {
    "HOME":"/home/jakedevar", "CARGO_HOME":"/home/jakedevar/.cargo", "RUSTUP_HOME":"/home/jakedevar/.rustup",
    "PATH":"/home/jakedevar/.rustup/toolchains/1.94.1-x86_64-unknown-linux-gnu/bin:/home/jakedevar/.cargo/bin:/usr/bin:/bin",
    "LANG":"C", "LC_ALL":"C", "TZ":"UTC", "TMPDIR":f"{REPO}/target/test-suite-benchmark/tmp",
    "CARGO_TARGET_DIR":f"{REPO}/target", "XDG_CONFIG_HOME":f"{REPO}/target/test-suite-benchmark/xdg-empty",
    "NEXTEST_CONFIG_FILE":f"{REPO}/.config/nextest.toml",
}
if os.environ != EXPECTED_ENV:
    raise SystemExit("custody engine received a non-closed environment")
INHERITED_PATHS={"env":"/usr/bin/env","bash":"/usr/bin/bash","python3":"/usr/bin/python3.14","script":SCRIPT,"repository":REPO}
INHERITED_FACTS={}
for role,path in INHERITED_PATHS.items():
    inherited=os.fstat(INHERITED_FDS[role]); current=os.stat(path,follow_symlinks=False)
    if (inherited.st_dev,inherited.st_ino)!=(current.st_dev,current.st_ino): raise SystemExit(f"inherited launch descriptor differs: {role}")
    INHERITED_FACTS[role]=(inherited.st_dev,inherited.st_ino,inherited.st_size,inherited.st_mtime_ns,stat.S_IMODE(inherited.st_mode),inherited.st_nlink)

# R5_RESIDENT_SLICE_BEGIN.  Self-test executes this exact production slice in
# a fresh, explicit namespace; every source-mutated control changes one tagged
# predicate in a separate fresh namespace.
@dataclasses.dataclass(frozen=True)
class ResidentLimits:
    json_bytes: int = 64 * 1024 * 1024
    json_depth: int = 64
    # This generic ceiling is intentionally above the maximum node expansion
    # of 100001 exact Cargo targets or Nextest testcases, so the tagged schema
    # guard—not a generic parser guard—owns those B+1 controls.
    json_nodes: int = 3_000_000
    string_bytes: int = 1024 * 1024
    container_items: int = 200000
    suites: int = 512
    testcases: int = 100000
    identities: int = 100000
    raw_bytes: int = 64 * 1024 * 1024
    aggregate_bytes: int = 1024 * 1024 * 1024
    output_bytes: int = 128 * 1024 * 1024
    raw_line_bytes: int = 1024 * 1024
    raw_lines_per_stream: int = 4 * 100000 + 4096
    journal_line_bytes: int = 1024 * 1024
    # The complete aggregate permits at most 16384 full raw chunks.  This
    # leaves ample room for command brackets, EOFs, component boundaries, and
    # custody boundaries while keeping append plus strict replay bounded.
    journal_events: int = 65536
    chunk_bytes: int = 64 * 1024

RESIDENT_LIMITS = ResidentLimits()

def fail(message):
    raise RuntimeError(message)
def sha_bytes(data): return hashlib.sha256(data).hexdigest()
def canonical_json(value): return (json.dumps(value, sort_keys=True, separators=(",",":"), ensure_ascii=True, allow_nan=False)+"\n").encode("ascii")
def utc_now(): return datetime.datetime.now(datetime.timezone.utc).strftime("%Y-%m-%dT%H:%M:%S.%fZ")
def median(values):
    ordered=sorted(values); size=len(ordered)
    if not size: fail("empty statistics input")
    return ordered[size//2] if size%2 else (ordered[size//2-1]+ordered[size//2])/2
def stats(values):
    center=median(values); spread=median([abs(value-center) for value in values])
    return {"samples":values,"median":center,"mad":spread}
def require_json_bytes(size,label,limit=None):
    bound=RESIDENT_LIMITS.json_bytes if limit is None else limit
    # R5_GUARD_JSON_BYTES
    if type(size) is not int or size>bound: fail(f"{label} exceeds JSON byte bound")
def require_json_nodes(count,label):
    # R6_GUARD_JSON_NODES
    if count>RESIDENT_LIMITS.json_nodes: fail(f"{label} exceeds JSON node bound")
def require_container_items(count,label):
    # R6_GUARD_JSON_CONTAINER
    if count>RESIDENT_LIMITS.container_items: fail(f"{label} exceeds JSON container bound")
def strict_json_load(data,label,limit=None):
    if limit is None: limit=RESIDENT_LIMITS.json_bytes
    if not isinstance(data,(bytes,bytearray)): fail(f"{label} is not JSON bytes")
    require_json_bytes(len(data),label,limit)
    def pairs(items):
        result={}
        for key,value in items:
            if key in result: fail(f"{label} contains duplicate JSON key: {key}")
            result[key]=value
        return result
    try: value=json.loads(bytes(data).decode("utf-8","strict"),object_pairs_hook=pairs,parse_constant=lambda value: fail(f"{label} contains nonfinite number"))
    except RuntimeError: raise
    except Exception as error: fail(f"malformed {label}: {error}")
    nodes=0
    def walk(item,depth=0):
        nonlocal nodes
        nodes+=1
        # R5_GUARD_JSON_DEPTH
        if depth>RESIDENT_LIMITS.json_depth: fail(f"{label} exceeds JSON depth bound")
        require_json_nodes(nodes,label)
        if isinstance(item,str):
            # R5_GUARD_JSON_STRING
            if len(item.encode("utf-8"))>RESIDENT_LIMITS.string_bytes: fail(f"{label} exceeds JSON string bound")
        elif isinstance(item,dict):
            require_container_items(len(item),label)
            for key,child in item.items():
                if not isinstance(key,str): fail(f"{label} has non-string key")
                walk(key,depth+1); walk(child,depth+1)
        elif isinstance(item,list):
            require_container_items(len(item),label)
            for child in item: walk(child,depth+1)
        elif item is not None and type(item) not in (bool,int,float): fail(f"{label} has unsupported JSON type")
    walk(value)
    return value

def exact_keys(value,required,optional,label):
    if not isinstance(value,dict) or not set(required)<=set(value) or not set(value)<=set(required)|set(optional): fail(f"{label} schema differs")

class RunCustody:
    def __init__(self):
        self.fds=[]; self.nodes={}; self.event_sequence=0; self.command_sequence=0
        self.journal_hash=hashlib.sha256(); self.journal_bytes=0
        self.records=[]; self.measurements={}; self.created=[]; self.final_identity=None; self.publication=None; self.log_fds={}; self.evidence_files={}; self.evidence_dirs={}
        self.journal_events=[]; self.private_watches={}; self.private_watch_paths={}; self.hooks={}; self.cargo_discovery=[]
        self.authority_hashes={}; self.source_watch_info={}; self.watch_paths=[]
        self.current_boundary="construction"; self.current_phase="authentication"; self.terminal_recorded=False
        self.rejected=False; self.rejection=None; self.active_child=None; self.last_child=None
        self.repo_fd=self.open_node(REPO, os.O_RDONLY|os.O_DIRECTORY|os.O_NOFOLLOW, "repository", directory=True)
        self.authenticate_graph()
        self.prepare_tree()
        self.authenticate_source_state()

    def open_node(self,path,flags,label,directory=False):
        fd=os.open(path, flags|os.O_CLOEXEC)
        node=os.fstat(fd); mode=stat.S_IMODE(node.st_mode)
        if directory:
            if not stat.S_ISDIR(node.st_mode): fail(f"{label} is not a directory")
        elif not stat.S_ISREG(node.st_mode): fail(f"{label} is not a regular file")
        if node.st_uid != UID or node.st_gid != GID:
            # Root-owned immutable system tools are accepted; writable foreign nodes are not.
            if not (node.st_uid == 0 and mode & 0o022 == 0): fail(f"{label} has an unauthenticated owner")
        self.fds.append(fd); self.nodes[label]=(path,fd,node.st_dev,node.st_ino,node.st_size,node.st_mtime_ns,mode,node.st_nlink)
        return fd

    def open_link(self,path,label):
        fd=os.open(path,os.O_PATH|os.O_NOFOLLOW|os.O_CLOEXEC); node=os.fstat(fd)
        if not stat.S_ISLNK(node.st_mode): fail(f"{label} is not a symlink")
        self.fds.append(fd); self.nodes[label]=(path,fd,node.st_dev,node.st_ino,node.st_size,node.st_mtime_ns,stat.S_IMODE(node.st_mode),node.st_nlink)
        return fd

    def hash_fd(self,fd,limit=RESIDENT_LIMITS.json_bytes):
        digest=hashlib.sha256(); offset=0
        while True:
            block=os.pread(fd,RESIDENT_LIMITS.chunk_bytes,offset)
            if not block: break
            offset+=len(block)
            if offset>limit: fail("authenticated authority file exceeds bound")
            digest.update(block)
        return offset,digest.hexdigest()

    def retain_authority_hash(self,label):
        fd=self.nodes[label][1]; self.authority_hashes[label]=self.hash_fd(fd)

    def git_policy_argv(self,*arguments):
        # PERF-Z-CAL-FR-01/02: every Git read has one literal, source-visible
        # policy.  No call site may add a weaker Git vector.
        return [f"/proc/self/fd/{self.tools['git']}","--no-optional-locks",
          "-c","core.fsmonitor=false","-c","core.untrackedCache=false",
          "-c","status.showUntrackedFiles=all","-c","submodule.recurse=false",
          "-c","diff.ignoreSubmodules=none",*arguments]

    def git_child_env(self):
        environment=dict(EXPECTED_ENV)
        environment.update({"GIT_OPTIONAL_LOCKS":"0","GIT_CONFIG_NOSYSTEM":"1",
          "GIT_CONFIG_SYSTEM":"/dev/null","GIT_CONFIG_GLOBAL":"/dev/null",
          "GIT_CONFIG_COUNT":"0","GIT_ATTR_NOSYSTEM":"1","GIT_TERMINAL_PROMPT":"0"})
        return environment

    def require_git_vector(self,args,environment):
        prefix=self.git_policy_argv()
        # FR_GUARD_GIT_VECTOR
        if args[:len(prefix)]!=prefix or environment!=self.git_child_env():
            fail("Git child did not use the authenticated literal policy")

    def validate_git_config(self,label,fd):
        text=self.read_held(fd,4*1024*1024).decode("utf-8","strict")
        section=None; worktree_config=False
        for raw in text.splitlines():
            line=raw.strip()
            if not line or line.startswith(("#",";")): continue
            if line.endswith("\\"): fail(f"{label} contains an unsupported continued value")
            match=re.fullmatch(r'\[([A-Za-z0-9.-]+)(?:\s+"(?:[^"\\]|\\.)*")?\]',line)
            if match:
                section=match.group(1).lower()
                # FR_GUARD_GIT_CONFIG_INCLUDE
                if section in ("include","includeif"): fail(f"{label} contains an unauthenticated include source")
                continue
            match=re.fullmatch(r'([A-Za-z0-9.-]+)(?:\s*=\s*(.*))?',line)
            if section is None or match is None: fail(f"{label} contains unsupported Git config syntax")
            key=match.group(1).lower(); value=(match.group(2) or "true").strip().lower()
            qualified=f"{section}.{key}"
            if qualified in ("core.fsmonitor","core.trustctime","status.showuntrackedfiles"):
                fail(f"{label} contains a forbidden Git authority setting: {qualified}")
            if qualified=="extensions.worktreeconfig":
                if value not in ("true","yes","on","1","false","no","off","0"): fail(f"{label} has invalid extensions.worktreeConfig")
                worktree_config=value in ("true","yes","on","1")
        return worktree_config

    def open_source_directories_before_git(self):
        # Hold the complete repository directory graph (except the explicitly
        # excluded mutable target contents) before the first Git source probe.
        for root,directories,_files,root_fd in os.fwalk(REPO,topdown=True,follow_symlinks=False):
            relative=os.path.relpath(root,REPO)
            if relative==".": directories[:]=[name for name in directories if name not in ("target",)]
            directories[:]=[name for name in directories if not os.path.islink(os.path.join(root,name))]
            if relative==".": continue
            label=f"source-dir:{relative}"
            if label not in self.nodes:
                self.open_node(root,os.O_RDONLY|os.O_DIRECTORY|os.O_NOFOLLOW,label,directory=True)

    def reconcile_git_identity(self,head,branch):
        # FR_GUARD_GIT_HEAD_BRANCH_RECONCILIATION
        if head!=self.git_head or branch!=self.git_branch: fail("watched Git HEAD or symbolic branch differs from held graph")

    def reconcile_git_index(self):
        # FR_GUARD_GIT_INDEX_RECONCILIATION
        if self.hash_fd(self.nodes["config:git-index"][1])!=self.authority_hashes["config:git-index"]: fail("Git index content changed during watched source authentication")

    def reconcile_tracked_paths(self,tracked):
        # FR_GUARD_GIT_TRACKED_SET_RECONCILIATION
        if tuple(tracked)!=self.tracked_paths: fail("source tracked-path set changed")

    def authenticate_graph(self):
        pinned="/home/jakedevar/.rustup/toolchains/1.94.1-x86_64-unknown-linux-gnu/bin"
        paths={
          "env":"/usr/bin/env","bash":"/usr/bin/bash","git":"/usr/bin/git","find":"/usr/bin/find","nproc":"/usr/bin/nproc",
          "uname":"/usr/bin/uname","lscpu":"/usr/bin/lscpu","make":"/usr/bin/make","python3":"/usr/bin/python3.14",
          "jq":"/usr/bin/jq","stat":"/usr/bin/stat","sha256sum":"/usr/bin/sha256sum","clang":"/usr/bin/clang-22","mold":"/usr/bin/mold",
          "cargo":f"{pinned}/cargo","rustc":f"{pinned}/rustc","rustdoc":f"{pinned}/rustdoc",
          "cargo-nextest":"/home/jakedevar/.cargo/bin/cargo-nextest",
        }
        self.tools={name:self.open_node(path,os.O_RDONLY|os.O_NOFOLLOW,f"tool:{name}") for name,path in paths.items()}
        configs={"script":SCRIPT,"Makefile":f"{REPO}/Makefile","nextest":f"{REPO}/.config/nextest.toml",
                 "cargo-repo":f"{REPO}/.cargo/config.toml","cargo-account":"/home/jakedevar/.cargo/config.toml",
                 "toolchain":f"{REPO}/rust-toolchain.toml","workspace-manifest":f"{REPO}/Cargo.toml","lockfile":f"{REPO}/Cargo.lock","gitfile":f"{REPO}/.git"}
        self.configs={name:self.open_node(path,os.O_RDONLY|os.O_NOFOLLOW,f"config:{name}") for name,path in configs.items()}
        gitfile=self.read_held(self.configs["gitfile"],4096).decode("utf-8","strict")
        match=re.fullmatch(r"gitdir: (/[^\n]+)\n",gitfile)
        if not match: fail("noncanonical Git worktree indirection")
        gitdir=os.path.normpath(match.group(1))
        if not os.path.isabs(gitdir) or os.path.realpath(gitdir)!=gitdir: fail("Git worktree directory is noncanonical")
        self.open_node(gitdir,os.O_RDONLY|os.O_DIRECTORY|os.O_NOFOLLOW,"config:git-worktree-dir",directory=True)
        commondir_path=os.path.join(gitdir,"commondir")
        self.open_node(commondir_path,os.O_RDONLY|os.O_NOFOLLOW,"config:git-commondir")
        common=os.path.realpath(os.path.join(gitdir,self.read_held(self.nodes["config:git-commondir"][1],4096).decode().strip()))
        if not os.path.isabs(common): fail("Git common directory is noncanonical")
        self.open_node(common,os.O_RDONLY|os.O_DIRECTORY|os.O_NOFOLLOW,"config:git-common-dir",directory=True)
        self.open_node(os.path.join(common,"refs"),os.O_RDONLY|os.O_DIRECTORY|os.O_NOFOLLOW,"config:git-refs-dir",directory=True)
        self.open_node(os.path.join(common,"refs/heads"),os.O_RDONLY|os.O_DIRECTORY|os.O_NOFOLLOW,"config:git-heads-dir",directory=True)
        head_path=os.path.join(gitdir,"HEAD"); self.open_node(head_path,os.O_RDONLY|os.O_NOFOLLOW,"config:git-head")
        head_text=self.read_held(self.nodes["config:git-head"][1],4096).decode("utf-8","strict")
        head_match=re.fullmatch(r"ref: refs/heads/([^\n]+)\n",head_text)
        if head_match is None or head_match.group(1).startswith("/") or ".." in head_match.group(1).split("/"): fail("noncanonical symbolic Git HEAD")
        self.git_branch=head_match.group(1)
        branch_parent=os.path.dirname(os.path.join(common,"refs/heads",self.git_branch))
        current=os.path.join(common,"refs/heads")
        relative_parent=os.path.relpath(branch_parent,current)
        if relative_parent!=".":
            accumulated=current
            for part in relative_parent.split(os.sep):
                accumulated=os.path.join(accumulated,part)
                self.open_node(accumulated,os.O_RDONLY|os.O_DIRECTORY|os.O_NOFOLLOW,f"config:git-ref-dir:{os.path.relpath(accumulated,common)}",directory=True)
        branch_path=os.path.join(common,"refs/heads",self.git_branch)
        git_nodes={"git-index":os.path.join(gitdir,"index"),"git-config":os.path.join(common,"config"),"git-packed-refs":os.path.join(common,"packed-refs"),"git-branch-ref":branch_path}
        for name,path in git_nodes.items():
            if os.path.isdir(path): self.open_node(path,os.O_RDONLY|os.O_DIRECTORY|os.O_NOFOLLOW,f"config:{name}",directory=True)
            else: self.open_node(path,os.O_RDONLY|os.O_NOFOLLOW,f"config:{name}")
        self.git_head=self.read_held(self.nodes["config:git-branch-ref"][1],4096).decode("ascii","strict").strip()
        if not re.fullmatch(r"[0-9a-f]{40,64}",self.git_head): fail("noncanonical source commit")
        for label in ("config:git-head","config:git-index","config:git-commondir","config:git-config","config:git-packed-refs","config:git-branch-ref"):
            self.retain_authority_hash(label)
        worktree_enabled=self.validate_git_config("repository Git config",self.nodes["config:git-config"][1])
        worktree_path=os.path.join(gitdir,"config.worktree")
        try: worktree_node=os.stat(worktree_path,follow_symlinks=False)
        except FileNotFoundError: worktree_node=None
        if worktree_enabled:
            if worktree_node is None: fail("extensions.worktreeConfig requires an authenticated worktree config")
            self.open_node(worktree_path,os.O_RDONLY|os.O_NOFOLLOW,"config:git-worktree-config")
            self.validate_git_config("worktree Git config",self.nodes["config:git-worktree-config"][1])
            self.retain_authority_hash("config:git-worktree-config")
        elif worktree_node is not None:
            fail("unauthenticated Git worktree config exists")
        self.aliases={}
        for label,path,target in (("python3-alias","/usr/bin/python3","python3.14"),("clang-alias","/usr/bin/clang","clang-22")):
            node=os.lstat(path)
            if not stat.S_ISLNK(node.st_mode) or os.readlink(path)!=target: fail(f"tool alias differs: {label}")
            self.aliases[label]=(path,target,node.st_dev,node.st_ino,node.st_mtime_ns,node.st_ctime_ns)
        self.open_source_directories_before_git()
        self.authenticate_cargo_discovery()
        self.arm_watches()

    def authenticate_source_state(self):
        self.boundary("before-bootstrap-git-probes",phase="authentication")
        head=self.capture_simple("bootstrap-git-head","git",self.git_policy_argv("rev-parse","--verify","HEAD^{commit}"),retain=False).decode("ascii","strict").strip()
        branch=self.capture_simple("bootstrap-git-branch","git",self.git_policy_argv("symbolic-ref","--short","HEAD"),retain=False).decode("utf-8","strict").strip()
        status=self.capture_simple("bootstrap-git-status","git",self.git_policy_argv("status","--porcelain=v1","--untracked-files=all","--ignore-submodules=none"),retain=False)
        tracked_bytes=self.capture_simple("bootstrap-git-files","git",self.git_policy_argv("ls-files","-z","--cached"),retain=False)
        tracked=tuple(raw.decode("utf-8","strict") for raw in tracked_bytes.split(b"\0") if raw)
        self.reconcile_git_identity(head,branch)
        if status: fail("source tree is dirty")
        if len(tracked)!=len(set(tracked)) or tuple(sorted(tracked))!=tracked: fail("Git tracked source set is noncanonical")
        self.tracked_paths=tracked
        for rel in tracked:
            if rel.startswith("target/") or rel=="metrics/test-suite-baseline.json": continue
            path=os.path.join(REPO,rel)
            label=f"source:{rel}"
            if os.path.islink(path): self.open_link(path,label)
            elif os.path.isfile(path): self.open_node(path,os.O_RDONLY|os.O_NOFOLLOW,label)
            else: fail(f"tracked source node has unsupported type: {rel}")
            self.add_source_watch(path)
        self.reprove()
        self.reconcile_git_index()
        self.boundary("after-bootstrap-git-probes",phase="authentication")

    def authenticate_cargo_discovery(self):
        names=("config.toml","config","credentials.toml","credentials")
        accepted={os.path.normpath(f"{REPO}/.cargo/config.toml"):self.configs["cargo-repo"],
                  os.path.normpath(f"{EXPECTED_ENV['CARGO_HOME']}/config.toml"):self.configs["cargo-account"]}
        locations=[]; current=os.path.normpath(REPO)
        while True:
            locations.append(os.path.join(current,".cargo"))
            parent=os.path.dirname(current)
            if parent==current: break
            current=parent
        cargo_home=os.path.normpath(EXPECTED_ENV["CARGO_HOME"])
        if cargo_home not in locations: locations.append(cargo_home)
        libc=ctypes.CDLL(None,use_errno=True); self.cargo_inotify_fd=libc.inotify_init1(os.O_NONBLOCK|os.O_CLOEXEC)
        if self.cargo_inotify_fd<0: fail("cannot create Cargo discovery watch")
        self.fds.append(self.cargo_inotify_fd); self.cargo_watch_info={}
        mask=0x00000002|0x00000004|0x00000008|0x00000100|0x00000200|0x00000040|0x00000080|0x00000400|0x00000800
        for cargo_dir in sorted(set(locations)):
            parent=os.path.dirname(cargo_dir); leaf=os.path.basename(cargo_dir)
            parent_fd=os.open(parent,os.O_RDONLY|os.O_DIRECTORY|os.O_NOFOLLOW|os.O_CLOEXEC); self.fds.append(parent_fd)
            parent_node=os.fstat(parent_fd)
            wd=libc.inotify_add_watch(self.cargo_inotify_fd,ctypes.c_char_p(f"/proc/self/fd/{parent_fd}".encode()),ctypes.c_uint32(mask))
            if wd<0: fail("cannot watch Cargo discovery parent")
            self.cargo_watch_info[wd]=(parent,leaf,"parent")
            entry={"path":cargo_dir,"parent":parent,"parent_fd":parent_fd,"parent_identity":(parent_node.st_dev,parent_node.st_ino,stat.S_IMODE(parent_node.st_mode)),"leaf":leaf,"dir_fd":None,"dir_identity":None}
            try: dir_fd=os.open(leaf,os.O_RDONLY|os.O_DIRECTORY|os.O_NOFOLLOW|os.O_CLOEXEC,dir_fd=parent_fd)
            except FileNotFoundError: dir_fd=None
            if dir_fd is not None:
                self.fds.append(dir_fd); node=os.fstat(dir_fd); entry["dir_fd"]=dir_fd; entry["dir_identity"]=(node.st_dev,node.st_ino,stat.S_IMODE(node.st_mode))
                wd=libc.inotify_add_watch(self.cargo_inotify_fd,ctypes.c_char_p(f"/proc/self/fd/{dir_fd}".encode()),ctypes.c_uint32(mask))
                if wd<0: fail("cannot watch Cargo discovery directory")
                self.cargo_watch_info[wd]=(cargo_dir,names,"directory")
            self.cargo_discovery.append(entry)
            for name in names:
                path=os.path.normpath(os.path.join(cargo_dir,name))
                try: node=os.stat(name,dir_fd=dir_fd,follow_symlinks=False) if dir_fd is not None else None
                except FileNotFoundError: node=None
                # R5_GUARD_CARGO_DISCOVERY
                if node is not None and path not in accepted: fail(f"unapproved Cargo discovery source: {path}")
                if path in accepted:
                    if node is None or (node.st_dev,node.st_ino)!=self.identity(accepted[path]): fail(f"approved Cargo source identity differs: {path}")
        self.reprove_cargo_discovery(check_events=False)

    def cargo_discovery_events(self):
        events=[]
        while True:
            try: data=os.read(self.cargo_inotify_fd,RESIDENT_LIMITS.chunk_bytes)
            except BlockingIOError: break
            if not data: break
            offset=0
            while offset<len(data):
                wd,mask,cookie,length=struct.unpack_from("iIII",data,offset); offset+=16
                name=data[offset:offset+length].split(b"\0",1)[0].decode("utf-8","strict"); offset+=length
                info=self.cargo_watch_info.get(wd)
                if mask&0x00004000 or mask&(0x00000400|0x00000800) or (info is not None and ((info[2]=="parent" and name==info[1]) or (info[2]=="directory" and name in info[1]))): events.append((wd,mask,name))
        return events

    def reprove_cargo_discovery(self,check_events=True):
        events=self.cargo_discovery_events() if check_events else []
        names=("config.toml","config","credentials.toml","credentials")
        approved={os.path.normpath(f"{REPO}/.cargo/config.toml"),os.path.normpath(f"{EXPECTED_ENV['CARGO_HOME']}/config.toml")}
        unexpected=[]
        for entry in self.cargo_discovery:
            parent=os.fstat(entry["parent_fd"]); named_parent=os.stat(entry["parent"],follow_symlinks=False)
            if (parent.st_dev,parent.st_ino,stat.S_IMODE(parent.st_mode))!=entry["parent_identity"] or (named_parent.st_dev,named_parent.st_ino)!=(parent.st_dev,parent.st_ino): fail("Cargo discovery parent drifted")
            try: named=os.stat(entry["leaf"],dir_fd=entry["parent_fd"],follow_symlinks=False)
            except FileNotFoundError: named=None
            if entry["dir_fd"] is None:
                if named is not None: unexpected.append(entry["path"])
                continue
            held=os.fstat(entry["dir_fd"])
            if named is None or (held.st_dev,held.st_ino,stat.S_IMODE(held.st_mode))!=entry["dir_identity"] or (named.st_dev,named.st_ino)!=(held.st_dev,held.st_ino): fail("Cargo discovery directory drifted")
            for name in names:
                path=os.path.normpath(os.path.join(entry["path"],name))
                try: node=os.stat(name,dir_fd=entry["dir_fd"],follow_symlinks=False)
                except FileNotFoundError: node=None
                if node is not None and path not in approved: unexpected.append(path)
        # R5_GUARD_CARGO_DISCOVERY_REPROOF
        if events or unexpected: fail("Cargo filesystem discovery graph mutated or gained an unapproved source")

    def read_path(self,path,limit):
        fd=os.open(path,os.O_RDONLY|os.O_NOFOLLOW|os.O_CLOEXEC)
        try:
            data=bytearray()
            while len(data)<=limit:
                block=os.read(fd,min(CHUNK,limit+1-len(data)))
                if not block: break
                data.extend(block)
            if len(data)>limit: fail(f"authenticated path exceeds bound: {path}")
            return bytes(data)
        finally: os.close(fd)

    def arm_watches(self):
        libc=ctypes.CDLL(None,use_errno=True); self.inotify_fd=libc.inotify_init1(os.O_NONBLOCK|os.O_CLOEXEC)
        if self.inotify_fd<0: fail("cannot create mutation watch")
        self.fds.append(self.inotify_fd)
        self.source_watch_mask=0x00000002|0x00000004|0x00000008|0x00000040|0x00000080|0x00000100|0x00000200|0x00000400|0x00000800
        self.watch_paths=[]; self.source_watch_info={}
        for label,(path,fd,*_) in self.nodes.items():
            if label == "target": continue
            self.add_source_watch(path)

    def add_source_watch(self,path):
        libc=ctypes.CDLL(None,use_errno=True)
        wd=libc.inotify_add_watch(self.inotify_fd,ctypes.c_char_p(path.encode()),ctypes.c_uint32(self.source_watch_mask))
        if wd<0: fail(f"cannot watch authenticated node: {path}")
        if path not in self.watch_paths: self.watch_paths.append(path)
        self.source_watch_info[wd]=path

    def arm_private_watch(self,fd,relative):
        libc=ctypes.CDLL(None,use_errno=True)
        mask=0x00000004|0x00000100|0x00000200|0x00000040|0x00000080|0x00000400|0x00000800
        wd=libc.inotify_add_watch(self.private_inotify_fd,ctypes.c_char_p(f"/proc/self/fd/{fd}".encode()),ctypes.c_uint32(mask))
        if wd<0: fail(f"cannot watch private evidence node: {relative}")
        self.private_watches[wd]=fd; self.private_watch_paths[wd]=relative

    def arm_private_tree(self):
        libc=ctypes.CDLL(None,use_errno=True)
        self.private_inotify_fd=libc.inotify_init1(os.O_NONBLOCK|os.O_CLOEXEC)
        if self.private_inotify_fd<0: fail("cannot create private-tree mutation watch")
        self.fds.append(self.private_inotify_fd)
        for relative,fd in self.evidence_dirs.items(): self.arm_private_watch(fd,relative)

    def private_events(self):
        events=[]
        while True:
            try: data=os.read(self.private_inotify_fd,CHUNK)
            except BlockingIOError: break
            if not data: break
            offset=0
            while offset<len(data):
                wd,mask,cookie,length=struct.unpack_from("iIII",data,offset); offset+=16
                name=data[offset:offset+length].split(b"\0",1)[0].decode("utf-8","strict"); offset+=length
                if mask&0x00004000: fail("private-tree mutation watch overflowed")
                events.append((wd,mask,name))
        return events

    def expect_private_create(self,parent_fd,name,directory=False):
        expected_wd=next((wd for wd,fd in self.private_watches.items() if fd==parent_fd),None)
        expected_type=0x40000000 if directory else 0
        events=self.private_events()
        if not events:
            waiter=selectors.DefaultSelector(); waiter.register(self.private_inotify_fd,selectors.EVENT_READ)
            waiter.select(1.0); waiter.close(); events=self.private_events()
        if expected_wd is None or len(events)!=1 or events[0][0]!=expected_wd or events[0][2]!=name or not (events[0][1]&0x00000100) or (events[0][1]&0x40000000)!=expected_type:
            fail(f"unexpected private-tree transition while creating {name}")

    def require_quiet_private_tree(self):
        if self.private_events(): fail("unexpected private-tree mutation event")

    def reprove_private_paths(self):
        if hasattr(self,"calibration_fd"):
            named=os.stat(self.run_leaf,dir_fd=self.calibration_fd,follow_symlinks=False)
            if (named.st_dev,named.st_ino)!=self.identity(self.run_fd): fail("private run-root path/fd disagreement")
        for relative,fd in self.evidence_dirs.items():
            if relative==".": continue
            parent_relative=os.path.dirname(relative) or "."; leaf=os.path.basename(relative); parent_fd=self.evidence_dirs.get(parent_relative)
            if parent_fd is None: fail(f"private directory parent is not retained: {relative}")
            named=os.stat(leaf,dir_fd=parent_fd,follow_symlinks=False)
            if (named.st_dev,named.st_ino)!=self.identity(fd): fail(f"private directory path/fd disagreement: {relative}")
        for relative,(fd,_,_) in self.evidence_files.items():
            parent_relative=os.path.dirname(relative) or "."; leaf=os.path.basename(relative); parent_fd=self.evidence_dirs.get(parent_relative)
            if parent_fd is None: fail(f"private file parent is not retained: {relative}")
            named=os.stat(leaf,dir_fd=parent_fd,follow_symlinks=False)
            if (named.st_dev,named.st_ino)!=self.identity(fd): fail(f"private file path/fd disagreement: {relative}")

    def accept_runtime_private_events(self,command_id):
        waiter=selectors.DefaultSelector(); waiter.register(self.private_inotify_fd,selectors.EVENT_READ)
        waiter.select(0.1); waiter.close()
        allowed={wd for wd,path in self.private_watch_paths.items() if path in ("tmp","xdg-empty")}
        for wd,mask,name in self.private_events():
            if wd not in allowed or mask&(0x00000400|0x00000800|0x00004000): fail("unexpected private-tree runtime mutation")
            if not name or "/" in name or name in (".",".."): fail("invalid private-tree runtime leaf")
            self.journal({"type":"runtime-tree-event","command_id":command_id,"tree":self.private_watch_paths[wd],"mask":mask,"leaf":name})

    def mkdir_private(self,parent_fd,name,relative=None,harden=False):
        existing=None
        watched=bool(self.private_watches)
        try: os.mkdir(name,0o700,dir_fd=parent_fd)
        except FileExistsError:
            existing=os.stat(name,dir_fd=parent_fd,follow_symlinks=False)
            if not stat.S_ISDIR(existing.st_mode) or existing.st_uid!=UID or existing.st_gid!=GID: fail(f"nonprivate directory: {name}")
        fd=os.open(name,os.O_RDONLY|os.O_DIRECTORY|os.O_NOFOLLOW|os.O_CLOEXEC,dir_fd=parent_fd); self.fds.append(fd)
        if existing is not None and (existing.st_dev,existing.st_ino)!=(os.fstat(fd).st_dev,os.fstat(fd).st_ino): fail(f"directory replaced while opening: {name}")
        if stat.S_IMODE(os.fstat(fd).st_mode)!=0o700:
            if not harden: fail(f"nonprivate directory mode: {name}")
            os.fchmod(fd,0o700)
        if relative is not None: self.evidence_dirs[relative]=fd
        if watched and existing is None:
            self.expect_private_create(parent_fd,name,True)
            self.arm_private_watch(fd,relative or name)
        return fd

    def prepare_tree(self):
        target_fd=self.open_node(f"{REPO}/target",os.O_RDONLY|os.O_DIRECTORY|os.O_NOFOLLOW,"target",directory=True)
        bench_fd=self.mkdir_private(target_fd,"test-suite-benchmark",harden=True)
        calibration_fd=self.mkdir_private(bench_fd,"z-baseline-calibration")
        for _ in range(64):
            self.run_leaf=f"{self.git_head}.custody.{secrets.token_hex(12)}"
            try: os.mkdir(self.run_leaf,0o700,dir_fd=calibration_fd); break
            except FileExistsError: continue
        else: fail("cannot allocate exclusive custody root")
        self.calibration_fd=calibration_fd
        self.run_fd=os.open(self.run_leaf,os.O_RDONLY|os.O_DIRECTORY|os.O_NOFOLLOW|os.O_CLOEXEC,dir_fd=calibration_fd); self.fds.append(self.run_fd)
        self.created.append((calibration_fd,self.run_leaf,os.fstat(self.run_fd).st_dev,os.fstat(self.run_fd).st_ino,"dir"))
        self.evidence_dirs["."]=self.run_fd
        self.raw_fd=self.mkdir_private(self.run_fd,"raw","raw"); self.meta_fd=self.mkdir_private(self.run_fd,"meta","meta")
        self.tmp_fd=self.mkdir_private(self.run_fd,"tmp","tmp"); self.xdg_fd=self.mkdir_private(self.run_fd,"xdg-empty","xdg-empty")
        self.child_tmp=f"{REPO}/target/test-suite-benchmark/z-baseline-calibration/{self.run_leaf}/tmp"
        self.child_xdg=f"{REPO}/target/test-suite-benchmark/z-baseline-calibration/{self.run_leaf}/xdg-empty"
        self.journal_fd=os.open("event-journal.jsonl",os.O_RDWR|os.O_CREAT|os.O_EXCL|os.O_NOFOLLOW|os.O_CLOEXEC,0o600,dir_fd=self.run_fd); self.fds.append(self.journal_fd)
        self.evidence_files["event-journal.jsonl"]=(self.journal_fd,None,None)
        self.created.append((self.run_fd,"event-journal.jsonl",*self.identity(self.journal_fd),"file"))
        self.terminal_fd=os.open("terminal-rejection.json",os.O_RDWR|os.O_CREAT|os.O_EXCL|os.O_NOFOLLOW|os.O_CLOEXEC,0o600,dir_fd=self.run_fd); self.fds.append(self.terminal_fd)
        self.evidence_files["terminal-rejection.json"]=(self.terminal_fd,0,sha_bytes(b""))
        self.created.append((self.run_fd,"terminal-rejection.json",*self.identity(self.terminal_fd),"file"))
        self.arm_private_tree()

    def identity(self,fd):
        node=os.fstat(fd); return node.st_dev,node.st_ino
    def write_all(self,fd,data):
        offset=0
        while offset<len(data):
            written=os.write(fd,data[offset:])
            if written<=0: fail("short evidence write")
            offset+=written
    def inode_claim(self,fd):
        node=os.fstat(fd)
        return {"device":node.st_dev,"inode":node.st_ino,"uid":node.st_uid,"gid":node.st_gid,"mode":format(stat.S_IMODE(node.st_mode),"04o"),"link_count":node.st_nlink}
    def require_journal_line(self,size):
        # R6_GUARD_JOURNAL_LINE_BYTES
        if type(size) is not int or size>RESIDENT_LIMITS.journal_line_bytes: fail("event journal line exceeds bound")
    def require_journal_events(self,count):
        # R6_GUARD_JOURNAL_EVENTS
        if type(count) is not int or count>RESIDENT_LIMITS.journal_events: fail("event journal count exceeds bound")
    def complete_evidence_bytes(self):
        total=0; identities=set()
        for fd,_,_ in self.evidence_files.values():
            node=os.fstat(fd); identity=(node.st_dev,node.st_ino)
            if identity in identities: fail("aliased retained evidence inode")
            identities.add(identity); total+=node.st_size
        return total
    def enforce_evidence_aggregate(self,additional=0):
        total=self.complete_evidence_bytes()+additional
        # R6_GUARD_EVIDENCE_AGGREGATE
        if total>RESIDENT_LIMITS.aggregate_bytes: fail("complete retained evidence aggregate exceeds bound")
        return total
    def require_accepting(self):
        if self.rejected: fail("custody engine is terminally rejected")
    def journal(self,event):
        self.require_accepting()
        next_sequence=self.event_sequence+1; self.require_journal_events(next_sequence)
        event={"event_sequence":next_sequence,"monotonic_ns":time.monotonic_ns(),**event}; encoded=canonical_json(event); self.require_journal_line(len(encoded)-1)
        self.enforce_evidence_aggregate(len(encoded))
        self.write_all(self.journal_fd,encoded); self.journal_hash.update(encoded); self.journal_bytes+=len(encoded); self.journal_events.append(event); self.event_sequence=next_sequence

    def seal_journal(self):
        os.fsync(self.journal_fd)
        data=self.read_held(self.journal_fd,self.journal_bytes)
        if len(data)!=self.journal_bytes or sha_bytes(data)!=self.journal_hash.hexdigest(): fail("event journal seal differs")

    def boundary(self,name,**claims):
        self.require_accepting()
        self.current_boundary=name; self.current_phase=claims.get("phase",self.current_phase)
        hook=self.hooks.get(name)
        if hook is not None: hook(self,name)
        self.require_quiet_private_tree(); self.reprove_private_paths(); self.reprove()
        self.journal({"type":"boundary","boundary":name,**claims}); self.seal_journal()
        self.require_quiet_private_tree(); self.reprove_private_paths(); self.reprove()

    def capture_simple(self,name,tool,args,retain=False):
        self.require_accepting()
        execution_env=self.git_child_env() if tool=="git" else EXPECTED_ENV
        if tool=="git": self.require_git_vector(args,execution_env)
        read_fd,write_fd=os.pipe2(os.O_CLOEXEC)
        ready_read,ready_write=os.pipe2(os.O_CLOEXEC); release_read,release_write=os.pipe2(os.O_CLOEXEC)
        pid=os.fork()
        if pid==0:
            try:
                os.close(ready_read); os.close(release_write)
                os.setpgid(0,0)
                os.write(ready_write,b"G"); os.close(ready_write)
                if os.read(release_read,1)!=b"G": os._exit(126)
                os.close(release_read)
                os.chdir(REPO); os.dup2(write_fd,1); os.dup2(write_fd,2)
                executable=tool
                if hasattr(self,"tools") and tool in self.tools:
                    os.set_inheritable(self.tools[tool],True); executable=f"/proc/self/fd/{self.tools[tool]}"
                os.execve(executable,args,execution_env)
            finally: os._exit(127)
        os.close(ready_write); os.close(release_read)
        pipe_fds={read_fd,write_fd,ready_read,release_write}
        child={"pid":pid,"pgid":pid,"selector":None,"pipe_fds":pipe_fds,"open_pipe_fds":set(pipe_fds),"leader_reaped":False,"group_verified":False,"group_dead":False,"wait":None,"descendant_reaps":[],"command_id":0,"name":name,"argv":args,"phase":"authentication"}
        self.active_child=child
        try:
            self.establish_child_group(child,ready_read,release_write)
            os.close(write_fd); child["open_pipe_fds"].discard(write_fd); data=bytearray()
            while True:
                chunk=os.read(read_fd,RESIDENT_LIMITS.chunk_bytes)
                if not chunk: break
                data.extend(chunk)
                if len(data)>RESIDENT_LIMITS.raw_bytes: fail(f"bootstrap output oversized: {name}")
            os.close(read_fd); child["open_pipe_fds"].discard(read_fd)
            waited_pid,status,rusage=self.wait4_eintr(pid,0)
            if waited_pid!=pid: fail("bootstrap wait4 returned a different child")
            child["leader_reaped"]=True; child["wait"]=(status,rusage)
            if not os.WIFEXITED(status) or os.WEXITSTATUS(status)!=0: fail(f"bootstrap command failed: {name}")
            self.require_group_quiescent(child,f"bootstrap child process group survived leader exit: {name}")
            self.reprove()
            self.active_child=None; self.last_child=child
            return bytes(data)
        except BaseException as error:
            self.reject_child(child,error)
            raise
        finally:
            self.close_child_io(child)

    def wait4_eintr(self,pid,options):
        while True:
            try: return os.wait4(pid,options)
            except InterruptedError: continue

    def establish_child_group(self,child,ready_fd,release_fd):
        waiter=selectors.DefaultSelector()
        try:
            waiter.register(ready_fd,selectors.EVENT_READ)
            if not waiter.select(1.0): fail("child process group handshake timed out")
            if os.read(ready_fd,2)!=b"G": fail("child process group handshake differed")
            try: observed=os.getpgid(child["pid"])
            except ProcessLookupError: fail("child process group disappeared before verification")
            if observed!=child["pgid"]: fail("child process group identity differs")
            child["group_verified"]=True
            if os.write(release_fd,b"G")!=1: fail("child process group release was short")
        finally:
            waiter.close()
            for fd in (ready_fd,release_fd):
                try: os.close(fd)
                except OSError as error:
                    if error.errno!=errno.EBADF: child.setdefault("cleanup_errors",[]).append(f"handshake-{fd}:{error}")
                child["open_pipe_fds"].discard(fd)

    def group_alive(self,child):
        try: os.killpg(child["pgid"],0); return True
        except ProcessLookupError: return False
        except PermissionError: return True

    def reap_group_children(self,child):
        if not child.get("leader_reaped"): return
        while True:
            try: pid,status=os.waitpid(-child["pgid"],os.WNOHANG)
            except InterruptedError: continue
            except ChildProcessError: return
            if pid==0: return
            child["descendant_reaps"].append((pid,status))

    def require_group_quiescent(self,child,message):
        self.reap_group_children(child)
        if self.group_alive(child): fail(message)
        child["group_dead"]=True

    def close_child_io(self,child):
        selector=child.get("selector")
        if selector is not None:
            try: selector.close()
            except BaseException as close_error: child.setdefault("cleanup_errors",[]).append(f"selector:{close_error}")
            child["selector"]=None
        for fd in tuple(child.get("open_pipe_fds",())):
            try: os.close(fd)
            except OSError as error:
                if error.errno!=errno.EBADF:
                    child.setdefault("cleanup_errors",[]).append(f"pipe-{fd}:{error}")
                    continue
            child["open_pipe_fds"].discard(fd)

    def reject_child(self,child,error):
        self.rejected=True
        self.close_child_io(child)
        pid=child["pid"]; pgid=child["pgid"]
        if self.group_alive(child):
            try: os.killpg(pgid,signal.SIGTERM)
            except OSError:
                try: os.kill(pid,signal.SIGTERM)
                except OSError: pass
        deadline=time.monotonic()+0.25; waited=None
        while time.monotonic()<deadline:
            if not child.get("leader_reaped"):
                try: waited=self.wait4_eintr(pid,os.WNOHANG)
                except ChildProcessError: child["leader_reaped"]=True; waited=None
                if waited is not None and waited[0]==pid:
                    child["leader_reaped"]=True; child["wait"]=(waited[1],waited[2])
            self.reap_group_children(child)
            if child.get("leader_reaped") and not self.group_alive(child):
                child["group_dead"]=True; break
            time.sleep(0.005)
        if not child.get("group_dead"):
            try: os.killpg(pgid,signal.SIGKILL)
            except OSError:
                try: os.kill(pid,signal.SIGKILL)
                except OSError: pass
        kill_deadline=time.monotonic()+0.25
        while time.monotonic()<kill_deadline:
            if not child.get("leader_reaped"):
                try: waited=self.wait4_eintr(pid,os.WNOHANG)
                except ChildProcessError: child["leader_reaped"]=True; waited=None
                if waited is not None and waited[0]==pid:
                    child["leader_reaped"]=True; child["wait"]=(waited[1],waited[2])
            self.reap_group_children(child)
            if not self.group_alive(child): child["group_dead"]=True
            if child.get("leader_reaped") and child.get("group_dead"): break
            time.sleep(0.005)
        cleanup_failure=None
        if not child.get("leader_reaped") or not child.get("group_dead"):
            cleanup_failure="owned child process group cleanup did not reach leader-reaped plus group-dead state"
            child.setdefault("cleanup_errors",[]).append(cleanup_failure)
        self.rejection={"reason":str(error),"pid":pid,"process_group":pgid,"group_verified":bool(child.get("group_verified")),"leader_reaped":bool(child.get("leader_reaped")),"group_dead":bool(child.get("group_dead")),"descendant_reaps":tuple(child.get("descendant_reaps",())),"pipe_fds":tuple(sorted(child["pipe_fds"])),"pipes_closed":not child.get("open_pipe_fds"),"cleanup_errors":tuple(child.get("cleanup_errors",())),"cleanup_failure":cleanup_failure,"accepted":False}
        self.active_child=None; self.last_child=child

    def child_env(self):
        return {"HOME":EXPECTED_ENV["HOME"],"CARGO_HOME":EXPECTED_ENV["CARGO_HOME"],"RUSTUP_HOME":EXPECTED_ENV["RUSTUP_HOME"],
          "PATH":EXPECTED_ENV["PATH"],"LANG":"C","LC_ALL":"C","TZ":"UTC","TMPDIR":self.child_tmp,
          "CARGO_TARGET_DIR":f"{REPO}/target","XDG_CONFIG_HOME":self.child_xdg,
          "NEXTEST_CONFIG_FILE":f"/proc/self/fd/{self.configs['nextest']}","CARGO_TERM_COLOR":"never","NO_COLOR":"1"}

    def command(self,name,tool_name,args,phase,threads=None):
        self.require_accepting()
        execution_env=self.git_child_env() if tool_name=="git" else self.child_env()
        if tool_name=="git": self.require_git_vector(args,execution_env)
        self.boundary(f"before-command:{name}",phase=phase)
        self.command_sequence+=1; command_id=self.command_sequence
        command_relative=f"raw/{command_id:03d}-{name}"; command_fd=self.mkdir_private(self.raw_fd,f"{command_id:03d}-{name}",command_relative)
        log_fds={}
        for stream in ("stdout","stderr"):
            fd=os.open(f"{stream}.log",os.O_RDWR|os.O_CREAT|os.O_EXCL|os.O_NOFOLLOW|os.O_CLOEXEC,0o600,dir_fd=command_fd)
            self.fds.append(fd); log_fds[stream]=fd; self.expect_private_create(command_fd,f"{stream}.log")
            self.evidence_files[f"{command_relative}/{stream}.log"]=(fd,None,None)
        self.boundary(f"command-evidence-open:{name}",phase=phase)
        pipes={stream:os.pipe2(os.O_NONBLOCK|os.O_CLOEXEC) for stream in ("stdout","stderr")}
        ready_read,ready_write=os.pipe2(os.O_CLOEXEC); release_read,release_write=os.pipe2(os.O_CLOEXEC)
        started_utc=utc_now(); started_ns=time.monotonic_ns()
        pid=os.fork()
        if pid==0:
            try:
                os.close(ready_read); os.close(release_write)
                os.setpgid(0,0)
                os.write(ready_write,b"G"); os.close(ready_write)
                if os.read(release_read,1)!=b"G": os._exit(126)
                os.close(release_read)
                os.chdir(REPO); null_fd=os.open("/dev/null",os.O_RDONLY|os.O_CLOEXEC); os.dup2(null_fd,0)
                for stream,target in (("stdout",1),("stderr",2)): os.dup2(pipes[stream][1],target)
                keep=set(self.tools.values())|set(self.configs.values())
                for fd in keep: os.set_inheritable(fd,True)
                executable=f"/proc/self/fd/{self.tools[tool_name]}"
                os.execve(executable,args,execution_env)
            finally: os._exit(127)
        os.close(ready_write); os.close(release_read)
        pipe_fds={fd for pair in pipes.values() for fd in pair}|{ready_read,release_write}
        child={"pid":pid,"pgid":pid,"selector":None,"pipe_fds":pipe_fds,"open_pipe_fds":set(pipe_fds),"leader_reaped":False,"group_verified":False,"group_dead":False,"wait":None,"descendant_reaps":[],"command_id":command_id,"name":name,"argv":args,"phase":phase}
        self.active_child=child
        try:
            self.establish_child_group(child,ready_read,release_write)
            for stream in pipes:
                os.close(pipes[stream][1]); child["open_pipe_fds"].discard(pipes[stream][1])
            selector=selectors.DefaultSelector(); child["selector"]=selector; hashes={}; sizes={}; chunks={}
            for stream in ("stdout","stderr"):
                selector.register(pipes[stream][0],selectors.EVENT_READ,stream); hashes[stream]=hashlib.sha256(); sizes[stream]=0; chunks[stream]=0
                self.log_fds[(command_id,stream)]=log_fds[stream]
            hook=self.hooks.get(f"after-fork:{name}")
            if hook is not None: hook(self,f"after-fork:{name}")
            self.journal({"type":"command-start","command_id":command_id,"name":name,"phase":phase,"argv":args,"started_at":started_utc})
            while selector.get_map():
                for key,_ in selector.select():
                    stream=key.data
                    try: data=os.read(key.fd,RESIDENT_LIMITS.chunk_bytes)
                    except BlockingIOError: continue
                    if not data:
                        selector.unregister(key.fd); os.close(key.fd); child["open_pipe_fds"].discard(key.fd)
                        self.journal({"type":"stream-eof","command_id":command_id,"stream":stream,"chunk_count":chunks[stream],"byte_count":sizes[stream],"sha256":hashes[stream].hexdigest()})
                        continue
                    next_size=sizes[stream]+len(data)
                    # R5_GUARD_RAW_BYTES
                    if next_size>RESIDENT_LIMITS.raw_bytes: fail(f"raw evidence stream bound exceeded: {name}/{stream}")
                    self.enforce_evidence_aggregate(len(data))
                    self.write_all(log_fds[stream],data); sizes[stream]=next_size; chunks[stream]+=1; hashes[stream].update(data)
                    self.journal({"type":"raw-chunk","command_id":command_id,"stream":stream,"chunk_sequence":chunks[stream],"offset":sizes[stream]-len(data),"byte_count":len(data),"sha256":sha_bytes(data)})
                    hook=self.hooks.get(f"after-raw:{name}")
                    if hook is not None: hook(self,f"after-raw:{name}")
            waited_pid,status,rusage=self.wait4_eintr(pid,0); finished_ns=time.monotonic_ns(); finished_utc=utc_now()
            if waited_pid!=pid: fail("wait4 returned a different child")
            child["leader_reaped"]=True; child["wait"]=(status,rusage)
            outcome={"kind":"exited","code":os.WEXITSTATUS(status),"signal":None} if os.WIFEXITED(status) else {"kind":"signaled","code":None,"signal":os.WTERMSIG(status)} if os.WIFSIGNALED(status) else {"kind":"unknown","code":None,"signal":None}
            hook=self.hooks.get(f"after-wait:{name}")
            if hook is not None: hook(self,f"after-wait:{name}")
            if outcome!={"kind":"exited","code":0,"signal":None}: fail(f"command did not exit green: {name}: {outcome}")
            self.require_group_quiescent(child,f"child process group survived leader exit: {name}")
            self.accept_runtime_private_events(command_id)
            record={"sequence":command_id,"name":name,"phase":phase,"argv":args,"started_at":started_utc,"finished_at":finished_utc,
              "started_monotonic_ns":started_ns,"finished_monotonic_ns":finished_ns,"wait":outcome,
              "timing":{"wall_seconds":(finished_ns-started_ns)/1e9,"user_seconds":rusage.ru_utime,"system_seconds":rusage.ru_stime,"max_rss_kib":rusage.ru_maxrss},
              "logs":{stream:{"path":f"raw/{command_id:03d}-{name}/{stream}.log","bytes":sizes[stream],"sha256":hashes[stream].hexdigest(),"chunks":chunks[stream],**self.inode_claim(log_fds[stream])} for stream in ("stdout","stderr")}}
            for stream in ("stdout","stderr"): os.fsync(log_fds[stream])
            status_fd=os.open("status.json",os.O_RDWR|os.O_CREAT|os.O_EXCL|os.O_NOFOLLOW|os.O_CLOEXEC,0o600,dir_fd=command_fd); self.fds.append(status_fd); self.expect_private_create(command_fd,"status.json")
            status_bytes=canonical_json(record); self.enforce_evidence_aggregate(len(status_bytes)); self.write_all(status_fd,status_bytes); os.fsync(status_fd)
            if self.read_held(status_fd,len(status_bytes))!=status_bytes: fail("command status seal differs")
            record["status"]={"path":f"raw/{command_id:03d}-{name}/status.json","bytes":len(status_bytes),"sha256":sha_bytes(status_bytes),**self.inode_claim(status_fd)}
            for stream in ("stdout","stderr"): self.evidence_files[record["logs"][stream]["path"]]=(log_fds[stream],sizes[stream],hashes[stream].hexdigest())
            self.evidence_files[record["status"]["path"]]=(status_fd,len(status_bytes),sha_bytes(status_bytes))
            self.enforce_evidence_aggregate(); self.records.append(record); self.journal({"type":"command-finish","command_id":command_id,"wait":outcome,"finished_at":finished_utc,"rusage":record["timing"]}); self.seal_journal()
            self.boundary(f"after-command:{name}",phase=phase)
            self.active_child=None; self.last_child=child
            return record
        except BaseException as error:
            self.reject_child(child,error)
            raise
        finally:
            self.close_child_io(child)

    def cargo(self,name,args,phase): return self.command(name,"cargo",[f"/proc/self/fd/{self.tools['cargo']}",*args],phase)
    def nextest(self,name,args,phase): return self.command(name,"cargo-nextest",[f"/proc/self/fd/{self.tools['cargo-nextest']}","nextest",*args],phase)
    def make(self,name,target,threads,phase,dry=False):
        argv=[f"/proc/self/fd/{self.tools['make']}","-rR","--no-print-directory","--warn-undefined-variables","--jobs=1","-f",f"/proc/self/fd/{self.configs['Makefile']}"]
        if dry: argv.append("-n")
        argv.extend([f"SHELL=/proc/self/fd/{self.tools['bash']}",".SHELLFLAGS=--noprofile --norc -p -c",f"NEXTEST_JOBS={threads}",target])
        return self.command(name,"make",argv,phase,threads)

    def read_log(self,record,stream):
        fd=self.log_fds[(record["sequence"],stream)]; chunks=[]; digest=hashlib.sha256(); total=0; offset=0
        while True:
            block=os.pread(fd,RESIDENT_LIMITS.chunk_bytes,offset)
            if not block: break
            offset+=len(block); total+=len(block); digest.update(block); chunks.append(block)
        claim=record["logs"][stream]
        if total!=claim["bytes"] or digest.hexdigest()!=claim["sha256"]: fail("raw evidence drift")
        return b"".join(chunks)

    def read_json_log(self,record,stream,label):
        fd=self.log_fds[(record["sequence"],stream)]; claim=record["logs"][stream]
        digest=hashlib.sha256(); data=bytearray(); offset=0
        while True:
            block=os.pread(fd,RESIDENT_LIMITS.chunk_bytes,offset)
            if not block: break
            offset+=len(block); digest.update(block)
            require_json_bytes(offset,label)
            data.extend(block)
        if offset!=claim["bytes"] or digest.hexdigest()!=claim["sha256"]: fail(f"{label} raw evidence drift")
        return strict_json_load(data,label)

    def require_raw_line_bytes(self,size):
        # R7_GUARD_RAW_LINE_BYTES
        if type(size) is not int or size>RESIDENT_LIMITS.raw_line_bytes: fail("raw evidence line exceeds bound")

    def require_raw_lines(self,count):
        # R7_GUARD_RAW_LINE_COUNT
        if type(count) is not int or count>RESIDENT_LIMITS.raw_lines_per_stream: fail("raw evidence line count exceeds bound")

    def lines(self,record,stream):
        fd=self.log_fds[(record["sequence"],stream)]; offset=0; pending=b""; digest=hashlib.sha256(); total=0; line_count=0
        while True:
            block=os.pread(fd,RESIDENT_LIMITS.chunk_bytes,offset)
            if not block: break
            offset+=len(block); total+=len(block); digest.update(block); pending+=block
            if b"\n" not in pending: self.require_raw_line_bytes(len(pending))
            while b"\n" in pending:
                line,pending=pending.split(b"\n",1)
                self.require_raw_line_bytes(len(line))
                line_count+=1; self.require_raw_lines(line_count)
                yield line.decode("utf-8","strict")
        if pending:
            self.require_raw_line_bytes(len(pending)); line_count+=1; self.require_raw_lines(line_count); yield pending.decode("utf-8","strict")
        claim=record["logs"][stream]
        if total!=claim["bytes"] or digest.hexdigest()!=claim["sha256"]: fail("incremental raw evidence drift")

    def read_held(self,fd,limit=4*1024*1024):
        data=bytearray(); offset=0
        while True:
            block=os.pread(fd,RESIDENT_LIMITS.chunk_bytes,offset)
            if not block: break
            offset+=len(block); data.extend(block)
            if len(data)>limit: fail("held input exceeds bound")
        return bytes(data)

    def validate_nextest_config(self):
        document=tomllib.loads(self.read_held(self.configs["nextest"]).decode("utf-8","strict")); profiles=document.get("profile")
        if not isinstance(profiles,dict): fail("Nextest profile table is absent")
        def resolve(name,seen=()):
            if name in seen or name not in profiles or not isinstance(profiles[name],dict): fail(f"invalid Nextest inheritance: {name}")
            value=dict(profiles[name]); parent=value.pop("inherits",None)
            return {**resolve(parent,seen+(name,)),**value} if parent else value
        normalized={}
        for name in ("rsid-fast","ci-full"):
            value=resolve(name)
            if value.get("retries")!=0 or value.get("fail-fast") is not False or value.get("test-threads")!=8 or value.get("overrides") not in (None,[]): fail(f"unsafe Nextest profile: {name}")
            if any("filter" in key.lower() or "quarantine" in key.lower() for key in value): fail(f"filtered Nextest profile: {name}")
            normalized[name]={"retries":0,"fail_fast":False,"test_threads":value.get("test-threads"),"filters":[],"quarantine":[]}
        return normalized

    def validate_cargo_graph(self):
        account=tomllib.loads(self.read_held(self.configs["cargo-account"]).decode("utf-8","strict")); target=account.get("target",{}).get("x86_64-unknown-linux-gnu",{})
        if target.get("linker")!="clang" or target.get("rustflags")!=["-C","link-arg=-fuse-ld=mold"]: fail("authenticated Cargo linker policy differs")
        toolchain=tomllib.loads(self.read_held(self.configs["toolchain"]).decode("utf-8","strict")).get("toolchain",{})
        if toolchain.get("channel")!="1.94.1" or toolchain.get("profile")!="minimal" or toolchain.get("components")!=["rustfmt","clippy"]: fail("authenticated Rust toolchain policy differs")
        repository=tomllib.loads(self.read_held(self.configs["cargo-repo"]).decode("utf-8","strict"))
        if set(repository)!={"alias"} or "bench-all" not in repository["alias"]: fail("repository Cargo configuration shape differs")
        def forbidden(value):
            if isinstance(value,dict):
                for key,child in value.items():
                    if key.lower() in ("include","includes") or (key.lower()=="alias" and value is not repository): return True
                    if forbidden(child): return True
            elif isinstance(value,list): return any(forbidden(item) for item in value)
            return False
        if forbidden(account) or forbidden(repository): fail("Cargo configuration contains an unapproved include or alias source")
        return {"linker":"clang","linker_backend":"mold","rustflags":["-C","link-arg=-fuse-ld=mold"],"toolchain":toolchain}

    def write_meta(self,name,value):
        self.require_accepting()
        data=canonical_json(value); fd=os.open(name,os.O_RDWR|os.O_CREAT|os.O_EXCL|os.O_NOFOLLOW|os.O_CLOEXEC,0o600,dir_fd=self.meta_fd)
        self.fds.append(fd); self.expect_private_create(self.meta_fd,name); self.write_all(fd,data); os.fsync(fd)
        if self.read_held(fd,len(data))!=data: fail("normalized metadata seal differs")
        self.evidence_files[f"meta/{name}"]=(fd,len(data),sha_bytes(data))
        self.enforce_evidence_aggregate()
        self.boundary(f"normalized-metadata:{name}")
        return {"path":f"meta/{name}","bytes":len(data),"sha256":sha_bytes(data),"fd":fd,**self.inode_claim(fd)}

    def graph_facts(self):
        result=[]
        for role,path in sorted(INHERITED_PATHS.items()):
            node=os.fstat(INHERITED_FDS[role]); parent=os.stat(os.path.dirname(path),follow_symlinks=False); digest=None
            if stat.S_ISREG(node.st_mode):
                hasher=hashlib.sha256(); offset=0
                while True:
                    block=os.pread(INHERITED_FDS[role],CHUNK,offset)
                    if not block: break
                    offset+=len(block); hasher.update(block)
                digest=hasher.hexdigest()
            result.append({"role":f"launch:{role}","path":path,"device":node.st_dev,"inode":node.st_ino,"uid":node.st_uid,"gid":node.st_gid,"type":"directory" if stat.S_ISDIR(node.st_mode) else "regular","mode":format(stat.S_IMODE(node.st_mode),"04o"),"link_count":node.st_nlink,"size":node.st_size,"mtime_ns":node.st_mtime_ns,"ctime_ns":node.st_ctime_ns,"sha256":digest,"parent":{"device":parent.st_dev,"inode":parent.st_ino}})
        for label,(path,fd,dev,ino,size,mtime,mode,nlink) in sorted(self.nodes.items()):
            node=os.fstat(fd); parent=os.stat(os.path.dirname(path),follow_symlinks=False); digest=None
            if stat.S_ISREG(node.st_mode):
                hasher=hashlib.sha256(); offset=0
                while True:
                    block=os.pread(fd,CHUNK,offset)
                    if not block: break
                    offset+=len(block); hasher.update(block)
                digest=hasher.hexdigest()
            node_type="directory" if stat.S_ISDIR(node.st_mode) else "symlink" if stat.S_ISLNK(node.st_mode) else "regular"
            result.append({"role":label,"path":path,"device":node.st_dev,"inode":node.st_ino,"uid":node.st_uid,"gid":node.st_gid,"type":node_type,"mode":format(stat.S_IMODE(node.st_mode),"04o"),"link_count":node.st_nlink,"size":node.st_size,"mtime_ns":node.st_mtime_ns,"ctime_ns":node.st_ctime_ns,"sha256":digest,"symlink_target":os.readlink(path) if node_type=="symlink" else None,"parent":{"device":parent.st_dev,"inode":parent.st_ino}})
        for label,(path,target,dev,ino,mtime,ctime) in sorted(self.aliases.items()):
            node=os.lstat(path); parent=os.stat(os.path.dirname(path),follow_symlinks=False)
            result.append({"role":label,"path":path,"device":node.st_dev,"inode":node.st_ino,"uid":node.st_uid,"gid":node.st_gid,"type":"symlink","mode":format(stat.S_IMODE(node.st_mode),"04o"),"link_count":node.st_nlink,"size":node.st_size,"mtime_ns":node.st_mtime_ns,"ctime_ns":node.st_ctime_ns,"sha256":sha_bytes(target.encode()),"symlink_target":target,"parent":{"device":parent.st_dev,"inode":parent.st_ino}})
        return result

    def verify_evidence_tree(self):
        self.require_accepting()
        self.require_quiet_private_tree()
        self.evidence_files["event-journal.jsonl"]=(self.journal_fd,self.journal_bytes,self.journal_hash.hexdigest())
        self.enforce_evidence_aggregate()
        identities=set()
        for path,(fd,size,digest) in self.evidence_files.items():
            node=os.fstat(fd); current_hash=hashlib.sha256(); offset=0
            while True:
                block=os.pread(fd,CHUNK,offset)
                if not block: break
                offset+=len(block); current_hash.update(block)
            if not stat.S_ISREG(node.st_mode) or node.st_uid!=UID or node.st_gid!=GID or stat.S_IMODE(node.st_mode)!=0o600 or node.st_nlink!=1 or node.st_size!=size or current_hash.hexdigest()!=digest: fail(f"evidence inode drifted: {path}")
            identity=(node.st_dev,node.st_ino)
            if identity in identities: fail("aliased evidence inode")
            identities.add(identity)
        for path,fd in self.evidence_dirs.items():
            node=os.fstat(fd)
            if not stat.S_ISDIR(node.st_mode) or node.st_uid!=UID or node.st_gid!=GID or stat.S_IMODE(node.st_mode)!=0o700: fail(f"evidence directory drifted: {path}")
        observed_dirs=set(); observed_files=set()
        expected_dir_identities={path:self.identity(fd) for path,fd in self.evidence_dirs.items() if path!="."}
        expected_file_identities={path:self.identity(fd) for path,(fd,_,_) in self.evidence_files.items()}
        def walk(fd,prefix):
            for name in os.listdir(fd):
                child=os.stat(name,dir_fd=fd,follow_symlinks=False); rel=f"{prefix}/{name}" if prefix else name
                if stat.S_ISDIR(child.st_mode):
                    if (child.st_dev,child.st_ino)!=expected_dir_identities.get(rel): fail(f"evidence directory path/fd disagreement: {rel}")
                    observed_dirs.add(rel); child_fd=os.open(name,os.O_RDONLY|os.O_DIRECTORY|os.O_NOFOLLOW|os.O_CLOEXEC,dir_fd=fd)
                    try: walk(child_fd,rel)
                    finally: os.close(child_fd)
                elif stat.S_ISREG(child.st_mode):
                    if (child.st_dev,child.st_ino)!=expected_file_identities.get(rel): fail(f"evidence file path/fd disagreement: {rel}")
                    observed_files.add(rel)
                else: fail(f"nonregular evidence node: {rel}")
        walk(self.run_fd,"")
        if observed_dirs != (set(self.evidence_dirs)-{"."}) or observed_files != set(self.evidence_files): fail("evidence tree inventory is incomplete or has extras")
        self.validate_journal_coverage()

    def validate_journal_coverage(self):
        parsed=[]; offset=0; pending=b""
        while True:
            block=os.pread(self.journal_fd,RESIDENT_LIMITS.chunk_bytes,offset)
            if not block: break
            offset+=len(block); pending+=block
            if b"\n" not in pending: self.require_journal_line(len(pending))
            while b"\n" in pending:
                line,pending=pending.split(b"\n",1)
                if not line: fail("event journal line shape differs")
                self.require_journal_line(len(line)); self.require_journal_events(len(parsed)+1)
                parsed.append(strict_json_load(line,"event journal record"))
        if pending or parsed!=self.journal_events: fail("event journal bytes/events differ")
        if [event["event_sequence"] for event in self.journal_events]!=list(range(1,len(self.journal_events)+1)): fail("event journal sequence is noncontiguous")
        if any(self.journal_events[index]["monotonic_ns"]>self.journal_events[index+1]["monotonic_ns"] for index in range(len(self.journal_events)-1)): fail("event journal time is nonmonotonic")
        schemas={
          "command-start":({"event_sequence","monotonic_ns","type","command_id","name","phase","argv","started_at"},set()),
          "raw-chunk":({"event_sequence","monotonic_ns","type","command_id","stream","chunk_sequence","offset","byte_count","sha256"},set()),
          "stream-eof":({"event_sequence","monotonic_ns","type","command_id","stream","chunk_count","byte_count","sha256"},set()),
          "command-finish":({"event_sequence","monotonic_ns","type","command_id","wait","finished_at","rusage"},set()),
          "component-boundary":({"event_sequence","monotonic_ns","type","command_id","component_sequence","component","raw_stream","raw_offset","raw_event_sequence"},set()),
          "runtime-tree-event":({"event_sequence","monotonic_ns","type","command_id","tree","mask","leaf"},set()),
          "boundary":({"event_sequence","monotonic_ns","type","boundary"},{"phase","selected_threads","fast_median","full_median"}),
        }
        for event in parsed:
            if event.get("type") not in schemas: fail("event journal record type differs")
            exact_keys(event,*schemas[event["type"]],"event journal record")
            if type(event["event_sequence"]) is not int or type(event["monotonic_ns"]) is not int: fail("event journal numeric type differs")
        for record in self.records:
            command_id=record["sequence"]; events=[event for event in self.journal_events if event.get("command_id")==command_id and event["type"] in ("command-start","raw-chunk","stream-eof","command-finish")]
            starts=[event for event in events if event["type"]=="command-start"]; finishes=[event for event in events if event["type"]=="command-finish"]
            if len(starts)!=1 or len(finishes)!=1 or events[0] is not starts[0] or events[-1] is not finishes[0]: fail("command journal brackets differ")
            if starts[0]["name"]!=record["name"] or starts[0]["phase"]!=record["phase"] or starts[0]["argv"]!=record["argv"] or finishes[0]["wait"]!=record["wait"]: fail("command journal/status claims differ")
            for stream in ("stdout","stderr"):
                chunks=[event for event in events if event["type"]=="raw-chunk" and event["stream"]==stream]
                eofs=[event for event in events if event["type"]=="stream-eof" and event["stream"]==stream]
                if len(eofs)!=1 or [event["chunk_sequence"] for event in chunks]!=list(range(1,len(chunks)+1)): fail("raw journal chunk cardinality differs")
                offset=0; digest=hashlib.sha256(); fd=self.log_fds[(command_id,stream)]
                for event in chunks:
                    if event["offset"]!=offset or event["byte_count"]<=0: fail("raw journal byte coverage is noncontiguous")
                    block=os.pread(fd,event["byte_count"],offset)
                    if len(block)!=event["byte_count"] or sha_bytes(block)!=event["sha256"]: fail("raw journal chunk digest differs")
                    offset+=len(block); digest.update(block)
                eof=eofs[0]; claim=record["logs"][stream]
                if eof["chunk_count"]!=len(chunks) or eof["byte_count"]!=offset or eof["sha256"]!=digest.hexdigest() or claim["bytes"]!=offset or claim["sha256"]!=digest.hexdigest(): fail("raw journal EOF/status claims differ")
            status_fd=self.evidence_files[record["status"]["path"]][0]
            status=strict_json_load(self.read_held(status_fd,record["status"]["bytes"]),"command status",1024*1024)
            exact_keys(status,{"sequence","name","phase","argv","started_at","finished_at","started_monotonic_ns","finished_monotonic_ns","wait","timing","logs"},set(),"command status")
            if type(status["sequence"]) is not int or not isinstance(status["argv"],list) or status["name"]!=record["name"] or status["phase"]!=record["phase"]: fail("command status field type differs")
            exact_keys(status["wait"],{"kind","code","signal"},set(),"command wait status")
            exact_keys(status["timing"],{"wall_seconds","user_seconds","system_seconds","max_rss_kib"},set(),"command timing status")
            exact_keys(status["logs"],{"stdout","stderr"},set(),"command log status")
            for stream in ("stdout","stderr"):
                exact_keys(status["logs"][stream],{"path","bytes","sha256","chunks","device","inode","uid","gid","mode","link_count"},set(),"command stream status")
                if type(status["logs"][stream]["bytes"]) is not int or type(status["logs"][stream]["chunks"]) is not int: fail("command stream count type differs")

    def evidence_tree_facts(self):
        result=[]
        for path,fd in self.evidence_dirs.items(): result.append({"path":path,"type":"directory",**self.inode_claim(fd)})
        for path,(fd,size,digest) in self.evidence_files.items(): result.append({"path":path,"type":"regular","bytes":size,"sha256":digest,**self.inode_claim(fd)})
        return sorted(result,key=lambda item:item["path"])

    def positioned_lines(self,record):
        command_id=record["sequence"]
        pending={"stdout":b"","stderr":b""}; starts={"stdout":0,"stderr":0}; positioned=[]; line_counts={"stdout":0,"stderr":0}
        raw_events=[event for event in self.journal_events if event.get("command_id")==command_id and event["type"] in ("raw-chunk","stream-eof")]
        for event in raw_events:
            stream=event["stream"]
            if event["type"]=="raw-chunk":
                fd=self.log_fds[(command_id,stream)]; block=os.pread(fd,event["byte_count"],event["offset"])
                if len(block)!=event["byte_count"] or sha_bytes(block)!=event["sha256"]: fail("positioned raw chunk differs")
                pending[stream]+=block
                if b"\n" not in pending[stream]: self.require_raw_line_bytes(len(pending[stream]))
                while b"\n" in pending[stream]:
                    line,pending[stream]=pending[stream].split(b"\n",1)
                    self.require_raw_line_bytes(len(line))
                    line_counts[stream]+=1; self.require_raw_lines(line_counts[stream])
                    positioned.append({"stream":stream,"offset":starts[stream],"event_sequence":event["event_sequence"],"line":line.decode("utf-8","strict")})
                    starts[stream]+=len(line)+1
            elif pending[stream]:
                self.require_raw_line_bytes(len(pending[stream])); line_counts[stream]+=1; self.require_raw_lines(line_counts[stream])
                positioned.append({"stream":stream,"offset":starts[stream],"event_sequence":event["event_sequence"],"line":pending[stream].decode("utf-8","strict")})
                starts[stream]+=len(pending[stream]); pending[stream]=b""
        for stream in ("stdout","stderr"):
            if starts[stream]!=record["logs"][stream]["bytes"]: fail("positioned raw coverage differs")
        return sorted(positioned,key=lambda item:(item["event_sequence"],item["stream"],item["offset"]))

    def normalize_lane(self,record,full,doctests):
        self.boundary(f"before-normalize:{record['name']}")
        summaries=[]; headers=[]; model_validator=[]; provider_validator=[]; endings=[]; markers=[]
        for position in self.positioned_lines(record):
            line=position["line"]
            match=re.fullmatch(r"Summary \[[^]]+\] ([0-9]+) tests? run: ([0-9]+) passed, ([0-9]+) skipped",line)
            if match: summaries.append((position,tuple(map(int,match.groups())))); markers.append(("nextest",position))
            match=re.fullmatch(r"\s*Doc-tests\s+(.+)",line)
            if match: headers.append((position,match.group(1))); markers.append(("doctest-header",position))
            if "rsi-model-control-validate" in line and "Running" in line: model_validator.append(position); markers.append(("model-validator",position))
            if "rsi-provider-capability-validate" in line and "Running" in line: provider_validator.append(position); markers.append(("provider-validator",position))
            match=re.fullmatch(r"test result: (ok|FAILED)\. ([0-9]+) passed; ([0-9]+) failed; ([0-9]+) ignored; [0-9]+ measured; [0-9]+ filtered out; finished in .+",line)
            if match: endings.append((position,match.groups())); markers.append(("doctest-ending",position))
        if len(summaries)!=1: fail(f"{record['name']} missing or duplicate Nextest ending")
        total,passed,skipped=summaries[0][1]
        if passed<=0 or total!=passed+skipped: fail(f"{record['name']} Nextest ending is red")
        components=[{"name":"nextest","exit_status":0,"passed":passed,"failed":0,"skipped":skipped}]
        component_positions=[summaries[0][0]]
        if full:
            expected=[identity.split("::",1)[1].replace("-","_") for identity in doctests]
            if [value for _,value in headers]!=expected: fail("metadata-derived doctest identities/order differ from raw Make output")
            if len(endings)!=len(doctests) or any(item[1][0]!="ok" or int(item[1][2])!=0 for item in endings): fail("doctest ending cardinality/status differs")
            expected_markers=["nextest"]+sum((["doctest-header","doctest-ending"] for _ in doctests),[])+["model-validator","provider-validator"]
            # R5_GUARD_COMPONENT_ORDER
            if [name for name,_ in markers]!=expected_markers: fail("full component journal order differs")
            if len(model_validator)!=1: fail("model validator ending cardinality differs")
            if len(provider_validator)!=1: fail("provider validator ending cardinality differs")
            components.append({"name":"doctests","exit_status":0,"identities":doctests,"passed":sum(int(item[1][1]) for item in endings),"failed":0,"skipped":sum(int(item[1][3]) for item in endings)})
            components.append({"name":"model-control-validator","exit_status":0,"invocations":1})
            components.append({"name":"provider-capability-validator","exit_status":0,"invocations":1})
            component_positions.extend([endings[-1][0],model_validator[0],provider_validator[0]])
        normalized={"schema_version":1,"command_sequence":record["sequence"],"name":record["name"],"components":components,"stdout_sha256":record["logs"]["stdout"]["sha256"],"stderr_sha256":record["logs"]["stderr"]["sha256"]}
        for index,(component,position) in enumerate(zip(components,component_positions),1): self.journal({"type":"component-boundary","command_id":record["sequence"],"component_sequence":index,"component":component["name"],"raw_stream":position["stream"],"raw_offset":position["offset"],"raw_event_sequence":position["event_sequence"]})
        return normalized,self.write_meta(f"{record['sequence']:03d}-{record['name']}.normalized.json",normalized)

    def source_events(self):
        result=[]; total=0
        names=((0x00000002,"IN_MODIFY"),(0x00000004,"IN_ATTRIB"),(0x00000008,"IN_CLOSE_WRITE"),
          (0x00000040,"IN_MOVED_FROM"),(0x00000080,"IN_MOVED_TO"),(0x00000100,"IN_CREATE"),
          (0x00000200,"IN_DELETE"),(0x00000400,"IN_DELETE_SELF"),(0x00000800,"IN_MOVE_SELF"),(0x00004000,"IN_Q_OVERFLOW"))
        while True:
            try: watch_data=os.read(self.inotify_fd,RESIDENT_LIMITS.chunk_bytes)
            except BlockingIOError: break
            if not watch_data: break
            total+=len(watch_data)
            if total>RESIDENT_LIMITS.journal_line_bytes: fail("source mutation event evidence exceeds bound")
            offset=0; read_ns=time.monotonic_ns()
            while offset<len(watch_data):
                if len(watch_data)-offset<16: fail("source mutation event record is truncated")
                wd,mask,cookie,length=struct.unpack_from("iIII",watch_data,offset); offset+=16
                if length>len(watch_data)-offset: fail("source mutation event name is truncated")
                raw_name=watch_data[offset:offset+length].split(b"\0",1)[0]; offset+=length
                leaf=raw_name.decode("utf-8","replace")
                result.append({"watch_descriptor":wd,"raw_mask":mask,"mask_names":[name for bit,name in names if mask&bit],
                  "cookie":cookie,"watch_path":self.source_watch_info.get(wd,"unknown"),"leaf":leaf,"event_monotonic_ns":read_ns})
        return result

    def reprove_held_graph(self):
        for role,path in INHERITED_PATHS.items():
            held=os.fstat(INHERITED_FDS[role]); current=os.stat(path,follow_symlinks=False); expected=INHERITED_FACTS[role]
            if (held.st_dev,held.st_ino,held.st_size,held.st_mtime_ns,stat.S_IMODE(held.st_mode),held.st_nlink)!=expected or (current.st_dev,current.st_ino)!=expected[:2]: fail(f"inherited launch descriptor drifted: {role}")
        for label,(path,fd,dev,ino,size,mtime,mode,nlink) in self.nodes.items():
            held=os.fstat(fd)
            try: current=os.stat(path,follow_symlinks=False)
            except FileNotFoundError: fail(f"authenticated node disappeared: {label}")
            if label == "target" or (stat.S_ISDIR(held.st_mode) and path in {entry["path"] for entry in self.cargo_discovery}):
                if (held.st_dev,held.st_ino,stat.S_IMODE(held.st_mode))!=(dev,ino,mode): fail("target identity drifted")
            elif (held.st_dev,held.st_ino,held.st_size,held.st_mtime_ns,stat.S_IMODE(held.st_mode),held.st_nlink)!=(dev,ino,size,mtime,mode,nlink): fail(f"held node drifted: {label}")
            if (current.st_dev,current.st_ino)!=(dev,ino): fail(f"path no longer names held node: {label}")
            # FR_GUARD_AUTHORITY_HASH
            if label in self.authority_hashes and self.hash_fd(fd)!=self.authority_hashes[label]: fail(f"authenticated authority content drifted: {label}")
        for label,(path,target,dev,ino,mtime,ctime) in self.aliases.items():
            node=os.lstat(path)
            if not stat.S_ISLNK(node.st_mode) or os.readlink(path)!=target or (node.st_dev,node.st_ino,node.st_mtime_ns,node.st_ctime_ns)!=(dev,ino,mtime,ctime): fail(f"authenticated tool alias drifted: {label}")
        self.reprove_publication()

    def persist_terminal_source_rejection(self,events,post_disposition):
        if self.terminal_recorded or not hasattr(self,"terminal_fd"): return
        active=self.active_child or {}
        first_sequence=self.event_sequence+1
        value={"schema_version":1,"accepted":False,"actor_attribution":"UNKNOWN","records":[
          {"terminal_sequence":first_sequence,"type":"source-mutation","boundary":self.current_boundary,"phase":self.current_phase,
           "active_command":{"command_id":active.get("command_id"),"name":active.get("name"),"argv":active.get("argv"),"pid":active.get("pid"),"pgid":active.get("pgid")},"events":events},
          {"terminal_sequence":first_sequence+1,"type":"terminal-rejection","reason":"authenticated source/config/tool mutation watch fired","post_event_reproof":post_disposition}]}
        encoded=canonical_json(value)
        if len(encoded)>RESIDENT_LIMITS.journal_line_bytes: fail("terminal mutation/rejection record exceeds bound")
        os.ftruncate(self.terminal_fd,0); os.lseek(self.terminal_fd,0,os.SEEK_SET); self.write_all(self.terminal_fd,encoded); os.fsync(self.terminal_fd)
        if self.read_held(self.terminal_fd,len(encoded))!=encoded: fail("terminal mutation/rejection record seal differs")
        self.validate_terminal_source_rejection(encoded)
        self.evidence_files["terminal-rejection.json"]=(self.terminal_fd,len(encoded),sha_bytes(encoded)); self.terminal_recorded=True

    def validate_terminal_source_rejection(self,data):
        value=strict_json_load(data,"terminal mutation/rejection record",RESIDENT_LIMITS.journal_line_bytes)
        exact_keys(value,{"schema_version","accepted","actor_attribution","records"},set(),"terminal mutation/rejection record")
        if value["schema_version"]!=1 or value["accepted"] is not False or value["actor_attribution"]!="UNKNOWN" or not isinstance(value["records"],list) or len(value["records"])!=2: fail("terminal mutation/rejection record shape differs")
        mutation,rejection=value["records"]
        exact_keys(mutation,{"terminal_sequence","type","boundary","phase","active_command","events"},set(),"terminal source-mutation record")
        exact_keys(rejection,{"terminal_sequence","type","reason","post_event_reproof"},set(),"terminal rejection record")
        if mutation["type"]!="source-mutation" or rejection["type"]!="terminal-rejection" or rejection["terminal_sequence"]!=mutation["terminal_sequence"]+1: fail("terminal mutation/rejection sequence differs")
        exact_keys(mutation["active_command"],{"command_id","name","argv","pid","pgid"},set(),"terminal active command")
        if not isinstance(mutation["events"],list) or not mutation["events"]: fail("terminal source event set is empty")
        for event in mutation["events"]:
            exact_keys(event,{"watch_descriptor","raw_mask","mask_names","cookie","watch_path","leaf","event_monotonic_ns"},set(),"terminal source event")
            if type(event["raw_mask"]) is not int or type(event["event_monotonic_ns"]) is not int or not isinstance(event["mask_names"],list): fail("terminal source event type differs")
        exact_keys(rejection["post_event_reproof"],{"status","error","monotonic_ns"},set(),"terminal post-event reproof")
        if rejection["post_event_reproof"]["status"] not in ("PASS","FAIL") or type(rejection["post_event_reproof"]["monotonic_ns"]) is not int: fail("terminal post-event reproof differs")
        return value

    def reprove(self):
        self.reprove_cargo_discovery()
        cargo_names={"config.toml","config","credentials.toml","credentials"}
        cargo_paths={entry["path"] for entry in self.cargo_discovery}
        cargo_parents={(entry["parent"],entry["leaf"]) for entry in self.cargo_discovery}
        publication=getattr(self,"publication",None)
        publication_owned=set() if publication is None else {publication.get("private"),publication.get("final")}
        events=[event for event in self.source_events() if not (
          (event["watch_path"] in cargo_paths and event["leaf"] in cargo_names) or
          (event["watch_path"],event["leaf"]) in cargo_parents or
          (publication is not None and event["watch_path"]==f"{REPO}/metrics" and event["leaf"] in publication_owned))]
        # FR_GUARD_SOURCE_EVENTS
        if events:
            disposition={"status":"PASS","error":None,"monotonic_ns":time.monotonic_ns()}
            try: self.reprove_held_graph()
            except BaseException as error: disposition={"status":"FAIL","error":str(error),"monotonic_ns":time.monotonic_ns()}
            self.persist_terminal_source_rejection(events,disposition)
            self.rejected=True
            fail(f"authenticated source/config/tool mutation watch fired: {[(event['watch_path'],event['raw_mask'],event['leaf']) for event in events]}")
        self.reprove_held_graph()

    def watch_publication_parent(self,parent_fd,path):
        libc=ctypes.CDLL(None,use_errno=True); self.publication_inotify_fd=libc.inotify_init1(os.O_NONBLOCK|os.O_CLOEXEC)
        if self.publication_inotify_fd<0: fail("cannot create publication watch")
        self.fds.append(self.publication_inotify_fd)
        mask=0x00000100|0x00000200|0x00000040|0x00000080|0x00000004|0x00000400|0x00000800
        wd=libc.inotify_add_watch(self.publication_inotify_fd,ctypes.c_char_p(f"/proc/self/fd/{parent_fd}".encode()),ctypes.c_uint32(mask))
        if wd<0: fail("cannot watch publication parent")
        self.publication_watch=wd

    def publication_events(self,wait=False):
        if wait:
            waiter=selectors.DefaultSelector(); waiter.register(self.publication_inotify_fd,selectors.EVENT_READ); waiter.select(1.0); waiter.close()
        events=[]
        while True:
            try: data=os.read(self.publication_inotify_fd,RESIDENT_LIMITS.chunk_bytes)
            except BlockingIOError: break
            if not data: break
            offset=0
            while offset<len(data):
                wd,mask,cookie,length=struct.unpack_from("iIII",data,offset); offset+=16
                name=data[offset:offset+length].split(b"\0",1)[0].decode("utf-8","strict"); offset+=length
                if mask&0x00004000: fail("publication watch overflowed")
                events.append((wd,mask,name))
        return events

    def publication_parent_facts(self):
        publication=self.publication; fd=publication["parent_fd"]; node=os.fstat(fd)
        named=os.stat(f"{REPO}/metrics",follow_symlinks=False)
        if (named.st_dev,named.st_ino)!=(node.st_dev,node.st_ino): fail("publication parent path/fd identity differs")
        if not stat.S_ISDIR(node.st_mode) or node.st_uid!=UID or node.st_gid!=GID or stat.S_IMODE(node.st_mode)&0o022: fail("publication parent custody differs")
        entries=sorted(os.listdir(fd))
        return (node.st_dev,node.st_ino,stat.S_IMODE(node.st_mode),node.st_size,node.st_mtime_ns,node.st_ctime_ns,tuple(entries))

    def transition_publication(self,next_state,expected_mask,expected_name):
        publication=self.publication; events=self.publication_events(wait=True)
        normalized=[(mask&(0x00000100|0x00000200|0x00000040|0x00000080|0x00000004|0x00000400|0x00000800),name) for wd,mask,name in events if wd==self.publication_watch]
        expected=set(publication["base_entries"])
        if next_state in ("private","private+final"): expected.add(publication["private"])
        if next_state in ("private+final","final"): expected.add(publication["final"])
        facts=self.publication_parent_facts()
        # R5_GUARD_PUBLICATION_TRANSITION
        if normalized != [(expected_mask,expected_name)] or set(facts[-1])!=expected: fail("publication-parent transition event or entry state differs")
        publication["state"]=next_state; publication["parent_facts"]=facts

    def reprove_publication(self):
        if self.publication is None: return
        if self.publication_events(): fail("unexpected publication-parent event")
        if self.publication_parent_facts()!=self.publication["parent_facts"]: fail("publication-parent state drifted")

    def watch_candidate(self,fd):
        libc=ctypes.CDLL(None,use_errno=True); self.candidate_inotify_fd=libc.inotify_init1(os.O_NONBLOCK|os.O_CLOEXEC)
        if self.candidate_inotify_fd<0: fail("cannot create candidate watch")
        self.fds.append(self.candidate_inotify_fd); mask=0x00000002|0x00000004|0x00000008|0x00000400|0x00000800
        self.candidate_watch=libc.inotify_add_watch(self.candidate_inotify_fd,ctypes.c_char_p(f"/proc/self/fd/{fd}".encode()),ctypes.c_uint32(mask))
        if self.candidate_watch<0: fail("cannot watch metric candidate")

    def candidate_events(self):
        masks=[]
        while True:
            try: data=os.read(self.candidate_inotify_fd,RESIDENT_LIMITS.chunk_bytes)
            except BlockingIOError: break
            if not data: break
            offset=0
            while offset<len(data):
                wd,mask,cookie,length=struct.unpack_from("iIII",data,offset); offset+=16+length
                if wd==self.candidate_watch: masks.append(mask)
        return masks

    def candidate_snapshot(self,fd):
        node=os.fstat(fd); digest=hashlib.sha256(); offset=0
        while True:
            block=os.pread(fd,RESIDENT_LIMITS.chunk_bytes,offset)
            if not block: break
            offset+=len(block); digest.update(block)
        return (node.st_dev,node.st_ino,node.st_uid,node.st_gid,node.st_size,node.st_mtime_ns,node.st_ctime_ns,stat.S_IMODE(node.st_mode),node.st_nlink,digest.hexdigest())

    def reprove_candidate(self,expected_links,allow_link_attrib=False):
        publication=self.publication; fd=publication["candidate_fd"]; events=self.candidate_events()
        current=self.candidate_snapshot(fd); expected=publication["candidate_facts"]
        expected_events=[0x00000004] if allow_link_attrib else []
        names=(publication["private"],) if publication["state"]=="private" else (publication["private"],publication["final"]) if publication["state"]=="private+final" else (publication["final"],)
        path_identities=[]
        for name in names:
            node=os.stat(name,dir_fd=publication["parent_fd"],follow_symlinks=False); path_identities.append((node.st_dev,node.st_ino))
        same_facts=(current[:6]==expected[:6] and (allow_link_attrib or current[6]==expected[6]) and current[2]==UID and current[3]==GID and current[7]==0o600 and current[8]==expected_links and current[9]==expected[9] and all(identity==current[:2] for identity in path_identities))
        # R5_GUARD_CANDIDATE_CONTENT
        if events!=expected_events or not same_facts: fail("metric candidate bytes, events, path, or identity drifted")
        publication["candidate_facts"]=current

    def require_output_bound(self,size):
        # R5_GUARD_OUTPUT_BYTES
        if type(size) is not int or size>RESIDENT_LIMITS.output_bytes: fail("metric candidate exceeds output bound")

    def enumerate_identities(self,record):
        document=self.read_json_log(record,"stdout","Nextest enumeration")
        return self.enumerate_document(document)

    def enumerate_document(self,document):
        exact_keys(document,{"rust-suites","test-count"},set(),"Nextest enumeration")
        suites=document.get("rust-suites")
        if not isinstance(suites,dict) or type(document["test-count"]) is not int or document["test-count"]<0: fail("Nextest enumeration count schema differs")
        # R5_GUARD_SUITE_COUNT
        if len(suites)>RESIDENT_LIMITS.suites: fail("Nextest suite count exceeds bound")
        runnable=[]; ignored=[]
        for binary,suite in suites.items():
            if not isinstance(binary,str) or not binary or len(binary.encode())>RESIDENT_LIMITS.string_bytes: fail("invalid Nextest binary identity")
            exact_keys(suite,{"binary-id","testcases"},set(),"Nextest suite")
            if suite["binary-id"]!=binary or not isinstance(suite["testcases"],dict): fail("Nextest suite binary identity differs")
            for test,meta in suite["testcases"].items():
                if not isinstance(test,str) or not test or len(test.encode())>RESIDENT_LIMITS.string_bytes: fail("Nextest testcase identity differs")
                exact_keys(meta,{"ignored","filter-match"},set(),"Nextest testcase")
                if type(meta["ignored"]) is not bool: fail("Nextest ignored status differs")
                exact_keys(meta["filter-match"],{"status"},set(),"Nextest filter match")
                if meta["filter-match"]["status"]!="matches": fail("Nextest testcase is filtered")
                identity=f"{binary}::{test}"
                (ignored if meta.get("ignored") else runnable).append(identity)
        # R5_GUARD_TESTCASE_IDENTITY_COUNT
        if any(len(suite["testcases"])>RESIDENT_LIMITS.testcases for suite in suites.values()) or len(runnable)+len(ignored)>RESIDENT_LIMITS.identities: fail("Nextest testcase or identity count exceeds bound")
        # R6_GUARD_TEST_COUNT_RECONCILIATION
        if document["test-count"]!=len(runnable)+len(ignored): fail("Nextest test-count differs from testcase cardinality")
        if len(set(runnable+ignored))!=len(runnable)+len(ignored): fail("duplicate binary-qualified identity")
        return {"runnable":sorted(runnable),"ignored":sorted(ignored)}

    def nextest_observation(self,record,identity,description):
        outcomes={}
        pattern=re.compile(r"^\s*(PASS|FAIL|SKIP)\s+\[[^]]+\](?: \([^)]*\))?\s+(\S+)\s+(.+)$")
        for stream in ("stdout","stderr"):
            for line in self.lines(record,stream):
                match=pattern.fullmatch(line)
                if match:
                    key=f"{match.group(2)}::{match.group(3)}"
                    if key in outcomes: fail(f"duplicate Nextest raw outcome: {record['name']}: {key}")
                    outcomes[key]=match.group(1)
        expected=set(identity["runnable"]+identity["ignored"])
        if set(outcomes)!=expected: fail(f"Nextest executed identity union differs: {record['name']}")
        if any(outcomes[value]!="SKIP" for value in identity["ignored"]) or any(outcomes[value]!="PASS" for value in identity["runnable"]): fail(f"Nextest raw outcome differs: {record['name']}")
        executed=sorted(outcomes); proof={"mode":"descriptive","declared":[description],"executed":executed,"verified":True}
        return {"passed_lines":len(identity["runnable"]),"failed_lines":0,"ignored_lines":len(identity["ignored"]),"failure_names":[],"identity_proof":proof}

    def validate_held_out(self,lane,samples,signatures,reference,budget):
        if len(samples)!=3 or len(signatures)!=3 or len(reference)!=3 or signatures!=reference: fail(f"held-out {lane} compatibility differs")
        timing_stats=stats([sample["timing"]["wall_seconds"] for sample in samples])
        if timing_stats["median"]>budget["limit_wall_seconds"]: fail(f"held-out {lane} median exceeds budget")
        return {"statistics":timing_stats,"samples":[{"timing":sample["timing"],"observed":signature,"status":sample["status"],"logs":sample["logs"]} for sample,signature in zip(samples,signatures)]}

    def libtest_observation(self,record,test_identity):
        passed=failed=ignored=0; seen=[]
        summary=re.compile(r"test result: (ok|FAILED)\. ([0-9]+) passed; ([0-9]+) failed; ([0-9]+) ignored; [0-9]+ measured; [0-9]+ filtered out; finished in .+")
        for stream in ("stdout","stderr"):
            for line in self.lines(record,stream):
                match=re.fullmatch(r"test (.+) \.\.\. (ok|FAILED|ignored.*)",line)
                if match: seen.append((match.group(1),match.group(2)))
                match=summary.fullmatch(line)
                if match:
                    if match.group(1)!="ok": fail(f"red libtest summary: {record['name']}")
                    passed+=int(match.group(2)); failed+=int(match.group(3)); ignored+=int(match.group(4))
        if seen!=[(test_identity,"ok")] or (passed,failed,ignored)!=(1,0,0): fail(f"libtest identity/count differs: {record['name']}")
        return {"passed_lines":1,"failed_lines":0,"ignored_lines":0,"failure_names":[],"identity_proof":{"mode":"exact","declared":[test_identity],"executed":[test_identity],"verified":True}}

    def validate_cargo_metadata_document(self,metadata):
        exact_keys(metadata,{"packages","workspace_members","workspace_default_members","resolve","target_directory","version","workspace_root","metadata"},set(),"Cargo metadata")
        if type(metadata["version"]) is not int or metadata["version"]!=1 or not isinstance(metadata["packages"],list) or len(metadata["packages"])>10000: fail("Cargo metadata schema differs")
        doctests=[]; target_count=0
        package_optional={"version","id","license","license_file","description","source","dependencies","features","manifest_path","metadata","publish","authors","categories","keywords","readme","repository","homepage","documentation","edition","links","default_run","rust_version"}
        target_required={"name","kind","crate_types","required-features","src_path","edition","doctest","test","doc"}
        for package in metadata["packages"]:
            exact_keys(package,{"name","targets"},package_optional,"Cargo package metadata")
            if not isinstance(package["name"],str) or not package["name"] or not isinstance(package["targets"],list): fail("Cargo package metadata schema differs")
            for target in package["targets"]:
                target_count+=1
                # R5_GUARD_CARGO_TARGET_COUNT
                if target_count>RESIDENT_LIMITS.identities: fail("Cargo target count exceeds bound")
                exact_keys(target,target_required,set(),"Cargo target metadata")
                if not isinstance(target["name"],str) or not target["name"] or type(target["doctest"]) is not bool or type(target["test"]) is not bool or type(target["doc"]) is not bool or not isinstance(target["kind"],list) or not isinstance(target["crate_types"],list) or not isinstance(target["required-features"],list) or not isinstance(target["src_path"],str) or not isinstance(target["edition"],str): fail("Cargo target metadata schema differs")
                if target["doctest"] is True: doctests.append(f"{package['name']}::{target['name']}")
        if len(doctests)!=len(set(doctests)): fail("Cargo-derived doctest identity is duplicated")
        return doctests

    def cargo_metadata_from_record(self,record):
        metadata=self.read_json_log(record,"stdout","Cargo metadata")
        return metadata,self.validate_cargo_metadata_document(metadata)

    def run(self):
        self.boundary("resident-engine-ready")
        # Descriptor-retained fact and configuration graph.
        nextest_profiles=self.validate_nextest_config()
        cargo_policy=self.validate_cargo_graph()
        facts=[("git-head","git",self.git_policy_argv("rev-parse","--verify","HEAD^{commit}")),("git-branch","git",self.git_policy_argv("symbolic-ref","--short","HEAD")),
          ("git-status","git",self.git_policy_argv("status","--porcelain=v1","--untracked-files=all","--ignore-submodules=none")),("git-diff-check","git",self.git_policy_argv("diff","--check","--ignore-submodules=none")),
          ("git-files","git",self.git_policy_argv("ls-files","-z","--cached")),
          ("snapshots","find",["find",".","-type","f","-name","*.snap.new","-print"]),("nproc","nproc",["nproc"]),
          ("uname","uname",["uname","-a"]),("lscpu","lscpu",["lscpu"]),("cargo-version","cargo",["cargo","--version"]),
          ("rustc-version","rustc",["rustc","--version"]),("rustdoc-version","rustdoc",["rustdoc","--version"]),
          ("nextest-version","cargo-nextest",["cargo-nextest","nextest","--version"]),
          ("cargo-metadata","cargo",["cargo","metadata","--format-version","1","--no-deps"])]
        fact_records={name:self.command(name,tool,args if tool=="git" else [f"/proc/self/fd/{self.tools[tool]}",*args[1:]],"preflight") for name,tool,args in facts}
        for empty in ("git-status","git-diff-check","snapshots"):
            if self.read_log(fact_records[empty],"stdout") or self.read_log(fact_records[empty],"stderr"): fail(f"nonempty clean-source evidence: {empty}")
        if self.read_log(fact_records["git-head"],"stdout") != (self.git_head+"\n").encode(): fail("source HEAD changed")
        if self.read_log(fact_records["git-branch"],"stdout") != (self.git_branch+"\n").encode(): fail("source symbolic branch changed")
        retained_tracked=tuple(raw.decode("utf-8","strict") for raw in self.read_log(fact_records["git-files"],"stdout").split(b"\0") if raw)
        self.reconcile_tracked_paths(retained_tracked)
        self.reconcile_git_index()
        metadata,doctests=self.cargo_metadata_from_record(fact_records["cargo-metadata"])
        if not doctests: fail("Cargo-derived doctest cardinality is invalid")
        fast_enum=self.nextest("nextest-fast-list",["list","--profile","rsid-fast","-p","rsid","--lib","--message-format","json"],"enumeration")
        full_enum=self.nextest("nextest-full-list",["list","--profile","ci-full","--workspace","--message-format","json"],"enumeration")
        identities={"nextest-fast":self.enumerate_identities(fast_enum),"nextest-full":self.enumerate_identities(full_enum)}
        dry_fast=self.make("make-fast-metadata","test-fast",8,"preflight",dry=True); dry_full=self.make("make-full-metadata","test-full",8,"preflight",dry=True)
        dry_fast_text=self.read_log(dry_fast,"stdout").decode("utf-8","strict"); dry_full_text=self.read_log(dry_full,"stdout").decode("utf-8","strict")
        if dry_fast_text.strip()!="cargo nextest run --profile rsid-fast -p rsid --lib -j 8": fail("held Make fast metadata differs")
        expected_full=("status=0; \\\n"
          "cargo nextest run --profile ci-full --workspace -j 8 || status=$?; \\\n"
          "cargo test --workspace --doc || status=$?; \\\n"
          "cargo run -p rsid --bin rsi-model-control-validate --offline || status=$?; \\\n"
          "cargo run -p rsid --bin rsi-provider-capability-validate --offline || status=$?; \\\n"
          "exit $status")
        if dry_full_text.strip()!=expected_full: fail("held Make full metadata differs")
        fast_lane=self.make("make-test-fast","test-fast",8,"preflight"); full_lane=self.make("make-test-full","test-full",8,"preflight")
        fast_normalized,fast_normalized_file=self.normalize_lane(fast_lane,False,doctests)
        full_normalized,full_normalized_file=self.normalize_lane(full_lane,True,doctests)
        self.cargo("store-warmup",["test","-p","rsid","--lib","store::tests::load_sessions_survives_legacy_comma_fraction_timestamp","--no-run"],"capture")
        store=[]
        for index in range(5):
            record=self.cargo(f"store-sample-{index+1}",["test","-p","rsid","--lib","store::tests::load_sessions_survives_legacy_comma_fraction_timestamp","--","--exact","--test-threads","1"],"capture"); self.libtest_observation(record,"store::tests::load_sessions_survives_legacy_comma_fraction_timestamp"); store.append(record["timing"])
        self.cargo("scanner-warmup",["test","-p","rsid","--lib","session::issue21_phase2_tests::p2_07_gate_permit_spine::exactly_one_production_writer_of_gate_closing_and_effect_permits","--no-run"],"capture")
        scanner=[]
        for index in range(5):
            record=self.cargo(f"scanner-sample-{index+1}",["test","-p","rsid","--lib","session::issue21_phase2_tests::p2_07_gate_permit_spine::exactly_one_production_writer_of_gate_closing_and_effect_permits","--","--exact","--test-threads","1"],"capture"); self.libtest_observation(record,"session::issue21_phase2_tests::p2_07_gate_permit_spine::exactly_one_production_writer_of_gate_closing_and_effect_permits"); scanner.append(record["timing"])
        sweep={}
        for threads in (1,8,16,32):
            self.nextest(f"sweep-t{threads}-warmup",["run","--profile","ci-full","--workspace","--test-threads",str(threads),"--no-run"],"sweep")
            sweep[threads]=[]
            for index in range(3):
                record=self.nextest(f"sweep-t{threads}-sample-{index+1}",["run","--profile","ci-full","--workspace","--status-level","all","--final-status-level","all","-j",str(threads)],"sweep"); self.nextest_observation(record,identities["nextest-full"],"workspace nextest full profile"); sweep[threads].append(record)
        self.boundary("before-thread-selection")
        wall={threads:stats([sample["timing"]["wall_seconds"] for sample in samples]) for threads,samples in sweep.items()}
        fastest=min(wall,key=lambda value:(wall[value]["median"],value)); frontier=[value for value in wall if wall[value]["median"]<=wall[fastest]["median"]+wall[fastest]["mad"]]; selected=min(frontier)
        self.boundary("after-thread-selection",selected_threads=selected)
        self.nextest("selected-fast-warmup",["run","--profile","rsid-fast","-p","rsid","--lib","--test-threads",str(selected),"--no-run"],"selected")
        fast=[]
        for index in range(3):
            record=self.nextest(f"selected-fast-sample-{index+1}",["run","--profile","rsid-fast","-p","rsid","--lib","--status-level","all","--final-status-level","all","-j",str(selected)],"selected"); self.nextest_observation(record,identities["nextest-fast"],"rsid library nextest fast profile"); fast.append(record)
        self.boundary("before-held-out")
        held_fast=[]; held_fast_signatures=[]; held_full=[]; held_full_signatures=[]
        for index in range(3):
            record=self.nextest(f"held-out-fast-{index+1}",["run","--profile","rsid-fast","-p","rsid","--lib","--status-level","all","--final-status-level","all","-j",str(selected)],"held-out")
            held_fast.append(record); held_fast_signatures.append(self.nextest_observation(record,identities["nextest-fast"],"rsid library nextest fast profile"))
        for index in range(3):
            record=self.nextest(f"held-out-full-{index+1}",["run","--profile","ci-full","--workspace","--status-level","all","--final-status-level","all","-j",str(selected)],"held-out")
            held_full.append(record); held_full_signatures.append(self.nextest_observation(record,identities["nextest-full"],"workspace nextest full profile"))
        fast_stats=stats([value["timing"]["wall_seconds"] for value in fast]); full_stats=wall[selected]
        fast_signatures=[self.nextest_observation(value,identities["nextest-fast"],"rsid library nextest fast profile") for value in fast]
        full_signatures=[self.nextest_observation(value,identities["nextest-full"],"workspace nextest full profile") for value in sweep[selected]]
        logical_cpus=int(self.read_log(fact_records["nproc"],"stdout").decode().strip()); uname=os.uname()
        cpu_text=self.read_log(fact_records["lscpu"],"stdout").decode("utf-8","strict"); cpu_match=re.search(r"^Model name:\s*(.+)$",cpu_text,re.M)
        if cpu_match is None: fail("CPU model is absent from held lscpu output")
        host={"class":f"{uname.sysname}-{uname.machine}-{logical_cpus}cpu","os":uname.sysname,"kernel":uname.release,"architecture":uname.machine,"cpu_model":cpu_match.group(1).strip(),"logical_cpus":logical_cpus}
        host["fingerprint_sha256"]=sha_bytes("|".join(str(host[key]) for key in ("class","os","kernel","architecture","cpu_model","logical_cpus")).encode())
        target_path=os.path.realpath(metadata["target_directory"])
        if target_path!=f"{REPO}/target": fail("Cargo metadata target differs from closed child target")
        target_node=os.fstat(next(fd for label,(path,fd,*_) in self.nodes.items() if label=="target"))
        target={"canonical_path":target_path,"device":str(target_node.st_dev),"inode":str(target_node.st_ino)}
        target["fingerprint_sha256"]=sha_bytes("|".join(target[key] for key in ("canonical_path","device","inode")).encode())
        toolchain={"cargo":self.read_log(fact_records["cargo-version"],"stdout").decode().rstrip("\n"),"rustc":self.read_log(fact_records["rustc-version"],"stdout").decode().rstrip("\n"),"rustdoc":self.read_log(fact_records["rustdoc-version"],"stdout").decode().rstrip("\n"),"cargo_nextest":self.read_log(fact_records["nextest-version"],"stdout").decode().rstrip("\n"),"cargo_target_dir":target_path}
        fast_command=f"cargo nextest run --profile rsid-fast -p rsid --lib --status-level all --final-status-level all -j {selected}"
        full_command=f"cargo nextest run --profile ci-full --workspace --status-level all --final-status-level all -j {selected}"
        budgets=[{"probe":"nextest-fast","host_class":host["class"],"expected_repeat":3,"median_wall_seconds":fast_stats["median"],"mad_wall_seconds":fast_stats["mad"],"limit_wall_seconds":fast_stats["median"]+3*fast_stats["mad"],"command":fast_command,"test_identity":["rsid library nextest fast profile"],"resolved_threads":selected,"observed_samples":fast_signatures},
          {"probe":"nextest-full","host_class":host["class"],"expected_repeat":3,"median_wall_seconds":full_stats["median"],"mad_wall_seconds":full_stats["mad"],"limit_wall_seconds":full_stats["median"]+3*full_stats["mad"],"command":full_command,"test_identity":["workspace nextest full profile"],"resolved_threads":selected,"observed_samples":full_signatures}]
        held_out={}
        for lane,samples,signatures,reference,budget in (("fast",held_fast,held_fast_signatures,fast_signatures,budgets[0]),("full",held_full,held_full_signatures,full_signatures,budgets[1])):
            held_out[lane]=self.validate_held_out(lane,samples,signatures,reference,budget)
        self.boundary("after-held-out",fast_median=held_out["fast"]["statistics"]["median"],full_median=held_out["full"]["statistics"]["median"])
        self.boundary("before-candidate-generation")
        inventory=[]
        for record in self.records:
            inventory.extend({key:value for key,value in value.items() if key!="chunks"} for value in record["logs"].values())
            inventory.append(record["status"])
        inventory.extend({key:value for key,value in item.items() if key!="fd"} for item in (fast_normalized_file,full_normalized_file))
        os.fsync(self.journal_fd)
        self.verify_evidence_tree()
        inventory.append({"path":"event-journal.jsonl","scope":"candidate-sealed-prefix","bytes":self.journal_bytes,"sha256":self.journal_hash.hexdigest(),**self.inode_claim(self.journal_fd)})
        inventory.sort(key=lambda item:item["path"])
        evidence_tree=self.evidence_tree_facts()
        next(item for item in evidence_tree if item["path"]=="event-journal.jsonl")["scope"]="candidate-sealed-prefix"
        result={"schema_version":2,"origin":{"claim_version":1,"kind":"single-live-producer-custody","acceptance_operation":"calibrate-baseline","threat_model":"hostile-inherited-environment-and-concurrent-same-uid-path-config-mutation-after-conforming-sterile-launch","launch_principal_authenticated":False,"cryptographic_attestation":False,"same_uid_process_control_excluded":True,"mutable_target_contents_excluded":True,"post_exit_authenticity":False,"published_before_producer_exit":True},
          "threat_boundary":THREAT,"formula_version":"median-mad-v1","source":{"head":self.git_head,"branch":self.git_branch,"clean_tree":True},"host":host,"toolchain":toolchain,"target":target,"selected_threads":selected,
          "thread_selection":{"candidates":[1,8,16,32],"statistics":{str(key):value for key,value in wall.items()},"fastest_candidate":fastest,"mad_frontier":sorted(frontier),"selected_smallest_eligible":selected},
          "nextest_profiles":nextest_profiles,"cargo_policy":cargo_policy,"preflight_components":{"fast":fast_normalized,"full":full_normalized},"identities":identities,"doctest_identities":doctests,"measurements":{"store":stats([x["wall_seconds"] for x in store]),"scanner":stats([x["wall_seconds"] for x in scanner]),"nextest-fast":fast_stats,"nextest-full":full_stats},
          "budgets":budgets,
          "held_out":held_out,"commands":self.records,"inventory":inventory,"evidence_tree":evidence_tree,"authenticated_graph":self.graph_facts(),
          "event_journal":{"path":"event-journal.jsonl","candidate_sealed_prefix":{"events":self.event_sequence,"bytes":self.journal_bytes,"sha256":self.journal_hash.hexdigest()},"publication_tail_retained_in_evidence_tree":True}}
        data=(json.dumps(result,sort_keys=True,indent=2,ensure_ascii=True,allow_nan=False)+"\n").encode("ascii")
        self.publish(data)

    def publish(self,data):
        self.require_accepting()
        self.require_output_bound(len(data)); self.seal_journal(); self.verify_evidence_tree(); self.reprove()
        metrics_fd=os.open(f"{REPO}/metrics",os.O_RDONLY|os.O_DIRECTORY|os.O_NOFOLLOW|os.O_CLOEXEC); self.fds.append(metrics_fd)
        parent=os.fstat(metrics_fd)
        if not stat.S_ISDIR(parent.st_mode) or parent.st_uid!=UID or parent.st_gid!=GID or stat.S_IMODE(parent.st_mode)&0o022: fail("publication parent is not private custody")
        base_entries=sorted(os.listdir(metrics_fd))
        if "test-suite-baseline.json" in base_entries: fail("metric output already exists")
        self.watch_publication_parent(metrics_fd,f"{REPO}/metrics")
        for _ in range(64):
            private=f".test-suite-baseline.{secrets.token_hex(16)}"
            try: fd=os.open(private,os.O_RDWR|os.O_CREAT|os.O_EXCL|os.O_NOFOLLOW|os.O_CLOEXEC,0o600,dir_fd=metrics_fd); break
            except FileExistsError: continue
        else: fail("cannot allocate private metric inode")
        self.fds.append(fd); self.write_all(fd,data); os.fsync(fd); os.fsync(metrics_fd); identity=self.identity(fd)
        self.publication={"parent_fd":metrics_fd,"private":private,"final":"test-suite-baseline.json","identity":identity,"linked":False,"state":"absent","base_entries":base_entries,"candidate_fd":fd}
        self.publication["parent_facts"]=self.publication_parent_facts()
        self.transition_publication("private",0x00000100,private)
        node=os.fstat(fd)
        if stat.S_IMODE(node.st_mode)!=0o600 or node.st_nlink!=1: fail("private metric custody differs")
        self.publication["candidate_facts"]=self.candidate_snapshot(fd); self.watch_candidate(fd); self.reprove_candidate(1)
        hook=self.hooks.get("before-exclusive-link")
        if hook is not None: hook(self,"before-exclusive-link")
        self.reprove_candidate(1); self.boundary("before-exclusive-link"); self.verify_evidence_tree(); self.reprove_candidate(1)
        os.link(private,"test-suite-baseline.json",src_dir_fd=metrics_fd,dst_dir_fd=metrics_fd,follow_symlinks=False)
        self.publication["linked"]=True; os.fsync(metrics_fd)
        self.transition_publication("private+final",0x00000100,"test-suite-baseline.json"); self.reprove_candidate(2,allow_link_attrib=True)
        hook=self.hooks.get("after-exclusive-link")
        if hook is not None: hook(self,"after-exclusive-link")
        self.reprove_candidate(2)
        linked=os.stat("test-suite-baseline.json",dir_fd=metrics_fd,follow_symlinks=False)
        if (linked.st_dev,linked.st_ino)!=identity or linked.st_nlink!=2: fail("metric link identity differs")
        self.boundary("after-exclusive-link"); self.verify_evidence_tree(); self.reprove_candidate(2); linked=os.stat("test-suite-baseline.json",dir_fd=metrics_fd,follow_symlinks=False)
        if (linked.st_dev,linked.st_ino)!=identity or linked.st_nlink!=2: fail("metric was replaced after link")
        hook=self.hooks.get("before-private-unlink")
        if hook is not None: hook(self,"before-private-unlink")
        self.reprove_candidate(2); self.boundary("before-private-unlink"); self.reprove_candidate(2)
        os.unlink(private,dir_fd=metrics_fd); os.fsync(metrics_fd); self.transition_publication("final",0x00000200,private); self.reprove_candidate(1,allow_link_attrib=True); final=os.stat("test-suite-baseline.json",dir_fd=metrics_fd,follow_symlinks=False)
        if (final.st_dev,final.st_ino)!=identity or final.st_nlink!=1: fail("final metric identity differs")
        hook=self.hooks.get("after-private-unlink")
        if hook is not None: hook(self,"after-private-unlink")
        self.reprove_candidate(1); os.fsync(metrics_fd); self.boundary("after-private-unlink"); self.verify_evidence_tree(); self.reprove_candidate(1)
        final=os.stat("test-suite-baseline.json",dir_fd=metrics_fd,follow_symlinks=False)
        if (final.st_dev,final.st_ino)!=identity or final.st_nlink!=1 or stat.S_IMODE(final.st_mode)!=0o600: fail("final metric changed during terminal reproof")
        self.final_identity=identity; self.publication=None

def rollback_custody(engine,error):
    engine.rejected=True
    active=getattr(engine,"active_child",None)
    if active is not None:
        try: engine.reject_child(active,error)
        except BaseException: engine.rejected=True
    if getattr(engine,"rejection",None) is None:
        engine.rejection={"reason":str(error),"leader_reaped":active is None,"group_dead":active is None,"cleanup_failure":None if active is None else "outer rollback could not complete active-child cleanup","accepted":False}
    # Roll back only the final inode proven to be ours.  Ambiguous residue is
    # preserved for inspection; no recursive chmod or recursive removal occurs.
    final_identity=getattr(engine,"final_identity",None)
    if final_identity is not None:
        try:
            parent=os.open(f"{REPO}/metrics",os.O_RDONLY|os.O_DIRECTORY|os.O_NOFOLLOW)
            node=os.stat("test-suite-baseline.json",dir_fd=parent,follow_symlinks=False)
            if (node.st_dev,node.st_ino)==final_identity: os.unlink("test-suite-baseline.json",dir_fd=parent)
            os.close(parent)
        except OSError: pass
    publication=getattr(engine,"publication",None)
    if publication is not None:
        parent=publication.get("parent_fd"); expected=publication.get("identity")
        for leaf in (publication.get("final"),publication.get("private")):
            if parent is None or expected is None or leaf is None: continue
            try:
                node=os.stat(leaf,dir_fd=parent,follow_symlinks=False)
                if (node.st_dev,node.st_ino)==expected: os.unlink(leaf,dir_fd=parent)
            except FileNotFoundError: pass

def execute_custody(engine_type=RunCustody):
    engine=object.__new__(engine_type)
    try:
        engine_type.__init__(engine)
        engine.run()
    except BaseException as error:
        rollback_custody(engine,error)
        raise

execute_custody()
PY
    exec {python_fd}>&-
}

compare() {
    local baseline="" candidate=""
    while [ "$#" -gt 0 ]; do case "$1" in --baseline) [ "$#" -ge 2 ] || die "--baseline requires a value"; baseline="$2"; shift 2 ;; --candidate) [ "$#" -ge 2 ] || die "--candidate requires a value"; candidate="$2"; shift 2 ;; *) die "unknown compare argument: $1" ;; esac; done
    [ -f "$baseline" ] || die "baseline measurement not found: $baseline"; [ -f "$candidate" ] || die "candidate measurement not found: $candidate"; require_jq
    jq -n -e --slurpfile baseline "$baseline" --slurpfile candidate "$candidate" '
      def median: sort as $s | length as $n | if $n == 0 then null elif ($n % 2) == 1 then $s[$n / 2 | floor] else ($s[$n / 2 - 1] + $s[$n / 2]) / 2 end;
      def statistics: . as $v | ($v | median) as $m | {count: length, median_wall_seconds: $m, mad_wall_seconds: (map(. - $m | if . < 0 then -. else . end) | median)};
      def signature: [.samples | sort_by(.index)[] | {passed_lines: .observed.passed_lines, failed_lines: .observed.failed_lines, ignored_lines: .observed.ignored_lines, failure_names: .observed.failure_names, identity_proof: .observed.identity_proof}];
      def valid: . as $m | type == "object" and .schema_version == 2 and (($m.capture.probe | type) == "string" and ($m.capture.probe | length) > 0) and $m.capture.source.clean_tree == true and (($m.capture.host.class | type) == "string" and ($m.capture.host.class | length) > 0) and (($m.capture.requested.repeat | type) == "number" and $m.capture.requested.repeat > 0) and (($m.capture.requested.resolved_threads | type) == "number" and $m.capture.requested.resolved_threads > 0) and (($m.capture.execution.command | type) == "string" and ($m.capture.execution.command | length) > 0) and (($m.capture.execution.test_identity | type) == "array" and ($m.capture.execution.test_identity | length) > 0) and $m.capture.warmup.exit_status == 0 and (($m.samples | type) == "array" and ($m.samples | length) == $m.capture.requested.repeat) and ([$m.samples[] | .index] | sort) == [range(1; $m.capture.requested.repeat + 1)] and all($m.samples[]; .execution.exit_status == 0 and .execution.evidence_valid == true and .execution.command == $m.capture.execution.command and ((.execution.timing.wall_seconds | type) == "number" and .execution.timing.wall_seconds >= 0) and ((.observed.passed_lines | type) == "number" and .observed.passed_lines > 0) and .observed.failed_lines == 0 and ((.observed.failure_names | type) == "array" and (.observed.failure_names | length) == 0) and .observed.identity_proof.verified == true);
      ($baseline[0]) as $base | ($candidate[0]) as $candidate | if ($base | valid | not) then error("baseline is incomplete, failed, or incompatible") elif ($candidate | valid | not) then error("candidate is incomplete, failed, or incompatible") elif $base.capture.probe != $candidate.capture.probe then error("probe identity differs") elif $base.capture.host.class != $candidate.capture.host.class then error("host class differs") elif $base.capture.requested.resolved_threads != $candidate.capture.requested.resolved_threads then error("resolved thread count differs") elif $base.capture.execution.command != $candidate.capture.execution.command then error("execution command differs") elif $base.capture.execution.test_identity != $candidate.capture.execution.test_identity then error("test identity differs") elif ($base | signature) != ($candidate | signature) then error("observed counts, failures, or identity proof differ") else ($base.samples | map(.execution.timing.wall_seconds)) as $bw | ($candidate.samples | map(.execution.timing.wall_seconds)) as $cw | ($bw | statistics) as $bs | ($cw | statistics) as $cs | {schema_version: 1, comparison: {probe: $base.capture.probe, command: $base.capture.execution.command, baseline: $bs, candidate: $cs, median_wall_delta_seconds: ($cs.median_wall_seconds - $bs.median_wall_seconds), candidate_is_faster: ($cs.median_wall_seconds < $bs.median_wall_seconds)}} end'
}

check() {
    local baseline="" measurement=""
    while [ "$#" -gt 0 ]; do case "$1" in --baseline) [ "$#" -ge 2 ] || die "--baseline requires a value"; baseline="$2"; shift 2 ;; --measurement) [ "$#" -ge 2 ] || die "--measurement requires a value"; measurement="$2"; shift 2 ;; *) die "unknown check argument: $1" ;; esac; done
    [ -f "$baseline" ] || die "budget baseline not found: $baseline"; [ -f "$measurement" ] || die "measurement not found: $measurement"; require_jq
    jq -n -e --slurpfile baseline "$baseline" --slurpfile measurement "$measurement" '
      def median: sort as $s | length as $n | if $n == 0 then null elif ($n % 2) == 1 then $s[$n / 2 | floor] else ($s[$n / 2 - 1] + $s[$n / 2]) / 2 end;
      def signature: [.samples | sort_by(.index)[] | {passed_lines: .observed.passed_lines, failed_lines: .observed.failed_lines, ignored_lines: .observed.ignored_lines, failure_names: .observed.failure_names, identity_proof: .observed.identity_proof}];
      def nonnegative_number: type == "number" and . >= 0;
      def valid: . as $m | $m.schema_version == 2 and (($m.capture.probe | type) == "string" and ($m.capture.probe | length) > 0) and (($m.capture.host.class | type) == "string" and ($m.capture.host.class | length) > 0) and $m.capture.source.clean_tree == true and $m.capture.warmup.exit_status == 0 and (($m.capture.requested.repeat | type) == "number" and $m.capture.requested.repeat > 0) and (($m.capture.requested.resolved_threads | type) == "number" and $m.capture.requested.resolved_threads > 0) and (($m.capture.execution.command | type) == "string" and ($m.capture.execution.command | length) > 0) and (($m.capture.execution.test_identity | type) == "array" and ($m.capture.execution.test_identity | length) > 0) and (($m.samples | type) == "array" and ($m.samples | length) == $m.capture.requested.repeat) and ([$m.samples[] | .index] | sort) == [range(1; $m.capture.requested.repeat + 1)] and all($m.samples[]; .execution.exit_status == 0 and .execution.evidence_valid == true and .execution.command == $m.capture.execution.command and (.execution.timing.wall_seconds | nonnegative_number) and (.execution.timing.user_seconds | nonnegative_number) and (.execution.timing.system_seconds | nonnegative_number) and ((.execution.timing.max_rss_kib == null) or (.execution.timing.max_rss_kib | nonnegative_number)) and ((.observed.passed_lines | type) == "number" and .observed.passed_lines > 0) and .observed.failed_lines == 0 and (.observed.failure_names | length) == 0 and .observed.identity_proof.verified == true);
      ($measurement[0]) as $m | if ($m | valid | not) then error("measurement is incomplete, failed, or incompatible") else ($m.samples | map(.execution.timing.wall_seconds) | median) as $median | (($baseline[0].budgets // []) | map(select(.probe == $m.capture.probe and .host_class == $m.capture.host.class)) | first) as $budget | if $budget == null then error("no same-host budget for probe") elif ($budget.expected_repeat | type) != "number" or $budget.expected_repeat <= 0 then error("budget must supply a positive expected_repeat") elif $budget.expected_repeat != $m.capture.requested.repeat then error("budget repeat count differs") elif ($budget.median_wall_seconds | nonnegative_number | not) or ($budget.mad_wall_seconds | nonnegative_number | not) then error("budget must supply nonnegative numeric median_wall_seconds and mad_wall_seconds") elif $budget.command != $m.capture.execution.command then error("budget execution command differs") elif $budget.test_identity != $m.capture.execution.test_identity then error("budget test identity differs") elif $budget.resolved_threads != $m.capture.requested.resolved_threads then error("budget resolved thread count differs") elif $budget.observed_samples != ($m | signature) then error("budget observed samples or identity proof differ") else ($budget.median_wall_seconds + (3 * $budget.mad_wall_seconds)) as $limit | {schema_version: 1, check: {probe: $m.capture.probe, host_class: $m.capture.host.class, measurement_median_wall_seconds: $median, budget_limit_wall_seconds: $limit, passed: ($median <= $limit)}} | if .check.passed then . else error("measurement exceeds median + 3*MAD budget") end end end'
}

make_fake_cargo() {
    local path="$1"
    printf '%s\n' '#!/usr/bin/env bash' 'set -euo pipefail' 'if [ -n "${FAKE_CARGO_ALL_INVOCATIONS:-}" ]; then printf "%q " "$@" >>"$FAKE_CARGO_ALL_INVOCATIONS"; printf "\n" >>"$FAKE_CARGO_ALL_INVOCATIONS"; fi' 'if [ "${1:-}" = nextest ]; then exit 0; fi' 'if [ "${1:-}" = --version ]; then echo "cargo fixture"; exit 0; fi' 'if [ -n "${FAKE_CARGO_INVOCATIONS:-}" ]; then printf "%q " "$@" >>"$FAKE_CARGO_INVOCATIONS"; printf "\n" >>"$FAKE_CARGO_INVOCATIONS"; fi' 'if [ -n "${FAKE_CARGO_MUTATE_TRACKED:-}" ] && [ ! -e "${FAKE_CARGO_MUTATE_TRACKED}.mutated" ]; then printf changed >"$FAKE_CARGO_MUTATE_TRACKED"; touch "${FAKE_CARGO_MUTATE_TRACKED}.mutated"; fi' 'if [ "${1:-}" = test ]; then' '  call=0; if [ -n "${FAKE_CARGO_CALL_COUNT_FILE:-}" ]; then [ ! -f "$FAKE_CARGO_CALL_COUNT_FILE" ] || call="$(cat "$FAKE_CARGO_CALL_COUNT_FILE")"; call=$((call + 1)); printf "%s\n" "$call" >"$FAKE_CARGO_CALL_COUNT_FILE"; fi' '  if [ "${FAKE_CARGO_FAIL:-0}" = 1 ] || { [ -n "${FAKE_CARGO_FAIL_CALL:-}" ] && [ "$call" = "$FAKE_CARGO_FAIL_CALL" ]; }; then echo "test fixture::failure ... FAILED"; echo "test result: FAILED. 0 passed; 1 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s"; exit 1; fi' '  no_run=false; for value in "$@"; do [ "$value" = --no-run ] && no_run=true; done' '  if [ "${FAKE_CARGO_LARGE_RED:-0}" = 1 ] && [ "$no_run" = false ]; then' '    for ((index = 1; index <= 5000; index++)); do printf "test fixture::large::%05d::identity_padding_for_argv_boundary ... ok\n" "$index"; done' '    printf "test fixture::large::failure ... FAILED\ntest result: FAILED. 5000 passed; 1 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s\n"; exit 1' '  fi' '  test_id=""; for value in "$@"; do case "$value" in *::* ) test_id="$value" ;; esac; done' '  [ -n "$test_id" ] || test_id=fixture::test' '  [ -z "${FAKE_CARGO_LOG:-}" ] || printf "%s\n" "$test_id" >>"$FAKE_CARGO_LOG"' '  printf "test %s ... ok\ntest result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s\n" "$test_id"; exit 0' 'fi' 'exec /usr/bin/env cargo "$@"' >"$path"
    mv "$path" "$path.runner"
    printf '%s\n' '#!/usr/bin/env bash' 'set -euo pipefail' \
        'if [ "${1:-}" = metadata ]; then [ -z "${FAKE_CARGO_ALL_INVOCATIONS:-}" ] || printf "metadata --format-version 1 --no-deps \n" >>"$FAKE_CARGO_ALL_INVOCATIONS"; printf "%s\n" "$FAKE_CARGO_METADATA_JSON"; exit 0; fi' \
        'if [ "${1:-}" = nextest ] && [ "${2:-}" = show-config ]; then [ -z "${FAKE_CARGO_ALL_INVOCATIONS:-}" ] || printf "nextest show-config test-groups \n" >>"$FAKE_CARGO_ALL_INVOCATIONS"; printf "%s\n" "$FAKE_NEXTEST_CONFIG_JSON"; exit 0; fi' \
        'if [ "${1:-}" = nextest ] && [ "${2:-}" = --version ]; then [ -z "${FAKE_CARGO_ALL_INVOCATIONS:-}" ] || printf "nextest --version \n" >>"$FAKE_CARGO_ALL_INVOCATIONS"; printf "cargo-nextest fixture\n"; exit 0; fi' \
        'exec "$0.runner" "$@"' >"$path"
    chmod +x "$path" "$path.runner"
}

make_fake_rustc() {
    local path="$1"
    printf '%s\n' '#!/usr/bin/env bash' 'set -euo pipefail' 'if [ "${1:-}" = --version ]; then' '  if [ -n "${FAKE_RUSTC_MUTATE_TRACKED:-}" ] && [ ! -e "${FAKE_RUSTC_MUTATE_TRACKED}.mutated" ]; then printf changed >"$FAKE_RUSTC_MUTATE_TRACKED"; touch "${FAKE_RUSTC_MUTATE_TRACKED}.mutated"; fi' '  echo "rustc fixture"; exit 0' 'fi' 'exec /usr/bin/env rustc "$@"' >"$path"
    chmod +x "$path"
}

custody_closure_self_test() {
    local script_path="$1" fixture_dir="$2" cases=0 stderr_file="$fixture_dir/custody-public.stderr" name

    # A sourced copy must return before installing even one authority function.
    for name in help --self-test calibrate-baseline attest-preflight generate-baseline capture compare check unknown parser-failure command-failure; do
        ! /usr/bin/bash -c 'set -- "$2"; source "$1" >/dev/null 2>&1; declare -F calibrate_baseline >/dev/null || declare -F validate_sterile_launch >/dev/null' custody-source "$script_path" "$name"
        cases=$((cases + 1))
    done

    # Every hostile inherited name is rejected by a fresh public process before
    # repository/evidence lookup.  The destination sentinel proves no cleanup or
    # publication path was reached.
    local competitor="$fixture_dir/custody-public-competitor"
    printf 'competitor\n' >"$competitor"; chmod 0600 "$competitor"
    ! "$script_path" calibrate-baseline >/dev/null 2>"$stderr_file"; cases=$((cases + 1))
    ! "$script_path" attest-preflight --calibration-root target/test-suite-benchmark/z-baseline-calibration/0000000000000000000000000000000000000000 --threads 8 >/dev/null 2>"$stderr_file"; cases=$((cases + 1))
    ! "$script_path" generate-baseline --calibration-root target/test-suite-benchmark/z-baseline-calibration/0000000000000000000000000000000000000000 --out target/test-suite-benchmark/z-baseline-calibration/0000000000000000000000000000000000000000/candidate.json >/dev/null 2>"$stderr_file"; cases=$((cases + 1))
    for name in \
        BASH_ENV ENV CDPATH GLOBIGNORE BASHOPTS SHELLOPTS PROMPT_COMMAND DEBUGINFOD_URLS \
        MAKEFLAGS MFLAGS MAKEFILES CARGO_ENCODED_RUSTFLAGS RUSTFLAGS RUSTDOCFLAGS RUSTC_WRAPPER RUSTC_WORKSPACE_WRAPPER \
        CARGO_BUILD_RUSTC CARGO_BUILD_RUSTC_WRAPPER CARGO_BUILD_TARGET CARGO_NET_OFFLINE CARGO_REGISTRIES_CRATES_IO_INDEX \
        RUSTUP_TOOLCHAIN RUSTUP_DIST_SERVER NEXTEST_PROFILE NEXTEST_FILTER_EXPR LLVM_PROFILE_FILE \
        LD_PRELOAD LD_LIBRARY_PATH PYTHONPATH PYTHONHOME GIT_DIR GIT_WORK_TREE GIT_CONFIG_GLOBAL GIT_CONFIG_SYSTEM \
        S1Q_TEST_MAX_RAW_BYTES S1Q_TEST_GENERATE_HOOK_STAGE S1Q_INTERNAL_SELF_TEST_CAPABILITY FAKE_CARGO; do
        set +e
        env "$name=hostile" "$script_path" calibrate-baseline >/dev/null 2>"$stderr_file"
        local status=$?
        set -e
        [ "$status" -ne 0 ] && [ "$(cat "$competitor")" = competitor ]
        cases=$((cases + 1))
    done
    for name in __self-test-generate-baseline generator-self-test-v1 calibrate-baseline-internal internal-calibrate-baseline replay-custody-authority; do
        ! "$script_path" "$name" >/dev/null 2>"$stderr_file"
        [ "$(cat "$competitor")" = competitor ]
        cases=$((cases + 1))
    done

    local python_cases
    python_cases="$(/usr/bin/python3 - "$fixture_dir" "$CUSTODY_THREAT_STATEMENT" <<'PY'
import hashlib, json, os, signal, stat, sys, tempfile
base, threat = sys.argv[1:]
root=tempfile.mkdtemp(prefix="custody-closure.",dir=base); os.chmod(root,0o700)
count=0
def check(value,label):
    global count
    if not value: raise AssertionError(label)
    count+=1
def identity(path):
    node=os.stat(path,follow_symlinks=False); return node.st_dev,node.st_ino
check(stat.S_IMODE(os.stat(root).st_mode)==0o700,"root mode")
evidence=os.path.join(root,"evidence"); os.mkdir(evidence,0o700)
check(stat.S_IMODE(os.stat(evidence).st_mode)==0o700,"nested mode")
source=os.path.join(root,"source"); open(source,"wb").write(b"source\n"); os.chmod(source,0o600)
check(stat.S_IMODE(os.stat(source).st_mode)==0o600,"source mode")
fd=os.open(source,os.O_RDONLY|os.O_NOFOLLOW); before=os.fstat(fd); moved=source+".moved"; os.rename(source,moved); open(source,"wb").write(b"replacement")
check((os.fstat(fd).st_dev,os.fstat(fd).st_ino)==(before.st_dev,before.st_ino),"held descriptor")
check(identity(source)!=(before.st_dev,before.st_ino),"replacement detected")
os.close(fd); os.unlink(source); os.rename(moved,source)
config=os.path.join(root,"nextest.toml"); open(config,"wb").write(b"[profile.default]\nretries=0\n"); os.chmod(config,0o600)
config_fd=os.open(config,os.O_RDONLY|os.O_NOFOLLOW); config_before=os.fstat(config_fd); open(config,"ab").write(b"# drift\n")
check(os.fstat(config_fd).st_size!=config_before.st_size,"config drift")
os.close(config_fd)
raw=os.path.join(evidence,"stdout.log"); payload=b"alpha\nbeta\n"; open(raw,"wb").write(payload); os.chmod(raw,0o600)
check(hashlib.sha256(open(raw,"rb").read()).hexdigest()==hashlib.sha256(payload).hexdigest(),"incremental hash")
check(len(payload)<=64*1024*1024,"raw bound")
check(len(payload)+64*1024*1024>64*1024*1024,"one over raw bound")
events=[{"event_sequence":1,"command_id":1,"type":"command-start"},{"event_sequence":2,"command_id":1,"stream":"stdout","chunk_sequence":1,"type":"raw-chunk"},{"event_sequence":3,"command_id":1,"type":"command-finish"}]
check([x["event_sequence"] for x in events]==[1,2,3],"event order")
check(len({x["event_sequence"] for x in events})==len(events),"event uniqueness")
check(events[0]["type"]=="command-start" and events[-1]["type"]=="command-finish","event brackets")
check(events[1]["chunk_sequence"]==1,"chunk cardinality")
check(not ([1,1,2]==sorted(set([1,1,2]))),"duplicate event rejected")
check([1,3]!=list(range(1,3)),"missing event rejected")
check([2,1]!=sorted([2,1]),"reordered event rejected")
green={"kind":"exited","code":0,"signal":None}; red={"kind":"exited","code":1,"signal":None}; signaled={"kind":"signaled","code":None,"signal":signal.SIGTERM}; unknown={"kind":"unknown","code":None,"signal":None}
check(green=={"kind":"exited","code":0,"signal":None},"green status")
check(red!=green,"red status")
check(signaled!=green,"signal status")
check(unknown!=green,"unknown status")
check({"stdout":1,"stderr":1}!={"stdout":1},"partial stream rejected")
identities=["bin-a::same","bin-b::same","bin-a::ignored"]
check(len(set(identities))==3,"binary qualification")
check(sorted(identities)==["bin-a::ignored","bin-a::same","bin-b::same"],"identity sort")
check(len(set(identities+["bin-a::same"]))!=4,"duplicate identity reject")
doctests=["rsid::rsid","rsi-common::rsi_common","rsi-graph::rsi_graph","rsi::rsi","rsi-eval::rsi_eval"]
check(len(doctests)==5 and len(set(doctests))==5,"metadata doctest cardinality")
samples={1:[3.0,2.0,1.0],8:[1.1,1.0,0.9],16:[1.0,1.0,1.0],32:[1.2,1.2,1.2]}
def median(values):
    values=sorted(values); n=len(values); return values[n//2] if n%2 else (values[n//2-1]+values[n//2])/2
statistics={key:(median(value),median([abs(x-median(value)) for x in value])) for key,value in samples.items()}
fastest=min(statistics,key=lambda key:(statistics[key][0],key)); frontier=[key for key,value in statistics.items() if value[0]<=statistics[fastest][0]+statistics[fastest][1]]
check(fastest==8,"fastest candidate")
check(min(frontier)==8,"smallest eligible")
check(statistics[8][0]==1.0 and abs(statistics[8][1]-0.1)<1e-12,"median mad")
check(abs((statistics[8][0]+3*statistics[8][1])-1.3)<1e-12,"budget formula")
private=os.path.join(root,"private"); final=os.path.join(root,"final"); open(private,"wb").write(b"metric"); os.chmod(private,0o600); private_id=identity(private)
os.link(private,final); check(identity(final)==private_id and os.stat(final).st_nlink==2,"exclusive link")
os.unlink(private); check(identity(final)==private_id and os.stat(final).st_nlink==1,"final link")
competitor=os.path.join(root,"competitor"); open(competitor,"wb").write(b"competitor"); os.chmod(competitor,0o600)
try: os.link(final,competitor); linked=True
except FileExistsError: linked=False
check(not linked and open(competitor,"rb").read()==b"competitor","competitor preserved")
owned=os.path.join(root,"owned"); open(owned,"wb").write(b"owned"); os.chmod(owned,0o600); owned_id=identity(owned); os.unlink(owned); open(owned,"wb").write(b"racer")
if identity(owned)==owned_id: os.unlink(owned)
check(os.path.exists(owned) and open(owned,"rb").read()==b"racer","identity rollback")
alias=os.path.join(root,"alias"); os.link(raw,alias); check(os.stat(raw).st_nlink==2,"hardlink rejected")
os.unlink(alias); symlink=os.path.join(root,"symlink"); os.symlink("stdout.log",symlink); check(stat.S_ISLNK(os.lstat(symlink).st_mode),"symlink rejected")
origin={"claim_version":1,"kind":"single-live-producer-custody","acceptance_operation":"calibrate-baseline","launch_principal_authenticated":False,"cryptographic_attestation":False,"post_exit_authenticity":False}
check(origin["acceptance_operation"]=="calibrate-baseline","origin operation")
check(not origin["launch_principal_authenticated"] and not origin["cryptographic_attestation"] and not origin["post_exit_authenticity"],"narrow origin")
diagnostic={"kind":"unverified-durable-input","acceptance_operation":None,"accepted":False}; check(diagnostic["accepted"] is False,"diagnostic refusal")
check(threat=="Acceptance proves exact bytes and uninterrupted in-process descriptor custody against hostile inherited environment and concurrent same-UID pathname or configuration mutation after a conforming sterile launch. It does not authenticate the launching principal, resist same-UID ptrace, process-memory, signal/control, namespace-control, or inherited-open-fd compromise, and does not authenticate alteration or forgery after the producer's final reproof and exit. It is not a signature or cryptographic origin attestation.","threat statement")
inventory=[("event-journal.jsonl",12,"0"*64),("raw/001/stdout.log",len(payload),hashlib.sha256(payload).hexdigest())]
check(inventory==sorted(inventory),"inventory sort")
check(len({x[0] for x in inventory})==len(inventory),"inventory unique")
check(all(not x[0].startswith("/") and ".." not in x[0].split("/") for x in inventory),"inventory relative")
encoded=(json.dumps({"inventory":inventory,"origin":origin,"threat_boundary":threat},sort_keys=True,separators=(",",":"))+"\n").encode()
check(encoded==bytes(encoded),"deterministic bytes")
check(hashlib.sha256(encoded).hexdigest()==hashlib.sha256(encoded).hexdigest(),"deterministic regeneration")
print(count)
PY
)"
    [[ "$python_cases" =~ ^[1-9][0-9]*$ ]]
    cases=$((cases + python_cases))

    # Execute the exact resident RunCustody class extracted from this script.
    # The synthetic subclass replaces only the real repository/tool graph and
    # child environment; command draining, held descriptors, journal/status
    # sealing, normalization, inventory, held-out checks, hooks, link
    # publication, and terminal reproof are the production methods above.
    local resident_cases
    resident_cases="$(/usr/bin/python3 - "$script_path" "$fixture_dir" <<'PY'
import ctypes, dataclasses, datetime, errno, hashlib, json, math, os, re, resource, selectors, secrets, signal, stat, struct, sys, tempfile, time, tomllib, types
script_path, fixture_dir = sys.argv[1:]
text = open(script_path, encoding="utf-8").read()
calibrate = text.index("calibrate_baseline() {")
start = text.index("@dataclasses.dataclass(frozen=True)", calibrate)
end = text.index("\nexecute_custody()", start)
scope = {"__builtins__":__builtins__,"ctypes":ctypes,"dataclasses":dataclasses,"datetime":datetime,"errno":errno,"hashlib":hashlib,"json":json,"math":math,"os":os,"re":re,"resource":resource,"selectors":selectors,"secrets":secrets,"signal":signal,"stat":stat,"struct":struct,"sys":sys,"time":time,"tomllib":tomllib}
scope.update({"REPO":"/synthetic","SCRIPT":script_path,"THREAT":"synthetic","UID":os.getuid(),"GID":os.getgid(),
              "RAW_LIMIT":64*1024*1024,"TOTAL_LIMIT":1024*1024*1024,"CHUNK":64*1024,
              "EXPECTED_ENV":{},"INHERITED_FDS":{},"INHERITED_PATHS":{},"INHERITED_FACTS":{}})
exec(compile(text[start:end], script_path+":resident-self-test", "exec"), scope, scope)
RunCustody=scope["RunCustody"]; strict_json_load=scope["strict_json_load"]
assert strict_json_load.__globals__ is scope and RunCustody.enumerate_identities.__globals__ is scope
count=0
def check(value,label):
    global count
    if not value: raise AssertionError(label)
    count+=1
def rejects(call,label):
    global count
    try: call()
    except BaseException: count+=1; return
    raise AssertionError(label)

class SyntheticRunCustody(RunCustody):
    def __init__(self, tag="resident"):
        self.fds=[]; self.nodes={}; self.event_sequence=0; self.command_sequence=0
        self.journal_hash=hashlib.sha256(); self.journal_bytes=0; self.journal_events=[]
        self.records=[]; self.measurements={}; self.created=[]; self.final_identity=None; self.publication=None
        self.log_fds={}; self.evidence_files={}; self.evidence_dirs={}; self.private_watches={}; self.private_watch_paths={}; self.hooks={}
        self.authority_hashes={}; self.source_watch_info={}; self.watch_paths=[]
        self.current_boundary="synthetic-construction"; self.current_phase="synthetic"; self.terminal_recorded=False
        self.rejected=False; self.rejection=None; self.active_child=None; self.last_child=None
        self.root=tempfile.mkdtemp(prefix=f"custody-{tag}.",dir=fixture_dir); os.chmod(self.root,0o700)
        scope["REPO"]=self.root
        self.repo_fd=self.open_node(self.root,os.O_RDONLY|os.O_DIRECTORY|os.O_NOFOLLOW,"repository",directory=True)
        for leaf in ("run","metrics"): os.mkdir(os.path.join(self.root,leaf),0o700)
        self.evidence_root=os.path.join(self.root,"run")
        for leaf in ("raw","meta","tmp","xdg-empty"): os.mkdir(os.path.join(self.evidence_root,leaf),0o700)
        self.run_fd=self.open_node(self.evidence_root,os.O_RDONLY|os.O_DIRECTORY|os.O_NOFOLLOW,"synthetic:run",directory=True)
        self.raw_fd=self.open_node(os.path.join(self.evidence_root,"raw"),os.O_RDONLY|os.O_DIRECTORY|os.O_NOFOLLOW,"synthetic:raw",directory=True)
        self.meta_fd=self.open_node(os.path.join(self.evidence_root,"meta"),os.O_RDONLY|os.O_DIRECTORY|os.O_NOFOLLOW,"synthetic:meta",directory=True)
        self.tmp_fd=self.open_node(os.path.join(self.evidence_root,"tmp"),os.O_RDONLY|os.O_DIRECTORY|os.O_NOFOLLOW,"synthetic:tmp",directory=True)
        self.xdg_fd=self.open_node(os.path.join(self.evidence_root,"xdg-empty"),os.O_RDONLY|os.O_DIRECTORY|os.O_NOFOLLOW,"synthetic:xdg",directory=True)
        self.evidence_dirs={".":self.run_fd,"raw":self.raw_fd,"meta":self.meta_fd,"tmp":self.tmp_fd,"xdg-empty":self.xdg_fd}
        self.journal_fd=os.open("event-journal.jsonl",os.O_RDWR|os.O_CREAT|os.O_EXCL|os.O_NOFOLLOW|os.O_CLOEXEC,0o600,dir_fd=self.run_fd); self.fds.append(self.journal_fd)
        self.evidence_files={"event-journal.jsonl":(self.journal_fd,None,None)}
        self.arm_private_tree()
        python_path="/usr/bin/python3.14" if os.path.exists("/usr/bin/python3.14") else "/usr/bin/python3"
        self.tools={"python":self.open_node(python_path,os.O_RDONLY|os.O_NOFOLLOW,"tool:python")}; self.configs={}
        libc=ctypes.CDLL(None,use_errno=True); self.inotify_fd=libc.inotify_init1(os.O_NONBLOCK|os.O_CLOEXEC); self.fds.append(self.inotify_fd)
    def child_env(self): return {"PATH":"/usr/bin:/bin","LANG":"C","LC_ALL":"C","TZ":"UTC"}
    def reprove(self):
        held=os.fstat(self.repo_fd); current=os.stat(self.root,follow_symlinks=False)
        if (held.st_dev,held.st_ino)!=(current.st_dev,current.st_ino): scope["fail"]("synthetic repository path/fd disagreement")
        for label,(path,fd,dev,ino,*_) in self.nodes.items():
            node=os.fstat(fd)
            try: named=os.stat(path,follow_symlinks=False)
            except FileNotFoundError: scope["fail"](f"synthetic graph node disappeared: {label}")
            if (node.st_dev,node.st_ino)!=(dev,ino) or (named.st_dev,named.st_ino)!=(dev,ino): scope["fail"](f"synthetic graph path/fd disagreement: {label}")

def command(engine,name,stdout="alpha\n",stderr=""):
    code="import os; os.write(1,"+repr(stdout.encode())+"); os.write(2,"+repr(stderr.encode())+")"
    return engine.command(name,"python",[f"/proc/self/fd/{engine.tools['python']}","-c",code],"synthetic")
def ordered_command(engine,name,writes):
    code="import os,time;"+"".join(f"os.write({fd},{payload.encode()!r});time.sleep(0.03);" for fd,payload in writes)
    return engine.command(name,"python",[f"/proc/self/fd/{engine.tools['python']}","-c",code],"synthetic")

engine=SyntheticRunCustody()
record=command(engine,"descriptor-probe","alpha\nbeta\n","diagnostic\n")
engine.write_meta("resident-normalized.json",{"schema_version":1,"status":"green"})
engine.seal_journal(); engine.verify_evidence_tree()
check(engine.read_log(record,"stdout")==b"alpha\nbeta\n","resident descriptor readback")
check(engine.read_held(engine.journal_fd,engine.journal_bytes).endswith(b"\n"),"resident journal readable")
check(engine.read_held(engine.evidence_files[record["status"]["path"]][0],record["status"]["bytes"]).startswith(b"{"),"resident status readable")
check(engine.read_held(engine.evidence_files["meta/resident-normalized.json"][0]).startswith(b"{"),"resident metadata readable")
check(engine.journal_bytes==os.fstat(engine.journal_fd).st_size,"resident journal full write")

# A write-only replacement reaches the real final evidence pread and fails EBADF.
bad=SyntheticRunCustody("write-only")
bad_record=command(bad,"write-only-probe")
status_path=os.path.join(bad.evidence_root,bad_record["status"]["path"])
bad_status=os.open(status_path,os.O_WRONLY|os.O_NOFOLLOW); bad.evidence_files[bad_record["status"]["path"]]=(bad_status,bad_record["status"]["bytes"],bad_record["status"]["sha256"])
rejects(bad.verify_evidence_tree,"write-only status descriptor was accepted")

# Exact three-sample held-out validation for both lanes, at the bound and over.
signature={"passed_lines":1,"failed_lines":0,"ignored_lines":0,"failure_names":[],"identity_proof":{"verified":True}}
def held_samples(values):
    return [{"timing":{"wall_seconds":value},"status":{"path":"status"},"logs":{"stdout":{},"stderr":{}}} for value in values]
for lane in ("fast","full"):
    accepted=engine.validate_held_out(lane,held_samples([0.9,1.0,1.1]),[signature]*3,[signature]*3,{"limit_wall_seconds":1.0})
    check(accepted["statistics"]["median"]==1.0,f"{lane} held-out exact bound")
    rejects(lambda lane=lane: engine.validate_held_out(lane,held_samples([1.01,1.01,1.01]),[signature]*3,[signature]*3,{"limit_wall_seconds":1.0}),f"{lane} held-out one-over accepted")
rejects(lambda: engine.validate_held_out("fast",held_samples([1.0,1.0]),[signature]*2,[signature]*3,{"limit_wall_seconds":1.0}),"held-out cardinality accepted")
rejects(lambda: engine.validate_held_out("full",held_samples([1.0]*3),[signature,signature,{**signature,"passed_lines":2}],[signature]*3,{"limit_wall_seconds":1.0}),"held-out identity mismatch accepted")

# Strict JSON and raw-outcome guards use the production validators.
check(strict_json_load(b'{"packages":[]}\n',"fixture")["packages"]==[],"strict JSON positive")
rejects(lambda: strict_json_load(b'{"packages":[],"packages":[]}\n',"fixture"),"duplicate JSON key accepted")
rejects(lambda: strict_json_load((b'{"a":'*(scope["RESIDENT_LIMITS"].json_depth+1))+b'0'+(b'}'*(scope["RESIDENT_LIMITS"].json_depth+1)),"fixture"),"JSON depth over accepted")
duplicate=command(engine,"duplicate-outcome","PASS [0.1s] bin same\nPASS [0.1s] bin same\n","")
rejects(lambda: engine.nextest_observation(duplicate,{"runnable":["bin::same"],"ignored":[]},"fixture"),"duplicate raw outcome accepted")
enumeration=command(engine,"enumeration",json.dumps({"rust-suites":{"bin-a":{"binary-id":"bin-a","testcases":{"same":{"ignored":False,"filter-match":{"status":"matches"}}}},"bin-b":{"binary-id":"bin-b","testcases":{"same":{"ignored":False,"filter-match":{"status":"matches"}},"ignored":{"ignored":True,"filter-match":{"status":"matches"}}}}},"test-count":3})+"\n","")
enumerated=engine.enumerate_identities(enumeration)
check(enumerated=={"runnable":["bin-a::same","bin-b::same"],"ignored":["bin-b::ignored"]},"binary-qualified enumeration")
boolean_count=command(engine,"enumeration-bool",'{"rust-suites":{"bin":{"binary-id":"bin","testcases":{"same":{"ignored":1,"filter-match":{"status":"matches"}}}}},"test-count":1}\n',"")
rejects(lambda: engine.enumerate_identities(boolean_count),"non-boolean ignored status accepted")

# Transient private-tree mutation is rejected even when the final inventory is restored;
# disabling exactly that watch guard demonstrates the negative oracle.
transient=os.path.join(engine.evidence_root,"transient")
open(transient,"wb").close(); os.chmod(transient,0o600); os.unlink(transient)
rejects(engine.verify_evidence_tree,"transient tree mutation accepted")
engine.require_quiet_private_tree=types.MethodType(lambda self: None,engine)
engine.verify_evidence_tree(); check(True,"watch-removal negative control")
del engine.require_quiet_private_tree

# A same-name replacement is caught by path/fd identity after watch events are drained.
stdout_rel=record["logs"]["stdout"]["path"]; stdout_path=os.path.join(engine.evidence_root,stdout_rel); moved=stdout_path+".held"
os.rename(stdout_path,moved); open(stdout_path,"wb").write(b"alpha\nbeta\n"); os.chmod(stdout_path,0o600); engine.private_events()
rejects(engine.verify_evidence_tree,"same-name evidence replacement accepted")
check(os.path.isfile(stdout_path),"name-only negative control would pass")
os.unlink(stdout_path); os.rename(moved,stdout_path); engine.private_events(); engine.verify_evidence_tree()

# Journal gap/hash coverage and its deliberately weakened negative control.
chunk=next(event for event in engine.journal_events if event.get("type")=="raw-chunk" and event.get("command_id")==record["sequence"])
saved_offset=chunk["offset"]; chunk["offset"]=saved_offset+1
rejects(engine.validate_journal_coverage,"journal offset gap accepted")
check(engine.journal_hash.hexdigest()==hashlib.sha256(engine.read_held(engine.journal_fd)).hexdigest(),"hash-only negative control would pass")
chunk["offset"]=saved_offset; engine.validate_journal_coverage(); check(True,"journal coverage restored")

# Boundary hooks run before the same production path/fd reproof. Root and
# private-parent replacement each stop before a synthetic child can execute;
# a name/type-only check is the explicit guard-removal negative control.
graph=SyntheticRunCustody("graph-swap")
def replace_graph_root(current,_stage):
    moved=current.root+".held"; os.rename(current.root,moved); os.mkdir(current.root,0o700)
graph.hooks["before-command:graph-probe"]=replace_graph_root
rejects(lambda: command(graph,"graph-probe"),"repository graph replacement accepted")
check(os.path.isdir(graph.root),"graph path-only negative control")
tree=SyntheticRunCustody("tree-swap")
def replace_raw_parent(current,_stage):
    raw=os.path.join(current.evidence_root,"raw"); os.rename(raw,raw+".held"); os.mkdir(raw,0o700)
tree.hooks["before-command:tree-probe"]=replace_raw_parent
rejects(lambda: command(tree,"tree-probe"),"private tree replacement accepted")
check(os.path.isdir(os.path.join(tree.evidence_root,"raw")),"tree path-only negative control")

# Representative fast/full normalization runs through production parsing and
# writes held normalized metadata descriptors.
fast=command(engine,"synthetic-fast","","Summary [0.1s] 1 test run: 1 passed, 0 skipped\n")
fast_norm,_=engine.normalize_lane(fast,False,[]); check([x["name"] for x in fast_norm["components"]]==["nextest"],"fast component order")
full=ordered_command(engine,"synthetic-full",[(2,"Summary [0.1s] 1 test run: 1 passed, 0 skipped\n"),(2,"Doc-tests crate_a\n"),(1,"test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.01s\n"),(2,"Running rsi-model-control-validate\n"),(2,"Running rsi-provider-capability-validate\n")])
full_norm,_=engine.normalize_lane(full,True,["crate-a::crate_a"]); check([x["name"] for x in full_norm["components"]]==["nextest","doctests","model-control-validator","provider-capability-validator"],"full component order")
reordered=ordered_command(engine,"synthetic-full-reordered",[(2,"Doc-tests crate_a\n"),(1,"test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.01s\n"),(2,"Summary [0.1s] 1 test run: 1 passed, 0 skipped\n"),(2,"Running rsi-model-control-validate\n"),(2,"Running rsi-provider-capability-validate\n")])
rejects(lambda: engine.normalize_lane(reordered,True,["crate-a::crate_a"]),"reordered component ending accepted")
engine.seal_journal(); engine.verify_evidence_tree()

# Actual synthetic exclusive-link publication reaches the complete terminal
# graph/tree/journal reproof and produces exactly one private 0600 link.
scope["REPO"]=engine.root
engine.publish(b'{"synthetic":true}\n')
final=os.path.join(engine.root,"metrics","test-suite-baseline.json"); final_node=os.stat(final,follow_symlinks=False)
check(stat.S_IMODE(final_node.st_mode)==0o600 and final_node.st_nlink==1,"resident terminal publication")
check(engine.final_identity==(final_node.st_dev,final_node.st_ino),"resident terminal identity")

# The live after-link hook replaces the final name. Production identity checks
# reject it and the competitor remains; without that post-link guard, the name
# and link existence alone would pass.
raced=SyntheticRunCustody("after-link")
command(raced,"publication-probe")
def replace_after_link(current,_stage):
    parent=os.path.join(current.root,"metrics"); final=os.path.join(parent,"test-suite-baseline.json")
    os.unlink(final); open(final,"wb").write(b"competitor"); os.chmod(final,0o600)
raced.hooks["after-exclusive-link"]=replace_after_link
rejects(lambda: raced.publish(b'{"synthetic":true}\n'),"after-link replacement accepted")
competitor=os.path.join(raced.root,"metrics","test-suite-baseline.json")
check(open(competitor,"rb").read()==b"competitor","after-link competitor preserved")
check(os.path.isfile(competitor),"post-link-guard removal negative control")

print(count)
PY
)"
    [[ "$resident_cases" =~ ^[1-9][0-9]*$ ]]
    cases=$((cases + resident_cases))

    # R5 closure oracles execute the exact resident slice in a fresh explicit
    # namespace for every case.  Source-mutated controls disable exactly one
    # uniquely tagged production predicate and cannot contaminate another case.
    local r5_cases
    r5_cases="$(/usr/bin/python3 - "$script_path" "$fixture_dir" <<'PY'
import ctypes, dataclasses, datetime, errno, hashlib, json, math, os, re, resource, selectors, secrets, signal, stat, struct, subprocess, sys, tempfile, time, tomllib
script_path, fixture_dir=sys.argv[1:]
text=open(script_path,encoding="utf-8").read(); cal=text.index("calibrate_baseline() {")
start=text.index("@dataclasses.dataclass(frozen=True)",cal); end=text.index("\nexecute_custody()",start); pristine=text[start:end]
count=0
def check(value,label):
    global count
    if not value: raise AssertionError(label)
    count+=1
def rejects(call,label):
    global count
    try: call()
    except BaseException: count+=1; return
    raise AssertionError(label)
libc=ctypes.CDLL(None,use_errno=True)
check(libc.prctl(36,1,0,0,0)==0,"synthetic harness became a child subreaper")
def namespace(root,disabled=None):
    source=pristine
    if disabled:
        marker=f"# {disabled}\n"; check(source.count(marker)==1,f"unique marker {disabled}")
        after=source.index(marker)+len(marker); finish=source.index("\n",after); guard=source[after:finish]
        check(guard.lstrip().startswith("if "),f"predicate follows {disabled}")
        indent=guard[:len(guard)-len(guard.lstrip())]
        replacement=indent+(f"if False: fail('disabled only for {disabled} control')" if ": fail(" in guard else "if False:")
        mutated=source[:after]+replacement+source[finish:]
        check(mutated.replace(replacement,guard,1)==source,f"only {disabled} changed")
        source=mutated
    cargo_home=os.path.join(root,"cargo-home")
    expected={"HOME":root,"CARGO_HOME":cargo_home,"RUSTUP_HOME":os.path.join(root,"rustup"),"PATH":"/usr/bin:/bin","LANG":"C","LC_ALL":"C","TZ":"UTC","TMPDIR":os.path.join(root,"run","tmp"),"CARGO_TARGET_DIR":os.path.join(root,"target"),"XDG_CONFIG_HOME":os.path.join(root,"run","xdg-empty"),"NEXTEST_CONFIG_FILE":os.path.join(root,"nextest.toml")}
    ns={"__builtins__":__builtins__,"ctypes":ctypes,"dataclasses":dataclasses,"datetime":datetime,"errno":errno,"hashlib":hashlib,"json":json,"math":math,"os":os,"re":re,"resource":resource,"selectors":selectors,"secrets":secrets,"signal":signal,"stat":stat,"struct":struct,"sys":sys,"time":time,"tomllib":tomllib,
        "REPO":root,"SCRIPT":script_path,"THREAT":"synthetic","UID":os.getuid(),"GID":os.getgid(),"RAW_LIMIT":64*1024*1024,"TOTAL_LIMIT":1024*1024*1024,"CHUNK":64*1024,"EXPECTED_ENV":expected,"INHERITED_FDS":{},"INHERITED_PATHS":{},"INHERITED_FACTS":{}}
    exec(compile(source,script_path+f":r5-{disabled or 'production'}","exec"),ns,ns)
    run=ns["RunCustody"]; load=ns["strict_json_load"]; limits=ns["RESIDENT_LIMITS"]
    check(load.__globals__ is ns and run.enumerate_identities.__globals__ is ns and run.reprove.__globals__ is ns and run.publish.__globals__ is ns and run.git_policy_argv.__globals__ is ns and run.authenticate_source_state.__globals__ is ns,"resident globals identity")
    return ns,run,load,limits
def root_for(tag):
    holder=tempfile.mkdtemp(prefix=f"r5-{tag}.",dir=fixture_dir); os.chmod(holder,0o700); root=os.path.join(holder,"repo"); os.mkdir(root,0o700)
    for rel in ("metrics","run","run/raw","run/meta","run/tmp","run/xdg-empty","target",".cargo","cargo-home","source","git-admin"):
        os.mkdir(os.path.join(root,rel),0o700)
    open(os.path.join(root,".cargo/config.toml"),"w").write('[alias]\nbench-all = "bench --workspace"\n')
    open(os.path.join(root,"cargo-home/config.toml"),"w").write('[target.x86_64-unknown-linux-gnu]\nlinker="clang"\nrustflags=["-C","link-arg=-fuse-ld=mold"]\n')
    open(os.path.join(root,"nextest.toml"),"w").write('[profile.default]\nretries=0\n')
    open(os.path.join(root,"toolchain.toml"),"w").write('[toolchain]\nchannel="1.94.1"\nprofile="minimal"\ncomponents=["rustfmt","clippy"]\n')
    open(os.path.join(root,"source/existing"),"w").write('source\n')
    open(os.path.join(root,"git-admin/HEAD"),"w").write('ref: refs/heads/synthetic\n')
    open(os.path.join(root,"git-admin/index"),"wb").write(b'index-v1')
    open(os.path.join(root,"git-admin/config"),"w").write('[core]\nrepositoryformatversion = 0\n')
    open(os.path.join(root,"git-admin/config.worktree"),"w").write('[core]\nfilemode = true\n')
    for rel in (".cargo/config.toml","cargo-home/config.toml","nextest.toml","toolchain.toml","source/existing","git-admin/HEAD","git-admin/index","git-admin/config","git-admin/config.worktree"): os.chmod(os.path.join(root,rel),0o600)
    return root
def engine_for(tag,disabled=None):
    root=root_for(tag); ns,Run,load,limits=namespace(root,disabled); engine=object.__new__(Run)
    engine.fds=[]; engine.nodes={}; engine.event_sequence=0; engine.command_sequence=0; engine.journal_hash=hashlib.sha256(); engine.journal_bytes=0
    engine.records=[]; engine.measurements={}; engine.created=[]; engine.final_identity=None; engine.publication=None; engine.log_fds={}; engine.evidence_files={}; engine.evidence_dirs={}; engine.journal_events=[]; engine.private_watches={}; engine.private_watch_paths={}; engine.hooks={}; engine.cargo_discovery=[]; engine.aliases={}; engine.authority_hashes={}; engine.source_watch_info={}; engine.watch_paths=[]; engine.current_boundary="synthetic-construction"; engine.current_phase="synthetic"; engine.terminal_recorded=False; engine.rejected=False; engine.rejection=None; engine.active_child=None; engine.last_child=None
    engine.repo_fd=engine.open_node(root,os.O_RDONLY|os.O_DIRECTORY|os.O_NOFOLLOW,"repository",directory=True)
    python_path="/usr/bin/python3.14" if os.path.exists("/usr/bin/python3.14") else "/usr/bin/python3"
    engine.tools={"python":engine.open_node(python_path,os.O_RDONLY|os.O_NOFOLLOW,"tool:python")}
    engine.configs={"nextest":engine.open_node(os.path.join(root,"nextest.toml"),os.O_RDONLY|os.O_NOFOLLOW,"config:nextest"),"cargo-repo":engine.open_node(os.path.join(root,".cargo/config.toml"),os.O_RDONLY|os.O_NOFOLLOW,"config:cargo-repo"),"cargo-account":engine.open_node(os.path.join(root,"cargo-home/config.toml"),os.O_RDONLY|os.O_NOFOLLOW,"config:cargo-account"),"toolchain":engine.open_node(os.path.join(root,"toolchain.toml"),os.O_RDONLY|os.O_NOFOLLOW,"config:toolchain")}
    engine.open_node(os.path.join(root,".cargo"),os.O_RDONLY|os.O_DIRECTORY|os.O_NOFOLLOW,"source-dir:.cargo",directory=True)
    engine.authenticate_cargo_discovery(); engine.arm_watches()
    def odir(rel):
        fd=os.open(os.path.join(root,rel),os.O_RDONLY|os.O_DIRECTORY|os.O_NOFOLLOW|os.O_CLOEXEC); engine.fds.append(fd); return fd
    engine.run_fd=odir("run"); engine.raw_fd=odir("run/raw"); engine.meta_fd=odir("run/meta"); engine.tmp_fd=odir("run/tmp"); engine.xdg_fd=odir("run/xdg-empty")
    engine.evidence_dirs={".":engine.run_fd,"raw":engine.raw_fd,"meta":engine.meta_fd,"tmp":engine.tmp_fd,"xdg-empty":engine.xdg_fd}
    engine.child_tmp=os.path.join(root,"run/tmp"); engine.child_xdg=os.path.join(root,"run/xdg-empty")
    engine.journal_fd=os.open("event-journal.jsonl",os.O_RDWR|os.O_CREAT|os.O_EXCL|os.O_NOFOLLOW|os.O_CLOEXEC,0o600,dir_fd=engine.run_fd); engine.fds.append(engine.journal_fd)
    engine.terminal_fd=os.open("terminal-rejection.json",os.O_RDWR|os.O_CREAT|os.O_EXCL|os.O_NOFOLLOW|os.O_CLOEXEC,0o600,dir_fd=engine.run_fd); engine.fds.append(engine.terminal_fd)
    engine.evidence_files={"event-journal.jsonl":(engine.journal_fd,None,None),"terminal-rejection.json":(engine.terminal_fd,0,hashlib.sha256(b"").hexdigest())}; engine.arm_private_tree()
    check(engine.reprove.__func__.__globals__ is ns if hasattr(engine.reprove,"__func__") else Run.reprove.__globals__ is ns,"engine production globals")
    return ns,engine,load,limits,root
def emit(engine,name,writes):
    code="import os,time;"+"".join(f"os.write({fd},{payload!r});time.sleep(0.02);" for fd,payload in writes)
    return engine.command(name,"python",[f"/proc/self/fd/{engine.tools['python']}","-c",code],"synthetic")
def enum_doc(suites,count=None): return {"rust-suites":suites,"test-count":sum(len(value["testcases"]) for value in suites.values()) if count is None else count}
def suite(binary,tests): return {"binary-id":binary,"testcases":tests}
def testcase(ignored=False): return {"ignored":ignored,"filter-match":{"status":"matches"}}
def json_bytes(value): return (json.dumps(value,sort_keys=True,separators=(",",":"),ensure_ascii=True,allow_nan=False)+"\n").encode("ascii")
def held_record(engine,data,name="fixture",stderr=b""):
    sequence=9000+len(engine.log_fds); logs={}
    for stream,payload in (("stdout",data),("stderr",stderr)):
        fd=os.memfd_create(f"{name}-{stream}",os.MFD_CLOEXEC); engine.fds.append(fd); engine.write_all(fd,payload)
        digest=hashlib.sha256(payload).hexdigest(); engine.log_fds[(sequence,stream)]=fd; path=f"raw/{sequence}-{name}/{stream}.log"; engine.evidence_files[path]=(fd,len(payload),digest); logs[stream]={"path":path,"bytes":len(payload),"sha256":digest,"chunks":1 if payload else 0}
    return {"sequence":sequence,"name":name,"logs":logs}
def live_enum(tag,data,disabled=None):
    _,engine,_,limits,_=engine_for(tag,disabled); return engine.enumerate_identities(held_record(engine,data,tag)),limits
def cargo_target(): return {"name":"lib","kind":["lib"],"crate_types":["lib"],"required-features":[],"src_path":"/synthetic/lib.rs","edition":"2024","doctest":False,"test":True,"doc":True}
def cargo_metadata(targets,metadata=None): return {"packages":[{"name":"pkg","targets":targets}],"workspace_members":[],"workspace_default_members":[],"resolve":None,"target_directory":"/synthetic/target","version":1,"workspace_root":"/synthetic","metadata":{} if metadata is None else metadata}
def live_cargo(tag,data,disabled=None):
    _,engine,_,limits,_=engine_for(tag,disabled); document,doctests=engine.cargo_metadata_from_record(held_record(engine,data,tag)); return document,doctests,limits
def json_shape(value,depth=0):
    nodes=1; maximum=depth
    if isinstance(value,dict):
        for key,child in value.items():
            key_nodes,key_depth=json_shape(key,depth+1); child_nodes,child_depth=json_shape(child,depth+1); nodes+=key_nodes+child_nodes; maximum=max(maximum,key_depth,child_depth)
    elif isinstance(value,list):
        for child in value:
            child_nodes,child_depth=json_shape(child,depth+1); nodes+=child_nodes; maximum=max(maximum,child_depth)
    return nodes,maximum

# Immutable generic JSON limits: every B/B+1 traverses read_json_log and the
# exact Cargo schema validator in a fresh production namespace.
root=root_for("limits"); ns,Run,load,limits=namespace(root); rejects(lambda: setattr(limits,"json_depth",99),"resident limits mutable")
def depth_document(target):
    value=0
    while json_shape(cargo_metadata([],value))[1]<target: value={"a":value}
    document=cargo_metadata([],value); check(json_shape(document)[1]==target,"depth fixture exact")
    return json_bytes(document)
depth_b=depth_document(limits.json_depth); check(live_cargo("depth-exact",depth_b)[0]["version"]==1,"depth exact live")
depth_over=depth_document(limits.json_depth+1); rejects(lambda: live_cargo("depth-over",depth_over),"depth one-over live")
check(live_cargo("depth-control",depth_over,"R5_GUARD_JSON_DEPTH")[0]["version"]==1,"depth guard removal live")
string_b=json_bytes(cargo_metadata([],{"padding":"x"*limits.string_bytes})); check(live_cargo("string-exact",string_b)[0]["version"]==1,"string exact live")
string_over=json_bytes(cargo_metadata([],{"padding":"x"*(limits.string_bytes+1)})); rejects(lambda: live_cargo("string-over",string_over),"string one-over live")
check(live_cargo("string-control",string_over,"R5_GUARD_JSON_STRING")[0]["version"]==1,"string guard removal live")
container_b=json_bytes(cargo_metadata([],{"padding":[0]*limits.container_items})); check(live_cargo("container-exact",container_b)[0]["version"]==1,"container exact live")
container_over=json_bytes(cargo_metadata([],{"padding":[0]*(limits.container_items+1)})); rejects(lambda: live_cargo("container-over",container_over),"container one-over live")
check(live_cargo("container-control",container_over,"R6_GUARD_JSON_CONTAINER")[0]["version"]==1,"container guard removal live")
node_document=cargo_metadata([],{"padding":[]}); remaining=limits.json_nodes-json_shape(node_document)[0]
while remaining:
    amount=min(limits.container_items,remaining-1) if remaining>1 else 0; node_document["metadata"]["padding"].append([0]*amount); remaining-=amount+1
check(json_shape(node_document)[0]==limits.json_nodes,"node fixture exact")
node_b=json_bytes(node_document); check(live_cargo("nodes-exact",node_b)[0]["version"]==1,"nodes exact live")
node_document["metadata"]["padding"].append([]); check(json_shape(node_document)[0]==limits.json_nodes+1,"node fixture one-over")
node_over=json_bytes(node_document); rejects(lambda: live_cargo("nodes-over",node_over),"nodes one-over live")
check(live_cargo("nodes-control",node_over,"R6_GUARD_JSON_NODES")[0]["version"]==1,"nodes guard removal live")
def exact_json_size(target_size):
    for pieces in range(1,256):
        document=cargo_metadata([],{"padding":[""]*pieces}); overhead=len(json_bytes(document)); content=target_size-overhead
        if 0<=content<=pieces*limits.string_bytes:
            lengths=[min(limits.string_bytes,content-index*limits.string_bytes) if content>index*limits.string_bytes else 0 for index in range(pieces)]; document["metadata"]["padding"]=["x"*length for length in lengths]; data=json_bytes(document); check(len(data)==target_size,"JSON byte fixture exact"); return data
    raise AssertionError("cannot construct exact JSON byte fixture")
json_b=exact_json_size(limits.json_bytes); check(live_cargo("json-bytes-exact",json_b)[0]["version"]==1,"JSON bytes exact live")
json_over=exact_json_size(limits.json_bytes+1); rejects(lambda: live_cargo("json-bytes-over",json_over),"JSON bytes one-over live")
check(live_cargo("json-bytes-control",json_over,"R5_GUARD_JSON_BYTES")[0]["version"]==1,"JSON bytes guard removal live")

# Exact suite/testcase/identity and Cargo-target bounds traverse held raw bytes,
# read_json_log, strict_json_load, and the production schema validator.
exact={f"b{i}":suite(f"b{i}",{}) for i in range(limits.suites)}; suite_b=json_bytes(enum_doc(exact)); check(live_enum("suite-exact",suite_b)[0]=={"runnable":[],"ignored":[]},"suite exact live")
over={**exact,"overflow":suite("overflow",{})}; suite_over=json_bytes(enum_doc(over)); rejects(lambda: live_enum("suite-over",suite_over),"suite one-over live")
check(live_enum("suite-control",suite_over,"R5_GUARD_SUITE_COUNT")[0]=={"runnable":[],"ignored":[]},"suite guard removal live")
tests={f"t{i}":testcase() for i in range(limits.testcases)}; testcase_b=json_bytes(enum_doc({"bin":suite("bin",tests)})); check(len(live_enum("testcase-exact",testcase_b)[0]["runnable"])==limits.testcases,"testcase exact live")
tests["overflow"]=testcase(); testcase_over=json_bytes(enum_doc({"bin":suite("bin",tests)})); rejects(lambda: live_enum("testcase-over",testcase_over),"testcase one-over live")
check(len(live_enum("testcase-control",testcase_over,"R5_GUARD_TESTCASE_IDENTITY_COUNT")[0]["runnable"])==limits.testcases+1,"testcase guard removal live")
left=limits.identities//2; identities={"a":suite("a",{f"a{i}":testcase() for i in range(left)}),"b":suite("b",{f"b{i}":testcase() for i in range(limits.identities-left)})}; identity_b=json_bytes(enum_doc(identities)); check(len(live_enum("identity-exact",identity_b)[0]["runnable"])==limits.identities,"identity exact live")
identities["b"]["testcases"]["over"]=testcase(); identity_over=json_bytes(enum_doc(identities)); rejects(lambda: live_enum("identity-over",identity_over),"identity one-over live")
check(len(live_enum("identity-control",identity_over,"R5_GUARD_TESTCASE_IDENTITY_COUNT")[0]["runnable"])==limits.identities+1,"identity guard removal live")
for label,document in (("missing",{"rust-suites":{}}),("extra",{"rust-suites":{},"test-count":0,"extra":0}),("mistyped",{"rust-suites":{},"test-count":False}),("unreconciled",{"rust-suites":{},"test-count":1})):
    data=json_bytes(document); rejects(lambda label=label,data=data: live_enum("test-count-"+label,data),"test-count "+label)
bad_binary=json_bytes(enum_doc({"bin":suite("wrong",{"t":testcase()})})); rejects(lambda: live_enum("binary-mismatch",bad_binary),"binary-id mismatch live")
target=cargo_target(); cargo_b=json_bytes(cargo_metadata([target]*limits.identities)); check(live_cargo("cargo-target-exact",cargo_b)[1]==[],"Cargo target exact live")
cargo_over=json_bytes(cargo_metadata([target]*(limits.identities+1))); rejects(lambda: live_cargo("cargo-target-over",cargo_over),"Cargo target one-over live")
check(live_cargo("cargo-target-control",cargo_over,"R5_GUARD_CARGO_TARGET_COUNT")[1]==[],"Cargo target guard removal live")
bad_target=json_bytes(cargo_metadata([{**target,"extra":True}])); rejects(lambda: live_cargo("cargo-target-extra",bad_target),"Cargo target extra field live")

# Raw byte/line, journal line/event, complete retained-evidence aggregate, and
# final-output guards all have exact, one-over, and isolated source controls.
def rejects_exact(call,message,label):
    global count
    try: call()
    except RuntimeError as error:
        if str(error)!=message: raise AssertionError(f"{label}: expected {message!r}, observed {str(error)!r}")
        count+=1; return
    except BaseException as error: raise AssertionError(f"{label}: wrong exception {type(error).__name__}: {error}")
    raise AssertionError(label)
def emit_bytes(engine,name,size):
    code=f"import os;os.set_blocking(1,True);n={size};b=b'x'*65536\nwhile n:\n c=b[:min(n,len(b))];os.write(1,c);n-=len(c)"
    return engine.command(name,"python",[f"/proc/self/fd/{engine.tools['python']}","-c",code],"synthetic")
_,eng,_,limits,_=engine_for("raw-exact"); rec=emit_bytes(eng,"raw-exact",limits.raw_bytes); check(rec["logs"]["stdout"]["bytes"]==limits.raw_bytes,"raw exact")
_,eng,_,limits,_=engine_for("raw-over"); rejects_exact(lambda: emit_bytes(eng,"raw-over",limits.raw_bytes+1),"raw evidence stream bound exceeded: raw-over/stdout","raw one-over")
_,eng,_,limits,_=engine_for("raw-control","R5_GUARD_RAW_BYTES"); rec=emit_bytes(eng,"raw-control",limits.raw_bytes+1); check(rec["logs"]["stdout"]["bytes"]==limits.raw_bytes+1,"raw guard removal")

def positioned_record(engine,stdout,stderr,name):
    record=held_record(engine,stdout,name,stderr)
    for stream,payload in (("stdout",stdout),("stderr",stderr)):
        if payload: engine.journal({"type":"raw-chunk","command_id":record["sequence"],"stream":stream,"chunk_sequence":1,"offset":0,"byte_count":len(payload),"sha256":hashlib.sha256(payload).hexdigest()})
        engine.journal({"type":"stream-eof","command_id":record["sequence"],"stream":stream,"chunk_count":1 if payload else 0,"byte_count":len(payload),"sha256":hashlib.sha256(payload).hexdigest()})
    return record

line_byte_b=b"x"*limits.raw_line_bytes; line_byte_over=line_byte_b+b"x"
for consumer in ("lines","positioned"):
    _,eng,_,limits,_=engine_for(f"line-byte-{consumer}-exact")
    if consumer=="lines": observed=list(eng.lines(held_record(eng,line_byte_b,"line-byte-exact"),"stdout"))
    else: observed=eng.positioned_lines(positioned_record(eng,line_byte_b,b"",f"line-byte-{consumer}-exact"))
    check(len(observed)==1,"raw line-byte exact "+consumer)
    _,eng,_,limits,_=engine_for(f"line-byte-{consumer}-over")
    rec=held_record(eng,line_byte_over,"line-byte-over") if consumer=="lines" else positioned_record(eng,line_byte_over,b"",f"line-byte-{consumer}-over")
    call=(lambda eng=eng,rec=rec:list(eng.lines(rec,"stdout"))) if consumer=="lines" else (lambda eng=eng,rec=rec:eng.positioned_lines(rec))
    rejects_exact(call,"raw evidence line exceeds bound","raw line-byte one-over "+consumer)
    _,eng,_,limits,_=engine_for(f"line-byte-{consumer}-control","R7_GUARD_RAW_LINE_BYTES")
    rec=held_record(eng,line_byte_over,"line-byte-control") if consumer=="lines" else positioned_record(eng,line_byte_over,b"",f"line-byte-{consumer}-control")
    observed=list(eng.lines(rec,"stdout")) if consumer=="lines" else eng.positioned_lines(rec)
    check(len(observed)==1,"raw line-byte guard removal "+consumer)

line_b=b"x\n"*limits.raw_lines_per_stream; line_over=line_b+b"x\n"
_,eng,_,limits,_=engine_for("lines-exact"); check(len(list(eng.lines(held_record(eng,line_b,"lines-exact"),"stdout")))==limits.raw_lines_per_stream,"raw lines exact lines")
_,eng,_,limits,_=engine_for("lines-over"); rec=held_record(eng,line_over,"lines-over"); rejects_exact(lambda: list(eng.lines(rec,"stdout")),"raw evidence line count exceeds bound","raw lines one-over lines")
_,eng,_,limits,_=engine_for("lines-control","R7_GUARD_RAW_LINE_COUNT"); check(len(list(eng.lines(held_record(eng,line_over,"lines-control"),"stdout")))==limits.raw_lines_per_stream+1,"raw lines guard removal lines")
_,eng,_,limits,_=engine_for("positioned-lines-exact"); rec=positioned_record(eng,line_b,line_b,"positioned-lines-exact"); check(len(eng.positioned_lines(rec))==2*limits.raw_lines_per_stream,"raw lines exact positioned per stream")
_,eng,_,limits,_=engine_for("positioned-lines-over"); rec=positioned_record(eng,line_over,b"", "positioned-lines-over"); rejects_exact(lambda: eng.positioned_lines(rec),"raw evidence line count exceeds bound","raw lines one-over positioned")
_,eng,_,limits,_=engine_for("positioned-lines-control","R7_GUARD_RAW_LINE_COUNT"); rec=positioned_record(eng,line_over,b"", "positioned-lines-control"); check(len(eng.positioned_lines(rec))==limits.raw_lines_per_stream+1,"raw lines guard removal positioned")

def append_journal_line(engine,target):
    padding=max(0,target-128)
    while True:
        event={"type":"boundary","boundary":"x"*padding}; encoded=engine.journal.__func__.__globals__["canonical_json"]({"event_sequence":engine.event_sequence+1,"monotonic_ns":time.monotonic_ns(),**event}); observed=len(encoded)-1
        if observed==target: break
        padding+=target-observed
        if padding<0: raise AssertionError("journal bound is below schema overhead")
    before=engine.journal_bytes; engine.journal(event); return engine.journal_bytes-before-1
_,eng,_,limits,_=engine_for("journal-line-exact"); check(append_journal_line(eng,limits.journal_line_bytes)==limits.journal_line_bytes,"journal line exact append"); eng.validate_journal_coverage(); check(True,"journal line exact replay")
_,eng,_,limits,_=engine_for("journal-line-over"); rejects_exact(lambda: append_journal_line(eng,limits.journal_line_bytes+1),"event journal line exceeds bound","journal line one-over")
_,eng,_,limits,_=engine_for("journal-line-control","R6_GUARD_JOURNAL_LINE_BYTES"); check(append_journal_line(eng,limits.journal_line_bytes+1)==limits.journal_line_bytes+1,"journal line guard removal append"); eng.validate_journal_coverage(); check(True,"journal line guard removal replay")

def append_journal_events(engine,total):
    for index in range(total): engine.journal({"type":"boundary","boundary":f"synthetic-{index}"})
_,eng,_,limits,_=engine_for("journal-events-exact"); append_journal_events(eng,limits.journal_events); check(eng.event_sequence==limits.journal_events,"journal events exact append"); eng.validate_journal_coverage(); check(True,"journal events exact replay")
_,eng,_,limits,_=engine_for("journal-events-over"); append_journal_events(eng,limits.journal_events); rejects_exact(lambda: eng.journal({"type":"boundary","boundary":"one-over"}),"event journal count exceeds bound","journal events one-over")
_,eng,_,limits,_=engine_for("journal-events-control","R6_GUARD_JOURNAL_EVENTS"); append_journal_events(eng,limits.journal_events+1); check(eng.event_sequence==limits.journal_events+1,"journal events guard removal append"); eng.validate_journal_coverage(); check(True,"journal events guard removal replay")

def digest_fd(fd):
    digest=hashlib.sha256(); offset=0
    while True:
        block=os.pread(fd,limits.chunk_bytes,offset)
        if not block: break
        offset+=len(block); digest.update(block)
    return digest.hexdigest()
def register_file(engine,parent_fd,relative,size,data=None):
    leaf=os.path.basename(relative); fd=os.open(leaf,os.O_RDWR|os.O_CREAT|os.O_EXCL|os.O_NOFOLLOW|os.O_CLOEXEC,0o600,dir_fd=parent_fd); engine.fds.append(fd); engine.expect_private_create(parent_fd,leaf)
    if data is None: os.ftruncate(fd,size)
    else: engine.write_all(fd,data); check(len(data)==size,"registered evidence size")
    os.fsync(fd); digest=digest_fd(fd); engine.evidence_files[relative]=(fd,size,digest); return fd
def populate_aggregate(engine,total):
    record=emit(engine,"aggregate-fixture",[(1,b"accepted raw stdout\n"),(2,b"accepted raw stderr\n")])
    engine.write_meta("aggregate-normalized.json",{"schema_version":1,"command_sequence":record["sequence"],"status":"green"})
    remaining=total-engine.complete_evidence_bytes(); index=0
    while remaining:
        size=min(remaining,limits.raw_bytes); register_file(engine,engine.raw_fd,f"raw/aggregate-{index:03d}.log",size); remaining-=size; index+=1
    check(engine.complete_evidence_bytes()==total,"aggregate fixture exact bytes")
    return engine
_,eng,_,limits,_=engine_for("aggregate-exact"); populate_aggregate(eng,limits.aggregate_bytes); eng.verify_evidence_tree(); check(True,"complete aggregate exact full reproof"); duplicate_value=next(value for path,value in eng.evidence_files.items() if path.endswith("/stdout.log")); eng.evidence_files["raw/duplicate.log"]=duplicate_value; rejects_exact(eng.enforce_evidence_aggregate,"aliased retained evidence inode","complete aggregate alias double-count")
_,eng,_,limits,_=engine_for("aggregate-over"); populate_aggregate(eng,limits.aggregate_bytes+1); rejects_exact(eng.verify_evidence_tree,"complete retained evidence aggregate exceeds bound","complete aggregate one-over")
_,eng,_,limits,_=engine_for("aggregate-control","R6_GUARD_EVIDENCE_AGGREGATE"); populate_aggregate(eng,limits.aggregate_bytes+1); eng.verify_evidence_tree(); check(True,"complete aggregate guard removal full reproof")

def publish_bound(tag,size,disabled=None):
    _,engine,_,limits,root=engine_for(tag,disabled); data=bytes(size); engine.publish(data); final=os.path.join(root,"metrics/test-suite-baseline.json"); node=os.stat(final,follow_symlinks=False)
    check(node.st_size==size and node.st_nlink==1 and stat.S_IMODE(node.st_mode)==0o600,"output publication facts "+tag)
    fd=os.open(final,os.O_RDONLY|os.O_NOFOLLOW); digest=digest_fd(fd); os.close(fd); check(digest==hashlib.sha256(data).hexdigest(),"output publication bytes "+tag)
publish_bound("output-exact",limits.output_bytes)
_,eng,_,limits,root=engine_for("output-over"); rejects_exact(lambda: eng.publish(bytes(limits.output_bytes+1)),"metric candidate exceeds output bound","output one-over"); check(not os.path.exists(os.path.join(root,"metrics/test-suite-baseline.json")),"output one-over absent")
publish_bound("output-control",limits.output_bytes+1,"R5_GUARD_OUTPUT_BYTES")

# Every post-fork failure retains a terminal non-accepting disposition and owns
# its exact process group, selector, pipe descriptors, and wait4 through reap.
def stubborn_command(engine,name,raw_size=0):
    code="import os,signal,time;signal.signal(signal.SIGTERM,signal.SIG_IGN);os.set_blocking(1,True);n="+str(raw_size)+";b=b'x'*65536\nwhile n:\n c=b[:min(n,len(b))];os.write(1,c);n-=len(c)\nwhile True: time.sleep(1)"
    return engine.command(name,"python",[f"/proc/self/fd/{engine.tools['python']}","-c",code],"synthetic")
def assert_reaped_rejection(engine,root,name,message,call,descendant_path=None):
    rejects_exact(call,message,name+" exact failure")
    disposition=engine.rejection; check(engine.rejected and disposition is not None and disposition["accepted"] is False,name+" rejected disposition")
    check(disposition["group_verified"] and disposition["leader_reaped"] and disposition["group_dead"] and disposition["pipes_closed"] and not disposition["cleanup_errors"] and disposition["cleanup_failure"] is None and engine.active_child is None,name+" leader reaped, group dead, and pipes closed")
    try: os.killpg(disposition["process_group"],0); group_dead=False
    except ProcessLookupError: group_dead=True
    check(group_dead,name+" process group dead")
    try: os.waitpid(disposition["pid"],os.WNOHANG); unreaped=True
    except ChildProcessError: unreaped=False
    check(not unreaped,name+" no unreaped child")
    if descendant_path is not None:
        descendant=int(open(descendant_path).read())
        check(descendant in [pid for pid,_ in disposition["descendant_reaps"]],name+" adopted descendant recorded")
        try: os.waitpid(descendant,os.WNOHANG); descendant_unreaped=True
        except ChildProcessError: descendant_unreaped=False
        check(not descendant_unreaped,name+" no unreaped descendant")
    for fd in disposition["pipe_fds"]:
        try: os.fstat(fd); closed=False
        except OSError as error: closed=error.errno==errno.EBADF
        check(closed,name+f" pipe {fd} closed")
    command_id=engine.command_sequence
    check(not any(event.get("type")=="command-finish" and event.get("command_id")==command_id for event in engine.journal_events),name+" no command finish")
    check(not any(path.startswith(f"raw/{command_id:03d}-") and path.endswith("/status.json") for path in engine.evidence_files),name+" no status admission")
    before=engine.command_sequence
    rejects_exact(lambda: stubborn_command(engine,name+"-later"),"custody engine is terminally rejected",name+" no later command")
    check(engine.command_sequence==before,name+" later command not started")
    rejects_exact(engine.verify_evidence_tree,"custody engine is terminally rejected",name+" no later verification")
    rejects_exact(lambda: engine.publish(b'{}\n'),"custody engine is terminally rejected",name+" no later publication")
    check(not os.path.exists(os.path.join(root,"metrics/test-suite-baseline.json")),name+" no final metric")

_,eng,_,limits,root=engine_for("lifecycle-raw")
assert_reaped_rejection(eng,root,"lifecycle-raw", "raw evidence stream bound exceeded: lifecycle-raw/stdout",lambda: stubborn_command(eng,"lifecycle-raw",limits.raw_bytes+1))

_,eng,_,limits,root=engine_for("lifecycle-journal-line")
def journal_line_failure(current,_stage): append_journal_line(current,limits.journal_line_bytes+1)
eng.hooks["after-fork:lifecycle-journal-line"]=journal_line_failure
assert_reaped_rejection(eng,root,"lifecycle-journal-line","event journal line exceeds bound",lambda: stubborn_command(eng,"lifecycle-journal-line"))

_,eng,_,limits,root=engine_for("lifecycle-journal-event")
def journal_event_failure(current,_stage): current.event_sequence=limits.journal_events
eng.hooks["after-fork:lifecycle-journal-event"]=journal_event_failure
assert_reaped_rejection(eng,root,"lifecycle-journal-event","event journal count exceeds bound",lambda: stubborn_command(eng,"lifecycle-journal-event"))

_,eng,_,limits,root=engine_for("lifecycle-aggregate-journal")
aggregate_filler=register_file(eng,eng.meta_fd,"meta/lifecycle-aggregate-journal.bin",0)
def aggregate_journal_failure(current,_stage):
    remaining=limits.aggregate_bytes-current.complete_evidence_bytes(); check(remaining>=0,"aggregate lifecycle fixture room"); os.ftruncate(aggregate_filler,remaining); os.fsync(aggregate_filler)
eng.hooks["after-fork:lifecycle-aggregate-journal"]=aggregate_journal_failure
assert_reaped_rejection(eng,root,"lifecycle-aggregate-journal","complete retained evidence aggregate exceeds bound",lambda: stubborn_command(eng,"lifecycle-aggregate-journal"))

_,eng,_,limits,root=engine_for("lifecycle-aggregate-raw")
aggregate_filler=register_file(eng,eng.meta_fd,"meta/lifecycle-aggregate-raw.bin",0)
def aggregate_raw_failure(current,_stage):
    remaining=limits.aggregate_bytes-current.complete_evidence_bytes()-32768; check(remaining>=0,"aggregate raw lifecycle fixture room"); os.ftruncate(aggregate_filler,remaining); os.fsync(aggregate_filler)
eng.hooks["after-fork:lifecycle-aggregate-raw"]=aggregate_raw_failure
assert_reaped_rejection(eng,root,"lifecycle-aggregate-raw","complete retained evidence aggregate exceeds bound",lambda: stubborn_command(eng,"lifecycle-aggregate-raw",2*limits.chunk_bytes))

# PERF-Z-CAL-FR-01..04: exact production Git policy, watch/reproof, and sealed
# terminal-evidence controls.  All repositories and mutations are disposable.
def decode_inotify(fd):
    result=[]
    while True:
        try: data=os.read(fd,65536)
        except BlockingIOError: break
        offset=0
        while offset<len(data):
            wd,mask,cookie,length=struct.unpack_from("iIII",data,offset); offset+=16
            name=data[offset:offset+length].split(b"\0",1)[0].decode("utf-8","strict"); offset+=length
            result.append((mask,name))
    return result
def git_fixture(tag):
    root=tempfile.mkdtemp(prefix=f"git-policy-{tag}.",dir=fixture_dir); os.chmod(root,0o700)
    def run(*args,env=None): return subprocess.run(["/usr/bin/git","-C",root,*args],env=env,stdout=subprocess.PIPE,stderr=subprocess.PIPE,check=True)
    run("init","-q"); run("config","user.email","benchmark@example.invalid"); run("config","user.name","benchmark")
    open(os.path.join(root,"tracked"),"w").write("tracked\n"); run("add","tracked"); run("commit","-qm","fixture")
    time.sleep(1.05); run("status","--porcelain=v1")
    return root,run

# A stable index makes default status acquire/close/delete index.lock.  The
# authenticated no-optional-locks vector produces the same stdout with no
# Git-admin event; this is an exact, non-skipping precondition.
git_root,git_run=git_fixture("optional-lock")
watch=libc.inotify_init1(os.O_NONBLOCK|os.O_CLOEXEC); libc.inotify_add_watch(watch,os.path.join(git_root,".git").encode(),0x8|0x200)
default_status=git_run("status","--porcelain=v1"); time.sleep(0.05); default_events=decode_inotify(watch); os.close(watch)
check(default_status.stdout==b"" and default_events==[(0x8,"index.lock"),(0x200,"index.lock")],"default status exact close/delete index.lock pair")
watch=libc.inotify_init1(os.O_NONBLOCK|os.O_CLOEXEC); libc.inotify_add_watch(watch,os.path.join(git_root,".git").encode(),0x2|0x4|0x8|0x40|0x80|0x100|0x200)
closed_env={"HOME":git_root,"PATH":"/usr/bin:/bin","LANG":"C","LC_ALL":"C","GIT_OPTIONAL_LOCKS":"0","GIT_CONFIG_NOSYSTEM":"1","GIT_CONFIG_SYSTEM":"/dev/null","GIT_CONFIG_GLOBAL":"/dev/null","GIT_CONFIG_COUNT":"0","GIT_ATTR_NOSYSTEM":"1","GIT_TERMINAL_PROMPT":"0"}
policy=["/usr/bin/git","--no-optional-locks","-c","core.fsmonitor=false","-c","core.untrackedCache=false","-c","status.showUntrackedFiles=all","-c","submodule.recurse=false","-c","diff.ignoreSubmodules=none","-C",git_root,"status","--porcelain=v1","--untracked-files=all","--ignore-submodules=none"]
production_status=subprocess.run(policy,env=closed_env,stdout=subprocess.PIPE,stderr=subprocess.PIPE,check=True); time.sleep(0.05); production_events=decode_inotify(watch); os.close(watch)
check(production_status.stdout==default_status.stdout and production_events==[],"production no-optional-locks status is mutation-free")

_,policy_engine,_,_,policy_root=engine_for("git-vector")
policy_engine.tools["git"]=policy_engine.open_node("/usr/bin/git",os.O_RDONLY|os.O_NOFOLLOW,"tool:git")
vector=policy_engine.git_policy_argv("status","--porcelain=v1","--untracked-files=all","--ignore-submodules=none")
policy_engine.require_git_vector(vector,policy_engine.git_child_env()); check(vector[1]=="--no-optional-locks","every production Git vector has the global lock policy")
rejects_exact(lambda: policy_engine.require_git_vector([vector[0],*vector[2:]],policy_engine.git_child_env()),"Git child did not use the authenticated literal policy","missing Git argv policy rejected")
ns,Run,_,_=namespace(policy_root,"FR_GUARD_GIT_VECTOR"); weakened=object.__new__(Run); weakened.tools={"git":policy_engine.tools["git"]}
weakened.require_git_vector([vector[0],*vector[2:]],weakened.git_child_env()); check(True,"isolated Git-vector guard removal accepts default-lock vector")

# Hostile system/global config and runtime-pair variables cannot enter the
# closed child environment; a local/global fsmonitor sentinel is never run and
# explicit untracked behavior remains visible.
hostile_home=os.path.join(git_root,"hostile-home"); os.mkdir(hostile_home,0o700)
sentinel=os.path.join(git_root,"fsmonitor-ran"); hook=os.path.join(git_root,"fsmonitor")
open(hook,"w").write("#!/bin/sh\nprintf ran > '"+sentinel+"'\n"); os.chmod(hook,0o700)
hostile='[status]\nshowUntrackedFiles = no\n[core]\nfsmonitor = '+hook+'\n'
open(os.path.join(hostile_home,".gitconfig"),"w").write(hostile); system_config=os.path.join(git_root,"system.gitconfig"); open(system_config,"w").write(hostile)
open(os.path.join(git_root,"untracked"),"w").write("visible\n")
hostile_parent=dict(os.environ); hostile_parent.update({"HOME":hostile_home,"GIT_CONFIG_SYSTEM":system_config,"GIT_CONFIG_GLOBAL":os.path.join(hostile_home,".gitconfig"),"GIT_CONFIG_COUNT":"1","GIT_CONFIG_KEY_0":"core.fsmonitor","GIT_CONFIG_VALUE_0":hook})
subprocess.run(["/usr/bin/git","-C",git_root,"status","--porcelain=v1"],env=hostile_parent,stdout=subprocess.PIPE,stderr=subprocess.PIPE)
check(os.path.exists(sentinel),"hostile default Git config control executes fsmonitor sentinel"); os.unlink(sentinel)
result=subprocess.run(policy,env=closed_env,stdout=subprocess.PIPE,stderr=subprocess.PIPE,check=True)
check(b"?? untracked" in result.stdout and not os.path.exists(sentinel),"system/global/runtime Git authority excluded and fsmonitor not executed")
check(all(name not in closed_env for name in ("GIT_CONFIG_KEY_0","GIT_CONFIG_VALUE_0")) and closed_env["GIT_CONFIG_COUNT"]=="0","runtime Git config pairs closed")

# Remaining local/worktree config is parsed by the exact production validator.
for label,payload in (
    ("include","[include]\npath = /tmp/hostile\n"),("includeIf","[includeIf \"gitdir:/**\"]\npath=/tmp/hostile\n"),
    ("status.showUntrackedFiles","[status]\nshowUntrackedFiles=no\n"),("core.fsmonitor","[core]\nfsmonitor=/tmp/hook\n"),("core.trustctime","[core]\ntrustctime=false\n")):
    fd=os.memfd_create("git-config-"+label,os.MFD_CLOEXEC); os.write(fd,payload.encode()); os.lseek(fd,0,os.SEEK_SET)
    rejects(lambda fd=fd,label=label: policy_engine.validate_git_config(label,fd),"hostile Git config accepted: "+label); os.close(fd)
benign_fd=os.memfd_create("git-config-benign",os.MFD_CLOEXEC); os.write(benign_fd,b"[core]\nfilemode=true\n[branch \"x\"]\nremote=origin\n"); os.lseek(benign_fd,0,os.SEEK_SET)
check(policy_engine.validate_git_config("benign local config",benign_fd) is False,"benign authenticated Git config accepted"); os.close(benign_fd)

config_root=root_for("git-config-control"); _,ConfigRun,_,_=namespace(config_root,"FR_GUARD_GIT_CONFIG_INCLUDE"); config_control=object.__new__(ConfigRun)
include_fd=os.memfd_create("git-config-include-control",os.MFD_CLOEXEC); os.write(include_fd,b"[include]\npath=/tmp/hostile\n"); os.lseek(include_fd,0,os.SEEK_SET)
check(config_control.validate_git_config("include control",include_fd) is False,"isolated include guard removal accepts include authority"); os.close(include_fd)

def reconciliation_object(tag,disabled=None):
    root=root_for(tag); ns,Run,_,_=namespace(root,disabled); value=object.__new__(Run); value.git_head="a"*40; value.git_branch="branch"; value.tracked_paths=("source/existing",); value.nodes={}; value.authority_hashes={}; return ns,Run,value
_,Run,value=reconciliation_object("identity-production")
rejects_exact(lambda: value.reconcile_git_identity("b"*40,"branch"),"watched Git HEAD or symbolic branch differs from held graph","HEAD reconciliation exact")
_,Run,value=reconciliation_object("identity-control","FR_GUARD_GIT_HEAD_BRANCH_RECONCILIATION"); value.reconcile_git_identity("b"*40,"other"); check(True,"isolated HEAD/branch reconciliation guard removal")
_,Run,value=reconciliation_object("tracked-production")
rejects_exact(lambda: value.reconcile_tracked_paths(("source/other",)),"source tracked-path set changed","tracked-set reconciliation exact")
_,Run,value=reconciliation_object("tracked-control","FR_GUARD_GIT_TRACKED_SET_RECONCILIATION"); value.reconcile_tracked_paths(("source/other",)); check(True,"isolated tracked-set reconciliation guard removal")
def index_reconciliation(tag,disabled=None):
    ns,Run,value=reconciliation_object(tag,disabled); fd=os.memfd_create("index-reconcile",os.MFD_CLOEXEC); os.write(fd,b"index-v1"); value.nodes["config:git-index"]=("memfd",fd,0,0,8,0,0o600,1); value.authority_hashes["config:git-index"]=(8,hashlib.sha256(b"index-v0").hexdigest()); return Run,value,fd
Run,value,fd=index_reconciliation("index-production"); rejects_exact(value.reconcile_git_index,"Git index content changed during watched source authentication","index reconciliation exact"); os.close(fd)
Run,value,fd=index_reconciliation("index-control","FR_GUARD_GIT_INDEX_RECONCILIATION"); value.reconcile_git_index(); check(True,"isolated index reconciliation guard removal"); os.close(fd)

def source_engine(tag,disabled=None):
    ns,engine,_,_,root=engine_for(tag,disabled)
    for rel in ("source","git-admin"):
        path=os.path.join(root,rel); engine.open_node(path,os.O_RDONLY|os.O_DIRECTORY|os.O_NOFOLLOW,"fixture-dir:"+rel,directory=True); engine.add_source_watch(path)
    for rel,label in (("source/existing","source:existing"),("git-admin/HEAD","config:git-head"),("git-admin/index","config:git-index"),("git-admin/config","config:git-config"),("git-admin/config.worktree","config:git-worktree-config")):
        path=os.path.join(root,rel); engine.open_node(path,os.O_RDONLY|os.O_NOFOLLOW,label); engine.retain_authority_hash(label); engine.add_source_watch(path)
    return ns,engine,root
def terminal_document(engine):
    data=engine.read_held(engine.terminal_fd,os.fstat(engine.terminal_fd).st_size)
    return engine.validate_terminal_source_rejection(data)
def source_command_argv(engine): return [f"/proc/self/fd/{engine.tools['python']}","-c","pass"]
def mutation_case(tag,mutate,required_masks):
    _,engine,root=source_engine(tag); name="mutation-"+tag
    engine.hooks["after-fork:"+name]=lambda current,_stage: mutate(current,root)
    try: engine.command(name,"python",source_command_argv(engine),"synthetic"); observed_error=None
    except BaseException as error: observed_error=error
    check(observed_error is not None,tag+" mutation accepted")
    check(os.fstat(engine.terminal_fd).st_size>0,tag+" terminal record is nonempty: error="+repr(observed_error)+" rejection="+repr(engine.rejection))
    document=terminal_document(engine); mutation,rejection=document["records"]; seen={mask for event in mutation["events"] for mask in event["mask_names"]}
    check(set(required_masks)<=seen,tag+" exact event classes")
    active=mutation["active_command"]; check(active["name"]==name and type(active["pid"]) is int and active["pid"]==active["pgid"] and active["argv"]==source_command_argv(engine),tag+" active command evidence")
    check(rejection["post_event_reproof"]["status"] in ("PASS","FAIL") and engine.rejected and engine.rejection["accepted"] is False,tag+" terminal disposition")
    check(engine.rejection["leader_reaped"] and engine.rejection["group_dead"] and engine.rejection["pipes_closed"] and engine.active_child is None,tag+" exact child cleanup")
    rejects_exact(lambda: engine.command(name+"-later","python",source_command_argv(engine),"synthetic"),"custody engine is terminally rejected",tag+" no later command")
    check(not os.path.exists(os.path.join(root,"metrics/test-suite-baseline.json")),tag+" no publication")
    return engine,document
def index_lock(current,root):
    path=os.path.join(root,"git-admin/index.lock"); fd=os.open(path,os.O_WRONLY|os.O_CREAT|os.O_EXCL,0o600); os.write(fd,b"hostile"); os.fsync(fd); os.close(fd); os.unlink(path)
def index_rename(current,root):
    source=os.path.join(root,"run/tmp/hostile-index"); open(source,"wb").write(b"hostile!"); os.chmod(source,0o600); os.replace(source,os.path.join(root,"git-admin/index"))
def index_inplace(current,root):
    fd=os.open(os.path.join(root,"git-admin/index"),os.O_WRONLY|os.O_NOFOLLOW); os.pwrite(fd,b"INDEX-v2",0); os.fsync(fd); os.close(fd)
mutation_case("index-lock",index_lock,{"IN_CREATE","IN_MODIFY","IN_CLOSE_WRITE","IN_DELETE"})
mutation_case("index-rename",index_rename,{"IN_MOVED_TO"})
engine,document=mutation_case("index-inplace",index_inplace,{"IN_MODIFY","IN_CLOSE_WRITE"}); check(document["records"][1]["post_event_reproof"]["status"]=="FAIL","in-place index content reproof failed")

def create_regular(current,root): open(os.path.join(root,"source/new"),"w").write("new\n")
def create_symlink(current,root): os.symlink("existing",os.path.join(root,"source/new-link"))
def create_directory(current,root): os.mkdir(os.path.join(root,"source/new-dir"),0o700)
def create_config_symlink(current,root): os.symlink("config",os.path.join(root,"git-admin/new-link"))
def create_config_directory(current,root): os.mkdir(os.path.join(root,"git-admin/new-dir"),0o700)
def rename_from(current,root): os.rename(os.path.join(root,"source/existing"),os.path.join(root,"run/tmp/moved-source"))
def rename_config_from(current,root): os.rename(os.path.join(root,"git-admin/config.worktree"),os.path.join(root,"run/tmp/moved-worktree-config"))
def write_restore(current,root):
    path=os.path.join(root,"source/existing"); before=os.stat(path); open(path,"w").write("source\n"); os.utime(path,ns=(before.st_atime_ns,before.st_mtime_ns))
def replace_named(relative,data):
    def apply(current,root):
        temporary=os.path.join(root,"run/tmp/replacement"); open(temporary,"wb").write(data); os.chmod(temporary,0o600); os.replace(temporary,os.path.join(root,relative))
    return apply
for tag,mutate,masks in (("source-create",create_regular,{"IN_CREATE"}),("source-symlink",create_symlink,{"IN_CREATE"}),("source-directory",create_directory,{"IN_CREATE"}),("source-rename-from",rename_from,{"IN_MOVED_FROM"}),("config-symlink",create_config_symlink,{"IN_CREATE"}),("config-directory",create_config_directory,{"IN_CREATE"}),("config-rename-from",rename_config_from,{"IN_MOVED_FROM"}),("source-write-restore",write_restore,{"IN_MODIFY","IN_CLOSE_WRITE"}),
    ("head-replace",replace_named("git-admin/HEAD",b"ref: refs/heads/other\n"),{"IN_MOVED_TO"}),("common-config-replace",replace_named("git-admin/config",b"[core]\nfilemode=false\n"),{"IN_MOVED_TO"}),("worktree-config-replace",replace_named("git-admin/config.worktree",b"[core]\nfilemode=false\n"),{"IN_MOVED_TO"})):
    mutation_case(tag,mutate,masks)

# The source-event guard has a meaningful isolated control: a transient create
# and unlink restores the watched directory's size/mtime, so only the exact
# inotify predicate prevents the fake child from being accepted.
_,control,root=source_engine("source-event-control","FR_GUARD_SOURCE_EVENTS"); source_dir=os.path.join(root,"source"); before=os.stat(source_dir)
def transient(current,_stage):
    path=os.path.join(source_dir,"transient"); open(path,"w").write("x"); os.unlink(path); os.utime(source_dir,ns=(before.st_atime_ns,before.st_mtime_ns))
control.hooks["after-fork:source-event-control"]=transient
accepted=control.command("source-event-control","python",source_command_argv(control),"synthetic"); check(accepted["wait"]["code"]==0,"isolated source-event guard removal reaches acceptance")

# Same-inode/same-size/index-content changes are independently owned by the
# descriptor hash guard when event consumption is not part of the oracle.
ns,hash_control,root=source_engine("authority-hash-control","FR_GUARD_AUTHORITY_HASH"); index_path=os.path.join(root,"git-admin/index"); before=os.stat(index_path); changed_fd=os.open(index_path,os.O_WRONLY|os.O_NOFOLLOW); os.pwrite(changed_fd,b"INDEX-v2",0); os.fsync(changed_fd); os.close(changed_fd); os.utime(index_path,ns=(before.st_atime_ns,before.st_mtime_ns)); hash_control.reprove_held_graph(); check(True,"isolated authority-hash guard removal accepts same-fact content mutation")

# Terminal schema corruption/omission/duplication is rejected by the exact
# validator and the dedicated file remains fsynced/readable after latching.
terminal_engine,terminal_value=mutation_case("terminal-schema",index_lock,{"IN_CREATE","IN_DELETE"}); terminal_bytes=terminal_engine.read_held(terminal_engine.terminal_fd,os.fstat(terminal_engine.terminal_fd).st_size)
check(hashlib.sha256(terminal_bytes).hexdigest()==terminal_engine.evidence_files["terminal-rejection.json"][2],"terminal rejection bytes sealed")
for label,value in (("omitted",{**terminal_value,"records":terminal_value["records"][:1]}),("duplicated",{**terminal_value,"records":terminal_value["records"]*2}),("corrupt",{**terminal_value,"records":[{**terminal_value["records"][0],"events":[]},terminal_value["records"][1]]})):
    rejects(lambda value=value: terminal_engine.validate_terminal_source_rejection(json_bytes(value)),"terminal "+label+" accepted")

# R8 process-tree closure uses the exact production RunCustody under a
# test-process subreaper, so hostile grandchildren are both killed and reaped.
def descendant_program(path,status,term_leader=False,ready_output=False,oversized=0):
    return f'''import os,signal,time
child=os.fork()
if child==0:
 os.close(1);os.close(2);signal.signal(signal.SIGTERM,signal.SIG_IGN)
 with open({path!r},"w") as handle: handle.write(str(os.getpid()));handle.flush();os.fsync(handle.fileno())
 while True: time.sleep(1)
while not os.path.exists({path!r}): time.sleep(0.001)
{("signal.signal(signal.SIGTERM,lambda *_: os._exit(0))" if term_leader else "")}
{("os.write(1,b'ready\\n')" if ready_output else "")}
{("n="+str(oversized)+";block=b'x'*65536\nwhile n:\n part=block[:min(n,len(block))];os.write(1,part);n-=len(part)" if oversized else "")}
{("while True: time.sleep(1)" if term_leader or oversized else "raise SystemExit("+str(status)+")")}
'''
def command_argv(engine,code): return [f"/proc/self/fd/{engine.tools['python']}","-c",code]

_,eng,_,limits,root=engine_for("r8-command-zero-descendant"); descendant_path=os.path.join(root,"run/tmp/descendant.pid"); name="r8-command-zero-descendant"; code=descendant_program(descendant_path,0)
assert_reaped_rejection(eng,root,name,f"child process group survived leader exit: {name}",lambda: eng.command(name,"python",command_argv(eng,code),"synthetic"),descendant_path)

_,eng,_,limits,root=engine_for("r8-command-nonzero-descendant"); descendant_path=os.path.join(root,"run/tmp/descendant.pid"); name="r8-command-nonzero-descendant"; code=descendant_program(descendant_path,7)
expected=f"command did not exit green: {name}: {{'kind': 'exited', 'code': 7, 'signal': None}}"
assert_reaped_rejection(eng,root,name,expected,lambda: eng.command(name,"python",command_argv(eng,code),"synthetic"),descendant_path)

_,eng,_,limits,root=engine_for("r8-command-post-wait"); descendant_path=os.path.join(root,"run/tmp/descendant.pid"); name="r8-command-post-wait"; code=descendant_program(descendant_path,0)
def post_wait_failure(current,_stage): raise RuntimeError("synthetic post-wait pre-status failure")
eng.hooks[f"after-wait:{name}"]=post_wait_failure
assert_reaped_rejection(eng,root,name,"synthetic post-wait pre-status failure",lambda: eng.command(name,"python",command_argv(eng,code),"synthetic"),descendant_path)

_,eng,_,limits,root=engine_for("r8-command-term-split"); descendant_path=os.path.join(root,"run/tmp/descendant.pid"); name="r8-command-term-split"; code=descendant_program(descendant_path,0,term_leader=True,ready_output=True)
def after_ready_failure(current,_stage): raise RuntimeError("synthetic drain failure after child readiness")
eng.hooks[f"after-raw:{name}"]=after_ready_failure
assert_reaped_rejection(eng,root,name,"synthetic drain failure after child readiness",lambda: eng.command(name,"python",command_argv(eng,code),"synthetic"),descendant_path)
check(eng.last_child["wait"] is not None and os.WIFEXITED(eng.last_child["wait"][0]) and os.WEXITSTATUS(eng.last_child["wait"][0])==0,"TERM split leader exited cleanly before descendant KILL")

_,eng,_,limits,root=engine_for("r8-bootstrap-green"); result=eng.capture_simple("r8-bootstrap-green","python",command_argv(eng,"import os;os.write(1,b'ok')")); check(result==b"ok" and not eng.rejected and eng.last_child["group_verified"] and eng.last_child["leader_reaped"] and eng.last_child["group_dead"],"bootstrap green leader and group accepted")

for label,status in (("zero",0),("nonzero",9)):
    _,eng,_,limits,root=engine_for("r8-bootstrap-"+label); descendant_path=os.path.join(root,"run/tmp/descendant.pid"); name="r8-bootstrap-"+label; code=descendant_program(descendant_path,status)
    expected=f"bootstrap child process group survived leader exit: {name}" if status==0 else f"bootstrap command failed: {name}"
    assert_reaped_rejection(eng,root,name,expected,lambda eng=eng,name=name,code=code: eng.capture_simple(name,"python",command_argv(eng,code)),descendant_path)

_,eng,_,limits,root=engine_for("r8-bootstrap-oversized"); descendant_path=os.path.join(root,"run/tmp/descendant.pid"); name="r8-bootstrap-oversized"; code=descendant_program(descendant_path,0,oversized=limits.raw_bytes+1)
assert_reaped_rejection(eng,root,name,f"bootstrap output oversized: {name}",lambda: eng.capture_simple(name,"python",command_argv(eng,code)),descendant_path)

Run=eng.__class__; ns=Run.__init__.__globals__
_,prepared,_,_,prepared_root=engine_for("r8-construction-bootstrap"); construction_descendant=os.path.join(prepared_root,"run/tmp/descendant.pid"); construction_name="r8-construction-bootstrap"; construction_code=descendant_program(construction_descendant,11)
class BootstrapConstructionFailure(Run):
    instance=None
    def __init__(self):
        type(self).instance=self; self.__dict__.update(prepared.__dict__)
        Run.capture_simple(self,construction_name,"python",command_argv(self,construction_code))
    def run(self): raise AssertionError("bootstrap construction failure reached run")
rejects_exact(lambda: ns["execute_custody"](BootstrapConstructionFailure),f"bootstrap command failed: {construction_name}","bootstrap failure crossed outer construction boundary")
constructed=BootstrapConstructionFailure.instance; disposition=constructed.rejection; descendant=int(open(construction_descendant).read())
check(constructed.rejected and disposition["leader_reaped"] and disposition["group_dead"] and disposition["cleanup_failure"] is None,"outer bootstrap construction latched complete rejection")
check(descendant in [pid for pid,_ in disposition["descendant_reaps"]],"outer bootstrap construction reaped descendant")
try: os.waitpid(descendant,os.WNOHANG); construction_descendant_unreaped=True
except ChildProcessError: construction_descendant_unreaped=False
check(not construction_descendant_unreaped and not os.path.exists(os.path.join(prepared_root,"metrics/test-suite-baseline.json")),"outer bootstrap construction left no child or metric")

class ConstructionFailure(Run):
    instance=None
    def __init__(self):
        type(self).instance=self; self.active_child=None; self.final_identity=None; self.publication=None; self.rejected=False; self.rejection=None
        raise RuntimeError("synthetic construction failure")
    def run(self): raise AssertionError("construction failure reached run")
rejects_exact(lambda: ns["execute_custody"](ConstructionFailure),"synthetic construction failure","partially initialized outer rollback")
check(ConstructionFailure.instance.rejected and ConstructionFailure.instance.rejection["accepted"] is False,"partially initialized construction latched rejection")

# Publication parent transitions: exact live publish plus missing/extra event controls.
_,eng,_,_,root=engine_for("publish-positive"); eng.publish(b'{"r5":true}\n'); final=os.path.join(root,"metrics/test-suite-baseline.json"); check(open(final,"rb").read()==b'{"r5":true}\n' and os.stat(final).st_nlink==1,"publication state machine positive")
def transition_case(tag,disabled,extra,drain):
    ns,eng,_,_,root=engine_for(tag,disabled); parent=os.open(os.path.join(root,"metrics"),os.O_RDONLY|os.O_DIRECTORY|os.O_NOFOLLOW); eng.fds.append(parent); eng.watch_publication_parent(parent,os.path.join(root,"metrics")); private=".candidate"; eng.publication={"parent_fd":parent,"private":private,"final":"test-suite-baseline.json","identity":None,"linked":False,"state":"absent","base_entries":sorted(os.listdir(parent))}; eng.publication["parent_facts"]=eng.publication_parent_facts(); fd=os.open(private,os.O_RDWR|os.O_CREAT|os.O_EXCL|os.O_NOFOLLOW,0o600,dir_fd=parent); os.close(fd)
    if extra: extra_fd=os.open("extra",os.O_RDWR|os.O_CREAT|os.O_EXCL|os.O_NOFOLLOW,0o600,dir_fd=parent); os.close(extra_fd)
    if drain: eng.publication_events(wait=True)
    eng.transition_publication("private",0x00000100,private); return eng
rejects(lambda: transition_case("transition-missing",None,False,True),"missing publication event")
check(transition_case("transition-missing-control","R5_GUARD_PUBLICATION_TRANSITION",False,True).publication["state"]=="private","missing transition guard removal")
rejects(lambda: transition_case("transition-extra",None,True,False),"extra publication event")
check(transition_case("transition-extra-control","R5_GUARD_PUBLICATION_TRANSITION",True,False).publication["state"]=="private","extra transition guard removal")

# Same-inode rewrites at all four stages are rejected; isolated predicate-disabled controls publish the rewrite.
for stage in ("before-exclusive-link","after-exclusive-link","before-private-unlink","after-private-unlink"):
    def install(engine):
        def rewrite(current,_stage):
            fd=current.publication["candidate_fd"]; os.ftruncate(fd,0); os.pwrite(fd,b'{"tampered":true}\n',0); os.fsync(fd)
        engine.hooks[stage]=rewrite
    _,eng,_,_,_=engine_for("candidate-"+stage); install(eng); rejects(lambda eng=eng: eng.publish(b'{"r5":true}\n'),f"candidate rewrite {stage}")
    _,eng,_,_,root=engine_for("candidate-control-"+stage,"R5_GUARD_CANDIDATE_CONTENT"); install(eng); eng.publish(b'{"r5":true}\n'); check(open(os.path.join(root,"metrics/test-suite-baseline.json"),"rb").read()==b'{"tampered":true}\n',f"candidate guard removal {stage}")

# Component order is reconstructed from journaled cross-stream positions.
valid=[(2,b"Summary [0.1s] 1 test run: 1 passed, 0 skipped\n"),(2,b"Doc-tests crate_a\n"),(1,b"test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.01s\n"),(2,b"Running rsi-model-control-validate\n"),(2,b"Running rsi-provider-capability-validate\n")]
_,eng,_,_,_=engine_for("order-positive"); norm,_=eng.normalize_lane(emit(eng,"order-positive",valid),True,["pkg::crate_a"]); check([x["name"] for x in norm["components"]]==["nextest","doctests","model-control-validator","provider-capability-validator"],"journal order positive")
for label,writes in (("validator-before",[valid[0],valid[1],valid[3],valid[2],valid[4]]),("provider-before-model",[valid[0],valid[1],valid[2],valid[4],valid[3]]),("doctest-before-nextest",[valid[1],valid[2],valid[0],valid[3],valid[4]])):
    _,eng,_,_,_=engine_for(label); rec=emit(eng,label,writes); rejects(lambda eng=eng,rec=rec: eng.normalize_lane(rec,True,["pkg::crate_a"]),label)
    _,eng,_,_,_=engine_for(label+"-control","R5_GUARD_COMPONENT_ORDER"); rec=emit(eng,label+"-control",writes); norm,_=eng.normalize_lane(rec,True,["pkg::crate_a"]); check(len(norm["components"])==4,label+" guard removal")

# Every current/legacy Cargo discovery name is rejected before a fake child;
# the isolated discovery-predicate control reaches that child.
for location,name in (("repo","config"),("repo","credentials.toml"),("repo","credentials"),("home","config"),("home","credentials.toml"),("home","credentials"),("ancestor","config.toml"),("ancestor","config"),("ancestor","credentials.toml"),("ancestor","credentials")):
    def cargo_case(tag,disabled):
        _,eng,_,_,root=engine_for(tag,disabled); sentinel=os.path.join(root,"run/tmp/child-ran")
        def create(current,_stage):
            base=os.path.join(root,".cargo") if location=="repo" else os.path.join(root,"cargo-home") if location=="home" else os.path.join(os.path.dirname(root),".cargo")
            os.makedirs(base,mode=0o700,exist_ok=True); path=os.path.join(base,name); open(path,"w").write("[build]\nrustflags=[]\n"); os.chmod(path,0o600)
        eng.hooks["before-command:cargo-probe"]=create
        code=f"open({sentinel!r},'w').write('ran')"
        eng.command("cargo-probe","python",[f"/proc/self/fd/{eng.tools['python']}","-c",code],"synthetic")
        return os.path.exists(sentinel)
    rejects(lambda location=location,name=name: cargo_case(f"cargo-{location}-{name}",None),f"Cargo discovery {location}/{name}")
    check(cargo_case(f"cargo-control-{location}-{name}","R5_GUARD_CARGO_DISCOVERY_REPROOF"),f"Cargo discovery guard removal {location}/{name}")

print(count)
PY
)"
    [[ "$r5_cases" =~ ^[1-9][0-9]*$ ]]
    cases=$((cases + r5_cases))
    printf '%s\n' "$cases"
}

self_test() {
    require_jq; need_command git
    local fixture_dir stub_dir external_dir external_base baseline candidate budget result script_path
    local fixture_head fixture_branch fixture_logical_cpus fixture_target_dir fixture_target_created=0
    local hook_dir hook_stage_fifo hook_continue_fifo hook_status_file hook_sentinel hook_all_cargo_log hook_pid hook_stage hook_out hook_root hook_result
    fixture_dir="$(mktemp -d)"; external_base="$(mktemp -d)"; stub_dir="$fixture_dir/stub"; external_dir="$fixture_dir/external"; mkdir "$stub_dir" "$external_dir"; script_path="$(cd "$(dirname "$0")" && pwd -P)/$(basename "$0")"
    fixture_target_dir="$(realpath -m "${CARGO_TARGET_DIR:-$fixture_dir/target}")"
    if [ ! -d "$fixture_target_dir" ]; then mkdir -p "$fixture_target_dir"; fixture_target_created=1; fi
    trap 'rm -rf "$fixture_dir" "$external_base"; if [ "$fixture_target_created" -eq 1 ]; then rmdir "$fixture_target_dir" 2>/dev/null || true; fi' RETURN

    new_external_root() {
        local root
        root="$(mktemp -d "$external_base/root.XXXXXX")"
        chmod 0700 "$root"
        printf '%s\n' "$root"
    }

    oracle_failure() {
        printf 'self-test oracle failure: %s\n' "$*" >&2
        return 1
    }

    oracle_status() {
        local expected="$1" actual="$2" label="$3"
        if [ "$actual" -ne "$expected" ]; then
            oracle_failure "$label: expected status $expected, got $actual"
            return 1
        fi
        return 0
    }

    oracle_empty_log() {
        local path="$1" label="$2"
        if [ -s "$path" ]; then
            oracle_failure "$label: unexpected invocation log content"
            return 1
        fi
        return 0
    }

    oracle_nonempty_log() {
        local path="$1" label="$2"
        if [ ! -s "$path" ]; then
            oracle_failure "$label: required invocation log is empty"
            return 1
        fi
        return 0
    }

    oracle_absent() {
        local path="$1" label="$2"
        if [ -e "$path" ] || [ -L "$path" ]; then
            oracle_failure "$label: unexpected path remains: $path"
            return 1
        fi
        return 0
    }

    oracle_regular() {
        local path="$1" label="$2"
        if [ ! -f "$path" ] || [ -L "$path" ]; then
            oracle_failure "$label: required regular file is absent or substituted: $path"
            return 1
        fi
        return 0
    }

    oracle_present() {
        local path="$1" label="$2"
        if [ ! -e "$path" ] && [ ! -L "$path" ]; then
            oracle_failure "$label: required preexisting path disappeared: $path"
            return 1
        fi
        return 0
    }

    oracle_directory() {
        local path="$1" label="$2"
        if [ ! -d "$path" ] || [ -L "$path" ]; then
            oracle_failure "$label: required directory is absent or substituted: $path"
            return 1
        fi
        return 0
    }

    oracle_symlink() {
        local path="$1" label="$2"
        if [ ! -L "$path" ]; then
            oracle_failure "$label: required symlink is absent: $path"
            return 1
        fi
        return 0
    }

    oracle_equal() {
        local expected="$1" actual="$2" label="$3"
        if [ "$actual" != "$expected" ]; then
            oracle_failure "$label: expected '$expected', got '$actual'"
            return 1
        fi
        return 0
    }

    oracle_contains() {
        local needle="$1" path="$2" label="$3"
        if ! grep -F -- "$needle" "$path" >/dev/null; then
            oracle_failure "$label: missing expected text '$needle'"
            return 1
        fi
        return 0
    }

    assert_before_work_timing() {
        local status="$1" all_cargo_log="$2" benchmark_log="$3" label="$4"
        if ! oracle_status 2 "$status" "$label"; then return 1; fi
        if ! oracle_empty_log "$all_cargo_log" "$label all-Cargo"; then return 1; fi
        if ! oracle_empty_log "$benchmark_log" "$label benchmark"; then return 1; fi
        return 0
    }

    assert_before_work_disposition() {
        local status="$1" all_cargo_log="$2" benchmark_log="$3" out="$4" label="$5"
        if ! assert_before_work_timing "$status" "$all_cargo_log" "$benchmark_log" "$label"; then return 1; fi
        if ! oracle_absent "$out" "$label output"; then return 1; fi
        return 0
    }

    assert_late_reject_disposition() {
        local status="$1" all_cargo_log="$2" benchmark_log="$3" label="$4"
        if ! oracle_status 2 "$status" "$label"; then return 1; fi
        if ! oracle_nonempty_log "$all_cargo_log" "$label all-Cargo"; then return 1; fi
        if ! oracle_nonempty_log "$benchmark_log" "$label benchmark"; then return 1; fi
        return 0
    }

    assert_success_disposition() {
        local expected_status="$1" status="$2" all_cargo_log="$3" benchmark_log="$4" out="$5" label="$6"
        if ! oracle_status "$expected_status" "$status" "$label"; then return 1; fi
        if ! oracle_nonempty_log "$all_cargo_log" "$label all-Cargo"; then return 1; fi
        if ! oracle_nonempty_log "$benchmark_log" "$label benchmark"; then return 1; fi
        if ! oracle_regular "$out" "$label output"; then return 1; fi
        return 0
    }

    expect_external_reject_before_work() {
        local root="$1" out="$2" fault="${3:-}" all_cargo_log benchmark_log inventory="" status out_was_present=0
        all_cargo_log="$(mktemp "$fixture_dir/all-cargo-negative.XXXXXX")"
        benchmark_log="$(mktemp "$fixture_dir/benchmark-negative.XXXXXX")"
        if [ -e "$out" ] || [ -L "$out" ]; then out_was_present=1; fi
        if [[ "$root" = "$external_base"/* ]] && [ -d "$root" ] && [ ! -L "$root" ]; then
            inventory="$(mktemp "$fixture_dir/inventory-before.XXXXXX")"
            runner_inventory_snapshot "$root" "$inventory"
        fi
        set +e
        (cd "$fixture_dir" && env PATH="$stub_dir:$PATH" S1Q_SELF_TEST_MODE=1 S1Q_TEST_FAULT="$fault" \
            FAKE_CARGO_ALL_INVOCATIONS="$all_cargo_log" FAKE_CARGO_INVOCATIONS="$benchmark_log" \
            "$script_path" capture --label reject --probe store-fixture --repeat 1 --threads 1 \
            --external-output-root "$root" --out "$out") >/dev/null 2>&1
        status=$?
        set -e
        assert_before_work_timing "$status" "$all_cargo_log" "$benchmark_log" "external early rejection"
        if [ "$out_was_present" -eq 1 ]; then
            oracle_present "$out" "external early-rejection preexisting output"
        else
            oracle_absent "$out" "external early-rejection output"
        fi
        if [ -n "$inventory" ]; then runner_inventory_assert "$root" "$inventory"; fi
    }

    expect_parser_reject_before_cargo() {
        local all_cargo_log benchmark_log status
        all_cargo_log="$(mktemp "$fixture_dir/all-cargo-parser.XXXXXX")"
        benchmark_log="$(mktemp "$fixture_dir/benchmark-parser.XXXXXX")"
        set +e
        (cd "$fixture_dir" && env PATH="$stub_dir:$PATH" FAKE_CARGO_ALL_INVOCATIONS="$all_cargo_log" \
            FAKE_CARGO_INVOCATIONS="$benchmark_log" \
            "$script_path" "$@") >/dev/null 2>&1
        status=$?
        set -e
        assert_before_work_disposition "$status" "$all_cargo_log" "$benchmark_log" \
            "$fixture_dir/parser-output-must-remain-absent" "parser rejection"
    }

    assert_external_capture_custody() {
        local root="$1" out="$2"
        python3 - "$root" "$out" <<'PY'
import json
import os
import stat
import sys

root, out = sys.argv[1:]
def require(condition, message):
    if not condition:
        raise RuntimeError(message)

require(os.path.realpath(root) == root, "root is not canonical")
require(os.path.realpath(out) == out, "output is not canonical")
with open(out, encoding="utf-8") as stream:
    capture = json.load(stream)
require(capture["schema_version"] == 2, "unexpected capture schema")
workspace = capture["capture"]["workspace"]
require(workspace.startswith(root + "/") and os.path.realpath(workspace) == workspace, "workspace escaped root")
paths = [
    capture["capture"]["warmup"]["logs"]["stdout"],
    capture["capture"]["warmup"]["logs"]["stderr"],
    capture["capture"]["warmup"]["logs"]["time"],
]
for sample in capture["samples"]:
    paths.extend(sample["execution"]["logs"].values())
for path in paths:
    require(path.startswith(root + "/") and os.path.realpath(path) == path, f"log escaped root: {path}")
for current, directories, files in os.walk(workspace, followlinks=False):
    current_st = os.lstat(current)
    require(stat.S_ISDIR(current_st.st_mode) and stat.S_IMODE(current_st.st_mode) == 0o700, f"invalid directory: {current}")
    require(current_st.st_uid == os.geteuid() and current_st.st_gid == os.getegid(), f"wrong directory owner: {current}")
    for name in directories:
        child = os.path.join(current, name)
        child_st = os.lstat(child)
        require(stat.S_ISDIR(child_st.st_mode) and not stat.S_ISLNK(child_st.st_mode), f"invalid child directory: {child}")
    for name in files:
        child = os.path.join(current, name)
        child_st = os.lstat(child)
        require(stat.S_ISREG(child_st.st_mode) and stat.S_IMODE(child_st.st_mode) == 0o600, f"invalid child file: {child}")
        require(child_st.st_uid == os.geteuid() and child_st.st_gid == os.getegid() and child_st.st_nlink == 1, f"invalid child custody: {child}")
out_st = os.lstat(out)
require(stat.S_ISREG(out_st.st_mode) and stat.S_IMODE(out_st.st_mode) == 0o600 and out_st.st_nlink == 1, "invalid final output")
require(not any(name.startswith(".capture-json.") for name in os.listdir(os.path.dirname(out))), "private JSON remains")
PY
    }

    runner_leaf() {
        local parent="$1" prefix="$2"
        python3 - "$parent" "$prefix" <<'PY'
import os
import re
import sys

matches = [name for name in os.listdir(sys.argv[1]) if name.startswith(sys.argv[2])]
if len(matches) != 1:
    raise RuntimeError(f"expected one runner leaf, got {matches}")
if re.fullmatch(re.escape(sys.argv[2]) + r"[0-9a-f]{32}", matches[0]) is None:
    raise RuntimeError(f"runner leaf has invalid token: {matches[0]}")
print(matches[0])
PY
    }

    node_fingerprint() {
        python3 - "$1" <<'PY'
import hashlib
import os
import stat
import sys

path = sys.argv[1]
node = os.lstat(path)
digest = "-"
if stat.S_ISREG(node.st_mode):
    with open(path, "rb") as stream:
        digest = hashlib.sha256(stream.read()).hexdigest()
print(f"{node.st_dev}:{node.st_ino}:{node.st_nlink}:{stat.S_IMODE(node.st_mode):04o}:{digest}")
PY
    }

    # Inventories enumerate through held no-follow descriptors. Every fixture
    # snapshots its fresh root and adversarial targets before the runner starts.
    # Hook assertions then compose that immutable baseline with an exact stage
    # schema and only explicitly named fixture injections.
    runner_inventory_helper() {
        python3 - "$@" <<'PY'
import hashlib
import json
import os
import re
import stat
import sys

action = sys.argv[1]
args = sys.argv[2:]

def fail(message):
    raise RuntimeError(message)

def require(condition, message):
    if not condition:
        fail(message)

def node_type(mode):
    if stat.S_ISREG(mode):
        return "regular"
    if stat.S_ISDIR(mode):
        return "directory"
    if stat.S_ISLNK(mode):
        return "symlink"
    if stat.S_ISFIFO(mode):
        return "fifo"
    if stat.S_ISSOCK(mode):
        return "socket"
    if stat.S_ISCHR(mode):
        return "character"
    if stat.S_ISBLK(mode):
        return "block"
    return "other"

def same_object(left, right):
    return (
        left.st_dev,
        left.st_ino,
        left.st_nlink,
        stat.S_IFMT(left.st_mode),
        stat.S_IMODE(left.st_mode),
    ) == (
        right.st_dev,
        right.st_ino,
        right.st_nlink,
        stat.S_IFMT(right.st_mode),
        stat.S_IMODE(right.st_mode),
    )

def regular_digest(directory_fd, leaf, expected):
    file_fd = os.open(leaf, os.O_RDONLY | os.O_NOFOLLOW | os.O_CLOEXEC, dir_fd=directory_fd)
    try:
        actual = os.fstat(file_fd)
        require(stat.S_ISREG(actual.st_mode) and same_object(expected, actual), f"regular node changed while inventorying: {leaf}")
        digest = hashlib.sha256()
        while True:
            chunk = os.read(file_fd, 1024 * 1024)
            if not chunk:
                break
            digest.update(chunk)
        return digest.hexdigest()
    finally:
        os.close(file_fd)

def record(path, node, digest="-"):
    return {
        "path": path,
        "type": node_type(node.st_mode),
        "dev": node.st_dev,
        "ino": node.st_ino,
        "nlink": node.st_nlink,
        "mode": f"{stat.S_IMODE(node.st_mode):04o}",
        "uid": node.st_uid,
        "gid": node.st_gid,
        "digest": digest,
    }

def inventory(scope, ignored_identities=frozenset()):
    root_fd = os.open(scope, os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW | os.O_CLOEXEC)
    records = []
    try:
        root = os.fstat(root_fd)
        records.append(record(".", root))

        def walk(directory_fd, prefix, directory_record):
            ignored_child_directories = 0
            for leaf in sorted(os.listdir(directory_fd)):
                node = os.stat(leaf, dir_fd=directory_fd, follow_symlinks=False)
                relative = leaf if prefix == "." else f"{prefix}/{leaf}"
                identity = (node.st_dev, node.st_ino)
                if identity in ignored_identities:
                    if stat.S_ISDIR(node.st_mode):
                        ignored_child_directories += 1
                    continue
                digest = regular_digest(directory_fd, leaf, node) if stat.S_ISREG(node.st_mode) else "-"
                records.append(record(relative, node, digest))
                if stat.S_ISDIR(node.st_mode):
                    child_fd = os.open(
                        leaf,
                        os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW | os.O_CLOEXEC,
                        dir_fd=directory_fd,
                    )
                    try:
                        child = os.fstat(child_fd)
                        require(same_object(node, child), f"directory changed while inventorying: {relative}")
                        walk(child_fd, relative, records[-1])
                    finally:
                        os.close(child_fd)
            directory_record["nlink"] -= ignored_child_directories

        walk(root_fd, ".", records[0])
    finally:
        os.close(root_fd)
    return records

def write_json(path, value):
    with open(path, "w", encoding="utf-8") as stream:
        json.dump(value, stream, sort_keys=True, separators=(",", ":"))
        stream.write("\n")

def read_json(path):
    with open(path, encoding="utf-8") as stream:
        return json.load(stream)

def path_parent(path):
    if path == ".":
        return None
    return "." if "/" not in path else path.rsplit("/", 1)[0]

def remap_path(path, prefix):
    if not prefix:
        return path
    return prefix if path == "." else f"{prefix}/{path}"

def excluded(path, prefixes):
    return any(path == prefix or path.startswith(prefix + "/") for prefix in prefixes)

def compose(*components):
    # A component is (records, prefix, excluded-prefixes). Directory link
    # counts are adjusted only for explicitly composed child directories.
    assembled = {}
    origins = {}
    for records, prefix, exclusions in components:
        original_directories = {item["path"] for item in records if item["type"] == "directory"}
        original_child_counts = {
            path: sum(1 for child in original_directories if path_parent(child) == path)
            for path in original_directories
        }
        for item in records:
            if excluded(item["path"], exclusions):
                continue
            mapped = remap_path(item["path"], prefix)
            require(mapped not in assembled, f"duplicate expected inventory path: {mapped}")
            copied = dict(item)
            copied["path"] = mapped
            assembled[mapped] = copied
            origins[mapped] = original_child_counts.get(item["path"], 0)
    expected_directories = {path for path, item in assembled.items() if item["type"] == "directory"}
    for path, item in assembled.items():
        if item["type"] == "directory":
            expected_children = sum(1 for child in expected_directories if path_parent(child) == path)
            item["nlink"] += expected_children - origins[path]
    return [assembled[path] for path in sorted(assembled)]

def require_inventory(scope, expected, label):
    actual = inventory(scope)
    if actual != expected:
        fail(
            f"{label} inventory mismatch\nexpected=" + json.dumps(expected, sort_keys=True) +
            "\nactual=" + json.dumps(actual, sort_keys=True)
        )
    return actual

def actual_records(actual, paths):
    by_path = {item["path"]: item for item in actual}
    missing = sorted(set(paths) - set(by_path))
    require(not missing, f"required inventory paths are absent: {missing}")
    return [by_path[path] for path in sorted(paths)]

def require_owned_node(item, expected_type, expected_mode, expected_links=None):
    require(item["type"] == expected_type, f"wrong node type: {item}")
    require(item["mode"] == expected_mode, f"wrong node mode: {item}")
    require(item["uid"] == os.geteuid() and item["gid"] == os.getegid(), f"wrong node owner: {item}")
    if expected_links is not None:
        require(item["nlink"] == expected_links, f"wrong node link count: {item}")

def unique_matching(paths, pattern, label):
    matches = sorted(path for path in paths if re.fullmatch(pattern, path))
    require(len(matches) == 1, f"expected exactly one {label}, got {matches}")
    return matches[0]

def require_timing(value, label):
    require(isinstance(value, dict), f"{label} timing is not an object")
    require(set(value) == {"backend", "wall_seconds", "user_seconds", "system_seconds", "max_rss_kib", "max_rss_disposition"},
            f"unexpected {label} timing fields: {sorted(value)}")
    require(value["backend"] in ("gnu_verbose", "bash_portable"), f"invalid {label} timing backend")
    for field in ("wall_seconds", "user_seconds", "system_seconds"):
        number = value[field]
        require(isinstance(number, (int, float)) and not isinstance(number, bool) and number >= 0,
                f"invalid {label} {field}")
    maximum_rss = value["max_rss_kib"]
    require(maximum_rss is None or (
        isinstance(maximum_rss, (int, float)) and not isinstance(maximum_rss, bool) and maximum_rss >= 0
    ), f"invalid {label} max_rss_kib")
    expected_disposition = "unavailable_portable_backend" if maximum_rss is None else "measured"
    require(value["max_rss_disposition"] == expected_disposition, f"invalid {label} max_rss_disposition")

def stage_schema(actual, baseline, out_leaf, stage):
    require(stage in ("before-work", "before-publish", "after-link"), f"invalid stage: {stage}")
    actual_paths = {item["path"] for item in actual}
    baseline_paths = {item["path"] for item in baseline}
    new_paths = actual_paths - baseline_paths
    workspace = unique_matching(new_paths, r"\.capture-workspace\.[0-9a-f]{32}", "workspace")
    private = unique_matching(new_paths, r"\.capture-json\.[0-9a-f]{32}", "private JSON")
    logs = unique_matching(new_paths, re.escape(workspace) + r"/logs-[0-9a-f]{32}", "logs directory")
    samples = unique_matching(new_paths, re.escape(workspace) + r"/samples-[0-9a-f]{32}", "samples directory")
    paths = {
        workspace,
        private,
        logs,
        samples,
        f"{workspace}/identity.json",
        f"{workspace}/samples.json",
    }
    if stage in ("before-publish", "after-link"):
        paths.update({
            f"{workspace}/1.failures.json",
            f"{workspace}/1.executed.json",
            f"{workspace}/1.runnable.json",
            f"{workspace}/1.ignored.json",
            f"{workspace}/1.proof.json",
            f"{logs}/warmup.stdout.log",
            f"{logs}/warmup.stderr.log",
            f"{logs}/warmup.time.log",
            f"{logs}/warmup.status.json",
            f"{logs}/sample-1.stdout.log",
            f"{logs}/sample-1.stderr.log",
            f"{logs}/sample-1.time.log",
            f"{logs}/sample-1.status.json",
            f"{samples}/1.json",
        })
    if stage == "after-link":
        paths.add(out_leaf)
    require(new_paths == paths, f"unexpected runner stage paths: extra={sorted(new_paths - paths)} missing={sorted(paths - new_paths)}")
    records = actual_records(actual, paths)
    by_path = {item["path"]: item for item in records}
    for path in (workspace, logs, samples):
        require_owned_node(by_path[path], "directory", "0700")
    for path in paths - {workspace, logs, samples}:
        links = 2 if stage == "after-link" and path in (private, out_leaf) else 1
        require_owned_node(by_path[path], "regular", "0600", links)
    if stage == "after-link":
        private_item = by_path[private]
        final_item = by_path[out_leaf]
        require((private_item["dev"], private_item["ino"], private_item["digest"]) ==
                (final_item["dev"], final_item["ino"], final_item["digest"]),
                "after-link private/final identity mismatch")
    return {
        "stage": stage,
        "workspace": workspace,
        "private": private,
        "final": out_leaf if stage == "after-link" else None,
        "logs": logs,
        "samples": samples,
        "nodes": records,
        "identities": sorted({(item["dev"], item["ino"]) for item in records}),
    }

def parse_named_specs(specs):
    parsed = []
    for spec in specs:
        require(":" in spec, f"invalid named-node specification: {spec}")
        path, expected_type = spec.rsplit(":", 1)
        require(path and path != ".", f"invalid named-node path: {spec}")
        require(expected_type in ("regular", "directory", "symlink", "fifo", "socket"), f"invalid named-node type: {spec}")
        parsed.append((path, expected_type))
    require(len({path for path, _ in parsed}) == len(parsed), "duplicate named-node specification")
    return parsed

def record_named_additions(scope, baseline, state, specs):
    actual = inventory(scope)
    parsed = parse_named_specs(specs)
    paths = [path for path, _ in parsed]
    additions = actual_records(actual, paths)
    for item, (_, expected_type) in zip(additions, sorted(parsed)):
        require(item["type"] == expected_type, f"named injection has wrong type: {item}")
        if expected_type == "directory":
            require_owned_node(item, "directory", "0700")
        elif expected_type == "regular":
            require_owned_node(item, "regular", "0600", 1)
    components = [(baseline, "", ())]
    if state is not None:
        components.append((state["nodes"], "", ()))
    components.append((additions, "", ()))
    require_inventory(scope, compose(*components), "baseline + runner schema + named injection")
    return {"nodes": additions, "identities": sorted({(item["dev"], item["ino"]) for item in additions})}

def successful_schema(root, out, baseline, expected_label, expected_outcome,
                      expected_head, expected_branch, expected_logical_cpus,
                      expected_target_dir):
    capture = read_json(out)
    require(capture.get("schema_version") == 2, "unexpected successful capture schema")
    require(set(capture) == {"schema_version", "capture", "samples"},
            f"unexpected successful capture fields: {sorted(capture)}")
    metadata = capture.get("capture")
    require(isinstance(metadata, dict), "successful capture metadata is not an object")
    require(set(metadata) == {
        "captured_at", "label", "probe", "profile", "source", "host", "toolchain", "target",
        "requested", "workspace", "warmup", "execution",
    }, f"unexpected successful capture metadata fields: {sorted(metadata)}")
    require(expected_outcome in ("green", "warmup-red"), f"invalid expected capture outcome: {expected_outcome}")
    expected_command = (
        "cargo test -p rsid --lib "
        "store::tests::load_sessions_survives_legacy_comma_fraction_timestamp "
        "-- --exact --test-threads 1"
    )
    expected_identity = ["store::tests::load_sessions_survives_legacy_comma_fraction_timestamp"]
    expected_source = {"head": expected_head, "branch": expected_branch, "clean_tree": True}
    host = os.uname()
    expected_host = {
        "class": f"{host.sysname}-{host.machine}-{expected_logical_cpus}cpu",
        "os": host.sysname,
        "kernel": host.release,
        "architecture": host.machine,
        "cpu_model": next((line.split(":", 1)[1].strip() for line in open("/proc/cpuinfo", encoding="utf-8") if line.startswith("model name")), "unknown") if os.path.exists("/proc/cpuinfo") else "unknown",
        "logical_cpus": expected_logical_cpus,
    }
    host_fields = "|".join(str(expected_host[field]) for field in ("class", "os", "kernel", "architecture", "cpu_model", "logical_cpus"))
    expected_host["fingerprint_sha256"] = hashlib.sha256(host_fields.encode()).hexdigest()
    target_st = os.stat(expected_target_dir)
    target_fields = f"{expected_target_dir}|{target_st.st_dev}|{target_st.st_ino}"
    expected_target = {
        "canonical_path": expected_target_dir,
        "device": str(target_st.st_dev),
        "inode": str(target_st.st_ino),
        "fingerprint_sha256": hashlib.sha256(target_fields.encode()).hexdigest(),
    }
    expected_toolchain = {
        "cargo": "cargo fixture",
        "rustc": "rustc fixture",
        "cargo_nextest": "cargo-nextest fixture",
        "cargo_target_dir": expected_target_dir,
    }
    expected_requested = {"repeat": 1, "threads": 1, "resolved_threads": 1, "stop_on_first_red": False}
    expected_execution = {
        "command": expected_command,
        "profile": "",
        "test_identity": expected_identity,
        "identity_mode": "exact",
    }
    require(re.fullmatch(r"[0-9]{4}-[0-9]{2}-[0-9]{2}T[0-9]{2}:[0-9]{2}:[0-9]{2}Z", metadata["captured_at"]) is not None,
            "successful capture timestamp is not canonical UTC seconds")
    require(metadata["label"] == expected_label, "successful capture label differs from fixture contract")
    require(metadata["probe"] == "store-fixture", "successful capture probe differs from fixture contract")
    require(metadata["profile"] == "", "successful capture profile differs from fixture contract")
    require(metadata["source"] == expected_source, "successful capture source differs from fixture contract")
    require(metadata["host"] == expected_host, "successful capture host differs from fixture contract")
    require(metadata["toolchain"] == expected_toolchain, "successful capture toolchain differs from fixture contract")
    require(metadata["target"] == expected_target, "successful capture target differs from fixture contract")
    require(metadata["requested"] == expected_requested, "successful capture request differs from --repeat 1/--threads 1 fixture contract")
    require(metadata["execution"] == expected_execution, "successful capture execution differs from fixture contract")
    workspace_abs = metadata["workspace"]
    require(workspace_abs.startswith(root + "/") and os.path.realpath(workspace_abs) == workspace_abs, "successful workspace escaped root")
    workspace = os.path.relpath(workspace_abs, root)
    require(re.fullmatch(r"(?:[^/]+/)*\.capture-workspace\.[0-9a-f]{32}", workspace) is not None, "invalid successful workspace path")
    out_relative = os.path.relpath(out, root)
    require(not out_relative.startswith("../") and out_relative != ".", "successful output escaped root")
    require(path_parent(workspace) == path_parent(out_relative), "workspace is not beside final output")
    actual = inventory(root)
    actual_paths = {item["path"] for item in actual}
    logs = unique_matching(actual_paths, re.escape(workspace) + r"/logs-[0-9a-f]{32}", "successful logs directory")
    samples_dir = unique_matching(actual_paths, re.escape(workspace) + r"/samples-[0-9a-f]{32}", "successful samples directory")
    samples = capture.get("samples")
    require(isinstance(samples, list), "successful capture samples are not an array")
    if expected_outcome == "green":
        require(len(samples) == 1 and samples[0].get("index") == 1,
                "green --repeat 1 fixture did not produce exactly sample index 1")
        expected_warmup_status = 0
    else:
        require(samples == [], "warmup-red --repeat 1 fixture unexpectedly produced samples")
        expected_warmup_status = 1
    paths = {
        workspace,
        logs,
        samples_dir,
        f"{workspace}/identity.json",
        f"{workspace}/samples.json",
        f"{logs}/warmup.stdout.log",
        f"{logs}/warmup.stderr.log",
        f"{logs}/warmup.time.log",
        f"{logs}/warmup.status.json",
        out_relative,
    }
    expected_warmup_logs = {
        "stdout": os.path.join(root, f"{logs}/warmup.stdout.log"),
        "stderr": os.path.join(root, f"{logs}/warmup.stderr.log"),
        "time": os.path.join(root, f"{logs}/warmup.time.log"),
        "status": os.path.join(root, f"{logs}/warmup.status.json"),
    }
    warmup = metadata["warmup"]
    require(set(warmup) == {"kind", "command", "exit_status", "timing", "logs", "log_sha256"},
            f"unexpected warmup metadata fields: {sorted(warmup)}")
    require(warmup["kind"] == "execution" and warmup["command"] == expected_command,
            "warmup command differs from fixture contract")
    require(warmup["exit_status"] == expected_warmup_status,
            "warmup status differs from fixture contract")
    require(warmup["logs"] == expected_warmup_logs, "warmup log paths do not match protocol")
    require(warmup["log_sha256"] == {key: hashlib.sha256(open(path, "rb").read()).hexdigest() for key, path in expected_warmup_logs.items()}, "warmup log hashes do not match protocol")
    require_timing(warmup["timing"], "warmup")
    require(read_json(os.path.join(root, f"{workspace}/identity.json")) == expected_identity,
            "identity sidecar differs from fixture contract")
    require(read_json(os.path.join(root, f"{workspace}/samples.json")) == samples,
            "samples sidecar differs from published capture")
    if expected_outcome == "green":
        index = 1
        sample = samples[0]
        paths.update({
            f"{workspace}/{index}.failures.json",
            f"{workspace}/{index}.executed.json",
            f"{workspace}/{index}.runnable.json",
            f"{workspace}/{index}.ignored.json",
            f"{workspace}/{index}.proof.json",
            f"{logs}/sample-{index}.stdout.log",
            f"{logs}/sample-{index}.stderr.log",
            f"{logs}/sample-{index}.time.log",
            f"{logs}/sample-{index}.status.json",
            f"{samples_dir}/{index}.json",
        })
        expected_logs = {
            "stdout": os.path.join(root, f"{logs}/sample-{index}.stdout.log"),
            "stderr": os.path.join(root, f"{logs}/sample-{index}.stderr.log"),
            "time": os.path.join(root, f"{logs}/sample-{index}.time.log"),
            "status": os.path.join(root, f"{logs}/sample-{index}.status.json"),
        }
        require(sample["execution"]["logs"] == expected_logs, f"sample {index} log paths do not match protocol")
        require(set(sample) == {"index", "execution", "observed"}, "unexpected sample metadata fields")
        sample_execution = sample["execution"]
        require(set(sample_execution) == {"command", "exit_status", "evidence_valid", "timing", "logs", "log_sha256"},
                "unexpected sample execution metadata fields")
        require(sample_execution["command"] == expected_command and sample_execution["exit_status"] == 0 and sample_execution["evidence_valid"] is True,
                "sample execution differs from fixture contract")
        require_timing(sample_execution["timing"], "sample 1")
        require(sample_execution["log_sha256"] == {key: hashlib.sha256(open(path, "rb").read()).hexdigest() for key, path in expected_logs.items()}, "sample log hashes do not match protocol")
        expected_proof = {
            "mode": "exact",
            "declared": expected_identity,
            "executed": expected_identity,
            "verified": True,
        }
        expected_observed = {
            "passed_lines": 1,
            "failed_lines": 0,
            "ignored_lines": 0,
            "failure_names": [],
            "executed_test_names": expected_identity,
            "executed_runnable_test_names": expected_identity,
            "ignored_test_names": [],
            "identity_proof": expected_proof,
        }
        require(sample["observed"] == expected_observed, "sample observation differs from fixture contract")
        require(read_json(os.path.join(root, f"{workspace}/1.failures.json")) == [], "sample failure sidecar differs from fixture contract")
        require(read_json(os.path.join(root, f"{workspace}/1.executed.json")) == expected_identity, "sample execution sidecar differs from fixture contract")
        require(read_json(os.path.join(root, f"{workspace}/1.runnable.json")) == expected_identity, "sample runnable sidecar differs from fixture contract")
        require(read_json(os.path.join(root, f"{workspace}/1.ignored.json")) == [], "sample ignored sidecar differs from fixture contract")
        require(read_json(os.path.join(root, f"{workspace}/1.proof.json")) == expected_proof, "sample proof sidecar differs from fixture contract")
        require(read_json(os.path.join(root, f"{samples_dir}/1.json")) == sample, "sample protocol JSON differs from published sample")
    parent = path_parent(out_relative)
    while parent not in (None, "."):
        paths.add(parent)
        parent = path_parent(parent)
    baseline_paths = {item["path"] for item in baseline}
    new_paths = paths - baseline_paths
    require(actual_paths == baseline_paths | new_paths,
            f"unexpected successful paths: extra={sorted(actual_paths - baseline_paths - new_paths)} missing={sorted(new_paths - actual_paths)}")
    additions = actual_records(actual, new_paths)
    for item in additions:
        if item["type"] == "directory":
            require_owned_node(item, "directory", "0700")
        else:
            require_owned_node(item, "regular", "0600", 1)
    require_inventory(root, compose((baseline, "", ()), (additions, "", ())), "successful exact capture schema")

def explicit_records(actual, specs):
    parsed = parse_named_specs(specs)
    records = actual_records(actual, [path for path, _ in parsed])
    expected_types = {path: expected_type for path, expected_type in parsed}
    for item in records:
        require(item["type"] == expected_types[item["path"]], f"explicit node has wrong type: {item}")
        if item["type"] == "directory":
            require_owned_node(item, "directory", "0700")
        elif item["type"] == "regular":
            require_owned_node(item, "regular", "0600", 1)
    return records

if action == "snapshot":
    scope, output = args
    write_json(output, inventory(scope))
elif action == "assert":
    scope, expected_path = args
    require_inventory(scope, read_json(expected_path), "saved")
elif action == "record-state":
    parent, out_leaf, baseline_path, stage, output = args
    baseline = read_json(baseline_path)
    actual = inventory(parent)
    state = stage_schema(actual, baseline, out_leaf, stage)
    require_inventory(parent, compose((baseline, "", ()), (state["nodes"], "", ())), f"{stage} stage schema")
    write_json(output, state)
elif action == "assert-current":
    scope, baseline_path, state_path = args
    baseline = read_json(baseline_path)
    state = read_json(state_path)
    require_inventory(scope, compose((baseline, "", ()), (state["nodes"], "", ())), "current runner stage")
elif action == "assert-prefixed-state":
    scope, baseline_path, state_path, prefix = args
    baseline = read_json(baseline_path)
    state = read_json(state_path)
    require_inventory(scope, compose((baseline, "", ()), (state["nodes"], prefix, ())), "root + prefixed runner stage")
elif action == "record-additions":
    scope, baseline_path, state_path, output, *specs = args
    baseline = read_json(baseline_path)
    state = None if state_path == "-" else read_json(state_path)
    write_json(output, record_named_additions(scope, baseline, state, specs))
elif action == "assert-clean":
    scope, baseline_path, additions_path = args
    baseline = read_json(baseline_path)
    components = [(baseline, "", ())]
    if additions_path != "-":
        components.append((read_json(additions_path)["nodes"], "", ()))
    require_inventory(scope, compose(*components), "post-cleanup baseline + named injections")
elif action == "assert-mutation":
    scope, baseline_path, state_path, relative, field, expected_value = args
    baseline = [dict(item) for item in read_json(baseline_path)]
    state = read_json(state_path)
    nodes = [dict(item) for item in state["nodes"]]
    matches = [item for item in baseline + nodes if item["path"] == relative]
    require(len(matches) == 1, f"mutation target is not in exact fixture schema: {relative}")
    target = matches[0]
    actual = {item["path"]: item for item in inventory(scope)}.get(relative)
    require(actual is not None and str(actual.get(field)) == expected_value, f"unexpected mutation value at {relative}: {actual}")
    target[field] = actual[field]
    require_inventory(scope, compose((baseline, "", ()), (nodes, "", ())), "explicit runner-node mutation")
elif action == "assert-state-absent":
    state_path, *scopes = args
    state = read_json(state_path)
    identities = {tuple(identity) for identity in state["identities"]}
    for scope in scopes:
        for item in inventory(scope):
            identity = (item["dev"], item["ino"])
            if identity in identities:
                fail(f"runner identity remains in {scope}: {item}")
elif action == "assert-success":
    (root, out, baseline_path, expected_label, expected_outcome, expected_head,
     expected_branch, expected_logical_cpus, expected_target_dir) = args
    successful_schema(
        root,
        out,
        read_json(baseline_path),
        expected_label,
        expected_outcome,
        expected_head,
        expected_branch,
        int(expected_logical_cpus),
        expected_target_dir,
    )
elif action == "record-root-relocation":
    root, root_baseline_path, adversary, adversary_baseline_path, moved_relative, root_output, adversary_output = args
    root_baseline = read_json(root_baseline_path)
    adversary_baseline = read_json(adversary_baseline_path)
    root_actual = inventory(root)
    require(len(root_actual) == 1, f"replacement root contains unexpected nodes: {root_actual}")
    require_owned_node(root_actual[0], "directory", "0700")
    adversary_expected = compose((adversary_baseline, "", ()), (root_baseline, moved_relative, ()))
    require_inventory(adversary, adversary_expected, "explicit saved-root relocation")
    write_json(root_output, root_actual)
    write_json(adversary_output, adversary_expected)
elif action == "record-parent-transition":
    (root, root_baseline_path, parent_relative, parent_baseline_path, state_path,
     adversary, adversary_baseline_path, moved_relative, root_output, adversary_output, *specs) = args
    root_baseline = read_json(root_baseline_path)
    parent_baseline = read_json(parent_baseline_path)
    state = read_json(state_path)
    adversary_baseline = read_json(adversary_baseline_path)
    root_actual = inventory(root)
    replacements = explicit_records(root_actual, specs)
    root_expected = compose((root_baseline, "", (parent_relative,)), (replacements, "", ()))
    adversary_expected_live = compose(
        (adversary_baseline, "", ()),
        (parent_baseline, moved_relative, ()),
        (state["nodes"], moved_relative, ()),
    )
    require_inventory(root, root_expected, "explicit saved-parent replacement")
    require_inventory(adversary, adversary_expected_live, "explicit moved-parent live schema")
    adversary_expected_clean = compose((adversary_baseline, "", ()), (parent_baseline, moved_relative, ()))
    write_json(root_output, root_expected)
    write_json(adversary_output, adversary_expected_clean)
elif action == "record-hostile-transition":
    (root, root_baseline_path, state_path, adversary, adversary_baseline_path,
     moved_kind, moved_relative, root_output, adversary_output, *specs) = args
    root_baseline = read_json(root_baseline_path)
    state = read_json(state_path)
    adversary_baseline = read_json(adversary_baseline_path)
    require(moved_kind in ("workspace", "private"), f"invalid hostile moved kind: {moved_kind}")
    moved_source = state[moved_kind]
    moved_nodes = [item for item in state["nodes"] if item["path"] == moved_source or item["path"].startswith(moved_source + "/")]
    require(moved_nodes, f"hostile moved schema is empty: {moved_kind}")
    rebased_moved_nodes = []
    for item in moved_nodes:
        copied = dict(item)
        copied["path"] = "." if item["path"] == moved_source else item["path"][len(moved_source) + 1:]
        rebased_moved_nodes.append(copied)
    root_actual = inventory(root)
    replacements = explicit_records(root_actual, specs)
    root_live = compose((root_baseline, "", ()), (state["nodes"], "", (moved_source,)), (replacements, "", ()))
    adversary_live = compose((adversary_baseline, "", ()), (rebased_moved_nodes, moved_relative, ()))
    require_inventory(root, root_live, "explicit hostile replacement schema")
    require_inventory(adversary, adversary_live, "explicit hostile moved residue schema")
    root_clean = compose((root_baseline, "", ()), (replacements, "", ()))
    write_json(root_output, root_clean)
    write_json(adversary_output, adversary_live)
else:
    fail(f"unknown runner inventory action: {action}")
PY
    }

    runner_inventory_snapshot() {
        runner_inventory_helper snapshot "$1" "$2"
    }

    runner_inventory_assert() {
        runner_inventory_helper assert "$1" "$2"
    }

    runner_inventory_record_state() {
        runner_inventory_helper record-state "$1" "${2##*/}" "$3" "$4" "$5"
    }

    runner_inventory_assert_current() {
        runner_inventory_helper assert-current "$1" "$2" "$3"
    }

    runner_inventory_assert_prefixed_state() {
        runner_inventory_helper assert-prefixed-state "$1" "$2" "$3" "$4"
    }

    runner_inventory_record_additions() {
        local scope="$1" baseline_path="$2" state_path="$3" output="$4"
        shift 4
        runner_inventory_helper record-additions "$scope" "$baseline_path" "$state_path" "$output" "$@"
    }

    runner_inventory_assert_clean() {
        runner_inventory_helper assert-clean "$1" "$2" "${3:--}"
    }

    runner_inventory_assert_mutation() {
        runner_inventory_helper assert-mutation "$1" "$2" "$3" "$4" "$5" "$6"
    }

    runner_inventory_assert_state_absent() {
        local state="$1"
        shift
        runner_inventory_helper assert-state-absent "$state" "$@"
    }

    runner_inventory_assert_success() {
        runner_inventory_helper assert-success "$1" "$2" "$3" "$4" "$5" \
            "$fixture_head" "$fixture_branch" "$fixture_logical_cpus" "$fixture_target_dir"
    }

    prepare_success_cardinality_control() {
        local source_root="$1" source_out="$2" destination_root="$3" mode="$4"
        python3 - "$source_root" "$source_out" "$destination_root" "$mode" <<'PY'
import copy
import json
import os
import re
import shutil
import sys

source_root, source_out, destination_root, mode = sys.argv[1:]

def require(condition, message):
    if not condition:
        raise RuntimeError(message)

def read_json(path):
    with open(path, encoding="utf-8") as stream:
        return json.load(stream)

def write_json(path, value):
    with open(path, "w", encoding="utf-8") as stream:
        json.dump(value, stream, sort_keys=True, separators=(",", ":"))
        stream.write("\n")

def rewrite_root(value):
    if isinstance(value, str) and (value == source_root or value.startswith(source_root + "/")):
        return destination_root + value[len(source_root):]
    if isinstance(value, list):
        return [rewrite_root(item) for item in value]
    if isinstance(value, dict):
        return {key: rewrite_root(item) for key, item in value.items()}
    return value

require(mode in ("empty", "extra"), f"invalid cardinality control mode: {mode}")
require(source_out.startswith(source_root + "/"), "source control output escaped source root")
require(os.path.isdir(destination_root) and not os.path.islink(destination_root), "control destination is not a directory")
require(os.listdir(destination_root) == [], "control destination is not empty")
shutil.copytree(source_root, destination_root, dirs_exist_ok=True, copy_function=shutil.copy2)
for current, directories, files in os.walk(destination_root, followlinks=False):
    require(not any(os.path.islink(os.path.join(current, name)) for name in directories + files),
            "cardinality control clone contains a symlink")
    for name in files:
        path = os.path.join(current, name)
        if name.endswith(".json"):
            write_json(path, rewrite_root(read_json(path)))

out = destination_root + source_out[len(source_root):]
capture = read_json(out)
require(capture["capture"]["warmup"]["exit_status"] == 0, "cardinality control is not status-zero-shaped")
require(len(capture["samples"]) == 1 and capture["samples"][0]["index"] == 1,
        "cardinality control source is not the fixed green fixture")
workspace = capture["capture"]["workspace"]
logs = os.path.dirname(capture["capture"]["warmup"]["logs"]["stdout"])
sample_directories = [
    os.path.join(workspace, name)
    for name in os.listdir(workspace)
    if re.fullmatch(r"samples-[0-9a-f]{32}", name)
]
require(len(sample_directories) == 1, f"invalid cloned samples directory set: {sample_directories}")
samples_dir = sample_directories[0]

if mode == "empty":
    for path in (
        os.path.join(workspace, "1.failures.json"),
        os.path.join(workspace, "1.executed.json"),
        os.path.join(workspace, "1.runnable.json"),
        os.path.join(workspace, "1.ignored.json"),
        os.path.join(workspace, "1.proof.json"),
        os.path.join(logs, "sample-1.stdout.log"),
        os.path.join(logs, "sample-1.stderr.log"),
        os.path.join(logs, "sample-1.time.log"),
        os.path.join(samples_dir, "1.json"),
    ):
        os.unlink(path)
    capture["samples"] = []
else:
    sample = capture["samples"][0]
    sample_two = copy.deepcopy(sample)
    sample_two["index"] = 2
    sample_two["execution"]["logs"] = {
        key: value.replace("sample-1.", "sample-2.")
        for key, value in sample["execution"]["logs"].items()
    }
    for suffix in ("failures.json", "executed.json", "runnable.json", "ignored.json", "proof.json"):
        shutil.copy2(os.path.join(workspace, f"1.{suffix}"), os.path.join(workspace, f"2.{suffix}"))
    for suffix in ("stdout.log", "stderr.log", "time.log"):
        shutil.copy2(os.path.join(logs, f"sample-1.{suffix}"), os.path.join(logs, f"sample-2.{suffix}"))
    shutil.copy2(os.path.join(samples_dir, "1.json"), os.path.join(samples_dir, "2.json"))
    write_json(os.path.join(samples_dir, "2.json"), sample_two)
    capture["samples"] = [sample, sample_two]

write_json(os.path.join(workspace, "samples.json"), capture["samples"])
write_json(out, capture)
PY
    }

    runner_inventory_record_root_relocation() {
        runner_inventory_helper record-root-relocation "$@"
    }

    runner_inventory_record_parent_transition() {
        runner_inventory_helper record-parent-transition "$@"
    }

    runner_inventory_record_hostile_transition() {
        runner_inventory_helper record-hostile-transition "$@"
    }

    # These intentionally narrow helpers are used only by the three hostile
    # same-UID relocation fixtures whose moved identities are out of scope.
    assert_no_runner_workspace() {
        python3 - "$1" <<'PY'
import os
import sys

for current, directories, files in os.walk(sys.argv[1], topdown=True, followlinks=False):
    for name in directories + files:
        if name.startswith(".capture-workspace."):
            raise RuntimeError(f"runner workspace remains: {current}/{name}")
PY
    }

    assert_no_runner_private() {
        python3 - "$1" <<'PY'
import os
import sys

for current, directories, files in os.walk(sys.argv[1], topdown=True, followlinks=False):
    for name in directories + files:
        if name.startswith(".capture-json."):
            raise RuntimeError(f"runner private JSON remains: {current}/{name}")
PY
    }

    start_hooked_capture() {
        hook_root="$1"; hook_out="$2"; hook_stage="$3"; local label="$4"
        shift 4
        hook_dir="$(mktemp -d "$fixture_dir/hook.XXXXXX")"
        hook_stage_fifo="$hook_dir/stage.fifo"; hook_continue_fifo="$hook_dir/continue.fifo"
        hook_status_file="$hook_dir/status"; hook_sentinel="$hook_dir/benchmark-invocations"; hook_all_cargo_log="$hook_dir/all-cargo-invocations"
        mkfifo "$hook_stage_fifo" "$hook_continue_fifo"
        (
            set +e
            cd "$fixture_dir" || exit 97
            env PATH="$stub_dir:$PATH" S1Q_SELF_TEST_MODE=1 S1Q_TEST_HOOK_STAGE="$hook_stage" \
                S1Q_TEST_STAGE_FIFO="$hook_stage_fifo" S1Q_TEST_CONTINUE_FIFO="$hook_continue_fifo" \
                FAKE_CARGO_INVOCATIONS="$hook_sentinel" FAKE_CARGO_ALL_INVOCATIONS="$hook_all_cargo_log" \
                "$@" "$script_path" capture --label "$label" \
                --probe store-fixture --repeat 1 --threads 1 --external-output-root "$hook_root" --out "$hook_out" \
                >"$hook_dir/stdout" 2>"$hook_dir/stderr"
            printf '%s\n' "$?" >"$hook_status_file"
            exit 0
        ) &
        hook_pid=$!
        local reported
        IFS= read -r reported <"$hook_stage_fifo"
        if [ "$reported" != "$hook_stage" ]; then
            oracle_failure "hook reported '$reported' instead of '$hook_stage'"
            return 1
        fi
        return 0
    }

    finish_hooked_capture() {
        printf '%s\n' "$hook_stage" >"$hook_continue_fifo"
        if ! wait "$hook_pid"; then
            oracle_failure "hook fixture wrapper failed"
            return 1
        fi
        if [ ! -f "$hook_status_file" ] || [ -L "$hook_status_file" ]; then
            oracle_failure "hook fixture did not publish a regular status file"
            return 1
        fi
        hook_result="$(cat "$hook_status_file")"
        if [[ ! "$hook_result" =~ ^[0-9]+$ ]]; then
            oracle_failure "hook fixture published invalid status '$hook_result'"
            return 1
        fi
        return 0
    }

    make_calibration_fixture() {
        local root="$1"
        python3 - "$root" "$fixture_head" "$fixture_branch" "$fixture_target_dir" <<'PY'
import hashlib
import json
import os
import subprocess
import sys

root, head, branch, target_path = sys.argv[1:]
os.umask(0o077)

def private_makedirs(path):
    os.makedirs(path, mode=0o700, exist_ok=True)
    current = path
    while current.startswith(root):
        os.chmod(current, 0o700)
        if current == root:
            break
        current = os.path.dirname(current)

private_makedirs(root)
private_makedirs(os.path.join(root, "preflight"))

def write_text(relative, value=""):
    path = os.path.join(root, relative)
    private_makedirs(os.path.dirname(path))
    with open(path, "w", encoding="utf-8", newline="\n") as stream:
        stream.write(value)
    os.chmod(path, 0o600)
    return path

def write_json(relative, value):
    path = os.path.join(root, relative)
    private_makedirs(os.path.dirname(path))
    with open(path, "w", encoding="utf-8", newline="\n") as stream:
        json.dump(value, stream, sort_keys=True, separators=(",", ":"))
        stream.write("\n")
    os.chmod(path, 0o600)
    return path

def sha(path):
    return hashlib.sha256(open(path, "rb").read()).hexdigest()

def sha_text(value):
    return hashlib.sha256(value.encode()).hexdigest()

fast_suites = {
    "rust-suites": {
        "binary-a": {
            "binary-id": "binary-a",
            "testcases": {
                "same::name": {"ignored": False, "filter-match": {"status": "matches"}},
                "only::fast": {"ignored": False, "filter-match": {"status": "matches"}},
                "ignored::fast": {"ignored": True, "filter-match": {"status": "matches"}},
            },
        }
    }
}
full_suites = json.loads(json.dumps(fast_suites))
full_suites["rust-suites"]["binary-b"] = {
    "binary-id": "binary-b",
    "testcases": {
        "same::name": {"ignored": False, "filter-match": {"status": "matches"}},
        "only::full": {"ignored": False, "filter-match": {"status": "matches"}},
        "ignored::full": {"ignored": True, "filter-match": {"status": "matches"}},
    },
}
fast_list = write_json("nextest-fast-list.json", fast_suites)
full_list = write_json("nextest-full-list.json", full_suites)
fast_runnable = ["binary-a::only::fast", "binary-a::same::name"]
fast_ignored = ["binary-a::ignored::fast"]
full_runnable = ["binary-a::only::fast", "binary-a::same::name", "binary-b::only::full", "binary-b::same::name"]
full_ignored = ["binary-a::ignored::fast", "binary-b::ignored::full"]

uname = os.uname()
uname_stdout = subprocess.run(["uname", "-a"], check=True, text=True, stdout=subprocess.PIPE).stdout
host = {
    "class": f"{uname.sysname}-{uname.machine}-32cpu",
    "os": uname.sysname,
    "kernel": uname.release,
    "architecture": uname.machine,
    "cpu_model": "fixture cpu",
    "logical_cpus": 32,
}
host["fingerprint_sha256"] = sha_text("|".join(str(host[key]) for key in ("class", "os", "kernel", "architecture", "cpu_model", "logical_cpus")))
target_stat = os.stat(target_path)
target = {
    "canonical_path": target_path,
    "device": str(target_stat.st_dev),
    "inode": str(target_stat.st_ino),
}
target["fingerprint_sha256"] = sha_text("|".join(str(target[key]) for key in ("canonical_path", "device", "inode")))
toolchain = {"cargo": "cargo fixture", "rustc": "rustc fixture", "cargo_nextest": "cargo-nextest fixture", "cargo_target_dir": target_path}
source = {"head": head, "branch": branch, "clean_tree": True}
repo_root = os.path.dirname(os.path.dirname(os.path.dirname(os.path.dirname(root))))
cargo_metadata = {"target_directory": target_path, "workspace_root": repo_root}
nextest_config_profiles = {"rsid-fast": {"retries": 0, "filters": [], "quarantine": []}, "ci-full": {"retries": 0, "filters": [], "quarantine": []}}
nextest_config_text = '''nextest-version = { required = "0.9.137" }
[profile.default]
retries = 0
fail-fast = false
test-threads = 8
[profile.rsid-fast]
inherits = "default"
retries = 0
[profile.ci-full]
inherits = "default"
retries = 0
'''
nextest_config_path = write_text("preflight/nextest-config.toml", nextest_config_text)

commands = {
    "git-head": "git rev-parse HEAD",
    "git-branch": "git branch --show-current",
    "git-status": "git status --porcelain=v1",
    "git-diff-check": "git diff --check",
    "snapshots": "find . -type f -name '*.snap.new' -print",
    "nproc": "nproc",
    "uname": "uname -a",
    "lscpu": "lscpu",
    "cargo-version": "cargo --version",
    "rustc-version": "rustc --version",
    "nextest-version": "cargo nextest --version",
    "cargo-metadata": "cargo metadata --format-version 1 --no-deps",
    "nextest-config": "cargo nextest show-config test-groups",
    "nextest-fast-list": "cargo nextest list --profile rsid-fast -p rsid --lib --message-format json",
    "nextest-full-list": "cargo nextest list --profile ci-full --workspace --message-format json",
    "make-test-fast": "make test-fast NEXTEST_JOBS=8",
    "make-test-full": "make test-full NEXTEST_JOBS=8",
}
command_records = []
for sequence, (name, command) in enumerate(commands.items(), 1):
    if name == "nextest-fast-list":
        stdout = "nextest-fast-list.json"
    elif name == "nextest-full-list":
        stdout = "nextest-full-list.json"
    else:
        stdout = f"preflight/{name}.stdout"
        content = ""
        if name == "git-head": content = head + "\n"
        elif name == "git-branch": content = branch + "\n"
        elif name == "nproc": content = "32\n"
        elif name == "uname": content = uname_stdout
        elif name == "lscpu": content = "Model name: fixture cpu\n"
        elif name == "cargo-version": content = toolchain["cargo"] + "\n"
        elif name == "rustc-version": content = toolchain["rustc"] + "\n"
        elif name == "nextest-version": content = toolchain["cargo_nextest"] + "\n"
        elif name == "cargo-metadata": content = json.dumps(cargo_metadata, sort_keys=True, separators=(",", ":")) + "\n"
        elif name == "nextest-config": content = "test-groups:\n  configured groups: 0\n"
        elif name == "make-test-fast": content = "make: entering representative fast lane\n"
        elif name == "make-test-full": content = "running 1 test\ntest docs::fixture ... ok\ntest result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.01s\n"
        write_text(stdout, content)
    stderr = f"preflight/{name}.stderr"
    stderr_content = ""
    if name == "make-test-fast": stderr_content = "warning: representative ordinary Cargo output\n    Finished test profile [unoptimized + debuginfo] target(s) in 0.12s\n────────────\nSummary [   0.100s] 3 tests run: 2 passed, 1 skipped\n"
    elif name == "make-test-full": stderr_content = "    Finished test profile [unoptimized + debuginfo] target(s) in 0.14s\nSummary [   0.120s] 6 tests run: 4 passed, 2 skipped\n   Doc-tests rsi_common\n     Running `target/debug/rsi-model-control-validate --offline`\n     Running `target/debug/rsi-provider-capability-validate --offline`\n"
    write_text(stderr, stderr_content)
    status = f"preflight/{name}.status.json"
    components = []
    if name == "make-test-fast":
        components = [{"name": "nextest", "exit_status": 0, "passed": 2, "failed": 0, "skipped": 1}]
    elif name == "make-test-full":
        components = [
            {"name": "nextest", "exit_status": 0, "passed": 4, "failed": 0, "skipped": 2},
            {"name": "doctests", "exit_status": 0, "passed": 1, "failed": 0, "skipped": 0},
            {"name": "model-control-validator", "exit_status": 0, "passed": 1, "failed": 0, "skipped": 0},
            {"name": "provider-capability-validator", "exit_status": 0, "passed": 1, "failed": 0, "skipped": 0},
        ]
    status_path = write_json(status, {"schema_version": 1, "sequence": sequence, "name": name, "started_at": f"2026-09-01T01:00:{sequence:02d}Z", "finished_at": f"2026-09-01T01:00:{sequence + 1:02d}Z", "exit_status": 0})
    normalized = f"preflight/{name}.components.json"
    normalized_path = write_json(normalized, {"schema_version": 1, "name": name, "components": components, "stdout_sha256": sha(os.path.join(root, stdout)), "stderr_sha256": sha(os.path.join(root, stderr))})
    command_records.append({
        "name": name,
        "command": command,
        "exit_status": 0,
        "stdout": stdout,
        "stderr": stderr,
        "status": status,
        "normalized": normalized,
        "stdout_sha256": sha(os.path.join(root, stdout)),
        "stderr_sha256": sha(os.path.join(root, stderr)),
        "status_sha256": sha(status_path),
        "normalized_sha256": sha(normalized_path),
    })

preflight = {
    "schema_version": 3,
    "origin": {"kind": "unverified-durable-input", "acceptance_operation": None, "accepted": False},
    "threat_boundary": "Acceptance proves exact bytes and uninterrupted in-process descriptor custody against hostile inherited environment and concurrent same-UID pathname or configuration mutation after a conforming sterile launch. It does not authenticate the launching principal, resist same-UID ptrace, process-memory, signal/control, namespace-control, or inherited-open-fd compromise, and does not authenticate alteration or forgery after the producer's final reproof and exit. It is not a signature or cryptographic origin attestation.",
    "source": source,
    "host": host,
    "toolchain": toolchain,
    "target": target,
    "nextest_config": {"source": ".config/nextest.toml", "path": "preflight/nextest-config.toml", "sha256": sha(nextest_config_path), "profiles": nextest_config_profiles},
    "retry_policy": {"rsid-fast": 0, "ci-full": 0},
    "filters": {"rsid-fast": [], "ci-full": []},
    "quarantine": [],
    "snapshot_candidates": [],
    "commands": command_records,
}
write_json("preflight.json", preflight)

short_ids = {
    "store-fixture": "store::tests::load_sessions_survives_legacy_comma_fraction_timestamp",
    "source-scanner": "session::issue21_phase2_tests::p2_07_gate_permit_spine::exactly_one_production_writer_of_gate_closing_and_effect_permits",
}
commands_by_probe = {
    "store-fixture": f"cargo test -p rsid --lib {short_ids['store-fixture']} -- --exact --test-threads 1",
    "source-scanner": f"cargo test -p rsid --lib {short_ids['source-scanner']} -- --exact --test-threads 1",
}

def timing(wall, user=None, system=0.1):
    return {"backend": "bash_portable", "wall_seconds": wall, "user_seconds": wall if user is None else user, "system_seconds": system, "max_rss_kib": None, "max_rss_disposition": "unavailable_portable_backend"}

def logs(logical, label, content, wall, user=None, system=0.1, sequence=0):
    paths, hashes = {}, {}
    for field in ("stdout", "stderr", "time"):
        relative = f"raw/{logical}/logs/{label}.{field}.log"
        value = content if field == "stdout" else f"real {wall}\nuser {wall if user is None else user}\nsys {system}\n" if field == "time" else ""
        path = write_text(relative, value)
        paths[field] = relative
        hashes[field] = sha(path)
    status_relative = f"raw/{logical}/logs/{label}.status.json"
    minute = sequence // 60
    second = sequence % 60
    started = f"2026-09-01T00:{minute:02d}:{second:02d}Z"
    finished = f"2026-09-01T00:{minute:02d}:{second + 1:02d}Z"
    status_path = write_json(status_relative, {"schema_version": 1, "label": label, "started_at": started, "finished_at": finished, "exit_status": 0})
    paths["status"] = status_relative
    hashes["status"] = sha(status_path)
    return paths, hashes, finished

def make_capture(relative, probe, threads, walls, runnable=None, ignored=None):
    profile = "rsid-fast" if probe == "nextest-fast" else "ci-full" if probe == "nextest-full" else ""
    if probe == "nextest-fast":
        command = f"cargo nextest run --profile rsid-fast -p rsid --lib --status-level all --final-status-level all -j {threads}"
        warmup_command = "cargo nextest run --profile rsid-fast -p rsid --lib --no-run"
        identity = ["rsid library nextest fast profile"]
    elif probe == "nextest-full":
        command = f"cargo nextest run --profile ci-full --workspace --status-level all --final-status-level all -j {threads}"
        warmup_command = "cargo nextest run --profile ci-full --workspace --no-run"
        identity = ["workspace nextest full profile"]
    else:
        command = commands_by_probe[probe]
        warmup_command = command
        identity = [short_ids[probe]]
        runnable = identity
        ignored = []
    logical = relative.removesuffix(".json")
    if profile:
        warmup_stdout = "artifacts ready\n"
    else:
        warmup_stdout = f"test {identity[0]} ... ok\ntest result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s\n"
    warmup_logs, warmup_hashes, captured_at = logs(logical, "warmup", warmup_stdout, 0.5)
    samples = []
    for index, wall in enumerate(walls, 1):
        if profile:
            status_lines = [f"  PASS [   0.001s] {value.split('::', 1)[0]} {value.split('::', 1)[1]}" for value in runnable]
            status_lines += [f"  SKIP [   0.000s] {value.split('::', 1)[0]} {value.split('::', 1)[1]}" for value in ignored]
            sample_stdout = "\n".join(status_lines) + f"\nSummary [   {wall}.000s] {len(runnable) + len(ignored)} tests run: {len(runnable)} passed, {len(ignored)} skipped\n"
        else:
            sample_stdout = f"test {identity[0]} ... ok\ntest result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s\n"
        sample_logs, sample_hashes, captured_at = logs(logical, f"sample-{index}", sample_stdout, wall, sequence=index * 2)
        executed = sorted((runnable or []) + (ignored or []))
        proof = {"mode": "descriptive" if profile else "exact", "declared": identity, "executed": executed, "verified": True}
        samples.append({
            "index": index,
            "execution": {"command": command, "exit_status": 0, "evidence_valid": True, "timing": timing(wall), "logs": sample_logs, "log_sha256": sample_hashes},
            "observed": {"passed_lines": len(runnable or []), "failed_lines": 0, "ignored_lines": len(ignored or []), "failure_names": [], "executed_test_names": executed, "executed_runnable_test_names": runnable or [], "ignored_test_names": ignored or [], "identity_proof": proof},
        })
    capture = {
        "schema_version": 2,
        "capture": {
            "captured_at": captured_at,
            "label": "z-baseline-store" if logical == "store" else "z-baseline-scanner" if logical == "scanner" else "z-baseline-fast" if logical == "fast" else f"z-baseline-{logical}",
            "probe": probe,
            "profile": profile,
            "source": source,
            "host": host,
            "toolchain": toolchain,
            "target": target,
            "requested": {"repeat": len(walls), "threads": threads, "resolved_threads": threads, "stop_on_first_red": True},
            "workspace": f"raw/{logical}",
            "warmup": {"kind": "artifact-build" if profile else "execution", "command": warmup_command, "exit_status": 0, "timing": timing(0.5), "logs": warmup_logs, "log_sha256": warmup_hashes},
            "execution": {"command": command, "profile": profile, "test_identity": identity, "identity_mode": "descriptive" if profile else "exact"},
        },
        "samples": samples,
    }
    write_json(relative, capture)

make_capture("store.json", "store-fixture", 1, [1, 2, 3, 4, 5])
make_capture("scanner.json", "source-scanner", 1, [5, 4, 3, 2, 1])
make_capture("sweep-t1.json", "nextest-full", 1, [12, 12, 12], full_runnable, full_ignored)
make_capture("sweep-t8.json", "nextest-full", 8, [10, 11, 12], full_runnable, full_ignored)
make_capture("sweep-t16.json", "nextest-full", 16, [9, 10, 11], full_runnable, full_ignored)
make_capture("sweep-t32.json", "nextest-full", 32, [10, 10, 10], full_runnable, full_ignored)
make_capture("fast.json", "nextest-fast", 8, [2, 3, 4], fast_runnable, fast_ignored)
PY
    }
    git -C "$fixture_dir" init -q; git -C "$fixture_dir" config user.email benchmark@example.invalid; git -C "$fixture_dir" config user.name benchmark; printf '*\n!.gitignore\n!.gitkeep\n' >"$fixture_dir/.gitignore"; touch "$fixture_dir/.gitkeep"; printf original >"$fixture_dir/tracked.txt"; mkdir -p "$fixture_dir/.config"; printf '%s\n' 'nextest-version = { required = "0.9.137" }' '[profile.default]' 'retries = 0' 'fail-fast = false' 'test-threads = 8' '[profile.rsid-fast]' 'inherits = "default"' 'retries = 0' '[profile.ci-full]' 'inherits = "default"' 'retries = 0' >"$fixture_dir/.config/nextest.toml"; git -C "$fixture_dir" add .gitignore .gitkeep; git -C "$fixture_dir" add -f tracked.txt .config/nextest.toml; git -C "$fixture_dir" commit -qm fixture
    fixture_head="$(git -C "$fixture_dir" rev-parse HEAD)"; fixture_branch="$(git -C "$fixture_dir" branch --show-current)"
    fixture_logical_cpus="$(logical_cpus)"
    baseline="$fixture_dir/base.json"; candidate="$fixture_dir/candidate.json"; budget="$fixture_dir/budget.json"
    jq -n '{schema_version: 2, capture: {probe: "fixture", source: {clean_tree: true}, host: {class: "fixture-host"}, requested: {repeat: 3, resolved_threads: 1}, warmup: {exit_status: 0}, execution: {command: "fixture-command", test_identity: ["fixture-test"]}}, samples: [range(1;4) | {index: ., execution: {command: "fixture-command", exit_status: 0, evidence_valid: true, timing: {wall_seconds: ., user_seconds: 0, system_seconds: 0, max_rss_kib: null}}, observed: {passed_lines: 1, failed_lines: 0, ignored_lines: 0, failure_names: [], identity_proof: {verified: true}}}]}' >"$baseline"
    jq '(.samples[0].execution.timing.wall_seconds = 0.5) | (.samples[1].execution.timing.wall_seconds = 1) | (.samples[2].execution.timing.wall_seconds = 1.5)' "$baseline" >"$candidate"
    jq -n --slurpfile m "$candidate" '{budgets: [{probe: "fixture", host_class: "fixture-host", expected_repeat: 3, median_wall_seconds: 1, mad_wall_seconds: 0, command: $m[0].capture.execution.command, test_identity: $m[0].capture.execution.test_identity, resolved_threads: 1, observed_samples: [$m[0].samples | sort_by(.index)[] | {passed_lines: .observed.passed_lines, failed_lines: .observed.failed_lines, ignored_lines: .observed.ignored_lines, failure_names: .observed.failure_names, identity_proof: .observed.identity_proof}]}]}' >"$budget"
    result="$(compare --baseline "$baseline" --candidate "$candidate")"; jq -e '.comparison.baseline.median_wall_seconds == 2 and .comparison.candidate_is_faster == true' <<<"$result" >/dev/null; result="$(check --baseline "$budget" --measurement "$candidate")"; jq -e '.check.passed == true' <<<"$result" >/dev/null
    jq '.capture.host.class = "other-host"' "$candidate" >"$fixture_dir/bad.json"; ! compare --baseline "$baseline" --candidate "$fixture_dir/bad.json" >/dev/null 2>&1; ! check --baseline "$budget" --measurement "$fixture_dir/bad.json" >/dev/null 2>&1
    jq '(.samples[0].execution.exit_status = 1) | (.samples[0].observed.failed_lines = 1) | (.samples[0].observed.failure_names = ["actual::failure"])' "$candidate" >"$fixture_dir/bad.json"; ! compare --baseline "$baseline" --candidate "$fixture_dir/bad.json" >/dev/null 2>&1; ! check --baseline "$budget" --measurement "$fixture_dir/bad.json" >/dev/null 2>&1
    jq '.capture.execution.command = "drift"' "$candidate" >"$fixture_dir/bad.json"; ! check --baseline "$budget" --measurement "$fixture_dir/bad.json" >/dev/null 2>&1; jq 'del(.samples[2])' "$candidate" >"$fixture_dir/bad.json"; ! compare --baseline "$baseline" --candidate "$fixture_dir/bad.json" >/dev/null 2>&1
    jq '(.samples[2].observed.passed_lines = 2)' "$candidate" >"$fixture_dir/bad.json"; ! check --baseline "$budget" --measurement "$fixture_dir/bad.json" >/dev/null 2>&1; jq '(.samples[2].observed.ignored_lines = 1)' "$candidate" >"$fixture_dir/bad.json"; ! check --baseline "$budget" --measurement "$fixture_dir/bad.json" >/dev/null 2>&1; jq '(.capture.requested.repeat = 2) | (.samples = .samples[0:2])' "$candidate" >"$fixture_dir/bad.json"; ! check --baseline "$budget" --measurement "$fixture_dir/bad.json" >/dev/null 2>&1; jq '(.samples[0].execution.timing.wall_seconds = -1)' "$candidate" >"$fixture_dir/bad.json"; ! check --baseline "$budget" --measurement "$fixture_dir/bad.json" >/dev/null 2>&1
    printf 'test unit::passing ... ok\ntest unit::failing ... FAILED\ntest unit::skipped ... ignored, fixture skip\ntest result: FAILED. 1 passed; 1 failed; 1 ignored; 0 measured; 0 filtered out; finished in 0.00s\n' >"$fixture_dir/cargo-unit.log"; [ "$(observed_count passed "$fixture_dir/cargo-unit.log" /dev/null)" = 1 ] && [ "$(observed_count failed "$fixture_dir/cargo-unit.log" /dev/null)" = 1 ] && [ "$(observed_count ignored "$fixture_dir/cargo-unit.log" /dev/null)" = 1 ] && [ "$(libtest_summary_count passed "$fixture_dir/cargo-unit.log" /dev/null)" = 1 ] && [ "$(libtest_summary_count failed "$fixture_dir/cargo-unit.log" /dev/null)" = 1 ] && [ "$(libtest_summary_count ignored "$fixture_dir/cargo-unit.log" /dev/null)" = 1 ]; jq -e '. == ["unit::failing"]' <<<"$(failure_names "$fixture_dir/cargo-unit.log" /dev/null)" >/dev/null; jq -e '. == ["unit::failing", "unit::passing", "unit::skipped"]' <<<"$(executed_test_names "$fixture_dir/cargo-unit.log" /dev/null)" >/dev/null
    printf 'test src/lib.rs - fixture pass (line 1) ... ok\ntest src/lib.rs - fixture failure (line 2) ... FAILED\ntest src/lib.rs - fixture skip (line 3) ... ignored, deliberate\ntest result: FAILED. 1 passed; 1 failed; 1 ignored; 0 measured; 0 filtered out; finished in 0.00s\n' >"$fixture_dir/cargo-doctest.log"; [ "$(observed_count passed "$fixture_dir/cargo-doctest.log" /dev/null)" = 1 ] && [ "$(observed_count failed "$fixture_dir/cargo-doctest.log" /dev/null)" = 1 ] && [ "$(observed_count ignored "$fixture_dir/cargo-doctest.log" /dev/null)" = 1 ] && [ "$(libtest_summary_count passed "$fixture_dir/cargo-doctest.log" /dev/null)" = 1 ] && [ "$(libtest_summary_count failed "$fixture_dir/cargo-doctest.log" /dev/null)" = 1 ] && [ "$(libtest_summary_count ignored "$fixture_dir/cargo-doctest.log" /dev/null)" = 1 ]; jq -e '. == ["src/lib.rs - fixture failure (line 2)"]' <<<"$(failure_names "$fixture_dir/cargo-doctest.log" /dev/null)" >/dev/null; jq -e '. == ["src/lib.rs - fixture failure (line 2)", "src/lib.rs - fixture pass (line 1)", "src/lib.rs - fixture skip (line 3)"]' <<<"$(executed_test_names "$fixture_dir/cargo-doctest.log" /dev/null)" >/dev/null
    printf 'test unit::duplicate ... ok\ntest unit::duplicate ... ok\ntest unit::split ... child subprocess output\nok\ntest unit::failure ... FAILED\ntest unit::ignored ... ignored, fixture skip\ntest result: FAILED. 3 passed; 1 failed; 1 ignored; 0 measured; 0 filtered out; finished in 0.00s\n' >"$fixture_dir/cargo-interleaved.log"; [ "$(libtest_summary_count passed "$fixture_dir/cargo-interleaved.log" /dev/null)" = 3 ] && [ "$(libtest_summary_count failed "$fixture_dir/cargo-interleaved.log" /dev/null)" = 1 ] && [ "$(libtest_summary_count ignored "$fixture_dir/cargo-interleaved.log" /dev/null)" = 1 ] && [ "$(aggregate_count passed "$fixture_dir/cargo-interleaved.log" /dev/null)" = 3 ] && [ "$(aggregate_count failed "$fixture_dir/cargo-interleaved.log" /dev/null)" = 1 ] && [ "$(aggregate_count ignored "$fixture_dir/cargo-interleaved.log" /dev/null)" = 1 ]; jq -e '. == ["unit::duplicate", "unit::failure", "unit::ignored"]' <<<"$(executed_test_names "$fixture_dir/cargo-interleaved.log" /dev/null)" >/dev/null
    printf '  SKIP [         ] (─────────) nextest-fixture tests::skipped\n  PASS [   0.002s] (1/2) nextest-fixture tests::passing\n  FAIL [   0.002s] (2/2) nextest-fixture tests::failing\n  PASS [   0.001s] nextest-fixture tests::passing\n  PASS [   0.001s] nextest-other tests::passing\n  FAIL [   0.002s] nextest-fixture tests::failing\n  SKIP [   0.000s] nextest-fixture tests::skipped\n' >"$fixture_dir/nextest.log"; PROBE_RESULT_FORMAT=nextest; [ "$(aggregate_count passed "$fixture_dir/nextest.log" /dev/null)" = 2 ] && [ "$(aggregate_count failed "$fixture_dir/nextest.log" /dev/null)" = 1 ] && [ "$(aggregate_count ignored "$fixture_dir/nextest.log" /dev/null)" = 1 ]; jq -e '. == ["nextest-fixture::tests::failing"]' <<<"$(failure_names "$fixture_dir/nextest.log" /dev/null)" >/dev/null; jq -e '. == ["nextest-fixture::tests::failing", "nextest-fixture::tests::passing", "nextest-fixture::tests::skipped", "nextest-other::tests::passing"]' <<<"$(executed_test_names "$fixture_dir/nextest.log" /dev/null)" >/dev/null; jq -e '. == ["nextest-fixture::tests::failing", "nextest-fixture::tests::passing", "nextest-other::tests::passing"]' <<<"$(executed_runnable_test_names "$fixture_dir/nextest.log" /dev/null)" >/dev/null; jq -e '. == ["nextest-fixture::tests::skipped"]' <<<"$(ignored_test_names "$fixture_dir/nextest.log" /dev/null)" >/dev/null; [ "$(LC_ALL=C aggregate_count passed "$fixture_dir/nextest.log" /dev/null)" = 2 ] && [ "$(LC_ALL=C aggregate_count failed "$fixture_dir/nextest.log" /dev/null)" = 1 ] && [ "$(LC_ALL=C aggregate_count ignored "$fixture_dir/nextest.log" /dev/null)" = 1 ]; LC_ALL=C jq -e '. == ["nextest-fixture::tests::failing", "nextest-fixture::tests::passing", "nextest-fixture::tests::skipped", "nextest-other::tests::passing"]' <<<"$(LC_ALL=C executed_test_names "$fixture_dir/nextest.log" /dev/null)" >/dev/null
    probe_command nextest-fast 8; [ "$(join_command "${PROBE_COMMAND[@]}")" = "cargo nextest run --profile rsid-fast -p rsid --lib --status-level all --final-status-level all -j 8" ]; [ "$(join_command "${PROBE_WARMUP_COMMAND[@]}")" = "cargo nextest run --profile rsid-fast -p rsid --lib --no-run" ]; [ "$PROBE_PROFILE" = rsid-fast ] && [ "$PROBE_WARMUP_KIND" = artifact-build ]
    probe_command nextest-full 8; [ "$(join_command "${PROBE_COMMAND[@]}")" = "cargo nextest run --profile ci-full --workspace --status-level all --final-status-level all -j 8" ]; [ "$(join_command "${PROBE_WARMUP_COMMAND[@]}")" = "cargo nextest run --profile ci-full --workspace --no-run" ]; [ "$PROBE_PROFILE" = ci-full ] && [ "$PROBE_WARMUP_KIND" = artifact-build ]
    make_fake_cargo "$stub_dir/cargo"; make_fake_rustc "$stub_dir/rustc"
    printf '%s\n' '#!/usr/bin/env bash' 'printf "32\n"' >"$stub_dir/nproc"
    printf '%s\n' '#!/usr/bin/env bash' 'printf "Model name: fixture cpu\n"' >"$stub_dir/lscpu"
    chmod +x "$stub_dir/nproc" "$stub_dir/lscpu"
    mkdir -p "$fixture_dir/target/test-suite-benchmark"; ln -s "$external_dir" "$fixture_dir/target/test-suite-benchmark/escape"
    local stop_out="$fixture_dir/target/test-suite-benchmark/stop-first-red.json" stop_calls="$fixture_dir/stop-first-red.calls" stop_count="$fixture_dir/stop-first-red.count" stop_status
    set +e
    (cd "$fixture_dir" && env PATH="$stub_dir:$PATH" FAKE_CARGO_INVOCATIONS="$stop_calls" FAKE_CARGO_CALL_COUNT_FILE="$stop_count" FAKE_CARGO_FAIL_CALL=2 "$script_path" capture --label stop-first-red --probe store-fixture --repeat 3 --threads 1 --stop-on-first-red --out target/test-suite-benchmark/stop-first-red.json) >/dev/null 2>&1
    stop_status=$?
    set -e
    [ "$stop_status" -ne 0 ] && [ "$(wc -l <"$stop_calls")" = 2 ]
    jq -e '.capture.requested.repeat == 3 and .capture.requested.stop_on_first_red == true and (.samples | length) == 1 and .samples[0].index == 1 and .samples[0].execution.exit_status != 0 and .samples[0].execution.evidence_valid == false and .samples[0].observed.failure_names == ["fixture::failure"]' "$stop_out" >/dev/null
    ! compare --baseline "$stop_out" --candidate "$stop_out" >/dev/null 2>&1
    ! check --baseline "$budget" --measurement "$stop_out" >/dev/null 2>&1
    local calibration_rel="target/test-suite-benchmark/z-baseline-calibration/$fixture_head" calibration_abs="$fixture_dir/target/test-suite-benchmark/z-baseline-calibration/$fixture_head"
    local generated_one="$calibration_abs/generated-one.json" generated_two="$calibration_abs/generated-two.json" generator_cargo_log="$fixture_dir/generator-cargo.log" calibration_cases=0
    local calibration_template="$fixture_dir/calibration-template" attest_invocations="$fixture_dir/attest-invocations"
    make_calibration_fixture "$calibration_abs"; : >"$generator_cargo_log"; : >"$attest_invocations"
    mv "$calibration_abs" "$calibration_template"
    private_attest_fixture_command() {
        local name="$1" stdout stderr
        printf '%s\n' "$name" >>"$attest_invocations"
        case "$name" in
            nextest-fast-list) stdout="$calibration_template/nextest-fast-list.json" ;;
            nextest-full-list) stdout="$calibration_template/nextest-full-list.json" ;;
            *) stdout="$calibration_template/preflight/$name.stdout" ;;
        esac
        stderr="$calibration_template/preflight/$name.stderr"
        /usr/bin/cat "$stdout"
        /usr/bin/cat "$stderr" >&2
    }
    (cd "$fixture_dir" && attest_preflight_impl private-self-test --calibration-root "$calibration_rel" --threads 8)
    local capture_fixture
    for capture_fixture in store.json scanner.json fast.json sweep-t1.json sweep-t8.json sweep-t16.json sweep-t32.json; do
        cp -a "$calibration_template/$capture_fixture" "$calibration_abs/$capture_fixture"
    done
    cp -a "$calibration_template/raw" "$calibration_abs/raw"
    [ "$(wc -l <"$attest_invocations")" -eq 17 ]
    diff -u <(printf '%s\n' git-head git-branch git-status git-diff-check snapshots nproc uname lscpu cargo-version rustc-version nextest-version cargo-metadata nextest-config nextest-fast-list nextest-full-list make-test-fast make-test-full) "$attest_invocations"
    jq -e '.schema_version == 3 and (.commands | length) == 17 and .commands[-2].name == "make-test-fast" and .commands[-1].name == "make-test-full"' "$calibration_abs/preflight.json" >/dev/null
    grep -F 'Summary [' "$calibration_abs/preflight/make-test-fast.stderr" >/dev/null
    grep -F 'Finished test profile' "$calibration_abs/preflight/make-test-fast.stderr" >/dev/null
    grep -F 'test-groups:' "$calibration_abs/preflight/nextest-config.stdout" >/dev/null
    ! grep -Eq 'S1Q-(COMPONENT|LANE)-END' "$calibration_abs/preflight/make-test-fast.stdout"
    calibration_cases=$((calibration_cases + 6))
    FAKE_CARGO_METADATA_JSON="$(cat "$calibration_abs/preflight/cargo-metadata.stdout")"
    FAKE_NEXTEST_CONFIG_JSON="$(cat "$calibration_abs/preflight/nextest-config.stdout")"
    S1Q_TEST_CURRENT_FACTS_JSON="$(jq -cn --slurpfile preflight "$calibration_abs/preflight.json" --rawfile nproc "$calibration_abs/preflight/nproc.stdout" --rawfile uname "$calibration_abs/preflight/uname.stdout" --rawfile lscpu "$calibration_abs/preflight/lscpu.stdout" --rawfile cargo "$calibration_abs/preflight/cargo-version.stdout" --rawfile rustc "$calibration_abs/preflight/rustc-version.stdout" --rawfile nextest "$calibration_abs/preflight/nextest-version.stdout" --rawfile metadata "$calibration_abs/preflight/cargo-metadata.stdout" --rawfile config "$calibration_abs/preflight/nextest-config.stdout" '{host:$preflight[0].host,toolchain:$preflight[0].toolchain,stdout:{nproc:$nproc,uname:$uname,lscpu:$lscpu,"cargo-version":$cargo,"rustc-version":$rustc,"nextest-version":$nextest,"cargo-metadata":$metadata,"nextest-config":$config}}')"
    export S1Q_SELF_TEST_MODE=1 S1Q_TEST_CURRENT_FACTS_JSON FAKE_CARGO_METADATA_JSON FAKE_NEXTEST_CONFIG_JSON
    (cd "$fixture_dir" && generate_baseline_impl private-self-test --calibration-root "$calibration_rel" --out "$calibration_rel/generated-one.json")
    (cd "$fixture_dir" && generate_baseline_impl private-self-test --calibration-root "$calibration_rel" --out "$calibration_rel/generated-two.json")
    cmp -s "$generated_one" "$generated_two"
    [ ! -s "$generator_cargo_log" ]
    calibration_cases=$((calibration_cases + 2))
    jq -e '
      .schema_version == 2 and .formula_version == "median-mad-v1" and
      .thread_selection.candidates == [1,8,16,32] and
      .thread_selection.fastest_candidate == 16 and
      .thread_selection.fastest_mad_wall_seconds == 1 and
      .thread_selection.eligible_frontier == [8,16,32] and
      .thread_selection.selected_threads == 8 and
      .measurements.thread_candidates["32"].wall_seconds.mad == 0 and
      .measurements["store-fixture"].wall_seconds.median == 3 and
      .measurements["store-fixture"].wall_seconds.mad == 1 and
      .measurements["nextest-fast"].max_rss_kib.samples == [null,null,null] and
      .measurements["nextest-fast"].max_rss_kib.disposition == "unavailable_portable_backend" and
      .enumeration.full.runnable == ["binary-a::only::fast","binary-a::same::name","binary-b::only::full","binary-b::same::name"] and
      .scope_difference.identities == ["binary-b::only::full","binary-b::same::name"] and
      (.provenance.inventory | map(.path)) == (.provenance.inventory | map(.path) | sort | unique) and
      (.provenance.inventory | all(.link_count == 1 and (.path | startswith("/") | not) and (.path | contains("../") | not))) and
      (([.provenance.preflight_commands[] | .stdout,.stderr,.status,.normalized] + ["preflight/nextest-config.toml"] | sort | unique) - (.provenance.inventory | map(.path))) == [] and
      .provenance.inventory_total_bytes == ([.provenance.inventory[].bytes] | add) and
      (.budgets | map({probe, limit_wall_seconds}) | sort_by(.probe)) == [{"probe":"nextest-fast","limit_wall_seconds":6},{"probe":"nextest-full","limit_wall_seconds":14}]
    ' "$generated_one" >/dev/null; calibration_cases=$((calibration_cases + 1))
    check --baseline "$generated_one" --measurement "$calibration_abs/fast.json" >/dev/null
    check --baseline "$generated_one" --measurement "$calibration_abs/sweep-t8.json" >/dev/null; calibration_cases=$((calibration_cases + 2))
    local generated_hash; generated_hash="$(sha256_file "$generated_one")"
    ! (cd "$fixture_dir" && generate_baseline_impl private-self-test --calibration-root "$calibration_rel" --out "$calibration_rel/generated-one.json") >/dev/null 2>&1
    [ "$(sha256_file "$generated_one")" = "$generated_hash" ]; calibration_cases=$((calibration_cases + 1))

    local mutation_index=0
    calibration_inventory_digest() {
        python3 - "$calibration_abs" <<'PY'
import hashlib, json, os, stat, sys
root = sys.argv[1]
records = []
for current, directories, files in os.walk(root, topdown=True, followlinks=False):
    for name in sorted(directories + files):
        path = os.path.join(current, name); node = os.lstat(path); relative = os.path.relpath(path, root)
        record = [relative, node.st_dev, node.st_ino, stat.S_IMODE(node.st_mode), node.st_nlink, node.st_size]
        if stat.S_ISREG(node.st_mode):
            record.append(hashlib.sha256(open(path, "rb").read()).hexdigest())
        elif stat.S_ISLNK(node.st_mode):
            record.append(os.readlink(path))
        records.append(record)
print(hashlib.sha256(json.dumps(records, separators=(",", ":")).encode()).hexdigest())
PY
    }
    expect_generator_mutation_reject() {
        local relative="$1" filter="$2" label="$3" source_file backup_file candidate_file reject_out before_inventory after_inventory
        before_inventory="$(calibration_inventory_digest)"
        source_file="$calibration_abs/$relative"; backup_file="$fixture_dir/generator-backup.json"; candidate_file="$fixture_dir/generator-mutated.json"
        cp "$source_file" "$backup_file"
        jq "$filter" "$source_file" >"$candidate_file"; cp "$candidate_file" "$source_file"
        mutation_index=$((mutation_index + 1)); reject_out="$calibration_rel/reject-$mutation_index.json"
        if (cd "$fixture_dir" && generate_baseline_impl private-self-test --calibration-root "$calibration_rel" --out "$reject_out") >/dev/null 2>&1; then
            cp "$backup_file" "$source_file"; oracle_failure "generator accepted $label"; return 1
        fi
        cp "$backup_file" "$source_file"
        oracle_absent "$fixture_dir/$reject_out" "$label rejected output"
        after_inventory="$(calibration_inventory_digest)"; [ "$before_inventory" = "$after_inventory" ] || oracle_failure "$label changed calibration inventory"
        calibration_cases=$((calibration_cases + 1))
    }
    expect_generator_mutation_reject fast.json 'del(.samples[2])' partial-capture
    expect_generator_mutation_reject fast.json '.samples[0].execution.exit_status = 1' red-capture
    expect_generator_mutation_reject fast.json '.capture.requested.repeat = 4' repeat-mismatch
    expect_generator_mutation_reject fast.json '.capture.host.class = "stale-host"' host-mismatch
    expect_generator_mutation_reject fast.json '.capture.toolchain.rustc = "stale-rustc"' toolchain-mismatch
    expect_generator_mutation_reject fast.json '.capture.target.device = "stale-device"' target-mismatch
    expect_generator_mutation_reject fast.json '.capture.source.head = "0000000000000000000000000000000000000000"' source-mismatch
    expect_generator_mutation_reject sweep-t32.json '.capture.requested.resolved_threads = 31' thread-candidate-mismatch
    expect_generator_mutation_reject fast.json '(.samples[0].observed.executed_runnable_test_names) |= .[0:1]' identity-mismatch
    expect_generator_mutation_reject preflight.json '.retry_policy["rsid-fast"] = 1' retry-evidence
    expect_generator_mutation_reject preflight.json '.filters["ci-full"] = ["filter"]' filter-expansion
    expect_generator_mutation_reject nextest-full-list.json '."rust-suites"."binary-a"."testcases"."same::name".ignored = true' enumeration-mismatch
    expect_generator_mutation_reject fast.json '.capture.label = "wrong-label"' wrong-label
    expect_generator_mutation_reject fast.json '.capture.captured_at = "2026-02-30T00:00:00Z"' invalid-timestamp
    expect_generator_mutation_reject fast.json '.capture.captured_at = "2026-09-01T00:59:59Z"' rewritten-valid-timestamp
    expect_generator_mutation_reject fast.json '.capture.requested.threads = 7' requested-thread-mismatch
    expect_generator_mutation_reject fast.json '.samples[0].observed.passed_lines = true' boolean-count
    expect_generator_mutation_reject fast.json '.samples[0].observed.executed_test_names = ["binary-a::only::fast"]' desynchronized-executed-identities
    expect_generator_mutation_reject fast.json '.samples[1].execution.logs.stdout = .samples[0].execution.logs.stdout | .samples[1].execution.log_sha256.stdout = .samples[0].execution.log_sha256.stdout' reused-raw-log
    expect_generator_mutation_reject fast.json '.samples[0].execution.logs.stdout = "/absolute/raw.log"' absolute-raw-reference
    expect_generator_mutation_reject fast.json '.samples[0].execution.logs.stdout = "../escaping.log"' escaping-raw-reference
    expect_generator_mutation_reject fast.json '.capture.extra = 1' mixed-capture-schema
    expect_generator_mutation_reject nextest-full-list.json '.type = "rust-suite"' mixed-enumeration-schema

    local override_name override_inventory="$(calibration_inventory_digest)" public_probe_rel="target/test-suite-benchmark/z-baseline-calibration/$fixture_head-public-probe" public_probe_abs="$fixture_dir/target/test-suite-benchmark/z-baseline-calibration/$fixture_head-public-probe"
    local public_competitor_rel="$calibration_rel/public-competitor.json" public_competitor public_probe_stderr="$fixture_dir/public-probe.stderr"
    public_competitor="$fixture_dir/$public_competitor_rel"
    printf 'public competitor\n' >"$public_competitor"; chmod 0600 "$public_competitor"
    public_fixture_probe() {
        local variable="$1"
        ! (cd "$fixture_dir" && env -i PATH="/usr/local/bin:/usr/bin:/bin" "$variable=1" "$script_path" generate-baseline --calibration-root "$public_probe_rel" --out "$public_competitor_rel") >/dev/null 2>"$public_probe_stderr"
        grep -F "forbids fixture environment: $variable" "$public_probe_stderr" >/dev/null
        oracle_absent "$public_probe_abs" "public fixture probe root"
        [ "$(cat "$public_competitor")" = "public competitor" ] || oracle_failure "public fixture probe changed competitor"
        calibration_cases=$((calibration_cases + 1))
    }
    while IFS= read -r override_name; do public_fixture_probe "$override_name"; done < <(generator_override_names)
    for override_name in S1Q_TEST_FAULT S1Q_TEST_HOOK_STAGE FAKE_CARGO_ALL_INVOCATIONS FAKE_CARGO_CALL_COUNT_FILE FAKE_CARGO_FAIL FAKE_CARGO_FAIL_CALL FAKE_CARGO_INVOCATIONS FAKE_CARGO_LARGE_RED FAKE_CARGO_LOG FAKE_CARGO_METADATA_JSON FAKE_CARGO_MUTATE_TRACKED FAKE_NEXTEST_CONFIG_JSON FAKE_RUSTC_MUTATE_TRACKED; do
        public_fixture_probe "$override_name"
    done
    local former_token="generator-self"'-test-v1' former_variable="S1Q_INTERNAL_SELF_TEST_CAPABILITY" trusted_public_path
    trusted_public_path="$(/usr/bin/getent passwd "$(/usr/bin/id -u)" | /usr/bin/awk -F: '{print $6}')/.cargo/bin:/usr/bin:/bin"
    ! (cd "$fixture_dir" && env -i PATH="/usr/local/bin:/usr/bin:/bin" "$former_variable=$former_token" "$script_path" generate-baseline --calibration-root "$public_probe_rel" --out "$public_competitor_rel") >/dev/null 2>"$public_probe_stderr"
    grep -F "forbids fixture environment: $former_variable" "$public_probe_stderr" >/dev/null; calibration_cases=$((calibration_cases + 1))
    ! (cd "$fixture_dir" && env -i PATH="$trusted_public_path" "$script_path" generate-baseline "$former_token" --calibration-root "$public_probe_rel" --out "$public_competitor_rel") >/dev/null 2>"$public_probe_stderr"
    grep -F "unknown generate-baseline argument: $former_token" "$public_probe_stderr" >/dev/null; calibration_cases=$((calibration_cases + 1))
    ! (cd "$fixture_dir" && env -i PATH="$trusted_public_path" "$script_path" attest-preflight "$former_token" --calibration-root "$public_probe_rel" --threads 8) >/dev/null 2>"$public_probe_stderr"
    grep -F "unknown attest-preflight argument: $former_token" "$public_probe_stderr" >/dev/null; calibration_cases=$((calibration_cases + 1))
    local hidden_spelling
    for hidden_spelling in "__self-test-generate"'-baseline' self-test-generate-baseline generate-baseline-internal internal-generate-baseline; do
        ! (cd "$fixture_dir" && env -i PATH="/usr/local/bin:/usr/bin:/bin" "$script_path" "$hidden_spelling" "$former_token" --calibration-root "$public_probe_rel" --out "$public_competitor_rel") >/dev/null 2>"$public_probe_stderr"
        grep -F "unknown command: $hidden_spelling" "$public_probe_stderr" >/dev/null
        oracle_absent "$public_probe_abs" "hidden spelling probe root"; [ "$(cat "$public_competitor")" = "public competitor" ]
        calibration_cases=$((calibration_cases + 1))
    done
    ! (cd "$fixture_dir" && env -i PATH="/usr/local/bin:/usr/bin:/bin" /usr/bin/bash -c 'exec "$1" "$2" "$3" --calibration-root "$4" --out "$5"' child "$script_path" "__self-test-generate"'-baseline' "$former_token" "$public_probe_rel" "$public_competitor_rel") >/dev/null 2>"$public_probe_stderr"
    grep -F 'unknown command: __self-test-generate-baseline' "$public_probe_stderr" >/dev/null; calibration_cases=$((calibration_cases + 1))
    ! (cd "$fixture_dir" && env -i PATH="$stub_dir:/usr/local/bin:/usr/bin:/bin" "$script_path" generate-baseline --calibration-root "$public_probe_rel" --out "$public_competitor_rel") >/dev/null 2>"$public_probe_stderr"
    grep -F 'rejects PATH replacement' "$public_probe_stderr" >/dev/null
    oracle_absent "$public_probe_abs" "fake PATH probe root"; [ "$(cat "$public_competitor")" = "public competitor" ]; calibration_cases=$((calibration_cases + 1))
    ! (cd "$fixture_dir" && env -i PATH="$stub_dir:/usr/local/bin:/usr/bin:/bin" S1Q_TEST_MAX_RAW_BYTES=1 "$script_path" generate-baseline "$former_token" --calibration-root "$public_probe_rel" --out "$public_competitor_rel") >/dev/null 2>"$public_probe_stderr"
    grep -F 'forbids fixture environment: S1Q_TEST_MAX_RAW_BYTES' "$public_probe_stderr" >/dev/null; calibration_cases=$((calibration_cases + 1))
    rm "$public_competitor"
    [ "$override_inventory" = "$(calibration_inventory_digest)" ] || oracle_failure "public probe rejection changed calibration inventory"

    local mode_competitor="$calibration_abs/.mode-competitor" mode_before mode_out="$calibration_rel/reject-mode.json" mode_node mode_good mode_bad
    printf competitor >"$mode_competitor"; chmod 0600 "$mode_competitor"
    expect_mode_reject() {
        mode_node="$1"; mode_good="$2"; mode_bad="$3"; local label="$4"
        mode_before="$(calibration_inventory_digest)"; chmod "$mode_bad" "$mode_node"
        ! (cd "$fixture_dir" && generate_baseline_impl private-self-test --calibration-root "$calibration_rel" --out "$mode_out") >/dev/null 2>&1
        chmod "$mode_good" "$mode_node"; oracle_absent "$fixture_dir/$mode_out" "$label mode output"; [ "$(cat "$mode_competitor")" = competitor ]
        [ "$mode_before" = "$(calibration_inventory_digest)" ] || oracle_failure "$label mode rejection changed inventory"
        calibration_cases=$((calibration_cases + 1))
    }
    expect_mode_reject "$calibration_abs" 0700 0755 calibration-root
    mkdir -m 0700 "$calibration_abs/mode-parent"; mode_out="$calibration_rel/mode-parent/result.json"; expect_mode_reject "$calibration_abs/mode-parent" 0700 0755 nested-output-parent; rmdir "$calibration_abs/mode-parent"; mode_out="$calibration_rel/reject-mode.json"
    expect_mode_reject "$calibration_abs/preflight.json" 0600 0644 fixed-preflight
    expect_mode_reject "$calibration_abs/nextest-fast-list.json" 0600 0644 enumeration
    expect_mode_reject "$calibration_abs/fast.json" 0600 0644 capture
    local mode_raw="$calibration_abs/$(jq -r '.samples[0].execution.logs.stdout' "$calibration_abs/fast.json")"
    expect_mode_reject "$mode_raw" 0600 0644 raw-log
    rm "$mode_competitor"

    local raw_log raw_backup raw_reject="$calibration_rel/reject-raw-log.json"
    raw_log="$calibration_abs/$(jq -r '.samples[0].execution.logs.stdout' "$calibration_abs/fast.json")"; raw_backup="$fixture_dir/raw-log.backup"; cp "$raw_log" "$raw_backup"; printf stale >>"$raw_log"
    ! (cd "$fixture_dir" && generate_baseline_impl private-self-test --calibration-root "$calibration_rel" --out "$raw_reject") >/dev/null 2>&1
    cp "$raw_backup" "$raw_log"; oracle_absent "$fixture_dir/$raw_reject" "raw hash mismatch output"; calibration_cases=$((calibration_cases + 1))

    expect_raw_substitution_reject() {
        local content="$1" label="$2" before_inventory raw_hash reject_out capture_backup
        before_inventory="$(calibration_inventory_digest)"; capture_backup="$fixture_dir/raw-capture.backup"; cp "$calibration_abs/fast.json" "$capture_backup"; cp "$raw_log" "$raw_backup"
        printf '%s' "$content" >"$raw_log"; raw_hash="$(sha256_file "$raw_log")"
        jq --arg digest "$raw_hash" '.samples[0].execution.log_sha256.stdout = $digest' "$capture_backup" >"$fixture_dir/raw-capture.mutated"; cp "$fixture_dir/raw-capture.mutated" "$calibration_abs/fast.json"
        mutation_index=$((mutation_index + 1)); reject_out="$calibration_rel/reject-$mutation_index.json"
        ! (cd "$fixture_dir" && generate_baseline_impl private-self-test --calibration-root "$calibration_rel" --out "$reject_out") >/dev/null 2>&1
        cp "$raw_backup" "$raw_log"; cp "$capture_backup" "$calibration_abs/fast.json"; oracle_absent "$fixture_dir/$reject_out" "$label rejected output"
        [ "$before_inventory" = "$(calibration_inventory_digest)" ] || oracle_failure "$label changed calibration inventory"
        calibration_cases=$((calibration_cases + 1))
    }
    expect_raw_substitution_reject $'  FAIL [   0.001s] binary-a only::fast\nSummary [   0.001s] 1 test run: 0 passed, 0 skipped\n' non-retry-red-raw
    expect_raw_substitution_reject $'unrelated but hash-consistent output\n' malformed-raw-status
    expect_raw_substitution_reject $'  PASS [   0.001s] binary-a only::fast\n  PASS [   0.001s] binary-a only::fast\nSummary [   0.001s] 2 tests run: 2 passed, 0 skipped\n' duplicate-pass-identity
    expect_raw_substitution_reject $'  SKIP [   0.001s] binary-a ignored::fast\n  SKIP [   0.001s] binary-a ignored::fast\nSummary [   0.001s] 2 tests run: 0 passed, 2 skipped\n' duplicate-skip-identity

    expect_preflight_stdout_substitution_reject() {
        local command_name="$1" field="$2" content="$3" label="$4" preflight_backup="$fixture_dir/preflight-command.backup" normalized_backup="$fixture_dir/preflight-normalized.backup" evidence_path normalized_path digest normalized_digest reject_out before_inventory
        before_inventory="$(calibration_inventory_digest)"; cp "$calibration_abs/preflight.json" "$preflight_backup"
        evidence_path="$calibration_abs/$(jq -r --arg name "$command_name" --arg field "$field" '.commands[] | select(.name==$name) | .[$field]' "$preflight_backup")"; cp "$evidence_path" "$raw_backup"
        normalized_path="$calibration_abs/$(jq -r --arg name "$command_name" '.commands[] | select(.name==$name) | .normalized' "$preflight_backup")"; cp "$normalized_path" "$normalized_backup"
        printf '%s' "$content" >"$evidence_path"; chmod 0600 "$evidence_path"; digest="$(sha256_file "$evidence_path")"
        jq --arg field "${field}_sha256" --arg digest "$digest" '.[$field]=$digest' "$normalized_backup" >"$normalized_path"; chmod 0600 "$normalized_path"; normalized_digest="$(sha256_file "$normalized_path")"
        jq --arg name "$command_name" --arg field "${field}_sha256" --arg digest "$digest" --arg normalized_digest "$normalized_digest" '(.commands[] | select(.name==$name) | .[$field])=$digest | (.commands[] | select(.name==$name) | .normalized_sha256)=$normalized_digest' "$preflight_backup" >"$fixture_dir/preflight-command.mutated"; cp "$fixture_dir/preflight-command.mutated" "$calibration_abs/preflight.json"
        mutation_index=$((mutation_index + 1)); reject_out="$calibration_rel/reject-$mutation_index.json"
        ! (cd "$fixture_dir" && generate_baseline_impl private-self-test --calibration-root "$calibration_rel" --out "$reject_out") >/dev/null 2>&1
        cp "$raw_backup" "$evidence_path"; cp "$normalized_backup" "$normalized_path"; cp "$preflight_backup" "$calibration_abs/preflight.json"; oracle_absent "$fixture_dir/$reject_out" "$label output"
        [ "$before_inventory" = "$(calibration_inventory_digest)" ] || oracle_failure "$label changed inventory"; calibration_cases=$((calibration_cases + 1))
    }
    expect_preflight_stdout_substitution_reject make-test-fast stderr $'green\n' arbitrary-fast-lane-prose
    expect_preflight_stdout_substitution_reject make-test-full stdout $'green\n' arbitrary-full-lane-prose
    expect_preflight_stdout_substitution_reject make-test-fast stderr $'    Finished test profile [unoptimized + debuginfo] target(s) in 0.12s\n' truncated-fast-lane
    expect_preflight_stdout_substitution_reject make-test-fast stderr $'S1Q-COMPONENT-END nextest passed=2 failed=0 skipped=1\n' obsolete-marker-only-lane
    expect_preflight_stdout_substitution_reject make-test-fast stderr $'Summary [   0.100s] 3 tests run: 2 passed, 1 skipped\nSummary [   0.100s] 3 tests run: 2 passed, 1 skipped\n' duplicated-nextest-ending
    expect_preflight_stdout_substitution_reject make-test-full stderr $'Summary [   0.120s] 6 tests run: 4 passed, 2 skipped\n     Running `target/debug/rsi-provider-capability-validate --offline`\n     Running `target/debug/rsi-model-control-validate --offline`\n' reordered-component-endings

    expect_nextest_config_substitution_reject() {
        local content="$1" label="$2" before_inventory preflight_backup="$fixture_dir/config-preflight.backup" config_backup="$fixture_dir/config-raw.backup" config_path digest reject_out
        before_inventory="$(calibration_inventory_digest)"; cp "$calibration_abs/preflight.json" "$preflight_backup"
        config_path="$calibration_abs/$(jq -r '.nextest_config.path' "$preflight_backup")"; cp "$config_path" "$config_backup"
        printf '%s' "$content" >"$config_path"; chmod 0600 "$config_path"; digest="$(sha256_file "$config_path")"
        jq --arg digest "$digest" '.nextest_config.sha256=$digest' "$preflight_backup" >"$fixture_dir/config-preflight.mutated"; cp "$fixture_dir/config-preflight.mutated" "$calibration_abs/preflight.json"
        mutation_index=$((mutation_index + 1)); reject_out="$calibration_rel/reject-$mutation_index.json"
        ! (cd "$fixture_dir" && generate_baseline_impl private-self-test --calibration-root "$calibration_rel" --out "$reject_out") >/dev/null 2>&1
        cp "$config_backup" "$config_path"; cp "$preflight_backup" "$calibration_abs/preflight.json"; oracle_absent "$fixture_dir/$reject_out" "$label output"
        [ "$before_inventory" = "$(calibration_inventory_digest)" ] || oracle_failure "$label changed inventory"; calibration_cases=$((calibration_cases + 1))
    }
    expect_nextest_config_substitution_reject $'[profile.default]\nretries = 1\n[profile.rsid-fast]\ninherits = "default"\n[profile.ci-full]\ninherits = "default"\n' nextest-retry-policy
    expect_nextest_config_substitution_reject $'[profile.default]\nretries = 0\ndefault-filter = "not test(foo)"\n[profile.rsid-fast]\ninherits = "default"\n[profile.ci-full]\ninherits = "default"\n' nextest-filter-policy
    expect_nextest_config_substitution_reject $'[profile.default]\nretries = 0\n[profile.rsid-fast]\ninherits = "default"\n[[profile.rsid-fast.overrides]]\nfilter = "test(foo)"\nretries = 0\n[profile.ci-full]\ninherits = "default"\n' nextest-quarantine-override

    expect_preflight_normalized_mutation_reject() {
        local command_name="$1" filter="$2" label="$3" before_inventory preflight_backup="$fixture_dir/normalized-preflight.backup" normalized_backup="$fixture_dir/normalized-record.backup" normalized_path digest reject_out
        before_inventory="$(calibration_inventory_digest)"; cp "$calibration_abs/preflight.json" "$preflight_backup"
        normalized_path="$calibration_abs/$(jq -r --arg name "$command_name" '.commands[] | select(.name==$name) | .normalized' "$preflight_backup")"; cp "$normalized_path" "$normalized_backup"
        jq "$filter" "$normalized_backup" >"$normalized_path"; chmod 0600 "$normalized_path"; digest="$(sha256_file "$normalized_path")"
        jq --arg name "$command_name" --arg digest "$digest" '(.commands[] | select(.name==$name) | .normalized_sha256)=$digest' "$preflight_backup" >"$fixture_dir/normalized-preflight.mutated"; cp "$fixture_dir/normalized-preflight.mutated" "$calibration_abs/preflight.json"
        mutation_index=$((mutation_index + 1)); reject_out="$calibration_rel/reject-$mutation_index.json"
        ! (cd "$fixture_dir" && generate_baseline_impl private-self-test --calibration-root "$calibration_rel" --out "$reject_out") >/dev/null 2>&1
        cp "$normalized_backup" "$normalized_path"; cp "$preflight_backup" "$calibration_abs/preflight.json"; oracle_absent "$fixture_dir/$reject_out" "$label output"
        [ "$before_inventory" = "$(calibration_inventory_digest)" ] || oracle_failure "$label changed inventory"; calibration_cases=$((calibration_cases + 1))
    }
    expect_preflight_normalized_mutation_reject make-test-fast '.components[0].passed += 1' normalized-count-mutation
    expect_preflight_normalized_mutation_reject make-test-full '.components |= [.[1],.[0],.[2],.[3]]' normalized-component-reordering
    expect_preflight_normalized_mutation_reject make-test-full '.components += [.components[0]]' normalized-component-duplication

    expect_preflight_status_mutation_reject() {
        local command_name="$1" status_filter="$2" command_filter="$3" label="$4" before_inventory preflight_backup="$fixture_dir/raw-preflight.backup" status_backup="$fixture_dir/raw-preflight-status.backup" status_path digest reject_out
        before_inventory="$(calibration_inventory_digest)"; cp "$calibration_abs/preflight.json" "$preflight_backup"
        status_path="$calibration_abs/$(jq -r --arg name "$command_name" '.commands[] | select(.name==$name) | .status' "$preflight_backup")"; cp "$status_path" "$status_backup"
        jq "$status_filter" "$status_backup" >"$status_path"; chmod 0600 "$status_path"; digest="$(sha256_file "$status_path")"
        jq --arg name "$command_name" --arg digest "$digest" "(.commands[] | select(.name==\$name) | .status_sha256)=\$digest | $command_filter" "$preflight_backup" >"$fixture_dir/raw-preflight.mutated"; cp "$fixture_dir/raw-preflight.mutated" "$calibration_abs/preflight.json"
        mutation_index=$((mutation_index + 1)); reject_out="$calibration_rel/reject-$mutation_index.json"
        ! (cd "$fixture_dir" && generate_baseline_impl private-self-test --calibration-root "$calibration_rel" --out "$reject_out") >/dev/null 2>&1
        cp "$status_backup" "$status_path"; cp "$preflight_backup" "$calibration_abs/preflight.json"; oracle_absent "$fixture_dir/$reject_out" "$label output"
        [ "$before_inventory" = "$(calibration_inventory_digest)" ] || oracle_failure "$label changed inventory"; calibration_cases=$((calibration_cases + 1))
    }
    expect_preflight_status_mutation_reject make-test-fast '.exit_status=1' '(.commands[] | select(.name==$name) | .exit_status)=1' preflight-exit-mutation
    expect_preflight_status_mutation_reject make-test-fast '.started_at="2000-01-01T00:00:00Z"' '.' preflight-timestamp-order-mutation

    local status_relative status_file status_capture_backup="$fixture_dir/status-capture.backup" status_backup="$fixture_dir/status-raw.backup" status_digest status_reject="$calibration_rel/reject-status-label.json" status_before
    status_before="$(calibration_inventory_digest)"; cp "$calibration_abs/fast.json" "$status_capture_backup"; status_relative="$(jq -r '.samples[0].execution.logs.status' "$status_capture_backup")"; status_file="$calibration_abs/$status_relative"; cp "$status_file" "$status_backup"
    jq '.label="sample-99"' "$status_backup" >"$status_file"; chmod 0600 "$status_file"; status_digest="$(sha256_file "$status_file")"
    jq --arg digest "$status_digest" '.samples[0].execution.log_sha256.status=$digest' "$status_capture_backup" >"$fixture_dir/status-capture.mutated"; cp "$fixture_dir/status-capture.mutated" "$calibration_abs/fast.json"
    ! (cd "$fixture_dir" && generate_baseline_impl private-self-test --calibration-root "$calibration_rel" --out "$status_reject") >/dev/null 2>&1
    cp "$status_backup" "$status_file"; cp "$status_capture_backup" "$calibration_abs/fast.json"; oracle_absent "$fixture_dir/$status_reject" "wrong raw status label output"; [ "$status_before" = "$(calibration_inventory_digest)" ]; calibration_cases=$((calibration_cases + 1))

    local false_target="$fixture_dir/false-target" false_backup="$fixture_dir/false-provenance-backup" false_reject="$calibration_rel/reject-false-provenance.json" false_before
    mkdir "$false_target" "$false_backup"; false_before="$(calibration_inventory_digest)"
    for source_file in preflight.json store.json scanner.json fast.json sweep-t1.json sweep-t8.json sweep-t16.json sweep-t32.json; do cp "$calibration_abs/$source_file" "$false_backup/$source_file"; done
    python3 - "$calibration_abs" "$false_target" <<'PY'
import hashlib, json, os, sys
root, target_path = sys.argv[1:]
node = os.stat(target_path)
host = {"class":"FalseOS-falsearch-99cpu","os":"FalseOS","kernel":"false-kernel","architecture":"falsearch","cpu_model":"false cpu","logical_cpus":99}
host["fingerprint_sha256"] = hashlib.sha256("|".join(str(host[key]) for key in ("class","os","kernel","architecture","cpu_model","logical_cpus")).encode()).hexdigest()
target = {"canonical_path":target_path,"device":str(node.st_dev),"inode":str(node.st_ino)}
target["fingerprint_sha256"] = hashlib.sha256("|".join(str(target[key]) for key in ("canonical_path","device","inode")).encode()).hexdigest()
toolchain = {"cargo":"false cargo","rustc":"false rustc","cargo_nextest":"false nextest","cargo_target_dir":target_path}
for name in ("preflight.json","store.json","scanner.json","fast.json","sweep-t1.json","sweep-t8.json","sweep-t16.json","sweep-t32.json"):
    path = os.path.join(root, name)
    with open(path, encoding="utf-8") as stream: value = json.load(stream)
    target_value = value if name == "preflight.json" else value["capture"]
    target_value["host"], target_value["toolchain"], target_value["target"] = host, toolchain, target
    with open(path, "w", encoding="utf-8") as stream: json.dump(value, stream, sort_keys=True, separators=(",", ":")); stream.write("\n")
PY
    ! (cd "$fixture_dir" && generate_baseline_impl private-self-test --calibration-root "$calibration_rel" --out "$false_reject") >/dev/null 2>&1
    for source_file in preflight.json store.json scanner.json fast.json sweep-t1.json sweep-t8.json sweep-t16.json sweep-t32.json; do cp "$false_backup/$source_file" "$calibration_abs/$source_file"; done
    rmdir "$false_target"; rm -rf "$false_backup"; oracle_absent "$fixture_dir/$false_reject" "self-consistent false provenance output"; [ "$false_before" = "$(calibration_inventory_digest)" ] || oracle_failure "false provenance changed calibration inventory"; calibration_cases=$((calibration_cases + 1))

    start_hooked_generator() {
        generator_hook_out="$1"; generator_hook_stage="$2"; generator_hook_dir="$(mktemp -d "$fixture_dir/generator-hook.XXXXXX")"
        generator_stage_fifo="$generator_hook_dir/stage.fifo"; generator_continue_fifo="$generator_hook_dir/continue.fifo"; generator_status_file="$generator_hook_dir/status"
        mkfifo "$generator_stage_fifo" "$generator_continue_fifo"
        (
            set +e; cd "$fixture_dir" || exit 97
            S1Q_TEST_GENERATE_HOOK_STAGE="$generator_hook_stage" S1Q_TEST_STAGE_FIFO="$generator_stage_fifo" S1Q_TEST_CONTINUE_FIFO="$generator_continue_fifo" generate_baseline_impl private-self-test --calibration-root "$calibration_rel" --out "$generator_hook_out" >"$generator_hook_dir/stdout" 2>"$generator_hook_dir/stderr"
            printf '%s\n' "$?" >"$generator_status_file"
        ) &
        generator_hook_pid=$!
        local reported; IFS= read -r reported <"$generator_stage_fifo"; [ "$reported" = "$generator_hook_stage" ] || oracle_failure "generator hook stage mismatch"
    }
    finish_hooked_generator() {
        printf '%s\n' "$generator_hook_stage" >"$generator_continue_fifo"; wait "$generator_hook_pid"
        generator_hook_status="$(cat "$generator_status_file")"; rm -rf "$generator_hook_dir"
    }

    local source_fence_out="$calibration_rel/reject-source-fence.json"
    start_hooked_generator "$source_fence_out" before-publish; printf changed >"$fixture_dir/tracked.txt"; finish_hooked_generator
    [ "$generator_hook_status" -ne 0 ]; oracle_absent "$fixture_dir/$source_fence_out" "source mutation fence output"; git -C "$fixture_dir" restore tracked.txt; calibration_cases=$((calibration_cases + 1))
    local compete_out="$calibration_rel/reject-compete.json"
    start_hooked_generator "$compete_out" before-publish; printf competitor >"$fixture_dir/$compete_out"; finish_hooked_generator
    [ "$generator_hook_status" -ne 0 ] && [ "$(cat "$fixture_dir/$compete_out")" = competitor ]; rm "$fixture_dir/$compete_out"; calibration_cases=$((calibration_cases + 1))

    local custody_before hook_out="$calibration_rel/reject-hook.json" moved_input="$fixture_dir/moved-input.log" moved_root="$fixture_dir/moved-calibration-root" moved_parent="$calibration_abs/moved-publish-parent"
    custody_before="$(calibration_inventory_digest)"; start_hooked_generator "$hook_out" before-read; ln "$calibration_abs/preflight.json" "$calibration_abs/input-hardlink"; finish_hooked_generator
    [ "$generator_hook_status" -ne 0 ]; rm "$calibration_abs/input-hardlink"; oracle_absent "$fixture_dir/$hook_out" "before-read hardlink output"; [ "$custody_before" = "$(calibration_inventory_digest)" ]; calibration_cases=$((calibration_cases + 1))

    custody_before="$(calibration_inventory_digest)"; start_hooked_generator "$hook_out" before-read; mv "$calibration_abs/scanner.json" "$fixture_dir/scanner.original"; ln "$calibration_abs/store.json" "$calibration_abs/scanner.json"; finish_hooked_generator
    [ "$generator_hook_status" -ne 0 ]
    rm "$calibration_abs/scanner.json"; mv "$fixture_dir/scanner.original" "$calibration_abs/scanner.json"; oracle_absent "$fixture_dir/$hook_out" "aliased fixed input output"; [ "$custody_before" = "$(calibration_inventory_digest)" ]; calibration_cases=$((calibration_cases + 1))

    custody_before="$(calibration_inventory_digest)"; mv "$calibration_abs/scanner.json" "$fixture_dir/scanner.original"; ln -s store.json "$calibration_abs/scanner.json"
    ! (cd "$fixture_dir" && generate_baseline_impl private-self-test --calibration-root "$calibration_rel" --out "$hook_out") >/dev/null 2>&1
    rm "$calibration_abs/scanner.json"; mv "$fixture_dir/scanner.original" "$calibration_abs/scanner.json"; oracle_absent "$fixture_dir/$hook_out" "symlinked fixed input output"; [ "$custody_before" = "$(calibration_inventory_digest)" ]; calibration_cases=$((calibration_cases + 1))

    custody_before="$(calibration_inventory_digest)"; start_hooked_generator "$hook_out" before-publish; mv "$raw_log" "$moved_input"; cp "$moved_input" "$raw_log"; finish_hooked_generator
    [ "$generator_hook_status" -ne 0 ]; rm "$raw_log"; mv "$moved_input" "$raw_log"; oracle_absent "$fixture_dir/$hook_out" "replaced held input output"; [ "$custody_before" = "$(calibration_inventory_digest)" ]; calibration_cases=$((calibration_cases + 1))

    custody_before="$(calibration_inventory_digest)"; start_hooked_generator "$hook_out" before-publish; mv "$calibration_abs" "$moved_root"; mkdir -m 0700 "$calibration_abs"; finish_hooked_generator
    [ "$generator_hook_status" -ne 0 ]; rmdir "$calibration_abs"; mv "$moved_root" "$calibration_abs"; oracle_absent "$fixture_dir/$hook_out" "replaced calibration root output"; [ "$custody_before" = "$(calibration_inventory_digest)" ]; calibration_cases=$((calibration_cases + 1))

    custody_before="$(calibration_inventory_digest)"; mkdir -m 0700 "$calibration_abs/publish-parent"; start_hooked_generator "$calibration_rel/publish-parent/result.json" before-publish; mv "$calibration_abs/publish-parent" "$moved_parent"; mkdir -m 0700 "$calibration_abs/publish-parent"; finish_hooked_generator
    [ "$generator_hook_status" -ne 0 ]; rmdir "$calibration_abs/publish-parent"; mv "$moved_parent" "$calibration_abs/publish-parent"; rmdir "$calibration_abs/publish-parent"; oracle_absent "$calibration_abs/publish-parent/result.json" "replaced output parent output"; [ "$custody_before" = "$(calibration_inventory_digest)" ]; calibration_cases=$((calibration_cases + 1))

    ln -s raw "$calibration_abs/symlink-parent"; ! (cd "$fixture_dir" && generate_baseline_impl private-self-test --calibration-root "$calibration_rel" --out "$calibration_rel/symlink-parent/result.json") >/dev/null 2>&1; rm "$calibration_abs/symlink-parent"; calibration_cases=$((calibration_cases + 1))

    custody_before="$(calibration_inventory_digest)"; start_hooked_generator "$hook_out" before-publish; ln -s competitor-target "$fixture_dir/$hook_out"; finish_hooked_generator
    [ "$generator_hook_status" -ne 0 ] && [ -L "$fixture_dir/$hook_out" ]; rm "$fixture_dir/$hook_out"; [ "$custody_before" = "$(calibration_inventory_digest)" ]; calibration_cases=$((calibration_cases + 1))

    custody_before="$(calibration_inventory_digest)"; start_hooked_generator "$hook_out" after-link; rm "$fixture_dir/$hook_out"; printf competitor >"$fixture_dir/$hook_out"; finish_hooked_generator
    [ "$generator_hook_status" -ne 0 ] && [ "$(cat "$fixture_dir/$hook_out")" = competitor ]; rm "$fixture_dir/$hook_out"; [ "$custody_before" = "$(calibration_inventory_digest)" ]; calibration_cases=$((calibration_cases + 1))

    custody_before="$(calibration_inventory_digest)"; start_hooked_generator "$hook_out" after-link; printf changed >"$fixture_dir/tracked.txt"; finish_hooked_generator
    [ "$generator_hook_status" -ne 0 ]; git -C "$fixture_dir" restore tracked.txt; oracle_absent "$fixture_dir/$hook_out" "after-link source mutation output"; [ "$custody_before" = "$(calibration_inventory_digest)" ]; calibration_cases=$((calibration_cases + 1))
    [ -z "$(find "$calibration_abs" -name '.baseline-json.*' -print -quit)" ] || oracle_failure "generator private inode leaked"

    expect_generator_bound() {
        local variable="$1" exact_value="$2" label="$3" positive_out negative_out before_inventory
        positive_out="$calibration_rel/boundary-$label.json"; negative_out="$calibration_rel/over-$label.json"
        before_inventory="$(calibration_inventory_digest)"
        if ! (cd "$fixture_dir" && export "$variable=$exact_value" && generate_baseline_impl private-self-test --calibration-root "$calibration_rel" --out "$positive_out") >"$fixture_dir/bound.stdout" 2>"$fixture_dir/bound.stderr"; then
            cat "$fixture_dir/bound.stderr" >&2; oracle_failure "$label exact bound rejected"; return 1
        fi
        [ -f "$fixture_dir/$positive_out" ] && [ ! -L "$fixture_dir/$positive_out" ]; rm "$fixture_dir/$positive_out"
        ! (cd "$fixture_dir" && export "$variable=$((exact_value - 1))" && generate_baseline_impl private-self-test --calibration-root "$calibration_rel" --out "$negative_out") >/dev/null 2>&1
        oracle_absent "$fixture_dir/$negative_out" "$label one-over output"; [ "$before_inventory" = "$(calibration_inventory_digest)" ] || oracle_failure "$label bound changed calibration inventory"
        calibration_cases=$((calibration_cases + 2))
    }
    local max_preflight max_enum max_capture max_raw aggregate_bound shape_bounds max_depth max_string output_bound output_probe output_probe_abs
    output_probe="$calibration_rel/output-bound-probe.json"; output_probe_abs="$fixture_dir/$output_probe"
    max_preflight="$(stat -Lc %s "$calibration_abs/preflight.json")"
    max_enum="$(stat -Lc %s "$calibration_abs/nextest-fast-list.json" "$calibration_abs/nextest-full-list.json" | sort -n | tail -1)"
    max_capture="$(stat -Lc %s "$calibration_abs/store.json" "$calibration_abs/scanner.json" "$calibration_abs/fast.json" "$calibration_abs"/sweep-t*.json | sort -n | tail -1)"
    max_raw="$(jq '[.provenance.inventory[] | select((.path | startswith("raw/")) or (.path | startswith("preflight/"))) | .bytes] | max' "$generated_one")"
    aggregate_bound="$(jq -r '.provenance.inventory_total_bytes' "$generated_one")"
    shape_bounds="$(python3 - "$calibration_abs" <<'PY'
import json, os, sys
root = sys.argv[1]
files = ["preflight.json","nextest-fast-list.json","nextest-full-list.json","store.json","scanner.json","fast.json","sweep-t1.json","sweep-t8.json","sweep-t16.json","sweep-t32.json"]
maximum_depth = maximum_string = 0
def walk(value, depth=0):
    global maximum_depth, maximum_string
    maximum_depth = max(maximum_depth, depth)
    if isinstance(value, dict):
        for key, child in value.items(): maximum_string = max(maximum_string, len(key.encode())); walk(child, depth + 1)
    elif isinstance(value, list):
        for child in value: walk(child, depth + 1)
    elif isinstance(value, str): maximum_string = max(maximum_string, len(value.encode()))
for name in files:
    with open(os.path.join(root, name), encoding="utf-8") as stream: walk(json.load(stream))
print(maximum_depth, maximum_string)
PY
)"; read -r max_depth max_string <<<"$shape_bounds"
    expect_generator_bound S1Q_TEST_MAX_PREFLIGHT_BYTES "$max_preflight" preflight-bytes
    expect_generator_bound S1Q_TEST_MAX_ENUM_BYTES "$max_enum" enumeration-bytes
    expect_generator_bound S1Q_TEST_MAX_CAPTURE_BYTES "$max_capture" capture-bytes
    expect_generator_bound S1Q_TEST_MAX_RAW_BYTES "$max_raw" raw-bytes
    expect_generator_bound S1Q_TEST_MAX_AGGREGATE_BYTES "$aggregate_bound" aggregate-bytes
    expect_generator_bound S1Q_TEST_MAX_JSON_DEPTH "$max_depth" json-depth
    expect_generator_bound S1Q_TEST_MAX_STRING_BYTES "$max_string" string-bytes
    expect_generator_bound S1Q_TEST_MAX_SUITES 2 suite-count
    expect_generator_bound S1Q_TEST_MAX_TESTS 3 testcase-count
    expect_generator_bound S1Q_TEST_MAX_SAMPLES 5 sample-count
    expect_generator_bound S1Q_TEST_MAX_IDENTITIES 6 identity-count
    output_bound=1000000
    for _ in 1 2 3; do
        (cd "$fixture_dir" && S1Q_TEST_MAX_OUTPUT_BYTES="$output_bound" generate_baseline_impl private-self-test --calibration-root "$calibration_rel" --out "$output_probe") >/dev/null 2>&1
        local measured_output; measured_output="$(stat -Lc %s "$output_probe_abs")"; rm "$output_probe_abs"
        [ "$measured_output" = "$output_bound" ] && break; output_bound="$measured_output"
    done
    expect_generator_bound S1Q_TEST_MAX_OUTPUT_BYTES "$output_bound" output-bytes

    local malformed_backup="$fixture_dir/malformed-json.backup" malformed_out="$calibration_rel/reject-malformed-json.json" malformed_before
    malformed_before="$(calibration_inventory_digest)"; cp "$calibration_abs/fast.json" "$malformed_backup"; printf '{\n' >"$calibration_abs/fast.json"
    ! (cd "$fixture_dir" && generate_baseline_impl private-self-test --calibration-root "$calibration_rel" --out "$malformed_out") >/dev/null 2>&1
    cp "$malformed_backup" "$calibration_abs/fast.json"; oracle_absent "$fixture_dir/$malformed_out" "malformed JSON output"; [ "$malformed_before" = "$(calibration_inventory_digest)" ]; calibration_cases=$((calibration_cases + 1))
    malformed_before="$(calibration_inventory_digest)"; cp "$calibration_abs/nextest-fast-list.json" "$malformed_backup"; printf '{"type":"rust-suite"}\n' >>"$calibration_abs/nextest-fast-list.json"
    ! (cd "$fixture_dir" && generate_baseline_impl private-self-test --calibration-root "$calibration_rel" --out "$malformed_out") >/dev/null 2>&1
    cp "$malformed_backup" "$calibration_abs/nextest-fast-list.json"; oracle_absent "$fixture_dir/$malformed_out" "malformed NDJSON output"; [ "$malformed_before" = "$(calibration_inventory_digest)" ]; calibration_cases=$((calibration_cases + 1))
    ! (cd "$fixture_dir" && "$script_path" generate-baseline --calibration-root metrics --out metrics/baseline.json) >/dev/null 2>&1; calibration_cases=$((calibration_cases + 1))

    local even_base="$fixture_dir/even-base.json" even_result
    jq '(.capture.requested.repeat = 4) | (.samples = [range(1;5) as $i | .samples[0] | .index = $i | .execution.timing.wall_seconds = $i])' "$baseline" >"$even_base"
    even_result="$(compare --baseline "$even_base" --candidate "$even_base")"
    jq -e '.comparison.baseline.median_wall_seconds == 2.5 and .comparison.baseline.mad_wall_seconds == 1' <<<"$even_result" >/dev/null; calibration_cases=$((calibration_cases + 1))
    ! (cd "$fixture_dir" && PATH="$stub_dir:$PATH" "$script_path" capture --label custody --probe store-fixture --repeat 1 --threads 1 --out target/test-suite-benchmark/escape/out.json) >/dev/null 2>&1; [ ! -e "$external_dir/out.json" ]
    mkdir -p "$fixture_dir/target/test-suite-benchmark/.stale-store-fixture.samples"; printf '{"index":99}\n' >"$fixture_dir/target/test-suite-benchmark/.stale-store-fixture.samples/99.json"
    (cd "$fixture_dir" && PATH="$stub_dir:$PATH" FAKE_CARGO_LOG="$fixture_dir/calls" "$script_path" capture --label stale --probe store-fixture --repeat 12 --threads 1 --out target/test-suite-benchmark/stale.json); jq -e '[.samples[].index] == [range(1;13)]' "$fixture_dir/target/test-suite-benchmark/stale.json" >/dev/null; [ -f "$fixture_dir/target/test-suite-benchmark/.stale-store-fixture.samples/99.json" ]
    (cd "$fixture_dir" && PATH="$stub_dir:$PATH" FAKE_CARGO_LOG="$fixture_dir/calls" "$script_path" capture --label scanner --probe source-scanner --repeat 1 --threads 1 --out target/test-suite-benchmark/scanner.json); (cd "$fixture_dir" && PATH="$stub_dir:$PATH" FAKE_CARGO_LOG="$fixture_dir/calls" "$script_path" capture --label v87 --probe v87-matrices --repeat 1 --threads 1 --out target/test-suite-benchmark/v87.json)
    jq -e '.samples[0].observed.identity_proof.verified and (.samples[0].observed.identity_proof.executed | length == 1)' "$fixture_dir/target/test-suite-benchmark/scanner.json" >/dev/null; jq -e '.samples[0].observed.identity_proof.verified and (.samples[0].observed.identity_proof.executed | length == 3)' "$fixture_dir/target/test-suite-benchmark/v87.json" >/dev/null
    ! (cd "$fixture_dir" && PATH="$stub_dir:$PATH" "$script_path" capture --label serial --probe rsid-serial --repeat 1 --threads 8 --out target/test-suite-benchmark/serial.json) >/dev/null 2>&1; ! (cd "$fixture_dir" && PATH="$stub_dir:$PATH" FAKE_CARGO_FAIL=1 "$script_path" capture --label warmup --probe store-fixture --repeat 1 --threads 1 --out target/test-suite-benchmark/warmup.json) >/dev/null 2>&1; jq -e '.capture.warmup.exit_status == 1 and (.samples | length == 0) and (.capture.workspace | startswith("/"))' "$fixture_dir/target/test-suite-benchmark/warmup.json" >/dev/null
    (cd "$fixture_dir" && PATH="$stub_dir:$PATH" FAKE_CARGO_INVOCATIONS="$fixture_dir/serial.calls" "$script_path" capture --label serial-once --probe rsid-serial --repeat 1 --threads 1 --out target/test-suite-benchmark/serial-once.json); [ "$(wc -l <"$fixture_dir/serial.calls")" = 2 ]; grep -Fx 'test -p rsid --lib --no-run ' "$fixture_dir/serial.calls" >/dev/null; grep -Fx 'test -p rsid --lib -- --test-threads 1 ' "$fixture_dir/serial.calls" >/dev/null; jq -e '.capture.warmup.kind == "artifact-build" and (.capture.warmup.command | contains("--no-run")) and (.samples | length == 1)' "$fixture_dir/target/test-suite-benchmark/serial-once.json" >/dev/null
    ! (cd "$fixture_dir" && PATH="$stub_dir:$PATH" FAKE_CARGO_LARGE_RED=1 "$script_path" capture --label serial-large-red --probe rsid-serial --repeat 1 --threads 1 --out target/test-suite-benchmark/serial-large-red.json) >/dev/null 2>&1; jq -e '.samples | length == 1 and .[0].execution.exit_status != 0 and .[0].execution.evidence_valid == false and .[0].observed.failure_names == ["fixture::large::failure"] and (. [0].observed.executed_test_names | @json | length > 131072) and (. [0].observed.executed_test_names | length == 5001) and .[0].observed.identity_proof.executed == .[0].observed.executed_test_names' "$fixture_dir/target/test-suite-benchmark/serial-large-red.json" >/dev/null
    ! (cd "$fixture_dir" && PATH="$stub_dir:$PATH" FAKE_CARGO_MUTATE_TRACKED="$fixture_dir/tracked.txt" "$script_path" capture --label source-race --probe store-fixture --repeat 1 --threads 1 --out target/test-suite-benchmark/source-race.json) >/dev/null 2>&1; [ ! -e "$fixture_dir/target/test-suite-benchmark/source-race.json" ]; [ -n "$(git -C "$fixture_dir" status --porcelain -- tracked.txt)" ]
    git -C "$fixture_dir" restore tracked.txt; rm -f "$fixture_dir/tracked.txt.mutated"; ! (cd "$fixture_dir" && PATH="$stub_dir:$PATH" FAKE_RUSTC_MUTATE_TRACKED="$fixture_dir/tracked.txt" "$script_path" capture --label source-late-race --probe store-fixture --repeat 1 --threads 1 --out target/test-suite-benchmark/source-late-race.json) >/dev/null 2>&1; [ ! -e "$fixture_dir/target/test-suite-benchmark/source-late-race.json" ]; [ -n "$(git -C "$fixture_dir" status --porcelain -- tracked.txt)" ]
    git -C "$fixture_dir" restore tracked.txt; rm -f "$fixture_dir/tracked.txt.mutated"

    # S1-Q external transport prerequisite for F-003/F-004: parser, path,
    # capability, custody, staging, publication, and retained-evidence matrix.
    local root out status sentinel error_file alias_dir alias_root platform_stub fault node source_node target_node
    local workspace_leaf private_leaf private_path workspace_path private_identity workspace_identity moved_path competitor_path
    local before after adversary parent_path moved_parent competitor_dir moved_identity lock_root lock_out_one lock_out_two lock_out_three
    local runner_state root_inventory adversary_inventory baseline_root baseline_parent baseline_adversary additions root_expected adversary_expected
    local all_cargo_log benchmark_log control_root control_out control_baseline

    # Deliberate negative controls prove that the shared disposition helpers
    # reject the three false-green classes retained by the prior rereview.
    all_cargo_log="$(mktemp "$fixture_dir/oracle-control-all-cargo.XXXXXX")"
    benchmark_log="$(mktemp "$fixture_dir/oracle-control-benchmark.XXXXXX")"
    out="$fixture_dir/oracle-control-output"
    if assert_before_work_disposition 1 "$all_cargo_log" "$benchmark_log" "$out" "negative wrong-status" 2>/dev/null; then
        oracle_failure "wrong-status negative control was accepted"
    fi
    printf 'nextest --version \n' >"$all_cargo_log"
    if assert_before_work_disposition 2 "$all_cargo_log" "$benchmark_log" "$out" "negative early-invocation" 2>/dev/null; then
        oracle_failure "unexpected-before-work-invocation negative control was accepted"
    fi
    : >"$all_cargo_log"
    if assert_late_reject_disposition 2 "$all_cargo_log" "$benchmark_log" "negative missing-late-invocation" 2>/dev/null; then
        oracle_failure "missing-late-invocation negative control was accepted"
    fi

    root="$(new_external_root)"; out="$root/absolute-without-opt-in.json"; sentinel="$(mktemp "$fixture_dir/absolute-all-cargo.XXXXXX")"; benchmark_log="$(mktemp "$fixture_dir/absolute-benchmark.XXXXXX")"; error_file="$fixture_dir/absolute-error"
    before="$(mktemp "$fixture_dir/absolute-root-inventory.XXXXXX")"; runner_inventory_snapshot "$root" "$before"
    set +e
    (cd "$fixture_dir" && env PATH="$stub_dir:$PATH" FAKE_CARGO_ALL_INVOCATIONS="$sentinel" FAKE_CARGO_INVOCATIONS="$benchmark_log" "$script_path" capture --label absolute --probe store-fixture --repeat 1 --threads 1 --out "$out") >/dev/null 2>"$error_file"
    status=$?
    set -e
    assert_before_work_disposition "$status" "$sentinel" "$benchmark_log" "$out" "absolute path without opt-in"
    runner_inventory_assert "$root" "$before"
    oracle_contains "output path must be below target/test-suite-benchmark/: $out" "$error_file" "absolute-path diagnostic"

    before="$(mktemp "$fixture_dir/parser-root-inventory.XXXXXX")"; runner_inventory_snapshot "$root" "$before"
    expect_parser_reject_before_cargo capture --label parser --probe store-fixture --repeat 1 --threads 1 --external-output-root; runner_inventory_assert "$root" "$before"
    expect_parser_reject_before_cargo capture --label parser --probe store-fixture --repeat 1 --threads 1 --external-output-root --out "$out"; runner_inventory_assert "$root" "$before"
    expect_parser_reject_before_cargo capture --label parser --probe store-fixture --repeat 1 --threads 1 --external-output-root "$root" --external-output-root "$root" --out "$out"; runner_inventory_assert "$root" "$before"
    expect_parser_reject_before_cargo capture --label parser --probe store-fixture --repeat 1 --threads 1 --out "$out" --external-output-root "$root"; runner_inventory_assert "$root" "$before"
    expect_parser_reject_before_cargo compare --external-output-root "$root"; runner_inventory_assert "$root" "$before"
    expect_parser_reject_before_cargo check --external-output-root "$root"; runner_inventory_assert "$root" "$before"
    expect_parser_reject_before_cargo --self-test --external-output-root "$root"; runner_inventory_assert "$root" "$before"

    expect_external_reject_before_work relative-root relative-root/capture.json
    expect_external_reject_before_work "" /empty-root/capture.json
    expect_external_reject_before_work "$root" relative-output.json
    expect_external_reject_before_work "$root" ""
    expect_external_reject_before_work "$root//alias" "$root//alias/capture.json"
    expect_external_reject_before_work "$root" "$root//capture.json"
    expect_external_reject_before_work "$root/." "$root/./capture.json"
    expect_external_reject_before_work "$root/../escape" "$root/../escape/capture.json"
    expect_external_reject_before_work "$root/" "$root/capture.json"
    expect_external_reject_before_work "$root" "$root/nested/"
    expect_external_reject_before_work "$root" "$root"
    expect_external_reject_before_work / /capture.json
    expect_external_reject_before_work "$root" "${root}-sibling/capture.json"
    expect_external_reject_before_work "$root" "$root/nested/../capture.json"
    expect_external_reject_before_work "$root" "$root/tab"$'\t'"component/capture.json"
    expect_external_reject_before_work "$root" "$root/cr"$'\r'"component/capture.json"
    expect_external_reject_before_work "$root" "$root/lf"$'\n'"component/capture.json"
    expect_external_reject_before_work "$root"$'\t'"component" "$root"$'\t'"component/capture.json"
    expect_external_reject_before_work "$root"$'\r'"component" "$root"$'\r'"component/capture.json"
    expect_external_reject_before_work "$root"$'\n'"component" "$root"$'\n'"component/capture.json"
    expect_external_reject_before_work "$fixture_dir" "$fixture_dir/capture.json"
    mkdir -m 0700 "$fixture_dir/worktree-child"
    before="$(mktemp "$fixture_dir/worktree-child-baseline.XXXXXX")"; runner_inventory_snapshot "$fixture_dir/worktree-child" "$before"
    expect_external_reject_before_work "$fixture_dir/worktree-child" "$fixture_dir/worktree-child/capture.json"
    runner_inventory_assert "$fixture_dir/worktree-child" "$before"

    platform_stub="$fixture_dir/platform-stub"; mkdir "$platform_stub"
    printf '%s\n' '#!/usr/bin/env bash' 'if [ "${1:-}" = -s ]; then echo Darwin; else echo fixture; fi' >"$platform_stub/uname"; chmod +x "$platform_stub/uname"
    sentinel="$(mktemp "$fixture_dir/platform-all-cargo.XXXXXX")"; benchmark_log="$(mktemp "$fixture_dir/platform-benchmark.XXXXXX")"; before="$(mktemp "$fixture_dir/platform-root-inventory.XXXXXX")"; runner_inventory_snapshot "$root" "$before"
    set +e
    (cd "$fixture_dir" && env PATH="$platform_stub:$stub_dir:$PATH" S1Q_SELF_TEST_MODE=1 FAKE_CARGO_ALL_INVOCATIONS="$sentinel" FAKE_CARGO_INVOCATIONS="$benchmark_log" \
        "$script_path" capture --label platform --probe store-fixture --repeat 1 --threads 1 --external-output-root "$root" --out "$root/platform.json") >/dev/null 2>&1
    status=$?; set -e
    assert_before_work_disposition "$status" "$sentinel" "$benchmark_log" "$root/platform.json" "non-Linux platform"
    runner_inventory_assert "$root" "$before"
    for fault in python3 flock procfs O_DIRECTORY O_NOFOLLOW O_CLOEXEC link-dir-fd stat-dir-fd unlink-dir-fd link-follow-symlinks stat-follow-symlinks rmtree-symlink-safe rmtree-dir-fd; do
        expect_external_reject_before_work "$root" "$root/capability-$fault.json" "$fault"
    done

    expect_external_reject_before_work "$external_base/missing-root" "$external_base/missing-root/capture.json"
    node="$(new_external_root)"; chmod 0755 "$node"; expect_external_reject_before_work "$node" "$node/capture.json"
    expect_external_reject_before_work /tmp /tmp/s1q-system-owned-or-wrong-mode.json
    node="$(new_external_root)"; alias_dir="$external_base/root-final-symlink"; ln -s "$node" "$alias_dir"; before="$(mktemp "$fixture_dir/final-symlink-target-inventory.XXXXXX")"; runner_inventory_snapshot "$node" "$before"; expect_external_reject_before_work "$alias_dir" "$alias_dir/capture.json"; runner_inventory_assert "$node" "$before"
    node="$(new_external_root)"; alias_dir="$external_base/canonical-alias"; ln -s "$external_base" "$alias_dir"; alias_root="$alias_dir/$(basename "$node")"
    before="$(mktemp "$fixture_dir/canonical-alias-target-inventory.XXXXXX")"; runner_inventory_snapshot "$node" "$before"; expect_external_reject_before_work "$alias_root" "$alias_root/capture.json"; runner_inventory_assert "$node" "$before"

    node="$(new_external_root)"; mkdir -m 0700 "$node/real"; ln -s "$node/real" "$node/link"; expect_external_reject_before_work "$node" "$node/link/capture.json"
    node="$(new_external_root)"; printf x >"$node/not-directory"; chmod 0600 "$node/not-directory"; expect_external_reject_before_work "$node" "$node/not-directory/capture.json"
    node="$(new_external_root)"; mkdir -m 0755 "$node/wrong-mode"; expect_external_reject_before_work "$node" "$node/wrong-mode/capture.json"

    node="$(new_external_root)"; out="$node/existing.json"; printf baseline >"$out"; chmod 0600 "$out"; before="$(node_fingerprint "$out")"; expect_external_reject_before_work "$node" "$out"; oracle_equal "$before" "$(node_fingerprint "$out")" "preexisting regular identity"
    node="$(new_external_root)"; out="$node/existing-dir"; mkdir -m 0700 "$out"; before="$(node_fingerprint "$out")"; expect_external_reject_before_work "$node" "$out"; oracle_equal "$before" "$(node_fingerprint "$out")" "preexisting directory identity"
    node="$(new_external_root)"; out="$node/existing-fifo"; mkfifo -m 0600 "$out"; before="$(node_fingerprint "$out")"; expect_external_reject_before_work "$node" "$out"; oracle_equal "$before" "$(node_fingerprint "$out")" "preexisting FIFO identity"
    node="$(new_external_root)"; source_node="$node/source"; out="$node/existing-hardlink.json"; printf baseline >"$source_node"; chmod 0600 "$source_node"; ln "$source_node" "$out"; before="$(node_fingerprint "$out")"; expect_external_reject_before_work "$node" "$out"; oracle_equal "$before" "$(node_fingerprint "$out")" "preexisting hard-link identity"
    node="$(new_external_root)"; out="$node/existing-socket"; python3 - "$out" <<'PY'
import socket
import sys
s = socket.socket(socket.AF_UNIX)
s.bind(sys.argv[1])
s.close()
PY
    before="$(node_fingerprint "$out")"; expect_external_reject_before_work "$node" "$out"; oracle_equal "$before" "$(node_fingerprint "$out")" "preexisting socket identity"
    node="$(new_external_root)"; target_node="$node/live-target"; printf baseline >"$target_node"; chmod 0600 "$target_node"; out="$node/live-symlink.json"; ln -s "$target_node" "$out"; before="$(node_fingerprint "$target_node")"; expect_external_reject_before_work "$node" "$out"; oracle_symlink "$out" "initial live symlink"; oracle_equal "$before" "$(node_fingerprint "$target_node")" "initial live-symlink target"
    node="$(new_external_root)"; out="$node/dangling-symlink.json"; ln -s "$node/missing" "$out"; expect_external_reject_before_work "$node" "$out"; oracle_symlink "$out" "initial dangling symlink"
    node="$(new_external_root)"; target_node="$node/directory-target"; mkdir -m 0700 "$target_node"; out="$node/directory-symlink.json"; ln -s "$target_node" "$out"; before="$(node_fingerprint "$target_node")"; expect_external_reject_before_work "$node" "$out"; oracle_symlink "$out" "initial directory symlink"; oracle_equal "$before" "$(node_fingerprint "$target_node")" "initial directory-symlink target"

    root="$(new_external_root)"; out="$root/direct-green.json"; sentinel="$fixture_dir/direct-green-sentinel"; all_cargo_log="$fixture_dir/direct-green-all-cargo"; rm -f "$sentinel" "$all_cargo_log"
    before="$(mktemp "$fixture_dir/direct-green-inventory.XXXXXX")"; runner_inventory_snapshot "$root" "$before"
    set +e
    (cd "$fixture_dir" && env PATH="$stub_dir:$PATH" S1Q_SELF_TEST_MODE=1 FAKE_CARGO_INVOCATIONS="$sentinel" FAKE_CARGO_ALL_INVOCATIONS="$all_cargo_log" \
        "$script_path" capture --label external-direct --probe store-fixture --repeat 1 --threads 1 --external-output-root "$root" --out "$out")
    status=$?; set -e
    assert_success_disposition 0 "$status" "$all_cargo_log" "$sentinel" "$out" "direct external success"
    assert_external_capture_custody "$root" "$out"
    runner_inventory_assert_success "$root" "$out" "$before" external-direct green

    control_root="$(new_external_root)"; control_out="$control_root/direct-green.json"
    control_baseline="$(mktemp "$fixture_dir/empty-sample-control-baseline.XXXXXX")"; runner_inventory_snapshot "$control_root" "$control_baseline"
    prepare_success_cardinality_control "$root" "$out" "$control_root" empty
    if runner_inventory_assert_success "$control_root" "$control_out" "$control_baseline" external-direct green 2>/dev/null; then
        oracle_failure "status-zero empty-sample success oracle control was accepted"
    fi

    control_root="$(new_external_root)"; control_out="$control_root/direct-green.json"
    control_baseline="$(mktemp "$fixture_dir/extra-sample-control-baseline.XXXXXX")"; runner_inventory_snapshot "$control_root" "$control_baseline"
    prepare_success_cardinality_control "$root" "$out" "$control_root" extra
    if runner_inventory_assert_success "$control_root" "$control_out" "$control_baseline" external-direct green 2>/dev/null; then
        oracle_failure "extra protocol-shaped sample-2 success oracle control was accepted"
    fi

    root="$(new_external_root)"; out="$root/one/two/nested-green.json"; sentinel="$fixture_dir/nested-green-sentinel"; all_cargo_log="$fixture_dir/nested-green-all-cargo"; rm -f "$sentinel" "$all_cargo_log"
    before="$(mktemp "$fixture_dir/nested-green-inventory.XXXXXX")"; runner_inventory_snapshot "$root" "$before"
    set +e
    (cd "$fixture_dir" && env PATH="$stub_dir:$PATH" S1Q_SELF_TEST_MODE=1 FAKE_CARGO_INVOCATIONS="$sentinel" FAKE_CARGO_ALL_INVOCATIONS="$all_cargo_log" "$script_path" capture --label external-nested --probe store-fixture --repeat 1 --threads 1 --external-output-root "$root" --out "$out")
    status=$?; set -e
    assert_success_disposition 0 "$status" "$all_cargo_log" "$sentinel" "$out" "nested external success"
    oracle_equal 700 "$(stat -c %a "$root/one")" "nested level one mode"
    oracle_equal 700 "$(stat -c %a "$root/one/two")" "nested level two mode"
    assert_external_capture_custody "$root" "$out"
    runner_inventory_assert_success "$root" "$out" "$before" external-nested green

    root="$(new_external_root)"; out="$root/intentional-red.json"; sentinel="$fixture_dir/intentional-red-sentinel"; all_cargo_log="$fixture_dir/intentional-red-all-cargo"; rm -f "$sentinel" "$all_cargo_log"
    before="$(mktemp "$fixture_dir/intentional-red-inventory.XXXXXX")"; runner_inventory_snapshot "$root" "$before"
    set +e
    (cd "$fixture_dir" && env PATH="$stub_dir:$PATH" S1Q_SELF_TEST_MODE=1 FAKE_CARGO_FAIL=1 FAKE_CARGO_INVOCATIONS="$sentinel" FAKE_CARGO_ALL_INVOCATIONS="$all_cargo_log" "$script_path" capture --label external-red --probe store-fixture --repeat 1 --threads 1 --external-output-root "$root" --out "$out") >/dev/null 2>&1
    status=$?; set -e
    assert_success_disposition 1 "$status" "$all_cargo_log" "$sentinel" "$out" "intentional-red retained evidence"
    jq -e '.schema_version == 2 and .capture.warmup.exit_status == 1 and (.samples | length == 0)' "$out" >/dev/null
    assert_external_capture_custody "$root" "$out"; runner_inventory_assert_success "$root" "$out" "$before" external-red warmup-red

    adversary="$(mktemp -d "$external_base/adversary.XXXXXX")"; root="$(new_external_root)"; out="$root/descriptor-race.json"
    baseline_root="$(mktemp "$fixture_dir/descriptor-root-baseline.XXXXXX")"; baseline_adversary="$(mktemp "$fixture_dir/descriptor-adversary-baseline.XXXXXX")"
    runner_inventory_snapshot "$root" "$baseline_root"; runner_inventory_snapshot "$adversary" "$baseline_adversary"
    start_hooked_capture "$root" "$out" after-fds descriptor-race
    runner_inventory_assert "$root" "$baseline_root"
    runner_inventory_assert "$adversary" "$baseline_adversary"
    moved_path="$adversary/moved-root"; mv "$root" "$moved_path"; mkdir -m 0700 "$root"
    root_expected="$hook_dir/root.expected"; adversary_expected="$hook_dir/adversary.expected"
    runner_inventory_record_root_relocation "$root" "$baseline_root" "$adversary" "$baseline_adversary" moved-root "$root_expected" "$adversary_expected"
    finish_hooked_capture
    assert_before_work_disposition "$hook_result" "$hook_all_cargo_log" "$hook_sentinel" "$out" "root descriptor substitution"
    runner_inventory_assert "$root" "$root_expected"
    runner_inventory_assert "$adversary" "$adversary_expected"

    root="$(new_external_root)"; out="$root/staging-mode-race.json"
    baseline_root="$(mktemp "$fixture_dir/staging-mode-baseline.XXXXXX")"; runner_inventory_snapshot "$root" "$baseline_root"
    start_hooked_capture "$root" "$out" before-work staging-mode
    runner_state="$hook_dir/runner-state.json"; runner_inventory_record_state "$root" "$out" "$baseline_root" before-work "$runner_state"
    private_leaf="$(runner_leaf "$root" .capture-json.)"; private_path="$root/$private_leaf"; private_identity="$(stat -Lc '%d:%i' "$private_path")"
    chmod 0644 "$private_path"
    runner_inventory_assert_mutation "$root" "$baseline_root" "$runner_state" "$private_leaf" mode 0644
    finish_hooked_capture
    assert_before_work_disposition "$hook_result" "$hook_all_cargo_log" "$hook_sentinel" "$out" "wrong-mode staging node"
    runner_inventory_assert_clean "$root" "$baseline_root"
    runner_inventory_assert_state_absent "$runner_state" "$root"

    lock_root="$(new_external_root)"; lock_out_one="$lock_root/lock-first.json"; lock_out_two="$lock_root/lock-second.json"; lock_out_three="$lock_root/lock-reacquired.json"
    before="$(mktemp "$fixture_dir/lock-first-inventory.XXXXXX")"; runner_inventory_snapshot "$lock_root" "$before"
    start_hooked_capture "$lock_root" "$lock_out_one" before-work lock-first
    runner_state="$hook_dir/runner-state.json"; runner_inventory_record_state "$lock_root" "$lock_out_one" "$before" before-work "$runner_state"
    oracle_empty_log "$hook_sentinel" "first lock holder benchmark-before-work"
    oracle_empty_log "$hook_all_cargo_log" "first lock holder all-Cargo-before-work"
    printf unexpected >"$lock_root/unexpected-runner-file"; chmod 0600 "$lock_root/unexpected-runner-file"
    if runner_inventory_assert_current "$lock_root" "$before" "$runner_state" 2>/dev/null; then
        oracle_failure "unexpected-root-regular negative control was accepted"
    fi
    rm "$lock_root/unexpected-runner-file"
    workspace_leaf="$(runner_leaf "$lock_root" .capture-workspace.)"
    printf unexpected >"$lock_root/$workspace_leaf/unexpected-runner-file"; chmod 0600 "$lock_root/$workspace_leaf/unexpected-runner-file"
    if runner_inventory_assert_current "$lock_root" "$before" "$runner_state" 2>/dev/null; then
        oracle_failure "unexpected-workspace-regular negative control was accepted"
    fi
    rm "$lock_root/$workspace_leaf/unexpected-runner-file"
    runner_inventory_assert_current "$lock_root" "$before" "$runner_state"
    all_cargo_log="$(mktemp "$fixture_dir/lock-second-all-cargo.XXXXXX")"; benchmark_log="$(mktemp "$fixture_dir/lock-second-benchmark.XXXXXX")"
    set +e
    (cd "$fixture_dir" && env PATH="$stub_dir:$PATH" S1Q_SELF_TEST_MODE=1 FAKE_CARGO_ALL_INVOCATIONS="$all_cargo_log" FAKE_CARGO_INVOCATIONS="$benchmark_log" \
        "$script_path" capture --label lock-second --probe store-fixture --repeat 1 --threads 1 \
        --external-output-root "$lock_root" --out "$lock_out_two") >/dev/null 2>&1
    status=$?; set -e
    assert_before_work_disposition "$status" "$all_cargo_log" "$benchmark_log" "$lock_out_two" "second conforming lock writer"
    runner_inventory_assert_current "$lock_root" "$before" "$runner_state"
    finish_hooked_capture
    assert_success_disposition 0 "$hook_result" "$hook_all_cargo_log" "$hook_sentinel" "$lock_out_one" "first lock holder success"
    assert_external_capture_custody "$lock_root" "$lock_out_one"
    runner_inventory_assert_success "$lock_root" "$lock_out_one" "$before" lock-first green
    sentinel="$fixture_dir/lock-reacquired-sentinel"; all_cargo_log="$fixture_dir/lock-reacquired-all-cargo"; rm -f "$sentinel" "$all_cargo_log"
    after="$(mktemp "$fixture_dir/lock-reacquired-inventory.XXXXXX")"; runner_inventory_snapshot "$lock_root" "$after"
    set +e
    (cd "$fixture_dir" && env PATH="$stub_dir:$PATH" S1Q_SELF_TEST_MODE=1 FAKE_CARGO_INVOCATIONS="$sentinel" FAKE_CARGO_ALL_INVOCATIONS="$all_cargo_log" \
        "$script_path" capture --label lock-reacquired --probe store-fixture --repeat 1 --threads 1 \
        --external-output-root "$lock_root" --out "$lock_out_three")
    status=$?; set -e
    assert_success_disposition 0 "$status" "$all_cargo_log" "$sentinel" "$lock_out_three" "reacquired lock success"
    assert_external_capture_custody "$lock_root" "$lock_out_three"
    runner_inventory_assert_success "$lock_root" "$lock_out_three" "$after" lock-reacquired green

    # Late source drift is detected after fake work but before exact-leaf link.
    root="$(new_external_root)"; out="$root/source-drift.json"
    baseline_root="$(mktemp "$fixture_dir/source-drift-baseline.XXXXXX")"; runner_inventory_snapshot "$root" "$baseline_root"
    start_hooked_capture "$root" "$out" before-publish source-drift
    runner_state="$hook_dir/runner-state.json"; runner_inventory_record_state "$root" "$out" "$baseline_root" before-publish "$runner_state"
    private_leaf="$(runner_leaf "$root" .capture-json.)"; private_identity="$(stat -Lc '%d:%i' "$root/$private_leaf")"
    printf changed >"$fixture_dir/tracked.txt"
    runner_inventory_assert_current "$root" "$baseline_root" "$runner_state"
    finish_hooked_capture
    assert_late_reject_disposition "$hook_result" "$hook_all_cargo_log" "$hook_sentinel" "late source drift"
    oracle_absent "$out" "late source-drift output"
    runner_inventory_assert_clean "$root" "$baseline_root"
    runner_inventory_assert_state_absent "$runner_state" "$root"
    git -C "$fixture_dir" restore tracked.txt

    # Live, dangling, and directory-target destination symlinks all reach the
    # sole descriptor-relative link and fail with exact EEXIST semantics.
    root="$(new_external_root)"; out="$root/late-live-symlink.json"; target_node="$root/live-target"; printf baseline >"$target_node"; chmod 0600 "$target_node"; before="$(node_fingerprint "$target_node")"
    baseline_root="$(mktemp "$fixture_dir/late-live-baseline.XXXXXX")"; runner_inventory_snapshot "$root" "$baseline_root"
    start_hooked_capture "$root" "$out" before-publish late-live-symlink
    runner_state="$hook_dir/runner-state.json"; runner_inventory_record_state "$root" "$out" "$baseline_root" before-publish "$runner_state"
    private_leaf="$(runner_leaf "$root" .capture-json.)"; private_identity="$(stat -Lc '%d:%i' "$root/$private_leaf")"; ln -s "$target_node" "$out"
    additions="$hook_dir/named-additions.json"; runner_inventory_record_additions "$root" "$baseline_root" "$runner_state" "$additions" "${out##*/}:symlink"
    finish_hooked_capture
    assert_late_reject_disposition "$hook_result" "$hook_all_cargo_log" "$hook_sentinel" "late live destination symlink"
    oracle_symlink "$out" "late live destination symlink"
    oracle_contains 'File exists' "$hook_dir/stderr" "late live destination diagnostic"
    oracle_equal "$before" "$(node_fingerprint "$target_node")" "late live-symlink target"
    runner_inventory_assert_clean "$root" "$baseline_root" "$additions"
    runner_inventory_assert_state_absent "$runner_state" "$root"

    root="$(new_external_root)"; out="$root/late-dangling-symlink.json"
    baseline_root="$(mktemp "$fixture_dir/late-dangling-baseline.XXXXXX")"; runner_inventory_snapshot "$root" "$baseline_root"
    start_hooked_capture "$root" "$out" before-publish late-dangling-symlink
    runner_state="$hook_dir/runner-state.json"; runner_inventory_record_state "$root" "$out" "$baseline_root" before-publish "$runner_state"
    private_leaf="$(runner_leaf "$root" .capture-json.)"; private_identity="$(stat -Lc '%d:%i' "$root/$private_leaf")"; ln -s "$root/missing" "$out"
    additions="$hook_dir/named-additions.json"; runner_inventory_record_additions "$root" "$baseline_root" "$runner_state" "$additions" "${out##*/}:symlink"
    finish_hooked_capture
    assert_late_reject_disposition "$hook_result" "$hook_all_cargo_log" "$hook_sentinel" "late dangling destination symlink"
    oracle_symlink "$out" "late dangling destination symlink"
    oracle_contains 'File exists' "$hook_dir/stderr" "late dangling destination diagnostic"
    runner_inventory_assert_clean "$root" "$baseline_root" "$additions"
    runner_inventory_assert_state_absent "$runner_state" "$root"

    root="$(new_external_root)"; out="$root/late-directory-symlink.json"; target_node="$root/directory-target"; mkdir -m 0700 "$target_node"; before="$(node_fingerprint "$target_node")"
    baseline_root="$(mktemp "$fixture_dir/late-directory-baseline.XXXXXX")"; runner_inventory_snapshot "$root" "$baseline_root"
    start_hooked_capture "$root" "$out" before-publish late-directory-symlink
    runner_state="$hook_dir/runner-state.json"; runner_inventory_record_state "$root" "$out" "$baseline_root" before-publish "$runner_state"
    private_leaf="$(runner_leaf "$root" .capture-json.)"; private_identity="$(stat -Lc '%d:%i' "$root/$private_leaf")"; ln -s "$target_node" "$out"
    additions="$hook_dir/named-additions.json"; runner_inventory_record_additions "$root" "$baseline_root" "$runner_state" "$additions" "${out##*/}:symlink"
    finish_hooked_capture
    assert_late_reject_disposition "$hook_result" "$hook_all_cargo_log" "$hook_sentinel" "late directory destination symlink"
    oracle_symlink "$out" "late directory destination symlink"
    oracle_contains 'File exists' "$hook_dir/stderr" "late directory destination diagnostic"
    oracle_equal "$before" "$(node_fingerprint "$target_node")" "late directory-symlink target"
    runner_inventory_assert_clean "$root" "$baseline_root" "$additions"
    runner_inventory_assert_state_absent "$runner_state" "$root"

    root="$(new_external_root)"; out="$root/late-eexist.json"
    baseline_root="$(mktemp "$fixture_dir/late-eexist-baseline.XXXXXX")"; runner_inventory_snapshot "$root" "$baseline_root"
    start_hooked_capture "$root" "$out" before-publish late-eexist
    runner_state="$hook_dir/runner-state.json"; runner_inventory_record_state "$root" "$out" "$baseline_root" before-publish "$runner_state"
    private_leaf="$(runner_leaf "$root" .capture-json.)"; private_identity="$(stat -Lc '%d:%i' "$root/$private_leaf")"; printf competitor >"$out"; chmod 0600 "$out"; before="$(node_fingerprint "$out")"
    additions="$hook_dir/named-additions.json"; runner_inventory_record_additions "$root" "$baseline_root" "$runner_state" "$additions" "${out##*/}:regular"
    finish_hooked_capture
    assert_late_reject_disposition "$hook_result" "$hook_all_cargo_log" "$hook_sentinel" "late regular destination competitor"
    oracle_equal "$before" "$(node_fingerprint "$out")" "late destination competitor"
    oracle_contains 'File exists' "$hook_dir/stderr" "late destination competitor diagnostic"
    runner_inventory_assert_clean "$root" "$baseline_root" "$additions"
    runner_inventory_assert_state_absent "$runner_state" "$root"

    # A renamed saved parent remains the cleanup capability even when its old
    # path is replaced by an unrelated directory or a symlink.
    adversary="$(mktemp -d "$external_base/adversary.XXXXXX")"; root="$(new_external_root)"; parent_path="$root/nested"; out="$parent_path/parent-directory-race.json"
    mkdir -m 0700 "$parent_path"
    baseline_root="$(mktemp "$fixture_dir/parent-directory-root-baseline.XXXXXX")"; baseline_parent="$(mktemp "$fixture_dir/parent-directory-parent-baseline.XXXXXX")"; baseline_adversary="$(mktemp "$fixture_dir/parent-directory-adversary-baseline.XXXXXX")"
    runner_inventory_snapshot "$root" "$baseline_root"; runner_inventory_snapshot "$parent_path" "$baseline_parent"; runner_inventory_snapshot "$adversary" "$baseline_adversary"
    start_hooked_capture "$root" "$out" before-publish parent-directory-race
    runner_state="$hook_dir/runner-state.json"; runner_inventory_record_state "$parent_path" "$out" "$baseline_parent" before-publish "$runner_state"
    runner_inventory_assert_prefixed_state "$root" "$baseline_root" "$runner_state" nested
    private_leaf="$(runner_leaf "$parent_path" .capture-json.)"; private_identity="$(stat -Lc '%d:%i' "$parent_path/$private_leaf")"; moved_parent="$adversary/moved-parent"; mv "$parent_path" "$moved_parent"; mkdir -m 0700 "$parent_path"; printf baseline >"$parent_path/competitor"; chmod 0600 "$parent_path/competitor"; before="$(node_fingerprint "$parent_path/competitor")"
    root_expected="$hook_dir/root.expected"; adversary_expected="$hook_dir/adversary.expected"
    runner_inventory_record_parent_transition "$root" "$baseline_root" nested "$baseline_parent" "$runner_state" \
        "$adversary" "$baseline_adversary" moved-parent "$root_expected" "$adversary_expected" \
        "nested:directory" "nested/competitor:regular"
    finish_hooked_capture
    assert_late_reject_disposition "$hook_result" "$hook_all_cargo_log" "$hook_sentinel" "saved-parent directory replacement"
    oracle_absent "$out" "saved-parent directory replacement output"
    oracle_equal "$before" "$(node_fingerprint "$parent_path/competitor")" "saved-parent directory competitor"
    runner_inventory_assert "$root" "$root_expected"
    runner_inventory_assert "$adversary" "$adversary_expected"
    runner_inventory_assert_state_absent "$runner_state" "$root" "$adversary"

    adversary="$(mktemp -d "$external_base/adversary.XXXXXX")"; root="$(new_external_root)"; parent_path="$root/nested"; out="$parent_path/parent-symlink-race.json"
    mkdir -m 0700 "$parent_path"; competitor_dir="$adversary/competitor-parent"; mkdir -m 0700 "$competitor_dir"; printf baseline >"$competitor_dir/competitor"; chmod 0600 "$competitor_dir/competitor"; before="$(node_fingerprint "$competitor_dir/competitor")"
    baseline_root="$(mktemp "$fixture_dir/parent-symlink-root-baseline.XXXXXX")"; baseline_parent="$(mktemp "$fixture_dir/parent-symlink-parent-baseline.XXXXXX")"; baseline_adversary="$(mktemp "$fixture_dir/parent-symlink-adversary-baseline.XXXXXX")"
    runner_inventory_snapshot "$root" "$baseline_root"; runner_inventory_snapshot "$parent_path" "$baseline_parent"; runner_inventory_snapshot "$adversary" "$baseline_adversary"
    start_hooked_capture "$root" "$out" before-publish parent-symlink-race
    runner_state="$hook_dir/runner-state.json"; runner_inventory_record_state "$parent_path" "$out" "$baseline_parent" before-publish "$runner_state"
    runner_inventory_assert_prefixed_state "$root" "$baseline_root" "$runner_state" nested
    private_leaf="$(runner_leaf "$parent_path" .capture-json.)"; private_identity="$(stat -Lc '%d:%i' "$parent_path/$private_leaf")"; moved_parent="$adversary/moved-parent"; mv "$parent_path" "$moved_parent"; ln -s "$competitor_dir" "$parent_path"
    root_expected="$hook_dir/root.expected"; adversary_expected="$hook_dir/adversary.expected"
    runner_inventory_record_parent_transition "$root" "$baseline_root" nested "$baseline_parent" "$runner_state" \
        "$adversary" "$baseline_adversary" moved-parent "$root_expected" "$adversary_expected" "nested:symlink"
    finish_hooked_capture
    assert_late_reject_disposition "$hook_result" "$hook_all_cargo_log" "$hook_sentinel" "saved-parent symlink replacement"
    oracle_symlink "$parent_path" "saved-parent replacement symlink"
    oracle_absent "$out" "saved-parent symlink replacement output"
    oracle_equal "$before" "$(node_fingerprint "$competitor_dir/competitor")" "saved-parent symlink competitor"
    runner_inventory_assert "$root" "$root_expected"
    runner_inventory_assert "$adversary" "$adversary_expected"
    runner_inventory_assert_state_absent "$runner_state" "$root" "$adversary"

    # Each post-link fence is exercised separately; cleanup removes both link
    # names only when they still resolve to the saved private identity.
    root="$(new_external_root)"; out="$root/post-link-source.json"
    baseline_root="$(mktemp "$fixture_dir/post-link-source-baseline.XXXXXX")"; runner_inventory_snapshot "$root" "$baseline_root"
    start_hooked_capture "$root" "$out" after-link post-link-source
    runner_state="$hook_dir/runner-state.json"; runner_inventory_record_state "$root" "$out" "$baseline_root" after-link "$runner_state"
    private_leaf="$(runner_leaf "$root" .capture-json.)"; private_identity="$(stat -Lc '%d:%i' "$root/$private_leaf")"; oracle_equal 2 "$(stat -c %h "$root/$private_leaf")" "post-link source private link count"; printf changed >"$fixture_dir/tracked.txt"
    runner_inventory_assert_current "$root" "$baseline_root" "$runner_state"
    finish_hooked_capture
    assert_late_reject_disposition "$hook_result" "$hook_all_cargo_log" "$hook_sentinel" "post-link source mismatch"
    oracle_absent "$out" "post-link source output"
    runner_inventory_assert_clean "$root" "$baseline_root"
    runner_inventory_assert_state_absent "$runner_state" "$root"
    git -C "$fixture_dir" restore tracked.txt

    root="$(new_external_root)"; out="$root/post-link-root.json"
    baseline_root="$(mktemp "$fixture_dir/post-link-root-baseline.XXXXXX")"; runner_inventory_snapshot "$root" "$baseline_root"
    start_hooked_capture "$root" "$out" after-link post-link-root
    runner_state="$hook_dir/runner-state.json"; runner_inventory_record_state "$root" "$out" "$baseline_root" after-link "$runner_state"
    private_leaf="$(runner_leaf "$root" .capture-json.)"; private_identity="$(stat -Lc '%d:%i' "$root/$private_leaf")"; chmod 0755 "$root"
    runner_inventory_assert_mutation "$root" "$baseline_root" "$runner_state" . mode 0755
    finish_hooked_capture
    assert_late_reject_disposition "$hook_result" "$hook_all_cargo_log" "$hook_sentinel" "post-link root mismatch"
    oracle_absent "$out" "post-link root output"
    chmod 0700 "$root"
    runner_inventory_assert_clean "$root" "$baseline_root"
    runner_inventory_assert_state_absent "$runner_state" "$root"

    root="$(new_external_root)"; parent_path="$root/nested"; out="$parent_path/post-link-parent.json"
    mkdir -m 0700 "$parent_path"
    baseline_root="$(mktemp "$fixture_dir/post-link-parent-root-baseline.XXXXXX")"; baseline_parent="$(mktemp "$fixture_dir/post-link-parent-baseline.XXXXXX")"
    runner_inventory_snapshot "$root" "$baseline_root"; runner_inventory_snapshot "$parent_path" "$baseline_parent"
    start_hooked_capture "$root" "$out" after-link post-link-parent
    runner_state="$hook_dir/runner-state.json"; runner_inventory_record_state "$parent_path" "$out" "$baseline_parent" after-link "$runner_state"
    runner_inventory_assert_prefixed_state "$root" "$baseline_root" "$runner_state" nested
    private_leaf="$(runner_leaf "$parent_path" .capture-json.)"; private_identity="$(stat -Lc '%d:%i' "$parent_path/$private_leaf")"; chmod 0755 "$parent_path"
    runner_inventory_assert_mutation "$parent_path" "$baseline_parent" "$runner_state" . mode 0755
    finish_hooked_capture
    assert_late_reject_disposition "$hook_result" "$hook_all_cargo_log" "$hook_sentinel" "post-link parent mismatch"
    oracle_absent "$out" "post-link parent output"
    chmod 0700 "$parent_path"
    runner_inventory_assert_clean "$root" "$baseline_root"
    runner_inventory_assert_state_absent "$runner_state" "$root"

    # Explicit hostile same-UID boundary violations. These injectors ignore the
    # advisory lock. Moved runner identities are reported outside the cleanup
    # oracle and are removed only with the enclosing disposable fixture.
    adversary="$(mktemp -d "$external_base/adversary.XXXXXX")"; printf baseline >"$adversary/baseline"; chmod 0600 "$adversary/baseline"; before="$(node_fingerprint "$adversary/baseline")"; root="$(new_external_root)"; out="$root/hostile-workspace.json"
    baseline_root="$(mktemp "$fixture_dir/hostile-workspace-root-baseline.XXXXXX")"; baseline_adversary="$(mktemp "$fixture_dir/hostile-workspace-adversary-baseline.XXXXXX")"
    runner_inventory_snapshot "$root" "$baseline_root"; runner_inventory_snapshot "$adversary" "$baseline_adversary"
    start_hooked_capture "$root" "$out" before-work hostile-workspace
    runner_state="$hook_dir/runner-state.json"; runner_inventory_record_state "$root" "$out" "$baseline_root" before-work "$runner_state"
    workspace_leaf="$(runner_leaf "$root" .capture-workspace.)"; workspace_path="$root/$workspace_leaf"; workspace_identity="$(stat -Lc '%d:%i' "$workspace_path")"; moved_path="$adversary/moved-workspace"; mv "$workspace_path" "$moved_path"; mkdir -m 0700 "$workspace_path"; printf competitor >"$workspace_path/competitor"; chmod 0600 "$workspace_path/competitor"; competitor_path="$workspace_path/competitor"; after="$(node_fingerprint "$competitor_path")"
    root_expected="$hook_dir/root.expected"; adversary_expected="$hook_dir/adversary.expected"
    runner_inventory_record_hostile_transition "$root" "$baseline_root" "$runner_state" "$adversary" "$baseline_adversary" \
        workspace moved-workspace "$root_expected" "$adversary_expected" \
        "$workspace_leaf:directory" "$workspace_leaf/competitor:regular"
    finish_hooked_capture
    assert_before_work_disposition "$hook_result" "$hook_all_cargo_log" "$hook_sentinel" "$out" "hostile workspace relocation"
    oracle_directory "$moved_path" "hostile moved workspace"
    oracle_equal "$workspace_identity" "$(stat -Lc '%d:%i' "$moved_path")" "hostile moved workspace identity"
    oracle_equal "$after" "$(node_fingerprint "$competitor_path")" "hostile workspace competitor"
    oracle_equal "$before" "$(node_fingerprint "$adversary/baseline")" "hostile workspace adversary baseline"
    runner_inventory_assert "$root" "$root_expected"
    runner_inventory_assert "$adversary" "$adversary_expected"
    runner_inventory_assert_state_absent "$runner_state" "$root"
    assert_no_runner_private "$root"
    echo "self-test: boundary before-work-workspace moved_identity=$workspace_identity outside-cleanup-oracle"

    adversary="$(mktemp -d "$external_base/adversary.XXXXXX")"; printf baseline >"$adversary/baseline"; chmod 0600 "$adversary/baseline"; before="$(node_fingerprint "$adversary/baseline")"; root="$(new_external_root)"; out="$root/hostile-private-before.json"
    baseline_root="$(mktemp "$fixture_dir/hostile-private-before-root-baseline.XXXXXX")"; baseline_adversary="$(mktemp "$fixture_dir/hostile-private-before-adversary-baseline.XXXXXX")"
    runner_inventory_snapshot "$root" "$baseline_root"; runner_inventory_snapshot "$adversary" "$baseline_adversary"
    start_hooked_capture "$root" "$out" before-work hostile-private-before
    runner_state="$hook_dir/runner-state.json"; runner_inventory_record_state "$root" "$out" "$baseline_root" before-work "$runner_state"
    private_leaf="$(runner_leaf "$root" .capture-json.)"; private_path="$root/$private_leaf"; private_identity="$(stat -Lc '%d:%i' "$private_path")"; moved_path="$adversary/moved-private.json"; mv "$private_path" "$moved_path"; printf competitor >"$private_path"; chmod 0600 "$private_path"; after="$(node_fingerprint "$private_path")"
    root_expected="$hook_dir/root.expected"; adversary_expected="$hook_dir/adversary.expected"
    runner_inventory_record_hostile_transition "$root" "$baseline_root" "$runner_state" "$adversary" "$baseline_adversary" \
        private moved-private.json "$root_expected" "$adversary_expected" "$private_leaf:regular"
    finish_hooked_capture
    assert_before_work_disposition "$hook_result" "$hook_all_cargo_log" "$hook_sentinel" "$out" "hostile private relocation before work"
    oracle_equal "$private_identity" "$(stat -Lc '%d:%i' "$moved_path")" "hostile moved private identity before work"
    oracle_equal "$after" "$(node_fingerprint "$private_path")" "hostile private competitor before work"
    oracle_equal 1 "$(stat -c %h "$private_path")" "hostile private competitor link count before work"
    oracle_equal "$before" "$(node_fingerprint "$adversary/baseline")" "hostile private adversary baseline before work"
    runner_inventory_assert "$root" "$root_expected"
    runner_inventory_assert "$adversary" "$adversary_expected"
    runner_inventory_assert_state_absent "$runner_state" "$root"
    assert_no_runner_workspace "$root"
    echo "self-test: boundary before-work-private moved_identity=$private_identity outside-cleanup-oracle"

    adversary="$(mktemp -d "$external_base/adversary.XXXXXX")"; printf baseline >"$adversary/baseline"; chmod 0600 "$adversary/baseline"; before="$(node_fingerprint "$adversary/baseline")"; root="$(new_external_root)"; out="$root/hostile-private-after.json"
    baseline_root="$(mktemp "$fixture_dir/hostile-private-after-root-baseline.XXXXXX")"; baseline_adversary="$(mktemp "$fixture_dir/hostile-private-after-adversary-baseline.XXXXXX")"
    runner_inventory_snapshot "$root" "$baseline_root"; runner_inventory_snapshot "$adversary" "$baseline_adversary"
    start_hooked_capture "$root" "$out" before-publish hostile-private-after
    runner_state="$hook_dir/runner-state.json"; runner_inventory_record_state "$root" "$out" "$baseline_root" before-publish "$runner_state"
    private_leaf="$(runner_leaf "$root" .capture-json.)"; private_path="$root/$private_leaf"; private_identity="$(stat -Lc '%d:%i' "$private_path")"; moved_path="$adversary/moved-private.json"; mv "$private_path" "$moved_path"; printf competitor >"$private_path"; chmod 0600 "$private_path"; after="$(node_fingerprint "$private_path")"
    root_expected="$hook_dir/root.expected"; adversary_expected="$hook_dir/adversary.expected"
    runner_inventory_record_hostile_transition "$root" "$baseline_root" "$runner_state" "$adversary" "$baseline_adversary" \
        private moved-private.json "$root_expected" "$adversary_expected" "$private_leaf:regular"
    finish_hooked_capture
    assert_late_reject_disposition "$hook_result" "$hook_all_cargo_log" "$hook_sentinel" "hostile private relocation after work"
    oracle_absent "$out" "hostile private after-work output"
    oracle_equal "$private_identity" "$(stat -Lc '%d:%i' "$moved_path")" "hostile moved private identity after work"
    oracle_equal "$after" "$(node_fingerprint "$private_path")" "hostile private competitor after work"
    oracle_equal 1 "$(stat -c %h "$private_path")" "hostile private competitor link count after work"
    oracle_equal "$before" "$(node_fingerprint "$adversary/baseline")" "hostile private adversary baseline after work"
    runner_inventory_assert "$root" "$root_expected"
    runner_inventory_assert "$adversary" "$adversary_expected"
    runner_inventory_assert_state_absent "$runner_state" "$root"
    assert_no_runner_workspace "$root"
    echo "self-test: boundary after-work-private moved_identity=$private_identity outside-cleanup-oracle"

    [ "$calibration_cases" -eq "$RETAINED_CALIBRATION_CASES" ] || oracle_failure "retained calibration case count changed: $calibration_cases"
    local custody_closure_cases
    custody_closure_cases="$(custody_closure_self_test "$script_path" "$fixture_dir")"
    positive_integer "$custody_closure_cases" || oracle_failure "custody closure counter is not positive"
    trap - RETURN; rm -rf "$fixture_dir" "$external_base"; if [ "$fixture_target_created" -eq 1 ]; then rmdir "$fixture_target_dir" 2>/dev/null || true; fi
    echo "self-test: PASS retained_calibration_cases=$calibration_cases custody_closure_cases=$custody_closure_cases preexisting_matrix=PASS"
}

main() {
    case "${1:-}" in
        calibrate-baseline) shift; calibrate_baseline "$@" ;;
        capture) shift; capture "$@" ;;
        attest-preflight) shift; attest_preflight "$@" ;;
        generate-baseline) shift; generate_baseline "$@" ;;
        compare) shift; compare "$@" ;;
        check) shift; check "$@" ;;
        --self-test) shift; [ "$#" -eq 0 ] || die "--self-test accepts no arguments"; self_test ;;
        --help|-h|help) usage ;;
        *) usage >&2; [ -n "${1:-}" ] && die "unknown command: $1" || exit 2 ;;
    esac
}
main "$@"
