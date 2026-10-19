#!/usr/bin/env python3
"""Starter digest drift guard (#3070).

A bundled starter topology (``crates/rsid/src/topology/starters/*.json``) is
pinned twice: the digest fixture in ``starters_tests.rs`` holds its *current*
canonical digest, and ``SHIPPED_BEFORE`` in ``starters.rs`` holds the digest of
every definition it *replaced* (an unedited stored copy at one of those is
upgraded at boot instead of reading as ``starter_drift``). Changing a starter's
JSON therefore moves both together:

1. the fixture takes the new canonical digest (the failing fixture test prints
   it), and
2. the digest the fixture held before is appended to that starter's
   ``SHIPPED_BEFORE`` entry.

The Rust tests catch a missed step only when a gate selects and runs them; #3045
changed all three issue-pipeline starters and left both pins alone, and the
drift surfaced as three baseline reds on later gates. This guard needs no build.
It compares the starter sources at ``BASE`` and ``HEAD`` (text only, like
``check-released-migrations.py``) and refuses when a starter's JSON changed but

* its fixture still pins the digest it pinned at ``BASE``, or
* the digest it pinned at ``BASE`` is not in its ``SHIPPED_BEFORE`` entry now,

and, at ``HEAD`` alone, when a fixture pins a digest that ``SHIPPED_BEFORE``
also lists as replaced, a ``SHIPPED_BEFORE`` entry repeats a digest or drops one
it held at ``BASE`` (history is append-only), or a starter has no fixture or no
history entry. Any retired digest literal outside ``starters.rs`` under the
topology directory is also refused: tests must share the current fixture
instead of carrying an independently stale pin (#411).

It cannot compute the canonical digest itself (that is the typed definition
re-serialized by ``rsid``), so it never says a pin is *correct*; the Rust
fixture tests still do. A change that leaves the typed digest unchanged (a key
the definition ignores) is the one thing this guard refuses wrongly: keep such
a JSON edit out of the starters directory, or move the pins anyway.

Usage: ``check_starter_pins.py [BASE [HEAD]]`` (defaults ``origin/rolling`` and
``HEAD``); ``check_starter_pins.py --tree`` checks the working tree alone.
Exit 0 when the sources are consistent or have no starters.
"""

from __future__ import annotations

import json
import re
import subprocess
import sys
from pathlib import Path
from typing import Optional

STARTERS_DIR = "crates/rsid/src/topology"
SOURCE_PATH = f"{STARTERS_DIR}/starters.rs"
TESTS_PATH = f"{STARTERS_DIR}/starters_tests.rs"

DIGEST = r"sha256:[0-9a-f]{64}"
# Where each starter's current digest is pinned in `starters_tests.rs`, keyed by the
# starter's name constant in `starters.rs`. A starter without a site here is refused,
# so a new starter cannot ship without a fixture this guard can read.
PIN_SITES = {
    "ISSUE_IMPLEMENT_REVIEW_LAND": rf'\(\s*ISSUE_IMPLEMENT_REVIEW_LAND,\s*"({DIGEST})"',
    "ROLLING_QA_SWEEP": rf'\(\s*ROLLING_QA_SWEEP,\s*"({DIGEST})"',
    "ISSUE_PIPELINE": rf'const ISSUE_PIPELINE_DIGEST: &str =\s*"({DIGEST})"',
    "ISSUE_PIPELINE_REVIEWED": rf'const REVIEWED_DIGEST: &str =\s*"({DIGEST})"',
    "ISSUE_PIPELINE_POST_REVIEW": rf'const POST_REVIEW_DIGEST: &str =\s*"({DIGEST})"',
}
NAME_CONST_RE = re.compile(r'pub\(crate\) const (\w+): &str = "([a-z0-9-]+)";')
STARTER_RE = re.compile(r'name:\s*(\w+),\s*definition:\s*include_str!\("starters/([^"]+)"\)')
HISTORY_START = "SHIPPED_BEFORE: &[(&str, &[&str])] = &["
HISTORY_ENTRY_RE = re.compile(r"\(\s*(\w+),\s*&\[(.*?)\],?\s*\)", re.DOTALL)

HOW_TO_MOVE = (
    "a starter JSON change moves both pins: set the fixture in starters_tests.rs to the "
    "new canonical digest (the failing fixture test prints it) and append the digest the "
    "fixture held before to that starter's SHIPPED_BEFORE entry in starters.rs"
)


class Sources:
    """The starter sources of one revision: file texts keyed by repository path."""

    def __init__(self, source: Optional[str], tests: Optional[str], jsons: dict[str, str],
                 topology_texts: Optional[dict[str, str]] = None):
        self.source = source
        self.tests = tests
        self.jsons = jsons
        self.topology_texts = dict(topology_texts or {})
        if tests is not None:
            self.topology_texts[TESTS_PATH] = tests

    def parse(self) -> dict[str, "Starter"]:
        """Every starter this revision bundles, by name constant."""
        names = dict(NAME_CONST_RE.findall(self.source or ""))
        history = parse_history(self.source or "")
        starters = {}
        for const, filename in STARTER_RE.findall(self.source or ""):
            starters[const] = Starter(
                const=const,
                name=names.get(const, const),
                filename=filename,
                json_text=self.jsons.get(filename),
                pin=find_pin(self.tests or "", const),
                history=history.get(const),
            )
        return starters


class Starter:
    def __init__(self, const, name, filename, json_text, pin, history):
        self.const = const
        self.name = name
        self.filename = filename
        self.json_text = json_text
        self.pin = pin
        self.history = history


