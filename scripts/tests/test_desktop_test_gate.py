"""The desktop test lane (hub #3001): selection, runner preflight, private tree and verdicts."""

import contextlib
import importlib.util
import io
import json
import os
from pathlib import Path
import shutil
import signal
import subprocess
import sys
import time
import unittest
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
from scratch_git import scratch_git, scratch_tempdir  # noqa: E402

SCRIPT = Path(__file__).resolve().parents[1] / "desktop-test-gate.py"
SPEC = importlib.util.spec_from_file_location("desktop_test_gate", SCRIPT)
GATE = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(GATE)

TAP = "TAP version 13\n# tests 31\n# suites 0\n# pass 31\n# fail 0\n"
SPEC_REPORTER = "ℹ tests 7\nℹ suites 0\nℹ pass 7\n"
CARGO = ("test result: ok. 31 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.23s\n"
         "test result: ok. 0 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s\n")


def present(*names):
    return lambda tool: f"/usr/bin/{tool}" if tool in names else None


def found(code):
    return lambda *args, **kwargs: subprocess.CompletedProcess(args, code)


class Selection(unittest.TestCase):
    def test_a_desktop_path_selects_the_lane_and_anything_else_does_not(self):
        self.assertTrue(GATE.selected(["desktop/src/model.ts"]))
        self.assertTrue(GATE.selected(["crates/rsid/src/lib.rs", "desktop/package.json"]))
        self.assertTrue(GATE.selected([Path("desktop/src-tauri/src/lib.rs")]))
        self.assertFalse(GATE.selected([]))
        self.assertFalse(GATE.selected(["crates/rsid/src/lib.rs", "scripts/scoped-test", "docs/desktop/x.md"]))
        self.assertFalse(GATE.selected(["desktop-notes.md", "desktopish/x"]))

    def test_the_selected_lane_is_one_command_that_runs_the_lane(self):
        self.assertEqual(GATE.lane_commands(["desktop/src/api.ts"], "python3"),
                         [["python3", "scripts/desktop-test-gate.py", "--run", "--repo", "."]])
        self.assertEqual(GATE.lane_commands(["crates/rsid/src/lib.rs"], "python3"), [])

    def test_changed_paths_reads_only_the_desktop_part_of_the_diff(self):
        temp = scratch_tempdir(prefix="desktop-gate-")
        self.addCleanup(temp.cleanup)
        root = Path(temp.name)
        scratch_git(root, "init", "-q", "-b", "main")
        scratch_git(root, "config", "user.name", "T")
        scratch_git(root, "config", "user.email", "t@example.invalid")
        for name in ("desktop/src/model.ts", "crates/a/lib.rs", "docs/a.md"):
            (root / name).parent.mkdir(parents=True, exist_ok=True)
            (root / name).write_text("one\n")
        scratch_git(root, "add", ".")
        scratch_git(root, "commit", "-q", "-m", "base")
        (root / "crates/a/lib.rs").write_text("two\n")
        scratch_git(root, "commit", "-q", "-am", "elsewhere")
        self.assertEqual(GATE.changed_paths(root, "HEAD~1", "HEAD"), [])
        (root / "desktop/src/model.ts").write_text("two\n")
        scratch_git(root, "commit", "-q", "-am", "desktop")
        self.assertEqual(GATE.changed_paths(root, "HEAD~1", "HEAD"), ["desktop/src/model.ts"])
        self.assertEqual(GATE.changed_paths(root, "HEAD~2", "HEAD"), ["desktop/src/model.ts"])


MANIFEST = """[package]
name = "rsi-desktop"
version = "0.1.0"

[workspace]

[dependencies]
tauri = { version = "2", features = [] }
rsi-graph = { path = "../../crates/rsi-graph" }
rsi-common = { path = "../../crates/rsi-common", features = ["x"] }
inner = { path = "../inner" }

[dev-dependencies]
fixture-kit = { path = "../../crates/fixture-kit" }

[build-dependencies]
tauri-build = { version = "2" }

[target.'cfg(windows)'.dependencies]
winlib = { path = "../../crates/winlib" }
"""
GRAPH_MANIFEST = """[package]
name = "rsi-graph"
version = "0.1.0"

[dependencies]
deep = { path = "../deep-leaf" }
"""
PATH_DEPENDENCIES = ["crates/deep-leaf", "crates/fixture-kit", "crates/rsi-common", "crates/rsi-graph", "crates/winlib"]


def synthetic_repo(testcase, manifest=MANIFEST):
    """A scratch git repository holding a desktop manifest with path dependencies and a few other crates."""
    temp = scratch_tempdir(prefix="desktop-gate-inputs-")
    testcase.addCleanup(temp.cleanup)
    root = Path(temp.name)
    scratch_git(root, "init", "-q", "-b", "main")
    scratch_git(root, "config", "user.name", "T")
    scratch_git(root, "config", "user.email", "t@example.invalid")
    files = {"desktop/src-tauri/Cargo.toml": manifest, "desktop/src/a.ts": "1\n", "docs/keybindings.md": "1\n",
             "docs/other.md": "1\n", "crates/rsi/src/keybindings.rs": "1\n", "crates/rsi/Cargo.toml": "1\n",
             "crates/rsid/src/lib.rs": "1\n", "crates/rsi-graph/Cargo.toml": GRAPH_MANIFEST,
             "crates/rsi-graph/src/lib.rs": "1\n", "crates/rsi-common/src/lib.rs": "1\n",
             "crates/deep-leaf/src/lib.rs": "1\n", "crates/fixture-kit/src/lib.rs": "1\n",
             "crates/winlib/src/lib.rs": "1\n", "crates/unrelated/src/lib.rs": "1\n"}
    for directory in [*PATH_DEPENDENCIES, "desktop/inner"]:
        files.setdefault(directory + "/Cargo.toml", '[package]\nname = "leaf"\nversion = "0.1.0"\n')
    for name, text in files.items():
        (root / name).parent.mkdir(parents=True, exist_ok=True)
        (root / name).write_text(text)
    scratch_git(root, "add", ".")
    scratch_git(root, "commit", "-q", "-m", "base")
    return root


