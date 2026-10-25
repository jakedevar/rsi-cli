//! #3491: a streamed index pass holds the resolver's inputs, not the corpus.
//!
//! The hub's startup peak (VmHWM 1.54 GB after the first fix) was the largest
//! checkout's pass: the index cache keeps a pass' facts whole while references
//! resolve. A corpus over the retention budget now spills what no later step
//! reads and streams the rest back into staging one file at a time. This pins,
//! on a fixture of this repository's own Rust and Markdown (the same fact
//! density as a real checkout), that a streamed pass peaks under its estimate
//! and under half of what the same pass holds in memory.
//!
//! The binary installs a counting allocator, so it holds exactly one test:
//! another test thread would add its own allocations to the measured peak.

#![allow(clippy::expect_used, clippy::unwrap_used)] // Measurement fixture.

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering::Relaxed};

use rsi_codegraph::SourceFile;
use rsi_codegraph::invalidate::{ExtractionCache, estimated_streamed_heap_bytes};

struct Counting;

static LIVE: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);

fn grow(by: usize) {
    let live = LIVE.fetch_add(by, Relaxed) + by;
    PEAK.fetch_max(live, Relaxed);
}

// SAFETY: every call forwards to the system allocator with the caller's layout;
// the counters only observe sizes.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        // SAFETY: forwarded unchanged.
        let ptr = unsafe { System.alloc(layout) };
        if !ptr.is_null() {
            grow(layout.size());
        }
        ptr
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: forwarded unchanged.
        unsafe { System.dealloc(ptr, layout) };
        LIVE.fetch_sub(layout.size(), Relaxed);
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        // SAFETY: forwarded unchanged.
        let ptr = unsafe { System.alloc_zeroed(layout) };
        if !ptr.is_null() {
            grow(layout.size());
        }
        ptr
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        // SAFETY: forwarded unchanged.
        let moved = unsafe { System.realloc(ptr, layout, new_size) };
        if !moved.is_null() {
            if new_size >= layout.size() {
                grow(new_size - layout.size());
            } else {
                LIVE.fetch_sub(layout.size() - new_size, Relaxed);
            }
        }
        moved
    }
}

#[global_allocator]
static ALLOCATOR: Counting = Counting;

/// A checkout-shaped corpus: this crate's Rust under distinct crate directories
/// and the repository's own Markdown under distinct note directories (about a
/// 2:1 Markdown to Rust byte ratio, as the hub's primary checkout), several MB.
fn corpus() -> Vec<SourceFile> {
    const RUST: [(&str, &str); 6] = [
        ("lib.rs", include_str!("../src/lib.rs")),
        ("blocks.rs", include_str!("../src/blocks.rs")),
        ("impact.rs", include_str!("../src/impact.rs")),
        ("query.rs", include_str!("../src/query.rs")),
        ("staged.rs", include_str!("../src/staged.rs")),
        ("rust_resolve.rs", include_str!("../src/rust_resolve.rs")),
    ];
    const MARKDOWN: [(&str, &str); 4] = [
        ("agents.md", include_str!("../../../AGENTS.md")),
        (
            "landing.md",
            include_str!("../../../docs/agents/landing-and-gates.md"),
        ),
        (
            "reference.md",
            include_str!("../../../docs/agents/reference.md"),
        ),
        (
            "contracts.md",
            include_str!("../../../docs/agents/rsid-rpc-contracts.md"),
        ),
    ];
    let mut files = Vec::new();
    for copy in 0..4 {
        for (name, text) in RUST {
            files.push(SourceFile {
                relative_path: format!("crates/copy{copy}/src/{name}"),
                bytes: text.as_bytes().to_vec(),
            });
        }
    }
    for copy in 0..2 {
        for (name, text) in MARKDOWN {
            files.push(SourceFile {
                relative_path: format!("notes/copy{copy}/{name}"),
                bytes: text.as_bytes().to_vec(),
            });
        }
    }
    files
}

/// Peak live bytes above the allocation level at the start of `run`.
fn peak_over_start<T>(run: impl FnOnce() -> T) -> (T, usize) {
    let start = LIVE.load(Relaxed);
    PEAK.store(start, Relaxed);
    let value = run();
    (value, PEAK.load(Relaxed).saturating_sub(start))
}

#[allow(clippy::cast_precision_loss)]
fn mb(bytes: usize) -> f64 {
    bytes as f64 / (1024.0 * 1024.0)
}

#[test]
fn a_streamed_pass_peaks_under_its_estimate_and_half_the_in_memory_pass() {
    let files = corpus();
    let source_bytes = files.iter().map(|file| file.bytes.len()).sum::<usize>();
    let estimate = files
        .iter()
        .map(|file| estimated_streamed_heap_bytes(&file.relative_path, file.bytes.len()))
        .sum::<usize>();
    drop(files);
    let spill = tempfile::tempdir().unwrap();

    // The same pass held whole (the pre-streaming path, no retained facts).
    let mut whole = ExtractionCache::with_retain_budget(0);
    let (_, in_memory) = peak_over_start(|| {
        let update = whole.update(corpus()).expect("in-memory pass");
        drop(update);
    });

    // The streamed pass reads every file back, as staging does.
    let mut streaming = ExtractionCache::with_retain_budget(0);
    let (visited, streamed) = peak_over_start(|| {
        let mut update = streaming
            .update_pass(corpus(), None, spill.path())
            .expect("streamed pass");
        assert!(update.facts.is_streamed());
        let mut visited = 0usize;
        update
            .facts
            .for_each(|_| {
                visited += 1;
                Ok::<(), rsi_codegraph::CodegraphError>(())
            })
            .expect("stage every file");
        visited
    });
    assert_eq!(visited, 4 * 6 + 2 * 4);
    // A second streamed pass over the same corpus is no larger: nothing is
    // retained between passes.
    let (_, streamed_again) = peak_over_start(|| {
        let update = streaming
            .update_pass(corpus(), None, spill.path())
            .expect("re-index pass");
        drop(update);
    });

    println!(
        "streamed_bound source_mb={:.1} estimate_mb={:.1} in_memory_mb={:.1} streamed_mb={:.1} \
         streamed_again_mb={:.1}",
        mb(source_bytes),
        mb(estimate),
        mb(in_memory),
        mb(streamed),
        mb(streamed_again),
    );
    // The estimate is the contract the daemon admits a pass against.
    assert!(
        streamed <= estimate,
        "a streamed pass peaked at {} MB over the {} MB estimate",
        mb(streamed),
        mb(estimate)
    );
    assert!(
        streamed_again <= estimate,
        "a streamed re-index peaked at {} MB over the {} MB estimate",
        mb(streamed_again),
        mb(estimate)
    );
    assert!(
        streamed * 2 <= in_memory,
        "a streamed pass ({} MB) must hold under half of the in-memory pass ({} MB)",
        mb(streamed),
        mb(in_memory)
    );
}
