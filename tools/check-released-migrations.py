#!/usr/bin/env python3
"""Reject retroactive edits to released SQLite migrations.

Each schema version lives in its own file, ``crates/rsid-store/src/store/
migrations/vNNN.rs`` (``crates/rsid/src/store/migrations`` before the #1021 S4
crate split; both layouts validate); ``build.rs`` collects them in order and
derives ``LATEST_SCHEMA_VERSION`` (the highest file).  The committed manifest pins
every released ``if version < N`` block plus explicitly marked catalog/helper
regions.  Against a base revision, existing pins are append-only: changing
source and refreshing its pin still fails.  Revisions that predate the split
(every block inline in ``store/mod.rs``) still validate: blocks are collected
from ``mod.rs`` and the migrations directory alike.

The pin file is derived (#1949): the lander runs ``--refresh`` against the
rolling tip on its candidate and commits the result at publication, so a source
may leave the file at the base's content; the check then reports
``pin deferred to the lander`` instead of failing.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import re
import subprocess
import sys
from pathlib import Path
from typing import Any, Iterable


# The store moved from `crates/rsid` to `crates/rsid-store` (issue #1021 S4);
# revisions older than that keep the first layout, so both are recognised.
STORE_ROOTS = ("crates/rsid-store/src/store", "crates/rsid/src/store")
MIGRATION_PATHS = tuple(f"{root}/mod.rs" for root in STORE_ROOTS)
MIGRATION_DIRS = tuple(f"{root}/migrations" for root in STORE_ROOTS)
# The current layout, where new migrations are written.
MIGRATION_PATH = MIGRATION_PATHS[0]
MIGRATION_DIR = MIGRATION_DIRS[0]
MANIFEST_PATH = "tools/released-migrations.json"
DECLARATIONS_DIR = "tools/provisional-migrations"
MAX_PENDING_PREDECESSORS = 8
LATEST_RE = re.compile(r"pub const LATEST_SCHEMA_VERSION: i32 = (\d+);")
REWIND_TEARDOWN_RE = re.compile(r"const REWIND_TEARDOWN_COVERED_THROUGH: i32 = (\d+);")
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
    if directory not in MIGRATION_DIRS:
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
    match = LATEST_RE.search(next((files[path] for path in MIGRATION_PATHS if path in files), ""))
    if match is None:
        raise GuardError(f"cannot find the schema head in {MIGRATION_DIR} or {MIGRATION_PATH}")
    return int(match.group(1))


REWIND_TESTS_PATH = "crates/rsid-store/src/store/tests.rs"
ALTERED_OBJECTS_PATH = "crates/rsid-store/src/store/migrations/altered_objects.rs"


def validate_rewind_teardown_matches_schema(files: dict[str, str], tests_text: str | None) -> None:
    """#2018: REWIND_TEARDOWN_COVERED_THROUGH in store/tests.rs equals the schema head.

    ``tests_text`` is that file's text. A tree without the file or without the
    constant (hermetic fixtures, older revisions) skips the check;
    test_the_rewind_constant_is_found_in_this_checkout keeps the pattern honest.
    """
    match = REWIND_TEARDOWN_RE.search(tests_text or "")
    if match is None:
        return
    schema_head = latest_schema_version(files)
    covered = int(match.group(1))
    if covered != schema_head:
        raise GuardError(
            f"REWIND_TEARDOWN_COVERED_THROUGH ({covered}) does not match the schema head (V{schema_head}). "
            f"Set it to {schema_head} in {REWIND_TESTS_PATH} and add the V{schema_head} "
            "rewind step (`active >= " + str(schema_head) + "`) in rewind_post_v121_tail_to."
        )


# #2064: SQL a migration uses to change an object an older migration created.
ALTER_TABLE_RE = re.compile(
    r"\bALTER\s+TABLE\s+[\"`\[]?(?P<name>\w+)[\"`\]]?\s+"
    r"(?:RENAME\s+TO\s+[\"`\[]?(?P<target>\w+)[\"`\]]?)?",
    re.IGNORECASE,
)
DROP_RE = re.compile(
    r"\bDROP\s+(?:TABLE|INDEX|TRIGGER|VIEW)\s+(?:IF\s+EXISTS\s+)?[\"`\[]?(?P<name>\w+)",
    re.IGNORECASE,
)
CREATE_RE = re.compile(
    r"\bCREATE\s+(?:UNIQUE\s+)?(?:TEMP(?:ORARY)?\s+)?(?:TABLE|INDEX|TRIGGER|VIEW)\s+"
    r"(?:IF\s+NOT\s+EXISTS\s+)?[\"`\[]?(?P<name>\w+)",
    re.IGNORECASE,
)
REBUILD_CALL_RE = re.compile(r"\brebuild_\w+\s*\(")
ALTERED_ENTRY_RE = re.compile(r"\(\s*(?P<version>\d+)\s*,\s*&\[(?P<names>[^\]]*)\]\s*,?\s*\)")
ALTERED_ENTRY_START_RE = re.compile(r"\(\s*\d+\s*,")
ALTERED_TABLE_RE = re.compile(
    r"MIGRATION_ALTERED_OBJECTS:\s*&\[\(i32,\s*&\[&str\]\)\]\s*=\s*&\[(?P<body>.*?)\n\];",
    re.DOTALL,
)


def altered_objects_table(text: str) -> dict[int, set[str]]:
    """``MIGRATION_ALTERED_OBJECTS`` (migrations/altered_objects.rs) by version."""
    table = ALTERED_TABLE_RE.search(text)
    if table is None:
        raise GuardError(f"cannot find MIGRATION_ALTERED_OBJECTS in {ALTERED_OBJECTS_PATH}")
    body_lines = table.group("body").splitlines()
    body = "\n".join(line for line in body_lines if not line.lstrip().startswith("//"))
    parsed_starts = {m.start() for m in ALTERED_ENTRY_RE.finditer(body)}
    for start in ALTERED_ENTRY_START_RE.finditer(body):
        if start.start() not in parsed_starts:
            line_no = body[: start.start()].count("\n") + 1
            shown = body.splitlines()[line_no - 1].strip()
            raise GuardError(
                f"unparseable MIGRATION_ALTERED_OBJECTS entry in {ALTERED_OBJECTS_PATH} "
                f"(table body line {line_no}): `{shown}`. Expected "
                '`(version, &["object", ...]),` (rustfmt multiline and a trailing comma are fine).'
            )
    entries: dict[int, set[str]] = {}
    for entry in ALTERED_ENTRY_RE.finditer(body):
        names = set(re.findall(r'"([^"]+)"', entry.group("names")))
        entries.setdefault(int(entry.group("version")), set()).update(names)
    return entries


def altered_by_migration(text: str) -> tuple[set[str], bool]:
    """Existing objects a migration file's SQL alters, and whether it calls a rebuild helper.

    Comments are ignored. An object the same file creates is a scratch object
    (``t_new``) and is not required, except as the target of a rename.
    """
    code = "\n".join(line for line in text.splitlines() if not line.lstrip().startswith("//"))
    created = {m.group("name").lower() for m in CREATE_RE.finditer(code)}
    required: set[str] = set()
    for match in ALTER_TABLE_RE.finditer(code):
        name, target = match.group("name"), match.group("target")
        if target:
            required.add(target)
            if name.lower() not in created:
                required.add(name)
        else:
            required.add(name)
    for match in DROP_RE.finditer(code):
        if match.group("name").lower() not in created:
            required.add(match.group("name"))
    return required, REBUILD_CALL_RE.search(code) is not None


def validate_altered_objects(
    files: dict[str, str], released_through: int | None, table_text: str | None
) -> None:
    """#2064: a NEW migration that alters an existing object lists it in the table.

    ``released_through`` is the base's schema head: only migration files above
    it are scanned (released ones are immutable and already covered by the
    table). A tree without the table file (hermetic fixtures, older revisions)
    skips the check; test_the_altered_objects_table_is_found_in_this_checkout
    keeps the pattern honest.
    """
    if released_through is None or table_text is None:
        return
    listed = altered_objects_table(table_text)
    for path in sorted(files):
        version = migration_file_version(path)
        if version is None or version <= released_through:
            continue
        required, rebuilds = altered_by_migration(files[path])
        have = listed.get(version, set())
        missing = sorted(name for name in required if name not in have)
        if not missing and not (rebuilds and not have):
            continue
        names = sorted(have | set(missing))
        quoted = ", ".join(f'"{name}"' for name in names) if names else '"<object the rebuild rewrites>"'
        reason = (
            f"V{version} ({path}) alters existing objects: {', '.join(missing)}"
            if missing
            else f"V{version} ({path}) calls a rebuild_* helper"
        )
        raise GuardError(
            f"{reason}, but MIGRATION_ALTERED_OBJECTS has no entry for it. Add "
            f"`({version}, &[{quoted}]),` to {ALTERED_OBJECTS_PATH}. Catalog-pin tests "
            "then accept the changed SQL of exactly those objects. This check is "
            "conservative (it matches ALTER TABLE, DROP and rebuild_* calls in the migration "
            "file, ignoring objects the file itself creates): a false refusal costs one line, "
            "a missed entry costs a merge-queue gate round."
        )


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
        if file_version is None and path not in MIGRATION_PATHS:
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
    layouts = sorted(
        {path.rpartition("/")[0] for path in files if migration_file_version(path) is not None}
    )
    if len(layouts) > 1:
        # build.rs collects only `MIGRATION_DIR`; a unit in the other directory
        # would pass every number/manifest check yet never run (#1021 S4).
        raise GuardError(
            "migration files exist in more than one store layout "
            f"({', '.join(layouts)}); only {MIGRATION_DIR} is built, move them there"
        )
    latest = latest_schema_version(files)
    blocks = migration_blocks(files)
    released = {
        version: digest(body)
        for version, body in sorted(blocks.items(), key=lambda item: int(item[0]))
        if int(version) <= latest
    }
    if not released or max(map(int, released)) != latest:
        raise GuardError(f"no migration block found for schema head V{latest}")
    migration_dirs = sorted(
        {path.rpartition("/")[0] for path in files if migration_file_version(path) is not None}
    )
    layout = (
        {"migration_dir": migration_dirs[0]}
        if migration_dirs
        else {"migration_file": next((path for path in MIGRATION_PATHS if path in files), MIGRATION_PATH)}
    )
    return {
        "schema_version": 1,
        "latest_schema_version": latest,
        **layout,
        "blocks": released,
        "protected_sections": protected_sections(files),
    }


def manifest_store_root(manifest: dict[str, Any] | None) -> str | None:
    """The store directory a manifest's layout key names, when it is a known one."""
    layout = (manifest or {}).get("migration_dir") or (manifest or {}).get("migration_file")
    root = layout.rpartition("/")[0] if isinstance(layout, str) else None
    return root if root in STORE_ROOTS else None


