#!/usr/bin/env bash
# Sourced by install-release.sh and the recovery runbook.

# A transient service does not inherit the installer's environment like a scope.
# Pass exported configuration by name: systemd-run reads the value from its own
# environment, so neither command arguments nor installer output contain it.
# Provider secrets continue to come from ~/.rsi/.env; agent custody and build
# metadata must not become the daemon's identity.
#
# #2823: the daemon's PATH decides which rustc every job unit builds with. A
# restart from a bare `systemd-run --user` (the hub's, after a reboot) carries
# /usr/local/bin:/usr/bin, so every job built with the system rustc instead of
# the rust-toolchain.toml pin and the warning gate failed on untouched code. The
# daemon PATH is the caller's PATH with ~/.cargo/bin (rustup) and ~/.local/bin
# in front when absent.
rsid_daemon_path() {
    local dir path=":${PATH:-}:" prefix=""
    for dir in "$HOME/.local/bin" "$HOME/.cargo/bin"; do
        case "$path" in
            *":$dir:"*) ;;
            *) prefix="$dir:$prefix" ;;
        esac
    done
    printf '%s%s\n' "$prefix" "${PATH:-/usr/local/bin:/usr/bin:/bin}"
}

rsid_daemon_environment() {
    local key
    DAEMON_ENV_ARGS=()
    while IFS= read -r key; do
        case "$key" in
            RSI_SESSION_*|RSI_PROCESS_OWNERSHIP_NAMESPACE|RSI_LISTEN_FD|RSI_CARGO_*|RSI_SCOPED_TEST_*|RSI_TEST_*|*KEY*|*TOKEN*|*SECRET*|*PASSWORD*|*CREDENTIAL*) continue ;;
            PATH) continue ;;
            HOME|USER|LOGNAME|SHELL|LANG|LC_*|TZ|TMPDIR|XDG_*|AWS_REGION|AWS_DEFAULT_REGION|AWS_PROFILE|RSI_*|MOTHERSHIP_*|FLYWHEEL_*)
                DAEMON_ENV_ARGS+=("--setenv=$key") ;;
        esac
    done < <(compgen -e | sort)
    DAEMON_ENV_ARGS+=("--setenv=PATH=$(rsid_daemon_path)")
}

# shellcheck source=scripts/repo-guard-bin.sh
source "$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)/repo-guard-bin.sh"

# rsid_rpc_ready <rsi-rpc> <socket> [launcher-pid]: one GetHealthStatus probe, run
# by `rsi-repo-guard rsid-health-probe` (#465; it was an inline script heredoc).
# Exit 0 ready, 1 not ready, 2 the launcher or daemon exited, 3 the process that
# answered exited right after replying.
rsid_rpc_ready() {
    rsid_resolve_repo_guard "$1" rsid-health-probe || return 1
    "$RSI_GUARD_BIN" rsid-health-probe "$1" "$2" "${3:-}"
}

wait_for_rsid_health() {
    local rpc="$1" socket="$2" limit="$3" unit="${4:-}" launcher="${5:-}"
    local started=$SECONDS next_progress=$SECONDS state="socket pending"
    # No probe binary is a fixed failure, not something to retry for the whole limit.
    rsid_resolve_repo_guard "$rpc" rsid-health-probe || return 1
    while ((SECONDS - started < limit)); do
        if [[ -n "$unit" ]]; then
            state="$(systemctl --user show "$unit" --property=ActiveState --value 2>/dev/null || echo unknown)"
            if [[ "$state" != active && "$state" != activating ]]; then
                echo "$unit exited before rsid became healthy ($state)" >&2
                return 1
            fi
            if [[ "$(systemctl --user show "$unit" --property=MainPID --value 2>/dev/null)" != "$launcher" ]]; then
                echo "$unit launcher changed before rsid became healthy" >&2
                return 1
            fi
        fi
        local probe_status=0
        rsid_rpc_ready "$rpc" "$socket" "$launcher" || probe_status=$?
        if ((probe_status == 0)); then
            # The probe also checks the launcher after reading the response.
            echo "rsid healthy after $((SECONDS - started))s on $socket"
            return 0
        fi
        if ((probe_status == 2)); then
            echo "rsid launcher exited before an identified GetHealthStatus reply" >&2
            return 1
        fi
        if ((probe_status == 3)); then
            echo "the rsid process that answered GetHealthStatus exited right after replying; the daemon is not staying up" >&2
            return 1
        fi
        if ((SECONDS >= next_progress)); then
            echo "Waiting for rsid health: ${state}, $((SECONDS - started))/${limit}s"
            next_progress=$((SECONDS + 5))
        fi
        sleep 1
    done
    echo "rsid did not answer an identified GetHealthStatus on $socket within ${limit}s" >&2
    return 1
}
