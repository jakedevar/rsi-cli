//! Operator setting `job_thermal_ceiling_c` (#2524): a host whose CPU
//! temperature is at or above the ceiling starts no new cargo build or test
//! job.
//!
//! The build slice bounds how many cores a job may take, but a laptop that is
//! already hot (Tctl 92-93 C under a satellite `scoped_test`) must not be given
//! more heat. A job admitted while the host is at or above the ceiling is held
//! (`held_for_thermal`, no unit, no timeout clock) and starts once the host
//! has cooled. Linux reads the `hwmon` CPU sensors (`k10temp` Tctl/Tdie,
//! `coretemp` package and cores, `zenpower`, `x86_pkg_temp`); every other
//! platform, and a Linux host with no readable CPU sensor, reports
//! [`Reading::Unsupported`] and holds nothing. The store mirrors the live
//! setting here so the job launcher reads it without the runtime config.

use std::path::Path;
use std::sync::atomic::{AtomicU32, Ordering};

/// Default ceiling in degrees Celsius.
pub const JOB_THERMAL_CEILING_C_DEFAULT: u32 = 85;
/// Smallest accepted non-zero value (below it an idle laptop would hold).
pub const JOB_THERMAL_CEILING_C_MIN: u32 = 60;
/// Largest accepted value (silicon throttles itself at about 100 C).
pub const JOB_THERMAL_CEILING_C_MAX: u32 = 110;

static CEILING_C: AtomicU32 = AtomicU32::new(JOB_THERMAL_CEILING_C_DEFAULT);

/// The live ceiling in degrees Celsius; 0 means no thermal hold.
#[must_use]
pub fn current_ceiling_c() -> u32 {
    CEILING_C.load(Ordering::Relaxed)
}

/// Publish the operator's value (the store calls this on every accepted write).
pub fn set_ceiling_c(celsius: u32) {
    CEILING_C.store(celsius, Ordering::Relaxed);
}

/// Whether `celsius` is an accepted setting: 0 (off) or inside the hard range.
#[must_use]
pub const fn is_valid_ceiling_c(celsius: u64) -> bool {
    celsius == 0
        || (celsius >= JOB_THERMAL_CEILING_C_MIN as u64
            && celsius <= JOB_THERMAL_CEILING_C_MAX as u64)
}

/// One CPU temperature sample.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Reading {
    /// The hottest CPU sensor, in degrees Celsius.
    Celsius(f64),
    /// No readable CPU sensor on this platform or host.
    Unsupported,
}

/// Why a job start is held: the host is at or above its ceiling.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ThermalHold {
    pub temperature_c: f64,
    pub ceiling_c: u32,
}

/// `Some` when `reading` is at or above `ceiling_c` (0 = off, never holds).
#[must_use]
pub fn hold_for(reading: Reading, ceiling_c: u32) -> Option<ThermalHold> {
    match reading {
        Reading::Celsius(temperature_c)
            if ceiling_c > 0 && temperature_c >= f64::from(ceiling_c) =>
        {
            Some(ThermalHold {
                temperature_c,
                ceiling_c,
            })
        }
        _ => None,
    }
}

/// Where Linux publishes hardware monitors.
pub const HWMON_ROOT: &str = "/sys/class/hwmon";

/// Chip names (`hwmon*/name`) that report the CPU package or die temperature.
const CPU_CHIPS: &[&str] = &[
    "k10temp",
    "coretemp",
    "zenpower",
    "x86_pkg_temp",
    "cpu_thermal",
];

