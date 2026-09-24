#!/usr/bin/env python3
"""Prove and rewrite one accepted provisional SQLite migration unit.

An author adds one ``tools/provisional-migrations/*.json`` file with:

  {"schema_version": 1, "version": 130, "files": [
    {"path": "crates/rsid/src/store/mod.rs",
     "path_template": "crates/rsid/src/store/mod.rs",
     "source_blob": "sha256:<sha256 of the complete source file>",
     "sites": [{"anchor": "if version < 130 {", "scope": "unit",
                "replacement": "if version < ${VERSION} {"}]}
  ]}

Every occurrence of the provisional number in a changed text file, including
filenames, must be accounted for by an exact, unique declaration.  The
replacement must render exactly the source anchor at the old version.  The
manifest is derived and may not carry hand-authored replacement text.
"""

from __future__ import annotations

import argparse
import difflib
import hashlib
import importlib.util
import json
import os
import re
import subprocess
import sys
import tempfile
from pathlib import Path

sys.dont_write_bytecode = True
ROOT = Path(__file__).resolve().parent
GUARD_PATH = ROOT / "check-released-migrations.py"
SPEC = importlib.util.spec_from_file_location("released_migration_guard", GUARD_PATH)
assert SPEC and SPEC.loader
guard = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(guard)

STORE = "crates/rsid/src/store/mod.rs"
MANIFEST = "tools/released-migrations.json"
DECLARATIONS = "tools/provisional-migrations/"
TOKEN = "${VERSION}"
OID_RE = re.compile(r"(?:[0-9a-f]{40}|[0-9a-f]{64})\Z")
PATH_RE = re.compile(r"[a-zA-Z0-9_./-]+\Z")
MAX_DECLARATION_BYTES = 512 * 1024
MAX_CHANGED_PATHS = 2048
MAX_DECLARED_FILES = 128
MAX_SITES = 4096
MAX_SOURCE_BLOB_BYTES = 8 * 1024 * 1024
MAX_TARGET_FIRST_PARENT_COMMITS = 4096


class Refusal(ValueError):
    pass


def safe_env() -> dict[str, str]:
    environment = {key: value for key, value in os.environ.items()
                   if not key.startswith(("GIT_", "RSI_", "PYTHON"))}
    environment.update({"HOME": "/nonexistent", "GIT_NO_REPLACE_OBJECTS": "1",
                        "GIT_GRAFT_FILE": "/dev/null", "GIT_TERMINAL_PROMPT": "0"})
    return environment


def git_argv(repo: Path, *args: str) -> list[str]:
    return ["git", "--no-optional-locks", "-c", "core.hooksPath=/dev/null",
            "-c", "commit.gpgsign=false", "-c", "rerere.enabled=false",
            "-c", "gc.auto=0", "-C", str(repo), *args]


def run(repo: Path, *args: str, data: bytes | None = None,
        env: dict[str, str] | None = None, check: bool = True) -> bytes:
    effective_env = safe_env()
    effective_env.update(env or {})
    result = subprocess.run(git_argv(repo, *args), input=data,
                            stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                            env=effective_env, check=False)
    if check and result.returncode:
        raise Refusal(f"git {args[0]} refused: {result.stderr.decode(errors='replace').strip()}")
    return result.stdout


def oid(value: str) -> str:
    if not OID_RE.fullmatch(value):
        raise Refusal("noncanonical commit ID")
    return value


def path(value: str) -> str:
    if (not isinstance(value, str) or not PATH_RE.fullmatch(value) or
            value.startswith("/") or any(part in ("", ".", "..") for part in value.split("/"))):
        raise Refusal("unsafe declaration path")
    return value


def show(repo: Path, revision: str, filename: str) -> bytes | None:
    result = subprocess.run(git_argv(repo, "show", f"{revision}:{filename}"),
                            stdout=subprocess.PIPE, stderr=subprocess.DEVNULL,
                            env=safe_env(), check=False)
    return result.stdout if result.returncode == 0 else None


def sha(data: bytes) -> str:
    return "sha256:" + hashlib.sha256(data).hexdigest()


def changed(repo: Path, base: str, source: str) -> set[str]:
    output = run(repo, "diff", "--no-renames", "--name-only", "-z", base, source)
    paths = {os.fsdecode(item) for item in output.split(b"\0") if item}
    if len(paths) > MAX_CHANGED_PATHS:
        raise Refusal("accepted source path count exceeds provisional bound")
    return paths


