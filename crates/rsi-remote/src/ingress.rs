use std::{collections::HashMap, net::IpAddr};

/// HTTP method of an allowed request line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Method {
    Get,
    Post,
}

/// The one optional query string a V1 GET may carry. Rejected shapes never
/// produce this struct; see [`parse_request`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Query {
    pub cursor: Option<String>,
    pub before: Option<(i32, i64)>,
}

/// A fully validated gateway request. Every field was checked by
/// [`parse_request`] before this struct exists.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Request {
    pub method: Method,
    /// The path with any query string removed.
    pub path: String,
    pub query: Query,
    pub source: IpAddr,
    pub content_length: usize,
    pub origin: Option<String>,
    pub sec_fetch_site: Option<String>,
    pub cookie: Option<String>,
    /// The `x-rsi-csrf` header, if present.
    pub csrf: Option<String>,
}

/// Parsing is deliberately narrower than HTTP. A future data path must retain
/// these checks and also establish `LocalAPI` and application-session identity.
///
/// `raw_headers` must end at the header terminator (`\r\n\r\n`); body bytes are
/// read separately by the caller using [`Request::content_length`].
///
/// # Errors
///
/// Returns a stable reason string when the request line, headers, forwarding
/// envelope, source tailnet address or method-specific content rules deny.
pub fn parse_request(raw_headers: &[u8], canonical_host: &str) -> Result<Request, &'static str> {
    if raw_headers.len() > 16 * 1024 || !raw_headers.ends_with(b"\r\n\r\n") {
        return Err("invalid headers");
    }
    let text = std::str::from_utf8(raw_headers).map_err(|_| "invalid headers")?;
    let header_block = text.strip_suffix("\r\n\r\n").ok_or("invalid headers")?;
    if header_block.contains("\r\n\r\n") {
        return Err("trailing request bytes");
    }
    let mut lines = header_block.split("\r\n");
    let line = lines.next().ok_or("invalid request line")?;
    if line.len() > 2048 {
        return Err("invalid request line");
    }
    let parts: Vec<_> = line.split(' ').collect();
    if parts.len() != 3 || parts[2] != "HTTP/1.1" {
        return Err("invalid request line");
    }
    let target = parts[1];
    let (path, query) = match target.split_once('?') {
        None => (target, Query::default()),
        Some((path, raw)) => {
            if !path.starts_with("/api/v1/") {
                return Err("invalid request line");
            }
            (path, parse_query(raw)?)
        }
    };
    if path.contains('?') || path.contains('%') || path.contains("..") {
        return Err("invalid request line");
    }
    let answer = answer_route(path);
    let method = match (parts[0], path) {
        ("GET", path) if allowed_get(path) && answer.is_none() => Method::Get,
        ("POST", "/auth/session" | "/auth/logout") => Method::Post,
        ("POST", _) if answer.is_some() => Method::Post,
        _ => return Err("invalid request line"),
    };
    // The answer POST and the decision-targets GET carry no query: a query on
    // either is refused before any header or body check.
    if query != Query::default()
        && (matches!(method, Method::Post)
            || read_method(path) == Some("RemoteGetDecisionTargetsV1"))
    {
        return Err("invalid request line");
    }
    let headers = parse_headers(lines)?;
    if headers.get("host") != Some(&"localhost")
        || headers.get("x-forwarded-proto") != Some(&"https")
        || headers.contains_key("forwarded")
        || !headers.contains_key("x-forwarded-host")
        || !matches!(headers.get("x-forwarded-host"), Some(value) if *value == canonical_host || *value == format!("{canonical_host}:443"))
        || headers.keys().any(|key| key.contains("funnel"))
        || headers.contains_key("transfer-encoding")
    {
        return Err("untrusted forwarding");
    }
    let content_length = content_length(&headers)?;
    if matches!(method, Method::Post) && (path == "/auth/session" || answer.is_some()) {
        if headers.get("content-type") != Some(&"application/json") {
            return Err("invalid content type");
        }
        if !(1..=4096).contains(&content_length) {
            return Err("invalid content-length");
        }
    } else if content_length != 0 {
        return Err("invalid content-length");
    }
    let source = headers.get("x-forwarded-for").ok_or("missing source")?;
    let ip: IpAddr = source.parse().map_err(|_| "invalid source")?;
    let tailnet = match ip {
        IpAddr::V4(v4) => {
            let octets = v4.octets();
            octets[0] == 100 && (64..=127).contains(&octets[1])
        }
        IpAddr::V6(v6) => {
            v6.segments()[..3] == [0xfd7a, 0x115c, 0xa1e0] && v6.to_ipv4_mapped().is_none()
        }
    };
    if ip.to_string() != *source || !tailnet {
        return Err("invalid source");
    }
    Ok(Request {
        method,
        path: path.to_string(),
        query,
        source: ip,
        content_length,
        origin: headers.get("origin").map(|value| (*value).to_string()),
        sec_fetch_site: headers
            .get("sec-fetch-site")
            .map(|value| (*value).to_string()),
        cookie: headers.get("cookie").map(|value| (*value).to_string()),
        csrf: headers.get("x-rsi-csrf").map(|value| (*value).to_string()),
    })
}

