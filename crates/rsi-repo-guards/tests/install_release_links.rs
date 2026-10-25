//! #1137: `install-release.sh` links rsi-spill next to rsi-rpc, and the link runs
//! as printed (the spill stub says a bare `rsi-spill show ...`). hub #2958: the
//! link resolves to a stable copy under ~/.rsi/bin, never into the installing
//! checkout. #1164, #1741: rsid, cargo-slot and target-pool install to one
//! stable place each; #3771: rsi-cloud is installed and linked with the set.
//!
//! Ported from scripts/tests/test_install_release_links.py (#3771). Hermetic: a
//! temp HOME, a fake cargo and a pre-built fake release directory, with the
//! shared fail-closed lifecycle/process boundary (#632).
#![cfg(unix)]

mod install_release_support;

use std::fs;
use std::io::Read;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::Command;
use std::time::Duration;

use install_release_support::{
    Fixture, RELEASE_BINS, Run, dotted, is_executable, is_plain_file, run_bounded, write_executable,
};

fn fixture() -> Fixture {
    Fixture::new("rsi-install-links-")
}

fn install(f: &Fixture) -> Run {
    f.run_ok(&["--link-only", "--no-restart"], &[])
}

fn mode(path: &Path) -> u32 {
    fs::metadata(path).unwrap().permissions().mode() & 0o777
}

fn canon(path: &Path) -> std::path::PathBuf {
    fs::canonicalize(path).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}

/// Run the linked `rsi-spill` from `cwd` with only HOME and `path_env` set.
fn run_linked(f: &Fixture, cwd: &Path, path_env: &str) -> Run {
    let mut command = Command::new(f.bin_dir.join("rsi-spill"));
    command
        .args(["show", "s/1", "--range", "1:3"])
        .env_clear()
        .env("HOME", &f.home)
        .env("PATH", path_env)
        .current_dir(cwd);
    run_bounded(command, Duration::from_secs(10))
}

fn elsewhere(f: &Fixture) -> std::path::PathBuf {
    let dir = f.base.join("elsewhere");
    fs::create_dir(&dir).unwrap();
    dir
}

fn bin_path(f: &Fixture) -> String {
    format!("{}:{}", f.bin_dir.display(), f.fakes.display())
}

#[test]
fn link_only_links_rsi_spill_next_to_rsi_rpc() {
    let f = fixture();
    let out = install(&f).out;
    let link = f.bin_dir.join("rsi-spill");
    assert!(
        fs::symlink_metadata(f.bin_dir.join("rsi-rpc"))
            .unwrap()
            .file_type()
            .is_symlink()
    );
    assert!(
        fs::symlink_metadata(&link)
            .unwrap()
            .file_type()
            .is_symlink()
    );
    let stable = f.rsi_home.join("bin/rsi-spill");
    assert_eq!(canon(&link), canon(&stable));
    assert!(out.contains(&format!("{} ->", link.display())), "{out}");
    assert_eq!(
        fs::read(&stable).unwrap(),
        fs::read(f.script("rsi-spill")).unwrap()
    );
    assert!(is_plain_file(&stable));
    assert_eq!(mode(&stable), 0o755);
    assert!(dotted(&f.rsi_home.join("bin"), ".rsi-spill.").is_empty());
}

#[test]
fn rsi_spill_link_never_resolves_into_the_checkout() {
    let f = fixture();
    install(&f);
    let resolved = canon(&f.bin_dir.join("rsi-spill"));
    assert!(
        !resolved.starts_with(canon(&f.root)),
        "{}",
        resolved.display()
    );
}

#[test]
fn linked_rsi_spill_survives_removal_of_the_checkout_copy() {
    let f = fixture();
    install(&f);
    fs::remove_dir_all(&f.root).unwrap();
    assert!(
        f.bin_dir.join("rsi-spill").exists(),
        "link dangles after the checkout is removed"
    );
    let run = run_linked(&f, &elsewhere(&f), &bin_path(&f));
    assert_eq!(run.code, 0, "{}", run.err);
    assert_eq!(run.out.trim(), "fake-rsi-rpc spill show s/1 --range 1:3");
}

#[test]
fn rsi_spill_is_atomically_refreshed_and_repoints_an_old_checkout_link() {
    let f = fixture();
    let stable = f.rsi_home.join("bin/rsi-spill");
    // An older install linked the checkout script directly.
    fs::create_dir_all(&f.bin_dir).unwrap();
    std::os::unix::fs::symlink(f.script("rsi-spill"), f.bin_dir.join("rsi-spill")).unwrap();
    install(&f);
    assert_eq!(canon(&f.bin_dir.join("rsi-spill")), canon(&stable));
    let source = f.script("rsi-spill");
    let original = fs::read(&stable).unwrap();
    let mut old_copy = fs::File::open(&stable).unwrap();
    let mut updated = original.clone();
    updated.extend_from_slice(b"\n# updated release\n");
    fs::write(&source, &updated).unwrap();
    install(&f);
    let mut seen = Vec::new();
    old_copy.read_to_end(&mut seen).unwrap();
    assert_eq!(seen, original, "an open reader keeps the old copy");
    assert_eq!(fs::read(&stable).unwrap(), fs::read(&source).unwrap());
    assert!(dotted(&f.rsi_home.join("bin"), ".rsi-spill.").is_empty());
}

