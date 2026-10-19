# shellcheck shell=bash
# Sourced by install-release.sh (#938): a release build never reuses workspace
# crates that another worktree compiled into the same Cargo target directory,
# so the installed binaries are built from exactly this checkout.
#
# Why a shared target is unsafe: Cargo decides whether a workspace (path) crate
# is fresh by comparing its source mtimes with the artifact, and the crate's
# `-C metadata` hash is workspace-relative, so two worktrees building into one
# target (a global `build.target-dir`, a shared CARGO_TARGET_DIR) write the same
# artifact names. A tree whose sources are older than the other tree's last
# build then links the other tree's crates (the laptop's E0432/E0560 "stale
# rsi-common metadata" reds; two trees' rsid builds in one shared target).
#
# The guard keeps a record, `<target>/release/.rsi-release-source`, of the
# worktree that last built there. Before a build from any other worktree (or
# into a release directory with artifacts and no record) it removes every
# workspace member's release artifacts (`cargo clean --release -p <member>`);
# third-party dependencies stay shared, their names hash the registry version.
# After the build it checks the commit rsid embeds (`rsid --build-info`, from
# crates/rsid/build.rs) against this checkout, and `--link-only` refuses to link
# binaries this checkout did not build from its current sources (HEAD plus a
# fingerprint of uncommitted edits and new files, #2179). No cleanup removes a
# file an installed link resolves to (#2180). One lock per target makes the
# check, the build and the copy out of `release/` a single step.

# install-desktop.sh (#2177) sources this for its standalone `desktop/src-tauri`
# workspace and sets its own RELEASE_GUARD_MARKER / RELEASE_GUARD_LOCK /
# RELEASE_GUARD_MANIFEST after sourcing, so the two builds keep separate records in
# a shared target and a cleanup removes only that workspace's members.
RELEASE_GUARD_MARKER=".rsi-release-source"
RELEASE_GUARD_LOCK=".rsi-release.lock"
# Cargo.toml whose workspace members a cleanup removes; empty means <root>/Cargo.toml.
RELEASE_GUARD_MANIFEST=""
# Set by the caller (release_guard_set_link_dirs): directories whose symlinks
# are installed links, and where a cleanup keeps the files they resolve to.
RELEASE_GUARD_LINK_DIRS=()
RELEASE_GUARD_KEEP_DIR=""
# The source fingerprint release_guard_prepare saw before the build.
RELEASE_GUARD_SOURCE_TREE=""

# release_guard_lock <target-dir>
#   Hold an exclusive lock on <target-dir>/.rsi-release.lock on fd 8 until
#   release_guard_unlock or exit. flock(2) locks belong to the open file, so the
#   lock taken by the helper below stays held by this shell's fd 8. Run every
#   command that may leave a long-lived child (cargo starts an sccache server)
#   with `8>&-`, so nothing else keeps the lock.
release_guard_lock() {
    mkdir -p "$1"
    exec 8>>"$1/$RELEASE_GUARD_LOCK"
    python3 - <<'PY'
import fcntl
import sys

try:
    fcntl.flock(8, fcntl.LOCK_EX | fcntl.LOCK_NB)
except BlockingIOError:
    print("Waiting for another release build into this target to finish (#938) ...", file=sys.stderr, flush=True)
    fcntl.flock(8, fcntl.LOCK_EX)
PY
}

release_guard_unlock() {
    exec 8>&-
}

# release_guard_worktree <root>: the checkout's physical path.
release_guard_worktree() {
    (cd "$1" && pwd -P)
}

# release_guard_head <root>: HEAD of the git checkout rooted exactly at <root>;
# empty when <root> is not the top of a checkout (a source tarball).
release_guard_head() {
    local root top
    root="$(release_guard_worktree "$1")"
    top="$(git -C "$root" rev-parse --show-toplevel 2>/dev/null)" || return 0
    [[ "$(cd "$top" && pwd -P)" == "$root" ]] || return 0
    git -C "$root" rev-parse --verify --quiet HEAD 2>/dev/null || true
}

