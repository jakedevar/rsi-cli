#!/usr/bin/env python3
"""Find concrete reversions of accepted source changes, then test touched crates."""

import argparse
import difflib
import hashlib
import importlib.util
import io
import json
import os
from pathlib import Path
import re
import subprocess
import sys
import tarfile
import tempfile

sys.dont_write_bytecode = True

OID = re.compile(r"(?:[0-9a-f]{40}|[0-9a-f]{64})\Z")


class GuardError(Exception):
    pass


def safe_env():
    return {key: value for key, value in os.environ.items()
            if not key.startswith(("GIT_", "RSI_"))}


def git(repo, *args, input_bytes=None, env=None, check=True):
    result = subprocess.run(
        ["git", "-C", str(repo), "--no-optional-locks", *args],
        input=input_bytes, capture_output=True, env=env or safe_env(), check=False,
    )
    if check and result.returncode:
        raise GuardError(f"git {args[0]} failed: {result.stderr.decode(errors='replace').strip()}")
    return result


def commit(repo, rev):
    if not OID.fullmatch(rev):
        raise GuardError("commit IDs must be full lowercase object names")
    result = git(repo, "cat-file", "-e", f"{rev}^{{commit}}", check=False)
    if result.returncode:
        raise GuardError(f"commit does not exist: {rev}")


def ancestor(repo, old, new):
    return git(repo, "merge-base", "--is-ancestor", old, new, check=False).returncode == 0


def paths_changed(repo, old, new):
    output = git(repo, "diff", "--no-ext-diff", "--no-renames", "--name-only", "-z", old, new).stdout
    return [os.fsdecode(path) for path in output.split(b"\0") if path]


def blob(repo, rev, path):
    result = git(repo, "show", f"{rev}:{path}", check=False)
    return result.stdout if result.returncode == 0 else None


def renamed_paths(repo, source, candidate):
    """Map source paths to candidate paths when Git can establish a rename."""
    output = git(repo, "diff", "--no-ext-diff", "--find-renames=20%",
                 "--name-status", "-z", source, candidate).stdout.split(b"\0")
    renames = {}
    index = 0
    while index < len(output) - 1:
        status = output[index]
        if status.startswith(b"R"):
            renames[os.fsdecode(output[index + 1])] = os.fsdecode(output[index + 2])
            index += 3
        else:
            index += 2
    return renames


def line_text(line):
    return repr(line.decode("utf-8", errors="backslashreplace").rstrip("\r\n"))


def missing_insertion_boundary(matches, old_len, candidate_len, position):
    """Return candidate boundary if the base boundary remains unchanged."""
    if old_len == candidate_len == 0:
        return 0
    for old_start, old_end, new_start, _ in matches:
        if old_start < position < old_end:
            return new_start + position - old_start
        if position == 0 and old_start == new_start == 0:
            return 0
        if position == old_len and old_end == old_len:
            new_end = new_start + old_end - old_start
            if new_end == candidate_len:
                return candidate_len
    left = [new_start + old_end - old_start for old_start, old_end, new_start, _
            in matches if old_end == position]
    right = [new_start for old_start, _, new_start, _ in matches
             if old_start == position]
    return left[-1] if left and right and left[-1] == right[0] else None


def insertion_survives_with_context(matches, source_len, start, end):
    """Require an inserted block and adjacent source context to map together."""
    for source_start, _candidate_start, length in matches:
        source_end = source_start + length
        if source_start <= start and end <= source_end:
            # A lone equal attribute may be a different copy added elsewhere.
            # A neighboring source line ties it to the accepted location.
            return (source_start < start or source_end > end
                    or (start == 0 and end == source_len))
    return False


