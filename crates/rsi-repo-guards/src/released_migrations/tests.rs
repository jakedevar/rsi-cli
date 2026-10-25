//! Policy fixtures ported from test_check_released_migrations.py.
use super::*;
use crate::testutil::{repo_root, run, write};
use std::path::{Path, PathBuf};

fn step(v: i64) -> String {
    format!("        if version < {v} {{\n            migrate();\n        }}\n")
}
fn files(n: i64) -> Files {
    let mut f = Files::new();
    f.insert(MIGRATION_PATH.into(), "// generated head\n".into());
    f.insert(
        format!("{MIGRATION_DIR}/v000.rs"),
        "// V0: Original schema\n        create_table();\n\n        Ok(())\n".into(),
    );
    for v in 1..=n {
        f.insert(format!("{MIGRATION_DIR}/v{v:03}.rs"), step(v));
    }
    f
}
fn section(name: &str) -> String {
    format!(
        "// RSI-RELEASED-MIGRATION-BEGIN: {name}\nfn helper() {{}}\n// RSI-RELEASED-MIGRATION-END: {name}\n"
    )
}
fn declaration(v: i64) -> Value {
    json!({"schema_version":1,"version":v,"files":[{"path":"test.rs","path_template":"test.rs","source_blob":"sha256:fixture","sites":[{"anchor":format!("version {v}"),"replacement":"version ${VERSION}","scope":"unit"}]}]})
}
fn table(entries: &str) -> String {
    format!(
        "pub(crate) const MIGRATION_ALTERED_OBJECTS: &[(i32, &[&str])] = &[\n    // why\n{entries}\n];\n"
    )
}
fn git_output(dir: &Path, args: &[&str]) -> std::process::Output {
    let root = repo_root();
    assert!(dir.is_absolute() && dir.is_dir() && !dir.starts_with(root));
    let null = if cfg!(windows) { "NUL" } else { "/dev/null" };
    let mut cmd = std::process::Command::new("git");
    for (k, _) in std::env::vars_os() {
        if k.to_string_lossy().starts_with("GIT_") {
            cmd.env_remove(k);
        }
    }
    cmd.env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", null)
        .args([
            "-C",
            dir.to_str().unwrap(),
            "-c",
            &format!("core.hooksPath={null}"),
            "-c",
            "commit.gpgsign=false",
            "-c",
            "user.name=fixture",
            "-c",
            "user.email=fixture@example.invalid",
        ])
        .args(args)
        .output()
        .unwrap()
}
fn git(dir: &Path, args: &[&str]) -> String {
    let done = git_output(dir, args);
    assert!(
        done.status.success(),
        "{args:?}: {}",
        String::from_utf8_lossy(&done.stderr)
    );
    String::from_utf8(done.stdout).unwrap().trim().into()
}
struct Repo {
    _tmp: tempfile::TempDir,
    path: PathBuf,
    base: String,
    pin: String,
}
impl Repo {
    fn new() -> Self {
        let tmp = if cfg!(unix) {
            tempfile::tempdir_in("/var/tmp").unwrap()
        } else {
            tempfile::tempdir().unwrap()
        };
        let path = tmp.path().canonicalize().unwrap();
        git(&path, &["init", "-q"]);
        let files = files(1);
        for (p, s) in &files {
            write(&path, p, s);
        }
        let pin = format!(
            "{}\n",
            serde_json::to_string_pretty(&inventory(&files).unwrap()).unwrap()
        );
        write(&path, MANIFEST, &pin);
        git(&path, &["add", "."]);
        git(&path, &["commit", "-qm", "base"]);
        let base = git(&path, &["rev-parse", "HEAD"]);
        git(&path, &["update-ref", "refs/remotes/origin/rolling", &base]);
        Self {
            _tmp: tmp,
            path,
            base,
            pin,
        }
    }
    fn ctx(&self) -> Ctx {
        Ctx::new(&self.path, &[])
    }
    fn commit(&self) -> String {
        git(&self.path, &["add", "."]);
        git(&self.path, &["commit", "-qm", "candidate"]);
        git(&self.path, &["rev-parse", "HEAD"])
    }
    fn add_version(&self, v: i64) {
        write(&self.path, &format!("{MIGRATION_DIR}/v{v:03}.rs"), &step(v));
    }
    fn unchanged(&self) {
        assert_eq!(
            std::fs::read_to_string(self.path.join(MANIFEST)).unwrap(),
            self.pin
        );
    }
}

