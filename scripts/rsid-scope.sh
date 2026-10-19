#!/usr/bin/env bash
# Sourced by install-release.sh and rsid-start-satellite: the rsid scope
# snapshot (~/.rsi/rsid-scope.env) and the rsi-workers.slice aggregate limits.
# Callers set RSI_HOME_DIR and RSID_SCOPE_SETTINGS first.

load_rsid_scope_settings() {
    # Seed the shared launcher snapshot once. UpdateDaemonConfig replaces it
    # after the operator changes any scope limit.
    mkdir -p "$RSI_HOME_DIR"
    if [[ ! -e "$RSID_SCOPE_SETTINGS" ]]; then
        local temp_settings
        temp_settings="$(mktemp "$RSI_HOME_DIR/.rsid-scope.XXXXXXXX")"
        cat >"$temp_settings" <<'EOF'
rsid_scope_memory_high_mib=6144
rsid_scope_memory_max_mib=8192
rsid_scope_memory_swap_max_mib=0
rsid_scope_cpu_weight=20
worker_scope_memory_high_mib=6144
worker_scope_memory_max_mib=8192
worker_scope_memory_swap_max_mib=0
worker_scope_cpu_weight=20
EOF
        # Publish a complete file without replacing a concurrent TUI launch
        # or an operator-updated snapshot.
        if ! ln "$temp_settings" "$RSID_SCOPE_SETTINGS" 2>/dev/null \
            && [[ ! -e "$RSID_SCOPE_SETTINGS" ]]; then
            rm -f "$temp_settings"
            echo "Cannot create rsid scope settings at $RSID_SCOPE_SETTINGS" >&2
            exit 1
        fi
        rm -f "$temp_settings"
    fi
    if [[ ! -r "$RSID_SCOPE_SETTINGS" ]]; then
        echo "Cannot read rsid scope settings at $RSID_SCOPE_SETTINGS" >&2
        exit 1
    fi

    local seen_high=0 seen_max=0 seen_swap=0 seen_weight=0 key value
    local seen_worker_high=0 seen_worker_max=0 seen_worker_swap=0 seen_worker_weight=0
    WORKER_SCOPE_MEMORY_HIGH_MIB=6144
    WORKER_SCOPE_MEMORY_MAX_MIB=8192
    WORKER_SCOPE_MEMORY_SWAP_MAX_MIB=0
    WORKER_SCOPE_CPU_WEIGHT=20
    while IFS='=' read -r key value || [[ -n "${key:-}${value:-}" ]]; do
        [[ -z "${key//[[:space:]]/}" || "$key" == \#* ]] && continue
        value="${value//[[:space:]]/}"
        if [[ ! "$value" =~ ^[0-9]+$ ]]; then
            echo "Invalid numeric value in $RSID_SCOPE_SETTINGS for $key" >&2
            exit 1
        fi
        case "$key" in
            rsid_scope_memory_high_mib)
                (( seen_high == 0 )) || { echo "Duplicate $key in $RSID_SCOPE_SETTINGS" >&2; exit 1; }
                seen_high=1; RSID_SCOPE_MEMORY_HIGH_MIB="$value" ;;
            rsid_scope_memory_max_mib)
                (( seen_max == 0 )) || { echo "Duplicate $key in $RSID_SCOPE_SETTINGS" >&2; exit 1; }
                seen_max=1; RSID_SCOPE_MEMORY_MAX_MIB="$value" ;;
            rsid_scope_memory_swap_max_mib)
                (( seen_swap == 0 )) || { echo "Duplicate $key in $RSID_SCOPE_SETTINGS" >&2; exit 1; }
                seen_swap=1; RSID_SCOPE_MEMORY_SWAP_MAX_MIB="$value" ;;
            rsid_scope_cpu_weight)
                (( seen_weight == 0 )) || { echo "Duplicate $key in $RSID_SCOPE_SETTINGS" >&2; exit 1; }
                seen_weight=1; RSID_SCOPE_CPU_WEIGHT="$value" ;;
            worker_scope_memory_high_mib)
                (( seen_worker_high == 0 )) || { echo "Duplicate $key in $RSID_SCOPE_SETTINGS" >&2; exit 1; }
                seen_worker_high=1; WORKER_SCOPE_MEMORY_HIGH_MIB="$value" ;;
            worker_scope_memory_max_mib)
                (( seen_worker_max == 0 )) || { echo "Duplicate $key in $RSID_SCOPE_SETTINGS" >&2; exit 1; }
                seen_worker_max=1; WORKER_SCOPE_MEMORY_MAX_MIB="$value" ;;
            worker_scope_memory_swap_max_mib)
                (( seen_worker_swap == 0 )) || { echo "Duplicate $key in $RSID_SCOPE_SETTINGS" >&2; exit 1; }
                seen_worker_swap=1; WORKER_SCOPE_MEMORY_SWAP_MAX_MIB="$value" ;;
            worker_scope_cpu_weight)
                (( seen_worker_weight == 0 )) || { echo "Duplicate $key in $RSID_SCOPE_SETTINGS" >&2; exit 1; }
                seen_worker_weight=1; WORKER_SCOPE_CPU_WEIGHT="$value" ;;
            *) echo "Unknown rsid scope setting $key in $RSID_SCOPE_SETTINGS" >&2; exit 1 ;;
        esac
    done <"$RSID_SCOPE_SETTINGS"

    if [[ "$seen_high" -ne 1 || "$seen_max" -ne 1 || "$seen_swap" -ne 1 || "$seen_weight" -ne 1 \
        || "$RSID_SCOPE_MEMORY_HIGH_MIB" -lt 256 || "$RSID_SCOPE_MEMORY_HIGH_MIB" -gt 1048576 \
        || "$RSID_SCOPE_MEMORY_MAX_MIB" -lt 256 || "$RSID_SCOPE_MEMORY_MAX_MIB" -gt 1048576 \
        || "$RSID_SCOPE_MEMORY_HIGH_MIB" -ge "$RSID_SCOPE_MEMORY_MAX_MIB" \
        || "$RSID_SCOPE_MEMORY_SWAP_MAX_MIB" -gt 1048576 \
        || "$RSID_SCOPE_CPU_WEIGHT" -lt 1 || "$RSID_SCOPE_CPU_WEIGHT" -gt 10000 ]]; then
        echo "Invalid or incomplete rsid scope settings in $RSID_SCOPE_SETTINGS" >&2
        exit 1
    fi
    local worker_keys_seen=$((seen_worker_high + seen_worker_max + seen_worker_swap + seen_worker_weight))
    if [[ "$worker_keys_seen" -ne 0 && "$worker_keys_seen" -ne 4 ]]; then
        echo "Incomplete worker slice settings in $RSID_SCOPE_SETTINGS" >&2
        exit 1
    fi
    if [[ "$WORKER_SCOPE_MEMORY_HIGH_MIB" -lt 256 || "$WORKER_SCOPE_MEMORY_HIGH_MIB" -gt 1048576 \
        || "$WORKER_SCOPE_MEMORY_MAX_MIB" -lt 256 || "$WORKER_SCOPE_MEMORY_MAX_MIB" -gt 1048576 \
        || "$WORKER_SCOPE_MEMORY_HIGH_MIB" -ge "$WORKER_SCOPE_MEMORY_MAX_MIB" \
        || "$WORKER_SCOPE_MEMORY_SWAP_MAX_MIB" -gt 1048576 \
        || "$WORKER_SCOPE_CPU_WEIGHT" -lt 1 || "$WORKER_SCOPE_CPU_WEIGHT" -gt 10000 ]]; then
        echo "Invalid worker slice settings in $RSID_SCOPE_SETTINGS" >&2
        exit 1
    fi
}

