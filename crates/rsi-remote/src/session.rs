//! Pure, in-memory browser session state for the remote gateway (ADR D5).
//!
//! This module owns no sockets and no clock: callers inject [`Instant`] values
//! and the HTTP layer is responsible for translating the returned cookie and
//! secret material into headers. All token material is 32 random bytes rendered
//! as lowercase hex and compared in constant time.

use std::collections::HashMap;
use std::sync::{Mutex, PoisonError};
use std::time::{Duration, Instant};

use rand::RngCore;
use rand::rngs::OsRng;

/// Lifetime of a single-use boot nonce.
pub const BOOT_TTL: Duration = Duration::from_secs(60);
/// Idle expiry: no use for this long and the session is gone.
pub const IDLE_TTL: Duration = Duration::from_secs(30 * 60);
/// Absolute expiry regardless of activity.
pub const ABSOLUTE_TTL: Duration = Duration::from_secs(8 * 60 * 60);
/// Maximum live boot nonces.
pub const MAX_BOOTS: usize = 64;
/// Maximum live sessions.
pub const MAX_SESSIONS: usize = 64;
/// Temporary host-only boot cookie name.
pub const BOOT_COOKIE: &str = "__Host-rsi_boot";
/// Rotated session cookie name.
pub const SESSION_COOKIE: &str = "__Host-rsi_remote";

const SECRET_LEN: usize = 64;
const HEX: &[u8; 16] = b"0123456789abcdef";

/// The identity a browser session is pinned to: owner, client node and server
/// node. A session never migrates between them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Binding {
    pub owner_user_id: u64,
    pub client_node: String,
    pub server_node: String,
}

/// Fail-closed reasons a request is refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionDenial {
    NoCookie,
    UnknownToken,
    Expired,
    BindingMismatch,
    BadOrigin,
    BadNonce,
    BadCsrf,
    Capacity,
}

impl SessionDenial {
    /// Stable machine code for the HTTP layer.
    #[must_use]
    pub const fn code(self) -> &'static str {
        match self {
            Self::NoCookie => "no_cookie",
            Self::UnknownToken => "unknown_token",
            Self::Expired => "expired",
            Self::BindingMismatch => "binding_mismatch",
            Self::BadOrigin => "bad_origin",
            Self::BadNonce => "bad_nonce",
            Self::BadCsrf => "bad_csrf",
            Self::Capacity => "capacity",
        }
    }
}

/// A freshly minted cookie value plus its matching nonce or CSRF token.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Issued {
    pub cookie: String,
    pub secret: String,
}

struct Boot {
    binding: Binding,
    created: Instant,
    nonce: String,
}

struct SessionEntry {
    binding: Binding,
    created: Instant,
    last_used: Instant,
    csrf: String,
}

#[derive(Default)]
struct Inner {
    boots: HashMap<String, Boot>,
    sessions: HashMap<String, SessionEntry>,
}

/// In-memory boot nonce and session store.
pub struct Sessions {
    inner: Mutex<Inner>,
}

impl Default for Sessions {
    fn default() -> Self {
        Self::new()
    }
}

// The lock is intentionally held across each whole operation: boot
// consumption, capacity checks and expiry eviction must be atomic. The
// nursery lint cannot see that, so it is allowed here.
#[allow(clippy::significant_drop_tightening)]
impl Sessions {
    /// Create an empty store.
    #[must_use]
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(Inner::default()),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Mint a single-use boot cookie and nonce for `binding`.
    ///
    /// # Errors
    ///
    /// Returns [`SessionDenial::Capacity`] when [`MAX_BOOTS`] live nonces
    /// remain after expired entries are evicted.
    pub fn bootstrap(&self, binding: &Binding, now: Instant) -> Result<Issued, SessionDenial> {
        let mut inner = self.lock();
        inner
            .boots
            .retain(|_, boot| !boot_expired(now, boot.created));
        if inner.boots.len() >= MAX_BOOTS {
            return Err(SessionDenial::Capacity);
        }
        let cookie = random_secret();
        let nonce = random_secret();
        inner.boots.insert(
            cookie.clone(),
            Boot {
                binding: binding.clone(),
                created: now,
                nonce: nonce.clone(),
            },
        );
        Ok(Issued {
            cookie,
            secret: nonce,
        })
    }

