//! `release-target`: the cargo target directory a build receipt may deploy from
//! (hub #3513/#3514/#3715/#3716). Port of the embedded python block of
//! `release_guard_receipt_target` in `scripts/release-target-guard.sh`.
//!
//! `rsi-repo-guard release-target <receipt-file> <override-dir|""> <allowed-dir>...`
//! prints the canonical target directory on stdout (exit 0) before anything
//! reads, locks or executes it. The receipt's `target_dir` (bare or under
//! `result`, #282) is untrusted input:
//!
//! - the key present with anything but a non-empty absolute path string
//!   (`null`, a number, a list, an object) is refused naming `target_dir`; only a
//!   key that is absent means "a receipt from before #282" (hub #3716);
//! - the directory, with symlinks and `..` resolved, must equal one `<allowed-dir>`;
//! - a non-empty override (`--target-dir`) must be absolute, canonical (no `..`
//!   component, no symlink) and an existing directory, and must itself resolve to
//!   one `<allowed-dir>` in every path, so it can never widen the allowed set
//!   (hub #3715). It is the only way to deploy a receipt that names no
//!   `target_dir`; when the receipt names one too, they must agree.
//!
//! Exit 2, printing nothing, for a receipt that names no `target_dir` and no
//! override. Exit 1 with a message on stderr for anything else wrong.

use std::io::Write;
use std::path::Path;

use serde_json::Value;

use crate::{Ctx, py};

/// Why a receipt target was refused: the message for stderr.
type Refusal = String;

pub fn main(args: &[String], _ctx: &Ctx, out: &mut dyn Write, err: &mut dyn Write) -> i32 {
    let [receipt, override_dir, allowed @ ..] = args else {
        let _ = writeln!(
            err,
            "usage: rsi-repo-guard release-target <receipt-file> <override-dir|\"\"> <allowed-dir>..."
        );
        return 1;
    };
    match receipt_target(receipt, override_dir, allowed) {
        Ok(Some(target)) => {
            let _ = writeln!(out, "{target}");
            0
        }
        Ok(None) => 2,
        Err(message) => {
            let _ = writeln!(err, "{message}");
            1
        }
    }
}

/// The canonical target to deploy from; `Ok(None)` when the receipt names none
/// and there is no override.
fn receipt_target(
    receipt: &str,
    override_dir: &str,
    allowed: &[String],
) -> Result<Option<String>, Refusal> {
    if !Path::new(receipt).is_file() {
        return Err(format!("Build receipt {receipt} does not exist (#157)."));
    }
    let data = read_receipt(receipt)?;
    let allowed: Vec<String> = allowed
        .iter()
        .filter(|path| !path.is_empty())
        .map(|path| realpath(path))
        .collect();

    let canonical_override = canonical_override(override_dir, &allowed)?;
    let Some(target) = target_dir_field(&data) else {
        return Ok(canonical_override);
    };
    let target = clean_absolute(target, "the receipt's target_dir")?;
    let canonical = realpath(target);
    match canonical_override {
        Some(chosen) if chosen != canonical => Err(format!(
            "the receipt's target_dir {target} (resolves to {canonical}) differs from --target-dir {override_dir} (#3513)."
        )),
        Some(_) => Ok(Some(canonical)),
        None if allowed.contains(&canonical) => Ok(Some(canonical)),
        None => Err(format!(
            "the receipt's target_dir {target} (resolves to {canonical}) is not an allowed deploy target: \
             the registered release target or this checkout's cargo target (#3513). \
             Deploy a build from there, or one whose target_dir is --target-dir naming it."
        )),
    }
}

fn read_receipt(receipt: &str) -> Result<Value, Refusal> {
    let unreadable = |reason: String| {
        format!(
            "Cannot read build receipt {receipt}: {} (#157).",
            reason.replace('\n', " ")
        )
    };
    let text = std::fs::read(receipt).map_err(|error| unreadable(error.to_string()))?;
    serde_json::from_slice(&text).map_err(|error| unreadable(error.to_string()))
}