def validate_renumber_proof(repo, base, source, candidate, proof_path, fetched_target=None):
    """Recompute the source-bound proof; a JSON claim grants no authority itself."""
    script = Path(__file__).resolve().parent.parent / "tools/rolling-migration-renumber.py"
    spec = importlib.util.spec_from_file_location("rolling_migration_renumber", script)
    if spec is None or spec.loader is None:
        raise GuardError("provisional proof validator is unavailable")
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    try:
        proof = json.loads(proof_path.read_text())
        if proof.get("base") != base or proof.get("source") != source or proof.get("candidate") != candidate:
            raise GuardError("provisional proof does not bind the accepted pair and candidate")
        target = proof.get("target")
        if not isinstance(target, str) or not OID.fullmatch(target):
            raise GuardError("provisional proof has no running target")
        if fetched_target is not None and not ancestor(repo, fetched_target, target):
            raise GuardError("provisional running target is outside the fetched target ancestry")
        unit = module.inspect(repo, base, source, target)
        if unit is None:
            raise GuardError("provisional proof has no source migration unit")
        expected = module.prove(repo, unit, proof.get("unit_candidate", ""), candidate)
        if proof != expected:
            raise GuardError("provisional proof differs from independently derived spans and blobs")
        return proof
    except (ValueError, KeyError, OSError, TypeError, module.Refusal) as error:
        raise GuardError(f"provisional proof refused: {error}") from error


MAX_VIRTUAL_BASE_DEPTH = 8


def virtual_base(repo, left, right, depth=0):
    """Return the merge base of two commits as Git itself would reduce it.

    Several merge bases (a criss-cross history) are merged pairwise into one
    synthetic parentless commit, the way ``git merge`` builds its virtual base.
    Returns ``None`` when Git cannot say; the caller then keeps the accepted
    base. The synthetic commit only adds an unreachable object.
    """
    result = git(repo, "merge-base", "--all", left, right, check=False)
    if result.returncode != 0:
        return None
    bases = result.stdout.decode().split()
    if not bases:
        return None
    if len(bases) == 1:
        return bases[0]
    if depth >= MAX_VIRTUAL_BASE_DEPTH:
        return None
    current = bases[0]
    for other in bases[1:]:
        inner = virtual_base(repo, current, other, depth + 1)
        merge_base = [f"--merge-base={inner}"] if inner else []
        merged = git(repo, "merge-tree", "--write-tree", *merge_base, current, other,
                     check=False)
        # Exit 1 is a conflicted merge; its first line is still the tree, and
        # Git's own virtual base keeps the conflict markers the same way.
        if merged.returncode not in (0, 1):
            return None
        tree = merged.stdout.decode().split("\n", 1)[0].strip()
        if not OID.fullmatch(tree):
            return None
        env = safe_env()
        for role in ("AUTHOR", "COMMITTER"):
            env[f"GIT_{role}_NAME"] = "rsi landing guard"
            env[f"GIT_{role}_EMAIL"] = "landing-guard@invalid"
            env[f"GIT_{role}_DATE"] = "2000-01-01T00:00:00Z"
        made = git(repo, "commit-tree", tree, "-m", "virtual merge base", env=env, check=False)
        if made.returncode != 0:
            return None
        current = made.stdout.decode().strip()
    return current


def own_base(repo, base, source, target):
    """Narrow the accepted base to the source's own delta against ``target``.

    A source that merged an older rolling state carries that state's changes
    in ``base..source``. When rolling later changed them on purpose, reading
    them as the source's own insertions flags a reversion that is not one.
    The merge-base of source and the rolling tip is the newest state the
    source shares with rolling, so only the source's own changes lie beyond
    it. When criss-cross merges leave several merge bases, none of them alone
    is that state: each one misses what the others share, so their virtual
    merge base is used (what ``git merge`` builds for the candidate). It is
    used only when the accepted base is that shared state or an ancestor of
    one of the merge bases; otherwise the accepted base stands (never widen
    the checked delta). A source already integrated into ``target`` has no
    delta left to narrow, so its explicit base keeps the full lost-hunk check.
    """
    if ancestor(repo, source, target):
        return base
    result = git(repo, "merge-base", "--all", source, target, check=False)
    if result.returncode != 0:
        return base
    bases = result.stdout.decode().split()
    if not bases or not any(ancestor(repo, base, shared) for shared in bases):
        return base
    shared = bases[0] if len(bases) == 1 else virtual_base(repo, source, target)
    if shared and shared != base:
        return shared
    return base