def revision_inventory(repo: Path, revision: str) -> dict:
    raw = show(repo, revision, MANIFEST)
    if raw is None:
        raise Refusal(f"{MANIFEST} missing at {revision}")
    try:
        manifest = guard.load_manifest_text(raw.decode(), f"{revision}:{MANIFEST}")
        files = {name: (show(repo, revision, name) or b"").decode()
                 for name in guard.tracked_source_paths(manifest)}
        if any(show(repo, revision, name) is None for name in files):
            raise Refusal("manifest names a missing protected source")
        guard.validate_manifest(manifest, guard.inventory(files), "provisional inventory")
        return manifest
    except (UnicodeError, KeyError, guard.GuardError) as error:
        raise Refusal(f"invalid migration inventory at {revision}: {error}") from error


def inspect(repo: Path, base: str, source: str, target: str) -> dict | None:
    for item in (base, source, target):
        oid(item)
        run(repo, "cat-file", "-e", f"{item}^{{commit}}")
    if subprocess.run(git_argv(repo, "merge-base", "--is-ancestor", base, source),
                      stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
                      env=safe_env()).returncode:
        raise Refusal("accepted base is not an ancestor of source")
    base_manifest = revision_inventory(repo, base)
    source_manifest = revision_inventory(repo, source)
    target_manifest = revision_inventory(repo, target)
    try:
        guard.validate_append_only(base_manifest, source_manifest)
    except guard.GuardError as error:
        raise Refusal(f"source changed released prefix: {error}") from error
    try:
        guard.validate_append_only(base_manifest, target_manifest)
    except guard.GuardError as error:
        raise Refusal(f"target changed accepted base's released prefix: {error}") from error
    old = source_manifest["latest_schema_version"]
    prior = base_manifest["latest_schema_version"]
    if old == prior:
        return None
    if old != prior + 1 or set(source_manifest["blocks"]) - set(base_manifest["blocks"]) != {str(old)}:
        raise Refusal("accepted source has more than one provisional migration")
    if target_manifest["latest_schema_version"] < prior:
        raise Refusal("target schema head is behind the accepted base")
    if target_manifest["latest_schema_version"] >= 2_147_483_647:
        raise Refusal("no representable next i32 schema version")
    declarations = sorted(name for name in changed(repo, base, source)
                          if name.startswith(DECLARATIONS) and name.endswith(".json"))
    if len(declarations) != 1:
        raise Refusal("one new provisional site declaration is required")
    declaration_path = path(declarations[0])
    if show(repo, base, declaration_path) is not None:
        raise Refusal("provisional site declaration must be newly added")
    raw = show(repo, source, declaration_path)
    if raw is None or len(raw) > MAX_DECLARATION_BYTES:
        raise Refusal("provisional site declaration is missing or too large")
    try:
        declaration = json.loads(raw)
    except (TypeError, ValueError) as error:
        raise Refusal("invalid provisional site declaration") from error
    if (set(declaration) != {"schema_version", "version", "files"} or
            declaration["schema_version"] != 1 or declaration["version"] != old or
            not isinstance(declaration["files"], list) or not declaration["files"] or
            len(declaration["files"]) > MAX_DECLARED_FILES):
        raise Refusal("invalid provisional declaration shape or version")
    all_changed = changed(repo, base, source)
    version_pattern = re.compile(rb"(?<![0-9])" + str(old).encode() + rb"(?![0-9])")
    declared_paths: set[str] = set()
    destinations: set[str] = set()
    replacements = []
    for file in declaration["files"]:
        if set(file) != {"path", "path_template", "source_blob", "sites"}:
            raise Refusal("invalid provisional file declaration")
        source_path = path(file["path"])
        template_path = path(file["path_template"].replace(TOKEN, "VERSION"))
        if template_path.count("VERSION") != file["path_template"].count(TOKEN):
            raise Refusal("invalid provisional path template")
        if file["path_template"].replace(TOKEN, str(old)) != source_path:
            raise Refusal("provisional path template does not match source")
        if version_pattern.search(source_path.encode()) and TOKEN not in file["path_template"]:
            raise Refusal(f"undeclared version-bearing filename: {source_path}")
        destination = path(file["path_template"].replace(TOKEN, str(target_manifest["latest_schema_version"] + 1)))
        if source_path in declared_paths or destination in destinations or source_path not in all_changed:
            raise Refusal("duplicate, unchanged, or colliding provisional file")
        if (source_path.startswith("crates/rsid/src/store/") and source_path != STORE and
                source_path not in guard.tracked_source_paths(source_manifest)):
            raise Refusal(f"migration helper or rewind source is not pinned: {source_path}")
        declared_paths.add(source_path)
        destinations.add(destination)
        original = show(repo, source, source_path)
        if (original is None or len(original) > MAX_SOURCE_BLOB_BYTES or
                sha(original) != file["source_blob"] or b"\0" in original):
            raise Refusal(f"source blob digest mismatch or binary file: {source_path}")
        if (not isinstance(file["sites"], list) or len(file["sites"]) > MAX_SITES or
                any(set(site) != {"anchor", "replacement", "scope"} or
                    site["scope"] not in ("unit", "head") for site in file["sites"])):
            raise Refusal("invalid provisional site list")
        covered = bytearray(len(original))
        converted = original
        for site in file["sites"]:
            anchor, replacement = site["anchor"], site["replacement"]
            if (not isinstance(anchor, str) or not isinstance(replacement, str) or
                    not anchor or replacement.count(TOKEN) == 0 or
                    replacement.replace(TOKEN, str(old)) != anchor):
                raise Refusal("site replacement must be a pure version substitution")
            before = anchor.encode()
            if original.count(before) != 1 or not version_pattern.search(before):
                raise Refusal(f"site anchor is missing, duplicate, or unversioned: {source_path}")
            start = original.index(before)
            if any(covered[start:start + len(before)]):
                raise Refusal("overlapping provisional site anchors")
            covered[start:start + len(before)] = b"\1" * len(before)
            after = replacement.replace(TOKEN, str(target_manifest["latest_schema_version"] + 1)).encode()
            converted = converted.replace(before, after, 1)
            replacements.append({"path": source_path, "target_path": destination,
                                 "before": anchor, "after": after.decode(),
                                 "scope": site["scope"],
                                 "source_blob": sha(original),
                                 "before_offset": start})
        for match in version_pattern.finditer(original):
            if not all(covered[match.start():match.end()]):
                raise Refusal(f"undeclared version-bearing site: {source_path}:{match.start()}")
        if destination != source_path and show(repo, source, destination) is not None:
            raise Refusal(f"destination already exists in source: {destination}")
        if destination != source_path and show(repo, target, destination) is not None:
            raise Refusal(f"destination already exists in target: {destination}")
        if show(repo, base, source_path) is None and show(repo, target, destination) is not None:
            raise Refusal(f"new source file collides with target: {destination}")
    for filename in all_changed - declared_paths - {MANIFEST, declaration_path}:
        if version_pattern.search(filename.encode()) or version_pattern.search(show(repo, source, filename) or b""):
            raise Refusal(f"undeclared version-bearing file or site: {filename}")
    if STORE not in declared_paths:
        raise Refusal("schema constant and migration gate must be declared")
    if (not any(site["path"] == STORE and site["scope"] == "head" and
                site["before"] == f"pub const LATEST_SCHEMA_VERSION: i32 = {old};"
                for site in replacements) or
            not any(site["path"] == STORE and site["scope"] == "unit" and
                    site["before"] == f"if version < {old} {{"
                    for site in replacements)):
        raise Refusal("schema constant and migration gate need exact head/unit sites")
    return {"base": base, "source": source, "target": target,
            "declaration": declaration_path, "old_version": old,
            "new_version": target_manifest["latest_schema_version"] + 1,
            "source_manifest": source_manifest, "target_manifest": target_manifest,
            "files": declaration["files"], "sites": replacements}