verify_worker_slice() {
    local properties control_group active_state memory_high memory_max memory_swap_max cpu_weight
    properties="$(systemctl --user show rsi-workers.slice \
        --property=ActiveState --property=ControlGroup --property=MemoryHigh \
        --property=MemoryMax --property=MemorySwapMax --property=CPUWeight)" || return 1
    active_state="$(sed -n 's/^ActiveState=//p' <<<"$properties")"
    control_group="$(sed -n 's/^ControlGroup=//p' <<<"$properties")"
    memory_high="$(sed -n 's/^MemoryHigh=//p' <<<"$properties")"
    memory_max="$(sed -n 's/^MemoryMax=//p' <<<"$properties")"
    memory_swap_max="$(sed -n 's/^MemorySwapMax=//p' <<<"$properties")"
    cpu_weight="$(sed -n 's/^CPUWeight=//p' <<<"$properties")"
    [[ "$active_state" == active && "$control_group" == /user.slice/* \
        && "$control_group" == */rsi-workers.slice \
        && "$memory_high" =~ ^[0-9]+$ && "$memory_max" =~ ^[0-9]+$ \
        && "$memory_swap_max" =~ ^[0-9]+$ && "$cpu_weight" =~ ^[0-9]+$ \
        && "$memory_high" -eq $((WORKER_SCOPE_MEMORY_HIGH_MIB * 1024 * 1024)) \
        && "$memory_max" -eq $((WORKER_SCOPE_MEMORY_MAX_MIB * 1024 * 1024)) \
        && "$memory_swap_max" -eq $((WORKER_SCOPE_MEMORY_SWAP_MAX_MIB * 1024 * 1024)) \
        && "$cpu_weight" -eq "$WORKER_SCOPE_CPU_WEIGHT" ]]
}