/// The V1 query grammar: `key=value` pairs joined by `&`, keys `cursor` and
/// `before` each at most once and never empty. Anything else (extra keys,
/// `%`/`+`/`;`, a repeated `?`, a duplicate key) is an invalid request line.
fn parse_query(raw: &str) -> Result<Query, &'static str> {
    if raw.is_empty() {
        return Err("invalid request line");
    }
    let mut query = Query::default();
    for pair in raw.split('&') {
        let (key, value) = pair.split_once('=').ok_or("invalid request line")?;
        match key {
            "cursor" => {
                if query.cursor.is_some() || !cursor_token(value) {
                    return Err("invalid request line");
                }
                query.cursor = Some(value.to_string());
            }
            "before" => {
                if query.before.is_some() {
                    return Err("invalid request line");
                }
                query.before = Some(parse_before(value)?);
            }
            _ => return Err("invalid request line"),
        }
    }
    Ok(query)
}

fn cursor_token(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 1024
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// `<sequence>:<id>` with a signed i32 sequence and a canonical positive
/// decimal i64 id (no leading zeros, no `+`).
fn parse_before(value: &str) -> Result<(i32, i64), &'static str> {
    let (sequence, id_text) = value.split_once(':').ok_or("invalid request line")?;
    let digits = sequence.strip_prefix('-').unwrap_or(sequence);
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return Err("invalid request line");
    }
    let sequence: i32 = sequence.parse().map_err(|_| "invalid request line")?;
    let id: i64 = id_text.parse().map_err(|_| "invalid request line")?;
    if id <= 0 || id.to_string() != id_text {
        return Err("invalid request line");
    }
    Ok((sequence, id))
}

fn parse_headers<'a>(
    lines: impl Iterator<Item = &'a str>,
) -> Result<HashMap<String, &'a str>, &'static str> {
    let mut headers = HashMap::new();
    for line in lines {
        let (name, value) = line.split_once(':').ok_or("invalid header")?;
        if name.is_empty() || !name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-') {
            return Err("invalid header");
        }
        if value.contains('\r')
            || value.contains('\n')
            || !value
                .bytes()
                .all(|b| b == b'\t' || (0x20..=0x7e).contains(&b))
        {
            return Err("invalid header");
        }
        if headers
            .insert(name.to_ascii_lowercase(), value.trim())
            .is_some()
        {
            return Err("duplicate header");
        }
    }
    Ok(headers)
}

fn allowed_get(path: &str) -> bool {
    matches!(
        path,
        "/" | "/app.js" | "/api.js" | "/state.js" | "/styles.css" | "/auth/bootstrap"
    ) || path.starts_with("/api/v1/")
}

