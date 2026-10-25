#[path = "../rsi-common/build_support/build_identity.rs"]
mod build_identity;

fn main() {
    println!("cargo:rerun-if-changed=../rsi-common/build_support/build_identity.rs");
    build_identity::emit_build_sha();
}
