"""Tests for tools/check_portable_shell.py (local #322, hub #2978)."""

import contextlib
import importlib.util
import io
import json
import sys
import unittest
from pathlib import Path

sys.dont_write_bytecode = True
ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
from scratch_git import scratch_git, scratch_tempdir  # noqa: E402

SPEC = importlib.util.spec_from_file_location("check_portable_shell", ROOT / "tools/check_portable_shell.py")
GUARD = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(GUARD)


def rules(text):
    return [f.rule for f in GUARD.scan_text(text)]


class FlaggedFormsTest(unittest.TestCase):
    def test_every_form_in_the_issue_is_flagged(self):
        cases = {
            "mktemp -t rsi-x": ["mktemp-t"],
            "sed -i '' 's/a/b/' f": ["sed-i-empty"],
            "sed -i 's/a/b/' f": ["sed-i-bare"],
            "sed -i -e 's/a/b/' f": ["sed-i-bare"],
            "stat -f %m f": ["stat-f"],
            "stat -c %Y f": ["stat-c"],
            "date -j -f %s 1": ["date-j"],
            "date -d @1 +%s": ["date-d"],
            "readlink -f \"$0\"": ["readlink-f"],
            "grep -P 'a(?=b)' f": ["grep-P"],
            "grep -oP 'a' f": ["grep-P"],
            "find . | xargs -r rm": ["xargs-r"],
            "declare -A map": ["declare-A"],
            "mapfile -t lines < f": ["mapfile"],
            "readarray -t lines < f": ["mapfile"],
            "x=${name,,}": ["case-mod"],
            "echo \"${name^^}\"": ["case-mod"],
            "wait -n": ["wait-n"],
            "local -n ref=$1": ["nameref"],
        }
        for source, expected in cases.items():
            with self.subTest(source=source):
                self.assertEqual(rules(source + "\n"), expected)

    def test_portable_spellings_pass(self):
        clean = [
            'mktemp "${TMPDIR:-/tmp}/rsi.XXXXXX"',
            "mktemp -d -t rsi.XXXXXX",
            "mktemp -d",
            "sed -i.bak 's/a/b/' f",
            "sed -E 's/a/b/' f",
            "sed -n 's/-i/x/p' f",
            "stat --version",
            "date -u +%s",
            "readlink \"$link\"",
            "grep -E 'a|b' f",
            "grep -r foo .",
            "xargs grep -r foo",
            "find . -print0 | xargs -0 rm",
            "declare -a plain",
            "x=${name:-default}",
            "echo ${#name}",
            "wait \"$pid\"",
            "echo 'sed -i is GNU only'",
            "echo \"use mapfile later\"",
            "# sed -i comment",
        ]
        for source in clean:
            with self.subTest(source=source):
                self.assertEqual(rules(source + "\n"), [])

    def test_forms_nested_in_pipes_substitutions_and_wrappers_are_found(self):
        cases = {
            'x=$(readlink -f "$0")': ["readlink-f"],
            'x="$(stat -c %Y f)"': ["stat-c"],
            "x=`readlink -f f`": ["readlink-f"],
            "ssh host 'sed -i s/a/b/ f'": ["sed-i-bare"],
            "ssh host sed -i s/a/b/ f": ["sed-i-bare"],
            "bash -c \"declare -A m\"": ["declare-A"],
            "sudo sed -i '' f": ["sed-i-empty"],
            "cat f | grep -P x | sort": ["grep-P"],
            "find . -exec sed -i s/a/b/ {} +": ["sed-i-bare"],
            "if grep -P x f; then echo y; fi": ["grep-P"],
            "FOO=1 mapfile -t a < f": ["mapfile"],
        }
        for source, expected in cases.items():
            with self.subTest(source=source):
                self.assertEqual(rules(source + "\n"), expected)

    def test_a_finding_names_the_portable_alternative(self):
        for rule in GUARD.RULES.values():
            self.assertIn(rule.side, ("bsd", "gnu"))
            self.assertTrue(rule.message.strip())
        finding = GUARD.scan_text("readlink -f x\n", "scripts/a.sh")[0]
        line = GUARD.format_finding(finding)
        self.assertIn("scripts/a.sh:1: [readlink-f]", line)
        self.assertIn("GNU-only", line)
        self.assertIn("pwd -P", line)


