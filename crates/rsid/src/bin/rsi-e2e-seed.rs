//! Test-only seeder for an ISOLATED rsid state directory (local #122).
//!
//! The desktop end-to-end harness needs a finished leaf session with a conversation in a throwaway
//! daemon, without launching a provider. This tool writes it through the real `Store` API
//! (`insert_session`, `insert_event`) into the isolated `rsi.db` while its daemon is stopped, so every
//! schema trigger and SQL function is the store's own and nothing is stubbed.
//!
//! It cannot reach production:
//! - It is a separate binary behind the `e2e-seed` cargo feature (`required-features`); no default or
//!   release build compiles it, and `crates/rsid/build.rs` refuses the feature in the release profile
//!   (`scripts/check-release-seam.sh` refuses it in the release dependency graph before `make release`).
//! - It is not an RPC, agent verb or daemon flag: the daemon has no code path that imports a fixture, so
//!   neither an agent nor an operator can fabricate history through the socket.
//! - Even when built, it writes only to a state directory that carries the harness's marker file, lies
//!   under the system temp directory, and is not the operator's `~/.rsi` itself; and the database it opens
//!   is a plain file in that directory: an `rsi.db` or `-wal`/`-shm`/`-journal` sibling that is a symlink,
//!   not a regular file, hard-linked elsewhere, outside the directory or the operator's own `rsi.db` is
//!   refused before `Store::open` (hub #3245).
//!
//! Usage: `rsi-e2e-seed --state-dir <dir> --spec <spec.json>`; the database is `<dir>/rsi.db`.

use std::path::{Path, PathBuf};

use rsi_common::types::{ConversationEvent, Session};
use rsid::store::Store;
use serde::Deserialize;
use serde_json::{Value, json};
use uuid::Uuid;