fn content_length(headers: &HashMap<String, &str>) -> Result<usize, &'static str> {
    match headers.get("content-length") {
        None => Ok(0),
        Some(value) => {
            if value.is_empty()
                || !value.bytes().all(|b| b.is_ascii_digit())
                || (value.len() > 1 && value.starts_with('0'))
            {
                return Err("invalid content-length");
            }
            value.parse().map_err(|_| "invalid content-length")
        }
    }
}

/// A closed route table. No request supplied method name is ever forwarded.
pub fn read_method(path: &str) -> Option<&'static str> {
    let parts: Vec<_> = path.split('/').collect();
    match parts.as_slice() {
        ["", "api", "v1", "info"] => Some("RemoteGetInfoV1"),
        ["", "api", "v1", "projects"] => Some("RemoteListProjectsV1"),
        ["", "api", "v1", "projects", project, "sessions"] if uuid(project) => {
            Some("RemoteListSessionsV1")
        }
        ["", "api", "v1", "projects", project, "sessions", session]
            if uuid(project) && uuid(session) =>
        {
            Some("RemoteGetSessionV1")
        }
        [
            "",
            "api",
            "v1",
            "projects",
            project,
            "sessions",
            session,
            "history",
        ] if uuid(project) && uuid(session) => Some("RemoteGetHistoryPageV1"),
        [
            "",
            "api",
            "v1",
            "projects",
            project,
            "sessions",
            session,
            "decisions",
        ] if uuid(project) && uuid(session) => Some("RemoteGetDecisionsV1"),
        [
            "",
            "api",
            "v1",
            "projects",
            project,
            "sessions",
            session,
            "decision-targets",
        ] if uuid(project) && uuid(session) => Some("RemoteGetDecisionTargetsV1"),
        _ => None,
    }
}

/// The exact answer-POST shape. This is the only route that may become an
/// answer request; every other path returns `None` here.
#[must_use]
pub fn answer_route(path: &str) -> Option<(String, String)> {
    let parts: Vec<_> = path.split('/').collect();
    match parts.as_slice() {
        [
            "",
            "api",
            "v1",
            "projects",
            project,
            "sessions",
            session,
            "decisions",
            "answer" | "answer-pending",
        ] if uuid(project) && uuid(session) => {
            Some(((*project).to_string(), (*session).to_string()))
        }
        _ => None,
    }
}

/// A canonical lowercase hyphenated UUID, the only form any route accepts.
#[must_use]
pub fn is_canonical_uuid(s: &str) -> bool {
    uuid(s)
}