class AllowedFormsTest(unittest.TestCase):
    def test_a_uname_if_block_allows_both_sides(self):
        source = (
            'if [ "$(uname -s)" = Darwin ]; then\n'
            "  stat -f %m f\n"
            "else\n"
            "  stat -c %Y f\n"
            "fi\n"
            "stat -c %Y g\n"
        )
        found = GUARD.scan_text(source)
        self.assertEqual([(f.line, f.rule) for f in found], [(6, "stat-c")])

    def test_a_uname_case_and_ostype_and_probe_variable(self):
        source = (
            'case "$(uname)" in\n'
            "  Linux) sed -i 's/a/b/' f ;;\n"
            "  Darwin) sed -i '' 's/a/b/' f ;;\n"
            "esac\n"
            'if [[ "$OSTYPE" == darwin* ]]; then date -j -f %s 1; fi\n'
            "os=$(uname -s)\n"
            'if [ "$os" = Linux ]; then readlink -f x; fi\n'
            "readlink -f y\n"
        )
        self.assertEqual([(f.line, f.rule) for f in GUARD.scan_text(source)], [(8, "readlink-f")])

    def test_a_same_line_uname_probe_allows_the_command(self):
        source = '[ "$(uname)" = Linux ] && readlink -f x\nreadlink -f y\n'
        self.assertEqual([f.line for f in GUARD.scan_text(source)], [2])

    def test_nested_blocks_leave_the_probe_at_the_right_fi(self):
        source = (
            'if [ "$(uname)" = Linux ]; then\n'
            "  if [ -n \"$x\" ]; then\n"
            "    readlink -f a\n"
            "  fi\n"
            "  readlink -f b\n"
            "fi\n"
            "readlink -f c\n"
        )
        self.assertEqual([f.line for f in GUARD.scan_text(source)], [7])

    def test_file_marker_in_the_header_exempts_the_matching_side(self):
        linux = "#!/bin/bash\n# portability: linux-only\nstat -c %Y f\nsed -i '' f\n"
        self.assertEqual(rules(linux), ["sed-i-empty"])
        macos = "#!/bin/bash\n# portability: macos-only\nstat -f %m f\nstat -c %Y f\n"
        self.assertEqual(rules(macos), ["stat-c"])

    def test_line_markers_apply_to_their_line_and_the_next_code_line(self):
        source = (
            "#!/bin/bash\n"
            "set -e\n"
            "readlink -f a  # portability: linux-only\n"
            "readlink -f b\n"
            "# portability: linux-only\n"
            "readlink -f c\n"
            "readlink -f d\n"
        )
        self.assertEqual([f.line for f in GUARD.scan_text(source)], [4, 7])

    def test_a_marker_after_code_is_not_a_file_marker(self):
        source = "#!/bin/bash\nset -e\n# portability: linux-only\nreadlink -f a\nreadlink -f b\n"
        self.assertEqual([f.line for f in GUARD.scan_text(source)], [5])

    def test_a_gnu_and_bsd_form_joined_by_or_is_the_fallback_idiom(self):
        linked = [
            "owner() {\n  stat -c %u \"$1\" 2>/dev/null || stat -f %u \"$1\"\n}\n",
            "stat -c %Y f || stat -f %m f\n",
            "stat -f %m f || stat -c %Y f\n",
            "m=$(stat -c %Y f 2>/dev/null \\\n  || stat -f %m f)\n",
            "m=$(stat -c %Y f ||\n  stat -f %m f)\n",
            "m=$(stat -c %Y f 2>/dev/null) || m=$(stat -f %m f)\n",
            "[ -e f ] && stat -c %Y f || stat -f %m f\n",
            "stat -c %Y f ||  # BSD\n  stat -f %m f\n",
            "sed -i 's/a/b/' f || sed -i '' 's/a/b/' f\n",
            "date -d @1 +%s || date -j -r 1 +%s\n",
            "date -j -r 1 +%s ||\n  date -d @1 +%s\n",
        ]
        for source in linked:
            with self.subTest(source=source):
                self.assertEqual(rules(source), [])

    def test_adjacent_independent_statements_are_both_flagged(self):
        separate = {
            "stat -c %Y f\nstat -f %m f\n": ["stat-c", "stat-f"],
            "stat -c %Y f; stat -f %m f\n": ["stat-c", "stat-f"],
            "stat -c %Y f && stat -f %m f\n": ["stat-c", "stat-f"],
            "x=$(stat -c %Y f)\ny=$(stat -f %m f)\n": ["stat-c", "stat-f"],
            "sed -i 's/a/b/' f\nsed -i '' 's/a/b/' f\n": ["sed-i-bare", "sed-i-empty"],
            "date -d @1 +%s\ndate -j -r 1 +%s\n": ["date-d", "date-j"],
            "stat -c %Y f || true\nstat -f %m f\n": ["stat-c", "stat-f"],
            "stat -c %Y f &\nstat -f %m f\n": ["stat-c", "stat-f"],
            "stat -c %Y f;\n  stat -f %m f\n": ["stat-c", "stat-f"],
            "stat -c %Y f\n\n\n\nstat -f %m f\n": ["stat-c", "stat-f"],
        }
        for source, expected in separate.items():
            with self.subTest(source=source):
                self.assertEqual(rules(source), expected)

    def test_a_linked_pair_does_not_hide_a_third_independent_form(self):
        source = "stat -c %Y a || stat -f %m a\nstat -c %Y b\n"
        self.assertEqual([(f.line, f.rule) for f in GUARD.scan_text(source)], [(2, "stat-c")])

    def test_or_links_do_not_cross_tool_families(self):
        self.assertEqual(rules("stat -c %Y f || date -j\n"), ["date-j", "stat-c"])
        self.assertEqual(rules("stat -c %Y f || stat -c %Z f\n"), ["stat-c", "stat-c"])


