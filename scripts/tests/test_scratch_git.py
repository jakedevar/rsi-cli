"""scratch_git fails closed before Git can act on an assigned sandbox (#2571).

Every "assigned sandbox" here is a disposable fake repository in a temporary
directory; no test runs a mutating Git command in the real sandbox.
"""

import contextlib
import os
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path
from unittest import mock

SCRIPTS = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(SCRIPTS))
import scratch_git as SG  # noqa: E402

IDENTITY = ("-c", "user.name=Scratch Test", "-c", "user.email=scratch@example.invalid")


def plain_git(root, *args, env=None):
    """Setup/inspection of the fake assigned repo only, with an absolute -C."""
    assert Path(root).is_absolute() and Path(root).is_dir(), root
    # fixture-git: allow the fake assigned repo is the protected root, scratch_git refuses it by design
    return subprocess.run(
        ["git", "-C", str(root), *args], capture_output=True, text=True, check=True, env=env
    ).stdout.strip()


class Fixture(unittest.TestCase):
    def setUp(self):
        self.temp = SG.scratch_tempdir("rsi-scratch-git-test-")
        self.addCleanup(self.temp.cleanup)
        self.base = Path(self.temp.name).resolve()
        self.assigned = self.base / "assigned"
        self.assigned.mkdir()
        plain_git(self.assigned, "init", "-q", "-b", "assigned-branch")
        (self.assigned / "tracked.txt").write_text("base\n")
        plain_git(self.assigned, "add", "tracked.txt")
        plain_git(self.assigned, *IDENTITY, "commit", "-q", "-m", "base")

    def snapshot(self):
        return {
            "branch": plain_git(self.assigned, "symbolic-ref", "HEAD"),
            "head": plain_git(self.assigned, "rev-parse", "HEAD"),
            "tree": plain_git(self.assigned, "rev-parse", "HEAD^{tree}"),
            "refs": plain_git(self.assigned, "for-each-ref"),
            "status": plain_git(self.assigned, "status", "--porcelain"),
            "log": plain_git(self.assigned, "rev-list", "--all", "--count"),
        }

    @contextlib.contextmanager
    def working_in_assigned(self):
        """Run as a worker would: the process cwd is the assigned sandbox."""
        with contextlib.chdir(self.assigned):
            yield

    def scratch(self, name="scratch"):
        path = self.base / name
        path.mkdir()
        return path


class MissingDirectoryTest(Fixture):
    def test_missing_directory_fails_before_git_and_leaves_the_assigned_repo_unchanged(self):
        before = self.snapshot()
        missing = self.base / "never-created"
        with self.working_in_assigned(), mock.patch.object(SG.subprocess, "run") as run:
            for args in (
                ("init", "-q"),
                (*IDENTITY, "commit", "--allow-empty", "-m", "junk"),
                ("reset", "--mixed", "HEAD"),
                ("update-ref", "refs/heads/stray", "HEAD"),
            ):
                with self.assertRaises(SG.ScratchGitError):
                    SG.scratch_git(missing, *args)
            run.assert_not_called()
        self.assertEqual(self.snapshot(), before)

    def test_unsafe_directory_arguments_are_rejected_without_starting_git(self):
        before = self.snapshot()
        file_path = self.base / "plain-file"
        file_path.write_text("x")
        with self.working_in_assigned(), mock.patch.object(SG.subprocess, "run") as run:
            for bad in ("", ".", "scratch", "../assigned", None, file_path, "/"):
                with self.assertRaises(SG.ScratchGitError, msg=repr(bad)):
                    SG.scratch_git(bad, "status")
            run.assert_not_called()
        self.assertEqual(self.snapshot(), before)

    def test_the_old_unvalidated_cd_chain_is_what_this_prevents(self):
        # `cd missing; git commit` runs the commit in the current directory.
        before = self.snapshot()
        with self.working_in_assigned():
            with self.assertRaises(SG.ScratchGitError):
                SG.scratch_git(self.base / "missing", *IDENTITY, "commit", "--allow-empty", "-m", "x")
        self.assertEqual(self.snapshot(), before)


