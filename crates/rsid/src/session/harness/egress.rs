//! SSRF-guarded HTTP fetch for Harness network tools (#774).
//!
//! The one enforcement point for a Harness tool that reaches the network. A
//! tool builds an [`EgressFetcher`] from the `EgressPolicy` in its
//! `ToolContext` (`context.policy.egress`) and calls [`EgressFetcher::fetch`];
//! it never opens its own HTTP client.
//!
//! Every hop of a fetch (the first request and each redirect target) is
//! vetted the same way, and the check is made on the address the connection
//! will actually use, so DNS rebinding cannot swap it afterwards:
//!
//! 1. only `http` and `https`, no embedded credentials;
//! 2. an IP literal is classified directly; a name is resolved once, and the
//!    fetch is refused when ANY answer is loopback, link-local, cloud
//!    metadata, private, unspecified, multicast or reserved;
//! 3. the client is pinned to the vetted addresses (`resolve_to_addrs`), with
//!    no proxy and no automatic redirects, so nothing re-resolves the name;
//! 4. redirects are followed by hand up to `max_redirects`, each target going
//!    back through steps 1-3;
//! 5. the body is read in chunks and refused once it passes
//!    `max_response_bytes`. Response decompression is not compiled in (reqwest
//!    has no gzip/brotli/deflate feature), no `Accept-Encoding` is sent and a
//!    `Content-Encoding` body is returned as raw bytes, so the cap counts the
//!    bytes on the wire and a compression bomb cannot expand past it.
//!
//! Name resolution, connect, redirects and body all share one total deadline
//! (`request_timeout_secs`) and the caller's cancellation token.

use async_trait::async_trait;
use reqwest::Url;
use reqwest::header::{CONTENT_LENGTH, CONTENT_TYPE, LOCATION};
use rsi_common::egress_policy::{
    BlockedClass, EgressError, EgressMode, EgressPolicy, blocked_class,
};
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;
use tokio_util::sync::CancellationToken;

/// The DNS seam. Production resolves through the system resolver; tests inject
/// fixed answers so no real network is touched.
#[async_trait]
pub trait EgressResolver: Send + Sync {
    async fn resolve(&self, host: &str, port: u16) -> std::io::Result<Vec<IpAddr>>;
}

pub struct SystemResolver;

