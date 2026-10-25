//! Read-only resource evidence. No subprocesses and no setting changes.
use rsi_common::host_tuning::{Reading, ResourceProbe};
use std::path::Path;

trait Source {
    fn parallelism(&self) -> Option<usize>;
    #[cfg(target_os = "linux")]
    fn read(&self, path: &str) -> Option<String>;
    fn disk_free(&self, path: &Path) -> Option<u64>;
    #[cfg(target_os = "linux")]
    fn temperature(&self) -> Option<f64>;
}
struct Host;
impl Source for Host {
    fn parallelism(&self) -> Option<usize> {
        std::thread::available_parallelism().ok().map(usize::from)
    }
    #[cfg(target_os = "linux")]
    fn read(&self, path: &str) -> Option<String> {
        std::fs::read_to_string(path).ok()
    }
    fn disk_free(&self, path: &Path) -> Option<u64> {
        #[cfg(unix)]
        {
            crate::daemon_info::disk_free_bytes(path)
        }
        #[cfg(not(unix))]
        {
            let _ = path;
            None
        }
    }
    #[cfg(target_os = "linux")]
    fn temperature(&self) -> Option<f64> {
        {
            match rsid_core::thermal_guard::current_reading() {
                rsid_core::thermal_guard::Reading::Celsius(value) => Some(value),
                rsid_core::thermal_guard::Reading::Unsupported => None,
            }
        }
    }
}
pub(crate) fn probe(data: &Path, sandboxes: &Path) -> ResourceProbe {
    collect(&Host, data, sandboxes, cfg!(target_os = "linux"))
}
fn collect(source: &impl Source, data: &Path, sandboxes: &Path, linux: bool) -> ResourceProbe {
    let result = ResourceProbe {
        effective_cpus: Reading::from_option(
            source.parallelism().map(|n| n as f64),
            "parallelism unavailable",
        ),
        cgroup_cpu_quota: Reading::Unsupported,
        cgroup: Reading::Unsupported,
        total_ram_bytes: Reading::Unsupported,
        available_ram_bytes: Reading::Unsupported,
        data_dir_free_bytes: Reading::from_option(source.disk_free(data), "disk free unavailable"),
        sandbox_base_free_bytes: Reading::from_option(
            source.disk_free(sandboxes),
            "disk free unavailable",
        ),
        load_average: Reading::Unsupported,
        cpu_temperature_c: Reading::Unsupported,
    };
    // All /proc and /sys access is compiled out on other platforms. Fakes run
    // the same parser on Linux; unsupported results are explicit elsewhere.
    #[cfg(target_os = "linux")]
    let mut result = result;
    #[cfg(target_os = "linux")]
    if linux {
        result.cgroup = Reading::from_option(
            source.read("/proc/self/cgroup"),
            "cgroup membership unreadable",
        );
        let quota = cgroup_quota(source);
        result.cgroup_cpu_quota = Reading::from_option(
            quota,
            "no readable finite cgroup CPU quota (unlimited or unavailable)",
        );
        if let (Reading::Known { value }, Some(quota)) = (&mut result.effective_cpus, quota) {
            *value = value.min(quota);
        }
        let memory = source.read("/proc/meminfo");
        let bytes = |key: &str| {
            memory.as_deref()?.lines().find_map(|line| {
                let (label, rest) = line.split_once(':')?;
                (label == key)
                    .then(|| {
                        rest.split_whitespace()
                            .next()?
                            .parse::<u64>()
                            .ok()?
                            .checked_mul(1024)
                    })
                    .flatten()
            })
        };
        result.total_ram_bytes = Reading::from_option(bytes("MemTotal"), "MemTotal unavailable");
        result.available_ram_bytes =
            Reading::from_option(bytes("MemAvailable"), "MemAvailable unavailable");
        result.load_average = Reading::from_option(
            source.read("/proc/loadavg").and_then(|body| {
                let load = crate::daemon_info::parse_loadavg(&body)?;
                Some([load.one, load.five, load.fifteen])
            }),
            "load average unavailable",
        );
        result.cpu_temperature_c =
            Reading::from_option(source.temperature(), "CPU sensor unreadable or unavailable");
    }
    #[cfg(not(target_os = "linux"))]
    let _ = linux;
    result
}