def lost_hunks(repo, base, source, candidate, proof=None):
    """Report exact base-state restorations; allow independently evolved content."""
    patch = git(repo, "diff", "--no-ext-diff", "--no-renames", "--binary",
                "--unified=0", base, source).stdout
    if not patch:
        return []
    errors = []
    renames = renamed_paths(repo, source, candidate)
    proved_sites = {}
    proved_paths = {}
    if proof is not None:
        for site in proof["sites"]:
            proved_sites.setdefault(site["path"], []).append(site)
            previous = proved_paths.setdefault(site["path"], site["target_path"])
            if previous != site["target_path"]:
                raise GuardError("one source file has conflicting proved destinations")
    for path in paths_changed(repo, base, source):
        candidate_path = proved_paths.get(path, renames.get(path, path))
        before = blob(repo, base, path)
        after = blob(repo, source, path)
        now = blob(repo, candidate, candidate_path)
        if proof is not None and path == "tools/released-migrations.json":
            # The inventory is a derived fingerprint catalog. The validator
            # above checked source, target, transformed block and candidate.
            after = now
        elif after is not None and path in proved_sites:
            for site in proved_sites[path]:
                old, new = site["before"].encode(), site["after"].encode()
                if after.count(old) != 1:
                    raise GuardError(f"proved source site is ambiguous: {path}")
                after = after.replace(old, new, 1)
        if after is None:
            if now is not None and now == before:
                errors.append(f"{candidate_path}: accepted file deletion restored base blob "
                              f"sha256:{hashlib.sha256(now).hexdigest()}")
            continue
        if now is None:
            # A removed source path may have been renamed and edited beyond Git's
            # similarity threshold. Absence alone cannot prove a stale rollback.
            continue
        if b"\0" in after or b"\0" in (before or b"") or b"\0" in now:
            if before is not None and before != after and now == before:
                errors.append(f"{candidate_path}: accepted binary edit reverted to base blob "
                              f"sha256:{hashlib.sha256(before).hexdigest()}")
            continue
        old_lines = (before or b"").splitlines(keepends=True)
        new_lines = after.splitlines(keepends=True)
        candidate_lines = now.splitlines(keepends=True)
        matches = [(old_start, old_end, candidate_start, candidate_end)
                   for tag, old_start, old_end, candidate_start, candidate_end
                   in difflib.SequenceMatcher(None, old_lines, candidate_lines,
                                               autojunk=False).get_opcodes()
                   if tag == "equal"]
        source_matches = difflib.SequenceMatcher(
            None, new_lines, candidate_lines, autojunk=False).get_matching_blocks()
        for tag, old_start, old_end, new_start, new_end in difflib.SequenceMatcher(
                None, old_lines, new_lines, autojunk=False).get_opcodes():
            if tag == "equal":
                continue
            if old_start == old_end:
                # The base boundary can map to another copy of a repeated
                # attribute. Only a source-to-candidate match that also keeps
                # neighboring context proves this insertion survived.
                if insertion_survives_with_context(
                        source_matches, len(new_lines), new_start, new_end):
                    continue
                boundary = missing_insertion_boundary(
                    matches, len(old_lines), len(candidate_lines), old_start)
                if boundary is not None:
                    errors.append(f"{candidate_path}:{boundary + 1}: accepted insertion "
                                  f"{line_text(new_lines[new_start])} from "
                                  f"{path}:{new_start + 1} is missing at its base boundary")
                continue
            for base_start, base_end, candidate_start, _ in matches:
                if base_start <= old_start and old_end <= base_end:
                    candidate_line = candidate_start + old_start - base_start + 1
                    accepted = (f"; replaced by accepted {line_text(new_lines[new_start])}"
                                if new_start < new_end else "; accepted deletion was lost")
                    errors.append(f"{candidate_path}:{candidate_line}: restored base line "
                                  f"{line_text(old_lines[old_start])} from "
                                  f"{path}:{old_start + 1}{accepted}")
                    break
    return errors