def touch_and_select(root, name):
    """Commit a change to `name` and the lane commands the diff selects."""
    (root / name).write_text("2\n")
    scratch_git(root, "commit", "-q", "-am", f"change {name}")
    inputs = GATE.trigger_inputs(GATE.git_reader(root, "HEAD"))
    return GATE.lane_commands(GATE.changed_paths(root, "HEAD~1", "HEAD", inputs), inputs=inputs)


class SharedSelectionCases(unittest.TestCase):
    """The lander consumes this same table with the real selector, without running a desktop build."""

    def test_python_selection_matches_the_shared_cases(self):
        fixture = json.loads((SCRIPT.parent / "tests/fixtures/desktop-selection.json").read_text())
        for case in fixture["cases"]:
            with self.subTest(case=case["name"]):
                temp = scratch_tempdir(prefix="desktop-selection-")
                self.addCleanup(temp.cleanup)
                root = Path(temp.name)
                scratch_git(root, "init", "-q", "-b", "main")
                scratch_git(root, "config", "user.name", "T")
                scratch_git(root, "config", "user.email", "t@example.invalid")
                files = {**fixture["files"], **case["overrides"]}
                if case["selector"] != "absent":
                    files[GATE.SCRIPT] = SCRIPT.read_text()
                for name, contents in files.items():
                    if contents is not None:
                        (root / name).parent.mkdir(parents=True, exist_ok=True)
                        (root / name).write_text(contents)
                scratch_git(root, "add", ".")
                scratch_git(root, "commit", "-q", "-m", "base")
                target = root / case["path"]
                if case["selector"] == "rename":
                    target.rename(target.with_name("renamed.py"))
                elif case["contents"] is None:
                    target.unlink()
                else:
                    target.write_text(case["contents"])
                scratch_git(root, "add", "--", case["path"])
                if case["selector"] == "rename":
                    scratch_git(root, "add", "--", "scripts/renamed.py")
                scratch_git(root, "commit", "-q", "-m", "candidate")
                if case["expected"] == "refused":
                    with self.assertRaisesRegex(GATE.SelectorMissing, GATE.SCRIPT):
                        GATE.require_selector(root, "HEAD~1", "HEAD")
                    with self.assertRaisesRegex(GATE.SelectorMissing, GATE.SCRIPT):
                        GATE.lane_selected(root, "HEAD~1", "HEAD")
                    cli = subprocess.run([sys.executable, str(SCRIPT), "--repo", str(root),
                                          "--base", "HEAD~1"], capture_output=True, text=True)
                    self.assertNotEqual(cli.returncode, 0)
                    self.assertIn(GATE.SCRIPT, cli.stderr)
                else:
                    selected = case["expected"] == "selected"
                    self.assertEqual(GATE.lane_selected(root, "HEAD~1", "HEAD"), selected)
                    inputs = GATE.trigger_inputs(GATE.git_reader(root, "HEAD"))
                    commands = GATE.lane_commands(GATE.changed_paths(root, "HEAD~1", "HEAD", inputs), inputs=inputs)
                    self.assertEqual(commands, [GATE.lane_command("python3")] if selected else [])


