//! Bounded, read-only HTTP/1.1 client for the Tailscale `LocalAPI` Unix socket.
//!
//! Callers receive raw response-body bytes and hand them to the pure decoders in
//! [`crate::localapi`]. Per ADR D4a this speaks only three fixed read-only
//! `GET`s; there is intentionally no public generic path method.

use std::net::IpAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;
use tokio::sync::Semaphore;

/// Default Tailscale `LocalAPI` Unix socket path.
pub const DEFAULT_SOCKET: &str = "/var/run/tailscale/tailscaled.sock";

/// Maximum header block size, including the terminating CRLFCRLF.
const MAX_HEADERS: usize = 16 * 1024;
/// Maximum decoded body size.
const MAX_BODY: usize = 1024 * 1024;
/// Maximum length of a chunk-size line: 32-byte token plus `;` plus 64-byte extension.
const CHUNK_LINE_MAX: usize = 32 + 1 + 64;
/// Maximum JSON nesting depth.
const MAX_DEPTH: usize = 32;
/// Maximum number of JSON values counted, including object members.
const MAX_NODES: usize = 16_384;
/// Maximum concurrent calls.
const PERMITS: usize = 4;

/// Why a `LocalAPI` call failed. Codes are stable `snake_case` strings.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LocalApiError {
    /// All four permits were already held; the call never waited.
    Busy,
    /// The deadline elapsed before connect, write and full read completed.
    Timeout,
    /// The Unix socket could not be connected to or written.
    Connect,
    /// The server answered `403 Forbidden`.
    PermissionDenied,
    /// The server answered `404 Not Found`.
    NotFound,
    /// The server answered a `3xx` redirect, which is never followed.
    Redirect,
    /// Any other non-`200` status.
    Status(u16),
    /// A header, body or chunked-body cap was exceeded.
    Oversize,
    /// The response framing or body was not valid HTTP or JSON.
    Malformed,
    /// Parsed JSON exceeded the depth or node bound.
    TooDeep,
}

impl LocalApiError {
    /// Stable sanitized code for diagnostics.
    #[must_use]
    pub const fn code(self) -> &'static str {
        match self {
            Self::Busy => "localapi_busy",
            Self::Timeout => "localapi_timeout",
            Self::Connect => "localapi_connect",
            Self::PermissionDenied => "localapi_permission_denied",
            Self::NotFound => "localapi_not_found",
            Self::Redirect => "localapi_redirect",
            Self::Status(_) => "localapi_status",
            Self::Oversize => "localapi_oversize",
            Self::Malformed => "localapi_malformed",
            Self::TooDeep => "localapi_too_deep",
        }
    }
}

/// A bounded read-only `LocalAPI` client.
pub struct LocalApiClient {
    socket: PathBuf,
    deadline: Duration,
    permits: Arc<Semaphore>,
}

impl LocalApiClient {
    /// Create a client for `socket` with a one-second deadline and four permits.
    #[must_use]
    pub fn new(socket: impl Into<PathBuf>) -> Self {
        Self {
            socket: socket.into(),
            deadline: Duration::from_secs(1),
            permits: Arc::new(Semaphore::new(PERMITS)),
        }
    }

    /// Override the per-call deadline.
    #[must_use]
    pub const fn with_deadline(mut self, deadline: Duration) -> Self {
        self.deadline = deadline;
        self
    }

    /// `GET /localapi/v0/status?peers=false`, returning the raw body bytes.
    ///
    /// # Errors
    ///
    /// Returns [`LocalApiError`] for busy, timeout, connect, HTTP status,
    /// oversize, malformed and JSON-bound failures.
    pub async fn status(&self) -> Result<Vec<u8>, LocalApiError> {
        self.get("/localapi/v0/status?peers=false").await
    }

    /// `GET /localapi/v0/serve-config`, returning the raw body bytes.
    ///
    /// # Errors
    ///
    /// Returns [`LocalApiError`] for busy, timeout, connect, HTTP status,
    /// oversize, malformed and JSON-bound failures.
    pub async fn serve_config(&self) -> Result<Vec<u8>, LocalApiError> {
        self.get("/localapi/v0/serve-config").await
    }

