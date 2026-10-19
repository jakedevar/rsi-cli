"""Intent checks for the shard harness-build warning counter (#2671)."""

import importlib.util
import json
from pathlib import Path
import unittest

SCRIPT = Path(__file__).resolve().parents[1] / "shard-build-warnings.py"
SPEC = importlib.util.spec_from_file_location("shard_build_warnings", SCRIPT)
assert SPEC and SPEC.loader
counter = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(counter)


def message(level: str, code: str | None, text: str, target: str = "rsid_store", file: str = "src/a.rs") -> str:
    return json.dumps(
        {
            "reason": "compiler-message",
            "target": {"name": target},
            "message": {
                "level": level,
                "code": {"code": code} if code else None,
                "message": text,
                "spans": [{"is_primary": True, "file_name": file}],
            },
        }
    )


class ClassifyTests(unittest.TestCase):
    def test_allowed_lints_are_counted_but_never_reported_as_new(self) -> None:
        out = "\n".join(
            [
                message("warning", "dead_code", "function `f` is never used"),
                message("warning", "unused_imports", "unused import: `g`"),
            ]
        )
        counts, keys, errors = counter.classify(out, "rsid_store")
        self.assertEqual({"dead_code": 1, "unused_imports": 1}, dict(counts))
        self.assertEqual(set(), keys)
        self.assertEqual(0, errors)

    def test_a_shard_only_warning_of_another_lint_is_reported(self) -> None:
        out = message("warning", "unused_variables", "unused variable: `x`")
        counts, keys, errors = counter.classify(out, "rsid_store")
        self.assertEqual({"unused_variables": 1}, dict(counts))
        self.assertEqual({"src/a.rs|unused_variables|unused variable: `x`"}, keys)
        self.assertEqual(0, errors)

    def test_errors_count_and_other_targets_and_noise_are_ignored(self) -> None:
        out = "\n".join(
            [
                message("error", "E0425", "cannot find value"),
                message("warning", "unused_mut", "variable does not need to be mutable", target="dep"),
                "not json",
                json.dumps({"reason": "build-finished", "success": False}),
                message("warning", "unused_mut", "dup"),
                message("warning", "unused_mut", "dup"),
            ]
        )
        counts, keys, errors = counter.classify(out, "rsid_store")
        self.assertEqual(1, errors)
        self.assertEqual({"unused_mut": 1}, dict(counts))
        self.assertEqual({"src/a.rs|unused_mut|dup"}, keys)


if __name__ == "__main__":
    unittest.main()
