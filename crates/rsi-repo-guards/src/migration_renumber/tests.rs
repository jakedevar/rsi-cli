//! Hermetic migration transforms, conflict boundaries and exact-parent proofs.
use super::*;
use crate::testutil::{repo_root, write};
use std::path::PathBuf;
const TESTS: &str = "// RSI-RELEASED-MIGRATION-BEGIN: test-catalog\n// fixed\n// RSI-RELEASED-MIGRATION-END: test-catalog\nconst REWIND: i32 = 129;\nfn pagination() { assert_eq!(count, 130); }\nfn rewind(active: i32, target: i32) {\n    if active >= 129 && target < 129 {\n        drop_old();\n    }\n}\nfn fixture(target_version: i32) {\n    let scoped = 1;\n}\n";
fn block(label: &str, v: i64) -> String {
    format!(
        "    if active >= {v} && target < {v} {{\n        drop_{label}();\n        set_version({v} - 1);\n    }}\n"
    )
}
fn fixture_run(label: &str, v: i64) -> String {
    format!(
        "    let {label}_present: bool = connection\n        .query_row(\n            \"SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE name='{label}')\",\n            [],\n            |row| row.get(0),\n        )\n        .unwrap();\n    assert_eq!(\n        {label}_present,\n        target_version >= {v},\n        \"fixture V{{target_version}}: {label}\"\n    );\n"
    )
}
fn site(anchor: String, replacement: &str, scope: &str) -> Value {
    json!({"anchor":anchor,"replacement":replacement,"scope":scope})
}
fn git(repo: &Path, args: &[&str]) -> String {
    assert!(repo.is_absolute() && repo.is_dir() && !repo.starts_with(repo_root()));
    String::from_utf8(run(repo, args, None, &IDENTITY).unwrap())
        .unwrap()
        .trim()
        .into()
}
fn commit(repo: &Path) -> String {
    git(repo, &["add", "."]);
    git(repo, &["commit", "-qm", "fixture"]);
    git(repo, &["rev-parse", "HEAD"])
}
struct Fixture {
    tmp: tempfile::TempDir,
    repo: PathBuf,
    base: String,
}
impl Fixture {
    fn new() -> Self {
        let tmp = if cfg!(unix) {
            tempfile::tempdir_in("/var/tmp").unwrap()
        } else {
            tempfile::tempdir().unwrap()
        };
        let repo = tmp.path().canonicalize().unwrap().join("repo");
        std::fs::create_dir(&repo).unwrap();
        git(&repo, &["init", "-q"]);
        let mut files = guard::Files::from([
            (STORE.into(), "// runner only\n".into()),
            (STORE_TESTS.into(), TESTS.into()),
            (
                "crates/rsid-store/src/store/cohort_settlement.rs".into(),
                "".into(),
            ),
            (
                format!("{MIGRATION_DIR}/v000.rs"),
                "// V0: Original schema\n// original\n        Ok(())\n".into(),
            ),
            (
                format!("{MIGRATION_DIR}/v129.rs"),
                "        if version < 129 {\n            settled();\n        }\n".into(),
            ),
        ]);
        files.insert(
            MANIFEST.into(),
            format!(
                "{}\n",
                serde_json::to_string_pretty(&guard::inventory(&files).unwrap()).unwrap()
            ),
        );
        for (p, s) in files {
            write(&repo, &p, &s);
        }
        let base = commit(&repo);
        Self { tmp, repo, base }
    }
    fn worktree(&self, label: &str) -> PathBuf {
        self.tmp.path().join(format!("source-{label}"))
    }
    fn source(&self, label: &str, deferred: bool, teardown: bool) -> String {
        self.numbered_source(label, deferred, teardown, 130, &[])
    }
    fn numbered_source(
        &self,
        label: &str,
        deferred: bool,
        teardown: bool,
        version: i64,
        pending: &[i64],
    ) -> String {
        let wt = self.worktree(label);
        git(
            &self.repo,
            &[
                "worktree",
                "add",
                "-q",
                "--detach",
                wt.to_str().unwrap(),
                &self.base,
            ],
        );
        let unit = format!(
            "impl Store {{\n    fn migrate_v{version}(&self, version: i32) -> Result<()> {{\n        if version < {version} {{\n            {label}_apply();\n        }}\n        Ok(())\n    }}\n}}\n"
        );
        let mut tests = TESTS.replace("REWIND: i32 = 129", &format!("REWIND: i32 = {version}"));
        let mut test_sites = vec![site(
            format!("const REWIND: i32 = {version};"),
            "const REWIND: i32 = ${VERSION};",
            "head",
        )];
        if teardown {
            tests = tests.replace(
                "    if active >= 129 && target < 129 {",
                &format!(
                    "{}    if active >= 129 && target < 129 {{",
                    block(label, version)
                ),
            );
            tests = tests.replace(
                "    let scoped = 1;\n",
                &format!("{}    let scoped = 1;\n", fixture_run(label, version)),
            );
            test_sites.extend([
                site(
                    format!("if active >= {version} && target < {version} {{"),
                    "if active >= ${VERSION} && target < ${VERSION} {",
                    "unit",
                ),
                site(
                    format!("set_version({version} - 1);"),
                    "set_version(${VERSION} - 1);",
                    "unit",
                ),
                site(
                    format!("target_version >= {version},"),
                    "target_version >= ${VERSION},",
                    "unit",
                ),
            ]);
        }
        let p = format!("{MIGRATION_DIR}/v{version}.rs");
        write(&wt, &p, &unit);
        write(&wt, STORE_TESTS, &tests);
        if !deferred {
            let bm = revision_inventory(&self.repo, &self.base, None).unwrap();
            let mut files = guard::Files::new();
            for name in protected_paths(&self.repo, &self.base, &bm).unwrap() {
                files.insert(
                    name.clone(),
                    std::fs::read_to_string(wt.join(&name)).unwrap(),
                );
            }
            files.insert(p.clone(), unit.clone());
            write(
                &wt,
                MANIFEST,
                &format!(
                    "{}\n",
                    serde_json::to_string_pretty(&guard::inventory(&files).unwrap()).unwrap()
                ),
            );
        }
        let mut d = json!({"schema_version":1,"version":version,"files":[{"path":p,"path_template":format!("{MIGRATION_DIR}/v${{VERSION}}.rs"),"source_blob":guard::digest(&unit),"sites":[site(format!("fn migrate_v{version}("),"fn migrate_v${VERSION}(","unit"),site(format!("if version < {version} {{"),"if version < ${VERSION} {","unit")]},{"path":STORE_TESTS,"path_template":STORE_TESTS,"source_blob":guard::digest(&tests),"sites":test_sites}]});
        if !pending.is_empty() {
            d["pending_predecessors"] = json!(pending);
        }
        write(
            &wt,
            &format!("{DECLARATIONS}{label}.json"),
            &format!("{d}\n"),
        );
        commit(&wt)
    }
    fn candidate(&self, source: &str, target: &str) -> (Value, String) {
        self.try_candidate(source, target).unwrap()
    }
    fn try_candidate(&self, source: &str, target: &str) -> Result<(Value, String)> {
        let mut unit = inspect(&self.repo, &self.base, source, target, false)?
            .ok_or("fixture source is not a migration")?;
        let transform = transform(&self.repo, &unit)?;
        for (k, v) in transform.as_object().unwrap() {
            unit[k] = v.clone();
        }
        let scratch = self.tmp.path().join("scratch");
        std::fs::create_dir_all(&scratch).map_err(|e| e.to_string())?;
        let result = build_candidate(&self.repo, &mut unit, &scratch)?;
        Ok((unit, result["candidate"].as_str().unwrap().into()))
    }
    fn edit_declaration(&self, label: &str, edit: impl FnOnce(&mut Value)) -> String {
        let wt = self.worktree(label);
        let name = format!("{DECLARATIONS}{label}.json");
        let mut d: Value = serde_json::from_slice(&std::fs::read(wt.join(&name)).unwrap()).unwrap();
        edit(&mut d);
        write(&wt, &name, &d.to_string());
        commit(&wt)
    }
    fn inline() -> Self {
        let mut f = Self::new();
        for v in [0, 129] {
            std::fs::remove_file(f.repo.join(format!("{MIGRATION_DIR}/v{v:03}.rs"))).unwrap();
        }
        let store = "pub const LATEST_SCHEMA_VERSION: i32 = 129;\n// V0: Original schema\n// original\n// V1: Session metadata columns\nif version < 1 {\n    migrate_v1();\n}\nif version < 129 {\n    migrate_v129();\n}\n";
        write(&f.repo, STORE, store);
        let files = guard::Files::from([
            (STORE.into(), store.into()),
            (STORE_TESTS.into(), TESTS.into()),
        ]);
        write(
            &f.repo,
            MANIFEST,
            &format!(
                "{}\n",
                serde_json::to_string_pretty(&guard::inventory(&files).unwrap()).unwrap()
            ),
        );
        f.base = commit(&f.repo);
        f
    }
    fn inline_source(&self, label: &str, extra: &str) -> String {
        let wt = self.worktree(label);
        git(
            &self.repo,
            &[
                "worktree",
                "add",
                "-q",
                "--detach",
                wt.to_str().unwrap(),
                &self.base,
            ],
        );
        let helper = format!("crates/rsid-store/src/store/{label}_v130.rs");
        let helper_text = format!(
            "// RSI-RELEASED-MIGRATION-BEGIN: v130-{label}-migration\npub fn apply_v130() {{}}\n// RSI-RELEASED-MIGRATION-END: v130-{label}-migration\n"
        );
        let base_store =
            String::from_utf8(show(&self.repo, &self.base, STORE).unwrap().unwrap()).unwrap();
        let store = format!(
            "{}// V130: {label} migration\nif version < 130 {{\n    {label}_v130::apply_v130();\n}}\n{extra}",
            base_store
                .replace(
                    "LATEST_SCHEMA_VERSION: i32 = 129",
                    "LATEST_SCHEMA_VERSION: i32 = 130"
                )
                .replace(
                    "// V0: Original schema",
                    &format!("mod {label}_v130;\n// V0: Original schema")
                )
        );
        let tests = TESTS.replace("REWIND: i32 = 129", "REWIND: i32 = 130");
        let files = guard::Files::from([
            (STORE.into(), store.clone()),
            (STORE_TESTS.into(), tests.clone()),
            (helper.clone(), helper_text.clone()),
        ]);
        for (name, text) in &files {
            write(&wt, name, text);
        }
        write(
            &wt,
            MANIFEST,
            &format!(
                "{}\n",
                serde_json::to_string_pretty(&guard::inventory(&files).unwrap()).unwrap()
            ),
        );
        let entries = vec![
            json!({"path":STORE,"path_template":STORE,"source_blob":guard::digest(&store),"sites":[
                site("pub const LATEST_SCHEMA_VERSION: i32 = 130;".into(), "pub const LATEST_SCHEMA_VERSION: i32 = ${VERSION};", "head"),
                site(format!("mod {label}_v130;"), &format!("mod {label}_v${{VERSION}};"), "unit"),
                site(format!("// V130: {label} migration"), &format!("// V${{VERSION}}: {label} migration"), "unit"),
                site("if version < 130 {".into(), "if version < ${VERSION} {", "unit"),
                site(format!("{label}_v130::apply_v130();"), &format!("{label}_v${{VERSION}}::apply_v${{VERSION}}();"), "unit")]}),
            json!({"path":STORE_TESTS,"path_template":STORE_TESTS,"source_blob":guard::digest(&tests),"sites":[site("const REWIND: i32 = 130;".into(), "const REWIND: i32 = ${VERSION};", "head")]}),
            json!({"path":helper,"path_template":format!("crates/rsid-store/src/store/{label}_v${{VERSION}}.rs"),"source_blob":guard::digest(&helper_text),"sites":[
                site(format!("// RSI-RELEASED-MIGRATION-BEGIN: v130-{label}-migration"), &format!("// RSI-RELEASED-MIGRATION-BEGIN: v${{VERSION}}-{label}-migration"), "unit"),
                site("pub fn apply_v130() {}".into(), "pub fn apply_v${VERSION}() {}", "unit"),
                site(format!("// RSI-RELEASED-MIGRATION-END: v130-{label}-migration"), &format!("// RSI-RELEASED-MIGRATION-END: v${{VERSION}}-{label}-migration"), "unit")]}),
        ];
        write(
            &wt,
            &format!("{DECLARATIONS}{label}.json"),
            &json!({"schema_version":1,"version":130,"files":entries}).to_string(),
        );
        commit(&wt)
    }
    fn inline_numbered_source(&self, label: &str, version: i64, pending: &[i64]) -> String {
        self.inline_source(label, "");
        let wt = self.worktree(label);
        let name = format!("{DECLARATIONS}{label}.json");
        let mut d: Value = serde_json::from_slice(&std::fs::read(wt.join(&name)).unwrap()).unwrap();
        for file in d["files"].as_array_mut().unwrap() {
            let old_path = file["path"].as_str().unwrap().to_string();
            let new_path = file["path_template"]
                .as_str()
                .unwrap()
                .replace(TOKEN, &version.to_string());
            let mut body = std::fs::read_to_string(wt.join(&old_path)).unwrap();
            for site in file["sites"].as_array_mut().unwrap() {
                let anchor = site["anchor"].as_str().unwrap();
                let new_anchor = site["replacement"]
                    .as_str()
                    .unwrap()
                    .replace(TOKEN, &version.to_string());
                assert_eq!(body.matches(anchor).count(), 1);
                body = body.replacen(anchor, &new_anchor, 1);
                site["anchor"] = json!(new_anchor);
            }
            if old_path != new_path {
                std::fs::remove_file(wt.join(&old_path)).unwrap();
            }
            write(&wt, &new_path, &body);
            file["path"] = json!(new_path);
            file["source_blob"] = json!(guard::digest(&body));
        }
        d["version"] = json!(version);
        if !pending.is_empty() {
            d["pending_predecessors"] = json!(pending);
        }
        write(&wt, &name, &d.to_string());
        if pending.is_empty() {
            // Retain the intentionally noncontiguous pin for the negative
            // fixture without asking refresh to authorize that invalid gap.
            let mut files = guard::Files::new();
            for path in [
                STORE.to_string(),
                STORE_TESTS.to_string(),
                format!("crates/rsid-store/src/store/{label}_v{version}.rs"),
            ] {
                files.insert(
                    path.clone(),
                    std::fs::read_to_string(wt.join(&path)).unwrap(),
                );
            }
            write(
                &wt,
                MANIFEST,
                &format!(
                    "{}\n",
                    serde_json::to_string_pretty(&guard::inventory(&files).unwrap()).unwrap()
                ),
            );
        } else {
            guard::refresh(&Ctx::new(&wt, &[]), &self.base, &[]).unwrap();
        }
        commit(&wt)
    }
}
#[test]
fn canonical_commit_arguments_and_missing_options_name_remedies() {
    let ctx = Ctx::new(repo_root(), &[]);
    for argument in ["--base", "--source", "--target"] {
        let mut args = vec![
            "--repo".into(),
            ".".into(),
            "--base".into(),
            "0".repeat(40),
            "--source".into(),
            "0".repeat(40),
            "--target".into(),
            "0".repeat(40),
        ];
        let index = args.iter().position(|a| a == argument).unwrap() + 1;
        args[index] = "HEAD".into();
        // Validate each argument before resolving any commit, through oid itself.
        assert!(
            oid("HEAD", argument)
                .unwrap_err()
                .contains("pass a full 40-hex SHA")
        );
    }
    let mut out = Vec::new();
    let mut err = Vec::new();
    assert_eq!(
        main(&["--repo".into(), ".".into()], &ctx, &mut out, &mut err),
        2
    );
    assert!(
        String::from_utf8(err)
            .unwrap()
            .contains("--base, --source and --target are required")
    );
}
#[test]
fn two_sources_land_as_distinct_files_with_exact_parents_and_reproducible_proofs() {
    let f = Fixture::new();
    let a = f.source("alpha", false, false);
    let b = f.source("beta", false, false);
    let (ua, ca) = f.candidate(&a, &f.base);
    let (ub, cb) = f.candidate(&b, &ca);
    assert_eq!(
        text_run(&f.repo, &["show", "-s", "--format=%P", &cb]).unwrap(),
        format!("{ca} {b}")
    );
    assert_eq!(ub["new_version"], 131);
    assert_eq!(ua["new_version"], 130);
    let proof = prove(&f.repo, &ua, &ca, &cb).unwrap();
    assert_eq!(proof["assigned_version"], 130);
    assert_eq!(proof["final_version"], 131);
    assert!(
        show(&f.repo, &cb, &format!("{MIGRATION_DIR}/v130.rs"))
            .unwrap()
            .is_some()
    );
    assert!(
        show(&f.repo, &cb, &format!("{MIGRATION_DIR}/v131.rs"))
            .unwrap()
            .is_some()
    );
    guard::check(&Ctx::new(&f.repo, &[]), &f.base, &cb).unwrap();
    assert_eq!(
        text_run(&f.repo, &["show", "-s", "--format=%ce", &cb]).unwrap(),
        "rsi-rolling-land@rsi.invalid"
    );
}
#[test]
fn three_teardown_sources_keep_descending_order_and_discover_published_target_proofs() {
    let f = Fixture::new();
    let a = f.source("alpha", false, true);
    let b = f.source("beta", false, true);
    let c = f.source("gamma", false, true);
    let (ua, ca) = f.candidate(&a, &f.base);
    let (_, cb) = f.candidate(&b, &ca);
    let (_, cc) = f.candidate(&c, &cb);
    let text = String::from_utf8(show(&f.repo, &cc, STORE_TESTS).unwrap().unwrap()).unwrap();
    assert!(text.find("active >= 132").unwrap() < text.find("active >= 131").unwrap());
    assert!(text.find("active >= 131").unwrap() < text.find("active >= 130").unwrap());
    assert!(
        text.contains("target_version >= 132,")
            && text.contains("target_version >= 131,")
            && text.contains("target_version >= 130,")
    );
    assert_eq!(prove(&f.repo, &ua, &ca, &cc).unwrap()["final_version"], 132);
    assert_eq!(
        discover_target_provisional_units(&f.repo, &f.base, &cc)
            .unwrap()
            .len(),
        3
    );
}
#[test]
fn deferred_pin_and_noop_inspection_share_guard_policy() {
    let f = Fixture::new();
    let a = f.source("alpha", true, false);
    assert!(
        guard::check(&Ctx::new(&f.repo, &[]), &f.base, &a)
            .unwrap()
            .contains("pin deferred to the lander")
    );
    let noop = inspect(&f.repo, &f.base, &a, &f.base, true)
        .unwrap()
        .unwrap();
    assert_eq!(noop["renumber_noop"], true);
    let (_, published) = f.candidate(&a, &f.base);
    let noop = inspect(&f.repo, &f.base, &a, &published, true)
        .unwrap()
        .unwrap();
    assert_eq!(noop["migration_already_landed"], true);
    let bad = f.edit_declaration("alpha", |d| {
        d["files"][0]["sites"][0]["replacement"] = json!("bad")
    });
    // The no-renumber path must still validate malformed declarations.
    assert!(
        inspect(&f.repo, &f.base, &bad, &f.base, true)
            .unwrap_err()
            .contains("replacement")
    );
}
#[test]
fn undeclared_sites_changed_released_history_and_partial_pins_refuse() {
    let f = Fixture::new();
    f.source("alpha", true, false);
    let wt = f.worktree("alpha");
    write(&wt, "extra.rs", "const EXTRA: usize = 130;\n");
    let source = commit(&wt);
    assert!(
        inspect(&f.repo, &f.base, &source, &f.base, false)
            .unwrap_err()
            .contains("undeclared version-bearing")
    );
    write(
        &wt,
        &format!("{MIGRATION_DIR}/v129.rs"),
        "        if version < 129 {\n            changed();\n        }\n",
    );
    let source = commit(&wt);
    assert!(
        inspect(&f.repo, &f.base, &source, &f.base, false)
            .unwrap_err()
            .contains("invalid migration inventory")
    );
    let f = Fixture::new();
    f.source("beta", false, false);
    let wt = f.worktree("beta");
    let mut pin: Value =
        serde_json::from_slice(&std::fs::read(wt.join(MANIFEST)).unwrap()).unwrap();
    pin["blocks"].as_object_mut().unwrap().shift_remove("130");
    write(&wt, MANIFEST, &pin.to_string());
    let source = commit(&wt);
    assert!(
        inspect(&f.repo, &f.base, &source, &f.base, false)
            .unwrap_err()
            .contains("invalid migration inventory")
    );
}
#[test]
fn undeclared_gate_and_unchanged_site_refuse_while_lockfiles_and_unchanged_literals_pass() {
    let f = Fixture::new();
    f.source("alpha", false, false);
    let source = f.edit_declaration("alpha", |d| {
        d["files"][0]["sites"].as_array_mut().unwrap().remove(1);
    });
    assert!(
        inspect(&f.repo, &f.base, &source, &f.base, false)
            .unwrap_err()
            .contains("undeclared version-bearing site")
    );
    let f = Fixture::new();
    f.source("beta", false, false);
    let wt = f.worktree("beta");
    write(&wt, "Cargo.lock", "checksum = \"130\"\n");
    let source = commit(&wt);
    inspect(&f.repo, &f.base, &source, &f.base, false).unwrap();
    let source = f.edit_declaration("beta", |d| {
        d["files"][1]["sites"].as_array_mut().unwrap().push(site(
            "assert_eq!(count, 130);".into(),
            "assert_eq!(count, ${VERSION});",
            "unit",
        ))
    });
    assert!(
        inspect(&f.repo, &f.base, &source, &f.base, false)
            .unwrap_err()
            .contains("unchanged base line")
    );
}
#[test]
fn declaration_shape_limits_uniqueness_and_blob_binding_apply_before_noop() {
    for mutation in 0..6 {
        let f = Fixture::new();
        f.source("alpha", true, false);
        let source = f.edit_declaration("alpha", |d| match mutation {
            0 => {
                d["files"][0]
                    .as_object_mut()
                    .unwrap()
                    .shift_remove("path_template");
            }
            1 => d["files"][0]["source_blob"] = json!("sha256:wrong"),
            2 => {
                let duplicate = d["files"][0].clone();
                d["files"].as_array_mut().unwrap().push(duplicate);
            }
            3 => d["files"][0]["sites"][0]["scope"] = json!("other"),
            4 => {
                d["files"][0]["sites"] = json!(vec![
                    site(
                        "fn migrate_v130(".into(),
                        "fn migrate_v${VERSION}(",
                        "unit"
                    );
                    declaration::MAX_SITES + 1
                ])
            }
            _ => d["files"][0]["path_template"] = json!("elsewhere.rs"),
        });
        assert!(
            inspect(&f.repo, &f.base, &source, &f.base, true).is_err(),
            "mutation {mutation}"
        );
    }
}
#[test]
fn proof_rejects_wrong_parents_changed_blocks_and_rewound_descendants() {
    let f = Fixture::new();
    let a = f.source("alpha", false, false);
    let (unit, candidate) = f.candidate(&a, &f.base);
    assert!(
        prove(&f.repo, &unit, &a, &candidate)
            .unwrap_err()
            .contains("exact (target, source) parents")
    );
    let tree = text_run(&f.repo, &["rev-parse", &format!("{}^{{tree}}", f.base)]).unwrap();
    let rewound = String::from_utf8(
        run(
            &f.repo,
            &["commit-tree", &tree, "-p", &candidate],
            Some(b"rewound\n"),
            &IDENTITY,
        )
        .unwrap(),
    )
    .unwrap()
    .trim()
    .to_string();
    assert!(prove(&f.repo, &unit, &candidate, &rewound).is_err());
    assert!(
        prove(&f.repo, &unit, &candidate, &f.base)
            .unwrap_err()
            .contains("final candidate ancestry")
    );
}
#[test]
fn nonmigration_conflicts_and_semantic_neighbours_refuse() {
    let f = Fixture::new();
    let a = f.source("alpha", false, false);
    let b = f.source("beta", false, false);
    write(&f.worktree("alpha"), "shared.txt", "alpha\n");
    let a2 = commit(&f.worktree("alpha"));
    write(&f.worktree("beta"), "shared.txt", "beta\n");
    let b2 = commit(&f.worktree("beta"));
    let (_, ca) = f.candidate(&a2, &f.base);
    let mut unit = inspect(&f.repo, &f.base, &b2, &ca, false).unwrap().unwrap();
    let t = transform(&f.repo, &unit).unwrap();
    for (k, v) in t.as_object().unwrap() {
        unit[k] = v.clone();
    }
    let scratch = f.tmp.path().join("scratch");
    assert!(
        build_candidate(&f.repo, &mut unit, &scratch)
            .unwrap_err()
            .contains("nonmigration conflict")
    );
    // Both original sources still identify exact immutable objects.
    assert_ne!(a, b);
}
#[test]
fn teardown_parser_fixture_grammar_and_attributes_preserve_conflict_boundaries() {
    let good = block("alpha", 130);
    assert_eq!(
        teardown_blocks(good.as_bytes()).unwrap()[&130],
        0..good.len()
    );
    assert!(
        teardown_blocks(good.replace("target < 130", "target < 131").as_bytes())
            .unwrap_err()
            .contains("two versions")
    );
    assert!(teardown_blocks(good.trim_end_matches("    }\n").as_bytes()).is_err());
    assert!(teardown_blocks(good.repeat(2).as_bytes()).is_err());
    let lower = block("beta", 129);
    let ordered = ordered_teardown_insertions(&[lower.as_bytes()], &[good.as_bytes()])
        .unwrap()
        .unwrap();
    assert!(ordered.starts_with(good.as_bytes()));
    assert!(
        ordered_teardown_insertions(
            &[good.as_bytes()],
            &[format!("{lower}extra();\n").as_bytes()]
        )
        .unwrap()
        .is_none()
    );
    let versions = BTreeSet::from([130]);
    let valid = fixture_run("alpha", 130);
    assert!(fixture_insertion_declared(&[valid.as_bytes()], &versions));
    for invalid in [
        valid.replace(">= 130", ">= 129"),
        format!("{valid}    extra();\n"),
        valid.replace("alpha_present,", "other_present,"),
        valid.split("    assert_eq!").next().unwrap().into(),
        format!(
            "{valid}{}",
            fixture_run("unpaired", 130)
                .split("    assert_eq!")
                .next()
                .unwrap()
        ),
    ] {
        assert!(
            !fixture_insertion_declared(&[invalid.as_bytes()], &versions),
            "{invalid}"
        );
    }
    let data = b"keep();\n#[cfg(test)]\n#[allow(dead_code)]\nconst X: i32 = 7;\n";
    let start = occurrences(data, b"const")[0];
    assert_eq!(attribute_lines_start(data, start), 8);
    assert_eq!(attribute_lines_start(b"keep(); const X: i32 = 7;\n", 8), 8);
}
#[test]
fn matching_blocks_follow_leftmost_longest_and_masks_never_authorize_extra_lines() {
    let a: Vec<&[u8]> = vec![b"a\n", b"b\n", b"a\n"];
    let b: Vec<&[u8]> = vec![b"b\n", b"a\n", b"b\n"];
    assert_eq!(matching_blocks(&a, &b), vec![(0, 1, 2), (3, 3, 0)]);
    let source = b"keep\nunit\nextra\n";
    let base: Vec<&[u8]> = vec![b"keep\n"];
    let theirs: Vec<&[u8]> = source.split_inclusive(|b| *b == b'\n').collect();
    let mut mask = vec![false; source.len()];
    mask[5..10].fill(true);
    assert!(!changed_lines_are_declared(&base, &theirs, source, &mask));
    mask[10..].fill(true);
    assert!(changed_lines_are_declared(&base, &theirs, source, &mask));
}