def load_toml_module():
    """Return a TOML parser: stdlib tomllib (Python >= 3.11) or tomli."""
    for name in ("tomllib", "tomli"):
        try:
            return importlib.import_module(name)
        except ModuleNotFoundError:
            continue
    raise GuardError(
        "no TOML parser: Python >= 3.11 (tomllib) or the tomli package is required; "
        f"this is Python {sys.version_info.major}.{sys.version_info.minor}"
    )


def affected_crates(repo, target, candidate):
    packages = set()
    for path in paths_changed(repo, target, candidate):
        parts = Path(path).parts
        if len(parts) < 3 or parts[0] != "crates":
            continue
        manifest = "/".join(parts[:2]) + "/Cargo.toml"
        contents = blob(repo, candidate, manifest) or blob(repo, target, manifest)
        if contents is None:
            raise GuardError(f"missing crate manifest: {manifest}")
        package = load_toml_module().loads(contents.decode())["package"]["name"]
        packages.add(package)
    return sorted(packages)


def test_commands(packages, filters):
    selected = {}
    for item in filters:
        package, separator, test_filter = item.partition("=")
        if not separator or not package or not test_filter or test_filter.startswith("-"):
            raise GuardError(f"invalid test filter (expected package=filter): {item}")
        selected.setdefault(package, []).append(test_filter)

    def command(package, test_filter):
        # bin:NAME / test:NAME select a cargo target rather than a lib filter.
        if test_filter.startswith("bin:"):
            return ["cargo", "test", "-p", package, "--bin", test_filter[4:]]
        if test_filter.startswith("test:"):
            return ["cargo", "test", "-p", package, "--test", test_filter[5:]]
        return ["cargo", "test", "-p", package, "--lib", *([test_filter] if test_filter else [])]

    # A filter may name a package the diff did not touch (a reader of a changed
    # doc or script); the Rust lander validates it against workspace members.
    gated = list(packages) + [name for name in selected if name not in packages]
    return [command(package, test_filter)
            for package in gated for test_filter in selected.get(package, [""])]


def run_tests(candidate, worktree, commands):
    head = git(worktree, "rev-parse", "HEAD").stdout.decode().strip()
    if head != candidate:
        raise GuardError("test worktree HEAD does not equal candidate")
    if git(worktree, "status", "--porcelain=v1", "-z").stdout:
        raise GuardError("test worktree is dirty")
    env = safe_env()
    env["CARGO_BUILD_JOBS"] = "6"
    env["CARGO_PROFILE_DEV_DEBUG"] = "line-tables-only"
    for args in commands:
        print("running:", " ".join(args), flush=True)
        result = subprocess.run(args, cwd=worktree, env=env, check=False)
        if result.returncode:
            raise GuardError(f"affected-crate tests failed: {' '.join(args)}")