/// Resolve memberships against actual mount roots, including cgroup namespaces
/// and v1 cpu controllers. The tightest readable ancestor quota wins.
#[cfg(target_os = "linux")]
fn cgroup_quota(source: &impl Source) -> Option<f64> {
    let membership = source.read("/proc/self/cgroup")?;
    let mounts = source.read("/proc/self/mountinfo")?;
    let mut tightest: Option<f64> = None;
    for line in membership.lines() {
        let mut fields = line.splitn(3, ':');
        let _id = fields.next();
        let controllers = fields.next()?;
        let group = fields.next()?;
        let v2 = controllers.is_empty();
        if !v2 && !controllers.split(',').any(|c| c == "cpu") {
            continue;
        }
        for mount in mounts.lines() {
            let Some((left, right)) = mount.split_once(" - ") else {
                continue;
            };
            let m: Vec<_> = left.split_whitespace().collect();
            let r: Vec<_> = right.split_whitespace().collect();
            if m.len() < 5 || r.len() < 3 {
                continue;
            }
            if (v2 && r[0] != "cgroup2")
                || (!v2 && (r[0] != "cgroup" || !r[2].split(',').any(|c| c == "cpu")))
            {
                continue;
            }
            let unescape = |s: &str| {
                s.replace("\\040", " ")
                    .replace("\\011", "\t")
                    .replace("\\134", "\\")
            };
            let root = unescape(m[3]);
            let mountpoint = unescape(m[4]);
            let group = Path::new(group);
            let Ok(relative) = group.strip_prefix(&root) else {
                continue;
            };
            // Never follow malformed proc data outside the mounted hierarchy.
            if relative
                .components()
                .any(|c| !matches!(c, std::path::Component::Normal(_)))
            {
                continue;
            }
            let directory = Path::new(&mountpoint).join(relative);
            for ancestor in directory.ancestors() {
                if !ancestor.starts_with(&mountpoint) {
                    break;
                }
                let read = |name| source.read(&ancestor.join(name).to_string_lossy());
                let quota = if v2 {
                    read("cpu.max").and_then(|text| {
                        let mut words = text.split_whitespace();
                        ratio(words.next()?, words.next()?)
                    })
                } else {
                    read("cpu.cfs_quota_us")
                        .zip(read("cpu.cfs_period_us"))
                        .and_then(|(q, p)| ratio(q.trim(), p.trim()))
                };
                if let Some(quota) = quota {
                    tightest = Some(tightest.map_or(quota, |n| n.min(quota)));
                }
            }
        }
    }
    tightest
}
#[cfg(target_os = "linux")]
fn ratio(quota: &str, period: &str) -> Option<f64> {
    let q = quota.parse::<f64>().ok()?;
    let p = period.parse::<f64>().ok()?;
    (q.is_finite() && p.is_finite() && q > 0.0 && p > 0.0).then_some(q / p)
}

#[cfg(all(
    test,
    target_os = "linux",
    any(not(feature = "test-shard-mode"), feature = "test-shard-other-01")
))]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    #[derive(Default)]
    struct Fake {
        files: BTreeMap<String, String>,
    }
    impl Source for Fake {
        fn parallelism(&self) -> Option<usize> {
            Some(16)
        }
        fn read(&self, path: &str) -> Option<String> {
            self.files.get(path).cloned()
        }
        fn disk_free(&self, path: &Path) -> Option<u64> {
            (path == Path::new("data")).then_some(42)
        }
        fn temperature(&self) -> Option<f64> {
            None
        }
    }
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn constrained_host_and_unavailable_sources_are_explicit() {
        let fake = Fake {
            files: BTreeMap::from([
                ("/proc/self/cgroup".into(), "0::/user/worker".into()),
                (
                    "/proc/self/mountinfo".into(),
                    "1 2 0:1 / /sys/fs/cgroup rw - cgroup2 cgroup rw".into(),
                ),
                (
                    "/sys/fs/cgroup/user/worker/cpu.max".into(),
                    "400000 100000".into(),
                ),
                ("/sys/fs/cgroup/user/cpu.max".into(), "150000 100000".into()),
                (
                    "/proc/meminfo".into(),
                    "MemTotal: 1000 kB\nMemAvailable: 250 kB\n".into(),
                ),
                ("/proc/loadavg".into(), "1.0 2.0 3.0 1/2 3".into()),
            ]),
        };
        let probe = collect(&fake, Path::new("data"), Path::new("sandbox"), true);
        assert_eq!(probe.effective_cpus, Reading::Known { value: 1.5 });
        assert_eq!(probe.total_ram_bytes, Reading::Known { value: 1_024_000 });
        assert_eq!(probe.available_ram_bytes, Reading::Known { value: 256_000 });
        assert_eq!(probe.data_dir_free_bytes, Reading::Known { value: 42 });
        assert!(matches!(
            probe.sandbox_base_free_bytes,
            Reading::Unknown { .. }
        ));
        assert!(matches!(probe.cpu_temperature_c, Reading::Unknown { .. }));
        assert_eq!(
            probe.load_average,
            Reading::Known {
                value: [1.0, 2.0, 3.0]
            }
        );
        let absent = collect(
            &Fake::default(),
            Path::new("data"),
            Path::new("sandbox"),
            true,
        );
        assert!(matches!(absent.cgroup_cpu_quota, Reading::Unknown { .. }));
        assert!(matches!(absent.total_ram_bytes, Reading::Unknown { .. }));
        let other = collect(&fake, Path::new("data"), Path::new("sandbox"), false);
        assert_eq!(other.total_ram_bytes, Reading::Unsupported);
        assert_eq!(other.cgroup_cpu_quota, Reading::Unsupported);
    }
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn v1_mount_root_and_unlimited_leaf_respect_parent_quota() {
        let fake = Fake {
            files: BTreeMap::from([
                (
                    "/proc/self/cgroup".into(),
                    "2:cpu,cpuacct:/slice/worker".into(),
                ),
                (
                    "/proc/self/mountinfo".into(),
                    "1 2 0:1 /slice /cg rw - cgroup cgroup rw,cpu,cpuacct".into(),
                ),
                ("/cg/worker/cpu.cfs_quota_us".into(), "-1".into()),
                ("/cg/worker/cpu.cfs_period_us".into(), "100000".into()),
                ("/cg/cpu.cfs_quota_us".into(), "50000".into()),
                ("/cg/cpu.cfs_period_us".into(), "100000".into()),
            ]),
        };
        assert_eq!(cgroup_quota(&fake), Some(0.5));
    }
}
