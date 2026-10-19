"""Exercise the commit-msg hook with real local commits (#2597)."""

import os
from pathlib import Path
import subprocess
import tempfile
import unittest


HOOKS = Path(__file__).resolve().parent / "git-hooks"


class CommitMsgHookTest(unittest.TestCase):
    def setUp(self) -> None:
        self.temp = tempfile.TemporaryDirectory(prefix="rsi-commit-msg-")
        self.addCleanup(self.temp.cleanup)
        self.repo = Path(self.temp.name)
        self.git("init", "-q", "-b", "scratch")
        self.git("config", "user.name", "Test")
        self.git("config", "user.email", "test@example.invalid")
        self.git("config", "core.hooksPath", str(HOOKS))
        (self.repo / "f.txt").write_text("x\n")
        self.git("add", "f.txt")

    def git(self, *args: str) -> subprocess.CompletedProcess[str]:
        env = os.environ.copy()
        env.pop("RSI_SKIP_STATIC_GUARDS", None)
        env.pop("CLAUDE_AGENT_ROLE", None)
        result = subprocess.run(
            ["git", *args],
            cwd=self.repo,
            env=env,
            text=True,
            capture_output=True,
            check=False,
        )
        self.assertEqual(result.returncode, 0, result.stderr)
        return result

    def commit_message(self, message: str) -> tuple[str, str]:
        result = self.git("commit", "-q", "-m", message)
        body = self.git("log", "-1", "--format=%B").stdout
        return body, result.stderr

    def test_ai_coauthor_trailer_is_stripped(self) -> None:
        body, stderr = self.commit_message(
            "fix: thing\n\nDetails here.\n\n"
            "Co-Authored-By: Claude Sonnet 5.5 <noreply@anthropic.com>\n"
            "Co-authored-by: Codex <codex@openai.com>"
        )
        self.assertEqual(body.strip(), "fix: thing\n\nDetails here.")
        self.assertIn("removed AI attribution", stderr)

    def test_generated_with_banner_is_stripped(self) -> None:
        body, _ = self.commit_message(
            "docs: x\n\n🤖 Generated with [Claude Code](https://claude.com/claude-code)"
        )
        self.assertEqual(body.strip(), "docs: x")

    def test_clean_message_is_untouched(self) -> None:
        message = "feat: plain\n\nBody line.\n\nFixes #1"
        body, stderr = self.commit_message(message)
        self.assertEqual(body.strip(), message)
        self.assertNotIn("removed AI attribution", stderr)

    def test_human_coauthor_is_kept(self) -> None:
        message = "feat: pair\n\nCo-authored-by: Jane Doe <jane@example.invalid>"
        body, _ = self.commit_message(message)
        self.assertEqual(body.strip(), message)

    def test_human_named_like_an_ai_product_is_kept(self) -> None:
        # #2600: a name or address merely containing an AI term is not AI attribution.
        for trailer in (
            "Co-authored-by: Claude Dupont <claude.dupont@example.invalid>",
            "Co-authored-by: Claude <claude@example.invalid>",
            "Co-authored-by: Gemma Cursor <gemini.fan@example.invalid>",
            "Co-authored-by: Pat Devin <pat@gemini-labs.example.invalid>",
            "Co-authored-by: Sam Codex <sam@bedrock.example.invalid>",
        ):
            with self.subTest(trailer=trailer):
                self.git("commit", "-q", "--allow-empty", "-m", f"feat: pair\n\n{trailer}")
                body = self.git("log", "-1", "--format=%B").stdout
                self.assertEqual(body.strip(), f"feat: pair\n\n{trailer}")

    def test_known_ai_signatures_are_stripped(self) -> None:
        for trailer in (
            "Co-authored-by: Claude <noreply@anthropic.com>",
            "Co-authored-by: Claude Code <claude@example.invalid>",
            "Co-authored-by: Gemini <noreply@google.com>",
            "Co-authored-by: Cursor Agent <cursoragent@cursor.com>",
            "Co-authored-by: Copilot <175728472+Copilot@users.noreply.github.com>",
            "Co-authored-by: aider (gpt-5) <noreply@aider.chat>",
        ):
            with self.subTest(trailer=trailer):
                self.git("commit", "-q", "--allow-empty", "-m", f"feat: x\n\n{trailer}")
                body = self.git("log", "-1", "--format=%B").stdout
                self.assertEqual(body.strip(), "feat: x")

    def test_human_kept_while_ai_stripped_in_same_message(self) -> None:
        body, _ = self.commit_message(
            "feat: mix\n\nCo-authored-by: Claude Dupont <claude@example.invalid>\n"
            "Co-authored-by: Claude <noreply@anthropic.com>"
        )
        self.assertEqual(
            body.strip(),
            "feat: mix\n\nCo-authored-by: Claude Dupont <claude@example.invalid>",
        )


if __name__ == "__main__":
    unittest.main()
