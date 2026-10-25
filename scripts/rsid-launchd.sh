#!/usr/bin/env bash
# Sourced by install-release.sh and rsid-start-satellite (#2936): run rsid on
# macOS as the launchd user agent dev.rsi.rsid, rendered from
# scripts/launchd/dev.rsi.rsid.plist.in into ~/Library/LaunchAgents. The agent
# owns the holder -> supervisor -> rsid chain, so a reboot (after login), a
# crash of the holder chain or a supervisor exit brings it back, while a deploy
# still restarts rsid inside the job through the supervisor's exit-75 path.
#
# Bash 3.2 compatible (macOS /bin/bash): no empty "${a[@]}" under `set -u`.
# Needs `rsid_running_pids` and `rsid_launcher_pids` (rsid-processes.sh).

RSID_LAUNCHD_LABEL="dev.rsi.rsid"

# RSI_LAUNCHD_AGENT=0 opts out: install-release.sh then keeps the detached
# holder chain of older installs.
rsid_launchd_available() {
    [[ "${RSI_LAUNCHD_AGENT:-1}" != 0 ]] && command -v launchctl >/dev/null 2>&1
}

rsid_launchd_domain() {
    printf 'gui/%s\n' "$(id -u)"
}

rsid_launchd_service() {
    printf '%s/%s\n' "$(rsid_launchd_domain)" "$RSID_LAUNCHD_LABEL"
}

rsid_launchd_plist() {
    printf '%s\n' "$HOME/Library/LaunchAgents/$RSID_LAUNCHD_LABEL.plist"
}

# A PATH entry a persistent agent may keep: absolute, an existing directory, and
# not a per-run location (temp dirs, agent sandboxes) that the next boot lacks.
rsid_launchd_path_entry_ok() {
    case "$1" in
        /*) ;;
        *) return 1 ;;
    esac
    case "$1" in
        "$HOME"/.rsi/sandboxes/*) return 1 ;;
        "$HOME"/*) ;;
        /tmp/* | /private/tmp/* | /var/folders/* | /private/var/folders/*) return 1 ;;
        *) [[ -z "${TMPDIR:-}" || "$1" != "${TMPDIR%/}"/* ]] || return 1 ;;
    esac
    [[ -d "$1" ]]
}

# The agent's PATH. launchd gives a job only /usr/bin:/bin:/usr/sbin:/sbin, so
# it is written into the plist: ~/.local/bin and ~/.cargo/bin first (as on
# Linux, rsid_daemon_path), then the macOS tool locations that exist (nvm's
# newest node, bun, Homebrew's rustup and bin), then the caller's own entries,
# then the system directories. RSI_LAUNCHD_PATH replaces it verbatim.
rsid_launchd_path() {
    if [[ -n "${RSI_LAUNCHD_PATH:-}" ]]; then
        printf '%s\n' "$RSI_LAUNCHD_PATH"
        return 0
    fi
    local result="" dir node_bin=""
    local -a entries=()
    node_bin="$(ls -d "$HOME"/.nvm/versions/node/*/bin 2>/dev/null | sort -V | tail -n 1 || true)"
    IFS=: read -r -a entries <<<"${PATH:-}"
    for dir in "$HOME/.local/bin" "$HOME/.cargo/bin" \
        "$node_bin" "$HOME/.bun/bin" /opt/homebrew/opt/rustup/bin /opt/homebrew/bin /opt/homebrew/sbin /usr/local/bin \
        ${entries[@]+"${entries[@]}"} /usr/bin /bin /usr/sbin /sbin; do
        [[ -n "$dir" ]] || continue
        case "$dir" in
            "$HOME/.local/bin" | "$HOME/.cargo/bin" | /usr/bin | /bin | /usr/sbin | /sbin) ;;
            *) rsid_launchd_path_entry_ok "$dir" || continue ;;
        esac
        case ":$result:" in
            *":$dir:"*) ;;
            *) result="${result:+$result:}$dir" ;;
        esac
    done
    printf '%s\n' "$result"
}

# rsid_launchd_render <template> <label> <install-dir> <socket> <log> <home> <path> <lang>
# Fill each @NAME@ placeholder of the template with its XML-escaped value in
# one pass (a value that itself contains @NAME@ is never re-expanded) and print
# the result. An unknown placeholder is refused. Values travel through the
# environment so awk never interprets backslashes or `&` in them (#465).
rsid_launchd_render() {
    RSID_R_LABEL="$2" RSID_R_INSTALL_DIR="$3" RSID_R_SOCKET="$4" RSID_R_LOG="$5" \
        RSID_R_HOME="$6" RSID_R_PATH="$7" RSID_R_LANG="$8" \
        awk -v template="$1" '
        function escape(v) {
            gsub(/&/, "\\&amp;", v)
            gsub(/</, "\\&lt;", v)
            gsub(/>/, "\\&gt;", v)
            return v
        }
        BEGIN {
            split("LABEL INSTALL_DIR SOCKET LOG HOME PATH LANG", names, " ")
            for (i in names) values[names[i]] = escape(ENVIRON["RSID_R_" names[i]])
        }
        {
            line = $0
            out = ""
            while (match(line, /@[A-Z_]+@/)) {
                name = substr(line, RSTART + 1, RLENGTH - 2)
                if (!(name in values)) {
                    unknown[name] = 1
                    out = out substr(line, 1, RSTART + RLENGTH - 1)
                } else {
                    out = out substr(line, 1, RSTART - 1) values[name]
                }
                line = substr(line, RSTART + RLENGTH)
            }
            print out line
        }
        END {
            n = 0
            for (name in unknown) list[++n] = name
            if (n == 0) exit 0
            for (i = 2; i <= n; i++) {
                key = list[i]
                for (j = i - 1; j >= 1 && list[j] > key; j--) list[j + 1] = list[j]
                list[j + 1] = key
            }
            msg = ""
            for (i = 1; i <= n; i++) msg = msg (i > 1 ? ", " : "") list[i]
            print template " names unknown placeholders: " msg > "/dev/stderr"
            exit 3
        }' "$1"
}

