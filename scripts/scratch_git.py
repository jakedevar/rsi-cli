"""Run fixture Git only inside a validated scratch directory (#2571, #224).

A fixture that does `cd "$scratch" && git commit` (or a Python helper that
runs `git` with an implicit working directory) acts on whatever directory it
happens to be in when the `cd` or the setup failed. In an RSI sandbox that is the
worker's own assigned worktree, and a stray `git commit` or `git reset` there
moves the assigned branch (AGENTS.md hard rule 2).

`scratch_git(directory, *args)` is the one way scripts and tests run fixture
Git. It checks that `directory` is an absolute, existing directory whose
resolved physical path is outside every protected root (this checkout, the
working tree around the current directory, and any `GIT_DIR`/`GIT_WORK_TREE`
location override) before it starts Git. There is no exemption: an RSI sandbox
points `TMPDIR` at `<sandbox>/target/.rsi-tmp`, which is inside the protected
root and is rejected like any other path there. Fixtures allocate their
directories with `scratch_tempdir()`, which picks a host temporary location
outside every protected root without touching `TMPDIR` or `tempfile.tempdir`.
It then runs `git -C <resolved path>`
with every repository-location environment variable removed, so a
`GIT_DIR=<assigned worktree>/.git` in the caller's environment cannot redirect
the command, and with `GIT_CEILING_DIRECTORIES` set so a scratch directory that
is not a repository does not discover an enclosing one.

The working tree around the current directory is found the way Git finds it,
including `GIT_CEILING_DIRECTORIES` in the caller's environment (#2612, #2634): a job
unit sets that variable to its private `TMPDIR` so a repository that merely
encloses the job's temporary directory (the operator's home directory is one) is
neither reachable by plain Git there nor mistaken for the assigned sandbox here.
A ceiling only hides ancestors above a directory the caller chose; this checkout,
`GIT_DIR`/`GIT_WORK_TREE`/`GIT_COMMON_DIR` and the caller's `extra` roots stay
protected whatever the ceiling says.

Shell fixtures follow the same rule without this module: `git -C <absolute
validated dir> ...` or an explicit `GIT_DIR`/`GIT_WORK_TREE`, never
`cd <dir> && git ...`.
"""

import os
import subprocess
import tempfile
from pathlib import Path

# Environment variables that tell Git which repository, index or object store
# to use, whatever the working directory is.
LOCATION_VARIABLES = (
    "GIT_DIR",
    "GIT_WORK_TREE",
    "GIT_COMMON_DIR",
    "GIT_INDEX_FILE",
    "GIT_OBJECT_DIRECTORY",
    "GIT_ALTERNATE_OBJECT_DIRECTORIES",
    "GIT_NAMESPACE",
    "GIT_PREFIX",
    "GIT_CEILING_DIRECTORIES",
    "GIT_DISCOVERY_ACROSS_FILESYSTEM",
)

# Global options that would move the command off the validated directory.
OVERRIDE_OPTIONS = ("-C", "--git-dir", "--work-tree", "--namespace", "--super-prefix")

REPOSITORY_ROOT = Path(__file__).resolve().parents[1]


class ScratchGitError(ValueError):
    """The scratch directory is unsafe; Git was not started."""


def ceiling_directories(environ=None):
    """Absolute entries of `GIT_CEILING_DIRECTORIES`, canonicalized as Git does.

    Relative entries are ignored. An empty entry is skipped, but it switches the
    entries after it to lexical normalization only: Git then keeps them without
    resolving symlinks (`canonicalize_ceiling_entry`), so a symlinked entry after
    an empty one names the link's own location, not its target (#2634). Entries
    before any empty entry are resolved physically.
    """
    environ = os.environ if environ is None else environ
    ceilings = []
    resolve = True
    for entry in (environ.get("GIT_CEILING_DIRECTORIES") or "").split(os.pathsep):
        if not entry:
            resolve = False
        elif os.path.isabs(entry):
            if resolve:
                try:
                    ceilings.append(Path(entry).resolve())
                except OSError:
                    continue
            else:
                ceilings.append(Path(os.path.normpath(entry)))
    return tuple(ceilings)


def _worktree_root(start, ceilings=()):
    """The directory holding `.git` at or above `start`, or None.

    Like Git's discovery, the walk stops before a ceiling directory that is a
    proper ancestor of `start`: the ceiling itself and everything above it is not
    looked at. A ceiling equal to `start` hides nothing.
    """
    for candidate in (start, *start.parents):
        if candidate != start and candidate in ceilings:
            return None
        if (candidate / ".git").exists():
            return candidate
    return None


def _gitdir_of(root):
    """The git directory a worktree's `.git` entry points at, or None."""
    entry = root / ".git"
    if entry.is_dir():
        return entry.resolve()
    try:
        text = entry.read_text(encoding="utf-8", errors="replace")
    except OSError:
        return None
    prefix = "gitdir:"
    if not text.startswith(prefix):
        return None
    target = Path(text[len(prefix):].strip())
    if not target.is_absolute():
        target = root / target
    return target.resolve()


