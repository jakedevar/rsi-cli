"""The rsi client-contract exporter (#3453 slice A, scripts/export-client-contract.py).

The out-of-tree desktop client vendors the folder this script writes. These tests
pin the contract: every input exists, the output is deterministic, the manifest
hashes match, `--check` reports drift, and the settings source list is exactly what
the desktop's settings generator reads.
"""

import hashlib
import importlib.util
import json
from pathlib import Path
import re
import shutil
import subprocess
import sys
import unittest

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
from scratch_git import scratch_git, scratch_tempdir  # noqa: E402

ROOT = Path(__file__).resolve().parents[2]
SCRIPT = ROOT / "scripts/export-client-contract.py"
SPEC = importlib.util.spec_from_file_location("export_client_contract", SCRIPT)
EXPORT = importlib.util.module_from_spec(SPEC)
sys.modules[SPEC.name] = EXPORT
SPEC.loader.exec_module(EXPORT)

# Every contract input is named in full so the python gate selects this suite when
# one of them changes (scripts/python-test-gate.py: a full path observes a path
# outside scripts/ and tools/). The TUI source tree is covered by the inventory.
# crates/rsi/src/client.rs, the rsi-common modules whose method constants it passes to request()
# and types.rs (Project::HOME_ID, SessionProvider::CANONICAL_NAMES).
CLIENT_INPUTS = (
    "crates/rsi/src/client.rs",
    "crates/rsi-common/src/input_assets.rs",
    "crates/rsi-common/src/manager_budget.rs",
    "crates/rsi-common/src/mcp.rs",
    "crates/rsi-common/src/operator_config.rs",
    "crates/rsi-common/src/prompt_files.rs",
    "crates/rsi-common/src/provider_contract.rs",
    "crates/rsi-common/src/provider_login.rs",
    "crates/rsi-common/src/types.rs",
)
INPUTS = (
    "docs/keybindings.md",
    "docs/manual/rsi-manual.md",
    "crates/rsi/src/settings_registry.rs",
    "crates/rsi/src/settings.rs",
    "crates/rsi/src/ui/theme.rs",
    "crates/rsi/src/ui/theme_roles.rs",
    "crates/rsi/src/overlay/daemon_text_editor.rs",
    "crates/rsi-common/src/daemon_config_catalog.rs",
    "crates/rsi-common/src/editing_mode.rs",
    "crates/rsi-common/src/model_control.rs",
    "crates/rsi-common/src/provider_profile.rs",
    "crates/rsi-common/src/provider_credentials.rs",
    "crates/rsi-common/src/provider_lifecycle_hooks.rs",
    "crates/rsi-common/src/provider_skill_delivery.rs",
    "crates/rsi-common/src/provider_launch_overrides.rs",
    "crates/rsi-common/src/exec_target.rs",
    "crates/rsi-common/src/bench_settings.rs",
    "rust-toolchain.toml",
) + CLIENT_INPUTS
SETTINGS_MJS = "desktop/scripts/settings.mjs"
GIT_IDENTITY = ("-c", "user.name=Test", "-c", "user.email=test@example.invalid")


def run(*args, expect=0):
    result = subprocess.run([sys.executable, str(SCRIPT), *map(str, args)], capture_output=True, text=True)
    assert result.returncode == expect, (args, result.returncode, result.stdout, result.stderr)
    return result


def tree(directory):
    return {p.relative_to(directory).as_posix(): p.read_bytes() for p in sorted(directory.rglob("*")) if p.is_file()}


def head():
    result = subprocess.run(["git", "-C", str(ROOT), "rev-parse", "HEAD"], capture_output=True, text=True)
    return result.stdout.strip() if result.returncode == 0 else None


CLIENT_RS = """\
async fn request(&mut self, method: &str, params: Value) {}
/// the next `request()` call
fn a() {
    self.request("ListSessions", Value::Null).await?;
    self.request(
        "GetSession",
        params,
    );
    self.request(rsi_common::calls::METHOD_LIST, Value::Null);
    use rsi_common::calls::{Other, METHOD_PUT};
    self.request(METHOD_PUT, Value::Null);
}
"""


class Scratch(unittest.TestCase):
    def setUp(self):
        self.temp = scratch_tempdir(prefix="export-client-contract-")
        self.addCleanup(self.temp.cleanup)
        self.dir = Path(self.temp.name)

    def write(self, root, rel, text):
        path = root / rel
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(text, encoding="utf-8")


