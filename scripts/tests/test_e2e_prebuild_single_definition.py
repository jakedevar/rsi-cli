"""The e2e prebuild binary list has exactly one definition (#1651, #1637).

scripts/e2e-prebuild-bins.txt names every binary the e2e_tui harness needs;
scoped-test, rsi-rolling-land and scripts/e2e.sh must read it instead of
spelling their own list, and it must carry rsi-agent-mcp.
"""

import re
import runpy
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
LIST = ROOT / "scripts" / "e2e-prebuild-bins.txt"
RUNNER = runpy.run_path(str(ROOT / "scripts" / "scoped-test"))
LANDER = (ROOT / "crates" / "rsid" / "src" / "bin" / "rsi-rolling-land.rs").read_text()


class E2ePrebuildSingleDefinition(unittest.TestCase):
    def pairs(self):
        return RUNNER["e2e_prebuild_bins"](LIST)

    def test_list_contains_the_daemon_siblings_the_harness_needs(self):
        binaries = [binary for _, binary in self.pairs()]
        for required in ("rsid", "rsi-agent-mcp", "rsi-rpc", "rsi-turn-shim"):
            self.assertIn(required, binaries)

    def test_every_listed_binary_exists_in_its_package(self):
        for package, binary in self.pairs():
            crate = ROOT / "crates" / package
            self.assertTrue(
                (crate / "src" / "bin" / f"{binary}.rs").is_file()
                or (crate / "src" / "main.rs").is_file() and binary == package,
                f"{package}:{binary} is not a binary of that package",
            )

    def test_scoped_test_command_builds_every_listed_binary(self):
        command = RUNNER["e2e_prebuild_command"](["cargo"])
        for package, binary in self.pairs():
            self.assertIn(package, command)
            self.assertIn(binary, command)

    def test_lander_reads_the_shared_list_and_has_no_literal_copy(self):
        self.assertIn('include_str!("../../../../scripts/e2e-prebuild-bins.txt")', LANDER)
        start = LANDER.index("fn e2e_prebuild_command")
        body = LANDER[start : LANDER.index("fn cargo_guard_command", start)]
        self.assertNotIn("rsi-agent-mcp", body)
        self.assertNotIn('"rsid"', body)

    def test_scoped_test_and_e2e_script_have_no_literal_binary_list(self):
        for path in (ROOT / "scripts" / "scoped-test", ROOT / "scripts" / "e2e.sh"):
            text = path.read_text()
            self.assertNotIn("rsi-agent-mcp", text, path.name)
            self.assertIn("e2e-prebuild-bins.txt", text, path.name)


if __name__ == "__main__":
    unittest.main()
