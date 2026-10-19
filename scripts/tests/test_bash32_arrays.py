"""bash 3.2 empty-array safety (mac-mini #50): rsi-stash under old and new bash,
and the static guard tools/check_bash32_arrays.py over the macOS-run scripts."""

import importlib.util
import re
import shutil
import subprocess
import sys
import unittest
from pathlib import Path

sys.dont_write_bytecode = True
ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
from scratch_git import scratch_git, scratch_tempdir  # noqa: E402

SPEC = importlib.util.spec_from_file_location("check_bash32_arrays", ROOT / "tools/check_bash32_arrays.py")
GUARD = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(GUARD)

STASH = ROOT / "scripts/rsi-stash"


def bash_version(path):
    try:
        out = subprocess.run([path, "-c", 'echo "${BASH_VERSINFO[0]}.${BASH_VERSINFO[1]}"'],
                             capture_output=True, text=True, timeout=10).stdout.strip()
        major, minor = out.split(".")
        return int(major), int(minor)
    except (OSError, ValueError, subprocess.SubprocessError):
        return None


def find_bash(predicate):
    for candidate in ("/bin/bash", "/usr/bin/bash", "/opt/homebrew/bin/bash",
                      "/usr/local/bin/bash", shutil.which("bash")):
        if candidate and Path(candidate).exists():
            version = bash_version(candidate)
            if version and predicate(version):
                return candidate
    return None


BASH32 = find_bash(lambda v: v < (4, 4))
BASH5 = find_bash(lambda v: v >= (5, 0))


class StashUnderBash:
    """Runs save, list, restore and drop; the empty temp_dirs case is `list`/`drop`."""

    bash = None

    def setUp(self):
        self.temp = scratch_tempdir("rsi-stash-bash-")
        self.addCleanup(self.temp.cleanup)
        self.repo = Path(self.temp.name) / "repo"
        self.repo.mkdir()
        self.git("init", "-q", "-b", "main")
        self.git("config", "user.name", "Bash Test")
        self.git("config", "user.email", "bash-test@example.invalid")
        (self.repo / "shared.txt").write_text("base\n")
        self.git("add", ".")
        self.git("commit", "-q", "-m", "base")

    def git(self, *args):
        return scratch_git(self.repo, *args).stdout.strip()

    def stash(self, *args):
        return subprocess.run([self.bash, str(STASH), *args], cwd=self.repo,
                              capture_output=True, text=True)

    def ok(self, *args):
        result = self.stash(*args)
        self.assertNotIn("unbound variable", result.stderr)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(result.stderr, "")
        return result.stdout.strip()

    def test_empty_temp_dirs_paths_do_not_trip_set_u(self):
        # No temp dir is created by list, drop, usage or a refused save: the cleanup
        # trap expands an empty temp_dirs array.
        self.assertEqual(self.ok("list"), "")
        usage = self.stash("--help")
        self.assertNotIn("unbound variable", usage.stderr)
        self.assertEqual(usage.returncode, 2)
        self.assertIn("Usage: rsi-stash", usage.stderr)
        clean = self.stash("save")
        self.assertNotIn("unbound variable", clean.stderr)
        self.assertEqual(clean.returncode, 1)
        self.assertIn("no uncommitted work", clean.stderr)

    def test_save_restore_drop_round_trip(self):
        (self.repo / "shared.txt").write_text("saved\n")
        (self.repo / "new.txt").write_text("untracked\n")
        sha = self.ok("save", "round trip")
        self.assertRegex(sha, r"^[0-9a-f]{40,64}$")
        self.assertEqual((self.repo / "shared.txt").read_text(), "base\n")
        self.assertFalse((self.repo / "new.txt").exists())
        self.assertEqual(self.ok("list").split("\t")[0], sha)
        self.assertEqual(self.ok("restore", sha), sha)
        self.assertEqual((self.repo / "shared.txt").read_text(), "saved\n")
        self.assertEqual((self.repo / "new.txt").read_text(), "untracked\n")
        self.ok("drop", sha)
        self.assertEqual(self.ok("list"), "")

    def test_two_saves_in_one_second_get_distinct_refs(self):
        (self.repo / "a.txt").write_text("a\n")
        first = self.ok("save", "first")
        (self.repo / "b.txt").write_text("b\n")
        second = self.ok("save", "second")
        self.assertNotEqual(first, second)
        self.assertEqual(len(self.ok("list").splitlines()), 2)


