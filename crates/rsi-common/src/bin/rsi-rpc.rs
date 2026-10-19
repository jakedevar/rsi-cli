//! `rsi-rpc` — minimal JSON-RPC CLI for the rsid Unix socket.
//!
//! Usage:
//!   rsi-rpc [--socket PATH] <METHOD> [--params JSON]
//!   rsi-rpc [--socket PATH] -- <METHOD> [--params JSON]
//!   rsi-rpc [--socket PATH] <AGENT_VERB> --schema
//!   rsi-rpc <AGENT_VERB> --params JSON --validate

use std::path::PathBuf;
use std::process::ExitCode;

use rsi_common::agent_control_schema::{AgentControlVerbV1, agent_control_catalog_v1};
use rsi_common::rpc::RpcResponse;
use serde_json::Value;

fn print_usage(writer: &mut impl std::io::Write) -> std::io::Result<()> {
    writeln!(
        writer,
        "usage: rsi-rpc [--socket PATH] <METHOD> [--params JSON]\n\
         usage: rsi-rpc [--socket PATH] -- <METHOD> [--params JSON]\n\
         usage: rsi-rpc [--socket PATH] <AGENT_VERB> --schema\n\
         usage: rsi-rpc <AGENT_VERB> --params JSON --validate\n\
         usage: rsi-rpc scratch-quota [PATH]   (scratch filesystem quota options and your usage)\n\
         --params takes inline JSON, @FILE, or - (stdin; needs no scratch file).\n\
         \n\
         Socket resolution: --socket, then RSI_DAEMON_SOCKET_PATH, then RSI_SOCKET/default.\n\
         Schema discovery is offline and applies only to the closed Agent* catalog.\n\
         Stdout is the full JSON-RPC response object. Exit 2 indicates RPC failure."
    )
}

#[derive(Debug)]
struct Args {
    socket: Option<PathBuf>,
    method: String,
    params: Value,
}

#[derive(Debug)]
enum Operation {
    Advertise,
    Help,
    Schema(AgentControlVerbV1),
    Validate(Args),
    Dispatch(Args),
}

/// Print the agent-control verb list. This is the advertise surface reached via
/// `rsi-rpc agent` / `rsi-rpc list-agent-verbs`.
fn print_agent_verbs(writer: &mut impl std::io::Write) -> std::io::Result<()> {
    writeln!(
        writer,
        "Start here: rsi-rpc AgentGetAuthorityCatalog  (your roles, their rules, and the controls you may call now)"
    )?;
    writeln!(writer)?;
    writeln!(
        writer,
        "rsi-rpc agent control verbs (the only surface advertised to agents):"
    )?;
    for descriptor in agent_control_catalog_v1() {
        writeln!(
            writer,
            "  {:<18} {}",
            descriptor.method, descriptor.description
        )?;
    }
    writeln!(writer)?;
    writeln!(writer, "Invoke a verb:  rsi-rpc <Verb> [--params JSON]")?;
    writeln!(writer, "Inspect params: rsi-rpc <Verb> --schema")?;
    writeln!(
        writer,
        "Authority token rides $RSI_SESSION_TOKEN (transport-only); never pass it in --params."
    )
}

/// True when the first positional (non-flag) token requests the agent-verb
/// advertise surface (`agent` or `list-agent-verbs`).
fn is_agent_advertise_request(raw_args: &[String]) -> bool {
    matches!(
        raw_args
            .iter()
            .find(|a| !a.starts_with('-'))
            .map(String::as_str),
        Some("agent" | "list-agent-verbs")
    )
}

