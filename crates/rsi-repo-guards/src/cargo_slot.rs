//! Python-free transport and local probes for `scripts/cargo-slot` (#423).
//! The shell still owns admission, one deadline, FIFO locks and child lifecycle.
use crate::Ctx;
use serde_json::{Value, json};
use std::fs::{self, OpenOptions};
use std::io::{self, Read, Write};
use std::path::Path;
use std::time::Duration;
#[cfg(unix)]
use std::time::Instant;

#[cfg(unix)]
const MAX_REPLY: usize = 1024 * 1024;

fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

/// Bound the whole transaction, including connect and fragmented reads. No token
/// or caller environment is part of the governor's unauthenticated wire protocol.
#[cfg(unix)]
pub fn request(path: &Path, method: &str, params: Value, timeout: Duration) -> io::Result<Value> {
    use std::os::fd::{FromRawFd, OwnedFd};
    use std::os::unix::{ffi::OsStrExt, net::UnixStream};
    let context = |stage: &str, error: io::Error| {
        io::Error::new(error.kind(), format!("governor {stage}: {error}"))
    };
    let deadline = Instant::now() + timeout;
    let bytes = path.as_os_str().as_bytes();
    // SAFETY: zero is a valid initial representation of sockaddr_un.
    let mut address: libc::sockaddr_un = unsafe { std::mem::zeroed() };
    if bytes.contains(&0) || bytes.len() >= address.sun_path.len() {
        return Err(invalid("governor socket path is too long or contains NUL"));
    }
    address.sun_family = libc::AF_UNIX as _;
    for (dest, src) in address.sun_path.iter_mut().zip(bytes) {
        *dest = *src as _;
    }
    // SAFETY: syscall has no borrowed pointers; ownership is immediately captured.
    let raw = unsafe { libc::socket(libc::AF_UNIX, libc::SOCK_STREAM, 0) };
    if raw < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: raw is a fresh descriptor owned by this function.
    let fd = unsafe { OwnedFd::from_raw_fd(raw) };
    // SAFETY: all calls use a live descriptor and integer fcntl arguments.
    if unsafe { libc::fcntl(raw, libc::F_SETFD, libc::FD_CLOEXEC) } < 0
        || unsafe { libc::fcntl(raw, libc::F_SETFL, libc::O_NONBLOCK) } < 0
    {
        return Err(io::Error::last_os_error());
    }
    let len =
        (std::mem::offset_of!(libc::sockaddr_un, sun_path) + bytes.len() + 1) as libc::socklen_t;
    #[cfg(any(
        target_os = "macos",
        target_os = "freebsd",
        target_os = "openbsd",
        target_os = "netbsd",
        target_os = "dragonfly"
    ))]
    {
        address.sun_len = len as _;
    }
    // SAFETY: address points at len initialized bytes, bounded above by its size.
    if unsafe { libc::connect(raw, (&raw const address).cast(), len) } < 0 {
        let error = io::Error::last_os_error();
        match error.raw_os_error() {
            Some(libc::EINPROGRESS) => {
                let mut poll = libc::pollfd {
                    fd: raw,
                    events: libc::POLLOUT,
                    revents: 0,
                };
                loop {
                    let remaining = deadline.saturating_duration_since(Instant::now());
                    if remaining.is_zero() {
                        return Err(io::ErrorKind::TimedOut.into());
                    }
                    // SAFETY: poll points at one writable pollfd for the call's lifetime.
                    let rc = unsafe {
                        libc::poll(
                            &raw mut poll,
                            1,
                            remaining.as_millis().max(1).min(i32::MAX as u128) as i32,
                        )
                    };
                    if rc == 0 {
                        return Err(io::ErrorKind::TimedOut.into());
                    }
                    if rc > 0 {
                        break;
                    }
                    let error = io::Error::last_os_error();
                    if error.kind() != io::ErrorKind::Interrupted {
                        return Err(error);
                    }
                }
            }
            _ => return Err(context("connect", error)),
        }
    }
    let mut stream = UnixStream::from(fd);
    if let Some(error) = stream
        .take_error()
        .map_err(|e| context("socket status", e))?
    {
        return Err(error);
    }
    // Keep the socket nonblocking throughout. Darwin can reject SO_RCVTIMEO
    // after a short-lived peer closes, even while reply bytes remain buffered.
    // Read before polling so HUP cannot discard a complete buffered reply.
    let mut wire =
        serde_json::to_vec(&json!({"jsonrpc":"2.0", "id":1, "method":method, "params":params}))?;
    wire.push(b'\n');
    let mut written = 0;
    while written < wire.len() {
        if Instant::now() >= deadline {
            return Err(io::ErrorKind::TimedOut.into());
        }
        match stream.write(&wire[written..]) {
            Ok(0) => return Err(io::ErrorKind::WriteZero.into()),
            Ok(count) => written += count,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                wait_ready(raw, libc::POLLOUT, deadline)?
            }
            Err(error) => return Err(context("request write", error)),
        }
    }
    let mut reply = Vec::new();
    let mut block = [0; 8192];
    loop {
        if Instant::now() >= deadline {
            return Err(io::ErrorKind::TimedOut.into());
        }
        let count = match stream.read(&mut block) {
            Ok(0) => return Err(io::ErrorKind::UnexpectedEof.into()),
            Ok(count) => count,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                wait_ready(raw, libc::POLLIN, deadline)?;
                continue;
            }
            Err(error) => return Err(context("reply read", error)),
        };
        let end = block[..count].iter().position(|b| *b == b'\n');
        reply.extend_from_slice(&block[..end.unwrap_or(count)]);
        if reply.len() > MAX_REPLY {
            return Err(invalid("governor reply exceeds 1 MiB"));
        }
        if end.is_some() {
            break;
        }
    }
    let reply: Value = serde_json::from_slice(&reply)?;
    if reply.get("id") != Some(&json!(1)) || reply.get("jsonrpc") != Some(&json!("2.0")) {
        return Err(invalid("invalid governor response identity"));
    }
    Ok(reply)
}