#[test]
fn malformed_and_unprovable_landed_declarations_cannot_mask_target_sites() {
    for malformed in [true, false] {
        let f = Fixture::new();
        f.source("owner", false, false);
        let owner = if malformed {
            f.edit_declaration("owner", |d| {
                d["files"][0]
                    .as_object_mut()
                    .unwrap()
                    .shift_remove("path_template");
            })
        } else {
            let wt = f.worktree("owner");
            write(
                &wt,
                "crates/rsid-store/src/store/extra.rs",
                "const SCHEMA: i32 = 130;\n",
            );
            let owner = commit(&wt);
            assert!(
                inspect(&f.repo, &f.base, &owner, &owner, false)
                    .unwrap_err()
                    .contains("undeclared version-bearing")
            );
            owner
        };
        let tree = git(&f.repo, &["rev-parse", &format!("{owner}^{{tree}}")]);
        let target = git(
            &f.repo,
            &[
                "commit-tree",
                &tree,
                "-p",
                &f.base,
                "-p",
                &owner,
                "-m",
                "landed historical declaration",
            ],
        );
        assert_eq!(
            discover_target_provisional_units(&f.repo, &f.base, &target).unwrap(),
            Vec::<Value>::new()
        );
        let source = f.source("beta", false, false);
        let mut unit = inspect(&f.repo, &f.base, &source, &target, false)
            .unwrap()
            .unwrap();
        assert_eq!(unit["new_version"], 131);
        unit["prior_units"] = json!([]);
        let data = show(&f.repo, &target, STORE_TESTS).unwrap().unwrap();
        let mask = target_migration_mask(&f.repo, &mut unit, STORE_TESTS, &data).unwrap();
        assert_eq!(unit["_verified_target_units"], json!([]));
        assert_eq!(mask, vec![false; data.len()]);
    }
}

