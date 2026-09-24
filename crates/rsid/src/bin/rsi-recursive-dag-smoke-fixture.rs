use rsid::error::{DaemonError, Result};
use rsid::recursive_dag::smoke_fixture::{SmokeFixtureOptions, generate_smoke_fixture};
use std::env;
use std::path::PathBuf;

fn main() {
    if let Err(error) = run() {
        eprintln!("{error}");
        std::process::exit(2);
    }
}

fn run() -> Result<()> {
    let args = env::args().skip(1).collect::<Vec<_>>();
    let options = parse_args(&args)?;
    let summary = generate_smoke_fixture(options)?;
    println!("{}", serde_json::to_string_pretty(&summary)?);
    Ok(())
}

fn parse_args(args: &[String]) -> Result<SmokeFixtureOptions> {
    if args.iter().any(|arg| arg == "--help" || arg == "-h") {
        print_usage();
        std::process::exit(0);
    }

    let mut source_db = None;
    let mut output_db = None;
    let mut output_home = None;
    let mut summary_json = None;
    let mut include_live_dogfood_graph = false;
    let mut index = 0;
    while index < args.len() {
        let flag = &args[index];
        if flag == "--include-live-dogfood-graph" {
            include_live_dogfood_graph = true;
            index += 1;
            continue;
        }
        let value = args.get(index + 1).ok_or_else(|| {
            DaemonError::InvalidParam(format!("missing value for argument {flag}"))
        })?;
        match flag.as_str() {
            "--source-db" => source_db = Some(PathBuf::from(value)),
            "--output-db" => output_db = Some(PathBuf::from(value)),
            "--output-home" => output_home = Some(PathBuf::from(value)),
            "--summary-json" => summary_json = Some(PathBuf::from(value)),
            other => {
                return Err(DaemonError::InvalidParam(format!(
                    "unknown argument: {other}"
                )));
            }
        }
        index += 2;
    }

    if output_db.is_some() && output_home.is_some() {
        return Err(DaemonError::InvalidParam(
            "use either --output-db or --output-home, not both".to_string(),
        ));
    }

    let mut options = if let Some(output_home) = output_home {
        let mut options = SmokeFixtureOptions::from_output_home(output_home);
        if let Some(summary_json) = summary_json {
            options.summary_json = summary_json;
        }
        options
    } else if let Some(output_db) = output_db {
        let summary_json = summary_json.unwrap_or_else(|| {
            output_db.parent().map_or_else(
                || PathBuf::from("recursive-dag-smoke-fixture.json"),
                |parent| parent.join("recursive-dag-smoke-fixture.json"),
            )
        });
        SmokeFixtureOptions::from_output_db(output_db, summary_json)
    } else {
        return Err(DaemonError::InvalidParam(
            "explicit --output-db or --output-home is required".to_string(),
        ));
    };

    if let Some(source_db) = source_db {
        options.source_db = Some(source_db);
    }
    options.include_live_dogfood_graph = include_live_dogfood_graph;
    Ok(options)
}

fn print_usage() {
    eprintln!(
        "Usage: rsi-recursive-dag-smoke-fixture [--source-db PATH] (--output-db PATH | --output-home PATH) [--summary-json PATH] [--include-live-dogfood-graph]\n\
         \n\
         Creates fixture-only recursive DAG smoke data in an explicit copied/generated output DB.\n\
         --include-live-dogfood-graph also seeds one ordinary live_session graph for intentional bounded dogfood.\n\
         Refuses the default production ~/.rsi/rsi.db path and never launches providers or sessions."
    );
}