fn main() -> ExitCode {
    let raw_args = std::env::args().skip(1).collect::<Vec<_>>();
    // #1049/#1097: the Claude PreToolUse/PostToolUse hook (mail delivery and
    // large-output spill). Handled before the verb parser: it is not a verb,
    // reads the hook event from stdin, and always exits 0 (a tool call is never
    // failed or delayed by it).
    if raw_args.first().map(String::as_str) == Some(rsi_common::boundary_mail_hook::HOOK_SUBCOMMAND)
    {
        let code = rsi_common::boundary_mail_hook::run_hook_stdin_with_args(
            &raw_args[1..],
            &mut std::io::stdout().lock(),
            rsi_common::agent_rpc_client::dispatch_with_timeout,
        );
        return ExitCode::from(code);
    }
    // #1097: `rsi-rpc spill ...` is the large-output spill wrapper and reader
    // (`scripts/rsi-spill`); not a daemon verb.
    if raw_args.first().map(String::as_str) == Some("spill") {
        let code = rsi_common::spill::cli_main(
            &raw_args[1..],
            &mut std::io::stdout().lock(),
            &mut std::io::stderr().lock(),
        );
        return ExitCode::from(u8::try_from(code).unwrap_or(1));
    }
    // #565: `rsi-rpc scratch-quota [PATH]` reads a scratch filesystem's quota
    // options and the caller's usage offline; not a daemon verb.
    if raw_args.first().map(String::as_str) == Some("scratch-quota") {
        let code = rsi_common::scratch_pressure::cli_main(
            &raw_args[1..],
            &mut std::io::stdout().lock(),
            &mut std::io::stderr().lock(),
        );
        return ExitCode::from(code);
    }
    let stdout = std::io::stdout();
    let stderr = std::io::stderr();
    let code = run(raw_args, &mut stdout.lock(), &mut stderr.lock(), dispatch);
    ExitCode::from(code)
}

fn run<F>(
    raw_args: Vec<String>,
    stdout: &mut impl std::io::Write,
    stderr: &mut impl std::io::Write,
    dispatch_fn: F,
) -> u8
where
    F: FnOnce(&std::path::Path, &str, Value) -> std::io::Result<RpcResponse>,
{
    let operation = match parse_operation(raw_args) {
        Ok(operation) => operation,
        Err(message) => {
            let _ = writeln!(stderr, "error: {message}");
            let _ = print_usage(stderr);
            return 1;
        }
    };

    match operation {
        Operation::Advertise => {
            if let Err(error) = print_agent_verbs(stdout) {
                let _ = writeln!(stderr, "output error: {error}");
                return 1;
            }
            0
        }
        Operation::Help => {
            if let Err(error) = print_usage(stderr) {
                let _ = writeln!(stderr, "output error: {error}");
                return 1;
            }
            0
        }
        Operation::Schema(verb) => {
            if let Err(error) = writeln!(stdout, "{}", verb.descriptor().envelope_json()) {
                let _ = writeln!(stderr, "output error: {error}");
                return 1;
            }
            0
        }
        Operation::Validate(args) => validate_agent_params(&args, stdout),
        Operation::Dispatch(args) => {
            if AgentControlVerbV1::from_method_name(&args.method).is_some() {
                let code = validate_agent_params(&args, stdout);
                if code != 0 {
                    return code;
                }
            }
            dispatch_after_validation(args, stdout, stderr, dispatch_fn)
        }
    }
}

fn validate_agent_params(args: &Args, stdout: &mut impl std::io::Write) -> u8 {
    let Some(verb) = AgentControlVerbV1::from_method_name(&args.method) else {
        return 0;
    };
    match verb.validate_params(&args.params) {
        Ok(()) => 0,
        Err(error) => {
            let mut envelope = serde_json::json!({"code":"invalid_input"});
            error.enrich(&mut envelope);
            let _ = writeln!(stdout, "{}", serde_json::json!({"error": envelope}));
            1
        }
    }
}

fn dispatch_after_validation<F>(
    args: Args,
    stdout: &mut impl std::io::Write,
    stderr: &mut impl std::io::Write,
    dispatch_fn: F,
) -> u8
where
    F: FnOnce(&std::path::Path, &str, Value) -> std::io::Result<RpcResponse>,
{
    let socket = args.socket.unwrap_or_else(resolve_socket_path);
    if is_pipeline_verify_role() && is_user_default_socket(&socket) {
        let _ = writeln!(
            stderr,
            "refusing to dispatch pipeline-verify RPCs against user daemon socket: {}",
            socket.display()
        );
        return 2;
    }
    let _ = writeln!(stderr, "rsi-rpc socket: {}", socket.display());
    let response = match dispatch_fn(&socket, &args.method, args.params) {
        Ok(response) => response,
        Err(error) => return report_transport_error(&error, stderr),
    };
    match serde_json::to_string_pretty(&response) {
        Ok(json) => {
            if let Err(error) = writeln!(stdout, "{json}") {
                let _ = writeln!(stderr, "output error: {error}");
                return 1;
            }
        }
        Err(error) => {
            let _ = writeln!(
                stderr,
                "internal error: failed to serialize response: {error}"
            );
            return 1;
        }
    }
    if response.error.is_some() { 2 } else { 0 }
}

