//! Harness model discovery: the models a Harness launch can use, given the
//! credentials the key vault can resolve.

/// Return a static model list for the Harness provider.
/// A backend is listed when the key vault can resolve its credential (vault
/// entry or env-compat variable), matching what a Harness launch will use.
pub fn harness_models() -> Vec<(String, String)> {
    harness_models_with(&crate::vault::global())
}

fn harness_models_with(vault: &crate::vault::VaultHandle) -> Vec<(String, String)> {
    use crate::vault::Slot;
    let mut models = Vec::new();

    if vault.resolvable(Slot::Inception) {
        models.push(("mercury-2".into(), "Mercury 2 (Inception Direct)".into()));
        models.push((
            "mercury-2.5".into(),
            "Mercury 2.5 (Inception Direct)".into(),
        ));
    }

    if vault.resolvable(Slot::Anthropic) {
        models.extend(
            rsi_common::claude_catalog::CLAUDE_MODEL_MENU
                .iter()
                .map(|(id, name)| ((*id).to_string(), format!("Claude {name} (Direct)"))),
        );
    }

    if vault.resolvable(Slot::Openai) {
        models.extend([
            ("gpt-4o".into(), "GPT-4o (Direct)".into()),
            ("o4-mini".into(), "o4-mini (Direct)".into()),
        ]);
    }

    // Always available: local Ollama fallback
    models.push(("local-model".into(), "Local Model (Ollama)".into()));

    models
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use std::sync::Arc;

    /// #2253: an Inception key lists both Mercury generations.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn harness_models_list_both_mercury_models_for_an_inception_key() {
        let root = tempfile::tempdir().unwrap();
        let vault =
            crate::vault::VaultHandleBuilder::new(Arc::new(crate::vault::VaultSettings::default()))
                .dir(root.path().join("vault"))
                .env(|_| None)
                .open()
                .unwrap();
        // Without a key only the local fallback is offered.
        let ids = |models: &[(String, String)]| -> Vec<String> {
            models.iter().map(|(id, _)| id.clone()).collect()
        };
        assert_eq!(ids(&harness_models_with(&vault)), ["local-model"]);
        vault
            .set(crate::vault::Slot::Inception, "inception-fake-key-for-menu")
            .unwrap();
        let models = harness_models_with(&vault);
        assert_eq!(ids(&models), ["mercury-2", "mercury-2.5", "local-model"]);
        assert_eq!(models[0].1, "Mercury 2 (Inception Direct)");
        assert_eq!(models[1].1, "Mercury 2.5 (Inception Direct)");
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn harness_models_list_catalog_claude_models_for_a_vault_key() {
        let root = tempfile::tempdir().unwrap();
        let vault =
            crate::vault::VaultHandleBuilder::new(Arc::new(crate::vault::VaultSettings::default()))
                .dir(root.path().join("vault"))
                .env(|_| None)
                .open()
                .unwrap();
        vault
            .set(
                crate::vault::Slot::Anthropic,
                "sk-ant-api03-test-harness-models",
            )
            .unwrap();
        let models = harness_models_with(&vault);
        let ids: Vec<&str> = models.iter().map(|(id, _)| id.as_str()).collect();
        for (id, _) in rsi_common::claude_catalog::CLAUDE_MODEL_MENU {
            assert!(ids.contains(id), "{id} missing from {ids:?}");
        }
        assert!(
            models.iter().any(
                |(id, name)| id == "claude-sonnet-5" && name == "Claude Sonnet 5 (1M) (Direct)"
            )
        );
        assert_eq!(ids.last(), Some(&"local-model"));
    }
}