def check_target_reclaim(repo, candidate):
    """Run the candidate's static guard on committed inputs, before any build.

    Like the pre-push guards, this uses an exported tree so dirty worktree
    changes cannot hide a bad writer. Older candidate trees predate the guard.
    """
    script = "scripts/check-target-reclaim.py"
    if blob(repo, candidate, script) is None:
        return
    paths = [path for path in ("crates", "scripts", "desktop/scripts")
             if git(repo, "cat-file", "-e", f"{candidate}:{path}", check=False).returncode == 0]
    archive = git(repo, "archive", candidate, *paths).stdout
    with tempfile.TemporaryDirectory(prefix="rsi-target-reclaim-guard-") as temporary:
        root = Path(temporary)
        with tarfile.open(fileobj=io.BytesIO(archive)) as tree:
            for entry in tree:
                if not entry.isfile():
                    continue
                relative = Path(entry.name)
                if relative.is_absolute() or ".." in relative.parts:
                    raise GuardError("unsafe path in candidate archive")
                dest = root / relative
                dest.parent.mkdir(parents=True, exist_ok=True)
                dest.write_bytes(tree.extractfile(entry).read())
        result = subprocess.run([sys.executable, str(root / script)], text=True, capture_output=True)
        if result.returncode:
            raise GuardError("check-target-reclaim failed: " + (result.stdout + result.stderr).strip())


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--repo", type=Path, default=Path.cwd())
    parser.add_argument("--base", required=True, help="accepted source's base commit")
    parser.add_argument("--source", required=True, help="accepted source commit")
    parser.add_argument("--target", required=True, help="rolling tip used to prepare candidate")
    parser.add_argument("--candidate", required=True, help="candidate commit to publish")
    parser.add_argument("--worktree", type=Path, help="clean candidate worktree for cargo tests")
    parser.add_argument("--plan-only", action="store_true", help="report tests without running them")
    parser.add_argument("--test-filter", action="append", default=[], metavar="PACKAGE=FILTER",
                        help="run a focused filter for an affected crate; repeatable")
    parser.add_argument("--proof", type=Path,
                        help="exact-source provisional migration proof JSON")
    args = parser.parse_args()
    try:
        repo = Path(git(args.repo, "rev-parse", "--show-toplevel").stdout.decode().strip())
        for oid in (args.base, args.source, args.target, args.candidate):
            commit(repo, oid)
        if not ancestor(repo, args.base, args.source):
            raise GuardError("accepted base is not an ancestor of source")
        if not ancestor(repo, args.source, args.candidate):
            raise GuardError("accepted source is not an ancestor of candidate")
        if not ancestor(repo, args.target, args.candidate):
            raise GuardError("target tip is not an ancestor of candidate")
        check_target_reclaim(repo, args.candidate)
        proof = (validate_renumber_proof(repo, args.base, args.source, args.candidate,
                                         args.proof, args.target)
                 if args.proof else None)
        # A provisional-migration proof is keyed to the accepted base's delta.
        base = args.base if proof is not None else own_base(
            repo, args.base, args.source, args.target)
        missing = lost_hunks(repo, base, args.source, args.candidate, proof)
        packages = affected_crates(repo, args.target, args.candidate)
        commands = test_commands(packages, args.test_filter)
        unchanged = (git(repo, "rev-parse", f"{args.target}^{{tree}}").stdout
                     == git(repo, "rev-parse", f"{args.candidate}^{{tree}}").stdout)
        report = {"outcome": "already_on_rolling" if unchanged and not missing else "candidate",
                  "accepted_source": args.source, "candidate": args.candidate,
                  "lost_hunks": missing, "affected_crates": packages,
                  "test_commands": [" ".join(command) for command in commands],
                  "tests_executed": False}
        if proof is not None:
            report["provisional_migration"] = {
                "assigned_version": proof["assigned_version"],
                "unit_candidate": proof["unit_candidate"],
                "proof_sha256": "sha256:" + hashlib.sha256(args.proof.read_bytes()).hexdigest(),
            }
        if not args.plan_only:
            run_tests(args.candidate, args.worktree or repo, commands)
            report["tests_executed"] = True
        print(json.dumps(report, indent=2))
        return 1 if missing else 0
    except (GuardError, OSError, KeyError, UnicodeError) as error:
        print(f"landing guard: {error}", file=sys.stderr)
        return 2


if __name__ == "__main__":
    sys.exit(main())
