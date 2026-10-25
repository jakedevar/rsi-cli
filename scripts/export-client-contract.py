#!/usr/bin/env python3
"""Export the rsi client contract (v1) that an out-of-tree client vendors (#3453).

The desktop client lives in its own repository and consumes rsi through two
channels at one pinned rsi commit: Cargo git dependencies for the Rust types, and
this generated folder for everything else it used to read from the rsi tree.
rsi owns the exporter, so a rename in rsi that drops a contract input fails rsi's
own gate (scripts/tests/test_export_client_contract.py), not the client's bump.

    scripts/export-client-contract.py --out DIR [--commit SHA]
    scripts/export-client-contract.py --check DIR [--commit SHA]

Layout of DIR (plan: thoughts/shared/plans/2026-10-21-3453-desktop-own-repo.md):

    manifest.json              contract_version, rsi_commit, rust_toolchain, exporter,
                               files{rel: sha256:hex}
    docs/keybindings.md        verbatim
    docs/manual/rsi-manual.md  verbatim
    tui-inventory.json         modules, tui_files (every crates/rsi/src/**/*.rs),
                               tui_ops, client_methods (the rsid methods
                               crates/rsi/src/client.rs calls, constants resolved)
    settings/sources/<repo-relative path>   verbatim Rust sources the settings
                               generator parses, plus every rsi-common module its
                               const resolver reads

The output is deterministic: the same tree gives the same bytes. Standard library
only. `--commit` defaults to the checkout's HEAD and must equal it; inputs with
uncommitted changes are refused (`--allow-dirty` to override) so the manifest
never names a commit whose bytes were not exported.
"""

import argparse
import hashlib
import json
import os
from pathlib import Path
import re
import subprocess
import sys

CONTRACT_VERSION = 1
EXPORTER = "scripts/export-client-contract.py"
MANIFEST = "manifest.json"
TUI_SRC = "crates/rsi/src"
COMMON_SRC = "crates/rsi-common/src"
CLIENT_RS = "crates/rsi/src/client.rs"
TOOLCHAIN_FILE = "rust-toolchain.toml"
SETTINGS_PREFIX = "settings/sources/"

VERBATIM_DOCS = (
    "docs/keybindings.md",
    "docs/manual/rsi-manual.md",
)

# desktop/scripts/settings.mjs `sourcePaths`: the files the settings generator parses.
SETTINGS_SOURCES = (
    "crates/rsi/src/settings_registry.rs",
    "crates/rsi/src/settings.rs",
    "crates/rsi/src/ui/theme.rs",
    "crates/rsi/src/ui/theme_roles.rs",
    "crates/rsi/src/overlay/daemon_text_editor.rs",
    "crates/rsi-common/src/daemon_config_catalog.rs",
)

# Where the generator's `commonConstResolver` is called: `settings.rs`
# (DaemonFeatureEntry::defaults) for a field name and an options list, and
# `daemon_text_editor.rs` for the text-editor field consts.
FIELD_CONST = re.compile(r"field:\s*(rsi_common(?:::\w+)+)\.to_string\(\)")
OPTIONS_CONST = re.compile(r"options:\s*(rsi_common(?:::\w+)+)\s*\.iter\(\)")
EDITOR_CONST = re.compile(r"pub const \w+: &str = (rsi_common(?:::\w+)+);")
RESOLVER_READERS = (
    ("crates/rsi/src/settings.rs", (FIELD_CONST, OPTIONS_CONST)),
    ("crates/rsi/src/overlay/daemon_text_editor.rs", (EDITOR_CONST,)),
)

QUOTED_OP = re.compile(r'"([A-Z][A-Za-z0-9]{3,})"')

