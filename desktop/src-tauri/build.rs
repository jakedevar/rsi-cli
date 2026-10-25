use std::path::Path;
use std::process::Command;

fn main() {
    // Frontend assets (`../dist`, see tauri.conf.json `frontendDist`) are
    // embedded at compile time. Without this, cargo does not notice a
    // frontend-only change (CSS, a new module) and ships a stale UI.
    println!("cargo:rerun-if-changed=../dist");
    generate_handlers();
    tauri_build::build()
}

/// `src/handlers.generated.rs` (the invoke handler lib.rs includes) and
/// `src/modules.generated.rs` (its `pub mod` list, #181) are build products, not
/// committed (#60): regenerate them from the src/*.rs modules and every
/// `#[tauri::command]` in them with the one generator the frontend build uses,
/// so a plain `cargo build`/`cargo test` sees the modules and commands a merge
/// brought in.
fn generate_handlers() {
    println!("cargo:rerun-if-changed=src");
    println!("cargo:rerun-if-changed=../scripts/slices.mjs");
    let generated = [
        Path::new("src/handlers.generated.rs"),
        Path::new("src/modules.generated.rs"),
    ];
    match Command::new("node").arg("../scripts/slices.mjs").status() {
        Ok(status) if status.success() => {}
        Ok(status) => {
            panic!("node ../scripts/slices.mjs failed ({status}); its message names the fix")
        }
        Err(err) if generated.iter().all(|p| p.exists()) => {
            println!(
                "cargo:warning=node is unavailable ({err}); using the existing src/*.generated.rs, which may be stale: run `npm run build` in desktop/"
            );
        }
        Err(err) => panic!(
            "node is unavailable ({err}) and src/handlers.generated.rs or src/modules.generated.rs does not exist: run `npm run build` in desktop/ first"
        ),
    }
}
