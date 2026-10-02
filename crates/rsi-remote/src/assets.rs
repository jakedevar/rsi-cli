//! Closed table of bundled browser assets served by the gateway.
//!
//! Every byte is compiled into the binary with `include_bytes!`; there is no
//! filesystem lookup and no path that can escape the table.

/// Content Security Policy for bundled browser assets (ADR D5).
pub const CSP: &str = "default-src 'none'; script-src 'self'; style-src 'self'; connect-src 'self'; img-src 'self'; base-uri 'none'; form-action 'none'; frame-ancestors 'none'";

const HTML: &str = "text/html; charset=utf-8";
const JS: &str = "text/javascript; charset=utf-8";
const CSS: &str = "text/css; charset=utf-8";

/// Map one request path to its content type and bytes, or `None`.
#[must_use]
pub fn asset(path: &str) -> Option<(&'static str, &'static [u8])> {
    match path {
        "/" => Some((HTML, include_bytes!("../../../remote/web/index.html"))),
        "/app.js" => Some((JS, include_bytes!("../../../remote/web/app.js"))),
        "/api.js" => Some((JS, include_bytes!("../../../remote/web/api.js"))),
        "/state.js" => Some((JS, include_bytes!("../../../remote/web/state.js"))),
        "/styles.css" => Some((CSS, include_bytes!("../../../remote/web/styles.css"))),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_asset_maps_to_its_content_type() {
        for (path, content_type) in [
            ("/", HTML),
            ("/app.js", JS),
            ("/api.js", JS),
            ("/state.js", JS),
            ("/styles.css", CSS),
        ] {
            let (kind, bytes) = asset(path).unwrap_or_else(|| panic!("missing {path}"));
            assert_eq!(kind, content_type);
            assert!(!bytes.is_empty(), "{path}");
        }
        assert_eq!(
            asset("/"),
            Some((HTML, &include_bytes!("../../../remote/web/index.html")[..]))
        );
    }

    #[test]
    fn unknown_and_traversal_paths_are_absent() {
        for path in [
            "/index.html",
            "/../remote/web/index.html",
            "/app.js?x=1",
            "/styles.css/",
            "",
            "/favicon.ico",
        ] {
            assert!(asset(path).is_none(), "{path}");
        }
    }
}