    /// `GET /localapi/v0/whois?addr=<addr>&proto=tcp`, returning the raw body
    /// bytes. IPv6 addresses are canonical and bracket-free with `:` written as
    /// `%3A`.
    ///
    /// # Errors
    ///
    /// Returns [`LocalApiError`] for busy, timeout, connect, HTTP status,
    /// oversize, malformed and JSON-bound failures.
    pub async fn whois(&self, addr: IpAddr) -> Result<Vec<u8>, LocalApiError> {
        let encoded = addr.to_string().replace(':', "%3A");
        self.get(&format!("/localapi/v0/whois?addr={encoded}&proto=tcp"))
            .await
    }

    async fn get(&self, target: &str) -> Result<Vec<u8>, LocalApiError> {
        let _permit = self
            .permits
            .try_acquire()
            .map_err(|_| LocalApiError::Busy)?;
        tokio::time::timeout(self.deadline, self.request(target))
            .await
            .map_err(|_| LocalApiError::Timeout)?
    }

    async fn request(&self, target: &str) -> Result<Vec<u8>, LocalApiError> {
        let mut stream = UnixStream::connect(&self.socket)
            .await
            .map_err(|_| LocalApiError::Connect)?;
        let request = format!(
            "GET {target} HTTP/1.1\r\nHost: local-tailscaled.sock\r\nConnection: close\r\n\r\n"
        );
        stream
            .write_all(request.as_bytes())
            .await
            .map_err(|_| LocalApiError::Connect)?;
        stream.flush().await.map_err(|_| LocalApiError::Connect)?;
        let mut connection = Connection::new(stream);
        connection.read_response().await
    }
}

/// Incremental reader over one connection, keeping unconsumed bytes buffered.
struct Connection {
    stream: UnixStream,
    buf: Vec<u8>,
    pos: usize,
}

impl Connection {
    const fn new(stream: UnixStream) -> Self {
        Self {
            stream,
            buf: Vec::new(),
            pos: 0,
        }
    }

    fn available(&self) -> &[u8] {
        &self.buf[self.pos..]
    }

    /// Pull more bytes from the socket; `false` means EOF.
    async fn fill(&mut self) -> Result<bool, LocalApiError> {
        let mut chunk = [0u8; 8192];
        let read = self
            .stream
            .read(&mut chunk)
            .await
            .map_err(|_| LocalApiError::Malformed)?;
        if read == 0 {
            return Ok(false);
        }
        self.buf.extend_from_slice(&chunk[..read]);
        Ok(true)
    }

    /// Ensure `needed` buffered bytes exist; `false` means EOF first.
    async fn ensure(&mut self, needed: usize) -> Result<bool, LocalApiError> {
        while self.available().len() < needed {
            if !self.fill().await? {
                return Ok(false);
            }
        }
        Ok(true)
    }

    async fn read_headers(&mut self) -> Result<Vec<u8>, LocalApiError> {
        loop {
            if let Some(index) = find(self.available(), b"\r\n\r\n") {
                let end = index + 4;
                if end > MAX_HEADERS {
                    return Err(LocalApiError::Oversize);
                }
                let block = self.available()[..end].to_vec();
                self.pos += end;
                return Ok(block);
            }
            if self.available().len() >= MAX_HEADERS {
                return Err(LocalApiError::Oversize);
            }
            if !self.fill().await? {
                return Err(LocalApiError::Malformed);
            }
        }
    }

    async fn read_exact_bytes(&mut self, count: usize) -> Result<Vec<u8>, LocalApiError> {
        if !self.ensure(count).await? {
            return Err(LocalApiError::Malformed);
        }
        let bytes = self.available()[..count].to_vec();
        self.pos += count;
        Ok(bytes)
    }

    async fn read_line(&mut self, max: usize) -> Result<Vec<u8>, LocalApiError> {
        loop {
            if let Some(index) = find(self.available(), b"\r\n") {
                if index > max {
                    return Err(LocalApiError::Malformed);
                }
                let line = self.available()[..index].to_vec();
                self.pos += index + 2;
                return Ok(line);
            }
            if self.available().len() > max {
                return Err(LocalApiError::Malformed);
            }
            if !self.fill().await? {
                return Err(LocalApiError::Malformed);
            }
        }
    }

    async fn read_to_eof(&mut self, cap: usize) -> Result<Vec<u8>, LocalApiError> {
        let mut body = Vec::new();
        loop {
            if !self.available().is_empty() {
                let pending = self.available().len();
                if body.len() + pending > cap {
                    return Err(LocalApiError::Oversize);
                }
                body.extend_from_slice(self.available());
                self.pos = self.buf.len();
            }
            if !self.fill().await? {
                break;
            }
        }
        Ok(body)
    }

