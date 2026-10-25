//! #136: `install-release.sh --stage-dir DIR` stages the built set into DIR and
//! touches nothing live (no ~/.rsi/install|bin|lib, no ~/.local/bin links, no
//! supervisor, no restart), and prints a sha256 manifest. `--no-restart` still
//! means "install but do not restart".
//!
//! #262 regressions (hub #2800, #2801, #2799, #2802): DIR is written as one fresh
//! tree, never through a symlink already in it; a staging build refuses while a
//! live link resolves into the cargo release directory; the staged names are
//! exactly the top-level files `AgentRequestDeploy` reads (`DEPLOY_BINARIES`,
//! `rsi-cloud` included, #3771); no GNU-only readlink.
//!
//! #3423: the quiet-point restart hands the daemon a staged directory.
//!
//! Ported from scripts/tests/test_install_release_stage.py (#3771). The shared
//! fail-closed boundary (#632) owns lifecycle calls; quiet restart uses only a
//! fixture socket and fake RPC executable.
#![cfg(unix)]

mod install_release_support;

use std::fs;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};

use install_release_support::{
    DEPLOY_SCRIPTS, Fixture, RELEASE_BINS, is_executable, is_plain_file, names, repo_root,
    sha256_hex, snapshot, text, write_executable,
};
use rsi_common::agent_deploy::DEPLOY_BINARIES;

fn fixture() -> Fixture {
    let f = Fixture::new("rsi-install-stage-");
    f.plant_live_install();
    f
}

fn read(path: &Path) -> Vec<u8> {
    fs::read(path).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}

fn assert_script_copy(f: &Fixture, name: &str) {
    assert_eq!(read(&f.stage.join(name)), read(&f.script(name)), "{name}");
}

#[test]
fn stage_dir_writes_the_set_and_touches_nothing_live() {
    let f = fixture();
    let before = f.live_state();
    let run = f.run_ok(&["--link-only", "--stage-dir", &text(&f.stage)], &[]);
    assert_eq!(f.live_state(), before);
    assert!(names(&f.called).is_empty());
    for name in RELEASE_BINS {
        let staged = f.stage.join(name);
        assert!(is_plain_file(&staged), "{name}");
        assert!(is_executable(&staged), "{name}");
    }
    for name in [
        "rsid-supervisor.sh",
        "cargo-slot",
        "target-pool",
        "rsi-spill",
    ] {
        assert_script_copy(&f, name);
    }
    // hub #2958: rsi-spill travels with the other copied tools.
    assert!(is_executable(&f.stage.join("rsi-spill")));
    assert!(!run.out.contains("Linked:"));
}

#[test]
fn stage_dir_stages_rsi_cloud() {
    // #3771: the spend safety net is a deploy member, so a staged directory (the
    // one an agent deploy or the quiet-point restart hands the daemon) carries it.
    let f = fixture();
    write_executable(
        &f.release("rsi-cloud"),
        "#!/bin/sh\necho fake-rsi-cloud-staged\n",
    );
    f.run_ok(
        &["--link-only", &format!("--stage-dir={}", text(&f.stage))],
        &[],
    );
    let staged = f.stage.join("rsi-cloud");
    assert!(is_plain_file(&staged));
    assert!(is_executable(&staged));
    assert_eq!(read(&staged), read(&f.release("rsi-cloud")));
    let manifest = fs::read_to_string(f.stage.join("MANIFEST.sha256")).unwrap();
    assert!(
        manifest.lines().any(|line| line.ends_with("  rsi-cloud")),
        "{manifest}"
    );
}

#[test]
fn stage_dir_prints_a_sha256_manifest_of_every_staged_file() {
    let f = fixture();
    let run = f.run_ok(
        &["--link-only", &format!("--stage-dir={}", text(&f.stage))],
        &[],
    );
    let staged: Vec<String> = names(&f.stage)
        .into_iter()
        .filter(|name| name != "MANIFEST.sha256")
        .collect();
    assert!(staged.iter().any(|name| name == "rsid"));
    assert!(staged.iter().any(|name| name == "rsid-supervisor.sh"));
    let printed: Vec<&str> = run.out.lines().collect();
    for rel in &staged {
        let digest = sha256_hex(&read(&f.stage.join(rel)));
        let line = format!("{digest}  {rel}");
        assert!(printed.contains(&line.as_str()), "{rel}: {}", run.out);
    }
    let written = fs::read_to_string(f.stage.join("MANIFEST.sha256")).unwrap();
    let listed: Vec<&str> = written
        .lines()
        .map(|line| line.split_once("  ").unwrap().1)
        .collect();
    assert_eq!(listed, staged);
}

