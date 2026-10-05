"""scripts/check-release-seam.sh refuses a release build that enables a test seam (#1021 S4)."""

import os
from pathlib import Path
import subprocess
import tempfile
import unittest


SCRIPT = Path(__file__).resolve().parents[1] / "check-release-seam.sh"
ROOT = SCRIPT.parents[1]


class CheckReleaseSeamTest(unittest.TestCase):
    def run_check(self, tree: str, exit_code: int = 0):
        with tempfile.TemporaryDirectory(prefix="release-seam-") as temp:
            cargo = Path(temp) / "cargo"
            cargo.write_text(f"#!/bin/sh\ncat <<'TREE'\n{tree}\nTREE\nexit {exit_code}\n")
            cargo.chmod(0o755)
            return subprocess.run(
                [str(SCRIPT)], capture_output=True, text=True,
                env=dict(os.environ, CHECK_RELEASE_SEAM_CARGO=str(cargo)),
            )

    def test_a_seam_free_graph_passes(self):
        result = self.run_check("rsid v0.1.0 (/x/crates/rsid)\nrsid-store v0.1.0 (/x/crates/rsid-store) \nrsid-core v0.1.0 (/x) \n")
        self.assertEqual(result.returncode, 0, result.stderr)

    def test_a_seam_on_a_normal_edge_is_refused_and_named(self):
        for features in ("test-seam", "foo,test-seam", "test-seam,bar"):
            result = self.run_check(f"rsid-store v0.1.0 (/x/crates/rsid-store) {features}\nrsid v0.1.0 (/x)\n")
            self.assertEqual(result.returncode, 1, features)
            self.assertIn("rsid-store", result.stderr)
            self.assertIn("test-seam is enabled in the release dependency graph", result.stderr)

    def test_a_similarly_named_feature_is_not_the_seam(self):
        result = self.run_check("rsid-store v0.1.0 (/x) test-seamless,not-test-seam\n")
        self.assertEqual(result.returncode, 0, result.stderr)

    def test_an_unreadable_graph_fails_closed(self):
        result = self.run_check("", exit_code=101)
        self.assertEqual(result.returncode, 2)

    def test_the_real_workspace_release_graph_has_no_seam(self):
        result = subprocess.run([str(SCRIPT)], capture_output=True, text=True, cwd=ROOT)
        self.assertEqual(result.returncode, 0, result.stderr)

    def test_every_release_build_path_runs_the_check(self):
        for path in ("scripts/install-release.sh", "scripts/build-aws-artifacts.sh", "Makefile"):
            self.assertIn("check-release-seam.sh", (ROOT / path).read_text(), path)


if __name__ == "__main__":
    unittest.main()
