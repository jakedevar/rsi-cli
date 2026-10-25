#!/usr/bin/env bash
# The `desktop-install` job recipe (#3239): build the desktop app and run
# `scripts/install-desktop.sh --required` against a throwaway HOME.
#
#   AgentSubmitJob {"kind":"test","params":{"recipe":"desktop-install"}}
#
# The daemon runs this through `make desktop-install-job` in rsi-build.slice, like every
# other build/test job, and its receipt carries the exit code and the log path. The
# install writes only under a job-owned directory made below the job's private TMPDIR
# (`<jobs>/<id>.tmp`); the directory is removed when the script ends. The operator's
# real HOME, XDG_* directories and ~/.local/bin are never written. Toolchain homes
# (CARGO_HOME, RUSTUP_HOME) keep their real values so cargo and rustup still find the
# installed toolchain and the crate cache; npm's cache is part of the throwaway HOME.
#
# Takes no argument: what runs is fixed. Any argument is refused.
set -euo pipefail

if [[ $# -ne 0 ]]; then
    echo "desktop_install_job_args_refused: this job takes no arguments (got: $*);" \
        "it only runs scripts/install-desktop.sh --required in the allowlisted" \
        "standalone workspace desktop/src-tauri" >&2
    exit 2
fi

HERE="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
REAL_HOME="${HOME:?HOME is not set}"
export CARGO_HOME="${CARGO_HOME:-$REAL_HOME/.cargo}"
export RUSTUP_HOME="${RUSTUP_HOME:-$REAL_HOME/.rustup}"

SCRATCH="$(mktemp -d "${TMPDIR:-/tmp}/rsi-desktop-install-job.XXXXXX")"
case "$SCRATCH" in
    */rsi-desktop-install-job.??????) ;;
    *) echo "error: unexpected scratch directory: $SCRATCH" >&2; exit 1 ;;
esac
cleanup() {
    rm -rf -- "$SCRATCH"
}
trap cleanup EXIT

export HOME="$SCRATCH/home"
export XDG_CONFIG_HOME="$SCRATCH/xdg/config"
export XDG_DATA_HOME="$SCRATCH/xdg/data"
export XDG_CACHE_HOME="$SCRATCH/xdg/cache"
export XDG_STATE_HOME="$SCRATCH/xdg/state"
mkdir -p "$HOME" "$XDG_CONFIG_HOME" "$XDG_DATA_HOME" "$XDG_CACHE_HOME" "$XDG_STATE_HOME"
# An inherited override must not send the install to the operator's real paths.
unset RSI_INSTALL_BIN_DIR RSI_DESKTOP_INSTALL_DIR RSI_DESKTOP_APP_DIR

echo "desktop-install-job: scratch=$SCRATCH (removed on exit)"
status=0
"$HERE/install-desktop.sh" --required || status=$?
if [[ "$status" -eq 0 && ! -x "$HOME/.rsi/install/rsi-desktop" ]]; then
    echo "error: install-desktop.sh exited 0 but wrote no binary under the job HOME" >&2
    status=1
fi
echo "desktop-install-job: exit=$status"
exit "$status"