#[test]
fn stage_dir_refuses_a_live_path() {
    let f = fixture();
    let before = f.live_state();
    for live in [
        f.rsi_home.join("install"),
        f.rsi_home.join("bin"),
        f.rsi_home.join("lib"),
        f.bin_dir.clone(),
        f.rsi_home.clone(),
        f.rsi_home.join("install/nested"),
    ] {
        let run = f.run_script(&["--link-only", "--stage-dir", &text(&live)], &[]);
        assert_ne!(run.code, 0, "{}", live.display());
        assert!(
            run.err.contains("overlaps the live path"),
            "{}: {}",
            live.display(),
            run.err
        );
    }
    assert_eq!(f.live_state(), before);
}

#[test]
fn stage_dir_cannot_combine_with_tui_only() {
    let f = fixture();
    let run = f.run_script(&["--tui-only", "--stage-dir", &text(&f.stage)], &[]);
    assert_ne!(run.code, 0);
    assert!(run.err.contains("--tui-only"), "{}", run.err);
    assert!(!f.stage.exists());
}

#[test]
fn stage_dir_needs_a_directory() {
    let f = fixture();
    let run = f.run_script(&["--stage-dir"], &[]);
    assert_ne!(run.code, 0);
    assert!(
        run.err.contains("--stage-dir needs a directory"),
        "{}",
        run.err
    );
}

#[test]
fn help_says_no_restart_is_not_staging() {
    let f = fixture();
    let run = f.run_ok(&["--help"], &[]);
    assert!(run.out.contains("--stage-dir DIR"), "{}", run.out);
    let no_restart = run
        .out
        .split("\n  --no-restart ")
        .nth(1)
        .and_then(|rest| rest.split("\n  --link-only ").next())
        .expect("--no-restart help paragraph");
    assert!(no_restart.contains("not staging"), "{no_restart}");
    assert!(no_restart.contains("overwritten"), "{no_restart}");
}

#[test]
fn no_restart_still_overwrites_the_live_install() {
    // The documented meaning of --no-restart: install, do not restart.
    let f = fixture();
    f.run_ok(&["--link-only", "--no-restart"], &[]);
    let installed = fs::read_to_string(f.rsi_home.join("install/rsid")).unwrap();
    assert!(installed.contains("fake-rsid"), "{installed}");
    assert!(names(&f.called).is_empty());
}

// --- #262 / hub #2800: never write through a path already inside DIR -------

#[test]
fn stage_dir_never_writes_through_a_symlink_inside_dir() {
    let f = fixture();
    fs::create_dir(&f.stage).unwrap();
    symlink(f.rsi_home.join("bin"), f.stage.join("tools")).unwrap();
    symlink(
        f.rsi_home.join("lib/cargo-slot"),
        f.stage.join("MANIFEST.sha256"),
    )
    .unwrap();
    let before = f.live_state();
    let planted = snapshot(&[&f.stage]);
    let run = f.run_script(&["--link-only", "--stage-dir", &text(&f.stage)], &[]);
    assert_ne!(run.code, 0);
    assert!(run.err.contains("not an empty directory"), "{}", run.err);
    assert_eq!(f.live_state(), before);
    assert_eq!(snapshot(&[&f.stage]), planted);
}

#[test]
fn stage_dir_fills_an_empty_dir_as_one_fresh_tree() {
    let f = fixture();
    fs::create_dir(&f.stage).unwrap();
    f.run_ok(&["--link-only", "--stage-dir", &text(&f.stage)], &[]);
    assert!(f.stage.join("rsid").is_file());
    assert!(f.stage.join("MANIFEST.sha256").is_file());
    // The private tree was renamed into place: the parent holds DIR alone.
    let hidden: Vec<String> = names(&f.base)
        .into_iter()
        .filter(|name| name.starts_with('.'))
        .collect();
    assert!(hidden.is_empty(), "{hidden:?}");
    assert_eq!(
        fs::metadata(&f.stage).unwrap().permissions().mode() & 0o777,
        0o755
    );
}

// --- #262 / hub #2801: a staging build never changes a live link's file ----