# release_guard_expected_sha <root>: the commit rsid's build script embeds for
# this checkout (a non-blank RSI_BUILD_SHA wins, as in crates/rsid/build.rs).
release_guard_expected_sha() {
    local pinned="${RSI_BUILD_SHA:-}"
    if [[ -n "${pinned//[[:space:]]/}" ]]; then
        printf '%s\n' "${pinned//[[:space:]]/}"
    else
        release_guard_head "$1"
    fi
}

# release_guard_source_tree <root>
#   A git tree id of the checkout's working tree: tracked edits and untracked,
#   non-ignored files included (HEAD's tree when clean). It is written from a
#   scratch copy of the index, so the real index is untouched. Empty when <root>
#   is not the top of a git checkout or git cannot compute it.
release_guard_source_tree() {
    local root index scratch seed tree=""
    root="$(release_guard_worktree "$1")"
    [[ -n "$(release_guard_head "$root")" ]] || return 0
    index="$(git -C "$root" rev-parse --path-format=absolute --git-path index 2>/dev/null)" || index=""
    scratch="$(mktemp -d "${TMPDIR:-/tmp}/rsi-release-index.XXXXXX")" || return 0
    # The copied index lets git rehash only changed files; an index git cannot
    # reuse elsewhere (a split index) falls back to hashing from scratch.
    for seed in copy empty; do
        rm -f "$scratch/index"
        if [[ "$seed" == copy ]]; then
            [[ -n "$index" && -f "$index" ]] || continue
            cp "$index" "$scratch/index" || continue
        fi
        if GIT_INDEX_FILE="$scratch/index" git -C "$root" add -A >/dev/null 2>&1 \
            && tree="$(GIT_INDEX_FILE="$scratch/index" git -C "$root" write-tree 2>/dev/null)"; then
            break
        fi
        tree=""
    done
    rm -rf "$scratch"
    printf '%s\n' "$tree"
}

# release_guard_field <marker> <key>: one recorded value, empty when absent.
release_guard_field() {
    [[ -f "$1" ]] || return 0
    awk -v key="$2" 'index($0, key "=") == 1 { print substr($0, length(key) + 2); exit }' "$1"
}

# release_guard_record <root> <target-dir> <state> [source-tree]
#   Record this worktree as the builder of <target-dir>/release, with its HEAD,
#   a dirty flag, the source fingerprint and the time. <state> is `building`
#   before the build and `built` once it succeeded.
release_guard_record() {
    local root head dirty=unknown release="$2/release" temp
    root="$(release_guard_worktree "$1")"
    head="$(release_guard_head "$1")"
    if [[ -n "$head" ]]; then
        if [[ -n "$(git -C "$root" status --porcelain --untracked-files=no 2>/dev/null)" ]]; then
            dirty=true
        else
            dirty=false
        fi
    fi
    mkdir -p "$release"
    temp="$(mktemp "$release/$RELEASE_GUARD_MARKER.XXXXXX")"
    {
        printf 'worktree=%s\n' "$root"
        printf 'state=%s\n' "$3"
        printf 'head=%s\n' "${head:-unknown}"
        printf 'dirty=%s\n' "$dirty"
        printf 'source_tree=%s\n' "${4:-unknown}"
        printf 'recorded_at=%s\n' "$(date -u +%Y-%m-%dT%H:%M:%SZ)"
    } >"$temp"
    mv -f "$temp" "$release/$RELEASE_GUARD_MARKER"
}

# release_guard_set_link_dirs <root> <bin-dir> <keep-dir>
#   Installed links live in <bin-dir> and in each worktree's target/release
#   (install-release.sh's compatibility links); kept files go to <keep-dir>.
release_guard_set_link_dirs() {
    local dir
    RELEASE_GUARD_KEEP_DIR="$3"
    RELEASE_GUARD_LINK_DIRS=()
    while IFS= read -r dir; do
        RELEASE_GUARD_LINK_DIRS+=("$dir")
    done < <(release_guard_link_dir_list "$1" "$2")
}

