//! Portable install bundle (#1406): a clean copy of this install's durable
//! state, without history or secrets, that starts a new machine.
//!
//! The bundle is one JSON document ([`PortableBundle`]) that names the format
//! version and the schema version it was written at.
//!
//! - **Carried over** ([`CARRIED_TABLES`]): projects (paths remappable at
//!   import), operator settings (`daemon_settings` minus runtime state and
//!   secret-named keys), global and project permission rules, durable model
//!   budget policies, and every Issue with its events and dependencies.
//! - **Manager templates** ([`TEMPLATE_TABLES`]): manager and portfolio
//!   policies with every live session reference removed. They are never
//!   inserted into the live manager tables (a seat needs a live session and an
//!   operator appointment); import hands them back for the operator to reuse.
//! - **Left behind**: sessions, conversation events, sandboxes, jobs, ledgers,
//!   wakes, landing queue state and every other table, and the key vault,
//!   which is a separate file this module never reads.
//!
//! Export refuses to write a bundle that contains any value the caller names as
//! a secret (the daemon passes every vault entry and every set credential env
//! var). Import runs in one transaction on a database that the normal
//! migrations already built, refuses a bundle from a newer schema, and refuses
//! a non-empty database unless asked to merge.

use super::Store;
use crate::error::{DaemonError, Result};
use crate::vault::SecretString;
use rusqlite::types::{Value as SqlValue, ValueRef};
use rusqlite::{Transaction, TransactionBehavior};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};

/// `format` field of every bundle.
pub const PORTABLE_BUNDLE_FORMAT: &str = "rsi.portable_bundle";
/// Newest bundle layout this build reads and the one it writes.
pub const PORTABLE_BUNDLE_FORMAT_VERSION: u32 = 1;
/// Shortest secret the export scan matches (shorter values would match
/// ordinary text; no real credential is this short).
pub const MIN_SCANNED_SECRET_BYTES: usize = 8;

/// One carried table: its name, the rows that are durable, and a stable order.
#[derive(Debug, Clone, Copy)]
pub struct CarriedTable {
    pub name: &'static str,
    /// SQL `WHERE` predicate selecting the durable rows (`"1"` for all).
    pub filter: &'static str,
    pub order_by: &'static str,
}

/// Tables copied into the bundle and imported in this (foreign-key) order.
pub const CARRIED_TABLES: &[CarriedTable] = &[
    CarriedTable {
        name: "projects",
        filter: "1",
        order_by: "created_at, id",
    },
    CarriedTable {
        name: "daemon_settings",
        filter: "1",
        order_by: "key",
    },
    CarriedTable {
        name: "permission_rules",
        filter: "scope IN ('global','project')",
        order_by: "id",
    },
    CarriedTable {
        name: "model_budget_policies",
        filter: "scope_kind IN ('global','provider','project','subsystem','issue_tracker','operator')",
        order_by: "policy_key",
    },
    CarriedTable {
        name: "issues",
        filter: "1",
        order_by: "display_number",
    },
    CarriedTable {
        name: "issue_events",
        filter: "1",
        order_by: "issue_id, sequence",
    },
    CarriedTable {
        name: "issue_deps",
        filter: "1",
        order_by: "project_id, issue_id, depends_on_id",
    },
];

/// Manager and portfolio policy tables exported as session-free templates.
pub const TEMPLATE_TABLES: &[CarriedTable] = &[
    CarriedTable {
        name: "harness_manager_v2_policies",
        filter: "1",
        order_by: "project_id",
    },
    CarriedTable {
        name: "harness_manager_scopes",
        filter: "1",
        order_by: "project_id",
    },
    CarriedTable {
        name: "manager_nodes",
        filter: "state='active'",
        order_by: "created_at, id",
    },
    CarriedTable {
        name: "manager_node_scopes",
        filter: "node_id IN (SELECT id FROM manager_nodes WHERE state='active')",
        order_by: "node_id, project_id",
    },
    CarriedTable {
        name: "manager_node_grants",
        filter: "state='granted' AND node_id IN (SELECT id FROM manager_nodes WHERE state='active')",
        order_by: "node_id, grant_version",
    },
    CarriedTable {
        name: "manager_portfolio_nodes",
        filter: "state='active'",
        order_by: "created_at, id",
    },
    CarriedTable {
        name: "manager_portfolio_coverage",
        filter: "node_id IN (SELECT id FROM manager_portfolio_nodes WHERE state='active')",
        order_by: "project_id, depth",
    },
    CarriedTable {
        name: "global_manager_grants",
        filter: "state='active'",
        order_by: "grant_version",
    },
];