    async fn read_chunked(&mut self) -> Result<Vec<u8>, LocalApiError> {
        let mut body = Vec::new();
        loop {
            let line = self.read_line(CHUNK_LINE_MAX).await?;
            let (token, extension): (&[u8], &[u8]) = line
                .iter()
                .position(|&byte| byte == b';')
                .map_or((line.as_slice(), &[]), |index| {
                    (&line[..index], &line[index + 1..])
                });
            if extension.len() > 64 || token.is_empty() || token.len() > 32 {
                return Err(LocalApiError::Malformed);
            }
            let mut size = 0usize;
            for &byte in token {
                let digit = match byte {
                    b'0'..=b'9' => byte - b'0',
                    b'a'..=b'f' => byte - b'a' + 10,
                    b'A'..=b'F' => byte - b'A' + 10,
                    _ => return Err(LocalApiError::Malformed),
                };
                size = size
                    .checked_mul(16)
                    .and_then(|value| value.checked_add(usize::from(digit)))
                    .ok_or(LocalApiError::Malformed)?;
            }
            if size == 0 {
                let mut trailers = 0usize;
                loop {
                    let trailer = self.read_line(MAX_HEADERS).await?;
                    if trailer.is_empty() {
                        break;
                    }
                    trailers += trailer.len() + 2;
                    if trailers > MAX_HEADERS {
                        return Err(LocalApiError::Oversize);
                    }
                }
                return Ok(body);
            }
            // `body.len() <= MAX_BODY` holds here, so this cannot overflow.
            if size > MAX_BODY - body.len() {
                return Err(LocalApiError::Oversize);
            }
            let data = self.read_exact_bytes(size).await?;
            body.extend_from_slice(&data);
            if self.read_exact_bytes(2).await?.as_slice() != b"\r\n" {
                return Err(LocalApiError::Malformed);
            }
        }
    }

    async fn read_response(&mut self) -> Result<Vec<u8>, LocalApiError> {
        let header = self.read_headers().await?;
        if !header.is_ascii() {
            return Err(LocalApiError::Malformed);
        }
        let text = std::str::from_utf8(&header).map_err(|_| LocalApiError::Malformed)?;
        let mut lines = text.split("\r\n");
        let status_line = lines.next().ok_or(LocalApiError::Malformed)?;
        let code = parse_status(status_line)?;
        let mut content_length: Option<usize> = None;
        let mut chunked = false;
        for line in lines {
            if line.is_empty() {
                continue;
            }
            let (name, value) = line.split_once(':').ok_or(LocalApiError::Malformed)?;
            let name = name.trim();
            let value = value.trim();
            if name.eq_ignore_ascii_case("content-length") {
                if content_length.is_some() {
                    return Err(LocalApiError::Malformed);
                }
                content_length = Some(value.parse().map_err(|_| LocalApiError::Malformed)?);
            } else if name.eq_ignore_ascii_case("transfer-encoding") {
                // Only a single plain `chunked` coding is understood.
                if chunked || !value.eq_ignore_ascii_case("chunked") {
                    return Err(LocalApiError::Malformed);
                }
                chunked = true;
            }
        }
        match code {
            200 => {}
            300..=399 => return Err(LocalApiError::Redirect),
            403 => return Err(LocalApiError::PermissionDenied),
            404 => return Err(LocalApiError::NotFound),
            other => return Err(LocalApiError::Status(other)),
        }
        let body = if chunked {
            self.read_chunked().await?
        } else if let Some(length) = content_length {
            if length > MAX_BODY {
                return Err(LocalApiError::Oversize);
            }
            self.read_exact_bytes(length).await?
        } else {
            self.read_to_eof(MAX_BODY).await?
        };
        check_json(&body)?;
        Ok(body)
    }
}

/// Parse `HTTP/1.1 <3 digits> ...` (or `HTTP/1.0`), returning the status code.
fn parse_status(line: &str) -> Result<u16, LocalApiError> {
    let rest = line
        .strip_prefix("HTTP/1.1 ")
        .or_else(|| line.strip_prefix("HTTP/1.0 "))
        .ok_or(LocalApiError::Malformed)?;
    let digits = rest.get(..3).ok_or(LocalApiError::Malformed)?;
    if !digits.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(LocalApiError::Malformed);
    }
    digits.parse().map_err(|_| LocalApiError::Malformed)
}

