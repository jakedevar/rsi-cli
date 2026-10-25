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

# shellcheck source=scripts/repo-guard-bin.sh
source "$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)/repo-guard-bin.sh"

# release_guard_tool <operation> [arg...]: run `rsi-repo-guard release-guard <operation>`
# (#465; these were inline interpreter heredocs). Its stdout and exit status are the
# operation's; a build that lacks the subcommand, or none at all, fails closed.
release_guard_tool() {
    # A build through scripts/repo-guard may start a long-lived child (an sccache server): it must
    # not inherit fd 8, which holds the release lock.
    rsid_resolve_repo_guard "" release-guard 8>&- || return 1
    "$RSI_GUARD_BIN" release-guard "$@"
}

# release_guard_lock <target-dir>
#   Hold an exclusive lock on <target-dir>/.rsi-release.lock on fd 8 until
#   release_guard_unlock or exit. flock(2) locks belong to the open file, so the
#   lock taken by the helper below stays held by this shell's fd 8. Run every
#   command that may leave a long-lived child (cargo starts an sccache server)
#   with `8>&-`, so nothing else keeps the lock.
release_guard_lock() {
    mkdir -p "$1"
    exec 8>>"$1/$RELEASE_GUARD_LOCK"
    # The lock is `rsi-repo-guard release-guard lock-fd 8` (#465): flock(2) on the
    # inherited descriptor, waiting (and saying so) when another build holds it.
    release_guard_tool lock-fd 8 || return 1
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
    release_guard_tool links-into "$@"
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
    release_guard_tool kept "$1" "$2" "$RELEASE_GUARD_KEEP_DIR" "${RELEASE_GUARD_LINK_DIRS[@]}"
}

