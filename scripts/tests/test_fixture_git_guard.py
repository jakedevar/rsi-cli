"""No scripts/tests fixture runs Git on an implicit working directory (#2574, #225).

A fixture that runs `git commit` with `cwd=<scratch>` or after `cd <scratch> &&` acts
on whatever directory it is in when the `cd` or the setup went wrong. In an RSI
sandbox that is the worker's own assigned worktree, and a stray `git commit` or
`git reset` there moves the assigned branch (AGENTS.md hard rule 2). Fixture Git
goes through `scripts/scratch_git.py` (`scratch_git(<validated dir>, ...)`, scratch
directories from `scratch_tempdir()`) or, for a read-only question about an existing
checkout, an explicit `git -C <dir>`; shell fixtures use `git -C "$REPO"`.

This module scans every `scripts/tests/*.py` and `scripts/tests/*.sh` and rejects:

* an executable `subprocess` (or `os.system`/`os.popen`) Git call with no `-C`
  (an implicit working directory, whether or not `cwd=` is passed);
* a `--git-dir`/`--work-tree` global option on such a call;
* a direct `git -C <dir> <mutating subcommand>` (it skips the physical outside-root
  validation and the repository-location environment cleanup: use `scratch_git`);
* `cd <dir> && git ...` in a shell string or shell fixture;
* in a module that runs fixture Git, a `tempfile.TemporaryDirectory()`/`mkdtemp()`
  with no `dir=`: that lands under `TMPDIR`, which an RSI sandbox points inside
  itself (use `scratch_tempdir()`).

Only executable calls are inspected. Fixture program source held in a string, a mock
that compares an argument vector with `("git", ...)`, and `scratch_git(...)` itself
are not Git calls and are not flagged. A line that must stay an exception ends with
`# fixture-git: allow <reason>`; the reason is mandatory and the list is asserted.
"""

import ast
import re
import unittest
from pathlib import Path

TESTS = Path(__file__).resolve().parent
ROOT = TESTS.parents[1]

SUBPROCESS_FUNCTIONS = frozenset({"run", "call", "check_call", "check_output", "Popen"})
# Questions that cannot move a ref, the index or the worktree; an explicit `-C` is enough.
READ_ONLY_SUBCOMMANDS = frozenset({
    "rev-parse", "hash-object", "check-ignore", "ls-files", "ls-tree", "diff", "log", "show",
    "status", "cat-file", "merge-base", "rev-list", "show-ref", "symbolic-ref", "for-each-ref",
    "describe", "grep", "blame", "name-rev", "var",
})
OVERRIDE_PREFIXES = ("--git-dir", "--work-tree")
ALLOW = re.compile(r"#\s*fixture-git:\s*allow\s+(\S.*)$")
TEMPFILE_ALLOCATORS = frozenset({"TemporaryDirectory", "mkdtemp"})
SHELL_CD_GIT = re.compile(r"\bcd\b[^\n]*?(&&|;|\|\|)\s*(\w+=\S+\s+)*git\b")
SHELL_COMMAND_GIT = re.compile(r"(?:^|[;&|(`]|\$\()\s*(?:\w+=\S+\s+)*git\s+(?P<rest>\S+)")
SHELL_FUNCTION_GIT = re.compile(r"^\s*(?:function\s+)?\w+\s*\(\)\s*\{\s*git\s+(?P<rest>\S+)")
HEREDOC = re.compile(r"<<-?\s*(?P<quote>['\"]?)(?P<tag>\w+)(?P=quote)")


def _text(node):
    """The string a Constant holds, else None."""
    if isinstance(node, ast.Constant) and isinstance(node.value, str):
        return node.value
    return None


def _joined_text(node):
    """The literal text of a string expression (constant parts of an f-string), else None."""
    text = _text(node)
    if text is not None:
        return text
    if isinstance(node, ast.JoinedStr):
        return "".join(part.value for part in node.values if isinstance(part, ast.Constant))
    return None


def _call_name(node):
    """`(module, name)` of a call target: `subprocess.run` -> ("subprocess", "run")."""
    func = node.func
    if isinstance(func, ast.Attribute) and isinstance(func.value, ast.Name):
        return func.value.id, func.attr
    if isinstance(func, ast.Name):
        return "", func.id
    return "", ""


def _argv_assignments(tree):
    """Name -> list/tuple literal assigned to it (last assignment wins)."""
    names = {}
    for node in ast.walk(tree):
        if not isinstance(node, ast.Assign) or not isinstance(node.value, (ast.List, ast.Tuple)):
            continue
        for target in node.targets:
            if isinstance(target, ast.Name):
                names[target.id] = node.value
    return names


def _argument(call):
    """The command argument of a subprocess-style call."""
    if call.args:
        return call.args[0]
    for keyword in call.keywords:
        if keyword.arg == "args":
            return keyword.value
    return None


