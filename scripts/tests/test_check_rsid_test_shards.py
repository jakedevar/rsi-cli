"""Intent checks for the source-derived rsid shard inventory."""

import contextlib
import importlib.util
import io
import json
from pathlib import Path
import runpy
import subprocess
import sys
import unittest
from unittest import mock


sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
from scratch_git import scratch_git, scratch_tempdir  # noqa: E402

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
        self.temp = scratch_tempdir("rsid-test-shards-")
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
        # source_inventory fills this module global; without a fresh dict per test, names
        # recorded by an earlier test leak into check_runtime on hosts that exclude them
        # (macOS vs the target_os = "linux" fixtures) and fail it only in module order.
        self.platform_patch = mock.patch.object(checker, "PLATFORM_CONDITIONAL_NAMES", {})
        self.root_patch.start()
        self.crates_patch.start()
        self.source_patch.start()
        self.conditional_patch.start()
        self.platform_patch.start()
        self.addCleanup(self.root_patch.stop)
        self.addCleanup(self.crates_patch.stop)
        self.addCleanup(self.source_patch.stop)
        self.addCleanup(self.conditional_patch.stop)
        self.addCleanup(self.platform_patch.stop)

    def write_test(self, file: str, name: str, shard: str = "store-01") -> None:
        path = self.source / file
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(source_test(name, shard))

    def git(self, *args: str) -> None:
        scratch_git(self.root, *args)

    def test_only_the_shard_lint_allow_may_be_a_cfg_attr_naming_a_shard_feature(self) -> None:
        path = self.source / "lib.rs"
        allowed = (
            "#![cfg_attr(\n    all(test, feature = \"test-shard-mode\"),\n"
            "    allow(dead_code, unused_imports)\n)]\n"
        )
        path.write_text(allowed)
        checker.check_shard_cfg_attributes(path)
        for hiding in (
            '#![cfg_attr(feature = "test-shard-mode", allow(dead_code))]\n',
            '#[cfg_attr(all(test, feature = "test-shard-mode"), allow(dead_code, unused_imports))]\n',
            '#![cfg_attr(all(test, feature = "test-shard-store-01"), ignore)]\n',
        ):
            path.write_text(hiding)
            with self.assertRaisesRegex(ValueError, "cfg_attr must not name a test shard feature"):
                checker.check_shard_cfg_attributes(path)

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

    def write_source(self, file: str, text: str) -> None:
        path = self.source / file
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(text)

    def mount_fixture(self, remote_answers: str) -> None:
        self.write_source("lib.rs", "pub mod store;\n")
        self.write_source("store/mod.rs", "mod pending_questions;\n")
        self.write_source(
            "store/pending_questions.rs", '#[path = "remote_answers.rs"]\npub mod remote_answers;\n'
        )
        self.write_source("store/remote_answers.rs", remote_answers)
        self.write_test("store/remote_answers/tests.rs", "new_answer_journal")

    def test_a_plain_mod_tests_under_a_path_mount_leaves_the_stem_directory_test_file_unmounted_1697(self) -> None:
        # rustc loads `store/tests.rs` for this `mod tests;`, so remote_answers/tests.rs never compiles.
        self.mount_fixture("pub fn f() {}\n#[cfg(test)]\nmod tests;\n")
        self.write_test("store/tests.rs", "compiled_beside")
        self.assertEqual(checker.unmounted_test_sources(), ["crates/rsid/src/store/remote_answers/tests.rs"])

    def test_an_explicit_path_test_mount_leaves_no_test_file_unmounted_1697(self) -> None:
        self.mount_fixture('pub fn f() {}\n#[cfg(test)]\n#[path = "remote_answers/tests.rs"]\nmod tests;\n')
        self.assertEqual(checker.unmounted_test_sources(), [])

    def test_ordinary_modules_include_and_bin_targets_are_mounted_1697(self) -> None:
        self.write_source("lib.rs", 'pub mod plain;\ninclude!("spliced.rs");\n')
        self.write_source("plain.rs", "mod tests;\n")
        self.write_test("plain/tests.rs", "plain_child")
        self.write_test("spliced.rs", "spliced_test")
        self.write_source("bin/tool/main.rs", "mod helper;\nfn main() {}\n")
        self.write_test("bin/tool/helper.rs", "bin_child")
        self.write_test("bin/single.rs", "bin_single")
        self.assertEqual(checker.unmounted_test_sources(), [])

    def test_the_mount_model_matches_the_touched_shard_mapper_1697(self) -> None:
        touched = runpy.run_path(str(Path(SCRIPT).parent / "check-touched-shards"), run_name="check_touched_shards")
        src = "crates/rsid/src/"
        files = {
            f"{src}lib.rs": 'pub mod store;\nmod plain;\nmod inline {\n    mod nested;\n}\ninclude!("spliced.rs");\n',
            f"{src}store/mod.rs": "mod pending;\n",
            f"{src}store/pending.rs": '#[path = "answers.rs"]\npub mod answers;\n#[path = "../shared/fix.rs"]\nmod fix;\n',
            f"{src}store/answers.rs": '#[cfg(test)]\nmod tests;\n#[path = "answers/more.rs"]\nmod more;\n',
            f"{src}store/tests.rs": "// beside the path-mounted file\n",
            f"{src}store/answers/tests.rs": "// never compiled\n",
            f"{src}store/answers/more.rs": "mod deep;\n",
            f"{src}store/deep.rs": "// beside more.rs, a path-mounted file\n",
            f"{src}shared/fix.rs": "mod leaf;\n",
            f"{src}shared/leaf.rs": "// beside fix.rs\n",
            f"{src}plain.rs": "mod child;\n",
            f"{src}plain/child.rs": "// stem directory\n",
            f"{src}child.rs": "// not plain's child\n",
            f"{src}inline/nested.rs": "// under the inline block\n",
            f"{src}spliced.rs": "fn spliced() {}\n",
        }
        root = f"{src}lib.rs"
        self.assertEqual(checker.mounted_sources(files, root), set(touched["mount_closure"](files, root)))

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

    def test_shards_only_listing_skips_the_source_inventory(self) -> None:
        # #1853: the shard names come from the manifests; an inventory that would
        # fail (an ungated test) must not be needed to list them.
        (self.source / "bad.rs").write_text("#[test]\nfn ungated() {}\n")
        out = io.StringIO()
        with mock.patch.object(sys, "argv", ["x", "--list-shard-packages", "--shards-only"]), contextlib.redirect_stdout(out):
            self.assertEqual(checker.main(), 0)
        self.assertEqual(out.getvalue().splitlines(), ["store-01 rsid", "store-02 rsid"])

    def test_masking_is_unchanged_by_the_jump_scan(self) -> None:
        source = 'let a = 1; // c\nlet s = "x//y"; let r = r#"q"#; let c = \'/\'; /* b */ fn f() {}\n'
        masked = checker.mask_literals_and_comments(source, "m.rs")
        self.assertEqual(len(masked), len(source))
        self.assertIn("fn f() {}", masked)
        self.assertNotIn("x//y", masked)

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

    def test_a_malformed_shard_gate_is_refused_with_the_fix_not_called_ungated(self) -> None:
        gate = 'any(not(feature = "test-shard-mode"), feature = "test-shard-store-01")'
        target = '#[cfg(target_os = "linux")]'
        for body in (
            f'#[cfg(all(target_os = "linux", {gate}))]\n#[test]\nfn t() {{}}\n',
            f'#[cfg(all(target_os = "linux",\n    {gate}))]\n#[test]\nfn t() {{}}\n',
            f'#[cfg({gate})]\n{target}\n#[test]\nfn t() {{}}\n',
        ):
            (self.source / "lib.rs").write_text(body)
            with self.assertRaisesRegex(
                ValueError, r"incorrect shard gate; .*its own line.*separate `#\[cfg"
            ):
                checker.source_inventory(checker.read_shard_packages(), require_gates=True)

        (self.source / "lib.rs").write_text(f"{target}\n" + source_test("separate_cfg_lines"))
        manifest, gated = checker.source_inventory(checker.read_shard_packages(), True)
        self.assertEqual(gated, 1)
        self.assertEqual(manifest[("crates/rsid/src/lib.rs", "separate_cfg_lines")], "store-01")

    def test_ungated_helper_in_a_shard_gated_test_file_fails(self) -> None:
        # #2628: `launch_refusal` had no shard cfg while the file's `use` items and
        # tests did, so every other shard build failed to compile it.
        gate = '#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-01"))]'
        path = self.source / "store/tests.rs"
        path.parent.mkdir(parents=True)
        for kind, helper in {
            "fn": "async fn launch_refusal(p: &Pilot) -> String { String::new() }\n",
            "struct": "struct Fixture(Pilot);\n",
            "impl": "impl Fixture { fn new() {} }\n",
            "const": "const LIMIT: usize = 3;\n",
        }.items():
            path.write_text(f"{gate}\nuse super::Pilot;\n\n{helper}\n" + source_test("a_test"))
            with self.assertRaisesRegex(ValueError, f"tests.rs:4: ungated {kind} .*shard-gated"):
                checker.source_inventory(checker.read_shard_packages(), require_gates=False)

        path.write_text(
            f"{gate}\nuse super::Pilot;\n\n{gate}\nasync fn launch_refusal(p: &Pilot) {{}}\n\n"
            + source_test("a_test")
        )
        manifest, _ = checker.source_inventory(checker.read_shard_packages(), require_gates=False)
        self.assertEqual(manifest, {("crates/rsid/src/store/tests.rs", "a_test"): "store-01"})

    def test_ungated_helper_inside_a_gated_module_or_ungated_file_is_allowed(self) -> None:
        gate = '#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-01"))]'
        path = self.source / "store/tests.rs"
        path.parent.mkdir(parents=True)
        # A gated module owns its helpers; an ungated `use` imposes nothing.
        path.write_text(
            f"use std::fmt;\nfn plain_helper() {{}}\n{gate}\nmod inner {{\n    use super::*;\n"
            "    fn helper() {}\n}\n" + source_test("a_test")
        )
        manifest, _ = checker.source_inventory(checker.read_shard_packages(), require_gates=False)
        self.assertEqual(manifest, {("crates/rsid/src/store/tests.rs", "a_test"): "store-01"})

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
        self.temp = scratch_tempdir("rsid-test-shards-")
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
            mock.patch.object(checker, "PLATFORM_CONDITIONAL_NAMES", {}),
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
