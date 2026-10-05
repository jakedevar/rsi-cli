#!/usr/bin/env python3
"""Derive and check rsid's unit-test partition before a shard is compiled.

Each library test's source gate declares its shard. A normal direct
``cargo test -p rsid --lib`` still sees every test because the gates are
disabled unless a shard feature enables ``test-shard-mode``.

The library tests live in two packages since the #1021 S4 crate split: ``rsid``
and ``rsid-store``. A shard is one feature name declared by every package that
has tests in it; the gate runs the shard across those packages
(``--list-shard-packages``).
"""

from __future__ import annotations

import argparse
from collections import Counter
import json
from pathlib import Path
import re
import sys
import tomllib


ROOT = Path(__file__).resolve().parent.parent
# Package name -> crate directory. Order is the order `-p` flags are emitted in.
CRATES = {"rsid": ROOT / "crates/rsid", "rsid-store": ROOT / "crates/rsid-store"}
SOURCES = [directory / "src" for directory in CRATES.values()]
SHARD_KIND_ORDER = ("store", "session", "memory", "other")
# These source tests are kept in the inventory but may be absent from the
# default Linux, no-default-features runtime list because of enclosing cfgs.
CONDITIONAL_RUNTIME_NAMES = {
    "store-01": {
        "v100_exact_v99_sources_converge_without_title_or_lineage_data_loss",
        "v100_failpoints_roll_back_both_exact_v99_sources_and_retry",
        "v100_unknown_or_hybrid_v99_catalog_fails_closed",
    },  # enclosing cfg(any()) historical fixture
    "session-03": {"proc_map_device_identity_rejects_oversized_macos_components"},
    "other-02": {
        "recursive_dag_smoke_fixture_can_opt_into_live_dogfood_graph",
        "recursive_dag_smoke_fixture_can_upgrade_marked_fixture_with_live_dogfood_graph",
        "recursive_dag_smoke_fixture_copies_source_without_mutating_it",
        "recursive_dag_smoke_fixture_generates_store_readbacks_and_is_idempotent",
        "recursive_dag_smoke_fixture_refuses_default_db_path",
        "recursive_dag_smoke_fixture_refuses_existing_unmarked_output_db",
        "recursive_dag_smoke_fixture_rehomes_copied_existing_fixture_summary",
    },  # enclosing dev-fixtures feature
    "other-03": {"refuses_reclaim_without_linux_openat2_containment"},
}
TARGET_OS_GATE = re.compile(r'#\[cfg\((?:all\(test, )?(not\()?target_os = "([a-z]+)"\)?\)?\)\]')
HOST_TARGET_OS = {"darwin": "macos", "linux": "linux"}.get(sys.platform, sys.platform)
# Tests whose `#[cfg(target_os = ...)]` excludes this host: present in the static
# manifest, absent at runtime here. Filled by `source_inventory`.
PLATFORM_CONDITIONAL_NAMES: dict[str, Counter] = {}
MOD_DECL = re.compile(r"^[ \t]*(?:pub(?:\([^)]*\))?\s+)?mod\s+([A-Za-z_][A-Za-z_0-9]*)\s*;")


def host_excluded(gate_line: str) -> bool:
    """Whether a `#[cfg([not(]target_os = "...")[)]]` line excludes this host."""
    stripped = gate_line.strip()
    gate = TARGET_OS_GATE.fullmatch(stripped)
    if not gate or stripped.count("(") != stripped.count(")"):
        return False
    return (gate.group(2) == HOST_TARGET_OS) == bool(gate.group(1))


MOD_BLOCK = re.compile(r"^([ \t]*)(?:pub(?:\([^)]*\))?\s+)?mod\s+[A-Za-z_][A-Za-z_0-9]*\s*\{\s*$")


def host_excluded_line_ranges(lines: list[str]) -> list[tuple[int, int]]:
    """1-based line ranges of inline `mod name { ... }` blocks gated to another OS."""
    ranges = []
    for index, line in enumerate(lines):
        block = MOD_BLOCK.match(line)
        if not block or index == 0 or not host_excluded(lines[index - 1]):
            continue
        closing = block.group(1) + "}"
        end = next((j for j in range(index + 1, len(lines)) if lines[j].rstrip() == closing), None)
        if end is None:
            fail(f"cannot find the end of an OS-gated module at line {index + 1}")
        ranges.append((index + 1, end + 1))
    return ranges


