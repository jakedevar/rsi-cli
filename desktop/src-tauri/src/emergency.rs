//! Emergency stop: halt all model work (hub #3008 follow-up, `<Space>X`).
//!
//! One command forwards the daemon's existing `UpdateModelControlPolicy` with
//! exactly the request the TUI sends (`Client::update_model_control_policy`
//! in `crates/rsi/src/client.rs`, called by `emergency_stop_all` in
//! `crates/rsi/src/action_handler/daemon_config.rs`): mode `stop_all`, active
//! work interrupted. There are no arguments, so the webview cannot choose any
//! other mode or turn the interrupt off. The returned report
//! (`ModelControlPolicyUpdateReport`) goes back to the webview unchanged; it
//! lists the cancelled invocations and the interrupted sessions.

use serde_json::{Value, json};
use tauri::State;

use crate::{AppState, CmdResult, forward};

/// The daemon method the TUI's emergency stop calls.
pub const STOP_METHOD: &str = "UpdateModelControlPolicy";

/// `{ "mode": "stop_all", "interrupt_active": true }`, the TUI's request.
pub fn stop_all_params() -> Value {
    json!({ "mode": "stop_all", "interrupt_active": true })
}

/// Stop all model work: deny new model work, cancel active invocations and
/// interrupt running sessions. The frontend asks first from menus, the palette
/// and buttons; the TUI chord and `:stopall` run at once.
#[tauri::command]
pub async fn emergency_stop_all(state: State<'_, AppState>) -> CmdResult {
    forward(&state, STOP_METHOD, stop_all_params()).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_request_is_the_tuis_stop_all() {
        assert_eq!(STOP_METHOD, "UpdateModelControlPolicy");
        assert_eq!(
            stop_all_params(),
            json!({ "mode": "stop_all", "interrupt_active": true })
        );
    }

    #[test]
    fn the_request_adds_no_policy_edit() {
        // A budget edit sends replace_policies; the emergency stop must not.
        let p = stop_all_params();
        assert!(p.get("replace_policies").is_none());
        assert!(p.get("policies").is_none());
        assert_eq!(p.as_object().unwrap().len(), 2);
    }
}
