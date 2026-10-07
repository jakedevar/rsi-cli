use super::registry::{Entry, Registry};
use super::*;
use std::process::{Command, Stdio};

const OLD: Duration = Duration::ZERO;

/// Whether `dir` is on a filesystem the fixtures can rely on: it keeps a birth
/// time (the allocation record binds a scratch directory to it) and exchanges
/// two directories atomically (`renameat2(RENAME_EXCHANGE)`, which the inventory
/// tests use). An old kernel's tmpfs, or a 9p/overlay mount, may lack either.
fn fixture_filesystem(dir: &Path) -> bool {
    let btime = record::pin_dir(dir).is_ok_and(|pinned| pinned.btime.is_some());
    let (a, b) = (dir.join("probe-a"), dir.join("probe-b"));
    let exchange = fs::create_dir(&a).is_ok()
        && fs::create_dir(&b).is_ok()
        && nix::fcntl::renameat2(None, &a, None, &b, nix::fcntl::RenameFlags::RENAME_EXCHANGE)
            .is_ok();
    let _ = fs::remove_dir(&a);
    let _ = fs::remove_dir(&b);
    btime && exchange
}

/// A temporary directory on such a filesystem: the system temporary directory
/// when it is one, else a directory on the filesystem of the build tree, so the
/// fixtures never depend on what `/tmp` is on the host (#1165).
fn tempdir_keeping_btime() -> tempfile::TempDir {
    let system = tempfile::tempdir().unwrap();
    if fixture_filesystem(system.path()) {
        return system;
    }
    let fallback = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/scratch-test-tmp");
    fs::create_dir_all(&fallback).unwrap();
    let dir = tempfile::tempdir_in(&fallback).unwrap();
    assert!(
        fixture_filesystem(dir.path()),
        "no filesystem here keeps a birth time and exchanges directories"
    );
    dir
}

/// A root and a private registry, both temporary.
struct Fx {
    root: tempfile::TempDir,
    registry: tempfile::TempDir,
    /// The private process view of the test (see [`Fx::mirror_proc`]).
    proc: tempfile::TempDir,
    /// Children the test spawned that the process view must show.
    watched: std::sync::Mutex<Vec<u32>>,
    /// The test's own mount table (see [`Fx::mountinfo`]).
    mounts: tempfile::TempDir,
}

impl Fx {
    fn new() -> Self {
        let fx = Self {
            root: tempdir_keeping_btime(),
            registry: tempfile::tempdir().unwrap(),
            proc: tempfile::tempdir().unwrap(),
            watched: std::sync::Mutex::new(Vec::new()),
            mounts: tempfile::tempdir().unwrap(),
        };
        // The registry is private by definition; a tempdir follows the umask.
        set_mode(fx.registry(), 0o700);
        fx
    }

    fn root(&self) -> &Path {
        self.root.path()
    }

    fn registry(&self) -> &Path {
        self.registry.path()
    }

    /// A private proc root holding exactly the processes this test owns: the
    /// test process itself (the lander-liveness owner of the fixtures) and the
    /// children it registered with [`Fx::watch`] (symlinks to their live
    /// `/proc/<pid>`). Nothing else the host runs is in it: a sandbox runtime,
    /// an init-like supervisor, another uid's process or a concurrent test's
    /// non-dumpable child can no longer change a classification (#1165). A
    /// process the kernel refuses to show is the reclaimer's business only in
    /// the tests that build such a process themselves ([`fake_proc`],
    /// [`hidden_process`]).
    fn mirror_proc(&self) -> PathBuf {
        let mirror = self.hermetic_proc();
        // The test process is listed for its liveness only (`stat`, `status`,
        // `comm`): it has no `task` directory, so it is never scanned. Its
        // threads are other tests' threads, which start and exit while a pass
        // reads them (a thread in exit answers `ESRCH`, an unexplained read
        // error), and the holder proof is about the fixture's own processes.
        let own = mirror.join(std::process::id().to_string());
        fs::create_dir(&own).unwrap();
        for file in ["stat", "status", "comm"] {
            let live = Path::new("/proc").join(std::process::id().to_string());
            let _ = std::os::unix::fs::symlink(live.join(file), own.join(file));
        }
        for pid in self.watched.lock().unwrap().clone() {
            let _ = std::os::unix::fs::symlink(
                Path::new("/proc").join(pid.to_string()),
                mirror.join(pid.to_string()),
            );
        }
        mirror
    }

    /// A private proc root with no processes in it (only the entries the
    /// lander-liveness check reads), so a test cannot be thrown off by what
    /// else runs on the host.
    fn hermetic_proc(&self) -> PathBuf {
        let root = self.proc.path().to_path_buf();
        for entry in fs::read_dir(&root).unwrap().flatten() {
            let _ = fs::remove_file(entry.path()).or_else(|_| fs::remove_dir_all(entry.path()));
        }
        // What the lander-liveness check reads next to the process list.
        for shared in ["self", "sys"] {
            std::os::unix::fs::symlink(Path::new("/proc").join(shared), root.join(shared)).unwrap();
        }
        root
    }

    /// The mount table a pass reads: the test's own, one root filesystem and
    /// nothing mounted at or under any fixture. The host's
    /// `/proc/self/mountinfo` (overlay layers, bind mounts, a runtime's mounts,
    /// a line the parser does not understand) would otherwise decide whether a
    /// candidate is "unmounted" (#1165).
    fn mountinfo(&self) -> PathBuf {
        mountinfo_with(self.mounts.path(), &[])
    }

    /// Show a real child of this test in the process view of later passes.
    fn watch(&self, child: &std::process::Child) {
        self.watched.lock().unwrap().push(child.id());
    }

    fn config(&self, kind: RootKind, min_age: Duration) -> ScratchConfig {
        let mut config = ScratchConfig::with_roots(vec![(self.root().to_path_buf(), kind)]);
        config.proc_root = self.mirror_proc();
        config.mountinfo = self.mountinfo();
        config.min_age = min_age;
        config.unregistered_lander_age = min_age;
        config.max_duration = Duration::from_secs(60);
        config.registry = self.registry().to_path_buf();
        config
    }

    /// A scratch directory RSI allocated (registry entry plus record) and then
    /// filled with some data.
    fn scratch(&self, name: &str, kind: RootKind) -> PathBuf {
        let dir = create_scratch_dir_in(self.registry(), self.root(), name, kind).unwrap();
        fs::write(dir.join("data.bin"), vec![7u8; 8192]).unwrap();
        dir
    }

    /// A lander workspace registered the way the lander does (owner = this
    /// test process, which is alive).
    fn lander(&self, name: &str) -> PathBuf {
        let dir = self.root().join(name);
        fs::create_dir(&dir).unwrap();
        register_lander_owner_in(self.registry(), &dir, Path::new("/sandbox")).unwrap();
        fs::write(dir.join("data.bin"), vec![7u8; 8192]).unwrap();
        dir
    }

    fn entry_of(&self, dir: &Path) -> Entry {
        let pinned = record::pin_dir(dir).unwrap();
        let rec = record::read_record(&pinned).unwrap();
        Registry::open(self.registry(), current_uid())
            .unwrap()
            .get(&rec.nonce)
            .unwrap()
    }

    fn rewrite_entry(&self, dir: &Path, change: impl FnOnce(&mut Entry)) {
        let mut entry = self.entry_of(dir);
        change(&mut entry);
        Registry::open(self.registry(), current_uid())
            .unwrap()
            .replace(&entry)
            .unwrap();
    }
}

/// A pass instant past every mtime written by the test.
fn later() -> SystemTime {
    SystemTime::now() + Duration::from_secs(2)
}

/// A scratch-looking directory with NO allocation (legacy / hand-made).
fn legacy_dir(root: &Path, name: &str) -> PathBuf {
    let dir = root.join(name);
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join("data.bin"), vec![7u8; 8192]).unwrap();
    dir
}

/// `git` without the host's configuration: a signing default or a global hook
/// of the machine running the tests cannot change what a fixture commits.
fn isolated_git() -> Command {
    let mut command = Command::new("git");
    command
        .args(["-c", "commit.gpgsign=false", "-c", "tag.gpgsign=false"])
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1");
    command
}

fn git(dir: &Path, args: &[&str]) {
    let status = isolated_git()
        .arg("-C")
        .arg(dir)
        .args(["-c", "user.name=t", "-c", "user.email=t@example.invalid"])
        .args(args)
        .env("GIT_CEILING_DIRECTORIES", dir.parent().unwrap())
        .status()
        .unwrap();
    assert!(status.success(), "git {args:?}");
}

/// `repo` is a repository with one commit pushed to a bare `origin` kept in
/// `origin_parent` (outside the scratch tree), so its history is published.
fn published_repo(repo: &Path, origin_parent: &Path) {
    fs::create_dir_all(repo).unwrap();
    git(repo, &["init", "-q", "-b", "main"]);
    fs::write(repo.join("a.txt"), "a\n").unwrap();
    git(repo, &["add", "a.txt"]);
    git(repo, &["commit", "-q", "-m", "base"]);
    // One origin per repository: two repositories named alike (`repo`) share a
    // parent, and a second push of a commit made in a later second is rejected
    // as a non-fast-forward of the first (#1165).
    static ORIGINS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let origin = origin_parent.join(format!(
        "origin-{}-{}.git",
        ORIGINS.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
        repo.file_name().unwrap().to_string_lossy()
    ));
    let init = isolated_git()
        .args(["init", "-q", "--bare"])
        .arg(&origin)
        .status()
        .unwrap();
    assert!(init.success());
    git(
        repo,
        &["remote", "add", "origin", &origin.to_string_lossy()],
    );
    git(repo, &["push", "-q", "origin", "main"]);
}

fn write_owner(dir: &Path, pid: u32, ticks: u64) {
    let host = HostIdentity::read(Path::new("/proc")).unwrap();
    fs::write(
        dir.join(LANDER_OWNER_FILE),
        holders::owner_text(pid, ticks, &host, Path::new("/sandbox")),
    )
    .unwrap();
}

/// The pid of a shell that has already exited.
fn dead_pid() -> u32 {
    let out = Command::new("sh").args(["-c", "echo $$"]).output().unwrap();
    String::from_utf8(out.stdout)
        .unwrap()
        .trim()
        .parse()
        .unwrap()
}

fn run_pass(cfg: &ScratchConfig) -> ScratchReport {
    run(cfg, later(), false)
}

/// Run `f` on a thread whose effective capabilities no longer bypass file
/// permissions (`CAP_DAC_OVERRIDE`, `CAP_DAC_READ_SEARCH`, `CAP_FOWNER`) or
/// the ptrace check on a non-dumpable process (`CAP_SYS_PTRACE`). A fixture
/// that is "unreadable" (mode 0, a non-dumpable process) is then unreadable
/// for the owner whoever runs the tests: a root runner (a container, a CI
/// job, `unshare -r`) reads a mode-0 directory otherwise, so the pass saw
/// nothing to refuse (#1165). Capabilities are per thread: nothing else in the
/// test process is affected, and a runner without them is unchanged.
fn unprivileged<T: Send>(f: impl FnOnce() -> T + Send) -> T {
    #[repr(C)]
    struct Header {
        version: u32,
        pid: i32,
    }
    #[repr(C)]
    #[derive(Clone, Copy, Default)]
    struct Data {
        effective: u32,
        permitted: u32,
        inheritable: u32,
    }
    const LINUX_CAPABILITY_VERSION_3: u32 = 0x2008_0522;
    // CAP_DAC_OVERRIDE, CAP_DAC_READ_SEARCH, CAP_FOWNER, CAP_SYS_PTRACE.
    const DROPPED: u32 = (1 << 1) | (1 << 2) | (1 << 3) | (1 << 19);
    std::thread::scope(|scope| {
        scope
            .spawn(|| {
                let mut header = Header {
                    version: LINUX_CAPABILITY_VERSION_3,
                    pid: 0,
                };
                let mut data = [Data::default(); 2];
                // SAFETY: `header` and the two-element `data` array are the
                // layout the v3 capget/capset syscalls read and write; pid 0
                // is the calling thread.
                unsafe {
                    let got =
                        nix::libc::syscall(nix::libc::SYS_capget, &mut header, data.as_mut_ptr());
                    assert_eq!(got, 0, "capget: {}", io::Error::last_os_error());
                    data[0].effective &= !DROPPED;
                    let set = nix::libc::syscall(nix::libc::SYS_capset, &mut header, data.as_ptr());
                    assert_eq!(set, 0, "capset: {}", io::Error::last_os_error());
                }
                f()
            })
            .join()
            .unwrap()
    })
}

fn only_entries(root: &Path) -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(root)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

