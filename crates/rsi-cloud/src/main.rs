//! `rsi-cloud`: the cloud spend safety net (#3767). See the library docs.

fn main() {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    let stdin = std::io::stdin();
    let stdout = std::io::stdout();
    let stderr = std::io::stderr();
    let code = rsi_cloud::run(
        &argv,
        &mut stdin.lock(),
        &mut stdout.lock(),
        &mut stderr.lock(),
    );
    std::process::exit(code);
}
