//! Access control for inbound messages.
//!
//! Filters messages based on DM policy and allowlist.

use crate::config::{DmPolicy, ImessageConfig};

/// Check if a sender is allowed to interact with the bridge.
pub fn is_allowed(sender: &str, config: &ImessageConfig) -> bool {
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
/// - Lowercase for emails
/// - Strip whitespace
/// - Normalize phone numbers to +1XXXXXXXXXX format
fn normalize_handle(handle: &str) -> String {
    let trimmed = handle.trim();

    // If it looks like an email
    if trimmed.contains('@') {
        return trimmed.to_lowercase();
    }

    // Phone number normalization
    let digits: String = trimmed.chars().filter(|c| c.is_ascii_digit()).collect();

    if digits.len() == 10 {
        // US number without country code
        format!("+1{}", digits)
    } else if digits.len() == 11 && digits.starts_with('1') {
        // US number with country code
        format!("+{}", digits)
    } else if trimmed.starts_with('+') {
        // Already has country code
        format!("+{}", digits)
    } else {
        // Unknown format — return as-is
        trimmed.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ImessageConfig;

    #[test]
    fn test_open_policy_allows_all() {
        let config = ImessageConfig {
            dm_policy: DmPolicy::Open,
            ..Default::default()
        };
        assert!(is_allowed("+15551234567", &config));
        assert!(is_allowed("random@email.com", &config));
    }

    #[test]
    fn test_disabled_policy_denies_all() {
        let config = ImessageConfig {
            dm_policy: DmPolicy::Disabled,
            ..Default::default()
        };
        assert!(!is_allowed("+15551234567", &config));
        assert!(!is_allowed("random@email.com", &config));
    }

    #[test]
    fn test_allowlist_exact_match() {
        let config = ImessageConfig {
            dm_policy: DmPolicy::Allowlist,
            allow_from: vec!["+15551234567".to_string()],
            ..Default::default()
        };
        assert!(is_allowed("+15551234567", &config));
        assert!(!is_allowed("+15559999999", &config));
    }

    #[test]
    fn test_allowlist_phone_normalization() {
        let config = ImessageConfig {
            dm_policy: DmPolicy::Allowlist,
            allow_from: vec!["+15551234567".to_string()],
            ..Default::default()
        };
        // 10-digit should normalize to +1...
        assert!(is_allowed("5551234567", &config));
        // 11-digit with leading 1
        assert!(is_allowed("15551234567", &config));
        // With dashes
        assert!(is_allowed("+1-555-123-4567", &config));
    }

    #[test]
    fn test_allowlist_email_normalization() {
        let config = ImessageConfig {
            dm_policy: DmPolicy::Allowlist,
            allow_from: vec!["user@icloud.com".to_string()],
            ..Default::default()
        };
        assert!(is_allowed("User@iCloud.com", &config));
        assert!(is_allowed("user@icloud.com", &config));
        assert!(!is_allowed("other@icloud.com", &config));
    }

    #[test]
    fn test_normalize_handle_phone_variants() {
        assert_eq!(normalize_handle("5551234567"), "+15551234567");
        assert_eq!(normalize_handle("15551234567"), "+15551234567"); // 11 digits with leading 1
        assert_eq!(normalize_handle("+15551234567"), "+15551234567");
        assert_eq!(normalize_handle("+1-555-123-4567"), "+15551234567");
    }

    #[test]
    fn test_normalize_handle_email() {
        assert_eq!(normalize_handle("User@Example.COM"), "user@example.com");
        assert_eq!(normalize_handle("  test@test.com  "), "test@test.com");
    }
}
