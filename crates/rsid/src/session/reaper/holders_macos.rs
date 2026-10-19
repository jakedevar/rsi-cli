//! macOS holder inventory for the quarantine proof (#102): the libproc
//! counterpart of the Linux `/proc` walk in `scan_quarantine_holders`.
//!
//! A process of this uid (real or effective) holds the quarantined tree when
//! its working directory or executable lies beneath the root, when one of its
//! open vnode descriptors lies beneath the root or has the identity
//! (`st_dev`, `st_ino`) of a tree entry, or when any memory mapping does (a
//! mapping outlives the descriptor that made it; regions are read one by one
//! with `PROC_PIDREGIONPATHINFO`). A process that exits or turns zombie during
//! the scan holds nothing. Credentials are observed: when libproc withholds a
//! process's basic record (`EPERM`) its owner is read from the process table.
//!
//! Anything else unreadable about a same-uid process keeps the root, keeping
//! the errno, as does an over-long table, a work bound or the deadline. The one
//! exemption is proved exactly (`trusted_platform_process`): `EPERM` from a
//! platform binary running from an OS-owned root, the OS's own daemons that
//! hide their descriptors from every caller.

use super::{
    QUARANTINE_HOLDER_INVENTORY_INCOMPLETE, QuarantineHolderScanLimits, QuarantineHolderTarget,
};
use nix::libc;
use rsid_core::libproc_macos::{
    Credentials, Region, VnodeRef, credentials, errno, executable, exited, fd_vnode,
    is_platform_binary, is_vnode, list_fds, list_pids, region_at, working_directory,
};
use std::path::{Path, PathBuf};
use std::time::Instant as StdInstant;

/// The libproc reads one process is scanned through; the tests substitute a
/// fake to drive every failure path.
trait Inventory {
    fn credentials(&self, pid: libc::pid_t) -> Result<Credentials, i32>;
    fn working_directory(&self, pid: libc::pid_t) -> Result<VnodeRef, i32>;
    fn executable(&self, pid: libc::pid_t) -> Result<Option<PathBuf>, i32>;
    fn list_fds(&self, pid: libc::pid_t) -> Result<Vec<libc::proc_fdinfo>, i32>;
    fn fd_vnode(&self, pid: libc::pid_t, fd: i32) -> Result<Option<VnodeRef>, i32>;
    fn region_at(&self, pid: libc::pid_t, address: u64) -> Result<Option<Region>, i32>;
    fn platform_binary(&self, pid: libc::pid_t) -> Result<bool, i32>;
}

struct Libproc;

impl Inventory for Libproc {
    fn credentials(&self, pid: libc::pid_t) -> Result<Credentials, i32> {
        credentials(pid)
    }
    fn working_directory(&self, pid: libc::pid_t) -> Result<VnodeRef, i32> {
        working_directory(pid)
    }
    fn executable(&self, pid: libc::pid_t) -> Result<Option<PathBuf>, i32> {
        executable(pid)
    }
    fn list_fds(&self, pid: libc::pid_t) -> Result<Vec<libc::proc_fdinfo>, i32> {
        list_fds(pid)
    }
    fn fd_vnode(&self, pid: libc::pid_t, fd: i32) -> Result<Option<VnodeRef>, i32> {
        fd_vnode(pid, fd)
    }
    fn region_at(&self, pid: libc::pid_t, address: u64) -> Result<Option<Region>, i32> {
        region_at(pid, address)
    }
    fn platform_binary(&self, pid: libc::pid_t) -> Result<bool, i32> {
        is_platform_binary(pid)
    }
}

/// Where Apple's platform binaries live: the signed system volume and the
/// `/usr`, `/bin` and `/sbin` trees SIP protects. A user cannot add a file
/// here, so a platform binary running from one is the OS's own daemon.
const PLATFORM_EXECUTABLE_ROOTS: [&str; 5] =
    ["/System/", "/usr/", "/bin/", "/sbin/", "/Library/Apple/"];

