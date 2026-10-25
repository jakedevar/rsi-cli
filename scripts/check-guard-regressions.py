#!/usr/bin/env python3
"""Compare changed static guards on the same committed head tree.

See docs/guard-regressions.md for adapters and exact, reasoned loss declarations.
Only stdlib and Git are required. No checkout, index or worktree is changed.
"""

import argparse
import ast
import contextlib
import io
import json
from pathlib import Path
import re
import subprocess
import sys
import tempfile
import types

DECLARATIONS = "scripts/guard-regressions-intended.json"
GUARD = re.compile(r"^(scripts|tools)/check[-_][^/]+\.py$")
STDOUT = re.compile(r"^# guard-regressions: stdout\s*$", re.M)
DIAGNOSTIC = re.compile(r"^(.+?):([1-9][0-9]*):\s*(\S.*)$")
SOURCE_SUFFIXES = {"", ".rs", ".py", ".sh", ".js", ".mjs", ".cjs", ".ts", ".tsx"}


def git(repo, *args):
    return subprocess.run(["git", "-C", str(repo), *args], check=True, capture_output=True).stdout


def blob(repo, ref, path):
    result = subprocess.run(["git", "-C", str(repo), "show", f"{ref}:{path}"], capture_output=True)
    if result.returncode:
        return None
    return result.stdout


def export_tree(repo, head, root):
    # Read exact Git blobs, ignoring export-ignore/export-subst attributes. Never
    # follow symlinks, materialize gitlinks, or consult the mutable worktree.
    files = []
    for entry in git(repo, "ls-tree", "-rz", head).split(b"\0"):
        if not entry:
            continue
        metadata, name = entry.split(b"\t", 1)
        mode, kind, oid = metadata.split()
        path = Path(name.decode("utf-8", "surrogateescape"))
        if path.is_absolute() or ".." in path.parts:
            raise ValueError("unsafe head tree path")
        if kind == b"blob" and mode in (b"100644", b"100755"):
            files.append((path, mode, oid))
    if not files:
        return
    objects = subprocess.run(["git", "-C", str(repo), "cat-file", "--batch"],
                             input=b"".join(oid + b"\n" for _, _, oid in files),
                             capture_output=True, check=True).stdout
    stream = io.BytesIO(objects)
    for path, mode, oid in files:
        actual, kind, size = stream.readline().split()
        if actual != oid or kind != b"blob":
            raise ValueError("unexpected head blob")
        dest = root / path
        dest.parent.mkdir(parents=True, exist_ok=True)
        dest.write_bytes(stream.read(int(size)))
        if stream.read(1) != b"\n":
            raise ValueError("truncated head blob")
        if mode == b"100755":
            dest.chmod(0o755)


def test_path(path):
    path = Path(path)
    return (any(part in {"tests", "fixtures", "__tests__"} or part.endswith("_tests") for part in path.parts)
            or path.stem == "tests" or path.name.startswith("test_") or path.stem.endswith(("_test", "_tests", ".test", ".spec")))


# Mask Rust strings/comments before balancing test-item braces. Keep byte offsets
# and newlines; raw strings, lifetimes and nested block comments matter here.
RUST_LITERAL = re.compile(
    r'//[^\n]*|/\*|\b(?:br|rb|r)(?P<hashes>\#*)"[\s\S]*?"(?P=hashes)'
    r'|b?"(?:\\[\s\S]|[^"\\])*"'
    r"|'(?:\\(?:u\{[0-9a-fA-F_]+\}|x[0-9a-fA-F]{2}|.)|[^'\\\n])'")


def rust_test_lines(source):
    masked = list(source)
    at = 0
    while match := RUST_LITERAL.search(source, at):
        end = match.end()
        if match[0] == "/*":
            depth = 1
            while depth:
                token = re.search(r"/\*|\*/", source[end:])
                if not token:
                    raise ValueError("unterminated Rust comment")
                end += token.end()
                depth += 1 if token[0] == "/*" else -1
        for offset in range(match.start(), end):
            if source[offset] != "\n":
                masked[offset] = " "
        at = end
    code = "".join(masked)
    hidden = set()
    for attr in re.finditer(r"#!?\[\s*cfg\(\s*test\s*\)\s*\]|#\[\s*(?:test|tokio::test)(?:\([^]]*\))?\s*\]", code):
        start_line = source.count("\n", 0, attr.start()) + 1
        if attr[0].startswith("#!"):
            prefix = code[:attr.start()]
            if prefix.count("{") == prefix.count("}"):
                return set(range(1, source.count("\n") + 2))
            continue
        line_start = code.rfind("\n", 0, attr.start()) + 1
        if code[line_start:attr.start()].strip():
            start_line += 1
        at = attr.end()
        # Skip other attributes before finding this item's body.
        while extra := re.match(r"\s*#\[[^\]]*\]", code[at:]):
            at += extra.end()
        body = re.search(r"[;{]", code[at:])
        if body is None:
            raise ValueError("cannot locate Rust test item")
        end = at + body.end()
        if body[0] == "{":
            depth = 1
            while depth and end < len(code):
                depth += (code[end] == "{") - (code[end] == "}")
                end += 1
            if depth:
                raise ValueError("unterminated Rust test item")
        # A line shared with production is conservatively production.
        end_line = source.count("\n", 0, end) + 1
        if code[end:code.find("\n", end) if "\n" in code[end:] else len(code)].strip():
            end_line -= 1
        hidden.update(range(start_line, end_line + 1))
    return hidden


