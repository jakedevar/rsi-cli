//! Markdown to sanitized HTML for conversation messages.
//!
//! Pipeline: `pulldown-cmark` parses; raw HTML events are turned into
//! literal text and images into their alt text; the generated HTML is then
//! run through `ammonia` with a strict allowlist as the final authority. The
//! webview may insert the result with `innerHTML`; nothing else from the
//! daemon may be.

use std::collections::HashSet;

use ammonia::{Builder, UrlRelative};
use pulldown_cmark::{Event, Options, Parser, Tag, TagEnd, html};

/// Most texts accepted in one `render_markdown` call.
pub const MAX_BATCH_TEXTS: usize = 200;
/// Largest single text rendered as Markdown; longer ones fall back to an escaped `<pre>`.
pub const MAX_TEXT_BYTES: usize = 256 * 1024;
/// Largest total payload accepted in one call.
pub const MAX_BATCH_BYTES: usize = 2 * 1024 * 1024;

const TAGS: &[&str] = &[
    "p",
    "br",
    "hr",
    "h1",
    "h2",
    "h3",
    "h4",
    "h5",
    "h6",
    "ul",
    "ol",
    "li",
    "blockquote",
    "pre",
    "code",
    "em",
    "strong",
    "del",
    "a",
    "table",
    "thead",
    "tbody",
    "tr",
    "th",
    "td",
];

fn sanitizer() -> Builder<'static> {
    let mut b = Builder::empty();
    b.tags(TAGS.iter().copied().collect::<HashSet<_>>())
        // Drop the contents (not just the tags) of these elements.
        .clean_content_tags(
            [
                "script", "style", "iframe", "object", "embed", "noscript", "template",
            ]
            .into_iter()
            .collect(),
        )
        .tag_attributes(
            [
                ("a", ["href", "title"].into_iter().collect::<HashSet<_>>()),
                ("code", ["class"].into_iter().collect()),
                ("ol", ["start"].into_iter().collect()),
            ]
            .into_iter()
            .collect(),
        )
        .generic_attributes(HashSet::new())
        .url_schemes(["http", "https", "mailto"].into_iter().collect())
        .url_relative(UrlRelative::Deny)
        .link_rel(Some("noopener noreferrer nofollow"))
        .strip_comments(true)
        // Only `language-xxx` classes survive, for the frontend to style.
        .attribute_filter(|element, attribute, value| match (element, attribute) {
            ("code", "class") => value
                .strip_prefix("language-")
                .filter(|l| {
                    !l.is_empty()
                        && l.len() <= 32
                        && l.chars()
                            .all(|c| c.is_ascii_alphanumeric() || "+-_#.".contains(c))
                })
                .map(|_| value.into()),
            ("ol", "start") => value.parse::<u32>().ok().map(|_| value.into()),
            _ => Some(value.into()),
        });
    b
}

fn escape_html(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(c),
        }
    }
    out
}

/// Render one Markdown text to sanitized HTML.
pub fn render_one(cleaner: &Builder<'_>, text: &str) -> String {
    if text.len() > MAX_TEXT_BYTES {
        return format!("<pre><code>{}</code></pre>", escape_html(text));
    }
    let options = Options::ENABLE_TABLES | Options::ENABLE_STRIKETHROUGH;
    let mut image_depth = 0usize;
    let events = Parser::new_ext(text, options).filter_map(|ev| match ev {
        // Raw HTML is shown literally, never interpreted.
        Event::Html(s) | Event::InlineHtml(s) => Some(Event::Text(s)),
        // Images are disabled: keep the alt text only.
        Event::Start(Tag::Image { .. }) => {
            image_depth += 1;
            None
        }
        Event::End(TagEnd::Image) => {
            image_depth = image_depth.saturating_sub(1);
            None
        }
        Event::Text(t) if image_depth > 0 => Some(Event::Text(t)),
        other => Some(other),
    });
    let mut raw = String::new();
    html::push_html(&mut raw, events);
    cleaner.clean(&raw).to_string()
}