/// The one exemption from an unreadable inventory: the OS's own daemons that
/// run as this user and withhold their descriptors and mappings (`EPERM`).
/// It is proved, not assumed: the errno is exactly `EPERM`, the kernel marks
/// the process a platform binary (`csops`), and its executable lies under an
/// OS-owned root outside the tree. Any other failure, a failed signature read
/// or an unknown executable keeps the root. Limit [inferred]: a platform
/// daemon that opened a tree file and also hides its descriptors is not seen.
fn trusted_platform_process(
    inventory: &impl Inventory,
    pid: libc::pid_t,
    executable: Option<&Path>,
    tree: &dyn QuarantineHolderTarget,
    code: i32,
) -> bool {
    code == libc::EPERM
        && executable.is_some_and(|exe| {
            !tree.names_path(exe)
                && PLATFORM_EXECUTABLE_ROOTS
                    .iter()
                    .any(|root| exe.starts_with(root))
        })
        && inventory.platform_binary(pid) == Ok(true)
}

fn incomplete(pid: libc::pid_t, what: &str, code: i32) -> crate::error::DaemonError {
    crate::error::DaemonError::Process(format!(
        "{QUARANTINE_HOLDER_INVENTORY_INCOMPLETE} {what} unreadable for pid {pid}: errno {code}"
    ))
}

fn holds(tree: &dyn QuarantineHolderTarget, vnode: &VnodeRef) -> bool {
    tree.names_path(&vnode.path) || tree.contains_identity(vnode.device, vnode.inode)
}

fn retained(pid: libc::pid_t, through: &str) -> crate::error::DaemonError {
    crate::error::DaemonError::Process(format!(
        "same-UID process {pid} retains quarantine through {through}"
    ))
}

/// One complete pass over every process on the host.
pub(super) fn scan_quarantine_holders(
    self_uid: u32,
    tree: &dyn QuarantineHolderTarget,
    deadline: StdInstant,
    limits: QuarantineHolderScanLimits,
) -> crate::error::Result<()> {
    let pids = list_pids().ok_or_else(|| {
        crate::error::DaemonError::Process(format!(
            "quarantine holder process list failed: errno {}",
            errno()
        ))
    })?;
    if pids.len() > limits.max_proc_entries {
        return Err(crate::error::DaemonError::Process(
            "quarantine holder process count exceeded bound".into(),
        ));
    }
    let mut fd_entries = 0_usize;
    for pid in pids {
        if StdInstant::now() >= deadline {
            return Err(crate::error::DaemonError::QuarantineProofDeferred);
        }
        // pid 0 is the kernel.
        if pid <= 0 {
            continue;
        }
        tree.begin_task();
        let live = scan_process(
            &Libproc,
            pid,
            self_uid,
            tree,
            deadline,
            limits,
            &mut fd_entries,
        );
        tree.finish_task(live.as_ref().is_ok_and(|seen| *seen));
        live?;
    }
    Ok(())
}

/// Whether `pid` was a live same-uid process whose inventory was read.
fn scan_process(
    inventory: &impl Inventory,
    pid: libc::pid_t,
    self_uid: u32,
    tree: &dyn QuarantineHolderTarget,
    deadline: StdInstant,
    limits: QuarantineHolderScanLimits,
    fd_entries: &mut usize,
) -> crate::error::Result<bool> {
    // Credentials are observed, never assumed: when libproc withholds the
    // basic record, the process table still names the owner. A process whose
    // owner cannot be read at all keeps the root.
    let info = match inventory.credentials(pid) {
        Ok(info) => info,
        Err(code) if exited(code) => return Ok(false),
        Err(code) => return Err(incomplete(pid, "credentials", code)),
    };
    if (info.uid != self_uid && info.ruid != self_uid) || info.zombie {
        return Ok(false);
    }
    // The executable first: the exemption below needs to know what runs.
    let executable = match inventory.executable(pid) {
        Ok(Some(exe)) if tree.names_path(&exe) => return Err(retained(pid, "its executable")),
        Ok(exe) => exe,
        Err(code) => return Err(incomplete(pid, "executable", code)),
    };
    let trusted =
        |code| trusted_platform_process(inventory, pid, executable.as_deref(), tree, code);
    match inventory.working_directory(pid) {
        Ok(cwd) if holds(tree, &cwd) => return Err(retained(pid, "cwd")),
        Ok(_) => {}
        Err(code) if exited(code) => return Ok(false),
        // A setuid OS tool (login) has this user's real uid and root's effective
        // one, which hides its working directory.
        Err(code) if trusted(code) => {}
        Err(code) => return Err(incomplete(pid, "working directory", code)),
    }
    let fds = match inventory.list_fds(pid) {
        Ok(fds) => fds,
        Err(code) if exited(code) => return Ok(false),
        Err(code) if trusted(code) => Vec::new(),
        Err(code) => return Err(incomplete(pid, "descriptor table", code)),
    };
    for fd in fds.iter().filter(|fd| is_vnode(fd)) {
        *fd_entries = fd_entries.checked_add(1).ok_or_else(|| {
            crate::error::DaemonError::Process("quarantine holder fd count overflowed".into())
        })?;
        if *fd_entries > limits.max_fd_entries {
            return Err(crate::error::DaemonError::Process(
                "quarantine holder fd count exceeded bound".into(),
            ));
        }
        if StdInstant::now() >= deadline {
            return Err(crate::error::DaemonError::QuarantineProofDeferred);
        }
        match inventory.fd_vnode(pid, fd.proc_fd) {
            Ok(Some(vnode)) if holds(tree, &vnode) => {
                return Err(retained(pid, &format!("fd {}", fd.proc_fd)));
            }
            // A closed or pathless (unlinked) descriptor names no tree entry; a
            // withheld one (EPERM) is an unreadable inventory and fails below.
            Ok(_) => {}
            Err(code) if exited(code) => return Ok(false),
            Err(code) if trusted(code) => {}
            Err(code) => return Err(incomplete(pid, "descriptor", code)),
        }
    }
    scan_regions(
        inventory,
        pid,
        executable.as_deref(),
        tree,
        deadline,
        limits,
    )?;
    Ok(true)
}