class ProtectedRootTest(Fixture):
    def test_directory_inside_the_current_worktree_is_rejected(self):
        inner = self.assigned / "nested"
        inner.mkdir()
        before = self.snapshot()
        with self.working_in_assigned():
            for target in (self.assigned, inner):
                with self.assertRaises(SG.ScratchGitError):
                    SG.scratch_git(target, *IDENTITY, "commit", "--allow-empty", "-m", "x")
        self.assertEqual(self.snapshot(), before)

    def test_symlink_into_the_assigned_repo_is_rejected(self):
        link = self.base / "looks-like-scratch"
        link.symlink_to(self.assigned, target_is_directory=True)
        before = self.snapshot()
        with self.assertRaises(SG.ScratchGitError):
            SG.scratch_git(link, "reset", "--mixed", "HEAD", extra_protected=[self.assigned])
        with self.working_in_assigned(), self.assertRaises(SG.ScratchGitError):
            SG.scratch_git(link, *IDENTITY, "commit", "--allow-empty", "-m", "x")
        self.assertEqual(self.snapshot(), before)

    def test_symlink_to_a_real_scratch_directory_is_followed_and_resolved(self):
        scratch = self.scratch()
        link = self.base / "link-to-scratch"
        link.symlink_to(scratch, target_is_directory=True)
        self.assertEqual(SG.validate_scratch_dir(link), scratch.resolve())
        SG.scratch_git(link, "init", "-q")
        self.assertTrue((scratch / ".git").exists())

    def test_this_checkout_is_always_protected(self):
        with self.assertRaises(SG.ScratchGitError):
            SG.validate_scratch_dir(SG.REPOSITORY_ROOT / "scripts")
        with self.assertRaises(SG.ScratchGitError):
            SG.validate_scratch_dir(SG.REPOSITORY_ROOT)

    def test_location_override_paths_are_protected_roots(self):
        scratch_inside_override = self.assigned / "scratch-inside"
        scratch_inside_override.mkdir()
        with mock.patch.dict(os.environ, {"GIT_WORK_TREE": str(self.assigned)}):
            with self.assertRaises(SG.ScratchGitError):
                SG.validate_scratch_dir(scratch_inside_override)

    def test_a_worktree_gitdir_pointer_is_protected(self):
        worktree = self.base / "linked"
        worktree.mkdir()
        shared_gitdir = self.base / "shared-gitdir"
        shared_gitdir.mkdir()
        (worktree / ".git").write_text(f"gitdir: {shared_gitdir}\n")
        inside_gitdir = shared_gitdir / "worktrees"
        inside_gitdir.mkdir()
        with contextlib.chdir(worktree), self.assertRaises(SG.ScratchGitError):
            SG.validate_scratch_dir(inside_gitdir)


