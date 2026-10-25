//! Retained daemon facades and exact sharded observers of leaf crate edits.
use super::{
    filters,
    git::Hunk,
    identity::ItemReferences,
    seeds, selection, slow,
    source::{self, SourceTree},
    trace::{self, Bounds, Trace},
};
use std::collections::BTreeSet;

#[derive(Debug)]
pub struct FacadeSelection {
    pub filter: String,
    pub tests: usize,
    pub broad: bool,
}

#[derive(Debug)]
pub struct LeafObservers {
    pub path: String,
    pub package: String,
    pub filters: Vec<String>,
    /// The final assembler must withdraw *all* gate filters when any leaf path
    /// is unfiltered. Its own package filters still belong in worker_filters.
    pub unfiltered: bool,
    pub reason: String,
    pub trace: Option<Trace>,
    /// Final receipt/worker budgeting must retain these broad-facade records.
    pub facades: Vec<FacadeSelection>,
}

pub fn daemon_observers(
    sources: &SourceTree,
    old: &SourceTree,
    index: &mut ItemReferences,
    path: &str,
    hunks: &[Hunk],
    comment_only: bool,
    bounds: Bounds,
) -> Result<Option<LeafObservers>, String> {
    let Some(package) = source::leaf_package(path) else {
        return Ok(None);
    };
    if comment_only {
        return Ok(None);
    }
    let before = old.texts.get(path).map_or("", String::as_str);
    let after = sources.texts.get(path).map_or("", String::as_str);
    let mut result = LeafObservers {
        path: path.into(),
        package: package.clone(),
        filters: Vec::new(),
        unfiltered: false,
        reason: String::new(),
        trace: None,
        facades: Vec::new(),
    };
    if slow::only_test_lines(before, &selection::changed_lines(hunks, true), &old.host)
        && slow::only_test_lines(
            after,
            &selection::changed_lines(hunks, false),
            &sources.host,
        )
    {
        result.reason = "only the leaf's test code changed: its own tests observe it".into();
        return Ok(Some(result));
    }
    let traced = seeds::changed_seed_proof(sources, old, index, path, hunks)
        .and_then(|proof| trace::trace_proof(index, &proof, bounds));
    let trace = match traced {
        Ok(trace) => trace,
        Err(reason) => {
            result.unfiltered = true;
            result.reason = format!(
                "production edit: the trace to the daemon tests is not provable ({reason}), so no filter is derived and the lander's complete closure over the leaf and rsid sources selects the observers"
            );
            return Ok(Some(result));
        }
    };
    let Some(picked) = &trace.tests else {
        result.unfiltered = true;
        result.reason = format!(
            "production edit: the trace to the daemon tests is not provable ({}), so no filter is derived and the lander's complete closure over the leaf and rsid sources selects the observers",
            trace.diagnostic.as_deref().unwrap_or("unresolved")
        );
        result.trace = Some(trace);
        return Ok(Some(result));
    };
    let facades: &[&str] = match package.as_str() {
        "rsid-remote-read" => &["remote_read"],
        "rsid-harness" => &["session::harness"],
        "rsi-graph" => &["topology", "session::graph_runner"],
        _ => return Err("leaf package has no retained facade contract".into()),
    };
    if package != "rsi-graph" {
        for facade in facades {
            let directory = format!("{}{}", source::RSID_SRC, facade.replace("::", "/"));
            if let Some(path) = [format!("{directory}/mod.rs"), format!("{directory}.rs")]
                .iter()
                .find(|p| sources.texts.contains_key(*p))
            {
                for shard in sources.shards_for(path)? {
                    let tests: Vec<_> = index
                        .functions
                        .iter()
                        .filter(|f| {
                            source::is_test(&f.function.attrs)
                                && !source::shards_in(&f.function.attrs).is_empty()
                        })
                        .collect();
                    let inside = |module: &str| {
                        module == *facade || module.starts_with(&format!("{facade}::"))
                    };
                    let foreign = tests.iter().any(|f| {
                        format!("{}::{}", f.effective, f.function.name).contains(facade)
                            && !inside(&f.effective)
                    });
                    let scope = if foreign {
                        filters::anchored_filterset(facade)
                    } else {
                        format!("test({facade})")
                    };
                    let count = tests
                        .iter()
                        .filter(|f| {
                            if foreign {
                                inside(&f.effective)
                            } else {
                                format!("{}::{}", f.effective, f.function.name).contains(facade)
                            }
                        })
                        .map(|f| source::shards_in(&f.function.attrs).len())
                        .sum();
                    let filter = format!("rsid=shard:{shard}:{scope}");
                    result.filters.push(filter.clone());
                    result.facades.push(FacadeSelection {
                        filter,
                        tests: count,
                        broad: count > filters::BROAD_TEST_LIMIT,
                    });
                }
            }
        }
    }
    result.filters.extend(filters::traced_filters(picked));
    let mut seen = BTreeSet::new();
    result.filters.retain(|f| seen.insert(f.clone()));
    result.reason = format!(
        "production edit: {}{}",
        if package == "rsi-graph" {
            String::new()
        } else {
            format!(
                "the {} facade tests plus ",
                facades
                    .iter()
                    .map(|f| format!("`{f}`"))
                    .collect::<Vec<_>>()
                    .join(" and ")
            )
        },
        if picked.is_empty() {
            "no other sharded test named the change".into()
        } else {
            format!(
                "{} sharded tests that name the change",
                picked
                    .iter()
                    .map(|o| (&o.module, &o.name))
                    .collect::<BTreeSet<_>>()
                    .len()
            )
        }
    );
    result.trace = Some(trace);
    Ok(Some(result))
}