# release_guard_link_dir_list <root> <bin-dir>
#   Print, one per line, the directories whose symlinks are installed links:
#   <bin-dir>, <root>/target/release and every worktree's target/release.
release_guard_link_dir_list() {
    local worktree
    printf '%s\n' "$2" "$1/target/release"
    while IFS= read -r worktree; do
        [[ -n "$worktree" ]] || continue
        printf '%s\n' "$worktree/target/release"
    done < <(git -C "$1" worktree list --porcelain 2>/dev/null | sed -n 's/^worktree //p')
}

# release_guard_links_into <release-dir> <link-dir>...
#   Print every symlink in the link directories whose file lies in
#   <release-dir>: an installed link a build or cleanup there would change or
#   leave dangling (#262, hub #2801). Read-only; prints nothing when none does.
release_guard_links_into() {
    python3 - "$@" <<'PY'
import os
import sys

release, *link_dirs = sys.argv[1:]
release = os.path.realpath(release)
for directory in dict.fromkeys(link_dirs):
    try:
        entries = sorted(os.scandir(directory), key=lambda entry: entry.name)
    except OSError:
        continue
    for entry in entries:
        if not entry.is_symlink():
            continue
        if os.path.realpath(entry.path).startswith(release + os.sep):
            print(entry.path)
PY
}

