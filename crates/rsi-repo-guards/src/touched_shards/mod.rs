//! #426: staged Rust conversion of `scripts/check-touched-shards`.
//!
//! The public components are callable without Python, Cargo metadata, compilation
//! or helper execution. Complete filter derivation is deliberately unavailable
//! until the remaining observer/receipt contracts are ported. Canonical callers
//! must not switch to this staged entry point yet.
pub mod data;
pub mod derive;
pub mod diffseq;
pub mod downstream;
pub mod downstream_tests;
pub mod e2e;
pub mod filters;
pub mod fixture;
pub mod git;
pub mod guards;
pub mod identity;
pub mod leaf;
pub mod migration;
pub mod modes;
pub mod narrow;
pub mod readers;
pub mod relocation;
pub mod rpc;
pub mod seeds;
pub mod selection;
pub mod slow;
pub mod source;
pub mod sql_expr;
pub mod trace;
pub mod tree;

use crate::rsid_test_shards::{self, Shard};
use crate::{Ctx, cli};
use serde::Serialize;
use std::collections::{BTreeMap, BTreeSet};
use std::io::Write;

pub type ShardPackages = BTreeMap<String, Vec<String>>;

/// Supplied by #362's shard-inventory API. Do not execute its old Python helper,
/// guess a shard list, or duplicate that helper's manifest parser here.
#[derive(Clone, Debug, Serialize)]
pub struct ShardMapping {
    pub map: BTreeMap<String, Vec<String>>,
    pub fallback: Option<String>,
    pub dependent_shard: Option<String>,
}

/// Map already-expanded affected paths. Registration expansion is a separate
/// required step; the complete derivation entry point does not run without it.
pub fn map_shards(
    tree: &source::SourceTree,
    affected: &[String],
    inventory: &[Shard],
) -> Result<ShardMapping, String> {
    if inventory.is_empty() {
        return Err("shard package inventory is unavailable; integrate #362".into());
    }
    let mut owners = BTreeMap::new();
    for Shard { name, packages } in inventory {
        if packages.is_empty()
            || packages
                .iter()
                .any(|p| !rsid_test_shards::PACKAGES.contains(&p.as_str()))
            || owners.insert(name, packages).is_some()
        {
            return Err(format!("invalid shard package inventory: {name}"));
        }
    }
    let changed: Vec<_> = affected
        .iter()
        .filter(|p| source::lib_source(p))
        .cloned()
        .collect();
    let mut map = BTreeMap::<String, Vec<String>>::new();
    for path in &changed {
        for shard in tree.shards_for(path)? {
            if !owners.contains_key(&shard) {
                return Err(format!("source names undeclared shard: {shard}"));
            }
            map.entry(shard).or_default().push(path.clone());
        }
    }
    let mut fallback = None;
    if !changed.is_empty() && map.is_empty() {
        let packages: BTreeSet<_> = changed.iter().map(|p| source::package_of(p)).collect();
        let name = inventory
            .iter()
            .find(|s| s.packages.iter().any(|p| packages.contains(p.as_str())))
            .or_else(|| inventory.first())
            .ok_or("shard inventory unavailable")?
            .name
            .clone();
        map.insert(name.clone(), changed.clone());
        fallback = Some(name);
    }
    let dependent_shard = if changed.iter().any(|p| p.starts_with(source::STORE_SRC)) {
        inventory
            .iter()
            .find(|s| s.packages == ["rsid"])
            .map(|s| s.name.clone())
    } else {
        None
    };
    if let Some(name) = &dependent_shard {
        map.entry(name.clone()).or_default();
    }
    Ok(ShardMapping {
        map,
        fallback,
        dependent_shard,
    })
}

/// Remaining contracts are explicit rather than an empty-filter fallback that
/// could replace the queue's selection with an incomplete gate.
pub const UNMET_CONTRACTS: &[&str] = &[
    "the legacy suite's 244 test intents are not all ported, so the assembled derivation is not yet proven equal to the legacy selector",
    "daemon-attested exact-head acceptance and canonical caller cutover are owned by #423/#3621",
];

