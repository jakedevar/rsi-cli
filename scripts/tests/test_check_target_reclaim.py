"""Literal writer detection and fail-closed agreement with the Rust policy."""

import importlib.util
import json
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import unittest


ROOT = Path(__file__).resolve().parents[2]
SCRIPT = ROOT / "scripts/check-target-reclaim.py"
# Explicit observer paths let python-test-gate select this suite when only
# the Rust policy or retained-entry registry changes.
POLICY = "crates/rsid-store/src/sandbox/target_reclaim.rs"
RETAINED = "scripts/target-reclaim-retained.json"
NEXTEST = ".config/nextest.toml"
SPEC = importlib.util.spec_from_file_location("check_target_reclaim", SCRIPT)
GUARD = importlib.util.module_from_spec(SPEC)
sys.modules[SPEC.name] = GUARD
SPEC.loader.exec_module(GUARD)


def install_policy(root):
    for relative in (GUARD.POLICY, GUARD.SHARED, GUARD.RETAINED, NEXTEST):
        path = root / relative
        path.parent.mkdir(parents=True, exist_ok=True)
        shutil.copyfile(ROOT / relative, path)
    # Fixture policy files are scanned too; their test-only names already have
    # the same narrowly scoped exceptions as the shipped tree.


class TargetReclaimTest(unittest.TestCase):
    def test_shipped_policy_agrees_with_the_guard_view(self):
        self.assertEqual(GUARD.POLICY, POLICY)
        self.assertEqual(GUARD.RETAINED, RETAINED)
        policy = GUARD.read_policy(ROOT)
        self.assertEqual(policy.directories, set(
            "debug release doc package tmp cargo-timings criterion dhat .rsi-tmp "
            "rsid-test-shards desktop-tauri e2e-artifacts nextest".split()))
        self.assertEqual(policy.files, set(
            ".rustc_info.json CACHEDIR.TAG .cargo-lock .future-incompat-report.json "
            ".rsid-test-shards.lock .rsi-job-admission.lock .rsi-release.lock .rsi-desktop.lock "
            ".rsi-build-env.json .rsi-target-pool.json python-discover.start "
            "python-discover.head python-discover.log".split()))
        for name in ("rsid-test-fixtures", "issue211-idle-costs-123.jsonl"):
            self.assertTrue(policy.allows(name), name)
        for name in ("-fixtures", "issue211-idle-costs-.jsonl", "issue211-idle-costs-x.jsonl", "new-thing"):
            self.assertFalse(policy.allows(name), name)

    def test_policy_names_and_constants_are_read_from_rust(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            install_policy(root)
            source = root / GUARD.POLICY
            source.write_text(source.read_text().replace('b"debug"\n', 'b"brand-new" | b"debug"\n', 1))
            shared = root / GUARD.SHARED
            shared.write_text(shared.read_text().replace('= "e2e-artifacts";', '= "renamed-e2e";'))
            policy = GUARD.read_policy(root)
            self.assertTrue(policy.allows("brand-new"))
            self.assertTrue(policy.allows("renamed-e2e"))

    def test_an_unrecognized_policy_predicate_fails_closed(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            install_policy(root)
            source = root / GUARD.POLICY
            source.write_text(source.read_text().replace(
                'name == E2E_ARTIFACTS_DIR_NAME.as_bytes()', 'name.starts_with(b"new-")', 1))
            with self.assertRaisesRegex(ValueError, "unsupported reclaim predicate"):
                GUARD.read_policy(root)

    def test_writer_spellings_find_the_literal_top_level_name(self):
        writers = (
            'std::fs::create_dir_all("target/new-thing");',
            'std::fs::write(root.join("target/new-thing/file"), b"data");',
            'std::fs::create_dir_all(root.join("target").join("new-thing"));',
            'let cache = root.join("target");\nstd::fs::create_dir_all(cache.join("new-thing"));',
            'let cache: PathBuf = root.join("target");\nstd::fs::create_dir_all(cache.join("new-thing"));',
            'let cache = std::env::var_os("CARGO_TARGET_DIR")\n'
            '    .map(PathBuf::from)\n    .unwrap_or_else(|| root.join("target"));\n'
            'std::fs::write(cache.join("new-thing/file"), b"x");',
            'let base = std::env::var_os("CARGO_TARGET_DIR")\n'
            '    .map(PathBuf::from)\n    .unwrap_or_else(|| root.join("target"))\n'
            '    .join("new-thing");\nstd::fs::create_dir_all(&base);',
            'mkdir -p target/new-thing',
            'mkdir -p "$CARGO_TARGET_DIR/new-thing"',
            'mkdir -p "${CARGO_TARGET_DIR}/new-thing"',
            'CACHE="$CARGO_TARGET_DIR"\nmkdir -p "$CACHE/new-thing"',
            'CACHE="${CARGO_TARGET_DIR:-$ROOT/target}"\nmkdir -p "$CACHE/new-thing"',
            'cargo build --target-dir target/new-thing',
            'cache = Path(os.environ["CARGO_TARGET_DIR"])\n(cache / "new-thing").mkdir()',
            'cache = Path(os.environ.get("CARGO_TARGET_DIR", Path(repo) / "target"))\n'
            'out = cache / "new-thing"\nout.mkdir()',
            'cache = Path(os.environ["CARGO_TARGET_DIR"])\nos.makedirs(os.path.join(cache, "new-thing"))',
            'os.makedirs(os.path.join(os.environ["CARGO_TARGET_DIR"], "new-thing"))',
            'out = Path(env["CARGO_TARGET_DIR"]) / "new-thing"\nout.mkdir()',
        )
        for source in writers:
            with self.subTest(source=source):
                self.assertEqual({name for _, name in GUARD.entries(source)}, {"new-thing"})

    def test_only_the_top_level_join_is_checked(self):
        source = ('let cache = root.join("target");\n'
                  'let deps = cache.join("debug");\n'
                  'std::fs::create_dir_all(deps.join("deps"));\n'
                  'std::fs::write(root.join("target/.rsi-tmp/scoped-test/file"), b"x");')
        self.assertEqual({name for _, name in GUARD.entries(source)}, {"debug", ".rsi-tmp"})

    def test_nextest_store_and_junit_output_are_checked(self):
        self.assertEqual(GUARD.nextest_entries((ROOT / NEXTEST).read_text()), [(1, "nextest")])
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            install_policy(root)
            (root / NEXTEST).write_text('[store]\ndir = "target/new-thing/reports"\n'
                                       '[profile.test-timings.junit]\npath = "junit.xml"\n')
            self.assertIn(f"{NEXTEST}:1: target/new-thing", "\n".join(GUARD.check(root)))

    def test_nextest_store_outside_target_has_no_target_entry(self):
        self.assertEqual(GUARD.nextest_entries('[store]\ndir = "reports"\n'), [])

    def test_nested_env_default_is_a_root_not_a_child(self):
        source = ('cache = Path(os.environ.get("CARGO_TARGET_DIR", str(Path.home() / ".cargo/shared-target")))\n'
                  '(cache / "new-thing").mkdir()')
        self.assertEqual({name for _, name in GUARD.entries(source)}, {"new-thing"})

    def test_target_aliases_do_not_leak_between_functions(self):
        sources = (
            '''fn production(root: &Path) {
    let own = root.join("target");
    let copy = own;
    std::fs::write(copy.join("new-thing"), b"x");
}
fn unrelated(own: &Path, copy: &Path) {
    std::fs::read(own.join("new-other"));
    std::fs::read(copy.join("new-other"));
}''',
            '''def production(root):
    own = root / "target"
    copy = own
    (copy / "new-thing").mkdir()

def unrelated(own, copy):
    (own / "new-other").read_bytes()
    (copy / "new-other").read_bytes()
''',
            '''production() {
    own="$CARGO_TARGET_DIR"
    copy="$own"
    mkdir -p "$copy/new-thing"
}
unrelated() {
    cat "$own/new-other" "$copy/new-other"
}''',
        )
        for source in sources:
            with self.subTest(source=source):
                self.assertEqual(GUARD.entries(source), [(4, "new-thing")])

    def test_aliases_follow_binding_order_and_block_shadowing(self):
        source = '''fn production(root: &Path, cache: &Path) {
    std::fs::read(cache.join("new-other"));
    let cache = root.join("target");
    {
        std::fs::write(cache.join("new-thing"), b"x");
        let temporary = tempfile::tempdir().unwrap();
        let cache = temporary.path();
        std::fs::read(cache.join("new-other"));
    }
    std::fs::write(cache.join("new-other"), b"x");
    {
        let inner = root.join("target");
    }
    std::fs::read(inner.join("new-other"));
}'''
        self.assertEqual(GUARD.entries(source), [(5, "new-thing"), (10, "new-other")])

    def test_relative_target_reassignment_keeps_incremental_entry(self):
        source = '''target_dir = Path(os.environ.get("CARGO_TARGET_DIR", Path(repo) / "target"))
if not target_dir.is_absolute():
    target_dir = Path(repo) / target_dir
cache = target_dir / "debug" / "incremental"
'''
        self.assertEqual(GUARD.entries(source), [(4, "debug")])

    def test_relative_candidate_reassignment_keeps_baseline_entry(self):
        source = '''candidate = Path(os.environ.get("CARGO_TARGET_DIR", Path(repo) / "target"))
if not candidate.is_absolute():
    candidate = Path(repo) / candidate
parent = candidate / "scoped-baselines"
'''
        self.assertEqual(GUARD.entries(source), [(4, "scoped-baselines")])

    def test_target_named_parameter_keeps_debug_entry(self):
        sources = ('''fn rsid_test_binary_sets(target: &Path) -> usize {
    let entries = fs::read_dir(target.join("debug/deps"));
}''', '''def rsid_test_binary_sets(target):
    return (target / "debug/deps").iterdir()
''')
        for source in sources:
            with self.subTest(source=source):
                self.assertEqual(GUARD.entries(source), [(2, "debug")])

    def test_parameters_preserve_visible_target_bindings(self):
        source = '''cache = Path(os.environ["CARGO_TARGET_DIR"])
def production(cache):
    (cache / "new-thing").mkdir()
'''
        self.assertEqual(GUARD.entries(source), [(3, "new-thing")])

    def test_unknown_rebinding_keeps_target_until_proven_fixture(self):
        source = '''fn production(root: &Path) {
    let cache = root.join("target");
    let cache = resolve(cache);
    std::fs::write(cache.join("new-thing"), b"x");
    let copy = resolve(cache);
    std::fs::write(copy.join("new-thing"), b"x");
    let cache = unrelated;
    std::fs::write(cache.join("new-thing"), b"x");
    let temporary = tempfile::tempdir().unwrap();
    let cache = temporary.path();
    std::fs::write(cache.join("new-other"), b"x");
}'''
        self.assertEqual(GUARD.entries(source),
                         [(4, "new-thing"), (6, "new-thing"), (8, "new-thing")])

    def test_own_target_admission_fixtures_are_isolated(self):
        # Minimal equivalent of #514's own_target.rs at 5c5ff3b94: `own`
        # denotes a production target in one function and a sandbox in another.
        # The guard also scans embedded source strings. Expand the fixture
        # names at runtime so this Python file needs no new retention entries.
        source = '''fn eligible_target(sandbox: &Path) {
    let root = sandbox.canonicalize().unwrap();
    let own = root.join("target");
}
fn target_identity_fixture() {
    let fixture = tempfile::tempdir().unwrap();
    let root = fixture.path().canonicalize().unwrap();
    let own = root.join("own");
    let other = root.join("other");
    std::fs::create_dir_all(own.join("@nested@")).unwrap();
    std::fs::create_dir_all(other.join("target")).unwrap();
    let target = own.join("@target@");
    std::fs::create_dir_all(target.join("@target@")).unwrap();
    std::fs::create_dir_all(root.join("target/@nested@")).unwrap();
    std::fs::create_dir_all(root.join("target/@target@")).unwrap();
}
fn own_target_bytes_fixture() {
    let fixture = tempfile::tempdir().unwrap();
    let own = fixture.path().join("own/target");
    let other = fixture.path().join("other/target");
    std::fs::write(own.join("@artifact@"), b"x");
    std::os::unix::fs::symlink(&other, own.join("@sibling@"));
}'''.replace("@", "")
        self.assertEqual(GUARD.entries(source), [])
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            install_policy(root)
            writer = root / "crates/fixture.rs"
            writer.parent.mkdir(exist_ok=True)
            writer.write_text(source)
            self.assertEqual(GUARD.check(root), [])
            writer.write_text(source + '\nstd::fs::write("target/new-thing", b"x");')
            self.assertIn("crates/fixture.rs:24: target/new-thing", "\n".join(GUARD.check(root)))

    def test_fixture_provenance_is_explicit_and_local(self):
        constructors = (
            "tempfile::tempdir().unwrap()", "TempDir::new().unwrap()",
            'crate::test_support::disk_backed_tempdir("target guard")',
        )
        for constructor in constructors:
            with self.subTest(constructor=constructor):
                source = f'''fn fixture() {{
    let temporary = {constructor};
    let root = temporary.path();
    let copy = root;
    std::fs::write(copy.join("target/{'nested'}"), b"x");
    std::fs::write({constructor}.path().join("target/{'target'}"), b"x");
}}
fn production(root: &Path) {{
    let target = root.join("target");
    std::fs::write(target.join("new-thing"), b"x");
}}'''
                self.assertEqual(GUARD.entries(source), [(10, "new-thing")])

    def test_fixture_names_do_not_hide_production_paths_or_escapes(self):
        sources = (
            'let fixture = repository;\nstd::fs::write(fixture.join("target/new-thing"), b"x");',
            'let fixture = tempfile::tempdir().unwrap();\n'
            'let cache = std::env::var_os("CARGO_TARGET_DIR")\n'
            '    .map(PathBuf::from)\n'
            '    .unwrap_or_else(|| fixture.path().join("target"));\n'
            'std::fs::write(cache.join("new-thing"), b"x");',
            'let fixture = tempfile::tempdir().unwrap();\n'
            'std::fs::write(fixture.path().join("../target/new-thing"), b"x");',
            'let fixture = tempfile::tempdir().unwrap();\n'
            'std::fs::write(fixture.path().join("/repo/target/new-thing"), b"x");',
            '''fn fixture() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path();
    {
        let root = repository;
        std::fs::write(root.join("target/new-thing"), b"x");
    }
    std::fs::write(root.join("target/new-other"), b"x");
}''',
        )
        for source in sources:
            with self.subTest(source=source):
                self.assertEqual({name for _, name in GUARD.entries(source)}, {"new-thing"})

    def test_python_tempdir_paths_and_parameter_shadowing(self):
        source = '''temporary = tempfile.TemporaryDirectory()
root = Path(temporary.name)
(root / "target/new-other").mkdir()
def production(root):
    (root / "target/new-thing").mkdir()
'''
        self.assertEqual(GUARD.entries(source), [(5, "new-thing")])
        source = '''def production(root):
    root = Path(os.environ["CARGO_TARGET_DIR"])
    (root / "new-thing").mkdir()
'''
        self.assertEqual(GUARD.entries(source), [(3, "new-thing")])

    def test_quoted_braces_do_not_end_a_target_alias_scope(self):
        source = '''fn production(root: &Path) {
    let cache = root.join("target");
    println!("}"); // }
    std::fs::write(cache.join("new-thing"), b"x");
}
fn unrelated(cache: &Path) {
    std::fs::read(cache.join("new-other"));
}'''
        self.assertEqual(GUARD.entries(source), [(4, "new-thing")])

    def test_unknown_writer_fails_cli_with_source_line_and_both_policy_paths(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            install_policy(root)
            writer = root / "scripts/writer.sh"
            writer.write_text('#!/bin/sh\nmkdir -p target/new-thing\n')
            result = subprocess.run([sys.executable, str(SCRIPT), "--root", str(root)],
                                    text=True, capture_output=True)
            self.assertEqual(result.returncode, 1, result.stderr)
            self.assertIn("scripts/writer.sh:2: target/new-thing", result.stderr)
            self.assertIn(GUARD.POLICY, result.stderr)
            self.assertIn(GUARD.RETAINED, result.stderr)
            retained = json.loads((root / GUARD.RETAINED).read_text())
            retained["scripts/writer.sh"] = {"entries": ["new-thing"], "reason": "Fixture intentionally retains evidence."}
            (root / GUARD.RETAINED).write_text(json.dumps(retained))
            self.assertEqual(GUARD.check(root), [])
            # An exception does not exempt a new name in that same file.
            writer.write_text('mkdir -p target/new-other\n')
            self.assertIn("target/new-other", "\n".join(GUARD.check(root)))

    def test_current_tree_passes(self):
        self.assertEqual(GUARD.check(ROOT), [])


if __name__ == "__main__":
    unittest.main()
