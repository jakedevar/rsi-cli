//! Require a scoped theme pin in tests that read or mutate active theme state (#1206).
//!
//! This is a lexical guard, not a Rust parser: it checks direct `theme::function`
//! references and unqualified theme setters in `#[test]`/`#[tokio::test]` bodies.
//! Indirect helper calls still need review. Immutable theme catalog lookups are
//! exempt. Comments and literals are masked before balancing test-body braces.
//!
//! Usage: `rsi-repo-guard theme-test-guards [paths...]` (default `crates/rsi/src`).

use std::io::Write;
use std::path::Path;

use crate::re::{Lazy, Re, end, group, start};
use crate::{Ctx, py};

/// Comments, raw strings, strings and char literals (masked to spaces).
pub(crate) static TRIVIA: Lazy = Lazy::new(|| {
    Re::new(concat!(
        r#"(?s)//[^\n]*|/\*.*?\*/|\br(#*)".*?"\1|"(?:[^"\\]|\\.)*""#,
        r#"|'(?:[^'\\\n]|\\(?:u\{[0-9a-fA-F_]+\}|x[0-9a-fA-F]{2}|.))'"#,
    ))
});
static TEST: Lazy = Lazy::new(|| {
    Re::new(
        r"#\[(?:tokio::)?test(?:\([^\]]*\))?\]\s*(?:#\[[^\]]*\]\s*)*(?:async\s+)?fn\s+(\w+)[^{]*\{",
    )
});
static THEME: Lazy = Lazy::new(|| Re::new(r"\btheme\s*::\s*([a-z_]\w*)\b(?!\s*::)"));
static SETTER: Lazy = Lazy::new(|| {
    Re::new(concat!(
        r"\b(?:set_theme_\w+|apply_theme_role_overrides|clear_theme_role_overrides|",
        r"set_border_color_override|apply_border_color_overrides|apply_startup_theme_state)\s*\(",
    ))
});
const EXEMPT: &[&str] = &[
    "pin_theme_state",
    "with_theme_state",
    "test_render_guard",
    "test_rgb_channels",
    "theme_count",
    "theme_display_name",
    "theme_key",
    "theme_swatch",
];
static PIN: Lazy = Lazy::new(|| {
    Re::new(
        r"\blet\s+(?!_\s*=)\w+\s*=\s*(?:[\w]+::)*(?:pin_theme_state|theme_test_guard)\s*\(\s*\)\s*;",
    )
});
static WRAPPER: Lazy = Lazy::new(|| Re::new(r"\bwith_theme_state\s*\(\s*\|\|\s*\{"));

/// `text` with every comment and literal blanked to spaces, byte for byte
/// (newlines kept), so offsets in the result are offsets in `text`.
#[must_use]
pub(crate) fn mask(text: &str, trivia: &Re) -> String {
    trivia.sub_with(text, &|caps| {
        group(caps, 0)
            .bytes()
            .map(|b| if b == b'\n' { '\n' } else { ' ' })
            .collect()
    })
}

fn closing_brace(code: &str, from: usize) -> Result<usize, String> {
    let mut depth: i64 = 1;
    for (pos, byte) in code.bytes().enumerate().skip(from) {
        depth += i64::from(byte == b'{') - i64::from(byte == b'}');
        if depth == 0 {
            return Ok(pos);
        }
    }
    Err("unbalanced test body".to_string())
}

/// (test name, body start offset, masked body) for every test in `text`.
pub fn test_bodies(text: &str) -> Result<Vec<(String, usize, String)>, String> {
    let code = mask(text, &TRIVIA);
    let mut bodies = Vec::new();
    for caps in TEST.find_all(&code) {
        let from = end(&caps);
        let to = closing_brace(&code, from)?;
        bodies.push((
            group(&caps, 1).to_string(),
            from,
            code[from..to].to_string(),
        ));
    }
    Ok(bodies)
}

/// True when the (masked) test body reads theme state without holding a pin.
pub fn unpinned(body: &str) -> Result<bool, String> {
    let mut reads: Vec<usize> = THEME
        .find_all(body)
        .iter()
        .filter(|caps| !EXEMPT.contains(&group(caps, 1)))
        .map(start)
        .collect();
    reads.extend(SETTER.find_all(body).iter().map(start));
    let Some(first) = reads.iter().min().copied() else {
        return Ok(false);
    };
    // A pin must be retained in a named binding in the test's outer scope,
    // before any theme read, so it survives the render and later assertions.
    for pin in PIN.find_all(body) {
        let prefix = &body[..start(&pin)];
        if prefix.matches('{').count() == prefix.matches('}').count() && end(&pin) <= first {
            return Ok(false);
        }
    }
    for wrapper in WRAPPER.find_all(body) {
        let from = end(&wrapper);
        let to = closing_brace(body, from)?;
        if reads.iter().all(|read| from <= *read && *read < to) {
            return Ok(false);
        }
    }
    Ok(true)
}

/// (line, test name) for every unpinned test in `text`.
pub fn scan_text(text: &str) -> Result<Vec<(usize, String)>, String> {
    let mut found = Vec::new();
    for (name, from, body) in test_bodies(text)? {
        if unpinned(&body)? {
            found.push((py::count_newlines(text, from) + 1, name));
        }
    }
    Ok(found)
}

fn scan(path: &Path) -> Result<Vec<(usize, String)>, String> {
    let text = py::read_text(path).map_err(|error| error.to_string())?;
    scan_text(&text).map_err(|error| format!("{}: {error}", path.display()))
}