    /// Exchange a boot cookie and nonce for a session cookie and CSRF token.
    ///
    /// The boot entry is consumed on presentation regardless of later checks.
    ///
    /// # Errors
    ///
    /// Returns the first failing denial in order: [`SessionDenial::NoCookie`],
    /// [`SessionDenial::UnknownToken`], [`SessionDenial::Expired`],
    /// [`SessionDenial::BadOrigin`], [`SessionDenial::BadNonce`],
    /// [`SessionDenial::BindingMismatch`], [`SessionDenial::Capacity`].
    pub fn establish(
        &self,
        boot_cookie: Option<&str>,
        nonce: &str,
        origin_ok: bool,
        binding: &Binding,
        now: Instant,
    ) -> Result<Issued, SessionDenial> {
        let mut inner = self.lock();
        let cookie = boot_cookie.ok_or(SessionDenial::NoCookie)?;
        if !is_secret(cookie) {
            return Err(SessionDenial::UnknownToken);
        }
        let boot = inner
            .boots
            .remove(cookie)
            .ok_or(SessionDenial::UnknownToken)?;
        if boot_expired(now, boot.created) {
            return Err(SessionDenial::Expired);
        }
        if !origin_ok {
            return Err(SessionDenial::BadOrigin);
        }
        if !is_secret(nonce) || !secret_eq(nonce, &boot.nonce) {
            return Err(SessionDenial::BadNonce);
        }
        if boot.binding != *binding {
            return Err(SessionDenial::BindingMismatch);
        }
        inner
            .sessions
            .retain(|_, session| !session_expired(now, session.created, session.last_used));
        if inner.sessions.len() >= MAX_SESSIONS {
            return Err(SessionDenial::Capacity);
        }
        let cookie = random_secret();
        let csrf = random_secret();
        inner.sessions.insert(
            cookie.clone(),
            SessionEntry {
                binding: binding.clone(),
                created: now,
                last_used: now,
                csrf: csrf.clone(),
            },
        );
        Ok(Issued {
            cookie,
            secret: csrf,
        })
    }

    /// Validate a private GET/SSE cookie against origin, binding and expiry.
    ///
    /// # Errors
    ///
    /// Returns [`SessionDenial::NoCookie`], [`SessionDenial::UnknownToken`],
    /// [`SessionDenial::Expired`], [`SessionDenial::BadOrigin`] or
    /// [`SessionDenial::BindingMismatch`].
    pub fn check(
        &self,
        session_cookie: Option<&str>,
        origin_ok: bool,
        binding: &Binding,
        now: Instant,
    ) -> Result<(), SessionDenial> {
        let mut inner = self.lock();
        let cookie = session_cookie.ok_or(SessionDenial::NoCookie)?;
        if !is_secret(cookie) {
            return Err(SessionDenial::UnknownToken);
        }
        let Some(session) = inner.sessions.get(cookie) else {
            return Err(SessionDenial::UnknownToken);
        };
        if session_expired(now, session.created, session.last_used) {
            inner.sessions.remove(cookie);
            return Err(SessionDenial::Expired);
        }
        if !origin_ok {
            return Err(SessionDenial::BadOrigin);
        }
        if session.binding != *binding {
            inner.sessions.remove(cookie);
            return Err(SessionDenial::BindingMismatch);
        }
        if let Some(session) = inner.sessions.get_mut(cookie) {
            session.last_used = now;
        }
        Ok(())
    }