def git_blob(repo: Path, data: bytes) -> str:
    return run(repo, "hash-object", "-w", "--stdin", data=data).decode().strip()


def git_index_update(repo: Path, env: dict[str, str], filename: str,
                     data: bytes, source_mode: str) -> None:
    blob_id = git_blob(repo, data)
    run(repo, "update-index", "--add", "--cacheinfo",
        f"{source_mode},{blob_id},{filename}", env=env)


def commit_tree(repo: Path, tree: str, source: str) -> str:
    env = {"GIT_AUTHOR_NAME": "rsi rolling landing",
           "GIT_AUTHOR_EMAIL": "rsi-rolling-land@rsi.invalid",
           "GIT_COMMITTER_NAME": "rsi rolling landing",
           "GIT_COMMITTER_EMAIL": "rsi-rolling-land@rsi.invalid"}
    return run(repo, "commit-tree", tree, "-p", source,
               data=b"rsi landing: provisional migration transform\n", env=env).decode().strip()


def preview_transformed_manifest(repo: Path, unit: dict) -> dict:
    version = unit["new_version"]
    entries = {file["path"]: file for file in unit["files"]}
    files = {}
    for filename in guard.tracked_source_paths(unit["source_manifest"]):
        original = show(repo, unit["source"], filename)
        if original is None:
            raise Refusal(f"protected source disappeared: {filename}")
        file = entries.get(filename)
        if file is not None:
            if sha(original) != file["source_blob"]:
                raise Refusal(f"source blob changed: {filename}")
            for site in file["sites"]:
                before = site["anchor"].encode()
                if original.count(before) != 1:
                    raise Refusal(f"source site changed: {filename}")
                original = original.replace(before,
                                            site["replacement"].replace(TOKEN, str(version)).encode(), 1)
            filename = file["path_template"].replace(TOKEN, str(version))
        files[filename] = original.decode()
    try:
        return guard.inventory(files)
    except (UnicodeError, guard.GuardError) as error:
        raise Refusal(f"cannot recompute transformed inventory: {error}") from error