class TriggerInputs(unittest.TestCase):
    """The lane's trigger set is derived from what the suite reads (#3042) and shared with the copy list."""

    def test_the_inputs_are_desktop_the_files_its_tests_read_and_the_manifests_path_dependencies(self):
        root = synthetic_repo(self)
        read = GATE.fs_reader(root)
        self.assertEqual(GATE.outside_inputs(read), sorted([*GATE.OUTSIDE_INPUTS, *PATH_DEPENDENCIES]))
        self.assertEqual(GATE.trigger_inputs(read)[0], "desktop")
        # The same list from the committed tree.
        self.assertEqual(GATE.trigger_inputs(GATE.git_reader(root, "HEAD")), GATE.trigger_inputs(read))

    def test_a_dependency_outside_the_repository_or_inside_desktop_is_not_an_outside_input(self):
        inputs = GATE.outside_inputs(GATE.fs_reader(synthetic_repo(
            self, MANIFEST + '\n[dependencies.outside]\npath = "../../../elsewhere"\n')))
        self.assertFalse([item for item in inputs if "elsewhere" in item or item.startswith("desktop") or ".." in item])

    def test_a_manifest_without_path_dependencies_or_missing_still_has_the_static_inputs(self):
        static = sorted(GATE.OUTSIDE_INPUTS)
        self.assertEqual(GATE.outside_inputs(lambda relative: None), static)
        self.assertEqual(GATE.outside_inputs(lambda relative: "not = [toml"), static)
        self.assertEqual(GATE.outside_inputs(lambda relative: '[dependencies]\ntauri = "2"\n'), static)

    def test_a_keybindings_only_diff_selects_the_lane(self):
        root = synthetic_repo(self)
        self.assertEqual(len(touch_and_select(root, "docs/keybindings.md")), 1)

    def test_a_tui_source_change_selects_the_lane(self):
        root = synthetic_repo(self)
        self.assertEqual(len(touch_and_select(root, "crates/rsi/src/keybindings.rs")), 1)

    def test_a_path_dependency_change_selects_the_lane_including_a_transitive_one(self):
        root = synthetic_repo(self)
        for name in ("crates/rsi-graph/src/lib.rs", "crates/rsi-common/src/lib.rs", "crates/deep-leaf/src/lib.rs",
                     "crates/fixture-kit/src/lib.rs", "crates/winlib/src/lib.rs"):
            self.assertEqual(len(touch_and_select(root, name)), 1, name)

    def test_a_desktop_change_still_selects_the_lane(self):
        self.assertEqual(len(touch_and_select(synthetic_repo(self), "desktop/src/a.ts")), 1)

    def test_a_change_outside_every_input_selects_nothing(self):
        root = synthetic_repo(self)
        for name in ("crates/rsid/src/lib.rs", "crates/unrelated/src/lib.rs", "docs/other.md", "crates/rsi/Cargo.toml"):
            self.assertEqual(touch_and_select(root, name), [], name)

    def test_a_new_path_dependency_needs_no_edit_to_the_gate(self):
        root = synthetic_repo(self)
        manifest = root / "desktop/src-tauri/Cargo.toml"
        manifest.write_text(MANIFEST + '\n[dependencies.newcomer]\npath = "../../crates/unrelated"\n')
        scratch_git(root, "commit", "-q", "-am", "add a path dependency")
        self.assertEqual(len(touch_and_select(root, "crates/unrelated/src/lib.rs")), 1)

    def test_the_copy_is_the_whole_candidate_tree_not_just_the_trigger_inputs(self):
        root = synthetic_repo(self)
        stage = root.parent / (root.name + "-stage")
        stage.mkdir()
        self.addCleanup(shutil.rmtree, stage, ignore_errors=True)
        GATE.stage_tree(root, stage)
        tracked = subprocess.run(["git", "-C", str(root), "ls-files"], check=True, capture_output=True,
                                 text=True).stdout.split()
        copied = sorted(path.relative_to(stage).as_posix() for path in stage.rglob("*") if path.is_file())
        self.assertEqual(copied, sorted(tracked))
        self.assertTrue((stage / "crates/deep-leaf/src/lib.rs").is_file())
        self.assertTrue((stage / "crates/unrelated/src/lib.rs").is_file())

    def test_a_candidate_without_the_selector_script_fails_closed_when_the_diff_needs_it(self):
        # #355: a deleted or renamed selector, or a lane input changed in a tree without it, is never an
        # empty selection; a diff that needs no lane is unaffected.
        root = synthetic_repo(self)
        script = root / GATE.SCRIPT
        script.parent.mkdir(parents=True, exist_ok=True)
        script.write_text("# selector\n")
        scratch_git(root, "add", GATE.SCRIPT)
        scratch_git(root, "commit", "-q", "-m", "add the selector")
        scratch_git(root, "mv", GATE.SCRIPT, "scripts/desktop-test-gate-renamed.py")
        scratch_git(root, "commit", "-q", "-m", "rename the selector")
        with self.assertRaisesRegex(GATE.SelectorMissing, "scripts/desktop-test-gate.py is missing"):
            GATE.lane_selected(root, "HEAD~1", "HEAD")
        scratch_git(root, "rm", "-q", "scripts/desktop-test-gate-renamed.py")
        scratch_git(root, "commit", "-q", "-m", "delete it")
        with self.assertRaises(GATE.SelectorMissing):
            GATE.lane_selected(root, "HEAD~2", "HEAD")
        # Missing from both sides: a lane input refuses, anything else selects nothing.
        for name, refused in (("desktop/src/a.ts", True), ("docs/keybindings.md", True), ("docs/other.md", False)):
            (root / name).write_text("changed\n")
            scratch_git(root, "commit", "-q", "-am", name)
            if refused:
                with self.assertRaises(GATE.SelectorMissing, msg=name):
                    GATE.lane_selected(root, "HEAD~1", "HEAD")
                continue
            self.assertFalse(GATE.lane_selected(root, "HEAD~1", "HEAD"), name)

    def test_the_command_line_selects_on_a_path_dependency_change(self):
        root = synthetic_repo(self)
        (root / GATE.SCRIPT).parent.mkdir(parents=True, exist_ok=True)
        (root / GATE.SCRIPT).write_text(SCRIPT.read_text())
        scratch_git(root, "add", "--", GATE.SCRIPT)
        scratch_git(root, "commit", "-q", "-m", "selector")
        (root / "crates/rsi-graph/src/lib.rs").write_text("2\n")
        scratch_git(root, "commit", "-q", "-am", "dependency")
        selected = json.loads(subprocess.run([sys.executable, "-I", str(SCRIPT), "--repo", str(root), "--base", "HEAD~1"],
                                             check=True, capture_output=True, text=True).stdout)
        self.assertEqual(selected[0][1:], ["scripts/desktop-test-gate.py", "--run", "--repo", "."])


INHERITING_ROOT = """[workspace]
resolver = "3"
members = ["crates/inheriting", "crates/not-staged"]

[workspace.package]
version = "0.1.0"
edition = "2024"
license = "AGPL-3.0-only"

[workspace.dependencies]
shared-leaf = { path = "crates/shared-leaf" }

[workspace.lints.rust]
unused_must_use = "warn"
"""
INHERITING_DESKTOP = """[package]
name = "rsi-desktop"
version = "0.1.0"
edition = "2021"

[workspace]

[lib]
path = "src/lib.rs"

[dependencies]
inheriting = { path = "../../crates/inheriting" }
"""
INHERITING_CRATE = """[package]
name = "inheriting"
version.workspace = true
edition.workspace = true
license.workspace = true

[dependencies]
shared-leaf = { workspace = true }

[lints]
workspace = true
"""
# The desktop crate compiles a file outside desktop/ in, like commandmode.rs does with the TUI manual (#3129).
INCLUDING_LIB = '''pub const MANUAL: &str = include_str!("../../../docs/manual/included.md");\n'''
LEAF_CRATE = """[package]
name = "shared-leaf"
version.workspace = true
edition.workspace = true
"""


def inheriting_repo(testcase):
    """A scratch repository whose desktop app depends on a crate inheriting from the root workspace (#3114)."""
    temp = scratch_tempdir(prefix="desktop-gate-inherit-")
    testcase.addCleanup(temp.cleanup)
    root = Path(temp.name) / "repo"
    files = {"Cargo.toml": INHERITING_ROOT, "desktop/src-tauri/Cargo.toml": INHERITING_DESKTOP,
             "desktop/src-tauri/src/lib.rs": INCLUDING_LIB, "docs/manual/included.md": "# manual\n",
             "crates/inheriting/Cargo.toml": INHERITING_CRATE,
             "crates/inheriting/src/lib.rs": "", "crates/shared-leaf/Cargo.toml": LEAF_CRATE,
             "crates/shared-leaf/src/lib.rs": "", "crates/not-staged/Cargo.toml": LEAF_CRATE.replace("shared-leaf", "n"),
             "crates/not-staged/src/lib.rs": ""}
    for name, text in files.items():
        (root / name).parent.mkdir(parents=True, exist_ok=True)
        (root / name).write_text(text)
    return root