#[test]
fn prior_unit_refusal_names_the_owner_source_and_declaration() {
    let f = Fixture::new();
    let first = f.source("alpha", false, false);
    let (_, candidate) = f.candidate(&first, &f.base);
    f.source("owner", false, false);
    let owner = f.edit_declaration("owner", |d| {
        d["files"][0]
            .as_object_mut()
            .unwrap()
            .shift_remove("path_template");
    });
    let second = f.source("beta", false, false);
    let mut unit = inspect(&f.repo, &f.base, &second, &candidate, false)
        .unwrap()
        .unwrap();
    unit["prior_units"] =
        json!([{"base": f.base, "source": owner, "target": f.base, "unit_candidate": candidate}]);
    unit["_verified_target_units"] = json!([]);
    let filename = format!("{MIGRATION_DIR}/v130.rs");
    let data = show(&f.repo, &candidate, &filename).unwrap().unwrap();
    let err = target_migration_mask(&f.repo, &mut unit, &filename, &data).unwrap_err();
    assert!(err.contains(&owner), "{err}");
    assert!(
        err.contains("missing key `path_template`") && err.contains("owner.json"),
        "{err}"
    );
}

#[test]
fn already_landed_migration_keeps_its_number_after_head_advances_and_refuses_different_bytes() {
    for mode in ["same", "advance", "different", "malformed"] {
        let f = Fixture::new();
        let migration = f.source("alpha", false, false);
        if mode == "malformed" {
            let owner = f.edit_declaration("alpha", |d| {
                d["files"][0]
                    .as_object_mut()
                    .unwrap()
                    .shift_remove("path_template");
            });
            assert_eq!(
                inspect(&f.repo, &f.base, &owner, &owner, true)
                    .unwrap()
                    .unwrap()["renumber_noop"],
                true
            );
            continue;
        }
        let tree = git(&f.repo, &["rev-parse", &format!("{migration}^{{tree}}")]);
        let target = git(
            &f.repo,
            &[
                "commit-tree",
                &tree,
                "-p",
                &f.base,
                "-m",
                "landed through another source",
            ],
        );
        let wt = f.tmp.path().join("target");
        git(
            &f.repo,
            &[
                "worktree",
                "add",
                "-q",
                "--detach",
                wt.to_str().unwrap(),
                &target,
            ],
        );
        if mode == "advance" {
            write(
                &wt,
                &format!("{MIGRATION_DIR}/v131.rs"),
                "if version < 131 {\n    later();\n}\n",
            );
            guard::refresh(&Ctx::new(&wt, &[]), &target, &[]).unwrap();
        } else if mode == "different" {
            let filename = format!("{MIGRATION_DIR}/v130.rs");
            let body = format!(
                "{}// landed fix outside the hashed gate\n",
                std::fs::read_to_string(wt.join(&filename)).unwrap()
            );
            write(&wt, &filename, &body);
            let name = format!("{DECLARATIONS}alpha.json");
            let mut d: Value =
                serde_json::from_slice(&std::fs::read(wt.join(&name)).unwrap()).unwrap();
            d["files"][0]["source_blob"] = json!(guard::digest(&body));
            write(&wt, &name, &d.to_string());
        }
        let target = if mode == "same" { target } else { commit(&wt) };
        let swt = f.worktree("alpha");
        write(
            &swt,
            "docs/contract.md",
            "V130 contract for the next slice\n",
        );
        write(
            &swt,
            STORE_TESTS,
            &format!(
                "{}// later slice\n",
                TESTS.replace("REWIND: i32 = 129", "REWIND: i32 = 130")
            ),
        );
        let source = commit(&swt);
        assert_eq!(git(&f.repo, &["merge-base", &source, &target]), f.base);
        let result = inspect(&f.repo, &f.base, &source, &target, true);
        if mode == "different" {
            let err = result.unwrap_err();
            for expected in [
                "already on rolling with differing content",
                "re-merge the source onto origin/rolling",
                "retain rolling's released migration",
                "re-submit the merged commit",
            ] {
                assert!(err.contains(expected), "{err}");
            }
        } else {
            let unit = result.unwrap().unwrap();
            assert_eq!(unit["renumber_noop"], true);
            assert_eq!(unit["migration_already_landed"], true);
            assert_eq!(unit["old_version"], 130);
            assert_eq!(unit["new_version"], 130);
            assert_eq!(
                revision_inventory(&f.repo, &target, None).unwrap()["latest_schema_version"],
                if mode == "advance" { 131 } else { 130 }
            );
            if mode == "same" {
                git(
                    &wt,
                    &[
                        "merge",
                        "--no-ff",
                        "-q",
                        &source,
                        "-m",
                        "plain stacked merge",
                    ],
                );
                let candidate = git(&wt, &["rev-parse", "HEAD"]);
                assert_eq!(
                    git(&wt, &["show", "-s", "--format=%P", &candidate]),
                    format!("{target} {source}")
                );
                assert_eq!(
                    show(&f.repo, &candidate, &format!("{MIGRATION_DIR}/v130.rs")).unwrap(),
                    show(&f.repo, &target, &format!("{MIGRATION_DIR}/v130.rs")).unwrap()
                );
                assert_eq!(
                    git(&wt, &["show", &format!("{candidate}:docs/contract.md")]),
                    "V130 contract for the next slice"
                );
            }
        }
    }
}