def classify_argv(elements):
    """`(problem, subcommand)` for a Git argument vector, `problem` None when safe.

    `elements` are the AST nodes after the leading "git"."""
    explicit = False
    subcommand = None
    index = 0
    while index < len(elements):
        value = _text(elements[index])
        if value is None:
            break
        if value.startswith(OVERRIDE_PREFIXES):
            return "a --git-dir/--work-tree override moves Git off the validated directory", None
        if value in ("-c", "--config-env"):
            index += 2
            continue
        if value == "-C":
            explicit = True
            index += 2
            continue
        if value.startswith("-"):
            index += 1
            continue
        subcommand = value
        break
    if not explicit:
        return "Git runs on an implicit working directory (pass the repository with scratch_git or `-C`)", subcommand
    if subcommand not in READ_ONLY_SUBCOMMANDS:
        return (
            f"`git -C <dir> {subcommand or '<unknown>'}` can change a repository without the outside-root "
            "validation (use scratch_git)", subcommand,
        )
    return None, subcommand


def _shell_string_problem(text):
    """Why a shell command string runs Git unsafely, else None."""
    if SHELL_CD_GIT.search(text):
        return "`cd <dir> && git ...` runs Git wherever the cd leaves the shell (use `git -C <dir>`)"
    for match in SHELL_COMMAND_GIT.finditer(text):
        if not match.group("rest").startswith("-C"):
            return "shell Git without `-C` runs on an implicit working directory"
    return None


def python_violations(source, name="<source>"):
    """`(line, message)` for every unsafe fixture Git use in one Python module."""
    tree = ast.parse(source, filename=name)
    lines = source.splitlines()
    assignments = _argv_assignments(tree)
    found = []
    uses_git = False
    allocators = []
    for node in ast.walk(tree):
        if not isinstance(node, ast.Call):
            continue
        module, function = _call_name(node)
        if function == "scratch_git":
            uses_git = True
            continue
        if function in TEMPFILE_ALLOCATORS and module in ("tempfile", ""):
            allocators.append(node)
            continue
        is_subprocess = (module == "subprocess" and function in SUBPROCESS_FUNCTIONS) or (
            module == "" and function in ("Popen", "check_output", "check_call")
        )
        is_shell = module == "os" and function in ("system", "popen")
        if not is_subprocess and not is_shell:
            continue
        argument = _argument(node)
        if argument is None:
            continue
        if isinstance(argument, ast.Name):
            argument = assignments.get(argument.id, argument)
        problem = None
        if isinstance(argument, (ast.List, ast.Tuple)) and argument.elts:
            if _text(argument.elts[0]) == "git":
                uses_git = True
                problem, _ = classify_argv(argument.elts[1:])
            elif _text(argument.elts[0]) in ("bash", "sh", "zsh") and len(argument.elts) >= 3 and (
                _text(argument.elts[1]) in ("-c", "-lc")
            ):
                shell = _joined_text(argument.elts[2])
                problem = _shell_string_problem(shell) if shell else None
        else:
            shell = _joined_text(argument)
            if shell is not None and re.search(r"\bgit\b", shell):
                uses_git = True
                problem = _shell_string_problem(shell)
        if problem is not None:
            found.append((node.lineno, problem))
    for node in allocators:
        has_dir = any(
            keyword.arg == "dir" and not (isinstance(keyword.value, ast.Constant) and keyword.value.value is None)
            for keyword in node.keywords
        ) or len(node.args) >= 3
        if uses_git and not has_dir:
            found.append((node.lineno, "a Git fixture module allocates a directory under TMPDIR (use scratch_tempdir)"))
    found.sort()
    return [(line, message) for line, message in found if not _allowed(lines, line)]


def _allowed(lines, number):
    """The line, or a comment-only line directly above it, carries a pragma."""
    if 1 <= number <= len(lines) and ALLOW.search(lines[number - 1]):
        return True
    above = lines[number - 2] if 2 <= number <= len(lines) + 1 else ""
    return above.lstrip().startswith("#") and ALLOW.search(above) is not None


def shell_violations(source):
    """`(line, message)` for every unsafe fixture Git use in one shell fixture."""
    found = []
    heredoc_end = None
    uses_git = bool(re.search(r"(?m)^\s*(?:\w+=\S+\s+)*(?:git|GIT)\b|\bgit\s+-C\b", source))
    lines = source.splitlines()
    for number, raw in enumerate(lines, start=1):
        if heredoc_end is not None:
            if raw.strip() == heredoc_end:
                heredoc_end = None
            continue
        if not raw.strip() or raw.lstrip().startswith("#"):
            continue
        code = raw
        start = HEREDOC.search(code)
        if start is not None:
            heredoc_end = start.group("tag")
            code = code[:start.start()]
        problem = _shell_string_problem(code)
        function = SHELL_FUNCTION_GIT.match(code)
        if function is not None and not function.group("rest").startswith("-C"):
            problem = "shell function wraps Git without `-C`"
        if problem is None and uses_git and re.search(r"\bmktemp\b", code) and "/" not in code.split("mktemp", 1)[1]:
            problem = "mktemp without a directory lands under TMPDIR, which a sandbox points inside itself"
        if problem is not None and not _allowed(lines, number):
            found.append((number, problem))
    return found


