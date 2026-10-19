"""Worker briefs name stdlib unittest for Python and never require pytest (#221)."""

from pathlib import Path
import re
import unittest

ROOT = Path(__file__).resolve().parents[2]
# Canonical Python-verification guidance: shared preamble, both worker
# contracts and the satellite delegation guide.
CANONICAL = [
    ".claude/commands/_shared/worker_preamble.md",
    "docs/agents/worker-contract.md",
    "thoughts/shared/manager/worker-contract.md",
    "docs/satellites.md",
]
# Rendered brief mirrors of the commands and skills workers are launched with.
RENDERED_DIRS = [".claude/commands", ".codex/prompts", ".agents/skills", ".gemini/commands"]
# A required pytest invocation. Historical handoffs and the optional adapter in
# python-test-gate.py are deliberately not scanned.
REQUIRED_PYTEST = re.compile(r"-m\s+pytest\b|(?<![\w/.-])pytest\s+(?:-\w+\s+)*(?:scripts|tools)/")


def briefs():
    for path in CANONICAL:
        yield ROOT / path
    for directory in RENDERED_DIRS:
        yield from sorted((ROOT / directory).rglob("*.md"))
        yield from sorted((ROOT / directory).rglob("*.toml"))


class PythonVerificationBriefs(unittest.TestCase):
    def test_canonical_briefs_name_stdlib_unittest(self):
        for path in CANONICAL:
            with self.subTest(path=path):
                self.assertIn("python3 -m unittest", (ROOT / path).read_text(encoding="utf-8"))

    def test_canonical_briefs_name_the_typed_missing_runner(self):
        for path in CANONICAL:
            with self.subTest(path=path):
                self.assertIn("runner_unavailable", (ROOT / path).read_text(encoding="utf-8"))

    def test_no_canonical_or_rendered_brief_requires_pytest(self):
        scanned = 0
        for path in briefs():
            scanned += 1
            for number, line in enumerate(path.read_text(encoding="utf-8").splitlines(), 1):
                with self.subTest(path=str(path.relative_to(ROOT)), line=number):
                    self.assertIsNone(REQUIRED_PYTEST.search(line), line)
        self.assertGreater(scanned, len(CANONICAL))

    def test_the_guard_pattern_catches_a_required_pytest_command(self):
        for line in ("python3 -m pytest scripts/tests/test_x.py", "run `pytest -q tools/tests/test_x.py`"):
            self.assertIsNotNone(REQUIRED_PYTEST.search(line), line)
        self.assertIsNone(REQUIRED_PYTEST.search("never require `pytest`; use `python3 -m unittest`"))


if __name__ == "__main__":
    unittest.main()