#[test]
fn fixture_query_pairs_accept_count_and_contains_shapes_and_refuse_unpaired_statements() {
    let plain = fixture_run("beta", 130);
    let counted = "    let n: i64 = connection\n        .query_row(\n            \"SELECT COUNT(*) FROM sqlite_master\",\n            [],\n            |row| row.get(0),\n        )\n        .unwrap();\n    assert_eq!(\n        n,\n        if target_version >= 130 { 3 } else { 0 },\n        \"fixture V{target_version}: n\"\n    );\n";
    let contained = "    let sql: String = connection.query_row(\n        \"SELECT sql FROM sqlite_master\", [], |row| row.get::<_, String>(0)\n    ).unwrap();\n    assert_eq!(sql.contains(\"column\"), target_version >= 130,\n        \"fixture V{target_version}: sql\");\n";
    assert!(fixture_insertion_declared(
        &[plain.as_bytes(), counted.as_bytes(), contained.as_bytes()],
        &BTreeSet::from([130])
    ));
    assert!(!fixture_insertion_declared(
        &[counted.as_bytes()],
        &BTreeSet::from([131])
    ));
    let probe =
        "    let unused = connection.query_row(\"SELECT 1\", [], |row| row.get(0)).unwrap();\n";
    let (query, rest) = plain.split_once("    assert_eq!").unwrap();
    let assertion = format!("    assert_eq!{rest}");
    for invalid in [
        format!("{probe}{plain}"),
        format!("{query}{probe}{assertion}"),
        format!("{plain}{probe}"),
        probe.into(),
        assertion.clone(),
        format!("{plain}{assertion}"),
    ] {
        assert!(
            !fixture_insertion_declared(&[invalid.as_bytes()], &BTreeSet::from([130])),
            "{invalid}"
        );
    }
}