/// The receipt's `target_dir` value, bare or under `result`: `None` only when
/// the key is absent. A key that is present with any value, `null` included, is
/// returned so the caller refuses it (hub #3716).
fn target_dir_field(data: &Value) -> Option<&Value> {
    let top = data.as_object();
    if let Some(value) = top.and_then(|map| map.get("target_dir")) {
        return Some(value);
    }
    top?.get("result")?.as_object()?.get("target_dir")
}

/// A `--target-dir` that is valid and allowed, canonicalised; `None` for no override.
fn canonical_override(override_dir: &str, allowed: &[String]) -> Result<Option<String>, Refusal> {
    if override_dir.is_empty() {
        return Ok(None);
    }
    clean_absolute(&Value::String(override_dir.to_string()), "--target-dir")?;
    let canonical = realpath(override_dir);
    if override_dir.split('/').any(|part| part == "..") || canonical != py::normpath(override_dir) {
        return Err(format!(
            "--target-dir {override_dir} must be canonical: no '..' component and no symlink (#3513)."
        ));
    }
    if !Path::new(override_dir).is_dir() {
        return Err(format!(
            "--target-dir {override_dir} is not a directory (#3513)."
        ));
    }
    if !allowed.contains(&canonical) {
        return Err(format!(
            "--target-dir {override_dir} (resolves to {canonical}) is not an allowed deploy target: \
             the registered release target or this checkout's cargo target (#3513, #3715)."
        ));
    }
    Ok(Some(canonical))
}

/// `value` as a non-empty absolute path string without a newline or NUL.
fn clean_absolute<'a>(value: &'a Value, field: &str) -> Result<&'a str, Refusal> {
    let Some(path) = value.as_str().filter(|path| !path.is_empty()) else {
        return Err(format!(
            "{field} must be a non-empty absolute path string, got {} (#3513).",
            value_repr(value)
        ));
    };
    if path.contains(['\n', '\0']) {
        return Err(format!("{field} must not contain a newline (#3513)."));
    }
    if !path.starts_with('/') {
        return Err(format!(
            "{field} must be an absolute path, got {} (#3513).",
            py::repr(path)
        ));
    }
    Ok(path)
}

/// Python's `%r` of a decoded JSON value, for messages.
fn value_repr(value: &Value) -> String {
    match value {
        Value::Null => "None".to_string(),
        Value::Bool(true) => "True".to_string(),
        Value::Bool(false) => "False".to_string(),
        Value::String(text) => py::repr(text),
        other => other.to_string(),
    }
}

