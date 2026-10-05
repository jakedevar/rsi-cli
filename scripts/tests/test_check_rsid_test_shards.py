"""Intent checks for the source-derived rsid shard inventory."""

import importlib.util
import json
from pathlib import Path
import subprocess
import tempfile
import unittest
from unittest import mock


SCRIPT = Path(__file__).resolve().parents[1] / "check-rsid-test-shards.py"
SPEC = importlib.util.spec_from_file_location("check_rsid_test_shards", SCRIPT)
assert SPEC and SPEC.loader
checker = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(checker)


def source_test(name: str, shard: str = "store-01") -> str:
    return (
        f'#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-{shard}"))]\n'
        f"#[test]\nfn {name}() {{}}\n"
    )


class SourceInventoryTests(unittest.TestCase):
    def setUp(self) -> None:
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.source = self.root / "crates/rsid/src"
        self.source.mkdir(parents=True)
        (self.root / "crates/rsid/Cargo.toml").write_text(
            '[features]\ndefault = []\ntest-shard-mode = []\n'
            'test-shard-store-01 = ["test-shard-mode"]\n'
            'test-shard-store-02 = ["test-shard-mode"]\n'
        )
        self.root_patch = mock.patch.object(checker, "ROOT", self.root)
        self.crates_patch = mock.patch.object(
            checker, "CRATES", {"rsid": self.root / "crates/rsid"}
        )
        self.source_patch = mock.patch.object(checker, "SOURCES", [self.source])
        self.conditional_patch = mock.patch.object(checker, "CONDITIONAL_RUNTIME_NAMES", {})
        self.root_patch.start()
        self.crates_patch.start()
        self.source_patch.start()
        self.conditional_patch.start()
        self.addCleanup(self.root_patch.stop)
        self.addCleanup(self.crates_patch.stop)
        self.addCleanup(self.source_patch.stop)
        self.addCleanup(self.conditional_patch.stop)

    def write_test(self, file: str, name: str, shard: str = "store-01") -> None:
        path = self.source / file
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(source_test(name, shard))

    def git(self, *args: str) -> None:
        subprocess.run(
            ["git", *args], cwd=self.root, check=True, capture_output=True, text=True
        )

    def test_tests_gated_to_another_os_are_runtime_conditional_on_this_host(self) -> None:
        gate = '#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-01"))]'
        (self.source / "lib.rs").write_text(
            '#[cfg(target_os = "linux")]\nmod gated_file;\n'
            + f'#[cfg(target_os = "linux")]\n{gate}\n#[test]\nfn item_linux() {{}}\n'
            + f'#[cfg(target_os = "macos")]\n{gate}\n#[test]\nfn item_macos() {{}}\n'
            + f'#[cfg(not(target_os = "macos"))]\n{gate}\n#[test]\nfn item_not_macos() {{}}\n'
            + f'#[cfg(all(test, target_os = "linux"))]\nmod tests {{\n    {gate}\n    #[test]\n    fn inline_linux() {{}}\n}}\n'
            + source_test("everywhere")
        )
        (self.source / "gated_file.rs").write_text(source_test("module_linux"))
        (self.source / "inner.rs").write_text(
            '#![cfg(target_os = "linux")]\n' + source_test("inner_linux")
        )
        with mock.patch.object(checker, "HOST_TARGET_OS", "macos"), mock.patch.object(
            checker, "PLATFORM_CONDITIONAL_NAMES", {}
        ):
            manifest, _ = checker.source_inventory(checker.read_shard_packages(), True)
            conditional = set(checker.PLATFORM_CONDITIONAL_NAMES["store-01"])
        self.assertEqual(
            conditional,
            {"item_linux", "item_not_macos", "inline_linux", "module_linux", "inner_linux"},
        )
        self.assertIn(("crates/rsid/src/lib.rs", "item_macos"), manifest)
        self.assertIn(("crates/rsid/src/lib.rs", "everywhere"), manifest)

    def test_two_branches_add_tests_to_same_shard_and_merge_cleanly(self) -> None:
        self.git("init", "-q")
        self.git("config", "user.name", "Shard fixture")
        self.git("config", "user.email", "shard-fixture@example.invalid")
        self.git("add", "crates/rsid/Cargo.toml")
        self.git("commit", "-qm", "base")
        self.git("branch", "left")
        self.git("branch", "right")

        self.git("checkout", "-q", "left")
        self.write_test("store/left.rs", "left_test")
        self.git("add", "crates/rsid/src/store/left.rs")
        self.git("commit", "-qm", "left test")

        self.git("checkout", "-q", "right")
        self.write_test("store/right.rs", "right_test")
        self.git("add", "crates/rsid/src/store/right.rs")
        self.git("commit", "-qm", "right test")
        self.git("merge", "--no-edit", "left")

        shards = checker.read_shards()
        manifest, gated = checker.source_inventory(
            checker.read_shard_packages(), require_gates=True
        )
        self.assertEqual(shards, ["store-01", "store-02"])
        self.assertEqual(gated, 2)
        self.assertEqual(set(manifest.values()), {"store-01"})
        self.assertEqual({name for _, name in manifest}, {"left_test", "right_test"})
        self.assertFalse((self.root / "crates/rsid/tests/test-shard-manifest.tsv").exists())

    def test_ungated_and_duplicate_gates_fail(self) -> None:
        path = self.source / "store/tests.rs"
        path.parent.mkdir(parents=True)
        path.write_text("#[test]\nfn ungated_test() {}\n")
        with self.assertRaisesRegex(ValueError, "ungated test"):
            checker.source_inventory(checker.read_shard_packages(), require_gates=True)

        gate = source_test("test_with_duplicate_gates")
        path.write_text(gate.splitlines()[0] + "\n" + gate)
        with self.assertRaisesRegex(ValueError, "duplicate shard gates"):
            checker.source_inventory(checker.read_shard_packages(), require_gates=True)

    def test_one_source_file_cannot_span_shards(self) -> None:
        path = self.source / "store/tests.rs"
        path.parent.mkdir(parents=True)
        path.write_text(source_test("first") + source_test("second", "store-02"))
        with self.assertRaisesRegex(ValueError, "one source file spans shards"):
            checker.source_inventory(checker.read_shard_packages(), require_gates=True)

    def test_runtime_count_follows_new_source_tests(self) -> None:
        self.write_test("store/tests.rs", "first")
        lists = self.root / "lists"
        lists.mkdir()
        (lists / "store-01.json").write_text(
            json.dumps(
                {
                    "rust-suites": {
                        "rsid": {
                            "binary-id": "rsid",
                            "testcases": {
                                "store::first": {
                                    "ignored": False,
                                    "filter-match": {"status": "matches"},
                                }
                            },
                        }
                    }
                }
            )
        )
        packages = checker.read_shard_packages()
        manifest, _ = checker.source_inventory(packages, require_gates=True)
        checker.check_runtime(lists, manifest, packages, ["store-01"])

        path = self.source / "store/tests.rs"
        path.write_text(path.read_text() + source_test("second"))
        manifest, _ = checker.source_inventory(packages, require_gates=True)
        with self.assertRaisesRegex(ValueError, "runtime/static test identities differ"):
            checker.check_runtime(lists, manifest, packages, ["store-01"])