class InheritedWorkspaceFields(unittest.TestCase):
    """The lane builds the desktop crate in a full checkout: inherited fields and outside includes resolve (#3114, #3129)."""

    def stage(self, root):
        stage = root.parent / "stage"
        stage.mkdir()
        GATE.stage_tree(root, stage)
        return stage

    def test_a_dependency_named_through_workspace_dependencies_is_an_input_too(self):
        root = inheriting_repo(self)
        inputs = GATE.outside_inputs(GATE.fs_reader(root))
        self.assertEqual(inputs, sorted([*GATE.OUTSIDE_INPUTS, "crates/inheriting",
                                         "crates/shared-leaf"]))

    def test_the_real_root_manifest_and_every_crate_are_staged_unchanged(self):
        root = inheriting_repo(self)
        stage = self.stage(root)
        self.assertEqual((stage / "Cargo.toml").read_text(encoding="utf-8"), INHERITING_ROOT)
        self.assertTrue((stage / "crates/not-staged/Cargo.toml").is_file())
        self.assertTrue((stage / "docs/manual/included.md").is_file())

    @unittest.skipUnless(shutil.which("cargo"), "cargo is not installed")
    def test_cargo_builds_the_desktop_crate_with_an_outside_include_and_an_inherited_dependency(self):
        stage = self.stage(inheriting_repo(self))
        env = {**os.environ, "CARGO_TARGET_DIR": str(stage / "target")}
        result = subprocess.run(["cargo", "check", "--offline", "--lib"], cwd=stage / "desktop/src-tauri", env=env,
                                capture_output=True, text=True)
        self.assertEqual(result.returncode, 0, result.stderr)
        metadata = subprocess.run(["cargo", "metadata", "--format-version", "1", "--offline"],
                                  cwd=stage / "desktop/src-tauri", env=env, capture_output=True, text=True)
        self.assertEqual(metadata.returncode, 0, metadata.stderr)
        packages = {package["name"]: package for package in json.loads(metadata.stdout)["packages"]}
        self.assertEqual(packages["inheriting"]["edition"], "2024")
        self.assertEqual(packages["shared-leaf"]["edition"], "2024")


class CommittedManifest(unittest.TestCase):
    """The lane's inputs on the real committed tree (hub #3089): what the desktop app actually consumes.

    The synthetic manifest above proves transitive path derivation; only the committed lock says which
    repository crates the app consumes, so these tests derive their expectations from it."""

    ROOT = Path(__file__).resolve().parents[2]

    def real_inputs(self):
        return GATE.trigger_inputs(GATE.fs_reader(self.ROOT))

    def local_lock_packages(self):
        """Packages of the desktop's Cargo.lock that are not from a registry or git: its real local dependencies."""
        import tomllib
        lock = tomllib.loads((self.ROOT / "desktop/src-tauri/Cargo.lock").read_text(encoding="utf-8"))
        return sorted(p["name"] for p in lock["package"] if "source" not in p and p["name"] != "rsi-desktop")

    def local_crate_directories(self):
        """The crate directory of each local lock package (a repository crate is `crates/<package name>`)."""
        directories = [f"crates/{name}" for name in self.local_lock_packages()]
        for directory in directories:
            self.assertTrue((self.ROOT / directory / "Cargo.toml").is_file(), directory)
        return directories

    def test_the_committed_trigger_set_is_the_desktop_root_manifest_the_lock_crates_and_the_outside_inputs(self):
        expected = ["desktop", "Cargo.toml", *self.local_crate_directories(), "crates/rsi/src", "docs/keybindings.md"]
        self.assertEqual(sorted(self.real_inputs()), sorted(expected))
        self.assertEqual(GATE.trigger_inputs(GATE.git_reader(self.ROOT, "HEAD")), self.real_inputs())

    def test_the_lock_names_no_local_package_so_no_crate_directory_is_missing_from_the_inputs(self):
        # Every local package of the desktop lock (no `source`) is a repository crate the app consumes, so its
        # directory must be a trigger input, and no other crate (crates/rsid) is. Adding a path dependency must
        # add its directory here: the expectation is derived from the lock, never from a hard-coded list.
        inputs = self.real_inputs()
        directories = self.local_crate_directories()
        self.assertTrue(directories, "the desktop app path-depends on repository crates")
        for directory in directories:
            self.assertIn(directory, inputs)
        crate_inputs = sorted(i for i in inputs if i.startswith("crates/") and i != "crates/rsi/src")
        self.assertEqual(crate_inputs, sorted(directories))

    def test_the_committed_set_selects_the_inputs_and_not_crates_the_app_does_not_consume(self):
        inputs = self.real_inputs()
        consumed = [f"{directory}/src/lib.rs" for directory in self.local_crate_directories()]
        for path in ("Cargo.toml", "desktop/src/model.ts", "docs/keybindings.md", "crates/rsi/src/ui/mod.rs", *consumed):
            self.assertTrue(GATE.selected([path], inputs), path)
        for path in ("crates/rsid/src/lib.rs", "README.md"):
            self.assertFalse(GATE.selected([path], inputs), path)
        # The two crates the app consumes today are named so dropping one is a deliberate edit.
        for directory in ("crates/rsi-common", "crates/rsi-graph"):
            self.assertIn(directory, self.local_crate_directories())


class RunnerPreflight(unittest.TestCase):
    def test_every_runner_present_is_available_on_linux(self):
        every = present("node", "npm", "cargo", "pkg-config")
        self.assertIsNone(GATE.runner_status("linux", every, found(0)))

    def test_a_missing_runner_is_the_typed_unavailable_result_with_zero_tests(self):
        for missing in ("node", "npm", "cargo"):
            tools = [tool for tool in ("node", "npm", "cargo", "pkg-config") if tool != missing]
            status = GATE.runner_status("linux", present(*tools), found(0))
            self.assertEqual((status["status"], status["runner"], status["executed_tests"]),
                             ("runner_unavailable", missing, 0), missing)
            self.assertEqual(status["lane"], "desktop")
            self.assertIn(missing, status["reason"])

    def test_linux_needs_pkg_config_and_the_webkit_development_files(self):
        status = GATE.runner_status("linux", present("node", "npm", "cargo"), found(0))
        self.assertEqual((status["status"], status["runner"]), ("runner_unavailable", "pkg-config"))
        every = present("node", "npm", "cargo", "pkg-config")
        status = GATE.runner_status("linux", every, found(1))
        self.assertEqual((status["status"], status["runner"], status["executed_tests"]),
                         ("runner_unavailable", "webkit2gtk-4.1", 0))

        def cannot_run(*args, **kwargs):
            raise OSError("exec format error")
        status = GATE.runner_status("linux", every, cannot_run)
        self.assertEqual(status["runner"], "webkit2gtk-4.1")
        self.assertIn("cannot run", status["reason"])

    def test_other_platforms_need_no_webkit_probe(self):
        tools = present("node", "npm", "cargo")
        for platform in ("darwin", "win32"):
            self.assertIsNone(GATE.runner_status(platform, tools, found(1)), platform)
        self.assertEqual(GATE.runner_status("darwin", present("npm", "cargo"), found(0))["runner"], "node")


