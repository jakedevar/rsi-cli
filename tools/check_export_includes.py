#!/usr/bin/env python3
"""Refuse Rust includes of paths the public rsi-cli export excludes.

scripts/export-rsi-cli.sh removes the EXCLUDES paths from the exported tree, so
an include_str!/include_bytes!/include!/#[path] that reaches into one breaks the
public build. The exclusion list is read from the export script itself.

Usage: tools/check_export_includes.py [--root DIR]   (exit 1 on any finding)
"""
import os
import re
import subprocess
import sys

INCLUDE = re.compile(
    r'(?:\b(?:include_str|include_bytes|include)!\s*\(\s*|#\[\s*path\s*=\s*)"([^"]+)"'
)


def read_excludes(root):
    text = open(os.path.join(root, "scripts", "export-rsi-cli.sh")).read()
    m = re.search(r"^EXCLUDES=\(\n(.*?)^\)", text, re.S | re.M)
    if not m:
        raise SystemExit("check_export_includes: EXCLUDES array not found")
    return [l.strip().rstrip("/") for l in m.group(1).splitlines() if l.strip()]


def is_excluded(rel, excludes):
    return any(rel == e or rel.startswith(e + "/") for e in excludes)


def scan(root, files, excludes):
    findings = []
    for f in files:
        if not f.endswith(".rs") or is_excluded(f, excludes):
            continue
        try:
            text = open(os.path.join(root, f), encoding="utf-8").read()
        except (OSError, UnicodeDecodeError):
            continue
        for m in INCLUDE.finditer(text):
            target = os.path.normpath(os.path.join(os.path.dirname(f), m.group(1)))
            if is_excluded(target, excludes):
                line = text.count("\n", 0, m.start()) + 1
                findings.append(f"{f}:{line}: includes excluded path {target}")
    return findings


def main(argv):
    root = "."
    if argv[:1] == ["--root"]:
        root = argv[1]
    else:
        root = subprocess.check_output(
            ["git", "rev-parse", "--show-toplevel"], text=True
        ).strip()
    files = subprocess.check_output(
        ["git", "-C", root, "ls-files"], text=True
    ).splitlines()
    findings = scan(root, files, read_excludes(root))
    for line in findings:
        print(line)
    if findings:
        print("export-rsi-cli.sh excludes these paths; move the file to a public path.")
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