#[test]
fn split_and_inline_layouts_share_exact_digests() {
    let split = inventory(&files(1)).unwrap();
    let inline = format!(
        "pub const LATEST_SCHEMA_VERSION: i32 = 1;\n// V0: Original schema\n        create_table();\n\n// V1: Session metadata columns\n{}",
        step(1)
    );
    let old = inventory(&Files::from([(MIGRATION_PATH.into(), inline)])).unwrap();
    assert_eq!(old["blocks"], split["blocks"]);
    validate_append_only(&old, &split, &[]).unwrap();
    assert_eq!(split["migration_dir"], MIGRATION_DIR);
    assert_eq!(
        migration_blocks(&files(1)).unwrap()[&0],
        "// V0: Original schema\n        create_table();\n\n"
    );
}
#[test]
fn wrong_file_number_missing_block_and_mixed_layouts_refuse() {
    let mut f = files(2);
    f.insert(format!("{MIGRATION_DIR}/v001.rs"), step(2));
    assert!(inventory(&f).unwrap_err().contains("block V2 found in"));
    assert!(
        migration_file_version(&format!("{MIGRATION_DIR}/v2.rs"))
            .unwrap_err()
            .contains("v002.rs")
    );
    let mut f = files(1);
    f.insert(format!("{MIGRATION_DIR}/v002.rs"), "fn empty() {}\n".into());
    assert_eq!(
        inventory(&f).unwrap_err(),
        "no migration block found for schema head V2"
    );
    f.insert("crates/rsid/src/store/migrations/v003.rs".into(), step(3));
    assert!(
        inventory(&f)
            .unwrap_err()
            .contains("more than one store layout")
    );
}
#[test]
fn duplicate_unterminated_and_misplaced_regions_refuse() {
    let mut f = files(1);
    f.insert(format!("{MIGRATION_DIR}/v002.rs"), step(1));
    assert!(
        migration_blocks(&f)
            .unwrap_err()
            .contains("duplicate migration block")
    );
    let mut f = files(1);
    f.insert(
        format!("{MIGRATION_DIR}/v001.rs"),
        "if version < 1 {\n".into(),
    );
    assert!(
        migration_blocks(&f)
            .unwrap_err()
            .contains("unterminated migration block")
    );
    let mut f = files(1);
    f.remove(&format!("{MIGRATION_DIR}/v000.rs"));
    assert!(
        migration_blocks(&f)
            .unwrap_err()
            .contains("V0 migration region")
    );
    f.insert(
        format!("{MIGRATION_DIR}/v001.rs"),
        "// V0: Original schema\n        Ok(())\n".into(),
    );
    assert!(
        migration_blocks(&f)
            .unwrap_err()
            .contains("V0 region belongs")
    );
}
#[test]
fn markers_validate_and_allow_only_content_preserving_moves() {
    let mut f = files(1);
    f.insert("a.rs".into(), section("alpha"));
    let before = inventory(&f).unwrap();
    f.remove("a.rs");
    f.insert("b.rs".into(), section("alpha"));
    let after = inventory(&f).unwrap();
    validate_append_only(&before, &after, &[]).unwrap();
    assert!(
        validate_manifest(&before, &after, "fixture")
            .unwrap_err()
            .contains("protected sections alpha")
    );
    let refreshed = refreshed_manifest(&before, &after).unwrap();
    assert_eq!(refreshed, after);
    f.insert("c.rs".into(), section("alpha"));
    assert!(inventory(&f).unwrap_err().contains("duplicate protected"));
    for text in [
        "// RSI-RELEASED-MIGRATION-BEGIN: x\n",
        "// RSI-RELEASED-MIGRATION-END: x\n",
    ] {
        assert!(protected_sections(&Files::from([("a.rs".into(), text.into())])).is_err());
    }
}
#[test]
fn append_only_checks_prefix_contiguity_gaps_and_retroactive_pins() {
    let a = inventory(&files(1)).unwrap();
    let b = inventory(&files(2)).unwrap();
    validate_append_only(&a, &b, &[]).unwrap();
    assert!(
        validate_append_only(&b, &a, &[])
            .unwrap_err()
            .contains("regressed")
    );
    let mut gapped = files(3);
    gapped.remove(&format!("{MIGRATION_DIR}/v002.rs"));
    let g = inventory(&gapped).unwrap();
    assert!(validate_append_only(&a, &g, &[]).is_err());
    validate_append_only(&a, &g, &[2]).unwrap();
    assert!(validate_append_only(&a, &g, &[3]).is_err());
    assert!(validate_append_only(&a, &inventory(&files(3)).unwrap(), &[2]).is_err());
    let mut changed = b.clone();
    changed["blocks"]["1"] = json!("edited");
    assert_eq!(
        validate_append_only(&a, &changed, &[]).unwrap_err(),
        "released migration block V1 changed relative to base"
    );
    let mut retro = a.clone();
    retro["blocks"]["-1"] = json!("added");
    retro["latest_schema_version"] = json!(2);
    assert!(
        validate_append_only(&a, &retro, &[])
            .unwrap_err()
            .contains("retroactively")
    );
}
#[test]
fn pending_predecessors_validate_types_limits_distinctness_and_active_range() {
    let mut d = declaration(5);
    d["pending_predecessors"] = json!([3, 4]);
    for (base, want) in [(2, vec![3, 4]), (3, vec![4]), (4, vec![])] {
        assert_eq!(
            parse_pending_predecessors(&[d.to_string()], base).unwrap(),
            want
        );
    }
    for bad in [
        json!([]),
        json!([5]),
        json!([6]),
        json!(["4"]),
        json!([true]),
        json!([4, 4]),
        json!([1, 2, 3, 4, 5, 6, 7, 8, 9]),
    ] {
        d["pending_predecessors"] = bad;
        assert!(parse_pending_predecessors(&[d.to_string()], 0).is_err());
    }
    assert!(parse_pending_predecessors(&["not json".into()], 0).is_err());
}
#[test]
fn shape_diagnostics_name_declaration_file_key_and_site() {
    let d = declaration(2);
    validate_declaration_shape(&d, "unit.json", Some(2)).unwrap();
    for key in ["path", "path_template", "source_blob", "sites"] {
        let mut bad = d.clone();
        bad["files"][0].as_object_mut().unwrap().shift_remove(key);
        let e = validate_declaration_shape(&bad, "unit.json", Some(2)).unwrap_err();
        assert!(
            e.contains("unit.json: file entry") && e.contains(&format!("missing key `{key}`")),
            "{e}"
        );
    }
    for (key, bad_value) in [
        ("scope", json!("other")),
        ("replacement", json!("version 3")),
        ("anchor", json!("")),
    ] {
        let mut bad = d.clone();
        bad["files"][0]["sites"][0][key] = bad_value;
        assert!(
            validate_declaration_shape(&bad, "unit.json", None)
                .unwrap_err()
                .contains("sites[0]")
        );
    }
    for (key, value) in [
        ("version", json!(true)),
        ("schema_version", json!(true)),
        ("files", json!([])),
    ] {
        let mut bad = d.clone();
        bad[key] = value;
        assert!(validate_declaration_shape(&bad, "unit.json", None).is_err());
    }
}
#[test]
fn rewind_check_detects_head_and_retains_old_tree_skip() {
    validate_rewind(&files(2), None).unwrap();
    validate_rewind(&files(2), Some("fn unrelated() {}\n")).unwrap();
    validate_rewind(
        &files(2),
        Some("const REWIND_TEARDOWN_COVERED_THROUGH: i32 = 2;"),
    )
    .unwrap();
    let e = validate_rewind(
        &files(2),
        Some("const REWIND_TEARDOWN_COVERED_THROUGH: i32 = 1;"),
    )
    .unwrap_err();
    assert!(e.contains("(1) does not match the schema head (V2)") && e.contains("active >= 2"));
}
#[test]
fn altered_objects_parse_rustfmt_entries_and_name_the_required_union() {
    let f = Files::from([(
        format!("{MIGRATION_DIR}/v007.rs"),
        "ALTER TABLE things ADD COLUMN extra TEXT; DROP TRIGGER IF EXISTS guard_a;".into(),
    )]);
    for entries in [
        "(\n 7,\n &[\n \"things\",\n \"guard_a\",\n ],\n),",
        "(7, &[\"things\", \"guard_a\",],),",
        "(7, &[\"things\"],),\n (7, &[\"guard_a\"]),",
    ] {
        validate_altered(&f, Some(6), Some(&table(entries))).unwrap();
    }
    let e = validate_altered(&f, Some(6), Some(&table("(7, &[\"things\"]),"))).unwrap_err();
    assert!(e.contains("(7, &[\"guard_a\", \"things\"]),") && e.contains("false refusal"));
    assert!(
        altered_objects_table(&table("(7, [\"things\"]),"))
            .unwrap_err()
            .contains("line 2")
    );
}
#[test]
fn rebuilds_comments_new_objects_and_historical_versions() {
    let sql = "CREATE TABLE things_new (id TEXT); DROP TABLE things; ALTER TABLE things_new RENAME TO things;";
    assert_eq!(
        altered_by_migration(sql).0,
        BTreeSet::from(["things".into()])
    );
    assert!(
        altered_by_migration(
            "// ALTER TABLE old ADD COLUMN x\n/// DROP TABLE old\nCREATE TABLE brand_new (id TEXT);"
        )
        .0
        .is_empty()
    );
    let f = Files::from([(
        format!("{MIGRATION_DIR}/v008.rs"),
        "cohort::rebuild_status_constraint(&tx)?;".into(),
    )]);
    assert!(
        validate_altered(&f, Some(7), Some(&table("")))
            .unwrap_err()
            .contains("rebuild_* helper")
    );
    validate_altered(&f, Some(7), Some(&table("(8, &[\"status_items\"]),"))).unwrap();
    validate_altered(&f, Some(8), Some(&table(""))).unwrap();
    validate_altered(&f, Some(7), None).unwrap();
    validate_altered(&f, None, Some(&table(""))).unwrap();
}
#[test]
fn real_checkout_inventory_matches_every_released_pin_and_pin_bytes() {
    let ctx = Ctx::new(repo_root(), &[]);
    let raw = git_show(&ctx, "HEAD", MANIFEST).unwrap();
    let manifest = load_manifest(&raw, MANIFEST).unwrap();
    let actual = inventory(&load_revision_files(&ctx, "HEAD", &manifest).unwrap()).unwrap();
    validate_manifest(&manifest, &actual, "checked-in pin").unwrap();
    assert_eq!(
        format!(
            "{}\n",
            serde_json::to_string_pretty(&refreshed_manifest(&manifest, &actual).unwrap()).unwrap()
        ),
        raw
    );
    let tests = git_show(&ctx, "HEAD", REWIND_TESTS).unwrap();
    assert!(tests.contains("const REWIND_TEARDOWN_COVERED_THROUGH: i32 = "));
    validate_rewind(
        &load_revision_files(&ctx, "HEAD", &manifest).unwrap(),
        Some(&tests),
    )
    .unwrap();
    let text = git_show(&ctx, "HEAD", ALTERED_OBJECTS).unwrap();
    assert_eq!(
        altered_objects_table(&text).unwrap()[&170],
        BTreeSet::from(["manager_portfolio_appointments".into()])
    );
    let floor = Re::new(r"COMPLETE_ABOVE: i32 = (\d+);")
        .search(&text)
        .unwrap();
    validate_altered(
        &load_revision_files(&ctx, "HEAD", &manifest).unwrap(),
        Some(group(&floor, 1).parse().unwrap()),
        Some(&text),
    )
    .unwrap();
}
#[test]
fn unchanged_refresh_preserves_bytes_and_new_migration_defers_pin() {
    let r = Repo::new();
    refresh(&r.ctx(), "origin/rolling", &[]).unwrap();
    r.unchanged();
    r.add_version(2);
    let new = r.commit();
    assert!(
        check(&r.ctx(), &r.base, &new)
            .unwrap()
            .ends_with("pin deferred to the lander")
    );
    r.unchanged();
    refresh(&r.ctx(), &r.base, &[]).unwrap();
    let new = r.commit();
    assert!(check(&r.ctx(), &r.base, &new).unwrap().ends_with("head V2"));
}
#[test]
fn refresh_discovers_marked_rust_helpers_and_repins_only_unreleased_work() {
    let r = Repo::new();
    r.add_version(2);
    write(&r.path, "crates/example/new.rs", &section("new-helper"));
    write(
        &r.path,
        "docs/quoted.md",
        "// RSI-RELEASED-MIGRATION-BEGIN: bogus\n",
    );
    refresh(&r.ctx(), &r.base, &[]).unwrap();
    write(
        &r.path,
        &format!("{MIGRATION_DIR}/v002.rs"),
        &step(2).replace("migrate()", "changed()"),
    );
    write(
        &r.path,
        "crates/example/new.rs",
        &section("new-helper").replace("helper()", "edited()"),
    );
    refresh(&r.ctx(), &r.base, &[]).unwrap();
    let pin = std::fs::read_to_string(r.path.join(MANIFEST)).unwrap();
    assert!(pin.contains("new-helper"));
    let published = r.commit();
    write(&r.path, &format!("{MIGRATION_DIR}/v002.rs"), &step(2));
    assert!(
        refresh(&r.ctx(), &published, &[])
            .unwrap_err()
            .contains("changed migration blocks V2")
    );
    assert_eq!(std::fs::read_to_string(r.path.join(MANIFEST)).unwrap(), pin);
}
#[test]
fn refresh_refuses_changed_released_history_even_with_deleted_or_forged_pin() {
    for deleted in [false, true] {
        let r = Repo::new();
        write(
            &r.path,
            &format!("{MIGRATION_DIR}/v001.rs"),
            &step(1).replace("migrate()", "changed()"),
        );
        if deleted {
            std::fs::remove_file(r.path.join(MANIFEST)).unwrap();
        } else {
            let mut f = files(1);
            f.insert(
                format!("{MIGRATION_DIR}/v001.rs"),
                step(1).replace("migrate()", "changed()"),
            );
            write(&r.path, MANIFEST, &inventory(&f).unwrap().to_string());
        }
        assert!(
            refresh(&r.ctx(), &r.base, &[])
                .unwrap_err()
                .contains("changed migration blocks V1")
        );
    }
}
#[test]
fn missing_base_invalid_base_noncontiguous_and_missing_pinned_sections_refuse_before_writes() {
    let r = Repo::new();
    assert!(
        refresh(&r.ctx(), "missing-ref", &[])
            .unwrap_err()
            .contains("fetch it first")
    );
    r.unchanged();
    r.add_version(3);
    assert!(
        refresh(&r.ctx(), &r.base, &[])
            .unwrap_err()
            .contains("every new version")
    );
    r.unchanged();
    let mut existing = inventory(&files(1)).unwrap();
    existing["protected_sections"]["lost"] = json!({"path":"gone.rs","sha256":"sha256:pin"});
    assert!(
        refreshed_manifest(&existing, &inventory(&files(1)).unwrap())
            .unwrap_err()
            .contains("removed protected sections lost")
    );
    write(
        &r.path,
        &format!("{MIGRATION_DIR}/v001.rs"),
        &step(1).replace("migrate()", "changed()"),
    );
    let broken = r.commit();
    assert!(
        refresh(&r.ctx(), &broken, &[])
            .unwrap_err()
            .contains("refresh base migration manifest")
    );
    r.unchanged();
}
#[test]
fn deferred_pin_refuses_released_edits_and_hand_edited_partial_pins() {
    let a = inventory(&files(1)).unwrap();
    let b = inventory(&files(2)).unwrap();
    assert!(validated_inventory(&a, &b, "head", Some(&a)).unwrap().1);
    let mut partial = b.clone();
    partial["blocks"].as_object_mut().unwrap().shift_remove("2");
    assert!(validated_inventory(&partial, &b, "head", Some(&a)).is_err());
    let mut changed = b.clone();
    changed["blocks"]["1"] = json!("edited");
    assert!(validated_inventory(&a, &changed, "head", Some(&a)).is_err());
}
#[test]
fn stacked_base_requires_same_revision_declaration_and_keeps_provisional_prefix_immutable() {
    let r = Repo::new();
    r.add_version(2);
    write(
        &r.path,
        "tools/provisional-migrations/two.json",
        &declaration(2).to_string(),
    );
    let stacked = r.commit();
    let new = r.commit_if_new_version(3);
    assert!(check(&r.ctx(), &stacked, &new).is_ok());
    write(
        &r.path,
        &format!("{MIGRATION_DIR}/v002.rs"),
        &step(2).replace("migrate()", "changed()"),
    );
    let changed = r.commit();
    assert!(check(&r.ctx(), &stacked, &changed).is_err());
    let r = Repo::new();
    r.add_version(2);
    let undeclared = r.commit();
    r.add_version(3);
    write(
        &r.path,
        "tools/provisional-migrations/two.json",
        &declaration(2).to_string(),
    );
    let new = r.commit();
    assert!(
        check(&r.ctx(), &undeclared, &new)
            .unwrap_err()
            .contains("base migration manifest")
    );
}
impl Repo {
    fn commit_if_new_version(&self, v: i64) -> String {
        self.add_version(v);
        self.commit()
    }
}
#[test]
fn cli_exit_codes_and_forward_migration_remedy() {
    let r = Repo::new();
    let (code, _, err) = run(main, &["--include-path", "a"], &r.ctx());
    assert_eq!(code, 2);
    assert!(err.contains("requires --refresh"));
    let (code, out, _) = run(main, &[&r.base, &r.base], &r.ctx());
    assert_eq!(code, 0);
    assert!(out.contains("PASS"));
    write(
        &r.path,
        &format!("{MIGRATION_DIR}/v001.rs"),
        &step(1).replace("migrate()", "changed()"),
    );
    let new = r.commit();
    let (code, _, err) = run(main, &[&r.base, &new], &r.ctx());
    assert_eq!(code, 1);
    assert!(err.contains("add a forward migration") && err.contains("never edit it by hand"));
}