/// The hottest CPU sensor under a `hwmon` root, `Unsupported` when none reads.
#[must_use]
pub fn read_hwmon(root: &Path) -> Reading {
    let Ok(chips) = std::fs::read_dir(root) else {
        return Reading::Unsupported;
    };
    let mut hottest: Option<f64> = None;
    for chip in chips.flatten() {
        let dir = chip.path();
        let name = std::fs::read_to_string(dir.join("name")).unwrap_or_default();
        if !CPU_CHIPS.contains(&name.trim()) {
            continue;
        }
        let Ok(files) = std::fs::read_dir(&dir) else {
            continue;
        };
        for file in files.flatten() {
            let file_name = file.file_name();
            let file_name = file_name.to_string_lossy();
            if !(file_name.starts_with("temp") && file_name.ends_with("_input")) {
                continue;
            }
            let Some(milli) = std::fs::read_to_string(file.path())
                .ok()
                .and_then(|text| text.trim().parse::<i64>().ok())
            else {
                continue;
            };
            let celsius = milli as f64 / 1000.0;
            // A sensor reporting 0 C or below is absent, not cold.
            if celsius > 0.0 && hottest.is_none_or(|best| celsius > best) {
                hottest = Some(celsius);
            }
        }
    }
    hottest.map_or(Reading::Unsupported, Reading::Celsius)
}

/// The host's CPU temperature now. Test seam builds report `Unsupported`, so a
/// hot CI host never holds a test (a test of the hold overrides
/// `rsid::agent_jobs::thermal_hold`).
#[must_use]
pub fn current_reading() -> Reading {
    #[cfg(feature = "test-seam")]
    {
        Reading::Unsupported
    }
    #[cfg(not(feature = "test-seam"))]
    {
        #[cfg(target_os = "linux")]
        {
            read_hwmon(Path::new(HWMON_ROOT))
        }
        #[cfg(not(target_os = "linux"))]
        {
            Reading::Unsupported
        }
    }
}

/// The hold in force now: the live reading against the live ceiling.
#[must_use]
pub fn hold_now() -> Option<ThermalHold> {
    let ceiling = current_ceiling_c();
    if ceiling == 0 {
        return None;
    }
    hold_for(current_reading(), ceiling)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chip(root: &Path, dir: &str, name: &str, sensors: &[(&str, &str)]) {
        let chip = root.join(dir);
        std::fs::create_dir_all(&chip).unwrap();
        std::fs::write(chip.join("name"), format!("{name}\n")).unwrap();
        for (file, value) in sensors {
            std::fs::write(chip.join(file), format!("{value}\n")).unwrap();
        }
    }

    #[test]
    fn the_hottest_cpu_sensor_is_read_and_other_chips_are_ignored() {
        let root = tempfile::tempdir().unwrap();
        chip(
            root.path(),
            "hwmon2",
            "k10temp",
            &[("temp1_input", "92750"), ("temp3_input", "71000")],
        );
        chip(root.path(), "hwmon0", "nvme", &[("temp1_input", "99000")]);
        chip(
            root.path(),
            "hwmon1",
            "acpitz",
            &[("temp1_input", "120000")],
        );
        assert_eq!(read_hwmon(root.path()), Reading::Celsius(92.75));
    }

    #[test]
    fn a_host_without_a_cpu_sensor_is_unsupported_not_cold() {
        let root = tempfile::tempdir().unwrap();
        assert_eq!(read_hwmon(root.path()), Reading::Unsupported);
        chip(root.path(), "hwmon0", "nvme", &[("temp1_input", "50000")]);
        chip(root.path(), "hwmon1", "coretemp", &[("temp1_input", "0")]);
        assert_eq!(read_hwmon(root.path()), Reading::Unsupported);
        assert_eq!(
            read_hwmon(&root.path().join("missing")),
            Reading::Unsupported
        );
    }

    #[test]
    fn at_or_above_the_ceiling_holds_and_below_or_off_does_not() {
        assert!(hold_for(Reading::Celsius(85.0), 85).is_some());
        assert!(hold_for(Reading::Celsius(92.5), 85).is_some());
        assert!(hold_for(Reading::Celsius(84.9), 85).is_none());
        assert!(hold_for(Reading::Celsius(99.0), 0).is_none());
        assert!(hold_for(Reading::Unsupported, 85).is_none());
        assert_eq!(JOB_THERMAL_CEILING_C_DEFAULT, 85);
    }

    #[test]
    fn zero_is_off_and_the_range_is_checked() {
        assert!(is_valid_ceiling_c(0));
        assert!(is_valid_ceiling_c(60));
        assert!(is_valid_ceiling_c(110));
        assert!(!is_valid_ceiling_c(59));
        assert!(!is_valid_ceiling_c(111));
    }
}