#[async_trait]
impl EgressResolver for SystemResolver {
    async fn resolve(&self, host: &str, port: u16) -> std::io::Result<Vec<IpAddr>> {
        Ok(tokio::net::lookup_host((host, port))
            .await?
            .map(|addr| addr.ip())
            .collect())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FetchedResponse {
    /// The URL that produced the body, after redirects.
    pub final_url: String,
    pub status: u16,
    pub content_type: Option<String>,
    pub redirects: u8,
    pub body: Vec<u8>,
}

pub struct EgressFetcher {
    policy: EgressPolicy,
    resolver: Arc<dyn EgressResolver>,
    /// Test seam: after a hop's addresses pass classification, connect here
    /// instead (a local test server standing in for a public host).
    #[cfg(test)]
    connect_override: Option<SocketAddr>,
}

enum Target {
    Ip(IpAddr),
    Domain(String),
}

/// The `url` crate has already normalised numeric hosts (`2130706433`,
/// `0x7f.1`, `[::ffff:127.0.0.1]`) to canonical IP text, so a host that parses
/// as an IP here is an IP literal and everything else is a name.
fn host_target(url: &Url) -> Option<Target> {
    let host = url.host_str()?;
    let bare = host.trim_start_matches('[').trim_end_matches(']');
    Some(match bare.parse::<IpAddr>() {
        Ok(ip) => Target::Ip(ip),
        Err(_) => Target::Domain(host.to_owned()),
    })
}

fn denied(message: String) -> EgressError {
    EgressError::Denied(message)
}

fn blocked_message(what: &str, class: BlockedClass) -> String {
    format!(
        "{what} is a {} address; Harness network tools reach public addresses only",
        class.as_str()
    )
}

impl EgressFetcher {
    #[must_use]
    pub fn new(policy: EgressPolicy) -> Self {
        Self::with_resolver(policy, Arc::new(SystemResolver))
    }

    #[must_use]
    pub fn with_resolver(policy: EgressPolicy, resolver: Arc<dyn EgressResolver>) -> Self {
        Self {
            policy,
            resolver,
            #[cfg(test)]
            connect_override: None,
        }
    }

    #[cfg(test)]
    fn with_connect_override(mut self, addr: SocketAddr) -> Self {
        self.connect_override = Some(addr);
        self
    }

    /// GET `raw_url` under the egress policy.
    ///
    /// # Errors
    /// `egress_denied` for a refused destination (including a redirect
    /// target), `egress_limit_exceeded` for the redirect, size or time caps,
    /// `egress_failed` for any other transport failure.
    pub async fn fetch(
        &self,
        raw_url: &str,
        cancel: &CancellationToken,
    ) -> Result<FetchedResponse, EgressError> {
        if self.policy.mode == EgressMode::Offline {
            return Err(denied(
                "network egress is offline for this session (harness_egress_mode=offline)".into(),
            ));
        }
        let mut url = Url::parse(raw_url)
            .map_err(|error| EgressError::Failed(format!("invalid URL {raw_url:?}: {error}")))?;
        let mut redirects: u8 = 0;
        let deadline = tokio::time::Instant::now()
            + Duration::from_secs(self.policy.request_timeout_secs.max(1));
        loop {
            let pinned = self.vet(&url, deadline, cancel).await?;
            let response = self.send(&url, pinned, deadline, cancel).await?;
            let status = response.status();
            if status.is_redirection()
                && let Some(location) = response.headers().get(LOCATION)
            {
                if redirects >= self.policy.max_redirects {
                    return Err(EgressError::LimitExceeded(format!(
                        "redirect limit of {} exceeded at {}",
                        self.policy.max_redirects, url
                    )));
                }
                redirects += 1;
                let location = location.to_str().map_err(|_| {
                    EgressError::Failed("redirect Location header is not valid text".into())
                })?;
                url = url.join(location).map_err(|error| {
                    EgressError::Failed(format!("invalid redirect target {location:?}: {error}"))
                })?;
                continue;
            }
            let content_type = response
                .headers()
                .get(CONTENT_TYPE)
                .and_then(|value| value.to_str().ok())
                .map(str::to_owned);
            let body = self.read_body(response, deadline, cancel).await?;
            return Ok(FetchedResponse {
                final_url: url.to_string(),
                status: status.as_u16(),
                content_type,
                redirects,
                body,
            });
        }
    }

    /// Vet one hop and return the addresses its connection is pinned to.
    ///
    /// Name resolution runs under the same total deadline and cancellation as
    /// connect, redirects and body, so a stalled resolver cannot outlive the
    /// advertised time limit.
    async fn vet(
        &self,
        url: &Url,
        deadline: tokio::time::Instant,
        cancel: &CancellationToken,
    ) -> Result<Vec<SocketAddr>, EgressError> {
        if !matches!(url.scheme(), "http" | "https") {
            return Err(denied(format!(
                "scheme {:?} is not allowed; only http and https",
                url.scheme()
            )));
        }
        if !url.username().is_empty() || url.password().is_some() {
            return Err(denied("URLs with embedded credentials are refused".into()));
        }
        let port = url
            .port_or_known_default()
            .ok_or_else(|| EgressError::Failed(format!("URL {url} has no usable port")))?;
        let addrs: Vec<IpAddr> = match host_target(url) {
            None => return Err(EgressError::Failed(format!("URL {url} has no host"))),
            Some(Target::Ip(ip)) => vec![ip],
            Some(Target::Domain(name)) => {
                let bare = name.trim_end_matches('.').to_ascii_lowercase();
                if bare == "localhost" || bare.ends_with(".localhost") {
                    return Err(denied(blocked_message(&name, BlockedClass::Loopback)));
                }
                let lookup = self.resolver.resolve(&name, port);
                let answers = tokio::select! {
                    biased;
                    () = cancel.cancelled() => {
                        return Err(EgressError::Failed("fetch cancelled".into()));
                    }
                    () = tokio::time::sleep_until(deadline) => return Err(self.timeout_error()),
                    answers = lookup => answers.map_err(|error| {
                        EgressError::Failed(format!("could not resolve {name}: {error}"))
                    })?,
                };
                if answers.is_empty() {
                    return Err(EgressError::Failed(format!("{name} did not resolve")));
                }
                answers
            }
        };
        for ip in &addrs {
            if let Some(class) = blocked_class(*ip) {
                return Err(denied(blocked_message(
                    &format!("{} ({ip})", url.host_str().unwrap_or_default()),
                    class,
                )));
            }
        }
        #[cfg(test)]
        if let Some(addr) = self.connect_override {
            return Ok(vec![addr]);
        }
        Ok(addrs
            .into_iter()
            .map(|ip| SocketAddr::new(ip, port))
            .collect())
    }

    async fn send(
        &self,
        url: &Url,
        pinned: Vec<SocketAddr>,
        deadline: tokio::time::Instant,
        cancel: &CancellationToken,
    ) -> Result<reqwest::Response, EgressError> {
        let mut builder = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .no_proxy()
            .pool_max_idle_per_host(0)
            .user_agent("rsi-harness-fetch");
        if let Some(Target::Domain(name)) = host_target(url) {
            builder = builder.resolve_to_addrs(&name, &pinned);
        }
        let client = builder
            .build()
            .map_err(|error| EgressError::Failed(format!("http client: {error}")))?;
        let request = client.get(url.clone()).send();
        tokio::select! {
            biased;
            () = cancel.cancelled() => Err(EgressError::Failed("fetch cancelled".into())),
            () = tokio::time::sleep_until(deadline) => Err(self.timeout_error()),
            response = request => response.map_err(|error| EgressError::Failed(
                format!("request to {url} failed: {}", error.without_url())
            )),
        }
    }

    fn timeout_error(&self) -> EgressError {
        EgressError::LimitExceeded(format!(
            "fetch exceeded the {} s time limit",
            self.policy.request_timeout_secs.max(1)
        ))
    }

    async fn read_body(
        &self,
        mut response: reqwest::Response,
        deadline: tokio::time::Instant,
        cancel: &CancellationToken,
    ) -> Result<Vec<u8>, EgressError> {
        let cap = self.policy.max_response_bytes;
        let too_big =
            || EgressError::LimitExceeded(format!("response body exceeds the {cap} byte limit"));
        if let Some(declared) = response
            .headers()
            .get(CONTENT_LENGTH)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.parse::<u64>().ok())
            && declared > cap
        {
            return Err(too_big());
        }
        let mut body = Vec::new();
        loop {
            let chunk = tokio::select! {
                biased;
                () = cancel.cancelled() => {
                    return Err(EgressError::Failed("fetch cancelled".into()));
                }
                () = tokio::time::sleep_until(deadline) => return Err(self.timeout_error()),
                chunk = response.chunk() => chunk.map_err(|error| {
                    EgressError::Failed(format!("reading response failed: {}", error.without_url()))
                })?,
            };
            let Some(chunk) = chunk else {
                return Ok(body);
            };
            if (body.len() as u64).saturating_add(chunk.len() as u64) > cap {
                return Err(too_big());
            }
            body.extend_from_slice(&chunk);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    /// Fixed DNS answers; an unknown name is an error, and every lookup is
    /// recorded so a test can prove a literal never reached the resolver.
    struct FixedResolver {
        answers: HashMap<&'static str, Vec<IpAddr>>,
        lookups: std::sync::Mutex<Vec<String>>,
    }

    impl FixedResolver {
        fn new(answers: &[(&'static str, &[&str])]) -> Arc<Self> {
            Arc::new(Self {
                answers: answers
                    .iter()
                    .map(|(name, ips)| (*name, ips.iter().map(|ip| ip.parse().unwrap()).collect()))
                    .collect(),
                lookups: std::sync::Mutex::new(Vec::new()),
            })
        }
    }

    #[async_trait]
    impl EgressResolver for FixedResolver {
        async fn resolve(&self, host: &str, _port: u16) -> std::io::Result<Vec<IpAddr>> {
            self.lookups.lock().unwrap().push(host.to_owned());
            self.answers
                .get(host)
                .cloned()
                .ok_or_else(|| std::io::Error::other("no such host"))
        }
    }

    fn resolver() -> Arc<FixedResolver> {
        FixedResolver::new(&[
            ("public.test", &["93.184.216.34"]),
            ("rebind.test", &["127.0.0.1"]),
            ("metadata.test", &["169.254.169.254"]),
            ("mixed.test", &["93.184.216.34", "10.0.0.5"]),
            ("v6private.test", &["fd12:3456::1"]),
        ])
    }

    /// Serve `respond(path)` for every connection until the test ends. The
    /// handler returns the whole raw HTTP response.
    async fn serve<F>(respond: F) -> SocketAddr
    where
        F: Fn(&str) -> String + Send + Sync + 'static,
    {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let respond = Arc::new(respond);
        tokio::spawn(async move {
            loop {
                let Ok((mut stream, _)) = listener.accept().await else {
                    return;
                };
                let respond = Arc::clone(&respond);
                tokio::spawn(async move {
                    let mut buf = vec![0u8; 8192];
                    let n = stream.read(&mut buf).await.unwrap_or(0);
                    let request = String::from_utf8_lossy(&buf[..n]).into_owned();
                    let path = request.split_whitespace().nth(1).unwrap_or("/").to_owned();
                    let _ = stream.write_all(respond(&path).as_bytes()).await;
                    let _ = stream.shutdown().await;
                });
            }
        });
        addr
    }

    fn ok(body: &str) -> String {
        format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )
    }

    fn redirect(location: &str) -> String {
        format!(
            "HTTP/1.1 302 Found\r\nLocation: {location}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
        )
    }

    fn fetcher(resolver: Arc<FixedResolver>) -> EgressFetcher {
        EgressFetcher::with_resolver(EgressPolicy::default(), resolver)
    }

    async fn fetch(fetcher: &EgressFetcher, url: &str) -> Result<FetchedResponse, EgressError> {
        fetcher.fetch(url, &CancellationToken::new()).await
    }

    fn assert_denied(result: Result<FetchedResponse, EgressError>, needle: &str) {
        match result {
            Err(EgressError::Denied(message)) => {
                assert!(message.contains(needle), "{message:?} lacks {needle:?}");
            }
            other => panic!("expected egress_denied ({needle}), got {other:?}"),
        }
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-02"))]
    #[tokio::test]
    async fn ip_literals_in_every_blocked_class_are_denied_without_a_lookup() {
        let resolver = resolver();
        let fetcher = fetcher(Arc::clone(&resolver));
        for (url, class) in [
            ("http://127.0.0.1/", "loopback"),
            ("http://127.0.0.1:8080/admin", "loopback"),
            ("http://[::1]/", "loopback"),
            ("http://[::ffff:127.0.0.1]/", "loopback"),
            ("http://2130706433/", "loopback"),
            ("http://0x7f.1/", "loopback"),
            ("http://169.254.169.254/latest/meta-data/", "cloud metadata"),
            ("http://[fd00:ec2::254]/", "cloud metadata"),
            ("http://169.254.10.10/", "link-local"),
            ("http://[fe80::1]/", "link-local"),
            ("http://10.0.0.1/", "private range"),
            ("http://172.16.5.5/", "private range"),
            ("http://192.168.1.1/", "private range"),
            ("http://100.64.0.1/", "private range"),
            ("http://[fc00::1]/", "private range"),
            ("http://0.0.0.0/", "unspecified"),
            ("https://224.0.0.1/", "multicast"),
        ] {
            assert_denied(fetch(&fetcher, url).await, class);
        }
        assert!(resolver.lookups.lock().unwrap().is_empty());
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-02"))]
    #[tokio::test]
    async fn localhost_names_and_bad_schemes_and_credentials_are_denied() {
        let fetcher = fetcher(resolver());
        assert_denied(fetch(&fetcher, "http://localhost:3000/").await, "loopback");
        assert_denied(fetch(&fetcher, "http://LOCALHOST./").await, "loopback");
        assert_denied(fetch(&fetcher, "http://app.localhost/").await, "loopback");
        assert_denied(fetch(&fetcher, "file:///etc/passwd").await, "scheme");
        assert_denied(fetch(&fetcher, "ftp://public.test/x").await, "scheme");
        assert_denied(
            fetch(&fetcher, "http://user:pw@public.test/").await,
            "credentials",
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-02"))]
    #[tokio::test]
    async fn a_hostname_resolving_to_a_blocked_address_is_denied() {
        let fetcher = fetcher(resolver());
        assert_denied(fetch(&fetcher, "http://rebind.test/").await, "loopback");
        assert_denied(
            fetch(&fetcher, "http://metadata.test/").await,
            "cloud metadata",
        );
        assert_denied(
            fetch(&fetcher, "http://v6private.test/").await,
            "private range",
        );
        // One private answer among public ones poisons the whole name.
        assert_denied(fetch(&fetcher, "http://mixed.test/").await, "private range");
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-02"))]
    #[tokio::test]
    async fn offline_mode_denies_even_a_public_destination() {
        let fetcher =
            EgressFetcher::with_resolver(EgressPolicy::for_mode(EgressMode::Offline), resolver());
        assert_denied(fetch(&fetcher, "http://public.test/").await, "offline");
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-02"))]
    #[tokio::test]
    async fn an_allowed_public_address_is_fetched() {
        let server = serve(|path| ok(&format!("hello from {path}"))).await;
        let fetcher = fetcher(resolver()).with_connect_override(server);
        let response = fetch(&fetcher, "http://public.test/page").await.unwrap();
        assert_eq!(response.status, 200);
        assert_eq!(response.body, b"hello from /page");
        assert_eq!(response.content_type.as_deref(), Some("text/plain"));
        assert_eq!(response.redirects, 0);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-02"))]
    #[tokio::test]
    async fn a_redirect_to_a_public_address_is_followed() {
        let server = serve(|path| match path {
            "/start" => redirect("/landing"),
            _ => ok("landed"),
        })
        .await;
        let fetcher = fetcher(resolver()).with_connect_override(server);
        let response = fetch(&fetcher, "http://public.test/start").await.unwrap();
        assert_eq!(response.body, b"landed");
        assert_eq!(response.redirects, 1);
        assert_eq!(response.final_url, "http://public.test/landing");
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-02"))]
    #[tokio::test]
    async fn a_redirect_to_a_blocked_address_is_denied() {
        for (target, class) in [
            ("http://127.0.0.1:1/secret", "loopback"),
            ("http://169.254.169.254/latest/meta-data/", "cloud metadata"),
            ("http://[::1]/", "loopback"),
            ("http://rebind.test/", "loopback"),
            ("//10.0.0.7/internal", "private range"),
        ] {
            let target = target.to_owned();
            let server = serve(move |_| redirect(&target)).await;
            let fetcher = fetcher(resolver()).with_connect_override(server);
            assert_denied(fetch(&fetcher, "http://public.test/").await, class);
        }
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-02"))]
    #[tokio::test]
    async fn redirect_and_size_caps_are_enforced_with_a_visible_error() {
        let looping = serve(|_| redirect("/again")).await;
        let mut policy = EgressPolicy::default();
        policy.max_redirects = 3;
        let fetcher =
            EgressFetcher::with_resolver(policy, resolver()).with_connect_override(looping);
        match fetch(&fetcher, "http://public.test/").await {
            Err(EgressError::LimitExceeded(message)) => {
                assert!(message.contains("redirect limit of 3"), "{message}");
            }
            other => panic!("expected the redirect cap, got {other:?}"),
        }

        let big = serve(|_| ok(&"x".repeat(2048))).await;
        let mut policy = EgressPolicy::default();
        policy.max_response_bytes = 1024;
        let fetcher = EgressFetcher::with_resolver(policy, resolver()).with_connect_override(big);
        let error = fetch(&fetcher, "http://public.test/").await.unwrap_err();
        assert_eq!(
            error.code(),
            rsi_common::egress_policy::EGRESS_LIMIT_EXCEEDED
        );
        assert!(error.to_string().contains("1024 byte limit"), "{error}");
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-02"))]
    #[tokio::test]
    async fn an_undeclared_length_body_is_capped_while_streaming() {
        // No Content-Length: the cap must trip on the bytes actually read.
        let server = serve(|_| {
            format!(
                "HTTP/1.1 200 OK\r\nConnection: close\r\n\r\n{}",
                "y".repeat(4096)
            )
        })
        .await;
        let mut policy = EgressPolicy::default();
        policy.max_response_bytes = 1000;
        let fetcher =
            EgressFetcher::with_resolver(policy, resolver()).with_connect_override(server);
        let error = fetch(&fetcher, "http://public.test/").await.unwrap_err();
        assert!(matches!(error, EgressError::LimitExceeded(_)), "{error:?}");
    }

    /// A resolver that never answers for `hang.test` (a stalled DNS server).
    struct StalledResolver {
        inner: Arc<FixedResolver>,
    }

    #[async_trait]
    impl EgressResolver for StalledResolver {
        async fn resolve(&self, host: &str, port: u16) -> std::io::Result<Vec<IpAddr>> {
            if host == "hang.test" {
                std::future::pending::<()>().await;
            }
            self.inner.resolve(host, port).await
        }
    }

    fn stalled_fetcher(server: Option<SocketAddr>) -> EgressFetcher {
        let mut policy = EgressPolicy::default();
        policy.request_timeout_secs = 1;
        let fetcher =
            EgressFetcher::with_resolver(policy, Arc::new(StalledResolver { inner: resolver() }));
        match server {
            Some(server) => fetcher.with_connect_override(server),
            None => fetcher,
        }
    }

    fn assert_time_limit(
        result: Result<FetchedResponse, EgressError>,
        started: std::time::Instant,
    ) {
        match result {
            Err(EgressError::LimitExceeded(message)) => {
                assert!(message.contains("1 s time limit"), "{message}");
            }
            other => panic!("expected the visible time limit, got {other:?}"),
        }
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "the resolver stall outlived the deadline: {:?}",
            started.elapsed()
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-02"))]
    #[tokio::test]
    async fn a_resolver_that_never_answers_hits_the_time_limit_on_the_first_hop() {
        let started = std::time::Instant::now();
        let result = fetch(&stalled_fetcher(None), "http://hang.test/").await;
        assert_time_limit(result, started);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-02"))]
    #[tokio::test]
    async fn a_resolver_that_never_answers_hits_the_time_limit_on_a_redirect_hop() {
        let server = serve(|path| match path {
            "/start" => redirect("http://hang.test/next"),
            _ => ok("unreachable"),
        })
        .await;
        let started = std::time::Instant::now();
        let result = fetch(&stalled_fetcher(Some(server)), "http://public.test/start").await;
        assert_time_limit(result, started);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-02"))]
    #[tokio::test]
    async fn cancellation_interrupts_a_stalled_resolver() {
        let cancel = CancellationToken::new();
        let trigger = cancel.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(50)).await;
            trigger.cancel();
        });
        let error = stalled_fetcher(None)
            .fetch("http://hang.test/", &cancel)
            .await
            .unwrap_err();
        assert_eq!(error, EgressError::Failed("fetch cancelled".into()));
    }

    /// A server that records the raw request and answers with `response`.
    async fn serve_capturing(response: String) -> (SocketAddr, Arc<std::sync::Mutex<String>>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let seen = Arc::new(std::sync::Mutex::new(String::new()));
        let record = Arc::clone(&seen);
        tokio::spawn(async move {
            loop {
                let Ok((mut stream, _)) = listener.accept().await else {
                    return;
                };
                let mut buf = vec![0u8; 8192];
                let n = stream.read(&mut buf).await.unwrap_or(0);
                *record.lock().unwrap() = String::from_utf8_lossy(&buf[..n]).into_owned();
                let _ = stream.write_all(response.as_bytes()).await;
                let _ = stream.shutdown().await;
            }
        });
        (addr, seen)
    }

    /// Decompression is disabled: no `Accept-Encoding` is offered and a
    /// `Content-Encoding` body comes back as the raw bytes on the wire, which
    /// is what the byte cap counts.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-02"))]
    #[tokio::test]
    async fn compressed_responses_are_not_decoded_and_the_cap_counts_wire_bytes() {
        let raw = "not-actually-gzip-bytes";
        let (server, seen) = serve_capturing(format!(
            "HTTP/1.1 200 OK\r\nContent-Encoding: gzip\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{raw}",
            raw.len()
        ))
        .await;
        let response = fetch(
            &fetcher(resolver()).with_connect_override(server),
            "http://public.test/",
        )
        .await
        .unwrap();
        assert_eq!(response.body, raw.as_bytes());
        assert!(
            !seen
                .lock()
                .unwrap()
                .to_ascii_lowercase()
                .contains("accept-encoding"),
            "{:?}",
            seen.lock().unwrap()
        );

        // A compressed body over the cap is refused on its wire size, with or
        // without a declared length.
        for headers in ["Content-Length: 4096\r\n", ""] {
            let (server, _) = serve_capturing(format!(
                "HTTP/1.1 200 OK\r\nContent-Encoding: gzip\r\n{headers}Connection: close\r\n\r\n{}",
                "z".repeat(4096)
            ))
            .await;
            let mut policy = EgressPolicy::default();
            policy.max_response_bytes = 1000;
            let fetcher =
                EgressFetcher::with_resolver(policy, resolver()).with_connect_override(server);
            let error = fetch(&fetcher, "http://public.test/").await.unwrap_err();
            assert!(
                matches!(&error, EgressError::LimitExceeded(m) if m.contains("1000 byte limit")),
                "{error:?}"
            );
        }
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-02"))]
    #[tokio::test]
    async fn the_policy_is_reachable_from_the_tool_context() {
        use crate::session::harness::tools::ToolPolicy;
        use rsi_common::harness_tool_policy::HarnessToolPolicy;
        let offline =
            crate::session::harness::tools::policy::ToolPolicyRuntime::new(HarnessToolPolicy {
                egress: Some(EgressMode::Offline),
                ..HarnessToolPolicy::default()
            });
        assert_eq!(offline.egress_policy().mode, EgressMode::Offline);
        assert_eq!(ToolPolicy::default().egress.mode, EgressMode::DenyPrivate);
        let unset = crate::session::harness::tools::policy::ToolPolicyRuntime::new(
            HarnessToolPolicy::default(),
        );
        assert_eq!(unset.egress_policy().mode, EgressMode::DenyPrivate);
    }
}
