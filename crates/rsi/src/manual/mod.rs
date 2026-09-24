//! Operator manual generator (Epic M design B).
//!
//! One model (`model::build_manual`) built from the registries, two writers
//! (`markdown`, `html`), and the generated regions of `docs/keybindings.md`.
//! The committed artifacts are compared by drift tests; regenerate them with
//! `make manual` (`RSI_BLESS_MANUAL=1 cargo test -p rsi --lib manual::`).

pub mod html;
pub mod markdown;
pub mod model;
pub mod open;

use std::collections::BTreeSet;

/// Committed artifacts, relative to the workspace root.
pub const MARKDOWN_PATH: &str = "docs/manual/rsi-manual.md";
pub const HTML_PATH: &str = "docs/manual/rsi-manual.html";
pub const KEYBINDINGS_PATH: &str = "docs/keybindings.md";

const REGION_BEGIN: &str = "<!-- rsi:generated:begin ";
const REGION_END: &str = "<!-- rsi:generated:end -->";

/// Render the manual as Markdown.
#[must_use]
pub fn render_markdown() -> String {
    markdown::render(&model::build_manual())
}

/// Render the manual as standalone HTML.
#[must_use]
pub fn render_html() -> String {
    html::render(&model::build_manual())
}

/// Rewrite every generated region of `doc` from the manual.
///
/// Text outside the markers is left untouched. Fails when a region id is
/// unknown, duplicated, unterminated, or when a manual region has no marker.
///
/// # Errors
///
/// Returns a description of the first malformed or missing region.
pub fn regenerate_regions(doc: &str, manual: &model::Manual) -> Result<String, String> {
    let regions = manual.regions();
    let known: BTreeSet<&str> = regions.iter().map(|(id, _)| *id).collect();
    let mut seen = BTreeSet::new();
    let mut out = String::with_capacity(doc.len());
    let mut rest = doc;
    while let Some(start) = rest.find(REGION_BEGIN) {
        let after_begin = &rest[start + REGION_BEGIN.len()..];
        let id_end = after_begin
            .find(" -->")
            .ok_or_else(|| "unterminated region begin marker".to_string())?;
        let id = &after_begin[..id_end];
        if !known.contains(id) {
            return Err(format!("unknown generated region `{id}`"));
        }
        if !seen.insert(id.to_string()) {
            return Err(format!("duplicate generated region `{id}`"));
        }
        let body_start = start + REGION_BEGIN.len() + id_end + " -->".len();
        let end = rest[body_start..]
            .find(REGION_END)
            .ok_or_else(|| format!("region `{id}` has no end marker"))?;
        let blocks = regions
            .iter()
            .find(|(region, _)| *region == id)
            .map(|(_, blocks)| *blocks)
            .unwrap_or_default();
        out.push_str(&rest[..body_start]);
        out.push('\n');
        out.push_str(&markdown::region_body(blocks));
        out.push('\n');
        out.push_str(REGION_END);
        rest = &rest[body_start + end + REGION_END.len()..];
    }
    out.push_str(rest);
    let missing: Vec<&str> = known
        .iter()
        .filter(|id| !seen.contains(**id))
        .copied()
        .collect();
    if !missing.is_empty() {
        return Err(format!("missing generated regions: {}", missing.join(", ")));
    }
    Ok(out)
}

/// Headings of a Markdown document (text after the `#`s).
#[must_use]
pub fn markdown_headings(doc: &str) -> BTreeSet<&str> {
    doc.lines()
        .filter(|line| line.starts_with('#'))
        .map(|line| line.trim_start_matches('#').trim())
        .collect()
}

#[cfg(test)]
#[allow(clippy::expect_used)] // Test fixtures fail loudly on a broken invariant.
mod tests {
    use super::*;
    use crate::action_registry::{
        DocAnchor, OVERLAY_HELP_ROUTES, OverlayHelpExemption, UNTABULATED_KEY_SURFACES,
    };

    fn workspace_path(relative: &str) -> std::path::PathBuf {
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../..")
            .join(relative)
    }

    fn blessing() -> bool {
        std::env::var_os("RSI_BLESS_MANUAL").is_some()
    }

