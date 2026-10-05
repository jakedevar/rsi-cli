// SEAM-GUARD-BEGIN
/// `test-seam` carries fake capabilities and route-validation bypasses; it must
/// never be compiled into a release build. A release-mode test run that needs
/// it opts in explicitly with `RSID_ALLOW_TEST_SEAM_RELEASE=1`.
fn refuse_test_seam_in_release() {
    println!("cargo:rerun-if-env-changed=RSID_ALLOW_TEST_SEAM_RELEASE");
    let seam = std::env::var_os("CARGO_FEATURE_TEST_SEAM").is_some();
    let release = std::env::var("PROFILE").is_ok_and(|profile| profile == "release");
    let allowed = std::env::var("RSID_ALLOW_TEST_SEAM_RELEASE").is_ok_and(|value| value == "1");
    assert!(
        !(seam && release && !allowed),
        "the `test-seam` feature is enabled in a release build; it must never ship \
         (set RSID_ALLOW_TEST_SEAM_RELEASE=1 only for a release-mode test run)"
    );
}
// SEAM-GUARD-END

fn main() {
    refuse_test_seam_in_release();
}