/// Exit code for a response timeout (#2357): the request was sent and may have
/// been applied, so it is neither a plain transport failure (1) nor a daemon
/// refusal (2). Callers replay it under the same `idempotency_key`.
const EXIT_RESPONSE_TIMEOUT: u8 = 3;

fn report_transport_error(error: &std::io::Error, stderr: &mut impl std::io::Write) -> u8 {
    let _ = writeln!(stderr, "RPC transport error: {error}");
    if rsi_common::agent_rpc_client::response_timeout(error).is_some() {
        EXIT_RESPONSE_TIMEOUT
    } else {
        1
    }
}

fn parse_operation(raw_args: Vec<String>) -> Result<Operation, String> {
    if raw_args.iter().any(|arg| arg == "--schema") {
        return parse_schema_operation(raw_args);
    }
    if raw_args.iter().any(|arg| arg == "--validate") {
        let args = parse_args(raw_args, true)?;
        AgentControlVerbV1::from_method_name(&args.method).ok_or_else(|| {
            "--validate is available only for an exact advertised Agent* verb".to_string()
        })?;
        return Ok(Operation::Validate(args));
    }
    // Preserve advertise precedence over global help for `rsi-rpc agent
    // --help`; schema-form aliases were handled and rejected above.
    if is_agent_advertise_request(&raw_args) {
        return Ok(Operation::Advertise);
    }
    if raw_args.iter().any(|arg| arg == "-h" || arg == "--help") {
        return Ok(Operation::Help);
    }
    parse_args(raw_args, false).map(Operation::Dispatch)
}

fn parse_schema_operation(raw_args: Vec<String>) -> Result<Operation, String> {
    let mut method = None;
    let mut schema_seen = false;
    let mut iter = raw_args.into_iter();

    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--schema" => {
                if schema_seen {
                    return Err("--schema may be supplied only once".to_string());
                }
                schema_seen = true;
            }
            "--params" => {
                return Err("--schema cannot be combined with --params".to_string());
            }
            "-h" | "--help" => {
                return Err("--schema cannot be combined with --help".to_string());
            }
            "--socket" => {
                let Some(_path) = iter.next() else {
                    return Err("--socket requires a path".to_string());
                };
            }
            "--" => {
                if method.is_some() {
                    return Err("only one method argument is allowed".to_string());
                }
                let Some(value) = iter.next() else {
                    return Err("missing method after `--`".to_string());
                };
                method = Some(value);
            }
            other if other.starts_with("--") => return Err(format!("unknown flag `{other}`")),
            other => {
                if method.is_some() {
                    return Err("only one method argument is allowed".to_string());
                }
                method = Some(other.to_string());
            }
        }
    }

    if !schema_seen {
        return Err("--schema must be supplied as a flag, not a flag value".to_string());
    }
    let method = method.ok_or_else(|| "missing <AGENT_VERB> for --schema".to_string())?;
    let verb = AgentControlVerbV1::from_method_name(&method).ok_or_else(|| {
        format!("--schema is available only for an exact advertised Agent* verb, not `{method}`")
    })?;
    Ok(Operation::Schema(verb))
}

fn parse_args<I>(args: I, allow_validate: bool) -> Result<Args, String>
where
    I: IntoIterator<Item = String>,
{
    let mut socket = None;
    let mut method = None;
    let mut params = Value::Null;
    let mut iter = args.into_iter();

    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--" => {
                method = iter.next();
                if method.is_none() {
                    return Err("missing method after `--`".to_string());
                }
            }
            "--socket" => {
                let Some(value) = iter.next() else {
                    return Err("--socket requires a path".to_string());
                };
                socket = Some(PathBuf::from(value));
            }
            "--params" => {
                let Some(value) = iter.next() else {
                    return Err("--params requires a JSON value".to_string());
                };
                params = parse_params(&value)?;
            }
            "--validate" if allow_validate => {}
            other if other.starts_with("--") => return Err(format!("unknown flag `{other}`")),
            other => {
                if method.is_some() {
                    return Err("only one method argument is allowed".to_string());
                }
                method = Some(other.to_string());
            }
        }
    }

    let Some(method) = method else {
        return Err("missing <METHOD>".to_string());
    };
    // Agent verbs take an object; an omitted `--params` means `{}` so
    // `rsi-rpc AgentGetAuthorityCatalog` works as written.
    if params.is_null() && AgentControlVerbV1::from_method_name(&method).is_some() {
        params = Value::Object(serde_json::Map::new());
    }

    Ok(Args {
        socket,
        method,
        params,
    })
}

