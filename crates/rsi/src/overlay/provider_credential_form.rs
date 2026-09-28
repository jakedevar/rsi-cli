//! Provider-credential set/rotate form overlay (Settings -> Provider Keys,
//! #694 K1b).
//!
//! Mirrors `budget_policy_form.rs`'s shape (plain `String` field, push/pop,
//! Enter submits, Esc cancels) reduced to a single field. Submit does NOT
//! write locally — it enqueues `LcAction::SetProviderCredentialSecret` /
//! `RotateProviderCredentialSecret` for the daemon RPC round-trip; see
//! `action_handler::provider_credentials`.
//!
//! Secret handling (#694 K1b slice contract): the secret is masked in the
//! renderer (every character renders as `*` — never a partial reveal, see
//! `ui::overlay::provider_credential_form`) and is NEVER echoed anywhere
//! else. On submit, the buffer is taken (`std::mem::take`) and moved
//! directly into the `LcAction` — one ownership transfer, no retained
//! clone — and the overlay itself is dropped in the same call, so no
//! secret-bearing `String` is left sitting in `App` state. On Esc-cancel,
//! where the typed content is discarded rather than sent, the buffer is
//! explicitly scrubbed first: `zeroize` is not a dependency of this crate
//! (only `rsid` has it), so `scrub_secret` overwrites the buffer's existing
//! allocation with `'\0'` bytes in place (reusing capacity, not
//! reallocating) before dropping it to length 0 — "overwrite and drop" per
//! the slice contract.

use crossterm::event::{KeyCode, KeyEvent};

use crate::app::App;
use crate::modalkit_types::LcAction;
use crate::types::OverlayState;
use rsi_common::provider_credentials::ProviderCredentialSlot;

/// Open the form. `rotate = true` builds `RotateProviderCredentialSecret` on
/// submit; `false` builds `SetProviderCredentialSecret`.
pub fn open_provider_credential_form(app: &mut App, slot: ProviderCredentialSlot, rotate: bool) {
    app.overlay = OverlayState::ProviderCredentialForm {
        slot,
        rotate,
        secret: String::new(),
    };
}

/// Overwrite `secret`'s existing buffer with `'\0'` bytes in place, then
/// drop it to length 0. See module doc for why this (rather than
/// `zeroize`) is the K1b secret-scrub contract in this crate.
pub(crate) fn scrub_secret(secret: &mut String) {
    let len = secret.len();
    secret.clear();
    secret.extend(std::iter::repeat_n('\0', len));
    secret.clear();
}

/// Handle key events inside the `ProviderCredentialForm` overlay.
pub(super) fn handle_provider_credential_form_key(app: &mut App, key: KeyEvent) {
    match key.code {
        KeyCode::Enter => submit_provider_credential_form(app),
        KeyCode::Esc => {
            if let OverlayState::ProviderCredentialForm { secret, .. } = &mut app.overlay {
                scrub_secret(secret);
            }
            app.overlay = OverlayState::None;
        }
        KeyCode::Char(c) => {
            if let OverlayState::ProviderCredentialForm { secret, .. } = &mut app.overlay {
                secret.push(c);
            }
        }
        KeyCode::Backspace => {
            if let OverlayState::ProviderCredentialForm { secret, .. } = &mut app.overlay {
                secret.pop();
            }
        }
        _ => {}
    }
}

fn submit_provider_credential_form(app: &mut App) {
    let (slot, rotate, secret) = match &mut app.overlay {
        OverlayState::ProviderCredentialForm {
            slot,
            rotate,
            secret,
        } => (*slot, *rotate, std::mem::take(secret)),
        _ => return,
    };

    if secret.trim().is_empty() {
        app.notify_error("Credential cannot be empty");
        return;
    }

    app.pending_lc_actions.push(if rotate {
        LcAction::RotateProviderCredentialSecret { slot, secret }
    } else {
        LcAction::SetProviderCredentialSecret { slot, secret }
    });
    app.overlay = OverlayState::None;
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crossterm::event::KeyModifiers;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn type_str(app: &mut App, s: &str) {
        for c in s.chars() {
            handle_provider_credential_form_key(app, key(KeyCode::Char(c)));
        }
    }

    #[test]
    fn scrub_secret_empties_buffer() {
        let mut secret = "sk-super-secret-value".to_string();
        scrub_secret(&mut secret);
        assert_eq!(secret, "");
        assert_eq!(secret.len(), 0);
    }

    #[test]
    fn submit_enqueues_set_action_and_clears_overlay_buffer() {
        let mut app = crate::app::app_test_helpers::with_session_list(0);
        open_provider_credential_form(&mut app, ProviderCredentialSlot::Openrouter, false);
        type_str(&mut app, "sk-test-canary-0002");
        handle_provider_credential_form_key(&mut app, key(KeyCode::Enter));

        // The overlay is gone entirely — no secret-bearing struct remains
        // anywhere in `App` state after submit.
        assert!(matches!(app.overlay, OverlayState::None));

        let action = app.pending_lc_actions.last().expect("action queued");
        match action {
            LcAction::SetProviderCredentialSecret { slot, secret } => {
                assert_eq!(*slot, ProviderCredentialSlot::Openrouter);
                assert_eq!(secret, "sk-test-canary-0002");
            }
            other => panic!("expected SetProviderCredentialSecret, got {other:?}"),
        }
    }

    #[test]
    fn submit_enqueues_rotate_action_when_opened_in_rotate_mode() {
        let mut app = crate::app::app_test_helpers::with_session_list(0);
        open_provider_credential_form(&mut app, ProviderCredentialSlot::Anthropic, true);
        type_str(&mut app, "sk-ant-rotate-0002");
        handle_provider_credential_form_key(&mut app, key(KeyCode::Enter));

        let action = app.pending_lc_actions.last().expect("action queued");
        assert!(matches!(
            action,
            LcAction::RotateProviderCredentialSecret { slot, secret }
                if *slot == ProviderCredentialSlot::Anthropic && secret == "sk-ant-rotate-0002"
        ));
    }

    #[test]
    fn esc_scrubs_buffer_and_does_not_enqueue_any_action() {
        let mut app = crate::app::app_test_helpers::with_session_list(0);
        open_provider_credential_form(&mut app, ProviderCredentialSlot::Openrouter, false);
        type_str(&mut app, "sk-should-never-be-sent");
        let before = app.pending_lc_actions.len();

        handle_provider_credential_form_key(&mut app, key(KeyCode::Esc));

        assert!(matches!(app.overlay, OverlayState::None));
        assert_eq!(app.pending_lc_actions.len(), before);
    }

    #[test]
    fn empty_secret_is_rejected_without_enqueueing_an_action() {
        let mut app = crate::app::app_test_helpers::with_session_list(0);
        open_provider_credential_form(&mut app, ProviderCredentialSlot::Openrouter, false);
        let before = app.pending_lc_actions.len();

        handle_provider_credential_form_key(&mut app, key(KeyCode::Enter));

        assert_eq!(app.pending_lc_actions.len(), before);
        assert!(matches!(
            app.overlay,
            OverlayState::ProviderCredentialForm { .. }
        ));
    }
}