    /// Validate a POST cookie plus CSRF token.
    ///
    /// # Errors
    ///
    /// Returns every [`Sessions::check`] denial plus [`SessionDenial::BadCsrf`].
    pub fn check_csrf(
        &self,
        session_cookie: Option<&str>,
        csrf: Option<&str>,
        origin_ok: bool,
        binding: &Binding,
        now: Instant,
    ) -> Result<(), SessionDenial> {
        self.check(session_cookie, origin_ok, binding, now)?;
        let csrf = csrf.ok_or(SessionDenial::BadCsrf)?;
        if !is_secret(csrf) {
            return Err(SessionDenial::BadCsrf);
        }
        let cookie = session_cookie.ok_or(SessionDenial::NoCookie)?;
        let inner = self.lock();
        let session = inner
            .sessions
            .get(cookie)
            .ok_or(SessionDenial::UnknownToken)?;
        if !secret_eq(csrf, &session.csrf) {
            return Err(SessionDenial::BadCsrf);
        }
        Ok(())
    }

    /// Validate a logout request and immediately remove the session.
    ///
    /// # Errors
    ///
    /// Returns every [`Sessions::check_csrf`] denial.
    pub fn logout(
        &self,
        session_cookie: Option<&str>,
        csrf: Option<&str>,
        origin_ok: bool,
        binding: &Binding,
        now: Instant,
    ) -> Result<(), SessionDenial> {
        self.check_csrf(session_cookie, csrf, origin_ok, binding, now)?;
        if let Some(cookie) = session_cookie {
            self.lock().sessions.remove(cookie);
        }
        Ok(())
    }

    /// Drop every boot nonce and session.
    pub fn clear_all(&self) {
        let mut inner = self.lock();
        inner.boots.clear();
        inner.sessions.clear();
    }
}

/// Exact-origin / Fetch Metadata origin gate for private requests.
#[must_use]
pub fn origin_ok(origin: Option<&str>, sec_fetch_site: Option<&str>, canonical_host: &str) -> bool {
    origin.map_or_else(
        || sec_fetch_site == Some("same-origin"),
        |value| value == format!("https://{canonical_host}"),
    )
}

/// Parse one `Cookie` header value, returning `name`'s non-empty value.
///
/// A duplicated name is ambiguous and yields `None`.
#[must_use]
pub fn cookie<'a>(header: Option<&'a str>, name: &str) -> Option<&'a str> {
    let header = header?;
    let mut found: Option<&str> = None;
    for part in header.split(';') {
        let Some((key, value)) = part.trim().split_once('=') else {
            continue;
        };
        if key.trim() != name {
            continue;
        }
        let value = value.trim();
        if value.is_empty() || found.is_some() {
            return None;
        }
        found = Some(value);
    }
    found
}

/// `Set-Cookie` value for the bootstrap nonce cookie.
#[must_use]
pub fn set_boot_cookie(value: &str) -> String {
    format!("{BOOT_COOKIE}={value}; Max-Age=60; Path=/; Secure; HttpOnly; SameSite=Strict")
}

/// `Set-Cookie` value for the rotated session cookie (no `Domain`, no `Max-Age`).
#[must_use]
pub fn set_session_cookie(value: &str) -> String {
    format!("{SESSION_COOKIE}={value}; Path=/; Secure; HttpOnly; SameSite=Strict")
}

/// `Set-Cookie` value that clears `name`.
#[must_use]
pub fn clear_cookie(name: &str) -> String {
    format!("{name}=; Max-Age=0; Path=/; Secure; HttpOnly; SameSite=Strict")
}

fn random_secret() -> String {
    let mut bytes = [0u8; 32];
    OsRng.fill_bytes(&mut bytes);
    let mut out = String::with_capacity(SECRET_LEN);
    for byte in bytes {
        out.push(HEX[(byte >> 4) as usize] as char);
        out.push(HEX[(byte & 0x0f) as usize] as char);
    }
    out
}

