#!/usr/bin/env python3
"""Guard: every AgentScheduleWake `mode:"when"` example in the guidance names `message`.

`AgentScheduleWake` refuses a call without `message` (invalid_input
`required "message"`), including `mode:"when"` (#119). A doc or shipped
guidance text that shows a `mode:"when"` request with a `when` predicate but no
`message` teaches workers a call the daemon refuses, so each such example block
must mention `message`.

Run: python3 -m unittest tools.test_wake_when_message_docs
(stdlib only).
"""
import re
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent

SCAN = [
    "AGENTS.md",
    "docs",
    ".claude",
    ".agents",
    ".codex",
    ".gemini",
    "crates/rsid/src/session/guidance",
]
SUFFIXES = {".md", ".toml"}

MODE_WHEN = re.compile(r"mode\W{0,4}when\b")
PAYLOAD = re.compile(
    r"when[\"`']?\s*:\s*\{|\bwhen\.(?:jobs_terminal|sha_on_rolling|issue_closed|todo_closed"
    r"|topology_execution_settled|settlement_run_settled|health_anomaly)"
)
WAKE_VERB = re.compile(r"AgentScheduleWake|schedule_wake")
MESSAGE = re.compile(r"\bmessage\b")
ITEM_START = re.compile(r"^\s*(?:[-*]|\d+\.)\s")


def blocks(text):
    """Yield (first_line_number, text) for paragraphs and list items."""
    cur, start = [], 0
    for n, line in enumerate(text.splitlines(), 1):
        if not line.strip() or ITEM_START.match(line):
            if cur:
                yield start, "\n".join(cur)
            cur, start = [], n
        if line.strip():
            if not cur:
                start = n
            cur.append(line)
    if cur:
        yield start, "\n".join(cur)


def violations(text):
    """Line numbers of `mode:"when"` example blocks with a predicate but no `message`."""
    found = []
    for start, block in blocks(text):
        if (
            WAKE_VERB.search(block)
            and MODE_WHEN.search(block)
            and PAYLOAD.search(block)
            and not MESSAGE.search(block)
        ):
            found.append(start)
    return found


def example_blocks(text):
    return [
        start
        for start, block in blocks(text)
        if WAKE_VERB.search(block) and MODE_WHEN.search(block) and PAYLOAD.search(block)
    ]


def scanned_files():
    for entry in SCAN:
        path = ROOT / entry
        if path.is_file():
            yield path
        elif path.is_dir():
            for p in sorted(path.rglob("*")):
                if p.is_file() and p.suffix in SUFFIXES:
                    yield p


class WakeWhenMessageDocs(unittest.TestCase):
    def test_detector_flags_missing_message(self):
        bad = 'Then ONE `AgentScheduleWake`\n`mode:"when"` (`when.jobs_terminal`), then end.\n'
        self.assertEqual(violations(bad), [1])
        bad2 = 'arm `AgentScheduleWake {mode:"when", when:{jobs_terminal:[ids]}}`.\n'
        self.assertEqual(violations(bad2), [1])

    def test_detector_accepts_message(self):
        good = (
            'ONE `AgentScheduleWake {"mode":"when","message":"done",'
            '"when":{"jobs_terminal":["<id>"]}}`.\n'
        )
        self.assertEqual(violations(good), [])

    def test_scan_is_not_vacuous(self):
        total = 0
        for path in scanned_files():
            total += len(example_blocks(path.read_text(encoding="utf-8")))
        self.assertGreaterEqual(total, 8, "expected the known mode:when examples to be scanned")

    def test_every_mode_when_example_names_message(self):
        problems = []
        for path in scanned_files():
            for line in violations(path.read_text(encoding="utf-8")):
                problems.append(f"{path.relative_to(ROOT)}:{line}")
        self.assertEqual(
            problems,
            [],
            "AgentScheduleWake mode:when examples must include `message` (#119)",
        )

    def test_shipped_preamble_text_requires_message(self):
        text = (ROOT / "crates/rsid/src/session/preamble.rs").read_text(encoding="utf-8")
        self.assertIn("`mode` and `message`", text)


if __name__ == "__main__":
    unittest.main()
