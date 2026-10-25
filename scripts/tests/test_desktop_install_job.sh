#!/usr/bin/env bash
# suite-gate: tap
# The desktop-install job wrapper keeps the install inside a job-owned HOME (#3239).
# Port of the Python suite of the same name (#366).
source "$(dirname "$0")/lib/tap.sh"

# A stand-in for install-desktop.sh: records the environment it was given and, like
# the real installer, writes the binary, the symlink and the data files under HOME.
write_fake_installer() { # <path>
    cat >"$1" <<'SH'
#!/usr/bin/env bash
set -eu
echo "$*" > "$RECORD/args"
for name in HOME XDG_CONFIG_HOME XDG_DATA_HOME XDG_CACHE_HOME XDG_STATE_HOME CARGO_HOME RUSTUP_HOME TMPDIR; do
    echo "$name=${!name-}" >> "$RECORD/env"
done
mkdir -p "$HOME/.rsi/install" "$HOME/.local/bin" "$XDG_DATA_HOME/applications"
echo bin > "$HOME/.rsi/install/rsi-desktop"
chmod 755 "$HOME/.rsi/install/rsi-desktop"
ln -sfn "$HOME/.rsi/install/rsi-desktop" "$HOME/.local/bin/rsi-desktop"
echo entry > "$XDG_DATA_HOME/applications/rsi-desktop.desktop"
exit "${FAKE_INSTALLER_EXIT:-0}"
SH
    chmod +x "$1"
}

setup() {
    BASE="$CASE_DIR"
    mkdir "$BASE/scripts"
    cp "$SCRIPTS_DIR/desktop-install-job.sh" "$BASE/scripts/desktop-install-job.sh"
    write_fake_installer "$BASE/scripts/install-desktop.sh"
    WRAPPER="$BASE/scripts/desktop-install-job.sh"
    REAL_HOME="$BASE/real-home"
    mkdir "$REAL_HOME"
    printf operator >"$REAL_HOME/marker"
    JOB_TMP="$BASE/job-tmp"
    RECORD="$BASE/record"
    mkdir "$JOB_TMP" "$RECORD"
}

# run_wrapper [wrapper args...]: EXTRA_ENV holds extra NAME=value assignments.
run_wrapper() {
    run_capture env -u CARGO_HOME -u RUSTUP_HOME \
        HOME="$REAL_HOME" TMPDIR="$JOB_TMP" RECORD="$RECORD" \
        XDG_DATA_HOME="$REAL_HOME/xdg-data" RSI_INSTALL_BIN_DIR="$REAL_HOME/bin" \
        RSI_DESKTOP_INSTALL_DIR="$REAL_HOME/install" \
        ${EXTRA_ENV:-} bash "$WRAPPER" "$@"
}

recorded() { # <NAME>: the value the installer saw
    sed -n "s/^$1=//p" "$RECORD/env"
}

