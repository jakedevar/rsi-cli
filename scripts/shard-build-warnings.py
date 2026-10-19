#!/usr/bin/env python3
"""Build every rsid/rsid-store test-shard harness and count its compiler warnings (#2671).

usage: scripts/shard-build-warnings.py [--shard NAME]... [--package NAME]... [--clean]

Runs `cargo test -p PACKAGE --lib --no-run --no-default-features --features
test-shard-NAME --message-format=json` for each package that declares the shard
(22 package/shard builds without filters). The `--no-run` harness build is the
one that compiles `cfg(test)` code and names the `(lib test)` target;
`cargo check --profile test --lib` builds only the plain library and checks
none of it.

A shard build compiles a strict subset of the lib tests, so a helper or import
that only another shard's tests use looks unused by construction. The crate
roots therefore allow `dead_code` and `unused_imports` for the shard-mode test
unit (`cfg_attr(all(test, feature = "test-shard-mode"), allow(...))`); the
ordinary lib-test build, which tools/warning-baseline.json measures, keeps both
lints. This script prints one line per build with the count of every lint code
and its errors, and exits nonzero when a build has an error, or a warning other
than those two allowed lints that tools/warning-baseline.json does not accept
(a shard-only `unused_variables`, `unused_mut`, ...).

`--clean` runs `cargo clean -p PACKAGE` after each build to bound disk use.
Run it under `systemd-run --user --collect` and tee the output to a log in /tmp.
"""

from __future__ import annotations

import argparse
import collections
import importlib.util
import json
from pathlib import Path
import subprocess
import sys

ROOT = Path(__file__).resolve().parent.parent
SPEC = importlib.util.spec_from_file_location("check_rsid_test_shards", ROOT / "scripts/check-rsid-test-shards.py")
assert SPEC and SPEC.loader
checker = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(checker)

# The lints the crate-root shard-mode `allow` silences; every other warning is real.
ALLOWED_LINTS = frozenset({"dead_code", "unused_imports"})


def baseline_keys(package: str) -> set[str]:
    """`file|lint|message` keys the warning baseline accepts for a package."""
    path = ROOT / "tools/warning-baseline.json"
    entries = json.loads(path.read_text(encoding="utf-8")).get("packages", {}).get(package, {})
    return set(entries.get("warnings", {}))


def classify(stdout: str, target: str) -> tuple[collections.Counter, set[str], int]:
    """Count lint codes, collect unaccepted warning keys and count errors of `target`.

    Returns (distinct warnings per lint code, `file|lint|message` keys of the
    distinct warnings whose lint is not in ALLOWED_LINTS, error count).
    """
    counts: collections.Counter = collections.Counter()
    keys: set[str] = set()
    errors = 0
    seen: set[str] = set()
    for line in stdout.splitlines():
        try:
            message = json.loads(line)
        except json.JSONDecodeError:
            continue
        if message.get("reason") != "compiler-message" or message["target"]["name"] != target:
            continue
        body = message["message"]
        if body["level"] in ("error", "error: internal compiler error"):
            errors += 1
            continue
        if body["level"] != "warning":
            continue
        code = (body.get("code") or {}).get("code") or "other"
        spans = [span for span in body["spans"] if span["is_primary"]]
        key = f"{spans[0]['file_name']}|{code}|{body['message']}" if spans else f"|{code}|{body['message']}"
        if key in seen:
            continue
        seen.add(key)
        counts[code] += 1
        if code not in ALLOWED_LINTS:
            keys.add(key)
    return counts, keys, errors


def count_build(package: str, shard: str) -> tuple[collections.Counter, set[str], int]:
    command = [
        "cargo", "test", "-p", package, "--lib", "--no-run", "--no-default-features",
        "--features", f"test-shard-{shard}", "--message-format=json",
    ]
    done = subprocess.run(command, cwd=ROOT, capture_output=True, text=True, check=False)
    counts, keys, errors = classify(done.stdout, package.replace("-", "_"))
    if done.returncode and not errors:
        errors = 1  # cargo failed without a rustc error (manifest, feature or link failure)
        print(done.stderr[-2000:], file=sys.stderr)
    return counts, keys, errors


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--shard", action="append", help="only this shard (repeatable)")
    parser.add_argument("--package", action="append", help="only this package (repeatable)")
    parser.add_argument("--clean", action="store_true", help="cargo clean -p PACKAGE after each build")
    args = parser.parse_args()
    failed = False
    total = 0
    for shard, packages in checker.read_shard_packages().items():
        if args.shard and shard not in args.shard:
            continue
        for package in packages:
            if args.package and package not in args.package:
                continue
            counts, keys, errors = count_build(package, shard)
            new = sorted(keys - baseline_keys(package))
            total += 1
            failed = failed or bool(errors or new)
            lints = " ".join(f"{code}={n}" for code, n in sorted(counts.items())) or "no warnings"
            print(
                f"{package} test-shard-{shard}: {lints} errors={errors}"
                + (f" NEW={len(new)}" if new else ""),
                flush=True,
            )
            for key in new[:20]:
                print(f"    {key}", flush=True)
            if args.clean:
                subprocess.run(["cargo", "clean", "-p", package], cwd=ROOT, check=False)
    print(f"{total} shard harness build(s), {'FAILED' if failed else 'ok'}")
    return 1 if failed else 0


if __name__ == "__main__":
    sys.exit(main())