@unittest.skipIf(BASH32 is None, "no bash < 4.4 on this host (macOS /bin/bash 3.2 is the target)")
class RsiStashBash32Test(StashUnderBash, unittest.TestCase):
    bash = BASH32


@unittest.skipIf(BASH5 is None, "no bash >= 5 on this host")
class RsiStashBash5Test(StashUnderBash, unittest.TestCase):
    bash = BASH5


class EmptyArrayGuardTest(unittest.TestCase):
    def flagged(self, source):
        return [(f[2]) for f in GUARD.scan_text(source)]

    def test_flags_unguarded_expansion_of_empty_array(self):
        self.assertEqual(self.flagged('set -u\na=()\nfor x in "${a[@]}"; do :; done\n'), ["a"])
        self.assertEqual(self.flagged('set -euo pipefail\nlocal -a a\nf "${a[*]}"\n'), ["a"])
        self.assertEqual(self.flagged('set -eu\nmapfile -t a < f\nf "${a[@]}"\n'), ["a"])

    def test_accepts_guard_forms_and_nonempty_literals(self):
        for source in (
            'set -u\na=()\nf ${a[@]+"${a[@]}"}\n',
            'set -u\na=(x y)\na+=(z)\nf "${a[@]}"\n',
            'set -u\na=()\n(( ${#a[@]} )) && f "${#a[@]}"\n',
            'set -u\na=()\n# bash32-safe: non-empty here\nf "${a[@]}"\n',
            'a=()\nf "${a[@]}"\n',  # no set -u
            '# f "${a[@]}"\nset -u\na=()\n',
        ):
            with self.subTest(source=source):
                self.assertEqual(self.flagged(source), [])

    @unittest.skipIf(BASH32 is None, "no bash 3.2 to prove the guard idiom")
    def test_guard_idiom_is_legal_on_bash32_and_bare_form_is_not(self):
        bare = subprocess.run([BASH32, "-c", 'set -u; a=(); printf "%s" "${a[@]}"'],
                              capture_output=True, text=True)
        guarded = subprocess.run([BASH32, "-c", 'set -u; a=(); printf "%s" ${a[@]+"${a[@]}"}'],
                                 capture_output=True, text=True)
        self.assertEqual(guarded.returncode, 0, guarded.stderr)
        if BASH32 and bash_version(BASH32) < (4, 4):
            self.assertNotEqual(bare.returncode, 0)

    def test_audited_macos_scripts_are_clean(self):
        findings = []
        for path in GUARD.expand_paths(GUARD.AUDITED):
            if path.suffix == ".sh" or GUARD.is_bash(path):
                rel = str(path.relative_to(ROOT))
                findings += GUARD.scan_text(path.read_text(errors="replace"), rel)
        self.assertEqual(findings, [])

    def test_cli_reports_a_planted_violation(self):
        temp = scratch_tempdir("bash32-guard-")
        self.addCleanup(temp.cleanup)
        script = Path(temp.name) / "bad.sh"
        script.write_text('#!/usr/bin/env bash\nset -euo pipefail\nargs=()\nexec cmd "${args[@]}"\n')
        result = subprocess.run([sys.executable, "-I", str(ROOT / "tools/check_bash32_arrays.py"), str(script)],
                                capture_output=True, text=True)
        self.assertEqual(result.returncode, 1)
        self.assertRegex(result.stdout, re.escape("bad.sh:4:") + ".*'args'")


if __name__ == "__main__":
    unittest.main()