def scan_tree(root=TESTS):
    """`{relative name: [(line, message), ...]}` for every violating fixture file."""
    report = {}
    for path in sorted(root.iterdir()):
        if path.suffix == ".py":
            problems = python_violations(path.read_text(encoding="utf-8"), path.name)
        elif path.suffix == ".sh":
            problems = shell_violations(path.read_text(encoding="utf-8"))
        else:
            continue
        if problems:
            report[path.name] = problems
    return report


def allowances(root=TESTS):
    """Every `# fixture-git: allow <reason>` line, as `(file, line, reason)`."""
    found = []
    for path in sorted(root.iterdir()):
        if path.suffix not in (".py", ".sh"):
            continue
        for number, line in enumerate(path.read_text(encoding="utf-8").splitlines(), start=1):
            match = ALLOW.search(line)
            if match is not None and path.name != Path(__file__).name:
                found.append((path.name, number, match.group(1)))
    return found


class FixtureGitGuardTest(unittest.TestCase):
    def messages(self, source):
        return [message for _, message in python_violations(source)]

    def test_every_scripts_tests_fixture_runs_git_in_an_explicit_validated_repository(self):
        report = scan_tree()
        self.assertEqual(report, {}, "unsafe fixture Git (use scripts/scratch_git.py): " + repr(report))

    def test_the_scan_covers_the_fixture_modules_that_run_git(self):
        names = {path.name for path in TESTS.iterdir()}
        for required in ("test_check_touched_shards.py", "test_scoped_test.py", "test_rsi_stash.py",
                         "check-corpus-pins.test.sh", "test_slow_tier.py"):
            self.assertIn(required, names)

    def test_every_allowance_names_its_reason(self):
        for name, number, reason in allowances():
            self.assertGreaterEqual(len(reason.split()), 3, f"{name}:{number} allowance needs a real reason")

    def test_the_only_allowance_is_the_scratch_git_helper_test_on_its_fake_assigned_repo(self):
        # scratch_git refuses a protected root by design, and test_scratch_git builds that
        # root on purpose; no other fixture needs an exception.
        self.assertEqual([name for name, _, _ in allowances()], ["test_scratch_git.py"])

    def test_bare_subprocess_git_with_cwd_is_rejected(self):
        source = (
            "import subprocess\n"
            "def git(root, *args):\n"
            "    subprocess.run(['git', *args], cwd=root, check=True)\n"
        )
        self.assertEqual(self.messages(source),
                         ["Git runs on an implicit working directory (pass the repository with scratch_git or `-C`)"])

    def test_bare_subprocess_git_through_a_variable_is_rejected(self):
        source = (
            "import subprocess\n"
            "command = ['git', 'commit', '-qm', 'x']\n"
            "subprocess.check_output(command)\n"
        )
        self.assertEqual(len(self.messages(source)), 1)

    def test_git_options_before_the_subcommand_do_not_hide_a_bare_call(self):
        source = "import subprocess\nsubprocess.run(['git', '-c', 'user.name=t', 'commit', '-qm', 'x'])\n"
        self.assertEqual(len(self.messages(source)), 1)

    def test_direct_mutating_git_c_is_rejected(self):
        for subcommand in ("commit", "reset", "checkout", "init", "worktree", "update-ref"):
            source = f"import subprocess\nsubprocess.run(['git', '-C', str(repo), '{subcommand}'])\n"
            messages = self.messages(source)
            self.assertEqual(len(messages), 1, subcommand)
            self.assertIn("use scratch_git", messages[0])

    def test_git_c_with_an_unknown_subcommand_is_rejected(self):
        source = "import subprocess\nsubprocess.run(['git', '-C', str(repo), *args])\n"
        self.assertEqual(len(self.messages(source)), 1)

    def test_git_dir_and_work_tree_options_are_rejected(self):
        for option in ("--git-dir=x/.git", "--work-tree=x"):
            source = f"import subprocess\nsubprocess.run(['git', '{option}', 'status'])\n"
            self.assertIn("override", self.messages(source)[0])

    def test_shell_string_cd_and_git_is_rejected(self):
        for command in ('cd "$scratch" && git commit -qm x', "cd scratch; git reset --hard", "git commit -qm x"):
            source = f"import os\nos.system({command!r})\n"
            self.assertEqual(len(self.messages(source)), 1, command)
        source = "import subprocess\nsubprocess.run('cd x && git add .', shell=True)\n"
        self.assertEqual(len(self.messages(source)), 1)
        source = "import subprocess\nsubprocess.run(['bash', '-c', 'cd x && git add .'])\n"
        self.assertEqual(len(self.messages(source)), 1)

    def test_a_fixture_module_allocating_under_tmpdir_is_rejected(self):
        source = (
            "import tempfile\n"
            "from scratch_git import scratch_git\n"
            "temp = tempfile.TemporaryDirectory(prefix='x-')\n"
            "scratch_git(temp.name, 'init')\n"
        )
        self.assertEqual(self.messages(source),
                         ["a Git fixture module allocates a directory under TMPDIR (use scratch_tempdir)"])
        self.assertEqual(len(self.messages(source.replace("tempfile.TemporaryDirectory(prefix='x-')",
                                                          "tempfile.mkdtemp()"))), 1)

    def test_scratch_git_and_scratch_tempdir_are_accepted(self):
        source = (
            "from scratch_git import scratch_git, scratch_tempdir\n"
            "with scratch_tempdir(prefix='x-') as root:\n"
            "    scratch_git(root, 'init', '-q')\n"
            "    scratch_git(root, '-c', 'user.name=t', 'commit', '--allow-empty', '-qm', 'x')\n"
        )
        self.assertEqual(self.messages(source), [])

    def test_explicit_read_only_git_c_is_accepted(self):
        source = (
            "import subprocess\n"
            "subprocess.run(['git', '-C', str(ROOT), 'check-ignore', '-q', 'a'])\n"
            "subprocess.check_output(['git', '-c', 'core.quotepath=off', '-C', str(ROOT), 'ls-files'])\n"
        )
        self.assertEqual(self.messages(source), [])

    def test_a_module_without_git_may_allocate_under_tmpdir(self):
        source = "import tempfile\ntemp = tempfile.TemporaryDirectory(prefix='plain-')\n"
        self.assertEqual(self.messages(source), [])

    def test_inert_source_strings_and_mock_matching_are_not_git_calls(self):
        source = (
            "import subprocess\n"
            "FAKE = '''\\nimport subprocess\\nsubprocess.run(['git', 'commit'], cwd='.')\\n'''\n"
            "WRAPPER = 'cd x && git commit'\n"
            "def fake_run(command, **kwargs):\n"
            "    if command[:2] == ('git', 'rev-parse'):\n"
            "        return subprocess.CompletedProcess(command, 0, '', '')\n"
            "    if command[0] == 'git':\n"
            "        raise AssertionError(command)\n"
            "SHIMS = (('git', '/bin/git'),)\n"
            "subprocess.run(['which', 'git'])\n"
            "subprocess.run(['echo', 'git commit'])\n"
        )
        self.assertEqual(self.messages(source), [])

    def test_an_allow_pragma_with_a_reason_exempts_only_its_line(self):
        source = (
            "import subprocess\n"
            "subprocess.run(['git', 'status'])  # fixture-git: allow product wrapper reads the caller cwd\n"
            "subprocess.run(['git', 'status'])\n"
        )
        self.assertEqual([line for line, _ in python_violations(source)], [3])

    def test_shell_fixture_bare_git_and_cd_chains_are_rejected(self):
        for body in ('cd "$REPO" && git commit -qm x\n', 'git init -q\n', 'GIT() { git "$@"; }\n',
                     'x="$(git rev-parse HEAD)"\n', 'cd "$REPO"; git add .\n'):
            self.assertEqual(len(shell_violations(body)), 1, body)

    def test_shell_fixture_mktemp_without_a_directory_is_rejected_when_it_runs_git(self):
        self.assertEqual(len(shell_violations('TMP="$(mktemp -d)"\ngit -C "$TMP" init\n')), 1)
        self.assertEqual(shell_violations('TMP="$(mktemp -d)"\necho hi\n'), [])

    def test_shell_fixture_explicit_repo_and_inert_text_are_accepted(self):
        body = (
            "TMP=\"$(mktemp -d \"$parent/fixture.XXXXXX\")\"\n"
            "GIT() { git -C \"$REPO\" \"$@\"; }\n"
            "GIT init -q\n"
            "git -C \"$REPO\" log --oneline\n"
            "# cd x && git commit is only a comment\n"
            "cat > file <<'EOF'\n"
            "cd x && git commit\n"
            "git add .\n"
            "EOF\n"
            "echo \"git add is just text\"\n"
        )
        self.assertEqual(shell_violations(body), [])


if __name__ == "__main__":
    unittest.main()