/// History and runtime tables a bundle never carries. The manifest records
/// their source row counts; a fresh import leaves them empty.
pub const LEFT_BEHIND_TABLES: &[&str] = &[
    "sessions",
    "conversation_events",
    "sandbox_custody_roots",
    "agent_jobs",
    "scheduled_jobs",
    "rolling_queue_entries",
    "model_invocations",
    "turn_metrics",
    "observations",
    "friction_events",
];

/// Tables whose rows make a database non-empty for import purposes. Settings
/// are excluded: a fresh daemon seeds some on its first start.
const OCCUPANCY_TABLES: &[&str] = &["projects", "issues", "sessions", "ideas"];

/// `daemon_settings` key prefixes that are runtime state of this install.
const RUNTIME_SETTING_PREFIXES: &[&str] = &[
    "c5.autofile.",
    "model_control_breaker_",
    "model_control_circuits",
    "internal_watchdog_probe_nonce",
    "manager_operator_pause:",
    "worker_baton:",
    "context_succession_due:",
    "target_reclaim_recent_cursor",
];

/// Key fragments that mark a setting as secret-bearing; such a row is
/// withheld from every bundle whatever its value.
const SECRET_SETTING_FRAGMENTS: &[&str] = &[
    "secret",
    "password",
    "passwd",
    "api_key",
    "apikey",
    "credential",
    "bearer",
    "private_key",
    "access_key",
    "session_token",
];

/// The serialized bundle.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PortableBundle {
    pub format: String,
    pub format_version: u32,
    /// `PRAGMA user_version` of the exporting database.
    pub schema_version: i32,
    pub exported_at: String,
    /// `std::env::consts::OS` of the exporting machine (informational).
    pub source_os: String,
    /// Carried rows by table, column name to value.
    pub tables: BTreeMap<String, Vec<Map<String, Value>>>,
    /// Session-free manager and portfolio policy templates by source table.
    #[serde(default)]
    pub manager_templates: BTreeMap<String, Vec<Map<String, Value>>>,
    /// Source row counts of tables deliberately left behind.
    #[serde(default)]
    pub left_behind: BTreeMap<String, i64>,
    /// Settings withheld as runtime state or secret-named, one label each (see
    /// [`withheld_label`]): a runtime-state key by name, a secret-named key
    /// (or any key holding a known secret) only as `redacted:<hash>`.
    #[serde(default)]
    pub withheld_settings: Vec<String>,
}

/// Rewrite every project path under `from` to the same place under `to`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PathRemap {
    pub from: String,
    pub to: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PortableImportOptions {
    /// Import alongside existing state: rows already present (by key) are
    /// kept and the bundle's copy is skipped.
    #[serde(default)]
    pub merge: bool,
    #[serde(default)]
    pub path_remaps: Vec<PathRemap>,
    /// Run every check and insert, report, then roll back (the CLI's preview
    /// of project paths before it asks for remaps).
    #[serde(default)]
    pub dry_run: bool,
}