/// Find the first occurrence of `needle` in `haystack`.
fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

/// Validate JSON bounds without retaining the parsed value.
fn check_json(bytes: &[u8]) -> Result<(), LocalApiError> {
    let value: serde_json::Value =
        serde_json::from_slice(bytes).map_err(|_| LocalApiError::Malformed)?;
    let mut stack = vec![(1usize, &value)];
    let mut nodes = 0usize;
    while let Some((depth, current)) = stack.pop() {
        nodes += 1;
        if nodes > MAX_NODES || depth > MAX_DEPTH {
            return Err(LocalApiError::TooDeep);
        }
        match current {
            serde_json::Value::Array(items) => {
                for item in items {
                    stack.push((depth + 1, item));
                }
            }
            serde_json::Value::Object(map) => {
                for item in map.values() {
                    stack.push((depth + 1, item));
                }
            }
            _ => {}
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::{UnixListener, UnixStream};

    const STATUS_BODY: &[u8] = br#"{"BackendState":"Running"}"#;

    const STATUS_REQUEST: &[u8] =
        b"GET /localapi/v0/status?peers=false HTTP/1.1\r\nHost: local-tailscaled.sock\r\nConnection: close\r\n\r\n";
    const SERVE_REQUEST: &[u8] =
        b"GET /localapi/v0/serve-config HTTP/1.1\r\nHost: local-tailscaled.sock\r\nConnection: close\r\n\r\n";

    fn body_response(body: &[u8]) -> Vec<u8> {
        let mut out =
            format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n", body.len()).into_bytes();
        out.extend_from_slice(body);
        out
    }

    async fn read_request(stream: &mut UnixStream) -> Vec<u8> {
        let mut request = Vec::new();
        let mut chunk = [0u8; 512];
        loop {
            let read = stream.read(&mut chunk).await.unwrap();
            if read == 0 {
                break;
            }
            request.extend_from_slice(&chunk[..read]);
            if request.ends_with(b"\r\n\r\n") {
                break;
            }
        }
        request
    }

    /// Bind a one-shot responder; returns the socket path and the request it saw.
    fn responder(
        dir: &tempfile::TempDir,
        response: Vec<u8>,
    ) -> (PathBuf, tokio::task::JoinHandle<Vec<u8>>) {
        let path = dir.path().join("sock");
        let listener = UnixListener::bind(&path).unwrap();
        let handle = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let request = read_request(&mut stream).await;
            let _ = stream.write_all(&response).await;
            let _ = stream.flush().await;
            request
        });
        (path, handle)
    }

    #[test]
    fn error_codes_are_stable() {
        assert_eq!(LocalApiError::Busy.code(), "localapi_busy");
        assert_eq!(LocalApiError::Timeout.code(), "localapi_timeout");
        assert_eq!(LocalApiError::Connect.code(), "localapi_connect");
        assert_eq!(
            LocalApiError::PermissionDenied.code(),
            "localapi_permission_denied"
        );
        assert_eq!(LocalApiError::NotFound.code(), "localapi_not_found");
        assert_eq!(LocalApiError::Redirect.code(), "localapi_redirect");
        assert_eq!(LocalApiError::Status(500).code(), "localapi_status");
        assert_eq!(LocalApiError::Oversize.code(), "localapi_oversize");
        assert_eq!(LocalApiError::Malformed.code(), "localapi_malformed");
        assert_eq!(LocalApiError::TooDeep.code(), "localapi_too_deep");
    }

    #[tokio::test]
    async fn status_content_length_returns_bytes_and_exact_request() {
        let dir = tempdir().unwrap();
        let (path, handle) = responder(&dir, body_response(STATUS_BODY));
        let client = LocalApiClient::new(&path);
        let body = client.status().await.unwrap();
        assert_eq!(body, STATUS_BODY);
        assert_eq!(handle.await.unwrap(), STATUS_REQUEST);
    }

    #[tokio::test]
    async fn serve_config_returns_bytes_and_exact_request() {
        let dir = tempdir().unwrap();
        let (path, handle) = responder(&dir, body_response(STATUS_BODY));
        let client = LocalApiClient::new(&path);
        let body = client.serve_config().await.unwrap();
        assert_eq!(body, STATUS_BODY);
        assert_eq!(handle.await.unwrap(), SERVE_REQUEST);
    }

    #[tokio::test]
    async fn whois_ipv6_is_canonical_and_percent_encoded() {
        let dir = tempdir().unwrap();
        let (path, handle) = responder(&dir, body_response(STATUS_BODY));
        let addr: IpAddr = "fd7a:115c:a1e0::1".parse().unwrap();
        let client = LocalApiClient::new(&path);
        let body = client.whois(addr).await.unwrap();
        assert_eq!(body, STATUS_BODY);
        let expected = format!(
            "GET /localapi/v0/whois?addr={}&proto=tcp HTTP/1.1\r\nHost: local-tailscaled.sock\r\nConnection: close\r\n\r\n",
            addr.to_string().replace(':', "%3A")
        );
        let request = handle.await.unwrap();
        assert_eq!(request, expected.as_bytes());
        assert!(request.windows(3).any(|w| w == b"%3A"));
    }

    #[tokio::test]
    async fn whois_ipv4_has_no_encoding() {
        let dir = tempdir().unwrap();
        let (path, handle) = responder(&dir, body_response(STATUS_BODY));
        let addr: IpAddr = "100.101.102.103".parse().unwrap();
        let client = LocalApiClient::new(&path);
        client.whois(addr).await.unwrap();
        assert_eq!(
            handle.await.unwrap(),
            b"GET /localapi/v0/whois?addr=100.101.102.103&proto=tcp HTTP/1.1\r\nHost: local-tailscaled.sock\r\nConnection: close\r\n\r\n".to_vec()
        );
    }

    #[tokio::test]
    async fn chunked_multi_chunk_body_is_decoded() {
        let dir = tempdir().unwrap();
        let response = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n5\r\n{\"a\":\r\n2\r\n1}\r\n0\r\n\r\n".to_vec();
        let (path, _handle) = responder(&dir, response);
        let client = LocalApiClient::new(&path);
        assert_eq!(client.status().await.unwrap(), br#"{"a":1}"#.to_vec());
    }

    #[tokio::test]
    async fn content_length_over_one_mib_is_oversize() {
        let dir = tempdir().unwrap();
        let (path, _handle) = responder(
            &dir,
            b"HTTP/1.1 200 OK\r\nContent-Length: 1048577\r\n\r\n".to_vec(),
        );
        let client = LocalApiClient::new(&path);
        assert_eq!(client.status().await, Err(LocalApiError::Oversize));
    }

    #[tokio::test]
    async fn chunked_body_over_one_mib_is_oversize() {
        let dir = tempdir().unwrap();
        let (path, _handle) = responder(
            &dir,
            b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n100001\r\n".to_vec(),
        );
        let client = LocalApiClient::new(&path);
        assert_eq!(client.status().await, Err(LocalApiError::Oversize));
    }

    #[tokio::test]
    async fn header_block_over_16_kib_is_oversize() {
        let dir = tempdir().unwrap();
        let response = format!("HTTP/1.1 200 OK\r\nX-Pad: {}\r\n\r\n", "a".repeat(20_000));
        let (path, _handle) = responder(&dir, response.into_bytes());
        let client = LocalApiClient::new(&path);
        assert_eq!(client.status().await, Err(LocalApiError::Oversize));
    }

    #[tokio::test]
    async fn forbidden_is_permission_denied() {
        let dir = tempdir().unwrap();
        let (path, _handle) = responder(
            &dir,
            b"HTTP/1.1 403 Forbidden\r\nContent-Length: 0\r\n\r\n".to_vec(),
        );
        let client = LocalApiClient::new(&path);
        assert_eq!(client.status().await, Err(LocalApiError::PermissionDenied));
    }

    #[tokio::test]
    async fn not_found_is_not_found() {
        let dir = tempdir().unwrap();
        let (path, _handle) = responder(
            &dir,
            b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n".to_vec(),
        );
        let client = LocalApiClient::new(&path);
        assert_eq!(
            client.whois("100.101.102.103".parse().unwrap()).await,
            Err(LocalApiError::NotFound)
        );
    }

    #[tokio::test]
    async fn redirect_is_rejected() {
        let dir = tempdir().unwrap();
        let (path, _handle) = responder(
            &dir,
            b"HTTP/1.1 302 Found\r\nLocation: /x\r\nContent-Length: 0\r\n\r\n".to_vec(),
        );
        let client = LocalApiClient::new(&path);
        assert_eq!(client.status().await, Err(LocalApiError::Redirect));
    }

    #[tokio::test]
    async fn other_status_is_reported() {
        let dir = tempdir().unwrap();
        let (path, _handle) = responder(
            &dir,
            b"HTTP/1.1 500 Oops\r\nContent-Length: 0\r\n\r\n".to_vec(),
        );
        let client = LocalApiClient::new(&path);
        assert_eq!(client.status().await, Err(LocalApiError::Status(500)));
    }

    #[tokio::test]
    async fn silent_server_times_out() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("sock");
        let listener = UnixListener::bind(&path).unwrap();
        let server = tokio::spawn(async move {
            let (_stream, _) = listener.accept().await.unwrap();
            tokio::time::sleep(Duration::from_secs(30)).await;
        });
        let client = LocalApiClient::new(&path).with_deadline(Duration::from_millis(100));
        assert_eq!(client.status().await, Err(LocalApiError::Timeout));
        server.abort();
    }

    #[tokio::test]
    async fn missing_socket_is_connect() {
        let dir = tempdir().unwrap();
        let client = LocalApiClient::new(dir.path().join("absent.sock"));
        assert_eq!(client.status().await, Err(LocalApiError::Connect));
    }

    #[tokio::test]
    async fn malformed_chunk_size_is_malformed() {
        let dir = tempdir().unwrap();
        let (path, _handle) = responder(
            &dir,
            b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\nzz\r\n".to_vec(),
        );
        let client = LocalApiClient::new(&path);
        assert_eq!(client.status().await, Err(LocalApiError::Malformed));
    }

    #[tokio::test]
    async fn json_nested_33_is_too_deep() {
        let dir = tempdir().unwrap();
        let body = format!("{}1{}", "[".repeat(33), "]".repeat(33));
        let (path, _handle) = responder(&dir, body_response(body.as_bytes()));
        let client = LocalApiClient::new(&path);
        assert_eq!(client.status().await, Err(LocalApiError::TooDeep));
    }

    #[tokio::test]
    async fn json_with_16385_nodes_is_too_deep() {
        let dir = tempdir().unwrap();
        let mut body = String::from("[0");
        for _ in 1..16_384 {
            body.push_str(",0");
        }
        body.push(']');
        let (path, _handle) = responder(&dir, body_response(body.as_bytes()));
        let client = LocalApiClient::new(&path);
        assert_eq!(client.status().await, Err(LocalApiError::TooDeep));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn fifth_concurrent_call_is_busy() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("sock");
        let listener = UnixListener::bind(&path).unwrap();
        let server = tokio::spawn(async move {
            let mut held = Vec::new();
            while let Ok((stream, _)) = listener.accept().await {
                held.push(stream);
            }
        });
        let client = LocalApiClient::new(&path).with_deadline(Duration::from_millis(300));
        let (a, b, c, d, e) = tokio::join!(
            client.status(),
            client.status(),
            client.status(),
            client.status(),
            client.status()
        );
        let results = [a, b, c, d, e];
        let busy = results
            .iter()
            .filter(|result| **result == Err(LocalApiError::Busy))
            .count();
        assert!(busy >= 1, "expected at least one Busy in {results:?}");
        server.abort();
    }

    #[tokio::test]
    async fn huge_second_chunk_size_is_oversize_not_overflow() {
        let dir = tempdir().unwrap();
        let response =
            b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n2\r\n{}\r\nffffffffffffffff\r\n"
                .to_vec();
        let (path, _handle) = responder(&dir, response);
        let client = LocalApiClient::new(&path);
        assert_eq!(client.status().await, Err(LocalApiError::Oversize));
    }

    #[tokio::test]
    async fn unsupported_transfer_encoding_is_malformed() {
        for coding in ["gzip, chunked", "identity"] {
            let dir = tempdir().unwrap();
            let response = format!(
                "HTTP/1.1 200 OK\r\nTransfer-Encoding: {coding}\r\nContent-Length: 2\r\n\r\n{{}}"
            )
            .into_bytes();
            let (path, _handle) = responder(&dir, response);
            let client = LocalApiClient::new(&path);
            assert_eq!(client.status().await, Err(LocalApiError::Malformed));
        }
    }
}
