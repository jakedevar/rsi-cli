//! Access control for inbound messages.
//!
//! Filters messages based on DM policy and allowlist. Handle normalization
//! preserves the iMessage parity (including the email branch, even though
//! Signal is phone-only) so test coverage transfers cleanly.

use crate::config::{DmPolicy, SignalConfig};

/// Check if a sender is allowed to interact with the bridge.
pub fn is_allowed(sender: &str, config: &SignalConfig) -> bool {
    match config.dm_policy {
        DmPolicy::Open => true,
        DmPolicy::Disabled => false,
        DmPolicy::Allowlist => {
            let normalized = normalize_handle(sender);
            config
                .allow_from
                .iter()
                .any(|allowed| normalize_handle(allowed) == normalized)
        }
    }
}

/// Normalize a handle for comparison.
/// - Lowercase for emails (no-op on Signal in practice, retained for parity).
/// - Strip whitespace.
/// - Normalize phone numbers to `+1XXXXXXXXXX` for US, or preserve `+<digits>`.
fn normalize_handle(handle: &str) -> String {
    let trimmed = handle.trim();

    if trimmed.contains('@') {
        return trimmed.to_lowercase();
    }

    let digits: String = trimmed.chars().filter(|c| c.is_ascii_digit()).collect();

    if digits.len() == 10 {
        format!("+1{}", digits)
    } else if (digits.len() == 11 && digits.starts_with('1')) || trimmed.starts_with('+') {
        format!("+{}", digits)
    } else {
        trimmed.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::SignalConfig;

    fn cfg_with(policy: DmPolicy, allow: Vec<String>) -> SignalConfig {
        SignalConfig {
            account: "+15559999999".to_string(),
            dm_policy: policy,
            allow_from: allow,
            ..Default::default()
        }
    }

    #[test]
    fn test_open_policy_allows_all() {
        let config = cfg_with(DmPolicy::Open, vec![]);
        assert!(is_allowed("+15551234567", &config));
        assert!(is_allowed("random@email.com", &config));
    }

    #[test]
    fn test_disabled_policy_denies_all() {
        let config = cfg_with(DmPolicy::Disabled, vec![]);
        assert!(!is_allowed("+15551234567", &config));
        assert!(!is_allowed("random@email.com", &config));
    }

    #[test]
    fn test_allowlist_exact_match() {
        let config = cfg_with(DmPolicy::Allowlist, vec!["+15551234567".to_string()]);
        assert!(is_allowed("+15551234567", &config));
        assert!(!is_allowed("+15559999999", &config));
    }

    #[test]
    fn test_allowlist_phone_normalization() {
        let config = cfg_with(DmPolicy::Allowlist, vec!["+15551234567".to_string()]);
        assert!(is_allowed("5551234567", &config));
        assert!(is_allowed("15551234567", &config));
        assert!(is_allowed("+1-555-123-4567", &config));
    }

    #[test]
    fn test_allowlist_email_normalization() {
        let config = cfg_with(DmPolicy::Allowlist, vec!["user@icloud.com".to_string()]);
        assert!(is_allowed("User@iCloud.com", &config));
        assert!(is_allowed("user@icloud.com", &config));
        assert!(!is_allowed("other@icloud.com", &config));
    }

    #[test]
    fn test_normalize_handle_phone_variants() {
        assert_eq!(normalize_handle("5551234567"), "+15551234567");
        assert_eq!(normalize_handle("15551234567"), "+15551234567");
        assert_eq!(normalize_handle("+15551234567"), "+15551234567");
        assert_eq!(normalize_handle("+1-555-123-4567"), "+15551234567");
    }

    #[test]
    fn test_normalize_handle_email() {
        assert_eq!(normalize_handle("User@Example.COM"), "user@example.com");
        assert_eq!(normalize_handle("  test@test.com  "), "test@test.com");
    }
}
