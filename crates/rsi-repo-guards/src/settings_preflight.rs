//! Cheap settings omissions before scoped-test compiles (#470).
//!
//! The Rust audit/manual tests remain authoritative. This source-text guard checks
//! audit membership, manual row text and whether manual inputs moved without their
//! outputs; it does not duplicate the Rust manual renderer. Desktop drift uses the
//! existing generator verbatim. Only settings-related diffs run these checks.
//!
//! Usage: `rsi-repo-guard settings-preflight [BASE [HEAD]]` (defaults `origin/rolling`, `HEAD`).

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::io::Write;
use std::path::Path;

use crate::cli;
use crate::re::{Lazy, Re};
use crate::{Ctx, py};

pub const REGISTRY: &str = "crates/rsi/src/settings_registry.rs";
pub const AUDIT: &str = "docs/settings-live-apply-audit.md";
pub const MANUALS: [&str; 2] = ["docs/manual/rsi-manual.md", "docs/manual/rsi-manual.html"];
const KEYBINDINGS: &str = "docs/keybindings.md";
pub const DESKTOP: &str = "desktop/src/settings.generated.ts";
const GENERATOR: &str = "desktop/scripts/settings.mjs";
const REGENERATE: &str = "node desktop/scripts/settings.mjs";
pub const AUDIT_FIX: &str = "${EDITOR:-vi} docs/settings-live-apply-audit.md";
const TRIGGERS: &[&str] = &[
    REGISTRY,
    AUDIT,
    MANUALS[0],
    MANUALS[1],
    KEYBINDINGS,
    DESKTOP,
    GENERATOR,
    "crates/rsi/src/settings.rs",
    "crates/rsi/src/overlay/daemon_text_editor.rs",
    "crates/rsi-common/src/daemon_config_catalog.rs",
];
static TOKEN: Lazy =
    Lazy::new(|| Re::new(r#"(?s)"(?:\\.|[^"\\])*"|//[^\n]*|/\*.*?\*/|::|\w+|[^\s]"#));
static TABLE: Lazy =
    Lazy::new(|| Re::new(r"pub\s+static\s+SETTINGS\s*:\s*&\[SettingSpec\]\s*=\s*&\["));
static CLOSE_TAGS: Lazy = Lazy::new(|| Re::new(r"</(?:td|tr|p|h[1-6])>"));
static TAGS: Lazy = Lazy::new(|| Re::new(r"<[^>]*>"));
static CONTINUATION: Lazy = Lazy::new(|| Re::new(r"\\\r?\n\s*"));
static CHARREF: Lazy =
    Lazy::new(|| Re::new(r"&(#[0-9]+;?|#[xX][0-9a-fA-F]+;?|[^\t\n\x0c <&#;]{1,32};?)"));

/// One `SettingSpec` row: field name -> its value tokens.
pub type Row = BTreeMap<String, Vec<String>>;

/// Read literal `SettingSpec` fields, ignoring comments and formatting.
///
/// Fail closed if the registry changes shape; the Rust test is the authority.
/// Tokens keep braces/commas inside strings from looking like Rust syntax.
pub fn registry_rows(source: &str) -> Result<Vec<Row>, String> {
    let Some(table) = TABLE.search(source) else {
        return Err("cannot find the SETTINGS table".into());
    };
    let tokens: Vec<String> = TOKEN
        .find_all(&source[crate::re::end(&table)..])
        .iter()
        .map(|caps| crate::re::group(caps, 0).to_string())
        .filter(|token| !token.starts_with("//") && !token.starts_with("/*"))
        .collect();
    let at = |pos: usize| {
        tokens
            .get(pos)
            .map(String::as_str)
            .ok_or_else(|| "list index out of range".to_string())
    };
    let mut rows = Vec::new();
    let mut pos = 0;
    while at(pos)? != "]" {
        if at(pos)? == "," {
            pos += 1;
            continue;
        }
        if tokens
            .get(pos..pos + 2)
            .is_none_or(|pair| pair != ["SettingSpec", "{"])
        {
            return Err("expected a literal SettingSpec row".into());
        }
        pos += 2;
        let mut fields = Row::new();
        while at(pos)? != "}" {
            let available = tokens.len().saturating_sub(pos).min(2);
            if available < 2 {
                return Err(format!(
                    "not enough values to unpack (expected 2, got {available})"
                ));
            }
            let (name, colon) = (tokens[pos].clone(), &tokens[pos + 1]);
            if colon != ":" {
                return Err("expected a SettingSpec field".into());
            }
            pos += 2;
            let mut value = Vec::new();
            let mut depth: i64 = 0;
            while depth != 0 || ![",", "}"].contains(&at(pos)?) {
                let token = at(pos)?;
                depth += i64::from(["(", "[", "{"].contains(&token));
                depth -= i64::from([")", "]", "}"].contains(&token));
                value.push(token.to_string());
                pos += 1;
            }
            fields.insert(name, value);
            if at(pos)? == "," {
                pos += 1;
            }
        }
        rows.push(fields);
        pos += 1;
    }
    if rows.is_empty() {
        return Err("SETTINGS has no rows".into());
    }
    Ok(rows)
}

fn manual_inputs(rows: &[Row]) -> Vec<Row> {
    // Keywords drive search, not the manual. Reformatting is already tokenized away.
    rows.iter()
        .map(|row| {
            row.iter()
                .filter(|(k, _)| *k != "keywords")
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect()
        })
        .collect()
}

fn plain(text: &str) -> String {
    let text = text.replace("\\|", "|").replace('`', "").replace("**", "");
    py::split_whitespace(&text).collect::<Vec<_>>().join(" ")
}

fn field<'r>(row: &'r Row, name: &str) -> Result<&'r [String], String> {
    row.get(name)
        .map(Vec::as_slice)
        .ok_or_else(|| format!("'{name}'"))
}

