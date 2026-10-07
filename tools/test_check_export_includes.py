import os
import sys
import tempfile
import unittest
from pathlib import Path

sys.path.insert(0, os.path.dirname(__file__))
import check_export_includes as cei  # noqa: E402

SCRIPT = "EXCLUDES=(\n    thoughts\n    docs/design\n)\n"


def scan(files):
    with tempfile.TemporaryDirectory() as d:
        root = Path(d)
        (root / "scripts").mkdir()
        (root / "scripts" / "export-rsi-cli.sh").write_text(SCRIPT)
        for name, text in files.items():
            p = root / name
            p.parent.mkdir(parents=True, exist_ok=True)
            p.write_text(text)
        return cei.scan(d, list(files), cei.read_excludes(d))


class CheckExportIncludes(unittest.TestCase):
    def test_reads_excludes_from_export_script(self):
        with tempfile.TemporaryDirectory() as d:
            (Path(d) / "scripts").mkdir()
            (Path(d) / "scripts" / "export-rsi-cli.sh").write_text(SCRIPT)
            self.assertEqual(cei.read_excludes(d), ["thoughts", "docs/design"])

    def test_include_str_of_excluded_dir_is_flagged(self):
        out = scan({"crates/a/src/x.rs": 'let s = include_str!("../../../thoughts/n.md");\n'})
        self.assertEqual(len(out), 1)
        self.assertIn("thoughts/n.md", out[0])

    def test_path_attribute_and_include_bytes_are_flagged(self):
        out = scan(
            {
                "crates/a/src/x.rs": '#[path = "../../../docs/design/t.rs"]\nmod t;\n'
                'const B: &[u8] = include_bytes!("../../../thoughts/b.bin");\n'
            }
        )
        self.assertEqual(len(out), 2)

    def test_public_include_is_accepted(self):
        out = scan({"crates/a/src/x.rs": 'include_str!("../../../docs/keybindings.md");\n'})
        self.assertEqual(out, [])


if __name__ == "__main__":
    unittest.main()