#[test]
fn pending_sibling_must_publish_before_gapped_source_can_build_and_prove() {
    // The legacy pending-sibling success case uses inline migrations. The
    // per-file extension is a separate inherited defect tracked in #438.
    let f = Fixture::inline();
    let sibling = f.inline_source("alpha", "");
    let source = f.inline_numbered_source("beta", 131, &[130]);
    assert!(
        inspect(&f.repo, &f.base, &source, &f.base, false)
            .unwrap_err()
            .contains("pending predecessor")
    );
    guard::check(&Ctx::new(&f.repo, &[]), &f.base, &source).unwrap();
    let undeclared = f.inline_numbered_source("gamma", 131, &[]);
    assert!(
        guard::check(&Ctx::new(&f.repo, &[]), &f.base, &undeclared)
            .unwrap_err()
            .contains("pending_predecessors")
    );
    assert!(inspect(&f.repo, &f.base, &undeclared, &f.base, false).is_err());
    let (_, target) = f.candidate(&sibling, &f.base);
    let (unit, candidate) = f.candidate(&source, &target);
    assert_eq!(unit["old_version"], 131);
    assert_eq!(unit["new_version"], 131);
    let inventory = revision_inventory(&f.repo, &candidate, None).unwrap();
    assert_eq!(inventory["latest_schema_version"], 131);
    for n in [130, 131] {
        assert!(inventory["blocks"].get(n.to_string()).is_some());
    }
    let proof = prove(&f.repo, &unit, &candidate, &candidate).unwrap();
    assert_eq!(proof["assigned_version"], 131);
    guard::check(&Ctx::new(&f.repo, &[]), &target, &candidate).unwrap();
}

