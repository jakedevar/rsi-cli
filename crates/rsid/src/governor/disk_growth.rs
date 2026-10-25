//! Remaining target growth for live cargo leases (#538). Filesystem work runs
//! on the resource sampler, never on an admission RPC or while State is locked.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use super::{Holder, own_target};

// Fresh rsid test builds on the laptop measured 20–22 GiB. Keep headroom when
// no completed-build history is available, including after daemon restart.
const DEFAULT_TARGET_BYTES: u64 = 24 * 1_073_741_824;

#[derive(Clone, Copy)]
pub(super) struct Usage {
    pub expected_bytes: u64,
    pub current_bytes: u64,
}

impl Default for Usage {
    fn default() -> Self {
        Self {
            expected_bytes: DEFAULT_TARGET_BYTES,
            current_bytes: 0,
        }
    }
}

impl Usage {
    pub(super) fn remaining_gb(self) -> f64 {
        self.expected_bytes.saturating_sub(self.current_bytes) as f64 / super::GIB
    }
}

struct Request {
    requested: Instant,
    sampled: Option<Instant>,
    usage: Usage,
}

#[derive(Default)]
struct Cache {
    requests: HashMap<(u32, u64), Request>,
    // A conservative in-memory high-water estimate per target. Never learn a
    // smaller estimate from an incomplete/failed unit; restart uses the default.
    targets: HashMap<PathBuf, (u64, Instant)>,
}

#[derive(Default)]
pub(super) struct Probe(Mutex<Cache>);

impl Probe {
    pub(super) fn usage(&self, holder: Holder) -> Usage {
        let mut cache = self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let entry = cache
            .requests
            .entry((holder.pid, holder.start_ticks))
            .or_insert_with(|| Request {
                requested: Instant::now(),
                sampled: None,
                usage: Usage::default(),
            });
        entry.requested = Instant::now();
        if entry
            .sampled
            .is_some_and(|at| at.elapsed() <= Duration::from_secs(5))
        {
            entry.usage
        } else {
            Usage {
                expected_bytes: entry.usage.expected_bytes.max(DEFAULT_TARGET_BYTES),
                current_bytes: 0,
            }
        }
    }

    pub(super) fn refresh(&self) {
        let holders: Vec<_> = {
            let mut cache = self
                .0
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            cache
                .requests
                .retain(|_, entry| entry.requested.elapsed() <= Duration::from_secs(120));
            cache
                .targets
                .retain(|_, (_, at)| at.elapsed() <= Duration::from_secs(3600));
            cache
                .requests
                .keys()
                .map(|&(pid, start_ticks)| Holder { pid, start_ticks })
                .collect()
        };
        for holder in holders {
            let observation = observe(holder);
            let mut cache = self
                .0
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let usage = if let Some((target, current_bytes)) = observation {
                // Bound historical bookkeeping independently of lease count.
                if cache.targets.len() >= 128 && !cache.targets.contains_key(&target) {
                    if let Some(oldest) = cache
                        .targets
                        .iter()
                        .min_by_key(|(_, (_, at))| *at)
                        .map(|(path, _)| path.clone())
                    {
                        cache.targets.remove(&oldest);
                    }
                }
                let estimate = cache
                    .targets
                    .entry(target)
                    .or_insert((DEFAULT_TARGET_BYTES, Instant::now()));
                estimate.0 = estimate.0.max(current_bytes);
                estimate.1 = Instant::now();
                Usage {
                    expected_bytes: estimate.0,
                    current_bytes,
                }
            } else {
                Usage::default()
            };
            if let Some(entry) = cache.requests.get_mut(&(holder.pid, holder.start_ticks)) {
                entry.usage = usage;
                entry.sampled = Some(Instant::now());
            }
        }
    }
}

fn observe(holder: Holder) -> Option<(PathBuf, u64)> {
    if super::resolve_proc_holder(holder.pid) != Some(holder) {
        return None;
    }
    // Linux reads only the job id and target from /proc; other platforms use
    // the conservative default until a portable process-context probe exists.
    let (_, target, cwd) = own_target::process_context(holder.pid)?;
    let root = target.parent()?;
    if !target.is_absolute()
        || target.file_name()? != "target"
        || target.canonicalize().ok()? != target
        || !cwd.canonicalize().ok()?.starts_with(root)
    {
        return None;
    }
    let current_bytes = own_target::allocated_bytes(&target);
    (super::resolve_proc_holder(holder.pid) == Some(holder)).then_some((target, current_bytes))
}

#[cfg(all(
    test,
    any(not(feature = "test-shard-mode"), feature = "test-shard-other-01")
))]
mod tests {
    use super::*;

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn missing_or_stale_measurement_reserves_a_whole_target() {
        let probe = Probe::default();
        let holder = Holder {
            pid: 17,
            start_ticks: 1,
        };
        assert_eq!(probe.usage(holder).remaining_gb(), 24.0);
        {
            let mut cache = probe.0.lock().unwrap();
            let entry = cache.requests.get_mut(&(17, 1)).unwrap();
            entry.usage = Usage {
                expected_bytes: 32 * 1_073_741_824,
                current_bytes: 10 * 1_073_741_824,
            };
            entry.sampled = Some(Instant::now());
        }
        assert_eq!(probe.usage(holder).remaining_gb(), 22.0);
        probe
            .0
            .lock()
            .unwrap()
            .requests
            .get_mut(&(17, 1))
            .unwrap()
            .sampled = Some(Instant::now() - Duration::from_secs(6));
        assert_eq!(probe.usage(holder).remaining_gb(), 32.0);
        // Reused pids do not inherit another process's current target usage.
        assert_eq!(
            probe
                .usage(Holder {
                    start_ticks: 2,
                    ..holder
                })
                .remaining_gb(),
            24.0
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn current_target_larger_than_estimate_has_no_negative_reservation() {
        assert_eq!(
            Usage {
                expected_bytes: DEFAULT_TARGET_BYTES,
                current_bytes: DEFAULT_TARGET_BYTES + 1,
            }
            .remaining_gb(),
            0.0
        );
    }
}