def protected_roots(environ=None, extra=()):
    """Resolved roots a scratch directory must stay outside of."""
    environ = os.environ if environ is None else environ
    roots = [REPOSITORY_ROOT]
    try:
        current = _worktree_root(Path.cwd().resolve(), ceiling_directories(environ))
    except OSError:
        current = None
    if current is not None:
        roots.append(current)
    for root in tuple(roots):
        gitdir = _gitdir_of(root)
        if gitdir is not None:
            roots.append(gitdir)
    for name in ("GIT_DIR", "GIT_WORK_TREE", "GIT_COMMON_DIR"):
        value = environ.get(name)
        if value:
            roots.append(Path(value).resolve())
    roots.extend(Path(path).resolve() for path in extra)
    return tuple(dict.fromkeys(roots))


def validate_scratch_dir(directory, environ=None, protected=()):
    """The resolved physical scratch path, or raise `ScratchGitError`.

    `directory` must be absolute and an existing directory (symlinks are
    followed, and the resolved path is what is returned and used), and neither
    it nor any parent may be a protected root.
    """
    if not isinstance(directory, (str, os.PathLike)) or not os.fspath(directory):
        raise ScratchGitError("scratch directory is empty")
    text = os.fspath(directory)
    if not os.path.isabs(text):
        raise ScratchGitError(f"scratch directory is not absolute: {text!r}")
    try:
        resolved = Path(text).resolve(strict=True)
    except OSError as error:
        raise ScratchGitError(f"scratch directory does not exist: {text!r} ({error})") from error
    if not resolved.is_dir():
        raise ScratchGitError(f"scratch path is not a directory: {text!r}")
    if resolved == Path(resolved.anchor):
        raise ScratchGitError("scratch directory is the filesystem root")
    for root in protected_roots(environ, protected):
        if resolved == root or root in resolved.parents:
            raise ScratchGitError(
                f"scratch directory {text!r} resolves to {str(resolved)!r}, inside protected "
                f"root {str(root)!r}; Git there would act on the assigned sandbox"
            )
    return resolved


# Disk-backed locations first: a fixture that reads index bytes or compares
# device ids (test_rsi_stash) is not stable on a tmpfs /tmp.
HOST_TEMP_CANDIDATES = ("/var/tmp", "/private/tmp", "/tmp", "/dev/shm")


def host_temp_root(environ=None, protected=()):
    """A writable existing temporary directory outside every protected root.

    Tries the process temp directory (`TMPDIR`) first, then the usual host
    locations. Raises `ScratchGitError` when none qualifies; never changes
    `TMPDIR` or `tempfile.tempdir`.
    """
    roots = protected_roots(environ, protected)
    for candidate in (tempfile.gettempdir(), *HOST_TEMP_CANDIDATES):
        try:
            resolved = Path(candidate).resolve(strict=True)
        except OSError:
            continue
        if not resolved.is_dir() or not os.access(resolved, os.W_OK):
            continue
        if any(resolved == root or root in resolved.parents for root in roots):
            continue
        return resolved
    raise ScratchGitError("no temporary directory outside the protected roots is available")


def scratch_tempdir(prefix="rsi-scratch-", environ=None, protected=()):
    """A `tempfile.TemporaryDirectory` created under `host_temp_root()`."""
    return tempfile.TemporaryDirectory(
        prefix=prefix, dir=host_temp_root(environ, protected)
    )


def clean_environment(resolved, base=None):
    """`base` (default `os.environ`) without repository-location variables, with a
    ceiling above the scratch directory."""
    environment = dict(os.environ if base is None else base)
    for name in LOCATION_VARIABLES:
        environment.pop(name, None)
    environment["GIT_CEILING_DIRECTORIES"] = str(resolved.parent)
    return environment


def reject_location_overrides(args):
    """Raise when a global option before the Git subcommand would move the
    command off the validated directory (`git commit -C <rev>` is not one)."""
    index = 0
    while index < len(args):
        arg = args[index]
        if arg in OVERRIDE_OPTIONS or arg.startswith(("--git-dir=", "--work-tree=")):
            raise ScratchGitError(f"git option {arg!r} would override the scratch directory")
        if arg in ("-c", "--config-env"):
            index += 2
        elif arg.startswith("-"):
            index += 1
        else:
            return


def scratch_git(directory, *args, check=True, env=None, input=None, extra_protected=()):
    """Run `git -C <validated directory> <args>`; return the `CompletedProcess`
    (text, stdout and stderr captured). Raises `ScratchGitError` before Git
    starts when the directory is unsafe, `CalledProcessError` when `check` and
    Git fails."""
    reject_location_overrides(args)
    base = os.environ if env is None else env
    resolved = validate_scratch_dir(directory, environ=base, protected=extra_protected)
    return subprocess.run(
        ["git", "-C", str(resolved), *args],
        env=clean_environment(resolved, base),
        input=input,
        capture_output=True,
        text=True,
        check=check,
    )