/// One imported project and its path on this machine.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImportedProject {
    pub id: String,
    pub name: String,
    pub path: Option<String>,
    /// The bundle's path when a remap changed it.
    pub remapped_from: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PortableImportReport {
    pub bundle_schema_version: i32,
    pub schema_version: i32,
    pub merge: bool,
    /// Nothing was committed.
    pub dry_run: bool,
    pub inserted: BTreeMap<String, u64>,
    /// Rows skipped in a merge because the key exists or a parent was skipped.
    pub skipped: BTreeMap<String, u64>,
    pub projects: Vec<ImportedProject>,
    pub manager_template_rows: u64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PortableExportSummary {
    pub schema_version: i32,
    pub rows: BTreeMap<String, u64>,
    pub manager_template_rows: u64,
    pub left_behind: BTreeMap<String, i64>,
    pub withheld_settings: Vec<String>,
}

impl PortableBundle {
    #[must_use]
    pub fn summary(&self) -> PortableExportSummary {
        PortableExportSummary {
            schema_version: self.schema_version,
            rows: self
                .tables
                .iter()
                .map(|(table, rows)| (table.clone(), rows.len() as u64))
                .collect(),
            manager_template_rows: self
                .manager_templates
                .values()
                .map(|r| r.len() as u64)
                .sum(),
            left_behind: self.left_behind.clone(),
            withheld_settings: self.withheld_settings.clone(),
        }
    }

    /// Check the format, format version and schema version against this build.
    ///
    /// # Errors
    ///
    /// Refuses another format, a newer format version, a bundle from a newer
    /// schema than this build's migrations reach, and unknown tables.
    pub fn validate(&self) -> Result<()> {
        if self.format != PORTABLE_BUNDLE_FORMAT {
            return Err(DaemonError::InvalidParam(format!(
                "portable_bundle_format_unknown: `{}` is not an rsi portable bundle",
                self.format
            )));
        }
        if self.format_version > PORTABLE_BUNDLE_FORMAT_VERSION {
            return Err(DaemonError::InvalidParam(format!(
                "portable_bundle_format_too_new: bundle format v{} is newer than this rsi reads (v{PORTABLE_BUNDLE_FORMAT_VERSION}); upgrade rsi first",
                self.format_version
            )));
        }
        let supported = super::LATEST_SCHEMA_VERSION;
        if self.schema_version > supported {
            return Err(DaemonError::InvalidParam(format!(
                "portable_bundle_schema_too_new: bundle schema v{} is newer than this rsi's schema v{supported}; upgrade rsi first",
                self.schema_version
            )));
        }
        for table in self.tables.keys() {
            if !CARRIED_TABLES.iter().any(|carried| carried.name == table) {
                return Err(DaemonError::InvalidParam(format!(
                    "portable_bundle_table_unknown: bundle carries `{table}`, which a portable import never writes"
                )));
            }
        }
        Ok(())
    }
}

/// Read and validate a bundle file.
///
/// # Errors
///
/// I/O and JSON errors, then [`PortableBundle::validate`].
pub fn read_bundle(path: &Path) -> Result<PortableBundle> {
    let text = std::fs::read_to_string(path).map_err(|error| {
        DaemonError::InvalidParam(format!(
            "portable_bundle_unreadable: {}: {error}",
            path.display()
        ))
    })?;
    let bundle: PortableBundle = serde_json::from_str(&text).map_err(|error| {
        DaemonError::InvalidParam(format!(
            "portable_bundle_malformed: {}: {error}",
            path.display()
        ))
    })?;
    bundle.validate()?;
    Ok(bundle)
}

/// Write `bundle` to `path` through a sibling temporary file and a rename.
///
/// # Errors
///
/// Refuses an existing `path` unless `overwrite`, and a missing parent.
pub fn write_bundle(path: &Path, bundle: &PortableBundle, overwrite: bool) -> Result<()> {
    if path.exists() && !overwrite {
        return Err(DaemonError::InvalidParam(format!(
            "portable_bundle_exists: {} already exists; pass overwrite to replace it",
            path.display()
        )));
    }
    let parent = match path.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent.to_path_buf(),
        _ => PathBuf::from("."),
    };
    if !parent.is_dir() {
        return Err(DaemonError::InvalidParam(format!(
            "portable_bundle_parent_missing: {} is not a directory",
            parent.display()
        )));
    }
    let file_name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "bundle".to_string());
    let tmp = parent.join(format!(".{file_name}.{}.tmp", uuid::Uuid::new_v4()));
    let text = serde_json::to_string_pretty(bundle)?;
    std::fs::write(&tmp, text)?;
    if overwrite && path.exists() {
        // Windows `rename` does not replace an existing file.
        std::fs::remove_file(path)?;
    }
    if let Err(error) = std::fs::rename(&tmp, path) {
        let _ = std::fs::remove_file(&tmp);
        return Err(error.into());
    }
    Ok(())
}

/// Whether `key` is runtime state or secret-named, so it never travels.
#[must_use]
pub fn setting_is_withheld(key: &str) -> bool {
    let lower = key.to_ascii_lowercase();
    RUNTIME_SETTING_PREFIXES
        .iter()
        .any(|prefix| lower.starts_with(prefix))
        || SECRET_SETTING_FRAGMENTS
            .iter()
            .any(|fragment| lower.contains(fragment))
        || contains_uuid(&lower)
}

