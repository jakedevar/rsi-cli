//! Harness values; provider compatibility remains owned by the daemon store.

#[allow(unused_imports)] // Preserves the harness type facade used by providers and callers.
pub use crate::store_support::compatible_table::{
    AuthStyle, MaxTokensField, ProviderQuirks, ReasoningEffortWire,
};
pub use rsid_harness::types::*;

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-03"))]
    #[test]
    fn test_provider_quirks_default() {
        let q = ProviderQuirks::default();
        assert!(!q.merge_system_into_user);
        assert!(!q.disable_streaming);
        assert!(q.max_tokens_non_streaming.is_none());
        assert!(!q.strip_think_tags);
        assert!(!q.native_tools);
        assert!(matches!(q.auth_style, AuthStyle::Bearer));
    }
}
