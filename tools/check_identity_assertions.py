#!/usr/bin/env python3
"""Flag tests asserting that an entity's own identity string is absent from the UI.

See scripts/check-identity-assertions.sh for rationale. Narrow by construction:
a literal is only considered "identity" if the same file assigns it to an
identity-bearing field.
"""
import re
import sys
from pathlib import Path

IDENT_FIELDS = r"(?:title|name|query|display_title|short_summary)"
# `x.title = Some("Lit")`, `title: Some("Lit")`, `.title = "Lit"`, `title: "Lit"`
ASSIGN = re.compile(
    rf"\.?\b{IDENT_FIELDS}\b\s*[:=]\s*(?:Some\(\s*)?\"((?:[^\"\\]|\\.)+)\""
)
# `!<expr>.contains("Lit")` — the negation is what makes it an absence assertion.
ABSENCE = re.compile(
    r"(?<![A-Za-z0-9_])!\s*[A-Za-z0-9_\.\[\]&\*]*\.contains\(\s*\"((?:[^\"\\]|\\.)+)\""
)


def scan(path: Path):
    try:
        text = path.read_text(encoding="utf-8", errors="replace")
    except OSError:
        return []
    identities = {m.group(1) for m in ASSIGN.finditer(text)}
    if not identities:
        return []
    out = []
    for lineno, line in enumerate(text.splitlines(), 1):
        for m in ABSENCE.finditer(line):
            needle = m.group(1)
            if needle in identities:
                out.append((lineno, needle, line.strip()))
    return out


def main(argv):
    roots = [Path(a) for a in argv[1:]] or [Path("crates")]
    files = []
    for r in roots:
        files.extend([r] if r.is_file() else sorted(r.rglob("*.rs")))

    findings = 0
    for f in files:
        for lineno, needle, line in scan(f):
            findings += 1
            print(f"{f}:{lineno}: asserts an entity's own identity is absent: {needle!r}")
            print(f"    {line}")

    if findings:
        print()
        print(f"check-identity-assertions: {findings} finding(s).")
        print("A test must not require an entity's own name to be invisible.")
        print("Assert the positive end state instead (what the user SHOULD see).")
        print("If the omission is genuinely correct, assert the specific")
        print("replacement is present and name the test so the rationale is explicit.")
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