def validate_finding(path, line, finding):
    if not isinstance(path, str):
        raise ValueError("finding path must be a string")
    relative = Path(path)
    if (relative.is_absolute() or ".." in relative.parts or not relative.parts
            or not isinstance(line, int) or isinstance(line, bool) or line < 1
            or not isinstance(finding, str) or not finding.strip()):
        raise ValueError(f"invalid finding: {path}:{line}: {finding}")
    return (relative.as_posix(), line, finding)


def has_entries(text):
    try:
        tree = ast.parse(text)
    except SyntaxError:
        return False
    return any(isinstance(node, ast.FunctionDef) and node.name == "entries"
               and len(node.args.args) == 1 for node in tree.body)


def collect(source, root, relative, adapter):
    """Worker entrypoint: execute each guard version in a separate interpreter."""
    text = source.read_text(encoding="utf-8")
    if adapter == "stdout":
        # Run as __main__, preserving __file__ beside head-tree helper modules.
        command = [sys.executable, "-c",
                   "import os, sys; p, f, root = sys.argv[1:]; sys.path.insert(0, os.path.dirname(f)); "
                   "sys.argv = [f, '--root', root]; "
                   "exec(compile(open(p).read(), f, 'exec'), {'__name__': '__main__', '__file__': f})",
                   str(source), str(root / relative), str(root)]
        result = subprocess.run(command, cwd=root, text=True, capture_output=True, timeout=90)
        if result.returncode not in (0, 1):
            raise ValueError(f"guard exited {result.returncode}: {result.stderr}")
        findings = []
        for line in (result.stdout + result.stderr).splitlines():
            match = DIAGNOSTIC.fullmatch(line)
            if not match:
                raise ValueError(f"unstable guard diagnostic: {line!r}")
            findings.append(validate_finding(match[1], int(match[2]), match[3]))
        if result.returncode == 1 and not findings:
            raise ValueError("guard exited 1 without findings")
        return findings
    if not has_entries(text):
        raise ValueError(f"{relative}: needs entries(text) or '# guard-regressions: stdout'; "
                         "see docs/guard-regressions.md")
    module = types.ModuleType("guard_under_comparison")
    module.__file__ = str(root / relative)
    sys.modules[module.__name__] = module  # dataclasses resolve their defining module
    sys.path.insert(0, str((root / relative).parent))
    with contextlib.redirect_stdout(io.StringIO()):
        exec(compile(text, module.__file__, "exec"), module.__dict__)
        findings = []
        for path in sorted(root.rglob("*")):
            if not path.is_file() or path.suffix not in SOURCE_SUFFIXES:
                continue
            try:
                content = path.read_text(encoding="utf-8")
            except UnicodeError:
                continue
            for line, finding in module.entries(content):
                findings.append(validate_finding(path.relative_to(root).as_posix(), line, finding))
    return findings


def run_guard(source, root, relative, adapter):
    result = subprocess.run([sys.executable, str(Path(__file__).resolve()), "--collect",
                             str(source), str(root), relative, adapter], cwd=root,
                            text=True, capture_output=True, timeout=120)
    if result.returncode:
        raise ValueError(f"{relative}: comparison failed: {result.stderr.strip()}")
    return {validate_finding(*entry) for entry in json.loads(result.stdout)}


