//! Per-request readiness and identity gate for the gateway ingress.
//!
//! A full ordered `LocalAPI` cycle (`status` -> `serve-config` -> self `whois`)
//! produces one cached [`ServerBinding`]; fresh client `whois` evidence is then
//! required before any route handling. Clocks are injected: this module never
//! reads the system clock, so tests can drive wall/monotonic discontinuities.

use crate::config::Config;
use crate::localapi::{
    ClientIdentity, Denial, ServerBinding, client_identity, self_whois_matches, serve_route_ok,
    server_binding,
};
use crate::localapi_client::{LocalApiClient, LocalApiError};
use chrono::{DateTime, Utc};
use std::net::IpAddr;
use std::path::PathBuf;
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, Instant};

/// A complete `Ready` observation is valid for this long after its cycle.
pub const READY_FOR: Duration = Duration::from_secs(5);
/// The background refresh loop re-observes the full cycle at this cadence.
pub const REFRESH_EVERY: Duration = Duration::from_secs(4);
/// A wall/monotonic discontinuity larger than this suspends readiness.
pub const MAX_CLOCK_SKEW: Duration = Duration::from_secs(2);

/// Why a refresh or an authorization was refused. Codes are stable strings.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GateDenial {
    /// No complete, unexpired `Ready` observation is cached.
    NotReady,
    /// The wall and monotonic clocks disagreed beyond [`MAX_CLOCK_SKEW`].
    ClockJump,
    /// A `LocalAPI` call itself failed.
    LocalApi(LocalApiError),
    /// A decoded binding, route or identity observation denied.
    Identity(Denial),
}

impl GateDenial {
    /// Stable sanitized code for diagnostics.
    #[must_use]
    pub const fn code(self) -> &'static str {
        match self {
            Self::NotReady => "not_ready",
            Self::ClockJump => "clock_jump",
            Self::LocalApi(error) => error.code(),
            Self::Identity(denial) => denial.code(),
        }
    }
}

/// The last complete, positively verified server observation.
struct Ready {
    binding: ServerBinding,
    observed: Instant,
    wall: DateTime<Utc>,
}

/// Cached readiness plus a fresh per-request client identity check.
pub struct Gate {
    client: LocalApiClient,
    ingress_socket: PathBuf,
    state: Mutex<Option<Ready>>,
}

impl Gate {
    /// Create a gate that is not ready until the first successful refresh.
    #[must_use]
    pub fn new(client: LocalApiClient, ingress_socket: impl Into<PathBuf>) -> Self {
        Self {
            client,
            ingress_socket: ingress_socket.into(),
            state: Mutex::new(None),
        }
    }

    /// Drop any cached readiness. The next successful refresh restores it.
    pub fn clear(&self) {
        *self.lock() = None;
    }

    /// Run one full ordered `LocalAPI` cycle and cache readiness on success.
    ///
    /// # Errors
    ///
    /// Returns [`GateDenial::ClockJump`] when the injected clocks disagree
    /// beyond [`MAX_CLOCK_SKEW`] with the cached observation, otherwise the
    /// [`GateDenial`] for the first failed `LocalAPI` call or decode. Cached
    /// state is cleared before any error is returned (fail closed).
    pub async fn refresh(
        &self,
        policy: &Config,
        wall: DateTime<Utc>,
        mono: Instant,
    ) -> Result<(), GateDenial> {
        let jumped = {
            let state = self.lock();
            state
                .as_ref()
                .is_some_and(|prev| clock_jumped(prev, wall, mono))
        };
        if jumped {
            self.clear();
            return Err(GateDenial::ClockJump);
        }

        match self.cycle(policy, wall, mono).await {
            Ok(ready) => {
                *self.lock() = Some(ready);
                Ok(())
            }
            Err(error) => {
                self.clear();
                Err(error)
            }
        }
    }

