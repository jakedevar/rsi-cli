"""Exercise the installed pre-push hook with real local Git pushes."""

import os
from pathlib import Path
import subprocess
import tempfile
import unittest


HOOKS = Path(__file__).resolve().parent / "git-hooks"
AGENT_MARKERS = ("RSI_SESSION_ID", "RSI_SESSION_TOKEN", "CLAUDE_AGENT_ROLE")


def git(cwd: Path, *args: str, agent_marker: str | None = None) -> subprocess.CompletedProcess[str]:
    env = os.environ.copy()
    for marker in AGENT_MARKERS:
        env.pop(marker, None)
    if agent_marker:
        env[agent_marker] = "test-session"
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

    def test_private_landing_clone_can_publish_with_agent_environment(self) -> None:
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
        )
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(self.remote_ref("refs/heads/rolling"), git(private, "rev-parse", "HEAD").stdout.strip())


if __name__ == "__main__":
    unittest.main()
