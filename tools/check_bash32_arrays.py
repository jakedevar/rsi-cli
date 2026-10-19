#!/usr/bin/env python3
"""Flag unguarded empty-array expansions under `set -u` (macOS /bin/bash 3.2).

bash < 4.4 treats `"${arr[@]}"` / `"${arr[*]}"` of an empty array as an unbound
variable under `set -u`. A script that runs on the macOS satellite must expand a
possibly-empty array as `${arr[@]+"${arr[@]}"}` (or guard on `${#arr[@]}`).

An array is "possibly empty" when the file declares it empty (`arr=()`,
`local -a arr`, `declare -a arr`) or fills it with `mapfile`/`read -a`. An array
whose only declaration is a non-empty literal (`arr=(a b)`, then `+=`) is safe. An expansion is allowed when it uses the
`+` guard form, or carries `# bash32-safe` on its line or the line above (use it
when a preceding length check proves the array is non-empty).

Linux-only scripts (systemd/journalctl hosts such as satellite/install-direct.sh) are
not in the audited set.

Usage: tools/check_bash32_arrays.py [paths...]   (default: the audited macOS set)
Exit 1 and name file:line:array for each finding.
"""

import re
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
AUDITED = [
    "scripts/rsi-stash",
    "scripts/fmt-changed.sh",
    "scripts/cargo-slot",
    "scripts/cargo-slot-wrapper",
    "scripts/rsid-launchd.sh",
    "scripts/rsid-supervisor.sh",
    "scripts/install-release.sh",
    "scripts/release-target-guard.sh",
    "scripts/clean-target",
    "scripts/rsi-scratch-mkdir",
    "scripts/e2e.sh",
    "scripts/sync-agent-commands.sh",
    "scripts/render-project-index.sh",
    "scripts/rsi-ssh-tui.sh",
    "scripts/cleanup",
    "scripts/ios",
    "scripts/satellite/dial-home.sh",
    "scripts/satellite/direct.sh",
    "scripts/satellite/install.sh",
]
SET_U = re.compile(r"^\s*set\s+-[A-Za-z]*u|^\s*set\s+-o\s+nounset", re.M)
EXPAND = re.compile(r"\$\{([A-Za-z_][A-Za-z0-9_]*)\[([@*])\]\}")
ASSIGN_LITERAL = re.compile(r"^\s*(?:local\s+|declare\s+(?:-[a-z]+\s+)?|readonly\s+)?([A-Za-z_][A-Za-z0-9_]*)=\((.*)")
DECL_ONLY = re.compile(r"^\s*(?:local|declare|typeset)\s+-a\s+([A-Za-z_][A-Za-z0-9_ ]*)\s*(?:#.*)?$")
FILL_CMD = re.compile(r"\b(?:mapfile|readarray)\b[^\n]*?\s([A-Za-z_][A-Za-z0-9_]*)\s*(?:<|$)|\bread\s+(?:-[a-zA-Z]+\s+)*-a\s+([A-Za-z_][A-Za-z0-9_]*)")
MARKER = "bash32-safe"


def possibly_empty_arrays(lines):
    empty, nonempty = set(), set()
    for raw in lines:
        line = raw.split("#", 1)[0]
        m = ASSIGN_LITERAL.match(line)
        if m:
            name, rest = m.group(1), m.group(2).strip()
            (empty if rest.startswith(")") else nonempty).add(name)
        m = DECL_ONLY.match(line)
        if m:
            empty.update(m.group(1).split())
        for m in FILL_CMD.finditer(line):
            empty.add(m.group(1) or m.group(2))
    return empty


def scan_text(text, name="<text>"):
    if not SET_U.search(text):
        return []
    lines = text.splitlines()
    empty = possibly_empty_arrays(lines)
    findings = []
    for number, raw in enumerate(lines, 1):
        if raw.lstrip().startswith("#"):
            continue
        code = raw
        previous = lines[number - 2] if number > 1 else ""
        if MARKER in raw or MARKER in previous:
            continue
        for m in EXPAND.finditer(code):
            array = m.group(1)
            if array not in empty:
                continue
            # `${arr[@]+"${arr[@]}"}`: the expansion we found is the inner one.
            if code[:m.start()].endswith(f"${{{array}[@]+\"") or code[:m.start()].endswith(f"${{{array}[@]+"):
                continue
            findings.append((name, number, array, raw.strip()))
    return findings


def expand_paths(paths):
    for item in paths:
        path = (ROOT / item) if not Path(item).is_absolute() else Path(item)
        if path.is_dir():
            yield from sorted(p for p in path.rglob("*") if p.is_file())
        elif path.is_file():
            yield path


def is_bash(path):
    try:
        head = path.read_text(errors="replace").splitlines()[:1]
    except OSError:
        return False
    return bool(head) and head[0].startswith("#!") and "bash" in head[0]


def main(argv):
    targets = argv or AUDITED
    findings = []
    for path in expand_paths(targets):
        if not (path.suffix == ".sh" or is_bash(path)):
            continue
        rel = path.relative_to(ROOT) if path.is_relative_to(ROOT) else path
        findings += scan_text(path.read_text(errors="replace"), str(rel))
    for name, number, array, line in findings:
        print(f"{name}:{number}: unguarded empty-array expansion of '{array}' under set -u "
              f"(bash 3.2): use ${{{array}[@]+\"${{{array}[@]}}\"}} or mark '# {MARKER}'\n    {line}")
    return 1 if findings else 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
