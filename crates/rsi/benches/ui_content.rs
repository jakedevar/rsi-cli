//! Phase 0.3 — UI content parse bench.
//!
//! Three hot fns from `crates/rsi/src/ui/content.rs`:
//! - `parse_content`        (line 27)  — code-fence segmentation
//! - `render_markdown_line` (line 208) — block + inline → ratatui Line
//! - `parse_inline_markdown`(line 690) — inline emphasis / code / link spans
//!
//! Hermetic: pure CPU on static markdown samples.

use criterion::{Criterion, black_box, criterion_group, criterion_main};
use rsi::ui::content::{
    BlockElement, detect_block_element, parse_content, parse_inline_markdown, render_markdown_line,
};

const SMALL_MD: &str = "Hello **world**, this is a `single` line of *markdown*.";

const MEDIUM_MD: &str = r#"# Header

A paragraph with **bold** and *italic* and `inline code` and a [link](https://example.com).

- list item one
- list item two with **emphasis**
- [ ] task unchecked
- [x] task checked

```rust
fn main() {
    println!("hello");
}
```

> a blockquote with __underscore bold__ and a `code` span

| col a | col b |
|-------|-------|
| 1     | 2     |
"#;

// Synthesize a large doc by repeating MEDIUM_MD 32× — exercises code-fence
// state machine across many segments without inflating the source file.
fn make_large_md() -> String {
    let mut s = String::with_capacity(MEDIUM_MD.len() * 32);
    for _ in 0..32 {
        s.push_str(MEDIUM_MD);
        s.push('\n');
    }
    s
}

fn bench_parse_content(c: &mut Criterion) {
    let large = make_large_md();
    let mut group = c.benchmark_group("ui_content/parse_content");
    group.bench_function("small", |b| {
        b.iter(|| {
            let segs = parse_content(black_box(SMALL_MD));
            black_box(segs);
        });
    });
    group.bench_function("medium", |b| {
        b.iter(|| {
            let segs = parse_content(black_box(MEDIUM_MD));
            black_box(segs);
        });
    });
    group.bench_function("large", |b| {
        b.iter(|| {
            let segs = parse_content(black_box(large.as_str()));
            black_box(segs);
        });
    });
    group.finish();
}

fn bench_parse_inline_markdown(c: &mut Criterion) {
    // Lines representative of real assistant output.
    let lines: &[&str] = &[
        "plain text with no markup at all",
        "Hello **world**, this is a `single` line of *markdown*.",
        "Mix of **bold**, *italic*, `code`, and [link](https://example.com/foo).",
        "Edge case: __underscore bold__ and unmatched *italic and `code without close",
    ];
    let mut group = c.benchmark_group("ui_content/parse_inline_markdown");
    for (i, line) in lines.iter().enumerate() {
        group.bench_function(format!("line_{i}"), |b| {
            b.iter(|| {
                let spans = parse_inline_markdown(black_box(line));
                black_box(spans);
            });
        });
    }
    group.finish();
}

fn bench_render_markdown_line(c: &mut Criterion) {
    // Pre-compute (block, content) pairs once; bench measures only render path.
    let lines: Vec<&str> = MEDIUM_MD.lines().collect();
    let parsed: Vec<(BlockElement, &str)> = lines.iter().map(|l| detect_block_element(l)).collect();

    c.bench_function("ui_content/render_markdown_line/medium_doc", |b| {
        b.iter(|| {
            for (block, content) in parsed.iter() {
                let line =
                    render_markdown_line(black_box(""), black_box(block), black_box(content));
                black_box(line);
            }
        });
    });
}

criterion_group!(
    benches,
    bench_parse_content,
    bench_parse_inline_markdown,
    bench_render_markdown_line
);
criterion_main!(benches);
