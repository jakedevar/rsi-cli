#!/usr/bin/env python3
"""Reject retroactive edits to released SQLite migrations.

Each schema version lives in its own file, ``crates/rsid/src/store/migrations/
vNNN.rs``; ``build.rs`` collects them in order and derives
``LATEST_SCHEMA_VERSION`` (the highest file).  The committed manifest pins
every released ``if version < N`` block plus explicitly marked catalog/helper
regions.  Against a base revision, existing pins are append-only: changing
source and refreshing its pin still fails.  Revisions that predate the split
(every block inline in ``store/mod.rs``) still validate: blocks are collected
from ``mod.rs`` and the migrations directory alike.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import re
import subprocess
import sys
from pathlib import Path
from typing import Any


MIGRATION_PATH = "crates/rsid/src/store/mod.rs"
MIGRATION_DIR = "crates/rsid/src/store/migrations"
MANIFEST_PATH = "tools/released-migrations.json"
LATEST_RE = re.compile(r"pub const LATEST_SCHEMA_VERSION: i32 = (\d+);")
MIGRATION_FILE_RE = re.compile(r"^v(?P<version>\d+)\.rs$")
# Manifest keys that name where migrations live; they describe the layout, not
# the pinned content, so they never take part in a comparison.
LAYOUT_KEYS = ("migration_file", "migration_dir")
BLOCK_RE = re.compile(r"^(?P<indent>\s*)if version < (?P<version>\d+) \{\s*$")
BEGIN_RE = re.compile(r"^\s*// RSI-RELEASED-MIGRATION-BEGIN: (?P<name>[a-z0-9-]+)\s*$")
END_RE = re.compile(r"^\s*// RSI-RELEASED-MIGRATION-END: (?P<name>[a-z0-9-]+)\s*$")


class GuardError(RuntimeError):
    pass


def digest(value: str) -> str:
    return "sha256:" + hashlib.sha256(value.encode()).hexdigest()


def git_show(ref: str, path: str) -> str | None:
    result = subprocess.run(
        ["git", "show", f"{ref}:{path}"],
        check=False,
        stdout=subprocess.PIPE,
        stderr=subprocess.DEVNULL,
        text=True,
    )
    return result.stdout if result.returncode == 0 else None


def read_revision(ref: str, path: str) -> str:
    value = git_show(ref, path)
    if value is None:
        raise GuardError(f"{path} is missing at {ref}")
    return value


def migration_file_version(path: str) -> int | None:
    """The schema version named by ``migrations/vNNN.rs``, else ``None``."""
    directory, _, name = path.rpartition("/")
    if directory != MIGRATION_DIR:
        return None
    match = MIGRATION_FILE_RE.match(name)
    if match is None:
        return None
    version = int(match.group("version"))
    if name != f"v{version:03d}.rs":
        raise GuardError(f"migration file {path} must be named v{version:03d}.rs")
    return version


def latest_schema_version(files: dict[str, str]) -> int:
    """Head version: the highest migration file, or the legacy constant."""
    file_versions = [
        version
        for version in (migration_file_version(path) for path in files)
        if version is not None
    ]
    if file_versions:
        return max(file_versions)
    match = LATEST_RE.search(files.get(MIGRATION_PATH, ""))
    if match is None:
        raise GuardError(f"cannot find the schema head in {MIGRATION_DIR} or {MIGRATION_PATH}")
    return int(match.group(1))


def v0_region(lines: list[str]) -> str | None:
    """The released V0 region, or ``None`` when this file does not hold it.

    It runs from the ``V0`` comment to the next version comment (inline
    layout) or to the step function's closing ``Ok(())`` (per-file layout).
    """
    start = next((i for i, line in enumerate(lines) if "// V0: Original schema" in line), None)
    if start is None:
        return None
    end = next(
        (
            i
            for i in range(start + 1, len(lines))
            if "// V1: Session metadata columns" in lines[i]
            or lines[i].rstrip("\r\n") == "        Ok(())"
        ),
        None,
    )
    if end is None:
        raise GuardError("cannot locate the released V0 migration region")
    return "".join(lines[start:end])


def migration_blocks(files: dict[str, str]) -> dict[str, str]:
    blocks: dict[str, str] = {}
    for path in sorted(files):
        file_version = migration_file_version(path)
        if file_version is None and path != MIGRATION_PATH:
            continue
        lines = files[path].splitlines(keepends=True)
        region = v0_region(lines)
        if region is not None:
            if "0" in blocks:
                raise GuardError("duplicate migration block for V0")
            if file_version not in (None, 0):
                raise GuardError(f"V0 region belongs in v000.rs, found in {path}")
            blocks["0"] = region

        for start, line in enumerate(lines):
            match = BLOCK_RE.match(line.rstrip("\n"))
            if match is None:
                continue
            version = match.group("version")
            if version in blocks:
                raise GuardError(f"duplicate migration block for V{version}")
            if file_version is not None and int(version) != file_version:
                raise GuardError(f"migration block V{version} found in {path}")
            closing = match.group("indent") + "}"
            end = next(
                (i for i in range(start + 1, len(lines)) if lines[i].rstrip("\r\n") == closing),
                None,
            )
            if end is None:
                raise GuardError(f"unterminated migration block for V{version}")
            blocks[version] = "".join(lines[start : end + 1])
    if "0" not in blocks:
        raise GuardError("cannot locate the released V0 migration region")
    return blocks


def protected_sections(files: dict[str, str]) -> dict[str, dict[str, str]]:
    sections: dict[str, dict[str, str]] = {}
    for path, source in files.items():
        lines = source.splitlines(keepends=True)
        open_sections: dict[str, int] = {}
        for index, line in enumerate(lines):
            begin = BEGIN_RE.match(line.rstrip("\r\n"))
            if begin:
                name = begin.group("name")
                if name in sections or name in open_sections:
                    raise GuardError(f"duplicate protected migration section {name}")
                open_sections[name] = index
                continue
            end = END_RE.match(line.rstrip("\r\n"))
            if end:
                name = end.group("name")
                start = open_sections.pop(name, None)
                if start is None:
                    raise GuardError(f"unmatched protected migration section end {name}")
                sections[name] = {
                    "path": path,
                    "sha256": digest("".join(lines[start : index + 1])),
                }
        if open_sections:
            names = ", ".join(sorted(open_sections))
            raise GuardError(f"unterminated protected migration section(s): {names}")
    return sections


def inventory(files: dict[str, str]) -> dict[str, Any]:
    latest = latest_schema_version(files)
    blocks = migration_blocks(files)
    released = {
        version: digest(body)
        for version, body in sorted(blocks.items(), key=lambda item: int(item[0]))
        if int(version) <= latest
    }
    if not released or max(map(int, released)) != latest:
        raise GuardError(f"no migration block found for schema head V{latest}")
    layout = (
        {"migration_dir": MIGRATION_DIR}
        if any(migration_file_version(path) is not None for path in files)
        else {"migration_file": MIGRATION_PATH}
    )
    return {
        "schema_version": 1,
        "latest_schema_version": latest,
        **layout,
        "blocks": released,
        "protected_sections": protected_sections(files),
    }


def tracked_source_paths(manifest: dict[str, Any] | None = None) -> list[str]:
    paths = {MIGRATION_PATH, "crates/rsid/src/store/cohort_settlement.rs"}
    if manifest is None:
        paths.add("crates/rsid/src/store/manager_prepared_actions.rs")
    if manifest:
        paths.update(section["path"] for section in manifest.get("protected_sections", {}).values())
    return sorted(paths)


def worktree_migration_files() -> list[str]:
    directory = Path(MIGRATION_DIR)
    if not directory.is_dir():
        return []
    return sorted(
        f"{MIGRATION_DIR}/{entry.name}"
        for entry in directory.iterdir()
        if entry.is_file() and entry.name.endswith(".rs")
    )


def revision_migration_files(ref: str) -> list[str]:
    result = subprocess.run(
        ["git", "ls-tree", "-r", "--name-only", ref, "--", f"{MIGRATION_DIR}/"],
        check=False,
        stdout=subprocess.PIPE,
        stderr=subprocess.DEVNULL,
        text=True,
    )
    return sorted(result.stdout.splitlines()) if result.returncode == 0 else []


def load_worktree_files(
    manifest: dict[str, Any] | None = None, extra_paths: list[str] | None = None
) -> dict[str, str]:
    paths = set(tracked_source_paths(manifest))
    paths.update(worktree_migration_files())
    paths.update(extra_paths or [])
    return {path: Path(path).read_text() for path in sorted(paths)}


def marked_source_paths(extra_paths: list[str] | None = None) -> set[str]:
    result = subprocess.run(
        # Pins live in Rust sources only; docs and test fixtures quote markers.
        ["git", "grep", "--untracked", "-l", "-e", "RSI-RELEASED-MIGRATION-BEGIN", "--", ":(glob)crates/**/*.rs"],
        check=False,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
    )
    if result.returncode not in (0, 1):
        raise GuardError(result.stderr.strip() or "git grep failed while finding marked sources")
    paths = set(result.stdout.splitlines())
    paths.update(extra_paths or [])
    return {path for path in paths if Path(path).is_file()}


def refreshed_manifest(existing: dict[str, Any], scanned: dict[str, Any]) -> dict[str, Any]:
    existing_blocks = existing["blocks"]
    existing_sections = existing.get("protected_sections", {})
    actual_blocks = scanned["blocks"]
    actual_sections = scanned.get("protected_sections", {})

    removed = [version for version in existing_blocks if version not in actual_blocks]
    changed = [
        version
        for version in existing_blocks
        if version in actual_blocks and actual_blocks[version] != existing_blocks[version]
    ]
    removed_sections = [name for name in existing_sections if name not in actual_sections]
    changed_sections = [
        name
        for name in existing_sections
        if name in actual_sections
        and actual_sections[name]["sha256"] != existing_sections[name]["sha256"]
    ]
    if removed or changed or removed_sections or changed_sections:
        details = []
        if removed:
            versions = ", ".join(f"V{version}" for version in removed)
            details.append("removed migration blocks " + versions)
        if changed:
            versions = ", ".join(f"V{version}" for version in changed)
            details.append("changed migration blocks " + versions)
        if removed_sections:
            details.append("removed protected sections " + ", ".join(removed_sections))
        if changed_sections:
            details.append("changed protected sections " + ", ".join(changed_sections))
        raise GuardError("; ".join(details))

    result = {key: value for key, value in existing.items() if key not in LAYOUT_KEYS}
    result.update({key: scanned[key] for key in LAYOUT_KEYS if key in scanned})
    result["latest_schema_version"] = scanned["latest_schema_version"]
    result["blocks"] = {
        **existing_blocks,
        **{
            version: fingerprint
            for version, fingerprint in actual_blocks.items()
            if version not in existing_blocks
        },
    }
    # A pinned section may move to another file; its digest may not change.
    result["protected_sections"] = {
        **{name: actual_sections[name] for name in existing_sections},
        **{
            name: section
            for name, section in actual_sections.items()
            if name not in existing_sections
        },
    }
    # Keep the layout key beside the head version, as `inventory` writes it.
    ordered = {key: result[key] for key in ("schema_version", "latest_schema_version") if key in result}
    ordered.update({key: result[key] for key in LAYOUT_KEYS if key in result})
    ordered.update({key: value for key, value in result.items() if key not in ordered})
    return ordered


def load_revision_files(ref: str, manifest: dict[str, Any]) -> dict[str, str]:
    paths = set(tracked_source_paths(manifest))
    paths.update(revision_migration_files(ref))
    files: dict[str, str] = {}
    for path in sorted(paths):
        text = git_show(ref, path)
        if text is None:
            if path == MIGRATION_PATH:
                raise GuardError(f"{path} is missing at {ref}")
            continue
        files[path] = text
    return files


def load_manifest_text(value: str, label: str) -> dict[str, Any]:
    try:
        manifest = json.loads(value)
    except json.JSONDecodeError as error:
        raise GuardError(f"invalid {label}: {error}") from error
    if manifest.get("schema_version") != 1:
        raise GuardError(f"unsupported {label} schema_version")
    return manifest


def without_layout(manifest: dict[str, Any]) -> dict[str, Any]:
    return {key: value for key, value in manifest.items() if key not in LAYOUT_KEYS}


def validate_manifest(manifest: dict[str, Any], actual: dict[str, Any], label: str) -> None:
    if without_layout(manifest) != without_layout(actual):
        expected_blocks = manifest.get("blocks", {})
        actual_blocks = actual.get("blocks", {})
        changed = sorted(
            version
            for version in set(expected_blocks) | set(actual_blocks)
            if expected_blocks.get(version) != actual_blocks.get(version)
        )
        expected_sections = manifest.get("protected_sections", {})
        actual_sections = actual.get("protected_sections", {})
        section_changes = sorted(
            name
            for name in set(expected_sections) | set(actual_sections)
            if expected_sections.get(name) != actual_sections.get(name)
        )
        details = []
        if changed:
            details.append("migration blocks " + ", ".join(f"V{version}" for version in changed))
        if section_changes:
            details.append("protected sections " + ", ".join(section_changes))
        if manifest.get("latest_schema_version") != actual.get("latest_schema_version"):
            details.append("LATEST_SCHEMA_VERSION")
        suffix = "; ".join(details) or "manifest metadata"
        raise GuardError(f"{label} does not match its source: {suffix}")


def validate_append_only(base: dict[str, Any], head: dict[str, Any]) -> None:
    base_latest = int(base["latest_schema_version"])
    head_latest = int(head["latest_schema_version"])
    if head_latest < base_latest:
        raise GuardError(f"schema head regressed from V{base_latest} to V{head_latest}")

    for version, fingerprint in base["blocks"].items():
        if head["blocks"].get(version) != fingerprint:
            raise GuardError(f"released migration block V{version} changed relative to base")
    for name, section in base.get("protected_sections", {}).items():
        # Only the digest is pinned; the section may move between files.
        head_section = head.get("protected_sections", {}).get(name)
        if head_section is None or head_section["sha256"] != section["sha256"]:
            raise GuardError(f"released migration section {name} changed relative to base")

    new_versions = sorted(set(head["blocks"]) - set(base["blocks"]), key=int)
    if head_latest == base_latest and new_versions:
        raise GuardError("new migration blocks require a LATEST_SCHEMA_VERSION bump")
    invalid_new = [version for version in new_versions if int(version) <= base_latest]
    if invalid_new:
        raise GuardError("previously released migration pins may not be added retroactively")
    if head_latest > base_latest:
        expected = [str(version) for version in range(base_latest + 1, head_latest + 1)]
        if new_versions != expected:
            raise GuardError(
                "schema version bump must append one pinned migration block for every new version"
            )


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("base", nargs="?", help="base git revision (default: HEAD^)")
    parser.add_argument("head", nargs="?", default="HEAD", help="head git revision")
    parser.add_argument(
        "--refresh",
        action="store_true",
        help="rewrite the worktree manifest from current protected source",
    )
    parser.add_argument(
        "--include-path",
        action="append",
        default=[],
        help="include a new migration helper file when refreshing (repeatable)",
    )
    args = parser.parse_args()
    if args.include_path and not args.refresh:
        parser.error("--include-path requires --refresh")

    try:
        if args.refresh:
            manifest_file = Path(MANIFEST_PATH)
            if not manifest_file.exists():
                # Bootstrap: nothing is released yet, so write the full inventory.
                actual = inventory(load_worktree_files(extra_paths=args.include_path))
                manifest_file.write_text(json.dumps(actual, indent=2) + "\n")
                print(f"refreshed {MANIFEST_PATH} through V{actual['latest_schema_version']}")
                return 0
            existing_text = manifest_file.read_text()
            existing = load_manifest_text(existing_text, MANIFEST_PATH)
            paths = tracked_source_paths(existing)
            paths.extend(marked_source_paths(args.include_path))
            actual = inventory(load_worktree_files(existing, extra_paths=paths))
            updated = refreshed_manifest(existing, actual)
            updated_text = json.dumps(updated, indent=2) + "\n"
            if updated_text != existing_text:
                Path(MANIFEST_PATH).write_text(updated_text)
            print(f"refreshed {MANIFEST_PATH} through V{updated['latest_schema_version']}")
            return 0

        head_text = read_revision(args.head, MANIFEST_PATH)
        head_manifest = load_manifest_text(head_text, f"{args.head}:{MANIFEST_PATH}")
        head_actual = inventory(load_revision_files(args.head, head_manifest))
        validate_manifest(head_manifest, head_actual, "head migration manifest")

        base_ref = args.base or f"{args.head}^"
        base_text = git_show(base_ref, MANIFEST_PATH)
        if base_text is None:
            print(
                f"released-migration guard: bootstrap manifest valid through "
                f"V{head_manifest['latest_schema_version']} (base has no manifest)"
            )
            return 0
        base_manifest = load_manifest_text(base_text, f"{base_ref}:{MANIFEST_PATH}")
        base_actual = inventory(load_revision_files(base_ref, base_manifest))
        validate_manifest(base_manifest, base_actual, "base migration manifest")
        validate_append_only(base_manifest, head_manifest)
        print(
            f"released-migration guard: PASS {base_ref}..{args.head}; "
            f"V0-V{base_manifest['latest_schema_version']} immutable, "
            f"head V{head_manifest['latest_schema_version']}"
        )
        return 0
    except (GuardError, OSError, subprocess.SubprocessError) as error:
        print(f"released-migration guard: FAIL: {error}", file=sys.stderr)
        print(
            "Released migration DDL, catalog projections, and fingerprints are immutable. "
            "Never repair them in place; add a forward migration.",
            file=sys.stderr,
        )
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