#[test]
fn inline_units_rename_helpers_rebind_prior_proofs_and_preserve_nonmigration_hunks() {
    let f = Fixture::inline();
    let a = f.inline_source("alpha", "");
    f.inline_source("beta", "");
    write(
        &f.worktree("beta"),
        "docs/accepted-source.txt",
        "accepted source content\n",
    );
    let b = commit(&f.worktree("beta"));
    let c = f.inline_source("gamma", "");
    let (ua, ca) = f.candidate(&a, &f.base);
    let (ub, cb) = f.candidate(&b, &ca);
    let (_, cc) = f.candidate(&c, &cb);
    assert_eq!(ub["new_version"], 131);
    let text = String::from_utf8(show(&f.repo, &cc, STORE).unwrap().unwrap()).unwrap();
    for expected in [
        "LATEST_SCHEMA_VERSION: i32 = 132",
        "mod alpha_v130;",
        "mod beta_v131;",
        "mod gamma_v132;",
    ] {
        assert!(text.contains(expected), "{expected}: {text}");
    }
    assert!(text.find("if version < 130").unwrap() < text.find("if version < 131").unwrap());
    assert!(text.find("if version < 131").unwrap() < text.find("if version < 132").unwrap());
    let helper = show(&f.repo, &cc, "crates/rsid-store/src/store/beta_v131.rs")
        .unwrap()
        .unwrap();
    assert!(
        String::from_utf8(helper)
            .unwrap()
            .contains("pub fn apply_v131() {}")
    );
    assert_eq!(
        show(&f.repo, &cc, "docs/accepted-source.txt")
            .unwrap()
            .unwrap(),
        b"accepted source content\n"
    );
    assert_eq!(prove(&f.repo, &ua, &ca, &cc).unwrap()["final_version"], 132);
    assert_eq!(
        prove(&f.repo, &ub, &cb, &cc).unwrap()["assigned_version"],
        131
    );
    assert_eq!(
        discover_target_provisional_units(&f.repo, &f.base, &cc)
            .unwrap()
            .len(),
        3
    );
    guard::check(&Ctx::new(&f.repo, &[]), &f.base, &cc).unwrap();
}