    /// Return the cached binding only while it is fresh at `mono`.
    #[must_use]
    pub fn ready(&self, mono: Instant) -> Option<ServerBinding> {
        let state = self.lock();
        let fresh = match state.as_ref() {
            Some(ready)
                if mono >= ready.observed
                    && mono.saturating_duration_since(ready.observed) <= READY_FOR =>
            {
                Some(ready.binding.clone())
            }
            _ => None,
        };
        drop(state);
        fresh
    }

    /// Require cached readiness, then check the client identity of `source`.
    ///
    /// # Errors
    ///
    /// Returns [`GateDenial::NotReady`] before making any `LocalAPI` call when
    /// readiness is absent or stale; otherwise maps a fresh client `whois`
    /// failure or identity decode to the matching [`GateDenial`].
    pub async fn authorize(
        &self,
        policy: &Config,
        source: IpAddr,
        wall: DateTime<Utc>,
        mono: Instant,
    ) -> Result<ClientIdentity, GateDenial> {
        if self.ready(mono).is_none() {
            return Err(GateDenial::NotReady);
        }
        let whois = self
            .client
            .whois(source)
            .await
            .map_err(|error| match error {
                LocalApiError::NotFound => GateDenial::Identity(Denial::NodeNotAllowed),
                other => GateDenial::LocalApi(other),
            })?;
        client_identity(&whois, policy, source, wall).map_err(GateDenial::Identity)
    }

    /// One ordered `status` -> `serve-config` -> self `whois` cycle.
    async fn cycle(
        &self,
        policy: &Config,
        wall: DateTime<Utc>,
        mono: Instant,
    ) -> Result<Ready, GateDenial> {
        let status = self.client.status().await.map_err(GateDenial::LocalApi)?;
        let binding = server_binding(&status, policy, wall).map_err(GateDenial::Identity)?;

        let serve = self
            .client
            .serve_config()
            .await
            .map_err(GateDenial::LocalApi)?;
        serve_route_ok(&serve, &policy.canonical_host, &self.ingress_socket)
            .map_err(GateDenial::Identity)?;

        let server_ip = binding
            .tailscale_ips
            .first()
            .copied()
            .ok_or(GateDenial::Identity(Denial::Malformed))?;
        let self_whois = self
            .client
            .whois(server_ip)
            .await
            .map_err(GateDenial::LocalApi)?;
        self_whois_matches(&self_whois, &binding, wall).map_err(GateDenial::Identity)?;

        Ok(Ready {
            binding,
            observed: mono,
            wall,
        })
    }

