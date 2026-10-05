//! A kernel-enforced write fence for the queue's destructive git (#1160).
//!
//! The queue resets its worktree with git. Pinning the worktree by open handle
//! does not keep git inside it: git normalizes its work tree to an absolute
//! path and later enters it by name, so a slot swapped for the operator
//! checkout in that window redirects the command. So the child is also placed
//! under a Landlock ruleset that denies every filesystem write except beneath
//! the directory handles it was given (the pinned worktree and a private git
//! directory). Rules attach to open handles, not paths, so no rename, symlink
//! or mount change under `~/.rsi/queue` can widen them: a command that ends up
//! in the operator checkout is refused by the kernel. Reads stay unrestricted.
//!
//! What the fence does NOT cover, and what covers it instead:
//! * A directory grant covers every name beneath it, including a hard link to
//!   an operator file planted there. Nothing the queue runs may therefore write
//!   in place to a pre-existing file under a granted directory: the caller runs
//!   only `reset --hard`, `clean` and `rev-parse` against a freshly
//!   created private git directory (no reflogs, so no append-in-place), and git
//!   replaces worktree files by unlink plus exclusive create.
//! * Landlock does not restrict chmod, chown, utimes or xattr calls, and a
//!   hook or an external filter inherits the ruleset without being stopped from
//!   them. The caller therefore gives git a private config (no hooks, no filter
//!   drivers, no global or system config), so no program but git's own builtin
//!   runs; the builtin only calls those syscalls on files it creates.
//!
//! * Residual (#1160, narrowed by #1170): a same-uid process that can win a
//!   race against the daemon inside the sub-second window after the private git
//!   directory is created can still REPLACE a file the daemon created there (a
//!   `config` or `info` swapped for another file, a hard link). It cannot be
//!   closed without a mount namespace: the directory is reachable by its name,
//!   and even a handle-only path is reachable through `/proc/<pid>/fd/N` by any
//!   process of the same uid, `mkdir` mode 0700 is no barrier to the owner, and
//!   an unlinked directory cannot hold git's lock files. What #1170 removed is
//!   every ADD-only plant (no race against an exclusive create needed): a
//!   `commondir` file no longer redirects the common directory (it is named by
//!   `GIT_COMMON_DIR`), a `refs/replace` ref no longer substitutes objects
//!   (`GIT_NO_REPLACE_OBJECTS`), and `info` (attributes, exclude) is a regular
//!   file. Hooks, fsmonitor, attributes and reflogs are off on git's command
//!   line, `logs` is a regular file, no ref is updated and the name is a fresh
//!   UUID. A configured filter driver planted by replacing `config` is not
//!   stoppable. Such a process can also edit `~/rsi` directly, so the fence's
//!   value is against accidental redirection and non-racing aliases, not
//!   against an actively racing same-uid attacker.
//! * First creation (#1170, ACCEPTED RESIDUAL): the worktree is registered by
//!   `git worktree add --no-checkout --detach --force`, preceded by
//!   `git worktree prune`, run unfenced in the source repository (hooks,
//!   fsmonitor and attributes are off on its command line, the environment is
//!   the allowlist, and nothing is checked out; the population that follows is
//!   pinned and fenced). The registration write and the prune resolve their
//!   paths by name, so a slot, `worktrees` directory or `.git` swapped inside
//!   that window redirects them, and the add cannot be fenced because it must
//!   write the operator's common directory. A hand-written registration writer
//!   (handle-relative, exclusive creates) was built and REMOVED after two
//!   review rounds: it writes and deletes in the operator's repository, and
//!   every round found a new hole. The findings are the reasons a separate
//!   design is needed: (1) publication must follow git's init-lock protocol
//!   (the entry is visible to a concurrent prune or gc between `mkdir` and the
//!   `locked` file, and an empty replacement cannot be told from our own);
//!   (2) the ownership marker must be bound to ONE slot and creator with a
//!   liveness check (a global marker lets another queue workspace sharing the
//!   same common directory retire a live initialization, since `stable_key`
//!   hashes only the common path); (3) repair of a dangling slot must prove the
//!   pointer's target and be race-safe against a concurrent repair; (4) a crash
//!   after the slot's `.git` and before the lock release must not leave
//!   `locked` forever; (5) every removal must be non-recursive, through
//!   validated handles, with operator-repository ownership proofs. Until then
//!   the residual is the path-based registration write under `~/.rsi/queue`.
//! * The source fetch (#1170): `fetch_rolling` in the parent module runs git in
//!   the source repository unfenced, with hooks, maintenance, `FETCH_HEAD`,
//!   submodules, tags and `ext::` off on the command line. Fencing it was
//!   evaluated and rejected: the transport children write their own state
//!   (`known_hosts`, control sockets, credential caches) and would inherit the
//!   fence; a ref update needs the `packed-refs` lock in the common directory
//!   root, which only a grant on the whole directory (config and hooks too)
//!   gives; and a read-only copy into a private directory leaves the fetched
//!   commits out of the shared store the lander and the queue worktree read. The
//!   transport settings of the source (`core.sshCommand`, `core.gitProxy`,
//!   `remote.origin.uploadpack`, `credential.helper`, `url.*.insteadOf`) are
//!   left to its config on purpose: they carry the operator's authentication,
//!   and a source whose config is attacker-controlled is the same-uid case
//!   above, where the attacker already runs as the daemon's user.
//! * The lander subprocess (#1170, design only, nothing implemented): it is NOT
//!   a writer of the shared repository. `rsi-rolling-land` clones the queue
//!   worktree it is given with `git clone --shared --no-checkout` into a fresh
//!   0700 scratch directory and runs every merge, `update-ref`, commit, apply,
//!   `worktree add` and `push` there (the only commands it runs against the
//!   given repository are `remote get-url --push`, the clone's read of its
//!   objects and ref reads), publishing over the `publish` remote. Its writes
//!   are the scratch directory, the cargo target directory and the network. The
//!   gate it runs executes the accepted sources' whole test suite, which needs
//!   unrestricted writes, so the whole process cannot sit under this fence. The
//!   enforcement that would fit is a fence on the lander's own git calls into
//!   the given repository only (granting nothing: all of them are reads, and
//!   the clone writes only inside the scratch directory): wrap `git_ok(&repo,
//!   ..)`/`git_text(&repo, ..)` for the pre-clone calls in a no-write ruleset
//!   and give the clone a ruleset naming only the scratch handle. It needs the
//!   same ABI policy and a typed refusal in the lander's report, so it is a
//!   change to the lander (a separate issue), not to this runner.
//! * Deliberate behaviour changes of the private git directory: `clean -fd`
//!   no longer reads the operator's `info/exclude` or `core.excludesFile`;
//!   there is no per-worktree reflog; SHA-256 repositories and filter-driven
//!   content (LFS smudge) are not supported by the queue worktree.
//!
//! Landlock ABI 3 or newer is required: ABI 1 and 2 cannot deny `truncate`, so
//! the content-write boundary would have a hole. Anything older, or no
//! Landlock at all, refuses with [`UNSUPPORTED_CODE`]: fail closed.

