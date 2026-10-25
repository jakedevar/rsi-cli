#!/usr/bin/env bash
# suite-gate: tap
# #2177, #938, #3454, #465: `scripts/install-desktop.sh` selects its platform path (macOS system
# WebKit and an app bundle, Linux WebKitGTK menu entry and icons), refuses when the toolchain
# is missing, and shares the release-target guard with install-release.sh: a second worktree
# building into the same cargo target cleans the desktop crate the first one built, under its
# own record (`.rsi-desktop-source`) that never disturbs the release install's. Port of the
# retired Python suite scripts/tests/test_install_desktop.py (#366, no Python #358); every
# build is an isolated fake (stub node, npm, cargo, xcrun, pkg-config and uname).
#
# Case map (legacy test -> case):
#   test_mac_installs_using_system_webkit
#       -> "mac installs using system webkit"
#   test_mac_build_only_leaves_the_bundle_in_the_target
#       -> "mac build-only leaves the bundle in the target"
#   test_mac_reports_missing_xcode                    -> "mac reports missing xcode"
#   test_linux_requires_webkitgtk                     -> "linux requires webkitgtk"
#   test_linux_installs_menu_and_icons                -> "linux installs menu and icons"
#   test_a_second_worktree_cleans_the_desktop_crate_the_first_built_in_the_shared_target
#       -> "a second worktree cleans the desktop crate the first built in the shared target"
#   test_a_fresh_target_is_not_cleaned_and_records_its_builder
#       -> "a fresh target is not cleaned and records its builder"
#   test_the_desktop_record_does_not_disturb_the_release_install_record
#       -> "the desktop record does not disturb the release install record"
# Added with the port (#465), negative proofs for the tauri.conf.json reader that replaced the
# interpreter: "a tauri.conf.json without an identifier is refused" and "a bundle's Info.plist
# values come from tauri.conf.json, not defaults" (distinct values, all five keys checked).
# The Info.plist is checked as text (its keys are one per line) and linted with plutil where the
# host has it, instead of a plist library.
source "$(dirname "$0")/lib/tap.sh"

if ! RSI_REPO_GUARD_BIN="$("$SCRIPTS_DIR/repo-guard" --bin)"; then
    echo "Bail out! scripts/repo-guard could not build rsi-repo-guard" >&2
    exit 1
fi
export RSI_REPO_GUARD_BIN

# make_root <name> [tauri.conf.json text]: a checkout of the installer and the desktop files it
# needs, as one worktree.
make_root() {
    local root="$BASE/$1" conf="${2:-}" desktop icon
    local default='{"productName": "RSI Desktop", "identifier": "dev.rsi.desktop", "version": "0.1.0"}'
    mkdir -p "$root/scripts"
    cp "$SCRIPTS_DIR/install-desktop.sh" "$SCRIPTS_DIR/release-target-guard.sh" "$SCRIPTS_DIR/repo-guard-bin.sh" "$root/scripts/" \
        || tap_fail "cannot copy the installer scripts"
    desktop="$root/desktop"
    mkdir -p "$desktop/node_modules" "$desktop/packaging" "$desktop/src-tauri/icons"
    printf '{}' >"$desktop/package-lock.json"
    printf 'Exec=@BIN@\n' >"$desktop/packaging/rsi-desktop.desktop"
    for icon in 32x32.png 128x128.png icon.png icon.icns; do
        printf 'icon:%s' "$icon" >"$desktop/src-tauri/icons/$icon"
    done
    # The macOS bundle step reads the app name, identifier and version from here.
    printf '%s' "${conf:-$default}" >"$desktop/src-tauri/tauri.conf.json"
}