def host_excluded_module_files() -> set[str]:
    """Source files whose `mod name;` declaration is gated to another OS."""
    excluded: set[str] = set()
    for path in sorted(path for source in SOURCES for path in source.rglob("*.rs")):
        lines = path.read_text(encoding="utf-8").splitlines()
        for line in lines:
            stripped = line.strip()
            if stripped.startswith("#![") and host_excluded(stripped.replace("#![", "#[", 1)):
                excluded.add(path.relative_to(ROOT).as_posix())
            elif stripped and not stripped.startswith(("//", "#![")):
                break
        for index, line in enumerate(lines):
            declared = MOD_DECL.match(line)
            if not declared or index == 0 or not host_excluded(lines[index - 1]):
                continue
            name = declared.group(1)
            module_dir = path.parent if path.name in ("mod.rs", "lib.rs", "main.rs") else path.with_suffix("")
            matches = [
                candidate
                for candidate in (*module_dir.rglob(f"{name}.rs"), *module_dir.rglob(f"{name}/mod.rs"))
            ]
            if len(matches) != 1:
                fail(f"{path.relative_to(ROOT).as_posix()}:{index + 1}: cannot resolve OS-gated module {name}")
            excluded.add(matches[0].relative_to(ROOT).as_posix())
    return excluded


ATTR = re.compile(r"^[ \t]*#\[\s*(?:test|tokio::test(?:\([^]\n]*\))?)\s*\]", re.M)
FUNCTION = re.compile(r"\b(?:async\s+)?fn\s+([A-Za-z_][A-Za-z_0-9]*)\s*\(")
CHAR = re.compile(r"'(?:\\(?:x[0-9a-fA-F]{2}|u\{[0-9a-fA-F_]+\}|.)|[^\\'\n])'")
SHARD_GATE = re.compile(
    r'#\[cfg\(any\(not\(feature = "test-shard-mode"\), feature = "test-shard-([a-z]+-[0-9]+)"\)\)\]'
)


def fail(message: str) -> None:
    raise ValueError(message)


def mask_literals_and_comments(source: str, path: str) -> str:
    """Keep positions/newlines while hiding non-code Rust text."""
    masked = list(source)
    length = len(source)

    def hide(start: int, end: int) -> None:
        for position in range(start, end):
            if source[position] != "\n":
                masked[position] = " "

    position = 0
    while position < length:
        if source.startswith("//", position):
            end = source.find("\n", position)
            end = length if end < 0 else end
            hide(position, end)
            position = end
            continue
        if source.startswith("/*", position):
            depth = 1
            end = position + 2
            while end < length and depth:
                if source.startswith("/*", end):
                    depth += 1
                    end += 2
                elif source.startswith("*/", end):
                    depth -= 1
                    end += 2
                else:
                    end += 1
            if depth:
                fail(f"{path}: unterminated block comment")
            hide(position, end)
            position = end
            continue
        prefix = 0
        if source.startswith(("br", "rb"), position):
            prefix = 2
        elif source[position] == "r":
            prefix = 1
        if prefix and (position == 0 or not (source[position - 1].isalnum() or source[position - 1] == "_")):
            quote = position + prefix
            while quote < length and source[quote] == "#":
                quote += 1
            if quote < length and source[quote] == '"':
                terminator = '"' + "#" * (quote - position - prefix)
                end = source.find(terminator, quote + 1)
                if end < 0:
                    fail(f"{path}: unterminated raw string")
                end += len(terminator)
                hide(position, end)
                position = end
                continue
        if source[position] == '"':
            end = position + 1
            while end < length:
                if source[end] == "\\":
                    end += 2
                elif source[end] == '"':
                    end += 1
                    break
                else:
                    end += 1
            else:
                fail(f"{path}: unterminated string")
            hide(position, end)
            position = end
            continue
        if source[position] == "'":
            match = CHAR.match(source, position)
            if match:
                hide(position, match.end())
                position = match.end()
                continue
        position += 1
    return "".join(masked)


def extract_tests(path: Path) -> list[tuple[str, int, bool]]:
    relative = path.relative_to(ROOT).as_posix()
    source = path.read_text(encoding="utf-8")
    code = mask_literals_and_comments(source, relative)
    raw_lines = source.splitlines()
    code_lines = code.splitlines()
    if len(raw_lines) != len(code_lines):
        fail(f"{relative}: scanner lost line alignment")
    identities = []
    for index, line in enumerate(code_lines):
        if not ATTR.match(line):
            continue
        name = None
        for following in code_lines[index + 1 : index + 16]:
            if ATTR.match(following):
                break
            match = FUNCTION.search(following)
            if match:
                name = match.group(1)
                break
        if name is None:
            fail(f"{relative}:{index + 1}: test attribute has no following function within 15 lines")
        gated = index > 0 and raw_lines[index - 1].lstrip().startswith(
            '#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-'
        )
        identities.append((name, index + 1, gated))
    names = [name for name, _, _ in identities]
    if len(names) != len(set(names)):
        fail(f"{relative}: duplicate test function names in one source file")
    return identities


