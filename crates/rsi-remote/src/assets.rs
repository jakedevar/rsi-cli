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

    fn text(path: &str) -> &'static str {
        std::str::from_utf8(asset(path).unwrap_or_else(|| panic!("missing {path}")).1)
            .unwrap_or_else(|_| panic!("{path} is not UTF-8"))
    }

    const SCRIPTS: [&str; 4] = ["/app.js", "/api.js", "/state.js", "/"];

    #[test]
    fn the_csp_still_forbids_inline_script_and_style() {
        assert!(CSP.contains("script-src 'self'"));
        assert!(CSP.contains("style-src 'self'"));
        assert!(CSP.contains("form-action 'none'"));
        assert!(!CSP.contains("unsafe-inline"));
        assert!(!CSP.contains("unsafe-eval"));
    }

    #[test]
    fn the_page_has_no_inline_script_style_or_handler_attributes() {
        let html = text("/");
        for script in html.match_indices("<script") {
            let tag = &html[script.0..];
            let tag = &tag[..tag.find('>').expect("closed tag")];
            assert!(tag.contains("src=\"./"), "inline script: {tag}");
        }
        assert!(!html.contains("<style"), "inline style element");
        assert!(!html.contains(" style=\""), "inline style attribute");
        for handler in [
            " onclick=",
            " onsubmit=",
            " oninput=",
            " onload=",
            "javascript:",
        ] {
            assert!(!html.contains(handler), "{handler}");
        }
    }

    #[test]
    fn scripts_build_the_answer_form_without_string_markup_or_eval() {
        for path in SCRIPTS {
            let source = text(path);
            for forbidden in [
                "innerHTML",
                "outerHTML",
                "insertAdjacentHTML",
                "document.write",
                "eval(",
                "new Function",
                "setTimeout('",
                "setTimeout(\"",
            ] {
                assert!(!source.contains(forbidden), "{path} uses {forbidden}");
            }
        }
    }

    #[test]
    fn every_module_import_is_a_served_asset() {
        for path in ["/app.js", "/api.js", "/state.js"] {
            for line in text(path)
                .lines()
                .filter(|line| line.starts_with("import "))
            {
                let from = line.rsplit("from './").next().expect("import source");
                let name = from.trim_end_matches(';').trim_end_matches('\'');
                assert!(
                    asset(&format!("/{name}")).is_some(),
                    "{path} imports /{name}"
                );
            }
        }
    }

    #[test]
    fn the_answer_form_posts_to_the_gateway_route_with_the_csrf_token() {
        let api = text("/api.js");
        assert!(api.contains("/decisions/answer"));
        assert!(api.contains("/decision-targets"));
        assert!(api.contains("'x-rsi-csrf': csrf"));
        assert!(api.contains("idempotency_key: request.idempotencyKey"));
        assert!(api.contains("expected_row_version: request.rowVersion"));
        // Only the answer POST and logout carry the token; reads never do.
        assert_eq!(api.matches("'x-rsi-csrf'").count(), 2);
        let app = text("/app.js");
        assert!(app.contains("submitDraft"));
        assert!(app.contains("describeDraft"));
        assert!(text("/state.js").contains("export function createDraftStore"));
        assert!(text("/").contains("id=\"footnote-mode\""));
        assert!(text("/").contains("id=\"announce\""));
        assert!(text("/styles.css").contains(".answer-form"));
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
