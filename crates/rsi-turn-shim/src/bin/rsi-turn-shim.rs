//! Durable stdio and completion status for one provider invocation.
//!
//! Usage: rsi-turn-shim --spool-dir DIR [--stdin-file FILE] -- PROGRAM [ARG...]
//! The launcher supplies a unique invocation directory and isolates this shim
//! from the daemon's process group/scope shutdown. The provider gets its own
//! process group. No daemon pipe is inherited for stdin, stdout or stderr.
//! `alive.lock` stays locked until `exit.json` is atomically published; readers
//! must check the lock before interpreting an exit record. SIGKILL/crashes may
//! release the lock without an exit record, which means abandoned, not success.
//! Windows builds this binary but explicitly refuses execution.

use std::ffi::OsString;
use std::io;
use std::path::PathBuf;
use std::process::ExitCode;

const USAGE: &str = "Usage: rsi-turn-shim --spool-dir DIR [--stdin-file FILE] -- PROGRAM [ARG...]";

// The unsupported-platform path validates argv but does not execute it.
#[cfg_attr(not(unix), allow(dead_code))]
struct Invocation {
    spool_dir: PathBuf,
    stdin_file: Option<PathBuf>,
    argv: Vec<OsString>,
}

fn parse(args: impl IntoIterator<Item = OsString>) -> io::Result<Invocation> {
    let mut args = args.into_iter();
    let mut spool_dir = None;
    let mut stdin_file = None;
    let mut argv = Vec::new();
    while let Some(arg) = args.next() {
        if arg == "--" {
            argv.extend(args);
            break;
        }
        let slot = if arg == "--spool-dir" {
            &mut spool_dir
        } else if arg == "--stdin-file" {
            &mut stdin_file
        } else {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, USAGE));
        };
        if slot.is_some() {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, USAGE));
        }
        *slot =
            Some(PathBuf::from(args.next().ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidInput, USAGE)
            })?));
    }
    let spool_dir = spool_dir
        .filter(|dir| !dir.as_os_str().is_empty())
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, USAGE))?;
    if argv.first().is_none_or(|program| program.is_empty()) {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, USAGE));
    }
    Ok(Invocation {
        spool_dir,
        stdin_file,
        argv,
    })
}

#[cfg(unix)]
mod unix {
    use super::*;
    use nix::sys::signal::{self, SaFlags, SigAction, SigHandler, SigSet, SigmaskHow, Signal};
    use nix::unistd::Pid;
    use serde::Serialize;
    use std::fs::{self, DirBuilder, File, OpenOptions};
    use std::io::Write;
    use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
    use std::os::unix::process::{CommandExt, ExitStatusExt};
    use std::path::Path;
    use std::process::{Command, ExitStatus, Stdio};

    /// Exactly one of exit_code/signal is set for a reaped provider. Setup or
    /// exec failure instead sets error and leaves both status fields null.
    #[derive(Serialize)]
    struct ExitRecord {
        exit_code: Option<i32>,
        signal: Option<i32>,
        error: Option<String>,
    }

    fn append(path: &Path) -> io::Result<File> {
        OpenOptions::new()
            .create(true)
            .append(true)
            .mode(0o600)
            .open(path)
    }

    fn publish_exit(dir: &Path, record: &ExitRecord) -> io::Result<()> {
        let temporary = dir.join(".exit.json.tmp");
        let mut file = OpenOptions::new()
            .create_new(true)
            .write(true)
            .mode(0o600)
            .open(&temporary)?;
        serde_json::to_writer(&mut file, record)?;
        file.write_all(b"\n")?;
        file.sync_all()?;
        fs::rename(&temporary, dir.join("exit.json"))?;
        File::open(dir)?.sync_all()
    }

    fn remove_if_present(path: &Path) -> io::Result<()> {
        match fs::remove_file(path) {
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            result => result,
        }
    }