/// Python `html.unescape` for the references a generated manual holds: numeric
/// references and the common named ones.
#[must_use]
pub fn html_unescape(text: &str) -> String {
    if !text.contains('&') {
        return text.to_string();
    }
    CHARREF.sub_with(text, &|caps| {
        let reference = crate::re::group(caps, 1);
        if let Some(number) = reference.strip_prefix('#') {
            let digits = number.trim_end_matches(';');
            let value = match digits.strip_prefix(['x', 'X']) {
                Some(hex) => u32::from_str_radix(hex, 16).ok(),
                None => digits.parse::<u32>().ok(),
            };
            return charref(value.unwrap_or(u32::MAX));
        }
        const NAMED: &[(&str, &str)] = &[
            ("amp", "&"),
            ("lt", "<"),
            ("gt", ">"),
            ("quot", "\""),
            ("apos", "'"),
            ("nbsp", "\u{a0}"),
            ("copy", "\u{a9}"),
            ("reg", "\u{ae}"),
            ("middot", "\u{b7}"),
            ("times", "\u{d7}"),
            ("ndash", "\u{2013}"),
            ("mdash", "\u{2014}"),
            ("lsquo", "\u{2018}"),
            ("rsquo", "\u{2019}"),
            ("ldquo", "\u{201c}"),
            ("rdquo", "\u{201d}"),
            ("bull", "\u{2022}"),
            ("hellip", "\u{2026}"),
            ("larr", "\u{2190}"),
            ("rarr", "\u{2192}"),
            ("uarr", "\u{2191}"),
            ("darr", "\u{2193}"),
        ];
        // Legacy names also match without the `;` (longest prefix first).
        const LEGACY: &[&str] = &[
            "amp", "lt", "gt", "quot", "nbsp", "copy", "reg", "times", "middot",
        ];
        if let Some(name) = reference.strip_suffix(';')
            && let Some((_, value)) = NAMED.iter().find(|(known, _)| *known == name)
        {
            return (*value).to_string();
        }
        for end in (1..=reference.len().min(6)).rev() {
            let Some(prefix) = reference.get(..end) else {
                continue;
            };
            if LEGACY.contains(&prefix)
                && let Some((_, value)) = NAMED.iter().find(|(known, _)| *known == prefix)
            {
                return format!("{value}{}", &reference[end..]);
            }
        }
        format!("&{reference}")
    })
}