fn parse_params(value: &str) -> Result<Value, String> {
    parse_params_from(value, &mut std::io::stdin().lock())
}

/// `--params` is inline JSON, `@FILE`, or `-` (stdin). Stdin needs no scratch
/// file, so a control payload never depends on a quota-limited filesystem
/// (#565). A file that cannot be read says so with the disk-pressure hint.
fn parse_params_from(value: &str, stdin: &mut dyn std::io::Read) -> Result<Value, String> {
    if value == "-" {
        let mut content = String::new();
        stdin
            .read_to_string(&mut content)
            .map_err(|err| format!("failed to read --params from stdin: {err}"))?;
        serde_json::from_str(&content)
            .map_err(|err| format!("--params from stdin is not valid JSON: {err}"))
    } else if let Some(path) = value.strip_prefix('@') {
        let content = std::fs::read_to_string(path).map_err(|err| {
            let hint = match rsi_common::scratch_pressure::classify(&err) {
                Some(kind) => format!(" [{}: {}]", kind.code(), kind.next_action()),
                None if err.kind() == std::io::ErrorKind::NotFound => {
                    " [the file does not exist: if the write that should have created it \
                     failed with `Disk quota exceeded` (EDQUOT) or `No space left on device`, \
                     the cause is the scratch filesystem, not these params; pipe the JSON with \
                     `--params -` or write it under an owned directory outside /tmp]"
                        .to_string()
                }
                None => String::new(),
            };
            format!("failed to read params file `{path}`: {err}{hint}")
        })?;
        serde_json::from_str(&content)
            .map_err(|err| format!("--params file `{path}` is not valid JSON: {err}"))
    } else {
        serde_json::from_str(value).map_err(|err| format!("--params is not valid JSON: {err}"))
    }
}

/// Read `$RSI_SESSION_TOKEN` for out-of-band attribution. Never accept the
/// token via `--params` — it must ride the transport-only `session_token`
/// field so it never appears in a persisted request params blob.
fn resolve_socket_path() -> PathBuf {
    rsi_common::agent_rpc_client::resolve_socket_path()
}

fn is_pipeline_verify_role() -> bool {
    std::env::var("CLAUDE_AGENT_ROLE").as_deref() == Ok("pipeline-verify")
}

fn is_user_default_socket(socket: &std::path::Path) -> bool {
    let user_default = rsi_common::identity::data_path("daemon.sock", "daemon.sock");
    socket == user_default
}

