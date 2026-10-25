#!/usr/bin/env bash
# Shared boundary for installer copy/stage fixtures (#613). HOME does not isolate
# launchd's gui/UID or systemd's user bus. Every control/probe is a fixture program;
# a missing stub is a refusal, never a fallback to the host's executable.
# Requires tap.sh and fixture BASE, FAKES, HOME_DIR, TARGET and ROOT.

install_isolation_fixture() {
    CALLED="$BASE/called"
    must mkdir -p "$CALLED"
    write_exec "$FAKES/uname" $'#!/bin/sh\n[ "$1" = -s ] || exit 99\nprintf "%s\\n" "$INSTALL_TEST_OS"\n'
    write_exec "$FAKES/id" $'#!/bin/sh\n[ "$1" = -u ] || exit 99\necho 424242\n'
    local name
    for name in systemctl systemd-run nohup kill pkill killall pgrep ps lsof; do
        write_exec "$FAKES/$name" '#!/bin/sh
name=${0##*/}
printf "%s\n" "$*" >>"$INSTALL_TEST_CALLED/$name"
case "$name" in pgrep|ps|lsof) exit 1 ;; esac
exit 99
'
    done
    write_exec "$FAKES/launchctl" '#!/bin/sh
printf "%s\n" "$*" >>"$INSTALL_TEST_CALLED/launchctl"
case "$1:$2" in
    bootstrap:gui/424242)
        [ "$3" = "$HOME/Library/LaunchAgents/dev.rsi.rsid.plist" ] || exit 98 ;;
    print:gui/424242/dev.rsi.rsid) exit 113 ;;
    enable:gui/424242/dev.rsi.rsid) exit 0 ;;
    bootout:gui/424242/dev.rsi.rsid|kickstart:gui/424242/dev.rsi.rsid) ;;
    *) echo "outside fixture launchd domain" >&2; exit 98 ;;
esac
# Deliberately refuse a start: these suites do not own a daemon process.
exit 99
'
    # Bash's builtin bypasses PATH. Disable it in every fixture Bash (including
    # child scripts), so `kill` and `command kill` resolve to our recording stub.
    write_file "$BASE/bash-env" 'enable -n kill
'
}

install_isolated() {
    # A clean environment also removes inherited socket overrides, drain hooks,
    # session credentials and exported shell functions. Extra fixture settings
    # are supplied before the pinned boundary below.
    env -i "$@" HOME="$HOME_DIR" CARGO_TARGET_DIR="$TARGET" \
        PATH="$FAKES:$PATH" BASH_ENV="$BASE/bash-env" \
        GIT_CEILING_DIRECTORIES="$TAP_PARENT" GIT_CONFIG_GLOBAL=/dev/null GIT_CONFIG_NOSYSTEM=1 \
        RSI_REPO_GUARD_BIN="$RSI_REPO_GUARD_BIN" \
        INSTALL_TEST_FAKES="$FAKES" INSTALL_TEST_CALLED="$CALLED" \
        INSTALL_TEST_OS="${INSTALL_TEST_OS:-Darwin}" \
        bash -c '
            set -eu
            for name in launchctl systemctl systemd-run nohup kill pkill killall pgrep ps lsof id uname; do
                if [ "$(command -v "$name")" != "$INSTALL_TEST_FAKES/$name" ] || [ ! -x "$INSTALL_TEST_FAKES/$name" ]; then
                    echo "installer isolation: missing fixture $name" >&2
                    exit 97
                fi
            done
            exec bash "$@"
        ' install-fixture "$ROOT/scripts/install-release.sh" ${INSTALL_ARGS[@]+"${INSTALL_ARGS[@]}"}
}