fn uuid(s: &str) -> bool {
    s.len() == 36
        && s.bytes().enumerate().all(|(i, b)| {
            if [8, 13, 18, 23].contains(&i) {
                b == b'-'
            } else {
                b.is_ascii_digit() || (b'a'..=b'f').contains(&b)
            }
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    const HOST: &str = "host.example.ts.net";

    fn request(line: &str, extra: &str) -> Vec<u8> {
        format!(
            "{line}\r\nhost: localhost\r\nx-forwarded-host: {HOST}\r\nx-forwarded-proto: https\r\nx-forwarded-for: 100.101.102.103\r\n{extra}\r\n"
        )
        .into_bytes()
    }

    fn get(path: &str) -> Vec<u8> {
        request(&format!("GET {path} HTTP/1.1"), "")
    }

    #[test]
    fn exact_forwarding_and_negative_cases() {
        assert!(parse_request(&get("/api/v1/info"), HOST).is_ok());
        assert!(
            parse_request(&get("/api/v1/info"), HOST)
                .unwrap()
                .path
                .eq("/api/v1/info")
        );
        let mixed_case = String::from_utf8(get("/api/v1/info"))
            .unwrap()
            .replace("host: localhost", "Host: localhost");
        assert!(parse_request(mixed_case.as_bytes(), HOST).is_ok());
        for extra in [
            "host: localhost\r\n",
            "Host: localhost\r\n",
            "x-forwarded-for: 100.101.102.104\r\n",
            "x-forwarded-proto: http\r\n",
            "forwarded: for=100.101.102.104\r\n",
            "x-forwarded-funnel-request: true\r\n",
            "content-length: 1\r\n",
        ] {
            assert!(parse_request(&request("GET /api/v1/info HTTP/1.1", extra), HOST).is_err());
        }
        let mapped = String::from_utf8(get("/api/v1/info"))
            .unwrap()
            .replace("100.101.102.103", "::ffff:100.101.102.103");
        assert!(parse_request(mapped.as_bytes(), HOST).is_err());
        assert!(parse_request(&get("/api/v1/info"), "other.example.ts.net").is_err());
        let public = String::from_utf8(get("/api/v1/info"))
            .unwrap()
            .replace("100.101.102.103", "8.8.8.8");
        assert!(parse_request(public.as_bytes(), HOST).is_err());
        let mut pipelined = get("/api/v1/info");
        pipelined.extend_from_slice(&get("/api/v1/info"));
        assert!(parse_request(&pipelined, HOST).is_err());
    }

    #[test]
    fn allowed_request_lines() {
        for path in [
            "/",
            "/app.js",
            "/api.js",
            "/state.js",
            "/styles.css",
            "/auth/bootstrap",
            "/api/v1/info",
            "/api/v1/projects",
        ] {
            let parsed = parse_request(&get(path), HOST).unwrap();
            assert_eq!(parsed.method, Method::Get);
            assert_eq!(parsed.path, path);
            assert_eq!(parsed.content_length, 0);
        }
        for line in [
            "GET /nope.js HTTP/1.1",
            "GET /index.html HTTP/1.1",
            "GET /auth/session HTTP/1.1",
            "GET /auth/logout HTTP/1.1",
            "POST / HTTP/1.1",
            "POST /api/v1/info HTTP/1.1",
            "DELETE /auth/logout HTTP/1.1",
            "GET /api/v2/info HTTP/1.1",
            "GET /api/v1/../secret HTTP/1.1",
            "GET /api/v1/info?x=1 HTTP/1.1",
        ] {
            assert_eq!(
                parse_request(&request(line, ""), HOST),
                Err("invalid request line"),
                "{line}",
            );
        }
    }

    #[test]
    fn get_must_not_have_a_body() {
        assert!(parse_request(&get("/"), HOST).is_ok());
        assert!(parse_request(&request("GET / HTTP/1.1", "content-length: 0\r\n"), HOST).is_ok());
        assert_eq!(
            parse_request(&request("GET / HTTP/1.1", "content-length: 4\r\n"), HOST),
            Err("invalid content-length"),
        );
        assert_eq!(
            parse_request(&request("GET / HTTP/1.1", "content-length: x\r\n"), HOST),
            Err("invalid content-length"),
        );
    }

    #[test]
    fn post_session_content_rules() {
        let ok = request(
            "POST /auth/session HTTP/1.1",
            "content-type: application/json\r\ncontent-length: 32\r\n",
        );
        let parsed = parse_request(&ok, HOST).unwrap();
        assert_eq!(parsed.method, Method::Post);
        assert_eq!(parsed.content_length, 32);
        for extra in [
            "content-length: 32\r\n",
            "content-type: text/plain\r\ncontent-length: 32\r\n",
            "content-type: application/json\r\n",
            "content-type: application/json\r\ncontent-length: 0\r\n",
            "content-type: application/json\r\ncontent-length: 4097\r\n",
        ] {
            assert!(
                parse_request(&request("POST /auth/session HTTP/1.1", extra), HOST).is_err(),
                "{extra}",
            );
        }
    }

    #[test]
    fn post_logout_content_rules() {
        assert!(parse_request(&request("POST /auth/logout HTTP/1.1", ""), HOST).is_ok());
        assert!(
            parse_request(
                &request("POST /auth/logout HTTP/1.1", "content-length: 0\r\n"),
                HOST
            )
            .is_ok()
        );
        assert!(
            parse_request(
                &request("POST /auth/logout HTTP/1.1", "content-length: 1\r\n"),
                HOST
            )
            .is_err()
        );
    }

    #[test]
    fn identity_headers_are_captured() {
        let parsed = parse_request(
            &request(
                "GET /api/v1/info HTTP/1.1",
                "origin: https://host.example.ts.net\r\nsec-fetch-site: same-origin\r\ncookie: __Host-rsi_remote=abc\r\nx-rsi-csrf: deadbeef\r\n",
            ),
            HOST,
        )
        .unwrap();
        assert_eq!(
            parsed.origin.as_deref(),
            Some("https://host.example.ts.net")
        );
        assert_eq!(parsed.sec_fetch_site.as_deref(), Some("same-origin"));
        assert_eq!(parsed.cookie.as_deref(), Some("__Host-rsi_remote=abc"));
        assert_eq!(parsed.csrf.as_deref(), Some("deadbeef"));
    }

    #[test]
    fn query_grammar_accepts_only_its_two_keys() {
        let base = "/api/v1/projects/550e8400-e29b-41d4-a716-446655440000/sessions";
        let cases: Vec<(&str, Option<&str>, Option<(i32, i64)>)> = vec![
            ("/api/v1/projects?cursor=abc_-9", Some("abc_-9"), None),
            (
                "/api/v1/projects/550e8400-e29b-41d4-a716-446655440000/sessions/550e8400-e29b-41d4-a716-446655440001/history?before=12:345",
                None,
                Some((12, 345)),
            ),
            (
                "/api/v1/projects/550e8400-e29b-41d4-a716-446655440000/sessions/550e8400-e29b-41d4-a716-446655440001/history?before=-1:7&cursor=x",
                Some("x"),
                Some((-1, 7)),
            ),
            (
                "/api/v1/projects?cursor=x&before=0:1",
                Some("x"),
                Some((0, 1)),
            ),
        ];
        for (target, cursor, before) in cases {
            let parsed = parse_request(&get(target), HOST).expect(target);
            assert_eq!(parsed.query.cursor.as_deref(), cursor, "{target}");
            assert_eq!(parsed.query.before, before, "{target}");
            assert_eq!(
                parsed.path,
                target.split('?').next().unwrap(),
                "{target} keeps the bare path"
            );
        }
        let parsed = parse_request(&get(base), HOST).unwrap();
        assert_eq!(parsed.query, Query::default());
        assert_eq!(parsed.path, base);

        let long = "a".repeat(1025);
        for target in [
            "/api/v1/projects?cursor=".to_string(),
            "/api/v1/projects?cursor=a%20b".to_string(),
            "/api/v1/projects?cursor=a+b".to_string(),
            "/api/v1/projects?x=1".to_string(),
            "/api/v1/projects?cursor=a&cursor=b".to_string(),
            "/api/v1/projects?before=1:0".to_string(),
            "/api/v1/projects?before=1:01".to_string(),
            "/api/v1/projects?before=a:1".to_string(),
            "/api/v1/projects?before=99999999999:1".to_string(),
            format!("/api/v1/projects?cursor={long}"),
            "/?cursor=a".to_string(),
            "/auth/bootstrap?cursor=a".to_string(),
            "/api/v1/info??cursor=a".to_string(),
        ] {
            assert_eq!(
                parse_request(&get(&target), HOST),
                Err("invalid request line"),
                "{target}"
            );
        }
        assert_eq!(
            parse_request(
                &request(
                    "POST /auth/session?cursor=a HTTP/1.1",
                    "content-type: application/json\r\ncontent-length: 2\r\n",
                ),
                HOST
            ),
            Err("invalid request line"),
        );
    }

    #[test]
    fn route_table_is_closed() {
        assert_eq!(read_method("/api/v1/info"), Some("RemoteGetInfoV1"));
        assert_eq!(
            read_method("/api/v1/projects"),
            Some("RemoteListProjectsV1")
        );
        assert_eq!(
            read_method("/api/v1/projects/550e8400-e29b-41d4-a716-446655440000/sessions"),
            Some("RemoteListSessionsV1")
        );
        assert_eq!(
            read_method(
                "/api/v1/projects/550e8400-e29b-41d4-a716-446655440000/sessions/550e8400-e29b-41d4-a716-446655440001"
            ),
            Some("RemoteGetSessionV1")
        );
        assert_eq!(
            read_method(
                "/api/v1/projects/550e8400-e29b-41d4-a716-446655440000/sessions/550e8400-e29b-41d4-a716-446655440001/history"
            ),
            Some("RemoteGetHistoryPageV1")
        );
        assert_eq!(
            read_method(
                "/api/v1/projects/550e8400-e29b-41d4-a716-446655440000/sessions/550e8400-e29b-41d4-a716-446655440001/decisions"
            ),
            Some("RemoteGetDecisionsV1")
        );
        assert_eq!(
            read_method(
                "/api/v1/projects/550e8400-e29b-41d4-a716-446655440000/sessions/550e8400-e29b-41d4-a716-446655440001/decision-targets"
            ),
            Some("RemoteGetDecisionTargetsV1")
        );
        assert_eq!(
            read_method(
                "/api/v1/projects/550e8400-e29b-41d4-a716-446655440000/sessions/550e8400-e29b-41d4-a716-446655440001/decisions/answer"
            ),
            None
        );
        assert_eq!(read_method("/api/v1/AgentHalt"), None);
        assert_eq!(
            read_method("/api/v1/projects/550E8400-e29b-41d4-a716-446655440000/sessions"),
            None
        );
    }

    #[test]
    fn decision_targets_and_answer_routes_parse_exactly() {
        const PROJECT: &str = "550e8400-e29b-41d4-a716-446655440000";
        const SESSION: &str = "550e8400-e29b-41d4-a716-446655440001";
        let targets = format!("/api/v1/projects/{PROJECT}/sessions/{SESSION}/decision-targets");
        let answer = format!("/api/v1/projects/{PROJECT}/sessions/{SESSION}/decisions/answer");

        let parsed = parse_request(&get(&targets), HOST).unwrap();
        assert_eq!(parsed.method, Method::Get);
        assert_eq!(parsed.path, targets);
        assert_eq!(answer_route(&targets), None);
        assert_eq!(
            answer_route(&answer),
            Some((PROJECT.to_string(), SESSION.to_string()))
        );

        let json = "content-type: application/json\r\ncontent-length: 2\r\n";
        let parsed =
            parse_request(&request(&format!("POST {answer} HTTP/1.1"), json), HOST).unwrap();
        assert_eq!(parsed.method, Method::Post);
        assert_eq!(parsed.content_length, 2);

        for extra in [
            "content-type: text/plain\r\ncontent-length: 2\r\n",
            "content-type: application/json\r\ncontent-length: 0\r\n",
            "content-type: application/json\r\ncontent-length: 4097\r\n",
        ] {
            assert!(
                parse_request(&request(&format!("POST {answer} HTTP/1.1"), extra), HOST).is_err(),
                "{extra}"
            );
        }
        assert!(
            parse_request(
                &request(&format!("POST {answer}?cursor=x HTTP/1.1"), json),
                HOST
            )
            .is_err(),
            "answer POST refuses a query"
        );
        assert!(
            parse_request(&get(&format!("{targets}?cursor=x")), HOST).is_err(),
            "decision-targets GET refuses a query"
        );
        assert!(
            parse_request(&get(&answer), HOST).is_err(),
            "GET answer denies"
        );
        for bad in [
            format!(
                "/api/v1/projects/{}/sessions/{SESSION}/decisions/answer",
                PROJECT.to_uppercase()
            ),
            format!(
                "/api/v1/projects/{PROJECT}/sessions/{}/decisions/answer",
                SESSION.to_uppercase()
            ),
            format!("/api/v1/projects/{PROJECT}/sessions/{SESSION}/decisions/answer/extra"),
            format!("/api/v1/projects/{PROJECT}/sessions/{SESSION}/decisions"),
        ] {
            assert!(
                parse_request(
                    &request(
                        &format!("POST {bad} HTTP/1.1"),
                        "content-type: application/json\r\ncontent-length: 2\r\n"
                    ),
                    HOST
                )
                .is_err(),
                "{bad}"
            );
        }
    }
}