    fn spawn_and_wait(invocation: &Invocation) -> io::Result<ExitStatus> {
        // sigwait works on Linux and macOS. Blocking before spawn closes the
        // signal-before-child-PID race. SIGCHLD wakes the same wait loop, so no
        // signal thread can race reaping and forward to a recycled child PID.
        let mut signals = SigSet::empty();
        for signal in [
            Signal::SIGTERM,
            Signal::SIGINT,
            Signal::SIGQUIT,
            Signal::SIGCHLD,
        ] {
            signals.add(signal);
        }
        let mut child_mask = signals.thread_swap_mask(SigmaskHow::SIG_BLOCK)?;
        let default = SigAction::new(SigHandler::SigDfl, SaFlags::empty(), SigSet::empty());
        for signal in [
            Signal::SIGTERM,
            Signal::SIGINT,
            Signal::SIGQUIT,
            Signal::SIGCHLD,
        ] {
            // SAFETY: default dispositions install no user handler; signals
            // are blocked on this single-threaded process during setup.
            unsafe { signal::sigaction(signal, &default)? };
            child_mask.remove(signal);
        }

        // The scope runner's PID is not ours. Publish the shim identity while
        // holding alive.lock, before starting any provider effect.
        let identity_tmp = invocation.spool_dir.join(".shim.json.tmp");
        remove_if_present(&identity_tmp)?;
        let mut identity = OpenOptions::new()
            .create_new(true)
            .write(true)
            .mode(0o600)
            .open(&identity_tmp)?;
        serde_json::to_writer(
            &mut identity,
            &serde_json::json!({"pid": std::process::id()}),
        )?;
        identity.write_all(b"\n")?;
        identity.sync_all()?;
        fs::rename(identity_tmp, invocation.spool_dir.join("shim.json"))?;
        File::open(&invocation.spool_dir)?.sync_all()?;

        let stdin = match &invocation.stdin_file {
            Some(path) => Stdio::from(File::open(path)?),
            None => Stdio::null(),
        };
        let mut command = Command::new(&invocation.argv[0]);
        command
            .args(&invocation.argv[1..])
            .stdin(stdin)
            .stdout(append(&invocation.spool_dir.join("stdout"))?)
            .stderr(append(&invocation.spool_dir.join("stderr"))?)
            .process_group(0);
        // SAFETY: after fork, only pthread_sigmask is called before exec;
        // no allocation, locks or inherited Rust state is used by the closure.
        unsafe {
            command.pre_exec(move || {
                child_mask.thread_set_mask()?;
                Ok(())
            });
        }
        let mut child = command.spawn()?;
        let group = Pid::from_raw(child.id() as i32);
        loop {
            if let Some(status) = child.try_wait()? {
                return Ok(status);
            }
            match signals.wait()? {
                Signal::SIGTERM => signal::killpg(group, Signal::SIGTERM),
                Signal::SIGINT => signal::killpg(group, Signal::SIGINT),
                // Daemon force-stop requests still reap the provider and publish exit.json.
                Signal::SIGQUIT => signal::killpg(group, Signal::SIGKILL),
                _ => continue,
            }
            .or_else(|error| {
                // A child can exit between try_wait and delivery. Its PID is
                // still reserved until we reap it on the next iteration.
                if error == nix::errno::Errno::ESRCH {
                    Ok(())
                } else {
                    Err(error)
                }
            })?;
        }
    }

    pub(super) fn run(invocation: Invocation) -> io::Result<ExitCode> {
        DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&invocation.spool_dir)?;
        // Rust's Unix file lock uses flock. This FD is close-on-exec, so the
        // provider cannot keep the shim's liveness lock alive after it dies.
        let lease = append(&invocation.spool_dir.join("alive.lock"))?;
        lease.try_lock().map_err(io::Error::other)?;
        // Reuse appends the spools but never exposes a previous completion.
        remove_if_present(&invocation.spool_dir.join("exit.json"))?;
        remove_if_present(&invocation.spool_dir.join(".exit.json.tmp"))?;
        let (record, code) = match spawn_and_wait(&invocation) {
            Ok(status) => (
                ExitRecord {
                    exit_code: status.code(),
                    signal: status.signal(),
                    error: None,
                },
                status
                    .code()
                    .unwrap_or_else(|| 128 + status.signal().unwrap_or(0)),
            ),
            Err(error) => (
                ExitRecord {
                    exit_code: None,
                    signal: None,
                    error: Some(error.to_string()),
                },
                1,
            ),
        };
        publish_exit(&invocation.spool_dir, &record)?;
        // Keep the lock through rename and fsync. A missing exit record after
        // lock release denotes a crashed/failed shim, never a successful turn.
        drop(lease);
        Ok(ExitCode::from(code.clamp(0, 255) as u8))
    }
}

fn main() -> ExitCode {
    if std::env::args_os()
        .nth(1)
        .is_some_and(|arg| arg == "--help")
    {
        println!("{USAGE}");
        return ExitCode::SUCCESS;
    }
    let invocation = match parse(std::env::args_os().skip(1)) {
        Ok(invocation) => invocation,
        Err(error) => {
            eprintln!("rsi-turn-shim: {error}");
            return ExitCode::FAILURE;
        }
    };
    #[cfg(unix)]
    match unix::run(invocation) {
        Ok(code) => code,
        Err(error) => {
            eprintln!("rsi-turn-shim: {error}");
            ExitCode::FAILURE
        }
    }
    #[cfg(not(unix))]
    {
        let _ = invocation;
        eprintln!("rsi-turn-shim: detached provider turns are unsupported on this platform");
        ExitCode::FAILURE
    }
}