class SandboxTmpdirTest(Fixture):
    """An RSI sandbox points TMPDIR inside itself (`target/.rsi-tmp`). The helper
    must reject that location like any other part of the sandbox, and fixtures
    must allocate outside it. `self.assigned` plays the sandbox."""

    def setUp(self):
        super().setUp()
        self.sandbox_tmp = self.assigned / "target" / ".rsi-tmp"
        self.inside = self.sandbox_tmp / "fixture"
        self.inside.mkdir(parents=True)
        self.before = self.snapshot()

    @contextlib.contextmanager
    def sandbox_environment(self):
        patches = (
            mock.patch.dict(os.environ, {"TMPDIR": str(self.sandbox_tmp)}),
            mock.patch.object(SG.tempfile, "gettempdir", return_value=str(self.sandbox_tmp)),
            mock.patch.object(tempfile, "tempdir", str(self.sandbox_tmp)),
        )
        with contextlib.ExitStack() as stack:
            for patch in patches:
                stack.enter_context(patch)
            with self.working_in_assigned():
                yield

    def test_the_sandbox_tmpdir_is_rejected_before_git_and_nothing_changes(self):
        with self.sandbox_environment():
            with mock.patch.object(SG.subprocess, "run") as run:
                for target in (self.inside, self.sandbox_tmp):
                    with self.assertRaises(SG.ScratchGitError):
                        SG.validate_scratch_dir(target)
                    with self.assertRaises(SG.ScratchGitError):
                        SG.scratch_git(target, "init", "-q")
                    with self.assertRaises(SG.ScratchGitError):
                        SG.scratch_git(target, *IDENTITY, "commit", "--allow-empty", "-m", "junk")
                run.assert_not_called()
        self.assertFalse((self.inside / ".git").exists())
        self.assertEqual(self.snapshot(), self.before)

    def test_fixtures_allocate_outside_the_sandbox_without_changing_tmpdir(self):
        with self.sandbox_environment():
            chosen = SG.host_temp_root()
            self.assertNotEqual(chosen, self.sandbox_tmp)
            self.assertNotIn(self.assigned, (chosen, *chosen.parents))
            self.assertEqual(SG.tempfile.gettempdir(), str(self.sandbox_tmp))
            self.assertEqual(os.environ["TMPDIR"], str(self.sandbox_tmp))
            with SG.scratch_tempdir("rsi-scratch-allocated-") as name:
                scratch = Path(name)
                self.assertNotIn(self.assigned, scratch.resolve().parents)
                SG.scratch_git(scratch, "init", "-q", "-b", "main")
                SG.scratch_git(scratch, *IDENTITY, "commit", "-q", "--allow-empty", "-m", "ok")
                self.assertEqual(
                    SG.scratch_git(scratch, "log", "-1", "--format=%s").stdout.strip(), "ok"
                )
        self.assertEqual(self.snapshot(), self.before)

    def test_no_allocation_when_every_candidate_is_inside_the_sandbox(self):
        with self.sandbox_environment(), mock.patch.object(SG, "HOST_TEMP_CANDIDATES", ()):
            with self.assertRaises(SG.ScratchGitError):
                SG.host_temp_root()
            with self.assertRaises(SG.ScratchGitError):
                SG.scratch_tempdir()


