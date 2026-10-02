#!/usr/bin/env python3
"""Fail on personal home paths / account names and public IP literals.

See scripts/check-personal-paths.sh for rationale and usage. Scans tracked
files (or the paths given), skipping `thoughts/` (historical records).
"""
import ipaddress
import re
import subprocess
import sys
from pathlib import Path

# Built by concatenation so this file does not trip its own scan.
PERSONAL = re.compile("jake" + "devar|jacob" + "devarennes")

# The whole allowlist. Each entry is deliberate and small:
#  * a GitHub repository slug is public project identity, not a home path;
ALLOWED_LINE = re.compile(r"github\.com/jake" + "devar/")
#  * well-known public resolver used as a "not a tailnet address" fixture.
ALLOWED_PUBLIC_IPS = {"8.8.8.8"}
#  * the egress classifier (#774) must name real public and range-boundary
#    addresses as test inputs; personal-path checks still apply there.
IP_FIXTURE_PATHS = {
    "crates/rsi-common/src/egress_policy.rs",
    "crates/rsid/src/session/harness/egress.rs",
}

SKIP_PREFIXES = ("thoughts/",)
SKIP_NAMES = {"Cargo.lock"}
SKIP_SUFFIXES = (".lock", ".svg", ".png", ".jpg", ".gif", ".ico", ".pdf", ".woff", ".woff2")

QUAD = re.compile(r"(?<![0-9.])([0-9]{1,3}(?:\.[0-9]{1,3}){3})(?![0-9])(?!\.[0-9])")


def tracked_files():
    out = subprocess.check_output(["git", "ls-files", "-z"])
    return [p for p in out.decode().split("\0") if p]


def skipped(path):
    return (
        path.startswith(SKIP_PREFIXES)
        or Path(path).name in SKIP_NAMES
        or path.endswith(SKIP_SUFFIXES)
    )


def public_ip(text):
    try:
        ip = ipaddress.IPv4Address(text)
    except ValueError:
        return False
    return ip.is_global and text not in ALLOWED_PUBLIC_IPS


def scan(path, check_ips=True):
    try:
        data = Path(path).read_bytes()
    except OSError:
        return []
    if b"\0" in data:
        return []
    out = []
    for lineno, line in enumerate(data.decode("utf-8", "replace").splitlines(), 1):
        stripped = ALLOWED_LINE.sub("", line)
        if PERSONAL.search(stripped):
            out.append((lineno, "personal path or account name"))
        for m in QUAD.finditer(line) if check_ips else ():
            if public_ip(m.group(1)):
                out.append((lineno, f"public IP literal {m.group(1)}"))
    return out


def main(argv):
    paths = argv[1:] or tracked_files()
    findings = 0
    for path in paths:
        norm = path[2:] if path.startswith("./") else path
        if skipped(norm):
            continue
        for lineno, what in scan(path, check_ips=norm not in IP_FIXTURE_PATHS):
            findings += 1
            print(f"{path}:{lineno}: {what}")
    if not findings:
        return 0
    print()
    print(f"check-personal-paths: {findings} finding(s).")
    print("Use $HOME, ~/, <repo>-relative paths, or a placeholder host such as")
    print("remote.example.net / 203.0.113.10. Historical records belong under thoughts/.")
    return 1


if __name__ == "__main__":
    sys.exit(main(sys.argv))