class HeredocTest(unittest.TestCase):
    def test_heredoc_to_a_shell_is_scanned_and_to_another_language_is_not(self):
        source = (
            "ssh host bash -s <<'EOF'\n"
            "readlink -f /x\n"
            "EOF\n"
            "python3 - <<'PY'\n"
            "import subprocess; subprocess.run(['readlink', '-f', 'x'])  # sed -i\n"
            "readlink -f /y\n"
            "PY\n"
            "cat <<EOF\n"
            "readlink -f in a message\n"
            "EOF\n"
            "cat > run.sh <<'EOF'\n"
            "mapfile -t a < f\n"
            "EOF\n"
            "readlink -f after\n"
        )
        found = [(f.line, f.rule) for f in GUARD.scan_text(source)]
        self.assertEqual(found, [(2, "readlink-f"), (12, "mapfile"), (14, "readlink-f")])

    def test_tab_stripped_heredoc_delimiter(self):
        source = "cat <<-EOF\n\treadlink -f x\n\tEOF\nreadlink -f y\n"
        self.assertEqual([f.line for f in GUARD.scan_text(source)], [4])


class LexerTest(unittest.TestCase):
    def test_multiline_quoted_text_is_not_code(self):
        source = 'echo "first\nreadlink -f inside the string\nlast"\nreadlink -f real\n'
        self.assertEqual([f.line for f in GUARD.scan_text(source)], [4])

    def test_line_continuation_joins_a_command(self):
        source = "sed -i \\\n  's/a/b/' f\n"
        self.assertEqual(rules(source), ["sed-i-bare"])

    def test_arithmetic_and_param_expansion_do_not_break_the_lexer(self):
        source = "n=$((1 + 2))\nm=${#arr[@]}\nx=${y:-$(readlink -f z)}\n"
        self.assertEqual(rules(source), ["readlink-f"])