/// `os.path.realpath`: symlinks resolved as far as the path exists.
fn realpath(path: &str) -> String {
    py::resolve(Path::new(path)).to_string_lossy().into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::run;

    struct Fixture {
        _dir: tempfile::TempDir,
        root: std::path::PathBuf,
        allowed: std::path::PathBuf,
        foreign: std::path::PathBuf,
    }

    fn fixture() -> Fixture {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let allowed = root.join("allowed");
        let foreign = root.join("foreign");
        std::fs::create_dir_all(&allowed).unwrap();
        std::fs::create_dir_all(&foreign).unwrap();
        Fixture {
            _dir: dir,
            root,
            allowed,
            foreign,
        }
    }

    impl Fixture {
        /// Run the guard on a receipt holding `receipt_json`.
        fn run(&self, receipt_json: &str, override_dir: &str) -> (i32, String, String) {
            let receipt = self.root.join("receipt.json");
            std::fs::write(&receipt, receipt_json).unwrap();
            let allowed = self.allowed.to_string_lossy().into_owned();
            let receipt = receipt.to_string_lossy().into_owned();
            run(
                main,
                &[&receipt, override_dir, &allowed],
                &Ctx::new(&self.root, &[]),
            )
        }

        fn allowed(&self) -> String {
            self.allowed.to_string_lossy().into_owned()
        }

        fn foreign(&self) -> String {
            self.foreign.to_string_lossy().into_owned()
        }
    }

    fn receipt_with(target: &str) -> String {
        format!(r#"{{"exit_code":0,"target_dir":{target}}}"#)
    }

    fn quoted(path: &str) -> String {
        Value::String(path.to_string()).to_string()
    }

    #[test]
    fn a_receipt_naming_the_allowed_target_succeeds() {
        let f = fixture();
        let (code, out, _) = f.run(&receipt_with(&quoted(&f.allowed())), "");
        assert_eq!((code, out), (0, format!("{}\n", f.allowed())));
    }

    #[test]
    fn a_receipt_naming_a_foreign_target_is_refused() {
        let f = fixture();
        let (code, out, err) = f.run(&receipt_with(&quoted(&f.foreign())), "");
        assert_eq!((code, out.as_str()), (1, ""));
        assert!(err.contains("not an allowed deploy target"), "{err}");
    }

    // (a) a legacy receipt plus a foreign --target-dir is refused before anything else.
    #[test]
    fn a_legacy_receipt_with_a_foreign_override_is_refused() {
        let f = fixture();
        let (code, out, err) = f.run(r#"{"exit_code":0}"#, &f.foreign());
        assert_eq!((code, out.as_str()), (1, ""));
        assert!(err.contains("--target-dir"), "{err}");
        assert!(err.contains("not an allowed deploy target"), "{err}");
    }

    // (b) the receipt names the foreign dir and the override says the same: still refused.
    #[test]
    fn an_override_identical_to_a_foreign_receipt_target_is_refused() {
        let f = fixture();
        let (code, out, err) = f.run(&receipt_with(&quoted(&f.foreign())), &f.foreign());
        assert_eq!((code, out.as_str()), (1, ""));
        assert!(err.contains("not an allowed deploy target"), "{err}");
    }

    // (c) a legacy receipt plus --target-dir equal to the allowed target succeeds.
    #[test]
    fn a_legacy_receipt_with_the_allowed_override_succeeds() {
        let f = fixture();
        let (code, out, _) = f.run(r#"{"exit_code":0}"#, &f.allowed());
        assert_eq!((code, out), (0, format!("{}\n", f.allowed())));
    }

    // (d) an explicit target_dir that is not a string is refused, with or without an override.
    #[test]
    fn a_target_dir_that_is_not_a_string_is_refused_naming_the_field() {
        let f = fixture();
        for value in ["null", "7", r#"["/x"]"#, r#"{"a":1}"#, "true", r#""""#] {
            for override_dir in ["", f.allowed().as_str()] {
                let (code, out, err) = f.run(&receipt_with(value), override_dir);
                assert_eq!((code, out.as_str()), (1, ""), "{value} / {override_dir:?}");
                assert!(err.contains("target_dir"), "{value}: {err}");
            }
        }
    }

    // (e) the key absent: exit 2 with no override, the allowed override succeeds.
    #[test]
    fn an_absent_target_dir_needs_an_allowed_override() {
        let f = fixture();
        let (code, out, err) = f.run(r#"{"exit_code":0}"#, "");
        assert_eq!((code, out.as_str(), err.as_str()), (2, "", ""));
        let (code, out, _) = f.run(r#"{"exit_code":0,"result":{"exit_code":0}}"#, &f.allowed());
        assert_eq!((code, out), (0, format!("{}\n", f.allowed())));
    }

    // (f) a nested result.target_dir of null is refused too.
    #[test]
    fn a_nested_null_target_dir_is_refused() {
        let f = fixture();
        for override_dir in ["", f.allowed().as_str()] {
            let (code, out, err) = f.run(r#"{"result":{"target_dir":null}}"#, override_dir);
            assert_eq!((code, out.as_str()), (1, ""), "{override_dir:?}");
            assert!(err.contains("target_dir"), "{err}");
        }
    }

    #[test]
    fn a_nested_allowed_target_dir_succeeds() {
        let f = fixture();
        let json = format!(r#"{{"result":{{"target_dir":{}}}}}"#, quoted(&f.allowed()));
        let (code, out, _) = f.run(&json, "");
        assert_eq!((code, out), (0, format!("{}\n", f.allowed())));
    }

    #[test]
    fn a_target_dir_through_dotdot_resolves_to_the_allowed_target() {
        let f = fixture();
        let spelled = format!("{}/../allowed", f.foreign());
        let (code, out, _) = f.run(&receipt_with(&quoted(&spelled)), "");
        assert_eq!((code, out), (0, format!("{}\n", f.allowed())));
    }

    #[test]
    fn a_relative_target_dir_is_refused() {
        let f = fixture();
        let (code, _, err) = f.run(&receipt_with(r#""allowed""#), "");
        assert_eq!(code, 1);
        assert!(err.contains("must be an absolute path"), "{err}");
    }

    #[test]
    fn an_override_with_dotdot_or_a_symlink_is_refused() {
        let f = fixture();
        let dotdot = format!("{}/../allowed", f.foreign());
        let (code, _, err) = f.run(r#"{"exit_code":0}"#, &dotdot);
        assert_eq!(code, 1);
        assert!(err.contains("must be canonical"), "{err}");
        #[cfg(unix)]
        {
            let link = f.root.join("link");
            std::os::unix::fs::symlink(&f.allowed, &link).unwrap();
            let (code, _, err) = f.run(r#"{"exit_code":0}"#, &link.to_string_lossy());
            assert_eq!(code, 1);
            assert!(err.contains("must be canonical"), "{err}");
        }
    }

    #[test]
    fn a_receipt_and_override_that_disagree_are_refused() {
        let f = fixture();
        let other = f.root.join("other");
        std::fs::create_dir_all(&other).unwrap();
        let allowed = vec![f.allowed(), other.to_string_lossy().into_owned()];
        let result = receipt_target(
            &write_receipt(&f, &receipt_with(&quoted(&f.allowed()))),
            &other.to_string_lossy(),
            &allowed,
        );
        assert!(result.unwrap_err().contains("differs from --target-dir"));
    }

    fn write_receipt(f: &Fixture, json: &str) -> String {
        let path = f.root.join("r2.json");
        std::fs::write(&path, json).unwrap();
        path.to_string_lossy().into_owned()
    }

    #[test]
    fn a_missing_or_unreadable_receipt_is_refused() {
        let f = fixture();
        let missing = f.root.join("none.json").to_string_lossy().into_owned();
        let args = [missing.clone(), String::new(), f.allowed()];
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let ctx = Ctx::new(&f.root, &[]);
        assert_eq!(main(&args, &ctx, &mut out, &mut err), 1);
        assert!(String::from_utf8_lossy(&err).contains("does not exist"));
        let (code, _, err) = f.run("{not json", "");
        assert_eq!(code, 1);
        assert!(err.contains("Cannot read build receipt"), "{err}");
    }
}

#[cfg(test)]
mod release_build_tests {
    use crate::testutil::repo_root;

    /// A real deploy must never meet the missing-guard refusal: the normal
    /// release builds (`make release-install` through `install-release.sh`, and
    /// `make release`) build the guard binary `release_guard_receipt_target` runs.
    #[test]
    fn the_release_builds_include_the_guard_binary() {
        let root = repo_root();
        let install = std::fs::read_to_string(root.join("scripts/install-release.sh")).unwrap();
        // The guard is built once, in its own cargo call (#451): one call with rsid
        // would unify serde_json/preserve_order into rsid's feature set.
        let guard_build = |text: &str| {
            text.lines()
                .filter(|line| line.contains("cargo build --release"))
                .filter(|line| line.contains("-p rsi-repo-guards --bin rsi-repo-guard"))
                .count()
        };
        assert_eq!(
            guard_build(&install),
            1,
            "install-release.sh builds the guard once"
        );
        let makefile = std::fs::read_to_string(root.join("Makefile")).unwrap();
        assert_eq!(
            guard_build(&makefile),
            1,
            "the Makefile release target builds the guard once"
        );
    }
}