class PrivateTree(unittest.TestCase):
    def setUp(self):
        self.temp = scratch_tempdir(prefix="desktop-gate-tree-")
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)

    def write(self, path, text="x\n"):
        target = self.root / path
        target.parent.mkdir(parents=True, exist_ok=True)
        target.write_text(text)

    def test_the_copy_leaves_out_what_is_rebuilt_or_machine_local_and_keeps_the_rest(self):
        for name in ("desktop/package.json", "desktop/src/gen/keep.ts", "desktop/src-tauri/src/lib.rs",
                     "desktop/tests/model.test.mjs", "desktop/node_modules/typescript/index.js",
                     "desktop/dist/main.js", "desktop/src-tauri/gen/schema.json",
                     "desktop/src-tauri/target/debug/x", "rust-toolchain.toml"):
            self.write("repo/" + name)
        stage = self.root / "stage"
        stage.mkdir()
        GATE.stage_tree(self.root / "repo", stage)
        copied = sorted(path.relative_to(stage).as_posix() for path in stage.rglob("*") if path.is_file())
        self.assertEqual(copied, ["desktop/package.json", "desktop/src-tauri/src/lib.rs", "desktop/src/gen/keep.ts",
                                  "desktop/tests/model.test.mjs", "rust-toolchain.toml"])
        # The committed tree is untouched.
        self.assertTrue((self.root / "repo/desktop/node_modules/typescript/index.js").is_file())

    def test_the_copy_carries_every_repository_file_not_only_the_desktop_inputs(self):
        names = ["desktop/package.json", "docs/keybindings.md", "docs/other.md", "docs/manual/rsi-manual.md",
                 "crates/rsi/src/keybindings.rs", "crates/rsi/src/ui/overlay/manager_tree.rs",
                 "crates/rsid/src/lib.rs", "Cargo.toml", ".cargo/config.toml"]
        for name in names:
            self.write("repo/" + name)
        self.write("repo/crates/rsid/target/debug/x")
        self.write("repo/.git/HEAD")
        stage = self.root / "stage"
        stage.mkdir()
        GATE.stage_tree(self.root / "repo", stage)
        copied = sorted(path.relative_to(stage).as_posix() for path in stage.rglob("*") if path.is_file())
        self.assertEqual(copied, sorted(names))

    def test_a_git_checkout_copies_tracked_and_new_files_and_leaves_ignored_output_out(self):
        root = synthetic_repo(self)
        (root / ".gitignore").write_text("scratch-output/\n")
        (root / "scratch-output").mkdir()
        (root / "scratch-output/big.bin").write_text("x")
        (root / "docs/untracked.md").write_text("new\n")
        (root / "docs/other.md").unlink()
        stage = root.parent / "git-stage"
        stage.mkdir()
        self.addCleanup(shutil.rmtree, stage, ignore_errors=True)
        GATE.stage_tree(root, stage)
        self.assertTrue((stage / "docs/untracked.md").is_file())
        self.assertTrue((stage / "crates/unrelated/src/lib.rs").is_file())
        self.assertFalse((stage / "scratch-output").exists())
        self.assertFalse((stage / "docs/other.md").exists())
        self.assertFalse((stage / ".git").exists())

    def test_the_environment_keeps_the_callers_wrapper_and_points_caches_at_private_places(self):
        base = {"RUSTC_WRAPPER": "/install/rsi-build-rustc", "CARGO_TARGET_DIR": "/job/target",
                "RSI_PROCESS_OWNERSHIP_NAMESPACE": "ns", "PATH": "/bin"}
        env = GATE.lane_env(base, "/scratch")
        self.assertEqual(env["RUSTC_WRAPPER"], "/install/rsi-build-rustc")
        self.assertEqual(env["CARGO_TARGET_DIR"], str(Path("/job/target/desktop-tauri")))
        self.assertEqual(env["npm_config_cache"], str(Path("/scratch/npm-cache")))
        self.assertNotIn("RSI_PROCESS_OWNERSHIP_NAMESPACE", env)
        self.assertEqual(env["CARGO_INCREMENTAL"], "0")
        own = GATE.lane_env({**base, "DESKTOP_LANE_CARGO_TARGET": "/q/t", "DESKTOP_LANE_NPM_CACHE": "/q/n"}, "/scratch")
        self.assertEqual((own["CARGO_TARGET_DIR"], own["npm_config_cache"]), ("/q/t", "/q/n"))
        bare = GATE.lane_env({"PATH": "/bin"}, "/scratch")
        self.assertEqual(bare["CARGO_TARGET_DIR"], str(Path("/scratch/cargo-target")))