def compare(repo, base, head, guards=None):
    # Resolve once: a moving ref must not mix trees or declaration versions.
    base = git(repo, "rev-parse", "--verify", f"{base}^{{commit}}").decode().strip()
    head = git(repo, "rev-parse", "--verify", f"{head}^{{commit}}").decode().strip()
    if guards is None:
        guards = [p.decode() for p in git(repo, "diff", "--name-only", "--no-renames", "-z", base, head).split(b"\0")
                  if p and GUARD.fullmatch(p.decode())]
    raw = blob(repo, head, DECLARATIONS)
    declarations = json.loads(raw) if raw is not None else []
    keys = ("guard", "base_guard", "head_guard", "path", "line", "finding")
    if not isinstance(declarations, list):
        raise ValueError(f"{DECLARATIONS} must be a list")
    intended = set()
    for entry in declarations:
        if (not isinstance(entry, dict) or set(entry) != {*keys, "reason"}
                or not isinstance(entry["reason"], str) or not entry["reason"].strip()):
            raise ValueError(f"{DECLARATIONS}: each exact loss needs {', '.join(keys)} and a reason")
        validate_finding(entry["path"], entry["line"], entry["finding"])
        if (not isinstance(entry["guard"], str) or not GUARD.fullmatch(entry["guard"])
                or not isinstance(entry["base_guard"], str)
                or not re.fullmatch(r"[0-9a-f]{40}|[0-9a-f]{64}", entry["base_guard"])
                or (entry["head_guard"] is not None and (not isinstance(entry["head_guard"], str)
                    or not re.fullmatch(r"[0-9a-f]{40}|[0-9a-f]{64}", entry["head_guard"])))):
            raise ValueError(f"{DECLARATIONS}: invalid guard path or blob hash")
        intended.add(tuple(entry[k] for k in keys))
    if not guards:
        return []
    with tempfile.TemporaryDirectory(prefix="rsi-guard-regressions-") as temporary:
        root = Path(temporary) / "tree"
        root.mkdir()
        export_tree(repo, head, root)
        losses = []
        for index, relative in enumerate(sorted(set(guards))):
            if not GUARD.fullmatch(relative):
                raise ValueError(f"not a static guard: {relative}")
            before, after = blob(repo, base, relative), blob(repo, head, relative)
            if before is None or before == after:
                continue
            adapter = "stdout" if any(STDOUT.search(s.decode()) for s in (before, after) if s) else "entries"
            # Either supported version keeps the comparison gated: removing an
            # existing adapter must not turn a supported guard into a skip.
            if adapter == "entries" and not any(has_entries(s.decode()) for s in (before, after) if s):
                print(f"unsupported: {relative} (no entries() or stdout opt-in; not compared)", file=sys.stderr)
                continue
            results = []
            hashes = []
            for version, source in enumerate((before, after)):
                if source is None:
                    results.append(set())
                    hashes.append(None)
                    continue
                hashes.append(subprocess.run(["git", "-C", str(repo), "hash-object", "--stdin"],
                                             input=source, check=True, capture_output=True).stdout.decode().strip())
                script = Path(temporary) / f"guard-{index}-{version}.py"
                script.write_bytes(source)
                results.append(run_guard(script, root, relative, adapter))
            hidden = {}
            for path, line, finding in sorted(results[0] - results[1]):
                if test_path(path):
                    continue
                if path.endswith(".rs"):
                    if path not in hidden:
                        hidden[path] = rust_test_lines((root / path).read_text())
                    if line in hidden[path]:
                        continue
                loss = dict(zip(keys, (relative, *hashes, path, line, finding)))
                loss["intended"] = tuple(loss[k] for k in keys) in intended
                losses.append(loss)
        return losses


def main():
    if len(sys.argv) == 6 and sys.argv[1] == "--collect":
        print(json.dumps(collect(Path(sys.argv[2]), Path(sys.argv[3]), sys.argv[4], sys.argv[5])))
        return 0
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("base")
    parser.add_argument("head", nargs="?", default="HEAD")
    parser.add_argument("--repo", type=Path, default=Path.cwd())
    parser.add_argument("--guard", action="append", help="compare only this guard (also for historical replays)")
    args = parser.parse_args()
    try:
        losses = compare(args.repo.resolve(), args.base, args.head, args.guard)
    except (OSError, ValueError, subprocess.SubprocessError) as error:
        print(f"guard regressions: {error}", file=sys.stderr)
        return 2
    for loss in losses:
        state = "intended" if loss["intended"] else "UNDECLARED"
        print(f"{loss['guard']}: lost production finding {loss['path']}:{loss['line']}: "
              f"{loss['finding']} ({state})")
        if not loss["intended"]:
            declaration = {k: v for k, v in loss.items() if k != "intended"}
            declaration["reason"] = "REPLACE with reviewable reason"
            print(f"  declare in {DECLARATIONS}: {json.dumps(declaration, sort_keys=True)}")
    return 1 if any(not loss["intended"] for loss in losses) else 0


if __name__ == "__main__":
    sys.exit(main())
