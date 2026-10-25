#!/usr/bin/env bash
# suite-gate: tap
# Satellite unit templates retry forever, and install-direct.sh protects other instances.
# Port of the Python suite of the same name (#366).
#
# Case map (legacy test_satellite_templates.py -> here; TemplateContentTest, InstallDirectLinuxOnlyTest,
# InstallDirectOverwriteTest):
#   test_templates_retry_forever_with_backoff                    -> the templates retry forever with backoff
#   test_templates_document_the_old_systemd_fallback             -> the templates document the old systemd fallback
#   test_non_linux_host_is_refused_with_the_hub_side_instruction -> a non-Linux host is refused ... (non-Linux only)
#   test_fresh_install_writes_the_repo_template / test_refuses_differing_template_when_another_instance_has_no_env_file /
#   test_refusal_also_finds_instances_by_drop_in_directory / test_overwrites_when_other_instances_have_env_files /
#   test_overwrites_when_the_only_instance_is_the_one_being_installed / test_replace_template_overrides_the_refusal /
#   test_identical_template_is_never_refused                     -> the same seven names (Linux only; SKIP elsewhere,
#                                                                   listed on the hub #3621 Linux checklist)
# ConfigParser(strict=False) kept the LAST duplicate key; ini_get does too. Negative coverage:
# ported_suites_tests.rs appends `Restart=no` after `Restart=always` in a scratch copy of a template.
source "$(dirname "$0")/lib/tap.sh"

SATELLITE="$SCRIPTS_DIR/satellite"
INSTALL="$SATELLITE/install-direct.sh"
DIRECT="$SATELLITE/rsi-satellite-direct@.service"
DIAL_HOME="$SATELLITE/rsi-satellite@.service"
UNIT_NAME="rsi-satellite-direct@.service"