def transform(repo: Path, unit: dict) -> dict:
    """Write an unreferenced source-child commit with only declared replacements.

    The final landing candidate will still use SOURCE as its second parent.
    This temporary commit supplies Git's ordinary three-way merge machinery.
    """
    source = unit["source"]
    version = unit["new_version"]
    paths = {}
    with tempfile.TemporaryDirectory(prefix="rsi-migration-index-") as temp:
        env = {"GIT_INDEX_FILE": str(Path(temp) / "index")}
        run(repo, "read-tree", source, env=env)
        for file in unit["files"]:
            old_path = file["path"]
            new_path = file["path_template"].replace(TOKEN, str(version))
            original = show(repo, source, old_path)
            if original is None or sha(original) != file["source_blob"]:
                raise Refusal(f"source blob changed during transform: {old_path}")
            converted = original
            for site in file["sites"]:
                before = site["anchor"].encode()
                after = site["replacement"].replace(TOKEN, str(version)).encode()
                if converted.count(before) != 1:
                    raise Refusal(f"source site changed during transform: {old_path}")
                converted = converted.replace(before, after, 1)
            entry = run(repo, "ls-tree", source, "--", old_path).decode().strip()
            mode = entry.split(" ", 1)[0]
            if mode not in ("100644", "100755"):
                raise Refusal(f"unsupported source file mode: {old_path}")
            if new_path != old_path:
                run(repo, "update-index", "--force-remove", "--", old_path, env=env)
            git_index_update(repo, env, new_path, converted, mode)
            paths[old_path] = new_path
        manifest = preview_transformed_manifest(repo, unit)
        manifest_text = (json.dumps(manifest, indent=2) + "\n").encode()
        git_index_update(repo, env, MANIFEST, manifest_text, "100644")
        tree = run(repo, "write-tree", env=env).decode().strip()
        transformed = commit_tree(repo, tree, source)
    return {"transformed_source": transformed, "paths": paths,
            "transformed_manifest": manifest, "transformed_tree": tree}


def allowed_mask(repo: Path, unit: dict, filename: str, transformed: bytes) -> bytearray:
    mask = bytearray(len(transformed))
    for site in unit["sites"]:
        if site["target_path"] != filename:
            continue
        value = site["after"].encode()
        if transformed.count(value) != 1:
            raise Refusal(f"transformed site is ambiguous: {filename}")
        start = transformed.index(value)
        end = start + len(value)
        if transformed[end:end + 1] == b"\n":
            end += 1
        mask[start:end] = b"\1" * (end - start)
    if filename == STORE:
        blocks = guard.migration_blocks(transformed.decode())
        block = blocks.get(str(unit["new_version"]))
        if block is None or transformed.count(block.encode()) != 1:
            raise Refusal("transformed migration gate is missing or ambiguous")
        start = transformed.index(block.encode())
        mask[start:start + len(block)] = b"\1" * len(block)
    return mask


