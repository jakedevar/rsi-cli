//! Zeroize-on-drop secret string whose `Debug`/`Display` print only a
//! redacted fingerprint.

use sha2::{Digest, Sha256};
use std::fmt;
use zeroize::Zeroizing;

/// Number of hex characters of the SHA-256 digest kept as the fingerprint.
pub const FINGERPRINT_HEX_LEN: usize = 8;

/// Redacted, non-reversible fingerprint: the first
/// [`FINGERPRINT_HEX_LEN`] hex characters of the value's SHA-256.
#[must_use]
pub fn fingerprint(value: &str) -> String {
    let digest = Sha256::digest(value.as_bytes());
    let mut hex = hex::encode(digest);
    hex.truncate(FINGERPRINT_HEX_LEN);
    hex
}

/// A provider credential, zeroed on drop.
///
/// Neither `Debug` nor `Display` ever prints the value. It deliberately
/// implements no `Serialize`: only the vault store module can write it to
/// disk.
#[derive(Clone, PartialEq, Eq)]
pub struct SecretString {
    value: Zeroizing<String>,
}

impl SecretString {
    #[must_use]
    pub fn new(value: String) -> Self {
        Self {
            value: Zeroizing::new(value),
        }
    }

    /// The raw value, for the one place that must hand it to a transport
    /// (HTTP header or a single injected child env var).
    #[must_use]
    pub fn expose(&self) -> &str {
        self.value.as_str()
    }

    #[must_use]
    pub fn fingerprint(&self) -> String {
        fingerprint(self.expose())
    }
}

impl fmt::Debug for SecretString {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "SecretString(fp:{})", self.fingerprint())
    }
}

impl fmt::Display for SecretString {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "<redacted fp:{}>", self.fingerprint())
    }
}

/// Serde adapter used only by the on-disk vault file.
pub(super) mod serde_exposed {
    use super::SecretString;
    use serde::{Deserialize, Deserializer, Serializer};

    pub(in crate::vault) fn serialize<S: Serializer>(
        value: &SecretString,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(value.expose())
    }

    pub(in crate::vault) fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<SecretString, D::Error> {
        String::deserialize(deserializer).map(SecretString::new)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-05"))]
    #[test]
    fn debug_and_display_are_redacted_to_fingerprint() {
        let secret = SecretString::new("sk-test-redaction-canary-0001".into());
        let debug = format!("{secret:?}");
        let display = format!("{secret}");
        let fp = secret.fingerprint();
        assert_eq!(fp.len(), FINGERPRINT_HEX_LEN);
        assert!(debug.contains(&fp), "{debug}");
        assert!(display.contains(&fp), "{display}");
        for rendered in [&debug, &display] {
            assert!(!rendered.contains("sk-test-redaction-canary-0001"));
            assert!(!rendered.contains("canary"));
        }
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-05"))]
    #[test]
    fn fingerprint_is_stable_sha256_prefix() {
        // sha256("abc") = ba7816bf...
        assert_eq!(fingerprint("abc"), "ba7816bf");
        assert_eq!(SecretString::new("abc".into()).fingerprint(), "ba7816bf");
    }
}