# `.request(<method>, ...)` in client.rs: a string literal or a (possibly bare) const.
REQUEST_CALL = re.compile(r"\brequest\(\s*([^,)]*?)\s*[,)]")
METHOD_LITERAL = re.compile(r'"(\w+)"')
METHOD_CONST = re.compile(r"(?:\w+::)*[A-Z][A-Z0-9_]*")
USE_COMMON = re.compile(r"\buse\s+rsi_common::([\w:]*?)(?:::)?(\{[^}]*\}|\w+)\s*;")
CHANNEL = re.compile(r'^channel\s*=\s*"([^"]+)"', re.MULTILINE)
SHA = re.compile(r"[0-9a-f]{40}")


class ExportError(Exception):
    pass


def sha256(data):
    return "sha256:" + hashlib.sha256(data).hexdigest()


def dump_json(value):
    return (json.dumps(value, indent=2, sort_keys=True) + "\n").encode("utf-8")


def read_input(root, rel):
    path = root / rel
    if not path.is_file():
        raise ExportError(f"contract input is missing: {rel}")
    return path.read_bytes()


def rust_files(root, rel):
    """Every `*.rs` regular file under `root/rel`, repo-relative, bytewise sorted."""
    base = root / rel
    if not base.is_dir():
        raise ExportError(f"contract input is missing: {rel}")
    found = []
    for directory, subdirs, names in os.walk(base):
        subdirs[:] = [name for name in subdirs if name not in ("node_modules", "target")]
        for name in names:
            path = Path(directory) / name
            if name.endswith(".rs") and path.is_file():
                found.append(path.relative_to(root).as_posix())
    return sorted(found)


def tui_modules(root):
    """`ui/*.rs` and `ui/overlay/*.rs` minus mod.rs and tests.rs (= parity.mjs listModules)."""
    modules = []
    for sub in (f"{TUI_SRC}/ui", f"{TUI_SRC}/ui/overlay"):
        directory = root / sub
        if not directory.is_dir():
            continue
        for path in directory.iterdir():
            if path.name.endswith(".rs") and path.name not in ("mod.rs", "tests.rs") and path.is_file():
                modules.append(f"{sub}/{path.name}")
    return sorted(modules)


def tui_inventory(root):
    files = rust_files(root, TUI_SRC)
    ops = set()
    for rel in files:
        text = (root / rel).read_bytes().decode("utf-8", errors="replace")
        ops.update(QUOTED_OP.findall(text))
    return {
        "client_methods": client_calls(root)[0],
        "modules": tui_modules(root),
        "tui_files": files,
        "tui_ops": sorted(ops),
    }


def common_const(root, mod_parts, name):
    """(module file, string value) of `pub const NAME: &str = "...";` in `rsi_common::<mod_parts>`."""
    mod = "/".join(mod_parts)
    hits = [f"{COMMON_SRC}/{mod}.rs", f"{COMMON_SRC}/{mod}/mod.rs"] if mod else [f"{COMMON_SRC}/lib.rs"]
    hit = next((h for h in hits if (root / h).is_file()), None)
    if hit is None:
        raise ExportError(f"{CLIENT_RS} names rsi_common::{'::'.join([*mod_parts, name])}: no module file under {COMMON_SRC}")
    text = (root / hit).read_bytes().decode("utf-8", errors="replace")
    found = re.search(rf'\bpub const {re.escape(name)}\s*:\s*&(?:\'static\s+)?str\s*=\s*"([^"]*)"\s*;', text)
    if found is None:
        raise ExportError(f"{CLIENT_RS} names {name}: no `pub const {name}: &str = \"...\";` in {hit}")
    return hit, found.group(1)


