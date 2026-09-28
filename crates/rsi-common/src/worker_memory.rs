//! Fresh-install limits for the aggregate systemd worker slice.

pub const MEMORY_MIN_MIB: u64 = 256;
pub const MEMORY_MAX_MIB: u64 = 1024 * 1024;
pub const FALLBACK_HIGH_MIB: u64 = 6 * 1024;
pub const FALLBACK_MAX_MIB: u64 = 8 * 1024;

/// Derive valid aggregate slice limits from Linux MemTotal, expressed in KiB.
pub fn limits_from_mem_total_kib(mem_total_kib: u64) -> (u64, u64) {
    let total_mib = mem_total_kib / 1024;
    let high = (total_mib.saturating_mul(55) / 100).clamp(MEMORY_MIN_MIB, MEMORY_MAX_MIB - 1);
    let max = (total_mib.saturating_mul(70) / 100)
        .clamp(MEMORY_MIN_MIB, MEMORY_MAX_MIB)
        .max(high + 1);
    (high, max)
}

/// Read host memory once for a new default; retain bounded limits if unavailable.
pub fn default_limits_mib() -> (u64, u64) {
    #[cfg(target_os = "linux")]
    {
        if let Ok(meminfo) = std::fs::read_to_string("/proc/meminfo") {
            if let Some(total_kib) = meminfo.lines().find_map(|line| {
                line.strip_prefix("MemTotal:")?
                    .split_whitespace()
                    .next()?
                    .parse::<u64>()
                    .ok()
            }) {
                return limits_from_mem_total_kib(total_kib);
            }
        }
    }
    (FALLBACK_HIGH_MIB, FALLBACK_MAX_MIB)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn aggregate_limits_follow_host_memory_and_clamp() {
        assert_eq!(limits_from_mem_total_kib(16 * 1024 * 1024), (9011, 11468));
        assert_eq!(limits_from_mem_total_kib(0), (256, 257));
        assert_eq!(limits_from_mem_total_kib(u64::MAX), (1_048_575, 1_048_576));
    }
}
