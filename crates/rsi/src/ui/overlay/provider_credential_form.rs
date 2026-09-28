//! Provider-credential set/rotate form rendering (Settings -> Provider Keys,
//! #694 K1b).
//!
//! Mirrors `budget_policy_form::render_budget_policy_form`'s layout
//! (bordered popup, one row per field, hint line at the bottom) reduced to
//! a single field. The secret is rendered FULLY masked — every character as
//! `*`, never a partial reveal like `provider_form.rs`'s API-key field —
//! because this form's whole purpose is a key vault credential, never
//! echoed per the #694 K design note.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Clear, Padding, Paragraph};

use crate::ui::theme;

use super::fixed_centered_rect;

/// Mask every character of `secret` with `*` (full mask — no partial
/// reveal). Char-counted, not byte-counted, so multi-byte input doesn't
/// leak length information through a mismatched mask width in a
/// misleading way (still leaks length, which is an accepted tradeoff of
/// any fixed-width mask; see `provider_form.rs`'s `mask_text` for the same
/// tradeoff elsewhere in this crate).
fn mask_secret(secret: &str) -> String {
    "*".repeat(secret.chars().count())
}

/// Render the provider-credential set/rotate form overlay.
pub(super) fn render_provider_credential_form(
    frame: &mut Frame,
    area: Rect,
    slot: rsi_common::provider_credentials::ProviderCredentialSlot,
    rotate: bool,
    secret: &str,
) {
    let popup_height: u16 = 7;
    let popup_area = fixed_centered_rect(area, 60, popup_height);
    frame.render_widget(Clear, popup_area);

    let title = if rotate {
        format!(" Rotate {slot} Credential ")
    } else {
        format!(" Set {slot} Credential ")
    };

    let block = theme::overlay_block()
        .title(Line::from(vec![Span::styled(
            title,
            Style::default()
                .fg(theme::overlay_title())
                .add_modifier(Modifier::BOLD),
        )]))
        .padding(Padding::horizontal(1));
    let inner = block.inner(popup_area);
    frame.render_widget(block, popup_area);

    if inner.height < 3 {
        return;
    }

    let masked = mask_secret(secret);
    let row = Line::from(vec![
        Span::styled(
            "Secret: ",
            Style::default()
                .fg(theme::mauve())
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(masked, Style::default().fg(theme::text())),
        Span::styled("█", Style::default().fg(theme::text())),
    ]);
    let row_area = Rect::new(inner.x, inner.y, inner.width, 1);
    frame.render_widget(Paragraph::new(row), row_area);

    let hint_y = inner.y + inner.height - 1;
    let hint_area = Rect::new(inner.x, hint_y, inner.width, 1);
    let hint = Line::from(vec![Span::styled(
        "Enter: save  Esc: cancel  (never echoed)",
        Style::default().fg(theme::overlay_hint()),
    )]);
    frame.render_widget(Paragraph::new(hint), hint_area);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mask_secret_is_all_asterisks_and_never_contains_the_plaintext() {
        let secret = "sk-openrouter-abc123XYZ";
        let masked = mask_secret(secret);
        assert_eq!(masked.len(), secret.chars().count());
        assert!(masked.chars().all(|c| c == '*'));
        assert!(!masked.contains("sk-openrouter"));
        assert!(!masked.contains("abc123XYZ"));
    }

    #[test]
    fn mask_secret_empty_input_is_empty_mask() {
        assert_eq!(mask_secret(""), "");
    }
}