case_install_runs_required_in_a_job_owned_home_and_cleans_it_up() {
    setup
    local name home scratch
    run_wrapper
    assert_code 0 "exit code (stderr: $RUN_ERR)"
    assert_eq "--required" "$(cat "$RECORD/args")" "installer arguments"
    home="$(recorded HOME)"
    scratch="$(dirname -- "$home")"
    assert_eq "$JOB_TMP" "$(dirname -- "$scratch")" "scratch parent"
    for name in XDG_CONFIG_HOME XDG_DATA_HOME XDG_CACHE_HOME XDG_STATE_HOME; do
        case "$(recorded "$name")" in "$scratch"/*) ;; *) tap_fail "$name is not under the scratch tree" ;; esac
    done
    # Toolchain homes keep their real defaults so cargo and rustup still work.
    assert_eq "$REAL_HOME/.cargo" "$(recorded CARGO_HOME)" "CARGO_HOME"
    assert_eq "$REAL_HOME/.rustup" "$(recorded RUSTUP_HOME)" "RUSTUP_HOME"
    # Nothing reached the operator's home, and the scratch tree is gone.
    assert_dir_only "$REAL_HOME" marker "the operator home"
    assert_no_file "$scratch" "scratch tree"
    assert_dir_empty "$JOB_TMP" "job tmp"
    assert_contains "$RUN_OUT" "desktop-install-job: exit=0"
}

case_explicit_toolchain_homes_are_kept() {
    setup
    EXTRA_ENV="CARGO_HOME=$BASE/cargo RUSTUP_HOME=$BASE/rustup" run_wrapper
    assert_code 0 "exit code (stderr: $RUN_ERR)"
    assert_eq "$BASE/cargo" "$(recorded CARGO_HOME)" "CARGO_HOME"
    assert_eq "$BASE/rustup" "$(recorded RUSTUP_HOME)" "RUSTUP_HOME"
}

case_a_failed_install_keeps_its_exit_code_and_still_cleans_up() {
    setup
    EXTRA_ENV="FAKE_INSTALLER_EXIT=7" run_wrapper
    assert_code 7 "exit code"
    assert_contains "$RUN_OUT" "desktop-install-job: exit=7"
    assert_dir_empty "$JOB_TMP" "job tmp"
    assert_dir_only "$REAL_HOME" marker "the operator home"
}

case_any_argument_is_refused_with_a_typed_code_before_anything_runs() {
    setup
    run_wrapper --build-only
    assert_code 2 "exit code"
    assert_contains "$RUN_ERR" "desktop_install_job_args_refused"
    assert_contains "$RUN_ERR" "desktop/src-tauri"
    assert_no_file "$RECORD/args" "installer record"
    assert_dir_empty "$JOB_TMP" "job tmp"
}

case_the_recipe_and_make_target_are_declared() {
    assert_contains "$(cat "$REPO_ROOT/.rsi/jobs.toml")" "[recipes.desktop-install]"
    assert_contains "$(cat "$REPO_ROOT/.rsi/jobs.toml")" 'target = "desktop-install-job"'
    assert_contains "$(cat "$REPO_ROOT/Makefile")" $'desktop-install-job:\n\t./scripts/desktop-install-job.sh'
}

case_release_install_never_builds_the_desktop() {
    local makefile target recipe forbidden
    makefile="$(cat "$REPO_ROOT/Makefile")"
    for target in release-install release-install-no-restart; do
        # The target's tab-indented recipe lines, up to the first non-recipe line.
        recipe="$(awk -v t="$target:" '$0==t{on=1;next} on&&/^\t/{print;next} on{exit}' "$REPO_ROOT/Makefile")"
        assert_ne "" "$recipe" "the $target recipe"
        assert_contains "$recipe" "./scripts/install-release.sh" "$target"
        for forbidden in install-desktop npm src-tauri desktop; do
            assert_not_contains "$recipe" "$forbidden" "$target must not build the desktop"
        done
    done
    # The explicit desktop paths stay.
    assert_contains "$makefile" $'desktop-install:\n\t./scripts/install-desktop.sh --required'
    assert_contains "$makefile" $'desktop:\n\t./scripts/install-desktop.sh --build-only'
}

case_release_install_docs_do_not_claim_the_desktop() {
    assert_not_contains "$(cat "$REPO_ROOT/AGENTS.md")" "plus the desktop UI"
}

run_case "install runs --required in a job-owned home and cleans it up" case_install_runs_required_in_a_job_owned_home_and_cleans_it_up
run_case "explicit toolchain homes are kept" case_explicit_toolchain_homes_are_kept
run_case "a failed install keeps its exit code and still cleans up" case_a_failed_install_keeps_its_exit_code_and_still_cleans_up
run_case "any argument is refused with a typed code before anything runs" case_any_argument_is_refused_with_a_typed_code_before_anything_runs
run_case "the recipe and make target are declared" case_the_recipe_and_make_target_are_declared
run_case "release install never builds the desktop" case_release_install_never_builds_the_desktop
run_case "release install docs do not claim the desktop" case_release_install_docs_do_not_claim_the_desktop
tap_done
