use std::fs;
use std::path::Path;

#[test]
fn recursive_dag_smoke_fixture_binary_requires_dev_fixtures_feature() {
    let manifest_path = Path::new(env!("CARGO_MANIFEST_DIR")).join("Cargo.toml");
    let manifest = fs::read_to_string(&manifest_path)
        .unwrap_or_else(|error| panic!("read {}: {error}", manifest_path.display()));

    assert!(
        manifest.contains("name = \"rsi-recursive-dag-smoke-fixture\""),
        "fixture binary target is missing"
    );
    assert!(
        manifest.contains("required-features = [\"dev-fixtures\"]"),
        "fixture binary must use Cargo required-features"
    );
    assert!(
        manifest.contains("dev-fixtures = [\"rusqlite/backup\"]"),
        "dev-fixtures feature must be explicit and non-default"
    );
}