#[test]
fn conflicted_refresh_unions_both_index_stages_and_refuses_disagreeing_pins() {
    for disagree in [false, true] {
        let r = Repo::new();
        let side_tmp = if cfg!(unix) {
            tempfile::tempdir_in("/var/tmp").unwrap()
        } else {
            tempfile::tempdir().unwrap()
        };
        let wt = side_tmp.path().join("side");
        git(
            &r.path,
            &[
                "worktree",
                "add",
                "-q",
                "--detach",
                wt.to_str().unwrap(),
                &r.base,
            ],
        );
        for (dir, name) in [(&r.path, "alpha"), (&wt, "beta")] {
            write(
                dir,
                &format!("crates/rsid-store/src/store/{name}.rs"),
                &section(name),
            );
            refresh(&Ctx::new(dir, &[]), &r.base, &[]).unwrap();
            if disagree && name == "beta" {
                let mut pin: Value =
                    serde_json::from_slice(&std::fs::read(dir.join(MANIFEST)).unwrap()).unwrap();
                pin["blocks"]["1"] = json!("sha256:different");
                write(
                    dir,
                    MANIFEST,
                    &format!("{}\n", serde_json::to_string_pretty(&pin).unwrap()),
                );
            }
            git(dir, &["add", "."]);
            git(dir, &["commit", "-qm", "add section"]);
        }
        let other = git(&wt, &["rev-parse", "HEAD"]);
        // A nonzero merge result is required fixture state, not a guard result.
        let merged = git_output(&r.path, &["merge", "--no-edit", &other]);
        assert!(!merged.status.success());
        let conflict = std::fs::read_to_string(r.path.join(MANIFEST)).unwrap();
        assert!(conflict.contains("<<<<<<<"));
        if disagree {
            assert_eq!(
                refresh(&r.ctx(), &r.base, &[]).unwrap_err(),
                "merged manifests pin different content for V1"
            );
            assert_eq!(
                std::fs::read_to_string(r.path.join(MANIFEST)).unwrap(),
                conflict
            );
        } else {
            refresh(&r.ctx(), &r.base, &[]).unwrap();
            let pin: Value =
                serde_json::from_slice(&std::fs::read(r.path.join(MANIFEST)).unwrap()).unwrap();
            assert_eq!(
                pin["protected_sections"]
                    .as_object()
                    .unwrap()
                    .keys()
                    .map(String::as_str)
                    .collect::<BTreeSet<_>>(),
                BTreeSet::from(["alpha", "beta"])
            );
            assert_eq!(pin["blocks"], inventory(&files(1)).unwrap()["blocks"]);
        }
    }
}