def tracked_source_paths(manifest: dict[str, Any] | None = None) -> list[str]:
    # The manifest's own layout when it names one; otherwise both layouts (the
    # loaders skip the one a revision does not have).
    root = manifest_store_root(manifest)
    roots = (root,) if root else STORE_ROOTS
    paths = {f"{store}/mod.rs" for store in roots}
    paths.update(f"{store}/cohort_settlement.rs" for store in roots)
    if manifest is None:
        paths.update(f"{store}/manager_prepared_actions.rs" for store in roots)
    if manifest:
        paths.update(section["path"] for section in manifest.get("protected_sections", {}).values())
    return sorted(paths)


def worktree_migration_files() -> list[str]:
    return sorted(
        f"{migration_dir}/{entry.name}"
        for migration_dir in MIGRATION_DIRS
        if Path(migration_dir).is_dir()
        for entry in Path(migration_dir).iterdir()
        if entry.is_file() and entry.name.endswith(".rs")
    )


def revision_migration_files(ref: str) -> list[str]:
    result = subprocess.run(
        ["git", "ls-tree", "-r", "--name-only", ref, "--", *(f"{directory}/" for directory in MIGRATION_DIRS)],
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
    return {path: Path(path).read_text() for path in sorted(paths) if Path(path).is_file()}


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
            continue
        files[path] = text
    if not any(path in files for path in MIGRATION_PATHS):
        raise GuardError(f"{MIGRATION_PATH} is missing at {ref}")
    return files


def has_conflict_markers(text: str) -> bool:
    return any(line.startswith(("<<<<<<< ", ">>>>>>> ")) for line in text.splitlines())


def merge_conflicted_manifest(path: str) -> dict[str, Any]:
    """Union the two sides of a conflicted manifest (index stages 2 and 3).

    The manifest is derived, never hand-edited: a merge that conflicts on it
    only has to keep every pin either side holds.  Pins that disagree on one
    version are a real conflict and stay an error; `--refresh` then adds the
    pins for the worktree's new versions from source.
    """
    sides = []
    for stage in (2, 3):
        text = git_show(f":{stage}", path)
        if text is None:
            raise GuardError(f"{path} has conflict markers but no merge stages; resolve the merge first")
        sides.append(load_manifest_text(text, f"{path} (stage {stage})"))
    ours, theirs = sides
    merged = dict(ours)
    blocks = dict(ours["blocks"])
    for version, fingerprint in theirs["blocks"].items():
        if blocks.setdefault(version, fingerprint) != fingerprint:
            raise GuardError(f"merged manifests pin different content for V{version}")
    merged["blocks"] = {version: blocks[version] for version in sorted(blocks, key=int)}
    sections = dict(ours.get("protected_sections", {}))
    for name, section in theirs.get("protected_sections", {}).items():
        if sections.setdefault(name, section)["sha256"] != section["sha256"]:
            raise GuardError(f"merged manifests pin different content for section {name}")
    merged["protected_sections"] = sections
    merged["latest_schema_version"] = max(
        int(ours["latest_schema_version"]), int(theirs["latest_schema_version"])
    )
    return merged


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


def deferred_pin(
    base_text: str | None,
    base_ref: str,
    head_manifest: dict[str, Any],
    head_actual: dict[str, Any],
) -> dict[str, Any] | None:
    """The pin the lander will derive, when the head left the pin file at the base's.

    ``None`` when the head's manifest differs from the base's (a hand edit is
    judged strictly, as before) or the head's source cannot be pinned on top of
    the base's released pins (a changed released block stays an error).
    """
    if base_text is None:
        return None
    base_manifest = load_manifest_text(base_text, f"{base_ref}:{MANIFEST_PATH}")
    if without_layout(head_manifest) != without_layout(base_manifest):
        return None
    try:
        return refreshed_manifest(base_manifest, head_actual)
    except GuardError:
        return None


def validated_inventory(
    manifest: dict[str, Any],
    actual: dict[str, Any],
    label: str,
    base_text: str | None = None,
    base_ref: str = "base",
) -> tuple[dict[str, Any], bool]:
    """Validate a pin, or derive the unchanged base pin for source inspection.

    The guard, renumber inspection and lander use this same deferred-pin rule.
    Without an explicit base, published inventories must still match strictly.
    The returned inventory is in memory only; publication belongs to the lander.
    """
    try:
        validate_manifest(manifest, actual, label)
    except GuardError:
        derived = deferred_pin(base_text, base_ref, manifest, actual)
        if derived is None:
            raise
        return derived, True
    return manifest, False


def parse_pending_predecessors(texts: Iterable[str], base_latest: int) -> list[int]:
    """Versions a slice declares as pending sibling migrations (#1285).

    A provisional declaration (`tools/provisional-migrations/*.json`) may carry
    `pending_predecessors`: the versions between the base's schema head and the
    slice's own version that an in-flight sibling is expected to land. The slice
    then numbers itself after its sibling instead of at base head + 1, and the
    guard and shard checker accept the gap. Values at or below `base_latest`
    already landed and are ignored, so a retained declaration goes inert.
    """
    gaps: set[int] = set()
    for text in texts:
        try:
            declaration = json.loads(text)
        except ValueError as error:
            raise GuardError(f"provisional declaration is not valid JSON: {error}") from error
        if not isinstance(declaration, dict):
            raise GuardError("provisional declaration must be a JSON object")
        pending = declaration.get("pending_predecessors")
        if pending is None:
            continue
        version = declaration.get("version")
        if (
            not isinstance(pending, list)
            or not pending
            or len(pending) > MAX_PENDING_PREDECESSORS
            or any(isinstance(item, bool) or not isinstance(item, int) for item in pending)
            or isinstance(version, bool)
            or not isinstance(version, int)
        ):
            raise GuardError(
                "provisional declaration pending_predecessors must be a short list of "
                "integer versions beside an integer version"
            )
        if len(set(pending)) != len(pending) or max(pending) >= version:
            raise GuardError(
                f"pending_predecessors {pending} must be distinct and below the declared V{version}"
            )
        gaps.update(item for item in pending if item > base_latest)
    return sorted(gaps)


def declared_gaps(base_ref: str, head_ref: str, base_latest: int) -> list[int]:
    """Pending predecessors declared by declarations added between base and head."""
    listing = subprocess.run(
        ["git", "diff", "--name-only", "--diff-filter=A", base_ref, head_ref, "--", DECLARATIONS_DIR],
        check=False, stdout=subprocess.PIPE, stderr=subprocess.DEVNULL, text=True,
    )
    if listing.returncode:
        return []
    names = [name for name in listing.stdout.splitlines() if name.endswith(".json")]
    return parse_pending_predecessors(
        [text for text in (git_show(head_ref, name) for name in names) if text is not None],
        base_latest,
    )


def validate_append_only(
    base: dict[str, Any], head: dict[str, Any], gaps: Iterable[int] = ()
) -> None:
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
        skipped = set(gaps)
        if any(not base_latest < gap < head_latest for gap in skipped):
            raise GuardError(
                f"declared pending predecessors {sorted(skipped)} must lie between "
                f"V{base_latest} and V{head_latest}"
            )
        expected = [
            str(version)
            for version in range(base_latest + 1, head_latest + 1)
            if version not in skipped
        ]
        if new_versions != expected:
            raise GuardError(
                "schema version bump must append one pinned migration block for every new version "
                "(a slice numbered after an in-flight sibling declares it as "
                "`pending_predecessors` in its tools/provisional-migrations declaration, #1285)"
            )


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "base", nargs="?",
        help="base git revision (refresh default: origin/rolling; check default: HEAD^)",
    )
    parser.add_argument("head", nargs="?", default="HEAD", help="head git revision")
    parser.add_argument(
        "--refresh",
        action="store_true",
        help="repin unreleased worktree source against the base's released pins",
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
            base_ref = args.base or "origin/rolling"
            resolved = subprocess.run(
                ["git", "rev-parse", "--verify", f"{base_ref}^{{commit}}"],
                stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, check=False,
            )
            if resolved.returncode:
                raise GuardError(f"refresh base revision {base_ref} cannot be resolved; fetch it first")
            base_text = git_show(base_ref, MANIFEST_PATH)
            base_manifest = None
            if base_text is not None:
                base_manifest = load_manifest_text(base_text, f"{base_ref}:{MANIFEST_PATH}")
                base_actual = inventory(load_revision_files(base_ref, base_manifest))
                validate_manifest(base_manifest, base_actual, "refresh base migration manifest")

            manifest_file = Path(MANIFEST_PATH)
            existing_text = manifest_file.read_text() if manifest_file.exists() else ""
            if not existing_text:
                existing = base_manifest
            elif has_conflict_markers(existing_text):
                existing = merge_conflicted_manifest(MANIFEST_PATH)
            else:
                existing = load_manifest_text(existing_text, MANIFEST_PATH)
            paths = tracked_source_paths(existing)
            if base_manifest is not None:
                paths.extend(tracked_source_paths(base_manifest))
            paths.extend(marked_source_paths(args.include_path))
            worktree_files = load_worktree_files(existing, extra_paths=paths)
            actual = inventory(worktree_files)
            tests_file = Path(REWIND_TESTS_PATH)
            validate_rewind_teardown_matches_schema(
                worktree_files, tests_file.read_text() if tests_file.exists() else None
            )
            # Only the base's pins are released. Branch-local new versions and
            # marked helpers can be edited before publication. If the base has
            # no manifest, retain the old conservative bootstrap behavior.
            released = base_manifest if base_manifest is not None else existing
            altered_file = Path(ALTERED_OBJECTS_PATH)
            if base_manifest is not None:
                validate_altered_objects(
                    worktree_files,
                    base_manifest["latest_schema_version"],
                    altered_file.read_text() if altered_file.exists() else None,
                )
            updated = refreshed_manifest(released, actual) if released is not None else actual
            if released is not None:
                validate_append_only(
                    released,
                    updated,
                    parse_pending_predecessors(
                        [path.read_text() for path in sorted(Path(DECLARATIONS_DIR).glob("*.json"))],
                        int(released["latest_schema_version"]),
                    ),
                )
            updated_text = json.dumps(updated, indent=2) + "\n"
            if updated_text != existing_text:
                Path(MANIFEST_PATH).write_text(updated_text)
            print(f"refreshed {MANIFEST_PATH} through V{updated['latest_schema_version']}")
            return 0

        head_text = read_revision(args.head, MANIFEST_PATH)
        head_manifest = load_manifest_text(head_text, f"{args.head}:{MANIFEST_PATH}")
        head_files = load_revision_files(args.head, head_manifest)
        head_actual = inventory(head_files)
        base_ref = args.base or f"{args.head}^"
        base_text = git_show(base_ref, MANIFEST_PATH)
        head_manifest, deferred = validated_inventory(
            head_manifest, head_actual, "head migration manifest", base_text, base_ref
        )
        validate_rewind_teardown_matches_schema(head_files, git_show(args.head, REWIND_TESTS_PATH))
        head_altered_table = git_show(args.head, ALTERED_OBJECTS_PATH)

        if base_text is None:
            print(
                f"released-migration guard: bootstrap manifest valid through "
                f"V{head_manifest['latest_schema_version']} (base has no manifest)"
            )
            return 0
        base_manifest = load_manifest_text(base_text, f"{base_ref}:{MANIFEST_PATH}")
        base_actual = inventory(load_revision_files(base_ref, base_manifest))
        validate_manifest(base_manifest, base_actual, "base migration manifest")
        validate_append_only(
            base_manifest,
            head_manifest,
            declared_gaps(base_ref, args.head, int(base_manifest["latest_schema_version"])),
        )
        validate_altered_objects(
            head_files, base_manifest["latest_schema_version"], head_altered_table
        )
        print(
            f"released-migration guard: PASS {base_ref}..{args.head}; "
            f"V0-V{base_manifest['latest_schema_version']} immutable, "
            f"head V{head_manifest['latest_schema_version']}"
            + ("; pin deferred to the lander" if deferred else "")
        )
        return 0
    except (GuardError, OSError, subprocess.SubprocessError) as error:
        print(f"released-migration guard: FAIL: {error}", file=sys.stderr)
        print(
            "Released migration DDL, catalog projections, and fingerprints are immutable. "
            "Never repair them in place; add a forward migration. The pin file is derived: "
            "never edit it by hand or resolve its conflicts. The lander derives it when it "
            "publishes; to see it earlier run `python3 tools/check-released-migrations.py --refresh`.",
            file=sys.stderr,
        )
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