use std::fs::File;
use std::process::Command;

/// The stable refusal code a queue batch is settled with when the host cannot
/// enforce the write fence.
pub(crate) const UNSUPPORTED_CODE: &str = "queue_write_fence_unsupported";

/// The oldest Landlock ABI that can deny every content write (truncation).
const MIN_ABI: i64 = 3;

/// The kernel's Landlock ABI version (an error when Landlock is unavailable).
pub(crate) type AbiProbe = fn() -> std::io::Result<i64>;

/// Apply the version policy to a probe result.
pub(crate) fn require_abi(probe: std::io::Result<i64>) -> Result<i64, String> {
    match probe {
        Ok(abi) if abi >= MIN_ABI => Ok(abi),
        Ok(abi) => Err(format!(
            "{UNSUPPORTED_CODE}: Landlock ABI {abi} cannot deny truncation (ABI {MIN_ABI} or \
             newer is required); refusing to run destructive git"
        )),
        Err(error) => Err(format!(
            "{UNSUPPORTED_CODE}: the kernel cannot enforce the queue's write fence \
             (Linux Landlock: {error}); refusing to run destructive git"
        )),
    }
}

/// A ruleset that permits writes only beneath the directories it was built from.
pub(super) struct WriteFence {
    #[cfg(target_os = "linux")]
    ruleset: std::os::fd::OwnedFd,
}

#[cfg(not(target_os = "linux"))]
impl WriteFence {
    pub(super) fn kernel_abi() -> std::io::Result<i64> {
        Err(std::io::Error::other("not Linux"))
    }

    pub(super) fn new(_writable: &[&File], probe: AbiProbe) -> Result<Self, String> {
        let _ = probe;
        Err(format!(
            "{UNSUPPORTED_CODE}: the queue's write fence needs Linux Landlock; \
             refusing to run destructive git"
        ))
    }

    pub(super) fn confine_on_exec(&self, _command: &mut Command) {}
}

#[cfg(target_os = "linux")]
mod access {
    pub(super) const WRITE_FILE: u64 = 1 << 1;
    pub(super) const REMOVE_DIR: u64 = 1 << 4;
    pub(super) const REMOVE_FILE: u64 = 1 << 5;
    pub(super) const MAKE_CHAR: u64 = 1 << 6;
    pub(super) const MAKE_DIR: u64 = 1 << 7;
    pub(super) const MAKE_REG: u64 = 1 << 8;
    pub(super) const MAKE_SOCK: u64 = 1 << 9;
    pub(super) const MAKE_FIFO: u64 = 1 << 10;
    pub(super) const MAKE_BLOCK: u64 = 1 << 11;
    pub(super) const MAKE_SYM: u64 = 1 << 12;
    /// ABI 2: re-parenting (rename or link across directories).
    pub(super) const REFER: u64 = 1 << 13;
    /// ABI 3: truncation.
    pub(super) const TRUNCATE: u64 = 1 << 14;
}