def target_migration_mask(repo: Path, unit: dict, filename: str, target: bytes) -> bytearray:
    """Bound the target side of a conflict to its appended migration tail."""
    mask = bytearray(len(target))
    prior = unit["source_manifest"]["latest_schema_version"] - 1
    latest = unit["target_manifest"]["latest_schema_version"]
    if latest <= prior:
        return mask
    if filename == STORE:
        constant = f"pub const LATEST_SCHEMA_VERSION: i32 = {latest};".encode()
        if target.count(constant) != 1:
            raise Refusal("target schema constant is ambiguous")
        start = target.index(constant)
        mask[start:start + len(constant)] = b"\1" * len(constant)
        blocks = guard.migration_blocks(target.decode())
        for number in range(prior + 1, latest + 1):
            block = blocks.get(str(number))
            if block is None or target.count(block.encode()) != 1:
                raise Refusal(f"target migration block V{number} is ambiguous")
            start = target.index(block.encode())
            end = start + len(block)
            if target[end:end + 1] == b"\n":
                end += 1
            mask[start:end] = b"\1" * (end - start)
    prior_units = unit.get("prior_units", [])
    if not isinstance(prior_units, list) or len(prior_units) > 64:
        raise Refusal("invalid prior provisional proof list")
    prior_units = {prior["unit_candidate"]: prior for prior in prior_units}
    if "_verified_target_units" not in unit:
        unit["_verified_target_units"] = discover_target_provisional_units(
            repo, unit["base"], unit["target"])
    for prior in unit["_verified_target_units"]:
        prior_units.setdefault(prior["unit_candidate"], prior)
    for prior_unit in prior_units.values():
        if set(prior_unit) != {"base", "source", "target", "unit_candidate"}:
            raise Refusal("invalid prior provisional proof binding")
        previous = inspect(repo, prior_unit["base"], prior_unit["source"], prior_unit["target"])
        if previous is None:
            raise Refusal("prior provisional source is not a migration")
        proof = prove(repo, previous, prior_unit["unit_candidate"], unit["target"])
        for site in proof["sites"]:
            if site["target_path"] != filename:
                continue
            after = site["after"].encode()
            if target.count(after) != 1:
                raise Refusal(f"prior proved site is ambiguous: {filename}")
            start = target.index(after)
            end = start + len(after)
            if target[end:end + 1] == b"\n":
                end += 1
            mask[start:end] = b"\1" * (end - start)
    return mask


def discover_target_provisional_units(repo: Path, base: str, target: str) -> list[dict]:
    """Reconstruct prior renumber proofs from the fetched target's first-parent chain."""
    if not is_ancestor(repo, base, target):
        raise Refusal("accepted base is outside target ancestry")
    history = run(repo, "log", "--first-parent", "--format=%H %P",
                  f"{base}..{target}").decode().splitlines()
    if len(history) > MAX_TARGET_FIRST_PARENT_COMMITS:
        raise Refusal("target ancestry exceeds provisional proof search bound")
    units = []
    for commit in reversed(history):
        candidate, *parents = commit.split()
        if len(parents) != 2:
            continue
        parent, source = parents
        source_base = run(repo, "merge-base", parent, source).decode().strip()
        declarations = [name for name in changed(repo, source_base, source)
                        if name.startswith(DECLARATIONS) and name.endswith(".json")]
        if not declarations:
            continue
        previous = inspect(repo, source_base, source, parent)
        if previous is None:
            raise Refusal("target provisional declaration has no appended migration")
        prove(repo, previous, candidate, target)
        units.append({"base": source_base, "source": source,
                      "target": parent, "unit_candidate": candidate})
        if len(units) > 64:
            raise Refusal("target provisional proof count exceeds bound")
    return units


def changed_lines_are_declared(base: list[bytes], theirs: list[bytes],
                               full_source: bytes, mask: bytearray) -> bool:
    chunk = b"".join(theirs)
    if not chunk or full_source.count(chunk) != 1:
        return False
    chunk_start = full_source.index(chunk)
    offsets = [chunk_start]
    for line in theirs:
        offsets.append(offsets[-1] + len(line))
    matcher = difflib.SequenceMatcher(None, base, theirs, autojunk=False)
    for tag, _start, _end, new_start, new_end in matcher.get_opcodes():
        if tag == "equal":
            continue
        for index in range(new_start, new_end):
            if not theirs[index].strip():
                continue
            if not all(mask[offsets[index]:offsets[index + 1]]):
                return False
    return True