#[test]
fn stage_build_refuses_a_live_link_into_the_build_output() {
    let f = fixture();
    symlink(f.release("rsi"), f.bin_dir.join("rsi")).unwrap();
    let before = f.live_state();
    let release_before = snapshot(&[&f.target.join("release")]);
    let run = f.run_script(&["--stage-dir", &text(&f.stage)], &[]);
    assert_ne!(run.code, 0);
    assert!(
        run.err.contains("resolve into the build output"),
        "{}",
        run.err
    );
    assert!(
        run.err.contains(&text(&f.bin_dir.join("rsi"))),
        "{}",
        run.err
    );
    assert!(names(&f.cargo_builds).is_empty());
    assert_eq!(snapshot(&[&f.target.join("release")]), release_before);
    assert!(is_executable(&f.release("rsi")));
    assert_eq!(f.live_state(), before);
    assert!(!f.stage.exists());
}

#[test]
fn stage_build_runs_when_no_live_link_resolves_into_the_output() {
    let f = fixture();
    let before = f.live_state();
    f.run_ok(&["--stage-dir", &text(&f.stage)], &[]);
    assert_eq!(names(&f.cargo_builds), ["build"]);
    assert!(f.stage.join("rsid").is_file());
    assert_eq!(f.live_state(), before);
    assert!(names(&f.called).is_empty());
}

// --- #262 / hub #2799: producer (stage) matches consumer (deploy) ----------

#[test]
fn rsi_cloud_is_a_deploy_member_everywhere_the_member_list_is_copied() {
    // #3771: the daemon's managed set, the installer's list and the build all
    // name rsi-cloud, so a deploy stages, verifies and swaps it in.
    assert!(DEPLOY_BINARIES.contains(&"rsi-cloud"));
    assert!(!DEPLOY_SCRIPTS.contains(&"rsi-cloud"));
    let build = fs::read_to_string(repo_root().join("scripts/install-release.sh")).unwrap();
    assert!(build.contains("--bin rsi-cloud"));
}

#[test]
fn staged_names_are_the_top_level_files_the_deploy_reads() {
    let f = fixture();
    for name in ["cargo-slot", "target-pool", "rsi-spill", "rsi-cloud"] {
        assert!(DEPLOY_BINARIES.contains(&name), "{name}");
    }
    // deploy.rs stage_all reads each member as `dir.join(name)`, top level.
    let deploy = fs::read_to_string(repo_root().join("crates/rsid/src/deploy.rs")).unwrap();
    assert!(deploy.contains("for name in DEPLOY_BINARIES"));
    assert!(deploy.contains("let source = dir.join(name);"));
    for name in DEPLOY_BINARIES {
        if !DEPLOY_SCRIPTS.contains(&name) {
            write_executable(&f.release(name), &format!("#!/bin/sh\necho fake-{name}\n"));
        }
    }
    f.run_ok(&["--link-only", "--stage-dir", &text(&f.stage)], &[]);
    let mut expected: Vec<String> = DEPLOY_BINARIES.iter().map(|n| (*n).to_string()).collect();
    expected.push("MANIFEST.sha256".into());
    expected.sort();
    assert_eq!(names(&f.stage), expected);
    for name in DEPLOY_BINARIES {
        let staged = f.stage.join(name);
        // The consumer requires a regular file (symlink_metadata is_file).
        assert!(is_plain_file(&staged), "{name}");
        assert!(is_executable(&staged), "{name}");
    }
    // cargo-slot is the full implementation, never the ~/.rsi/bin wrapper.
    for name in ["cargo-slot", "target-pool", "rsi-spill"] {
        assert_script_copy(&f, name);
    }
    assert_eq!(
        read(&f.stage.join("rsi-cloud")),
        read(&f.release("rsi-cloud"))
    );
}

// --- #262 / hub #2802: no GNU-only readlink ---------------------------------

#[test]
fn stage_dir_works_with_a_readlink_lacking_gnu_flags() {
    let f = fixture();
    write_executable(
        &f.fakes.join("readlink"),
        "#!/bin/sh\n\
         for a in \"$@\"; do case \"$a\" in -m|-e|--canonicalize*) \
         echo \"readlink: illegal option -- $a\" >&2; exit 1;; esac; done\n\
         for real in /usr/bin/readlink /bin/readlink; do [ -x \"$real\" ] && exec \"$real\" \"$@\"; done\n\
         exit 127\n",
    );
    // Overlap detection still resolves a symlinked path to the live one.
    let alias = f.base.join("alias-of-rsi-home");
    symlink(&f.rsi_home, &alias).unwrap();
    let before = f.live_state();
    let run = f.run_script(
        &["--link-only", "--stage-dir", &text(&alias.join("bin/new"))],
        &[],
    );
    assert_ne!(run.code, 0);
    assert!(run.err.contains("overlaps the live path"), "{}", run.err);
    assert_eq!(f.live_state(), before);
    let run = f.run_ok(&["--link-only", "--stage-dir", &text(&f.stage)], &[]);
    assert!(!run.err.contains("illegal option"), "{}", run.err);
    assert!(f.stage.join("rsid").is_file());
    assert_eq!(f.live_state(), before);
}