#[test]
fn stacked_zero_diff_checks_validate_every_extra_block_and_its_declaration_shape() {
    for case in ["declared", "extra", "malformed", "released"] {
        let r = Repo::new();
        r.add_version(2);
        let mut d = declaration(2);
        if case == "malformed" {
            d["files"][0]
                .as_object_mut()
                .unwrap()
                .shift_remove("path_template");
        }
        write(
            &r.path,
            "tools/provisional-migrations/two.json",
            &d.to_string(),
        );
        if case == "extra" {
            r.add_version(3);
        }
        if case == "released" {
            write(
                &r.path,
                &format!("{MIGRATION_DIR}/v001.rs"),
                &step(1).replace("migrate()", "changed()"),
            );
        }
        let base = r.commit();
        let result = check(&r.ctx(), &base, &base);
        match case {
            "declared" => assert!(result.unwrap().contains("V0-V2 immutable, head V2")),
            "extra" => assert!(
                result
                    .unwrap_err()
                    .contains("base migration manifest does not match its source")
            ),
            "malformed" => assert!(result.unwrap_err().contains("missing key `path_template`")),
            "released" => {
                assert_eq!(
                    result.unwrap_err(),
                    "head migration manifest does not match its source: migration blocks V1, V2; LATEST_SCHEMA_VERSION"
                );
                // Restoring the head must not make its corrupted base valid.
                write(&r.path, &format!("{MIGRATION_DIR}/v001.rs"), &step(1));
                let restored = r.commit();
                assert_eq!(
                    check(&r.ctx(), &base, &restored).unwrap_err(),
                    "changed migration blocks V1"
                );
            }
            _ => unreachable!(),
        }
        r.unchanged();
    }
}