pub fn main(args: &[String], ctx: &Ctx, out: &mut dyn Write, err: &mut dyn Write) -> i32 {
    let opts: Vec<_> = [
        ("--base", true),
        ("--head", true),
        ("--repo", true),
        ("--compile", false),
        ("--no-compile", false),
        ("--filters-only", false),
        ("--run-filters", false),
        ("--filter", true),
        ("--json", false),
        ("--receipt-line", false),
        ("--receipt", true),
        ("--slow-tier-only", false),
        ("--write-slow-tier-filter", false),
    ]
    .into_iter()
    .map(|(name, value)| cli::Opt { name, value })
    .collect();
    let parsed = match cli::parse(
        "check-touched-shards",
        "[--base BASE] [--head HEAD] [--repo REPO] [--filters-only --json] [--slow-tier-only]",
        args,
        &opts,
        &[],
        Some(0),
        err,
    ) {
        Ok(parsed) => parsed,
        Err(exit) => return exit.code(),
    };
    let root = ctx.path(parsed.value("--repo").unwrap_or("."));
    if parsed.flag("--write-slow-tier-filter") {
        return match slow::write_slow_tier_filter(&root) {
            Ok(message) => {
                let _ = writeln!(out, "{message}");
                0
            }
            Err(error) => {
                let _ = writeln!(err, "check-touched-shards: {error}");
                1
            }
        };
    }
    let usage = |message: &str, err: &mut dyn Write| {
        let _ = writeln!(err, "check-touched-shards: error: {message}");
        2
    };
    if parsed.flag("--compile") && (parsed.flag("--no-compile") || parsed.flag("--filters-only")) {
        return usage("--compile contradicts --no-compile and --filters-only", err);
    }
    if ![
        "--compile",
        "--no-compile",
        "--filters-only",
        "--slow-tier-only",
    ]
    .iter()
    .any(|s| parsed.flag(s))
    {
        return usage(
            "compiling is an explicit verifier mode: pass --compile only inside a supervised job; to plan filters pass --filters-only (no cargo, seconds), or --no-compile",
            err,
        );
    }
    if parsed.flag("--filters-only") && parsed.flag("--run-filters") {
        return usage(
            "--filters-only does not run filters; drop --run-filters",
            err,
        );
    }
    if parsed.flag("--slow-tier-only") {
        let result = (|| -> std::io::Result<_> {
            let repo = git::Repository::new(&root)?;
            let diff = git::DiffContext::load(
                &repo,
                parsed.value("--base").unwrap_or("origin/rolling"),
                parsed.value("--head").unwrap_or("HEAD"),
            )?;
            Ok(slow::decision(
                &repo,
                &diff.merge_base,
                &diff.head,
                &diff.changed,
            ))
        })();
        return match result {
            Ok(decision) => {
                let _ = writeln!(out, "{}", modes::dumps_sorted(&decision));
                0
            }
            Err(error) => {
                let _ = writeln!(err, "check-touched-shards: {error}");
                2
            }
        };
    }
    // Derivation is staged: until the legacy suite's intents are all ported, it
    // runs only on an explicit opt-in. No receipt of any shape is emitted
    // otherwise, even if --json/--receipt/--receipt-line was requested.
    if ctx.var("RSI_TOUCHED_SHARDS_STAGED") != Some("1") {
        let _ = writeln!(
            err,
            "check-touched-shards: Rust derivation is staged; incomplete_contract: {}. Canonical callers must remain unchanged; integrate #426 before requesting filters.",
            UNMET_CONTRACTS.join("; ")
        );
        return 2;
    }
    let extra: Vec<String> = {
        let mut found = Vec::new();
        let mut iter = args.iter();
        while let Some(arg) = iter.next() {
            if arg == "--filter" {
                found.extend(iter.next().cloned());
            } else if let Some(value) = arg.strip_prefix("--filter=") {
                found.push(value.to_owned());
            }
        }
        found
    };
    let options = derive::Options {
        filters_only: parsed.flag("--filters-only"),
        run_filters: parsed.flag("--run-filters"),
        compile: parsed.flag("--compile"),
        no_compile: parsed.flag("--no-compile") || parsed.flag("--filters-only"),
        extra_filters: extra,
        receipt: parsed.value("--receipt").map(|p| ctx.path(p)),
        receipt_line: parsed.flag("--receipt-line"),
        env: Some(
            ctx.vars()
                .map(|(k, v)| (k.to_owned(), v.to_owned()))
                .collect(),
        ),
        interrupt: None,
    };
    let loaded = match derive::Loaded::load(
        &root,
        parsed.value("--base").unwrap_or("origin/rolling"),
        parsed.value("--head").unwrap_or("HEAD"),
    ) {
        Ok(loaded) => loaded,
        Err(failure) => return report_failure(failure, &options, out, err),
    };
    match derive::derive(&loaded, &options) {
        Ok(derived) => {
            for line in &derived.stderr {
                let _ = writeln!(err, "{line}");
            }
            if let Some(path) = &options.receipt {
                let _ = modes::write_receipt(path, &derived.receipt);
            }
            if !parsed.flag("--json") {
                let _ = writeln!(out, "{}", derived.summary);
            }
            if options.receipt_line {
                let _ = writeln!(
                    out,
                    "RECEIPT_JSON {}",
                    modes::python_json(&derived.receipt, false)
                );
            } else {
                let _ = writeln!(out, "{}", modes::python_json(&derived.receipt, true));
            }
            i32::from(derived.failed)
        }
        Err(failure) => report_failure(failure, &options, out, err),
    }
}

fn report_failure(
    failure: derive::Failure,
    options: &derive::Options,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> i32 {
    match failure {
        derive::Failure::Refusal(message) => {
            let _ = writeln!(err, "check-touched-shards: {message}");
            2
        }
        derive::Failure::Fatal(message) => {
            let _ = writeln!(err, "{message}");
            1
        }
        derive::Failure::Interrupted {
            code,
            partial,
            message,
        } => {
            let _ = writeln!(err, "{message}");
            if options.receipt_line {
                let _ = writeln!(out, "RECEIPT_JSON {}", modes::python_json(&partial, false));
            }
            code
        }
    }
}

#[cfg(test)]
mod cli_tests;
#[cfg(test)]
mod continuation_tests;
#[cfg(test)]
mod derive_tests;
#[cfg(test)]
mod downstream_public_items_tests;
#[cfg(test)]
mod field_tests;
#[cfg(test)]
mod leaf_tests;
#[cfg(test)]
mod reader_tests;
#[cfg(test)]
mod relocation_tests;
#[cfg(test)]
mod rpc_tests;
#[cfg(test)]
mod slow_checkout_tests;
#[cfg(test)]
mod tests;
#[cfg(test)]
mod unit_tests;