/// The label under which a withheld settings key appears in the bundle and the
/// export summary. A key is a name chosen by the operator or a feature and can
/// hold a credential (`credential:<token>`), so a secret-named key or one that
/// contains a known secret is never written: it becomes `redacted:` plus the
/// first 12 hex digits of its SHA-256, which lets the operator tell the
/// withheld keys apart (and match one by hashing its name) without the name.
fn withheld_label(key: &str, secrets: &[&str]) -> String {
    let lower = key.to_ascii_lowercase();
    let secret_named = SECRET_SETTING_FRAGMENTS
        .iter()
        .any(|fragment| lower.contains(fragment))
        || secrets.iter().any(|secret| key.contains(secret));
    if !secret_named {
        return key.to_string();
    }
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(key.as_bytes());
    format!("redacted:{}", &hex::encode(digest)[..12])
}

/// Whether `text` holds a canonical lowercase UUID (an entity-scoped key).
fn contains_uuid(text: &str) -> bool {
    let bytes = text.as_bytes();
    bytes.len() >= 36
        && (0..=bytes.len() - 36).any(|start| {
            text.get(start..start + 36)
                .and_then(|candidate| uuid::Uuid::parse_str(candidate).ok())
                .is_some_and(|parsed| parsed.hyphenated().to_string() == text[start..start + 36])
        })
}

/// Apply the first matching remap to `path`. A remap matches the whole path or
/// a prefix that ends at a separator (`/` or `\`, so a bundle from any OS
/// remaps); the rest is joined under `to` with this machine's separator.
#[must_use]
pub fn remap_path(path: &str, remaps: &[PathRemap]) -> Option<String> {
    for remap in remaps {
        let from = remap.from.trim_end_matches(['/', '\\']);
        if from.is_empty() {
            continue;
        }
        if path == from || path == remap.from {
            return Some(remap.to.clone());
        }
        if let Some(rest) = path.strip_prefix(from)
            && rest.starts_with(['/', '\\'])
        {
            let mut target = PathBuf::from(&remap.to);
            for component in rest.split(['/', '\\']).filter(|part| !part.is_empty()) {
                target.push(component);
            }
            return Some(target.to_string_lossy().into_owned());
        }
    }
    None
}

fn sql_to_json(value: ValueRef<'_>) -> Value {
    match value {
        ValueRef::Null => Value::Null,
        ValueRef::Integer(number) => Value::from(number),
        ValueRef::Real(number) => serde_json::Number::from_f64(number)
            .map(Value::Number)
            .unwrap_or(Value::Null),
        ValueRef::Text(bytes) => Value::String(String::from_utf8_lossy(bytes).into_owned()),
        ValueRef::Blob(bytes) => {
            let hex: String = bytes.iter().map(|byte| format!("{byte:02x}")).collect();
            serde_json::json!({ "blob_hex": hex })
        }
    }
}

fn json_to_sql(table: &str, column: &str, value: &Value) -> Result<SqlValue> {
    Ok(match value {
        Value::Null => SqlValue::Null,
        Value::Bool(flag) => SqlValue::Integer(i64::from(*flag)),
        Value::Number(number) => match number.as_i64() {
            Some(integer) => SqlValue::Integer(integer),
            None => SqlValue::Real(number.as_f64().unwrap_or_default()),
        },
        Value::String(text) => SqlValue::Text(text.clone()),
        Value::Object(object) if object.len() == 1 && object.contains_key("blob_hex") => {
            let hex = object["blob_hex"].as_str().unwrap_or_default();
            let bytes = (0..hex.len())
                .step_by(2)
                .map(|start| {
                    hex.get(start..start + 2)
                        .and_then(|pair| u8::from_str_radix(pair, 16).ok())
                })
                .collect::<Option<Vec<u8>>>()
                .ok_or_else(|| {
                    DaemonError::InvalidParam(format!(
                        "portable_bundle_malformed: {table}.{column} holds an invalid blob"
                    ))
                })?;
            SqlValue::Blob(bytes)
        }
        _ => {
            return Err(DaemonError::InvalidParam(format!(
                "portable_bundle_malformed: {table}.{column} holds a nested JSON value"
            )));
        }
    })
}

/// Recursively whether any string inside `value` contains a secret.
fn holds_secret(value: &Value, secrets: &[&str]) -> bool {
    match value {
        Value::String(text) => secrets.iter().any(|secret| text.contains(secret)),
        Value::Array(items) => items.iter().any(|item| holds_secret(item, secrets)),
        Value::Object(object) => object.iter().any(|(key, item)| {
            secrets.iter().any(|s| key.contains(s)) || holds_secret(item, secrets)
        }),
        _ => false,
    }
}