#[cfg(target_os = "linux")]
impl WriteFence {
    const CREATE_RULESET_VERSION: u32 = 1;
    const RULE_PATH_BENEATH: u32 = 1;
    const PR_SET_NO_NEW_PRIVS: nix::libc::c_int = 38;

    /// The kernel's Landlock ABI version.
    pub(super) fn kernel_abi() -> std::io::Result<i64> {
        // SAFETY: a version query takes a null attribute and size 0.
        let abi = unsafe {
            nix::libc::syscall(
                nix::libc::SYS_landlock_create_ruleset,
                std::ptr::null::<u8>(),
                0_usize,
                Self::CREATE_RULESET_VERSION,
            )
        };
        if abi < 1 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(i64::from(abi as i32))
    }

    /// Build a fence that allows writes only beneath the given open handles
    /// (directories) plus `/dev/null`. Fails when the kernel cannot enforce it
    /// completely (see [`require_abi`]).
    pub(super) fn new(writable: &[&File], probe: AbiProbe) -> Result<Self, String> {
        use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
        let unavailable = |error: std::io::Error| {
            format!(
                "{UNSUPPORTED_CODE}: the kernel cannot enforce the queue's write fence \
                 (Linux Landlock: {error}); refusing to run destructive git"
            )
        };
        require_abi(probe())?;
        let handled = access::WRITE_FILE
            | access::REMOVE_DIR
            | access::REMOVE_FILE
            | access::MAKE_CHAR
            | access::MAKE_DIR
            | access::MAKE_REG
            | access::MAKE_SOCK
            | access::MAKE_FIFO
            | access::MAKE_BLOCK
            | access::MAKE_SYM
            | access::REFER
            | access::TRUNCATE;
        let attr = handled.to_ne_bytes();
        // SAFETY: `attr` is a valid `landlock_ruleset_attr` prefix (the first
        // field, 8 bytes) and its length is passed.
        let fd = unsafe {
            nix::libc::syscall(
                nix::libc::SYS_landlock_create_ruleset,
                attr.as_ptr(),
                attr.len(),
                0_u32,
            )
        };
        if fd < 0 {
            return Err(unavailable(std::io::Error::last_os_error()));
        }
        // SAFETY: the syscall returned a new descriptor that nothing else owns.
        let ruleset = unsafe { OwnedFd::from_raw_fd(fd as nix::libc::c_int) };
        let null = File::open("/dev/null")
            .map_err(|error| format!("cannot open /dev/null for the write fence: {error}"))?;
        for handle in writable.iter().copied().chain(std::iter::once(&null)) {
            let is_dir = handle
                .metadata()
                .map_err(|error| format!("cannot stat a write fence handle: {error}"))?
                .is_dir();
            // A non-directory only takes the file rights.
            let allowed = match is_dir {
                true => handled,
                false => handled & (access::WRITE_FILE | access::TRUNCATE),
            };
            // `struct landlock_path_beneath_attr` is packed: u64 then s32.
            let mut rule = [0_u8; 12];
            rule[..8].copy_from_slice(&allowed.to_ne_bytes());
            rule[8..].copy_from_slice(&handle.as_raw_fd().to_ne_bytes());
            // SAFETY: `rule` is a valid packed `landlock_path_beneath_attr`.
            let added = unsafe {
                nix::libc::syscall(
                    nix::libc::SYS_landlock_add_rule,
                    ruleset.as_raw_fd(),
                    Self::RULE_PATH_BENEATH,
                    rule.as_ptr(),
                    0_u32,
                )
            };
            if added != 0 {
                return Err(unavailable(std::io::Error::last_os_error()));
            }
        }
        Ok(Self { ruleset })
    }

    /// Restrict the child between `fork` and `exec`: no new privileges, then the
    /// ruleset, inherited by every process it starts. If either call fails the
    /// spawn fails, so git never runs unfenced.
    pub(super) fn confine_on_exec(&self, command: &mut Command) {
        use std::os::fd::AsRawFd;
        use std::os::unix::process::CommandExt;
        let ruleset = self.ruleset.as_raw_fd();
        // SAFETY: the closure only calls `prctl` and `landlock_restrict_self`,
        // both async-signal-safe raw system calls, and never allocates.
        unsafe {
            command.pre_exec(move || {
                if nix::libc::prctl(Self::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) != 0 {
                    return Err(std::io::Error::last_os_error());
                }
                if nix::libc::syscall(nix::libc::SYS_landlock_restrict_self, ruleset, 0_u32) != 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
    }
}
