//! Terminal-watch limits the store enforces, moved down from `session` so
//! `store` has no edge into it (issue #1021 S3a).

/// A8: hard ceiling on enabled `OnTerminal` watch rows per wake target
/// (master). Bounds self-DoS from a runaway arming loop; dedup on the
/// (caller, watched) natural key keeps legitimate re-arms free. Enforced at
/// the single arm point for every arm transport.
pub const MAX_TERMINAL_WATCHES_PER_MASTER: usize = 64;

/// Depth cap for the rotation-lineage chase (`continued_from` successors).
/// Rotation chains are short in practice; the cap only bounds pathological
/// or cyclic data.
pub const WATCH_LINEAGE_DEPTH_CAP: usize = 8;
