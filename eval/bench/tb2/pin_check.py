#!/usr/bin/env python3
"""Fail when a Terminal-Bench 2.0 checkout or its pulled images drift from dataset.toml.

    pin_check.py CHECKOUT [--images] [--require-pulled] [--task NAME ...]

Checks, each drift printed on its own line and the exit code 1:
  * the checkout has exactly the pinned task directories (a task.toml each);
  * every task.toml has the pinned sha256 (CRLF folded to LF);
  * a git checkout is at the pinned commit;
  * with --images, every image present on the docker host has the pinned digest in its
    RepoDigests. An image that is not pulled is skipped unless --require-pulled, since the
    laptop pulls and prunes images shard by shard.
--task limits the image check (the checkout check is always whole). Exit 2 is a usage or
environment error (no docker, unreadable checkout). The S5 runner calls this before every run.
"""

import argparse
import json
import pathlib
import subprocess
import sys

import tb2lib


def check_checkout(checkout, header, tasks, run=subprocess.run):
    drift = []
    root = pathlib.Path(checkout)
    have = set(tb2lib.checkout_task_dirs(root))
    want = {t["name"] for t in tasks}
    for name in sorted(want - have):
        drift.append(f"task missing from checkout: {name}")
    for name in sorted(have - want):
        drift.append(f"task not in the pinned set: {name}")
    for t in tasks:
        if t["name"] not in have:
            continue
        got = tb2lib.task_toml_sha256(root / t["name"] / "task.toml")
        if got != t["task_toml_sha256"]:
            drift.append(f"task.toml changed: {t['name']} (pinned {t['task_toml_sha256']}, found {got})")
    if (root / ".git").exists():
        out = run(["git", "-C", str(root), "rev-parse", "HEAD"], capture_output=True, text=True)
        head = out.stdout.strip()
        if out.returncode == 0 and head != header["commit"]:
            drift.append(f"checkout commit {head} is not the pinned {header['commit']}")
    return drift


def image_repo_digests(tag, run=subprocess.run):
    """RepoDigests of a local image, or None when the image is not pulled."""
    out = run(["docker", "image", "inspect", "--format", "{{json .RepoDigests}}", tag],
              capture_output=True, text=True)
    if out.returncode:
        if "No such image" in out.stderr or "No such object" in out.stderr:
            return None
        raise RuntimeError(f"docker image inspect {tag}: {out.stderr.strip() or out.returncode}")
    return json.loads(out.stdout.strip() or "[]") or []


def check_images(tasks, names=None, require_pulled=False, inspect=image_repo_digests):
    drift = []
    for t in tasks:
        if names and t["name"] not in names:
            continue
        digests = inspect(t["image"])
        if digests is None:
            if require_pulled:
                drift.append(f"image not pulled: {t['image']} ({t['name']})")
            continue
        if not any(d.endswith("@" + t["image_digest"]) for d in digests):
            drift.append(f"image digest drift: {t['image']} ({t['name']}) pinned {t['image_digest']}, "
                         f"local RepoDigests {digests}")
    return drift


def main(argv=None):
    ap = argparse.ArgumentParser(description="Detect drift from the pinned Terminal-Bench 2.0 definition.")
    ap.add_argument("checkout")
    ap.add_argument("--dataset", default=str(tb2lib.DATASET))
    ap.add_argument("--images", action="store_true", help="also check pulled images on the docker host")
    ap.add_argument("--require-pulled", action="store_true", help="with --images, an unpulled image is drift")
    ap.add_argument("--task", action="append", default=[], help="limit the image check to this task")
    args = ap.parse_args(argv)
    try:
        header, tasks, _ = tb2lib.load_dataset(args.dataset)
        drift = check_checkout(args.checkout, header, tasks)
        if args.images:
            drift += check_images(tasks, set(args.task), args.require_pulled)
    except (OSError, RuntimeError, KeyError, ValueError) as e:
        print(f"pin_check error: {e}", file=sys.stderr)
        return 2
    for line in drift:
        print("DRIFT " + line)
    if drift:
        print(f"pin_check: {len(drift)} drift(s) from {args.dataset}", file=sys.stderr)
        return 1
    print(f"pin_check: ok ({len(tasks)} tasks at {header['commit'][:8]})")
    return 0


if __name__ == "__main__":
    sys.exit(main())
