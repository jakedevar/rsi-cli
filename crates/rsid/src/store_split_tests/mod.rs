//! Tests that lived next to the store and moved out with the `rsid-store`
//! crate split (issue #1021 S4) because they exercise `rsid` modules (the
//! codegraph host, native tools, the RPC error mapping, provider capabilities,
//! the monitor, the vault-backed Pioneer credential). One shard per file.

mod codegraph;
mod misc;
mod scan;