class Verdicts(unittest.TestCase):
    def setUp(self):
        self.temp = scratch_tempdir(prefix="desktop-gate-run-")
        self.addCleanup(self.temp.cleanup)
        self.repo = Path(self.temp.name) / "repo"
        for name in ("package.json", "src-tauri/Cargo.toml"):
            (self.repo / "desktop" / name).parent.mkdir(parents=True, exist_ok=True)
            (self.repo / "desktop" / name).write_text("x\n")
        self.calls = []

    def lane(self, outcomes, status=lambda: None):
        """Run the lane with scripted (code, output) per step name; the steps never start a process."""
        def step(command, cwd, env, name):
            self.calls.append((name, Path(cwd)))
            code, output = outcomes[name]
            return code, output.splitlines(keepends=True)
        with patch("builtins.print"), patch.object(GATE.shutil, "which", return_value="/usr/bin/npm"):
            return GATE.run_lane(self.repo, {"PATH": "/bin"}, step=step, status=status)

    def test_both_sets_passing_counts_their_tests_and_is_green(self):
        code, result = self.lane({"npm ci": (0, ""), "npm test": (0, TAP), "cargo test": (0, CARGO)})
        self.assertEqual(code, 0)
        self.assertEqual((result["status"], result["npm_tests"], result["cargo_tests"], result["executed_tests"]),
                         ("passed", 31, 31, 62))
        self.assertEqual([name for name, _ in self.calls], ["npm ci", "npm test", "cargo test"])
        self.assertEqual(self.calls[2][1].name, "src-tauri")

    def test_the_spec_reporter_and_cargo_sums_are_counted_too(self):
        code, result = self.lane({"npm ci": (0, ""), "npm test": (0, SPEC_REPORTER),
                                  "cargo test": (0, CARGO + "test result: ok. 4 passed; 0 failed;\n")})
        self.assertEqual((code, result["npm_tests"], result["cargo_tests"]), (0, 7, 35))

    def test_the_run_never_touches_the_committed_tree_and_removes_its_private_copy(self):
        self.lane({"npm ci": (0, ""), "npm test": (0, TAP), "cargo test": (0, CARGO)})
        frontend = self.calls[0][1]
        self.assertEqual(frontend.name, "desktop")
        self.assertNotEqual(frontend, self.repo / "desktop")
        self.assertFalse(frontend.exists(), "the private copy is removed")
        self.assertFalse((self.repo / "desktop" / "node_modules").exists())

    def test_a_failing_frontend_test_fails_the_lane_before_cargo_starts(self):
        code, result = self.lane({"npm ci": (0, ""), "npm test": (1, TAP.replace("# fail 0", "# fail 2"))})
        self.assertEqual((code, result["status"], result["failed_step"]), (1, "failed", "npm test"))
        self.assertEqual([name for name, _ in self.calls], ["npm ci", "npm test"])

    def test_a_failing_cargo_test_fails_the_lane(self):
        code, result = self.lane({"npm ci": (0, ""), "npm test": (0, TAP), "cargo test": (101, "test result: FAILED. 1 passed; 1 failed\n")})
        self.assertEqual((code, result["status"], result["failed_step"]), (1, "failed", "cargo test"))

    def test_a_locked_update_refusal_names_the_lock_refresh_in_a_typed_result(self):
        for diagnostic in (
            "error: cannot update the lock file /copy/desktop/src-tauri/Cargo.lock because --locked was passed to prevent this",
            "error: the lock file /copy/desktop/src-tauri/Cargo.lock needs to be updated but --locked was passed to prevent this",
            r"error: cannot update the lock file C:\copy\desktop\src-tauri\Cargo.lock because --locked was passed to prevent this",
        ):
            with self.subTest(diagnostic=diagnostic):
                code, result = self.lane({"npm ci": (0, ""), "npm test": (0, TAP),
                                          "cargo test": (101, diagnostic + "\n")})
                self.assertEqual((code, result["status"], result["failed_step"], result["exit_code"]),
                                 (1, "desktop_lock_stale", "cargo test", 101))
                self.assertIn("refresh desktop/src-tauri/Cargo.lock", result["reason"])
                self.assertIn("cargo update --workspace --manifest-path desktop/src-tauri/Cargo.toml", result["reason"])
                self.assertEqual(result["executed_tests"], 31)

    def test_other_cargo_errors_keep_their_failure_status(self):
        for diagnostic in ("error: failed to download `block2 v0.6.2`\n--offline was specified\n",
                           "error: failed to get a dependency\n", "error[E0308]: mismatched types\n"):
            with self.subTest(diagnostic=diagnostic):
                code, result = self.lane({"npm ci": (0, ""), "npm test": (0, TAP),
                                          "cargo test": (101, diagnostic)})
                self.assertEqual((code, result["status"]), (1, "failed"))

    def test_a_failing_install_fails_the_lane_without_running_tests(self):
        code, result = self.lane({"npm ci": (1, "")})
        self.assertEqual((code, result["failed_step"], result["executed_tests"]), (1, "npm ci", 0))
        self.assertEqual([name for name, _ in self.calls], ["npm ci"])

    def test_a_test_set_that_executes_nothing_is_no_evidence_and_red(self):
        code, result = self.lane({"npm ci": (0, ""), "npm test": (0, "no tap here\n"), "cargo test": (0, CARGO)})
        self.assertEqual((code, result["status"], result["npm_tests"]), (1, "no_tests_executed", 0))
        code, result = self.lane({"npm ci": (0, ""), "npm test": (0, TAP),
                                  "cargo test": (0, "test result: ok. 0 passed; 0 failed;\n")})
        self.assertEqual((code, result["status"], result["cargo_tests"]), (1, "no_tests_executed", 0))

    def test_a_missing_runner_is_runner_unavailable_with_zero_tests_and_runs_nothing(self):
        missing = GATE.unavailable("node", "node is not installed")
        code, result = self.lane({}, status=lambda: missing)
        self.assertEqual((code, result["status"], result["executed_tests"]), (1, "runner_unavailable", 0))
        self.assertEqual(self.calls, [])

    def test_the_command_line_prints_the_selection_and_the_result_line(self):
        temp = scratch_tempdir(prefix="desktop-gate-cli-")
        self.addCleanup(temp.cleanup)
        root = Path(temp.name)
        scratch_git(root, "init", "-q", "-b", "main")
        scratch_git(root, "config", "user.name", "T")
        scratch_git(root, "config", "user.email", "t@example.invalid")
        (root / "desktop").mkdir()
        (root / "desktop/a.ts").write_text("1\n")
        (root / "b.txt").write_text("1\n")
        (root / GATE.SCRIPT).parent.mkdir(parents=True, exist_ok=True)
        (root / GATE.SCRIPT).write_text(SCRIPT.read_text())
        scratch_git(root, "add", ".")
        scratch_git(root, "commit", "-q", "-m", "base")
        (root / "b.txt").write_text("2\n")
        scratch_git(root, "commit", "-q", "-am", "elsewhere")
        (root / "desktop/a.ts").write_text("2\n")
        scratch_git(root, "commit", "-q", "-am", "desktop")
        select = lambda base: json.loads(subprocess.run(  # noqa: E731
            [sys.executable, "-I", str(SCRIPT), "--repo", str(root), "--base", base], check=True,
            capture_output=True, text=True).stdout)
        self.assertEqual(select("HEAD~2")[0][1:], ["scripts/desktop-test-gate.py", "--run", "--repo", "."])
        self.assertEqual(select("HEAD~1")[0][1:], ["scripts/desktop-test-gate.py", "--run", "--repo", "."])
        scratch_git(root, "commit", "-q", "--allow-empty", "-m", "empty")
        self.assertEqual(select("HEAD~1"), [])