def resolve_diff3(repo: Path, unit: dict, filename: str, contents: bytes) -> bytes:
    transformed = show(repo, unit["transformed_source"], filename)
    target = show(repo, unit["target"], filename)
    if transformed is None or target is None:
        raise Refusal(f"conflicted path missing in transformed source: {filename}")
    mask = allowed_mask(repo, unit, filename, transformed)
    target_mask = target_migration_mask(repo, unit, filename, target)
    lines = contents.splitlines(keepends=True)
    output: list[bytes] = []
    index = 0
    while index < len(lines):
        if not lines[index].startswith(b"<<<<<<< "):
            output.append(lines[index])
            index += 1
            continue
        index += 1
        ours: list[bytes] = []
        while index < len(lines) and not lines[index].startswith(b"||||||| "):
            ours.append(lines[index]); index += 1
        if index == len(lines):
            raise Refusal(f"conflict lacks diff3 base: {filename}")
        index += 1
        base: list[bytes] = []
        while index < len(lines) and lines[index] != b"=======\n":
            base.append(lines[index]); index += 1
        if index == len(lines):
            raise Refusal(f"malformed diff3 conflict: {filename}")
        index += 1
        theirs: list[bytes] = []
        while index < len(lines) and not lines[index].startswith(b">>>>>>> "):
            theirs.append(lines[index]); index += 1
        if (index == len(lines) or
                not changed_lines_are_declared(base, theirs, transformed, mask) or
                not changed_lines_are_declared(base, ours, target, target_mask)):
            raise Refusal(f"unproved migration conflict: {filename}")
        index += 1
        if not base:
            output.extend(ours if ours == theirs else ours + theirs)
            continue
        if len(base) != 1 or not ours or not theirs:
            raise Refusal(f"semantic replacement conflict: {filename}")
        head_sites = [site for site in unit["sites"]
                      if site["target_path"] == filename and site["scope"] == "head"
                      and site["after"].encode() in theirs[0]]
        if len(head_sites) != 1:
            raise Refusal(f"non-head replacement conflict: {filename}")
        site = head_sites[0]
        old = str(unit["old_version"])
        current = str(unit["new_version"] - 1)
        expected_old = site["before"].replace(old, current).encode()
        if expected_old not in ours[0] or site["after"].encode() not in theirs[0]:
            raise Refusal(f"target head site changed semantically: {filename}")
        output.extend(theirs[:1] + ours[1:] + theirs[1:])
    return b"".join(output)


def resolve(repo: Path, worktree: Path, unit: dict) -> dict:
    """Resolve only proved version conflicts, then derive the merged inventory."""
    unmerged = run(worktree, "diff", "--name-only", "--diff-filter=U", "-z")
    paths = [os.fsdecode(value) for value in unmerged.split(b"\0") if value]
    allowed = {site["target_path"] for site in unit["sites"]} | {MANIFEST}
    if any(filename not in allowed for filename in paths):
        raise Refusal(f"nonmigration conflict: {', '.join(paths)}")
    for filename in paths:
        if filename == MANIFEST:
            continue
        destination = worktree / filename
        if not destination.resolve().is_relative_to(worktree.resolve()) or destination.is_symlink():
            raise Refusal(f"conflict path escapes private candidate: {filename}")
        contents = destination.read_bytes()
        resolved = resolve_diff3(repo, unit, filename, contents)
        destination.write_bytes(resolved)
        run(worktree, "add", "--", filename)
    target_manifest = unit["target_manifest"]
    transformed_manifest = unit["transformed_manifest"]
    protected_paths = (set(guard.tracked_source_paths(target_manifest)) |
                       set(guard.tracked_source_paths(transformed_manifest)))
    try:
        files = {}
        for filename in protected_paths:
            destination = worktree / filename
            if not destination.resolve().is_relative_to(worktree.resolve()) or destination.is_symlink():
                raise Refusal(f"protected path escapes private candidate: {filename}")
            files[filename] = destination.read_text()
        merged_manifest = guard.inventory(files)
    except (OSError, UnicodeError, guard.GuardError) as error:
        raise Refusal(f"cannot derive merged migration inventory: {error}") from error
    manifest_file = worktree / MANIFEST
    if not manifest_file.resolve().is_relative_to(worktree.resolve()) or manifest_file.is_symlink():
        raise Refusal("migration inventory path escapes private candidate")
    manifest_file.write_text(json.dumps(merged_manifest, indent=2) + "\n")
    run(worktree, "add", "--", MANIFEST)
    if run(worktree, "diff", "--name-only", "--diff-filter=U", "-z"):
        raise Refusal("unmerged paths remain after provisional resolution")
    run(worktree, "diff", "--cached", "--check")
    return {"resolved_paths": paths, "candidate_manifest": merged_manifest}


