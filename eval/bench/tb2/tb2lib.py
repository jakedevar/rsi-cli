"""Shared helpers for the pinned Terminal-Bench 2.0 definition (stdlib only)."""

import hashlib
import json
import pathlib
import tomllib

HERE = pathlib.Path(__file__).resolve().parent
DATASET = HERE / "dataset.toml"
SPLIT = HERE / "split.json"


def task_toml_sha256(path):
    """sha256 of a task.toml with CRLF folded to LF, so a Windows checkout agrees."""
    data = pathlib.Path(path).read_bytes().replace(b"\r\n", b"\n")
    return hashlib.sha256(data).hexdigest()


def load_dataset(path=DATASET):
    """Return (header dict, task rows sorted by name, shard rows)."""
    doc = tomllib.loads(pathlib.Path(path).read_text(encoding="utf-8"))
    tasks = sorted(doc.get("task", []), key=lambda t: t["name"])
    return doc["dataset"], tasks, doc.get("shard", [])


def load_split(path=SPLIT):
    return json.loads(pathlib.Path(path).read_text(encoding="utf-8"))


def checkout_task_dirs(checkout):
    """Names of the task directories (those holding a task.toml) in a checkout."""
    root = pathlib.Path(checkout)
    return sorted(p.name for p in root.iterdir() if (p / "task.toml").is_file())