// --- #3423: the quiet-point restart hands the daemon a staged directory -----

const QUIET_SHA: &str = "0123456789abcdef0123456789abcdef01234567";

/// A listening socket plus an `rsi-rpc` that answers like the daemon: it accepts
/// `binaries_dir` only under ~/.rsi/staging (its allowed root) and checks what
/// deploy.rs stage_all checks: MANIFEST present, rsid embeds the sha, every
/// DEPLOY_BINARIES member at the top level.
struct FakeDaemon {
    socket: PathBuf,
    rpc_log: PathBuf,
    _dir: tempfile::TempDir,
}

fn fake_daemon(f: &Fixture) -> FakeDaemon {
    // A short path: a unix socket path is limited to about 100 bytes.
    let dir = tempfile::Builder::new()
        .prefix("rqs.")
        .tempdir_in("/tmp")
        .unwrap();
    let socket = dir.path().join("d.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    std::thread::spawn(move || for _ in listener.incoming() {});
    let rpc_log = f.base.join("rpc-params.json");
    for name in DEPLOY_BINARIES {
        // rsi-rpc itself is the fake daemon front below.
        if DEPLOY_SCRIPTS.contains(&name) || name == "rsi-rpc" {
            continue;
        }
        write_executable(
            &f.release(name),
            &format!(
                "#!/bin/sh\n[ \"$1\" = --build-info ] && echo \"{QUIET_SHA} 1\"\necho \"fake-{name} $*\"\n"
            ),
        );
    }
    let need = DEPLOY_BINARIES.join(" ");
    write_executable(
        &f.release("rsi-rpc"),
        &format!(
            r#"#!/bin/sh
params=""
while [ $# -gt 0 ]; do
    [ "$1" = --params ] && params="$2"
    shift
done
printf '%s' "$params" > '{log}'
fail() {{
    printf '{{"jsonrpc":"2.0","id":1,"error":{{"message":"Invalid parameter: %s"}}}}\n' "$1"
    exit 0
}}
dir=$(printf '%s' "$params" | sed -n 's/.*"binaries_dir": *"\([^"]*\)".*/\1/p')
root=$(cd "$HOME/.rsi/staging" 2>/dev/null && pwd -P)
d=$(cd "$dir" 2>/dev/null && pwd -P)
case "$d" in
    "$root"/*) ;;
    *) fail deploy_directory_not_allowed ;;
esac
for n in MANIFEST.sha256 {need}; do
    [ -f "$d/$n" ] || fail "deploy_binary_missing $n"
done
sha=$("$d/rsid" --build-info | sed -n '1s/ .*//p')
[ "$sha" = '{QUIET_SHA}' ] || fail deploy_sha_mismatch
printf '{{"jsonrpc":"2.0","id":1,"result":{{"sha":"%s","release_by":"soon"}}}}\n' "$sha"
"#,
            log = rpc_log.display(),
        ),
    );
    FakeDaemon {
        socket,
        rpc_log,
        _dir: dir,
    }
}

fn quiet_install(f: &Fixture, daemon: &FakeDaemon) -> install_release_support::Run {
    f.run_ok(
        &[],
        &[
            ("RSI_DAEMON_SOCKET_PATH", &text(&daemon.socket)),
            ("RSI_BUILD_SHA", QUIET_SHA),
            ("NOW", "0"),
        ],
    )
}

#[test]
fn quiet_restart_stages_into_the_daemon_allowed_root() {
    // The checkout's target directory is outside every root the daemon allows;
    // the quiet restart must go through staging instead.
    let f = fixture();
    let daemon = fake_daemon(&f);
    let run = quiet_install(&f, &daemon);
    let staged = f.rsi_home.join(format!("staging/bin-{}", &QUIET_SHA[..9]));
    assert!(run.out.contains("Restart pending"), "{}", run.out);
    let sent: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&daemon.rpc_log).unwrap()).unwrap();
    let dir = fs::canonicalize(sent["binaries_dir"].as_str().unwrap()).unwrap();
    assert_eq!(dir, fs::canonicalize(&staged).unwrap());
    assert_ne!(dir, fs::canonicalize(f.target.join("release")).unwrap());
    // Same checks as an agent deploy: MANIFEST, every DEPLOY_BINARIES member at
    // the top level as a regular executable, rsid embeds the commit.
    assert!(staged.join("MANIFEST.sha256").is_file());
    for name in DEPLOY_BINARIES {
        let member = staged.join(name);
        assert!(is_plain_file(&member), "{name}");
        assert!(is_executable(&member), "{name}");
    }
    let embedded = std::process::Command::new(staged.join("rsid"))
        .arg("--build-info")
        .env_clear()
        .env("HOME", &f.home)
        .env("PATH", &f.fakes)
        .output()
        .unwrap();
    let embedded = String::from_utf8_lossy(&embedded.stdout);
    assert_eq!(embedded.split_whitespace().next(), Some(QUIET_SHA));
}

