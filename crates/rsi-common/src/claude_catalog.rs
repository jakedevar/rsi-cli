//! Canonical Claude model catalog — the single source of truth for which
//! Claude models RSI offers, what they are called, and how large their context
//! window is.
//!
//! Before this module the same list existed twice: `CLAUDE_MODEL_CATALOG` in
//! `crates/rsid/src/claude.rs` (5 entries, ids only) and `CLAUDE_MODELS` in
//! `crates/rsi/src/app/mod.rs` (6 entries, ids + display names), with the
//! context window kept in a third place — the ordered substring table in
//! `crates/rsid/src/monitor.rs`. The three drifted: the daemon catalog was
//! missing `claude-opus-4-5`, and `claude-fable-5` matched no substring rule
//! and so resolved to the bare 128k fallback while the picker advertised it as
//! "Fable 5 (1M)". Both crates now consume this module, and the window travels
//! with the model instead of living in a parallel table.
//!
//! Maintenance: add a model here once. `CLAUDE_MODEL_CATALOG` (rich) and
//! `CLAUDE_MODEL_MENU` (the `(id, display_name)` projection the pickers want)
//! are generated from one list by [`claude_catalog!`], so a new entry cannot
//! reach one consumer and miss the other. Retired models the CLI no longer
//! accepts should be pruned rather than left to rot in the menu.

/// One selectable Claude model: its CLI id, the label shown in the picker, and
/// its context window in tokens.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ClaudeModelSpec {
    /// The exact id passed to `claude --model`.
    pub id: &'static str,
    /// Human-readable label shown in the model picker.
    pub display_name: &'static str,
    /// Context window in tokens, used as the pre-`result`-event fallback for
    /// context fill % and rotation decisions.
    pub context_window: u64,
}

/// Generates the rich catalog and its `(id, name)` projection from one list so
/// the two can never disagree.
macro_rules! claude_catalog {
    ($(($id:literal, $display_name:literal, $context_window:literal)),+ $(,)?) => {
        /// Every Claude model RSI offers, newest first within a family, in
        /// picker order.
        pub const CLAUDE_MODEL_CATALOG: &[ClaudeModelSpec] = &[
            $(ClaudeModelSpec {
                id: $id,
                display_name: $display_name,
                context_window: $context_window,
            }),+
        ];

        /// `(id, display_name)` projection of [`CLAUDE_MODEL_CATALOG`], in the
        /// same order. This is the shape both model pickers consume.
        pub const CLAUDE_MODEL_MENU: &[(&str, &str)] = &[$(($id, $display_name)),+];
    };
}

claude_catalog![
    ("claude-fable-5-1", "Fable 5.1 (1M)", 1_000_000),
    ("claude-opus-5-5", "Opus 5.5 (1M)", 1_000_000),
    ("claude-opus-4-8", "Opus 4.8 (1M)", 1_000_000),
    // Opus 4.7/4.6 are intentionally omitted: same price as 4.8
    // ($5/$25 per 1M tokens) with less capability, so there is no reason to
    // offer them in the picker. Opus 4.5 stays for now — unconfirmed pricing.
    ("claude-opus-4-5", "Opus 4.5 (1M)", 1_000_000),
    ("claude-sonnet-5", "Sonnet 5 (1M)", 1_000_000),
    ("claude-haiku-4-5-20251001", "Haiku 4.5", 200_000),
];

/// Look up a catalogued Claude model by its exact CLI id.
///
/// Exact match only: variant ids the catalog does not carry (e.g. a `-200k`
/// SKU) deliberately fall through to the caller's own resolution.
pub fn claude_model_spec(model_id: &str) -> Option<&'static ClaudeModelSpec> {
    CLAUDE_MODEL_CATALOG.iter().find(|spec| spec.id == model_id)
}

/// Context window for a catalogued Claude model, or `None` when the id is not
/// one RSI offers.
pub fn claude_catalog_context_window(model_id: &str) -> Option<u64> {
    claude_model_spec(model_id).map(|spec| spec.context_window)
}

