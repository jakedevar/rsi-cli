//! `rsi-repo-guard <guard> [args...]`: run one repository static guard (#360).
//! Callers use `scripts/repo-guard`, which builds this binary first.

use std::io::Write;
use std::process::ExitCode;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args_os()
        .skip(1)
        .map(|a| a.to_string_lossy().into_owned())
        .collect();
    let ctx = rsi_repo_guards::Ctx::from_process();
    let stdout = std::io::stdout();
    let stderr = std::io::stderr();
    let (mut out, mut err) = (stdout.lock(), stderr.lock());
    let code = rsi_repo_guards::dispatch(&args, &ctx, &mut out, &mut err);
    let _ = out.flush();
    let _ = err.flush();
    ExitCode::from(u8::try_from(code).unwrap_or(1))
}