stub() { write_exec "$FAKE_BIN/$1" "#!/bin/sh
$2
"; }

fixture() {
    BASE="$(cd -- "$CASE_DIR" && pwd -P)/base"
    FAKE_BIN="$BASE/bin"
    TARGET="$BASE/target"
    ROOT="$BASE/repo"
    mkdir -p "$FAKE_BIN" "$BASE/home"
    make_root repo "$@"
    write_exec "$TARGET/release/rsi-desktop" $'#!/bin/sh\nexit 0\n'
    stub node "exit 0"
    stub npm 'echo "$*" >> "$BUILD_CALLS"'
    stub cargo "case \"\$1\" in metadata) echo '{\"packages\":[{\"name\":\"rsi-desktop\",\"version\":\"0.1.0\"}],\"target_directory\":\"$TARGET\"}' ;; *) echo \"\$*\" >> \"\$BUILD_CALLS\" ;; esac"
    stub xcrun "exit 0"
    stub pkg-config "exit 1"
}

# run_install <platform> [flags...]: the installer from $INSTALL_ROOT (default the repo) under
# the fixture's environment; sets RUN_CODE/RUN_OUT/RUN_ERR.
run_install() {
    local platform="$1"
    shift
    stub uname "echo $platform"
    run_capture env HOME="$BASE/home" PATH="$FAKE_BIN:/usr/bin:/bin" \
        RSI_DESKTOP_INSTALL_DIR="$BASE/install" RSI_INSTALL_BIN_DIR="$BASE/installed-bin" \
        RSI_DESKTOP_APP_DIR="$BASE/Applications" XDG_DATA_HOME="$BASE/data" BUILD_CALLS="$BASE/calls" \
        RSI_REPO_GUARD_BIN="$RSI_REPO_GUARD_BIN" \
        bash "${INSTALL_ROOT:-$ROOT}/scripts/install-desktop.sh" "$@"
}

build_calls() { [[ -f "$BASE/calls" ]] && cat "$BASE/calls" || true; }
clean_calls() { build_calls | grep '^clean ' || true; }
count_lines() { printf '%s' "$1" | grep -c . || true; }
marker() { cat "$TARGET/release/.rsi-desktop-source"; }

# plist_string <plist> <key>: the <string> of a key written on one line.
plist_string() {
    sed -n "s|.*<key>$2</key><string>\\([^<]*\\)</string>.*|\\1|p" "$1"
}

case_mac_installs_using_system_webkit() {
    fixture
    run_install Darwin --required
    assert_code 0 "install on macOS"
    local link="$BASE/installed-bin/rsi-desktop" app="$BASE/Applications/RSI Desktop.app/Contents"
    assert_eq "$BASE/install/rsi-desktop" "$(cd "$BASE/install" && pwd -P)/rsi-desktop" "the install directory"
    [[ "$(readlink "$link")" == "$BASE/install/rsi-desktop" ]] || tap_fail "the link points at $(readlink "$link")"
    [[ -x "$link" ]] || tap_fail "the link is not executable"
    assert_contains "$(build_calls)" "build --release" "the cargo build"
    assert_contains "$RUN_OUT" "Desktop UI installed:" "the summary"
    assert_file_is "$app/MacOS/rsi-desktop" $'#!/bin/sh\nexit 0\n' "the bundled binary"
    [[ -x "$app/MacOS/rsi-desktop" ]] || tap_fail "the bundled binary is not executable"
    assert_file_is "$app/Resources/icon.icns" "icon:icon.icns" "the bundled icon"
    assert_eq "RSI Desktop" "$(plist_string "$app/Info.plist" CFBundleName)" CFBundleName
    assert_eq "dev.rsi.desktop" "$(plist_string "$app/Info.plist" CFBundleIdentifier)" CFBundleIdentifier
    assert_eq "0.1.0" "$(plist_string "$app/Info.plist" CFBundleShortVersionString)" CFBundleShortVersionString
    assert_eq "rsi-desktop" "$(plist_string "$app/Info.plist" CFBundleExecutable)" CFBundleExecutable
    assert_eq "icon.icns" "$(plist_string "$app/Info.plist" CFBundleIconFile)" CFBundleIconFile
    assert_contains "$RUN_OUT" "$BASE/Applications/RSI Desktop.app" "the app path"
    if command -v plutil >/dev/null 2>&1; then
        must plutil -lint -s "$app/Info.plist"
    fi
}

case_mac_build_only_leaves_the_bundle_in_the_target() {
    fixture
    run_install Darwin --build-only
    assert_code 0 "build-only on macOS"
    local built="$TARGET/release/bundle/macos/RSI Desktop.app/Contents"
    assert_file_is "$built/Resources/icon.icns" "icon:icon.icns" "the bundled icon"
    assert_eq "dev.rsi.desktop" "$(plist_string "$built/Info.plist" CFBundleIdentifier)" CFBundleIdentifier
    assert_contains "$RUN_OUT" "built: $(dirname -- "$built")" "the built bundle"
    assert_no_file "$BASE/Applications" "--build-only installs nothing"
}

case_a_bundles_info_plist_values_come_from_tauri_conf_json_not_defaults() {
    fixture '{"productName": "Other App", "identifier": "org.example.other", "version": "9.8.7"}'
    run_install Darwin --build-only
    assert_code 0 "build-only on macOS"
    local plist="$TARGET/release/bundle/macos/Other App.app/Contents/Info.plist"
    assert_eq "Other App" "$(plist_string "$plist" CFBundleName)" CFBundleName
    assert_eq "Other App" "$(plist_string "$plist" CFBundleDisplayName)" CFBundleDisplayName
    assert_eq "org.example.other" "$(plist_string "$plist" CFBundleIdentifier)" CFBundleIdentifier
    assert_eq "9.8.7" "$(plist_string "$plist" CFBundleShortVersionString)" CFBundleShortVersionString
    assert_eq "9.8.7" "$(plist_string "$plist" CFBundleVersion)" CFBundleVersion
}

case_a_tauri_conf_json_without_an_identifier_is_refused() {
    fixture '{"productName": "RSI Desktop", "version": "0.1.0"}'
    run_install Darwin --build-only
    assert_ne 0 "$RUN_CODE" "a conf without an identifier was accepted"
    assert_contains "$RUN_ERR" "must define productName, identifier and version" "the refusal"
    assert_no_file "$TARGET/release/bundle" "no bundle is written after the refusal"
}

case_mac_reports_missing_xcode() {
    fixture
    stub xcrun "exit 1"
    run_install Darwin --required
    assert_code 1 "install without xcode"
    assert_contains "$RUN_ERR" "xcode-select --install" "the hint"
}

case_linux_requires_webkitgtk() {
    fixture
    run_install Linux --required
    assert_code 1 "install without webkitgtk"
    assert_contains "$RUN_ERR" "webkit2gtk-4.1" "the hint"
}

case_linux_installs_menu_and_icons() {
    fixture
    stub pkg-config "exit 0"
    run_install Linux --required
    assert_code 0 "install on Linux"
    assert_file_is "$BASE/data/applications/rsi-desktop.desktop" "Exec=$BASE/installed-bin/rsi-desktop"$'\n' "the menu entry"
    local size
    for size in 32x32 128x128 512x512; do
        [[ -f "$BASE/data/icons/hicolor/$size/apps/rsi-desktop.png" ]] || tap_fail "no $size icon"
    done
}

case_a_second_worktree_cleans_the_desktop_crate_the_first_built_in_the_shared_target() {
    fixture
    # #2177 (#938): two worktrees, one target. Cargo's mtime freshness would otherwise link the
    # first tree's rsi-desktop into the second tree's binary.
    stub pkg-config "exit 0"
    make_root other-worktree
    mkdir -p "$TARGET/release/.fingerprint"
    local real_root real_other calls
    real_root="$(cd "$ROOT" && pwd -P)"
    real_other="$(cd "$BASE/other-worktree" && pwd -P)"

    run_install Linux --build-only
    assert_code 0 "the first build"
    assert_contains "$(marker)" "worktree=$real_root"$'\n' "the builder record"
    assert_contains "$(marker)" "state=built" "the builder state"
    # The first build found artifacts with no record: its crate is cleaned once.
    assert_eq 1 "$(count_lines "$(clean_calls)")" "clean calls after the first build: $(build_calls)"

    run_install Linux --build-only
    assert_code 0 "the second build from the same worktree"
    assert_eq 1 "$(count_lines "$(clean_calls)")" "the same worktree reuses its own build"

    INSTALL_ROOT="$BASE/other-worktree" run_install Linux --build-only
    assert_code 0 "the build from the other worktree"
    assert_contains "$RUN_ERR" "was last built from $real_root" "the guard says why it cleans"
    calls="$(build_calls)"
    assert_eq 2 "$(count_lines "$(clean_calls)")" "clean calls: $calls"
    case "$(clean_calls | tail -n 1)" in
        "clean --release --manifest-path "*"other-worktree/desktop/src-tauri/Cargo.toml -p rsi-desktop") ;;
        *) tap_fail "the last clean was [$(clean_calls | tail -n 1)]" ;;
    esac
    # The clean precedes the other worktree's build.
    local clean_line build_line
    clean_line="$(printf '%s\n' "$calls" | awk '/^clean / { n = NR } END { print n + 0 }')"
    build_line="$(printf '%s\n' "$calls" | awk '/^build / { n = NR } END { print n + 0 }')"
    [[ "$clean_line" -gt 0 && "$clean_line" -lt "$build_line" ]] || tap_fail "the clean (line $clean_line) does not precede the build (line $build_line): $calls"
    assert_contains "$(marker)" "worktree=$real_other"$'\n' "the builder record after the switch"
    assert_contains "$(marker)" "state=built" "the builder state after the switch"
}

