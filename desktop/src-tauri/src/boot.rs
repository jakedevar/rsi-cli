//! Boot evidence: the webview reports how its boot went and the app prints one
//! `rsi-desktop: boot ...` line to stderr. A window that opens is not proof the
//! app started (a failed Settings read was invisible); this line is what the
//! install proof and a bug report read. The report is validated and bounded:
//! it comes from the page, so it is never trusted as text for a log.

use serde::Deserialize;

const MAX_STEPS: usize = 16;
const MAX_ERRORS: usize = 5;
const STATES: [&str; 3] = ["ready", "offline", "failed"];
const DAEMONS: [&str; 3] = ["connected", "unreachable", "error"];

#[derive(Debug, Deserialize)]
pub struct BootStep {
    pub name: String,
    pub ok: bool,
    #[serde(default)]
    pub skipped: bool,
    #[serde(default)]
    pub error: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BootReport {
    pub state: String,
    pub daemon: String,
    pub sessions: u64,
    pub steps: Vec<BootStep>,
    #[serde(default)]
    pub page_errors: Vec<String>,
}

fn one_line(text: &str, max: usize) -> String {
    let flat: String = text
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    let flat = flat.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() > max {
        let cut: String = flat.chars().take(max.saturating_sub(1)).collect();
        format!("{cut}…")
    } else {
        flat
    }
}

fn step_name_ok(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 24
        && name.bytes().all(|b| b.is_ascii_lowercase() || b == b'-')
}

/// The log line, or why the report is refused. Same shape as boot-model.ts `bootLine`,
/// followed by one `error` clause per failed step.
pub fn boot_line(report: &BootReport) -> Result<String, String> {
    if !STATES.contains(&report.state.as_str()) {
        return Err(format!(
            "unknown boot state {:?}",
            one_line(&report.state, 24)
        ));
    }
    if !DAEMONS.contains(&report.daemon.as_str()) {
        return Err(format!(
            "unknown daemon state {:?}",
            one_line(&report.daemon, 24)
        ));
    }
    if report.steps.len() > MAX_STEPS {
        return Err("too many boot steps".into());
    }
    let mut steps = Vec::new();
    let mut errors = Vec::new();
    for s in &report.steps {
        if !step_name_ok(&s.name) {
            return Err("invalid boot step name".into());
        }
        let status = if s.skipped {
            "skipped"
        } else if s.ok {
            "ok"
        } else {
            "failed"
        };
        steps.push(format!("{}:{status}", s.name));
        if !s.ok && !s.skipped {
            let msg = s.error.as_deref().unwrap_or("failed");
            errors.push(format!("{}: {}", s.name, one_line(msg, 160)));
        }
    }
    let mut line = format!(
        "rsi-desktop: boot={} daemon={} sessions={} steps={}",
        report.state,
        report.daemon,
        report.sessions,
        steps.join(",")
    );
    if !report.page_errors.is_empty() {
        line.push_str(&format!(" page_errors={}", report.page_errors.len()));
        for e in report.page_errors.iter().take(MAX_ERRORS) {
            errors.push(format!("page: {}", one_line(e, 160)));
        }
    }
    for e in errors {
        line.push_str(&format!(" error=\"{}\"", e.replace('"', "'")));
    }
    Ok(line)
}

/// The webview's boot result, printed to stderr (the process log).
#[tauri::command]
pub fn boot_report(report: BootReport) -> Result<(), String> {
    let line = boot_line(&report)?;
    eprintln!("{line}");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn parse(v: serde_json::Value) -> BootReport {
        serde_json::from_value(v).expect("report")
    }

    #[test]
    fn a_ready_boot_prints_every_step() {
        let r = parse(json!({
            "state": "ready", "daemon": "connected", "sessions": 3,
            "steps": [{"name": "daemon", "ok": true}, {"name": "settings", "ok": true}],
            "pageErrors": [],
        }));
        assert_eq!(
            boot_line(&r).unwrap(),
            "rsi-desktop: boot=ready daemon=connected sessions=3 steps=daemon:ok,settings:ok"
        );
    }

    #[test]
    fn a_failed_step_and_page_errors_are_named_and_flattened() {
        let r = parse(json!({
            "state": "failed", "daemon": "connected", "sessions": 0,
            "steps": [
                {"name": "daemon", "ok": true},
                {"name": "settings", "ok": false, "error": "get_daemon_config\nMethod \"x\" not found"},
                {"name": "live", "ok": false, "skipped": true},
            ],
            "pageErrors": ["TypeError: x is undefined"],
        }));
        let line = boot_line(&r).unwrap();
        assert!(
            line.contains("steps=daemon:ok,settings:failed,live:skipped"),
            "{line}"
        );
        assert!(line.contains("page_errors=1"), "{line}");
        assert!(
            line.contains("error=\"settings: get_daemon_config Method 'x' not found\""),
            "{line}"
        );
        assert!(
            line.contains("error=\"page: TypeError: x is undefined\""),
            "{line}"
        );
        assert!(!line.contains('\n'));
    }

    #[test]
    fn an_offline_boot_reports_the_unreachable_daemon() {
        let r = parse(json!({
            "state": "offline", "daemon": "unreachable", "sessions": 0,
            "steps": [{"name": "daemon", "ok": false, "error": "no socket"}, {"name": "settings", "ok": false, "skipped": true}],
        }));
        let line = boot_line(&r).unwrap();
        assert!(
            line.starts_with("rsi-desktop: boot=offline daemon=unreachable sessions=0"),
            "{line}"
        );
        assert!(line.contains("daemon:failed,settings:skipped"), "{line}");
    }

    #[test]
    fn a_malformed_report_is_refused() {
        let bad_state =
            parse(json!({"state": "great", "daemon": "connected", "sessions": 0, "steps": []}));
        assert!(boot_line(&bad_state).is_err());
        let bad_daemon =
            parse(json!({"state": "ready", "daemon": "x\ny", "sessions": 0, "steps": []}));
        assert!(boot_line(&bad_daemon).is_err());
        let bad_name = parse(
            json!({"state": "ready", "daemon": "connected", "sessions": 0, "steps": [{"name": "a b", "ok": true}]}),
        );
        assert!(boot_line(&bad_name).is_err());
        let many: Vec<_> = (0..17).map(|_| json!({"name": "x", "ok": true})).collect();
        let too_many =
            parse(json!({"state": "ready", "daemon": "connected", "sessions": 0, "steps": many}));
        assert!(boot_line(&too_many).is_err());
    }
}