class EnclosingRepositoryTest(Fixture):
    """A job unit's private TMPDIR (`<jobs>/<id>.tmp`) can sit under a repository
    that has nothing to do with the job: the operator's home directory is one
    (#2612). `self.assigned` plays that enclosing repository; the job's
    `GIT_CEILING_DIRECTORIES` is its private tmp dir."""

    def setUp(self):
        super().setUp()
        self.job_tmp = self.assigned / "jobs" / "job-1.tmp"
        self.process_dir = self.job_tmp / ".tmpAbC123"
        self.process_dir.mkdir(parents=True)
        self.repo_dir = self.process_dir / "repo"
        self.repo_dir.mkdir()
        self.before = self.snapshot()

    def assert_enclosing_repository_untouched(self):
        """No ref, commit or tracked-file change; the job's own files are only
        ever untracked content of the enclosing repository."""
        after = self.snapshot()
        status = after.pop("status")
        before = dict(self.before)
        before.pop("status")
        self.assertEqual(after, before)
        self.assertTrue(
            all(line.startswith("?? ") for line in status.splitlines()), status
        )

    def job_environment(self, ceiling=None):
        env = {k: v for k, v in os.environ.items() if k != "GIT_CEILING_DIRECTORIES"}
        env["TMPDIR"] = str(self.job_tmp)
        if ceiling is not None:
            env["GIT_CEILING_DIRECTORIES"] = str(ceiling)
        return env

    def test_without_a_ceiling_the_enclosing_repository_is_still_protected(self):
        # The #2571 fail-closed rule: nothing declares that the enclosing
        # repository is out of reach, so it may be the assigned sandbox.
        with contextlib.chdir(self.process_dir):
            with self.assertRaises(SG.ScratchGitError) as raised:
                SG.validate_scratch_dir(self.repo_dir, environ=self.job_environment())
            self.assertIn("protected root", str(raised.exception))
            with mock.patch.dict(os.environ, self.job_environment(), clear=True):
                with mock.patch.object(SG.subprocess, "run") as run:
                    with self.assertRaises(SG.ScratchGitError):
                        SG.scratch_git(self.repo_dir, "init", "-q")
                    run.assert_not_called()
        self.assert_enclosing_repository_untouched()

    def test_a_job_ceiling_above_the_scratch_directory_makes_it_usable(self):
        env = self.job_environment(self.job_tmp)
        with contextlib.chdir(self.process_dir), mock.patch.dict(os.environ, env, clear=True):
            self.assertEqual(SG.validate_scratch_dir(self.repo_dir), self.repo_dir.resolve())
            SG.scratch_git(self.repo_dir, "init", "-q", "-b", "main")
            SG.scratch_git(self.repo_dir, *IDENTITY, "commit", "-q", "--allow-empty", "-m", "job")
            self.assertEqual(
                SG.scratch_git(self.repo_dir, "log", "-1", "--format=%s").stdout.strip(), "job"
            )
        self.assertEqual(plain_git(self.repo_dir, "symbolic-ref", "HEAD"), "refs/heads/main")
        self.assert_enclosing_repository_untouched()

    def test_the_job_ceiling_also_stops_plain_git_reaching_the_enclosing_repository(self):
        env = self.job_environment(self.job_tmp)
        plain = subprocess.run(
            ["git", "-C", str(self.repo_dir), "rev-parse", "--show-toplevel"], env=env,
            capture_output=True, text=True,
        )
        self.assertNotEqual(plain.returncode, 0)
        self.assertIn("not a git repository", plain.stderr)
        # Without it the same call finds the enclosing repository.
        found = subprocess.run(
            ["git", "-C", str(self.repo_dir), "rev-parse", "--show-toplevel"],
            env=self.job_environment(), capture_output=True, text=True,
        )
        self.assertEqual(Path(found.stdout.strip()).resolve(), self.assigned.resolve())

    def test_fixtures_allocate_in_the_job_tmpdir_under_the_ceiling(self):
        env = self.job_environment(self.job_tmp)
        with contextlib.chdir(self.process_dir), mock.patch.dict(os.environ, env, clear=True):
            with mock.patch.object(tempfile, "tempdir", str(self.job_tmp)):
                with SG.scratch_tempdir("rsi-scratch-job-") as name:
                    scratch = Path(name).resolve()
                    self.assertEqual(scratch.parent, self.job_tmp.resolve())
                    SG.scratch_git(scratch, "init", "-q", "-b", "main")
                    SG.scratch_git(scratch, *IDENTITY, "commit", "-q", "--allow-empty", "-m", "ok")
        self.assert_enclosing_repository_untouched()

    def test_a_ceiling_does_not_unprotect_the_checkout_overrides_or_extra_roots(self):
        env = self.job_environment(self.job_tmp)
        env["GIT_WORK_TREE"] = str(self.assigned)
        with contextlib.chdir(self.process_dir), mock.patch.dict(os.environ, env, clear=True):
            for target in (self.repo_dir, SG.REPOSITORY_ROOT / "scripts"):
                with self.assertRaises(SG.ScratchGitError, msg=str(target)):
                    SG.validate_scratch_dir(target)
        extra_env = self.job_environment(self.job_tmp)
        with contextlib.chdir(self.process_dir):
            with self.assertRaises(SG.ScratchGitError):
                SG.validate_scratch_dir(self.repo_dir, environ=extra_env, protected=[self.process_dir])

    def test_a_ceiling_above_the_repository_hides_nothing(self):
        # The ceiling is above the enclosing repo, so Git (and the guard) still
        # find it from a directory inside it.
        env = self.job_environment(self.base)
        with contextlib.chdir(self.process_dir):
            with self.assertRaises(SG.ScratchGitError):
                SG.validate_scratch_dir(self.repo_dir, environ=env)

    def test_a_ceiling_equal_to_the_current_directory_hides_nothing(self):
        env = self.job_environment(self.process_dir)
        with contextlib.chdir(self.process_dir):
            with self.assertRaises(SG.ScratchGitError):
                SG.validate_scratch_dir(self.repo_dir, environ=env)

    def test_relative_and_empty_ceiling_entries_are_ignored_like_git_does(self):
        self.assertEqual(SG.ceiling_directories({"GIT_CEILING_DIRECTORIES": ""}), ())
        self.assertEqual(SG.ceiling_directories({"GIT_CEILING_DIRECTORIES": ":relative/dir:"}), ())
        joined = os.pathsep.join(("", "relative", str(self.job_tmp), str(self.base)))
        self.assertEqual(
            SG.ceiling_directories({"GIT_CEILING_DIRECTORIES": joined}),
            (self.job_tmp.resolve(), self.base),
        )
        with contextlib.chdir(self.process_dir):
            with self.assertRaises(SG.ScratchGitError):
                SG.validate_scratch_dir(
                    self.repo_dir,
                    environ=self.job_environment(os.pathsep + "relative/job-1.tmp"),
                )

    def test_a_symlinked_ceiling_entry_is_resolved(self):
        link = self.base / "job-tmp-link"
        link.symlink_to(self.job_tmp, target_is_directory=True)
        env = self.job_environment(link)
        with contextlib.chdir(self.process_dir):
            self.assertEqual(SG.validate_scratch_dir(self.repo_dir, environ=env), self.repo_dir.resolve())

    def test_entries_after_an_empty_entry_are_not_resolved_like_git(self):
        link = self.base / "job-tmp-link"
        link.symlink_to(self.job_tmp, target_is_directory=True)
        dotted = str(self.base / "x" / ".." / "job-tmp-link")
        joined = os.pathsep.join((str(link), "", dotted))
        self.assertEqual(
            SG.ceiling_directories({"GIT_CEILING_DIRECTORIES": joined}),
            (self.job_tmp.resolve(), link),
        )

    def test_a_symlinked_ceiling_after_an_empty_entry_keeps_the_caller_repository_protected(self):
        # Git does not resolve this ceiling, so it never matches the physical
        # cwd and the caller's repository is still discovered; the guard must
        # agree and protect it instead of treating the resolved link as a ceiling.
        caller = self.base / "caller"
        caller.mkdir()
        plain_git(caller, "init", "-q", "-b", "caller-branch")
        work = caller / "sub"
        work.mkdir()
        link = self.base / "caller-link"
        link.symlink_to(caller, target_is_directory=True)
        ceiling = os.pathsep + str(link)
        env = dict(os.environ)
        env["GIT_CEILING_DIRECTORIES"] = ceiling
        env.pop("GIT_DIR", None)
        env.pop("GIT_WORK_TREE", None)
        self.assertEqual(
            Path(plain_git(work, "rev-parse", "--show-toplevel", env=env)).resolve(),
            caller.resolve(),
        )
        with contextlib.chdir(work):
            self.assertIn(caller.resolve(), SG.protected_roots(env))
            with self.assertRaises(SG.ScratchGitError):
                SG.validate_scratch_dir(caller, environ=env)


