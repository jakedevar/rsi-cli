"""#938: a release build never reuses another worktree's workspace crates.

Two git worktrees of one throwaway repository build into one shared
CARGO_TARGET_DIR, as a global `build.target-dir` makes them. The fake `cargo`
on PATH models the hazard: a workspace crate artifact that another worktree
compiled is reused (Cargo's mtime freshness check passes), and the fake rsid
embeds the commit of the worktree whose sources built it, like
crates/rsid/build.rs. No real build, daemon or user file is touched (HOME is a
temp directory and every install runs with --no-restart).
"""

import fcntl
import json
import os
import shutil
import subprocess
import sys
import time
import unittest
from pathlib import Path

SCRIPTS = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(SCRIPTS))
from scratch_git import scratch_git, scratch_tempdir  # noqa: E402

MEMBERS = ["stub", "stub-tools"]

FAKE_CARGO = r'''
import json, os, shutil, subprocess, sys
from pathlib import Path

args = sys.argv[1:]
target = Path(os.environ["CARGO_TARGET_DIR"])
release = target / "release"
with open(os.environ["FAKE_CARGO_LOG"], "a") as log:
    log.write(json.dumps({"args": args}) + "\n")
command = args[0] if args else ""
root = Path(args[args.index("--manifest-path") + 1]).parent if "--manifest-path" in args else Path.cwd()
crate = release / ".fingerprint" / "stub-0123456789abcdef"
dependency = release / ".fingerprint" / "serde-fedcba9876543210"
BINS = ["rsi", "rsid", "rsi-rpc", "rsi-agent-mcp", "rsi-build-rustc", "rsi-contract-validate",
        "rsi-rolling-land", "rsi-remote", "rsi-turn-shim", "rsi-socket-hold"]

if command == "metadata":
    print(json.dumps({"target_directory": str(target), "packages": [{"name": n} for n in json.loads(os.environ["FAKE_MEMBERS"])]}))
elif command == "tree":
    print("stub v0.0.0 ")
elif command == "clean":
    if not os.environ.get("FAKE_CLEAN_IGNORED"):
        # `cargo clean --release -p <member>`: the members' artifacts and
        # binaries go; third-party dependencies stay.
        shutil.rmtree(crate, ignore_errors=True)
        for name in BINS:
            (release / name).unlink(missing_ok=True)
elif command == "build":
    if os.environ.get("FAKE_BUILD_FAILS"):
        sys.exit(101)
    if os.environ.get("FAKE_TOUCH_DURING_BUILD"):
        with open(os.environ["FAKE_TOUCH_DURING_BUILD"], "a") as edited:
            edited.write("// edited while cargo ran\n")
    wanted = [args[i + 1] for i, arg in enumerate(args) if arg == "--bin"]
    head = subprocess.run(["git", "-C", str(root), "rev-parse", "HEAD"],
                          capture_output=True, text=True).stdout.strip()
    built_from = {"root": str(root), "sha": head}
    if crate.is_dir():
        previous = json.loads((crate / "source").read_text())
        if previous["root"] != str(root):
            # Another worktree's newer artifact looks fresh: Cargo reuses it.
            built_from = previous
    crate.mkdir(parents=True, exist_ok=True)
    dependency.mkdir(parents=True, exist_ok=True)
    (crate / "source").write_text(json.dumps(built_from))
    for name in wanted:
        path = release / name
        path.unlink(missing_ok=True)
        if name == "rsid":
            body = 'if [ "$1" = --build-info ]; then echo "%s 999"; else echo "fake-rsid from %s"; fi\n' % (
                built_from["sha"], built_from["root"])
        else:
            body = 'echo "fake-%s from %s"\n' % (name, built_from["root"])
        path.write_text("#!/bin/sh\n" + body)
        path.chmod(0o755)
'''


