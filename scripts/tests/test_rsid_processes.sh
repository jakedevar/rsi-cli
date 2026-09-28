#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
source "$SCRIPT_DIR/rsid-processes.sh"

pgrep() {
    printf '%s\n' 29712 474038 991
}

ps() {
    case "${!#}" in
        29712) printf 'Z\n' ;;
        474038) printf 'S\n' ;;
        991) return 1 ;; # exited between pgrep and ps
        *) return 2 ;;
    esac
}

actual="$(rsid_running_pids)"
if [[ "$actual" != 474038 ]]; then
    printf 'expected only the live rsid PID, got: %s\n' "$actual" >&2
    exit 1
fi

pgrep() { printf '%s\n' 29712; }
if [[ -n "$(rsid_running_pids)" ]]; then
    echo "expected a zombie-only rsid match to be treated as stopped" >&2
    exit 1
fi

echo "rsid process selection ignores zombies and exited PIDs"
