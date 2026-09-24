use std::path::PathBuf;

use rsid::model_control::registry::{render_exclusion_markdown_table, render_markdown_table};
use rsid::model_control::validator::{ValidationInput, validate_repository};

fn main() {
    if let Err(error) = run() {
        eprintln!("{error}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), String> {
    let argument = std::env::args().nth(1);
    if argument.as_deref() == Some("--print") {
        print!("{}", render_markdown_table());
        return Ok(());
    }
    if argument.as_deref() == Some("--print-exclusions") {
        print!("{}", render_exclusion_markdown_table());
        return Ok(());
    }
    let repo_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(|path| path.parent())
        .ok_or_else(|| "failed to resolve repository root".to_string())?
        .to_path_buf();
    let report = validate_repository(ValidationInput::production(repo_root))?;
    if argument.as_deref() == Some("--inventory") {
        println!(
            "{}",
            serde_json::to_string_pretty(&report.inventory)
                .map_err(|error| format!("failed to render inventory: {error}"))?
        );
    } else if argument.is_some() {
        return Err(format!(
            "unknown argument `{}`; expected --print, --print-exclusions, or --inventory",
            argument.unwrap_or_default()
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::{Path, PathBuf};

    use rsi_common::model_control::PaidRisk;
    use rsid::model_control::registry::{
        AllowedExclusionOperation, CapabilityConsumption, ExecutionBoundaryContract,
        NonInvocationContract, PrimitiveKind, RuntimeExecutionRoute,
    };
    use rsid::model_control::validator::{
        PurposeDisposition, RouteDisposition, ValidationInput, validate_repository,
    };

    const HTTP_BOUNDARY: ExecutionBoundaryContract = ExecutionBoundaryContract {
        id: "http",
        path: "crates/rsid/src/provider.rs",
        item: "execute",
        capability_type: "ModelExecutionCapability",
        capability_ident: "execution",
        primitive_kind: PrimitiveKind::HttpPost,
        consumption: CapabilityConsumption::BindHttpSend,
        runtime_route: RuntimeExecutionRoute::LocalOpenAiCompatibleHttp,
        occurrences: 1,
    };
    const CLI_BOUNDARY: ExecutionBoundaryContract = ExecutionBoundaryContract {
        id: "cli",
        path: "crates/rsid/src/provider.rs",
        item: "execute",
        capability_type: "CliExecutionCapability",
        capability_ident: "execution",
        primitive_kind: PrimitiveKind::CliSpawn,
        consumption: CapabilityConsumption::BindCommandSpawn,
        runtime_route: RuntimeExecutionRoute::ClaudeCli,
        occurrences: 1,
    };
    const PROVIDER_CHAT_BOUNDARY: ExecutionBoundaryContract = ExecutionBoundaryContract {
        id: "provider-chat",
        path: "crates/rsid/src/provider.rs",
        item: "execute",
        capability_type: "ModelExecutionCapability",
        capability_ident: "execution",
        primitive_kind: PrimitiveKind::ProviderChat,
        consumption: CapabilityConsumption::ProviderChatArgument,
        runtime_route: RuntimeExecutionRoute::HarnessOpenAiHttp,
        occurrences: 1,
    };

    fn fixture_root(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "rsi-model-control-validator-{name}-{}",
            uuid::Uuid::new_v4()
        ))
    }

    fn write(root: &Path, relative: &str, source: &str) {
        let path = root.join(relative);
        fs::create_dir_all(path.parent().expect("fixture parent")).expect("fixture directory");
        fs::write(path, source).expect("fixture source");
    }

    fn copy_tree(source: &Path, destination: &Path) {
        fs::create_dir_all(destination).expect("fixture directory");
        for entry in fs::read_dir(source).expect("source directory") {
            let entry = entry.expect("source entry");
            let source_path = entry.path();
            let destination_path = destination.join(entry.file_name());
            if source_path.is_dir() {
                copy_tree(&source_path, &destination_path);
            } else {
                fs::copy(source_path, destination_path).expect("fixture file");
            }
        }
    }

    fn production_fixture_root(name: &str) -> PathBuf {
        let source_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../..")
            .canonicalize()
            .expect("repository root");
        let fixture = fixture_root(name);
        copy_tree(
            &source_root.join("crates/rsid/src"),
            &fixture.join("crates/rsid/src"),
        );
        let registry = "thoughts/shared/reference/model-invocation-registry.md";
        let destination = fixture.join(registry);
        fs::create_dir_all(destination.parent().expect("registry parent"))
            .expect("registry directory");
        fs::copy(source_root.join(registry), destination).expect("registry fixture");
        fixture
    }

    fn purpose(boundary: &str) -> PurposeDisposition {
        PurposeDisposition {
            purpose: "fixture.paid".to_string(),
            paid_risk: PaidRisk::PaidCapable,
            route: RouteDisposition::Boundaries(vec![boundary.to_string()]),
        }
    }

    fn input(
        root: &Path,
        purposes: Vec<PurposeDisposition>,
        boundaries: Vec<ExecutionBoundaryContract>,
        exclusions: Vec<NonInvocationContract>,
    ) -> ValidationInput {
        ValidationInput {
            repo_root: root.to_path_buf(),
            source_root: root.join("crates/rsid/src"),
            markdown: None,
            purposes,
            boundaries,
            exclusions,
        }
    }

    fn valid_http_source() -> &'static str {
        r#"
use crate::model_control::ModelExecutionCapability;
async fn execute(
    client: &Client,
    url: &str,
    execution: ModelExecutionCapability,
) {
    execution
        .bind_http(
            RuntimeExecutionRoute::LocalOpenAiCompatibleHttp,
            client.post(url),
        )
        .send("fixture")
        .await;
}
"#
    }

    #[test]
    fn production_engine_accepts_by_value_http_and_cli_boundaries() {
        let http_root = fixture_root("valid-http");
        write(
            &http_root,
            "crates/rsid/src/provider.rs",
            valid_http_source(),
        );
        validate_repository(input(
            &http_root,
            vec![purpose("http")],
            vec![HTTP_BOUNDARY],
            vec![],
        ))
        .expect("by-value bind/send boundary");
        fs::remove_dir_all(http_root).expect("cleanup");

        let cli_root = fixture_root("valid-cli");
        write(
            &cli_root,
            "crates/rsid/src/provider.rs",
            r#"
use crate::model_control::CliExecutionCapability;
fn execute(execution: CliExecutionCapability) {
    execution
        .bind_command(RuntimeExecutionRoute::ClaudeCli, cmd)
        .spawn();
}
"#,
        );
        validate_repository(input(
            &cli_root,
            vec![purpose("cli")],
            vec![CLI_BOUNDARY],
            vec![],
        ))
        .expect("by-value CLI boundary");
        fs::remove_dir_all(cli_root).expect("cleanup");

        let provider_chat_root = fixture_root("valid-provider-chat");
        write(
            &provider_chat_root,
            "crates/rsid/src/provider.rs",
            r#"
use crate::model_control::ModelExecutionCapability;
async fn execute(
    provider: &Provider,
    request: Request,
    execution: ModelExecutionCapability,
) {
    provider.chat(request, execution).await;
}
"#,
        );
        let report = validate_repository(input(
            &provider_chat_root,
            vec![purpose("provider-chat")],
            vec![PROVIDER_CHAT_BOUNDARY],
            vec![],
        ))
        .expect("by-value provider chat boundary");
        assert!(
            report
                .inventory
                .iter()
                .any(|entry| entry.contains("kind=ProviderChat count=1"))
        );
        fs::remove_dir_all(provider_chat_root).expect("cleanup");
    }

    #[test]
    fn rejects_marker_or_unrelated_authority_text() {
        for (name, source) in [
            (
                "marker",
                "// MODEL_CALL_ADMISSION: comment only\nfn execute() { client.post(url); }",
            ),
            (
                "unrelated",
                "struct AdmissionPermit; struct ModelCallControl; fn permit() {} fn execute() { client.post(url); }",
            ),
        ] {
            let root = fixture_root(name);
            write(&root, "crates/rsid/src/provider.rs", source);
            let error = validate_repository(input(
                &root,
                vec![purpose("http")],
                vec![HTTP_BOUNDARY],
                vec![],
            ))
            .expect_err("text cannot authorize a sink");
            assert!(error.contains("missing capability") || error.contains("direct consume"));
            fs::remove_dir_all(root).expect("cleanup");
        }
    }

    #[test]
    fn rejects_unused_borrowed_shadowed_displaced_and_aliased_capabilities() {
        let cases = [
            (
                "unused",
                "fn execute(execution: ModelExecutionCapability) { client.post(url); }",
            ),
            (
                "borrowed",
                "fn execute(execution: &ModelExecutionCapability) { execution.bind_http(client.post(url)).send(\"x\"); }",
            ),
            (
                "shadowed",
                "fn execute(execution: ModelExecutionCapability) { let execution = other; execution.bind_http(client.post(url)).send(\"x\"); }",
            ),
            (
                "displaced",
                "fn execute(execution: ModelExecutionCapability) { helper(execution, client.post(url)); }",
            ),
            (
                "alias",
                "fn execute(execution: ModelExecutionCapability) { let admitted = execution.bind_http(client.post(url)); admitted.send(\"x\"); }",
            ),
        ];
        for (name, source) in cases {
            let root = fixture_root(name);
            write(&root, "crates/rsid/src/provider.rs", source);
            validate_repository(input(
                &root,
                vec![purpose("http")],
                vec![HTTP_BOUNDARY],
                vec![],
            ))
            .expect_err("invalid capability flow must fail");
            fs::remove_dir_all(root).expect("cleanup");
        }
    }

    #[test]
    fn rejects_macro_wrapped_sink_and_sink_in_new_file() {
        let macro_root = fixture_root("macro");
        write(
            &macro_root,
            "crates/rsid/src/provider.rs",
            "fn execute(execution: ModelExecutionCapability) { wrapped!(client.post(url)); let _ = execution; }",
        );
        let error = validate_repository(input(
            &macro_root,
            vec![purpose("http")],
            vec![HTTP_BOUNDARY],
            vec![],
        ))
        .expect_err("macro sink");
        assert!(error.contains("macro-wrapped"));
        fs::remove_dir_all(macro_root).expect("cleanup");

        let new_file_root = fixture_root("new-file");
        write(
            &new_file_root,
            "crates/rsid/src/provider.rs",
            valid_http_source(),
        );
        write(
            &new_file_root,
            "crates/rsid/src/injected.rs",
            "fn injected() { client.post(\"https://example.com/v1/chat/completions\"); }",
        );
        let error = validate_repository(input(
            &new_file_root,
            vec![purpose("http")],
            vec![HTTP_BOUNDARY],
            vec![],
        ))
        .expect_err("new-file sink");
        assert!(error.contains("injected.rs"));
        fs::remove_dir_all(new_file_root).expect("cleanup");
    }

    #[test]
    fn rejects_all_macro_wrapped_model_sink_shapes() {
        let cases = [
            (
                "request-post",
                "fn injected(client: &Client, url: &str) { wrapped!(client.request(Method::POST, url)); }",
            ),
            (
                "execute",
                "fn injected(url: &str) { let requester = Client::new(); wrapped!(requester.execute(build_post_request(url))); }",
            ),
            (
                "associated-execute",
                "fn injected(client: &Client, request: Request) { wrapped!(Client::execute(client, request)); }",
            ),
            (
                "provider-chat",
                "fn injected(provider: &Provider, request: Request) { wrapped!(provider.chat(request)); }",
            ),
            (
                "provider-chat-displaced-capability",
                "use crate::model_control::ModelExecutionCapability; fn injected(provider: &Provider, request: Request, execution: ModelExecutionCapability) { wrapped!(provider.chat(request); drop(execution)); }",
            ),
            (
                "provider-stream",
                "fn injected(provider: &Provider, request: Request) { wrapped!(provider.stream_chat(request)); }",
            ),
            (
                "cli-spawn",
                "fn injected(binary: &str) { let runner = Command::new(binary); wrapped!(runner.spawn()); }",
            ),
            (
                "request-send",
                "fn injected(request: RequestBuilder) { wrapped!(request.send()); }",
            ),
            (
                "appserver-send",
                r#"fn injected(model_tx: Sender<Vec<u8>>) {
                    let request = json!({"method": "thread/start"});
                    let bytes = serialize(&request);
                    wrapped!(model_tx.send(bytes));
                }"#,
            ),
        ];

        for (name, source) in cases {
            let root = fixture_root(name);
            write(&root, "crates/rsid/src/injected.rs", source);
            let error = validate_repository(input(&root, vec![], vec![], vec![]))
                .expect_err("macro-wrapped sink must fail closed");
            assert!(error.contains("macro-wrapped"), "{name}: {error}");
            fs::remove_dir_all(root).expect("cleanup");
        }
    }

    #[test]
    fn one_contract_rejects_two_primitives_and_two_contracts_accept_two_items() {
        let one_root = fixture_root("two-one-contract");
        write(
            &one_root,
            "crates/rsid/src/provider.rs",
            r#"
use crate::model_control::ModelExecutionCapability;
fn execute(execution: ModelExecutionCapability) {
    execution
        .bind_http(
            RuntimeExecutionRoute::LocalOpenAiCompatibleHttp,
            client.post(url),
        )
        .send("x");
    client.post(other);
}
"#,
        );
        validate_repository(input(
            &one_root,
            vec![purpose("http")],
            vec![HTTP_BOUNDARY],
            vec![],
        ))
        .expect_err("second primitive");
        fs::remove_dir_all(one_root).expect("cleanup");

        let two_root = fixture_root("two-contracts");
        write(
            &two_root,
            "crates/rsid/src/provider.rs",
            r#"
use crate::model_control::ModelExecutionCapability;
fn execute(execution: ModelExecutionCapability) {
    execution
        .bind_http(
            RuntimeExecutionRoute::LocalOpenAiCompatibleHttp,
            client.post(url),
        )
        .send("x");
}
fn execute_second(execution: ModelExecutionCapability) {
    execution
        .bind_http(
            RuntimeExecutionRoute::LocalOpenAiCompatibleHttp,
            client.post(url),
        )
        .send("x");
}
"#,
        );
        let second = ExecutionBoundaryContract {
            id: "http-second",
            item: "execute_second",
            ..HTTP_BOUNDARY
        };
        validate_repository(input(
            &two_root,
            vec![PurposeDisposition {
                purpose: "fixture.paid".to_string(),
                paid_risk: PaidRisk::PaidCapable,
                route: RouteDisposition::Boundaries(vec![
                    "http".to_string(),
                    "http-second".to_string(),
                ]),
            }],
            vec![HTTP_BOUNDARY, second],
            vec![],
        ))
        .expect("independent boundaries");
        fs::remove_dir_all(two_root).expect("cleanup");
    }

    #[test]
    fn appserver_model_request_and_transport_exclusion_are_distinct() {
        let root = fixture_root("appserver");
        write(
            &root,
            "crates/rsid/src/provider.rs",
            r#"
use crate::model_control::ModelExecutionCapability;
fn send_model(execution: ModelExecutionCapability) {
    execution.send_codex_app_server(tx, bytes, method);
}
fn spawn_transport() {
    cmd.arg("app-server");
    cmd.spawn();
}
"#,
        );
        let boundary = ExecutionBoundaryContract {
            id: "appserver",
            item: "send_model",
            primitive_kind: PrimitiveKind::AppServerModelRequest,
            consumption: CapabilityConsumption::SendCodexAppServer,
            runtime_route: RuntimeExecutionRoute::CodexAppServer,
            ..HTTP_BOUNDARY
        };
        let exclusion = NonInvocationContract {
            id: "transport",
            path: "crates/rsid/src/provider.rs",
            item: "spawn_transport",
            allowed_operation: AllowedExclusionOperation::TransportSpawn,
            proof_ident: "app-server",
            occurrences: 1,
        };
        validate_repository(input(
            &root,
            vec![purpose("appserver")],
            vec![boundary],
            vec![exclusion],
        ))
        .expect("AppServer model request and transport spawn");
        fs::remove_dir_all(root).expect("cleanup");
    }

    #[test]
    fn exclusions_fail_when_paid_sink_families_appear() {
        for (name, body) in [
            ("post", "client.get(url).send(); client.post(url);"),
            ("spawn", "client.get(url).send(); cmd.spawn();"),
            (
                "chat",
                "client.get(url).send(); provider.chat(req, execution);",
            ),
            (
                "appserver",
                "client.get(url).send(); execution.send_codex_app_server(tx, bytes, method);",
            ),
        ] {
            let root = fixture_root(name);
            write(
                &root,
                "crates/rsid/src/provider.rs",
                &format!("fn probe() {{ {body} }}"),
            );
            let exclusion = NonInvocationContract {
                id: "probe",
                path: "crates/rsid/src/provider.rs",
                item: "probe",
                allowed_operation: AllowedExclusionOperation::HttpGetProbe,
                proof_ident: "get",
                occurrences: 1,
            };
            validate_repository(input(
                &root,
                vec![PurposeDisposition {
                    purpose: "fixture.catalog".to_string(),
                    paid_risk: PaidRisk::CatalogOnly,
                    route: RouteDisposition::Excluded("probe".to_string()),
                }],
                vec![],
                vec![exclusion],
            ))
            .expect_err("exclusion with paid sink");
            fs::remove_dir_all(root).expect("cleanup");
        }
    }

    #[test]
    fn rejects_paid_purpose_without_boundary_and_unknown_primitive_kind() {
        let root = fixture_root("route");
        write(&root, "crates/rsid/src/provider.rs", valid_http_source());
        let missing = PurposeDisposition {
            purpose: "fixture.paid".to_string(),
            paid_risk: PaidRisk::PaidCapable,
            route: RouteDisposition::Boundaries(vec![]),
        };
        let error = validate_repository(input(&root, vec![missing], vec![], vec![]))
            .expect_err("paid route");
        assert!(error.contains("no execution boundary"));

        let unknown = ExecutionBoundaryContract {
            primitive_kind: PrimitiveKind::Unknown,
            ..HTTP_BOUNDARY
        };
        let error = validate_repository(input(&root, vec![purpose("http")], vec![unknown], vec![]))
            .expect_err("unknown primitive");
        assert!(error.contains("unknown primitive kind"));
        fs::remove_dir_all(root).expect("cleanup");
    }

    #[test]
    fn rejects_fake_capability_wrong_route_and_detached_http_send() {
        let cases = [
            (
                "fake-capability",
                r#"
struct ModelExecutionCapability;
fn execute(execution: ModelExecutionCapability) {
    execution
        .bind_http(
            RuntimeExecutionRoute::LocalOpenAiCompatibleHttp,
            client.post(url),
        )
        .send("fixture");
}
"#,
            ),
            (
                "wrong-route",
                r#"
use crate::model_control::ModelExecutionCapability;
fn execute(execution: ModelExecutionCapability) {
    execution
        .bind_http(RuntimeExecutionRoute::DialecticHttp, client.post(url))
        .send("fixture");
}
"#,
            ),
            (
                "detached-send",
                r#"
use crate::model_control::ModelExecutionCapability;
fn execute(execution: ModelExecutionCapability) {
    let detached = client.post(url);
    detached.send();
    execution
        .bind_http(
            RuntimeExecutionRoute::LocalOpenAiCompatibleHttp,
            client.post(other_url),
        )
        .send("fixture");
}
"#,
            ),
        ];

        for (name, source) in cases {
            let root = fixture_root(name);
            write(&root, "crates/rsid/src/provider.rs", source);
            validate_repository(input(
                &root,
                vec![purpose("http")],
                vec![HTTP_BOUNDARY],
                vec![],
            ))
            .expect_err("spoofed or disconnected authority must fail");
            fs::remove_dir_all(root).expect("cleanup");
        }
    }

    #[test]
    fn rejects_shadowed_reassigned_and_disconnected_request_builders() {
        let cases = [
            (
                "shadowed-builder",
                r#"
use crate::model_control::ModelExecutionCapability;
fn execute(execution: ModelExecutionCapability) {
    let request = client.post(model_url);
    let request = client.get(catalog_url);
    execution
        .bind_http(RuntimeExecutionRoute::LocalOpenAiCompatibleHttp, request)
        .send("fixture");
}
"#,
            ),
            (
                "reassigned-builder",
                r#"
use crate::model_control::ModelExecutionCapability;
fn execute(execution: ModelExecutionCapability) {
    let mut request = client.post(model_url);
    request = client.get(catalog_url);
    execution
        .bind_http(RuntimeExecutionRoute::LocalOpenAiCompatibleHttp, request)
        .send("fixture");
}
"#,
            ),
            (
                "disconnected-builder",
                r#"
use crate::model_control::ModelExecutionCapability;
fn execute(execution: ModelExecutionCapability) {
    let _model_request = client.post(model_url);
    let catalog_request = client.get(catalog_url);
    execution
        .bind_http(RuntimeExecutionRoute::LocalOpenAiCompatibleHttp, catalog_request)
        .send("fixture");
}
"#,
            ),
            (
                "aliased-builder",
                r#"
use crate::model_control::ModelExecutionCapability;
fn execute(execution: ModelExecutionCapability) {
    let request = client.post(model_url);
    let replacement = request.header("x-test", "value");
    execution
        .bind_http(RuntimeExecutionRoute::LocalOpenAiCompatibleHttp, replacement)
        .send("fixture");
}
"#,
            ),
        ];

        for (name, source) in cases {
            let root = fixture_root(name);
            write(&root, "crates/rsid/src/provider.rs", source);
            let error = validate_repository(input(
                &root,
                vec![purpose("http")],
                vec![HTTP_BOUNDARY],
                vec![],
            ))
            .expect_err("disconnected builder flow must fail closed");
            assert!(
                error.contains("request builder") || error.contains("direct consume"),
                "{name}: {error}"
            );
            fs::remove_dir_all(root).expect("cleanup");
        }
    }

    #[test]
    fn rejects_wrong_cli_route_at_the_spawn_sink() {
        let root = fixture_root("wrong-cli-route");
        write(
            &root,
            "crates/rsid/src/provider.rs",
            r#"
use crate::model_control::CliExecutionCapability;
fn execute(execution: CliExecutionCapability) {
    execution
        .bind_command(RuntimeExecutionRoute::CodexCli, cmd)
        .spawn();
}
"#,
        );
        let error = validate_repository(input(
            &root,
            vec![purpose("cli")],
            vec![CLI_BOUNDARY],
            vec![],
        ))
        .expect_err("wrong CLI route");
        assert!(error.contains("direct consume"), "{error}");
        fs::remove_dir_all(root).expect("cleanup");
    }

    #[test]
    fn discovers_dynamic_and_alternative_unregistered_sink_shapes() {
        let cases = [
            (
                "dynamic-post",
                "fn injected(client: &Client, url: &str) { client.post(url).send(); }",
            ),
            (
                "request-post",
                "fn injected(client: &Client, url: &str) { client.request(Method::POST, url).send(); }",
            ),
            (
                "execute-post",
                "fn injected(client: &Client, url: &str) { let request = client.request(Method::POST, url); client.execute(request); }",
            ),
            (
                "prebuilt-request-post",
                "fn injected(client: &Client, url: Url) { let request = Request::new(Method::POST, url); client.execute(request); }",
            ),
            (
                "helper-request-post",
                "fn injected(transport: &Client, url: &str) { transport.execute(build_post_request(url)); }",
            ),
            (
                "associated-client-execute",
                "fn injected(client: &Client, request: Request) { Client::execute(client, request); }",
            ),
            (
                "associated-client-post",
                "fn injected(client: reqwest::Client, url: &str) { reqwest::Client::post(client, url); }",
            ),
            (
                "associated-request-builder-send",
                "fn injected(request: reqwest::RequestBuilder) { reqwest::RequestBuilder::send(request); }",
            ),
            (
                "associated-command-spawn",
                "fn injected(command: &mut tokio::process::Command) { tokio::process::Command::spawn(command); }",
            ),
            (
                "associated-sender-send",
                r#"fn injected(sender: tokio::sync::mpsc::Sender<Vec<u8>>) {
                    let request = json!({"method": "turn/start"});
                    let bytes = serialize(&request);
                    tokio::sync::mpsc::Sender::send(sender, bytes);
                }"#,
            ),
            (
                "aliased-associated-client-post",
                "use reqwest::Client as HttpClient; fn injected(client: HttpClient, url: &str) { HttpClient::post(client, url); }",
            ),
            (
                "crate-aliased-associated-client-post",
                "use reqwest as http; fn injected(client: http::Client, url: &str) { http::Client::post(client, url); }",
            ),
            (
                "type-aliased-associated-client-post",
                "type HttpClient = reqwest::Client; fn injected(client: HttpClient, url: &str) { HttpClient::post(client, url); }",
            ),
            (
                "aliased-associated-request-builder-send",
                "use reqwest::RequestBuilder as HttpRequest; fn injected(request: HttpRequest) { HttpRequest::send(request); }",
            ),
            (
                "aliased-associated-command-spawn",
                "use tokio::process::Command as TokioCommand; fn injected(command: &mut TokioCommand) { TokioCommand::spawn(command); }",
            ),
            (
                "aliased-associated-sender-send",
                r#"use tokio::sync::mpsc::Sender as TokioSender;
                fn injected(sender: TokioSender<Vec<u8>>) {
                    let request = json!({"method": "turn/start"});
                    let bytes = serialize(&request);
                    TokioSender::send(sender, bytes);
                }"#,
            ),
            (
                "provider-chat",
                "async fn injected(provider: &Provider, request: Request) { provider.chat(request).await; }",
            ),
            (
                "dynamic-cli",
                "fn injected(binary: &str) { let mut runner = Command::new(binary); runner.spawn(); }",
            ),
            (
                "raw-send",
                "fn injected(request: RequestBuilder) { request.send(); }",
            ),
            (
                "raw-appserver-send",
                r#"fn injected(model_tx: Sender<Vec<u8>>) {
                    let request = json!({"method": "turn/start"});
                    let bytes = serialize(&request);
                    model_tx.send(bytes);
                }"#,
            ),
        ];

        for (name, source) in cases {
            let root = fixture_root(name);
            write(&root, "crates/rsid/src/injected.rs", source);
            let error = validate_repository(input(&root, vec![], vec![], vec![]))
                .expect_err("unregistered sink shape must be discovered");
            assert!(error.contains("injected.rs"), "{name}: {error}");
            fs::remove_dir_all(root).expect("cleanup");
        }
    }

    #[test]
    fn rejects_real_tree_associated_dialectic_send_and_linear_url_drift() {
        let root = production_fixture_root("real-tree-associated-and-linear");
        validate_repository(ValidationInput::production(root.clone()))
            .expect("canonical production tree");

        let linear_path = root.join("crates/rsid/src/issue_tracker/linear.rs");
        let canonical_linear = fs::read_to_string(&linear_path).expect("Linear source");
        let model_linear = canonical_linear.replacen(
            r#"const LINEAR_API_URL: &str = "https://api.linear.app/graphql";"#,
            r#"const LINEAR_API_URL: &str = "https://model.invalid/v1/chat/completions";"#,
            1,
        );
        assert_ne!(canonical_linear, model_linear, "Linear constant mutation");
        fs::write(&linear_path, model_linear).expect("mutated Linear source");
        validate_repository(ValidationInput::production(root.clone()))
            .expect_err("model-family Linear destination must fail");
        fs::write(&linear_path, canonical_linear).expect("restore Linear source");

        let dialectic_path = root.join("crates/rsid/src/dialectic/mod.rs");
        let dialectic = fs::read_to_string(&dialectic_path).expect("Dialectic source");
        let extra_send = dialectic.replacen(
            "        let response = execution\n",
            "        let hidden_request = reqwest::Client::post(&self.client, &url);\n\
             let _hidden_response = reqwest::RequestBuilder::send(hidden_request).await;\n\
             \n\
             let response = execution\n",
            1,
        );
        assert_ne!(dialectic, extra_send, "Dialectic send mutation");
        fs::write(dialectic_path, extra_send).expect("mutated Dialectic source");
        validate_repository(ValidationInput::production(root.clone()))
            .expect_err("additional associated-form Dialectic send must fail");

        fs::remove_dir_all(root).expect("cleanup");
    }

    #[test]
    fn binds_non_model_post_to_one_direct_immutable_string_constant() {
        let exclusion = NonInvocationContract {
            id: "linear",
            path: "crates/rsid/src/provider.rs",
            item: "excluded",
            allowed_operation: AllowedExclusionOperation::NonModelHttpPost,
            proof_ident: "LINEAR_API_URL",
            occurrences: 1,
        };
        let purpose = PurposeDisposition {
            purpose: "fixture.linear".to_string(),
            paid_risk: PaidRisk::NonInvocation,
            route: RouteDisposition::Excluded("linear".to_string()),
        };

        let canonical = fixture_root("canonical-linear-constant");
        write(
            &canonical,
            "crates/rsid/src/provider.rs",
            r#"
const LINEAR_API_URL: &str = "https://api.linear.app/graphql";
fn excluded(client: &Client) {
    client.post(LINEAR_API_URL).send();
}
"#,
        );
        validate_repository(input(
            &canonical,
            vec![purpose.clone()],
            vec![],
            vec![exclusion],
        ))
        .expect("canonical Linear GraphQL constant");
        fs::remove_dir_all(canonical).expect("cleanup");

        let cases = [
            (
                "unresolved-linear-constant",
                r#"fn excluded(client: &Client) {
                    client.post(LINEAR_API_URL).send();
                }"#,
            ),
            (
                "mutable-linear-static",
                r#"static mut LINEAR_API_URL: &str = "https://api.linear.app/graphql";
                fn excluded(client: &Client) {
                    client.post(LINEAR_API_URL).send();
                }"#,
            ),
            (
                "shadowed-linear-constant",
                r#"const LINEAR_API_URL: &str = "https://api.linear.app/graphql";
                fn excluded(client: &Client, dynamic_url: &str) {
                    let LINEAR_API_URL = dynamic_url;
                    client.post(LINEAR_API_URL).send();
                }"#,
            ),
            (
                "reassigned-linear-constant",
                r#"const LINEAR_API_URL: &str = "https://api.linear.app/graphql";
                fn excluded(client: &Client, dynamic_url: &str) {
                    LINEAR_API_URL = dynamic_url;
                    client.post(LINEAR_API_URL).send();
                }"#,
            ),
            (
                "non-string-linear-constant",
                r#"const LINEAR_API_URL: usize = 7;
                fn excluded(client: &Client) {
                    client.post(LINEAR_API_URL).send();
                }"#,
            ),
            (
                "indirect-linear-constant",
                r#"const LINEAR_API_URL: &str = "https://api.linear.app/graphql";
                fn excluded(client: &Client) {
                    let endpoint = LINEAR_API_URL;
                    client.post(endpoint).send();
                }"#,
            ),
            (
                "ambiguous-linear-constant",
                r#"const LINEAR_API_URL: &str = "https://api.linear.app/graphql";
                const LINEAR_API_URL: &str = "https://api.linear.app/graphql";
                fn excluded(client: &Client) {
                    client.post(LINEAR_API_URL).send();
                }"#,
            ),
            (
                "indirect-linear-constant-expression",
                r#"const LINEAR_HOST: &str = "https://api.linear.app";
                const LINEAR_API_URL: &str = LINEAR_HOST;
                fn excluded(client: &Client) {
                    client.post(LINEAR_API_URL).send();
                }"#,
            ),
            (
                "model-family-linear-constant",
                r#"const LINEAR_API_URL: &str = "https://model.invalid/v1/chat/completions";
                fn excluded(client: &Client) {
                    client.post(LINEAR_API_URL).send();
                }"#,
            ),
        ];

        for (name, source) in cases {
            let root = fixture_root(name);
            write(&root, "crates/rsid/src/provider.rs", source);
            let error =
                validate_repository(input(&root, vec![purpose.clone()], vec![], vec![exclusion]))
                    .expect_err("invalid non-model POST constant must fail");
            assert!(
                error.contains("non-model POST proof") || error.contains("exclusion predicate"),
                "{name}: {error}"
            );
            fs::remove_dir_all(root).expect("cleanup");
        }
    }

    #[test]
    fn rejects_decoupled_or_drifted_exclusion_proofs() {
        let cases = [
            (
                "loopback-decoupled",
                "fn excluded() { let _validated = ollama_url(); client.post(remote_url).send(); }",
                AllowedExclusionOperation::LoopbackModelPost,
                "ollama_url",
            ),
            (
                "transport-replaced",
                "fn excluded() { let _marker = \"app-server\"; command.arg(\"serve\"); command.spawn(); }",
                AllowedExclusionOperation::TransportSpawn,
                "app-server",
            ),
            (
                "catalog-drift",
                "fn excluded() { Vec::new() }",
                AllowedExclusionOperation::CatalogOnly,
                "append_catalog_family",
            ),
            (
                "queue-drift",
                "fn excluded() { Ok(()) }",
                AllowedExclusionOperation::QueueNoOp,
                "Ok",
            ),
            (
                "http-get-decoupled",
                r#"fn excluded(client: &Client, dynamic_url: &str) {
                    let _allowed = "/api/tags";
                    client.get(dynamic_url).send();
                }"#,
                AllowedExclusionOperation::HttpGetProbe,
                "/api/tags",
            ),
            (
                "http-get-decoy-variable-name",
                r#"fn excluded(client: &Client, dynamic_url: &str) {
                    let models = dynamic_url;
                    client.get(models).send();
                }"#,
                AllowedExclusionOperation::HttpGetProbe,
                "models",
            ),
            (
                "non-model-post-decoupled",
                r#"fn excluded(client: &Client, dynamic_url: &str) {
                    let _allowed = LINEAR_API_URL;
                    client.post(dynamic_url).send();
                }"#,
                AllowedExclusionOperation::NonModelHttpPost,
                "LINEAR_API_URL",
            ),
            (
                "non-model-post-url-reassigned",
                r#"fn excluded(client: &Client, dynamic_url: &str) {
                    let mut endpoint = LINEAR_API_URL;
                    endpoint = dynamic_url;
                    client.post(endpoint).send();
                }"#,
                AllowedExclusionOperation::NonModelHttpPost,
                "LINEAR_API_URL",
            ),
            (
                "non-model-post-shadowed-constant",
                r#"fn excluded(client: &Client, dynamic_url: &str) {
                    let LINEAR_API_URL = dynamic_url;
                    client.post(LINEAR_API_URL).send();
                }"#,
                AllowedExclusionOperation::NonModelHttpPost,
                "LINEAR_API_URL",
            ),
            (
                "non-model-post-parameter-shadow",
                r#"fn excluded(client: &Client, LINEAR_API_URL: &str) {
                    client.post(LINEAR_API_URL).send();
                }"#,
                AllowedExclusionOperation::NonModelHttpPost,
                "LINEAR_API_URL",
            ),
            (
                "non-model-post-literal-decoy",
                r#"fn excluded(client: &Client) {
                    client.post("https://model.invalid/LINEAR_API_URL").send();
                }"#,
                AllowedExclusionOperation::NonModelHttpPost,
                "LINEAR_API_URL",
            ),
        ];

        for (name, source, allowed_operation, proof_ident) in cases {
            let root = fixture_root(name);
            write(&root, "crates/rsid/src/provider.rs", source);
            let exclusion = NonInvocationContract {
                id: "excluded",
                path: "crates/rsid/src/provider.rs",
                item: "excluded",
                allowed_operation,
                proof_ident,
                occurrences: 1,
            };
            validate_repository(input(
                &root,
                vec![PurposeDisposition {
                    purpose: "fixture.excluded".to_string(),
                    paid_risk: PaidRisk::NonInvocation,
                    route: RouteDisposition::Excluded("excluded".to_string()),
                }],
                vec![],
                vec![exclusion],
            ))
            .expect_err("decoupled exclusion proof must fail");
            fs::remove_dir_all(root).expect("cleanup");
        }
    }

    #[test]
    fn rejects_registry_markdown_drift() {
        let root = fixture_root("markdown");
        write(&root, "crates/rsid/src/provider.rs", valid_http_source());
        let document = root.join("registry.md");
        fs::write(
            &document,
            "## Registered Invocation Purposes\n| stale |\n## Explicit Non-Invocation Exclusions\n",
        )
        .expect("document");
        let mut validation = input(&root, vec![purpose("http")], vec![HTTP_BOUNDARY], vec![]);
        validation.markdown = Some((
            document,
            "| expected |\n".to_string(),
            "| expected exclusion |\n".to_string(),
        ));
        let error = validate_repository(validation).expect_err("Markdown drift");
        assert!(error.contains("Markdown drift"));
        fs::remove_dir_all(root).expect("cleanup");
    }

    #[test]
    fn rejects_exclusion_markdown_drift_independently() {
        let root = fixture_root("exclusion-markdown");
        write(&root, "crates/rsid/src/provider.rs", valid_http_source());
        let document = root.join("registry.md");
        fs::write(
            &document,
            "## Registered Invocation Purposes\n| expected purpose |\n\
             ## Explicit Non-Invocation Exclusions\n| stale exclusion |\n",
        )
        .expect("document");
        let mut validation = input(&root, vec![purpose("http")], vec![HTTP_BOUNDARY], vec![]);
        validation.markdown = Some((
            document,
            "| expected purpose |\n".to_string(),
            "| expected exclusion |\n".to_string(),
        ));
        let error = validate_repository(validation).expect_err("exclusion Markdown drift");
        assert!(error.contains("exclusion Markdown drift"), "{error}");
        assert!(!error.contains("registry Markdown drift"), "{error}");
        fs::remove_dir_all(root).expect("cleanup");
    }
}
