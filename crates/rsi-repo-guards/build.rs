#[path = "src/source_stamp.rs"]
#[allow(dead_code)]
mod source_stamp;

fn main() {
    let manifest = std::env::var_os("CARGO_MANIFEST_DIR").expect("Cargo build manifest");
    let root = std::path::Path::new(&manifest)
        .parent()
        .and_then(|p| p.parent())
        .expect("workspace root");
    for path in source_stamp::input_paths(root).expect("runner source inputs") {
        println!("cargo:rerun-if-changed={}", path.display());
    }
    println!("cargo:rerun-if-changed=src");
    println!(
        "cargo:rustc-env=RSI_REPO_RUNNER_SOURCE_STAMP={}",
        source_stamp::stamp(root).expect("runner content identity")
    );
}