#[cfg(unix)]
fn wait_ready(fd: i32, events: i16, deadline: Instant) -> io::Result<()> {
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(io::ErrorKind::TimedOut.into());
        }
        let mut poll = libc::pollfd {
            fd,
            events,
            revents: 0,
        };
        // SAFETY: poll borrows one writable record containing our owned fd.
        let rc = unsafe {
            libc::poll(
                &raw mut poll,
                1,
                remaining.as_millis().max(1).min(i32::MAX as u128) as i32,
            )
        };
        if rc == 0 {
            return Err(io::ErrorKind::TimedOut.into());
        }
        if rc < 0 {
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(error);
        }
        if poll.revents & libc::POLLNVAL != 0 {
            return Err(invalid("governor socket descriptor is invalid"));
        }
        // HUP also means readiness: read drains final bytes or observes EOF.
        return Ok(());
    }
}

#[cfg(not(unix))]
pub fn request(_: &Path, _: &str, _: Value, _: Duration) -> io::Result<Value> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "cargo-slot governor requires Unix sockets",
    ))
}

fn text_field<'a>(value: &'a Value, name: &str) -> &'a str {
    value[name].as_str().unwrap_or("")
}
fn flat(text: &str, cap: usize) -> String {
    text.replace(['\r', '\n'], " ").chars().take(cap).collect()
}
fn transient(error: &io::Error) -> bool {
    matches!(
        error.kind(),
        io::ErrorKind::TimedOut
            | io::ErrorKind::WouldBlock
            | io::ErrorKind::ConnectionReset
            | io::ErrorKind::BrokenPipe
            | io::ErrorKind::UnexpectedEof
    )
}

