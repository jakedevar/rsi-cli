"""Stable execution-host class for rolling shard cache fingerprints."""

import hashlib
import os
from pathlib import Path
import platform
import socket


def host_class():
    """Keep a class label, but never let it erase the physical host identity."""
    label = os.environ.get("RSI_QA_HOST_CLASS") or f"{platform.system().lower()}-{platform.machine().lower()}"
    hostname = socket.gethostname().strip().lower()
    if not hostname:
        raise ValueError("QA host has no hostname")
    try:
        machine_id = Path("/etc/machine-id").read_text().strip()
    except OSError:
        machine_id = ""
    identity = hashlib.sha256(f"{hostname}\0{machine_id}".encode()).hexdigest()[:16]
    return f"{label}@{hostname}:{identity}"
