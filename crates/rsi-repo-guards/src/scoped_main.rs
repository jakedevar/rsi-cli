fn main() -> std::process::ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let ctx = rsi_repo_guards::Ctx::from_process();
    let code = rsi_repo_guards::scoped_runner::main(
        &args,
        &ctx,
        &mut std::io::stdout(),
        &mut std::io::stderr(),
    );
    std::process::ExitCode::from(u8::try_from(code).unwrap_or(1))
}
