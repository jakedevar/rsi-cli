"""The full-library QA recipe checks every workspace package standalone (#2444)."""

import json
import os
from pathlib import Path
import subprocess
import tempfile
import unittest


SCRIPT = Path(__file__).resolve().parents[1] / "check-standalone-packages.sh"
ROOT = SCRIPT.parents[1]


class CheckStandalonePackagesTest(unittest.TestCase):
    def run_script(self, packages, failing=()):
        with tempfile.TemporaryDirectory(prefix="standalone-") as temp:
            log = Path(temp) / "log"
            metadata = json.dumps({"packages": [{"name": name} for name in packages]})
            cargo = Path(temp) / "cargo"
            cargo.write_text(
                "#!/bin/sh\n"
                f"if [ \"$1\" = metadata ]; then printf '%s' '{metadata}'; exit 0; fi\n"
                f"echo \"$*\" >> '{log}'\n"
                f"case \"$3\" in {'|'.join(failing) or 'NONE_'}) exit 101;; esac\n"
                "exit 0\n"
            )
            cargo.chmod(0o755)
            result = subprocess.run(
                [str(SCRIPT)], capture_output=True, text=True,
                env=dict(os.environ, CHECK_STANDALONE_CARGO=str(cargo)),
            )
            calls = log.read_text().splitlines() if log.exists() else []
            return result, calls

    def test_every_package_is_checked_alone_with_all_targets(self):
        result, calls = self.run_script(["beta", "alpha"])
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(calls, ["check -p alpha --all-targets", "check -p beta --all-targets"])

    def test_a_failure_is_named_and_does_not_stop_the_sweep(self):
        result, calls = self.run_script(["alpha", "beta", "gamma"], failing=("alpha",))
        self.assertEqual(result.returncode, 1)
        self.assertIn("standalone_check_failed:alpha", result.stderr)
        self.assertEqual(len(calls), 3)

    def test_an_empty_workspace_fails_closed(self):
        result, _ = self.run_script([])
        self.assertEqual(result.returncode, 2)

    def test_the_library_qa_recipe_runs_the_standalone_step(self):
        makefile = (ROOT / "Makefile").read_text()
        recipe = makefile.split("\nrolling-library-qa:\n", 1)[1].split("\n\n", 1)[0]
        self.assertIn("scripts/check-standalone-packages.sh", recipe)
        self.assertLess(recipe.index("nextest run"), recipe.index("check-standalone-packages"))
        self.assertIn("exit $$status", recipe)


if __name__ == "__main__":
    unittest.main()