#[test]
fn bare_check_after_merging_rolling_names_explicit_base_and_defers_only_the_pin() {
    let r = Repo::new();
    let rolling_tmp = if cfg!(unix) {
        tempfile::tempdir_in("/var/tmp").unwrap()
    } else {
        tempfile::tempdir().unwrap()
    };
    let rolling = rolling_tmp.path().join("rolling");
    git(
        &r.path,
        &[
            "worktree",
            "add",
            "-q",
            "--detach",
            rolling.to_str().unwrap(),
            &r.base,
        ],
    );
    r.add_version(2);
    r.commit();
    write(&rolling, "README.md", "rolling advanced\n");
    git(&rolling, &["add", "."]);
    git(&rolling, &["commit", "-qm", "advance rolling"]);
    let tip = git(&rolling, &["rev-parse", "HEAD"]);
    git(
        &r.path,
        &["update-ref", "refs/remotes/origin/rolling", &tip],
    );
    git(&r.path, &["merge", "--no-edit", "--no-ff", &tip]);
    let (code, _, err) = run(main, &[], &r.ctx());
    assert_eq!(code, 1);
    assert!(err.contains(
        "Rerun against the real base: `scripts/repo-guard released-migrations origin/rolling HEAD`"
    ));
    let (code, out, err) = run(main, &["origin/rolling", "HEAD"], &r.ctx());
    assert_eq!(code, 0, "{err}");
    assert!(out.contains("pin deferred to the lander"));
    r.unchanged();
}