def read_crate_shards(package: str) -> list[str]:
    cargo = tomllib.loads((CRATES[package] / "Cargo.toml").read_text(encoding="utf-8"))
    features = cargo["features"]
    if features.get("default") != [] or features.get("test-shard-mode") != []:
        fail(f"{package} default/test-shard-mode features changed")
    shards = []
    for feature, dependencies in features.items():
        if not feature.startswith("test-shard-") or feature == "test-shard-mode":
            continue
        shard = feature.removeprefix("test-shard-")
        if not re.fullmatch(r"[a-z]+-[0-9]+", shard):
            fail(f"invalid shard feature {feature}")
        if dependencies != ["test-shard-mode"]:
            fail(f"{feature} must imply only test-shard-mode")
        shards.append(shard)
    if not shards:
        fail(f"{package} declares no library shard features")
    return shards


def shard_sort_key(shard: str) -> tuple[int, int]:
    kind, number = shard.rsplit("-", 1)
    order = SHARD_KIND_ORDER.index(kind) if kind in SHARD_KIND_ORDER else len(SHARD_KIND_ORDER)
    return order, int(number)


def read_shard_packages() -> dict[str, list[str]]:
    """Shard name -> packages (in `CRATES` order) that declare its feature."""
    packages: dict[str, list[str]] = {}
    for package in CRATES:
        for shard in read_crate_shards(package):
            packages.setdefault(shard, []).append(package)
    return dict(sorted(packages.items(), key=lambda item: shard_sort_key(item[0])))


def read_shards() -> list[str]:
    return list(read_shard_packages())


def package_of(relative: str) -> str:
    for package, directory in CRATES.items():
        if relative.startswith(directory.relative_to(ROOT).as_posix() + "/"):
            return package
    fail(f"{relative}: not under a sharded rsid package")


def source_inventory(
    shard_packages: dict[str, list[str]], require_gates: bool
) -> tuple[dict[tuple[str, str], str], int]:
    """Assign tests from their source gates, with bin tests in a separate lane."""
    manifest: dict[tuple[str, str], str] = {}
    file_shards: dict[str, str] = {}
    ungated: list[tuple[str, str, int]] = []
    gated = 0
    excluded_files = host_excluded_module_files()
    for path in sorted(path for source in SOURCES for path in source.rglob("*.rs")):
        relative = path.relative_to(ROOT).as_posix()
        package = package_of(relative)
        is_bin = relative == "crates/rsid/src/main.rs" or relative.startswith("crates/rsid/src/bin/")
        raw_lines = path.read_text(encoding="utf-8").splitlines()
        excluded_ranges = host_excluded_line_ranges(raw_lines)
        for name, line, has_gate in extract_tests(path):
            identity = (relative, name)
            previous = raw_lines[line - 2] if line > 1 else ""
            if is_bin:
                if has_gate or "test-shard-" in previous:
                    fail(f"{relative}:{line}: binary test has a library shard gate")
                manifest[identity] = "bin"
                continue
            match = SHARD_GATE.fullmatch(previous.strip())
            if match:
                shard = match.group(1)
                if shard not in shard_packages:
                    fail(f"{relative}:{line}: unknown shard {shard}")
                if package not in shard_packages[shard]:
                    fail(f"{relative}:{line}: shard {shard} is not a {package} feature")
                if previous[: len(previous) - len(previous.lstrip())] != raw_lines[line - 1][: len(raw_lines[line - 1]) - len(raw_lines[line - 1].lstrip())]:
                    fail(f"{relative}:{line}: incorrect shard gate indentation")
                before_gate = raw_lines[line - 3] if line > 2 else ""
                if before_gate.lstrip().startswith("#[cfg(") and "test-shard-" in before_gate:
                    fail(f"{relative}:{line}: duplicate shard gates")
                if (
                    host_excluded(before_gate)
                    or relative in excluded_files
                    or any(start < line < end for start, end in excluded_ranges)
                ):
                    PLATFORM_CONDITIONAL_NAMES.setdefault(shard, Counter())[name] += 1
                if relative in file_shards and file_shards[relative] != shard:
                    fail(f"{relative}:{line}: one source file spans shards")
                file_shards[relative] = shard
                manifest[identity] = shard
                gated += 1
            elif "test-shard-" in previous:
                fail(f"{relative}:{line}: incorrect shard gate")
            elif require_gates:
                fail(f"{relative}:{line}: ungated test blocks a bounded lib harness")
            else:
                ungated.append((relative, name, line))
    for relative, name, line in ungated:
        shard = file_shards.get(relative)
        if shard is None:
            fail(f"{relative}:{line}: ungated test has no declared file shard")
        manifest[(relative, name)] = shard
    return manifest, gated


