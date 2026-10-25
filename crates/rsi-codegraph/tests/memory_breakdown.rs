//! #3491 measurement: where does an index pass' live heap go on a real checkout?
//!
//! Ignored by default (it reads this whole checkout, ~130 MB of source). Run it
//! by exact name to print the breakdown the bound tests are derived from.

#![allow(clippy::expect_used, clippy::unwrap_used)] // Measurement fixture.

use std::alloc::{GlobalAlloc, Layout, System};
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering::Relaxed};

use rsi_codegraph::SourceFile;
use rsi_codegraph::extract::extract_file;
use rsi_codegraph::resolve::resolve_workspace;

struct Counting;
static LIVE: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);

fn grow(by: usize) {
    let live = LIVE.fetch_add(by, Relaxed) + by;
    PEAK.fetch_max(live, Relaxed);
}

// SAFETY: forwards to the system allocator; counters only observe sizes.
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

fn walk(dir: &Path, root: &Path, out: &mut Vec<SourceFile>) {
    let Ok(read) = std::fs::read_dir(dir) else {
        return;
    };
    let mut entries = read.flatten().collect::<Vec<_>>();
    entries.sort_by_key(std::fs::DirEntry::file_name);
    for entry in entries {
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().into_owned();
        let Ok(kind) = entry.file_type() else {
            continue;
        };
        if kind.is_dir() {
            if [".git", ".rsi", "target", "node_modules", "vendor", ".venv"]
                .contains(&name.as_str())
            {
                continue;
            }
            walk(&path, root, out);
        } else if kind.is_file()
            && path
                .extension()
                .and_then(|ext| ext.to_str())
                .is_some_and(|ext| matches!(ext, "rs" | "md" | "markdown" | "toml"))
        {
            let relative = path
                .strip_prefix(root)
                .unwrap()
                .to_string_lossy()
                .into_owned();
            out.push(SourceFile {
                relative_path: relative,
                bytes: std::fs::read(&path).unwrap(),
            });
        }
    }
}

fn mb(bytes: usize) -> f64 {
    #[allow(clippy::cast_precision_loss)]
    let value = bytes as f64 / (1024.0 * 1024.0);
    value
}

fn live() -> usize {
    LIVE.load(Relaxed)
}

#[test]
#[ignore = "reads the whole checkout; run by exact name to print the breakdown"]
fn breakdown_of_a_real_checkout() {
    let root = Path::new(rsi_common::manifest_dir!()).join("../..");
    let root = root.canonicalize().unwrap();
    let base = live();
    let mut files = Vec::new();
    walk(&root, &root, &mut files);
    let source = live() - base;
    let rs_bytes: usize = files
        .iter()
        .filter(|f| f.relative_path.ends_with(".rs"))
        .map(|f| f.bytes.len())
        .sum();
    println!(
        "breakdown files={} source_mb={:.1} rs_mb={:.1}",
        files.len(),
        mb(source),
        mb(rs_bytes)
    );

    PEAK.store(live(), Relaxed);
    let mut facts = Vec::with_capacity(files.len());
    for file in files {
        facts.push(extract_file(file));
    }
    println!(
        "breakdown after_extract_mb={:.1} peak_mb={:.1}",
        mb(live() - base),
        mb(PEAK.load(Relaxed) - base)
    );
    let mut rs = (0usize, 0usize, 0usize, 0usize);
    let mut other = (0usize, 0usize, 0usize);
    for f in &facts {
        let is_rs = f.file.relative_path.ends_with(".rs");
        let n = f.nodes.len();
        let r = f.relations.len();
        let u = f.unresolved_references.len();
        if is_rs {
            rs.0 += 1;
            rs.1 += n;
            rs.2 += r;
            rs.3 += u;
        } else {
            other.0 += n;
            other.1 += r;
            other.2 += u;
        }
    }
    println!(
        "breakdown rs files={} nodes={} rels={} unres={}",
        rs.0, rs.1, rs.2, rs.3
    );
    println!(
        "breakdown other nodes={} rels={} unres={}",
        other.0, other.1, other.2
    );

    // Component sizes, measured by dropping each and watching live bytes.
    let step = |label: &str, f: &mut dyn FnMut()| {
        let before = live();
        f();
        println!(
            "breakdown {label}_mb={:.1}",
            mb(before.saturating_sub(live()))
        );
    };
    resolve_workspace(&mut facts, None);
    println!(
        "breakdown after_resolve_mb={:.1} peak_mb={:.1}",
        mb(live() - base),
        mb(PEAK.load(Relaxed) - base)
    );
    step("scope", &mut || {
        for f in &mut facts {
            f.scope = rsi_codegraph::scope::RustScope::default();
        }
    });
    step("relations", &mut || {
        for f in &mut facts {
            f.relations = Vec::new();
        }
    });
    step("unresolved", &mut || {
        for f in &mut facts {
            f.unresolved_references = Vec::new();
        }
    });
    step("nodes_rs", &mut || {
        for f in facts
            .iter_mut()
            .filter(|f| f.file.relative_path.ends_with(".rs"))
        {
            f.nodes = Vec::new();
        }
    });
    step("nodes_other", &mut || {
        for f in &mut facts {
            f.nodes = Vec::new();
        }
    });
    step("bytes", &mut || {
        for f in &mut facts {
            f.file.bytes = Vec::new();
        }
    });
}

#[test]
#[ignore = "reads the whole checkout; run by exact name to print the streamed peak"]
fn streamed_pass_on_a_real_checkout() {
    use rsi_codegraph::invalidate::ExtractionCache;

    let root = Path::new(rsi_common::manifest_dir!()).join("../..");
    let root = root.canonicalize().unwrap();
    let spill = tempfile::tempdir().unwrap();
    let mut files = Vec::new();
    walk(&root, &root, &mut files);
    let source_bytes: usize = files.iter().map(|file| file.bytes.len()).sum();
    let base = live();
    PEAK.store(base, Relaxed);
    let mut cache = ExtractionCache::with_retain_budget(0);
    let mut update = cache.update_pass(files, None, spill.path()).unwrap();
    println!(
        "streamed source_mb={:.1} after_update_mb={:.1} update_peak_mb={:.1}",
        mb(source_bytes),
        mb(live().saturating_sub(base)),
        mb(PEAK.load(Relaxed).saturating_sub(base)),
    );
    let mut count = 0usize;
    update
        .facts
        .for_each(|_| {
            count += 1;
            Ok::<(), rsi_codegraph::CodegraphError>(())
        })
        .unwrap();
    println!(
        "streamed visited={count} total_peak_mb={:.1}",
        mb(PEAK.load(Relaxed).saturating_sub(base))
    );
}
