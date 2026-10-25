use super::*;

fn write(root: &Path, file: &str, text: &str) {
    let path = root.join(file);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, text).unwrap();
}

fn recipe(name: &str) -> String {
    format!(
        "[recipes.{name}]\nrunner = 'make'\ntarget = '{name}'\ntimeout_minutes = 20\ncpu_quota_percent = 200\n"
    )
}

fn profile() -> String {
    format!(
        "version = 1\nverification = ['test']\nbuild = 'build'\ncleanup = 'clean'\nwarnings = 'generic'\n[integration]\nbranch = 'develop'\nmode = 'fast_forward'\n[resources.build]\npath = 'build/output'\nsetup = 'build'\ncleanup = 'clean'\n{}{}{}",
        recipe("test"),
        recipe("build"),
        recipe("clean")
    )
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[test]
fn makefile_only_and_justfile_projects_need_no_rsi_files() {
    for (file, runner) in [("Makefile", Runner::Make), ("justfile", Runner::Just)] {
        let dir = crate::test_support::disk_backed_tempdir("profile-local");
        write(
            dir.path(),
            file,
            "test:\n\techo test\nbuild:\n\techo build\n",
        );
        let resolved = resolve(dir.path()).unwrap();
        assert_eq!(resolved.source, ProfileSource::LocalEvidence);
        assert_eq!(
            resolved.verification,
            Verification::Available(vec![ProjectCommand::Recipe(Recipe {
                runner,
                target: "test".into(),
                timeout_minutes: 20,
                cpu_quota_percent: 200,
            })])
        );
        assert_eq!(
            resolved.build,
            Some(ProjectCommand::Recipe(Recipe {
                runner,
                target: "build".into(),
                timeout_minutes: 20,
                cpu_quota_percent: 200,
            }))
        );
        assert_eq!(resolved.integration, None);
        assert_eq!(resolved.warnings, WarningPolicy::Generic);
        assert!(resolved.resources.is_empty());
    }
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[test]
fn rust_package_and_workspace_fallback_use_typed_cargo_roles() {
    let dir = crate::test_support::disk_backed_tempdir("profile-rust");
    for manifest in [
        "[package]\nname='example'\nversion='0.1.0'\n",
        "[workspace]\nmembers=[]\n",
    ] {
        write(dir.path(), "Cargo.toml", manifest);
        let resolved = resolve(dir.path()).unwrap();
        assert_eq!(
            resolved.verification,
            Verification::Available(vec![ProjectCommand::CargoTest])
        );
        assert_eq!(resolved.build, Some(ProjectCommand::CargoBuild));
        assert_eq!(resolved.warnings, WarningPolicy::NoNewRustWarnings);
        assert_eq!(resolved.integration, None);
    }
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[test]
fn empty_or_unrecognized_evidence_never_becomes_a_passing_test() {
    let dir = crate::test_support::disk_backed_tempdir("profile-no-tests");
    assert_eq!(
        resolve(dir.path()).unwrap().verification,
        Verification::NoTests
    );
    for text in [
        "all:\n",
        "test-other:\n",
        "test := value\n",
        "\ttest:\n",
        "test::\n",
    ] {
        write(dir.path(), "Makefile", text);
        assert_eq!(
            resolve(dir.path()).unwrap().verification,
            Verification::Unsupported
        );
    }
    write(dir.path(), PROFILE_PATH, "version = 1\n");
    assert_eq!(
        resolve(dir.path()).unwrap().verification,
        Verification::NoTests
    );
    // A malformed declaration must not fall through to locally detectable tests.
    write(dir.path(), "Makefile", "test:\n");
    write(dir.path(), PROFILE_PATH, "version = 99\n");
    assert_eq!(resolve(dir.path()), Err(ProfileError::UnknownVersion(99)));
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[test]
fn legacy_jobs_share_validation_and_merge_without_reinterpreting_commands() {
    let dir = crate::test_support::disk_backed_tempdir("profile-legacy");
    write(dir.path(), "Makefile", "test:\n");
    let jobs = format!("version = 1\n{}{}", recipe("test"), recipe("custom"));
    write(dir.path(), ".rsi/jobs.toml", &jobs);
    let old = super::super::recipe::load(dir.path()).unwrap().recipes;
    let legacy = resolve(dir.path()).unwrap();
    assert_eq!(legacy.source, ProfileSource::LegacyJobs);
    assert_eq!(legacy.recipes, old);
    assert_eq!(
        legacy.verification,
        Verification::Available(vec![ProjectCommand::Recipe(old["test"].clone())])
    );
    write(
        dir.path(),
        PROFILE_PATH,
        "version=1\nverification=['custom']\n",
    );
    let resolved = resolve(dir.path()).unwrap();
    assert_eq!(resolved.source, ProfileSource::Project);
    assert_eq!(
        resolved.verification,
        Verification::Available(vec![ProjectCommand::Recipe(old["custom"].clone())])
    );
    // Equal declarations coexist, conflicting caps/targets fail closed.
    write(
        dir.path(),
        PROFILE_PATH,
        &format!("version=1\n{}", recipe("test")),
    );
    assert_eq!(resolve(dir.path()).unwrap().recipes, old);
    write(
        dir.path(),
        PROFILE_PATH,
        &format!("version=1\n{}", recipe("test").replace("200", "100")),
    );
    assert_eq!(
        resolve(dir.path()),
        Err(ProfileError::ConflictingRecipe("test".into()))
    );
    write(
        dir.path(),
        ".rsi/jobs.toml",
        &jobs.replace("'make'", "'shell'"),
    );
    assert_eq!(resolve(dir.path()), Err(ProfileError::Invalid));
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[test]
fn explicit_profile_resolves_roles_branch_warning_policy_and_owned_resources() {
    let dir = crate::test_support::disk_backed_tempdir("profile-declared");
    write(dir.path(), "Makefile", "test:\nbuild:\nclean:\n");
    write(dir.path(), PROFILE_PATH, &profile());
    let resolved = resolve(dir.path()).unwrap();
    assert_eq!(
        resolved.integration,
        Some(Integration {
            branch: "develop".into(),
            mode: LandingMode::FastForward
        })
    );
    assert_eq!(
        resolved.verification,
        Verification::Available(vec![ProjectCommand::Recipe(
            resolved.recipes["test"].clone()
        )])
    );
    assert_eq!(
        resolved.build,
        Some(ProjectCommand::Recipe(resolved.recipes["build"].clone()))
    );
    assert_eq!(
        resolved.cleanup,
        Some(ProjectCommand::Recipe(resolved.recipes["clean"].clone()))
    );
    assert_eq!(
        resource_path(&resolved.root, &resolved.resources["build"].path).unwrap(),
        resolved.root.join("build/output")
    );
    assert_eq!(resolved.resources["build"].setup.as_deref(), Some("build"));
    assert_eq!(
        resolved.resources["build"].cleanup.as_deref(),
        Some("clean")
    );
    write(
        dir.path(),
        PROFILE_PATH,
        &profile()
            .replace("'fast_forward'", "'queue'")
            .replace("'generic'", "'no_new_rust_warnings'"),
    );
    assert_eq!(
        resolve(dir.path()).unwrap().integration.unwrap().mode,
        LandingMode::Queue
    );
    assert_eq!(
        resolve(dir.path()).unwrap().warnings,
        WarningPolicy::NoNewRustWarnings
    );
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[test]
fn invalid_versions_fields_commands_caps_and_unresolved_roles_are_typed_errors() {
    for bad in [
        "".into(),
        "version = -1".into(),
        "not TOML".into(),
        "version=1\nargv=['sh']".into(),
        profile().replace("'make'", "'sh'"),
        profile().replace("target = 'test'", "target = '--eval'"),
        profile().replace("timeout_minutes = 20", "timeout_minutes = 0"),
        profile().replace("timeout_minutes = 20", "timeout_minutes = 181"),
        profile().replace("cpu_quota_percent = 200", "cpu_quota_percent = 1601"),
        profile().replace("cpu_quota_percent = 200", "cpu_quota_percent = 0"),
        profile().replace("mode = 'fast_forward'", "mode = 'force'"),
        profile().replace("branch = 'develop'", "branch = 'refs/heads/main'"),
        profile().replace("branch = 'develop'", "branch = '../escape'"),
        profile().replace("verification = ['test']", "verification = ['scoped-test']"),
        "x".repeat(MAX_MANIFEST_BYTES as usize + 1),
        format!(
            "version=1\n{}",
            (0..65)
                .map(|n| recipe(&format!("t{n}")))
                .collect::<String>()
        ),
        format!(
            "version=1\n{}",
            (0..33)
                .map(|n| format!("[resources.r{n}]\npath='r{n}'\n"))
                .collect::<String>()
        ),
    ] {
        assert_eq!(parse(&bad), Err(ProfileError::Invalid), "{bad}");
    }
    assert_eq!(
        parse("version=2\nfuture_field=true"),
        Err(ProfileError::UnknownVersion(2))
    );
    let dir = crate::test_support::disk_backed_tempdir("profile-invalid-role");
    write(
        dir.path(),
        PROFILE_PATH,
        "version=1\nverification=['unknown']\n",
    );
    assert_eq!(
        resolve(dir.path()),
        Err(ProfileError::UnknownRecipe("unknown".into()))
    );
    write(dir.path(), PROFILE_PATH, &profile());
    assert_eq!(
        resolve(dir.path()),
        Err(ProfileError::MissingRecipeFile("Makefile".into()))
    );
    write(dir.path(), "Makefile", "test:\n");
    write(
        dir.path(),
        PROFILE_PATH,
        &profile().replace("target = 'test'", "target = 'scoped-test'"),
    );
    assert_eq!(resolve(dir.path()), Err(ProfileError::Invalid));
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[test]
fn resource_paths_refuse_portable_escapes_aliases_and_overlapping_ownership() {
    let dir = crate::test_support::disk_backed_tempdir("profile-resource-paths");
    for path in [
        "",
        "/tmp/build",
        "../build",
        "build/../src",
        "./build",
        "build//out",
        "build/",
        ".git",
        ".rsi",
        "C:/build",
        "C:\\build",
        "build\\out",
        "build.",
        "CON",
        "nul.txt",
        "aux/out",
        "build/out ",
    ] {
        assert_eq!(
            resource_path(dir.path(), path),
            Err(ProfileError::UnsafePath(path.into())),
            "{path}"
        );
    }
    write(dir.path(), "Makefile", "test:\n");
    for path in [
        "build/output",
        "build",
        "build/output/nested",
        "BUILD/Output",
    ] {
        write(
            dir.path(),
            PROFILE_PATH,
            &format!("{}\n[resources.second]\npath='{path}'\n", profile()),
        );
        assert_eq!(
            resolve(dir.path()),
            Err(ProfileError::AmbiguousOwnership(path.into()))
        );
    }
    std::fs::create_dir_all(dir.path().join("build/.git")).unwrap();
    assert_eq!(
        resource_path(dir.path(), "build/output"),
        Err(ProfileError::AmbiguousOwnership("build/output".into()))
    );
    write(dir.path(), "file", "content");
    assert_eq!(
        resource_path(dir.path(), "file/output"),
        Err(ProfileError::UnsafePath("file/output".into()))
    );
}

#[cfg(unix)]
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[test]
fn symlinked_manifests_runners_and_resources_refuse_internal_external_and_dangling_aliases() {
    use std::os::unix::fs::symlink;
    let dir = crate::test_support::disk_backed_tempdir("profile-symlinks");
    let other = crate::test_support::disk_backed_tempdir("profile-foreign");
    write(other.path(), PROFILE_PATH, "version=1");
    symlink(other.path().join(".rsi"), dir.path().join(".rsi")).unwrap();
    assert_eq!(
        resolve(dir.path()),
        Err(ProfileError::UnsafePath(PROFILE_PATH.into()))
    );
    std::fs::remove_file(dir.path().join(".rsi")).unwrap();
    write(dir.path(), "declaration", "version=1");
    std::fs::create_dir(dir.path().join(".rsi")).unwrap();
    for target in [
        dir.path().join("declaration"),
        other.path().join(PROFILE_PATH),
        other.path().join("missing"),
    ] {
        symlink(target, dir.path().join(PROFILE_PATH)).unwrap();
        assert_eq!(
            resolve(dir.path()),
            Err(ProfileError::UnsafePath(PROFILE_PATH.into()))
        );
        std::fs::remove_file(dir.path().join(PROFILE_PATH)).unwrap();
    }
    write(dir.path(), PROFILE_PATH, &profile());
    write(other.path(), "Makefile", "test:\n");
    symlink(other.path().join("Makefile"), dir.path().join("Makefile")).unwrap();
    assert_eq!(
        resolve(dir.path()),
        Err(ProfileError::UnsafePath("Makefile".into()))
    );
    std::fs::remove_file(dir.path().join("Makefile")).unwrap();
    write(dir.path(), "Makefile", "test:\n");
    std::fs::create_dir(dir.path().join("owned")).unwrap();
    for target in [
        dir.path().join("owned"),
        other.path().to_path_buf(),
        dir.path().join("missing"),
    ] {
        symlink(target, dir.path().join("build")).unwrap();
        assert_eq!(
            resolve(dir.path()),
            Err(ProfileError::UnsafePath("build/output".into()))
        );
        std::fs::remove_file(dir.path().join("build")).unwrap();
    }
    // A previously resolved missing path is not proof at apply time.
    let resolved = resolve(dir.path()).unwrap();
    symlink(other.path(), dir.path().join("build")).unwrap();
    assert_eq!(
        resource_path(&resolved.root, "build/output"),
        Err(ProfileError::UnsafePath("build/output".into()))
    );
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[test]
fn resolver_reads_only_bounded_files_from_the_exact_root() {
    let dir = crate::test_support::disk_backed_tempdir("profile-root");
    write(dir.path(), "Makefile", "test:\n");
    std::fs::create_dir(dir.path().join("child")).unwrap();
    assert_eq!(
        resolve(&dir.path().join("child")).unwrap().verification,
        Verification::NoTests
    );
    write(
        dir.path(),
        PROFILE_PATH,
        &"x".repeat(MAX_MANIFEST_BYTES as usize + 1),
    );
    assert_eq!(resolve(dir.path()), Err(ProfileError::Invalid));
}