class IsolationTest(Fixture):
    def test_location_environment_cannot_target_the_assigned_repo(self):
        scratch = self.scratch()
        before = self.snapshot()
        hostile = {
            "GIT_DIR": str(self.assigned / ".git"),
            "GIT_WORK_TREE": str(self.assigned),
            "GIT_INDEX_FILE": str(self.assigned / ".git" / "index"),
            "GIT_COMMON_DIR": str(self.assigned / ".git"),
        }
        with mock.patch.dict(os.environ, hostile), self.working_in_assigned():
            SG.scratch_git(scratch, "init", "-q", "-b", "scratch-main")
            (scratch / "f.txt").write_text("scratch\n")
            SG.scratch_git(scratch, "add", "f.txt")
            SG.scratch_git(scratch, *IDENTITY, "commit", "-q", "-m", "scratch commit")
            SG.scratch_git(scratch, "branch", "-f", "stray", "HEAD")
        self.assertEqual(self.snapshot(), before)
        self.assertEqual(plain_git(scratch, "log", "-1", "--format=%s"), "scratch commit")
        self.assertEqual(plain_git(scratch, "symbolic-ref", "HEAD"), "refs/heads/scratch-main")

    def test_an_explicit_env_argument_is_cleaned_too(self):
        scratch = self.scratch()
        before = self.snapshot()
        env = dict(os.environ, GIT_DIR=str(self.assigned / ".git"))
        SG.scratch_git(scratch, "init", "-q", env=env)
        self.assertTrue((scratch / ".git").is_dir())
        self.assertEqual(self.snapshot(), before)

    def test_override_options_before_the_subcommand_are_rejected(self):
        scratch = self.scratch()
        before = self.snapshot()
        for args in (
            ("-C", str(self.assigned), "status"),
            (f"--git-dir={self.assigned}/.git", "status"),
            ("--work-tree", str(self.assigned), "status"),
            ("-c", "core.x=1", "-C", str(self.assigned), "status"),
        ):
            with self.assertRaises(SG.ScratchGitError, msg=args):
                SG.scratch_git(scratch, *args)
        self.assertEqual(self.snapshot(), before)

    def test_a_subcommand_option_named_dash_c_is_not_an_override(self):
        scratch = self.scratch()
        SG.scratch_git(scratch, "init", "-q")
        SG.scratch_git(scratch, *IDENTITY, "commit", "-q", "--allow-empty", "-m", "first")
        SG.scratch_git(scratch, *IDENTITY, "commit", "-q", "--allow-empty", "-C", "HEAD")
        self.assertEqual(SG.scratch_git(scratch, "rev-list", "--count", "HEAD").stdout.strip(), "2")

    def test_a_scratch_directory_that_is_not_a_repo_does_not_find_an_enclosing_repo(self):
        outer = self.scratch("outer")
        plain_git(outer, "init", "-q")
        inner = outer / "inner"
        inner.mkdir()
        outer_before = plain_git(outer, "status", "--porcelain=v2", "--branch")
        result = SG.scratch_git(inner, *IDENTITY, "commit", "--allow-empty", "-m", "x", check=False)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("not a git repository", result.stderr)
        self.assertEqual(plain_git(outer, "status", "--porcelain=v2", "--branch"), outer_before)
        self.assertEqual(plain_git(outer, "rev-list", "--all", "--count"), "0")

    def test_valid_isolated_fixture_repo_supports_ordinary_git(self):
        scratch = self.scratch()
        before = self.snapshot()
        SG.scratch_git(scratch, "init", "-q", "-b", "main")
        (scratch / "a.txt").write_text("a\n")
        SG.scratch_git(scratch, "add", "a.txt")
        SG.scratch_git(scratch, *IDENTITY, "commit", "-q", "-m", "first")
        SG.scratch_git(scratch, "branch", "side")
        SG.scratch_git(scratch, "checkout", "-q", "side")
        SG.scratch_git(scratch, *IDENTITY, "commit", "-q", "--allow-empty", "-m", "second")
        SG.scratch_git(scratch, "reset", "--mixed", "HEAD~1")
        self.assertEqual(SG.scratch_git(scratch, "rev-parse", "--abbrev-ref", "HEAD").stdout.strip(), "side")
        self.assertEqual(SG.scratch_git(scratch, "log", "--format=%s").stdout.strip(), "first")
        with self.assertRaises(subprocess.CalledProcessError):
            SG.scratch_git(scratch, "rev-parse", "--verify", "no-such-ref")
        self.assertEqual(self.snapshot(), before)


if __name__ == "__main__":
    unittest.main()