/// Entry point: `theme-test-guards [paths...]`.
pub fn main(args: &[String], ctx: &Ctx, out: &mut dyn Write, err: &mut dyn Write) -> i32 {
    let roots: Vec<String> = if args.is_empty() {
        vec!["crates/rsi/src".to_string()]
    } else {
        args.to_vec()
    };
    let mut findings = 0;
    for root in roots {
        let shown = py::path_str(&root);
        let path = ctx.path(&root);
        let paths: Vec<(String, std::path::PathBuf)> = if path.is_file() {
            vec![(shown, path)]
        } else {
            py::rglob(&path, &|name| name.ends_with(".rs"))
                .into_iter()
                .map(|found| {
                    let rel = py::as_posix(found.strip_prefix(&path).unwrap_or(&found));
                    (
                        crate::identity_assertions::join_display(&shown, &rel),
                        found,
                    )
                })
                .collect()
        };
        for (shown, file) in paths {
            match scan(&file) {
                Ok(found) => {
                    for (line, name) in found {
                        findings += 1;
                        let _ = writeln!(out, "{shown}:{line}: {name}: theme state is not pinned");
                    }
                }
                Err(error) => {
                    let _ = writeln!(err, "check_theme_test_guards: {error}");
                    return 1;
                }
            }
        }
    }
    if findings > 0 {
        let _ = writeln!(
            out,
            "Hold `let _theme = crate::ui::theme::pin_theme_state();` from the"
        );
        let _ = writeln!(
            out,
            "start of the test, or wrap its theme reads in `with_theme_state`."
        );
    }
    i32::from(findings > 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn findings(source: &str) -> Vec<String> {
        test_bodies(source)
            .unwrap()
            .into_iter()
            .filter(|(_, _, body)| unpinned(body).unwrap())
            .map(|(name, _, _)| name)
            .collect()
    }

    #[test]
    fn render_and_projected_colors_need_a_pin() {
        let source = "
            #[test] fn render_color() {
                terminal.draw(|f| render(f));
                assert_eq!(cell.fg, theme::green());
            }
            #[test] fn projected_color() {
                let row = compute_row();
                assert_eq!(row.color, crate::ui::theme::status_running());
            }
        ";
        assert_eq!(findings(source), vec!["render_color", "projected_color"]);
    }

    #[test]
    fn pin_and_wrapper_cover_reads() {
        let source = "
            #[test] fn pinned() {
                let _theme = crate::ui::theme::pin_theme_state();
                terminal.draw(|f| render(f));
                assert_eq!(cell.fg, theme::green());
            }
            #[test] fn wrapped() {
                theme::with_theme_state(|| {
                    theme::set_theme_by_name(\"goth\");
                    assert_eq!(cell.fg, theme::green());
                });
            }
            #[test] fn internal_guard() {
                let _guard = theme_test_guard();
                set_theme_by_index(0);
            }
        ";
        assert!(findings(source).is_empty());
    }

    #[test]
    fn ineffective_pins_and_outside_wrapper_are_rejected() {
        for pin in [
            "theme::pin_theme_state();",
            "let _ = theme::pin_theme_state();",
            "{ let _theme = theme::pin_theme_state(); }",
            "theme::with_theme_state(|| { render(); });",
            "// let _theme = theme::pin_theme_state();\n",
        ] {
            let source = format!("#[test] fn unpinned() {{{pin}theme::green();}}");
            assert_eq!(findings(&source), vec!["unpinned"], "{pin}");
        }
        let source = "
            #[test] fn too_late() {
                theme::green();
                let _theme = theme::pin_theme_state();
            }
        ";
        assert_eq!(findings(source), vec!["too_late"]);
    }

    #[test]
    fn async_attributes_multiline_signature_and_unqualified_setter() {
        let source = "
            #[tokio::test(flavor = \"current_thread\")]
            #[ignore]
            async fn changes_theme()
            {
                set_theme_by_name(\"goth\");
            }
        ";
        assert_eq!(findings(source), vec!["changes_theme"]);
    }

    #[test]
    fn ignores_literals_comments_and_non_tests() {
        let source = r###"
            fn production() { theme::green(); }
            #[test] fn literals() {
                let raw = r##"} theme::green() " {"##;
                let escaped = "quote \" theme::green() \\
                               continuation }";
                let chars = ('}', '\u{7b}', '"');
                // theme::set_theme_by_name("goth"); }
                /* theme::green(); { */
            }
            #[test] fn next_test() { theme::green(); }
        "###;
        assert_eq!(findings(source), vec!["next_test"]);
    }

    #[test]
    fn catalog_lookups_and_types_do_not_need_a_pin() {
        let source = "
            #[test] fn metadata() {
                assert_eq!(theme::theme_count(), 14);
                theme::theme_display_name(0);
                let role = theme::ThemeRole::PrimaryText;
            }
        ";
        assert!(findings(source).is_empty());
    }

    #[test]
    fn the_cli_names_file_line_and_test() {
        let dir = tempfile::tempdir().unwrap();
        crate::testutil::write(
            dir.path(),
            "crates/rsi/src/a.rs",
            "\n#[test] fn t() { theme::green(); }\n",
        );
        let ctx = Ctx::new(dir.path(), &[]);
        let (code, out, _) = crate::testutil::run(main, &[], &ctx);
        assert_eq!(code, 1);
        assert!(
            out.starts_with(
                "crates/rsi/src/a.rs:2: t: theme state is not pinned\nHold `let _theme"
            ),
            "{out}"
        );
    }

    #[test]
    fn the_repository_rsi_sources_pass() {
        let ctx = Ctx::new(crate::testutil::repo_root(), &[]);
        let (code, out, err) = crate::testutil::run(main, &[], &ctx);
        assert_eq!(code, 0, "{out}{err}");
    }
}