def build_candidate(repo: Path, unit: dict, scratch: Path) -> dict:
    """Build only an exact-parent migration commit in an owned private worktree.

    The caller passes the resulting commit to the integration engine's ordinary
    fast-forward candidate preparation. That engine mints the custody handle
    and owns all later candidate cleanup and publication checks.
    """
    if not scratch.is_dir() or not scratch.resolve().is_relative_to(repo.parent.resolve()):
        raise Refusal("provisional scratch must be inside the private landing workspace")
    with tempfile.TemporaryDirectory(prefix="rsi-migration-build-", dir=scratch) as temporary:
        worktree = Path(temporary) / "tree"
        run(repo, "worktree", "add", "--detach", "--quiet", str(worktree), unit["target"])
        error = None
        outcome = None
        try:
            merge = subprocess.run(
                git_argv(worktree, "-c", "merge.conflictStyle=diff3", "merge", "--no-ff",
                         "--no-commit", "--no-stat", "-q", unit["transformed_source"]),
                stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                env=safe_env(), check=False,
            )
            if merge.returncode != 0:
                unmerged = run(worktree, "diff", "--name-only", "--diff-filter=U", "-z")
                if not unmerged:
                    raise Refusal(f"provisional merge refused: {merge.stderr.decode(errors='replace').strip()}")
            resolution = resolve(repo, worktree, unit)
            tree = run(worktree, "write-tree").decode().strip()
            env = {"GIT_AUTHOR_NAME": "rsi rolling landing",
                   "GIT_AUTHOR_EMAIL": "rsi-rolling-land@rsi.invalid",
                   "GIT_COMMITTER_NAME": "rsi rolling landing",
                   "GIT_COMMITTER_EMAIL": "rsi-rolling-land@rsi.invalid"}
            candidate = run(repo, "commit-tree", tree, "-p", unit["target"],
                            "-p", unit["source"], "-m",
                            "rsi landing: renumber provisional migration", env=env).decode().strip()
            prove(repo, unit, candidate, candidate)
            outcome = {"candidate": candidate, "tree": tree,
                       "resolved_paths": resolution["resolved_paths"]}
        except Exception as caught:
            error = caught
        cleanup = subprocess.run(git_argv(repo, "worktree", "remove", "--force", str(worktree)),
                                 stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                                 env=safe_env(), check=False)
        if cleanup.returncode:
            message = cleanup.stderr.decode(errors="replace").strip()
            raise Refusal(f"provisional worktree cleanup failed: {message}; build_error={error}")
        if error is not None:
            raise error
        return outcome


def is_ancestor(repo: Path, old: str, new: str) -> bool:
    return subprocess.run(git_argv(repo, "merge-base", "--is-ancestor", old, new),
                          stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
                          env=safe_env()).returncode == 0


