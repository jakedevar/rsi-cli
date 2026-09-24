use std::path::PathBuf;

use rsid::provider_capability_validation::{
    ValidationInput, render_provider_capability_documentation, validate_repository,
};

fn main() {
    if let Err(error) = run() {
        eprintln!("{error}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), String> {
    let argument = std::env::args().nth(1);
    if argument.as_deref() == Some("--print") {
        print!("{}", render_provider_capability_documentation()?);
        return Ok(());
    }
    let repo_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(|path| path.parent())
        .ok_or_else(|| "failed to resolve repository root".to_string())?
        .to_path_buf();
    let report = validate_repository(ValidationInput::production(repo_root))?;
    match argument.as_deref() {
        None | Some("--offline") => {}
        Some("--inventory") => println!(
            "{}",
            serde_json::to_string_pretty(&report.inventory)
                .map_err(|error| format!("failed to render inventory: {error}"))?
        ),
        Some(argument) => {
            return Err(format!(
                "unknown argument `{argument}`; expected --offline, --print, or --inventory"
            ));
        }
    }
    Ok(())
}