/// Files mapped into `pid` through any mapping, including one whose descriptor
/// is closed (an `mmap` outlives its fd). The walk is bounded by the maps
/// record limit and the deadline; a region that cannot be read, a walk that
/// does not advance, or one over the bound fails the proof closed.
fn scan_regions(
    inventory: &impl Inventory,
    pid: libc::pid_t,
    executable: Option<&Path>,
    tree: &dyn QuarantineHolderTarget,
    deadline: StdInstant,
    limits: QuarantineHolderScanLimits,
) -> crate::error::Result<()> {
    let mut address = 0_u64;
    let mut records = 0_usize;
    loop {
        if StdInstant::now() >= deadline {
            return Err(crate::error::DaemonError::QuarantineProofDeferred);
        }
        let region = match inventory.region_at(pid, address) {
            Ok(Some(region)) => region,
            Ok(None) => return Ok(()),
            // Exited during the walk: it holds nothing now.
            Err(code) if exited(code) => return Ok(()),
            // The OS's own daemons withhold their mappings like their descriptors.
            Err(code) if trusted_platform_process(inventory, pid, executable, tree, code) => {
                return Ok(());
            }
            Err(code) => return Err(incomplete(pid, "memory regions", code)),
        };
        records += 1;
        if records > limits.max_maps_records {
            return Err(crate::error::DaemonError::Process(
                "quarantine holder memory region count exceeded bound".into(),
            ));
        }
        if let Some(vnode) = &region.vnode
            && holds(tree, vnode)
        {
            return Err(retained(pid, "a memory mapping"));
        }
        // An empty region (the kernel reports one at the end of some maps) has
        // no extent to skip: step one byte past it. Otherwise continue after
        // the region. The next query must lie beyond this one, or the walk is
        // not advancing and cannot be trusted to end.
        let next = if region.size == 0 {
            region.address.max(address).checked_add(1)
        } else {
            region.address.checked_add(region.size)
        };
        match next {
            Some(next) if next > address => address = next,
            _ => return Err(incomplete(pid, "memory regions", libc::EIO)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sandbox::git_worktree::{QuarantineTreeProof, prove_quarantine_tree_safe};
    use std::time::Duration;

    /// A tree proof over a fresh canonical directory holding one file.
    fn tree_fixture() -> (tempfile::TempDir, QuarantineTreeProof, PathBuf) {
        let temp = tempfile::tempdir().expect("holder tree fixture");
        let root = std::fs::canonicalize(temp.path())
            .expect("canonical temp")
            .join("quarantine");
        std::fs::create_dir(&root).expect("quarantine root");
        let file = root.join("held");
        std::fs::write(&file, "holder bytes\n").expect("tree file");
        let proof = prove_quarantine_tree_safe(&root).expect("safe holder tree");
        (temp, proof, file)
    }

    fn scan(tree: &QuarantineTreeProof) -> crate::error::Result<()> {
        scan_quarantine_holders(
            nix::unistd::getuid().as_raw(),
            tree,
            StdInstant::now() + Duration::from_secs(60),
            QuarantineHolderScanLimits::default(),
        )
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-03"))]
    #[test]
    fn macos_scan_finds_a_process_holding_a_file_open_under_the_tree() {
        let (_temp, tree, file) = tree_fixture();
        scan(&tree).expect("an idle tree has no holder");
        let open = std::fs::File::open(&file).expect("hold the file open");
        let error = scan(&tree).expect_err("an open descriptor under the tree is a holder");
        assert!(error.to_string().contains("through fd"), "{error}");
        drop(open);
        scan(&tree).expect("the holder released the file");
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-03"))]
    #[test]
    fn macos_scan_finds_a_process_whose_cwd_is_inside_the_tree() {
        let (_temp, tree, _file) = tree_fixture();
        let mut child = std::process::Command::new("/bin/sleep")
            .arg("30")
            .current_dir(tree.root())
            .spawn()
            .expect("spawn a child inside the tree");
        let verdict = scan(&tree);
        child.kill().ok();
        child.wait().ok();
        let error = verdict.expect_err("a child inside the tree is a holder");
        assert!(error.to_string().contains("through cwd"), "{error}");
        scan(&tree).expect("the child is gone");
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-03"))]
    #[test]
    fn macos_scan_finds_a_descriptor_by_identity_after_the_file_left_the_tree() {
        let (temp, tree, file) = tree_fixture();
        let open = std::fs::File::open(&file).expect("hold the file open");
        let outside = temp.path().join("moved");
        std::fs::rename(&file, &outside).expect("move the held file out of the tree");
        let error = scan(&tree).expect_err("the held inode is a tree entry");
        assert!(error.to_string().contains("through fd"), "{error}");
        drop(open);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-03"))]
    #[test]
    fn macos_full_proof_retains_a_held_tree_and_proves_a_released_one() {
        let (_temp, tree, file) = tree_fixture();
        let uid = nix::unistd::getuid().as_raw();
        let open = std::fs::File::open(&file).expect("hold the file open");
        let error = super::super::prove_quarantine_has_no_untrusted_same_uid_holders_native(
            uid,
            &tree,
            QuarantineHolderScanLimits::default(),
        )
        .expect_err("a holder retains the quarantine");
        assert!(error.to_string().contains("retains quarantine"), "{error}");
        drop(open);
        let proof = super::super::prove_quarantine_has_no_untrusted_same_uid_holders_native(
            uid,
            &tree,
            QuarantineHolderScanLimits::default(),
        )
        .expect("no holder remains");
        assert!(proof.fixed_point_passes() >= 2);
    }

    /// A scripted process for driving each failure path of `scan_process`.
    struct Fake {
        credentials: Result<Credentials, i32>,
        cwd: Result<VnodeRef, i32>,
        executable: Result<Option<PathBuf>, i32>,
        fds: Result<Vec<libc::proc_fdinfo>, i32>,
        fd: Result<Option<VnodeRef>, i32>,
        regions: Vec<Result<Option<Region>, i32>>,
        platform: Result<bool, i32>,
        next_region: std::cell::Cell<usize>,
    }

    impl Fake {
        fn quiet() -> Self {
            let uid = nix::unistd::getuid().as_raw();
            Self {
                credentials: Ok(Credentials {
                    uid,
                    ruid: uid,
                    zombie: false,
                }),
                cwd: Ok(vnode("/nowhere/cwd", 1, 1)),
                executable: Ok(Some(PathBuf::from("/usr/bin/true"))),
                fds: Ok(Vec::new()),
                fd: Ok(Some(vnode("/nowhere/fd", 1, 2))),
                regions: vec![Ok(None)],
                platform: Ok(false),
                next_region: std::cell::Cell::new(0),
            }
        }
    }

    fn vnode(path: &str, device: u64, inode: u64) -> VnodeRef {
        VnodeRef {
            path: PathBuf::from(path),
            device,
            inode,
        }
    }

    fn region(address: u64, size: u64, vnode: Option<VnodeRef>) -> Region {
        Region {
            address,
            size,
            vnode,
        }
    }

    fn one_vnode_fd() -> Result<Vec<libc::proc_fdinfo>, i32> {
        Ok(vec![libc::proc_fdinfo {
            proc_fd: 3,
            proc_fdtype: libc::PROX_FDTYPE_VNODE as u32,
        }])
    }

    impl Inventory for Fake {
        fn credentials(&self, _: libc::pid_t) -> Result<Credentials, i32> {
            self.credentials
        }
        fn working_directory(&self, _: libc::pid_t) -> Result<VnodeRef, i32> {
            self.cwd.clone()
        }
        fn executable(&self, _: libc::pid_t) -> Result<Option<PathBuf>, i32> {
            self.executable.clone()
        }
        fn list_fds(&self, _: libc::pid_t) -> Result<Vec<libc::proc_fdinfo>, i32> {
            match &self.fds {
                Ok(fds) => Ok(fds
                    .iter()
                    .map(|fd| libc::proc_fdinfo {
                        proc_fd: fd.proc_fd,
                        proc_fdtype: fd.proc_fdtype,
                    })
                    .collect()),
                Err(code) => Err(*code),
            }
        }
        fn fd_vnode(&self, _: libc::pid_t, _: i32) -> Result<Option<VnodeRef>, i32> {
            self.fd.clone()
        }
        fn region_at(&self, _: libc::pid_t, _: u64) -> Result<Option<Region>, i32> {
            let index = self.next_region.get();
            self.next_region.set(index + 1);
            self.regions.get(index).cloned().unwrap_or(Ok(None))
        }
        fn platform_binary(&self, _: libc::pid_t) -> Result<bool, i32> {
            self.platform
        }
    }

    fn scan_fake(fake: &Fake, tree: &QuarantineTreeProof) -> crate::error::Result<bool> {
        scan_fake_with(fake, tree, QuarantineHolderScanLimits::default())
    }

    fn scan_fake_with(
        fake: &Fake,
        tree: &QuarantineTreeProof,
        limits: QuarantineHolderScanLimits,
    ) -> crate::error::Result<bool> {
        let mut fd_entries = 0;
        scan_process(
            fake,
            4242,
            nix::unistd::getuid().as_raw(),
            tree,
            StdInstant::now() + Duration::from_secs(60),
            limits,
            &mut fd_entries,
        )
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-03"))]
    #[test]
    fn every_unexplained_read_failure_of_a_same_uid_process_keeps_the_root() {
        let (_temp, tree, _file) = tree_fixture();
        assert!(scan_fake(&Fake::quiet(), &tree).expect("a quiet process holds nothing"));
        type Break = fn(&mut Fake, i32);
        let breaks: [(&str, Break); 6] = [
            ("credentials", |f, c| f.credentials = Err(c)),
            ("working directory", |f, c| f.cwd = Err(c)),
            ("executable", |f, c| f.executable = Err(c)),
            ("descriptor table", |f, c| f.fds = Err(c)),
            ("descriptor", |f, c| {
                f.fds = one_vnode_fd();
                f.fd = Err(c);
            }),
            ("memory regions", |f, c| f.regions = vec![Err(c)]),
        ];
        for (what, break_it) in breaks {
            // EPERM is no exemption for a same-uid process: it is not a proof of
            // foreign credentials, only an unread inventory. Nor is EIO.
            for code in [libc::EPERM, libc::EIO, libc::ENOMEM] {
                let mut fake = Fake::quiet();
                break_it(&mut fake, code);
                let error = scan_fake(&fake, &tree)
                    .expect_err(&format!("{what} errno {code} must keep the root"));
                let text = error.to_string();
                assert!(
                    text.contains(QUARANTINE_HOLDER_INVENTORY_INCOMPLETE)
                        && text.contains(&format!("{what} unreadable"))
                        && text.contains(&format!("errno {code}")),
                    "{what} {code}: {text}"
                );
            }
        }
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-03"))]
    #[test]
    fn a_confirmed_exit_a_foreign_owner_or_a_pathless_descriptor_holds_nothing() {
        let (_temp, tree, _file) = tree_fixture();
        type Break = fn(&mut Fake);
        let gone: [(&str, Break); 5] = [
            ("credentials", |f| f.credentials = Err(libc::ESRCH)),
            ("working directory", |f| f.cwd = Err(libc::ESRCH)),
            ("descriptor table", |f| f.fds = Err(libc::ESRCH)),
            ("descriptor", |f| {
                f.fds = one_vnode_fd();
                f.fd = Err(libc::ESRCH);
            }),
            ("memory regions", |f| f.regions = vec![Err(libc::ESRCH)]),
        ];
        for (what, break_it) in gone {
            let mut fake = Fake::quiet();
            break_it(&mut fake);
            // A process that exited mid-scan is not a live reader of anything.
            let live = scan_fake(&fake, &tree).unwrap_or_else(|e| panic!("{what}: {e}"));
            assert_eq!(live, what == "memory regions", "{what}");
        }
        // The owner is observed: a process of another user is skipped without
        // reading its inventory, which would fail.
        let mut foreign = Fake::quiet();
        foreign.credentials = Ok(Credentials {
            uid: u32::MAX - 1,
            ruid: u32::MAX - 1,
            zombie: false,
        });
        foreign.cwd = Err(libc::EPERM);
        assert!(!scan_fake(&foreign, &tree).expect("foreign owner is skipped"));
        let mut zombie = Fake::quiet();
        zombie.credentials = zombie
            .credentials
            .map(|c| Credentials { zombie: true, ..c });
        zombie.cwd = Err(libc::EPERM);
        assert!(!scan_fake(&zombie, &tree).expect("a zombie is skipped"));
        // An exited executable and an unlinked descriptor name no tree entry.
        let mut pathless = Fake::quiet();
        pathless.executable = Ok(None);
        pathless.fds = one_vnode_fd();
        pathless.fd = Ok(None);
        assert!(scan_fake(&pathless, &tree).expect("nothing to hold"));
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-03"))]
    #[test]
    fn a_mapped_region_of_a_tree_file_holds_it_by_path_or_identity() {
        use std::os::unix::fs::MetadataExt;
        let (_temp, tree, file) = tree_fixture();
        let meta = std::fs::metadata(&file).unwrap();
        let mut by_path = Fake::quiet();
        by_path.regions = vec![
            Ok(Some(region(0x1000, 0x1000, None))),
            Ok(Some(region(
                0x4000,
                0x1000,
                Some(vnode(file.to_str().unwrap(), 0, 0)),
            ))),
        ];
        let error = scan_fake(&by_path, &tree).expect_err("a mapped tree path holds the tree");
        assert!(error.to_string().contains("a memory mapping"), "{error}");
        let mut by_identity = Fake::quiet();
        by_identity.regions = vec![Ok(Some(region(
            0x4000,
            0x1000,
            Some(vnode("/moved/away", meta.dev(), meta.ino())),
        )))];
        let error = scan_fake(&by_identity, &tree).expect_err("a mapped tree inode holds it");
        assert!(error.to_string().contains("a memory mapping"), "{error}");
        // A mapping of something else does not.
        let mut elsewhere = Fake::quiet();
        elsewhere.regions = vec![Ok(Some(region(
            0x4000,
            0x1000,
            Some(vnode("/usr/lib/dyld", 1, 3)),
        )))];
        assert!(scan_fake(&elsewhere, &tree).expect("an unrelated mapping"));
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-03"))]
    #[test]
    fn the_region_walk_is_bounded_and_must_advance() {
        let (_temp, tree, _file) = tree_fixture();
        let mut crowded = Fake::quiet();
        crowded.regions = (0..4)
            .map(|n| Ok(Some(region(0x1000 * (n + 1), 0x1000, None))))
            .collect();
        let mut limits = QuarantineHolderScanLimits::default();
        limits.max_maps_records = 3;
        let error = scan_fake_with(&crowded, &tree, limits).expect_err("over the bound");
        assert!(error.to_string().contains("exceeded bound"), "{error}");
        limits.max_maps_records = 4;
        assert!(scan_fake_with(&crowded, &tree, limits).expect("within the bound"));
        // An empty region has no extent: the walk steps past it and ends.
        let mut empty = Fake::quiet();
        empty.regions = vec![Ok(Some(region(0x1000, 0, None))), Ok(None)];
        assert!(scan_fake(&empty, &tree).expect("an empty region is skipped"));
        // A region that does not move the walk on never ends: refuse it.
        for backwards in [region(0x1000, 0x1000, None), region(0x4000, u64::MAX, None)] {
            let mut fake = Fake::quiet();
            fake.regions = vec![Ok(Some(region(0x4000, 0x1000, None))), Ok(Some(backwards))];
            let error = scan_fake(&fake, &tree).expect_err("a stuck walk is incomplete");
            assert!(
                error.to_string().contains("memory regions unreadable"),
                "{error}"
            );
        }
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-03"))]
    #[test]
    fn only_a_proved_platform_daemon_may_withhold_its_descriptors_or_mappings() {
        let (_temp, tree, file) = tree_fixture();
        let hides = |executable: Option<&str>, platform: Result<bool, i32>, code: i32| {
            let mut fake = Fake::quiet();
            fake.executable = Ok(executable.map(PathBuf::from));
            fake.platform = platform;
            fake.fds = one_vnode_fd();
            fake.fd = Err(code);
            let by_fd = scan_fake(&fake, &tree);
            let mut fake = Fake::quiet();
            fake.executable = Ok(executable.map(PathBuf::from));
            fake.platform = platform;
            fake.regions = vec![Err(code)];
            (by_fd, scan_fake(&fake, &tree))
        };
        // dmd-like: EPERM, platform binary, OS-owned executable.
        let (by_fd, by_region) = hides(Some("/usr/libexec/dmd"), Ok(true), libc::EPERM);
        assert!(by_fd.expect("exempt"));
        assert!(by_region.expect("exempt"));
        // Each leg of the proof is required.
        let dmd = Some("/usr/libexec/dmd");
        let denied = [
            ("not a platform binary", hides(dmd, Ok(false), libc::EPERM)),
            (
                "signature unreadable",
                hides(dmd, Err(libc::EPERM), libc::EPERM),
            ),
            (
                "executable outside the OS roots",
                hides(Some("/Users/x/dmd"), Ok(true), libc::EPERM),
            ),
            ("executable unknown", hides(None, Ok(true), libc::EPERM)),
            ("a different errno", hides(dmd, Ok(true), libc::EIO)),
        ];
        for (why, (by_fd, by_region)) in denied {
            assert!(by_fd.is_err(), "{why}: descriptor");
            assert!(by_region.is_err(), "{why}: regions");
        }
        // A platform binary copied into the tree is its executable, not an exemption.
        let mut copied = Fake::quiet();
        copied.executable = Ok(Some(file));
        copied.platform = Ok(true);
        let error = scan_fake(&copied, &tree).expect_err("the executable is in the tree");
        assert!(error.to_string().contains("its executable"), "{error}");
    }

    /// Re-executed by the test below as an isolated child: maps the file named
    /// in the environment, closes the descriptor, reports and stays alive.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-03"))]
    #[test]
    fn mmap_holder_child_helper() {
        use std::io::Write;
        let Some(path) = std::env::var_os("RSI_TEST_MMAP_HOLDER_FILE") else {
            return;
        };
        let file = std::fs::File::open(path).expect("open the file to map");
        // SAFETY: maps a regular file read-only and never unmaps it.
        let map = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                4096,
                libc::PROT_READ,
                libc::MAP_PRIVATE,
                std::os::unix::io::AsRawFd::as_raw_fd(&file),
                0,
            )
        };
        assert_ne!(map, libc::MAP_FAILED);
        drop(file);
        // libtest prefixes this line with "test <name> ... ".
        println!("MMAP-HELD");
        std::io::stdout().flush().unwrap();
        std::thread::sleep(Duration::from_secs(60));
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-03"))]
    #[test]
    fn macos_scan_finds_a_file_mapped_by_a_child_whose_descriptor_is_closed() {
        use std::io::{BufRead, BufReader};
        let (_temp, tree, file) = tree_fixture();
        std::fs::write(&file, vec![7u8; 4096]).unwrap();
        scan(&tree).expect("an idle tree has no holder");
        let elsewhere = tempfile::tempdir().unwrap();
        let mut child = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "session::reaper::holders_macos::tests::mmap_holder_child_helper",
                "--nocapture",
                "--test-threads=1",
            ])
            .env("RSI_TEST_MMAP_HOLDER_FILE", &file)
            // Neither its cwd nor its executable is inside the tree.
            .current_dir(elsewhere.path())
            .stdout(std::process::Stdio::piped())
            .spawn()
            .expect("spawn the mapping child");
        let mut lines = BufReader::new(child.stdout.take().unwrap()).lines();
        let ready = lines.any(|line| line.is_ok_and(|line| line.contains("MMAP-HELD")));
        let verdict = scan(&tree);
        child.kill().ok();
        child.wait().ok();
        assert!(ready, "the child mapped the file");
        let error = verdict.expect_err("a mapping with a closed descriptor holds the tree");
        assert!(error.to_string().contains("a memory mapping"), "{error}");
        scan(&tree).expect("the child is gone");
    }
}