def client_calls(root):
    """(rsid methods client.rs calls, the rsi-common module files their constants live in).

    Reads `request(<method>, ...)`: a literal is the method, a const path resolves
    through its rsi-common module, and a bare const through the `use rsi_common::...`
    that imports it. A call this cannot resolve fails the export rather than dropping
    the method from the audit's list.
    """
    text = read_input(root, CLIENT_RS).decode("utf-8", errors="replace")
    imports = {}
    for prefix, items in USE_COMMON.findall(text):
        parts = [p for p in prefix.split("::") if p]
        for item in re.findall(r"\w+", items):
            imports[item] = parts
    methods, modules = set(), set()
    for arg in REQUEST_CALL.findall(text):
        if arg in ("", "&mut self"):
            continue
        literal = METHOD_LITERAL.fullmatch(arg)
        if literal:
            methods.add(literal.group(1))
        elif METHOD_CONST.fullmatch(arg):
            parts = arg.split("::")
            if len(parts) == 1:
                if parts[0] not in imports:
                    raise ExportError(f"{CLIENT_RS} calls request({arg}) but no `use rsi_common::...` imports it")
                parts = ["rsi_common", *imports[parts[0]], parts[0]]
            if parts[0] != "rsi_common":
                raise ExportError(f"{CLIENT_RS} calls request({arg}): only rsi_common constants resolve")
            module, value = common_const(root, parts[1:-1], parts[-1])
            methods.add(value)
            modules.add(module)
        else:
            raise ExportError(f"{CLIENT_RS} calls request({arg}): not a literal or an rsi_common constant")
    return sorted(methods), sorted(modules)


def rust_toolchain(root):
    """The `channel` of rust-toolchain.toml, so the client pins the same compiler."""
    found = CHANNEL.search(read_input(root, TOOLCHAIN_FILE).decode("utf-8", errors="replace"))
    if found is None:
        raise ExportError(f"{TOOLCHAIN_FILE} has no channel")
    return found.group(1)


def resolver_modules(root):
    """The rsi-common module files the settings generator's const resolver reads.

    Mirrors `commonConstResolver`: `rsi_common::<mod path>::CONST` reads
    `<mod path>.rs` or `<mod path>/mod.rs`. A const list that names no module file
    (for example `ProviderProfile::ALL`) resolves to nothing, as in the generator.
    """
    modules = set()
    for rel, patterns in RESOLVER_READERS:
        text = read_input(root, rel).decode("utf-8", errors="replace")
        for pattern in patterns:
            for path in pattern.findall(text):
                parts = path.split("::")
                if len(parts) < 3:
                    raise ExportError(f"cannot resolve {path} named in {rel}")
                mod = "/".join(parts[1:-1])
                hits = [f"{COMMON_SRC}/{mod}.rs", f"{COMMON_SRC}/{mod}/mod.rs"]
                hit = next((h for h in hits if (root / h).is_file()), None)
                if hit is not None:
                    modules.add(hit)
                elif pattern is not OPTIONS_CONST:
                    raise ExportError(f"{path} named in {rel} has no module file under {COMMON_SRC}")
    return sorted(modules)


def settings_sources(root):
    return sorted(set(SETTINGS_SOURCES) | set(resolver_modules(root)))


def build_files(root):
    """The contract payload: relative path -> bytes, without the manifest."""
    files = {}
    for rel in VERBATIM_DOCS:
        files[rel] = read_input(root, rel)
    files["tui-inventory.json"] = dump_json(tui_inventory(root))
    for rel in settings_sources(root):
        files[SETTINGS_PREFIX + rel] = read_input(root, rel)
    return files


def manifest_bytes(files, commit, toolchain):
    return dump_json({
        "contract_version": CONTRACT_VERSION,
        "rsi_commit": commit,
        "rust_toolchain": toolchain,
        "exporter": EXPORTER,
        "files": {rel: sha256(files[rel]) for rel in sorted(files)},
    })


def git(root, *args):
    try:
        result = subprocess.run(["git", *args], cwd=root, capture_output=True, text=True)
    except OSError:
        return None
    return result.stdout.strip() if result.returncode == 0 else None


def input_paths(root):
    return [
        *VERBATIM_DOCS, TOOLCHAIN_FILE, TUI_SRC, *SETTINGS_SOURCES,
        *resolver_modules(root), *client_calls(root)[1],
    ]


