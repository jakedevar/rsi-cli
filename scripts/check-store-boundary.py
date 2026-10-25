#!/usr/bin/env python3
"""Guard rsid's SQLite ownership boundary (#453).

This is a cheap source guard, not Rust type checking. It checks tracked Rust
sources (or an exported archive), ignores comments/literals and test items, and
requires literal/constant daemon_settings keys outside rsid-store. Computed
keys, including helper calls, and direct rsi.db connections belong in rsid-store.
"""

import argparse
from collections import Counter
from functools import lru_cache
from pathlib import Path
import re
import runpy
import subprocess
import sys


FIX = "add an rsid-store accessor or derive the state from existing records"
RSID = "crates/rsid/src/"
# Reviewed separate databases only; never allow an entire codegraph subtree.
# memory_config.db_path defaults to memory.sqlite (rsid-store/src/config.rs).
SEPARATE_STORES = {RSID + "memory/store.rs"}
# codegraph's current raw connections are test-only. rsi-db-snapshot already
# calls store::migration_backup::snapshot_database, so neither needs an exemption.

# All legacy settings writers and fixture connections moved behind accessors (#457).
LEGACY_OPENS = {}
LEGACY_KEYS = {}


@lru_cache(maxsize=1)
def masker():
    # Reuse the existing Rust comment/string lexer, including nested comments,
    # raw strings and lifetimes. It preserves offsets for actionable line numbers.
    return runpy.run_path(str(Path(__file__).with_name("check-rsid-test-shards.py")))[
        "mask_literals_and_comments"]


def closing(code, start):
    """End of one balanced group (strings/comments have already been masked)."""
    stack = []
    pairs = {"(": ")", "[": "]", "{": "}"}
    for at in range(start, len(code)):
        char = code[at]
        if char in pairs:
            stack.append(pairs[char])
        elif stack and char == stack[-1]:
            stack.pop()
            if not stack:
                return at + 1
    raise ValueError("unbalanced Rust group")


def production(source, path):
    code = masker()(source, path)
    hidden = []
    # Conventional external test modules; inline test items are removed below.
    name = Path(path).name
    if "tests" in Path(path).parts or name == "tests.rs" or name.endswith("_tests.rs"):
        return " " * len(source), [(0, len(source))]
    for attr in re.finditer(r"#!?\[\s*cfg\(\s*test\s*\)\s*\]|#\[\s*(?:test|tokio::test)(?:\([^]]*\))?\s*\]", code):
        if code[attr.start():].startswith("#!"):
            return " " * len(source), [(0, len(source))]
        at = attr.end()
        while at < len(code):
            if code[at].isspace():
                at += 1
            elif code.startswith("#[", at):
                at = closing(code, at + 1)
            else:
                break
        item = re.search(r"[;{]", code[at:])
        if item:
            start = at + item.start()
            end = closing(code, start) if code[start] == "{" else start + 1
            hidden.append((attr.start(), end))
    for start, end in reversed(hidden):
        code = code[:start] + re.sub(r"[^\n]", " ", code[start:end]) + code[end:]
    return code, hidden


LITERAL = re.compile(
    r'//[^\n]*|/\*|\b(?:br|rb|r)(?P<hashes>\#*)"[\s\S]*?"(?P=hashes)'
    r'|b?"(?:\\[\s\S]|[^"\\])*"'
    r"|'(?:\\(?:u\{[0-9a-fA-F_]+\}|x[0-9a-fA-F]{2}|.)|[^'\\\n])'"
)


def sql_literals(source):
    """Yield real string literals, skipping comments (including nested blocks)."""
    at = 0
    while match := LITERAL.search(source, at):
        at = match.end()
        token = match[0]
        if token == "/*":
            depth = 1
            while depth:
                marker = re.search(r"/\*|\*/", source[at:])
                if marker is None:
                    raise ValueError("unterminated block comment")
                at += marker.end()
                depth += 1 if marker[0] == "/*" else -1
        elif not token.startswith(("//", "'")):
            yield match.start(), token