def prove(repo: Path, unit: dict, unit_candidate: str, final_candidate: str) -> dict:
    """Return a reproducible exact-source proof of the allowed substitutions."""
    for item in (unit_candidate, final_candidate):
        oid(item)
    parents = run(repo, "show", "-s", "--format=%P", unit_candidate).decode().split()
    if parents != [unit["target"], unit["source"]]:
        raise Refusal("provisional candidate lacks exact (target, source) parents")
    if not is_ancestor(repo, unit_candidate, final_candidate):
        raise Refusal("provisional candidate is not in final candidate ancestry")
    final_manifest = revision_inventory(repo, final_candidate)
    try:
        guard.validate_append_only(unit["target_manifest"], final_manifest)
    except guard.GuardError as error:
        raise Refusal(f"final candidate changed released prefix: {error}") from error
    version = unit["new_version"]
    transformed = preview_transformed_manifest(repo, unit)
    if final_manifest["blocks"].get(str(version)) != transformed["blocks"].get(str(version)):
        raise Refusal("provisional migration block changed after transform")
    for name, entry in transformed["protected_sections"].items():
        if name not in unit["target_manifest"]["protected_sections"] and final_manifest["protected_sections"].get(name) != entry:
            raise Refusal(f"provisional protected section changed: {name}")
    sites = []
    for site in unit["sites"]:
        original = show(repo, unit["source"], site["path"])
        candidate = show(repo, final_candidate, site["target_path"])
        target = show(repo, unit["target"], site["target_path"])
        if original is None or candidate is None or sha(original) != site["source_blob"]:
            raise Refusal(f"proof blob missing or changed: {site['path']}")
        expected = next(
            file_site["replacement"].replace(
                TOKEN, str(final_manifest["latest_schema_version"] if site["scope"] == "head" else version))
            for file in unit["files"] if file["path"] == site["path"]
            for file_site in file["sites"] if file_site["anchor"] == site["before"]
        )
        after = expected.encode()
        if original.count(site["before"].encode()) != 1 or candidate.count(after) != 1:
            raise Refusal(f"proved span missing or ambiguous: {site['target_path']}")
        sites.append({"path": site["path"], "target_path": site["target_path"],
                      "scope": site["scope"], "before": site["before"], "after": expected,
                      "before_offset": original.index(site["before"].encode()),
                      "after_offset": candidate.index(after),
                      "source_blob": sha(original),
                      "target_blob": sha(target) if target is not None else None,
                      "candidate_blob": sha(candidate)})
    return {"schema_version": 1, "base": unit["base"], "source": unit["source"],
            "target": unit["target"], "unit_candidate": unit_candidate,
            "candidate": final_candidate, "old_version": unit["old_version"],
            "assigned_version": version, "final_version": final_manifest["latest_schema_version"],
            "declaration": unit["declaration"],
            "source_manifest_blob": sha(show(repo, unit["source"], MANIFEST)),
            "target_manifest_blob": sha(show(repo, unit["target"], MANIFEST)),
            "candidate_manifest_blob": sha(show(repo, final_candidate, MANIFEST)),
            "sites": sites}


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--transform", action="store_true")
    parser.add_argument("--resolve", action="store_true")
    parser.add_argument("--build", action="store_true")
    parser.add_argument("--scratch", type=Path)
    parser.add_argument("--prove", action="store_true")
    parser.add_argument("--unit-candidate")
    parser.add_argument("--candidate")
    parser.add_argument("--unit-file", type=Path)
    parser.add_argument("--worktree", type=Path)
    parser.add_argument("--repo", type=Path, required=True)
    parser.add_argument("--base")
    parser.add_argument("--source")
    parser.add_argument("--target")
    args = parser.parse_args()
    try:
        if args.resolve or args.prove or args.build:
            if not args.unit_file or not args.worktree:
                if args.resolve:
                    raise Refusal("--resolve requires --unit-file and --worktree")
            if not args.unit_file:
                raise Refusal("--unit-file is required")
            unit = json.loads(args.unit_file.read_text())
            fresh = inspect(args.repo, unit["base"], unit["source"], unit["target"])
            if fresh is None or any(fresh[key] != unit[key] for key in fresh):
                raise Refusal("provisional unit changed before conflict resolution")
            if args.resolve:
                print(json.dumps(resolve(args.repo, args.worktree, unit), sort_keys=True))
            elif args.build:
                if args.scratch is None:
                    raise Refusal("--build requires --scratch")
                print(json.dumps(build_candidate(args.repo, unit, args.scratch), sort_keys=True))
            else:
                if not args.unit_candidate or not args.candidate:
                    raise Refusal("--prove requires --unit-candidate and --candidate")
                print(json.dumps(prove(args.repo, unit, args.unit_candidate, args.candidate), sort_keys=True))
        else:
            if not args.base or not args.source or not args.target:
                raise Refusal("--base, --source and --target are required")
            unit = inspect(args.repo, args.base, args.source, args.target)
            if args.transform:
                if unit is None:
                    raise Refusal("provisional declaration has no appended migration")
                unit.update(transform(args.repo, unit))
            print(json.dumps(unit, sort_keys=True))
        return 0
    except Refusal as error:
        print(f"provisional migration refused: {error}", file=sys.stderr)
        return 2


if __name__ == "__main__":
    raise SystemExit(main())