case_a_fresh_target_is_not_cleaned_and_records_its_builder() {
    fixture
    stub pkg-config "exit 0"
    run_install Linux --build-only
    assert_code 0 "the build"
    assert_eq "" "$(clean_calls)" "a fresh target needs no clean"
    assert_contains "$(marker)" "worktree=$(cd "$ROOT" && pwd -P)"$'\n' "the builder record"
}

case_the_desktop_record_does_not_disturb_the_release_install_record() {
    fixture
    # install-release.sh keeps `.rsi-release-source`; the desktop build uses its own key.
    stub pkg-config "exit 0"
    mkdir -p "$TARGET/release/.fingerprint"
    printf 'worktree=/elsewhere\nstate=built\n' >"$TARGET/release/.rsi-release-source"
    run_install Linux --build-only
    assert_code 0 "the build"
    assert_file_is "$TARGET/release/.rsi-release-source" $'worktree=/elsewhere\nstate=built\n' "the release record"
}

run_case "mac installs using system webkit" case_mac_installs_using_system_webkit
run_case "mac build-only leaves the bundle in the target" case_mac_build_only_leaves_the_bundle_in_the_target
run_case "a bundle's Info.plist values come from tauri.conf.json, not defaults" case_a_bundles_info_plist_values_come_from_tauri_conf_json_not_defaults
run_case "a tauri.conf.json without an identifier is refused" case_a_tauri_conf_json_without_an_identifier_is_refused
run_case "mac reports missing xcode" case_mac_reports_missing_xcode
run_case "linux requires webkitgtk" case_linux_requires_webkitgtk
run_case "linux installs menu and icons" case_linux_installs_menu_and_icons
run_case "a second worktree cleans the desktop crate the first built in the shared target" case_a_second_worktree_cleans_the_desktop_crate_the_first_built_in_the_shared_target
run_case "a fresh target is not cleaned and records its builder" case_a_fresh_target_is_not_cleaned_and_records_its_builder
run_case "the desktop record does not disturb the release install record" case_the_desktop_record_does_not_disturb_the_release_install_record
tap_done