#[test]
fn link_only_installs_rsid_under_rsi_install_and_links_there() {
    // #1164: the supervisor, the links and a deploy share one rsid path.
    let f = fixture();
    install(&f);
    let installed = f.rsi_home.join("install");
    for name in RELEASE_BINS {
        let copy = installed.join(name);
        assert!(is_plain_file(&copy), "{name}");
        assert!(is_executable(&copy), "{name}");
    }
    for name in [
        "rsi-known-failure",
        "rsi-socket-hold",
        "rsi-turn-shim",
        "rsid",
        "rsi-cloud",
    ] {
        assert_eq!(
            canon(&f.bin_dir.join(name)),
            canon(&installed.join(name)),
            "{name}"
        );
    }
    assert_ne!(canon(&f.bin_dir.join("rsid")), canon(&f.release("rsid")));
    // Reinstalling replaces the file atomically with the new build.
    write_executable(&f.release("rsid"), "#!/bin/sh\necho \"fake-rsid-v2\"\n");
    install(&f);
    assert!(
        fs::read_to_string(installed.join("rsid"))
            .unwrap()
            .contains("fake-rsid-v2")
    );
}

#[test]
fn rsi_cloud_is_installed_with_the_set_and_linked() {
    // #3771: the watchdog service and the cloud gates find rsi-cloud on PATH and
    // under ~/.rsi/install; it is installed as a stable copy, never Cargo's output.
    let f = fixture();
    write_executable(
        &f.release("rsi-cloud"),
        "#!/bin/sh\necho fake-rsi-cloud-v1\n",
    );
    install(&f);
    let installed = f.rsi_home.join("install/rsi-cloud");
    assert!(is_plain_file(&installed));
    assert!(is_executable(&installed));
    assert_eq!(
        fs::read(&installed).unwrap(),
        fs::read(f.release("rsi-cloud")).unwrap()
    );
    let link = f.bin_dir.join("rsi-cloud");
    assert!(
        fs::symlink_metadata(&link)
            .unwrap()
            .file_type()
            .is_symlink()
    );
    assert_ne!(canon(&link), canon(&f.release("rsi-cloud")));
    assert!(dotted(&f.rsi_home.join("install"), ".rsi-cloud.").is_empty());
}

#[test]
fn cargo_slot_is_installed_and_atomically_refreshed() {
    let f = fixture();
    let source = f.script("cargo-slot");
    let installed = f.rsi_home.join("bin/cargo-slot");
    let implementation = f.rsi_home.join("lib/cargo-slot");
    let wrapper = f.script("cargo-slot-wrapper");
    install(&f);
    assert_eq!(fs::read(&installed).unwrap(), fs::read(&wrapper).unwrap());
    assert_eq!(
        fs::read(&implementation).unwrap(),
        fs::read(&source).unwrap()
    );
    assert_eq!(mode(&installed), 0o755);
    assert_eq!(mode(&implementation), 0o755);

    // An open reader keeps the old implementation while install refreshes it.
    let original = fs::read(&implementation).unwrap();
    let mut old_copy = fs::File::open(&implementation).unwrap();
    let mut updated = original.clone();
    updated.extend_from_slice(b"\n# updated release\n");
    fs::write(&source, &updated).unwrap();
    fs::set_permissions(&source, fs::Permissions::from_mode(0o600)).unwrap();
    install(&f);
    let mut seen = Vec::new();
    old_copy.read_to_end(&mut seen).unwrap();
    assert_eq!(seen, original);
    assert_eq!(
        fs::read(&implementation).unwrap(),
        fs::read(&source).unwrap()
    );
    assert_eq!(fs::read(&installed).unwrap(), fs::read(&wrapper).unwrap());
    assert_eq!(mode(&implementation), 0o755);
    assert!(dotted(&f.rsi_home.join("bin"), ".cargo-slot.").is_empty());
    assert!(dotted(&f.rsi_home.join("lib"), ".cargo-slot.").is_empty());
}

#[test]
fn target_pool_is_installed_for_the_daemon_seed() {
    // #1741: the daemon runs ~/.rsi/bin/target-pool to seed worker targets.
    let f = fixture();
    let installed = f.rsi_home.join("bin/target-pool");
    install(&f);
    assert_eq!(
        fs::read(&installed).unwrap(),
        fs::read(f.script("target-pool")).unwrap()
    );
    assert_eq!(mode(&installed), 0o755);
    assert!(dotted(&f.rsi_home.join("bin"), ".target-pool.").is_empty());
}

#[test]
fn linked_rsi_spill_runs_from_another_cwd_via_path_rsi_rpc() {
    let f = fixture();
    install(&f);
    let run = run_linked(&f, &elsewhere(&f), &bin_path(&f));
    assert_eq!(run.code, 0, "{}", run.err);
    assert_eq!(run.out.trim(), "fake-rsi-rpc spill show s/1 --range 1:3");
}

#[test]
fn linked_rsi_spill_falls_back_to_the_installed_rsi_rpc() {
    // No rsi-rpc on PATH: the stable copy has no checkout beside it, so it finds
    // the installed ~/.rsi/install/rsi-rpc instead.
    let f = fixture();
    install(&f);
    fs::remove_file(f.bin_dir.join("rsi-rpc")).unwrap();
    write_executable(
        &f.rsi_home.join("install/rsi-rpc"),
        "#!/bin/sh\necho \"debug-rsi-rpc $*\"\n",
    );
    let run = run_linked(&f, &elsewhere(&f), &bin_path(&f));
    assert_eq!(run.code, 0, "{}", run.err);
    assert_eq!(run.out.trim(), "debug-rsi-rpc spill show s/1 --range 1:3");
}
