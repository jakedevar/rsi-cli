//! Portable install bundle RPC handlers (#1406).
//!
//! `ExportPortableBundle {path, overwrite?}` writes a clean bundle of this
//! install's durable state (no history, no secrets) to an absolute `path`.
//! `ImportPortableBundle {path, merge?, path_remaps?, dry_run?}` imports one into this
//! daemon's database in one transaction; the database itself was built by the
//! normal migrations when the daemon first opened it.
//!
//! Both are operator-only (AGENTS.md hard rule 10): they are not declared in
//! the attributed verb registry, so a tokened caller is default-denied before
//! dispatch, and the handlers refuse one again. The export passes every vault
//! entry and set credential env var as forbidden values, so a bundle that
//! would carry one is refused, never written. Logic lives in
//! `rsid_store::store::portable_bundle`.

use std::path::{Path, PathBuf};

use super::{RpcRequest, RpcServer};
use crate::error::{DaemonError, Result};
use crate::store::portable_bundle::{self, PathRemap, PortableImportOptions};
use serde::Deserialize;

/// Operator RPC methods of this family. Never in an agent catalog.
pub const OPERATOR_METHODS: [&str; 2] = ["ExportPortableBundle", "ImportPortableBundle"];
/// Stable refusal code for a session-attributed call.
pub const OPERATOR_ONLY_REFUSAL: &str = "portable_bundle_operator_only";
/// Directory (beside the database) that receives imported manager templates.
pub const TEMPLATES_DIR: &str = "portable";

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ExportParams {
    path: String,
    #[serde(default)]
    overwrite: bool,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ImportParams {
    path: String,
    #[serde(default)]
    merge: bool,
    #[serde(default)]
    path_remaps: Vec<PathRemap>,
    /// Check and report without committing (the CLI's remap preview).
    #[serde(default)]
    dry_run: bool,
}

fn parse<T: for<'de> Deserialize<'de>>(method: &str, request: &RpcRequest) -> Result<T> {
    serde_json::from_value(request.params.clone())
        .map_err(|error| DaemonError::InvalidParam(format!("invalid {method} params: {error}")))
}

fn refuse_attributed(method: &str, request: &RpcRequest) -> Result<()> {
    if request.session_token.is_some() {
        return Err(DaemonError::PolicyDenied(format!(
            "{OPERATOR_ONLY_REFUSAL}: `{method}` is operator-only and is not available to session-attributed callers"
        )));
    }
    Ok(())
}

fn absolute(path: &str) -> Result<PathBuf> {
    let path = PathBuf::from(path);
    if !path.is_absolute() {
        return Err(DaemonError::InvalidParam(format!(
            "portable_bundle_path_relative: `{}` must be an absolute path",
            path.display()
        )));
    }
    Ok(path)
}

/// Write the bundle's manager templates beside the database, if it has any.
fn write_templates(
    db_path: Option<&Path>,
    bundle: &portable_bundle::PortableBundle,
) -> Result<Option<String>> {
    if bundle.manager_templates.is_empty() {
        return Ok(None);
    }
    let Some(dir) = db_path.and_then(Path::parent) else {
        return Ok(None);
    };
    let dir = dir.join(TEMPLATES_DIR);
    std::fs::create_dir_all(&dir)?;
    let stamp: String = bundle
        .exported_at
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    let path = dir.join(format!("manager-templates-{stamp}.json"));
    let body = serde_json::json!({
        "source": "rsi portable bundle manager templates (#1406)",
        "note": "Session-free manager and portfolio policies from the exporting install. Reuse them when you appoint managers here; they are not live.",
        "exported_at": bundle.exported_at,
        "templates": bundle.manager_templates,
    });
    std::fs::write(&path, serde_json::to_string_pretty(&body)?)?;
    Ok(Some(path.display().to_string()))
}

impl RpcServer {
    pub(super) async fn handle_export_portable_bundle(
        &self,
        request: &RpcRequest,
    ) -> Result<serde_json::Value> {
        let method = "ExportPortableBundle";
        refuse_attributed(method, request)?;
        let params: ExportParams = parse(method, request)?;
        let path = absolute(&params.path)?;
        let secrets = crate::vault::global().known_secret_values();
        let bundle = {
            let store = self.session_manager.store().lock().await;
            store.export_portable_bundle(&secrets)?
        };
        let summary = bundle.summary();
        let write_path = path.clone();
        tokio::task::spawn_blocking(move || {
            portable_bundle::write_bundle(&write_path, &bundle, params.overwrite)
        })
        .await
        .map_err(|_| DaemonError::Process("portable bundle write task failed".into()))??;
        Ok(serde_json::json!({
            "path": path.display().to_string(),
            "summary": summary,
        }))
    }

    pub(super) async fn handle_import_portable_bundle(
        &self,
        request: &RpcRequest,
    ) -> Result<serde_json::Value> {
        let method = "ImportPortableBundle";
        refuse_attributed(method, request)?;
        let params: ImportParams = parse(method, request)?;
        let path = absolute(&params.path)?;
        let bundle = tokio::task::spawn_blocking(move || portable_bundle::read_bundle(&path))
            .await
            .map_err(|_| DaemonError::Process("portable bundle read task failed".into()))??;
        let options = PortableImportOptions {
            merge: params.merge,
            path_remaps: params.path_remaps,
            dry_run: params.dry_run,
        };
        let (report, db_path) = {
            let store = self.session_manager.store().lock().await;
            let report = store.import_portable_bundle(&bundle, &options)?;
            let db_path = store
                .conn
                .path()
                .filter(|path| !path.is_empty())
                .map(PathBuf::from);
            (report, db_path)
        };
        let templates_path = if options.dry_run {
            None
        } else {
            write_templates(db_path.as_deref(), &bundle)?
        };
        Ok(serde_json::json!({
            "report": report,
            "manager_templates_path": templates_path,
            // Settings are read at daemon start; credentials stay an operator gate.
            "restart_required": !options.dry_run,
            "credentials_imported": false,
        }))
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn portable_bundle_methods_stay_out_of_every_agent_catalog() {
        let catalog = rsi_common::agent_control_schema::agent_control_catalog_v1();
        for method in OPERATOR_METHODS {
            assert!(
                !super::super::agent_gate::AGENT_VERBS.contains(&method),
                "{method}"
            );
            assert!(
                !super::super::agent_gate::READ_VERBS.contains(&method),
                "{method}"
            );
            assert!(
                !super::super::agent_gate::is_allowed_for_attributed_caller(method),
                "{method}"
            );
            assert!(
                !rsi_common::rpc_verb_registry::cli_verb_methods().contains(&method),
                "{method}"
            );
            for descriptor in catalog {
                assert_ne!(descriptor.method, method, "agent CLI catalog: {method}");
                if let Some(tool) = descriptor.native_tool {
                    assert!(
                        !tool.name().to_ascii_lowercase().contains("portable"),
                        "native tool {} exposes {method}",
                        tool.name()
                    );
                }
            }
            let mut request = RpcRequest::new(method, serde_json::json!({"path": "/tmp/x"}));
            request.session_token = Some("agent-token".to_string());
            let error = refuse_attributed(method, &request).unwrap_err().to_string();
            assert!(error.contains(OPERATOR_ONLY_REFUSAL), "{error}");
        }
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn portable_bundle_paths_must_be_absolute() {
        let error = absolute("bundle.json").unwrap_err().to_string();
        assert!(error.contains("portable_bundle_path_relative"), "{error}");
        let root = std::env::temp_dir().join("bundle.json");
        assert_eq!(absolute(&root.to_string_lossy()).unwrap(), root);
    }
}