/// Template column names that reference live sessions or idempotency state.
fn template_column_dropped(column: &str) -> bool {
    column.ends_with("session_id")
        || matches!(
            column,
            "epic_ids_json" | "group_ids_json" | "idempotency_key" | "operator_origin"
        )
}

/// JSON keys inside a template policy that name live sessions.
fn template_key_scrubbed(key: &str) -> bool {
    key.ends_with("session_id")
        || key.ends_with("session_ids")
        || matches!(
            key,
            "epic_id" | "epic_ids" | "paused_epic_ids" | "group_id" | "group_ids"
        )
}

fn scrub_template_json(value: &mut Value, sessions: &HashSet<String>) {
    match value {
        Value::String(text) if sessions.contains(text.as_str()) => *value = Value::Null,
        Value::Array(items) => {
            for item in items.iter_mut() {
                scrub_template_json(item, sessions);
            }
            items.retain(|item| !item.is_null());
        }
        Value::Object(object) => {
            for (key, item) in object.iter_mut() {
                if template_key_scrubbed(key) {
                    *item = if item.is_array() {
                        Value::Array(Vec::new())
                    } else {
                        Value::Null
                    };
                } else {
                    scrub_template_json(item, sessions);
                }
            }
        }
        _ => {}
    }
}

impl Store {
    fn table_exists(&self, table: &str) -> Result<bool> {
        Ok(self.conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name=?1)",
            [table],
            |row| row.get(0),
        )?)
    }

    fn table_columns(conn: &rusqlite::Connection, table: &str) -> Result<Vec<String>> {
        let mut statement = conn.prepare("SELECT name FROM pragma_table_info(?1)")?;
        let columns = statement
            .query_map([table], |row| row.get::<_, String>(0))?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(columns)
    }

    fn dump_table(&self, table: &CarriedTable) -> Result<Vec<Map<String, Value>>> {
        if !self.table_exists(table.name)? {
            return Ok(Vec::new());
        }
        let sql = format!(
            // sql-dynamic-ok: table, filter and order come from the static carried lists.
            "SELECT * FROM \"{}\" WHERE {} ORDER BY {}",
            table.name, table.filter, table.order_by
        );
        let mut statement = self.conn.prepare(&sql)?;
        let names: Vec<String> = statement
            .column_names()
            .into_iter()
            .map(str::to_string)
            .collect();
        let rows = statement
            .query_map([], |row| {
                let mut object = Map::new();
                for (index, name) in names.iter().enumerate() {
                    object.insert(name.clone(), sql_to_json(row.get_ref(index)?));
                }
                Ok(object)
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Build a clean bundle of this database's durable state.
    ///
    /// `forbidden_secrets` are values that must not appear anywhere in the
    /// bundle (the daemon passes every vault entry and set credential env
    /// var); values shorter than [`MIN_SCANNED_SECRET_BYTES`] are ignored.
    ///
    /// # Errors
    ///
    /// SQL errors, and `portable_export_secret_found` naming the table, row
    /// and column (never the value) when a carried value holds a secret.
    pub fn export_portable_bundle(
        &self,
        forbidden_secrets: &[SecretString],
    ) -> Result<PortableBundle> {
        let schema_version: i32 = self
            .conn
            .query_row("PRAGMA user_version", [], |row| row.get(0))?;
        let mut tables = BTreeMap::new();
        let mut withheld_settings = Vec::new();
        let scanned = scanned_secrets(forbidden_secrets);
        for table in CARRIED_TABLES {
            let mut rows = self.dump_table(table)?;
            if table.name == "daemon_settings" {
                rows.retain(|row| {
                    let key = row.get("key").and_then(Value::as_str).unwrap_or_default();
                    let keep = !setting_is_withheld(key);
                    if !keep {
                        withheld_settings.push(withheld_label(key, &scanned));
                    }
                    keep
                });
            }
            tables.insert(table.name.to_string(), rows);
        }

        let sessions: HashSet<String> = {
            let mut statement = self.conn.prepare("SELECT id FROM sessions")?;
            statement
                .query_map([], |row| row.get::<_, String>(0))?
                .collect::<std::result::Result<HashSet<_>, _>>()?
        };
        let mut manager_templates = BTreeMap::new();
        for table in TEMPLATE_TABLES {
            let mut rows = self.dump_table(table)?;
            for row in &mut rows {
                row.retain(|column, _| !template_column_dropped(column));
                for (column, value) in row.iter_mut() {
                    if column.ends_with("_json")
                        && let Some(text) = value.as_str()
                        && let Ok(mut parsed) = serde_json::from_str::<Value>(text)
                    {
                        scrub_template_json(&mut parsed, &sessions);
                        *value = parsed;
                    } else {
                        scrub_template_json(value, &sessions);
                    }
                }
            }
            if !rows.is_empty() {
                manager_templates.insert(table.name.to_string(), rows);
            }
        }

        let mut left_behind = BTreeMap::new();
        for table in LEFT_BEHIND_TABLES {
            if self.table_exists(table)? {
                let sql = format!(
                    // sql-dynamic-ok: table from the static LEFT_BEHIND_TABLES list.
                    "SELECT COUNT(*) FROM \"{table}\""
                );
                let count: i64 = self.conn.query_row(&sql, [], |row| row.get(0))?;
                left_behind.insert((*table).to_string(), count);
            }
        }

        let bundle = PortableBundle {
            format: PORTABLE_BUNDLE_FORMAT.to_string(),
            format_version: PORTABLE_BUNDLE_FORMAT_VERSION,
            schema_version,
            exported_at: chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Nanos, true),
            source_os: std::env::consts::OS.to_string(),
            tables,
            manager_templates,
            left_behind,
            withheld_settings,
        };
        refuse_secrets(&bundle, forbidden_secrets)?;
        Ok(bundle)
    }

    /// Whether a portable import would find this database occupied.
    ///
    /// # Errors
    ///
    /// SQL errors.
    pub fn portable_import_target_occupied(&self) -> Result<Vec<String>> {
        let mut occupied = Vec::new();
        for table in OCCUPANCY_TABLES {
            if self.table_exists(table)? {
                let sql = format!(
                    // sql-dynamic-ok: table from the static OCCUPANCY_TABLES list.
                    "SELECT EXISTS(SELECT 1 FROM \"{table}\")"
                );
                if self.conn.query_row(&sql, [], |row| row.get::<_, bool>(0))? {
                    occupied.push((*table).to_string());
                }
            }
        }
        Ok(occupied)
    }

    /// Import `bundle` in one transaction.
    ///
    /// # Errors
    ///
    /// [`PortableBundle::validate`]; `portable_import_target_not_empty` for an
    /// occupied database without `merge`; a bundle column the schema lacks;
    /// any constraint failure (the whole import rolls back).
    pub fn import_portable_bundle(
        &self,
        bundle: &PortableBundle,
        options: &PortableImportOptions,
    ) -> Result<PortableImportReport> {
        bundle.validate()?;
        let schema_version: i32 = self
            .conn
            .query_row("PRAGMA user_version", [], |row| row.get(0))?;
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let occupied = self.portable_import_target_occupied()?;
        if !occupied.is_empty() && !options.merge {
            return Err(DaemonError::InvalidParam(format!(
                "portable_import_target_not_empty: this database already holds {}; import into a fresh install or pass merge",
                occupied.join(", ")
            )));
        }
        let mut report = PortableImportReport {
            bundle_schema_version: bundle.schema_version,
            schema_version,
            merge: options.merge,
            dry_run: options.dry_run,
            manager_template_rows: bundle
                .manager_templates
                .values()
                .map(|rows| rows.len() as u64)
                .sum(),
            ..PortableImportReport::default()
        };
        for table in CARRIED_TABLES {
            let Some(rows) = bundle.tables.get(table.name) else {
                continue;
            };
            let columns: HashSet<String> =
                Self::table_columns(&tx, table.name)?.into_iter().collect();
            if columns.is_empty() {
                return Err(DaemonError::InvalidParam(format!(
                    "portable_bundle_table_unknown: this schema has no `{}` table",
                    table.name
                )));
            }
            let mut inserted = 0_u64;
            let mut skipped = 0_u64;
            for source in rows {
                let mut row = source.clone();
                let Some(row) =
                    prepare_import_row(&tx, table.name, &mut row, options, &mut report)?
                else {
                    skipped += 1;
                    continue;
                };
                if insert_row(&tx, table.name, &columns, row, options.merge)? {
                    inserted += 1;
                } else {
                    skipped += 1;
                }
            }
            report.inserted.insert(table.name.to_string(), inserted);
            if skipped > 0 {
                report.skipped.insert(table.name.to_string(), skipped);
            }
        }
        if options.dry_run {
            tx.rollback()?;
        } else {
            tx.commit()?;
        }
        Ok(report)
    }
}

/// The scannable secrets: trimmed, and long enough to be a credential.
fn scanned_secrets(secrets: &[SecretString]) -> Vec<&str> {
    secrets
        .iter()
        .map(SecretString::expose)
        .map(str::trim)
        .filter(|secret| secret.len() >= MIN_SCANNED_SECRET_BYTES)
        .collect()
}

/// Refuse a bundle (or its export summary) in which any string or map key,
/// anywhere, holds one of `secrets`.
fn refuse_secrets(bundle: &PortableBundle, secrets: &[SecretString]) -> Result<()> {
    let secrets = scanned_secrets(secrets);
    if secrets.is_empty() {
        return Ok(());
    }
    // Carried tables in import order (an Issue before its events), then
    // templates: the most precise location for the common case.
    let carried = CARRIED_TABLES
        .iter()
        .filter_map(|table| bundle.tables.get_key_value(table.name))
        .map(|(table, rows)| ("", table, rows));
    let templates = bundle
        .manager_templates
        .iter()
        .map(|(table, rows)| ("manager template ", table, rows));
    for (label, table, rows) in carried.chain(templates) {
        for (index, row) in rows.iter().enumerate() {
            for (column, value) in row {
                if holds_secret(value, &secrets) {
                    return Err(DaemonError::PolicyDenied(format!(
                        "portable_export_secret_found: {label}{table} row {} column {column} holds a credential; remove it before exporting",
                        index + 1
                    )));
                }
            }
        }
    }
    // Everything else, by walking the whole serialized document and the
    // summary the RPC returns: metadata, withheld and left-behind labels and
    // every map key, so a new field is covered without touching this list.
    for (what, document) in [
        ("bundle", serde_json::to_value(bundle)),
        ("summary", serde_json::to_value(bundle.summary())),
    ] {
        let document = document.map_err(|error| {
            DaemonError::InvalidParam(format!("portable_bundle_unserializable: {error}"))
        })?;
        if let Some(path) = secret_path(&document, &secrets, what) {
            return Err(DaemonError::PolicyDenied(format!(
                "portable_export_secret_found: {path} holds a credential; remove it before exporting"
            )));
        }
    }
    Ok(())
}

/// The path of the first string or key in `value` that holds a secret. A key
/// that holds one is shown as `<key>`, never as itself.
fn secret_path(value: &Value, secrets: &[&str], path: &str) -> Option<String> {
    let hit = |text: &str| secrets.iter().any(|secret| text.contains(secret));
    match value {
        Value::String(text) => hit(text).then(|| path.to_string()),
        Value::Array(items) => items
            .iter()
            .enumerate()
            .find_map(|(index, item)| secret_path(item, secrets, &format!("{path}[{index}]"))),
        Value::Object(object) => object.iter().find_map(|(key, item)| {
            if hit(key) {
                return Some(format!("{path}.<key>"));
            }
            secret_path(item, secrets, &format!("{path}.{key}"))
        }),
        _ => None,
    }
}

fn row_str<'a>(row: &'a Map<String, Value>, column: &str) -> Option<&'a str> {
    row.get(column).and_then(Value::as_str)
}