# rsid_launchd_write_plist <template> <install-dir> <socket> <log>
# Render the agent (atomic rename, mode 0644). Prints `changed` or `unchanged`.
rsid_launchd_write_plist() {
    local template="$1" install_dir="$2" socket="$3" log="$4" plist temp
    plist="$(rsid_launchd_plist)"
    mkdir -p "$(dirname -- "$plist")"
    case "$socket" in
        /*) ;;
        *) socket="$PWD/$socket" ;;
    esac
    temp="$(mktemp "$(dirname -- "$plist")/.$RSID_LAUNCHD_LABEL.XXXXXX")" || return 1
    if ! rsid_launchd_render "$template" "$RSID_LAUNCHD_LABEL" "$install_dir" "$socket" "$log" \
        "$HOME" "$(rsid_launchd_path)" "${LANG:-en_US.UTF-8}" >"$temp"; then
        rm -f "$temp"
        return 1
    fi
    # Refuse to install a plist launchd cannot parse (plutil on macOS, xmllint
    # where present; a host with neither has no launchd to parse it anyway).
    if command -v plutil >/dev/null 2>&1; then
        if ! plutil -lint "$temp" >/dev/null 2>&1; then
            echo "$template rendered a plist launchd cannot parse" >&2
            rm -f "$temp"
            return 1
        fi
    elif command -v xmllint >/dev/null 2>&1; then
        if ! xmllint --noout "$temp" >/dev/null 2>&1; then
            echo "$template rendered a plist launchd cannot parse" >&2
            rm -f "$temp"
            return 1
        fi
    fi
    if [[ -f "$plist" ]] && cmp -s "$temp" "$plist"; then
        rm -f "$temp"
        echo unchanged
        return 0
    fi
    chmod 0644 "$temp" && mv -f "$temp" "$plist" || {
        rm -f "$temp"
        return 1
    }
    echo changed
}

rsid_launchd_loaded() {
    launchctl print "$(rsid_launchd_service)" >/dev/null 2>&1
}

# The job's running process (the holder), or nothing.
rsid_launchd_pid() {
    launchctl print "$(rsid_launchd_service)" 2>/dev/null \
        | awk '$1 == "pid" && $2 == "=" && $3 ~ /^[0-9]+$/ { print $3; exit }'
}

# rsid_launchd_stop <seconds>: boot the job out and wait until launchd has
# removed it (bootout returns while the chain is still stopping).
rsid_launchd_stop() {
    local limit="$1" waited=0
    rsid_launchd_loaded || return 0
    echo "Stopping launchd agent $(rsid_launchd_service) ..."
    launchctl bootout "$(rsid_launchd_service)" 2>/dev/null || true
    while rsid_launchd_loaded; do
        if ((waited >= limit * 10)); then
            echo "launchd agent $(rsid_launchd_service) did not stop within ${limit}s" >&2
            return 1
        fi
        sleep 0.1
        waited=$((waited + 1))
    done
}

# rsid_launchd_start <seconds>: load (or start) the agent and wait for its
# process. Sets RSID_LAUNCHD_PID to the holder's PID.
rsid_launchd_start() {
    local limit="$1" waited=0 service
    service="$(rsid_launchd_service)"
    RSID_LAUNCHD_PID=""
    # A job the operator disabled cannot be loaded; installing it means it runs.
    launchctl enable "$service" 2>/dev/null || true
    if rsid_launchd_loaded; then
        echo "Starting loaded launchd agent $service ..."
        launchctl kickstart "$service" >/dev/null || return 1
    else
        echo "Loading launchd agent $service from $(rsid_launchd_plist) ..."
        launchctl bootstrap "$(rsid_launchd_domain)" "$(rsid_launchd_plist)" || return 1
    fi
    while :; do
        RSID_LAUNCHD_PID="$(rsid_launchd_pid)"
        [[ -z "$RSID_LAUNCHD_PID" ]] || return 0
        if ((waited >= limit * 10)); then
            echo "launchd agent $service has no running process after ${limit}s" >&2
            return 1
        fi
        sleep 0.1
        waited=$((waited + 1))
    done
}
