#!/usr/bin/env bash
# suite-gate: tap
# build-aws-artifacts.sh packages the binaries cargo built, wherever cargo's target
# is (#2177). Port of the Python suite of the same name (#366).
#
# Case map (legacy test_build_aws_artifacts.py -> here):
#   test_a_global_target_dir_without_the_env_var_is_the_directory_packaged -> same name (Linux x86_64/aarch64 only:
#       the packaging step uses GNU tar flags and sha256sum; SKIP elsewhere, hub #3621 Linux checklist)
#   test_an_unresolvable_target_dir_fails_instead_of_packaging_target     -> same name (RUNS ON EVERY HOST: the
#       script fails at `cargo metadata` before any build, so no Linux tool is needed)
# Added (every host): the interpreter-free `target_directory` reader of the subject is exercised on synthetic
# metadata that omits the field or needs JSON escaping; each must fail closed with its own message.
# Archive members are compared byte for byte (the stub writes `built-by-cargo` plus a newline).
source "$(dirname "$0")/lib/tap.sh"

BINARIES="rsi rsid rsi-rpc rsi-agent-mcp"

# The native cargo builder is only used on a Linux x86_64/aarch64 host.
native_host() {
    [[ "$(uname -s)" == Linux ]] || return 1
    case "$(uname -m)" in x86_64 | aarch64) return 0 ;; *) return 1 ;; esac
}

setup_repo() {
    local machine
    machine="$(uname -m)"
    ARCH=arm64
    [[ "$machine" == x86_64 ]] && ARCH=x86_64
    TRIPLE="$machine-unknown-linux-gnu"
    REPO="$CASE_DIR/repo"
    mkdir -p "$REPO/scripts"
    cp "$SCRIPTS_DIR/build-aws-artifacts.sh" "$SCRIPTS_DIR/check-release-seam.sh" "$REPO/scripts/"
    : >"$REPO/Cargo.toml"
    fixture_repo "$REPO"
    fixture_commit "$REPO" init
    SHA="$(git -C "$REPO" rev-parse HEAD)"
    # Where cargo builds: a global `build.target-dir`, not ./target and not CARGO_TARGET_DIR.
    GLOBAL_TARGET="$CASE_DIR/global-target"
    FAKE_BIN="$CASE_DIR/bin"
    mkdir -p "$FAKE_BIN"
    cat >"$FAKE_BIN/cargo" <<'SH'
#!/bin/sh
case "$1" in
  metadata) printf '{"target_directory": "%s"}\n' "$STUB_TARGET" ;;
  tree) ;;
  build)
    while [ $# -gt 0 ]; do [ "$1" = --target ] && triple=$2; shift; done
    mkdir -p "$STUB_TARGET/$triple/release"
    for bin in rsi rsid rsi-rpc rsi-agent-mcp; do
      echo built-by-cargo > "$STUB_TARGET/$triple/release/$bin"
    done ;;
esac
SH
    chmod +x "$FAKE_BIN/cargo"
    OUTPUT="$CASE_DIR/out"
}

plant_decoy() {
    local name
    mkdir -p "$REPO/target/$TRIPLE/release"
    for name in $BINARIES; do
        echo decoy-from-another-build >"$REPO/target/$TRIPLE/release/$name"
    done
    # A decoy is untracked build output; the checkout must stay clean for the script.
    echo 'target/' >"$REPO/.gitignore"
    git -C "$REPO" add .gitignore
    git -C "$REPO" -c user.name=t -c user.email=t@example.invalid commit -q -m ignore
    SHA="$(git -C "$REPO" rev-parse HEAD)"
}

build() { # <path prefix directory>
    cd "$REPO" || exit 1
    env -u CARGO_TARGET_DIR PATH="$1:$PATH" STUB_TARGET="$GLOBAL_TARGET" \
        bash "$REPO/scripts/build-aws-artifacts.sh" "$SHA" "$OUTPUT" "$ARCH" >"$CASE_DIR/build.out" 2>"$CASE_DIR/build.err"
}

case_a_global_target_dir_without_the_env_var_is_the_directory_packaged() {
    local name
    setup_repo
    plant_decoy
    build "$FAKE_BIN" || tap_fail "build failed: $(cat "$CASE_DIR/build.err")"
    for name in $BINARIES; do
        tar -xOzf "$OUTPUT/rsi-$SHA-$ARCH.tar.gz" "./$name" >"$CASE_DIR/member" || tap_fail "cannot read ./$name from the archive"
        assert_file_is "$CASE_DIR/member" $'built-by-cargo\n' "$name"
    done
}

case_an_unresolvable_target_dir_fails_instead_of_packaging_target() {
    setup_repo
    plant_decoy
    mkdir "$CASE_DIR/broken-bin"
    printf '#!/bin/sh\necho metadata-asked > "%s"\nexit 1\n' "$CASE_DIR/asked" >"$CASE_DIR/broken-bin/cargo"
    chmod +x "$CASE_DIR/broken-bin/cargo"
    build "$CASE_DIR/broken-bin" && tap_fail "the build succeeded although cargo metadata failed"
    # It failed BECAUSE of the metadata call, not for another reason on this host.
    assert_file "$CASE_DIR/asked" "cargo metadata was never asked"
    assert_not_contains "$(cat "$CASE_DIR/build.err")" "cross is required" "the failure came from the build step"
    assert_eq 0 "$(find "$OUTPUT" -name '*.tar.gz' 2>/dev/null | wc -l | tr -d ' ')" "packaged archives"
}

# metadata_case <metadata json> <expected stderr text>: cargo metadata succeeds but yields a
# document the subject cannot use; the build must stop there with that message (every host).
metadata_case() {
    setup_repo
    plant_decoy
    mkdir "$CASE_DIR/odd-bin"
    printf '#!/bin/sh\ncase "$1" in metadata) printf "%%s\\n" "$STUB_METADATA" ;; esac\n' >"$CASE_DIR/odd-bin/cargo"
    chmod +x "$CASE_DIR/odd-bin/cargo"
    STUB_METADATA="$1" build "$CASE_DIR/odd-bin" && tap_fail "the build succeeded on unusable cargo metadata"
    assert_contains "$(cat "$CASE_DIR/build.err")" "$2" "the refusal"
    assert_eq 0 "$(find "$OUTPUT" -name '*.tar.gz' 2>/dev/null | wc -l | tr -d ' ')" "packaged archives"
}

case_metadata_without_a_target_directory_is_refused() {
    metadata_case '{"packages":[],"workspace_members":[],"version":1}' "Cannot resolve the cargo target directory"
}

case_a_target_directory_needing_json_escaping_is_refused() {
    metadata_case '{"target_directory": "/tmp/a\\\\b"}' "needs JSON escaping"
}

run_case "an unresolvable target dir fails instead of packaging ./target" case_an_unresolvable_target_dir_fails_instead_of_packaging_target
run_case "cargo metadata without a target directory is refused" case_metadata_without_a_target_directory_is_refused
run_case "a target directory needing JSON escaping is refused" case_a_target_directory_needing_json_escaping_is_refused
if native_host; then
    run_case "a global target dir without the env var is the directory packaged" case_a_global_target_dir_without_the_env_var_is_the_directory_packaged
else
    skip_linux_only "a global target dir without the env var is the directory packaged" "a Linux x86_64/aarch64 host (GNU tar flags, sha256sum, native cargo builder)"
fi
tap_done