#[test]
fn committed_store_relocation_preserves_released_bytes_and_mixed_layout_refuses() {
    let r = Repo::new();
    // Recreate the old inline store, including a pinned helper, then move it
    // into rsid-store as the S4 extraction did. Only paths may change.
    for path in files(1).keys() {
        std::fs::remove_file(r.path.join(path)).unwrap();
    }
    let old_root = "crates/rsid/src/store";
    let inline = format!(
        "pub const LATEST_SCHEMA_VERSION: i32 = 1;\n// V0: Original schema\n        create_table();\n\n// V1: Session metadata columns\n{}",
        step(1)
    );
    let old = Files::from([
        (format!("{old_root}/mod.rs"), inline),
        (format!("{old_root}/helper.rs"), section("alpha")),
    ]);
    for (path, body) in &old {
        write(&r.path, path, body);
    }
    let old_pin = inventory(&old).unwrap();
    assert_eq!(old_pin["migration_file"], format!("{old_root}/mod.rs"));
    write(
        &r.path,
        MANIFEST,
        &format!("{}\n", serde_json::to_string_pretty(&old_pin).unwrap()),
    );
    let base = r.commit();
    let moved: Files = old
        .iter()
        .map(|(path, body)| {
            (
                path.replace("crates/rsid/", "crates/rsid-store/"),
                body.clone(),
            )
        })
        .collect();
    for path in old.keys() {
        std::fs::remove_file(r.path.join(path)).unwrap();
    }
    for (path, body) in &moved {
        write(&r.path, path, body);
    }
    write(
        &r.path,
        MANIFEST,
        &format!(
            "{}\n",
            serde_json::to_string_pretty(&inventory(&moved).unwrap()).unwrap()
        ),
    );
    let candidate = r.commit();
    assert!(
        check(&r.ctx(), &base, &candidate)
            .unwrap()
            .contains("V0-V1 immutable, head V1")
    );
    assert_eq!(load_revision_files(&r.ctx(), &base, &old_pin).unwrap(), old);
    // Mixing migration directories is refused independently of an inline
    // layout move, before either store can conceal the other inventory.
    for root in STORE_ROOTS {
        write(&r.path, &format!("{root}/migrations/v002.rs"), &step(2));
    }
    let mixed = r.commit();
    assert!(
        check(&r.ctx(), &base, &mixed)
            .unwrap_err()
            .contains("more than one store layout")
    );
}

