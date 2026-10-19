#!/usr/bin/env python3
"""Write dataset.toml from a checkout of the pinned repo plus the registry manifests.

Needs a checkout at the pinned commit and, for the image digests and sizes, a docker
host that can run `docker manifest inspect -v` (or a `--manifests` JSON captured earlier:
{"<image tag>": {"digest": "sha256:...", "bytes": N}}). Run it only to re-pin: the
committed dataset.toml is what every benchmark run is checked against (pin_check.py).
"""

import argparse
import json
import pathlib
import subprocess
import sys
import tomllib

import split
import tb2lib

REPO = "https://github.com/harbor-framework/terminal-bench-2"
COMMIT = "2fd12b88aafdd04a52c298e3940bcb189f9766d6"
HARBOR = "0.24.0"
PLATFORM = "linux/amd64"


def inspect_image(tag, run=subprocess.run):
    """Digest and compressed bytes (config + layers) of the linux/amd64 manifest of `tag`."""
    out = run(["docker", "manifest", "inspect", "-v", tag], capture_output=True, text=True, timeout=180)
    if out.returncode:
        raise SystemExit(f"docker manifest inspect {tag} failed: {out.stderr.strip()}")
    doc = json.loads(out.stdout)
    for entry in doc if isinstance(doc, list) else [doc]:
        plat = entry["Descriptor"].get("platform", {})
        if (plat.get("os"), plat.get("architecture")) == ("linux", "amd64"):
            m = entry["SchemaV2Manifest"]
            return {"digest": entry["Descriptor"]["digest"],
                    "bytes": m["config"]["size"] + sum(layer["size"] for layer in m["layers"])}
    raise SystemExit(f"{tag}: no linux/amd64 manifest")


def read_tasks(checkout, manifests=None):
    rows = []
    for name in tb2lib.checkout_task_dirs(checkout):
        path = pathlib.Path(checkout) / name / "task.toml"
        t = tomllib.loads(path.read_text(encoding="utf-8"))
        env = t["environment"]
        image = env["docker_image"]
        info = manifests[image] if manifests else inspect_image(image)
        rows.append({
            "name": name,
            "difficulty": t["metadata"]["difficulty"],
            "category": t["metadata"]["category"],
            "cpus": env["cpus"],
            "memory_mb": env["memory_mb"],
            "agent_timeout_sec": float(t["agent"]["timeout_sec"]),
            "verifier_timeout_sec": float(t["verifier"]["timeout_sec"]),
            "image": image,
            "image_digest": info["digest"],
            "image_bytes": info["bytes"],
            "task_toml_sha256": tb2lib.task_toml_sha256(path),
        })
    return rows


def render(rows, shards):
    lines = [
        "# Pinned Terminal-Bench 2.0 definition (#3175). Do not edit by hand:",
        "# regenerate with refresh_dataset.py; pin_check.py fails a run on any drift.",
        "",
        "[dataset]",
        'name = "terminal-bench-2"',
        f'repo = "{REPO}"',
        f'commit = "{COMMIT}"',
        f'harbor_version = "{HARBOR}"',
        f'platform = "{PLATFORM}"',
        f"task_count = {len(rows)}",
        f"shard_cap_bytes = {split.SHARD_CAP_BYTES}",
        f"heavy_image_bytes = {split.HEAVY_BYTES}",
    ]
    for r in rows:
        lines += [
            "",
            "[[task]]",
            f'name = "{r["name"]}"',
            f'difficulty = "{r["difficulty"]}"',
            f'category = "{r["category"]}"',
            f'cpus = {r["cpus"]}',
            f'memory_mb = {r["memory_mb"]}',
            f'agent_timeout_sec = {r["agent_timeout_sec"]}',
            f'verifier_timeout_sec = {r["verifier_timeout_sec"]}',
            f'image = "{r["image"]}"',
            f'image_digest = "{r["image_digest"]}"',
            f'image_bytes = {r["image_bytes"]}',
            f'task_toml_sha256 = "{r["task_toml_sha256"]}"',
        ]
    for s in shards:
        tasks = ", ".join(f'"{t}"' for t in s["tasks"])
        lines += [
            "",
            "[[shard]]",
            f'name = "{s["name"]}"',
            f'split = "{s["split"]}"',
            f'heavy = {"true" if s["heavy"] else "false"}',
            f'bytes = {s["bytes"]}',
            f"tasks = [{tasks}]",
        ]
    return "\n".join(lines) + "\n"


def main(argv=None):
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    ap.add_argument("checkout", help="checkout of the benchmark repo at the pinned commit")
    ap.add_argument("--manifests", help="JSON of image tag -> {digest, bytes}; skips docker")
    ap.add_argument("--out", default=str(tb2lib.DATASET))
    args = ap.parse_args(argv)
    manifests = None
    if args.manifests:
        with open(args.manifests, encoding="utf-8") as f:
            manifests = json.load(f)
    rows = read_tasks(args.checkout, manifests)
    sp = split.make_split(rows)
    with open(args.out, "w", encoding="utf-8") as f:
        f.write(render(rows, split.plan_shards(rows, sp)))
    return 0


if __name__ == "__main__":
    sys.exit(main())