def compact(text):
    return re.sub(r"\s+", "", text)


def first_argument(source, code, start, end):
    at = start
    while at < end:
        if code[at] in "([{":
            at = closing(code, at)
        elif code[at] == ",":
            break
        else:
            at += 1
    return source[start:at].strip()


def scan(path, source):
    code, test_ranges = production(source, path)
    findings = []
    opens, keys = Counter(), Counter()

    def report(at, rule):
        findings.append(f"{path}:{source.count(chr(10), 0, at) + 1}: "
                        f"store boundary rule: {rule}; {FIX}")

    if path.startswith(RSID) and path not in SEPARATE_STORES:
        # Cover fully qualified calls, imported Connection, and explicit aliases.
        aliases = ["Connection"] + re.findall(r"\bConnection\s+as\s+(\w+)", code)
        pattern = rf"\b(?:rusqlite\s*::\s*)?(?:{'|'.join(aliases)})\s*::\s*open\w*\s*\("
        for call in re.finditer(pattern, code):
            end = closing(code, call.end() - 1)
            signature = compact(source[call.start():end])
            opens[signature] += 1
            if opens[signature] > LEGACY_OPENS.get((path, signature), 0):
                report(call.start(), "rsid must not open raw SQLite connections to rsi.db")

    # rsid-store owns table accessors, including the settings implementation.
    if path.startswith("crates/rsid-store/"):
        return findings
    for call in re.finditer(r"\bset_daemon_setting\s*\(", code):
        end = closing(code, call.end() - 1)
        key = first_argument(source, code, call.end(), end - 1)
        signature = compact(key)
        # A string literal or named constant has bounded cardinality. Every
        # other expression (format!/concat!/+, uuid, helper or variable) needs
        # an accessor, so local aliases cannot hide a runtime-built key.
        if re.fullmatch(r'&?\s*(?:"(?:[^"\\]|\\.)*"|(?:[\w]+::)*[A-Z][A-Z0-9_]*)', key, re.S):
            continue
        keys[signature] += 1
        if keys[signature] > LEGACY_KEYS.get((path, signature), 0):
            report(call.start(), "daemon_settings keys must not be built at runtime outside rsid-store")

    # A raw SQL writer bypasses the accessor regardless of how its key was
    # assembled. Inspect only literals in production, never comments or tests.
    for at, literal in sql_literals(source):
        if any(start <= at < end for start, end in test_ranges):
            continue
        if re.search(r"\b(?:INSERT\s+(?:OR\s+\w+\s+)?INTO|REPLACE\s+INTO|UPDATE)\s+daemon_settings\b", literal, re.I):
            report(at, "daemon_settings writes must use an rsid-store accessor")
    return findings


def candidates(root):
    if (root / ".git").exists():
        result = subprocess.run(["git", "-C", str(root), "grep", "-l", "-z", "-E",
                                 "Connection|daemon_setting", "--", "crates/**/*.rs"],
                                capture_output=True)
        if result.returncode not in (0, 1):
            raise ValueError(result.stderr.decode(errors="replace"))
        return [root / item.decode("utf-8", "surrogateescape")
                for item in result.stdout.split(b"\0") if item]
    # pre-push checks a git archive: every file in it came from the pushed tree.
    return sorted((root / "crates").rglob("*.rs"))


def check(root):
    findings = []
    for path in candidates(root):
        source = path.read_text(encoding="utf-8")
        if "Connection" in source or "daemon_setting" in source:
            findings.extend(scan(path.relative_to(root).as_posix(), source))
    return findings


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--root", type=Path, default=Path(__file__).resolve().parents[1])
    args = parser.parse_args()
    try:
        findings = check(args.root.resolve())
    except (OSError, ValueError) as error:
        print(f"store boundary guard: {error}", file=sys.stderr)
        return 2
    for finding in findings:
        print(finding, file=sys.stderr)
    return int(bool(findings))


if __name__ == "__main__":
    sys.exit(main())