def find_pin(tests: str, const: str) -> Optional[str]:
    pattern = PIN_SITES.get(const)
    if pattern is None:
        return None
    found = re.findall(pattern, tests)
    return found[0] if len(found) == 1 else None


def parse_history(source: str) -> dict[str, list[str]]:
    start = source.find(HISTORY_START)
    if start < 0:
        return {}
    block = source[start + len(HISTORY_START):]
    end = block.find("\n];")
    block = block if end < 0 else block[:end]
    return {const: re.findall(DIGEST, body) for const, body in HISTORY_ENTRY_RE.findall(block)}


def same_definition(left: Optional[str], right: Optional[str]) -> bool:
    """True when two JSON texts are one document (formatting and key order aside)."""
    try:
        return json.loads(left or "") == json.loads(right or "")
    except ValueError:
        return left == right


def check(base: Optional[Sources], head: Sources) -> list[str]:
    """The refusals for ``head`` against ``base`` (``None``: no base, check ``head`` alone)."""
    if head.source is None:
        return []
    errors = []
    head_starters = head.parse()
    base_starters = base.parse() if base is not None and base.source is not None else {}
    retired = {digest for history in parse_history(head.source).values() for digest in history}
    for path, text in sorted(head.topology_texts.items()):
        if path == SOURCE_PATH:
            continue
        for number, line in enumerate(text.splitlines(), 1):
            for digest in sorted(set(re.findall(DIGEST, line)) & retired):
                errors.append(f"{path}:{number}: retired starter digest literal {digest} "
                              "is in SHIPPED_BEFORE; use the shared current fixture in "
                              "starters_tests.rs instead of duplicating a pin")
    for const, starter in head_starters.items():
        label = f"starter {starter.name}"
        if const not in PIN_SITES:
            errors.append(f"{label}: no pin site in tools/check_starter_pins.py PIN_SITES "
                          f"(add the fixture in starters_tests.rs and its pattern)")
            continue
        if starter.json_text is None:
            errors.append(f"{label}: starters/{starter.filename} is not in the tree")
            continue
        if starter.pin is None:
            errors.append(f"{label}: its digest fixture is missing or ambiguous in starters_tests.rs")
        if starter.history is None:
            errors.append(f"{label}: no SHIPPED_BEFORE entry in starters.rs")
            continue
        if len(set(starter.history)) != len(starter.history):
            errors.append(f"{label}: SHIPPED_BEFORE repeats a digest")
        if starter.pin is not None and starter.pin in starter.history:
            errors.append(f"{label}: the fixture pins {starter.pin}, which SHIPPED_BEFORE lists as "
                          f"a definition it replaced (the current digest is never in the history)")
        earlier = base_starters.get(const)
        if earlier is None:
            continue
        dropped = [digest for digest in earlier.history or [] if digest not in starter.history]
        if dropped:
            errors.append(f"{label}: SHIPPED_BEFORE dropped {', '.join(dropped)} (history is append-only)")
        if earlier.json_text is None or same_definition(earlier.json_text, starter.json_text):
            continue
        if earlier.pin is None or starter.pin is None:
            continue
        if starter.pin == earlier.pin:
            errors.append(f"{label}: starters/{starter.filename} changed but its digest fixture "
                          f"still pins {earlier.pin}")
        if earlier.pin not in starter.history:
            errors.append(f"{label}: starters/{starter.filename} changed but SHIPPED_BEFORE lacks the "
                          f"digest it replaced, {earlier.pin}")
    if errors:
        errors.append(HOW_TO_MOVE)
    return errors


def git_text(repo: Path, ref: str, path: str) -> Optional[str]:
    result = subprocess.run(["git", "-C", str(repo), "show", f"{ref}:{path}"],
                            capture_output=True, text=True)
    return result.stdout if result.returncode == 0 else None


def load_ref(repo: Path, ref: str) -> Sources:
    source = git_text(repo, ref, SOURCE_PATH)
    paths = subprocess.run(["git", "-C", str(repo), "ls-tree", "-r", "--name-only",
                            ref, "--", STARTERS_DIR],
                           capture_output=True, text=True, check=True).stdout.splitlines()
    return Sources(source, git_text(repo, ref, TESTS_PATH), {
        filename: text
        for _, filename in STARTER_RE.findall(source or "")
        if (text := git_text(repo, ref, f"{STARTERS_DIR}/starters/{filename}")) is not None
    }, {path: text for path in paths
        if (text := git_text(repo, ref, path)) is not None})


def load_tree(repo: Path) -> Sources:
    def read(path: str) -> Optional[str]:
        target = repo / path
        return target.read_text() if target.is_file() else None

    source = read(SOURCE_PATH)
    return Sources(source, read(TESTS_PATH), {
        filename: text
        for _, filename in STARTER_RE.findall(source or "")
        if (text := read(f"{STARTERS_DIR}/starters/{filename}")) is not None
    }, {path.relative_to(repo).as_posix(): path.read_text()
        for path in (repo / STARTERS_DIR).rglob("*") if path.is_file()})


def main(argv: list[str], repo: Optional[Path] = None) -> int:
    repo = repo or Path.cwd()
    args = [arg for arg in argv if not arg.startswith("--")]
    if "--tree" in argv:
        errors = check(None, load_tree(repo))
    else:
        base_ref = args[0] if args else "origin/rolling"
        head_ref = args[1] if len(args) > 1 else "HEAD"
        base = load_ref(repo, base_ref)
        errors = check(base if base.source is not None else None, load_ref(repo, head_ref))
    if errors:
        print("check_starter_pins: starter digest pins drifted from the shipped JSON (#3070):",
              file=sys.stderr)
        for error in errors:
            print(f"  {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