fn exists(tx: &Transaction<'_>, sql: &str, params: &[&str]) -> Result<bool> {
    Ok(
        tx.query_row(sql, rusqlite::params_from_iter(params), |row| {
            row.get::<_, bool>(0)
        })?,
    )
}

/// Per-table import transforms. `None` skips the row (merge: a parent was
/// skipped, or an identical rule exists).
fn prepare_import_row<'r>(
    tx: &Transaction<'_>,
    table: &str,
    row: &'r mut Map<String, Value>,
    options: &PortableImportOptions,
    report: &mut PortableImportReport,
) -> Result<Option<&'r mut Map<String, Value>>> {
    match table {
        "projects" => {
            let original = row_str(row, "path").map(str::to_string);
            let remapped = original
                .as_deref()
                .and_then(|path| remap_path(path, &options.path_remaps));
            if let Some(path) = &remapped {
                row.insert("path".to_string(), Value::String(path.clone()));
            }
            report.projects.push(ImportedProject {
                id: row_str(row, "id").unwrap_or_default().to_string(),
                name: row_str(row, "name").unwrap_or_default().to_string(),
                path: remapped.clone().or_else(|| original.clone()),
                remapped_from: remapped.and(original),
            });
        }
        "permission_rules" => {
            // A fresh autoincrement id; skip an identical existing rule.
            row.remove("id");
            let duplicate = exists(
                tx,
                "SELECT EXISTS(SELECT 1 FROM permission_rules WHERE tool_pattern=?1
                   AND action=?2 AND scope=?3 AND scope_id IS ?4)",
                &[
                    row_str(row, "tool_pattern").unwrap_or_default(),
                    row_str(row, "action").unwrap_or_default(),
                    row_str(row, "scope").unwrap_or_default(),
                    row_str(row, "scope_id").unwrap_or("\u{0}"),
                ],
            )?;
            // `scope_id IS ?4` with a NUL sentinel never matches a NULL id, so
            // compare NULL scope ids separately.
            let duplicate = duplicate
                || (row_str(row, "scope_id").is_none()
                    && exists(
                        tx,
                        "SELECT EXISTS(SELECT 1 FROM permission_rules WHERE tool_pattern=?1
                           AND action=?2 AND scope=?3 AND scope_id IS NULL)",
                        &[
                            row_str(row, "tool_pattern").unwrap_or_default(),
                            row_str(row, "action").unwrap_or_default(),
                            row_str(row, "scope").unwrap_or_default(),
                        ],
                    )?);
            if duplicate {
                return Ok(None);
            }
        }
        "issues" => {
            // Ideas (and the capture pipeline behind them) stay behind, so
            // the Idea link of an Issue does too.
            for column in ["idea_id", "source_event_id", "source_finding_ref"] {
                if row.contains_key(column) {
                    row.insert(column.to_string(), Value::Null);
                }
            }
            if options.merge
                && !exists(
                    tx,
                    "SELECT EXISTS(SELECT 1 FROM projects WHERE id=?1)",
                    &[row_str(row, "project_id").unwrap_or_default()],
                )?
            {
                return Ok(None);
            }
        }
        "issue_events" if options.merge => {
            if !exists(
                tx,
                "SELECT EXISTS(SELECT 1 FROM issues WHERE id=?1 AND project_id=?2)",
                &[
                    row_str(row, "issue_id").unwrap_or_default(),
                    row_str(row, "project_id").unwrap_or_default(),
                ],
            )? {
                return Ok(None);
            }
        }
        "issue_deps" if options.merge => {
            let project = row_str(row, "project_id").unwrap_or_default();
            for column in ["issue_id", "depends_on_id"] {
                if !exists(
                    tx,
                    "SELECT EXISTS(SELECT 1 FROM issues WHERE id=?1 AND project_id=?2)",
                    &[row_str(row, column).unwrap_or_default(), project],
                )? {
                    return Ok(None);
                }
            }
        }
        _ => {}
    }
    Ok(Some(row))
}

