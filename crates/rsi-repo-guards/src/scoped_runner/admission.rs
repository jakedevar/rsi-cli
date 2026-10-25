//! One held lease across package units, with a separate admission clock.
use super::process::{self, OwnedChild};
use crate::Ctx;
use std::collections::BTreeMap;
use std::io::{self, BufRead, Read, Write};
use std::path::Path;
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    mpsc,
};
use std::time::{Duration, Instant};

pub const PREFIX: &str = "SCOPED_SLOT_ENV ";

pub struct Admission {
    child: OwnedChild,
    pub environment: BTreeMap<String, String>,
    _messages: mpsc::Receiver<String>,
}
impl Admission {
    pub fn acquire(
        slot: &Path,
        runner: &Path,
        ctx: &Ctx,
        cap: Duration,
        cancel: &AtomicUsize,
        mut emit: impl FnMut(&str),
    ) -> io::Result<Self> {
        let mut env = process::build_env(ctx);
        // Policy inputs go only to cargo-slot, never to package units. Authority
        // remains transport-only and is absent from both environments.
        for key in [
            "RSI_DAEMON_SOCKET_PATH",
            "RSI_CARGO_SLOT_LOCAL",
            "RSI_CARGO_SLOT_PRIORITY",
            "RSI_CARGO_SLOTS",
            "RSI_CHECK_SLOTS",
            "RSI_LANDER_SLOTS",
            "RSI_CARGO_MIN_FREE_GB",
            "RSI_CARGO_MAX_LOAD",
            "RSI_CARGO_MIN_AVAIL_GB",
            "RSI_CARGO_MAX_SLICE_GB",
            "RSI_TARGET_POOL_TOOL",
            "RSI_CARGO_NO_POOL_RECLAIM",
            "RSI_JOB_BUILD_JOBS",
        ] {
            if let Some(value) = ctx.var(key) {
                env.insert(key.into(), value.into());
            }
        }
        env.insert(
            "RSI_CARGO_ADMISSION_MAX_SEC".into(),
            cap.as_secs().max(1).to_string(),
        );
        let argv = vec![
            slot.to_string_lossy().into_owned(),
            runner.to_string_lossy().into_owned(),
            "--hold-admission".into(),
        ];
        let mut child = OwnedChild::spawn(&argv, &ctx.cwd, &env, true)?;
        let (send, messages) = mpsc::sync_channel(64);
        let stdout = child
            .child
            .stdout
            .take()
            .ok_or_else(|| io::Error::other("slot stdout missing"))?;
        let stderr = child
            .child
            .stderr
            .take()
            .ok_or_else(|| io::Error::other("slot stderr missing"))?;
        for pipe in [
            Box::new(stdout) as Box<dyn Read + Send>,
            Box::new(stderr) as Box<dyn Read + Send>,
        ] {
            let send = send.clone();
            std::thread::spawn(move || {
                // Cap any single slot protocol line independently of the deadline.
                let mut reader = io::BufReader::new(pipe);
                loop {
                    let mut line = Vec::new();
                    match reader.by_ref().take(64 * 1024).read_until(b'\n', &mut line) {
                        Ok(0) | Err(_) => break,
                        Ok(_) => {
                            if send
                                .send(String::from_utf8_lossy(&line).into_owned())
                                .is_err()
                            {
                                break;
                            }
                        }
                    }
                }
            });
        }
        drop(send);
        let deadline = Instant::now() + cap;
        loop {
            if cancel.load(Ordering::Relaxed) != 0 {
                return Err(io::ErrorKind::Interrupted.into());
            }
            if Instant::now() >= deadline {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "admission-timeout: governor build slot",
                ));
            }
            if child.child.try_wait()?.is_some() {
                return Err(io::Error::other(
                    "build-slot holder exited before admission",
                ));
            }
            match messages.recv_timeout(Duration::from_millis(10)) {
                Ok(line) => {
                    if let Some(payload) = line.strip_prefix(PREFIX) {
                        let environment: BTreeMap<String, String> =
                            serde_json::from_str(payload).map_err(io::Error::other)?;
                        if environment.get("RSI_CARGO_SLOT_HELD").map(String::as_str) != Some("1") {
                            return Err(io::Error::other(
                                "build-slot holder did not attest a held admission",
                            ));
                        }
                        let environment = environment
                            .into_iter()
                            .filter(|(key, _)| process::BUILD_ENV_NAMES.contains(&key.as_str()))
                            .collect();
                        return Ok(Self {
                            child,
                            environment,
                            _messages: messages,
                        });
                    }
                    emit(&line);
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    return Err(io::Error::other("slot holder closed without admission"));
                }
                Err(mpsc::RecvTimeoutError::Timeout) => (),
            }
        }
    }
    pub fn check(&mut self) -> io::Result<()> {
        if self.child.child.try_wait()?.is_some() {
            Err(io::Error::other(
                "shared build-slot holder exited before the package run finished",
            ))
        } else {
            Ok(())
        }
    }
}
impl Drop for Admission {
    fn drop(&mut self) {
        self.child.stdin.take();
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            if self.child.child.try_wait().ok().flatten().is_some() {
                return;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        self.child.stop();
    }
}

/// This child never acquires a lease; it only attests cargo-slot's environment
/// and holds that parent's stdin open until the orchestrator closes it.
pub fn hold(ctx: &Ctx, mut input: impl Read, mut output: impl Write) -> io::Result<()> {
    writeln!(
        output,
        "{PREFIX}{}",
        serde_json::to_string(&process::build_env(ctx))?
    )?;
    output.flush()?;
    io::copy(&mut input, &mut io::sink())?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn holder_attests_only_build_knobs_and_exits_on_eof() {
        let ctx = Ctx::new(
            ".",
            &[
                ("RSI_CARGO_SLOT_HELD", "1"),
                ("CARGO_TARGET_DIR", "target"),
                ("UNRELATED_AUTHORITY", "credential"),
            ],
        );
        let mut output = Vec::new();
        hold(&ctx, &b""[..], &mut output).unwrap();
        let line = String::from_utf8(output).unwrap();
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(line.strip_prefix(PREFIX).unwrap()).unwrap(),
            serde_json::json!({"RSI_CARGO_SLOT_HELD":"1","CARGO_TARGET_DIR":"target"})
        );
    }
}
