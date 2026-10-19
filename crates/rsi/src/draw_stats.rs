//! Bounded aggregate draw counters for the TUI event loop (#332).
//!
//! Every `mark_dirty*` call records a [`DirtyReason`]; one draw consumes every
//! reason pending since the previous draw. Marks that arrive before the next
//! render opportunity coalesce into that one draw, so `marks >= draws` per
//! reason and `frames <= total marks + animation frames`. The key set is the
//! closed [`DirtyReason`] enum: no payloads, no per-frame log, no per-session
//! identity, memory independent of uptime.

use std::time::Instant;

/// Why the UI asked to be redrawn. A closed set: the metrics key space.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DirtyReason {
    /// A key press.
    Key,
    /// A mouse event.
    Mouse,
    /// A bracketed paste.
    Paste,
    /// A terminal resize.
    Resize,
    /// A daemon push notification.
    Push,
    /// A poll, fetch or other background result applied on the loop.
    Poll,
    /// A startup / reconnect bootstrap result.
    Bootstrap,
    /// A TTL expiry (notifications, the ESP flash).
    Expiry,
    /// The 1 s defensive repaint tick.
    Defensive,
    /// An internal mark that names no more specific reason.
    Other,
}

impl DirtyReason {
    /// Every reason, in the order snapshots list them.
    pub const ALL: [DirtyReason; 10] = [
        DirtyReason::Key,
        DirtyReason::Mouse,
        DirtyReason::Paste,
        DirtyReason::Resize,
        DirtyReason::Push,
        DirtyReason::Poll,
        DirtyReason::Bootstrap,
        DirtyReason::Expiry,
        DirtyReason::Defensive,
        DirtyReason::Other,
    ];

    /// Stable lowercase name used in the snapshot.
    pub const fn name(self) -> &'static str {
        match self {
            DirtyReason::Key => "key",
            DirtyReason::Mouse => "mouse",
            DirtyReason::Paste => "paste",
            DirtyReason::Resize => "resize",
            DirtyReason::Push => "push",
            DirtyReason::Poll => "poll",
            DirtyReason::Bootstrap => "bootstrap",
            DirtyReason::Expiry => "expiry",
            DirtyReason::Defensive => "defensive",
            DirtyReason::Other => "other",
        }
    }

    const fn index(self) -> usize {
        self as usize
    }
}

const REASONS: usize = DirtyReason::ALL.len();

/// One reason's counters.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ReasonCounts {
    /// `mark_dirty*` calls with this reason.
    pub marks: u64,
    /// Draws that consumed at least one pending mark of this reason.
    pub draws: u64,
}

/// A point-in-time copy of the counters.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DrawStatsSnapshot {
    /// Whole seconds since the counters started.
    pub window_secs: u64,
    /// Completed draws.
    pub frames: u64,
    /// Draws no pending mark asked for: the pacer's animation follow-ups.
    pub animation_frames: u64,
    /// Dirty draws that no `mark_dirty*` call explains (a direct
    /// `needs_redraw = true` write).
    pub unattributed_frames: u64,
    /// `(reason name, counts)` for every reason, in [`DirtyReason::ALL`] order.
    pub by_reason: Vec<(&'static str, ReasonCounts)>,
}

impl DrawStatsSnapshot {
    /// One JSON object, no payloads: what `RSI_DRAW_STATS_FILE` holds.
    pub fn to_json(&self) -> String {
        let reasons: Vec<String> = self
            .by_reason
            .iter()
            .map(|(name, counts)| {
                format!(
                    "\"{name}\":{{\"marks\":{},\"draws\":{}}}",
                    counts.marks, counts.draws
                )
            })
            .collect();
        format!(
            "{{\"window_secs\":{},\"frames\":{},\"animation_frames\":{},\"unattributed_frames\":{},\"by_reason\":{{{}}}}}",
            self.window_secs,
            self.frames,
            self.animation_frames,
            self.unattributed_frames,
            reasons.join(",")
        )
    }
}

/// The live counters. Single-threaded: owned by the event loop's `App`.
#[derive(Debug, Clone)]
pub struct DrawStats {
    started: Instant,
    counts: [ReasonCounts; REASONS],
    /// Reasons marked since the last draw, one bit per [`DirtyReason`].
    pending: u16,
    frames: u64,
    animation_frames: u64,
    unattributed_frames: u64,
}

impl Default for DrawStats {
    fn default() -> Self {
        Self::new()
    }
}

impl DrawStats {
    pub fn new() -> Self {
        Self {
            started: Instant::now(),
            counts: [ReasonCounts::default(); REASONS],
            pending: 0,
            frames: 0,
            animation_frames: 0,
            unattributed_frames: 0,
        }
    }

    /// Record one `mark_dirty*` call.
    pub fn mark(&mut self, reason: DirtyReason) {
        let counts = &mut self.counts[reason.index()];
        counts.marks = counts.marks.saturating_add(1);
        self.pending |= 1 << reason.index();
    }