class TreeFixture(unittest.TestCase):
    def setUp(self):
        self.temp = scratch_tempdir("rsi-portable-shell-test-")
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name) / "repo"
        (self.root / "scripts").mkdir(parents=True)
        (self.root / "tools").mkdir()

    def write(self, rel, text, mode=0o644):
        path = self.root / rel
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(text)
        path.chmod(mode)

    def baseline(self, files, rel=GUARD.BASELINE):
        self.write(rel, json.dumps({"schema": 1, "files": files}))

    def run_guard(self, *argv):
        out, err = io.StringIO(), io.StringIO()
        with contextlib.redirect_stdout(out), contextlib.redirect_stderr(err):
            code = GUARD.main(["--root", str(self.root), *argv])
        return code, out.getvalue() + err.getvalue()

    def git(self, *args):
        return scratch_git(self.root, *args)


class CandidateFilesTest(TreeFixture):
    def test_shell_scripts_by_suffix_and_shebang_only(self):
        self.write("scripts/a.sh", "true\n")
        self.write("scripts/tool", "#!/usr/bin/env bash\ntrue\n")
        self.write("scripts/py", "#!/usr/bin/env python3\nprint(1)\n")
        self.write("scripts/data.json", "{}\n")
        self.write("scripts/sub/b.bash", "true\n")
        self.write("scripts/tests/fixtures/bad.sh", "readlink -f x\n")
        self.write("tools/c.sh", "true\n")
        self.assertEqual(GUARD.candidate_files(self.root, []),
                         ["scripts/a.sh", "scripts/sub/b.bash", "scripts/tool"])

    def test_explicit_paths_are_filtered_to_scripts_shell_files(self):
        self.write("scripts/a.sh", "true\n")
        self.write("scripts/p.py", "x = 1\n")
        self.write("tools/c.sh", "true\n")
        paths = ["scripts/a.sh", "scripts/p.py", "tools/c.sh", "scripts/missing.sh", str(self.root / "scripts/a.sh")]
        self.assertEqual(GUARD.candidate_files(self.root, paths), ["scripts/a.sh"])


class BaselineGateTest(TreeFixture):
    def test_new_use_above_the_baseline_fails_and_names_the_alternative(self):
        self.write("scripts/a.sh", "readlink -f x\nreadlink -f y\n")
        self.baseline({"scripts/a.sh": 1})
        code, output = self.run_guard()
        self.assertEqual(code, 1, output)
        self.assertIn("scripts/a.sh: 2 non-portable form(s), baseline allows 1", output)
        self.assertIn("scripts/a.sh:2: [readlink-f]", output)
        self.assertIn("pwd -P", output)

    def test_a_new_file_with_a_use_fails_with_no_baseline_entry(self):
        self.write("scripts/new.sh", "mapfile -t a < f\n")
        self.baseline({})
        code, output = self.run_guard()
        self.assertEqual(code, 1, output)
        self.assertIn("while IFS= read -r", output)

    def test_counts_equal_to_the_baseline_pass(self):
        self.write("scripts/a.sh", "readlink -f x\n")
        self.baseline({"scripts/a.sh": 1})
        self.assertEqual(self.run_guard()[0], 0)

    def test_a_stale_count_must_be_lowered(self):
        self.write("scripts/a.sh", "true\n")
        self.baseline({"scripts/a.sh": 2, "scripts/gone.sh": 1})
        code, output = self.run_guard()
        self.assertEqual(code, 1, output)
        self.assertIn("baseline allows 2 but only 0 remain", output)
        self.assertIn("scripts/gone.sh: baseline allows 1", output)

    def test_explicit_paths_only_judge_those_files(self):
        self.write("scripts/a.sh", "true\n")
        self.write("scripts/b.sh", "readlink -f x\n")
        self.baseline({})
        self.assertEqual(self.run_guard("scripts/a.sh")[0], 0)
        self.assertEqual(self.run_guard("scripts/b.sh")[0], 1)

    def test_a_missing_baseline_counts_every_use_as_new(self):
        self.write("scripts/a.sh", "readlink -f x\n")
        self.assertEqual(self.run_guard()[0], 1)

    def test_a_marker_or_uname_branch_keeps_the_guard_green(self):
        self.write("scripts/a.sh", "# portability: linux-only\nreadlink -f x\n")
        self.write("scripts/b.sh", 'if [ "$(uname)" = Linux ]; then readlink -f x; fi\n')
        self.baseline({})
        self.assertEqual(self.run_guard()[0], 0)

    def test_the_real_baseline_matches_the_real_tree(self):
        code, output = self.real_tree()
        self.assertEqual(code, 0, output)

    def real_tree(self):
        out, err = io.StringIO(), io.StringIO()
        with contextlib.redirect_stdout(out), contextlib.redirect_stderr(err):
            code = GUARD.main(["--root", str(ROOT)])
        return code, out.getvalue() + err.getvalue()