/// Only complete, structurally valid grants can admit a command. Queued ticket
/// ids survive transient transport failures; badticket explicitly clears them.
pub fn rpc_reply(reply: io::Result<Value>, release: bool) -> String {
    let reply = match reply {
        Ok(reply) => reply,
        Err(error) => {
            return format!(
                "{} {}",
                if transient(&error) {
                    "retry"
                } else {
                    "unreachable"
                },
                flat(&error.to_string(), 200)
            );
        }
    };
    if let Some(error) = reply.get("error").filter(|e| !e.is_null()) {
        let message = flat(text_field(error, "message"), 1000);
        return format!(
            "{} {message}",
            if message.contains("ticket") {
                "badticket"
            } else {
                "unreachable"
            }
        );
    }
    let result = &reply["result"];
    if !result.is_object() {
        return "unreachable invalid governor result".into();
    }
    if release {
        return "released".into();
    }
    let identifier = |name| {
        result[name]
            .as_str()
            .filter(|s| !s.is_empty() && !s.chars().any(char::is_whitespace))
    };
    match text_field(result, "status") {
        "granted" => identifier("lease_id").map_or_else(
            || "unreachable invalid grant".into(),
            |id| format!("granted {id}"),
        ),
        "queued" => match (identifier("ticket_id"), result["position"].as_u64()) {
            (Some(id), Some(position)) => format!(
                "queued {id} {position} {}",
                flat(text_field(result, "message"), 1000)
            ),
            _ => "unreachable invalid queue reply".into(),
        },
        _ => "unreachable".into(),
    }
}

#[cfg(unix)]
fn alive(pid: u32) -> bool {
    if pid == 0 || pid > i32::MAX as u32 {
        return false;
    }
    // SAFETY: signal zero only probes existence/permission; it sends no signal.
    unsafe {
        libc::kill(pid as i32, 0) == 0
            || io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH)
    }
}
#[cfg(not(unix))]
fn alive(_: u32) -> bool {
    false
}

#[cfg(target_os = "linux")]
pub fn process_start(pid: u32) -> Option<(String, String)> {
    let raw = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let fields: Vec<_> = raw.get(raw.rfind(')')? + 2..)?.split_whitespace().collect();
    Some((fields.first()?.to_string(), fields.get(19)?.to_string()))
}
#[cfg(not(target_os = "linux"))]
pub fn process_start(_: u32) -> Option<(String, String)> {
    None
}

fn ticket_live(path: &Path) -> bool {
    let Some(pid) = path
        .file_name()
        .and_then(|v| v.to_str())
        .and_then(|v| v.rsplit('.').next())
        .and_then(|v| v.parse::<u32>().ok())
    else {
        return false;
    };
    let Ok(raw) = fs::read_to_string(path) else {
        return false;
    };
    let fields: Vec<_> = raw.split_whitespace().collect();
    if fields.len() < 2 || fields[0].parse::<u32>().ok() != Some(pid) || !alive(pid) {
        return false;
    }
    if fields[1] != "0" {
        if let Some((state, start)) = process_start(pid) {
            return state != "Z" && start == fields[1];
        }
    }
    true
}

/// Called while the shell holds the queue lock. Do not remove live tickets on a
/// failed /proc read; other processes may have narrower visibility.
pub fn queue_head(directory: &Path) -> io::Result<String> {
    let mut paths = fs::read_dir(directory)?
        .map(|e| e.map(|e| e.path()))
        .collect::<io::Result<Vec<_>>>()?;
    paths.sort();
    let mut first = String::new();
    for path in paths {
        if ticket_live(&path) {
            if first.is_empty() {
                first = path
                    .file_name()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .into_owned();
            }
        } else {
            let _ = fs::remove_file(path);
        }
    }
    Ok(first)
}