fn dispatch(socket: &std::path::Path, method: &str, params: Value) -> std::io::Result<RpcResponse> {
    rsi_common::agent_rpc_client::dispatch(socket, method, params)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    fn run_offline(args: &[&str]) -> (u8, String, String, bool) {
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let dispatched = Cell::new(false);
        let code = run(
            args.iter().map(|arg| (*arg).to_string()).collect(),
            &mut stdout,
            &mut stderr,
            |_, _, _| {
                dispatched.set(true);
                Err(std::io::Error::other("unexpected dispatch"))
            },
        );
        (
            code,
            String::from_utf8(stdout).unwrap(),
            String::from_utf8(stderr).unwrap(),
            dispatched.get(),
        )
    }

    #[test]
    fn agent_advertise_lists_only_agent_verbs() {
        let names: Vec<&str> = agent_control_catalog_v1()
            .iter()
            .map(|descriptor| descriptor.method)
            .collect();
        // Derived from the descriptor table (#1116): the listing is checked
        // against the declarations below rather than a hand-pinned roster.
        assert!(names.first() == Some(&"AgentGetAuthorityCatalog"));
        assert!(names.iter().all(|name| name.starts_with("Agent")));
        let wake_description = agent_control_catalog_v1()
            .iter()
            .find_map(|descriptor| {
                (descriptor.method == "AgentScheduleWake").then_some(descriptor.description)
            })
            .expect("AgentScheduleWake description");
        assert!(wake_description.contains("explicit mode is required"));
        assert!(wake_description.contains("resume"));

        let (code, stdout, stderr, dispatched) = run_offline(&["agent"]);
        assert_eq!(code, 0);
        assert!(stderr.is_empty());
        assert!(!dispatched);
        // #1011: the listing is exactly the declarations flagged `cli`.
        assert_eq!(names, rsi_common::rpc_verb_registry::cli_verb_methods());
        let listed: Vec<&str> = stdout
            .lines()
            .filter_map(|line| line.strip_prefix("  "))
            .filter_map(|line| line.split_whitespace().next())
            .filter(|word| word.starts_with("Agent"))
            .collect();
        assert_eq!(listed, names);
        for method in names {
            assert!(stdout.contains(method));
        }
        assert!(stdout.contains("Inspect params: rsi-rpc <Verb> --schema"));
        assert!(!stdout.contains("GetSession"));
        for operator_method in [
            "GetHarnessManager",
            "ConfigureHarnessManager",
            "GetHarnessManagerPolicy",
            "ConfigureHarnessManagerPolicy",
            "GetHarnessManagerState",
            "AnswerHarnessManagerDecision",
            "GetRollingQueue",
            "EnqueueRollingQueueSource",
        ] {
            assert!(!stdout.contains(operator_method));
            let (code, _, _, dispatched) = run_offline(&[operator_method, "--schema"]);
            assert_eq!(code, 1);
            assert!(!dispatched, "operator schema discovery must stay offline");
        }
    }

    #[test]
    fn agent_advertise_request_detects_subcommands() {
        assert!(is_agent_advertise_request(&["agent".to_string()]));
        assert!(is_agent_advertise_request(&[
            "agent".to_string(),
            "--help".to_string()
        ]));
        assert!(is_agent_advertise_request(
            &["list-agent-verbs".to_string()]
        ));
        // A real RPC method must NOT be intercepted as the advertise surface.
        assert!(!is_agent_advertise_request(
            &["GetHealthStatus".to_string()]
        ));
        assert!(!is_agent_advertise_request(&["AgentGetStatus".to_string()]));
    }

    #[test]
    fn every_agent_schema_is_deterministic_versioned_and_offline() {
        for descriptor in agent_control_catalog_v1() {
            let args = [
                "--socket",
                "/definitely/not/a/socket",
                descriptor.method,
                "--schema",
            ];
            let first = run_offline(&args);
            let second = run_offline(&args);
            assert_eq!(first, second, "method: {}", descriptor.method);
            let (code, stdout, stderr, dispatched) = first;
            assert_eq!(code, 0, "method: {}", descriptor.method);
            assert!(stderr.is_empty(), "method: {}", descriptor.method);
            assert!(!dispatched, "method: {}", descriptor.method);
            assert_eq!(stdout, format!("{}\n", descriptor.envelope_json()));
            let envelope: Value = serde_json::from_str(stdout.trim()).unwrap();
            assert_eq!(envelope["schema_version"], 1);
            assert_eq!(envelope["method"], descriptor.method);
            assert_eq!(envelope["parameters"], descriptor.parameters());
        }
    }

    #[test]
    fn params_read_from_stdin_without_a_scratch_file() {
        let mut stdin = std::io::Cursor::new(br#"{"a":1}"#.to_vec());
        assert_eq!(
            parse_params_from("-", &mut stdin).unwrap(),
            serde_json::json!({"a": 1})
        );
        let mut bad = std::io::Cursor::new(b"nope".to_vec());
        assert!(
            parse_params_from("-", &mut bad)
                .unwrap_err()
                .contains("stdin")
        );
    }

    #[test]
    fn missing_params_file_names_the_scratch_filesystem_cause() {
        let mut stdin = std::io::empty();
        let message = parse_params_from("@/nonexistent-dir-565/p.json", &mut stdin).unwrap_err();
        assert!(message.contains("failed to read params file"));
        assert!(message.contains("Disk quota exceeded"));
        assert!(message.contains("--params -"));
    }

    #[test]
    fn schema_form_accepts_the_normal_method_separator() {
        let direct = run_offline(&["AgentGetStatus", "--schema"]);
        let separated = run_offline(&["--", "AgentGetStatus", "--schema"]);
        assert_eq!(direct, separated);
        assert_eq!(direct.0, 0);
        assert!(!direct.3);
    }

    #[test]
    fn malformed_or_non_agent_schema_forms_fail_before_dispatch() {
        let cases: &[&[&str]] = &[
            &[
                "AgentGetStatus",
                "--schema",
                "--params",
                "@/missing/params.json",
            ],
            &["agent", "--schema"],
            &["list-agent-verbs", "--schema"],
            &["agentspawnchild", "--schema"],
            &["UnknownMethod", "--schema"],
            &["AgentGetStatus", "OtherMethod", "--schema"],
            &["AgentGetStatus", "--schema", "--schema"],
            &["--socket", "--schema", "AgentGetStatus"],
        ];
        for args in cases {
            let (code, stdout, stderr, dispatched) = run_offline(args);
            assert_eq!(code, 1, "args: {args:?}");
            assert!(stdout.is_empty(), "args: {args:?}");
            assert!(stderr.contains("error:"), "args: {args:?}");
            assert!(!stderr.contains("rsi-rpc socket:"), "args: {args:?}");
            assert!(!dispatched, "args: {args:?}");
        }
    }

    #[test]
    fn generic_and_operator_methods_have_no_schema_surface() {
        for method in [
            "GetSession",
            "LaunchSession",
            "CreateIssue",
            "ListProgramRuns",
            "CreateProgramRun",
            "ListSourceWorktreeCohorts",
            "UpdateDaemonConfig",
        ] {
            let (code, stdout, stderr, dispatched) = run_offline(&[method, "--schema"]);
            assert_eq!(code, 1, "method: {method}");
            assert!(stdout.is_empty(), "method: {method}");
            assert!(stderr.contains("available only for an exact advertised Agent* verb"));
            assert!(!stderr.contains("rsi-rpc socket:"));
            assert!(!dispatched, "method: {method}");
        }
    }

    #[test]
    fn ordinary_rpc_invocation_retains_dispatch_and_exit_behavior() {
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let dispatched = Cell::new(false);
        let code = run(
            [
                "--socket",
                "/tmp/rsi-rpc-test.sock",
                "GetSession",
                "--params",
                "{\"session_id\":\"5d73c05d-1040-49f7-92ab-0123456789ab\"}",
            ]
            .into_iter()
            .map(str::to_string)
            .collect(),
            &mut stdout,
            &mut stderr,
            |socket, method, params| {
                dispatched.set(true);
                assert_eq!(socket, std::path::Path::new("/tmp/rsi-rpc-test.sock"));
                assert_eq!(method, "GetSession");
                assert!(params["session_id"].is_string());
                Ok(RpcResponse::success(
                    Some(Value::Number(1.into())),
                    serde_json::json!({"ok": true}),
                ))
            },
        );
        assert_eq!(code, 0);
        assert!(dispatched.get());
        assert!(String::from_utf8(stdout).unwrap().contains("\"ok\": true"));
        assert!(
            String::from_utf8(stderr)
                .unwrap()
                .contains("rsi-rpc socket: /tmp/rsi-rpc-test.sock")
        );
    }

    #[cfg(unix)]
    #[test]
    fn response_timeout_prints_replay_hint_and_has_its_own_exit_code() {
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let code = run(
            [
                "--socket",
                "/tmp/rsi-rpc-test.sock",
                "GetSession",
                "--params",
                "{}",
            ]
            .into_iter()
            .map(str::to_string)
            .collect(),
            &mut stdout,
            &mut stderr,
            |socket, method, _| {
                // The real client's slow-response path, via a throwaway server.
                let _ = socket;
                let dir = tempfile::tempdir().unwrap();
                let path = dir.path().join("d.sock");
                let listener = std::os::unix::net::UnixListener::bind(&path).unwrap();
                let server = std::thread::spawn(move || {
                    let (stream, _) = listener.accept().unwrap();
                    std::thread::sleep(std::time::Duration::from_millis(500));
                    drop(stream);
                });
                let result = rsi_common::agent_rpc_client::dispatch_with_timeout(
                    &path,
                    method,
                    Value::Null,
                    std::time::Duration::from_millis(150),
                );
                server.join().unwrap();
                result
            },
        );
        assert_eq!(code, EXIT_RESPONSE_TIMEOUT);
        assert!(stdout.is_empty());
        let stderr = String::from_utf8(stderr).unwrap();
        assert!(
            stderr.contains("rpc_response_timeout: GetSession"),
            "{stderr}"
        );
        assert!(stderr.contains("same idempotency_key"), "{stderr}");
        assert!(!stderr.contains("os error 11"), "{stderr}");
    }

    #[test]
    fn plain_transport_errors_keep_exit_code_one() {
        let mut stderr = Vec::new();
        let code = report_transport_error(&std::io::Error::other("boom"), &mut stderr);
        assert_eq!(code, 1);
        assert!(
            String::from_utf8(stderr)
                .unwrap()
                .contains("RPC transport error: boom")
        );
    }

    #[test]
    fn omitted_agent_params_dispatch_an_empty_object() {
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let seen = std::cell::RefCell::new(None);
        let code = run(
            vec!["AgentGetAuthorityCatalog".to_string()],
            &mut stdout,
            &mut stderr,
            |_, method, params| {
                *seen.borrow_mut() = Some((method.to_string(), params));
                Ok(RpcResponse::success(
                    Some(Value::Number(1.into())),
                    Value::Null,
                ))
            },
        );
        assert_eq!(code, 0);
        assert_eq!(
            seen.into_inner(),
            Some((
                "AgentGetAuthorityCatalog".to_string(),
                serde_json::json!({})
            ))
        );
        let (_, stdout, _, _) = run_offline(&["agent"]);
        assert!(stdout.starts_with("Start here: rsi-rpc AgentGetAuthorityCatalog"));
    }

    #[test]
    fn invalid_agent_input_is_rejected_before_socket_dispatch() {
        let (code, stdout, stderr, dispatched) = run_offline(&[
            "--socket",
            "/definitely/not/a/socket",
            "AgentManagerControl",
            "--params",
            r#"{"fence":{"scope_version":0,"policy_version":1},"idempotency_key":"x","operation":{"action":"create_container","kind":"Group","parent_id":null,"name":"group","tags":[]}}"#,
        ]);
        assert_eq!(code, 1);
        let envelope: Value = serde_json::from_str(&stdout).unwrap();
        assert_eq!(envelope["error"]["code"], "invalid_input");
        assert_eq!(envelope["error"]["class"], "invalid_input");
        assert_eq!(envelope["error"]["field"], "params");
        assert_eq!(envelope["error"]["path"], "params.fence.scope_version");
        assert_eq!(envelope["error"]["constraint"], "minimum 1");
        assert_eq!(envelope["error"]["value"], 0);
        assert!(stderr.is_empty());
        assert!(!dispatched);
    }

    #[test]
    fn nested_job_limit_is_visible_in_offline_cli_refusal() {
        let (code, stdout, stderr, dispatched) = run_offline(&[
            "AgentSubmitJob",
            "--params",
            r#"{"kind":"test","params":{"package":"rsi","timeout_minutes":181}}"#,
            "--validate",
        ]);
        assert_eq!(code, 1);
        assert!(stderr.is_empty());
        assert!(!dispatched);
        let envelope: Value = serde_json::from_str(&stdout).unwrap();
        assert_eq!(envelope["error"]["code"], "invalid_input");
        assert_eq!(envelope["error"]["field"], "params");
        assert_eq!(envelope["error"]["path"], "params.params.timeout_minutes");
        assert_eq!(envelope["error"]["constraint"], "maximum 180");
        assert_eq!(envelope["error"]["value"], 181);
    }

    #[test]
    fn validate_mode_is_offline_and_valid_agent_input_dispatches_normally() {
        let (code, stdout, stderr, dispatched) =
            run_offline(&["AgentGetStatus", "--params", "{}", "--validate"]);
        assert_eq!(code, 0);
        assert!(stdout.is_empty());
        assert!(stderr.is_empty());
        assert!(!dispatched);

        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let dispatched = Cell::new(false);
        let code = run(
            ["AgentGetStatus", "--params", "{}"]
                .into_iter()
                .map(str::to_string)
                .collect(),
            &mut stdout,
            &mut stderr,
            |_, method, _| {
                dispatched.set(true);
                assert_eq!(method, "AgentGetStatus");
                Ok(RpcResponse::success(
                    Some(Value::Number(1.into())),
                    Value::Null,
                ))
            },
        );
        assert_eq!(code, 0);
        assert!(dispatched.get());
    }
}