class RefreshTest(TreeFixture):
    def test_refresh_creates_then_lowers_but_never_raises(self):
        self.write("scripts/a.sh", "readlink -f x\nreadlink -f y\n")
        self.assertEqual(self.run_guard("--refresh")[0], 0)
        path = self.root / GUARD.BASELINE
        self.assertEqual(json.loads(path.read_text())["files"], {"scripts/a.sh": 2})
        self.write("scripts/a.sh", "readlink -f x\n")
        self.assertEqual(self.run_guard("--refresh")[0], 0)
        self.assertEqual(json.loads(path.read_text())["files"], {"scripts/a.sh": 1})
        self.write("scripts/a.sh", "readlink -f x\nreadlink -f y\n")
        code, output = self.run_guard("--refresh")
        self.assertEqual(code, 1, output)
        self.assertIn("never raises", output)
        self.assertEqual(json.loads(path.read_text())["files"], {"scripts/a.sh": 1})

    def test_refresh_drops_files_with_no_remaining_uses(self):
        self.write("scripts/a.sh", "true\n")
        self.baseline({"scripts/a.sh": 3})
        self.assertEqual(self.run_guard("--refresh")[0], 0)
        self.assertEqual(json.loads((self.root / GUARD.BASELINE).read_text())["files"], {})