# release_guard_clean_workspace <root> <target-dir>
#   Remove the release artifacts of every workspace member of <root>, after
#   repointing installed links that resolve into them (#2180).
release_guard_clean_workspace() {
    local root="$1" manifest members name
    local -a args=()
    manifest="${RELEASE_GUARD_MANIFEST:-$root/Cargo.toml}"
    release_guard_preserve_links "$2"
    # `cargo metadata --no-deps` lists only the workspace's packages, each object
    # starting `{"name":"X","version":"V"` (dependency entries carry no version
    # there), so the member names come out with grep and sed (#465).
    members="$(cargo metadata --format-version 1 --no-deps --manifest-path "$manifest" 8>&- \
        | grep -o '{"name":"[^"]*","version":"' | sed 's/^{"name":"//; s/","version":"$//')"
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

# release_guard_receipt_target <receipt-file> <override-dir|""> <allowed-dir>...
#   Print the canonical cargo target directory a build receipt may deploy from,
#   before anything reads, locks or executes it (#3513, #3514). The check is the
#   `release-target` guard of crates/rsi-repo-guards (hub #3715, #3716), run through
#   scripts/repo-guard; a guard that cannot be built or run refuses the deploy.
#   The receipt's `target_dir` (bare or under `result`, #282), when the key is
#   present at all, must be a non-empty absolute string (null, a number, a list
#   or an object is refused) and - with symlinks and `..` resolved - equal one
#   <allowed-dir> (the registered release target the build job used, or the
#   caller's own cargo target); the resolved path is the one printed.
#   A non-empty <override-dir> (--target-dir) must itself be absolute and
#   canonical (no `..`, no symlink component), an existing directory, and resolve
#   to one <allowed-dir> too: it never widens the allowed set. It is the only way
#   to deploy a receipt that names no `target_dir` (a job from before #282), and
#   when the receipt names one too, they must agree.
#   Exit 2, printing nothing, for a receipt whose `target_dir` key is absent and
#   no override. Exit 1 with a message naming the field for anything else wrong
#   (missing or unreadable receipt, missing/relative/non-string/not-allowed
#   target_dir, bad override, no guard binary): fail closed.
release_guard_receipt_target() {
    local receipt="$1" override="$2"
    shift 2
    if [[ ! -f "$receipt" ]]; then
        echo "Build receipt $receipt does not exist (#157)." >&2
        return 1
    fi
    local root guard
    root="$(CDPATH='' cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd -P)" || {
        echo "Cannot locate the checkout of $(dirname "${BASH_SOURCE[0]}") to find rsi-repo-guard; refusing to trust the build receipt (#3715)." >&2
        return 1
    }
    # Fail closed (same lookup as every other guard, scripts/repo-guard): a missing
    # or unbuildable guard binary refuses the deploy, it never skips the check. Its
    # exit 2 would read as "receipt names no target_dir" below, so map it to 1 here.
    guard="$("$root/scripts/repo-guard" --bin)" || {
        echo "Cannot run the rsi-repo-guard binary that checks the build receipt's target; refusing to trust $receipt. Build it first: (cd '$root' && cargo build --release -p rsi-repo-guards --bin rsi-repo-guard) (#3715)." >&2
        return 1
    }
    "$guard" release-target "$receipt" "$override" "$@"
}

# release_guard_check_receipt <root> <receipt-file> [rsid-binary]
#   For --link-only --build-receipt: a build job (AgentSubmitJob `build`) leaves
#   no builder record because it runs cargo directly, but the daemon stamps its
#   receipt with the one commit it compiled. Succeed only when <receipt-file>
#   (the job's result JSON, bare or under `result`) is a finished build
#   (exit_code 0, no refusal) of a clean tree whose source_head is this
#   checkout's HEAD, this checkout has no edits or new files since, and, with
#   <rsid-binary>, rsid embeds that commit. Otherwise name the mismatch and fail.
release_guard_check_receipt() {
    local root="$1" receipt="$2" rsid="${3:-}" real head parsed status source_head detail
    local current_tree head_tree
    local remedy="build it here (make release-install), or pass --allow-foreign-build to link it anyway"
    real="$(release_guard_worktree "$root")"
    if [[ ! -f "$receipt" ]]; then
        echo "Build receipt $receipt does not exist (#157)." >&2
        return 1
    fi
    parsed="$(release_guard_tool receipt-status "$receipt")" || {
        echo "Cannot read build receipt $receipt: the receipt helper failed (#157)." >&2
        return 1
    }
    IFS='|' read -r status source_head detail <<<"$parsed"
    case "$status" in
        ok) ;;
        unreadable) echo "Cannot read build receipt $receipt: $detail (#157)." >&2; return 1 ;;
        refused) echo "The build job in $receipt was refused ($detail); $remedy (#157)." >&2; return 1 ;;
        failed) echo "The build job in $receipt did not succeed ($detail); $remedy (#157)." >&2; return 1 ;;
        nohead) echo "The build receipt $receipt carries no source_head, so it proves nothing about the binaries; $remedy (#157)." >&2; return 1 ;;
        dirty) echo "The build job in $receipt compiled a dirty tree (HEAD $source_head) or did not record it; $remedy (#157)." >&2; return 1 ;;
        *) echo "Cannot interpret build receipt $receipt (#157)." >&2; return 1 ;;
    esac
    head="$(release_guard_head "$root")"
    if [[ -z "$head" ]]; then
        echo "$real is not a git checkout, so a build receipt cannot be matched to it; $remedy (#157)." >&2
        return 1
    fi
    if [[ "$source_head" != "$head" ]]; then
        echo "The build job compiled $source_head, but $real is at $head; $remedy (#157)." >&2
        return 1
    fi
    head_tree="$(git -C "$real" rev-parse --verify --quiet "HEAD^{tree}" 2>/dev/null)" || head_tree=""
    current_tree="$(release_guard_source_tree "$root")"
    if [[ -z "$head_tree" || "$current_tree" != "$head_tree" ]]; then
        echo "The sources in $real differ from commit $head (uncommitted edits or new files); $remedy (#157)." >&2
        return 1
    fi
    if [[ -n "$rsid" ]] && ! release_guard_check_sha "$root" "$rsid"; then
        echo "The release rsid does not match the build job's commit; $remedy." >&2
        return 1
    fi
}