class RealTree(Scratch):
    def test_every_named_input_exists(self):
        for rel in INPUTS:
            self.assertTrue((ROOT / rel).is_file(), rel)

    def test_two_exports_are_identical_bytes(self):
        run("--out", self.dir / "a", "--allow-dirty")
        run("--out", self.dir / "b", "--allow-dirty")
        first, second = tree(self.dir / "a"), tree(self.dir / "b")
        self.assertEqual(first, second)
        self.assertGreater(len(first), 10)

    def test_the_manifest_lists_and_hashes_every_other_file(self):
        out = self.dir / "c"
        run("--out", out, "--allow-dirty")
        files = tree(out)
        manifest = json.loads(files.pop("manifest.json"))
        self.assertEqual(manifest["contract_version"], 1)
        self.assertEqual(manifest["exporter"], "scripts/export-client-contract.py")
        self.assertRegex(manifest["rsi_commit"], r"^[0-9a-f]{40}$")
        self.assertEqual(sorted(manifest["files"]), sorted(files))
        for rel, data in files.items():
            self.assertEqual(manifest["files"][rel], "sha256:" + hashlib.sha256(data).hexdigest(), rel)
        raw = (out / "manifest.json").read_bytes()
        self.assertTrue(raw.endswith(b"\n") and b"\r" not in raw)
        self.assertEqual(raw, (json.dumps(manifest, indent=2, sort_keys=True) + "\n").encode())

    def test_verbatim_files_are_byte_copies(self):
        out = self.dir / "c"
        run("--out", out, "--allow-dirty")
        for rel in ("docs/keybindings.md", "docs/manual/rsi-manual.md"):
            self.assertEqual((out / rel).read_bytes(), (ROOT / rel).read_bytes())
        for rel in EXPORT.settings_sources(ROOT):
            self.assertEqual((out / "settings/sources" / rel).read_bytes(), (ROOT / rel).read_bytes())

    def test_inventory_matches_the_tui_tree(self):
        out = self.dir / "c"
        run("--out", out, "--allow-dirty")
        inventory = json.loads((out / "tui-inventory.json").read_text())
        self.assertEqual(sorted(inventory), ["client_methods", "modules", "tui_files", "tui_ops"])
        every = sorted(p.relative_to(ROOT).as_posix() for p in (ROOT / "crates/rsi/src").rglob("*.rs"))
        self.assertEqual(inventory["tui_files"], every)
        self.assertIn("crates/rsi/src/settings_registry.rs", every)
        self.assertTrue(inventory["modules"])
        for module in inventory["modules"]:
            self.assertRegex(module, r"^crates/rsi/src/ui/(overlay/)?[^/]+\.rs$")
            self.assertNotIn(module.rsplit("/", 1)[1], ("mod.rs", "tests.rs"))
        self.assertEqual(inventory["modules"], sorted(inventory["modules"]))
        self.assertEqual(inventory["tui_ops"], sorted(set(inventory["tui_ops"])))
        self.assertTrue(inventory["tui_ops"])

    def test_tui_files_cover_every_rust_file_including_overlay_and_the_top_level(self):
        out = self.dir / "c"
        run("--out", out, "--allow-dirty")
        files = json.loads((out / "tui-inventory.json").read_text())["tui_files"]
        for rel in ("crates/rsi/src/client.rs", "crates/rsi/src/overlay/daemon_text_editor.rs",
                    "crates/rsi/src/ui/theme.rs", "crates/rsi/src/settings.rs"):
            self.assertIn(rel, files)
        self.assertTrue(any(f.startswith("crates/rsi/src/ui/") for f in files))
        self.assertTrue(any(f.startswith("crates/rsi/src/overlay/") for f in files))

    def test_client_methods_are_the_sorted_methods_client_rs_calls_with_constants_resolved(self):
        out = self.dir / "c"
        run("--out", out, "--allow-dirty")
        methods = json.loads((out / "tui-inventory.json").read_text())["client_methods"]
        self.assertEqual(methods, sorted(set(methods)))
        # A literal, a path constant, a bare imported constant and the launch helper's call.
        for method in ("ListSessions", "ListMcpServers", "GetProviderLoginState", "GetGlobalManagerBudget",
                       "ListSkills", "GetSkill", "LaunchSession"):
            self.assertIn(method, methods)
        for method in methods:
            self.assertRegex(method, r"^[A-Z][A-Za-z0-9]+$")
        self.assertFalse([m for m in methods if "::" in m or m.startswith("METHOD")])

    def test_the_manifest_pins_the_rust_toolchain_channel(self):
        out = self.dir / "c"
        run("--out", out, "--allow-dirty")
        channel = re.search(r'^channel\s*=\s*"([^"]+)"', (ROOT / "rust-toolchain.toml").read_text(), re.M).group(1)
        manifest = json.loads((out / "manifest.json").read_text())
        self.assertEqual(manifest["rust_toolchain"], channel)

    def test_the_rust_symbols_the_client_names_keep_their_public_names(self):
        # The client reads these through the rsi-common git dependency (#3453 slice A2).
        text = (ROOT / "crates/rsi-common/src/types.rs").read_text()
        project = re.search(r"^impl Project \{\n(.*?)^\}", text, re.M | re.S)
        self.assertIsNotNone(project)
        self.assertRegex(project.group(1), r"(?m)^    pub const HOME_ID: Uuid = ")
        provider = re.search(r"^impl SessionProvider \{\n(.*?)^\}", text, re.M | re.S)
        self.assertIsNotNone(provider)
        self.assertRegex(
            provider.group(1),
            r"(?m)^    pub const CANONICAL_NAMES: &'static \[&'static str\] = &\[",
        )

    def test_check_passes_on_a_fresh_export_and_detects_a_one_byte_edit(self):
        out = self.dir / "c"
        run("--out", out, "--allow-dirty")
        run("--check", out, "--allow-dirty")
        for rel in ("docs/keybindings.md", "tui-inventory.json", "manifest.json",
                    "settings/sources/crates/rsi/src/settings_registry.rs"):
            target = out / rel
            original = target.read_bytes()
            edited = bytearray(original)
            edited[len(edited) // 2] ^= 0x01
            target.write_bytes(bytes(edited))
            result = run("--check", out, "--allow-dirty", expect=1)
            self.assertIn(f"changed: {rel}", result.stderr)
            target.write_bytes(original)
        run("--check", out, "--allow-dirty")

    def test_check_reports_a_missing_and_an_unlisted_file(self):
        out = self.dir / "c"
        run("--out", out, "--allow-dirty")
        (out / "docs/manual/rsi-manual.md").unlink()
        (out / "stray.txt").write_text("x")
        result = run("--check", out, "--allow-dirty", expect=1)
        self.assertIn("missing: docs/manual/rsi-manual.md", result.stderr)
        self.assertIn("unlisted: stray.txt", result.stderr)
        run("--check", self.dir / "absent", "--allow-dirty", expect=1)

    def test_a_commit_other_than_head_is_refused_and_head_is_accepted(self):
        current = head()
        if current is None:
            self.skipTest("not a git checkout")
        other = ("0" if current[0] != "0" else "1") * 40
        result = run("--out", self.dir / "x", "--commit", other, "--allow-dirty", expect=2)
        self.assertIn("not " + other, result.stderr)
        run("--out", self.dir / "y", "--commit", "ABC", "--allow-dirty", expect=2)
        run("--out", self.dir / "z", "--commit", current, "--allow-dirty")
        self.assertEqual(json.loads((self.dir / "z/manifest.json").read_text())["rsi_commit"], current)

    def test_the_settings_source_list_is_what_the_desktop_generator_reads(self):
        # Dropped in slice D2 together with desktop/ (#3453): the client then owns this check.
        mjs = ROOT / SETTINGS_MJS
        if not mjs.is_file():
            self.skipTest("desktop/ is gone: the client repository owns this check")
        text = mjs.read_text()
        body = text[text.index("export function sourcePaths"):text.index("// ---------- Rust text helpers")]
        read = set()
        for call in re.findall(r"join\((tui|repo)((?:, '[^']+')+)\)", body):
            base = "crates/rsi/src" if call[0] == "tui" else ""
            parts = re.findall(r"'([^']+)'", call[1])
            read.add("/".join(([base] if base else []) + parts))
        read.discard("crates/rsi/src")
        self.assertEqual(set(EXPORT.SETTINGS_SOURCES), read)
        # The resolver half: the exporter's modules are exactly what the const resolver opens.
        self.assertEqual(set(EXPORT.settings_sources(ROOT)) - read, set(EXPORT.resolver_modules(ROOT)) - read)
        for rel in INPUTS:
            if rel.startswith("crates/rsi-common/src/") and rel not in read and rel not in CLIENT_INPUTS:
                self.assertIn(rel, EXPORT.resolver_modules(ROOT))

    @unittest.skipUnless(shutil.which("node"), "node is not installed")
    def test_the_desktop_settings_model_is_identical_from_the_exported_sources(self):
        mjs = ROOT / SETTINGS_MJS
        if not mjs.is_file():
            self.skipTest("desktop/ is gone: the client repository owns this check")
        out = self.dir / "c"
        run("--out", out, "--allow-dirty")
        script = (
            f"import {{buildModel, sourcePaths}} from {json.dumps(mjs.as_uri())};"
            + "const a = JSON.stringify(buildModel(sourcePaths(process.argv[2])));"
            + "const b = JSON.stringify(buildModel(sourcePaths(process.argv[3])));"
            + "if (a !== b) { console.error('models differ'); process.exit(1); } console.log(a.length);"
        )
        result = subprocess.run(
            ["node", "--input-type=module", "-e", script, "-", str(out / "settings/sources"), str(ROOT)],
            capture_output=True, text=True,
        )
        self.assertEqual(result.returncode, 0, result.stderr)


class SyntheticTree(Scratch):
    def setUp(self):
        super().setUp()
        self.src = self.dir / "src"
        for rel in EXPORT.VERBATIM_DOCS:
            self.write(self.src, rel, f"# {rel}\n")
        for rel in EXPORT.SETTINGS_SOURCES:
            self.write(self.src, rel, f"// {rel}\n")
        self.write(self.src, "crates/rsi/src/settings.rs",
                   "Self { field: rsi_common::foo::BAR.to_string(),\n"
                   "  options: rsi_common::foo::Kind::ALL\n    .iter() }\n"
                   "options: rsi_common::lst::CHOICES\n    .iter()\n")
        self.write(self.src, "crates/rsi/src/overlay/daemon_text_editor.rs",
                   'pub const NAME_FIELD: &str = rsi_common::deep::FIELD;\npub const LIT: &str = "x";\n')
        self.write(self.src, "crates/rsi-common/src/foo.rs", "pub const BAR: &str = \"bar\";\n")
        self.write(self.src, "crates/rsi-common/src/lst.rs", "// list\n")
        self.write(self.src, "crates/rsi-common/src/deep/mod.rs", "pub const FIELD: &str = \"f\";\n")
        self.write(self.src, "crates/rsi-common/src/unrelated.rs", "// not read\n")
        self.write(self.src, "crates/rsi/src/lib.rs", 'const A: &str = "ListSessions"; const B: &str = "Abc"; const C: &str = "lower";\n')
        self.write(self.src, "crates/rsi/src/ui/zeta.rs", 'x("SendMessageNow"); x("ListSessions");\n')
        self.write(self.src, "crates/rsi/src/ui/alpha.rs", "\n")
        self.write(self.src, "crates/rsi/src/ui/mod.rs", "\n")
        self.write(self.src, "crates/rsi/src/ui/tests.rs", "\n")
        self.write(self.src, "crates/rsi/src/ui/overlay/beta.rs", "\n")
        self.write(self.src, "crates/rsi/src/ui/notes.txt", "x\n")
        self.write(self.src, "rust-toolchain.toml", '[toolchain]\n# channel = "ignored"\nchannel = "9.8.7"\nprofile = "minimal"\n')
        self.write(self.src, "crates/rsi-common/src/calls.rs",
                   'pub const METHOD_LIST: &str = "ListThings";\npub const METHOD_PUT: &str = "PutThing";\n')
        self.write(self.src, "crates/rsi/src/client.rs", CLIENT_RS)

    def export(self, *extra, expect=0, out="out"):
        return run("--root", self.src, "--out", self.dir / out, "--commit", "a" * 40, *extra, expect=expect)

    def test_modules_ops_and_resolver_modules(self):
        self.export()
        out = self.dir / "out"
        inventory = json.loads((out / "tui-inventory.json").read_text())
        self.assertEqual(inventory["modules"], [
            "crates/rsi/src/ui/alpha.rs", "crates/rsi/src/ui/overlay/beta.rs", "crates/rsi/src/ui/theme.rs",
            "crates/rsi/src/ui/theme_roles.rs", "crates/rsi/src/ui/zeta.rs"])
        self.assertEqual(inventory["tui_ops"], ["GetSession", "ListSessions", "SendMessageNow"])
        self.assertEqual(inventory["client_methods"], ["GetSession", "ListSessions", "ListThings", "PutThing"])
        self.assertIn("crates/rsi/src/client.rs", inventory["tui_files"])
        self.assertIn("crates/rsi/src/ui/mod.rs", inventory["tui_files"])
        self.assertNotIn("crates/rsi/src/ui/notes.txt", inventory["tui_files"])
        sources = {p for p in tree(out) if p.startswith("settings/sources/crates/rsi-common/")}
        self.assertEqual(sources, {
            "settings/sources/crates/rsi-common/src/daemon_config_catalog.rs",
            "settings/sources/crates/rsi-common/src/foo.rs",
            "settings/sources/crates/rsi-common/src/deep/mod.rs",
            "settings/sources/crates/rsi-common/src/lst.rs",
        })
        manifest = json.loads((out / "manifest.json").read_text())
        self.assertEqual(manifest["rsi_commit"], "a" * 40)
        self.assertEqual(manifest["rust_toolchain"], "9.8.7")

    def test_an_unresolvable_client_call_is_a_named_failure(self):
        for call, needle in (
            ("request(method, Value::Null)", "request(method)"),
            ("request(NO_IMPORT, Value::Null)", "no `use rsi_common"),
            ("request(rsi_common::calls::MISSING, Value::Null)", "no `pub const MISSING"),
            ("request(rsi_common::nomod::METHOD_X, Value::Null)", "no module file"),
        ):
            self.write(self.src, "crates/rsi/src/client.rs", CLIENT_RS + f"fn b() {{ self.{call}; }}\n")
            self.assertIn(needle, self.export(expect=2).stderr)

    def test_a_missing_or_channel_less_toolchain_file_is_a_failure(self):
        self.write(self.src, "rust-toolchain.toml", "[toolchain]\n")
        self.assertIn("has no channel", self.export(expect=2).stderr)
        (self.src / "rust-toolchain.toml").unlink()
        self.assertIn("contract input is missing: rust-toolchain.toml", self.export(expect=2).stderr)

    def test_check_detects_a_toolchain_bump_and_a_new_client_method(self):
        self.export()
        out = self.dir / "out"
        args = ("--root", self.src, "--check", out, "--commit", "a" * 40)
        run(*args)
        self.write(self.src, "rust-toolchain.toml", '[toolchain]\nchannel = "9.8.8"\n')
        self.assertIn("changed: manifest.json", run(*args, expect=1).stderr)
        self.write(self.src, "rust-toolchain.toml", '[toolchain]\nchannel = "9.8.7"\n')
        run(*args)
        self.write(self.src, "crates/rsi/src/client.rs", CLIENT_RS + 'fn b() { self.request("NewMethod", v); }\n')
        self.assertIn("changed: tui-inventory.json", run(*args, expect=1).stderr)

    def test_a_missing_input_is_a_named_failure(self):
        (self.src / "docs/keybindings.md").unlink()
        result = self.export(expect=2)
        self.assertIn("contract input is missing: docs/keybindings.md", result.stderr)

    def test_a_field_const_with_no_module_file_is_a_failure_but_a_list_const_is_not(self):
        (self.src / "crates/rsi-common/src/foo.rs").unlink()
        result = self.export(expect=2)
        self.assertIn("rsi_common::foo::BAR", result.stderr)

    def test_a_reexport_drops_files_the_old_manifest_listed_and_keeps_strays(self):
        self.export()
        out = self.dir / "out"
        (self.src / "crates/rsi-common/src/lst.rs").unlink()
        self.write(self.src, "crates/rsi/src/settings.rs", "// no resolver reads\n")
        (out / "stray.txt").write_text("keep")
        self.export()
        files = tree(out)
        self.assertNotIn("settings/sources/crates/rsi-common/src/lst.rs", files)
        self.assertNotIn("settings/sources/crates/rsi-common/src/foo.rs", files)
        self.assertIn("stray.txt", files)

    def test_a_non_contract_directory_is_never_written_into(self):
        (self.dir / "other").mkdir()
        (self.dir / "other" / "precious.txt").write_text("keep")
        result = self.export(out="other", expect=2)
        self.assertIn("refusing to write", result.stderr)
        self.assertEqual((self.dir / "other/precious.txt").read_text(), "keep")

    def test_uncommitted_inputs_are_refused_unless_allowed(self):
        scratch_git(self.src, "init", "-q")
        scratch_git(self.src, "add", "-A")
        scratch_git(self.src, *GIT_IDENTITY, "commit", "-q", "-m", "inputs")
        commit = scratch_git(self.src, "rev-parse", "HEAD").stdout.strip()
        run("--root", self.src, "--out", self.dir / "clean")
        self.assertEqual(json.loads((self.dir / "clean/manifest.json").read_text())["rsi_commit"], commit)
        run("--root", self.src, "--check", self.dir / "clean")
        self.write(self.src, "docs/keybindings.md", "# edited\n")
        result = run("--root", self.src, "--out", self.dir / "dirty", expect=2)
        self.assertIn("uncommitted changes", result.stderr)
        run("--root", self.src, "--out", self.dir / "dirty", "--allow-dirty")
        # An unrelated untracked file outside the inputs does not count as dirty.
        self.write(self.src, "docs/keybindings.md", "# docs/keybindings.md\n")
        self.write(self.src, "notes.txt", "x")
        run("--root", self.src, "--out", self.dir / "ok")


if __name__ == "__main__":
    unittest.main()
