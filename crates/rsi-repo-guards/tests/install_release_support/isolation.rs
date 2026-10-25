//! Installer-only test boundary (#632). No host PATH or inherited shell state.
//! Every Bash entry verifies all stubs, including child Bash invocations. The
//! allowlisted utility PATH has no host lifecycle fallback if a stub disappears.
use std::fs;
use std::path::Path;
use std::process::Command;

use super::write_executable;

pub const STUBS: &[&str] = &[
    "launchctl",
    "systemctl",
    "systemd-run",
    "nohup",
    "kill",
    "pkill",
    "killall",
    "pgrep",
    "ps",
    "lsof",
    "id",
    "uname",
    "plutil",
    "xmllint",
];

pub fn install(base: &Path, fakes: &Path) {
    fs::create_dir_all(base.join("called")).unwrap();
    fs::create_dir_all(base.join("probes")).unwrap();
    fs::create_dir_all(fakes).unwrap();
    // Only ordinary file/text tools used by the installer and its sourced
    // helpers. Do not add service managers, process tools, env or a host PATH.
    for name in [
        "awk",
        "basename",
        "bash",
        "cat",
        "chmod",
        "cmp",
        "cp",
        "cut",
        "date",
        "dirname",
        "find",
        "flock",
        "git",
        "grep",
        "head",
        "ln",
        "ls",
        "mkdir",
        "mktemp",
        "mv",
        "readlink",
        "rm",
        "rmdir",
        "sed",
        "sha256sum",
        "shasum",
        "sort",
        "stat",
        "tail",
        "touch",
        "tr",
        "sleep",
        "wc",
    ] {
        if let Some(real) = ["/usr/bin", "/bin"]
            .iter()
            .map(|dir| Path::new(dir).join(name))
            .find(|p| p.is_file())
        {
            // Tests replace utilities (notably readlink) in place. A symlink
            // here would let that write follow through to a host executable.
            write_executable(
                &fakes.join(name),
                &format!("#!/bin/sh\nexec {} \"$@\"\n", real.display()),
            );
        }
    }
    for name in STUBS {
        write_executable(&fakes.join(name), STUB);
    }
    write_executable(&base.join("bash-env"), BASH_ENV);
}

/// Apply last, after per-test settings. A test cannot override the boundary.
pub fn pin(command: &mut Command, base: &Path, fakes: &Path, home: &Path) {
    command
        .env("HOME", home)
        .env("PATH", fakes)
        .env("BASH_ENV", base.join("bash-env"))
        .env("INSTALL_TEST_FAKES", fakes)
        .env("INSTALL_TEST_CALLED", base.join("called"))
        .env("INSTALL_TEST_PROBES", base.join("probes"))
        .env("LC_ALL", "C")
        .env("GIT_CEILING_DIRECTORIES", base)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1");
}

const BASH_ENV: &str = r#"# Bash's kill builtin bypasses PATH, even as `command kill`.
enable -n kill
for name in launchctl systemctl systemd-run nohup kill pkill killall pgrep ps lsof id uname plutil xmllint; do
    if [ "$(command -v "$name")" != "$INSTALL_TEST_FAKES/$name" ] || [ ! -x "$INSTALL_TEST_FAKES/$name" ] || [ -L "$INSTALL_TEST_FAKES/$name" ]; then
        echo "installer isolation: missing fixture $name" >&2
        exit 97
    fi
done
"#;

const STUB: &str = r#"#!/bin/sh
name=${0##*/}
log=$INSTALL_TEST_CALLED
case "$name" in uname|id|pgrep|ps|lsof|plutil|xmllint) log=$INSTALL_TEST_PROBES ;; esac
printf '%s\n' "$*" >>"$log/$name" || exit 97
case "$name" in
    uname) [ "$#:$1" = '1:-s' ] || exit 98; printf '%s\n' "${INSTALL_TEST_OS:-Darwin}"; exit 0 ;;
    id) [ "$#:$1" = '1:-u' ] || exit 98; echo 424242; exit 0 ;;
    pgrep|ps|lsof) exit 1 ;;
    plutil|xmllint)
        case "$name:$#:$1:$2" in
            plutil:2:-lint:"$HOME/Library/LaunchAgents/.dev.rsi.rsid."*|xmllint:2:--noout:"$HOME/Library/LaunchAgents/.dev.rsi.rsid."*) ;;
            *) echo 'outside fixture plist' >&2; exit 98 ;;
        esac
        [ -f "$2" ] && [ ! -L "$2" ] || exit 98
        grep -qF '<string>dev.rsi.rsid</string>' "$2" &&
        grep -qF "$HOME/.rsi/install/rsi-socket-hold" "$2" || exit 98
        exit 0 ;;
    launchctl)
        case "$#:$1:$2" in
            3:bootstrap:gui/424242)
                [ "$3" = "$HOME/Library/LaunchAgents/dev.rsi.rsid.plist" ] &&
                [ -f "$3" ] && [ ! -L "$3" ] &&
                grep -qF '<string>dev.rsi.rsid</string>' "$3" &&
                grep -qF "$HOME/.rsi/install/rsi-socket-hold" "$3" || {
                    echo 'outside fixture plist' >&2; exit 98;
                } ;;
            2:print:gui/424242/dev.rsi.rsid) exit 113 ;;
            2:enable:gui/424242/dev.rsi.rsid) exit 0 ;;
            2:bootout:gui/424242/dev.rsi.rsid|2:kickstart:gui/424242/dev.rsi.rsid) ;;
            *) echo 'outside fixture launchd domain or arguments' >&2; exit 98 ;;
        esac ;;
esac
# Never execute a launcher, signal a PID or claim a healthy daemon.
exit 99
"#;