#[test]
fn inline_conflicts_refuse_semantic_neighbours_from_either_side() {
    for on_target in [false, true] {
        let f = Fixture::inline();
        let a = f.inline_source(
            "alpha",
            if on_target {
                "semantic_change();\n"
            } else {
                ""
            },
        );
        let b = f.inline_source(
            "beta",
            if on_target {
                ""
            } else {
                "semantic_change();\n"
            },
        );
        let (_, target) = f.candidate(&a, &f.base);
        let mut unit = inspect(&f.repo, &f.base, &b, &target, false)
            .unwrap()
            .unwrap();
        for (k, v) in transform(&f.repo, &unit).unwrap().as_object().unwrap() {
            unit[k] = v.clone();
        }
        assert!(
            build_candidate(&f.repo, &mut unit, &f.tmp.path().join("scratch"))
                .unwrap_err()
                .contains("unproved migration conflict")
        );
    }
}

fn excerpt_tests(fixture: &Value, label: &str, extra: &str) -> String {
    let constants = fixture[label]["constants"].as_str().unwrap().replace(
        "const FLEET_INDEX_SCHEMA_VERSION:",
        &format!("{extra}const FLEET_INDEX_SCHEMA_VERSION:"),
    );
    format!(
        "// RSI-RELEASED-MIGRATION-BEGIN: test-catalog\n// fixed\n// RSI-RELEASED-MIGRATION-END: test-catalog\n{constants}\nfn rewind(active: i32, target: i32) {{\n{}{}}}\n",
        if label == "base" {
            ""
        } else {
            fixture[label]["teardown"].as_str().unwrap()
        },
        fixture["base"]["teardown"].as_str().unwrap()
    )
}
fn excerpt_source(f: &Fixture, fixture: &Value, label: &str, extra: &str) -> String {
    let wt = f.worktree(label);
    git(
        &f.repo,
        &[
            "worktree",
            "add",
            "-q",
            "--detach",
            wt.to_str().unwrap(),
            &f.base,
        ],
    );
    let tests = excerpt_tests(fixture, label, extra);
    let migration = fixture[label]["migration"].as_str().unwrap();
    let path = format!("{MIGRATION_DIR}/v189.rs");
    write(&wt, STORE_TESTS, &tests);
    write(&wt, &path, migration);
    guard::refresh(&Ctx::new(&wt, &[]), &f.base, &[]).unwrap();
    let mut test_sites = vec![
        site(
            "const REWIND_TEARDOWN_COVERED_THROUGH: i32 = 189;".into(),
            "const REWIND_TEARDOWN_COVERED_THROUGH: i32 = ${VERSION};",
            "head",
        ),
        site(
            "if active >= 189 && target < 189 {".into(),
            "if active >= ${VERSION} && target < ${VERSION} {",
            "unit",
        ),
        site(
            ".pragma_update(None, \"user_version\", 189 - 1)".into(),
            ".pragma_update(None, \"user_version\", ${VERSION} - 1)",
            "unit",
        ),
    ];
    if label == "landed" {
        test_sites.push(site(
            "pub(crate) const SESSION_START_PREAMBLE_SCHEMA_VERSION: i32 = 189;".into(),
            "pub(crate) const SESSION_START_PREAMBLE_SCHEMA_VERSION: i32 = ${VERSION};",
            "unit",
        ));
    }
    let d = json!({"schema_version":1,"version":189,"files":[
        {"path":STORE_TESTS,"path_template":STORE_TESTS,"source_blob":guard::digest(&tests),"sites":test_sites},
        {"path":path,"path_template":format!("{MIGRATION_DIR}/v${{VERSION}}.rs"),"source_blob":guard::digest(migration),"sites":[
            site("fn migrate_v189(".into(),"fn migrate_v${VERSION}(","unit"),
            site("if version < 189 {".into(),"if version < ${VERSION} {","unit"),
            site("tx.pragma_update(None, \"user_version\", 189)?;".into(),"tx.pragma_update(None, \"user_version\", ${VERSION})?;","unit")]}]});
    write(&wt, &format!("{DECLARATIONS}{label}.json"), &d.to_string());
    commit(&wt)
}

