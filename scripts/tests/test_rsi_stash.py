import os
import shutil
import subprocess
import tempfile
import unittest
from pathlib import Path


SCRIPT = Path(__file__).resolve().parents[1] / "rsi-stash"


class RsiStashTest(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix="rsi-stash-test-")
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.repository = self.root / "repository"
        self.repository.mkdir()
        self.git("init", "-q", "-b", "main")
        self.git("config", "user.name", "Rsi Stash Test")
        self.git("config", "user.email", "rsi-stash@example.invalid")
        (self.repository / "shared.txt").write_text("base\n")
        self.commit("base")

    def git(self, *args, worktree=None):
        return subprocess.check_output(
            ["git", "-C", str(worktree or self.repository), *args],
            text=True,
        ).strip()

    def commit(self, message):
        self.git("add", ".")
        self.git("commit", "-q", "-m", message)
        return self.git("rev-parse", "HEAD")

    def linked_worktree(self, name):
        path = self.root / name
        branch = f"feature-{name}"
        self.git("worktree", "add", "-b", branch, str(path))
        return path

    def stash(self, worktree, filename, contents):
        (worktree / filename).write_text(contents)
        output = subprocess.check_output(
            [str(SCRIPT), "save", f"save-{filename}"],
            cwd=worktree,
            text=True,
        ).strip()
        return output

    def assert_worktree_contents(self, worktree, expected):
        self.assertEqual((worktree / "shared.txt").read_text(), expected)

    def run_stash(self, worktree, *args, env=None):
        return subprocess.run(
            [str(SCRIPT), *args], cwd=worktree, env=env,
            text=True, capture_output=True,
        )

    def state(self, worktree):
        return (
            self.git("status", "--porcelain=v1", "--untracked-files=all", worktree=worktree),
            self.git("ls-files", "-s", worktree=worktree),
            (worktree / "shared.txt").read_text(),
        )

    def git_wrapper(self, body):
        wrapper_dir = self.root / "bin"
        wrapper_dir.mkdir(exist_ok=True)
        wrapper = wrapper_dir / "git"
        wrapper.write_text("#!/usr/bin/env bash\n" + body)
        wrapper.chmod(0o755)
        return {**os.environ, "PATH": f"{wrapper_dir}:{os.environ['PATH']}"}

    def ignored_build_tree(self, worktree):
        excludes = self.root / "excludes"
        excludes.write_text("target/\n")
        self.git("config", "core.excludesFile", str(excludes))
        target = worktree / "target"
        target.mkdir()
        (target / "large.bin").write_bytes(b"x" * (4 * 1024 * 1024))
        return target

    def test_linked_worktrees_cannot_consume_each_others_entries(self):
        first = self.linked_worktree("first")
        second = self.linked_worktree("second")

        first_sha = self.stash(first, "first.txt", "first work\n")
        second_sha = self.stash(second, "second.txt", "second work\n")

        first_list = subprocess.check_output(
            [str(SCRIPT), "list"], cwd=first, text=True
        ).splitlines()
        second_list = subprocess.check_output(
            [str(SCRIPT), "list"], cwd=second, text=True
        ).splitlines()
        self.assertEqual([row.split("\t")[0] for row in first_list], [first_sha])
        self.assertEqual([row.split("\t")[0] for row in second_list], [second_sha])

        for owner, foreign_sha in ((first, second_sha), (second, first_sha)):
            before = self.state(owner)
            other = second if owner == first else first
            other_before = self.state(other)
            for command in ("restore", "drop"):
                result = self.run_stash(owner, command, foreign_sha)
                self.assertNotEqual(result.returncode, 0)
                self.assertIn("not owned by this worktree", result.stderr)
                self.assertEqual(self.state(owner), before)
                self.assertEqual(self.state(other), other_before)

        subprocess.check_call([str(SCRIPT), "restore", first_sha], cwd=first)
        subprocess.check_call([str(SCRIPT), "restore", second_sha], cwd=second)

        self.assert_worktree_contents(first, "base\n")
        self.assert_worktree_contents(second, "base\n")
        self.assertTrue((first / "first.txt").is_file())
        self.assertTrue((second / "second.txt").is_file())

        subprocess.check_call([str(SCRIPT), "drop", first_sha], cwd=first)
        first_list_after_drop = subprocess.check_output(
            [str(SCRIPT), "list"], cwd=first, text=True
        ).strip()
        second_list_after_other_drop = subprocess.check_output(
            [str(SCRIPT), "list"], cwd=second, text=True
        ).strip()
        self.assertEqual(first_list_after_drop, "")
        self.assertIn(second_sha, second_list_after_other_drop)

    def test_save_captures_tracked_and_untracked_changes(self):
        worktree = self.linked_worktree("capture")
        (worktree / "shared.txt").write_text("modified\n")
        subprocess.check_call(["git", "add", "shared.txt"], cwd=worktree)
        sha = self.stash(worktree, "untracked.txt", "new work\n")

        self.assert_worktree_contents(worktree, "base\n")
        self.assertFalse((worktree / "untracked.txt").exists())
        subprocess.check_call([str(SCRIPT), "restore", sha], cwd=worktree)
        self.assert_worktree_contents(worktree, "modified\n")
        self.assertEqual((worktree / "untracked.txt").read_text(), "new work\n")

    def test_stash_layout_restores_index_worktree_and_untracked_parent(self):
        worktree = self.linked_worktree("layout")
        (worktree / "shared.txt").write_text("staged\n")
        self.git("add", "shared.txt", worktree=worktree)
        (worktree / "shared.txt").write_text("unstaged\n")
        (worktree / "new.txt").write_text("untracked\n")
        sha = self.run_stash(worktree, "save", "layout").stdout.strip()

        self.assertEqual(len(self.git("rev-list", "--parents", "-n", "1", sha).split()), 4)
        self.assertEqual(self.git("show", f"{sha}^2:shared.txt"), "staged")
        self.assertEqual(self.git("show", f"{sha}:shared.txt"), "unstaged")
        self.assertEqual(self.git("show", f"{sha}^3:new.txt"), "untracked")

        result = self.run_stash(worktree, "restore", sha)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(result.stdout.strip(), sha)
        self.assertEqual(self.git("show", ":shared.txt", worktree=worktree), "staged")
        self.assertEqual((worktree / "shared.txt").read_text(), "unstaged\n")
        self.assertEqual((worktree / "new.txt").read_text(), "untracked\n")

    def test_staged_deletion_with_untracked_replacement_cleans_safely(self):
        worktree = self.linked_worktree("replacement")
        self.git("rm", "-q", "shared.txt", worktree=worktree)
        (worktree / "shared.txt").write_text("replacement\n")

        result = self.run_stash(worktree, "save", "replacement")
        self.assertEqual(result.returncode, 0, result.stderr)
        sha = result.stdout.strip()
        self.assertEqual((worktree / "shared.txt").read_text(), "base\n")
        self.assertEqual(self.git("status", "--porcelain", worktree=worktree), "")
        self.assertEqual(self.git("show", f"{sha}^3:shared.txt"), "replacement")

    def test_save_from_subdirectory_captures_root_changes(self):
        worktree = self.linked_worktree("subdirectory")
        subdirectory = worktree / "sub"
        subdirectory.mkdir()
        (worktree / "shared.txt").write_text("changed\n")
        (subdirectory / "new.txt").write_text("new\n")

        result = self.run_stash(subdirectory, "save", "from subdirectory")
        self.assertEqual(result.returncode, 0, result.stderr)
        sha = result.stdout.strip()
        self.assertEqual((worktree / "shared.txt").read_text(), "base\n")
        self.assertEqual(self.git("status", "--porcelain", worktree=worktree), "")
        self.assertEqual(self.git("show", f"{sha}^3:sub/new.txt"), "new")

    def test_save_cleanup_failure_rolls_back_with_ref_preserved(self):
        worktree = self.linked_worktree("rollback")
        (worktree / "shared.txt").write_text("changed\n")
        (worktree / "new.txt").write_text("new\n")
        before = self.state(worktree)
        real_git = subprocess.check_output(["which", "git"], text=True).strip()
        env = self.git_wrapper(
            f'if [[ "$1" == restore ]]; then "{real_git}" "$@"; '
            'printf "later\\n" > late.txt; exit 72; fi\n'
            f'exec "{real_git}" "$@"\n'
        )
        env["TMPDIR"] = str(self.root)
        result = self.run_stash(worktree, "save", "failure", env=env)

        self.assertNotEqual(result.returncode, 0)
        self.assertIn("working tree and index restored", result.stderr)
        self.assertEqual(self.state(worktree)[1:], before[1:])
        self.assertEqual((worktree / "new.txt").read_text(), "new\n")
        self.assertEqual((worktree / "late.txt").read_text(), "later\n")
        recovery_path = Path(result.stderr.split("recovery snapshot: ")[-1].splitlines()[0])
        self.assertFalse((recovery_path / "tree" / "late.txt").exists())
        saved_sha = self.run_stash(worktree, "list").stdout.split("\t")[0]
        self.assertEqual(self.git("show", f"{saved_sha}:shared.txt"), "changed")
        self.assertEqual(self.git("show", f"{saved_sha}^3:new.txt"), "new")

    def test_new_file_after_capture_survives_aborted_save(self):
        worktree = self.linked_worktree("late")
        (worktree / "shared.txt").write_text("changed\n")
        real_git = subprocess.check_output(["which", "git"], text=True).strip()
        env = self.git_wrapper(
            f'if [[ "$1" == update-ref ]]; then "{real_git}" "$@" || exit $?; '
            'printf "later\\n" > late.txt; exit 0; fi\n'
            f'exec "{real_git}" "$@"\n'
        )
        result = self.run_stash(worktree, "save", "late", env=env)

        self.assertNotEqual(result.returncode, 0)
        self.assertIn("changed during capture", result.stderr)
        self.assertEqual((worktree / "shared.txt").read_text(), "changed\n")
        self.assertEqual((worktree / "late.txt").read_text(), "later\n")
        saved_sha = self.run_stash(worktree, "list").stdout.split("\t")[0]
        self.assertEqual(self.git("show", f"{saved_sha}:shared.txt"), "changed")

    def test_interrupted_cleanup_restores_original_and_keeps_capture(self):
        worktree = self.linked_worktree("interrupted")
        (worktree / "shared.txt").write_text("changed\n")
        (worktree / "new.txt").write_text("new\n")
        before = self.state(worktree)
        real_git = subprocess.check_output(["which", "git"], text=True).strip()
        env = self.git_wrapper(
            f'if [[ "$1" == restore ]]; then "{real_git}" "$@" || exit $?; '
            'kill -TERM "$PPID"; exit 0; fi\n'
            f'exec "{real_git}" "$@"\n'
        )
        result = self.run_stash(worktree, "save", "interrupted", env=env)

        self.assertNotEqual(result.returncode, 0)
        self.assertIn("working tree and index restored", result.stderr)
        self.assertEqual(self.state(worktree), before)
        self.assertEqual((worktree / "new.txt").read_text(), "new\n")
        saved_sha = self.run_stash(worktree, "list").stdout.split("\t")[0]
        self.assertEqual(self.git("show", f"{saved_sha}:shared.txt"), "changed")

    def test_restore_conflict_keeps_tree_and_index_intact(self):
        worktree = self.linked_worktree("conflict")
        (worktree / "shared.txt").write_text("saved\n")
        sha = self.run_stash(worktree, "save", "conflict").stdout.strip()
        (worktree / "shared.txt").write_text("current\n")
        self.git("add", "shared.txt", worktree=worktree)
        before = self.state(worktree)

        result = self.run_stash(worktree, "restore", sha)
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual(self.state(worktree), before)
        self.assertEqual(self.git("show", ":shared.txt", worktree=worktree), "current")
        self.assertEqual((worktree / "shared.txt").read_text(), "current\n")

    def test_restore_apply_failure_rolls_back_original_state(self):
        worktree = self.linked_worktree("restore-failure")
        (worktree / "shared.txt").write_text("saved\n")
        sha = self.run_stash(worktree, "save", "restore failure").stdout.strip()
        before = self.state(worktree)
        real_git = subprocess.check_output(["which", "git"], text=True).strip()
        env = self.git_wrapper(
            f'if [[ "$1" == stash && "$2" == apply ]]; then "{real_git}" "$@" || exit $?; '
            'exit 73; fi\n'
            f'exec "{real_git}" "$@"\n'
        )
        env["TMPDIR"] = str(self.root)

        result = self.run_stash(worktree, "restore", sha, env=env)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("working tree and index restored", result.stderr)
        self.assertEqual(self.state(worktree), before)
        self.assertEqual((worktree / "shared.txt").read_text(), "base\n")

    def test_save_failure_keeps_ignored_tree_out_of_snapshot(self):
        worktree = self.linked_worktree("ignored-save")
        target = self.ignored_build_tree(worktree)
        (worktree / "shared.txt").write_text("changed\n")
        before = self.state(worktree)
        index = Path(self.git("rev-parse", "--git-path", "index", worktree=worktree))
        index_before = index.read_bytes()
        real_git = subprocess.check_output(["which", "git"], text=True).strip()
        env = self.git_wrapper(
            f'if [[ "$1" == restore ]]; then "{real_git}" "$@"; exit 72; fi\n'
            f'exec "{real_git}" "$@"\n'
        )

        result = self.run_stash(worktree, "save", "ignored", env=env)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("working tree and index restored", result.stderr)
        self.assertEqual(self.state(worktree), before)
        self.assertEqual(index.read_bytes(), index_before)
        self.assertEqual((target / "large.bin").stat().st_size, 4 * 1024 * 1024)
        recovery_path = Path(result.stderr.split("recovery snapshot: ")[-1].splitlines()[0])
        self.assertEqual(list(recovery_path.rglob("large.bin")), [])
        self.assertEqual((recovery_path / "tree" / "shared.txt").read_text(), "changed\n")

    def test_restore_preflight_and_rollback_skip_ignored_tree(self):
        worktree = self.linked_worktree("ignored-restore")
        (worktree / "shared.txt").write_text("saved\n")
        sha = self.run_stash(worktree, "save", "ignored restore").stdout.strip()
        target = self.ignored_build_tree(worktree)
        before = self.state(worktree)
        index = Path(self.git("rev-parse", "--git-path", "index", worktree=worktree))
        index_before = index.read_bytes()
        real_git = subprocess.check_output(["which", "git"], text=True).strip()
        probe = self.root / "preflight-probe"
        env = self.git_wrapper(
            'if [[ "$1" == -C && "$3" == stash && "$4" == apply ]]; then '
            '[[ ! -e "$2/target" ]] || exit 91; '
            '[[ "$(stat -c %d "$2")" == "$EXPECTED_DEVICE" ]] || exit 92; '
            'printf checked > "$PROBE_FILE"; fi\n'
            f'if [[ "$1" == stash && "$2" == apply ]]; then "{real_git}" "$@" || exit $?; '
            'exit 73; fi\n'
            f'exec "{real_git}" "$@"\n'
        )
        env["EXPECTED_DEVICE"] = str(worktree.stat().st_dev)
        env["PROBE_FILE"] = str(probe)

        result = self.run_stash(worktree, "restore", sha, env=env)
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual(probe.read_text(), "checked")
        self.assertEqual(self.state(worktree), before)
        self.assertEqual(index.read_bytes(), index_before)
        self.assertEqual((target / "large.bin").stat().st_size, 4 * 1024 * 1024)
        recovery_path = Path(result.stderr.split("recovery snapshot: ")[-1].splitlines()[0])
        self.assertEqual(list(recovery_path.rglob("large.bin")), [])

    def test_detached_recovery_copy_preserves_original_index_and_namespace(self):
        original = self.linked_worktree("recovery-source")
        existing_sha = self.stash(original, "older.txt", "older\n")
        (original / "shared.txt").write_text("staged\n")
        self.git("add", "shared.txt", worktree=original)
        (original / "shared.txt").write_text("unstaged\n")
        (original / "new.txt").write_text("untracked\n")
        original_index = Path(self.git("rev-parse", "--git-path", "index", worktree=original))
        index_before = original_index.read_bytes()
        status_before = self.state(original)
        list_before = self.run_stash(original, "list").stdout

        # Mirror the documented safe-copy procedure: detached worktree, selected
        # tracked/untracked paths, then the original index in the detached index.
        detached = self.root / "recovery-copy"
        self.git("worktree", "add", "--detach", str(detached), "HEAD")
        path_lists = [
            ("ls-files", "--cached", "-z"),
            ("ls-files", "--others", "--exclude-standard", "-z"),
            ("diff", "--no-renames", "HEAD", "--name-only", "-z"),
        ]
        paths = set()
        for args in path_lists:
            output = subprocess.check_output(["git", "-C", str(original), *args])
            paths.update(os.fsdecode(path) for path in output.split(b"\0") if path)
        for path in sorted(paths):
            source = original / path
            destination = detached / path
            if source.is_file() or source.is_symlink():
                destination.parent.mkdir(parents=True, exist_ok=True)
                shutil.copy2(source, destination, follow_symlinks=False)
            elif destination.is_file() or destination.is_symlink():
                destination.unlink()
        detached_index = Path(self.git("rev-parse", "--git-path", "index", worktree=detached))
        shutil.copy2(original_index, detached_index)

        result = self.run_stash(detached, "save", "recovery copy")
        self.assertEqual(result.returncode, 0, result.stderr)
        detached_sha = result.stdout.strip()
        self.assertEqual(original_index.read_bytes(), index_before)
        self.assertEqual(self.state(original), status_before)
        self.assertEqual(self.run_stash(original, "list").stdout, list_before)
        self.assertIn(existing_sha, list_before)
        self.assertIn(detached_sha, self.run_stash(detached, "list").stdout)
        self.assertNotIn(detached_sha, list_before)


if __name__ == "__main__":
    unittest.main()