#[test]
fn quiet_restart_replaces_a_stale_staging_dir_of_the_same_commit() {
    let f = fixture();
    let daemon = fake_daemon(&f);
    let stale = f.rsi_home.join(format!("staging/bin-{}", &QUIET_SHA[..9]));
    write_executable(&stale.join("stale-leftover"), "#!/bin/sh\n");
    quiet_install(&f, &daemon);
    assert!(!stale.join("stale-leftover").exists());
    assert!(stale.join("MANIFEST.sha256").is_file());
}

#[test]
fn now_restart_does_not_stage_for_the_daemon() {
    // NOW=1 behaves as before: no quiet request, nothing under staging.
    let f = fixture();
    let daemon = fake_daemon(&f);
    let run = f.run_script(
        &["--link-only", "--now"],
        &[("RSI_DAEMON_SOCKET_PATH", &text(&daemon.socket))],
    );
    assert_eq!(run.code, 1, "{}{}", run.out, run.err);
    assert!(run.err.contains("NO DAEMON IS RUNNING"), "{}", run.err);
    let calls = fs::read_to_string(f.called.join("launchctl")).unwrap();
    assert!(
        calls.lines().any(|line| line
            == format!(
                "bootstrap gui/424242 {}/Library/LaunchAgents/dev.rsi.rsid.plist",
                f.home.display()
            )),
        "{calls}"
    );
    assert_eq!(
        fs::read_to_string(f.base.join("probes/pgrep"))
            .unwrap()
            .trim(),
        "-x -u 424242 rsid"
    );
    assert!(f.rsi_home.join("install/rsid").is_file());
    assert!(
        f.home
            .join("Library/LaunchAgents/dev.rsi.rsid.plist")
            .is_file()
    );
    assert!(
        !f.rsi_home.join("staging").exists(),
        "{}{}",
        run.out,
        run.err
    );
    assert!(!daemon.rpc_log.exists());
}

#[test]
fn missing_lifecycle_stub_refuses_before_installer_entry() {
    let f = fixture();
    // An inert marker proves refusal precedes script entry, even for --help.
    write_executable(
        &f.script("install-release.sh"),
        "#!/bin/bash\nprintf entered > \"$HOME/entered\"\n",
    );
    for name in install_release_support::isolation::STUBS {
        let path = f.fakes.join(name);
        let stub = fs::read_to_string(&path).unwrap();
        fs::remove_file(&path).unwrap();
        let run = f.run_script(&["--help"], &[]);
        assert_eq!(run.code, 97, "{name}: {}{}", run.out, run.err);
        assert!(
            run.err.contains(&format!("missing fixture {name}")),
            "{}",
            run.err
        );
        assert!(!f.home.join("entered").exists());
        write_executable(&path, &stub);
    }
    assert!(names(&f.called).is_empty());
    assert!(names(&f.base.join("probes")).is_empty());
}

