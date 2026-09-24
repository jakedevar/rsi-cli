mod fixture_support;
fn main() {
    println!(
        "{}",
        serde_json::to_string(&fixture_support::run()).expect("fixture JSON")
    );
}
