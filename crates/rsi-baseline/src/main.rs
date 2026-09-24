use std::process::ExitCode;

fn main() -> ExitCode {
    match rsi_baseline::dormant_result() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("{error}");
            ExitCode::from(2)
        }
    }
}