def resolve_commit(root, requested, allow_dirty):
    head = git(root, "rev-parse", "HEAD")
    if requested is not None and not SHA.fullmatch(requested):
        raise ExportError(f"--commit must be a lowercase 40-hex commit id, got {requested!r}")
    if requested is None:
        if head is None:
            raise ExportError("not a git checkout: pass --commit <sha>")
        commit = head
    else:
        commit = requested
        if head is not None and head != commit:
            raise ExportError(f"the checkout is at {head}, not {commit}: export from a checkout of that commit")
    if head is not None and not allow_dirty:
        dirty = git(root, "status", "--porcelain", "--untracked-files=all", "--", *input_paths(root))
        if dirty:
            raise ExportError("contract inputs have uncommitted changes (--allow-dirty to export anyway):\n" + dirty)
    return commit


def write_contract(out, files, manifest):
    previous = out / MANIFEST
    stale = []
    if out.exists():
        if not out.is_dir():
            raise ExportError(f"--out is not a directory: {out}")
        if previous.is_file():
            try:
                old = json.loads(previous.read_text(encoding="utf-8"))
                stale = [rel for rel in old.get("files", {}) if rel not in files]
            except (ValueError, AttributeError) as error:
                raise ExportError(f"{previous} is not a contract manifest: {error}")
        elif any(out.iterdir()):
            raise ExportError(f"--out {out} is not empty and holds no {MANIFEST}: refusing to write into it")
    for rel in stale:
        target = out / rel
        if target.is_file() and out.resolve() in target.resolve().parents:
            target.unlink()
            parent = target.parent
            while parent != out and not any(parent.iterdir()):
                parent.rmdir()
                parent = parent.parent
    for rel, data in {**files, MANIFEST: manifest}.items():
        target = out / rel
        target.parent.mkdir(parents=True, exist_ok=True)
        target.write_bytes(data)


def contract_files(directory):
    found = {}
    for path in sorted(directory.rglob("*")):
        if path.is_file():
            found[path.relative_to(directory).as_posix()] = path.read_bytes()
    return found


def drift(directory, files, manifest):
    """Human-readable differences between `directory` and the freshly built contract."""
    if not directory.is_dir():
        return [f"{directory}: not a directory"]
    expected = {**files, MANIFEST: manifest}
    actual = contract_files(directory)
    problems = []
    for rel in sorted(expected.keys() | actual.keys()):
        if rel not in actual:
            problems.append(f"missing: {rel}")
        elif rel not in expected:
            problems.append(f"unlisted: {rel}")
        elif actual[rel] != expected[rel]:
            problems.append(f"changed: {rel}")
    return problems


def main(argv=None):
    parser = argparse.ArgumentParser(description="Export or check the rsi client contract (v1).")
    mode = parser.add_mutually_exclusive_group(required=True)
    mode.add_argument("--out", type=Path, help="write the contract into this directory")
    mode.add_argument("--check", type=Path, help="exit 1 when this directory differs from a fresh export")
    parser.add_argument("--commit", help="the rsi commit the contract is pinned to (default: HEAD)")
    parser.add_argument("--root", type=Path, default=Path(__file__).resolve().parents[1], help=argparse.SUPPRESS)
    parser.add_argument("--allow-dirty", action="store_true", help="export inputs with uncommitted changes")
    args = parser.parse_args(argv)
    root = args.root.resolve()
    try:
        commit = resolve_commit(root, args.commit, args.allow_dirty)
        files = build_files(root)
        manifest = manifest_bytes(files, commit, rust_toolchain(root))
        if args.out is not None:
            write_contract(args.out, files, manifest)
            print(f"wrote {len(files) + 1} files to {args.out} (rsi_commit {commit})")
            return 0
        problems = drift(args.check, files, manifest)
    except ExportError as error:
        print(f"export-client-contract: {error}", file=sys.stderr)
        return 2
    if problems:
        print(f"export-client-contract: {args.check} drifted from rsi {commit}:", file=sys.stderr)
        for problem in problems:
            print(f"  {problem}", file=sys.stderr)
        return 1
    print(f"{args.check} matches rsi {commit}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