class TwoPackageShardTests(unittest.TestCase):
    """The #1021 S4 split: one shard name spans rsid and rsid-store."""

    def setUp(self) -> None:
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        crates = {"rsid": self.root / "crates/rsid", "rsid-store": self.root / "crates/rsid-store"}
        for package, directory in crates.items():
            (directory / "src").mkdir(parents=True)
        (crates["rsid"] / "Cargo.toml").write_text(
            '[features]\ndefault = []\ntest-shard-mode = []\n'
            'test-shard-store-04 = ["test-shard-mode"]\n'
            'test-shard-session-01 = ["test-shard-mode"]\n'
        )
        (crates["rsid-store"] / "Cargo.toml").write_text(
            '[features]\ndefault = []\ntest-shard-mode = []\n'
            'test-shard-store-01 = ["test-shard-mode"]\n'
            'test-shard-store-04 = ["test-shard-mode"]\n'
        )
        for patch in (
            mock.patch.object(checker, "ROOT", self.root),
            mock.patch.object(checker, "CRATES", crates),
            mock.patch.object(checker, "SOURCES", [crate / "src" for crate in crates.values()]),
            mock.patch.object(checker, "CONDITIONAL_RUNTIME_NAMES", {}),
        ):
            patch.start()
            self.addCleanup(patch.stop)

    def write(self, package: str, file: str, name: str, shard: str) -> None:
        path = self.root / "crates" / package / "src" / file
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(source_test(name, shard))

    def test_shards_are_the_ordered_union_and_list_their_packages(self) -> None:
        self.assertEqual(checker.read_shards(), ["store-01", "store-04", "session-01"])
        self.assertEqual(
            checker.read_shard_packages(),
            {
                "store-01": ["rsid-store"],
                "store-04": ["rsid", "rsid-store"],
                "session-01": ["rsid"],
            },
        )

    def test_a_gate_must_name_a_shard_its_own_package_declares(self) -> None:
        self.write("rsid", "session/a.rs", "in_session", "session-01")
        self.write("rsid", "session/b.rs", "wrong_package_shard", "store-01")
        with self.assertRaisesRegex(ValueError, "shard store-01 is not a rsid feature"):
            checker.source_inventory(checker.read_shard_packages(), require_gates=True)

    def test_runtime_check_requires_one_suite_per_declared_package(self) -> None:
        self.write("rsid", "remote/a.rs", "remote_one", "store-04")
        self.write("rsid-store", "store/a.rs", "store_one", "store-04")
        packages = checker.read_shard_packages()
        manifest, _ = checker.source_inventory(packages, require_gates=True)
        lists = self.root / "lists"
        lists.mkdir()

        def suite(binary: str, name: str) -> dict:
            return {
                binary: {
                    "binary-id": binary,
                    "testcases": {
                        f"mod::{name}": {"ignored": False, "filter-match": {"status": "matches"}}
                    },
                }
            }

        (lists / "store-04.json").write_text(
            json.dumps({"rust-suites": {**suite("rsid", "remote_one"), **suite("rsid-store", "store_one")}})
        )
        checker.check_runtime(lists, manifest, packages, ["store-04"])
        (lists / "store-04.json").write_text(json.dumps({"rust-suites": suite("rsid", "remote_one")}))
        with self.assertRaisesRegex(ValueError, "expected one library suite for each"):
            checker.check_runtime(lists, manifest, packages, ["store-04"])
        # A test that moved to the other package is a runtime/static difference.
        (lists / "store-04.json").write_text(
            json.dumps({"rust-suites": {**suite("rsid", "store_one"), **suite("rsid-store", "remote_one")}})
        )
        with self.assertRaisesRegex(ValueError, "runtime/static test identities differ"):
            checker.check_runtime(lists, manifest, packages, ["store-04"])


if __name__ == "__main__":
    unittest.main()
