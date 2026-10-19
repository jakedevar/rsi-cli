#!/usr/bin/env python3
"""Regenerate the Terminal-Bench 2.0 search/held-out split and the image shard plan.

Algorithm version 1, plan appendix A of thoughts/shared/plans/2026-10-19-issue-3135-m3-benchmark-adapter.md.
Stdlib only, no network. Reads dataset.toml; `--write` rewrites split.json, `--check` (default)
exits non-zero when the committed split.json differs from the regenerated one.
"""

import argparse
import hashlib
import json
import math
import random
import sys

import tb2lib

SEED = 3135
N_SEARCH = 30
N_SMOKE = 5
N_CAL = 10
ALGORITHM = "tb2-split-v1"
ORDER = ["easy", "medium", "hard"]
SMOKE_MAX_AGENT_TIMEOUT = 900.0
SHARD_CAP_BYTES = 12 * 10**9
HEAVY_BYTES = 4 * 10**9


def _largest_remainder(total, counts):
    """Allocate `total` over `counts` (dict stratum -> size) by largest remainder."""
    n = sum(counts.values())
    quota = {d: total * counts[d] / n for d in ORDER}
    alloc = {d: math.floor(q) for d, q in quota.items()}
    extra = total - sum(alloc.values())
    for d in sorted(ORDER, key=lambda d: -(quota[d] - alloc[d]))[:extra]:
        alloc[d] += 1
    return alloc


def _hash(*parts):
    return hashlib.sha256(":".join(str(p) for p in parts).encode()).hexdigest()


def make_split(rows, seed=SEED):
    """rows: dicts with name, difficulty, category, agent_timeout_sec, image_bytes."""
    by_name = {r["name"]: r for r in rows}
    strata = {d: [r for r in rows if r["difficulty"] == d] for d in ORDER}
    alloc = _largest_remainder(N_SEARCH, {d: len(strata[d]) for d in ORDER})
    search = []
    for d in ORDER:
        lst = sorted(strata[d], key=lambda r: (r["category"], _hash(seed, r["name"])))
        k, m = alloc[d], len(lst)
        off = random.Random(f"{seed}:{d}").random() * m / k
        search += [lst[i]["name"] for i in sorted({math.floor(off + i * m / k) for i in range(k)})]
    search = sorted(search)
    held = sorted(r["name"] for r in rows if r["name"] not in set(search))
    # smoke: the search tasks with agent timeout <= 900 s and the smallest images (size, then name)
    cheap = [by_name[n] for n in search if by_name[n]["agent_timeout_sec"] <= SMOKE_MAX_AGENT_TIMEOUT]
    smoke = sorted(r["name"] for r in sorted(cheap, key=lambda r: (r["image_bytes"], r["name"]))[:N_SMOKE])
    # calibration: 10 more search tasks, largest remainder by difficulty over the rest,
    # each stratum ordered by sha256(seed:cal:name)
    rest = [by_name[n] for n in search if n not in set(smoke)]
    rest_strata = {d: [r for r in rest if r["difficulty"] == d] for d in ORDER}
    cal_alloc = _largest_remainder(N_CAL, {d: len(rest_strata[d]) for d in ORDER})
    cal = []
    for d in ORDER:
        ranked = sorted(rest_strata[d], key=lambda r: _hash(seed, "cal", r["name"]))
        cal += [r["name"] for r in ranked[: cal_alloc[d]]]
    return {
        "seed": seed,
        "algorithm": ALGORITHM,
        "search": search,
        "held_out": held,
        "smoke": smoke,
        "calibration": sorted(cal),
    }


def plan_shards(rows, split):
    """Shards of at most 12 GB of compressed images, never mixing search and held-out.

    An image of HEAVY_BYTES or more gets a shard of its own. The rest are packed
    first-fit-decreasing (size, then name) so the plan is deterministic.
    """
    shards = []
    for part in ("search", "held_out"):
        members = sorted((r for r in rows if r["name"] in set(split[part])),
                         key=lambda r: (-r["image_bytes"], r["name"]))
        bins = []
        for r in members:
            if r["image_bytes"] >= HEAVY_BYTES:
                bins.append({"heavy": True, "tasks": [r], "bytes": r["image_bytes"]})
        light = [r for r in members if r["image_bytes"] < HEAVY_BYTES]
        packed = []
        for r in light:
            for b in packed:
                if b["bytes"] + r["image_bytes"] <= SHARD_CAP_BYTES:
                    b["tasks"].append(r)
                    b["bytes"] += r["image_bytes"]
                    break
            else:
                packed.append({"heavy": False, "tasks": [r], "bytes": r["image_bytes"]})
        bins = packed + bins
        for i, b in enumerate(bins, 1):
            shards.append({
                "name": f"{part.replace('_', '-')}-{i:02d}",
                "split": part,
                "heavy": b["heavy"],
                "bytes": b["bytes"],
                "tasks": sorted(t["name"] for t in b["tasks"]),
            })
    return shards


def main(argv=None):
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    ap.add_argument("--dataset", default=str(tb2lib.DATASET))
    ap.add_argument("--split", default=str(tb2lib.SPLIT))
    ap.add_argument("--write", action="store_true", help="rewrite split.json")
    args = ap.parse_args(argv)
    _, tasks, _ = tb2lib.load_dataset(args.dataset)
    text = json.dumps(make_split(tasks), indent=2) + "\n"
    if args.write:
        with open(args.split, "w", encoding="utf-8") as f:
            f.write(text)
        return 0
    with open(args.split, encoding="utf-8") as f:
        committed = f.read()
    if committed != text:
        print(f"drift: {args.split} differs from the split regenerated from {args.dataset}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