    /// Compare a committed artifact with its regenerated form, or rewrite it
    /// under `RSI_BLESS_MANUAL=1`.
    fn check_artifact(relative: &str, generated: &str) {
        let path = workspace_path(relative);
        if blessing() {
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent).expect("create artifact directory");
            }
            std::fs::write(&path, generated).expect("write blessed artifact");
            return;
        }
        let committed = std::fs::read_to_string(&path)
            .unwrap_or_else(|error| panic!("read {relative}: {error}; run `make manual`"));
        assert!(
            committed == generated,
            "{relative} is stale; run `make manual` to regenerate it"
        );
    }

    /// T1.
    #[test]
    fn markdown_is_current() {
        check_artifact(MARKDOWN_PATH, &render_markdown());
    }

    /// T2.
    #[test]
    fn html_is_current() {
        check_artifact(HTML_PATH, &render_html());
    }

    /// T3: every generated region of docs/keybindings.md is current, and the
    /// region ids are exactly the manual's (none missing, none duplicated).
    #[test]
    fn keybindings_md_regions_are_current() {
        let path = workspace_path(KEYBINDINGS_PATH);
        let doc = std::fs::read_to_string(&path).expect("read docs/keybindings.md");
        let manual = model::build_manual();
        let regenerated = regenerate_regions(&doc, &manual).expect("regions are well formed");
        if blessing() {
            std::fs::write(&path, &regenerated).expect("write docs/keybindings.md");
            return;
        }
        assert!(
            regenerated == doc,
            "docs/keybindings.md generated regions are stale; run `make manual`"
        );
        let ids: Vec<&str> = manual.regions().iter().map(|(id, _)| *id).collect();
        let unique: BTreeSet<&str> = ids.iter().copied().collect();
        assert_eq!(unique.len(), ids.len(), "manual region ids are unique");
        for id in model::FIXED_REGION_IDS {
            assert!(unique.contains(id), "manual has region {id}");
        }
    }

    /// T11b: every overlay route has a titled section whose rows are exactly
    /// the route's entries.
    #[test]
    fn every_overlay_route_and_row_is_in_manual() {
        let manual = model::build_manual();
        let headings: BTreeSet<&str> = manual.headings().into_iter().collect();
        for route in OVERLAY_HELP_ROUTES {
            assert!(headings.contains(route.title), "section {}", route.title);
            let expected: BTreeSet<(String, String)> = route
                .entries()
                .map(|entry| (model::code(entry.keys), entry.label.to_string()))
                .collect();
            let tables = manual.region_tables(&model::overlay_region_id(route));
            let rendered: BTreeSet<(String, String)> = tables
                .iter()
                .flat_map(|table| table.rows.iter())
                .map(|row| (row[0].clone(), row[1].clone()))
                .collect();
            assert_eq!(rendered, expected, "rows of {}", route.title);
        }
    }

    /// T11e: the Exemptions table has exactly one row per exemption, with its
    /// reason and a documented-by anchor that resolves.
    #[test]
    fn every_exemption_is_in_manual_exemption_table() {
        let manual = model::build_manual();
        let doc = std::fs::read_to_string(workspace_path(KEYBINDINGS_PATH))
            .expect("read docs/keybindings.md");
        let headings = markdown_headings(&doc);
        let region_ids: BTreeSet<&str> = manual.regions().iter().map(|(id, _)| *id).collect();
        let rows: Vec<Vec<String>> = manual
            .region_tables("overlay-exemptions")
            .iter()
            .flat_map(|table| table.rows.clone())
            .collect();
        let expected: Vec<Vec<String>> = OverlayHelpExemption::ALL
            .iter()
            .map(|exemption| {
                let anchor = exemption.documented_by();
                match anchor {
                    DocAnchor::GeneratedRegion(id) => {
                        assert!(region_ids.contains(id), "{exemption:?} region {id}");
                    }
                    DocAnchor::Narrative(heading) => {
                        assert!(
                            headings.contains(heading),
                            "{exemption:?} heading {heading}"
                        );
                    }
                }
                vec![
                    exemption.state().to_string(),
                    exemption.reason().to_string(),
                    match anchor {
                        DocAnchor::GeneratedRegion(id) => format!("generated table `{id}`"),
                        DocAnchor::Narrative(heading) => format!("keybindings.md § {heading}"),
                    },
                ]
            })
            .collect();
        assert_eq!(rows, expected);
    }

    /// T12: every hand-documented surface points at a real heading.
    #[test]
    fn untabulated_surfaces_link_to_existing_narrative() {
        let doc = std::fs::read_to_string(workspace_path(KEYBINDINGS_PATH))
            .expect("read docs/keybindings.md");
        let headings = markdown_headings(&doc);
        for surface in UNTABULATED_KEY_SURFACES {
            assert!(
                headings.contains(surface.narrative_anchor),
                "{} links to § {}",
                surface.surface,
                surface.narrative_anchor
            );
        }
    }

    /// The theme table and count come from the theme registry (D-001).
    #[test]
    fn theming_chapter_lists_every_built_in_theme() {
        let manual = model::build_manual();
        let rows: Vec<String> = manual
            .region_tables("themes")
            .iter()
            .flat_map(|table| table.rows.iter().map(|row| row[1].clone()))
            .collect();
        let expected: Vec<String> = (0..crate::ui::theme::theme_count())
            .map(|index| crate::ui::theme::theme_display_name(index).to_string())
            .collect();
        assert_eq!(rows, expected);
    }
}
