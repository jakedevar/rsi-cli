//! `rsi export --clean <bundle>` and `rsi init --from <bundle>` (#1406).
//!
//! Both are thin operator clients of the daemon's operator-only
//! `ExportPortableBundle` / `ImportPortableBundle` RPCs; the bundle logic lives
//! in the store. `init` first asks the daemon for a dry run, prompts for a new
//! path for every project whose path does not exist on this machine, then
//! imports for real. Credentials never travel: the operator enters them here.

use crate::client::DaemonClient;
use serde_json::Value;
use std::io::{BufRead, IsTerminal, Write};
use std::path::{Path, PathBuf};

pub const USAGE: &str = "\
usage:
  rsi export --clean <bundle> [--overwrite]
      Write this install's durable state (projects, settings, policies,
      Issues) without history or credentials to <bundle>.
  rsi init --from <bundle> [--remap OLD=NEW]... [--merge] [--yes]
      Start this machine from <bundle>: the daemon builds its database
      through the normal migrations and imports the bundle in one
      transaction. --remap rewrites project paths under OLD to NEW; you are
      asked for any project path that does not exist here unless --yes.
      --merge imports into a database that already holds state.";

/// One parsed portable subcommand.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PortableCommand {
    Export {
        bundle: PathBuf,
        overwrite: bool,
    },
    Init {
        bundle: PathBuf,
        remaps: Vec<(String, String)>,
        merge: bool,
        assume_yes: bool,
    },
}

/// Parse `args` (without the program name). `None` when the first argument
/// is not a portable subcommand, so the TUI starts as before.
#[must_use]
pub fn parse(args: &[String]) -> Option<Result<PortableCommand, String>> {
    let (command, rest) = args.split_first()?;
    match command.as_str() {
        "export" => Some(parse_export(rest)),
        "init" => Some(parse_init(rest)),
        _ => None,
    }
}

fn parse_export(args: &[String]) -> Result<PortableCommand, String> {
    let mut clean = false;
    let mut overwrite = false;
    let mut bundle = None;
    for arg in args {
        match arg.as_str() {
            "--clean" => clean = true,
            "--overwrite" => overwrite = true,
            flag if flag.starts_with('-') => return Err(format!("unknown flag `{flag}`")),
            path if bundle.is_none() => bundle = Some(PathBuf::from(path)),
            extra => return Err(format!("unexpected argument `{extra}`")),
        }
    }
    if !clean {
        return Err(
            "`rsi export` needs --clean: the bundle leaves history and credentials behind"
                .to_string(),
        );
    }
    Ok(PortableCommand::Export {
        bundle: bundle.ok_or("missing <bundle> path")?,
        overwrite,
    })
}

fn parse_init(args: &[String]) -> Result<PortableCommand, String> {
    let mut bundle = None;
    let mut remaps = Vec::new();
    let mut merge = false;
    let mut assume_yes = false;
    let mut args = args.iter();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--from" => {
                bundle = Some(PathBuf::from(
                    args.next().ok_or("--from needs a <bundle> path")?,
                ));
            }
            "--remap" => {
                let pair = args.next().ok_or("--remap needs OLD=NEW")?;
                let (from, to) = pair
                    .split_once('=')
                    .filter(|(from, to)| !from.is_empty() && !to.is_empty())
                    .ok_or_else(|| format!("--remap `{pair}` is not OLD=NEW"))?;
                remaps.push((from.to_string(), to.to_string()));
            }
            "--merge" => merge = true,
            "--yes" | "-y" => assume_yes = true,
            other => return Err(format!("unexpected argument `{other}`")),
        }
    }
    Ok(PortableCommand::Init {
        bundle: bundle.ok_or("`rsi init` needs --from <bundle>")?,
        remaps,
        merge,
        assume_yes,
    })
}

/// `path` made absolute against the current directory (the daemon refuses a
/// relative path; it may run in another directory).
fn absolute(path: &Path) -> Result<String, String> {
    std::path::absolute(path)
        .map(|path| path.to_string_lossy().into_owned())
        .map_err(|error| format!("cannot resolve {}: {error}", path.display()))
}

fn remaps_json(remaps: &[(String, String)]) -> Value {
    Value::Array(
        remaps
            .iter()
            .map(|(from, to)| serde_json::json!({ "from": from, "to": to }))
            .collect(),
    )
}

/// Projects from an import report whose path does not exist on this machine:
/// `(name, bundle path, path after remaps)`.
#[must_use]
pub fn missing_project_paths(report: &Value) -> Vec<(String, String, String)> {
    report["projects"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|project| {
            let path = project["path"].as_str()?;
            if Path::new(path).exists() {
                return None;
            }
            let original = project["remapped_from"].as_str().unwrap_or(path);
            Some((
                project["name"].as_str().unwrap_or_default().to_string(),
                original.to_string(),
                path.to_string(),
            ))
        })
        .collect()
}

fn print_counts(label: &str, counts: &Value) {
    if let Some(counts) = counts.as_object().filter(|counts| !counts.is_empty()) {
        let line = counts
            .iter()
            .map(|(table, count)| format!("{table} {count}"))
            .collect::<Vec<_>>()
            .join(", ");
        println!("  {label}: {line}");
    }
}

fn prompt(question: &str) -> Result<String, String> {
    print!("{question}");
    std::io::stdout()
        .flush()
        .map_err(|error| error.to_string())?;
    let mut answer = String::new();
    std::io::stdin()
        .lock()
        .read_line(&mut answer)
        .map_err(|error| error.to_string())?;
    Ok(answer.trim().to_string())
}