/// Overwrite the loose object behind `rev:path` so `git show` fails while the tree still lists it.
fn corrupt_blob(r: &Repo, rev: &str, path: &str) {
    let id = git(&r.path, &["rev-parse", &format!("{rev}:{path}")]);
    let object = r.path.join(".git/objects").join(&id[..2]).join(&id[2..]);
    let mut perms = std::fs::metadata(&object).unwrap().permissions();
    #[allow(clippy::permissions_set_readonly_false)]
    perms.set_readonly(false);
    std::fs::set_permissions(&object, perms).unwrap();
    std::fs::write(&object, b"not a zlib object").unwrap();
}

#[test]
fn git_show_opt_separates_an_absent_path_from_an_unreadable_one() {
    let r = Repo::new();
    assert!(
        git_show_opt(&r.ctx(), &r.base, MANIFEST)
            .unwrap()
            .is_some_and(|t| t == r.pin)
    );
    assert_eq!(
        git_show_opt(&r.ctx(), &r.base, "no/such/file.rs").unwrap(),
        None
    );
    let unresolved = git_show_opt(&r.ctx(), "missing-ref", MANIFEST).unwrap_err();
    assert!(unresolved.contains("does not resolve"), "{unresolved}");
    corrupt_blob(&r, &r.base, MANIFEST);
    let unreadable = git_show_opt(&r.ctx(), &r.base, MANIFEST).unwrap_err();
    assert!(unreadable.contains("cannot read"), "{unreadable}");
}