fn is_secret(value: &str) -> bool {
    value.len() == SECRET_LEN
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn secret_eq(left: &str, right: &str) -> bool {
    let (left, right) = (left.as_bytes(), right.as_bytes());
    if left.len() != right.len() {
        return false;
    }
    let mut diff = 0u8;
    for (a, b) in left.iter().zip(right) {
        diff |= a ^ b;
    }
    diff == 0
}

fn boot_expired(now: Instant, created: Instant) -> bool {
    now.saturating_duration_since(created) > BOOT_TTL
}

fn session_expired(now: Instant, created: Instant, last_used: Instant) -> bool {
    now.saturating_duration_since(last_used) > IDLE_TTL
        || now.saturating_duration_since(created) > ABSOLUTE_TTL
}

#[cfg(test)]
mod tests {
    use super::*;

    fn binding_for(client_node: &str) -> Binding {
        Binding {
            owner_user_id: 7,
            client_node: client_node.to_string(),
            server_node: "server-a".to_string(),
        }
    }

    fn establish_at(sessions: &Sessions, binding: &Binding, now: Instant) -> Issued {
        let boot = sessions.bootstrap(binding, now).expect("bootstrap");
        sessions
            .establish(Some(&boot.cookie), &boot.secret, true, binding, now)
            .expect("establish")
    }

    #[test]
    fn happy_path_lifecycle() {
        let sessions = Sessions::new();
        let now = Instant::now();
        let binding = binding_for("client-1");
        let issued = establish_at(&sessions, &binding, now);
        assert_eq!(issued.cookie.len(), 64);
        assert_eq!(issued.secret.len(), 64);
        sessions
            .check(Some(&issued.cookie), true, &binding, now)
            .expect("check");
        sessions
            .check_csrf(
                Some(&issued.cookie),
                Some(&issued.secret),
                true,
                &binding,
                now,
            )
            .expect("check_csrf");
        sessions
            .logout(
                Some(&issued.cookie),
                Some(&issued.secret),
                true,
                &binding,
                now,
            )
            .expect("logout");
        assert_eq!(
            sessions.check(Some(&issued.cookie), true, &binding, now),
            Err(SessionDenial::UnknownToken)
        );
    }

    #[test]
    fn boot_cookie_is_single_use_after_success() {
        let sessions = Sessions::new();
        let now = Instant::now();
        let binding = binding_for("client-1");
        let boot = sessions.bootstrap(&binding, now).expect("bootstrap");
        sessions
            .establish(Some(&boot.cookie), &boot.secret, true, &binding, now)
            .expect("establish");
        assert_eq!(
            sessions.establish(Some(&boot.cookie), &boot.secret, true, &binding, now),
            Err(SessionDenial::UnknownToken)
        );
    }

    #[test]
    fn boot_cookie_is_single_use_after_failed_nonce() {
        let sessions = Sessions::new();
        let now = Instant::now();
        let binding = binding_for("client-1");
        let boot = sessions.bootstrap(&binding, now).expect("bootstrap");
        let wrong = random_secret();
        assert_eq!(
            sessions.establish(Some(&boot.cookie), &wrong, true, &binding, now),
            Err(SessionDenial::BadNonce)
        );
        assert_eq!(
            sessions.establish(Some(&boot.cookie), &boot.secret, true, &binding, now),
            Err(SessionDenial::UnknownToken)
        );
    }

    #[test]
    fn boot_expires_after_ttl() {
        let sessions = Sessions::new();
        let now = Instant::now();
        let binding = binding_for("client-1");
        let boot = sessions.bootstrap(&binding, now).expect("bootstrap");
        let late = now + Duration::from_secs(61);
        assert_eq!(
            sessions.establish(Some(&boot.cookie), &boot.secret, true, &binding, late),
            Err(SessionDenial::Expired)
        );
    }

    #[test]
    fn establish_denials() {
        let sessions = Sessions::new();
        let now = Instant::now();
        let binding = binding_for("client-1");

        let boot = sessions.bootstrap(&binding, now).expect("bootstrap");
        assert_eq!(
            sessions.establish(Some(&boot.cookie), &boot.secret, false, &binding, now),
            Err(SessionDenial::BadOrigin)
        );

        let boot = sessions.bootstrap(&binding, now).expect("bootstrap");
        assert_eq!(
            sessions.establish(
                Some(&boot.cookie),
                &boot.secret,
                true,
                &binding_for("client-2"),
                now
            ),
            Err(SessionDenial::BindingMismatch)
        );

        assert_eq!(
            sessions.establish(None, "x", true, &binding, now),
            Err(SessionDenial::NoCookie)
        );
        assert_eq!(
            sessions.establish(Some("short"), "x", true, &binding, now),
            Err(SessionDenial::UnknownToken)
        );
    }

    #[test]
    fn idle_expiry_removes_entry() {
        let sessions = Sessions::new();
        let now = Instant::now();
        let binding = binding_for("client-1");
        let issued = establish_at(&sessions, &binding, now);
        let late = now + IDLE_TTL + Duration::from_secs(1);
        assert_eq!(
            sessions.check(Some(&issued.cookie), true, &binding, late),
            Err(SessionDenial::Expired)
        );
        assert_eq!(
            sessions.check(Some(&issued.cookie), true, &binding, late),
            Err(SessionDenial::UnknownToken)
        );
    }

    #[test]
    fn absolute_expiry_survives_activity() {
        let sessions = Sessions::new();
        let now = Instant::now();
        let binding = binding_for("client-1");
        let issued = establish_at(&sessions, &binding, now);
        let mut cursor = now;
        for _ in 0..48 {
            cursor += Duration::from_secs(10 * 60);
            sessions
                .check(Some(&issued.cookie), true, &binding, cursor)
                .expect("active check");
        }
        let after = cursor + Duration::from_secs(1);
        assert_eq!(
            sessions.check(Some(&issued.cookie), true, &binding, after),
            Err(SessionDenial::Expired)
        );
    }

    #[test]
    fn binding_mismatch_removes_session() {
        let sessions = Sessions::new();
        let now = Instant::now();
        let binding = binding_for("client-1");
        let issued = establish_at(&sessions, &binding, now);
        assert_eq!(
            sessions.check(Some(&issued.cookie), true, &binding_for("client-2"), now),
            Err(SessionDenial::BindingMismatch)
        );
        assert_eq!(
            sessions.check(Some(&issued.cookie), true, &binding, now),
            Err(SessionDenial::UnknownToken)
        );
    }

    #[test]
    fn malformed_session_cookie_is_unknown() {
        let sessions = Sessions::new();
        let now = Instant::now();
        let binding = binding_for("client-1");
        let issued = establish_at(&sessions, &binding, now);
        let short = &issued.cookie[..63];
        let upper = issued.cookie.to_uppercase();
        assert_eq!(
            sessions.check(Some(short), true, &binding, now),
            Err(SessionDenial::UnknownToken)
        );
        assert_eq!(
            sessions.check(Some(&upper), true, &binding, now),
            Err(SessionDenial::UnknownToken)
        );
        sessions
            .check(Some(&issued.cookie), true, &binding, now)
            .expect("entry untouched by malformed values");
    }

    #[test]
    fn csrf_denials() {
        let sessions = Sessions::new();
        let now = Instant::now();
        let binding = binding_for("client-1");
        let issued = establish_at(&sessions, &binding, now);
        assert_eq!(
            sessions.check_csrf(Some(&issued.cookie), None, true, &binding, now),
            Err(SessionDenial::BadCsrf)
        );
        assert_eq!(
            sessions.check_csrf(
                Some(&issued.cookie),
                Some(&random_secret()),
                true,
                &binding,
                now
            ),
            Err(SessionDenial::BadCsrf)
        );
        assert_eq!(
            sessions.check_csrf(Some(&issued.cookie), Some("nope"), true, &binding, now),
            Err(SessionDenial::BadCsrf)
        );
        assert_eq!(
            sessions.logout(Some(&issued.cookie), Some("nope"), true, &binding, now),
            Err(SessionDenial::BadCsrf)
        );
        sessions
            .check_csrf(
                Some(&issued.cookie),
                Some(&issued.secret),
                true,
                &binding,
                now,
            )
            .expect("session survives failed csrf");
    }

    #[test]
    fn capacity_is_enforced_and_recovers() {
        let sessions = Sessions::new();
        let now = Instant::now();
        let binding = binding_for("client-1");
        for _ in 0..MAX_SESSIONS {
            establish_at(&sessions, &binding, now);
        }
        let boot = sessions.bootstrap(&binding, now).expect("bootstrap");
        assert_eq!(
            sessions.establish(Some(&boot.cookie), &boot.secret, true, &binding, now),
            Err(SessionDenial::Capacity)
        );
        let late = now + IDLE_TTL + Duration::from_secs(1);
        let boot = sessions.bootstrap(&binding, late).expect("bootstrap");
        sessions
            .establish(Some(&boot.cookie), &boot.secret, true, &binding, late)
            .expect("capacity recovered after expiry");
    }

    #[test]
    fn origin_ok_table() {
        let host = "host.example.ts.net";
        for (origin, site, expected) in [
            (Some("https://host.example.ts.net"), None, true),
            (Some("https://host.example.ts.net/"), None, false),
            (Some("http://host.example.ts.net"), None, false),
            (Some("https://host.example.ts.net:443"), None, false),
            (Some("null"), Some("same-origin"), false),
            (None, Some("same-origin"), true),
            (None, Some("cross-site"), false),
            (None, Some("none"), false),
            (None, None, false),
        ] {
            assert_eq!(
                origin_ok(origin, site, host),
                expected,
                "{origin:?} {site:?}"
            );
        }
    }

    #[test]
    fn cookie_parsing() {
        assert_eq!(cookie(Some("a=1; b=2"), "b"), Some("2"));
        assert_eq!(cookie(Some("  a=1 ;  b=2  "), "a"), Some("1"));
        assert_eq!(cookie(Some("a=1; a=2"), "a"), None);
        assert_eq!(cookie(Some("a=; b=2"), "a"), None);
        assert_eq!(cookie(Some("b=2"), "a"), None);
        assert_eq!(cookie(None, "a"), None);
    }

    #[test]
    fn cookie_string_formats() {
        assert_eq!(
            set_boot_cookie("v"),
            "__Host-rsi_boot=v; Max-Age=60; Path=/; Secure; HttpOnly; SameSite=Strict"
        );
        assert_eq!(
            set_session_cookie("v"),
            "__Host-rsi_remote=v; Path=/; Secure; HttpOnly; SameSite=Strict"
        );
        assert_eq!(
            clear_cookie(SESSION_COOKIE),
            "__Host-rsi_remote=; Max-Age=0; Path=/; Secure; HttpOnly; SameSite=Strict"
        );
        assert_eq!(BOOT_TTL, Duration::from_secs(60));
        assert_eq!(IDLE_TTL, Duration::from_secs(1800));
        assert_eq!(ABSOLUTE_TTL, Duration::from_secs(28800));
    }

    #[test]
    fn clear_all_drops_everything() {
        let sessions = Sessions::new();
        let now = Instant::now();
        let binding = binding_for("client-1");
        let boot = sessions.bootstrap(&binding, now).expect("bootstrap");
        let issued = establish_at(&sessions, &binding, now);
        sessions.clear_all();
        assert_eq!(
            sessions.establish(Some(&boot.cookie), &boot.secret, true, &binding, now),
            Err(SessionDenial::UnknownToken)
        );
        assert_eq!(
            sessions.check(Some(&issued.cookie), true, &binding, now),
            Err(SessionDenial::UnknownToken)
        );
    }

    #[test]
    fn denial_codes_are_stable() {
        assert_eq!(SessionDenial::NoCookie.code(), "no_cookie");
        assert_eq!(SessionDenial::UnknownToken.code(), "unknown_token");
        assert_eq!(SessionDenial::Expired.code(), "expired");
        assert_eq!(SessionDenial::BindingMismatch.code(), "binding_mismatch");
        assert_eq!(SessionDenial::BadOrigin.code(), "bad_origin");
        assert_eq!(SessionDenial::BadNonce.code(), "bad_nonce");
        assert_eq!(SessionDenial::BadCsrf.code(), "bad_csrf");
        assert_eq!(SessionDenial::Capacity.code(), "capacity");
    }
}