fn set_mode(path: &Path, mode: u32) {
    fs::set_permissions(path, std::os::unix::fs::PermissionsExt::from_mode(mode)).unwrap();
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[test]
fn stale_recorded_scratch_is_reclaimed() {
    let fx = Fx::new();
    let stale = fx.scratch("rsi-s2a-old", RootKind::VarTmp);

    let report = run_pass(&fx.config(RootKind::VarTmp, OLD));

    assert_eq!(report.reclaimed, 1, "{report:?}");
    assert!(report.reclaimed_bytes >= 8192);
    assert!(!stale.exists());
    assert!(only_entries(fx.root()).is_empty());
    // The allocation is consumed with the directory.
    assert!(only_entries(fx.registry()).is_empty());
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[test]
fn dry_run_reports_without_effects() {
    let fx = Fx::new();
    let stale = fx.scratch("rsi-s2a-old", RootKind::VarTmp);

    let report = run(&fx.config(RootKind::VarTmp, OLD), later(), true);

    assert!(report.dry_run);
    assert_eq!(report.reclaimed, 1);
    assert!(stale.join("data.bin").exists());
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[test]
fn fresh_directory_is_kept() {
    let fx = Fx::new();
    let fresh = fx.scratch("rsi-w999-fresh", RootKind::VarTmp);
    let cfg = fx.config(RootKind::VarTmp, Duration::from_secs(72 * 3600));

    let report = run(&cfg, SystemTime::now(), false);

    assert_eq!(report.kept_young, 1);
    assert_eq!(report.reclaimed, 0);
    assert!(fresh.join("data.bin").exists());
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[test]
fn non_rsi_directory_is_kept() {
    let fx = Fx::new();
    let other = fx.scratch("photos-backup", RootKind::WorkerCache);
    let cache_only = fx.scratch("rsi-keep", RootKind::WorkerCache);

    let report = run_pass(&fx.config(RootKind::WorkerCache, OLD));

    assert_eq!(report.considered, 0, "{report:?}");
    assert!(other.join("data.bin").exists());
    // The worker-cache root only allows `rsi-*-tmp`.
    assert!(cache_only.join("data.bin").exists());
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[test]
fn recorded_worker_tmpdir_in_cache_root_is_reclaimed() {
    let fx = Fx::new();
    let tmp = fx.scratch("rsi-w999-tmp", RootKind::WorkerCache);

    let report = run_pass(&fx.config(RootKind::WorkerCache, OLD));

    assert_eq!(report.reclaimed, 1, "{report:?}");
    assert!(!tmp.exists());
}

// R1: adoption is not possible; only a fresh, empty directory is allocated.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[test]
fn allocation_refuses_to_adopt_an_existing_or_populated_directory() {
    let fx = Fx::new();
    let populated = fx.root().join("rsi-populated");
    fs::create_dir(&populated).unwrap();
    fs::write(populated.join("work.txt"), b"mine").unwrap();
    assert!(record::allocate(fx.registry(), &populated, RootKind::VarTmp, None).is_err());

    let existing = fx.root().join("rsi-exists");
    fs::create_dir(&existing).unwrap();
    assert!(
        create_scratch_dir_in(fx.registry(), fx.root(), "rsi-exists", RootKind::VarTmp).is_err()
    );
    // Neither is recorded, so the pass retains both.
    let report = run_pass(&fx.config(RootKind::VarTmp, OLD));
    assert_eq!(report.kept_unrecorded, 2, "{report:?}");
    assert!(populated.join("work.txt").exists());
}

// R1: a record without an allocation entry, or one that does not bind this
// directory, never authorizes deletion.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[test]
fn forged_copied_replayed_and_moved_records_are_not_provenance() {
    let fx = Fx::new();
    let genuine = fx.scratch("rsi-genuine", RootKind::VarTmp);
    let record_text = fs::read_to_string(genuine.join(RECORD_FILE)).unwrap();

    // Synthesized: a plausible record, but no registry entry exists for it.
    let forged = legacy_dir(fx.root(), "rsi-forged");
    fs::write(
        forged.join(RECORD_FILE),
        record::record_text(&registry::new_nonce(), RootKind::VarTmp.tag()),
    )
    .unwrap();
    // Copied record: names a real entry, but for another directory.
    let copied = legacy_dir(fx.root(), "rsi-copied");
    fs::write(copied.join(RECORD_FILE), &record_text).unwrap();
    // A genuine allocation whose registry entry no longer matches: another
    // filesystem's inode, a reused inode (different birth time), another owner.
    let other_dev = fx.scratch("rsi-other-dev", RootKind::VarTmp);
    fx.rewrite_entry(&other_dev, |e| e.ident.dev += 1);
    let replayed = fx.scratch("rsi-replayed", RootKind::VarTmp);
    fx.rewrite_entry(&replayed, |e| e.btime += 1);
    let other_uid = fx.scratch("rsi-other-uid", RootKind::VarTmp);
    fx.rewrite_entry(&other_uid, |e| e.uid += 1);
    // A record with a wrong kind.
    let wrong_kind = fx.scratch("rsi-wrong", RootKind::LanderScratch);

    let report = run_pass(&fx.config(RootKind::VarTmp, OLD));

    assert_eq!(report.reclaimed, 1, "{report:?}");
    assert_eq!(report.kept_unrecorded, 6, "{report:?}");
    assert!(!genuine.exists());
    for dir in [forged, copied, other_dev, replayed, other_uid, wrong_kind] {
        assert!(dir.join("data.bin").exists(), "{dir:?}");
    }
}

// R1: a recorded tree moved (same filesystem, inode kept) into another root is
// not admitted there.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[test]
fn recorded_tree_moved_into_another_root_is_not_admitted() {
    let fx = Fx::new();
    let elsewhere = tempfile::tempdir().unwrap();
    let moved = fx.scratch("rsi-moved", RootKind::VarTmp);
    let target = elsewhere.path().join("rsi-moved");
    fs::rename(&moved, &target).unwrap();
    let mut cfg = fx.config(RootKind::VarTmp, OLD);
    cfg.roots = vec![(elsewhere.path().to_path_buf(), RootKind::VarTmp)];

    let report = run_pass(&cfg);

    assert_eq!(report.kept_unrecorded, 1, "{report:?}");
    assert!(target.join("data.bin").exists());
}

// R1: with no private registry there is no provenance.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[test]
fn missing_or_open_registry_retains_everything() {
    let fx = Fx::new();
    let dir = fx.scratch("rsi-needs-registry", RootKind::VarTmp);
    let mut cfg = fx.config(RootKind::VarTmp, OLD);

    set_mode(fx.registry(), 0o755);
    let open = run_pass(&cfg);
    assert_eq!(open.kept_unrecorded, 1, "{open:?}");

    set_mode(fx.registry(), 0o700);
    cfg.registry = fx.registry().join("no-such-registry");
    let missing = run_pass(&cfg);
    assert_eq!(missing.kept_unrecorded, 1, "{missing:?}");
    assert!(dir.join("data.bin").exists());
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[test]
fn directory_held_by_a_process_cwd_is_kept() {
    let fx = Fx::new();
    let held = fx.scratch("rsi-s2a-held", RootKind::VarTmp);
    let mut holder = Command::new("sleep")
        .arg("30")
        .current_dir(&held)
        .spawn()
        .unwrap();
    fx.watch(&holder);

    let report = run_pass(&fx.config(RootKind::VarTmp, OLD));
    let survived = held.join("data.bin").exists();
    holder.kill().unwrap();
    holder.wait().unwrap();

    assert_eq!(report.kept_held, 1, "{report:?}");
    assert_eq!(report.reclaimed, 0);
    assert!(survived);
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[test]
fn directory_with_a_file_held_open_is_kept() {
    let fx = Fx::new();
    let held = fx.scratch("rsi-s2a-fd", RootKind::VarTmp);
    let file = fs::File::open(held.join("data.bin")).unwrap();
    let mut holder = Command::new("sleep")
        .arg("30")
        .current_dir("/")
        .stdin(Stdio::from(file))
        .spawn()
        .unwrap();
    fx.watch(&holder);

    let report = run_pass(&fx.config(RootKind::VarTmp, OLD));
    holder.kill().unwrap();
    holder.wait().unwrap();

    assert_eq!(report.kept_held, 1, "{report:?}");
    assert!(held.join("data.bin").exists());
}

// F2: a closed-fd mmap with cwd and exe elsewhere is still a holder.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[test]
fn mmap_holder_with_closed_fd_is_retained() {
    let fx = Fx::new();
    let held = fx.scratch("rsi-s2a-mmap", RootKind::VarTmp);
    let script = "import mmap,sys,time\n\
                  f=open(sys.argv[1],'r+b')\n\
                  m=mmap.mmap(f.fileno(),0)\n\
                  f.close()\n\
                  print('ready',flush=True)\n\
                  time.sleep(60)\n";
    let mut holder = Command::new("python3")
        .arg("-c")
        .arg(script)
        .arg(held.join("data.bin"))
        .current_dir("/")
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    fx.watch(&holder);
    let mut ready = String::new();
    std::io::BufRead::read_line(
        &mut std::io::BufReader::new(holder.stdout.take().unwrap()),
        &mut ready,
    )
    .unwrap();
    assert_eq!(ready.trim(), "ready");

    let report = run_pass(&fx.config(RootKind::VarTmp, OLD));
    holder.kill().unwrap();
    holder.wait().unwrap();

    assert_eq!(report.kept_held, 1, "{report:?}");
    assert_eq!(report.reclaimed, 0);
    assert!(held.join("data.bin").exists());
}

/// Spawns a python3 process whose worker thread unshares `flag` (`CLONE_FS` or
/// `CLONE_FILES`), runs `setup` on `target`, then sits while the leader and every
/// other thread stay elsewhere (#1178). It calls `unshare(2)` through ctypes, not
/// `os.unshare`, which only exists from python 3.12 (Amazon Linux ships 3.9). When
/// the host provably cannot create the private-state thread (`unshare` refused:
/// seccomp, a container profile) it prints the reason and returns `None` so the
/// caller skips; any other failure to reach `ready` panics.
fn spawn_private_thread_holder(
    fx: &Fx,
    flag: &str,
    setup: &str,
    target: &Path,
) -> Option<std::process::Child> {
    let script = format!(
        "import ctypes,os,sys,threading,time\n\
         libc=ctypes.CDLL(None,use_errno=True)\n\
         def work():\n\
         \x20   if libc.unshare({flag})!=0:\n\
         \x20       print('skip unshare failed: '+os.strerror(ctypes.get_errno()),flush=True)\n\
         \x20       return\n\
         \x20   {setup}\n\
         \x20   print('ready',flush=True)\n\
         \x20   time.sleep(60)\n\
         t=threading.Thread(target=work,daemon=True)\n\
         t.start()\n\
         time.sleep(60)\n"
    );
    let mut holder = Command::new("python3")
        .arg("-c")
        .arg(script)
        .arg(target)
        .current_dir("/")
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    fx.watch(&holder);
    let mut line = String::new();
    std::io::BufRead::read_line(
        &mut std::io::BufReader::new(holder.stdout.take().unwrap()),
        &mut line,
    )
    .unwrap();
    let line = line.trim();
    if line == "ready" {
        return Some(holder);
    }
    holder.kill().unwrap();
    holder.wait().unwrap();
    assert!(
        line.starts_with("skip "),
        "private-thread holder did not become ready: {line:?}"
    );
    eprintln!("SKIPPED: this host cannot create a private-state thread ({line})");
    None
}

// R4: a worker thread that unshared its filesystem state and sits in the
// candidate while the leader and every other thread are elsewhere.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[test]
fn private_thread_cwd_holder_is_retained() {
    let fx = Fx::new();
    let held = fx.scratch("rsi-s2a-thread", RootKind::VarTmp);
    let Some(mut holder) = spawn_private_thread_holder(
        &fx,
        "0x200", // CLONE_FS
        "os.chdir(sys.argv[1])",
        &held,
    ) else {
        return;
    };

    let report = run_pass(&fx.config(RootKind::VarTmp, OLD));
    holder.kill().unwrap();
    holder.wait().unwrap();

    assert_eq!(report.kept_held, 1, "{report:?}");
    assert!(held.join("data.bin").exists());
}

/// A fake `/proc` with one process `4242` whose leader (`task/4242`) has no
/// inventory and whose worker task `4243` runs `setup`.
fn fake_proc(dir: &Path, comm: &str, cgroup: Option<&str>) -> PathBuf {
    let proc_root = dir.join("proc");
    let pid = proc_root.join("4242");
    fs::create_dir_all(pid.join("task/4242")).unwrap();
    fs::create_dir_all(pid.join("task/4243/fd")).unwrap();
    fs::write(pid.join("comm"), format!("{comm}\n")).unwrap();
    if let Some(cgroup) = cgroup {
        fs::write(pid.join("cgroup"), format!("{cgroup}\n")).unwrap();
    }
    proc_root
}

// R4: the leader has no fd directory (exited leader, live worker); the worker's
// mappings are still read.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[test]
fn live_worker_task_mapping_is_held_even_when_the_leader_has_nothing() {
    let fx = Fx::new();
    let meta = tempfile::tempdir().unwrap();
    let held = fx.scratch("rsi-s2a-task", RootKind::VarTmp);
    let proc_root = fake_proc(meta.path(), "worker", None);
    fs::write(
        proc_root.join("4242/task/4243/maps"),
        format!(
            "7f00-7f01 r--p 00000000 08:01 99 {}\n",
            held.join("data.bin").display()
        ),
    )
    .unwrap();
    let mut cfg = fx.config(RootKind::VarTmp, OLD);
    cfg.proc_root = proc_root;

    let report = run_pass(&cfg);

    assert_eq!(report.kept_held, 1, "{report:?}");
    assert!(held.join("data.bin").exists());
}

fn lock_down_fd(proc_root: &Path) {
    set_mode(&proc_root.join("4242/task/4243/fd"), 0);
}

fn unlock_fd(proc_root: &Path) {
    set_mode(&proc_root.join("4242/task/4243/fd"), 0o700);
}

// F2/R4: a live same-user task whose fds cannot be read leaves the proof
// incomplete; only the authenticated user manager is exempt.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[test]
fn unreadable_live_holder_is_unproven_and_only_the_user_manager_is_exempt() {
    let fx = Fx::new();
    let meta = tempfile::tempdir().unwrap();
    let stale = fx.scratch("rsi-s2a-old", RootKind::VarTmp);
    let uid = current_uid();

    // A process merely *named* like the user manager: not authenticated.
    let mut cfg = fx.config(RootKind::VarTmp, OLD);
    cfg.proc_root = fake_proc(meta.path(), "(sd-pam)", None);
    lock_down_fd(&cfg.proc_root);
    let report = unprivileged(|| run_pass(&cfg));
    assert_eq!(report.kept_unproven, 1, "{report:?}");
    assert!(stale.join("data.bin").exists());
    unlock_fd(&cfg.proc_root);

    // The real thing lives in the user manager's cgroup.
    let meta2 = tempfile::tempdir().unwrap();
    cfg.proc_root = fake_proc(
        meta2.path(),
        "(sd-pam)",
        Some(&format!(
            "0::/user.slice/user-{uid}.slice/user@{uid}.service/init.scope"
        )),
    );
    lock_down_fd(&cfg.proc_root);
    let report = unprivileged(|| run_pass(&cfg));
    assert_eq!(report.reclaimed, 1, "{report:?}");
    assert!(!stale.exists());
    unlock_fd(&cfg.proc_root);
}

/// Restores the permissions the fixtures removed, so the temporary
/// directories can be deleted.
struct UnlockTree(PathBuf);

impl UnlockTree {
    fn unlock(dir: &Path) {
        let _ = fs::set_permissions(dir, std::os::unix::fs::PermissionsExt::from_mode(0o700));
        for entry in fs::read_dir(dir).into_iter().flatten().flatten() {
            let is_dir = entry.file_type().is_ok_and(|t| t.is_dir());
            if is_dir {
                Self::unlock(&entry.path());
            }
        }
    }
}

impl Drop for UnlockTree {
    fn drop(&mut self) {
        Self::unlock(&self.0);
    }
}

fn stat_line(pid: u32, ppid: u32) -> String {
    // The name holds a space and a paren: fields are counted after the last one.
    format!(
        "{pid} (sshd (session)) S {ppid} {pid} {pid} 0 -1 4194560 0 0 0 0 0 0 0 0 20 0 1 0 777 0 0\n"
    )
}

fn status_text(comm: &str, uid: u32) -> String {
    format!("Name:\t{comm}\nUid:\t{uid}\t{uid}\t{uid}\t{uid}\n")
}

/// Process `pid` (comm `comm`, our uid) whose parent `parent` reads
/// `parent_uid`, with a chain `parent` -> `grandparent` -> init, and no part of
/// it inspectable. Returns the process directory.
fn hidden_process(
    proc_root: &Path,
    pid: u32,
    comm: &str,
    parent: u32,
    parent_uid: u32,
    grandparent: u32,
) -> PathBuf {
    let dir = proc_root.join(pid.to_string());
    fs::create_dir_all(dir.join(format!("task/{pid}/fd"))).unwrap();
    fs::write(dir.join("comm"), format!("{comm}\n")).unwrap();
    fs::write(dir.join("status"), status_text(comm, current_uid())).unwrap();
    fs::write(dir.join("stat"), stat_line(pid, parent)).unwrap();
    for (up, up_parent, uid) in [(parent, grandparent, parent_uid), (1, 0, 0)] {
        let at = proc_root.join(up.to_string());
        fs::create_dir_all(&at).unwrap();
        fs::write(at.join("stat"), stat_line(up, up_parent)).unwrap();
        fs::write(at.join("status"), status_text("sshd", uid)).unwrap();
    }
    // The kernel refuses a non-dumpable process's inventory.
    set_mode(&dir.join(format!("task/{pid}")), 0);
    dir
}

fn blockers_of(report: &ScratchReport) -> &ProofFailure {
    report
        .entries
        .iter()
        .find_map(|entry| entry.holder_proof.as_ref())
        .unwrap_or_else(|| panic!("no holder proof failure in {report:?}"))
}

// #1159: an unreadable same-user process is never excused; the pass names it
// (kernel-reported facts, comm for display only) so the operator can act.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[test]
fn an_unreadable_process_keeps_the_candidate_and_is_named() {
    let fx = Fx::new();
    let stale = fx.scratch("rsi-s2a-old", RootKind::VarTmp);
    let meta = tempfile::tempdir().unwrap();
    let _unlock = UnlockTree(meta.path().to_path_buf());
    let proc_root = meta.path().join("proc");
    hidden_process(&proc_root, 1234, "sshd-session", 1200, 0, 1);
    let mut cfg = fx.config(RootKind::VarTmp, OLD);
    cfg.proc_root = proc_root;

    let report = unprivileged(|| run_pass(&cfg));

    assert_eq!(report.kept_unproven, 1, "{report:?}");
    assert_eq!(report.reclaimed, 0);
    assert!(stale.join("data.bin").exists());
    let failure = blockers_of(&report);
    assert_eq!(failure.cause, "process-unreadable");
    assert_eq!(failure.omitted, 0);
    let blocker = &failure.blockers[0];
    assert_eq!(blocker.pid, 1234);
    assert_eq!(blocker.comm, "sshd-session");
    assert_eq!(blocker.parent_pid, Some(1200));
    assert_eq!(blocker.parent_uid, Some(0));
    assert_eq!(blocker.refused, "cwd");
    let uid = current_uid();
    assert_eq!(
        blocker.summary,
        format!(
            "kept: unreadable process 1234 (sshd-session, uid {uid}, parent 1200 root-owned); permission denied reading cwd"
        )
    );
}

// The hub's regression: a same-user child of a root-supervised parent (chain to
// init, not our descendant) that already holds the candidate as its cwd and
// then made itself non-dumpable satisfies every identity fact an "outsider"
// rule could use. It must still keep the candidate.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[test]
fn a_hidden_root_parented_holder_never_lets_the_candidate_be_reclaimed() {
    let own = std::process::id();
    let user = current_uid();
    // (parent, parent uid, grandparent): every identity shape.
    let shapes: [(u32, u32, u32); 5] = [
        (4000, 0, 1),    // root supervisor, chain to init
        (4000, 0, own),  // below one of ours
        (1, 0, 0),       // re-parented to init
        (4000, user, 1), // same-user parent
        (0, 0, 0),       // pid namespace edge
    ];
    for (parent, parent_uid, grandparent) in shapes {
        let fx = Fx::new();
        let held = fx.scratch("rsi-s2a-held", RootKind::VarTmp);
        let meta = tempfile::tempdir().unwrap();
        let _unlock = UnlockTree(meta.path().to_path_buf());
        let proc_root = meta.path().join("proc");
        let dir = hidden_process(&proc_root, 4242, "worker", parent, parent_uid, grandparent);
        // The hidden task's cwd is the candidate; the refusal hides it.
        set_mode(&dir.join("task/4242"), 0o700);
        std::os::unix::fs::symlink(&held, dir.join("task/4242/cwd")).unwrap();
        set_mode(&dir.join("task/4242"), 0);
        let mut cfg = fx.config(RootKind::VarTmp, OLD);
        cfg.proc_root = proc_root;

        let report = unprivileged(|| run_pass(&cfg));

        assert_eq!(
            report.reclaimed, 0,
            "{parent}/{parent_uid}/{grandparent}: {report:?}"
        );
        assert_eq!(
            report.kept_unproven, 1,
            "{parent}/{parent_uid}/{grandparent}: {report:?}"
        );
        assert!(held.join("data.bin").exists());
    }
}

// The same, with a real kernel-hidden process: a child sits in the candidate
// and sets itself non-dumpable, which makes its `/proc` entries unreadable.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[test]
fn a_real_non_dumpable_holder_keeps_the_candidate() {
    use std::io::BufRead;
    let fx = Fx::new();
    let held = fx.scratch("rsi-s2a-dumpable", RootKind::VarTmp);
    let mut child = Command::new("python3")
        .args([
            "-c",
            "import ctypes, os, sys, time\nos.chdir(sys.argv[1])\nctypes.CDLL(None).prctl(4, 0, 0, 0, 0)\nprint('ready', flush=True)\ntime.sleep(60)",
        ])
        .arg(&held)
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut ready = String::new();
    std::io::BufReader::new(child.stdout.take().unwrap())
        .read_line(&mut ready)
        .unwrap();
    assert_eq!(ready.trim(), "ready");
    let meta = tempfile::tempdir().unwrap();
    let proc_root = meta.path().join("proc");
    fs::create_dir(&proc_root).unwrap();
    std::os::unix::fs::symlink(
        format!("/proc/{}", child.id()),
        proc_root.join(child.id().to_string()),
    )
    .unwrap();
    let mut cfg = fx.config(RootKind::VarTmp, OLD);
    cfg.proc_root = proc_root;

    let report = unprivileged(|| run_pass(&cfg));
    let survived = held.join("data.bin").exists();
    child.kill().unwrap();
    child.wait().unwrap();

    // Hidden: kept unproven (and named); readable (a root runner): held.
    assert_eq!(report.reclaimed, 0, "{report:?}");
    assert_eq!(report.kept_held + report.kept_unproven, 1, "{report:?}");
    assert!(survived);
}

// The readable tasks of a process whose other task is hidden: whichever task is
// refused (first or second), the candidate stays, and once the refusal is lifted
// the same process's readable task is what holds it.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[test]
fn a_hidden_task_beside_a_readable_holder_in_the_same_process_keeps_the_candidate() {
    for hidden_task in [4243u32, 4244] {
        let readable_task = if hidden_task == 4243 { 4244 } else { 4243 };
        let fx = Fx::new();
        let held = fx.scratch("rsi-s2a-two-tasks", RootKind::VarTmp);
        let meta = tempfile::tempdir().unwrap();
        let _unlock = UnlockTree(meta.path().to_path_buf());
        let proc_root = fake_proc(meta.path(), "worker", None);
        let process = proc_root.join("4242");
        for task in [4243u32, 4244] {
            fs::create_dir_all(process.join(format!("task/{task}/fd"))).unwrap();
        }
        fs::write(
            process.join(format!("task/{readable_task}/maps")),
            format!(
                "7f00-7f01 r--p 00000000 08:01 99 {}\n",
                held.join("data.bin").display()
            ),
        )
        .unwrap();
        set_mode(&process.join(format!("task/{hidden_task}/fd")), 0);
        let mut cfg = fx.config(RootKind::VarTmp, OLD);
        cfg.proc_root = proc_root;

        let report = unprivileged(|| run_pass(&cfg));

        assert_eq!(report.reclaimed, 0, "hidden {hidden_task}: {report:?}");
        assert_eq!(report.kept_unproven, 1, "hidden {hidden_task}: {report:?}");
        assert!(held.join("data.bin").exists());

        set_mode(&process.join(format!("task/{hidden_task}/fd")), 0o700);
        let report = unprivileged(|| run_pass(&cfg));
        assert_eq!(
            report.kept_held, 1,
            "visible, hidden {hidden_task}: {report:?}"
        );
        assert!(held.join("data.bin").exists());
    }
}

// A readable holder in another process is still found beside a hidden one: the
// proof is incomplete, so the candidate is retained, never reclaimed.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[test]
fn a_hidden_process_beside_a_readable_holder_in_another_process_keeps_the_candidate() {
    let fx = Fx::new();
    let held = fx.scratch("rsi-s2a-held", RootKind::VarTmp);
    let meta = tempfile::tempdir().unwrap();
    let _unlock = UnlockTree(meta.path().to_path_buf());
    let proc_root = meta.path().join("proc");
    hidden_process(&proc_root, 1234, "sshd-session", 1200, 0, 1);
    let other = proc_root.join("4300/task/4300");
    fs::create_dir_all(other.join("fd")).unwrap();
    fs::write(
        other.join("maps"),
        format!(
            "7f00-7f01 r--p 00000000 08:01 99 {}\n",
            held.join("data.bin").display()
        ),
    )
    .unwrap();
    let mut cfg = fx.config(RootKind::VarTmp, OLD);
    cfg.proc_root = proc_root;

    let report = unprivileged(|| run_pass(&cfg));

    assert_eq!(report.reclaimed, 0, "{report:?}");
    assert_eq!(report.kept_unproven, 1, "{report:?}");
    assert!(held.join("data.bin").exists());
}

// The names are bounded per candidate; the rest are counted.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[test]
fn blockers_are_capped_per_candidate_and_counted() {
    let fx = Fx::new();
    let stale = fx.scratch("rsi-s2a-old", RootKind::VarTmp);
    let meta = tempfile::tempdir().unwrap();
    let _unlock = UnlockTree(meta.path().to_path_buf());
    let proc_root = meta.path().join("proc");
    for pid in 5000..5012 {
        hidden_process(&proc_root, pid, "hidden", 1200, 0, 1);
    }
    let mut cfg = fx.config(RootKind::VarTmp, OLD);
    cfg.proc_root = proc_root;

    let report = unprivileged(|| run_pass(&cfg));

    assert!(stale.join("data.bin").exists());
    let failure = blockers_of(&report);
    assert_eq!(failure.blockers.len(), holders::MAX_BLOCKERS);
    assert_eq!(
        failure.omitted,
        12 - u32::try_from(holders::MAX_BLOCKERS).unwrap()
    );
}

// ... and across the whole report.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[test]
fn a_report_names_a_bounded_number_of_blockers_in_total() {
    let failure = |n: u32| ProofFailure {
        cause: "process-unreadable".into(),
        blockers: (0..8)
            .map(|i| Blocker {
                pid: n * 10 + i,
                comm: "x".into(),
                uids: None,
                parent_pid: None,
                parent_uid: None,
                refused: "fd".into(),
                why: "permission denied".into(),
                summary: String::new(),
            })
            .collect(),
        omitted: 0,
    };
    let mut report = ScratchReport::default();
    for n in 0..20 {
        report.record_with(
            Path::new(&format!("/x/{n}")),
            Decision::Unproven,
            0,
            Some(failure(n)),
        );
    }
    let named: usize = report
        .entries
        .iter()
        .filter_map(|e| e.holder_proof.as_ref())
        .map(|f| f.blockers.len())
        .sum();
    let omitted: u32 = report
        .entries
        .iter()
        .filter_map(|e| e.holder_proof.as_ref())
        .map(|f| f.omitted)
        .sum();
    assert_eq!(named, MAX_REPORTED_BLOCKERS);
    assert_eq!(u32::try_from(named).unwrap() + omitted, 160);
    // A candidate that was not unproven never carries any.
    report.record_with(Path::new("/x/held"), Decision::Held, 0, Some(failure(99)));
    assert!(report.entries.last().unwrap().holder_proof.is_none());
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[test]
fn kernel_text_is_parsed_and_displayed_safely() {
    assert_eq!(holders::stat_ppid(&stat_line(9, 77)), Some(77));
    assert!(holders::stat_ppid("9 (x) S").is_none());
    let ids = holders::id_line("Name:\tx\nUid:\t1\t2\t3\t4\nGid:\t5\t6\t7\t8\n", "Uid");
    assert_eq!(ids, Some([1, 2, 3, 4]));
    assert!(holders::id_line("Uid:\t1\t2\n", "Uid").is_none());
    // A name is display only: control characters are dropped, length is capped.
    assert_eq!(holders::display_only("a\x1b[31mb\nc\n"), "a[31mbc");
    assert_eq!(holders::display_only(&"x".repeat(100)).len(), 32);
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[test]
fn unreadable_process_inventory_keeps_everything() {
    let fx = Fx::new();
    let stale = fx.scratch("rsi-s2a-old", RootKind::VarTmp);
    let mut cfg = fx.config(RootKind::VarTmp, OLD);
    cfg.proc_root = fx.root().join("no-such-proc");

    let report = run_pass(&cfg);

    assert_eq!(report.kept_unproven, 1, "{report:?}");
    assert!(stale.join("data.bin").exists());
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[test]
fn dirty_git_worktree_is_kept_and_clean_published_one_is_reclaimed() {
    let fx = Fx::new();
    let origins = tempfile::tempdir().unwrap();
    let dirty = fx.scratch("rsi-s2a-dirty", RootKind::VarTmp);
    let clean = fx.scratch("rsi-s2a-clean", RootKind::VarTmp);
    published_repo(&dirty.join("repo"), origins.path());
    published_repo(&clean.join("repo"), origins.path());
    fs::write(dirty.join("repo/untracked.txt"), "work\n").unwrap();

    let report = run_pass(&fx.config(RootKind::VarTmp, OLD));

    assert_eq!(report.kept_dirty, 1, "{report:?}");
    assert_eq!(report.reclaimed, 1, "{report:?}");
    assert!(dirty.join("repo/untracked.txt").exists());
    assert!(!clean.exists());
}

// F3: a clean tree whose commits exist nowhere else is not disposable.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[test]
fn clean_repo_with_local_only_commit_is_retained() {
    let fx = Fx::new();
    let dir = fx.scratch("rsi-s2a-local", RootKind::VarTmp);
    let repo = dir.join("repo");
    fs::create_dir_all(&repo).unwrap();
    git(&repo, &["init", "-q", "-b", "main"]);
    fs::write(repo.join("a.txt"), "a\n").unwrap();
    git(&repo, &["add", "a.txt"]);
    git(&repo, &["commit", "-q", "-m", "local only"]);

    let report = run_pass(&fx.config(RootKind::VarTmp, OLD));

    assert_eq!(report.kept_unpublished, 1, "{report:?}");
    assert_eq!(report.reclaimed, 0);
    assert!(repo.join("a.txt").exists());
}

// F3: a published branch does not cover a local commit on a detached HEAD or
// a stash.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[test]
fn detached_head_and_stash_beyond_the_remote_are_retained() {
    let fx = Fx::new();
    let origins = tempfile::tempdir().unwrap();
    let detached = fx.scratch("rsi-s2a-detached", RootKind::VarTmp);
    let stashed = fx.scratch("rsi-s2a-stash", RootKind::VarTmp);
    for dir in [&detached, &stashed] {
        published_repo(&dir.join("repo"), origins.path());
    }
    let repo = detached.join("repo");
    git(&repo, &["checkout", "-q", "--detach"]);
    fs::write(repo.join("b.txt"), "b\n").unwrap();
    git(&repo, &["add", "b.txt"]);
    git(&repo, &["commit", "-q", "-m", "detached work"]);
    let repo = stashed.join("repo");
    fs::write(repo.join("a.txt"), "changed\n").unwrap();
    git(&repo, &["stash", "push", "-q"]);

    let report = run_pass(&fx.config(RootKind::VarTmp, OLD));

    assert_eq!(report.kept_unpublished, 2, "{report:?}");
    assert!(detached.join("repo/b.txt").exists());
    assert!(stashed.join("repo/a.txt").exists());
}

// F4: build-output directories and deep nesting are walked, not skipped.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[test]
fn dirty_repo_under_target_and_node_modules_and_deep_is_retained() {
    let fx = Fx::new();
    let origins = tempfile::tempdir().unwrap();
    let under_target = fx.scratch("rsi-old-target", RootKind::VarTmp);
    let under_modules = fx.scratch("rsi-old-modules", RootKind::VarTmp);
    let deep = fx.scratch("rsi-old-deep", RootKind::VarTmp);
    let nested = |base: &Path, parts: &[&str]| {
        let repo = parts.iter().fold(base.to_path_buf(), |acc, p| acc.join(p));
        published_repo(&repo, origins.path());
        fs::write(repo.join("wip.txt"), "unsaved\n").unwrap();
        repo
    };
    let a = nested(&under_target, &["target", "debug", "repo-a"]);
    let b = nested(&under_modules, &["node_modules", "pkg", "repo-b"]);
    let deep_parts: Vec<String> = (0..12).map(|i| format!("d{i}")).collect();
    let deep_refs: Vec<&str> = deep_parts.iter().map(String::as_str).collect();
    let c = nested(&deep, &deep_refs);

    let report = run_pass(&fx.config(RootKind::VarTmp, OLD));

    assert_eq!(report.kept_dirty, 3, "{report:?}");
    assert_eq!(report.reclaimed, 0);
    for repo in [a, b, c] {
        assert!(repo.join("wip.txt").exists());
    }
}

// R6: a dirty checkout saved inside another repository's own `.git` is still a
// repository to check.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[test]
fn dirty_checkout_saved_under_a_dot_git_is_retained() {
    let fx = Fx::new();
    let origins = tempfile::tempdir().unwrap();
    let dir = fx.scratch("rsi-old-saved", RootKind::VarTmp);
    let outer = dir.join("outer");
    published_repo(&outer, origins.path());
    let saved = outer.join(".git/saved/repo");
    fs::create_dir_all(&saved).unwrap();
    git(&saved, &["init", "-q", "-b", "main"]);
    fs::write(saved.join("precious.txt"), "unsaved work\n").unwrap();

    let report = run_pass(&fx.config(RootKind::VarTmp, OLD));

    assert_eq!(report.reclaimed, 0, "{report:?}");
    assert_eq!(report.kept_dirty, 1, "{report:?}");
    assert!(saved.join("precious.txt").exists());
}

// F5: a writer that touches the tree after the rename (an fd opened before
// it) sends the candidate back; the new data survives at the original name.
fn late_writer(aside: &Path) {
    fs::write(aside.join("late.txt"), b"written after the proof").unwrap();
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[test]
fn late_writer_after_rename_is_retained_and_restored() {
    let fx = Fx::new();
    let dir = fx.scratch("rsi-s2a-late", RootKind::VarTmp);
    let mut cfg = fx.config(RootKind::VarTmp, OLD);
    cfg.after_rename = Some(late_writer);

    let report = run_pass(&cfg);

    assert_eq!(report.kept_changed, 1, "{report:?}");
    assert_eq!(report.reclaimed, 0);
    assert_eq!(
        fs::read(dir.join("late.txt")).unwrap(),
        b"written after the proof"
    );
    assert_eq!(only_entries(fx.root()), ["rsi-s2a-late"]);
}

fn late_dirty_edit(aside: &Path) {
    fs::write(aside.join("repo/a.txt"), "edited late\n").unwrap();
}

// F5: dirt created after the first git check is caught by the re-proof.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[test]
fn tracked_file_edited_after_rename_is_retained() {
    let fx = Fx::new();
    let origins = tempfile::tempdir().unwrap();
    let dir = fx.scratch("rsi-s2a-edit", RootKind::VarTmp);
    published_repo(&dir.join("repo"), origins.path());
    let mut cfg = fx.config(RootKind::VarTmp, OLD);
    cfg.after_rename = Some(late_dirty_edit);

    let report = run_pass(&cfg);

    assert_eq!(report.reclaimed, 0, "{report:?}");
    assert_eq!(report.kept_changed + report.kept_dirty, 1, "{report:?}");
    assert_eq!(
        fs::read_to_string(dir.join("repo/a.txt")).unwrap(),
        "edited late\n"
    );
}

fn replace_proved_file(aside: &Path) {
    fs::remove_file(aside.join("data.bin")).unwrap();
    fs::write(aside.join("data.bin"), b"a different file, same name").unwrap();
}

fn add_work_after_proof(aside: &Path) {
    fs::create_dir_all(aside.join("added/repo")).unwrap();
    fs::write(aside.join("added/repo/precious.txt"), b"unpublished").unwrap();
}

// R8: an entry replaced or added after the final proof is not deleted: the
// deletion only touches what the proof named.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[test]
fn replacement_or_addition_after_the_final_proof_is_not_deleted() {
    for hook in [
        replace_proved_file as fn(&Path),
        add_work_after_proof as fn(&Path),
    ] {
        let fx = Fx::new();
        let dir = fx.scratch("rsi-s2a-swap", RootKind::VarTmp);
        for i in 0..20 {
            fs::write(dir.join(format!("f{i}")), b"x").unwrap();
        }
        let mut cfg = fx.config(RootKind::VarTmp, OLD);
        cfg.after_proof = Some(hook);

        let report = run_pass(&cfg);

        assert_eq!(report.kept_changed, 1, "{report:?}");
        assert_eq!(report.reclaimed, 0, "{report:?}");
        // The aside directory keeps whatever the proof did not name.
        let leftovers = only_entries(fx.root());
        assert_eq!(leftovers.len(), 1, "{leftovers:?}");
        let aside = fx.root().join(&leftovers[0]);
        let survivor = if aside.join("added/repo/precious.txt").exists() {
            aside.join("added/repo/precious.txt")
        } else {
            aside.join("data.bin")
        };
        assert!(survivor.exists(), "{leftovers:?}");
    }
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[test]
fn interrupted_aside_leftover_without_a_manifest_is_judged_afresh_and_finished() {
    let fx = Fx::new();
    let leftover = fx.scratch(".rsi-reclaiming-rsi-x-1", RootKind::VarTmp);

    let report = run_pass(&fx.config(RootKind::VarTmp, OLD));

    assert_eq!(report.reclaimed, 1, "{report:?}");
    assert!(!leftover.exists());
}

// F8: an aside name does not bypass the root's name policy or provenance.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[test]
fn aside_names_must_derive_from_an_allowed_name_and_carry_a_record() {
    let fx = Fx::new();
    let foreign = fx.scratch(".rsi-reclaiming-photos-7", RootKind::VarTmp);
    let malformed = fx.scratch(".rsi-reclaiming-rsi-x", RootKind::VarTmp);
    let unrecorded = legacy_dir(fx.root(), ".rsi-reclaiming-rsi-y-9");

    let report = run_pass(&fx.config(RootKind::VarTmp, OLD));

    assert_eq!(report.reclaimed, 0, "{report:?}");
    assert_eq!(report.kept_unrecorded, 1, "{report:?}");
    for dir in [foreign, malformed, unrecorded] {
        assert!(dir.join("data.bin").exists());
    }
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[test]
fn entry_budget_bounds_a_pass() {
    let fx = Fx::new();
    for i in 0..3 {
        fx.scratch(&format!("rsi-old-{i}"), RootKind::VarTmp);
    }
    let mut cfg = fx.config(RootKind::VarTmp, OLD);
    cfg.max_entries = 2;

    let report = run_pass(&cfg);

    assert_eq!(report.reclaimed, 2, "{report:?}");
    assert!(report.budget_exhausted);
    assert_eq!(only_entries(fx.root()).len(), 1);
}

// F6: the time budget bounds the whole pass, not just the gaps between
// candidates.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[test]
fn expired_time_budget_deletes_nothing() {
    let fx = Fx::new();
    let dir = fx.scratch("rsi-old-time", RootKind::VarTmp);
    let mut cfg = fx.config(RootKind::VarTmp, OLD);
    cfg.max_duration = Duration::ZERO;

    let report = run_pass(&cfg);

    assert!(report.budget_exhausted, "{report:?}");
    assert_eq!(report.reclaimed, 0, "{report:?}");
    assert!(dir.join("data.bin").exists());
}

// F6: scanning a big tree draws on the same budget; running out retains it.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[test]
fn scan_budget_smaller_than_the_tree_retains_it() {
    let fx = Fx::new();
    let dir = fx.scratch("rsi-old-scan", RootKind::VarTmp);
    for i in 0..60 {
        fs::write(dir.join(format!("f{i}")), b"x").unwrap();
    }
    let mut cfg = fx.config(RootKind::VarTmp, OLD);
    cfg.max_scan_entries = 40;

    let report = run_pass(&cfg);

    assert!(report.budget_exhausted, "{report:?}");
    assert_eq!(report.reclaimed, 0, "{report:?}");
    assert_eq!(report.kept_unproven, 1, "{report:?}");
    assert!(dir.join("f59").exists());
}

// R5: the root listing itself is charged entry by entry: a root bigger than the
// budget is abandoned before anything is allocated or touched.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[test]
fn root_listing_larger_than_the_budget_is_abandoned() {
    let fx = Fx::new();
    let dir = fx.scratch("rsi-old-big-root", RootKind::VarTmp);
    for i in 0..200 {
        fs::write(fx.root().join(format!("noise-{i}")), b"x").unwrap();
    }
    let mut cfg = fx.config(RootKind::VarTmp, OLD);
    cfg.max_scan_entries = 50;

    let report = run_pass(&cfg);

    assert!(report.budget_exhausted, "{report:?}");
    assert_eq!(report.considered, 0, "{report:?}");
    assert!(dir.join("data.bin").exists());
}

fn pass_that_runs_out_of_budget_mid_removal(fx: &Fx, dir: &Path, meta: &Path) -> ScratchConfig {
    // An empty inventory keeps the budget arithmetic about the tree only.
    let proc_root = meta.join("proc");
    fs::create_dir_all(&proc_root).unwrap();
    let mut cfg = fx.config(RootKind::VarTmp, OLD);
    cfg.proc_root = proc_root;
    let tree = fsys::census(
        &record::pin_dir(dir).unwrap(),
        &mut Budget::new(Instant::now() + Duration::from_secs(60), usize::MAX),
    )
    .unwrap()
    .entries;
    let top = fs::read_dir(dir).unwrap().count();
    // Enough to prove (age scan, two walks) but not to also delete.
    cfg.max_scan_entries = 1 + top + 2 * tree + tree / 2;
    cfg
}

// F6: removal is bounded too; the leftover resumes from the persisted manifest.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[test]
fn removal_runs_out_of_budget_then_resumes_from_the_aside_leftover() {
    let fx = Fx::new();
    let meta = tempfile::tempdir().unwrap();
    let origins = tempfile::tempdir().unwrap();
    let dir = fx.scratch("rsi-old-resume", RootKind::VarTmp);
    published_repo(&dir.join("repo"), origins.path());
    for i in 0..300 {
        fs::write(dir.join(format!("f{i}")), b"x").unwrap();
    }
    let mut cfg = pass_that_runs_out_of_budget_mid_removal(&fx, &dir, meta.path());

    let first = run_pass(&cfg);
    assert_eq!(first.partial, 1, "{first:?}");
    assert!(!dir.exists(), "the original name is gone");
    let leftover = only_entries(fx.root());
    assert_eq!(leftover.len(), 1, "{leftover:?}");
    assert!(leftover[0].starts_with(ASIDE_PREFIX));

    cfg.max_scan_entries = MAX_SCAN_ENTRIES_PER_PASS;
    let second = run_pass(&cfg);
    assert_eq!(second.reclaimed, 1, "{second:?}");
    assert!(only_entries(fx.root()).is_empty());
    assert!(only_entries(fx.registry()).is_empty());
}

fn partial_leftover(fx: &Fx, meta: &Path, origins: &Path) -> (ScratchConfig, PathBuf) {
    let dir = fx.scratch("rsi-old-resume", RootKind::VarTmp);
    published_repo(&dir.join("repo"), origins);
    for i in 0..300 {
        fs::write(dir.join(format!("f{i}")), b"x").unwrap();
    }
    let mut cfg = pass_that_runs_out_of_budget_mid_removal(fx, &dir, meta);
    let first = run_pass(&cfg);
    assert_eq!(first.partial, 1, "{first:?}");
    cfg.max_scan_entries = MAX_SCAN_ENTRIES_PER_PASS;
    let leftover = only_entries(fx.root());
    assert_eq!(leftover.len(), 1, "{leftover:?}");
    (cfg, fx.root().join(&leftover[0]))
}

// R2: work added to a half-deleted tree between passes is never deleted, and
// neither is a survivor edited in place.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[test]
fn work_added_or_edited_between_passes_survives_a_resumed_removal() {
    for edit in ["add-file", "add-repo", "edit-survivor"] {
        let fx = Fx::new();
        let meta = tempfile::tempdir().unwrap();
        let origins = tempfile::tempdir().unwrap();
        let (cfg, leftover) = partial_leftover(&fx, meta.path(), origins.path());
        let precious = match edit {
            "add-file" => {
                fs::write(leftover.join("new-work.txt"), b"unpublished").unwrap();
                leftover.join("new-work.txt")
            }
            "add-repo" => {
                let repo = leftover.join("saved-checkout");
                fs::create_dir_all(&repo).unwrap();
                git(&repo, &["init", "-q", "-b", "main"]);
                fs::write(repo.join("wip.txt"), b"dirty").unwrap();
                repo.join("wip.txt")
            }
            _ => {
                let survivor = fs::read_dir(&leftover)
                    .unwrap()
                    .map(|e| e.unwrap().path())
                    .find(|p| {
                        p.is_file()
                            && p.file_name()
                                .is_some_and(|n| n.to_string_lossy().starts_with('f'))
                    })
                    .expect("a surviving file");
                fs::write(&survivor, b"edited, longer content").unwrap();
                survivor
            }
        };

        let report = run_pass(&cfg);

        assert_eq!(report.kept_changed, 1, "{edit}: {report:?}");
        assert_eq!(report.reclaimed, 0, "{edit}: {report:?}");
        assert!(precious.exists(), "{edit}");
    }
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[test]
fn killed_lander_scratch_is_reclaimed_and_live_lander_scratch_is_kept() {
    let fx = Fx::new();
    let orphan = fx.lander("rsi-rolling-land-orphan");
    write_owner(&orphan, dead_pid(), 1);
    // The current test process is a live owner with its real start ticks.
    let live = fx.lander("rsi-rolling-land-live");
    // A pid-reuse look-alike: live pid, different start time -> gone.
    let reused = fx.lander("rsi-rolling-land-reused");
    write_owner(&reused, std::process::id(), 1);
    let cfg = fx.config(RootKind::LanderScratch, Duration::from_secs(3600));

    // `now` is the present: the orphan is reclaimed with no age gate.
    let report = run(&cfg, SystemTime::now(), false);

    assert_eq!(report.reclaimed, 2, "{report:?}");
    assert_eq!(report.kept_held, 1, "{report:?}");
    assert!(!orphan.exists());
    assert!(!reused.exists());
    assert!(live.join("data.bin").exists());
}

// F7: a pid recorded in another PID namespace means nothing here; the owner
// may be alive, so the workspace is kept even though no such pid exists.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[test]
fn live_lander_in_a_foreign_pid_namespace_is_retained() {
    let fx = Fx::new();
    let foreign = fx.lander("rsi-rolling-land-foreign");
    let host = HostIdentity::read(Path::new("/proc")).unwrap();
    let foreign_ns = HostIdentity {
        pid_ns: "pid:[1]".to_string(),
        boot_id: host.boot_id.clone(),
    };
    fs::write(
        foreign.join(LANDER_OWNER_FILE),
        holders::owner_text(dead_pid(), 12345, &foreign_ns, Path::new("/sandbox")),
    )
    .unwrap();
    // The same pid from a previous boot is genuinely gone.
    let rebooted = fx.lander("rsi-rolling-land-rebooted");
    let old_boot = HostIdentity {
        pid_ns: host.pid_ns.clone(),
        boot_id: "00000000-0000-0000-0000-000000000000".to_string(),
    };
    fs::write(
        rebooted.join(LANDER_OWNER_FILE),
        holders::owner_text(std::process::id(), 1, &old_boot, Path::new("/sandbox")),
    )
    .unwrap();
    // A malformed owner file is ambiguous, not "unregistered".
    let garbled = fx.lander("rsi-rolling-land-garbled");
    fs::write(garbled.join(LANDER_OWNER_FILE), "not an owner file\n").unwrap();

    let cfg = fx.config(RootKind::LanderScratch, Duration::from_secs(3600));
    let report = run(&cfg, SystemTime::now(), false);

    assert_eq!(report.kept_unproven, 2, "{report:?}");
    assert_eq!(report.reclaimed, 1, "{report:?}");
    assert!(foreign.join("data.bin").exists());
    assert!(garbled.join("data.bin").exists());
    assert!(!rebooted.exists());
}

// F7: the owner is re-read for aside leftovers too; a live owner keeps one.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[test]
fn aside_leftover_of_a_live_lander_is_retained() {
    let fx = Fx::new();
    let name = format!("{ASIDE_PREFIX}rsi-rolling-land-live-{}", std::process::id());
    let aside = fx.lander(&name);

    let report = run_pass(&fx.config(RootKind::LanderScratch, OLD));

    assert_eq!(report.kept_held, 1, "{report:?}");
    assert_eq!(report.reclaimed, 0);
    assert!(aside.join("data.bin").exists());
}

// F8: only a registered workspace is a lander's; a look-alike name (even an
// aged checkout with real work) is retained.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[test]
fn unregistered_rsi_rolling_land_dir_is_retained() {
    let fx = Fx::new();
    let lookalike = legacy_dir(fx.root(), "rsi-rolling-land-manual");
    fs::create_dir_all(lookalike.join("checkout")).unwrap();
    git(&lookalike.join("checkout"), &["init", "-q", "-b", "main"]);
    fs::write(lookalike.join("checkout/work.txt"), "operator work\n").unwrap();
    let old_owner = legacy_dir(fx.root(), "rsi-rolling-land-legacy-owner");
    write_owner(&old_owner, dead_pid(), 1);

    let report = run_pass(&fx.config(RootKind::LanderScratch, OLD));

    assert_eq!(report.kept_unrecorded, 2, "{report:?}");
    assert_eq!(report.reclaimed, 0, "{report:?}");
    assert!(lookalike.join("checkout/work.txt").exists());
    assert!(old_owner.join("data.bin").exists());
}

// A recorded workspace with no owner file waits for the age gate.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[test]
fn recorded_lander_scratch_without_owner_waits_for_the_age_gate() {
    let fx = Fx::new();
    let dir = fx.scratch("rsi-rolling-land-noowner", RootKind::LanderScratch);
    let cfg = fx.config(RootKind::LanderScratch, Duration::from_secs(3600));

    let fresh = run(&cfg, SystemTime::now(), false);
    assert_eq!(fresh.kept_young, 1, "{fresh:?}");
    assert!(dir.exists());

    let later = run(&cfg, SystemTime::now() + Duration::from_secs(7200), false);
    assert_eq!(later.reclaimed, 1, "{later:?}");
    assert!(!dir.exists());
}

/// A lander workspace whose owner is dead, with the exact private clone
/// registered and a real `--shared --no-checkout` clone made into it, the way
/// the lander does.
fn lander_with_clone(fx: &Fx, name: &str, source: &Path) -> PathBuf {
    let dir = fx.root().join(name);
    fs::create_dir(&dir).unwrap();
    register_lander_owner_in(fx.registry(), &dir, source).unwrap();
    let repo = dir.join(LANDER_CLONE_DIR);
    fs::create_dir(&repo).unwrap();
    register_lander_clone_in(fx.registry(), &dir, &repo).unwrap();
    let status = isolated_git()
        .args(["clone", "-q", "--shared", "--no-checkout"])
        .arg(source)
        .arg(&repo)
        .status()
        .unwrap();
    assert!(status.success());
    // The lander commits its integration merge in the clone: unpublished.
    git(
        &repo,
        &["commit", "-q", "--allow-empty", "-m", "integration merge"],
    );
    write_owner(&dir, dead_pid(), 1);
    dir
}

// R3: the lander's exact registered private clone is derived data: dirty and
// unpublished by design, reclaimed once its owner is gone.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[test]
fn dead_lander_workspace_with_its_registered_shared_clone_is_reclaimed() {
    let fx = Fx::new();
    let origins = tempfile::tempdir().unwrap();
    let source = origins.path().join("source");
    published_repo(&source, origins.path());
    let workspace = lander_with_clone(&fx, "rsi-rolling-land-clone", &source);

    let report = run_pass(&fx.config(RootKind::LanderScratch, OLD));

    assert_eq!(report.reclaimed, 1, "{report:?}");
    assert!(!workspace.exists());
}

// R3: the exemption is the one registered clone, not the workspace: any other
// repository inside it, or a replaced clone, gets the full checks.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[test]
fn other_repositories_in_a_lander_workspace_are_fully_checked() {
    let fx = Fx::new();
    let origins = tempfile::tempdir().unwrap();
    let source = origins.path().join("source");
    published_repo(&source, origins.path());

    // A user checkout moved in next to the genuine clone.
    let beside = lander_with_clone(&fx, "rsi-rolling-land-beside", &source);
    git(&beside, &["init", "-q", "-b", "main", "checkout"]);
    fs::write(beside.join("checkout/work.txt"), b"mine").unwrap();
    // A nested repository below the genuine clone.
    let below = lander_with_clone(&fx, "rsi-rolling-land-below", &source);
    fs::create_dir_all(below.join("repo/sub")).unwrap();
    git(&below.join("repo/sub"), &["init", "-q", "-b", "main"]);
    fs::write(below.join("repo/sub/work.txt"), b"mine").unwrap();
    // The registered clone replaced by an ordinary repository with a commit.
    let replaced = lander_with_clone(&fx, "rsi-rolling-land-replaced", &source);
    fs::remove_dir_all(replaced.join("repo")).unwrap();
    fs::create_dir(replaced.join("repo")).unwrap();
    git(&replaced.join("repo"), &["init", "-q", "-b", "main"]);
    fs::write(replaced.join("repo/a.txt"), b"a").unwrap();
    git(&replaced.join("repo"), &["add", "a.txt"]);
    git(&replaced.join("repo"), &["commit", "-q", "-m", "mine"]);

    let report = run_pass(&fx.config(RootKind::LanderScratch, OLD));

    assert_eq!(report.reclaimed, 0, "{report:?}");
    assert_eq!(report.kept_dirty + report.kept_unpublished, 3, "{report:?}");
    assert!(beside.join("checkout/work.txt").exists());
    assert!(below.join("repo/sub/work.txt").exists());
    assert!(replaced.join("repo/a.txt").exists());
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[test]
fn lander_clone_registration_requires_a_fresh_empty_directory_named_repo() {
    let fx = Fx::new();
    let workspace = fx.lander("rsi-rolling-land-reg");
    let wrong_name = workspace.join("elsewhere");
    fs::create_dir(&wrong_name).unwrap();
    assert!(register_lander_clone_in(fx.registry(), &workspace, &wrong_name).is_err());
    let populated = workspace.join(LANDER_CLONE_DIR);
    fs::create_dir(&populated).unwrap();
    fs::write(populated.join("work.txt"), b"existing").unwrap();
    assert!(register_lander_clone_in(fx.registry(), &workspace, &populated).is_err());
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[test]
fn lander_workspace_sweep_reclaims_only_dead_owners() {
    let fx = Fx::new();
    let orphan = fx.lander("rsi-rolling-land-dead");
    write_owner(&orphan, dead_pid(), 1);
    let live = fx.lander("rsi-rolling-land-live");
    let mut config =
        ScratchConfig::with_roots(vec![(fx.root().to_path_buf(), RootKind::LanderScratch)]);
    config.registry = fx.registry().to_path_buf();
    config.proc_root = fx.mirror_proc();
    config.mountinfo = fx.mountinfo();
    config.max_duration = Duration::from_secs(20);

    let report = run(&config, SystemTime::now(), false);

    assert_eq!(report.reclaimed, 1, "{report:?}");
    assert!(!orphan.exists());
    assert!(live.exists());
}

// F1: a symlink anywhere on the root path, or at the root, refuses the root
// and nothing is deleted through it.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[test]
fn symlinked_root_or_ancestor_is_refused() {
    let fx = Fx::new();
    let base = tempfile::tempdir().unwrap();
    let real = base.path().join("real-cache");
    fs::create_dir_all(&real).unwrap();
    let stale =
        create_scratch_dir_in(fx.registry(), &real, "rsi-w1-tmp", RootKind::WorkerCache).unwrap();
    fs::write(stale.join("data.bin"), b"x").unwrap();
    // The root itself is a symlink to the real directory.
    let link = base.path().join("link-cache");
    std::os::unix::fs::symlink(&real, &link).unwrap();
    // An ancestor of the root is a symlink.
    let via_ancestor = base.path().join("ancestor-link");
    std::os::unix::fs::symlink(base.path(), &via_ancestor).unwrap();

    for root in [link, via_ancestor.join("real-cache")] {
        let mut cfg = fx.config(RootKind::WorkerCache, OLD);
        cfg.roots = vec![(root, RootKind::WorkerCache)];
        let report = run_pass(&cfg);
        assert_eq!(report.refused_roots, 1, "{report:?}");
        assert_eq!(report.reclaimed, 0, "{report:?}");
        assert!(stale.join("data.bin").exists());
    }

    // The same directory reached without a symlink is fine.
    let mut cfg = fx.config(RootKind::WorkerCache, OLD);
    cfg.roots = vec![(real, RootKind::WorkerCache)];
    let report = run_pass(&cfg);
    assert_eq!(report.reclaimed, 1, "{report:?}");
}

// R7: an ancestor another user could rename entries in is not authentic, even
// when we own it.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[test]
fn group_or_world_writable_ancestor_is_refused_unless_sticky() {
    let fx = Fx::new();
    let base = tempfile::tempdir().unwrap();
    let parent = base.path().join("parent");
    let root = parent.join("cache");
    fs::create_dir_all(&root).unwrap();
    let stale =
        create_scratch_dir_in(fx.registry(), &root, "rsi-w2-tmp", RootKind::WorkerCache).unwrap();
    fs::write(stale.join("data.bin"), b"x").unwrap();
    let mut cfg = fx.config(RootKind::WorkerCache, OLD);
    cfg.roots = vec![(root.clone(), RootKind::WorkerCache)];

    for (mode, target) in [
        (0o770, &parent),
        (0o777, &parent),
        (0o775, &root),
        (0o702, &root),
    ] {
        set_mode(target, mode);
        let report = run_pass(&cfg);
        assert_eq!(report.refused_roots, 1, "mode {mode:o}: {report:?}");
        assert!(stale.join("data.bin").exists(), "mode {mode:o}");
        set_mode(target, 0o700);
    }

    // Sticky keeps others from renaming our entries: acceptable.
    set_mode(&parent, 0o1777);
    let report = run_pass(&cfg);
    assert_eq!(report.refused_roots, 0, "{report:?}");
    assert_eq!(report.reclaimed, 1, "{report:?}");
    set_mode(&parent, 0o700);
}

// F1: a symlink in place of a candidate is never followed.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[test]
fn symlinked_candidate_is_never_followed() {
    let fx = Fx::new();
    let outside = tempfile::tempdir().unwrap();
    let precious =
        create_scratch_dir_in(fx.registry(), outside.path(), "precious", RootKind::VarTmp).unwrap();
    fs::write(precious.join("data.bin"), b"x").unwrap();
    std::os::unix::fs::symlink(&precious, fx.root().join("rsi-link")).unwrap();

    let report = run_pass(&fx.config(RootKind::VarTmp, OLD));

    assert_eq!(report.reclaimed, 0, "{report:?}");
    assert!(precious.join("data.bin").exists());
}

fn mountinfo_with(dir: &Path, mount_points: &[&Path]) -> PathBuf {
    let mut text = String::from("1 0 8:1 / / rw - ext4 /dev/root rw\n");
    for (i, mount) in mount_points.iter().enumerate() {
        text.push_str(&format!(
            "{} 1 8:1 /elsewhere {} rw - ext4 /dev/root rw\n",
            100 + i,
            mount.display().to_string().replace(' ', "\\040")
        ));
    }
    let path = dir.join("mountinfo");
    fs::write(&path, text).unwrap();
    path
}

// F1: a mount at or under the candidate (a same-device bind included) is
// never traversed.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[test]
fn mount_at_or_under_a_candidate_is_refused() {
    let fx = Fx::new();
    let meta = tempfile::tempdir().unwrap();
    let under = fx.scratch("rsi-mount-under", RootKind::VarTmp);
    fs::create_dir_all(under.join("deep/inner")).unwrap();
    let at = fx.scratch("rsi-mount-at", RootKind::VarTmp);
    let clear = fx.scratch("rsi-mount-clear", RootKind::VarTmp);
    let sibling_mount = fx.root().join("rsi-mount-clear-but-not-under");
    let mut cfg = fx.config(RootKind::VarTmp, OLD);
    cfg.mountinfo = mountinfo_with(
        meta.path(),
        &[&under.join("deep/inner"), &at, &sibling_mount],
    );

    let report = run_pass(&cfg);

    assert_eq!(report.kept_unproven, 2, "{report:?}");
    assert_eq!(report.reclaimed, 1, "{report:?}");
    assert!(under.join("deep/inner").exists());
    assert!(at.join("data.bin").exists());
    assert!(!clear.exists());
}

// F1: no mount inventory, no deletion.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[test]
fn unreadable_mount_table_retains_everything() {
    let fx = Fx::new();
    let dir = fx.scratch("rsi-mount-unknown", RootKind::VarTmp);
    let mut cfg = fx.config(RootKind::VarTmp, OLD);
    cfg.mountinfo = fx.root().join("no-such-mountinfo");

    let report = run_pass(&cfg);

    assert_eq!(report.kept_unproven, 1, "{report:?}");
    assert!(dir.join("data.bin").exists());
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[test]
fn mountinfo_fields_are_unescaped_and_a_table_not_wholly_understood_is_refused() {
    let meta = tempfile::tempdir().unwrap();
    let info = mountinfo_with(meta.path(), &[Path::new("/var/tmp/has space")]);
    let mounts = fsys::read_mounts(&info).unwrap();
    assert!(fsys::mount_at_or_under(
        &mounts,
        Path::new("/var/tmp/has space")
    ));
    assert!(fsys::mount_at_or_under(&mounts, Path::new("/var/tmp")));
    assert!(!fsys::mount_at_or_under(&mounts, Path::new("/var/tmp/has")));

    // One line this parser does not understand makes the whole table unusable.
    let garbled = meta.path().join("garbled");
    fs::write(&garbled, "1 0 8:1 / / rw - ext4 /dev/root rw\nnonsense\n").unwrap();
    assert!(fsys::read_mounts(&garbled).is_none());
}

// A mount id the kernel cannot report is not a mount boundary we can rule out.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[test]
fn unknown_mount_ids_do_not_count_as_the_same_mount() {
    assert!(!fsys::same_known_mount(None, Some(1)));
    assert!(!fsys::same_known_mount(Some(1), None));
    assert!(!fsys::same_known_mount(Some(1), Some(2)));
    assert!(fsys::same_known_mount(Some(7), Some(7)));
}

// The age gate looks at the whole tree: a recent write deep down keeps a
// directory whose top level is old.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[test]
fn recent_deep_write_keeps_the_tree_young() {
    let fx = Fx::new();
    let busy = fx.scratch("rsi-busy-deep", RootKind::VarTmp);
    let quiet = fx.scratch("rsi-quiet-deep", RootKind::VarTmp);
    let old = SystemTime::now() - Duration::from_secs(7200);
    let age = |path: &Path| {
        fs::File::open(path).unwrap().set_modified(old).unwrap();
    };
    for dir in [&busy, &quiet] {
        fs::create_dir_all(dir.join("sub/deeper")).unwrap();
        fs::write(dir.join("sub/deeper/file"), b"x").unwrap();
    }
    fs::write(busy.join("sub/deeper/fresh"), b"just written").unwrap();
    for dir in [&busy, &quiet] {
        for rel in [
            "sub/deeper/file",
            "sub/deeper",
            "sub",
            "data.bin",
            RECORD_FILE,
            "",
        ] {
            age(&dir.join(rel));
        }
    }
    // The fresh file is the only thing written recently under `busy`.
    age(&busy.join("sub/deeper"));
    age(&busy.join("sub"));
    let cfg = fx.config(RootKind::VarTmp, Duration::from_secs(3600));

    let report = run(&cfg, SystemTime::now(), false);

    assert_eq!(report.kept_young, 1, "{report:?}");
    assert_eq!(report.reclaimed, 1, "{report:?}");
    assert!(busy.join("sub/deeper/fresh").exists());
    assert!(!quiet.exists());
}

// The shell helper workers use must write an allocation the pass accepts.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[test]
fn shell_helper_allocation_is_accepted_and_existing_dirs_are_refused() {
    let fx = Fx::new();
    let script = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../scripts/rsi-scratch-mkdir"
    );
    let mkdir = |kind: &str, name: &str| {
        Command::new(script)
            .args([kind, name])
            .env("RSI_SCRATCH_PARENT", fx.root())
            .env("RSI_SCRATCH_REGISTRY", fx.registry())
            .output()
            .unwrap()
    };
    let made = mkdir("worker", "w1");
    assert!(made.status.success(), "{made:?}");
    let dir = PathBuf::from(String::from_utf8(made.stdout).unwrap().trim());
    assert_eq!(dir, fx.root().join("rsi-w1-tmp"));
    fs::write(dir.join("data.bin"), b"x").unwrap();
    // Re-running for a directory the script made is a no-op; an existing
    // directory cannot be adopted.
    assert!(mkdir("worker", "w1").status.success());
    let legacy = legacy_dir(fx.root(), "rsi-legacy-tmp");
    assert!(!mkdir("worker", "legacy").status.success(), "adopted");

    let report = run_pass(&fx.config(RootKind::WorkerCache, OLD));

    assert_eq!(report.reclaimed, 1, "{report:?}");
    assert_eq!(report.kept_unrecorded, 1, "{report:?}");
    assert!(!dir.exists());
    assert!(legacy.join("data.bin").exists());
}

// Birth times keep their nanoseconds end to end: an inode reused within the
// same second is a different generation.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[test]
fn birth_time_is_compared_to_the_nanosecond() {
    let fx = Fx::new();
    let dir = fx.scratch("rsi-btime", RootKind::VarTmp);
    let entry = fx.entry_of(&dir);
    // The recorded value is the filesystem's full birth timestamp.
    let pinned = record::pin_dir(&dir).unwrap();
    assert_eq!(pinned.btime, Some(entry.btime));
    // Text round trip keeps every digit.
    let mut odd = entry.clone();
    odd.btime = 1_791_044_800_935_582_673;
    assert_eq!(Entry::parse(&odd.to_text()).unwrap().btime, odd.btime);
    // One nanosecond off is not the same directory generation.
    let parent = record::pin_dir(fx.root()).unwrap();
    assert!(entry.binds(&pinned, &parent, RootKind::VarTmp.tag(), current_uid()));
    let mut shifted = entry;
    shifted.btime += 1;
    assert!(!shifted.binds(&pinned, &parent, RootKind::VarTmp.tag(), current_uid()));
    let report = {
        fx.rewrite_entry(&dir, |e| e.btime += 1);
        run_pass(&fx.config(RootKind::VarTmp, OLD))
    };
    assert_eq!(report.kept_unrecorded, 1, "{report:?}");
    assert!(dir.join("data.bin").exists());
}

// R3 (delta): the registered clone's birth time is part of its identity: a
// different shared clone that reused the inode is not exempt.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[test]
fn clone_that_reused_the_registered_inode_is_not_exempt() {
    let fx = Fx::new();
    let origins = tempfile::tempdir().unwrap();
    let source = origins.path().join("source");
    published_repo(&source, origins.path());
    let workspace = lander_with_clone(&fx, "rsi-rolling-land-reuse", &source);
    fx.rewrite_entry(&workspace, |e| {
        if let Some(clone) = e.clone.as_mut() {
            clone.btime += 1;
        }
    });

    let report = run_pass(&fx.config(RootKind::LanderScratch, OLD));

    assert_eq!(report.reclaimed, 0, "{report:?}");
    assert_eq!(report.kept_dirty + report.kept_unpublished, 1, "{report:?}");
    assert!(workspace.join(LANDER_CLONE_DIR).exists());
}

fn nonce_of(dir: &Path) -> String {
    record::read_record(&record::pin_dir(dir).unwrap())
        .unwrap()
        .nonce
}

fn entries_below(dir: &Path) -> usize {
    fs::read_dir(dir).unwrap().count()
}

// R2 (delta): a persisted manifest that cannot be used is not "no manifest":
// the leftover is retained, never judged afresh and deleted.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[test]
fn unusable_persisted_manifest_retains_the_leftover() {
    for damage in [
        "garbage",
        "unreadable",
        "group-writable",
        "truncated",
        "unsorted",
    ] {
        let fx = Fx::new();
        let meta = tempfile::tempdir().unwrap();
        let origins = tempfile::tempdir().unwrap();
        let (cfg, leftover) = partial_leftover(&fx, meta.path(), origins.path());
        let manifest = fx
            .registry()
            .join(format!("{}.manifest", nonce_of(&leftover)));
        let before = entries_below(&leftover);
        match damage {
            "garbage" => fs::write(&manifest, b"not a manifest").unwrap(),
            "unreadable" => set_mode(&manifest, 0),
            "group-writable" => set_mode(&manifest, 0o660),
            "truncated" => {
                let mut data = fs::read(&manifest).unwrap();
                data.pop();
                fs::write(&manifest, data).unwrap();
            }
            _ => {
                let mut data = fs::read(&manifest).unwrap();
                let header = data.len() - 8 * ((data.len() - 8 - 24) / 8);
                // Swap the first two hashes so the list is no longer sorted.
                let (a, b) = (header, header + 8);
                for i in 0..8 {
                    data.swap(a + i, b + i);
                }
                fs::write(&manifest, data).unwrap();
            }
        }

        let report = unprivileged(|| run_pass(&cfg));

        assert_eq!(report.reclaimed, 0, "{damage}: {report:?}");
        assert_eq!(report.kept_unproven, 1, "{damage}: {report:?}");
        assert_eq!(entries_below(&leftover), before, "{damage}");
        set_mode(&manifest, 0o600);
    }
}

fn swap_sub_for_an_empty_directory(dir: std::os::fd::RawFd, name: &str) {
    if name != "sub" {
        return;
    }
    let base = format!("/proc/self/fd/{dir}");
    fs::rename(format!("{base}/sub"), format!("{base}/sub-moved")).unwrap();
    fs::create_dir(format!("{base}/sub")).unwrap();
}

// R8 (delta): a name that no longer holds the directory just emptied is not
// unlinked.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[test]
fn directory_replaced_before_its_unlink_is_not_deleted() {
    let fx = Fx::new();
    let dir = fx.scratch("rsi-s2a-replace", RootKind::VarTmp);
    fs::create_dir(dir.join("sub")).unwrap();
    fs::write(dir.join("sub/file.txt"), b"x").unwrap();
    fsys::BEFORE_DIR_UNLINK.with(|hook| hook.set(Some(swap_sub_for_an_empty_directory)));

    let report = run_pass(&fx.config(RootKind::VarTmp, OLD));
    fsys::BEFORE_DIR_UNLINK.with(|hook| hook.set(None));

    assert_eq!(report.kept_changed, 1, "{report:?}");
    assert_eq!(report.reclaimed, 0, "{report:?}");
    let leftovers = only_entries(fx.root());
    assert_eq!(leftovers.len(), 1, "{leftovers:?}");
    let aside = fx.root().join(&leftovers[0]);
    assert!(
        aside.join("sub").is_dir(),
        "the replacement directory survives"
    );
    assert!(aside.join("sub-moved").is_dir());
}

fn swap_the_aside_directory(aside: &Path) {
    let moved = aside.with_file_name("moved-away");
    fs::rename(aside, &moved).unwrap();
    fs::create_dir(aside).unwrap();
}

// R8 (delta): the final removal of the aside name is of the proved directory
// only.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[test]
fn aside_name_replaced_before_the_final_rmdir_is_not_deleted() {
    let fx = Fx::new();
    fx.scratch("rsi-s2a-final", RootKind::VarTmp);
    let mut cfg = fx.config(RootKind::VarTmp, OLD);
    cfg.before_final_rmdir = Some(swap_the_aside_directory);

    let report = run_pass(&cfg);

    assert_eq!(report.kept_changed, 1, "{report:?}");
    assert_eq!(report.reclaimed, 0, "{report:?}");
    let names = only_entries(fx.root());
    assert_eq!(names.len(), 2, "{names:?}");
    assert!(names.contains(&"moved-away".to_string()), "{names:?}");
    let replacement = names.iter().find(|n| n.starts_with(ASIDE_PREFIX)).unwrap();
    assert!(fx.root().join(replacement).is_dir());
}

/// A registry entry with no directory behind it.
fn bare_entry(ino: u64) -> Entry {
    Entry {
        nonce: registry::new_nonce(),
        kind: RootKind::VarTmp.tag().to_string(),
        uid: current_uid(),
        ident: fsys::Ident { dev: 1, ino },
        btime: 1,
        parent: fsys::Ident { dev: 1, ino: 2 },
        created_unix: 0,
        path: "/nowhere".to_string(),
        source: None,
        clone: None,
    }
}

fn tombstone_file(nonce: &str) -> String {
    format!("{nonce}.reclaimed")
}

// #1171: pruning is decided by the registry's own durable record, never by what
// the filesystem shows: only a tombstoned allocation loses its registry files.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[test]
fn registry_prune_drops_only_tombstoned_allocations() {
    let fx = Fx::new();
    let names = [
        "rsi-p-live",
        "rsi-p-hand",
        "rsi-p-crash",
        "rsi-p-done",
        "rsi-p-mismatch",
        "rsi-p-foreign",
        "rsi-p-garbage",
    ];
    let dirs: Vec<PathBuf> = names
        .iter()
        .map(|name| fx.scratch(name, RootKind::VarTmp))
        .collect();
    let [live, by_hand, crashed, done, mismatched, foreign, _] = &dirs[..] else {
        unreachable!()
    };
    let nonces: Vec<String> = dirs.iter().map(|dir| nonce_of(dir)).collect();
    // Old, as the identity-proof prune demanded: age proves nothing here.
    for dir in &dirs {
        fx.rewrite_entry(dir, |e| e.created_unix = 0);
    }
    let registry = Registry::open(fx.registry(), current_uid()).unwrap();

    // Removed behind the daemon's back: no tombstone, no prune.
    fs::remove_dir_all(by_hand).unwrap();
    // The crash window: a reclaim published its manifest and removed the tree
    // but wrote no tombstone.
    registry.manifest_put(&nonces[2], &[1, 2, 3]).unwrap();
    fs::remove_dir_all(crashed).unwrap();
    // Finished by a reclaim: entry, manifest and tombstone. The tree is still
    // there with content, and prune must not touch it.
    registry.manifest_put(&nonces[3], &[1]).unwrap();
    registry.tombstone_put(&fx.entry_of(done)).unwrap();
    // A tombstone for some other identity does not drop the entry it names.
    let mut other = fx.entry_of(mismatched);
    other.ident.ino ^= 1;
    registry.tombstone_put(&other).unwrap();
    // A tombstone others could write is not ours.
    registry.tombstone_put(&fx.entry_of(foreign)).unwrap();
    set_mode(&fx.registry().join(tombstone_file(&nonces[5])), 0o666);
    // A tombstone that is not one.
    fs::write(
        fx.registry().join(tombstone_file(&nonces[6])),
        b"not a tombstone",
    )
    .unwrap();
    // A tombstone whose entry is already gone.
    let orphan = bare_entry(9);
    registry.tombstone_put(&orphan).unwrap();
    let before = only_entries(done);

    let pruned = prune_expecting(&registry, 2);

    let remaining = only_entries(fx.registry());
    assert_eq!(pruned, 2, "{remaining:?}");
    for kept in [0, 1, 2, 4, 5, 6] {
        assert!(remaining.contains(&nonces[kept]), "{kept}: {remaining:?}");
    }
    assert!(remaining.contains(&format!("{}.manifest", nonces[2])));
    for kept in [4, 5, 6] {
        let name = tombstone_file(&nonces[kept]);
        assert!(remaining.contains(&name), "{kept}: {remaining:?}");
    }
    for dropped in [
        nonces[3].clone(),
        format!("{}.manifest", nonces[3]),
        tombstone_file(&nonces[3]),
        tombstone_file(&orphan.nonce),
    ] {
        assert!(!remaining.contains(&dropped), "{dropped}: {remaining:?}");
    }
    // No tree was opened for writing: the tombstoned one is intact.
    assert_eq!(only_entries(done), before);
    assert_eq!(fs::read(done.join("data.bin")).unwrap(), vec![7u8; 8192]);
    assert!(live.join("data.bin").is_file());
    // A live allocation binds; a tombstoned one never does again.
    assert!(registry.get(&nonces[0]).is_some());
    assert!(registry.get(&nonces[4]).is_none());
}

// #1171: plain entries (which are never pruned) must not crowd tombstones out
// of the per-call limit.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[test]
fn registry_prune_reaches_a_tombstone_past_many_live_entries() {
    let fx = Fx::new();
    let registry = Registry::open_or_create(fx.registry(), current_uid()).unwrap();
    for ino in 0..64 {
        registry.put_new(&bare_entry(ino)).unwrap();
    }
    let done = bare_entry(1000);
    registry.put_new(&done).unwrap();
    registry.tombstone_put(&done).unwrap();

    let pruned = eventually(|| {
        let mut budget = Budget::new(Instant::now() + Duration::from_secs(30), 100_000);
        let pruned = registry.prune(1, &mut budget);
        (pruned == 1).then_some(pruned)
    });

    assert_eq!(pruned, Some(1));
    assert!(!only_entries(fx.registry()).contains(&done.nonce));
    assert_eq!(only_entries(fx.registry()).len(), 64);
}

/// Retry `attempt` for a few seconds: the custody lock is an `flock` on a
/// descriptor, and every process another test of this binary forks has a copy
/// of every open descriptor until it execs, so a lock the test just released is
/// briefly still held by that child. A hold that must be *refused* is asserted
/// at once; only "becomes available" is awaited (#1165).
fn eventually<T>(mut attempt: impl FnMut() -> Option<T>) -> Option<T> {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Some(value) = attempt() {
            return Some(value);
        }
        if Instant::now() >= deadline {
            return None;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// Prune until it drops `expected` entries (a pass that could not take the
/// custody lock prunes nothing and has no other effect), and return the last
/// count.
fn prune_expecting(registry: &Registry, expected: usize) -> usize {
    let mut last = 0;
    eventually(|| {
        last = prune_everything(registry);
        (last == expected).then_some(())
    });
    last
}

/// Run passes until `done` holds for the report (a pass that finds the custody
/// lock still held by a forked child defers its candidate and changes nothing
/// else, see [`eventually`]); the last report.
fn run_pass_until(cfg: &ScratchConfig, done: impl Fn(&ScratchReport) -> bool) -> ScratchReport {
    let mut report = run_pass(cfg);
    eventually(|| {
        if done(&report) {
            return Some(());
        }
        report = run_pass(cfg);
        None
    });
    report
}

fn prune_everything(registry: &Registry) -> usize {
    let mut budget = Budget::new(Instant::now() + Duration::from_secs(30), 100_000);
    registry.prune(100, &mut budget)
}

// #1171: a prune never runs while a reclaim holds the custody lock, so the
// registry files of a reclaim in flight cannot be removed from under it.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[test]
fn registry_prune_is_serialized_with_a_reclaim_in_flight() {
    let fx = Fx::new();
    let dir = fx.scratch("rsi-f-flight", RootKind::VarTmp);
    let nonce = nonce_of(&dir);
    let registry = Registry::open(fx.registry(), current_uid()).unwrap();
    registry.manifest_put(&nonce, &[1, 2, 3]).unwrap();
    registry.tombstone_put(&fx.entry_of(&dir)).unwrap();

    let reclaim = registry.reclaim_guard().unwrap().unwrap();
    assert_eq!(prune_everything(&registry), 0, "prune ran under a reclaim");
    let held = only_entries(fx.registry());
    assert!(held.contains(&nonce), "{held:?}");
    assert!(held.contains(&format!("{nonce}.manifest")), "{held:?}");
    drop(reclaim);

    assert_eq!(prune_expecting(&registry, 1), 1);
    assert_eq!(only_entries(fx.registry()), Vec::<String>::new());
}

// #1152: reclaims share the custody lock; a prune excludes them.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[test]
fn registry_custody_lock_is_shared_by_reclaims_and_exclusive_for_prune() {
    let fx = Fx::new();
    let registry = Registry::open(fx.registry(), current_uid()).unwrap();

    let first = registry.custody(false).unwrap();
    let second = registry.custody(false).unwrap();
    assert!(first.is_some() && second.is_some(), "reclaims must share");
    assert!(registry.custody(true).unwrap().is_none());
    drop((first, second));

    let prune = eventually(|| registry.custody(true).unwrap());
    assert!(prune.is_some());
    assert!(registry.custody(false).unwrap().is_none());
    assert!(registry.custody(true).unwrap().is_none());
    drop(prune);
    assert!(eventually(|| registry.custody(false).unwrap()).is_some());
}

thread_local! {
    static PRUNE_BLOCKED_DURING_RECLAIM: std::cell::RefCell<Vec<(PathBuf, bool)>> =
        const { std::cell::RefCell::new(Vec::new()) };
}

/// `after_rename` hook: the reclaim is mid-flight; may a prune start?
fn probe_prune_lock(_aside: &Path) {
    PRUNE_BLOCKED_DURING_RECLAIM.with(|seen| {
        let (registry_path, _) = seen.borrow()[0].clone();
        let registry = Registry::open(&registry_path, current_uid()).unwrap();
        let blocked = registry.custody(true).unwrap().is_none();
        seen.borrow_mut().push((registry_path, blocked));
    });
}

// #1152: a real reclaim holds the custody lock from its rename aside on.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[test]
fn a_reclaim_holds_the_custody_lock_while_it_works() {
    let fx = Fx::new();
    fx.scratch("rsi-h-lock", RootKind::VarTmp);
    let mut cfg = fx.config(RootKind::VarTmp, OLD);
    cfg.after_rename = Some(probe_prune_lock);
    PRUNE_BLOCKED_DURING_RECLAIM.with(|seen| {
        *seen.borrow_mut() = vec![(fx.registry().to_path_buf(), false)];
    });

    let report = run_pass(&cfg);

    assert_eq!(report.reclaimed, 1, "{report:?}");
    let seen = PRUNE_BLOCKED_DURING_RECLAIM.with(|seen| seen.borrow().clone());
    assert_eq!(seen.len(), 2, "hook did not run: {seen:?}");
    assert!(seen[1].1, "a prune could start mid-reclaim");
    // And the lock is free again afterwards.
    let registry = Registry::open(fx.registry(), current_uid()).unwrap();
    assert!(eventually(|| registry.custody(true).unwrap()).is_some());
}

// #1171: a reclaim that completes leaves nothing behind in the registry, and
// a pass prunes only what a reclaim of this daemon finished: an interrupted
// finish (stopped right after the tombstone) is completed by the next pass, while an allocation removed behind the daemon's back and one
// whose reclaim stopped before the tombstone keep their entries.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[test]
fn a_pass_prunes_only_what_a_reclaim_finished() {
    let fx = Fx::new();
    let interrupted = fx.scratch("rsi-q-done", RootKind::VarTmp);
    let by_hand = fx.scratch("rsi-q-hand", RootKind::VarTmp);
    let crashed = fx.scratch("rsi-q-crash", RootKind::VarTmp);
    let (done_nonce, hand_nonce, crash_nonce) = (
        nonce_of(&interrupted),
        nonce_of(&by_hand),
        nonce_of(&crashed),
    );
    let registry = Registry::open(fx.registry(), current_uid()).unwrap();
    fs::remove_dir_all(&by_hand).unwrap();
    registry.manifest_put(&crash_nonce, &[1, 2]).unwrap();
    fs::remove_dir_all(&crashed).unwrap();
    // The registry is on by default, in production and in the fixture.
    let mut cfg = fx.config(RootKind::VarTmp, OLD);
    assert!(cfg.registry_prune);
    assert!(ScratchConfig::standard().registry_prune);

    // The reclaim got as far as the tombstone, then was interrupted.
    cfg.registry_prune = false;
    cfg.stop_after_tombstone = true;
    let report = run_pass(&cfg);
    assert_eq!(report.reclaimed, 1, "{report:?}");
    assert!(!interrupted.exists());
    let names = only_entries(fx.registry());
    assert!(names.contains(&done_nonce), "{names:?}");
    assert!(names.contains(&tombstone_file(&done_nonce)), "{names:?}");

    // The next pass (pruning, as in production) finishes the removal.
    cfg.registry_prune = true;
    cfg.stop_after_tombstone = false;
    run_pass_until(&cfg, |_| {
        !only_entries(fx.registry()).contains(&tombstone_file(&done_nonce))
    });
    let names = only_entries(fx.registry());
    assert!(!names.contains(&done_nonce), "{names:?}");
    assert!(!names.contains(&tombstone_file(&done_nonce)), "{names:?}");
    // Nothing else was dropped.
    assert!(names.contains(&hand_nonce), "{names:?}");
    assert!(names.contains(&crash_nonce), "{names:?}");
    assert!(
        names.contains(&format!("{crash_nonce}.manifest")),
        "{names:?}"
    );
}

fn add_a_file_after_the_proof(aside: &Path) {
    fs::write(aside.join("late.bin"), b"late").unwrap();
}

// #1171: a reclaim writes its tombstone only after the contents and provenance
// are removed; one that stopped earlier leaves the entry (and no tombstone).
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[test]
fn a_reclaim_that_stops_before_the_removal_is_done_leaves_no_tombstone() {
    let fx = Fx::new();
    let dir = fx.scratch("rsi-t-late", RootKind::VarTmp);
    let nonce = nonce_of(&dir);
    let mut cfg = fx.config(RootKind::VarTmp, OLD);
    cfg.after_proof = Some(add_a_file_after_the_proof);

    let report = run_pass(&cfg);

    assert_eq!(report.kept_changed, 1, "{report:?}");
    let names = only_entries(fx.registry());
    assert!(names.contains(&nonce), "{names:?}");
    assert!(!names.contains(&tombstone_file(&nonce)), "{names:?}");
}

// #1171: a reclaim that completes removes every registry file of the
// allocation (entry, manifest, tombstone).
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[test]
fn a_completed_reclaim_leaves_nothing_in_the_registry() {
    let fx = Fx::new();
    let dir = fx.scratch("rsi-t-clean", RootKind::VarTmp);
    // The reclaim itself cleans up; a prune is not what removes these.
    let mut cfg = fx.config(RootKind::VarTmp, OLD);
    cfg.registry_prune = false;

    let report = run_pass(&cfg);

    assert_eq!(report.reclaimed, 1, "{report:?}");
    assert!(!dir.exists());
    assert_eq!(only_entries(fx.registry()), Vec::<String>::new());
}

// #1171: a tombstoned allocation never binds again, even if its directory and
// record are still there.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[test]
fn a_tombstoned_allocation_is_never_reclaimed_again() {
    let fx = Fx::new();
    let dir = fx.scratch("rsi-t-bound", RootKind::VarTmp);
    let registry = Registry::open(fx.registry(), current_uid()).unwrap();
    registry.tombstone_put(&fx.entry_of(&dir)).unwrap();
    let mut cfg = fx.config(RootKind::VarTmp, OLD);
    cfg.registry_prune = false;

    let report = run_pass(&cfg);

    assert_eq!(report.reclaimed, 0, "{report:?}");
    assert!(dir.join("data.bin").is_file());
}

// #1152 review: a reclaim never waits for a prune; the candidate is deferred.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[test]
fn a_reclaim_defers_promptly_while_a_prune_holds_the_registry() {
    let fx = Fx::new();
    let dir = fx.scratch("rsi-d-defer", RootKind::VarTmp);
    let mut cfg = fx.config(RootKind::VarTmp, OLD);
    // No host process can make the holder proof inconclusive: the deferral is
    // the only reason this pass can keep the directory.
    cfg.proc_root = fx.hermetic_proc();
    let registry = Registry::open(fx.registry(), current_uid()).unwrap();
    let prune = registry.custody(true).unwrap().unwrap();

    let (tx, rx) = std::sync::mpsc::channel();
    let worker = {
        let cfg = cfg.clone();
        std::thread::spawn(move || tx.send(run_pass(&cfg)).unwrap())
    };
    let waited = rx.recv_timeout(Duration::from_secs(20));
    drop(prune);
    worker.join().unwrap();

    let report = waited.expect("the reclaim blocked on the prune lock");
    assert_eq!(report.kept_held, 1, "{report:?}");
    assert_eq!(report.reclaimed, 0, "{report:?}");
    assert!(dir.is_dir());
    // Deferred, not lost: the next pass reclaims it.
    let report = run_pass_until(&cfg, |report| report.reclaimed == 1);
    assert_eq!(report.reclaimed, 1, "{report:?}");
    assert!(!dir.exists());
}

// #1171 review: a directory kept because late contents arrived before the
// final rmdir is never tombstoned, so a prune cannot drop its custody entry.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[test]
fn a_directory_kept_for_late_contents_is_never_tombstoned() {
    let fx = Fx::new();
    let dir = fx.scratch("rsi-t-rmdir", RootKind::VarTmp);
    let nonce = nonce_of(&dir);
    let mut cfg = fx.config(RootKind::VarTmp, OLD);
    cfg.before_final_rmdir = Some(add_a_file_after_the_proof);

    let report = run_pass(&cfg);
    let again = run_pass(&cfg);

    assert_eq!(report.kept_changed, 1, "{report:?}");
    assert_eq!(again.reclaimed, 0, "{again:?}");
    let names = only_entries(fx.registry());
    assert!(names.contains(&nonce), "{names:?}");
    assert!(!names.contains(&tombstone_file(&nonce)), "{names:?}");
    let aside = only_entries(fx.root());
    assert_eq!(aside.len(), 1, "{aside:?}");
    assert!(fx.root().join(&aside[0]).join("late.bin").is_file());
}

fn fail_sync_number(n: usize) {
    fsys::SYNC_CALLS.with(|c| c.set(0));
    fsys::FAIL_SYNC_AT.with(|c| c.set(n));
}

// #1171 review: the deletions are made durable (scratch directory, then its
// parent) before a tombstone exists; a failed sync stops with no tombstone.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[test]
fn a_failed_directory_sync_publishes_no_tombstone() {
    for failing in [1, 2] {
        let fx = Fx::new();
        let dir = fx.scratch("rsi-t-sync", RootKind::VarTmp);
        let nonce = nonce_of(&dir);
        let cfg = fx.config(RootKind::VarTmp, OLD);

        fail_sync_number(failing);
        let report = run_pass(&cfg);
        fail_sync_number(0);

        assert_eq!(report.failed, 1, "sync {failing}: {report:?}");
        let names = only_entries(fx.registry());
        assert!(names.contains(&nonce), "sync {failing}: {names:?}");
        assert!(
            !names.contains(&tombstone_file(&nonce)),
            "sync {failing}: {names:?}"
        );
    }
}

// #1171 review: the tombstone outlives the entry and manifest removals until
// those are durable; when the registry sync fails it is kept for a prune.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[test]
fn the_tombstone_is_kept_until_the_registry_removals_are_durable() {
    let fx = Fx::new();
    let dir = fx.scratch("rsi-t-regsync", RootKind::VarTmp);
    let nonce = nonce_of(&dir);
    let mut cfg = fx.config(RootKind::VarTmp, OLD);
    cfg.registry_prune = false;

    // Syncs: scratch dir (1), its parent (2), then the registry's.
    fail_sync_number(3);
    let report = run_pass(&cfg);
    fail_sync_number(0);

    assert_eq!(report.reclaimed, 1, "{report:?}");
    assert_eq!(only_entries(fx.registry()), vec![tombstone_file(&nonce)]);
    // A later prune completes it.
    let registry = Registry::open(fx.registry(), current_uid()).unwrap();
    assert_eq!(prune_expecting(&registry, 1), 1);
    assert_eq!(only_entries(fx.registry()), Vec::<String>::new());
}

// #1171 review: an entry whose identity cannot be checked against its tombstone
// is kept with its manifest and tombstone; only agreement or proven absence
// drops it.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[test]
fn registry_prune_keeps_an_entry_it_cannot_check_against_the_tombstone() {
    let fx = Fx::new();
    let registry = Registry::open_or_create(fx.registry(), current_uid()).unwrap();
    // Entry-only birth-time mismatch (same device and inode).
    let reborn = bare_entry(1);
    registry.put_new(&reborn).unwrap();
    let mut other = reborn.clone();
    other.btime += 1;
    registry.tombstone_put(&other).unwrap();
    // An entry that cannot be read.
    let unreadable = bare_entry(2);
    registry.put_new(&unreadable).unwrap();
    registry.manifest_put(&unreadable.nonce, &[1]).unwrap();
    registry.tombstone_put(&unreadable).unwrap();
    fs::write(
        fx.registry().join(&unreadable.nonce),
        b"\xff\xfe not an entry",
    )
    .unwrap();

    assert_eq!(prune_everything(&registry), 0);
    let kept = only_entries(fx.registry());
    for nonce in [&reborn.nonce, &unreadable.nonce] {
        assert!(kept.contains(nonce), "{kept:?}");
        assert!(kept.contains(&tombstone_file(nonce)), "{kept:?}");
    }
    assert!(kept.contains(&format!("{}.manifest", unreadable.nonce)));

    // Repaired to agree, the entry is dropped.
    registry.replace(&other).unwrap();
    assert_eq!(prune_expecting(&registry, 1), 1);
    let kept = only_entries(fx.registry());
    assert!(!kept.contains(&reborn.nonce), "{kept:?}");
    assert!(kept.contains(&unreadable.nonce), "{kept:?}");
}

// #1171 review: a nonce a reclaim tombstoned is not reused by a new entry, so
// a finish left half done cannot drop an entry created afterwards.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[test]
fn a_tombstoned_nonce_is_never_registered_again() {
    let fx = Fx::new();
    let registry = Registry::open_or_create(fx.registry(), current_uid()).unwrap();
    let done = bare_entry(5);
    registry.put_new(&done).unwrap();
    registry.tombstone_put(&done).unwrap();
    // A partial finish: the entry is gone, the tombstone remains.
    registry.remove_entry(&done.nonce);

    let error = registry.put_new(&done).unwrap_err();

    assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);
    assert_eq!(
        only_entries(fx.registry()),
        vec![tombstone_file(&done.nonce)]
    );
    // Once the finish completes the nonce is free again.
    assert_eq!(prune_expecting(&registry, 1), 1);
    registry.put_new(&done).unwrap();
}

// #1171 review: the per-call limit applies to tombstones only, whatever order
// the directory lists them in.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[test]
fn tombstone_selection_is_not_crowded_out_by_live_entries() {
    let mut names: Vec<String> = (0..64).map(|_| registry::new_nonce()).collect();
    let done = registry::new_nonce();
    names.push(format!("{done}.reclaimed"));
    names.push(format!("{}.manifest", registry::new_nonce()));

    assert_eq!(registry::tombstone_candidates(&names, 1), vec![done]);
    assert!(registry::tombstone_candidates(&names, 0).is_empty());
}

fn move_the_allocation_and_install_an_empty_directory(parent: std::os::fd::RawFd, name: &str) {
    if !name.starts_with(ASIDE_PREFIX) {
        return;
    }
    let base = format!("/proc/self/fd/{parent}");
    fs::rename(format!("{base}/{name}"), format!("{base}/moved-away")).unwrap();
    fs::create_dir(format!("{base}/{name}")).unwrap();
}

// #1171 delta review: the final rmdir removes whatever directory has the name,
// so a successful one proves the allocation gone only if its own inode has no
// links left. Moved aside and replaced by an empty directory between the
// identity stat and the rmdir, the allocation is retained with its entry.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[test]
fn a_moved_allocation_whose_name_was_removed_is_not_tombstoned() {
    let fx = Fx::new();
    let dir = fx.scratch("rsi-t-moved", RootKind::VarTmp);
    let nonce = nonce_of(&dir);
    let cfg = fx.config(RootKind::VarTmp, OLD);
    fsys::BEFORE_ROOT_RMDIR.with(|hook| {
        hook.set(Some(move_the_allocation_and_install_an_empty_directory));
    });

    let report = run_pass(&cfg);
    fsys::BEFORE_ROOT_RMDIR.with(|hook| hook.set(None));
    let again = run_pass(&cfg);

    assert_eq!(report.kept_changed, 1, "{report:?}");
    assert_eq!(report.reclaimed, 0, "{report:?}");
    assert_eq!(again.reclaimed, 0, "{again:?}");
    let names = only_entries(fx.registry());
    assert!(names.contains(&nonce), "{names:?}");
    assert!(!names.contains(&tombstone_file(&nonce)), "{names:?}");
    assert!(fx.root().join("moved-away").is_dir());
}

fn tombstone_after_the_create(registry: &Registry, entry: &Entry) {
    registry.tombstone_put(entry).unwrap();
}

// #1171 delta review: a tombstone that appears between put_new's check and its
// create leaves no entry behind.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[test]
fn put_new_withdraws_an_entry_whose_nonce_was_tombstoned_meanwhile() {
    let fx = Fx::new();
    let registry = Registry::open_or_create(fx.registry(), current_uid()).unwrap();
    let entry = bare_entry(3);
    registry::AFTER_PUT_NEW.with(|hook| hook.set(Some(tombstone_after_the_create)));

    let error = registry.put_new(&entry).unwrap_err();
    registry::AFTER_PUT_NEW.with(|hook| hook.set(None));

    assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);
    assert_eq!(
        only_entries(fx.registry()),
        vec![tombstone_file(&entry.nonce)]
    );
}

thread_local! {
    static RENAME_ROOT: std::cell::RefCell<PathBuf> = std::cell::RefCell::new(PathBuf::new());
}

fn rename_in_root(from: &str, to: &str) {
    RENAME_ROOT.with(|root| {
        let root = root.borrow();
        fs::rename(root.join(from), root.join(to)).unwrap();
    });
}

fn fx_inventory(
    root: &Path,
    budget: &mut Budget,
) -> Result<Vec<fsys::Named>, fsys::InventoryError> {
    let pinned = fsys::open_root(root, current_uid()).unwrap();
    fsys::named_inventory(&pinned, budget)
}

fn roomy_budget() -> Budget {
    Budget::new(Instant::now() + Duration::from_secs(30), 100_000)
}

fn rename_a_listed_name(call: usize) {
    if call == 1 {
        rename_in_root("rsi-q-a", "rsi-q-b");
    }
}

// #1152 review: the inventory itself reports a name that vanished between the
// listing and its stat as `Mutated`, rather than skipping it.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[test]
fn inventory_reports_a_vanished_name_as_mutated() {
    let fx = Fx::new();
    fs::create_dir(fx.root().join("rsi-q-a")).unwrap();
    RENAME_ROOT.with(|root| *root.borrow_mut() = fx.root().to_path_buf());
    fsys::LISTINGS.with(|calls| calls.set(0));
    fsys::AFTER_LISTING.with(|hook| hook.set(Some(rename_a_listed_name)));

    let result = fx_inventory(fx.root(), &mut roomy_budget());
    fsys::AFTER_LISTING.with(|hook| hook.set(None));

    assert_eq!(result.unwrap_err(), fsys::InventoryError::Mutated);
}

thread_local! {
    static HIDDEN_INO: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

/// Before each name is stat'ed, make sure it holds the unrelated directory:
/// the allocation is exchanged out from under the scan, back and forth.
fn exchange_the_allocation_away(name: &str) {
    use std::os::unix::fs::MetadataExt;
    RENAME_ROOT.with(|root| {
        let root = root.borrow();
        let holds_it = fs::symlink_metadata(root.join(name))
            .is_ok_and(|m| m.ino() == HIDDEN_INO.with(std::cell::Cell::get));
        if !holds_it {
            return;
        }
        let other = if name == "rsi-x-a" {
            "rsi-x-b"
        } else {
            "rsi-x-a"
        };
        nix::fcntl::renameat2(
            None,
            &root.join(name),
            None,
            &root.join(other),
            nix::fcntl::RenameFlags::RENAME_EXCHANGE,
        )
        .unwrap();
    });
}

// #1152 review: two names cannot be one directory. An exchange schedule that
// makes every stat see the unrelated directory (so both inventories agree and
// hide the allocation) shows the same directory twice and is rejected.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[test]
fn inventory_rejects_one_directory_seen_under_two_names() {
    use std::os::unix::fs::MetadataExt;
    let fx = Fx::new();
    let live = fx.root().join("rsi-x-a");
    fs::create_dir(&live).unwrap();
    fs::create_dir(fx.root().join("rsi-x-b")).unwrap();
    HIDDEN_INO.with(|ino| ino.set(fs::metadata(&live).unwrap().ino()));
    RENAME_ROOT.with(|root| *root.borrow_mut() = fx.root().to_path_buf());
    fsys::BEFORE_STAT.with(|hook| hook.set(Some(exchange_the_allocation_away)));

    let result = fx_inventory(fx.root(), &mut roomy_budget());
    fsys::BEFORE_STAT.with(|hook| hook.set(None));

    assert_eq!(result.unwrap_err(), fsys::InventoryError::Mutated);
}

fn flip_the_device(ident: &mut fsys::Ident) {
    ident.dev ^= 1;
}

// #1152 review: the two stats of a name must agree on the complete device and
// inode, not the inode number alone.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[test]
fn inventory_rejects_a_name_whose_stats_disagree_on_the_device() {
    let fx = Fx::new();
    fs::create_dir(fx.root().join("rsi-dev")).unwrap();
    fsys::AFTER_FSTATAT.with(|hook| hook.set(Some(flip_the_device)));

    let result = fx_inventory(fx.root(), &mut roomy_budget());
    fsys::AFTER_FSTATAT.with(|hook| hook.set(None));

    assert_eq!(result.unwrap_err(), fsys::InventoryError::Mutated);
}

thread_local! {
    static STATTED: std::cell::RefCell<Vec<String>> = const { std::cell::RefCell::new(Vec::new()) };
}

fn slow_first_stat(name: &str) {
    STATTED.with(|seen| seen.borrow_mut().push(name.to_string()));
    if STATTED.with(|seen| seen.borrow().len()) == 1 {
        std::thread::sleep(Duration::from_millis(400));
    }
}

// #1152 review: the stat phase honours the pass deadline: once time is up no
// later name is examined and the inventory is `Unproven`.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[test]
fn inventory_stops_statting_when_the_deadline_passes() {
    let fx = Fx::new();
    for name in ["rsi-t-1", "rsi-t-2", "rsi-t-3"] {
        fs::create_dir(fx.root().join(name)).unwrap();
    }
    STATTED.with(|seen| seen.borrow_mut().clear());
    fsys::BEFORE_STAT.with(|hook| hook.set(Some(slow_first_stat)));
    let mut budget = Budget::new(Instant::now() + Duration::from_millis(200), 100_000);

    let result = fx_inventory(fx.root(), &mut budget);
    fsys::BEFORE_STAT.with(|hook| hook.set(None));

    assert_eq!(result.unwrap_err(), fsys::InventoryError::Unproven);
    assert_eq!(
        STATTED.with(|seen| seen.borrow().len()),
        1,
        "later names were examined"
    );
}

// ---- operator adoption of legacy scratch (#1147) ------------------------------

const TEN_DAYS: Duration = Duration::from_secs(10 * 24 * 3600);

/// Make `dir` and every file directly in it look last written ten days ago.
fn make_old(dir: &Path) {
    let old = SystemTime::now() - TEN_DAYS;
    for entry in fs::read_dir(dir).unwrap() {
        fs::File::open(entry.unwrap().path())
            .unwrap()
            .set_modified(old)
            .unwrap();
    }
    fs::File::open(dir).unwrap().set_modified(old).unwrap();
}

fn adopt_one_path(fx: &Fx, kind: RootKind, path: &Path) -> AdoptOutcome {
    let mut outcomes = adopt_legacy(
        &fx.config(kind, MIN_AGE),
        SystemTime::now(),
        &[path.to_path_buf()],
    );
    assert_eq!(outcomes.len(), 1);
    outcomes.remove(0)
}

fn spawn_holder_in(dir: &Path) -> std::process::Child {
    Command::new("sleep")
        .arg("60")
        .current_dir(dir)
        .spawn()
        .unwrap()
}

// The adopted directory is recorded, still on disk (adoption deletes nothing),
// and the next reclaim pass lists it as a candidate: the record write did not
// restart its age clock.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[test]
fn adopting_a_legacy_dir_records_it_and_reclaim_then_lists_it() {
    let fx = Fx::new();
    let legacy = legacy_dir(fx.root(), "rsi-legacy-old");
    make_old(&legacy);
    let config = fx.config(RootKind::VarTmp, MIN_AGE);

    let before = run(&config, SystemTime::now(), true);
    assert_eq!(before.kept_unrecorded, 1, "{before:?}");
    assert_eq!(before.reclaimed, 0);

    let listing = list_legacy(&config, SystemTime::now());
    assert_eq!(listing.candidates.len(), 1, "{listing:?}");
    assert_eq!(listing.candidates[0].path, legacy);
    assert_eq!(listing.candidates[0].kind, "var_tmp");
    assert_eq!(listing.candidates[0].blocker, None);

    let outcome = adopt_one_path(&fx, RootKind::VarTmp, &legacy);
    assert!(outcome.adopted, "{outcome:?}");
    assert_eq!(outcome.refusal, None);

    assert!(legacy.join(RECORD_FILE).is_file());
    assert!(legacy.join("data.bin").is_file(), "adopt deletes nothing");
    let after = run(&config, SystemTime::now(), true);
    assert_eq!(after.kept_unrecorded, 0, "{after:?}");
    assert_eq!(after.reclaimed, 1, "{after:?}");
    assert_eq!(after.entries[0].path, legacy);
    assert_eq!(after.entries[0].decision, Decision::Reclaim);
    // Adopted directories are no longer listed.
    assert!(
        list_legacy(&config, SystemTime::now())
            .candidates
            .is_empty()
    );
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[test]
fn adopt_refuses_a_dir_with_a_live_holder() {
    let fx = Fx::new();
    let legacy = legacy_dir(fx.root(), "rsi-legacy-held");
    make_old(&legacy);
    let mut holder = spawn_holder_in(&legacy);
    fx.watch(&holder);

    let listing = list_legacy(&fx.config(RootKind::VarTmp, MIN_AGE), SystemTime::now());
    let outcome = adopt_one_path(&fx, RootKind::VarTmp, &legacy);
    holder.kill().unwrap();
    holder.wait().unwrap();

    assert_eq!(listing.candidates[0].blocker, Some(AdoptRefusal::Held));
    assert!(!outcome.adopted);
    assert_eq!(outcome.refusal, Some(AdoptRefusal::Held));
    assert!(!legacy.join(RECORD_FILE).exists());
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[test]
fn adopt_refuses_a_symlink_and_records_nothing() {
    let fx = Fx::new();
    let target = legacy_dir(fx.root(), "rsi-legacy-target");
    make_old(&target);
    let link = fx.root().join("rsi-legacy-link");
    std::os::unix::fs::symlink(&target, &link).unwrap();

    let outcome = adopt_one_path(&fx, RootKind::VarTmp, &link);

    assert_eq!(outcome.refusal, Some(AdoptRefusal::Symlink), "{outcome:?}");
    assert!(!target.join(RECORD_FILE).exists());
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[test]
fn adopt_refuses_paths_outside_the_scratch_roots() {
    let fx = Fx::new();
    let elsewhere = tempfile::tempdir().unwrap();
    let outside = legacy_dir(elsewhere.path(), "rsi-outside");
    make_old(&outside);
    let unlisted_name = legacy_dir(fx.root(), "precious-data");
    make_old(&unlisted_name);
    let nested = legacy_dir(&fx.root().join("rsi-parent"), "rsi-nested");
    make_old(&nested);
    let traversal = fx.root().join("rsi-x").join("..").join("rsi-legacy-t");
    legacy_dir(fx.root(), "rsi-legacy-t");

    for path in [&outside, &unlisted_name, &nested, &traversal] {
        let outcome = adopt_one_path(&fx, RootKind::VarTmp, path);
        assert_eq!(
            outcome.refusal,
            Some(AdoptRefusal::OutsideRoots),
            "{path:?}: {outcome:?}"
        );
        assert!(!path.join(RECORD_FILE).exists());
    }
    let relative = adopt_one_path(&fx, RootKind::VarTmp, Path::new("rsi-legacy-t"));
    assert_eq!(relative.refusal, Some(AdoptRefusal::OutsideRoots));
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[test]
fn adopt_refuses_young_missing_and_already_recorded() {
    let fx = Fx::new();
    let young = legacy_dir(fx.root(), "rsi-legacy-young");
    let recorded = fx.scratch("rsi-recorded", RootKind::VarTmp);
    make_old(&recorded);

    assert_eq!(
        adopt_one_path(&fx, RootKind::VarTmp, &young).refusal,
        Some(AdoptRefusal::Young)
    );
    assert_eq!(
        adopt_one_path(&fx, RootKind::VarTmp, &fx.root().join("rsi-gone")).refusal,
        Some(AdoptRefusal::Missing)
    );
    assert_eq!(
        adopt_one_path(&fx, RootKind::VarTmp, &recorded).refusal,
        Some(AdoptRefusal::AlreadyRecorded)
    );
    assert!(!young.join(RECORD_FILE).exists());
}

// The same git proof as reclaim: a legacy repository with unpublished work is
// not adopted, so reclaim could never be asked to delete it.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[test]
fn adopt_refuses_a_dir_with_unpublished_git_work() {
    let fx = Fx::new();
    let legacy = fx.root().join("rsi-legacy-repo");
    let origin = tempfile::tempdir().unwrap();
    published_repo(&legacy, origin.path());
    fs::write(legacy.join("b.txt"), "b\n").unwrap();
    git(&legacy, &["add", "b.txt"]);
    git(&legacy, &["commit", "-q", "-m", "local only"]);
    // Deep writes (.git) are fresh; the age gate is not what this test is about.
    let outcome = adopt_legacy(
        &fx.config(RootKind::VarTmp, OLD),
        later(),
        std::slice::from_ref(&legacy),
    )
    .remove(0);

    assert_eq!(
        outcome.refusal,
        Some(AdoptRefusal::Unpublished),
        "{outcome:?}"
    );
    assert!(!legacy.join(RECORD_FILE).exists());
}

// A lander workspace made before #1140 has no owner file or record: adopting
// it records it, and reclaim then applies the lander rules (no owner file: the
// unregistered-age gate, which a ten-day-old tree passes).
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[test]
fn adopting_a_legacy_lander_workspace_lets_reclaim_list_it() {
    let fx = Fx::new();
    let legacy = legacy_dir(fx.root(), "rsi-rolling-land-old");
    make_old(&legacy);

    let outcome = adopt_one_path(&fx, RootKind::LanderScratch, &legacy);

    assert!(outcome.adopted, "{outcome:?}");
    let report = run(
        &fx.config(RootKind::LanderScratch, MIN_AGE),
        SystemTime::now(),
        true,
    );
    assert_eq!(report.reclaimed, 1, "{report:?}");
}

// R4/#1147: a thread that unshared its descriptor table and holds a file in the
// candidate while the leader and the other threads do not.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[test]
fn adopt_refuses_a_dir_a_private_thread_fd_holds() {
    let fx = Fx::new();
    let legacy = legacy_dir(fx.root(), "rsi-legacy-thread-fd");
    make_old(&legacy);
    let Some(mut holder) = spawn_private_thread_holder(
        &fx,
        "0x400", // CLONE_FILES
        "f=open(os.path.join(sys.argv[1],'data.bin'),'rb')",
        &legacy,
    ) else {
        return;
    };

    let outcome = adopt_one_path(&fx, RootKind::VarTmp, &legacy);
    let report = run_pass(&fx.config(RootKind::VarTmp, OLD));
    holder.kill().unwrap();
    holder.wait().unwrap();

    assert_eq!(outcome.refusal, Some(AdoptRefusal::Held), "{outcome:?}");
    assert!(!legacy.join(RECORD_FILE).exists());
    assert_eq!(report.reclaimed, 0, "{report:?}");
}

// ---- review fixes for #1147 ---------------------------------------------------

fn move_old_file_in(dir: &Path) {
    let staged = dir.parent().unwrap().join("staged.bin");
    fs::write(&staged, b"imported").unwrap();
    fs::File::open(&staged)
        .unwrap()
        .set_modified(SystemTime::now() - TEN_DAYS)
        .unwrap();
    fs::rename(&staged, dir.join("imported.bin")).unwrap();
}

// A file with an old mtime renamed in while the record is written leaves the
// directory mtime as the only recent trace; the adoption must not rewind it.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[test]
fn old_file_moved_in_during_recording_keeps_the_directory_young() {
    let fx = Fx::new();
    let legacy = legacy_dir(fx.root(), "rsi-legacy-race");
    make_old(&legacy);
    let mut config = fx.config(RootKind::VarTmp, MIN_AGE);
    config.after_record_write = Some(move_old_file_in);

    let outcome = adopt_legacy(&config, SystemTime::now(), std::slice::from_ref(&legacy)).remove(0);

    assert!(outcome.adopted, "{outcome:?}");
    assert!(legacy.join("imported.bin").is_file());
    let report = run(&config, SystemTime::now(), true);
    assert_eq!(report.kept_young, 1, "{report:?}");
    assert_eq!(report.reclaimed, 0, "{report:?}");
}

static LATE_HOLDER: std::sync::Mutex<Option<std::process::Child>> = std::sync::Mutex::new(None);

static LATE_MIRROR: std::sync::Mutex<Option<PathBuf>> = std::sync::Mutex::new(None);

fn start_holder_in_sibling(dir: &Path) {
    let sibling = dir.parent().unwrap().join("rsi-legacy-late");
    let child = spawn_holder_in(&sibling);
    // The test's process view is a mirror taken up front; show the new process.
    if let Some(mirror) = LATE_MIRROR.lock().unwrap().as_ref() {
        std::os::unix::fs::symlink(
            Path::new("/proc").join(child.id().to_string()),
            mirror.join(child.id().to_string()),
        )
        .unwrap();
    }
    *LATE_HOLDER.lock().unwrap() = Some(child);
}

// The process inventory is taken per directory: a process that starts holding
// the second directory after the first was recorded is seen.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[test]
fn a_holder_that_appears_during_a_batch_blocks_the_later_directory() {
    let fx = Fx::new();
    let first = legacy_dir(fx.root(), "rsi-legacy-first");
    let late = legacy_dir(fx.root(), "rsi-legacy-late");
    make_old(&first);
    make_old(&late);
    let mut config = fx.config(RootKind::VarTmp, MIN_AGE);
    config.after_record_write = Some(start_holder_in_sibling);
    *LATE_MIRROR.lock().unwrap() = Some(config.proc_root.clone());

    let outcomes = adopt_legacy(&config, SystemTime::now(), &[first.clone(), late.clone()]);
    if let Some(mut holder) = LATE_HOLDER.lock().unwrap().take() {
        holder.kill().unwrap();
        holder.wait().unwrap();
    }

    assert!(outcomes[0].adopted, "{outcomes:?}");
    assert_eq!(
        outcomes[1].refusal,
        Some(AdoptRefusal::Held),
        "{outcomes:?}"
    );
    assert!(!late.join(RECORD_FILE).exists());
}

// A clean, published repository at the candidate root stays reclaimable after
// adoption wrote its (untracked) record into it.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[test]
fn adopting_a_clean_repository_root_keeps_it_reclaimable() {
    let fx = Fx::new();
    let legacy = fx.root().join("rsi-legacy-repo-root");
    let origin = tempfile::tempdir().unwrap();
    published_repo(&legacy, origin.path());
    let config = fx.config(RootKind::VarTmp, OLD);

    let outcome = adopt_legacy(&config, later(), std::slice::from_ref(&legacy)).remove(0);
    assert!(outcome.adopted, "{outcome:?}");

    let report = run(&config, later(), true);
    assert_eq!(report.reclaimed, 1, "{report:?}");
    assert_eq!(report.kept_dirty, 0, "{report:?}");
}

// Only the exact untracked record line is excused, and only at the candidate
// root; any other change in the same repository is still dirt.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[test]
fn only_the_exact_untracked_record_is_excused_from_git_dirtiness() {
    use gitproof::dirty_apart_from_record;
    let record = format!("?? {RECORD_FILE}\n");
    assert!(!dirty_apart_from_record(&record, true));
    assert!(dirty_apart_from_record(&record, false));
    assert!(dirty_apart_from_record(
        &format!("{record}?? notes.txt\n"),
        true
    ));
    assert!(dirty_apart_from_record(
        &format!(" M {RECORD_FILE}\n"),
        true
    ));
    assert!(dirty_apart_from_record(
        &format!("?? {RECORD_FILE}/x\n"),
        true
    ));
    assert!(!dirty_apart_from_record("", true));

    let fx = Fx::new();
    let legacy = fx.root().join("rsi-legacy-repo-dirty");
    let origin = tempfile::tempdir().unwrap();
    published_repo(&legacy, origin.path());
    fs::write(legacy.join("scratch-notes.txt"), "keep me\n").unwrap();
    let outcome = adopt_legacy(
        &fx.config(RootKind::VarTmp, OLD),
        later(),
        std::slice::from_ref(&legacy),
    )
    .remove(0);
    assert_eq!(
        outcome.refusal,
        Some(AdoptRefusal::DirtyWorktree),
        "{outcome:?}"
    );
}

// #1159 x #1147: an adoption refused because the holder proof is incomplete
// names the blocking process (pid, comm), in the listing and in the result.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[test]
fn an_unproven_adoption_names_the_blocking_process() {
    let fx = Fx::new();
    let legacy = legacy_dir(fx.root(), "rsi-legacy-hidden-holder");
    make_old(&legacy);
    let meta = tempfile::tempdir().unwrap();
    let _unlock = UnlockTree(meta.path().to_path_buf());
    let proc_root = meta.path().join("proc");
    hidden_process(&proc_root, 1234, "sshd-session", 1200, 0, 1);
    let mut config = fx.config(RootKind::VarTmp, MIN_AGE);
    config.proc_root = proc_root;

    let (listing, outcome) = unprivileged(|| {
        let listing = list_legacy(&config, SystemTime::now());
        let outcome =
            adopt_legacy(&config, SystemTime::now(), std::slice::from_ref(&legacy)).remove(0);
        (listing, outcome)
    });

    assert_eq!(outcome.refusal, Some(AdoptRefusal::Unproven), "{outcome:?}");
    let detail = outcome.detail.expect("blocker detail");
    assert!(
        detail.contains("1234") && detail.contains("sshd-session"),
        "{detail}"
    );
    assert_eq!(listing.candidates[0].blocker, Some(AdoptRefusal::Unproven));
    assert_eq!(
        listing.candidates[0].detail.as_deref(),
        Some(detail.as_str())
    );
    assert!(!legacy.join(RECORD_FILE).exists());
}

// ---- hermetic fixtures (#1165) ---------------------------------------------------

// The fixtures' unreadable things must be unreadable whoever runs the tests: a
// root runner reads a mode-0 directory unless the pass drops its file-permission
// capabilities.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[test]
fn a_mode_zero_directory_is_unreadable_to_an_unprivileged_pass_whoever_runs_it() {
    let dir = tempfile::tempdir().unwrap();
    let locked = dir.path().join("locked");
    fs::create_dir(&locked).unwrap();
    set_mode(&locked, 0);

    let denied = unprivileged(|| fs::read_dir(&locked).map(|_| ()).map_err(|e| e.kind()));
    set_mode(&locked, 0o700);

    assert_eq!(denied, Err(io::ErrorKind::PermissionDenied));
}

// The process view is the test's own: a same-user process the test did not
// register, and the test process's own descriptors, are not in it, so what else
// runs on the host (or in a concurrent test) cannot hold a candidate.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[test]
fn the_process_view_shows_only_the_processes_the_test_registered() {
    let fx = Fx::new();
    let outsider_dir = fx.scratch("rsi-s2a-outsider", RootKind::VarTmp);
    let registered_dir = fx.scratch("rsi-s2a-registered", RootKind::VarTmp);
    let own_dir = fx.scratch("rsi-s2a-own", RootKind::VarTmp);
    let mut outsider = spawn_holder_in(&outsider_dir);
    let mut registered = spawn_holder_in(&registered_dir);
    fx.watch(&registered);
    let _own_fd = fs::File::open(own_dir.join("data.bin")).unwrap();

    let view = fx.mirror_proc();
    let mut shown: Vec<String> = only_entries(&view);
    shown.sort();
    let mut expected = vec![
        "self".to_string(),
        "sys".to_string(),
        std::process::id().to_string(),
        registered.id().to_string(),
    ];
    expected.sort();
    let report = run_pass(&fx.config(RootKind::VarTmp, OLD));
    for child in [&mut outsider, &mut registered] {
        child.kill().unwrap();
        child.wait().unwrap();
    }

    assert_eq!(shown, expected);
    assert_eq!(report.kept_held, 1, "{report:?}");
    assert_eq!(report.reclaimed, 2, "{report:?}");
    assert!(registered_dir.join("data.bin").exists());
    assert!(!outsider_dir.exists());
    assert!(!own_dir.exists());
}

// Repositories named alike share a parent directory; each is published to its
// own origin, so a later commit never meets another repository's history.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[test]
fn published_repositories_named_alike_get_their_own_origin() {
    let origins = tempfile::tempdir().unwrap();
    let work = tempfile::tempdir().unwrap();
    let urls: Vec<String> = ["a", "b"]
        .iter()
        .map(|name| {
            let repo = work.path().join(name).join("repo");
            published_repo(&repo, origins.path());
            let out = isolated_git()
                .arg("-C")
                .arg(&repo)
                .args(["remote", "get-url", "origin"])
                .output()
                .unwrap();
            String::from_utf8(out.stdout).unwrap()
        })
        .collect();

    assert_ne!(urls[0], urls[1]);
    assert_eq!(only_entries(origins.path()).len(), 2);
}

// A scratch directory is bound to its filesystem birth time: the fixtures sit
// on a filesystem that keeps one, whatever the host's temporary directory is.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[test]
fn the_fixture_root_is_on_a_filesystem_that_keeps_a_birth_time() {
    let fx = Fx::new();

    let pinned = record::pin_dir(fx.root()).unwrap();

    assert!(pinned.btime.is_some());
}

// A pass reads the test's own mount table, never the host's: a mount below a
// fixture (or a line the parser refuses) on the machine running the tests
// cannot keep a candidate.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[test]
fn a_pass_reads_the_tests_own_mount_table() {
    let fx = Fx::new();
    let dir = fx.scratch("rsi-s2a-mounts", RootKind::VarTmp);

    let cfg = fx.config(RootKind::VarTmp, OLD);
    let mounts = fsys::read_mounts(&cfg.mountinfo).expect("the fixture table parses");
    let report = run_pass(&cfg);

    assert_ne!(cfg.mountinfo, Path::new("/proc/self/mountinfo"));
    assert!(!fsys::mount_at_or_under(&mounts, &dir));
    assert_eq!(report.reclaimed, 1, "{report:?}");
}

// The premise of `eventually`: a process another test forks holds a copy of the
// custody descriptor until it execs, so a lock the test released is still held
// for that moment and an exclusive request is refused, then granted.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[test]
fn a_released_custody_lock_stays_held_by_a_child_that_has_not_yet_exec_d() {
    use std::os::unix::process::CommandExt;
    let fx = Fx::new();
    let registry = Registry::open(fx.registry(), current_uid()).unwrap();
    let shared = registry.custody(false).unwrap();
    assert!(shared.is_some());
    let spawner = std::thread::spawn(|| {
        let mut command = Command::new("true");
        // SAFETY: the closure only sleeps (async-signal-safe) between the fork
        // and the exec.
        unsafe {
            command.pre_exec(|| {
                std::thread::sleep(Duration::from_millis(400));
                Ok(())
            });
        }
        command.status().unwrap();
    });
    // The child has forked and sits before its exec.
    std::thread::sleep(Duration::from_millis(100));

    drop(shared);
    let refused = registry.custody(true).unwrap().is_none();
    let granted = eventually(|| registry.custody(true).unwrap()).is_some();
    spawner.join().unwrap();

    assert!(refused, "the forked child no longer shares the descriptor");
    assert!(granted);
}