#[cfg(unix)]
pub fn user_free_bytes(path: &Path) -> io::Result<u64> {
    use std::os::unix::ffi::OsStrExt;
    let path_c =
        std::ffi::CString::new(path.as_os_str().as_bytes()).map_err(|_| invalid("NUL in path"))?;
    // SAFETY: statvfs writes one valid struct through a live pointer; path is NUL terminated.
    let mut stat: libc::statvfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statvfs(path_c.as_ptr(), &raw mut stat) } != 0 {
        return Err(io::Error::last_os_error());
    }
    #[allow(clippy::unnecessary_cast)]
    let free = (stat.f_bavail as u64).saturating_mul(stat.f_frsize as u64);
    #[cfg(all(
        target_os = "linux",
        not(any(target_arch = "mips", target_arch = "mips64"))
    ))]
    {
        use std::os::fd::AsRawFd;
        // struct if_dqblk, Q_GETQUOTA/USRQUOTA; unsupported filesystems leave statvfs unchanged.
        let file = fs::File::open(path)?;
        let mut quota = [0_u64; 9];
        // SAFETY: the kernel receives a live fd and a writable 72-byte quota record.
        let rc = unsafe {
            libc::syscall(
                libc::SYS_quotactl_fd,
                file.as_raw_fd(),
                0x80000700_u32,
                libc::geteuid(),
                quota.as_mut_ptr(),
            )
        };
        if rc == 0 && quota[0] != 0 && (quota[8] as u32 & 5) == 5 {
            return Ok(free.min(quota[0].saturating_mul(1024).saturating_sub(quota[2])));
        }
    }
    Ok(free)
}
#[cfg(not(unix))]
pub fn user_free_bytes(_: &Path) -> io::Result<u64> {
    Err(io::ErrorKind::Unsupported.into())
}

fn local_state(ctx: &Ctx) -> Vec<String> {
    let root = ctx.home().join(".rsi");
    let mut lines = Vec::new();
    let mut paths: Vec<_> = fs::read_dir(&root)
        .into_iter()
        .flatten()
        .filter_map(Result::ok)
        .map(|e| e.path())
        .collect();
    paths.sort();
    for path in paths {
        let name = path.file_name().unwrap_or_default().to_string_lossy();
        if !name.ends_with(".lock")
            || !["build-slot-", "check-slot-", "cargo-slot"]
                .iter()
                .any(|prefix| name.starts_with(prefix))
        {
            continue;
        }
        if let Ok(file) = OpenOptions::new().create(true).append(true).open(&path) {
            if matches!(file.try_lock(), Err(std::fs::TryLockError::WouldBlock)) {
                let marker = fs::read_to_string(path.with_file_name(format!("{name}.holder")))
                    .unwrap_or_else(|_| "pid unknown".into());
                lines.push(format!("holder {name}: {}", marker.trim()));
            }
        }
    }
    for pool in ["lander", "build", "check"] {
        let mut tickets: Vec<_> =
            fs::read_dir(root.join("cargo-slot-queue").join(pool).join("tickets"))
                .into_iter()
                .flatten()
                .filter_map(Result::ok)
                .map(|e| e.path())
                .collect();
        tickets.sort();
        for (position, path) in tickets.iter().filter(|p| ticket_live(p)).enumerate() {
            let name = path.file_name().unwrap_or_default().to_string_lossy();
            lines.push(format!(
                "queue {pool} position {position} pid {}",
                name.rsplit('.').next().unwrap_or("")
            ));
        }
    }
    lines
}

fn socket_path(ctx: &Ctx) -> std::path::PathBuf {
    ctx.var("RSI_DAEMON_SOCKET_PATH")
        .filter(|s| !s.is_empty())
        .map_or_else(|| ctx.home().join(".rsi/daemon.sock"), Into::into)
}

