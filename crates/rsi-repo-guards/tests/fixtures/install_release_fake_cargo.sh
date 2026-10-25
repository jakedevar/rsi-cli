#!/bin/bash
# The fake `cargo` of tests/install_release_provenance.rs: one log line per call
# (its arguments, space separated), and a model of the #938 hazard. A workspace
# crate artifact that another worktree compiled is reused (Cargo's mtime freshness
# check passes), and the fake rsid embeds the commit of the worktree whose sources
# built it, like crates/rsid/build.rs.
target="$CARGO_TARGET_DIR"
release="$target/release"
echo "$*" >> "$FAKE_CARGO_LOG"
root="$PWD"
prev=""
for arg in "$@"; do
    if [ "$prev" = --manifest-path ]; then root="$(dirname "$arg")"; fi
    prev="$arg"
done
crate="$release/.fingerprint/stub-0123456789abcdef"
dependency="$release/.fingerprint/serde-fedcba9876543210"
BINS="rsi rsid rsi-rpc rsi-agent-mcp rsi-build-rustc rsi-contract-validate rsi-known-failure rsi-rolling-land rsi-remote rsi-turn-shim rsi-socket-hold"
case "${1:-}" in
# Shaped like real compact `cargo metadata`: each package object starts {"name":"X","version":"V", which is
# what scripts/release-target-guard.sh reads without an interpreter (#465).
metadata)
    printf '{"target_directory": "%s", "packages": [{"name":"stub","version":"0.0.0","id":"stub 0.0.0"}, {"name":"stub-tools","version":"0.0.0","id":"stub-tools 0.0.0"}]}\n' "$target"
    ;;
tree)
    echo "stub v0.0.0 "
    ;;
clean)
    # `cargo clean --release -p <member>`: the members' artifacts and binaries
    # go; third-party dependencies stay.
    if [ -z "${FAKE_CLEAN_IGNORED:-}" ]; then
        rm -rf "$crate"
        for name in $BINS; do
            rm -f "$release/$name"
        done
    fi
    ;;
build)
    if [ -n "${FAKE_BUILD_FAILS:-}" ]; then exit 101; fi
    if [ -n "${FAKE_TOUCH_DURING_BUILD:-}" ]; then
        echo "// edited while cargo ran" >> "$FAKE_TOUCH_DURING_BUILD"
    fi
    wanted=""
    prev=""
    for arg in "$@"; do
        if [ "$prev" = --bin ]; then wanted="$wanted $arg"; fi
        prev="$arg"
    done
    built_root="$root"
    built_sha="$(git -C "$root" rev-parse HEAD 2>/dev/null)"
    if [ -d "$crate" ]; then
        previous_root="$(sed -n 's/^root=//p' "$crate/source")"
        if [ "$previous_root" != "$root" ]; then
            # Another worktree's newer artifact looks fresh: Cargo reuses it.
            built_root="$previous_root"
            built_sha="$(sed -n 's/^sha=//p' "$crate/source")"
        fi
    fi
    mkdir -p "$crate" "$dependency"
    printf 'root=%s\nsha=%s\n' "$built_root" "$built_sha" > "$crate/source"
    for name in $wanted; do
        rm -f "$release/$name"
        if [ "$name" = rsid ]; then
            printf '#!/bin/sh\nif [ "$1" = --build-info ]; then echo "%s 999"; else echo "fake-rsid from %s"; fi\n' \
                "$built_sha" "$built_root" > "$release/$name"
        else
            printf '#!/bin/sh\necho "fake-%s from %s"\n' "$name" "$built_root" > "$release/$name"
        fi
        chmod 755 "$release/$name"
    done
    ;;
esac