def check_runtime(
    directory: Path, manifest: dict[tuple[str, str], str], shard_packages: dict[str, list[str]], selected: list[str]
) -> None:
    seen: set[str] = set()
    total = 0
    conditional_absent = 0
    counts = Counter(manifest.values())
    for shard in selected:
        packages = shard_packages[shard]
        expected_count = counts[shard]
        path = directory / f"{shard}.json"
        data = json.loads(path.read_text(encoding="utf-8"))
        suites = data.get("rust-suites")
        if not isinstance(suites, dict) or sorted(suites) != sorted(packages):
            fail(f"{path}: expected one library suite for each of {packages}")
        names = []
        optional = Counter(CONDITIONAL_RUNTIME_NAMES.get(shard, ())) | PLATFORM_CONDITIONAL_NAMES.get(
            shard, Counter()
        )
        shard_expected = Counter(name for (source, name), assigned in manifest.items() if assigned == shard)
        if optional - shard_expected:
            fail(f"{path}: conditional runtime exception is absent from the static manifest")
        shard_missing = 0
        for binary_id, suite in suites.items():
            if suite.get("binary-id") != binary_id or not isinstance(suite.get("testcases"), dict):
                fail(f"{path}: malformed Nextest library suite")
            suite_names = []
            for runtime_name, metadata in suite["testcases"].items():
                match = metadata.get("filter-match", {})
                ignored = metadata.get("ignored")
                selected_case = match.get("status") == "matches" or (
                    ignored is True and match == {"status": "mismatch", "reason": "ignored"}
                )
                if not selected_case or not isinstance(ignored, bool):
                    fail(f"{path}: filtered or malformed test {runtime_name}")
                identity = f"{binary_id}::{runtime_name}"
                if identity in seen:
                    fail(f"runtime test appears in multiple shards: {identity}")
                seen.add(identity)
                suite_names.append(runtime_name.rsplit("::", 1)[-1])
            expected = Counter(
                name
                for (source, name), assigned in manifest.items()
                if assigned == shard and package_of(source) == binary_id
            )
            actual = Counter(suite_names)
            missing = expected - actual
            unexpected_missing = missing - optional
            extra = actual - expected
            if unexpected_missing or extra:
                fail(f"{path}: runtime/static test identities differ in {binary_id}: missing={list(unexpected_missing.elements())[:10]}, extra={list(extra.elements())[:10]}")
            shard_missing += sum(missing.values())
            names.extend(suite_names)
        conditional_absent += shard_missing
        if len(names) != expected_count - shard_missing:
            fail(f"{path}: expected {expected_count - shard_missing} runtime tests, observed {len(names)}")
        total += len(names)
    if total != sum(counts[shard] for shard in selected) - conditional_absent:
        fail(f"runtime union count differs: {total}")
    print(f"runtime union: {total} distinct rsid library tests in {len(selected)} shard(s); {conditional_absent} conditional source tests absent")


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--require-gates", action="store_true", help="reject any ungated library test")
    parser.add_argument("--list-shards", action="store_true", help="print shard names for the runner")
    parser.add_argument(
        "--list-shard-packages",
        action="store_true",
        help="print `shard package...` lines: the packages whose tests a shard run must select",
    )
    parser.add_argument("--runtime-dir", type=Path, help="check one Nextest JSON list per shard")
    parser.add_argument("--runtime-shard", help="check one selected shard")
    args = parser.parse_args()
    if args.runtime_shard and args.runtime_dir is None:
        parser.error("--runtime-shard requires --runtime-dir")
    try:
        shard_packages = read_shard_packages()
        shards = list(shard_packages)
        if args.runtime_shard and args.runtime_shard not in shards:
            fail(f"unknown runtime shard {args.runtime_shard}")
        manifest, gated = source_inventory(shard_packages, args.require_gates or args.runtime_dir is not None)
        populated = {(package_of(source), assigned) for (source, _), assigned in manifest.items()}
        for shard, packages in shard_packages.items():
            for package in packages:
                if (package, shard) not in populated:
                    fail(f"{package} declares shard {shard} but has no test in it")
        bin_count = sum(shard == "bin" for shard in manifest.values())
        total = len(manifest)
        if args.runtime_dir is not None:
            check_runtime(
                args.runtime_dir, manifest, shard_packages, [args.runtime_shard] if args.runtime_shard else shards
            )
        if args.list_shard_packages:
            print("\n".join(f"{shard} {' '.join(packages)}" for shard, packages in shard_packages.items()))
        elif args.list_shards:
            print("\n".join(shards))
        else:
            print(f"static inventory: {total} test identities ({total - bin_count} library, {bin_count} binary); {gated}/{total - bin_count} library gates present")
    except (OSError, ValueError, KeyError, TypeError, json.JSONDecodeError, tomllib.TOMLDecodeError) as error:
        print(f"rsid test shard check failed: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