@unittest.skipUnless(shutil.which("cargo"), "Cargo resolves the local-only lock fixture")
class LockConsistency(unittest.TestCase):
    def test_dependency_change_selects_the_lane_and_requires_its_own_lock_refresh(self):
        temp = scratch_tempdir(prefix="desktop-lock-fixture-")
        self.addCleanup(temp.cleanup)
        root = Path(temp.name)
        files = {"Cargo.toml": '[workspace]\nmembers = ["crates/*"]\nresolver = "2"\n',
                 "README.md": "fixture\n", GATE.SCRIPT: SCRIPT.read_text(),
                 "desktop/src-tauri/Cargo.toml": '[package]\nname = "app"\nversion = "0.1.0"\n'
                 'edition = "2021"\n[workspace]\n[dependencies]\ncommon = {path = "../../crates/common"}\n',
                 "desktop/src-tauri/src/lib.rs": ""}
        for name in ("common", "graph", "added"):
            files[f"crates/{name}/Cargo.toml"] = (f'[package]\nname = "{name}"\nversion = "0.1.0"\n'
                                                 'edition = "2021"\n')
            files[f"crates/{name}/src/lib.rs"] = ""
        files["crates/common/Cargo.toml"] += '[dependencies]\ngraph = {path = "../graph"}\n'
        for name, contents in files.items():
            path = root / name
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_text(contents)

        def metadata(directory, locked=False):
            # Real Cargo resolution, entirely local and offline, with no compiler/build scripts.
            return subprocess.run(["cargo", "metadata", "--format-version", "1", "--offline",
                                   *(["--locked"] if locked else [])], cwd=directory,
                                  capture_output=True, text=True, timeout=30)

        for directory in (root, root / "desktop/src-tauri"):
            initial = metadata(directory)
            self.assertEqual(initial.returncode, 0, initial.stderr)
        scratch_git(root, "init", "-q", "-b", "main")
        scratch_git(root, "config", "user.name", "T")
        scratch_git(root, "config", "user.email", "t@example.invalid")
        scratch_git(root, "add", ".")
        scratch_git(root, "commit", "-qm", "base")
        desktop_lock = root / "desktop/src-tauri/Cargo.lock"
        old_desktop_lock = desktop_lock.read_bytes()
        graph = root / "crates/graph/Cargo.toml"
        graph.write_text(graph.read_text() + '[dependencies]\nadded = {path = "../added"}\n')
        updated = metadata(root)
        self.assertEqual(updated.returncode, 0, updated.stderr)
        scratch_git(root, "add", "Cargo.lock", "crates/graph/Cargo.toml")
        scratch_git(root, "commit", "-qm", "dependency change with only workspace lock refreshed")
        changed = scratch_git(root, "diff", "--name-only", "HEAD^", "HEAD").stdout.splitlines()
        self.assertIn("Cargo.lock", changed)
        self.assertEqual(desktop_lock.read_bytes(), old_desktop_lock)
        self.assertTrue(GATE.lane_selected(root, "HEAD^", "HEAD"))
        inputs = GATE.trigger_inputs(GATE.git_reader(root, "HEAD"))
        self.assertEqual(GATE.lane_commands(changed, inputs=inputs), [GATE.lane_command("python3")])

        def step(command, cwd, env, name):
            if name == "cargo test":
                self.assertEqual(command, ["cargo", "test", "--locked"])
                resolved = metadata(cwd, locked=True)
                return resolved.returncode, (resolved.stderr if resolved.returncode else CARGO).splitlines(True)
            return 0, (TAP if name == "npm test" else "").splitlines(True)

        def lane():
            with contextlib.redirect_stdout(io.StringIO()) as output:
                code, result = GATE.run_lane(root, step=step, status=lambda: None)
            return code, result, output.getvalue()

        code, result, output = lane()
        self.assertEqual((code, result["status"]), (1, "desktop_lock_stale"))
        self.assertIn("refresh desktop/src-tauri/Cargo.lock", output)
        self.assertEqual(desktop_lock.read_bytes(), old_desktop_lock)
        refreshed = subprocess.run(["cargo", "update", "--workspace", "--offline", "--manifest-path",
                                    "desktop/src-tauri/Cargo.toml"], cwd=root,
                                   capture_output=True, text=True, timeout=30)
        self.assertEqual(refreshed.returncode, 0, refreshed.stderr)
        self.assertIn('"added"', desktop_lock.read_text())
        code, result, _ = lane()
        self.assertEqual((code, result["status"]), (0, "passed"))
        scratch_git(root, "add", "desktop/src-tauri/Cargo.lock")
        scratch_git(root, "commit", "-qm", "refresh desktop lock")
        selected = touch_and_select(root, "README.md")
        self.assertEqual(selected, [])