/// Connect to the daemon, starting it once with `start` when it is not up.
async fn connect(
    client: &mut DaemonClient,
    start: impl FnOnce() -> Result<(), String>,
) -> Result<(), String> {
    if client.connect().await.is_ok() {
        return Ok(());
    }
    start()?;
    client.connect().await.map_err(|error| {
        format!(
            "cannot reach rsid at {}: {error}",
            client.socket_path().display()
        )
    })
}

/// Run one portable subcommand against the daemon.
///
/// # Errors
///
/// A message for the operator (connection, refusal or I/O).
pub async fn run(
    command: PortableCommand,
    client: &mut DaemonClient,
    start_daemon: impl FnOnce() -> Result<(), String>,
) -> Result<(), String> {
    connect(client, start_daemon).await?;
    match command {
        PortableCommand::Export { bundle, overwrite } => {
            let path = absolute(&bundle)?;
            let result = client
                .export_portable_bundle(&path, overwrite)
                .await
                .map_err(|error| error.to_string())?;
            println!("Wrote a clean bundle to {path}");
            let summary = &result["summary"];
            print_counts("carried", &summary["rows"]);
            print_counts("left behind", &summary["left_behind"]);
            if let Some(withheld) = summary["withheld_settings"].as_array() {
                println!(
                    "  withheld settings (runtime state or secret-named): {}",
                    withheld.len()
                );
            }
            println!("  credentials: none (enter them on the new machine)");
            Ok(())
        }
        PortableCommand::Init {
            bundle,
            mut remaps,
            merge,
            assume_yes,
        } => {
            let path = absolute(&bundle)?;
            let preview = client
                .import_portable_bundle(&path, merge, remaps_json(&remaps), true)
                .await
                .map_err(|error| error.to_string())?;
            let missing = missing_project_paths(&preview["report"]);
            let interactive = !assume_yes && std::io::stdin().is_terminal();
            for (name, original, current) in &missing {
                if interactive {
                    let answer = prompt(&format!(
                        "Project `{name}` was at {original}, which does not exist here.\n  New path (Enter keeps {current}): "
                    ))?;
                    if !answer.is_empty() {
                        // An exact remap of this project, ahead of any prefix.
                        remaps.insert(0, (original.clone(), answer));
                    }
                } else {
                    eprintln!("warning: project `{name}` path {current} does not exist here");
                }
            }
            let result = client
                .import_portable_bundle(&path, merge, remaps_json(&remaps), false)
                .await
                .map_err(|error| error.to_string())?;
            let report = &result["report"];
            println!(
                "Imported {path} (bundle schema v{}, this schema v{})",
                report["bundle_schema_version"], report["schema_version"]
            );
            print_counts("inserted", &report["inserted"]);
            print_counts("skipped (already present)", &report["skipped"]);
            if let Some(projects) = report["projects"].as_array() {
                for project in projects {
                    println!(
                        "  project {}: {}",
                        project["name"].as_str().unwrap_or_default(),
                        project["path"].as_str().unwrap_or("(no path)")
                    );
                }
            }
            if let Some(templates) = result["manager_templates_path"].as_str() {
                println!(
                    "  manager policy templates (not live; reuse when appointing): {templates}"
                );
            }
            println!(
                "No credentials were imported. Enter them in the rsi TUI (Settings, provider credentials) before launching sessions."
            );
            println!("Restart rsid so the imported settings take effect.");
            Ok(())
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|arg| (*arg).to_string()).collect()
    }

    #[test]
    fn non_portable_arguments_start_the_tui() {
        assert_eq!(parse(&args(&[])), None);
        assert_eq!(parse(&args(&["--version"])), None);
    }

    #[test]
    fn export_requires_clean_and_a_path() {
        assert_eq!(
            parse(&args(&["export", "--clean", "b.json"])),
            Some(Ok(PortableCommand::Export {
                bundle: PathBuf::from("b.json"),
                overwrite: false
            }))
        );
        assert!(parse(&args(&["export", "b.json"])).unwrap().is_err());
        assert!(parse(&args(&["export", "--clean"])).unwrap().is_err());
    }

    #[test]
    fn init_parses_remaps_merge_and_yes() {
        assert_eq!(
            parse(&args(&[
                "init",
                "--from",
                "b.json",
                "--remap",
                "/old=/new",
                "--merge",
                "--yes"
            ])),
            Some(Ok(PortableCommand::Init {
                bundle: PathBuf::from("b.json"),
                remaps: vec![("/old".to_string(), "/new".to_string())],
                merge: true,
                assume_yes: true,
            }))
        );
        assert!(parse(&args(&["init"])).unwrap().is_err());
        assert!(
            parse(&args(&["init", "--from", "b", "--remap", "nope"]))
                .unwrap()
                .is_err()
        );
    }

    #[test]
    fn missing_project_paths_lists_only_absent_paths() {
        let here = std::env::temp_dir().to_string_lossy().into_owned();
        let report = serde_json::json!({"projects": [
            {"name": "present", "path": here, "remapped_from": null},
            {"name": "gone", "path": "/definitely/not/here/rsi", "remapped_from": "/old/rsi"},
            {"name": "pathless", "path": null, "remapped_from": null},
        ]});
        assert_eq!(
            missing_project_paths(&report),
            vec![(
                "gone".to_string(),
                "/old/rsi".to_string(),
                "/definitely/not/here/rsi".to_string()
            )]
        );
    }
}
