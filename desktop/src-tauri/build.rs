fn main() {
    // Frontend assets (`../dist`, see tauri.conf.json `frontendDist`) are
    // embedded at compile time. Without this, cargo does not notice a
    // frontend-only change (CSS, a new module) and ships a stale UI.
    println!("cargo:rerun-if-changed=../dist");
    tauri_build::build()
}
