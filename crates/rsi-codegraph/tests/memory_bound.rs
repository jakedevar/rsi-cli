//! #3491: one index pass must hold a bounded multiple of its source bytes.
//!
//! A hub start indexed ten checkouts (the largest 193 MB of source) and rsid
//! reached 2.8 GB resident, because `ExtractionCache::update` kept the previous
//! pass's facts, a clone of them and the resolved copy alive together
//! (measured on a 125 MB checkout: 1.3 GB for a fresh pass, 1.8 GB for a
//! re-index). This test pins the multiples on a fixture built from this
//! crate's own sources, which has the same fact density as real checkouts.
//!
//! The binary installs a counting allocator, so it holds exactly one test:
//! another test thread would add its own allocations to the measured peak.

#![allow(clippy::expect_used, clippy::unwrap_used)] // Measurement fixture.

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering::Relaxed};

use rsi_codegraph::SourceFile;
use rsi_codegraph::invalidate::{ExtractionCache, estimated_fact_heap_bytes};

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

/// Rust sources of this crate, copied under distinct crate directories until
/// the corpus is a few MB: realistic nodes, relations and unresolved sites.
fn corpus() -> Vec<SourceFile> {
    const SOURCES: [(&str, &str); 6] = [
        ("lib.rs", include_str!("../src/lib.rs")),
        ("blocks.rs", include_str!("../src/blocks.rs")),
        ("impact.rs", include_str!("../src/impact.rs")),
        ("query.rs", include_str!("../src/query.rs")),
        ("staged.rs", include_str!("../src/staged.rs")),
        ("rust_resolve.rs", include_str!("../src/rust_resolve.rs")),
    ];
    let mut files = Vec::new();
    for copy in 0..4 {
        for (name, text) in SOURCES {
            files.push(SourceFile {
                relative_path: format!("crates/copy{copy}/src/{name}"),
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
fn an_index_pass_holds_a_bounded_multiple_of_its_source_bytes() {
    let files = corpus();
    let source_bytes = files.iter().map(|file| file.bytes.len()).sum::<usize>();
    let estimate = files
        .iter()
        .map(|file| estimated_fact_heap_bytes(&file.relative_path, file.bytes.len()))
        .sum::<usize>();
    drop(files);

    // A corpus over the retention budget (the hub's large checkouts): a pass
    // holds one copy of its facts, whether it is the first or a re-index.
    let mut unretained = ExtractionCache::with_retain_budget(0);
    let (_, unretained_fresh) = peak_over_start(|| {
        let files = corpus();
        let update = unretained.update(files).expect("fresh pass");
        drop(update);
    });
    assert!(!unretained.retains_raw_facts());
    let (_, unretained_again) = peak_over_start(|| {
        let files = corpus();
        let update = unretained.update(files).expect("re-index pass");
        drop(update);
    });

    // A small corpus keeps its raw facts for the next pass: the cache and one
    // resolved copy are alive together, never a third (the clone the cache used
    // to make of every reused entry).
    let mut retained = ExtractionCache::new();
    let (_, retained_fresh) = peak_over_start(|| {
        let files = corpus();
        let update = retained.update(files).expect("fresh pass");
        drop(update);
    });
    assert!(retained.retains_raw_facts());
    let (_, retained_again) = peak_over_start(|| {
        let files = corpus();
        let update = retained.update(files).expect("re-index pass");
        drop(update);
    });

    let times = |peak: usize| peak as f64 / source_bytes as f64;
    println!(
        "memory_bound source_mb={:.1} estimate_mb={:.1} unretained_fresh_x={:.2} \
         unretained_again_x={:.2} retained_fresh_x={:.2} retained_again_x={:.2}",
        mb(source_bytes),
        mb(estimate),
        times(unretained_fresh),
        times(unretained_again),
        times(retained_fresh),
        times(retained_again),
    );

    // The estimate is the contract the daemon admits a pass against.
    assert!(
        unretained_fresh <= estimate,
        "a fresh pass peaked at {} MB over the {} MB estimate",
        mb(unretained_fresh),
        mb(estimate)
    );
    assert!(
        unretained_again <= estimate,
        "a re-index pass peaked at {} MB over the {} MB estimate",
        mb(unretained_again),
        mb(estimate)
    );
    // A first pass with retention holds the resolved copy and the raw copy it
    // keeps (the raw one without source bytes): under 1.75x the estimate.
    assert!(
        retained_fresh <= estimate + estimate * 3 / 4,
        "a retaining fresh pass peaked at {} MB over 1.75x the {} MB estimate",
        mb(retained_fresh),
        mb(estimate)
    );
    // A re-index moves the retained facts instead of cloning them next to the
    // resolved copy: the cache cloned every reused entry into a third copy
    // before, which put this pass near 2.3x the estimate.
    assert!(
        retained_again <= estimate,
        "a retaining re-index pass peaked at {} MB over the {} MB estimate",
        mb(retained_again),
        mb(estimate)
    );
}