# release_guard_preserve_links <target-dir>
#   Before any cleanup of <target-dir>/release (#2180): every symlink in
#   RELEASE_GUARD_LINK_DIRS whose file lies in that release directory (a link an
#   older --tui-only or pre-#1164 install made, a compatibility link) is
#   repointed at a byte-identical copy in RELEASE_GUARD_KEEP_DIR named
#   `<binary>.<sha256 prefix>`, so cleaning never leaves an installed link
#   dangling. Then unreferenced kept copies are pruned (#2188). Any failure
#   stops the cleanup.
release_guard_preserve_links() {
    ((${#RELEASE_GUARD_LINK_DIRS[@]})) || return 0
    [[ -n "$RELEASE_GUARD_KEEP_DIR" && -d "$1/release" ]] || return 0
    release_guard_kept preserve "$1/release"
}

# release_guard_prune_kept
#   Remove every kept copy no installed link resolves to any more (#2188), e.g.
#   after an install repointed its links at ~/.rsi/install.
release_guard_prune_kept() {
    ((${#RELEASE_GUARD_LINK_DIRS[@]})) || return 0
    [[ -n "$RELEASE_GUARD_KEEP_DIR" && -d "$RELEASE_GUARD_KEEP_DIR" ]] || return 0
    release_guard_kept prune ""
}

# release_guard_kept <preserve|prune> <release-dir>
#   One step under an exclusive lock on <keep>/.lock, so no run prunes a copy
#   another run just made but has not linked yet. <keep>/.links records every
#   link this guard ever repointed, so a link in another clone's worktree (not
#   in this run's RELEASE_GUARD_LINK_DIRS) still protects its copy. A kept copy
#   is removed only when neither a scanned link nor a recorded one resolves to it.
release_guard_kept() {
    python3 - "$1" "$2" "$RELEASE_GUARD_KEEP_DIR" "${RELEASE_GUARD_LINK_DIRS[@]}" <<'PY'
import fcntl
import hashlib
import os
import shutil
import sys
import tempfile

mode, release, keep, *link_dirs = sys.argv[1:]
link_dirs = list(dict.fromkeys(link_dirs))
os.makedirs(keep, exist_ok=True)
keep_real = os.path.realpath(keep)
manifest = os.path.join(keep, ".links")


def symlinks(directory):
    try:
        return [entry.path for entry in os.scandir(directory) if entry.is_symlink()]
    except OSError:
        return []


with open(os.path.join(keep, ".lock"), "a") as lock:
    fcntl.flock(lock, fcntl.LOCK_EX)
    try:
        with open(manifest) as recorded_links:
            recorded = {line.rstrip("\n") for line in recorded_links if line.strip()}
    except FileNotFoundError:
        recorded = set()

    if mode == "preserve":
        release = os.path.realpath(release)
        for directory in link_dirs:
            for path in symlinks(directory):
                real = os.path.realpath(path)
                if not real.startswith(release + os.sep) or not os.path.isfile(real):
                    continue
                digest = hashlib.sha256()
                with open(real, "rb") as source:
                    for chunk in iter(lambda: source.read(1 << 20), b""):
                        digest.update(chunk)
                kept = os.path.join(keep, f"{os.path.basename(real)}.{digest.hexdigest()[:16]}")
                if not os.path.isfile(kept):
                    handle, temp = tempfile.mkstemp(dir=keep, prefix=".keep-")
                    os.close(handle)
                    shutil.copyfile(real, temp)
                    os.chmod(temp, 0o755)
                    os.replace(temp, kept)
                link_temp = os.path.join(directory, f".{os.path.basename(path)}.keep-{os.getpid()}")
                try:
                    os.unlink(link_temp)
                except FileNotFoundError:
                    pass
                os.symlink(kept, link_temp)
                os.replace(link_temp, path)
                recorded.add(path)
                print(f"Kept installed link {path} -> {kept} before cleaning {release} (#938).", file=sys.stderr)

    # Prune (#2188): keep exactly the copies a live link resolves to.
    live = {}
    for path in recorded.union(*(symlinks(directory) for directory in link_dirs)):
        if os.path.islink(path):
            real = os.path.realpath(path)
            if os.path.dirname(real) == keep_real and os.path.isfile(real):
                live[path] = real
    referenced = set(live.values())
    for entry in os.scandir(keep):
        if entry.name in (".lock", ".links") or not entry.is_file(follow_symlinks=False):
            continue
        # `.keep-*` are copies a run died writing; every writer holds the lock.
        if entry.name.startswith(".keep-") or os.path.realpath(entry.path) not in referenced:
            try:
                os.unlink(entry.path)
            except OSError as error:  # a failed prune never blocks a cleanup or install
                print(f"warning: cannot prune kept copy {entry.path}: {error}", file=sys.stderr)
                continue
            if not entry.name.startswith("."):
                print(f"Pruned kept copy {entry.path}: no installed link resolves to it (#2188).", file=sys.stderr)
    handle, temp = tempfile.mkstemp(dir=keep, prefix=".links-")
    with os.fdopen(handle, "w") as links:
        links.writelines(f"{path}\n" for path in sorted(live))
    os.replace(temp, manifest)
PY
}

# release_guard_clean_workspace <root> <target-dir>
#   Remove the release artifacts of every workspace member of <root>, after
#   repointing installed links that resolve into them (#2180).
release_guard_clean_workspace() {
    local root="$1" manifest members name
    local -a args=()
    manifest="${RELEASE_GUARD_MANIFEST:-$root/Cargo.toml}"
    release_guard_preserve_links "$2"
    members="$(cargo metadata --format-version 1 --no-deps --manifest-path "$manifest" 8>&- \
        | python3 -c 'import json, sys; print("\n".join(p["name"] for p in json.load(sys.stdin)["packages"]))')"
    while IFS= read -r name; do
        if [[ -n "$name" ]]; then
            args+=(-p "$name")
        fi
    done <<<"$members"
    if ((${#args[@]} == 0)); then
        echo "Cannot list the workspace members of $manifest to clean them (#938)." >&2
        return 1
    fi
    cargo clean --release --manifest-path "$manifest" "${args[@]}" 8>&-
}

# release_guard_prepare <root> <target-dir>
#   Before a release build: when <target-dir>/release holds artifacts this
#   worktree did not build (another worktree's record, or none at all), remove
#   every workspace member's release artifacts. Then record this worktree as the
#   builder (`state=building`), so a failed build is never taken for a good one.
release_guard_prepare() {
    local root="$1" target="$2" real recorded
    real="$(release_guard_worktree "$root")"
    recorded="$(release_guard_field "$target/release/$RELEASE_GUARD_MARKER" worktree)"
    if [[ "$recorded" != "$real" && -d "$target/release/.fingerprint" ]]; then
        if [[ -n "$recorded" ]]; then
            echo "Release target $target was last built from $recorded; removing its workspace crates before building from $real (#938)." >&2
        else
            echo "Release target $target has no record of the worktree that built it; removing its workspace crates before building from $real (#938)." >&2
        fi
        release_guard_clean_workspace "$root" "$target"
    fi
    RELEASE_GUARD_SOURCE_TREE="$(release_guard_source_tree "$root")"
    release_guard_record "$root" "$target" building "$RELEASE_GUARD_SOURCE_TREE"
}

# release_guard_finish <root> <target-dir>
#   After a successful build: record it as `built` with the source fingerprint
#   release_guard_prepare saw. Sources that changed while the build ran are
#   recorded as `changed-during-build`, which --link-only never links (#2179).
release_guard_finish() {
    local root="$1" target="$2" source="$RELEASE_GUARD_SOURCE_TREE" after
    after="$(release_guard_source_tree "$root")"
    if [[ "$after" != "$source" ]]; then
        echo "warning: sources in $(release_guard_worktree "$root") changed while the release build ran; --link-only will not link this build (#938)." >&2
        source=changed-during-build
    fi
    release_guard_record "$root" "$target" built "$source"
}

# release_guard_check_sha <root> <rsid-binary>
#   Succeed when <rsid-binary> embeds the commit this checkout builds, or when
#   no commit is known (not a git checkout); otherwise say why and fail.
release_guard_check_sha() {
    local expected actual
    expected="$(release_guard_expected_sha "$1")"
    [[ -n "$expected" ]] || return 0
    actual="$("$2" --build-info 2>/dev/null | awk 'NR == 1 { print $1 }')" || actual=""
    [[ "$actual" == "$expected" ]] && return 0
    echo "$2 embeds commit ${actual:-<none>}, but $(release_guard_worktree "$1") is at $expected (#938)." >&2
    return 1
}

# release_guard_check_link <root> <target-dir> [rsid-binary]
#   For --link-only: succeed only when this worktree's finished build of its
#   current sources (HEAD and the source fingerprint, so uncommitted edits and
#   new files count, #2179) produced <target-dir>/release, and, with
#   <rsid-binary>, rsid embeds that commit. Otherwise name the mismatch and fail.
release_guard_check_link() {
    local root="$1" target="$2" rsid="${3:-}" marker real recorded state head current
    local built_tree current_tree
    local remedy="build it here (make release-install), or pass --allow-foreign-build to link it anyway"
    marker="$target/release/$RELEASE_GUARD_MARKER"
    real="$(release_guard_worktree "$root")"
    if [[ ! -f "$marker" ]]; then
        echo "No release build from $real is recorded in $target/release; $remedy (#938)." >&2
        return 1
    fi
    recorded="$(release_guard_field "$marker" worktree)"
    state="$(release_guard_field "$marker" state)"
    head="$(release_guard_field "$marker" head)"
    if [[ "$recorded" != "$real" ]]; then
        echo "The release binaries in $target/release were built from $recorded, not $real; $remedy (#938)." >&2
        return 1
    fi
    if [[ "$state" != built ]]; then
        echo "The last release build from $real into $target/release did not finish; $remedy (#938)." >&2
        return 1
    fi
    current="$(release_guard_head "$root")"
    if [[ -n "$current" && "$head" != "$current" ]]; then
        echo "The release binaries in $target/release were built at $head, but $real is now at $current; $remedy (#938)." >&2
        return 1
    fi
    if [[ -n "$current" ]]; then
        built_tree="$(release_guard_field "$marker" source_tree)"
        current_tree="$(release_guard_source_tree "$root")"
        if [[ "$built_tree" == changed-during-build ]]; then
            echo "The sources in $real changed while the last release build ran; $remedy (#2179)." >&2
            return 1
        fi
        if [[ -z "$built_tree" || "$built_tree" == unknown ]]; then
            echo "The last release build from $real recorded no source fingerprint; $remedy (#2179)." >&2
            return 1
        fi
        if [[ -z "$current_tree" ]]; then
            echo "Cannot fingerprint the sources in $real; $remedy (#2179)." >&2
            return 1
        fi
        if [[ "$built_tree" != "$current_tree" ]]; then
            echo "The sources in $real changed since the release build (uncommitted edits or new files: built tree $built_tree, now $current_tree); $remedy (#2179)." >&2
            return 1
        fi
    fi
    if [[ -n "$rsid" ]] && ! release_guard_check_sha "$root" "$rsid"; then
        echo "The release rsid does not match this checkout; $remedy." >&2
        return 1
    fi
}