class InstallReleaseProvenanceTest(unittest.TestCase):
    def setUp(self):
        if shutil.which("git") is None:
            self.skipTest("git is required")
        self.temp = scratch_tempdir(prefix="rsi-install-provenance-")
        self.addCleanup(self.temp.cleanup)
        self.base = Path(self.temp.name).resolve()
        self.home = self.base / "home"
        self.home.mkdir()
        self.target = self.base / "shared-target"
        self.log = self.base / "cargo.log"
        fakes = self.base / "fakes"
        fakes.mkdir()
        cargo = fakes / "cargo"
        cargo.write_text(f"#!{sys.executable}\n" + FAKE_CARGO)
        cargo.chmod(0o755)
        rustup = fakes / "rustup"
        rustup.write_text("#!/bin/sh\nexit 99\n")
        rustup.chmod(0o755)
        self.env = {
            key: value for key, value in os.environ.items()
            if not key.startswith(("GIT_", "RSI_", "CARGO_")) and key not in ("NOW",)
        }
        self.env.update(
            HOME=str(self.home),
            PATH=f"{fakes}:{os.environ['PATH']}",
            CARGO_TARGET_DIR=str(self.target),
            FAKE_CARGO_LOG=str(self.log),
            FAKE_MEMBERS=json.dumps(MEMBERS),
            GIT_AUTHOR_NAME="t", GIT_AUTHOR_EMAIL="t@example.invalid",
            GIT_COMMITTER_NAME="t", GIT_COMMITTER_EMAIL="t@example.invalid",
            GIT_CONFIG_NOSYSTEM="1",
        )
        # Worktree A: the repository; worktree B: a linked worktree one commit ahead.
        self.a = self.base / "a"
        shutil.copytree(SCRIPTS, self.a / "scripts",
                        ignore=shutil.ignore_patterns("tests", "__pycache__"))
        (self.a / "Cargo.toml").write_text('[workspace]\nmembers = []\n')
        (self.a / ".gitignore").write_text("target/\n")
        (self.a / "src").mkdir()
        (self.a / "src" / "lib.rs").write_text("pub fn built() {}\n")
        self.git(self.a, "init", "-q", "-b", "main")
        self.git(self.a, "add", "-A")
        self.git(self.a, "commit", "-q", "-m", "base")
        self.b = self.base / "b"
        self.git(self.a, "worktree", "add", "-q", "-b", "other", str(self.b))
        (self.b / "change.txt").write_text("b\n")
        self.git(self.b, "add", "change.txt")
        self.git(self.b, "commit", "-q", "-m", "b ahead")

    def git(self, cwd, *args):
        return scratch_git(
            cwd, "-c", "commit.gpgsign=false", "-c", "core.hooksPath=/dev/null", *args,
            env=self.env,
        ).stdout.strip()

    def head(self, worktree):
        return self.git(worktree, "rev-parse", "HEAD")

    def install(self, worktree, *flags, check=True, **extra_env):
        result = subprocess.run(
            ["bash", str(worktree / "scripts" / "install-release.sh"), "--no-restart", *flags],
            env=dict(self.env, **extra_env), cwd=self.base,
            text=True, capture_output=True, timeout=60,
        )
        if check:
            self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        return result

    def cargo_calls(self, command):
        if not self.log.exists():
            return []
        calls = [json.loads(line)["args"] for line in self.log.read_text().splitlines()]
        return [call for call in calls if call and call[0] == command]

    def marker(self):
        path = self.target / "release" / ".rsi-release-source"
        return dict(line.split("=", 1) for line in path.read_text().splitlines())

    def installed_rsid_build(self):
        rsid = self.home / ".rsi" / "install" / "rsid"
        return subprocess.run([str(rsid), "--build-info"], capture_output=True, text=True,
                              check=True).stdout.split()[0]

    def test_building_from_another_worktree_cleans_only_the_workspace_crates(self):
        self.install(self.a)
        self.assertEqual(self.cargo_calls("clean"), [])
        self.assertEqual(self.installed_rsid_build(), self.head(self.a))

        self.log.unlink()
        result = self.install(self.b)
        self.assertIn(f"was last built from {self.a}", result.stderr)
        # The guard cleans before the one build; the embedded-commit backstop
        # never has to rebuild.
        commands = [json.loads(line)["args"][0] for line in self.log.read_text().splitlines()]
        self.assertEqual([c for c in commands if c in ("clean", "build")], ["clean", "build"])
        cleans = self.cargo_calls("clean")
        self.assertIn("--release", cleans[0])
        self.assertEqual([cleans[0][i + 1] for i, arg in enumerate(cleans[0]) if arg == "-p"], MEMBERS)
        # Third-party artifacts stay shared; B's binaries carry B's commit.
        self.assertTrue((self.target / "release" / ".fingerprint" / "serde-fedcba9876543210").is_dir())
        self.assertEqual(self.installed_rsid_build(), self.head(self.b))
        marker = self.marker()
        self.assertEqual(marker["worktree"], str(self.b))
        self.assertEqual(marker["state"], "built")
        self.assertEqual(marker["head"], self.head(self.b))
        self.assertEqual(marker["dirty"], "false")

    def test_rebuilding_from_the_same_worktree_keeps_its_artifacts(self):
        self.install(self.a)
        (self.a / "next.txt").write_text("a2\n")
        self.git(self.a, "add", "next.txt")
        self.git(self.a, "commit", "-q", "-m", "a moves")
        self.install(self.a)
        self.assertEqual(self.cargo_calls("clean"), [])
        self.assertEqual(self.installed_rsid_build(), self.head(self.a))

    def test_a_target_with_no_builder_record_is_cleaned_before_the_build(self):
        (self.target / "release" / ".fingerprint" / "stub-0123456789abcdef").mkdir(parents=True)
        (self.target / "release" / ".fingerprint" / "stub-0123456789abcdef" / "source").write_text(
            json.dumps({"root": str(self.base / "elsewhere"), "sha": "0" * 40}))
        result = self.install(self.a)
        self.assertIn("no record of the worktree that built it", result.stderr)
        self.assertEqual(len(self.cargo_calls("clean")), 1)
        self.assertEqual(self.installed_rsid_build(), self.head(self.a))

    def test_tui_only_build_from_another_worktree_also_cleans(self):
        self.install(self.a)
        self.install(self.b, "--tui-only")
        self.assertEqual(len(self.cargo_calls("clean")), 1)
        self.assertEqual(self.marker()["worktree"], str(self.b))
        rsi = (self.target / "release" / "rsi").read_text()
        self.assertIn(f"from {self.b}", rsi)

    def test_an_out_of_band_build_is_caught_by_the_embedded_commit_and_rebuilt(self):
        self.install(self.b)
        # Someone builds worktree A into the shared target by hand: the record
        # still names B, but the workspace crates are A's.
        shutil.rmtree(self.target / "release" / ".fingerprint" / "stub-0123456789abcdef")
        subprocess.run(["cargo", "build", "--release", "--manifest-path", str(self.a / "Cargo.toml"),
                        "--bin", "rsid"], env=self.env, check=True)
        result = self.install(self.b)
        self.assertIn(f"is at {self.head(self.b)}", result.stderr)
        self.assertIn("Rebuilding this workspace's release crates", result.stderr)
        self.assertEqual(len(self.cargo_calls("clean")), 1)
        self.assertEqual(self.installed_rsid_build(), self.head(self.b))

    def test_a_binary_that_stays_foreign_is_never_installed(self):
        self.install(self.b)
        installed_before = self.installed_rsid_build()
        shutil.rmtree(self.target / "release" / ".fingerprint" / "stub-0123456789abcdef")
        subprocess.run(["cargo", "build", "--release", "--manifest-path", str(self.a / "Cargo.toml"),
                        "--bin", "rsid"], env=self.env, check=True)
        result = self.install(self.b, check=False, FAKE_CLEAN_IGNORED="1")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("Refusing to install", result.stderr)
        self.assertEqual(self.installed_rsid_build(), installed_before)
        self.assertEqual(self.marker()["state"], "building")

    def test_link_only_links_this_worktrees_build(self):
        self.install(self.a)
        result = self.install(self.a, "--link-only")
        self.assertIn("Linked:", result.stdout)
        self.assertEqual(self.installed_rsid_build(), self.head(self.a))

    def test_link_only_from_another_worktree_refuses_unless_allowed(self):
        self.install(self.a)
        refused = self.install(self.b, "--link-only", check=False)
        self.assertNotEqual(refused.returncode, 0)
        self.assertIn(f"were built from {self.a}, not {self.b}", refused.stderr)
        self.assertIn("--allow-foreign-build", refused.stderr)

        allowed = self.install(self.b, "--link-only", "--allow-foreign-build")
        self.assertIn("--allow-foreign-build", allowed.stderr)
        self.assertEqual(self.installed_rsid_build(), self.head(self.a))

    def test_link_only_after_head_moved_refuses(self):
        self.install(self.a)
        built = self.head(self.a)
        (self.a / "next.txt").write_text("a2\n")
        self.git(self.a, "add", "next.txt")
        self.git(self.a, "commit", "-q", "-m", "a moves")
        refused = self.install(self.a, "--link-only", check=False)
        self.assertNotEqual(refused.returncode, 0)
        self.assertIn(f"built at {built}, but {self.a} is now at {self.head(self.a)}", refused.stderr)

    def test_link_only_without_a_builder_record_refuses(self):
        self.install(self.a)
        (self.target / "release" / ".rsi-release-source").unlink()
        refused = self.install(self.a, "--link-only", check=False)
        self.assertNotEqual(refused.returncode, 0)
        self.assertIn("No release build from", refused.stderr)

    # --- #2179: --link-only compares the sources, not just HEAD ---------------

    def test_link_only_refuses_source_edits_that_leave_head_unchanged(self):
        self.install(self.a)
        source = self.a / "src" / "lib.rs"
        source.write_text("pub fn edited_after_the_build() {}\n")
        refused = self.install(self.a, "--link-only", check=False)
        self.assertNotEqual(refused.returncode, 0)
        self.assertIn(f"The sources in {self.a} changed since the release build", refused.stderr)
        # The fingerprint is the content: restoring the file makes it linkable.
        self.git(self.a, "checkout", "--", "src/lib.rs")
        self.assertIn("Linked:", self.install(self.a, "--link-only").stdout)

    def test_a_dirty_build_links_until_its_sources_change(self):
        (self.a / "src" / "lib.rs").write_text("pub fn uncommitted() {}\n")
        self.install(self.a)
        self.assertEqual(self.marker()["dirty"], "true")
        self.assertIn("Linked:", self.install(self.a, "--link-only").stdout)
        # A new, untracked source file is a source change too.
        (self.a / "src" / "new_module.rs").write_text("pub fn added() {}\n")
        refused = self.install(self.a, "--link-only", check=False)
        self.assertNotEqual(refused.returncode, 0)
        self.assertIn("uncommitted edits or new files", refused.stderr)

    def test_sources_changed_during_the_build_are_never_linked(self):
        result = self.install(self.a, FAKE_TOUCH_DURING_BUILD=str(self.a / "src" / "lib.rs"))
        self.assertIn("changed while the release build ran", result.stderr)
        self.assertEqual(self.marker()["source_tree"], "changed-during-build")
        refused = self.install(self.a, "--link-only", check=False)
        self.assertNotEqual(refused.returncode, 0)
        self.assertIn("changed while the last release build ran", refused.stderr)

    # --- #2180: no cleanup leaves an installed link dangling -------------------

    def test_tui_only_links_a_stable_copy_that_survives_another_worktrees_clean(self):
        self.install(self.a, "--tui-only")
        link = self.home / ".local" / "bin" / "rsi"
        self.assertEqual(link.resolve(), (self.home / ".rsi" / "install" / "rsi").resolve())
        self.assertEqual((self.a / "target" / "release" / "rsi").resolve(), link.resolve())
        # Worktree B cleans the shared target, then its build fails.
        failed = self.install(self.b, check=False, FAKE_BUILD_FAILS="1")
        self.assertNotEqual(failed.returncode, 0)
        self.assertEqual(len(self.cargo_calls("clean")), 1)
        self.assertEqual(subprocess.run([str(link)], capture_output=True, text=True, check=True).stdout,
                         f"fake-rsi from {self.a}\n")

    def test_a_cleanup_keeps_every_installed_link_into_the_target(self):
        self.install(self.a)
        release = self.target / "release"
        bin_dir = self.home / ".local" / "bin"
        # Links an older --tui-only install made: into Cargo's output directly.
        (bin_dir / "rsi").unlink()
        (bin_dir / "rsi").symlink_to(release / "rsi")
        compat = self.a / "target" / "release" / "rsi"
        compat.unlink()
        compat.symlink_to(release / "rsi")
        expected = (release / "rsi").read_bytes()
        failed = self.install(self.b, check=False, FAKE_BUILD_FAILS="1")
        self.assertNotEqual(failed.returncode, 0)
        self.assertFalse((release / "rsi").exists())  # the clean ran
        kept = self.home / ".rsi" / "install-kept"
        for link in (bin_dir / "rsi", compat):
            self.assertEqual(link.resolve().parent, kept.resolve(), link)
            self.assertEqual(link.resolve().read_bytes(), expected, link)
            self.assertIn(f"Kept installed link {link}", failed.stderr)

    # --- #2188: kept copies live exactly as long as a link resolves to them ----

    def kept_copies(self):
        keep = self.home / ".rsi" / "install-kept"
        return sorted(p.name for p in keep.iterdir() if not p.name.startswith(".")) if keep.is_dir() else []

    def link_into_release(self, link):
        link.unlink(missing_ok=True)
        link.parent.mkdir(parents=True, exist_ok=True)
        link.symlink_to(self.target / "release" / "rsi")

    def test_an_install_prunes_kept_copies_its_links_no_longer_use(self):
        self.install(self.a)
        bin_rsi = self.home / ".local" / "bin" / "rsi"
        self.link_into_release(bin_rsi)
        self.install(self.b, check=False, FAKE_BUILD_FAILS="1")  # cleans, keeps one copy
        self.assertEqual(len(self.kept_copies()), 1)
        result = self.install(self.a)  # relinks at ~/.rsi/install
        self.assertEqual(self.kept_copies(), [])
        self.assertIn("Pruned kept copy", result.stderr)
        self.assertEqual(bin_rsi.resolve(), (self.home / ".rsi" / "install" / "rsi").resolve())

    def test_repeated_distinct_binaries_keep_only_the_linked_copy(self):
        self.install(self.a)
        release = self.target / "release"
        bin_rsi = self.home / ".local" / "bin" / "rsi"
        for generation in range(1, 4):
            (release / "rsi").write_text(f"#!/bin/sh\necho generation-{generation}\n")
            (release / "rsi").chmod(0o755)
            self.link_into_release(bin_rsi)
            # Another worktree last built here, so this build cleans first.
            (release / ".rsi-release-source").write_text("worktree=/elsewhere\nstate=built\n")
            self.install(self.a, check=False, FAKE_BUILD_FAILS="1")
            self.assertEqual(len(self.kept_copies()), 1, (generation, self.kept_copies()))
            self.assertEqual(subprocess.run([str(bin_rsi)], capture_output=True, text=True,
                                            check=True).stdout, f"generation-{generation}\n")

    def test_a_copy_another_clones_link_resolves_to_is_never_pruned(self):
        clone = self.base / "clone"
        self.git(self.base, "clone", "-q", str(self.a), str(clone))
        self.install(self.a)
        expected = (self.target / "release" / "rsi").read_bytes()
        clone_link = clone / "target" / "release" / "rsi"
        self.link_into_release(clone_link)
        self.install(clone, check=False, FAKE_BUILD_FAILS="1")  # keeps the clone's link
        self.assertEqual(len(self.kept_copies()), 1)
        # Worktree A's runs never scan the clone; the record protects its copy.
        self.install(self.a)
        self.assertEqual(clone_link.resolve().read_bytes(), expected)
        self.assertEqual(len(self.kept_copies()), 1)
        # Once that link is gone, the next run prunes the copy.
        clone_link.unlink()
        self.install(self.a, "--link-only")
        self.assertEqual(self.kept_copies(), [])
        links = (self.home / ".rsi" / "install-kept" / ".links").read_text()
        self.assertEqual(links, "")

    def test_a_concurrent_release_build_into_the_same_target_waits_for_the_lock(self):
        self.target.mkdir(parents=True, exist_ok=True)
        with open(self.target / ".rsi-release.lock", "a") as lock:
            fcntl.flock(lock, fcntl.LOCK_EX)
            install = subprocess.Popen(
                ["bash", str(self.b / "scripts" / "install-release.sh"), "--no-restart"],
                env=self.env, cwd=self.base, text=True,
                stdout=subprocess.PIPE, stderr=subprocess.PIPE,
            )
            self.addCleanup(lambda: install.poll() is None and install.kill())
            time.sleep(1.5)
            self.assertIsNone(install.poll())
            self.assertEqual(self.cargo_calls("build"), [])
            fcntl.flock(lock, fcntl.LOCK_UN)
        stdout, stderr = install.communicate(timeout=60)
        self.assertEqual(install.returncode, 0, stdout + stderr)
        self.assertIn("Waiting for another release build into this target", stderr)
        self.assertEqual(len(self.cargo_calls("build")), 1)


if __name__ == "__main__":
    unittest.main()