#[test]
fn an_unreadable_base_manifest_refuses_instead_of_the_bootstrap_pass() {
    let r = Repo::new();
    r.add_version(2);
    refresh(&r.ctx(), &r.base, &[]).unwrap();
    let new = r.commit();
    corrupt_blob(&r, &r.base, MANIFEST);
    let refused = check(&r.ctx(), &r.base, &new).unwrap_err();
    assert!(refused.contains("cannot read"), "{refused}");
    assert!(!refused.contains("bootstrap"), "{refused}");
    let unresolved = check(&r.ctx(), "missing-ref", &new).unwrap_err();
    assert!(unresolved.contains("does not resolve"), "{unresolved}");
}

#[test]
fn a_base_that_genuinely_lacks_the_manifest_still_bootstraps() {
    let r = Repo::new();
    git(&r.path, &["rm", "-q", MANIFEST]);
    git(&r.path, &["commit", "-qm", "no manifest"]);
    let bare = git(&r.path, &["rev-parse", "HEAD"]);
    write(&r.path, MANIFEST, &r.pin);
    let new = r.commit();
    let passed = check(&r.ctx(), &bare, &new).unwrap();
    assert!(
        passed.contains("bootstrap manifest valid through V1"),
        "{passed}"
    );
}

#[test]
fn rewind_coverage_cannot_vanish_once_the_base_uses_the_constant() {
    let r = Repo::new();
    let tests = "const REWIND_TEARDOWN_COVERED_THROUGH: i32 = 1;\n";
    write(&r.path, REWIND_TESTS, tests);
    let mid = r.commit();
    // Missing at the head: refused, not read as the empty old-tree skip.
    git(&r.path, &["rm", "-q", REWIND_TESTS]);
    r.add_version(2);
    let new = r.commit();
    let missing = check(&r.ctx(), &mid, &new).unwrap_err();
    assert!(
        missing.contains("REWIND_TEARDOWN_COVERED_THROUGH"),
        "{missing}"
    );
    // Present but without the constant: also refused.
    write(&r.path, REWIND_TESTS, "fn unrelated() {}\n");
    let stripped = r.commit();
    let lacking = check(&r.ctx(), &mid, &stripped).unwrap_err();
    assert!(
        lacking.contains("REWIND_TEARDOWN_COVERED_THROUGH"),
        "{lacking}"
    );
    // An old tree that never had the constant keeps the skip.
    assert!(check(&r.ctx(), &r.base, &new).is_ok());
    // An unreadable head file is an error, never an empty text.
    write(
        &r.path,
        REWIND_TESTS,
        "const REWIND_TEARDOWN_COVERED_THROUGH: i32 = 2;\n",
    );
    let covered = r.commit();
    corrupt_blob(&r, &covered, REWIND_TESTS);
    let unreadable = check(&r.ctx(), &mid, &covered).unwrap_err();
    assert!(unreadable.contains("cannot read"), "{unreadable}");
}
