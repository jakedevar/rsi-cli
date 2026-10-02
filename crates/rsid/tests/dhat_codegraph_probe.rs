//! #960 measurement probe: heap held by one codegraph workspace cache.
//!
//! The daemon keeps one `WorkerState` (an `ExtractionCache`) per registered
//! codegraph workspace and re-indexes sandboxes as they change. This probe
//! replays that for one real checkout and reports the live bytes the cache
//! retains after the source bytes are dropped, the peak, and total churn.
//!
//! Run: RSI_DHAT_PROBE_ROOT=<checkout> cargo test --features dhat-heap -p rsid \
//!      --test dhat_codegraph_probe -- --ignored --nocapture

#![cfg(feature = "dhat-heap")]

use rsi_codegraph::SourceFile;
use rsi_codegraph::invalidate::ExtractionCache;
use std::path::Path;

#[global_allocator]
static ALLOC: dhat::Alloc = dhat::Alloc;

const IGNORED_DIRS: &[&str] = &[".git", ".rsi", "target", "node_modules", "vendor", ".venv"];

fn included(path: &Path) -> bool {
    path.file_name().is_some_and(|name| name == "Cargo.lock")
        || path
            .extension()
            .and_then(|ext| ext.to_str())
            .is_some_and(|ext| matches!(ext, "rs" | "md" | "markdown" | "toml"))
}

fn collect(root: &Path, dir: &Path, files: &mut Vec<SourceFile>) {
    let mut entries = std::fs::read_dir(dir)
        .expect("read_dir")
        .map(|entry| entry.expect("dir entry").path())
        .collect::<Vec<_>>();
    entries.sort();
    for path in entries {
        let meta = std::fs::symlink_metadata(&path).expect("metadata");
        if meta.is_dir() {
            let name = path
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or("");
            if !IGNORED_DIRS.contains(&name) {
                collect(root, &path, files);
            }
        } else if meta.is_file() && included(&path) {
            files.push(SourceFile {
                relative_path: path
                    .strip_prefix(root)
                    .expect("inside root")
                    .to_string_lossy()
                    .into_owned(),
                bytes: std::fs::read(&path).expect("read source"),
            });
        }
    }
}

fn mb(bytes: u64) -> f64 {
    bytes as f64 / (1024.0 * 1024.0)
}

#[test]
#[ignore]
fn codegraph_workspace_cache_heap() {
    let root = std::env::var_os("RSI_DHAT_PROBE_ROOT").expect("set RSI_DHAT_PROBE_ROOT");
    let root = Path::new(&root);
    let _profiler = dhat::Profiler::builder().testing().build();

    let mut files = Vec::new();
    collect(root, root, &mut files);
    let file_count = files.len();
    let source_bytes: usize = files.iter().map(|file| file.bytes.len()).sum();
    let mut cache = ExtractionCache::new();
    let after_read = dhat::HeapStats::get();
    cache.update(files).expect("cache update");
    let retained = dhat::HeapStats::get();

    // A second pass over unchanged bytes is what a re-index does.
    let mut again = Vec::new();
    collect(root, root, &mut again);
    cache.update(again).expect("second cache update");
    let second = dhat::HeapStats::get();

    println!(
        "probe files={file_count} source_mb={:.1} after_read_live_mb={:.1} \
         cache_retained_live_mb={:.1} peak_mb={:.1} total_allocated_mb={:.1} \
         after_reindex_live_mb={:.1} total_after_reindex_mb={:.1} allocations={}",
        mb(source_bytes as u64),
        mb(after_read.curr_bytes as u64),
        mb(retained.curr_bytes as u64),
        mb(retained.max_bytes as u64),
        mb(retained.total_bytes),
        mb(second.curr_bytes as u64),
        mb(second.total_bytes),
        second.total_blocks,
    );
    drop(cache);
}
