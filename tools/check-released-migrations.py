#!/usr/bin/env python3
"""Reject retroactive edits to released SQLite migrations.

The committed manifest pins every released ``if version < N`` block plus
explicitly marked catalog/helper regions.  Against a base revision, existing
pins are append-only: changing source and refreshing its pin still fails.
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
MANIFEST_PATH = "tools/released-migrations.json"
LATEST_RE = re.compile(r"pub const LATEST_SCHEMA_VERSION: i32 = (\d+);")
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


def latest_schema_version(source: str) -> int:
    match = LATEST_RE.search(source)
    if match is None:
        raise GuardError(f"cannot find LATEST_SCHEMA_VERSION in {MIGRATION_PATH}")
    return int(match.group(1))


def migration_blocks(source: str) -> dict[str, str]:
    lines = source.splitlines(keepends=True)
    blocks: dict[str, str] = {}

    v0_start = next((i for i, line in enumerate(lines) if "// V0: Original schema" in line), None)
    v1_start = next(
        (i for i, line in enumerate(lines) if "// V1: Session metadata columns" in line), None
    )
    if v0_start is None or v1_start is None or v1_start <= v0_start:
        raise GuardError("cannot locate the released V0 migration region")
    blocks["0"] = "".join(lines[v0_start:v1_start])

    for start, line in enumerate(lines):
        match = BLOCK_RE.match(line.rstrip("\n"))
        if match is None:
            continue
        version = match.group("version")
        if version in blocks:
            raise GuardError(f"duplicate migration block for V{version}")
        closing = match.group("indent") + "}"
        end = next(
            (i for i in range(start + 1, len(lines)) if lines[i].rstrip("\r\n") == closing),
            None,
        )
        if end is None:
            raise GuardError(f"unterminated migration block for V{version}")
        blocks[version] = "".join(lines[start : end + 1])
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
    migration_source = files[MIGRATION_PATH]
    latest = latest_schema_version(migration_source)
    blocks = migration_blocks(migration_source)
    released = {
        version: digest(body)
        for version, body in sorted(blocks.items(), key=lambda item: int(item[0]))
        if int(version) <= latest
    }
    if not released or max(map(int, released)) != latest:
        raise GuardError(f"no migration block found for schema head V{latest}")
    return {
        "schema_version": 1,
        "latest_schema_version": latest,
        "migration_file": MIGRATION_PATH,
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


def load_worktree_files(
    manifest: dict[str, Any] | None = None, extra_paths: list[str] | None = None
) -> dict[str, str]:
    paths = set(tracked_source_paths(manifest))
    paths.update(extra_paths or [])
    return {path: Path(path).read_text() for path in sorted(paths)}


def load_revision_files(ref: str, manifest: dict[str, Any]) -> dict[str, str]:
    return {path: read_revision(ref, path) for path in tracked_source_paths(manifest)}


def load_manifest_text(value: str, label: str) -> dict[str, Any]:
    try:
        manifest = json.loads(value)
    except json.JSONDecodeError as error:
        raise GuardError(f"invalid {label}: {error}") from error
    if manifest.get("schema_version") != 1:
        raise GuardError(f"unsupported {label} schema_version")
    return manifest


def validate_manifest(manifest: dict[str, Any], actual: dict[str, Any], label: str) -> None:
    if manifest != actual:
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
        if head.get("protected_sections", {}).get(name) != section:
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
            actual = inventory(load_worktree_files(extra_paths=args.include_path))
            Path(MANIFEST_PATH).write_text(json.dumps(actual, indent=2) + "\n")
            print(f"refreshed {MANIFEST_PATH} through V{actual['latest_schema_version']}")
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