/// Render a batch. The whole batch is refused if it exceeds the size bounds.
pub fn render_batch(texts: &[String]) -> Result<Vec<String>, String> {
    if texts.len() > MAX_BATCH_TEXTS {
        return Err(format!("too many texts (max {MAX_BATCH_TEXTS})"));
    }
    let total: usize = texts.iter().map(String::len).sum();
    if total > MAX_BATCH_BYTES {
        return Err(format!("batch exceeds {MAX_BATCH_BYTES} bytes"));
    }
    let cleaner = sanitizer();
    Ok(texts.iter().map(|t| render_one(&cleaner, t)).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn r(text: &str) -> String {
        render_one(&sanitizer(), text)
    }

    #[test]
    fn renders_basic_markdown() {
        let out = r("# Title\n\n- a\n- b\n\n**bold** and `code`\n\n```rust\nfn main() {}\n```\n");
        assert!(out.contains("<h1>Title</h1>"));
        assert!(out.contains("<li>a</li>"));
        assert!(out.contains("<strong>bold</strong>"));
        assert!(out.contains("<code>code</code>"));
        assert!(out.contains("<pre><code class=\"language-rust\">fn main() {}\n</code></pre>"));
    }

    #[test]
    fn renders_tables_and_strikethrough() {
        let out = r("|a|b|\n|-|-|\n|1|2|\n\n~~gone~~");
        assert!(out.contains("<table>") && out.contains("<td>1</td>"));
        assert!(out.contains("<del>gone</del>"));
    }

    #[test]
    fn strips_script_tags_and_contents() {
        let out = r("hello <script>alert(1)</script> world\n\n<script>\nalert(2)\n</script>\n");
        assert!(!out.contains("<script"), "{out}");
        assert!(out.contains("hello") && out.contains("world"));
    }

    /// Every real markup tag in the output (escaped text such as `&lt;a&gt;` is not markup).
    fn markup(out: &str) -> Vec<String> {
        let mut tags = Vec::new();
        let mut rest = out;
        while let Some(i) = rest.find('<') {
            let Some(j) = rest[i..].find('>') else { break };
            tags.push(rest[i..i + j + 1].to_lowercase());
            rest = &rest[i + j + 1..];
        }
        tags
    }

    #[test]
    fn strips_inline_event_handlers_and_styles() {
        let out = r(
            "<img src=x onerror=alert(1)>\n\n<div onclick=\"x()\" style=\"color:red\">hi</div>\n\n<a href=\"https://e.com\" onclick=\"x()\" style=\"x\">l</a>",
        );
        for tag in markup(&out) {
            assert!(
                !tag.contains("onerror") && !tag.contains("onclick") && !tag.contains("style"),
                "{tag} in {out}"
            );
            assert!(
                !tag.starts_with("<img") && !tag.starts_with("<div"),
                "{tag} in {out}"
            );
        }
        // Markdown links keep only href/title/rel.
        let link = r("[l](https://e.com \"t\")");
        assert!(
            link.contains(
                "<a href=\"https://e.com\" title=\"t\" rel=\"noopener noreferrer nofollow\">l</a>"
            ),
            "{link}"
        );
    }

    #[test]
    fn neutralises_javascript_urls() {
        for md in [
            "[x](javascript:alert(1))",
            "[x](JaVaScRiPt:alert(1))",
            "[x](data:text/html;base64,PHNjcmlwdD4=)",
            "[x](vbscript:msgbox)",
            "<a href=\"javascript:alert(1)\">x</a>",
            "[x](  javascript:alert(1))",
            "<javascript:alert(1)>",
        ] {
            let out = r(md);
            for tag in markup(&out) {
                for bad in ["javascript:", "data:", "vbscript:"] {
                    assert!(!tag.contains(bad), "{md} -> {out}");
                }
            }
        }
    }

    #[test]
    fn raw_html_is_shown_as_text_not_markup() {
        let out = r("<b>bold</b> <iframe src=\"https://e.com\"></iframe> <style>*{}</style>");
        assert!(
            !out.contains("<b>") && !out.contains("<iframe") && !out.contains("<style"),
            "{out}"
        );
        assert!(out.contains("&lt;b&gt;"), "{out}");
    }

    #[test]
    fn images_are_disabled_alt_text_kept() {
        let out = r("![diagram](https://e.com/a.png)");
        assert!(!out.contains("<img") && !out.contains("a.png"), "{out}");
        assert!(out.contains("diagram"), "{out}");
    }

    #[test]
    fn links_get_rel_and_safe_schemes_only() {
        let out = r("[ok](https://example.com/a?b=1) [mail](mailto:a@b.c) [rel](/relative)");
        assert!(out.contains("href=\"https://example.com/a?b=1\""), "{out}");
        assert!(
            out.contains("rel=\"noopener noreferrer nofollow\""),
            "{out}"
        );
        assert!(out.contains("mailto:a@b.c"));
        assert!(!out.contains("href=\"/relative\""), "{out}");
        assert!(!out.contains("target="), "{out}");
    }

    #[test]
    fn code_class_is_restricted() {
        let out = r("```\"onmouseover=alert(1)\nx\n```");
        assert!(!out.contains("onmouseover"), "{out}");
        let out = r("```c++\nx\n```");
        assert!(out.contains("class=\"language-c++\""), "{out}");
    }

    #[test]
    fn oversized_text_falls_back_to_escaped_pre() {
        let big = format!("<script>{}</script>", "x".repeat(MAX_TEXT_BYTES));
        let out = r(&big);
        assert!(out.starts_with("<pre><code>&lt;script&gt;"));
        assert!(!out.contains("<script"));
    }

    #[test]
    fn batch_bounds() {
        let many: Vec<String> = (0..=MAX_BATCH_TEXTS).map(|i| i.to_string()).collect();
        assert!(render_batch(&many).is_err());
        let huge = vec!["x".repeat(MAX_TEXT_BYTES); MAX_BATCH_BYTES / MAX_TEXT_BYTES + 1];
        assert!(render_batch(&huge).is_err());
        let ok = render_batch(&["a".into(), "".into(), "**b**".into()]).unwrap();
        assert_eq!(ok.len(), 3);
        assert_eq!(ok[0], "<p>a</p>\n");
        assert_eq!(ok[1], "");
        assert!(ok[2].contains("<strong>b</strong>"));
    }
}