class StepBounds(unittest.TestCase):
    def setUp(self):
        self.temp = scratch_tempdir(prefix="desktop-gate-bound-")
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)

    def test_fast_steps_keep_their_output_and_exit_code(self):
        for name, (variable, _) in GATE.STEP_TIMEOUTS.items():
            for expected in (0, 7):
                with self.subTest(step=name, code=expected), contextlib.redirect_stdout(io.StringIO()) as output:
                    code, lines = GATE.run_step([sys.executable, "-c", f"print('healthy'); raise SystemExit({expected})"],
                                                self.root, {**os.environ, variable: "5"}, name)
                    self.assertEqual((code, lines), (expected, ["healthy\n"]))
                    self.assertIn("healthy", output.getvalue())

    def test_closing_stdout_does_not_remove_the_process_wait_deadline(self):
        with contextlib.redirect_stdout(io.StringIO()), self.assertRaises(GATE.StepTimedOut):
            GATE.run_step([sys.executable, "-c", "import os, time; os.close(1); os.close(2); time.sleep(60)"],
                          self.root, {**os.environ, "DESKTOP_LANE_NPM_TEST_TIMEOUT_SECONDS": "0.2"}, "npm test")

    def test_invalid_bounds_cannot_disable_the_deadline(self):
        variable, _ = GATE.STEP_TIMEOUTS["npm test"]
        for value in ("0", "-1", "inf", "nan", "bad"):
            with self.subTest(value=value), self.assertRaises(ValueError):
                GATE.run_step([sys.executable], self.root, {variable: value}, "npm test")

    @unittest.skipUnless(os.name == "posix", "Unix process-group acceptance")
    def test_each_wedged_lane_step_fails_with_a_typed_result_and_no_surviving_child(self):
        # Ignore TERM in the leader so only group-wide TERM reaches the child;
        # ignore SIGCHLD so the child is reaped before the leader needs SIGKILL.
        stub = self.root / "wedged.py"
        stub.write_text("import os, signal, subprocess, sys\n"
                        "from pathlib import Path\n"
                        "signal.signal(signal.SIGTERM, signal.SIG_IGN)\n"
                        "signal.signal(signal.SIGCHLD, signal.SIG_IGN)\n"
                        "child = subprocess.Popen([sys.executable, '-c', "
                        "'import signal; signal.signal(signal.SIGTERM, signal.SIG_DFL); print(\"ready\", flush=True); signal.pause()'])\n"
                        "Path(sys.argv[1]).write_text(str(child.pid))\n"
                        "while True: signal.pause()\n")
        repo = self.root / "repo"
        (repo / "desktop/src-tauri").mkdir(parents=True)
        (repo / "desktop/src-tauri/Cargo.toml").write_text("# stub\n")
        for name, (variable, _) in GATE.STEP_TIMEOUTS.items():
            with self.subTest(step=name):
                pid_file = self.root / (name.replace(" ", "-") + ".pid")
                calls = []

                def step(command, cwd, env, current):
                    calls.append(current)
                    if current == name:
                        return GATE.run_step([sys.executable, str(stub), str(pid_file)], cwd, env, current)
                    return 0, (TAP if current == "npm test" else CARGO).splitlines(keepends=True)

                started = time.monotonic()
                with contextlib.redirect_stdout(io.StringIO()) as output:
                    code, result = GATE.run_lane(repo, {**os.environ, variable: "1"}, step=step,
                                                 status=lambda: None)
                    # Same serialization as the gate CLI, consumed by scoped-test and logged by the lander.
                    print(GATE.RESULT_MARK + json.dumps(result))
                self.assertLess(time.monotonic() - started, 8)
                self.assertEqual(code, 1)
                self.assertEqual((result["status"], result["failed_step"], result["timeout_seconds"]),
                                 ("desktop_lane_timed_out", name, 1))
                self.assertEqual(calls[-1], name)
                self.assertIn(f"desktop_lane_timed_out: {name}", output.getvalue())
                reported = json.loads(output.getvalue().split(GATE.RESULT_MARK)[-1])
                self.assertEqual(reported, result)
                child_pid = int(pid_file.read_text())
                try:
                    with self.assertRaises(ProcessLookupError, msg=f"child {child_pid} survived {name}"):
                        os.kill(child_pid, 0)
                finally:
                    try:
                        os.kill(child_pid, signal.SIGKILL)
                    except ProcessLookupError:
                        pass

    @unittest.skipUnless(os.name == "posix" and shutil.which("node"), "Unix Chrome group inheritance needs node")
    def test_chrome_launcher_keeps_browser_and_grandchild_in_the_bounded_step_group(self):
        browser = self.root / "browser"
        pid_file = self.root / "browser.pids"
        browser.write_text(f"#!{sys.executable}\n"
                           "import json, os, signal, subprocess, sys\n"
                           "from pathlib import Path\n"
                           "child = subprocess.Popen([sys.executable, '-c', 'import signal; signal.pause()'])\n"
                           "def stop(*args):\n"
                           "    child.wait()\n"
                           "    sys.exit(0)\n"
                           "signal.signal(signal.SIGTERM, stop)\n"
                           "Path(os.environ['BROWSER_PIDS']).write_text(json.dumps([os.getpid(), child.pid]))\n"
                           "while True: signal.pause()\n")
        browser.chmod(0o755)
        helper = (SCRIPT.parents[1] / "desktop/scripts/smoke-lib.mjs").as_uri()
        source = (f"import {{ launchChrome }} from {json.dumps(helper)}; "
                  "process.on('SIGTERM', () => {}); "
                  "try { await launchChrome(process.env.BROWSER, { launchTimeout: 30000 }); } catch {} "
                  "setInterval(() => {}, 1000);")
        env = {**os.environ, "BROWSER": str(browser), "BROWSER_PIDS": str(pid_file),
               "DESKTOP_LANE_NPM_TEST_TIMEOUT_SECONDS": "2"}
        try:
            with contextlib.redirect_stdout(io.StringIO()), self.assertRaises(GATE.StepTimedOut):
                GATE.run_step([shutil.which("node"), "--input-type=module", "-e", source], self.root, env, "npm test")
            for pid in json.loads(pid_file.read_text()):
                with self.assertRaises(ProcessLookupError, msg=f"browser descendant {pid} survived"):
                    os.kill(pid, 0)
        finally:
            if pid_file.exists():
                for pid in json.loads(pid_file.read_text()):
                    try:
                        os.kill(pid, signal.SIGKILL)
                    except ProcessLookupError:
                        pass

    def test_windows_uses_bounded_tree_kill_and_direct_fallback(self):
        from unittest.mock import Mock
        for outcome in (None, OSError("taskkill missing"), subprocess.TimeoutExpired("taskkill", 2)):
            with self.subTest(outcome=outcome):
                process = Mock(pid=123)
                process.poll.return_value = None
                with patch.object(GATE.os, "name", "nt"), patch.object(GATE.subprocess, "run", side_effect=outcome) as run:
                    GATE.stop_step(process)
                self.assertEqual(run.call_args.args[0], ["taskkill", "/PID", "123", "/T", "/F"])
                self.assertEqual(run.call_args.kwargs["timeout"], GATE.TERMINATE_GRACE_SECONDS)
                process.kill.assert_called_once()
                process.wait.assert_called_once_with(timeout=GATE.TERMINATE_GRACE_SECONDS)


if __name__ == "__main__":
    unittest.main()
