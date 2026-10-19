"""Exercise the pre-commit research-artifact classifier (#2948).

Raw measurement JSON under thoughts/shared/research/ must not be run through
rsi-research-validate; real sidecars still must be, and a malformed one fails.
A stub validator (RSI_VALIDATE_BIN_DIR) makes the cases deterministic: it
records every path it is given and rejects a file containing MALFORMED.
"""

import os
from pathlib import Path
import subprocess
import tempfile
import unittest


HOOKS = Path(__file__).resolve().parent / "git-hooks"
STUB = """#!/bin/sh
echo "$1" >> "$VALIDATOR_LOG"
if grep -q MALFORMED "$1"; then
  echo "Parse: unknown field" >&2
  exit 2
fi
exit 0
"""
RAW = '{"source_revision": "abc", "cpu": {"user": 1.5}, "io": {"read_bytes": 10}}\n'
SIDECAR = '{"version": 2, "research_question": "q", "areas": ["a"]}\n'
BAD_SIDECAR = '{"version": 2, "research_question": "MALFORMED"}\n'


class ResearchSidecarClassifierTest(unittest.TestCase):
    def setUp(self) -> None:
        self.temp = tempfile.TemporaryDirectory(prefix="rsi-pre-commit-")
        self.addCleanup(self.temp.cleanup)
        root = Path(self.temp.name)
        self.repo = root / "repo"
        self.repo.mkdir()
        self.bin = root / "bin"
        self.bin.mkdir()
        stub = self.bin / "rsi-research-validate"
        stub.write_text(STUB)
        stub.chmod(0o755)
        self.log = root / "validator.log"
        self.log.write_text("")
        for args in (
            ("init", "-q", "-b", "main"),
            ("config", "user.name", "Test"),
            ("config", "user.email", "test@example.invalid"),
        ):
            self.git(*args)
        (self.repo / "seed.txt").write_text("seed\n")
        self.git("add", "seed.txt")
        self.assertEqual(self.git("commit", "-q", "-m", "seed").returncode, 0)
        (self.repo / "thoughts/shared/research").mkdir(parents=True)

    def git(self, *args: str, env: dict[str, str] | None = None):
        return subprocess.run(
            ["git", *args],
            cwd=self.repo,
            env=env or os.environ.copy(),
            text=True,
            capture_output=True,
            check=False,
        )

    def stage(self, name: str, body: str) -> str:
        path = f"thoughts/shared/research/{name}"
        (self.repo / path).write_text(body)
        self.assertEqual(self.git("add", path).returncode, 0)
        return path

    def pre_commit(self) -> subprocess.CompletedProcess[str]:
        env = os.environ.copy()
        env.pop("RSI_SKIP_STATIC_GUARDS", None)
        env["CLAUDE_AGENT_ROLE"] = "pipeline-research"
        env["RSI_VALIDATE_BIN_DIR"] = str(self.bin)
        env["VALIDATOR_LOG"] = str(self.log)
        return subprocess.run(
            ["sh", str(HOOKS / "pre-commit")],
            cwd=self.repo,
            env=env,
            text=True,
            capture_output=True,
            check=False,
        )

    def validated(self) -> list[str]:
        return self.log.read_text().split()

    def test_raw_evidence_json_is_not_validated_as_a_sidecar(self) -> None:
        for name in ("cpu-io.json", "run.evidence.json", "run.measurements.json"):
            self.stage(name, RAW)
        result = self.pre_commit()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(self.validated(), [])

    def test_exempt_suffix_wins_over_sibling_doc_and_marker(self) -> None:
        self.stage("run.md", "# run\n")
        self.stage("run.evidence.json", BAD_SIDECAR)
        result = self.pre_commit()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(self.validated(), [])

    def test_valid_sidecars_are_validated_by_every_marker(self) -> None:
        paths = [
            self.stage("a.research.json", SIDECAR),
            self.stage("b.json", SIDECAR),  # research_question marker
            self.stage("c.json", '{"version": 2}\n'),  # sibling doc below
        ]
        self.stage("c.md", "# c\n")
        result = self.pre_commit()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(sorted(self.validated()), sorted(paths))

    def test_malformed_sidecars_are_rejected(self) -> None:
        for name in ("x.research.json", "y.json"):
            self.stage(name, BAD_SIDECAR if name == "y.json" else '{"MALFORMED": 1}\n')
        result = self.pre_commit()
        self.assertEqual(result.returncode, 1)
        self.assertIn("research sidecar FAILED validation", result.stderr)
        self.assertEqual(len(self.validated()), 4)  # each run twice on failure

    def test_malformed_sidecar_is_rejected_beside_raw_evidence(self) -> None:
        self.stage("cpu-io.json", RAW)
        self.stage("bad.research.json", '{"MALFORMED": 1}\n')
        result = self.pre_commit()
        self.assertEqual(result.returncode, 1)
        self.assertIn("thoughts/shared/research/bad.research.json", result.stderr)
        self.assertNotIn("cpu-io.json", result.stderr)


if __name__ == "__main__":
    unittest.main()
