"""Exercise the installed pre-push hook with real local Git pushes."""

import os
from pathlib import Path
import subprocess
import tempfile
import unittest


HOOKS = Path(__file__).resolve().parent / "git-hooks"
AGENT_MARKERS = ("RSI_SESSION_ID", "RSI_SESSION_TOKEN", "CLAUDE_AGENT_ROLE")


def git(
    cwd: Path,
    *args: str,
    agent_marker: str | None = None,
    lander_marker: bool = False,
) -> subprocess.CompletedProcess[str]:
    env = os.environ.copy()
    for marker in AGENT_MARKERS:
        env.pop(marker, None)
    if agent_marker:
        env[agent_marker] = "test-session"
    if lander_marker:
        env["RSI_ROLLING_LANDER"] = "1"
    return subprocess.run(
        ["git", *args], cwd=cwd, env=env, text=True, capture_output=True, check=False
    )


class PrePushTest(unittest.TestCase):
    def setUp(self) -> None:
        self.temp = tempfile.TemporaryDirectory(prefix="rsi-pre-push-")
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.repo = self.root / "source"
        self.remote = self.root / "remote.git"
        self.repo.mkdir()
        self.remote.mkdir()
        self.assert_git(self.repo, "init", "-q", "-b", "scratch")
        self.assert_git(self.remote, "init", "-q", "--bare")
        self.assert_git(self.repo, "config", "user.name", "Test")
        self.assert_git(self.repo, "config", "user.email", "test@example.invalid")
        (self.repo / "file.txt").write_text("fixture\n")
        self.assert_git(self.repo, "add", "file.txt")
        self.assert_git(self.repo, "commit", "-q", "-m", "fixture")
        self.assert_git(self.repo, "config", "core.hooksPath", str(HOOKS))
        self.assert_git(self.repo, "remote", "add", "publish", str(self.remote))

    def assert_git(self, cwd: Path, *args: str) -> None:
        result = git(cwd, *args)
        self.assertEqual(result.returncode, 0, result.stderr)

    def remote_ref(self, ref: str) -> str:
        return git(self.remote, "rev-parse", "--verify", ref).stdout.strip()

    def test_agent_protected_refs_rejected_without_remote_update(self) -> None:
        for marker, ref in (
            ("RSI_SESSION_ID", "rolling"),
            ("RSI_SESSION_TOKEN", "main"),
        ):
            with self.subTest(marker=marker, ref=ref):
                result = git(
                    self.repo,
                    "push",
                    "publish",
                    f"HEAD:refs/heads/{ref}",
                    agent_marker=marker,
                )
                self.assertNotEqual(result.returncode, 0)
                self.assertIn(f"refs/heads/{ref}", result.stderr)
                self.assertIn("land with rsi-rolling-land after an ACCEPTED review", result.stderr)
                self.assertEqual(self.remote_ref(f"refs/heads/{ref}"), "")

    def test_lander_marker_allows_rolling_but_never_main(self) -> None:
        rolling = git(
            self.repo,
            "push",
            "publish",
            "HEAD:refs/heads/rolling",
            agent_marker="RSI_SESSION_ID",
            lander_marker=True,
        )
        self.assertEqual(rolling.returncode, 0, rolling.stderr)
        source = git(self.repo, "rev-parse", "HEAD").stdout.strip()
        self.assertEqual(self.remote_ref("refs/heads/rolling"), source)

        main = git(
            self.repo,
            "push",
            "publish",
            "HEAD:refs/heads/main",
            agent_marker="RSI_SESSION_ID",
            lander_marker=True,
        )
        self.assertNotEqual(main.returncode, 0)
        self.assertEqual(self.remote_ref("refs/heads/main"), "")

    def test_agent_unprotected_ref_and_operator_protected_ref_succeed(self) -> None:
        agent = git(
            self.repo,
            "push",
            "publish",
            "HEAD:refs/heads/feature",
            agent_marker="RSI_SESSION_ID",
        )
        self.assertEqual(agent.returncode, 0, agent.stderr)
        for ref in ("main", "rolling"):
            operator = git(self.repo, "push", "publish", f"HEAD:refs/heads/{ref}")
            self.assertEqual(operator.returncode, 0, operator.stderr)
        source = git(self.repo, "rev-parse", "HEAD").stdout.strip()
        self.assertEqual(self.remote_ref("refs/heads/feature"), source)
        self.assertEqual(self.remote_ref("refs/heads/main"), source)
        self.assertEqual(self.remote_ref("refs/heads/rolling"), source)

    def test_role_only_environment_without_rsi_session_can_push(self) -> None:
        result = git(
            self.repo,
            "push",
            "publish",
            "HEAD:refs/heads/rolling",
            agent_marker="CLAUDE_AGENT_ROLE",
        )
        self.assertEqual(result.returncode, 0, result.stderr)

    def test_agent_cannot_delete_protected_ref_or_push_it_with_other_refs(self) -> None:
        self.assert_git(self.repo, "push", "publish", "HEAD:refs/heads/rolling")
        source = self.remote_ref("refs/heads/rolling")
        deletion = git(
            self.repo,
            "push",
            "publish",
            ":refs/heads/rolling",
            agent_marker="RSI_SESSION_ID",
        )
        self.assertNotEqual(deletion.returncode, 0)
        self.assertEqual(self.remote_ref("refs/heads/rolling"), source)
        mixed = git(
            self.repo,
            "push",
            "publish",
            "HEAD:refs/heads/feature",
            "HEAD:refs/heads/main",
            agent_marker="RSI_SESSION_ID",
        )
        self.assertNotEqual(mixed.returncode, 0)
        self.assertEqual(self.remote_ref("refs/heads/main"), "")
        self.assertEqual(self.remote_ref("refs/heads/feature"), "")

    def test_private_landing_clone_lander_marker_can_publish_with_agent_environment(self) -> None:
        private = self.root / "landing-clone"
        self.assert_git(
            self.root, "clone", "--shared", "--no-checkout", "-q", str(self.repo), str(private)
        )
        self.assert_git(private, "remote", "add", "publish", str(self.remote))
        self.assertNotEqual(git(private, "config", "--local", "--get", "core.hooksPath").returncode, 0)
        result = git(
            private,
            "push",
            "publish",
            "HEAD:refs/heads/rolling",
            agent_marker="RSI_SESSION_ID",
            lander_marker=True,
        )
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(self.remote_ref("refs/heads/rolling"), git(private, "rev-parse", "HEAD").stdout.strip())


    def commit_shard_checker(self, bad: bool) -> str:
        """Commit a stand-in shard checker that fails iff the exported tree holds
        crates/rsid/BAD, so the check is proven to run on the pushed commit."""
        script = self.repo / "scripts" / "check-rsid-test-shards.py"
        script.parent.mkdir(exist_ok=True)
        script.write_text(
            "import sys\n"
            "from pathlib import Path\n"
            "root = Path(__file__).resolve().parent.parent\n"
            "if (root / 'crates/rsid/BAD').exists():\n"
            "    print('strict inventory red'); sys.exit(1)\n"
            "print('ok')\n"
        )
        crate = self.repo / "crates" / "rsid"
        crate.mkdir(parents=True, exist_ok=True)
        (crate / "Cargo.toml").write_text("[package]\n")
        bad_marker = crate / "BAD"
        if bad:
            bad_marker.write_text("x\n")
        elif bad_marker.exists():
            bad_marker.unlink()
        self.assert_git(self.repo, "add", "-A", "scripts", "crates")
        self.assert_git(self.repo, "commit", "-q", "-m", "shard checker fixture")
        return git(self.repo, "rev-parse", "HEAD").stdout.strip()

    def test_rolling_push_of_a_strict_inventory_failure_is_refused(self) -> None:
        self.commit_shard_checker(bad=True)
        for kwargs in ({}, {"agent_marker": "RSI_SESSION_ID", "lander_marker": True}):
            with self.subTest(kwargs=kwargs):
                result = git(self.repo, "push", "publish", "HEAD:refs/heads/rolling", **kwargs)
                self.assertNotEqual(result.returncode, 0)
                self.assertIn("strict rsid test shard inventory", result.stderr)
                self.assertEqual(self.remote_ref("refs/heads/rolling"), "")

    def test_rolling_push_checks_the_pushed_commit_not_the_working_tree(self) -> None:
        good = self.commit_shard_checker(bad=False)
        (self.repo / "crates" / "rsid" / "BAD").write_text("uncommitted\n")
        result = git(self.repo, "push", "publish", "HEAD:refs/heads/rolling")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(self.remote_ref("refs/heads/rolling"), good)

    def test_strict_inventory_failure_does_not_block_other_refs(self) -> None:
        bad = self.commit_shard_checker(bad=True)
        result = git(
            self.repo,
            "push",
            "publish",
            "HEAD:refs/heads/feature",
            agent_marker="RSI_SESSION_ID",
        )
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(self.remote_ref("refs/heads/feature"), bad)

    def test_rolling_push_adding_an_operator_identifier_is_refused(self) -> None:
        import shutil
        from unittest import mock

        tool = self.repo / "tools" / "check_operator_identifiers.py"
        tool.parent.mkdir(exist_ok=True)
        shutil.copy(HOOKS.parent / "check_operator_identifiers.py", tool)
        self.assert_git(self.repo, "add", "tools")
        self.assert_git(self.repo, "commit", "-q", "-m", "add guard")
        self.assert_git(self.repo, "push", "publish", "HEAD:refs/heads/rolling")
        base = self.remote_ref("refs/heads/rolling")
        ids = self.root / "operator-identifiers"
        ids.write_text("jane.doe" + "@example.invalid\n")
        (self.repo / "note.md").write_text("contact jane.doe" + "@example.invalid\n")
        self.assert_git(self.repo, "add", "note.md")
        self.assert_git(self.repo, "commit", "-q", "-m", "leak")
        with mock.patch.dict(os.environ, {"RSI_OPERATOR_IDENTIFIERS_FILE": str(ids)}):
            refused = git(self.repo, "push", "publish", "HEAD:refs/heads/rolling")
            other = git(self.repo, "push", "publish", "HEAD:refs/heads/feature")
        self.assertNotEqual(refused.returncode, 0)
        self.assertIn("note.md:1: operator identifier #1", refused.stderr)
        self.assertNotIn("jane.doe" + "@example.invalid", refused.stderr)
        self.assertEqual(self.remote_ref("refs/heads/rolling"), base)
        self.assertEqual(other.returncode, 0, other.stderr)
        # Without the list the same push goes through.
        ok = git(self.repo, "push", "publish", "HEAD:refs/heads/rolling")
        self.assertEqual(ok.returncode, 0, ok.stderr)


if __name__ == "__main__":
    unittest.main()
