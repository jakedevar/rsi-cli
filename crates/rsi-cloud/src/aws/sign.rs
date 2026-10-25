//! SigV4 for fixed HTTPS endpoints, `/` paths and EC2 form bodies. Cryptographic
//! primitives come from the workspace's existing ring (also used by rustls).
use std::collections::BTreeMap;

use super::credentials::Credentials;
use ring::{digest, hmac};

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}
fn hash(bytes: &[u8]) -> String {
    hex(digest::digest(&digest::SHA256, bytes).as_ref())
}
fn mac(key: &[u8], bytes: &[u8]) -> Vec<u8> {
    hmac::sign(&hmac::Key::new(hmac::HMAC_SHA256, key), bytes)
        .as_ref()
        .to_vec()
}

pub(super) struct Request<'a> {
    pub method: &'a str,
    pub host: &'a str,
    pub service: &'a str,
    pub region: &'a str,
    pub body: &'a [u8],
    pub checksum_header: bool,
}

pub(super) fn headers(
    request: &Request<'_>,
    credentials: &Credentials,
    timestamp: &str,
    mut headers: BTreeMap<String, String>,
) -> BTreeMap<String, String> {
    let payload = hash(request.body);
    headers.insert("host".into(), request.host.into());
    headers.insert("x-amz-date".into(), timestamp.into());
    if request.checksum_header {
        headers.insert("x-amz-content-sha256".into(), payload.clone());
    }
    if let Some(token) = &credentials.token {
        headers.insert("x-amz-security-token".into(), token.clone());
    }
    // Normalize precisely the values sent, before hashing them.
    for value in headers.values_mut() {
        *value = value.split_whitespace().collect::<Vec<_>>().join(" ");
    }
    let names = headers.keys().cloned().collect::<Vec<_>>().join(";");
    let canonical_headers = headers
        .iter()
        .map(|(k, v)| format!("{k}:{v}\n"))
        .collect::<String>();
    let canonical = format!(
        "{}\n/\n\n{canonical_headers}\n{names}\n{payload}",
        request.method
    );
    let date = &timestamp[..8];
    let scope = format!("{date}/{}/{}/aws4_request", request.region, request.service);
    let to_sign = format!(
        "AWS4-HMAC-SHA256\n{timestamp}\n{scope}\n{}",
        hash(canonical.as_bytes())
    );
    let key = mac(
        format!("AWS4{}", credentials.secret).as_bytes(),
        date.as_bytes(),
    );
    let key = mac(&key, request.region.as_bytes());
    let key = mac(&key, request.service.as_bytes());
    let key = mac(&key, b"aws4_request");
    let signature = hex(&mac(&key, to_sign.as_bytes()));
    headers.insert(
        "authorization".into(),
        format!(
            "AWS4-HMAC-SHA256 Credential={}/{scope}, SignedHeaders={names}, Signature={signature}",
            credentials.access
        ),
    );
    headers
}