/// `html._replace_charref` for a numeric reference.
fn charref(number: u32) -> String {
    const CP1252: [u32; 32] = [
        0x20ac, 0x81, 0x201a, 0x0192, 0x201e, 0x2026, 0x2020, 0x2021, 0x02c6, 0x2030, 0x0160,
        0x2039, 0x0152, 0x8d, 0x017d, 0x8f, 0x90, 0x2018, 0x2019, 0x201c, 0x201d, 0x2022, 0x2013,
        0x2014, 0x02dc, 0x2122, 0x0161, 0x203a, 0x0153, 0x9d, 0x017e, 0x0178,
    ];
    let mapped = match number {
        0 => Some(0xfffd),
        0x0d => Some(0x0d),
        0x80..=0x9f => Some(CP1252[(number - 0x80) as usize]),
        _ => None,
    };
    if let Some(mapped) = mapped {
        return char::from_u32(mapped).map_or_else(String::new, String::from);
    }
    if (0xd800..=0xdfff).contains(&number) || number > 0x10ffff {
        return "\u{fffd}".to_string();
    }
    if matches!(number, 0x1..=0x8 | 0xb | 0xe..=0x1f | 0x7f..=0x9f | 0xfdd0..=0xfdef)
        || (number & 0xfffe) == 0xfffe
    {
        return String::new();
    }
    char::from_u32(number).map_or_else(String::new, String::from)
}

/// Actionable source omissions; `read(path)` returns candidate text.
pub fn source_checks(
    read: &dyn Fn(&str) -> Result<String, String>,
    changed: &HashSet<String>,
    base_registry: Option<&str>,
) -> Result<Vec<String>, String> {
    let mut errors = Vec::new();
    let rows = registry_rows(&read(REGISTRY)?)?;
    let audit = read(AUDIT)?;
    for row in &rows {
        let id = field(row, "id")?;
        if id.len() != 3 || id[0] != "SettingId" || id[1] != "::" {
            return Err("expected id: SettingId::Variant".into());
        }
        let marker = format!("(`{}`)", id[2]);
        if !audit.contains(&marker) {
            errors.push(format!(
                "{} audit row missing ({marker}). Fix: {AUDIT_FIX} ; add its owner, consumer and live-apply outcome.",
                id[2]
            ));
        }
    }
    for path in MANUALS {
        let mut text = read(path)?;
        if path.ends_with(".html") {
            text = CLOSE_TAGS.sub(&text, " ").into_owned();
            text = html_unescape(&TAGS.sub(&text, ""));
        }
        let text = plain(&text);
        let mut missing = BTreeSet::new();
        for row in &rows {
            for name in ["label", "summary", "detail"] {
                for token in field(row, name)? {
                    if token.starts_with('"') {
                        // Rust's escaped newline continuation is absent from JSON strings.
                        let literal = CONTINUATION.sub(token, "");
                        let decoded: String = serde_json::from_str(&literal)
                            .map_err(|error| format!("{error}: {literal}"))?;
                        if !text.contains(&plain(&decoded)) {
                            missing.insert(field(row, "id")?.last().cloned().unwrap_or_default());
                        }
                    }
                }
            }
        }
        if !missing.is_empty() {
            let missing: Vec<String> = missing.into_iter().collect();
            errors.push(format!(
                "{path} omits settings text for {}. Regenerate: make manual",
                missing.join(", ")
            ));
        }
    }
    if let Some(base) = base_registry
        && manual_inputs(&registry_rows(base)?) != manual_inputs(&rows)
    {
        let unchanged: Vec<&str> = MANUALS
            .iter()
            .copied()
            .filter(|path| !changed.contains(*path))
            .collect();
        if !unchanged.is_empty() {
            errors.push(format!(
                "Settings manual inputs changed without updating {}. Regenerate: make manual",
                unchanged.join(", ")
            ));
        }
    }
    Ok(errors)
}

/// Runs the desktop drift check in the repository: (exit code, stdout, stderr).
pub type DesktopCheck<'a> = &'a dyn Fn(&Path) -> std::io::Result<(i32, String, String)>;