fn status(ctx: &Ctx, out: &mut dyn Write) -> io::Result<()> {
    let local = local_state(ctx);
    if ctx
        .var("RSI_CARGO_SLOT_LOCAL")
        .is_some_and(|v| !v.is_empty())
    {
        writeln!(
            out,
            "local fallback (RSI_CARGO_SLOT_LOCAL=1: daemon skipped)"
        )?;
    } else {
        match request(
            &socket_path(ctx),
            "GetResourceGovernor",
            json!({}),
            Duration::from_secs(3),
        ) {
            Ok(reply) if reply["result"].is_object() => {
                let mut leases: Vec<_> = reply["result"]["leases"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .collect();
                leases.sort_by_key(|v| text_field(v, "granted_at"));
                for lease in leases {
                    writeln!(
                        out,
                        "holder {} pid {}: {}",
                        text_field(lease, "class"),
                        lease["pid"],
                        text_field(lease, "label")
                    )?;
                }
                for item in reply["result"]["queue"].as_array().into_iter().flatten() {
                    writeln!(
                        out,
                        "queue {} position {} pid {}: {}",
                        text_field(item, "class"),
                        item["position"],
                        item["pid"],
                        text_field(item, "label")
                    )?;
                }
                if !local.is_empty() {
                    writeln!(
                        out,
                        "local fallback (live local holders and waiters beside the daemon)"
                    )?;
                }
            }
            other => writeln!(
                out,
                "local fallback (governor status unavailable: {})",
                other
                    .err()
                    .map_or_else(|| "governor status unavailable".into(), |e| e.to_string())
            )?,
        }
    }
    for line in local {
        writeln!(out, "{line}")?;
    }
    Ok(())
}

pub fn main(args: &[String], ctx: &Ctx, out: &mut dyn Write, err: &mut dyn Write) -> i32 {
    if args.first().is_some_and(|arg| arg == "supervise") {
        return match crate::scoped_runner::process::supervise(&args[1..]) {
            Ok(code) => code,
            Err(error) => {
                let _ = writeln!(err, "cargo-slot: {error}");
                2
            }
        };
    }
    let result = (|| -> io::Result<()> {
        match args.first().map(String::as_str) {
            Some("verify-build") if args.len() == 2 => crate::source_stamp::verify(
                &ctx.path(&args[1]),
                env!("RSI_REPO_RUNNER_SOURCE_STAMP"),
            ),
            Some("status") if args.len() == 1 => status(ctx, out),
            Some("free-gb") if args.len() == 2 => {
                writeln!(out, "{}", user_free_bytes(&ctx.path(&args[1]))? >> 30)
            }
            Some("start") if args.len() == 2 => {
                let pid = args[1].parse().map_err(|_| invalid("invalid pid"))?;
                writeln!(
                    out,
                    "{}",
                    process_start(pid).map_or_else(|| "0".into(), |(_, start)| start)
                )
            }
            Some("queue-head") if args.len() == 2 => {
                writeln!(out, "{}", queue_head(&ctx.path(&args[1]))?)
            }
            Some("rpc") => {
                let (method, params) = match args.get(1).map(String::as_str) {
                    Some("acquire") if (5..=6).contains(&args.len()) => {
                        if !["build", "check", "lander"].contains(&args[2].as_str()) {
                            return Err(invalid("invalid admission class"));
                        }
                        let pid: u32 = args[3]
                            .parse()
                            .map_err(|_| invalid("invalid admission pid"))?;
                        if pid == 0 || pid > i32::MAX as u32 {
                            return Err(invalid("invalid admission pid"));
                        }
                        let mut params = json!({"class":args[2],"pid":pid,"label":args[4].chars().take(200).collect::<String>()});
                        if let Some(ticket) = args.get(5).filter(|s| !s.is_empty()) {
                            params["ticket_id"] = json!(ticket);
                        }
                        ("AcquireAdmission", params)
                    }
                    Some("release") if args.len() == 3 => {
                        ("ReleaseAdmission", json!({"lease_id":args[2]}))
                    }
                    _ => {
                        return Err(invalid(
                            "usage: cargo-slot rpc acquire CLASS PID LABEL [TICKET] | release LEASE",
                        ));
                    }
                };
                writeln!(
                    out,
                    "{}",
                    rpc_reply(
                        request(&socket_path(ctx), method, params, Duration::from_secs(5)),
                        method == "ReleaseAdmission"
                    )
                )
            }
            _ => Err(invalid(
                "usage: cargo-slot status | free-gb PATH | start PID | queue-head PATH | rpc ...",
            )),
        }
    })();
    match result {
        Ok(()) => 0,
        Err(error) => {
            let _ = writeln!(err, "cargo-slot: {error}");
            2
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::run;

    #[test]
    fn grants_queues_errors_and_malformed_results() {
        assert_eq!(
            rpc_reply(
                Ok(json!({"result":{"status":"granted","lease_id":"lease"}})),
                false
            ),
            "granted lease"
        );
        assert_eq!(
            rpc_reply(
                Ok(
                    json!({"result":{"status":"queued","ticket_id":"ticket","position":2,"message":"memory\nwait"}})
                ),
                false
            ),
            "queued ticket 2 memory wait"
        );
        assert_eq!(
            rpc_reply(Ok(json!({"error":{"message":"stale ticket"}})), false),
            "badticket stale ticket"
        );
        for result in [
            json!({"status":"granted"}),
            json!({"status":"granted","lease_id":"bad\nid"}),
            json!({"status":"queued","ticket_id":"t","position":-1}),
            Value::Null,
        ] {
            assert!(rpc_reply(Ok(json!({"result":result})), false).starts_with("unreachable"));
        }
        assert_eq!(
            rpc_reply(Ok(json!({"result":{"released":true}})), true),
            "released"
        );
    }

    #[test]
    fn transient_failures_preserve_governor_policy() {
        for kind in [
            io::ErrorKind::TimedOut,
            io::ErrorKind::WouldBlock,
            io::ErrorKind::ConnectionReset,
            io::ErrorKind::BrokenPipe,
            io::ErrorKind::UnexpectedEof,
        ] {
            assert!(rpc_reply(Err(kind.into()), false).starts_with("retry "));
        }
        assert!(rpc_reply(Err(io::ErrorKind::NotFound.into()), false).starts_with("unreachable "));
    }

    #[cfg(unix)]
    fn serve(
        wire: Vec<u8>,
        delay: Duration,
    ) -> (
        tempfile::TempDir,
        std::thread::JoinHandle<Value>,
        std::path::PathBuf,
    ) {
        use std::os::unix::net::UnixListener;
        // A socket path must fit sun_path, even under a long job TMPDIR.
        let dir = tempfile::Builder::new()
            .prefix("rsi-slot-")
            .tempdir_in("/tmp")
            .unwrap();
        let path = dir.path().join("s");
        let listener = UnixListener::bind(&path).unwrap();
        let thread = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(2)))
                .unwrap();
            let mut request = Vec::new();
            let mut byte = [0];
            while stream.read(&mut byte).unwrap() == 1 {
                if byte[0] == b'\n' {
                    break;
                }
                request.push(byte[0]);
            }
            std::thread::sleep(delay);
            for chunk in wire.chunks(3) {
                if stream.write_all(chunk).is_err() {
                    break;
                }
            }
            serde_json::from_slice(&request).unwrap()
        });
        (dir, thread, path)
    }

    #[cfg(unix)]
    #[test]
    fn socket_transaction_handles_fragments_without_transport_credentials() {
        let (_dir,thread,path) = serve(b"{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"status\":\"granted\",\"lease_id\":\"l\"}}\n".to_vec(),Duration::ZERO);
        let reply = request(
            &path,
            "AcquireAdmission",
            json!({"class":"check","pid":12,"ticket_id":"same"}),
            Duration::from_secs(1),
        )
        .unwrap();
        assert_eq!(rpc_reply(Ok(reply), false), "granted l");
        assert_eq!(
            thread.join().unwrap(),
            json!({"jsonrpc":"2.0","id":1,"method":"AcquireAdmission","params":{"class":"check","pid":12,"ticket_id":"same"}})
        );
    }

    #[cfg(unix)]
    #[test]
    fn socket_deadline_eof_invalid_identity_and_size_are_fail_closed() {
        for wire in [
            b"{}\n".to_vec(),
            b"{\"jsonrpc\":\"2.0\",\"id\":2}\n".to_vec(),
            b"partial".to_vec(),
            vec![b'x'; MAX_REPLY + 1],
        ] {
            let (_dir, thread, path) = serve(wire, Duration::ZERO);
            assert!(request(&path, "AcquireAdmission", json!({}), Duration::from_secs(2)).is_err());
            thread.join().unwrap();
        }
        let (_dir, thread, path) = serve(b"{}\n".to_vec(), Duration::from_millis(150));
        let start = Instant::now();
        let reply = request(
            &path,
            "AcquireAdmission",
            json!({}),
            Duration::from_millis(30),
        );
        assert!(rpc_reply(reply, false).starts_with("retry "));
        assert!(start.elapsed() < Duration::from_millis(140));
        thread.join().unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn local_status_and_fifo_keep_live_tickets_and_drop_invalid_ones() {
        let dir = tempfile::tempdir().unwrap();
        let tickets = dir.path().join(".rsi/cargo-slot-queue/build/tickets");
        fs::create_dir_all(&tickets).unwrap();
        let pid = std::process::id();
        let live = tickets.join(format!("9{:020}.{pid}", 1));
        fs::write(&live, format!("{pid} 0\n")).unwrap();
        fs::write(tickets.join("000.0"), "0 0").unwrap();
        fs::write(tickets.join("bad"), "invalid").unwrap();
        fs::write(tickets.join("001.1"), format!("{pid} 0")).unwrap();
        assert_eq!(
            queue_head(&tickets).unwrap(),
            live.file_name().unwrap().to_str().unwrap()
        );
        assert_eq!(fs::read_dir(&tickets).unwrap().count(), 1);
        let lock_path = dir.path().join(".rsi/build-slot-0.lock");
        let lock = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&lock_path)
            .unwrap();
        lock.lock().unwrap();
        fs::write(
            lock_path.with_file_name("build-slot-0.lock.holder"),
            "pid 7: held\n",
        )
        .unwrap();
        let home = dir.path().to_str().unwrap();
        let ctx = Ctx::new(dir.path(), &[("HOME", home), ("RSI_CARGO_SLOT_LOCAL", "1")]);
        let (code, out, err) = run(main, &["status"], &ctx);
        assert_eq!((code, err), (0, String::new()));
        assert_eq!(
            out,
            format!(
                "local fallback (RSI_CARGO_SLOT_LOCAL=1: daemon skipped)\nholder build-slot-0.lock: pid 7: held\nqueue build position 0 pid {pid}\n"
            )
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn pid_reuse_is_pruned_from_queue() {
        let dir = tempfile::tempdir().unwrap();
        let pid = std::process::id();
        let path = dir.path().join(format!("000.{pid}"));
        fs::write(path, format!("{pid} impossible-start\n")).unwrap();
        assert_eq!(queue_head(dir.path()).unwrap(), "");
    }

    #[cfg(unix)]
    #[test]
    fn free_space_and_start_have_machine_readable_outputs() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = Ctx::new(dir.path(), &[]);
        let (code, out, _) = run(main, &["free-gb", dir.path().to_str().unwrap()], &ctx);
        assert_eq!(code, 0);
        assert!(out.trim().parse::<u64>().is_ok());
        let pid = std::process::id().to_string();
        let (code, out, _) = run(main, &["start", &pid], &ctx);
        assert_eq!(code, 0);
        assert!(out.trim().parse::<u64>().is_ok());
    }
}