    fn lock(&self) -> MutexGuard<'_, Option<Ready>> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

/// True when the wall and monotonic deltas since `prev` differ by more than
/// [`MAX_CLOCK_SKEW`]. A backwards monotonic step counts as a discontinuity.
fn clock_jumped(prev: &Ready, wall: DateTime<Utc>, mono: Instant) -> bool {
    let wall_delta = wall.signed_duration_since(prev.wall);
    let mono_delta = if mono >= prev.observed {
        chrono::Duration::from_std(mono - prev.observed).unwrap_or_default()
    } else {
        -chrono::Duration::from_std(prev.observed - mono).unwrap_or_default()
    };
    let skew = (wall_delta - mono_delta).num_milliseconds().unsigned_abs();
    u128::from(skew) > MAX_CLOCK_SKEW.as_millis()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;
    use std::sync::Arc;
    use tempfile::{TempDir, tempdir};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::UnixListener;

    const HOST: &str = "host.example.ts.net";
    const OWNER: u64 = 12_345;
    const SERVER_NODE: &str = "nSERVER";
    const CLIENT_NODE: &str = "nCLIENT";
    const SERVER_IP: &str = "100.100.100.1";
    const CLIENT_IP: &str = "100.101.102.103";

    fn policy() -> Config {
        Config {
            enabled: true,
            canonical_host: HOST.into(),
            owner_user_id: OWNER,
            allowed_node_ids: vec![CLIENT_NODE.into()],
            project_ids: vec![],
            ..Config::default()
        }
    }

    fn wall() -> DateTime<Utc> {
        DateTime::parse_from_rfc3339("2026-09-28T12:00:00Z")
            .unwrap()
            .with_timezone(&Utc)
    }

    fn client_ip() -> IpAddr {
        CLIENT_IP.parse().unwrap()
    }

    fn status_json(backend_state: &str) -> String {
        format!(
            r#"{{"BackendState":"{backend_state}","Self":{{"ID":"{SERVER_NODE}","StableID":"nSelfStable","UserID":{OWNER},"DNSName":"{HOST}.","TailscaleIPs":["{SERVER_IP}"],"Expired":false,"KeyExpiry":"0001-01-01T00:00:00Z"}},"CurrentTailnet":{{"MagicDNSSuffix":"example.ts.net"}}}}"#
        )
    }

    fn serve_json(ingress: &Path, funnel_on: bool) -> String {
        let funnel = if funnel_on {
            format!(r#"{{"{HOST}:443":true}}"#)
        } else {
            "{}".to_string()
        };
        format!(
            r#"{{"TCP":{{"443":{{"HTTPS":true}}}},"Web":{{"{HOST}:443":{{"Handlers":{{"/":{{"Proxy":"unix:{}"}}}}}}}},"AllowFunnel":{funnel}}}"#,
            ingress.display()
        )
    }

    fn self_whois_json() -> String {
        format!(
            r#"{{"UserProfile":{{"ID":{OWNER}}},"Node":{{"StableID":"{SERVER_NODE}","User":{OWNER},"MachineAuthorized":null,"Expired":false,"KeyExpiry":"0001-01-01T00:00:00Z"}}}}"#
        )
    }

    fn client_whois_json(stable_id: &str) -> String {
        format!(
            r#"{{"UserProfile":{{"ID":{OWNER}}},"Node":{{"StableID":"{stable_id}","User":{OWNER},"MachineAuthorized":null,"Expired":false,"KeyExpiry":"0001-01-01T00:00:00Z","Addresses":["{CLIENT_IP}/32"],"Tags":[],"Sharer":0}}}}"#
        )
    }

    fn http_ok(body: &str) -> Vec<u8> {
        format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n{body}",
            body.len()
        )
        .into_bytes()
    }

    fn http_not_found() -> Vec<u8> {
        b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n".to_vec()
    }

    #[derive(Clone)]
    struct Responses {
        status: String,
        serve: String,
        self_whois: String,
        client_whois: Option<String>,
    }

    struct FakeLocalApi {
        _dir: TempDir,
        socket: PathBuf,
        paths: Arc<Mutex<Vec<String>>>,
        responses: Arc<Mutex<Responses>>,
    }

    impl FakeLocalApi {
        fn start(ingress: &Path, client_whois: Option<String>) -> Self {
            let dir = tempdir().unwrap();
            let socket = dir.path().join("sock");
            let listener = UnixListener::bind(&socket).unwrap();
            let paths = Arc::new(Mutex::new(Vec::new()));
            let responses = Arc::new(Mutex::new(Responses {
                status: status_json("Running"),
                serve: serve_json(ingress, false),
                self_whois: self_whois_json(),
                client_whois,
            }));
            let recorded = Arc::clone(&paths);
            let shared = Arc::clone(&responses);
            tokio::spawn(async move {
                loop {
                    let Ok((mut stream, _)) = listener.accept().await else {
                        break;
                    };
                    let mut buf = Vec::new();
                    let mut chunk = [0u8; 1024];
                    loop {
                        let Ok(n) = stream.read(&mut chunk).await else {
                            break;
                        };
                        if n == 0 {
                            break;
                        }
                        buf.extend_from_slice(&chunk[..n]);
                        if buf.ends_with(b"\r\n\r\n") {
                            break;
                        }
                    }
                    let target = String::from_utf8_lossy(&buf)
                        .split("\r\n")
                        .next()
                        .unwrap_or("")
                        .split(' ')
                        .nth(1)
                        .unwrap_or("")
                        .to_string();
                    recorded.lock().unwrap().push(target.clone());
                    let current = shared.lock().unwrap().clone();
                    let _ = stream.write_all(&reply(&target, &current)).await;
                    let _ = stream.shutdown().await;
                }
            });
            Self {
                _dir: dir,
                socket,
                paths,
                responses,
            }
        }

        fn client(&self) -> LocalApiClient {
            LocalApiClient::new(&self.socket)
        }

        fn recorded(&self) -> Vec<String> {
            self.paths.lock().unwrap().clone()
        }

        fn set_status(&self, status: &str) {
            self.responses.lock().unwrap().status = status.to_string();
        }
    }

    fn reply(target: &str, current: &Responses) -> Vec<u8> {
        if target.starts_with("/localapi/v0/status") {
            http_ok(&current.status)
        } else if target.starts_with("/localapi/v0/serve-config") {
            http_ok(&current.serve)
        } else if target.starts_with("/localapi/v0/whois") {
            if target.contains(SERVER_IP) {
                http_ok(&current.self_whois)
            } else if let Some(client) = current.client_whois.as_deref() {
                http_ok(client)
            } else {
                http_not_found()
            }
        } else {
            http_not_found()
        }
    }

    #[tokio::test]
    async fn good_refresh_then_authorize_allowed_client() {
        let ingress = tempdir().unwrap();
        let ingress_socket = ingress.path().join("ingress.sock");
        let fake = FakeLocalApi::start(&ingress_socket, Some(client_whois_json(CLIENT_NODE)));
        let gate = Gate::new(fake.client(), &ingress_socket);
        let policy = policy();
        let wall = wall();
        let mono = Instant::now();

        gate.refresh(&policy, wall, mono).await.unwrap();
        assert!(gate.ready(mono).is_some());
        let identity = gate
            .authorize(&policy, client_ip(), wall, mono)
            .await
            .unwrap();
        assert_eq!(
            identity,
            ClientIdentity {
                node_stable_id: CLIENT_NODE.into(),
                user_id: OWNER,
            }
        );

        let paths = fake.recorded();
        assert!(paths.iter().any(|p| p.starts_with("/localapi/v0/status")));
        assert!(
            paths
                .iter()
                .any(|p| p.starts_with("/localapi/v0/serve-config"))
        );
        assert!(
            paths
                .iter()
                .any(|p| p.contains("/localapi/v0/whois") && p.contains(SERVER_IP))
        );
        assert!(
            paths
                .iter()
                .any(|p| p.contains("/localapi/v0/whois") && p.contains(CLIENT_IP))
        );
    }

    #[tokio::test]
    async fn authorize_before_refresh_is_not_ready_without_calls() {
        let ingress = tempdir().unwrap();
        let ingress_socket = ingress.path().join("ingress.sock");
        let fake = FakeLocalApi::start(&ingress_socket, Some(client_whois_json(CLIENT_NODE)));
        let gate = Gate::new(fake.client(), &ingress_socket);

        let result = gate
            .authorize(&policy(), client_ip(), wall(), Instant::now())
            .await;
        assert_eq!(result.unwrap_err(), GateDenial::NotReady);
        assert!(fake.recorded().is_empty());
    }

    #[tokio::test]
    async fn stale_readiness_denies_and_skips_client_whois() {
        let ingress = tempdir().unwrap();
        let ingress_socket = ingress.path().join("ingress.sock");
        let fake = FakeLocalApi::start(&ingress_socket, Some(client_whois_json(CLIENT_NODE)));
        let gate = Gate::new(fake.client(), &ingress_socket);
        let policy = policy();
        let wall = wall();
        let mono = Instant::now();

        gate.refresh(&policy, wall, mono).await.unwrap();
        let later = mono + Duration::from_secs(6);
        let result = gate
            .authorize(&policy, client_ip(), wall + Duration::from_secs(6), later)
            .await;
        assert_eq!(result.unwrap_err(), GateDenial::NotReady);
        assert!(!fake.recorded().iter().any(|p| p.contains(CLIENT_IP)));
    }

    #[tokio::test]
    async fn funnel_on_denies_refresh_and_readiness() {
        let ingress = tempdir().unwrap();
        let ingress_socket = ingress.path().join("ingress.sock");
        let fake = FakeLocalApi::start(&ingress_socket, Some(client_whois_json(CLIENT_NODE)));
        fake.responses.lock().unwrap().serve = serve_json(&ingress_socket, true);
        let gate = Gate::new(fake.client(), &ingress_socket);
        let mono = Instant::now();

        let result = gate.refresh(&policy(), wall(), mono).await;
        assert_eq!(result.unwrap_err(), GateDenial::Identity(Denial::FunnelOn));
        assert!(gate.ready(mono).is_none());
    }

    #[tokio::test]
    async fn failing_refresh_clears_previous_readiness() {
        let ingress = tempdir().unwrap();
        let ingress_socket = ingress.path().join("ingress.sock");
        let fake = FakeLocalApi::start(&ingress_socket, Some(client_whois_json(CLIENT_NODE)));
        let gate = Gate::new(fake.client(), &ingress_socket);
        let policy = policy();
        let wall = wall();
        let mono = Instant::now();

        gate.refresh(&policy, wall, mono).await.unwrap();
        assert!(gate.ready(mono).is_some());

        fake.set_status(&status_json("Stopped"));
        let later = mono + Duration::from_secs(4);
        let result = gate
            .refresh(&policy, wall + Duration::from_secs(4), later)
            .await;
        assert_eq!(
            result.unwrap_err(),
            GateDenial::Identity(Denial::NotRunning)
        );
        assert!(gate.ready(later).is_none());
    }

    #[tokio::test]
    async fn client_denials_map_to_identity() {
        let policy = policy();
        let wall = wall();

        let missing = tempdir().unwrap();
        let missing_socket = missing.path().join("ingress.sock");
        let fake = FakeLocalApi::start(&missing_socket, None);
        let gate = Gate::new(fake.client(), &missing_socket);
        let mono = Instant::now();
        gate.refresh(&policy, wall, mono).await.unwrap();
        let result = gate.authorize(&policy, client_ip(), wall, mono).await;
        assert_eq!(
            result.unwrap_err(),
            GateDenial::Identity(Denial::NodeNotAllowed)
        );

        let other = tempdir().unwrap();
        let other_socket = other.path().join("ingress.sock");
        let fake = FakeLocalApi::start(&other_socket, Some(client_whois_json("nOTHER")));
        let gate = Gate::new(fake.client(), &other_socket);
        gate.refresh(&policy, wall, mono).await.unwrap();
        let result = gate.authorize(&policy, client_ip(), wall, mono).await;
        assert_eq!(
            result.unwrap_err(),
            GateDenial::Identity(Denial::NodeNotAllowed)
        );
    }

    #[tokio::test]
    async fn clock_jump_suspends_then_good_cycle_restores() {
        let ingress = tempdir().unwrap();
        let ingress_socket = ingress.path().join("ingress.sock");
        let fake = FakeLocalApi::start(&ingress_socket, Some(client_whois_json(CLIENT_NODE)));
        let gate = Gate::new(fake.client(), &ingress_socket);
        let policy = policy();
        let wall = wall();
        let mono = Instant::now();

        gate.refresh(&policy, wall, mono).await.unwrap();

        let jumped = gate
            .refresh(
                &policy,
                wall + Duration::from_secs(10),
                mono + Duration::from_secs(5),
            )
            .await;
        assert_eq!(jumped.unwrap_err(), GateDenial::ClockJump);
        assert!(gate.ready(mono + Duration::from_secs(5)).is_none());

        gate.refresh(
            &policy,
            wall + Duration::from_secs(14),
            mono + Duration::from_secs(9),
        )
        .await
        .unwrap();
        assert!(gate.ready(mono + Duration::from_secs(9)).is_some());
    }

    #[test]
    fn denial_codes_are_stable() {
        assert_eq!(GateDenial::NotReady.code(), "not_ready");
        assert_eq!(GateDenial::ClockJump.code(), "clock_jump");
        assert_eq!(
            GateDenial::LocalApi(LocalApiError::Timeout).code(),
            "localapi_timeout"
        );
        assert_eq!(GateDenial::Identity(Denial::FunnelOn).code(), "funnel_on");
    }
}