fn node_check(repo: &Path) -> std::io::Result<(i32, String, String)> {
    let done = std::process::Command::new("node")
        .args([GENERATOR, "--check"])
        .current_dir(repo)
        .output()?;
    let text = |bytes: &[u8]| py::universal_newlines(&String::from_utf8_lossy(bytes));
    Ok((
        done.status.code().unwrap_or(1),
        text(&done.stdout),
        text(&done.stderr),
    ))
}

/// The guard proper for the files a change touches.
pub fn preflight(
    repo: &Path,
    changed: &HashSet<String>,
    base_registry: Option<&str>,
    desktop: DesktopCheck<'_>,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> i32 {
    if !TRIGGERS.iter().any(|trigger| changed.contains(*trigger)) {
        let _ = writeln!(out, "settings preflight: skipped (unrelated diff)");
        return 0;
    }
    let read = |path: &str| py::read_text(&repo.join(path)).map_err(|error| error.to_string());
    let mut errors = source_checks(&read, changed, base_registry).unwrap_or_else(|error| {
        vec![format!(
            "Cannot check settings sources: {error}. Fix: ${{EDITOR:-vi}} {REGISTRY} {AUDIT} \
             crates/rsi-repo-guards/src/settings_preflight.rs; check SETTINGS rows and the source parser. \
             Regenerate: make manual"
        )]
    });
    let _ = writeln!(out, "settings preflight: node {GENERATOR} --check");
    let _ = out.flush();
    match desktop(repo) {
        Ok((0, _, _)) => {}
        Ok((_, stdout, stderr)) => {
            errors.push(format!("{}\nRegenerate: {REGENERATE}", py::strip(&format!("{stdout}{stderr}"))));
        }
        Err(error) => errors.push(format!(
            "Cannot run desktop settings check: {error}. After providing Node.js, regenerate: {REGENERATE}"
        )),
    }
    for error in &errors {
        let _ = writeln!(err, "settings preflight: {error}");
    }
    if errors.is_empty() {
        let _ = writeln!(
            out,
            "settings preflight: audit/manual source coverage and desktop drift passed; \
             Rust tests still verify full manual drift and live-apply semantics"
        );
    }
    i32::from(!errors.is_empty())
}