/// Strips a single trailing bracketed context-variant tag from a model id.
///
/// The Claude CLI reports the *variant-suffixed* form of the model back to the
/// host even when it was launched with the bare catalog id: a `system/init`
/// handshake for a plain `claude-opus-5` session announces
/// `"model":"claude-opus-5[1m]"` (V-020, `[observed]` against `claude 2.1.259`).
/// That suffix is not a distinct model — it addresses the 1M-context variant of
/// the same model — but it defeats every id-keyed lookup RSI performs, because
/// the catalog matches exactly and the ordered substring table carries no
/// `opus-5` pattern. The suffixed id therefore fell through to the bare 128k
/// default for a session whose real window is `1_000_000`: a 7.8x overstatement
/// of context fill, on the default model path, from the first event.
///
/// The match is structural (a trailing `[` … `]`), not a literal `[1m]` test,
/// so a future `[200k]` or any other variant tag normalizes the same way
/// without another edit here.
///
/// This deliberately answers only the *window* question. The variant tag is
/// still what the CLI calls the model, so the stored `session.model` keeps it;
/// normalizing the persisted identity is entangled with effort-ladder and
/// abbreviation logic and is tracked separately as P2-MODELID.
pub fn strip_context_variant_suffix(model_id: &str) -> &str {
    let Some(stripped) = model_id.strip_suffix(']') else {
        return model_id;
    };
    let Some(open) = stripped.rfind('[') else {
        return model_id;
    };
    let base = &stripped[..open];
    // `[1m]` with nothing in front of it is not a variant of anything; leaving
    // it intact keeps the caller's miss behaviour unchanged.
    if base.is_empty() {
        return model_id;
    }
    base
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn catalog_and_menu_are_the_same_list() {
        assert_eq!(CLAUDE_MODEL_CATALOG.len(), CLAUDE_MODEL_MENU.len());
        for (spec, (id, name)) in CLAUDE_MODEL_CATALOG.iter().zip(CLAUDE_MODEL_MENU) {
            assert_eq!(spec.id, *id);
            assert_eq!(spec.display_name, *name);
        }
    }

    #[test]
    fn catalog_ids_are_unique_and_windows_are_populated() {
        for (index, spec) in CLAUDE_MODEL_CATALOG.iter().enumerate() {
            assert!(
                CLAUDE_MODEL_CATALOG[..index]
                    .iter()
                    .all(|earlier| earlier.id != spec.id),
                "duplicate catalog id {}",
                spec.id
            );
            assert!(spec.context_window > 0, "{} has no context window", spec.id);
            assert!(
                !spec.display_name.is_empty(),
                "{} has no display name",
                spec.id
            );
        }
    }

    #[test]
    fn fable_five_one_is_a_one_million_token_model() {
        // V-001/F-130: the picker advertised "Fable 5 (1M)" while the daemon
        // computed context fill against 128k. The window now travels with the
        // model, so the two agree by construction.
        assert_eq!(
            claude_catalog_context_window("claude-fable-5-1"),
            Some(1_000_000)
        );
        assert_eq!(
            claude_model_spec("claude-fable-5-1").map(|spec| spec.display_name),
            Some("Fable 5.1 (1M)")
        );
    }

    #[test]
    fn catalogued_windows_are_looked_up_by_exact_id() {
        assert_eq!(
            claude_catalog_context_window("claude-opus-5-5"),
            Some(1_000_000)
        );
        assert_eq!(
            claude_catalog_context_window("claude-haiku-4-5-20251001"),
            Some(200_000)
        );
        // Not catalogued: the caller falls back to its own resolution.
        assert_eq!(claude_catalog_context_window("claude-opus-4-7-200k"), None);
        assert_eq!(claude_catalog_context_window("gpt-6-astra"), None);
    }

    #[test]
    fn variant_suffix_strips_to_the_catalogued_id() {
        // V-020: `system/init` announces the variant-suffixed id for a session
        // launched with the bare catalog id (originally observed as
        // `claude-opus-5[1m]` for `claude-opus-5`; the same structural
        // announcement now applies to the current default, `claude-opus-5-5`).
        assert_eq!(
            strip_context_variant_suffix("claude-opus-5-5[1m]"),
            "claude-opus-5-5"
        );
        assert_eq!(
            claude_catalog_context_window(strip_context_variant_suffix("claude-opus-5-5[1m]")),
            Some(1_000_000)
        );
    }

    #[test]
    fn variant_suffix_match_is_structural_not_a_literal_1m_test() {
        // Any bracketed trailing tag normalizes, so a future variant needs no
        // edit here.
        assert_eq!(
            strip_context_variant_suffix("claude-sonnet-5[200k]"),
            "claude-sonnet-5"
        );
        assert_eq!(
            strip_context_variant_suffix("claude-fable-5-1[whatever]"),
            "claude-fable-5-1"
        );
    }

    #[test]
    fn ids_without_a_trailing_bracketed_tag_are_returned_verbatim() {
        for id in [
            "claude-opus-5",
            "claude-haiku-4-5-20251001",
            "gpt-6-astra",
            "qwen3.6:27b",
            // Bracketed but not trailing: not a variant tag.
            "weird[1m]model",
            // Unbalanced: no opening bracket to pair with.
            "claude-opus-5]",
            // A bare tag is not a variant of anything.
            "[1m]",
        ] {
            assert_eq!(strip_context_variant_suffix(id), id, "id {id} was altered");
        }
    }
}
