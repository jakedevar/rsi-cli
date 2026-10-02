//! Print conservatively selected landing tests for a source range.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::Command;

use rsi_codegraph::impact::{
    ChangedFile, GraphImpactSource, ReaderIndex, ReaderSource, WorkspaceImpactMetadata,
    select_tests_with_readers,
};
use serde_json::Value;
use uuid::Uuid;

const USAGE: &str = "usage: rsi-test-impact --repo PATH (--base BASE --head HEAD | --path PATH ...) [--graph PATH --workspace-id UUID]";

#[derive(Default)]
struct Options {
    repo: Option<PathBuf>,
    base: Option<String>,
    head: Option<String>,
    paths: Vec<String>,
    graph: Option<PathBuf>,
    workspace_id: Option<Uuid>,
}

fn main() {
    if let Err(error) = run() {
        eprintln!("rsi-test-impact: {error}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), String> {
    let options = parse_args()?;
    let repo = options.repo.as_deref().unwrap_or_else(|| Path::new("."));
    let changed_paths = if let (Some(base), Some(head)) = (options.base.as_deref(), options.head.as_deref()) {
        changed_paths(repo, base, head)?
    } else if options.base.is_none() && options.head.is_none() {
        options.paths.clone()
    } else {
        return Err("supply both --base and --head, or only --path arguments".into());
    };
    if changed_paths.is_empty() {
        return Err("no changed paths".into());
    }
    let changed_files = changed_paths
        .into_iter()
        .map(|path| {
            let content = if options.head.is_some() {
                head_content(repo, options.head.as_deref().expect("checked above"), &path)?
            } else {
                std::fs::read(repo.join(&path)).ok()
            };
            Ok::<_, String>(ChangedFile {
                path,
                content,
            })
        })
        .collect::<Result<Vec<_>, _>>()?;
    let metadata = workspace_metadata(repo)?;
    let readers = reader_index(repo, options.head.as_deref())?;
    let selection = if let (Some(graph), Some(workspace_id)) = (options.graph.as_deref(), options.workspace_id) {
        let project_id = graph
            .parent()
            .and_then(Path::file_name)
            .and_then(|value| value.to_str())
            .and_then(|value| value.parse::<Uuid>().ok())
            .ok_or_else(|| format!("{} does not live in a project-id directory", graph.display()))?;
        let store = rsi_codegraph::CodegraphStore::open(graph, project_id)
            .map_err(|error| format!("cannot open codegraph: {error}"))?;
        select_tests_with_readers(
            &changed_files,
            &metadata,
            Some(GraphImpactSource {
                store: &store,
                workspace_id,
            }),
            Some(&readers),
        )
        .map_err(|error| format!("codegraph impact query failed: {error}"))?
    } else {
        select_tests_with_readers(&changed_files, &metadata, None, Some(&readers)).map_err(|error| format!("impact selection failed: {error}"))?
    };

    println!("Lander arguments:");
    if selection.lander_arguments().is_empty() {
        println!("  (none; affected package gates run unfiltered)");
    } else {
        for argument in selection.lander_arguments() {
            println!("  {}", argument.join(" "));
        }
    }
    for note in &selection.notes {
        println!("Note: {note}");
    }
    println!("Cargo test commands:");
    for command in selection.cargo_test_commands() {
        println!("  {}", command.join(" "));
    }
    println!("JSON:");
    println!(
        "{}",
        serde_json::to_string_pretty(&selection).map_err(|error| error.to_string())?
    );
    Ok(())
}

fn parse_args() -> Result<Options, String> {
    let mut args = std::env::args().skip(1);
    let mut options = Options::default();
    while let Some(argument) = args.next() {
        let mut value = || {
            args.next()
                .ok_or_else(|| format!("{argument} requires a value"))
        };
        match argument.as_str() {
            "--repo" => options.repo = Some(PathBuf::from(value()?)),
            "--base" => options.base = Some(value()?),
            "--head" => options.head = Some(value()?),
            "--path" => options.paths.push(value()?),
            "--graph" => options.graph = Some(PathBuf::from(value()?)),
            "--workspace-id" => options.workspace_id = Some(value()?.parse().map_err(|_| "invalid workspace UUID")?),
            "-h" | "--help" => {
                println!("{USAGE}");
                std::process::exit(0);
            }
            _ => return Err(format!("unknown argument {argument}\n{USAGE}")),
        }
    }
    if options.base.is_some() != options.head.is_some() {
        return Err(format!("--base and --head are paired\n{USAGE}"));
    }
    if !options.paths.is_empty() && options.base.is_some() {
        return Err(format!("--path cannot be combined with --base/--head\n{USAGE}"));
    }
    if options.workspace_id.is_some() != options.graph.is_some() {
        return Err(format!("--workspace-id requires --graph\n{USAGE}"));
    }
    Ok(options)
}

fn changed_paths(repo: &Path, base: &str, head: &str) -> Result<Vec<String>, String> {
    let output = git(repo, &["diff", "--name-only", "-z", base, head])?;
    let paths: BTreeSet<String> = output
        .split('\0')
        .filter(|path| !path.is_empty())
        .map(str::to_owned)
        .collect();
    Ok(paths.into_iter().collect())
}

/// Every Rust file under `crates/` at `head` (or the working tree), for the
/// non-Rust reader map.
fn reader_index(repo: &Path, head: Option<&str>) -> Result<ReaderIndex, String> {
    let listing = match head {
        Some(head) => git(repo, &["ls-tree", "-r", "--name-only", head, "--", "crates"])?,
        None => git(repo, &["ls-files", "--", "crates"])?,
    };
    let mut sources = Vec::new();
    for path in listing.lines().filter(|path| path.ends_with(".rs")) {
        let bytes = match head {
            Some(head) => git_show(repo, head, path),
            None => std::fs::read(repo.join(path)).ok(),
        };
        if let Some(bytes) = bytes {
            sources.push(ReaderSource {
                path: path.to_owned(),
                content: String::from_utf8_lossy(&bytes).into_owned(),
            });
        }
    }
    Ok(ReaderIndex::new(sources))
}

fn git_show(repo: &Path, head: &str, path: &str) -> Option<Vec<u8>> {
    let output = Command::new("git")
        .args(["show", &format!("{head}:{path}")])
        .current_dir(repo)
        .output()
        .ok()?;
    output.status.success().then_some(output.stdout)
}

fn head_content(repo: &Path, head: &str, path: &str) -> Result<Option<Vec<u8>>, String> {
    let exists = Command::new("git")
        .args(["cat-file", "-e", &format!("{head}:{path}")])
        .current_dir(repo)
        .output()
        .map_err(|error| format!("cannot inspect git object: {error}"))?;
    if !exists.status.success() {
        return Ok(None);
    }
    let output = Command::new("git")
        .args(["show", &format!("{head}:{path}")])
        .current_dir(repo)
        .output()
        .map_err(|error| format!("cannot read {path}: {error}"))?;
    if !output.status.success() {
        return Err(format!(
            "cannot read {path}: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(Some(output.stdout))
}

fn git(repo: &Path, args: &[&str]) -> Result<String, String> {
    let output = Command::new("git")
        .args(args)
        .current_dir(repo)
        .output()
        .map_err(|error| format!("cannot run git: {error}"))?;
    if !output.status.success() {
        return Err(format!(
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    String::from_utf8(output.stdout).map_err(|error| format!("git output is not UTF-8: {error}"))
}

fn workspace_metadata(repo: &Path) -> Result<WorkspaceImpactMetadata, String> {
    let output = Command::new("cargo")
        .args([
            "metadata",
            "--no-deps",
            "--format-version",
            "1",
            "--offline",
        ])
        .current_dir(repo)
        .output()
        .map_err(|error| format!("cannot inspect workspace: {error}"))?;
    if !output.status.success() {
        return Err(format!(
            "cannot inspect workspace: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    let value: Value = serde_json::from_slice(&output.stdout)
        .map_err(|error| format!("invalid workspace metadata: {error}"))?;
    serde_json::from_value(value).map_err(|error| format!("invalid workspace metadata: {error}"))
}
