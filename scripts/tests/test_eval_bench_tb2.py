"""Run the pinned Terminal-Bench 2.0 suite (eval/bench/tb2/tests) under scoped-test (#3175)."""

import pathlib
import unittest

SUITE = pathlib.Path(__file__).resolve().parents[2] / "eval" / "bench" / "tb2" / "tests"


def load_tests(loader, tests, pattern):
    return loader.discover(str(SUITE), pattern="test_*.py")


if __name__ == "__main__":
    unittest.main()
