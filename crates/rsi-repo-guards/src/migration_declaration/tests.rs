//! Every intent from test_refresh_migration_declaration_digests.py, without an interpreter.
use super::*;
use crate::testutil::{run, write};
use serde_json::json;
const NAME: &str = "tools/provisional-migrations/test.json";
const ORIGINAL: &str = "// existing documentation\nif version < 130 {\n}\n";
fn entry(name: &str) -> Value {
    json!({"path":name,"path_template":name,"source_blob":guard::digest(ORIGINAL),"sites":[{"anchor":"if version < 130 {","scope":"unit","replacement":"if version < ${VERSION} {"}]})
}
struct Fixture {
    dir: tempfile::TempDir,
    declaration: Value,
}
impl Fixture {
    fn new() -> Self {
        let dir = if cfg!(unix) {
            tempfile::tempdir_in("/var/tmp").unwrap()
        } else {
            tempfile::tempdir().unwrap()
        };
        let f = Self {
            dir,
            declaration: json!({"schema_version":1,"version":130,"pending_predecessors":[129],"files":[entry("docs/contracts.md"),entry("other.rs")]}),
        };
        for p in ["docs/contracts.md", "other.rs"] {
            write(f.dir.path(), p, ORIGINAL);
        }
        f.save();
        f
    }
    fn save(&self) -> Vec<u8> {
        let mut out = Vec::new();
        let format = serde_json::ser::PrettyFormatter::with_indent(b"   ");
        // Nonstandard spacing and CRLF prove that refresh does not reserialize.
        use serde::Serialize;
        self.declaration
            .serialize(&mut serde_json::Serializer::with_formatter(
                &mut out, format,
            ))
            .unwrap();
        let text = String::from_utf8(out)
            .unwrap()
            .lines()
            .map(|line| format!("{}\r\n", line.replacen("\": ", "\" : ", 1)))
            .collect::<String>();
        write(self.dir.path(), NAME, &text);
        text.into_bytes()
    }
    fn bytes(&self) -> Vec<u8> {
        std::fs::read(self.dir.path().join(NAME)).unwrap()
    }
    fn change(&self) -> String {
        let s = format!("{ORIGINAL}// unrelated merged documentation\n");
        write(self.dir.path(), "docs/contracts.md", &s);
        s
    }
    fn refused(&self, needle: &str) {
        let before = self.bytes();
        let e = refresh(self.dir.path(), NAME, false).unwrap_err();
        assert!(e.contains(needle), "{e}");
        assert_eq!(self.bytes(), before);
    }
}
#[test]
fn stale_digest_refresh_preserves_all_other_bytes_and_is_idempotent() {
    let f = Fixture::new();
    let before = f.bytes();
    let changed = f.change();
    assert_eq!(
        refresh(f.dir.path(), NAME, false).unwrap(),
        ["docs/contracts.md"]
    );
    let old = guard::digest(ORIGINAL);
    let new = guard::digest(&changed);
    let expected = String::from_utf8(before)
        .unwrap()
        .replacen(&old, &new, 1)
        .into_bytes();
    assert_eq!(f.bytes(), expected);
    assert_eq!(
        std::fs::read(f.dir.path().join("docs/contracts.md")).unwrap(),
        changed.as_bytes()
    );
    assert!(refresh(f.dir.path(), NAME, false).unwrap().is_empty());
    assert_eq!(f.bytes(), expected);
}
#[test]
fn moved_changed_duplicate_and_current_digest_missing_anchors_refuse_atomically() {
    for content in [
        "// moved elsewhere\n".into(),
        ORIGINAL.replace("130", "131"),
        ORIGINAL.repeat(2),
    ] {
        let f = Fixture::new();
        write(f.dir.path(), "docs/contracts.md", &content);
        write(f.dir.path(), "moved.rs", ORIGINAL);
        f.refused("anchor is missing");
    }
    let f = Fixture::new();
    f.change();
    std::fs::remove_file(f.dir.path().join("other.rs")).unwrap();
    f.refused("file is missing");
    let mut f = Fixture::new();
    f.declaration["files"][1]["sites"][0]["anchor"] = json!("if version < 130 { absent");
    f.declaration["files"][1]["sites"][0]["replacement"] =
        json!("if version < ${VERSION} { absent");
    f.save();
    f.change();
    f.refused("anchor is missing");
}
#[test]
fn substitutions_overlap_template_duplicates_and_json_keys_are_strict() {
    let mut f = Fixture::new();
    f.declaration["files"][0]["sites"][0]["replacement"] = json!("if version <= ${VERSION} {");
    f.save();
    f.refused("pure version substitution");
    let mut f = Fixture::new();
    f.declaration["files"][0]["sites"]
        .as_array_mut()
        .unwrap()
        .push(
            json!({"anchor":"version < 130","replacement":"version < ${VERSION}","scope":"unit"}),
        );
    f.save();
    f.refused("overlapping");
    let mut f = Fixture::new();
    f.declaration["files"][0]["path_template"] = json!("moved.rs");
    f.save();
    f.refused("template does not match");
    let mut f = Fixture::new();
    let duplicate = f.declaration["files"][0].clone();
    f.declaration["files"]
        .as_array_mut()
        .unwrap()
        .push(duplicate);
    f.save();
    f.refused("duplicate declared file");
    let f = Fixture::new();
    let text = String::from_utf8(f.bytes())
        .unwrap()
        .replace("\"version\" : 130", "\"version\" : 130, \"version\": 130");
    write(f.dir.path(), NAME, &text);
    f.refused("duplicate declaration key: version");
    let f = Fixture::new();
    let text = String::from_utf8(f.bytes()).unwrap().replace(
        "\"version\" : 130",
        "\"version\" : 130, \"vers\\u0069on\": 130",
    );
    write(f.dir.path(), NAME, &text);
    f.refused("duplicate declaration key: version");
}
#[test]
fn binary_large_symlink_and_unsafe_paths_refuse() {
    for bytes in [
        format!("\0{ORIGINAL}").into_bytes(),
        vec![b'x'; MAX_SOURCE_BLOB_BYTES + 1],
    ] {
        let f = Fixture::new();
        std::fs::write(f.dir.path().join("docs/contracts.md"), bytes).unwrap();
        f.refused("too large or binary");
    }
    #[cfg(unix)]
    {
        let f = Fixture::new();
        let source = f.dir.path().join("docs/contracts.md");
        std::fs::remove_file(&source).unwrap();
        std::os::unix::fs::symlink(f.dir.path().join("other.rs"), source).unwrap();
        f.refused("symlink");
        let f = Fixture::new();
        std::fs::remove_file(f.dir.path().join("docs/contracts.md")).unwrap();
        std::fs::remove_dir(f.dir.path().join("docs")).unwrap();
        std::os::unix::fs::symlink(f.dir.path().join("tools"), f.dir.path().join("docs")).unwrap();
        f.refused("symlink");
    }
    for p in [
        "../outside",
        "/tmp/outside",
        "a//b",
        "a/./b",
        "a/../b",
        "a b",
    ] {
        assert!(path(p).is_err());
    }
}
#[test]
fn quoted_source_blob_inside_anchor_is_not_a_field() {
    let mut f = Fixture::new();
    let anchor = "if version < 130 { // \"source_blob\": \"text\"";
    f.declaration["files"][0]["sites"][0]["anchor"] = json!(anchor);
    f.declaration["files"][0]["sites"][0]["replacement"] = json!(anchor.replace("130", TOKEN));
    let source = format!("{anchor}\n");
    write(f.dir.path(), "docs/contracts.md", &source);
    let before = f.save();
    refresh(f.dir.path(), NAME, false).unwrap();
    let expected = String::from_utf8(before)
        .unwrap()
        .replacen(&guard::digest(ORIGINAL), &guard::digest(source), 1)
        .into_bytes();
    assert_eq!(f.bytes(), expected);
    assert_eq!(
        serde_json::from_slice::<Value>(&f.bytes()).unwrap()["files"][0]["sites"],
        f.declaration["files"][0]["sites"]
    );
}
#[test]
fn cli_check_refresh_and_refusal_exit_codes() {
    let f = Fixture::new();
    f.change();
    let before = f.bytes();
    let ctx = Ctx::new(f.dir.path(), &[]);
    assert_eq!(run(main, &[NAME, "--check"], &ctx).0, 1);
    assert_eq!(f.bytes(), before);
    let (code, out, _) = run(main, &[NAME], &ctx);
    assert_eq!(code, 0);
    assert!(out.contains("refreshed source_blob values: 1"));
    assert_eq!(run(main, &[NAME, "--check"], &ctx).0, 0);
    std::fs::remove_file(f.dir.path().join("other.rs")).unwrap();
    let (code, _, err) = run(main, &[NAME], &ctx);
    assert_eq!(code, 2);
    assert!(err.contains("refresh refused"));
}