# ini_get <file> <section> <key>: the LAST value of `key=value` in `[section]` (the legacy
# ConfigParser(strict=False) kept the last duplicate, and so does systemd: `Restart=always`
# followed by `Restart=no` is `no`), or nothing. Comment lines (# and ;) are skipped; the key is
# matched exactly (case-sensitive); whitespace around the key and value is trimmed.
ini_get() {
    awk -v section="$2" -v key="$3" '
        /^[[:space:]]*[#;]/ { next }
        /^[[:space:]]*\[/ { current = $0; sub(/^[[:space:]]*\[/, "", current); sub(/\].*$/, "", current); next }
        current == section {
            line = $0
            idx = index(line, "=")
            if (idx > 0) {
                k = substr(line, 1, idx - 1); gsub(/^[[:space:]]+|[[:space:]]+$/, "", k)
                if (k == key) { v = substr(line, idx + 1); gsub(/^[[:space:]]+|[[:space:]]+$/, "", v); last = v; seen = 1 }
            }
        }
        END { if (seen) print last }' "$1"
}

ini_has_key() { # <file> <section> <key>
    awk -v section="$2" -v key="$3" '
        /^[[:space:]]*#/ { next }
        /^\[/ { current = $0; sub(/^\[/, "", current); sub(/\].*$/, "", current); next }
        current == section { idx = index($0, "="); if (idx > 0 && substr($0, 1, idx - 1) == key) found = 1 }
        END { exit found ? 0 : 1 }' "$1"
}

case_the_templates_retry_forever_with_backoff() {
    local path
    for path in "$DIRECT" "$DIAL_HOME"; do
        assert_eq 0 "$(ini_get "$path" Unit StartLimitIntervalSec)" "$(basename "$path") StartLimitIntervalSec"
        ! ini_has_key "$path" Unit StartLimitBurst || tap_fail "$(basename "$path") sets StartLimitBurst"
        assert_eq always "$(ini_get "$path" Service Restart)" "$(basename "$path") Restart"
        assert_eq 20s "$(ini_get "$path" Service RestartSec)" "$(basename "$path") RestartSec"
        assert_eq 6 "$(ini_get "$path" Service RestartSteps)" "$(basename "$path") RestartSteps"
        assert_eq 5min "$(ini_get "$path" Service RestartMaxDelaySec)" "$(basename "$path") RestartMaxDelaySec"
    done
}

# Keys unknown to systemd < 254 are ignored, leaving the fixed RestartSec retry.
case_the_templates_document_the_old_systemd_fallback() {
    local path text before_sec before_steps
    for path in "$DIRECT" "$DIAL_HOME"; do
        text="$(cat "$path")"
        assert_contains "$text" "systemd >= 254" "$(basename "$path")"
        before_sec="${text%%RestartSec=20s*}"
        before_steps="${text%%RestartSteps=6*}"
        [[ ${#before_sec} -lt ${#before_steps} ]] || tap_fail "$(basename "$path"): RestartSec=20s must precede RestartSteps=6"
    done
}

case_a_non_linux_host_is_refused_with_the_hub_side_instruction() {
    run_capture bash "$INSTALL" fresh peer-ssh
    assert_code 1 "exit code"
    assert_contains "$RUN_ERR" "install-direct.sh is Linux-only (systemd user units); run it on the hub"
}

# --- install-direct.sh overwrite protection (Linux systemd installer) ---

stub() { # <name> <body>
    write_exec "$BIN/$1" "#!/usr/bin/env bash
$2
"
}

setup_install() {
    # Short path: Unix socket paths cap at 107 bytes.
    BASE="$(mktemp -d /tmp/rsi-idt-XXXXXX)"
    HOME_DIR="$BASE/home"
    BIN="$BASE/bin"
    mkdir -p "$BIN" "$HOME_DIR/.config/systemd/user" "$HOME_DIR/.config/rsi/satellites"
    UNIT_DIR="$HOME_DIR/.config/systemd/user"
    CONFIG_DIR="$HOME_DIR/.config/rsi/satellites"
    SYSTEMCTL_LOG="$BASE/systemctl.log"
    : >"$SYSTEMCTL_LOG"
    stub ssh 'printf "RSI_HOME=%s\n" "/home/peer"'
    stub rsi-rpc "exit 0"
    stub journalctl "exit 0"
    # The stub binds (and closes) the unix socket the installer waits for.
    stub systemctl 'echo "$*" >> "$FAKE_SYSTEMCTL_LOG"
if [[ "$*" == *restart* ]]; then
  mkdir -p -m 700 "$(dirname "$HOME/.rsi/satellites/fresh-direct.sock")"
  perl -MSocket -e '"'"'socket(S, PF_UNIX, SOCK_STREAM, 0) or exit 1; bind(S, sockaddr_un($ARGV[0])) or exit 1; close(S)'"'"' "$HOME/.rsi/satellites/fresh-direct.sock"
fi'
    INSTALLED="$UNIT_DIR/$UNIT_NAME"
    trap 'rm -rf "$BASE"' EXIT
}

install() { # [extra args]
    HOME="$HOME_DIR" PATH="$BIN:$PATH" FAKE_SYSTEMCTL_LOG="$SYSTEMCTL_LOG" \
        run_capture bash "$INSTALL" fresh peer-ssh "$@"
}

old_template() { printf '[Service]\nExecStart=/bin/true\n' >"$INSTALLED"; }

add_instance() { # <name> <with env file: yes|no>
    mkdir -p "$UNIT_DIR/default.target.wants"
    ln -s "$INSTALLED" "$UNIT_DIR/default.target.wants/rsi-satellite-direct@$1.service"
    if [[ "$2" == yes ]]; then
        printf 'RSI_SATELLITE_TARGET=x\n' >"$CONFIG_DIR/$1-direct.env"
    fi
}

same_file() { cmp -s "$1" "$2"; }

case_a_fresh_install_writes_the_repo_template() {
    setup_install
    install
    assert_code 0 "exit code (stderr: $RUN_ERR)"
    same_file "$INSTALLED" "$DIRECT" || tap_fail "the installed unit differs from the repository template"
}

case_it_refuses_a_differing_template_when_another_instance_has_no_env_file() {
    setup_install
    old_template
    add_instance arch-laptop no
    install
    assert_ne 0 "$RUN_CODE" "exit code"
    assert_contains "$RUN_ERR" "refusing to overwrite"
    assert_contains "$RUN_ERR" "arch-laptop"
    assert_file_is "$INSTALLED" $'[Service]\nExecStart=/bin/true\n' "the installed unit is untouched"
    assert_no_file "$CONFIG_DIR/fresh-direct.env"
    assert_eq "" "$(cat "$SYSTEMCTL_LOG")" "systemctl calls"
}

case_the_refusal_also_finds_instances_by_drop_in_directory() {
    setup_install
    old_template
    mkdir "$UNIT_DIR/rsi-satellite-direct@mac-mini.service.d"
    install
    assert_ne 0 "$RUN_CODE" "exit code"
    assert_contains "$RUN_ERR" "mac-mini"
}

case_it_overwrites_when_other_instances_have_env_files() {
    setup_install
    old_template
    add_instance arch-laptop yes
    install
    assert_code 0 "exit code (stderr: $RUN_ERR)"
    same_file "$INSTALLED" "$DIRECT" || tap_fail "the template was not replaced"
}

case_it_overwrites_when_the_only_instance_is_the_one_being_installed() {
    setup_install
    old_template
    add_instance fresh no
    install
    assert_code 0 "exit code (stderr: $RUN_ERR)"
    same_file "$INSTALLED" "$DIRECT" || tap_fail "the template was not replaced"
}

case_replace_template_overrides_the_refusal() {
    setup_install
    old_template
    add_instance arch-laptop no
    install --replace-template
    assert_code 0 "exit code (stderr: $RUN_ERR)"
    same_file "$INSTALLED" "$DIRECT" || tap_fail "the template was not replaced"
}

case_an_identical_template_is_never_refused() {
    setup_install
    cp "$DIRECT" "$INSTALLED"
    add_instance arch-laptop no
    install
    assert_code 0 "exit code (stderr: $RUN_ERR)"
}

run_case "the templates retry forever with backoff" case_the_templates_retry_forever_with_backoff
run_case "the templates document the old systemd fallback" case_the_templates_document_the_old_systemd_fallback
if [[ "$(uname -s)" == Linux ]]; then
    run_case "a fresh install writes the repo template" case_a_fresh_install_writes_the_repo_template
    run_case "it refuses a differing template when another instance has no env file" case_it_refuses_a_differing_template_when_another_instance_has_no_env_file
    run_case "the refusal also finds instances by drop-in directory" case_the_refusal_also_finds_instances_by_drop_in_directory
    run_case "it overwrites when other instances have env files" case_it_overwrites_when_other_instances_have_env_files
    run_case "it overwrites when the only instance is the one being installed" case_it_overwrites_when_the_only_instance_is_the_one_being_installed
    run_case "--replace-template overrides the refusal" case_replace_template_overrides_the_refusal
    run_case "an identical template is never refused" case_an_identical_template_is_never_refused
    skip_case "a non-Linux host is refused with the hub-side instruction" "the refusal only fires off Linux"
else
    run_case "a non-Linux host is refused with the hub-side instruction" case_a_non_linux_host_is_refused_with_the_hub_side_instruction
    # Seven Linux-only cases: each is named so the skip is auditable (hub #3621 checklist).
    for linux_case in \
        "a fresh install writes the repo template" \
        "it refuses a differing template when another instance has no env file" \
        "the refusal also finds instances by drop-in directory" \
        "it overwrites when other instances have env files" \
        "it overwrites when the only instance is the one being installed" \
        "--replace-template overrides the refusal" \
        "an identical template is never refused"; do
        skip_linux_only "$linux_case" "install-direct.sh, a Linux systemd installer (GNU timeout, stat -c, a Perl Unix-socket stub)"
    done
fi
tap_done