    /// Record one completed draw. `was_dirty` is `needs_redraw` before the
    /// draw: false means the pacer drew for an animation follow-up. Consumes
    /// every pending reason, so marks that coalesced before this draw count as
    /// one draw per distinct reason.
    pub fn record_draw(&mut self, was_dirty: bool) {
        self.frames = self.frames.saturating_add(1);
        let pending = std::mem::take(&mut self.pending);
        if pending == 0 {
            if was_dirty {
                self.unattributed_frames = self.unattributed_frames.saturating_add(1);
            } else {
                self.animation_frames = self.animation_frames.saturating_add(1);
            }
            return;
        }
        for reason in DirtyReason::ALL {
            if pending & (1 << reason.index()) != 0 {
                let counts = &mut self.counts[reason.index()];
                counts.draws = counts.draws.saturating_add(1);
            }
        }
    }

    pub fn snapshot(&self) -> DrawStatsSnapshot {
        DrawStatsSnapshot {
            window_secs: self.started.elapsed().as_secs(),
            frames: self.frames,
            animation_frames: self.animation_frames,
            unattributed_frames: self.unattributed_frames,
            by_reason: DirtyReason::ALL
                .iter()
                .map(|reason| (reason.name(), self.counts[reason.index()]))
                .collect(),
        }
    }
}

/// Write `snapshot` to `path` atomically (temp file in the same directory, then
/// rename), for scripts that observe a headless run. Best effort: a failure is
/// ignored so diagnostics never affect the UI.
pub fn write_snapshot_file(path: &std::path::Path, snapshot: &DrawStatsSnapshot) {
    let Some(name) = path.file_name() else {
        return;
    };
    let mut tmp_name = name.to_os_string();
    tmp_name.push(".tmp");
    let tmp = path.with_file_name(tmp_name);
    if std::fs::write(&tmp, snapshot.to_json()).is_err() {
        return;
    }
    let _ = std::fs::rename(&tmp, path);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn counts(stats: &DrawStats, reason: DirtyReason) -> ReasonCounts {
        stats.counts[reason.index()]
    }

    #[test]
    fn marks_before_one_draw_coalesce_into_one_frame_per_reason() {
        let mut stats = DrawStats::new();
        for _ in 0..50 {
            stats.mark(DirtyReason::Push);
        }
        for _ in 0..3 {
            stats.mark(DirtyReason::Key);
        }
        stats.record_draw(true);
        let snap = stats.snapshot();
        assert_eq!(snap.frames, 1, "53 marks, one render opportunity, one draw");
        assert_eq!(
            counts(&stats, DirtyReason::Push),
            ReasonCounts {
                marks: 50,
                draws: 1
            }
        );
        assert_eq!(
            counts(&stats, DirtyReason::Key),
            ReasonCounts { marks: 3, draws: 1 }
        );
        assert_eq!(snap.unattributed_frames, 0);
        assert_eq!(snap.animation_frames, 0);
    }

    #[test]
    fn a_draw_consumes_the_pending_reasons_so_the_next_draw_starts_clean() {
        let mut stats = DrawStats::new();
        stats.mark(DirtyReason::Resize);
        stats.record_draw(true);
        stats.mark(DirtyReason::Defensive);
        stats.record_draw(true);
        assert_eq!(
            counts(&stats, DirtyReason::Resize),
            ReasonCounts { marks: 1, draws: 1 }
        );
        assert_eq!(
            counts(&stats, DirtyReason::Defensive),
            ReasonCounts { marks: 1, draws: 1 }
        );
        assert_eq!(stats.snapshot().frames, 2);
    }

    #[test]
    fn a_draw_without_a_mark_is_animation_or_unattributed() {
        let mut stats = DrawStats::new();
        stats.record_draw(false);
        stats.record_draw(false);
        stats.record_draw(true);
        let snap = stats.snapshot();
        assert_eq!(snap.frames, 3);
        assert_eq!(snap.animation_frames, 2, "pacer follow-ups");
        assert_eq!(snap.unattributed_frames, 1, "dirty with no recorded mark");
        assert!(
            snap.by_reason
                .iter()
                .all(|(_, c)| c.marks == 0 && c.draws == 0)
        );
    }

    #[test]
    fn snapshot_key_set_is_the_closed_reason_enum() {
        let snap = DrawStats::new().snapshot();
        let names: Vec<&str> = snap.by_reason.iter().map(|(n, _)| *n).collect();
        assert_eq!(
            names,
            [
                "key",
                "mouse",
                "paste",
                "resize",
                "push",
                "poll",
                "bootstrap",
                "expiry",
                "defensive",
                "other"
            ]
        );
        let json: serde_json::Value = serde_json::from_str(&snap.to_json()).unwrap();
        assert_eq!(json["by_reason"]["defensive"]["marks"], 0);
        assert_eq!(json["frames"], 0);
    }

    #[test]
    fn snapshot_file_is_replaced_atomically() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("draws.json");
        let mut stats = DrawStats::new();
        stats.mark(DirtyReason::Key);
        stats.record_draw(true);
        write_snapshot_file(&path, &stats.snapshot());
        stats.record_draw(false);
        write_snapshot_file(&path, &stats.snapshot());
        let json: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(json["frames"], 2);
        assert!(!dir.path().join("draws.json.tmp").exists());
    }
}