worker_slice_is_empty() {
    local properties control_group populated descendants
    properties="$(systemctl --user show rsi-workers.slice --property=ControlGroup)" || return 1
    control_group="$(sed -n 's/^ControlGroup=//p' <<<"$properties")"
    [[ "$control_group" == "/user.slice/user-${EUID}.slice/user@${EUID}.service/rsi.slice/rsi-workers.slice" ]] || return 1
    populated="$(sed -n 's/^populated \([0-9][0-9]*\)$/\1/p' "/sys/fs/cgroup${control_group}/cgroup.events")" || return 1
    descendants="$(sed -n 's/^nr_descendants \([0-9][0-9]*\)$/\1/p' "/sys/fs/cgroup${control_group}/cgroup.stat")" || return 1
    [[ "$populated" == 0 && "$descendants" == 0 ]]
}

provision_worker_slice() {
    local before
    before="$(systemctl --user show rsi-workers.slice --property=ActiveState)" || {
        echo "Cannot inspect rsi-workers.slice." >&2
        exit 1
    }
    if [[ "$before" == ActiveState=active ]]; then
        if verify_worker_slice; then
            return 0
        fi
        if ! worker_slice_is_empty; then
            echo "Active rsi-workers.slice differs from configured limits and may contain workers; daemon will restart to reap owned workers, while new worker admission stays closed." >&2
            return 0
        fi
        echo "Reconciling empty rsi-workers.slice with configured limits." >&2
    fi
    systemctl --user set-property --runtime rsi-workers.slice \
        "MemoryHigh=${WORKER_SCOPE_MEMORY_HIGH_MIB}M" \
        "MemoryMax=${WORKER_SCOPE_MEMORY_MAX_MIB}M" \
        "MemorySwapMax=${WORKER_SCOPE_MEMORY_SWAP_MAX_MIB}M" \
        "CPUWeight=${WORKER_SCOPE_CPU_WEIGHT}" || exit 1
    systemctl --user start rsi-workers.slice || exit 1
    if ! verify_worker_slice; then
        echo "rsi-workers.slice did not acquire configured aggregate limits; refusing daemon start." >&2
        exit 1
    fi
}

verify_rsid_scope() {
    local scope_unit="$1"
    local properties control_group active_state memory_high memory_max memory_swap_max cpu_weight
    properties="$(systemctl --user show "$scope_unit" \
        --property=ActiveState --property=ControlGroup --property=MemoryHigh \
        --property=MemoryMax --property=MemorySwapMax --property=CPUWeight)" || {
        echo "Cannot inspect effective rsid scope limits." >&2
        exit 1
    }
    active_state="$(sed -n 's/^ActiveState=//p' <<<"$properties")"
    control_group="$(sed -n 's/^ControlGroup=//p' <<<"$properties")"
    memory_high="$(sed -n 's/^MemoryHigh=//p' <<<"$properties")"
    memory_max="$(sed -n 's/^MemoryMax=//p' <<<"$properties")"
    memory_swap_max="$(sed -n 's/^MemorySwapMax=//p' <<<"$properties")"
    cpu_weight="$(sed -n 's/^CPUWeight=//p' <<<"$properties")"
    if [[ "$active_state" != active || "$control_group" != /user.slice/* \
        || "$control_group" != */"$scope_unit" || ! "$memory_high" =~ ^[0-9]+$ \
        || ! "$memory_max" =~ ^[0-9]+$ || ! "$memory_swap_max" =~ ^[0-9]+$ \
        || ! "$cpu_weight" =~ ^[0-9]+$ \
        || "$memory_high" -ne $((RSID_SCOPE_MEMORY_HIGH_MIB * 1024 * 1024)) \
        || "$memory_max" -ne $((RSID_SCOPE_MEMORY_MAX_MIB * 1024 * 1024)) \
        || "$memory_swap_max" -ne $((RSID_SCOPE_MEMORY_SWAP_MAX_MIB * 1024 * 1024)) \
        || "$cpu_weight" -ne "$RSID_SCOPE_CPU_WEIGHT" ]]; then
        echo "$scope_unit is active without the configured effective limits or user-slice placement." >&2
        systemctl --user stop "$scope_unit" >/dev/null 2>&1 || true

        exit 1
    fi
}

# Persist the verified aggregate limits (a drop-in under ~/.config/systemd/user),
# so they survive a reboot: provision_worker_slice sets them runtime-only, and a
# reboot clears them, after which every worker launch is refused (#2820). A busy
# slice with conflicting limits is left untouched and is not persisted.
persist_worker_slice_limits() {
    if verify_worker_slice; then
        systemctl --user set-property rsi-workers.slice \
            "MemoryHigh=${WORKER_SCOPE_MEMORY_HIGH_MIB}M" \
            "MemoryMax=${WORKER_SCOPE_MEMORY_MAX_MIB}M" \
            "MemorySwapMax=${WORKER_SCOPE_MEMORY_SWAP_MAX_MIB}M" \
            "CPUWeight=${WORKER_SCOPE_CPU_WEIGHT}"
    fi
}