#[test]
fn launchctl_stub_rejects_real_domain_and_wrong_plist() {
    let f = fixture();
    for args in [
        "launchctl bootstrap gui/501 \"$HOME/Library/LaunchAgents/dev.rsi.rsid.plist\"",
        "launchctl print gui/501/dev.rsi.rsid",
        "launchctl bootstrap gui/424242 \"$HOME/wrong.plist\"",
        "launchctl bootstrap gui/424242 \"$HOME/Library/LaunchAgents/dev.rsi.rsid.plist\"",
    ] {
        let run = f.run_bash(&["-c", args], &[], &[]);
        assert_eq!(run.code, 98, "{args}: {}{}", run.out, run.err);
        assert!(run.err.contains("outside fixture"), "{}", run.err);
    }
    assert_eq!(
        fs::read_to_string(f.called.join("launchctl"))
            .unwrap()
            .lines()
            .count(),
        4
    );
    let plist = f.home.join("Library/LaunchAgents/dev.rsi.rsid.plist");
    fs::create_dir_all(plist.parent().unwrap()).unwrap();
    fs::write(&plist, "wrong fixture program").unwrap();
    let run = f.run_bash(
        &[
            "-c",
            "launchctl bootstrap gui/424242 \"$HOME/Library/LaunchAgents/dev.rsi.rsid.plist\"",
        ],
        &[],
        &[],
    );
    assert_eq!(run.code, 98);
    assert!(run.err.contains("outside fixture plist"), "{}", run.err);
    let run = f.run_bash(&["-c", "plutil -lint \"$HOME/wrong.plist\""], &[], &[]);
    assert_eq!(run.code, 98);
    assert!(run.err.contains("outside fixture plist"), "{}", run.err);
}

#[test]
fn signal_spellings_and_process_probes_are_recording_stubs() {
    let f = fixture();
    // A deliberately invalid PID operand stays inert even if this test regresses.
    for signal in ["-0", "-INT", "-s TERM", "--signal=TERM"] {
        for spelling in ["kill", "command kill"] {
            let run = f.run_bash(
                &["-c", &format!("{spelling} {signal} fixture-pid")],
                &[],
                &[],
            );
            assert_eq!(run.code, 99, "{}{}", run.out, run.err);
        }
    }
    let run = f.run_bash(&["-c", "builtin kill -0 fixture-pid"], &[], &[]);
    assert_eq!(run.code, 1);
    assert!(run.err.contains("not a shell builtin"), "{}", run.err);
    let calls = fs::read_to_string(f.called.join("kill")).unwrap();
    assert_eq!(calls.lines().count(), 8);
    for signal in ["-0", "-INT", "-s TERM", "--signal=TERM"] {
        assert_eq!(
            calls
                .lines()
                .filter(|line| *line == format!("{signal} fixture-pid"))
                .count(),
            2
        );
    }
    for probe in ["pgrep", "ps", "lsof"] {
        let run = f.run_bash(&["-c", &format!("command {probe} fixture-probe")], &[], &[]);
        assert_eq!(run.code, 1);
        assert_eq!(
            fs::read_to_string(f.base.join("probes").join(probe)).unwrap(),
            "fixture-probe\n"
        );
    }
}

#[test]
fn fixture_pins_boundary_after_extra_environment() {
    let f = fixture();
    let run = f.run_bash(
        &[
            "-c",
            "printf '%s\\n' \"$HOME\" \"$PATH\" \"$BASH_ENV\"; command -v kill; id -u",
        ],
        &[],
        &[
            ("HOME", "/fixture-escape"),
            ("PATH", "/usr/bin:/bin"),
            ("BASH_ENV", "/dev/null"),
        ],
    );
    assert_eq!(run.code, 0, "{}", run.err);
    assert_eq!(
        run.out.lines().collect::<Vec<_>>(),
        [
            text(&f.home),
            text(&f.fakes),
            text(&f.base.join("bash-env")),
            text(&f.fakes.join("kill")),
            "424242".into(),
        ]
    );
}

#[test]
fn child_bash_also_refuses_a_missing_stub() {
    let f = fixture();
    let run = f.run_bash(
        &[
            "-c",
            "rm \"$INSTALL_TEST_FAKES/kill\"; bash -c 'printf entered > \"$HOME/entered\"'",
        ],
        &[],
        &[],
    );
    assert_eq!(run.code, 97);
    assert!(run.err.contains("missing fixture kill"), "{}", run.err);
    assert!(!f.home.join("entered").exists());
}

#[test]
fn linux_now_restart_fails_at_the_fake_scope_probe() {
    let f = fixture();
    let run = f.run_script(&["--link-only", "--now"], &[("INSTALL_TEST_OS", "Linux")]);
    assert_eq!(run.code, 1, "{}{}", run.out, run.err);
    assert!(
        run.err.contains("Cannot inspect rsi-workers.slice"),
        "{}",
        run.err
    );
    assert_eq!(
        fs::read_to_string(f.called.join("systemctl")).unwrap(),
        "--user cat rsid.service\n--user show rsi-workers.slice --property=ActiveState\n"
    );
    assert_eq!(names(&f.called), ["systemctl"]);
}