#[test]
fn recorded_v189_collision_preserves_attribute_ownership_and_refuses_nonunit_neighbours() {
    let fixture: Value = serde_json::from_slice(
        &std::fs::read(repo_root().join("scripts/tests/fixtures/landed-v189-renumber.json"))
            .unwrap(),
    )
    .unwrap();
    for mode in ["normal", "reverse", "extra_target", "extra_source"] {
        let mut f = Fixture::new();
        std::fs::rename(
            f.repo.join(format!("{MIGRATION_DIR}/v129.rs")),
            f.repo.join(format!("{MIGRATION_DIR}/v188.rs")),
        )
        .unwrap();
        let migration = "        if version < 188 {\n            settled();\n        }\n";
        let tests = excerpt_tests(&fixture, "base", "");
        write(&f.repo, &format!("{MIGRATION_DIR}/v188.rs"), migration);
        write(&f.repo, STORE_TESTS, &tests);
        let files = guard::Files::from([
            (STORE.into(), "// runner only\n".into()),
            (STORE_TESTS.into(), tests),
            (
                format!("{MIGRATION_DIR}/v000.rs"),
                "// V0: Original schema\n// original\n        Ok(())\n".into(),
            ),
            (format!("{MIGRATION_DIR}/v188.rs"), migration.into()),
        ]);
        write(
            &f.repo,
            MANIFEST,
            &format!(
                "{}\n",
                serde_json::to_string_pretty(&guard::inventory(&files).unwrap()).unwrap()
            ),
        );
        f.base = commit(&f.repo);
        let extra = "const UNRELATED: i32 = 7;\n";
        let landed = excerpt_source(
            &f,
            &fixture,
            "landed",
            if mode == "extra_target" { extra } else { "" },
        );
        let source = excerpt_source(
            &f,
            &fixture,
            "source",
            if mode == "extra_source" { extra } else { "" },
        );
        let (first, second, first_label, second_label) = if mode == "reverse" {
            (&source, &landed, "source", "landed")
        } else {
            (&landed, &source, "landed", "source")
        };
        let (_, target) = f.candidate(first, &f.base);
        let result = f.try_candidate(second, &target);
        if mode.starts_with("extra_") {
            assert!(
                result
                    .unwrap_err()
                    .contains(&format!("unproved migration conflict: {STORE_TESTS}"))
            );
            continue;
        }
        let (unit, candidate) = result.unwrap();
        assert_eq!(unit["old_version"], 189);
        assert_eq!(unit["new_version"], 190);
        let text =
            String::from_utf8(show(&f.repo, &candidate, STORE_TESTS).unwrap().unwrap()).unwrap();
        let constants = fixture["landed"]["constants"]
            .as_str()
            .unwrap()
            .split_once('\n')
            .unwrap()
            .1;
        let expected = if mode == "reverse" {
            constants.replace("189", "190")
        } else {
            constants.into()
        };
        assert!(text.contains(&expected), "{text}");
        assert!(text.contains("const REWIND_TEARDOWN_COVERED_THROUGH: i32 = 190;"));
        assert!(
            text.contains(&format!(
                "{}{}",
                fixture[second_label]["teardown"]
                    .as_str()
                    .unwrap()
                    .replace("189", "190"),
                fixture[first_label]["teardown"].as_str().unwrap()
            )),
            "{text}"
        );
        assert_eq!(
            show(&f.repo, &candidate, &format!("{MIGRATION_DIR}/v189.rs"))
                .unwrap()
                .unwrap(),
            fixture[first_label]["migration"]
                .as_str()
                .unwrap()
                .as_bytes()
        );
        assert_eq!(
            show(&f.repo, &candidate, &format!("{MIGRATION_DIR}/v190.rs"))
                .unwrap()
                .unwrap(),
            fixture[second_label]["migration"]
                .as_str()
                .unwrap()
                .replace("189", "190")
                .as_bytes()
        );
        assert_eq!(
            prove(&f.repo, &unit, &candidate, &candidate).unwrap()["assigned_version"],
            190
        );
    }
}

#[test]
fn cli_serialized_transform_build_and_prove_revalidate_the_unit_binding() {
    let f = Fixture::new();
    let first = f.source("alpha", false, true);
    let second = f.source("beta", false, true);
    let (_, target) = f.candidate(&first, &f.base);
    let ctx = Ctx::new(&f.repo, &[]);
    let repo = f.repo.to_str().unwrap();
    let (code, out, err) = crate::testutil::run(
        main,
        &[
            "--repo",
            repo,
            "--transform",
            "--base",
            &f.base,
            "--source",
            &second,
            "--target",
            &target,
        ],
        &ctx,
    );
    assert_eq!(code, 0, "{err}");
    let unit: Value = serde_json::from_str(&out).unwrap();
    assert_eq!(unit["old_version"], 130);
    assert_eq!(unit["new_version"], 131);
    let unit_file = f.tmp.path().join("unit.json");
    std::fs::write(&unit_file, &out).unwrap();
    let unit_path = unit_file.to_str().unwrap();
    let scratch = f.tmp.path().join("scratch");
    let args = [
        "--repo",
        repo,
        "--build",
        "--unit-file",
        unit_path,
        "--scratch",
        scratch.to_str().unwrap(),
    ];
    let (code, out, err) = crate::testutil::run(main, &args, &ctx);
    assert_eq!(code, 0, "{err}");
    let built: Value = serde_json::from_str(&out).unwrap();
    let candidate = built["candidate"].as_str().unwrap();
    let (code, out, err) = crate::testutil::run(
        main,
        &[
            "--repo",
            repo,
            "--prove",
            "--unit-file",
            unit_path,
            "--unit-candidate",
            candidate,
            "--candidate",
            candidate,
        ],
        &ctx,
    );
    assert_eq!(code, 0, "{err}");
    let proof: Value = serde_json::from_str(&out).unwrap();
    assert_eq!(proof["source"], second);
    assert_eq!(proof["target"], target);
    assert_eq!(proof["candidate"], candidate);
    assert_eq!(proof["assigned_version"], 131);
    assert_eq!(proof["final_version"], 131);
    assert_eq!(
        git(&f.repo, &["show", "-s", "--format=%P", candidate]),
        format!("{target} {second}")
    );
    let mut stale = unit;
    stale["new_version"] = json!(999);
    std::fs::write(&unit_file, stale.to_string()).unwrap();
    let (code, _, err) = crate::testutil::run(main, &args, &ctx);
    assert_eq!(code, 2);
    assert!(
        err.contains("provisional unit changed before conflict resolution"),
        "{err}"
    );
    guard::check(&ctx, &f.base, candidate).unwrap();
}

#[test]
fn inherited_landed_declaration_is_skipped_but_new_exact_parent_unit_is_discovered() {
    let f = Fixture::new();
    let migration = f.source("alpha", false, false);
    let tree = git(&f.repo, &["rev-parse", &format!("{migration}^{{tree}}")]);
    let parent = git(
        &f.repo,
        &[
            "commit-tree",
            &tree,
            "-p",
            &f.base,
            "-m",
            "landed by another source",
        ],
    );
    let wt = f.worktree("alpha");
    write(&wt, "docs/stacked.txt", "stacked change\n");
    let stacked = commit(&wt);
    let tree = git(&f.repo, &["rev-parse", &format!("{stacked}^{{tree}}")]);
    let target = git(
        &f.repo,
        &[
            "commit-tree",
            &tree,
            "-p",
            &parent,
            "-p",
            &stacked,
            "-m",
            "land stacked source",
        ],
    );
    assert_eq!(
        discover_target_provisional_units(&f.repo, &f.base, &target).unwrap(),
        Vec::<Value>::new()
    );
    let (_, published) = f.candidate(&migration, &f.base);
    let units = discover_target_provisional_units(&f.repo, &f.base, &published).unwrap();
    assert_eq!(units.len(), 1);
    assert_eq!(units[0]["source"], migration);
    assert_eq!(units[0]["unit_candidate"], published);
}

#[test]
fn target_merge_that_dropped_its_migration_refuses_prior_proof_discovery() {
    let f = Fixture::new();
    let source = f.source("alpha", false, false);
    let tree = git(&f.repo, &["rev-parse", &format!("{}^{{tree}}", f.base)]);
    let candidate = git(
        &f.repo,
        &[
            "commit-tree",
            &tree,
            "-p",
            &f.base,
            "-p",
            &source,
            "-m",
            "unproved migration merge",
        ],
    );
    let err = discover_target_provisional_units(&f.repo, &f.base, &candidate).unwrap_err();
    assert!(
        err.contains("migration block") || err.contains("released prefix"),
        "{err}"
    );
    assert!(err.contains(&source), "{err}");
}