/// The file the harness writes into the state directory it created, so the seeder can tell it from any
/// other directory (the operator's included).
const MARKER_FILE: &str = ".rsi-e2e-isolated-state";
const MARKER_TEXT: &str = "rsi-e2e-isolated-state v1";

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Spec {
    session: SessionSpec,
    events: Vec<EventSpec>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct SessionSpec {
    id: Uuid,
    query: String,
    title: Option<String>,
    working_dir: PathBuf,
    project_id: Option<Uuid>,
    parent_id: Option<Uuid>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct EventSpec {
    event_type: String,
    role: Option<String>,
    #[serde(default)]
    content: String,
    tool_name: Option<String>,
    tool_input: Option<Value>,
    tool_use_id: Option<String>,
}

/// Refuse any directory that is not a harness-created isolated state directory.
fn check_state_dir(dir: &Path) -> Result<PathBuf, String> {
    let canonical = dir
        .canonicalize()
        .map_err(|error| format!("state dir {}: {error}", dir.display()))?;
    let temp = std::env::temp_dir()
        .canonicalize()
        .unwrap_or_else(|_| PathBuf::from("/tmp"));
    let under_temp = canonical.starts_with(&temp)
        || canonical.starts_with("/tmp")
        || canonical.starts_with("/private/tmp");
    if !under_temp {
        return Err(format!(
            "refused: {} is not under the system temp directory",
            canonical.display()
        ));
    }
    if let Some(operator) = dirs::home_dir().map(|home| home.join(".rsi")) {
        let operator = operator.canonicalize().unwrap_or(operator);
        if canonical == operator {
            return Err("refused: that is the operator's ~/.rsi state".to_string());
        }
    }
    let marker = std::fs::read_to_string(canonical.join(MARKER_FILE)).unwrap_or_default();
    if marker.trim() != MARKER_TEXT {
        return Err(format!(
            "refused: {} has no {MARKER_FILE} marker; only a harness-created isolated state directory is seeded",
            canonical.display()
        ));
    }
    Ok(canonical)
}

/// The database file the seeder opens, and the SQLite sidecars `Store::open` would follow with it.
const DB_FILE: &str = "rsi.db";
const DB_SIDECARS: [&str; 3] = ["-wal", "-shm", "-journal"];

/// Refuse a database in `dir` (the canonical state directory) that is not a plain file of its own: a
/// symlinked or non-regular `rsi.db` or sidecar would make `Store::open` write somewhere else (hub #3245).
fn check_db_files(dir: &Path) -> Result<PathBuf, String> {
    let db = dir.join(DB_FILE);
    let names = std::iter::once(DB_FILE.to_string()).chain(
        DB_SIDECARS
            .iter()
            .map(|suffix| format!("{DB_FILE}{suffix}")),
    );
    for name in names {
        let path = dir.join(&name);
        let meta = match std::fs::symlink_metadata(&path) {
            Ok(meta) => meta,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(format!("refused: {}: {error}", path.display())),
        };
        if meta.file_type().is_symlink() {
            return Err(format!("refused: {} is a symlink", path.display()));
        }
        if !meta.is_file() {
            return Err(format!("refused: {} is not a regular file", path.display()));
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            if meta.nlink() > 1 {
                return Err(format!("refused: {} has another hard link", path.display()));
            }
        }
    }
    if db.exists() {
        let canonical = db
            .canonicalize()
            .map_err(|error| format!("refused: {}: {error}", db.display()))?;
        if canonical.parent() != Some(dir) {
            return Err(format!(
                "refused: {} resolves outside the state directory",
                db.display()
            ));
        }
        if let Some(operator) = dirs::home_dir().map(|home| home.join(".rsi").join(DB_FILE))
            && operator
                .canonicalize()
                .is_ok_and(|operator| operator == canonical)
        {
            return Err("refused: that is the operator's ~/.rsi/rsi.db".to_string());
        }
    }
    Ok(db)
}

fn build_session(spec: &SessionSpec, now: &str) -> Result<Session, String> {
    serde_json::from_value(json!({
        "id": spec.id,
        "status": "Completed",
        "session_kind": "Standard",
        "provider": "Claude",
        "created_at": now,
        "updated_at": now,
        "query": spec.query,
        "title": spec.title,
        "working_dir": spec.working_dir,
        "project_id": spec.project_id,
        "parent_id": spec.parent_id,
        "model": "default",
    }))
    .map_err(|error| format!("session spec: {error}"))
}

fn build_event(
    session_id: Uuid,
    sequence: usize,
    spec: &EventSpec,
    now: &str,
) -> Result<ConversationEvent, String> {
    serde_json::from_value(json!({
        "id": 0,
        "session_id": session_id,
        "sequence": sequence,
        "event_type": spec.event_type,
        "role": spec.role,
        "created_at": now,
        "content": spec.content,
        "tool_name": spec.tool_name,
        "tool_input": spec.tool_input,
        "tool_use_id": spec.tool_use_id,
    }))
    .map_err(|error| format!("event {sequence}: {error}"))
}

fn run(args: &[String]) -> Result<String, String> {
    let (mut state_dir, mut spec_path) = (None, None);
    let mut it = args.iter();
    while let Some(flag) = it.next() {
        let value = it
            .next()
            .ok_or_else(|| format!("missing value for {flag}"))?;
        match flag.as_str() {
            "--state-dir" => state_dir = Some(PathBuf::from(value)),
            "--spec" => spec_path = Some(PathBuf::from(value)),
            other => return Err(format!("unknown argument: {other}")),
        }
    }
    let state_dir = state_dir.ok_or("--state-dir is required")?;
    let spec_path = spec_path.ok_or("--spec is required")?;
    let dir = check_state_dir(&state_dir)?;
    let spec: Spec = serde_json::from_str(
        &std::fs::read_to_string(&spec_path)
            .map_err(|error| format!("{}: {error}", spec_path.display()))?,
    )
    .map_err(|error| format!("spec: {error}"))?;
    if spec.events.is_empty() {
        return Err("spec: a finished session needs at least one event".to_string());
    }
    let now = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Nanos, true);
    let session = build_session(&spec.session, &now)?;
    let db = check_db_files(&dir)?;
    let store = Store::open(&db).map_err(|error| format!("open store: {error}"))?;
    store
        .insert_session(&session)
        .map_err(|error| format!("insert session: {error}"))?;
    for (index, event) in spec.events.iter().enumerate() {
        let event = build_event(session.id, index + 1, event, &now)?;
        store
            .insert_event(&event)
            .map_err(|error| format!("insert event {}: {error}", index + 1))?;
    }
    Ok(format!(
        "seeded session {} with {} events",
        session.id,
        spec.events.len()
    ))
}

fn main() {
    let args = std::env::args().skip(1).collect::<Vec<_>>();
    match run(&args) {
        Ok(summary) => println!("{summary}"),
        Err(error) => {
            eprintln!("rsi-e2e-seed: {error}");
            std::process::exit(2);
        }
    }
}
