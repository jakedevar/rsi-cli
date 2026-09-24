use std::env;
use std::process::ExitCode;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

fn socket_path() -> Result<String, String> {
    rsi_common::identity::env_with_legacy("RSI_SOCKET", &["MOTHERSHIP_SOCKET", "FLYWHEEL_SOCKET"])
        .map_err(|_| "RSI_SOCKET not set — are you running inside a rsi session?".to_string())
}

fn session_id() -> Result<String, String> {
    rsi_common::identity::env_with_legacy(
        "RSI_SESSION_ID",
        &["MOTHERSHIP_SESSION_ID", "FLYWHEEL_SESSION_ID"],
    )
    .map_err(|_| "RSI_SESSION_ID not set — are you running inside a rsi session?".to_string())
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> ExitCode {
    let args: Vec<String> = env::args().collect();
    let command = args.get(1).map(|s| s.as_str());

    match command {
        Some("archive") => match run_archive().await {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("rsi-signal: {}", e);
                ExitCode::FAILURE
            }
        },
        Some("help" | "--help" | "-h") => {
            print_usage();
            ExitCode::SUCCESS
        }
        Some(other) => {
            eprintln!("rsi-signal: unknown command '{}'", other);
            print_usage();
            ExitCode::FAILURE
        }
        None => {
            print_usage();
            ExitCode::FAILURE
        }
    }
}

async fn send_rpc(method: &str, session_id: &str) -> Result<(), String> {
    let socket = socket_path()?;

    let request = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": method,
        "params": {
            "session_id": session_id,
        }
    });

    let mut stream = UnixStream::connect(&socket)
        .await
        .map_err(|e| format!("cannot connect to daemon at {}: {}", socket, e))?;

    let mut payload =
        serde_json::to_string(&request).map_err(|e| format!("serialization error: {}", e))?;
    payload.push('\n');

    stream
        .write_all(payload.as_bytes())
        .await
        .map_err(|e| format!("write error: {}", e))?;

    let mut reader = BufReader::new(&mut stream);
    let mut line = String::new();
    reader
        .read_line(&mut line)
        .await
        .map_err(|e| format!("read error: {}", e))?;

    if line.is_empty() {
        return Err("daemon closed connection without responding".to_string());
    }

    let response: serde_json::Value =
        serde_json::from_str(&line).map_err(|e| format!("invalid response: {}", e))?;

    if let Some(error) = response.get("error") {
        let msg = error
            .get("message")
            .and_then(|m| m.as_str())
            .unwrap_or("unknown error");
        return Err(format!("daemon error: {}", msg));
    }

    Ok(())
}

async fn run_archive() -> Result<(), String> {
    let sid = session_id()?;
    send_rpc("MarkPendingArchive", &sid).await
}

fn print_usage() {
    eprintln!("Usage: rsi-signal <command>");
    eprintln!();
    eprintln!("Commands:");
    eprintln!("  archive    Mark current session for auto-archive on completion");
    eprintln!();
    eprintln!("Environment:");
    eprintln!("  RSI_SESSION_ID  Session UUID (set automatically by rsi)");
    eprintln!("  RSI_SOCKET      Daemon socket path (set automatically by rsi)");
}