/// Entry point: `settings-preflight [BASE [HEAD]]`.
pub fn main(args: &[String], ctx: &Ctx, out: &mut dyn Write, err: &mut dyn Write) -> i32 {
    let parsed = match cli::parse(
        "check_settings_preflight",
        "[-h] [base] [head]",
        args,
        &[],
        &[],
        Some(2),
        err,
    ) {
        Ok(parsed) => parsed,
        Err(exit) => return exit.code(),
    };
    let base = parsed
        .positionals
        .first()
        .map_or("origin/rolling", String::as_str);
    let head = parsed.positionals.get(1).map_or("HEAD", String::as_str);
    let repo = ctx.cwd.clone();
    let git = |args: &[&str]| -> Result<Vec<u8>, String> {
        match ctx.git().arg("-C").arg(&repo).args(args).output() {
            Ok(done) if done.status.success() => Ok(done.stdout),
            Ok(done) => Err(format!(
                "git {} failed: {}",
                args.join(" "),
                py::strip(&String::from_utf8_lossy(&done.stderr))
            )),
            Err(error) => Err(format!("git {}: {error}", args.join(" "))),
        }
    };
    let changed: HashSet<String> =
        match git(&["diff", "--name-only", "--no-renames", "-z", base, head]) {
            Ok(names) => names
                .split(|b| *b == 0)
                .filter(|name| !name.is_empty())
                .map(|name| String::from_utf8_lossy(name).into_owned())
                .collect(),
            Err(error) => {
                let _ = writeln!(err, "check_settings_preflight: {error}");
                return 1;
            }
        };
    let base_registry = if changed.contains(REGISTRY) {
        match git(&["show", &format!("{base}:{REGISTRY}")]).map(String::from_utf8) {
            Ok(Ok(text)) => Some(text),
            Ok(Err(error)) => {
                let _ = writeln!(err, "check_settings_preflight: {base}:{REGISTRY}: {error}");
                return 1;
            }
            Err(error) => {
                let _ = writeln!(err, "check_settings_preflight: {error}");
                return 1;
            }
        }
    } else {
        None
    };
    preflight(
        &repo,
        &changed,
        base_registry.as_deref(),
        &node_check,
        out,
        err,
    )
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;

    use super::*;
    use crate::testutil::write;

    const ROW: &str = "
pub static SETTINGS: &[SettingSpec] = &[
    // id: SettingId::CommentOnly,
    SettingSpec {
        id: SettingId::WorkerBuildReserveGb,
        section: SettingsSection::Build,
        label: \"Worker build reserve\",
        summary: \"Reserve space for `builds`; keep {room}, too.\",
        detail: Some(\"Applies live.\"),
        keywords: &[\"build\", \"reserve\"],
        kind: SettingKind::Cycle,
        owner: SettingOwner::Daemon(\"worker_build_reserve_gb\"),
        apply: SettingApply::Daemon(ApplyClass::Live),
        destructive: false,
    },
];
";
    const MANUAL_TEXT: &str =
        "Worker build reserve Reserve space for builds; keep {room}, too. Applies live.";

    struct Fixture {
        dir: tempfile::TempDir,
    }

    impl Fixture {
        fn new() -> Self {
            let fixture = Self {
                dir: tempfile::tempdir().unwrap(),
            };
            fixture.write(REGISTRY, ROW);
            fixture.write(
                AUDIT,
                "| Worker build reserve (`WorkerBuildReserveGb`) | live consumer |",
            );
            for path in MANUALS {
                fixture.write(path, MANUAL_TEXT);
            }
            fixture
        }

        fn write(&self, path: &str, text: &str) {
            write(self.dir.path(), path, text);
        }

        /// (exit code, stdout + stderr, desktop check calls).
        fn run(
            &self,
            changed: Option<&[&str]>,
            base: Option<&str>,
            desktop_code: i32,
        ) -> (i32, String, usize) {
            let calls = RefCell::new(0);
            let desktop = |repo: &Path| {
                assert_eq!(repo, self.dir.path());
                *calls.borrow_mut() += 1;
                Ok((
                    desktop_code,
                    String::new(),
                    if desktop_code == 0 {
                        String::new()
                    } else {
                        "desktop drift".into()
                    },
                ))
            };
            let changed: HashSet<String> = changed
                .unwrap_or(&[REGISTRY])
                .iter()
                .map(|path| (*path).to_string())
                .collect();
            let (mut out, mut err) = (Vec::new(), Vec::new());
            let code = preflight(
                self.dir.path(),
                &changed,
                base,
                &desktop,
                &mut out,
                &mut err,
            );
            let text = format!(
                "{}{}",
                String::from_utf8_lossy(&out),
                String::from_utf8_lossy(&err)
            );
            (code, text, calls.into_inner())
        }
    }

    #[test]
    fn settings_diff_missing_audit_row_fails_with_fix_command() {
        let f = Fixture::new();
        f.write(AUDIT, "| Existing setting (`Existing`) | live consumer |");
        let (code, output, calls) = f.run(None, None, 0);
        assert_eq!(code, 1, "{output}");
        assert!(
            output.contains("WorkerBuildReserveGb audit row missing"),
            "{output}"
        );
        assert!(output.contains(&format!("Fix: {AUDIT_FIX}")), "{output}");
        assert!(
            output.contains("settings preflight: node desktop/scripts/settings.mjs --check"),
            "{output}"
        );
        assert_eq!(calls, 1);
    }

    #[test]
    fn complete_settings_diff_passes() {
        let f = Fixture::new();
        let base = ROW.replace("WorkerBuildReserveGb", "OldSetting");
        let (code, output, _) = f.run(
            Some(&[REGISTRY, AUDIT, MANUALS[0], MANUALS[1], DESKTOP]),
            Some(&base),
            0,
        );
        assert_eq!(code, 0, "{output}");
        assert!(
            output.contains("audit/manual source coverage and desktop drift passed"),
            "{output}"
        );
    }

    #[test]
    fn unrelated_diff_skips_without_reading_settings_or_running_node() {
        let f = Fixture::new();
        std::fs::remove_file(f.dir.path().join(REGISTRY)).unwrap();
        let (code, output, calls) = f.run(Some(&["crates/rsid/src/unrelated.rs"]), None, 0);
        assert_eq!(code, 0, "{output}");
        assert!(output.contains("skipped (unrelated diff)"), "{output}");
        assert_eq!(calls, 0);
    }

    #[test]
    fn desktop_drift_and_audit_omission_both_report_exact_commands() {
        let f = Fixture::new();
        f.write(AUDIT, "");
        let (code, output, _) = f.run(None, None, 1);
        assert_eq!(code, 1, "{output}");
        assert!(output.contains(AUDIT_FIX), "{output}");
        assert!(
            output.contains("Regenerate: node desktop/scripts/settings.mjs"),
            "{output}"
        );
    }

    #[test]
    fn manual_row_omission_names_regeneration_command() {
        let f = Fixture::new();
        f.write(MANUALS[1], "<html><body>Old manual</body></html>");
        let (code, output, _) = f.run(None, None, 0);
        assert_eq!(code, 1, "{output}");
        assert!(
            output.contains(&format!(
                "{} omits settings text for WorkerBuildReserveGb",
                MANUALS[1]
            )),
            "{output}"
        );
        assert!(output.contains("Regenerate: make manual"), "{output}");
    }

    #[test]
    fn changed_apply_class_requires_manual_outputs_to_move() {
        let f = Fixture::new();
        let (code, output, _) = f.run(
            None,
            Some(&ROW.replace("ApplyClass::Live", "ApplyClass::NextSpawn")),
            0,
        );
        assert_eq!(code, 1, "{output}");
        assert!(
            output.contains("Settings manual inputs changed without updating"),
            "{output}"
        );
        assert!(output.contains("Regenerate: make manual"), "{output}");
    }

    #[test]
    fn keywords_and_formatting_do_not_require_manual_changes() {
        let f = Fixture::new();
        let base = ROW
            .replace("\"reserve\"", "\"disk\"")
            .replace("id: SettingId", "id:  SettingId");
        let (code, output, _) = f.run(None, Some(&base), 0);
        assert_eq!(code, 0, "{output}");
    }

    #[test]
    fn generated_settings_artifact_diff_alone_runs_the_guard() {
        let f = Fixture::new();
        let (code, output, calls) = f.run(Some(&[DESKTOP]), None, 0);
        assert_eq!(code, 0, "{output}");
        assert_eq!(calls, 1);
    }

    #[test]
    fn html_inline_markup_preserves_source_punctuation() {
        let f = Fixture::new();
        f.write(
            MANUALS[1],
            "<td>Worker build reserve</td><td>Reserve space for <code>builds</code>; keep {room}, too. Applies live.</td>",
        );
        let (code, output, _) = f.run(None, None, 0);
        assert_eq!(code, 0, "{output}");
    }

    #[test]
    fn unrecognized_registry_shape_fails_closed() {
        let f = Fixture::new();
        f.write(
            REGISTRY,
            "pub static SETTINGS: &[SettingSpec] = &[make_row!()];",
        );
        let (code, output, _) = f.run(None, None, 0);
        assert_eq!(code, 1, "{output}");
        assert!(output.contains("Cannot check settings sources"), "{output}");
        assert!(output.contains("make manual"), "{output}");
    }

    #[test]
    fn html_unescape_follows_python() {
        assert_eq!(
            html_unescape("a &lt;b&gt; &amp; &quot;c&quot; &#39;d&#x27; &amp e &bogus; &#0;"),
            "a <b> & \"c\" 'd' & e &bogus; \u{fffd}"
        );
    }

    #[test]
    fn the_repository_settings_sources_are_complete() {
        let root = crate::testutil::repo_root();
        let read = |path: &str| py::read_text(&root.join(path)).map_err(|error| error.to_string());
        assert_eq!(source_checks(&read, &HashSet::new(), None), Ok(Vec::new()));
    }
}