/// Insert one row. Settings replace a fresh database's seeded defaults; a
/// merge keeps every existing row. Returns whether a row was written.
fn insert_row(
    tx: &Transaction<'_>,
    table: &str,
    columns: &HashSet<String>,
    row: &Map<String, Value>,
    merge: bool,
) -> Result<bool> {
    let mut names = Vec::with_capacity(row.len());
    let mut values = Vec::with_capacity(row.len());
    for (column, value) in row {
        if !columns.contains(column) {
            return Err(DaemonError::InvalidParam(format!(
                "portable_bundle_column_unknown: this schema's `{table}` has no column `{column}`"
            )));
        }
        names.push(format!("\"{column}\""));
        values.push(json_to_sql(table, column, value)?);
    }
    let verb = match (merge, table) {
        (true, _) => "INSERT OR IGNORE",
        (false, "daemon_settings") => "INSERT OR REPLACE",
        (false, _) => "INSERT",
    };
    let placeholders = (1..=names.len())
        .map(|index| format!("?{index}"))
        .collect::<Vec<_>>()
        .join(",");
    let sql = format!(
        // sql-dynamic-ok: table from CARRIED_TABLES; columns checked against pragma_table_info.
        "{verb} INTO \"{table}\" ({}) VALUES ({placeholders})",
        names.join(",")
    );
    let changed = tx.execute(&sql, rusqlite::params_from_iter(values))?;
    Ok(changed > 0)
}

#[cfg(test)]
#[path = "portable_bundle_tests.rs"]
mod tests;