class BaseRefTest(TreeFixture):
    def setUp(self):
        super().setUp()
        self.git("init", "-q", "-b", "rolling")
        self.git("config", "user.name", "T")
        self.git("config", "user.email", "t@example.invalid")
        self.write("scripts/a.sh", "readlink -f x\n")
        self.baseline({"scripts/a.sh": 1})
        self.git("add", ".")
        self.git("commit", "-q", "-m", "base")

    def test_a_raised_baseline_fails_even_when_the_tree_agrees(self):
        self.write("scripts/a.sh", "readlink -f x\nreadlink -f y\n")
        self.baseline({"scripts/a.sh": 2})
        code, output = self.run_guard("--base", "HEAD")
        self.assertEqual(code, 1, output)
        self.assertIn("baseline raised from 1 to 2", output)

    def test_a_new_baseline_entry_counts_as_a_raise(self):
        self.write("scripts/b.sh", "readlink -f x\n")
        self.baseline({"scripts/a.sh": 1, "scripts/b.sh": 1})
        code, output = self.run_guard("--base", "HEAD")
        self.assertEqual(code, 1, output)
        self.assertIn("scripts/b.sh: baseline raised from 0 to 1", output)

    def test_an_unchanged_or_lowered_baseline_passes(self):
        self.assertEqual(self.run_guard("--base", "HEAD")[0], 0)
        self.write("scripts/a.sh", "true\n")
        self.baseline({})
        self.assertEqual(self.run_guard("--base", "HEAD")[0], 0)

    def stale_sibling(self):
        """A second file whose baseline count went stale on rolling, outside this change."""
        self.write("scripts/b.sh", "true\n")
        self.baseline({"scripts/a.sh": 1, "scripts/b.sh": 2})
        self.git("add", ".")
        self.git("commit", "-q", "-m", "stale sibling")
        self.write("scripts/c.sh", "true\n")
        self.git("add", ".")
        self.git("commit", "-q", "-m", "unrelated change")

    def test_an_untouched_file_with_a_stale_count_passes_in_diff_mode(self):
        self.stale_sibling()
        self.assertEqual(self.run_guard("--base", "HEAD~2")[0], 1, "b.sh is touched by HEAD~1")
        code, output = self.run_guard("--base", "HEAD~1")
        self.assertEqual(code, 0, output)
        self.assertEqual(self.run_guard()[0], 1, "a full-tree run still reports every stale count")

    def test_a_touched_file_with_a_stale_count_names_the_lower_baseline(self):
        self.stale_sibling()
        self.write("scripts/b.sh", "true\n# touched\n")
        self.git("add", ".")
        self.git("commit", "-q", "-m", "touch b")
        code, output = self.run_guard("--base", "HEAD~1")
        self.assertEqual(code, 1, output)
        self.assertIn("lower the baseline for scripts/b.sh to 0", output)

    def test_a_new_use_in_any_file_fails_in_diff_mode(self):
        self.stale_sibling()
        self.write("scripts/c.sh", "readlink -f x\n")
        code, output = self.run_guard("--base", "HEAD")
        self.assertEqual(code, 1, output)
        self.assertIn("scripts/c.sh: 1 non-portable form(s), baseline allows 0", output)

    def test_a_deleted_file_with_a_baseline_count_must_be_lowered(self):
        self.git("rm", "-q", "scripts/a.sh")
        self.git("commit", "-q", "-m", "delete a")
        code, output = self.run_guard("--base", "HEAD~1")
        self.assertEqual(code, 1, output)
        self.assertIn("lower the baseline for scripts/a.sh to 0", output)

    def test_changes_that_rolling_gained_after_the_fork_are_not_this_changes(self):
        self.git("branch", "feature")
        self.write("scripts/a.sh", "true\n")
        self.baseline({})
        self.git("add", ".")
        self.git("commit", "-q", "-m", "rolling fixes a.sh")
        self.git("checkout", "-q", "feature")
        self.write("scripts/d.sh", "true\n")
        self.git("add", ".")
        self.git("commit", "-q", "-m", "feature work")
        self.assertEqual(GUARD.changed_since(self.root, "rolling"), {"scripts/d.sh"})

    def test_a_touched_list_scopes_the_stale_check_like_the_pre_push_hook(self):
        self.stale_sibling()
        listing = self.root / "touched.list"
        listing.write_bytes(b"scripts/c.sh\0")
        self.assertEqual(self.run_guard("--touched-from", str(listing))[0], 0)
        listing.write_bytes(b"scripts/c.sh\0scripts/b.sh\0")
        code, output = self.run_guard("--touched-from", str(listing))
        self.assertEqual(code, 1, output)
        self.assertIn("lower the baseline for scripts/b.sh to 0", output)
        listing.write_bytes(b"")
        self.assertEqual(self.run_guard("--touched-from", str(listing))[0], 0)

    def test_an_unreadable_base_falls_back_to_judging_every_count(self):
        self.stale_sibling()
        code, output = self.run_guard("--base", "no-such-ref")
        self.assertEqual(code, 1, output)
        self.assertIn("lower the baseline for scripts/b.sh", output)

    def test_a_base_without_a_baseline_file_is_skipped(self):
        self.assertIsNone(GUARD.read_base_baseline(self.root, "HEAD", "tools/none.json"))
        self.assertIsNone(GUARD.read_base_baseline(self.root, "no-such-ref", GUARD.BASELINE))


class WiringTest(unittest.TestCase):
    def test_the_guard_is_registered_like_the_identity_guard(self):
        hook = (ROOT / "tools/git-hooks/pre-push").read_text()
        self.assertIn("tools/check_portable_shell.py", hook)
        commit = (ROOT / "tools/git-hooks/pre-commit").read_text()
        self.assertIn("tools/check_portable_shell.py", commit)
        gate = (ROOT / "scripts/scoped-test").read_text()
        self.assertIn('"portable_shell"', gate)
        self.assertTrue((ROOT / "scripts/check-portable-shell.sh").is_file())


if __name__ == "__main__":
    unittest.main()
