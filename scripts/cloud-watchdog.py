#!/usr/bin/env python3
"""Stop the tonight cloud fleet before its UTC deadline or compute budget."""

import argparse
import fcntl
import json
import os
import subprocess
import sys
from datetime import datetime, timezone
from pathlib import Path

REGION = "us-west-1"
PROFILE = "rsi-cloud-watchdog"
OWNER = "rsi-cloud-tonight"
HOURLY_USD = 3.5616
DEADLINE = datetime(2026, 9, 27, 16, 0, tzinfo=timezone.utc)


def aws(*args):
    return subprocess.run(
        ["aws", *args, "--region", REGION, "--profile", PROFILE, "--output", "json"],
        check=True,
        capture_output=True,
        text=True,
    ).stdout


def timestamp(value):
    return datetime.fromisoformat(value.replace("Z", "+00:00")).astimezone(timezone.utc)


def instances():
    response = json.loads(
        aws(
            "ec2",
            "describe-instances",
            "--filters",
            "Name=instance-state-name,Values=running",
        )
    )
    return [
        instance
        for reservation in response.get("Reservations", [])
        for instance in reservation.get("Instances", [])
    ]


def update_cost(state, running, now):
    tracked = state.setdefault("tracked", {})
    current_owner = set()
    for instance in running:
        tags = {tag["Key"]: tag["Value"] for tag in instance.get("Tags", [])}
        if tags.get("Owner") != OWNER:
            continue
        instance_id = instance["InstanceId"]
        current_owner.add(instance_id)
        launch = timestamp(instance["LaunchTime"])
        record = tracked.setdefault(instance_id, {"seconds": 0.0, "last_at": launch.isoformat()})
        last = timestamp(record["last_at"])
        # Include time since launch when the timer first observes an instance.
        record["seconds"] += max(0.0, (now - max(last, launch)).total_seconds())
        record["last_at"] = now.isoformat()
    for instance_id, record in tracked.items():
        if instance_id not in current_owner:
            # Conservative: charge through this poll if an instance stopped
            # between polls. This can overestimate but cannot hide spend.
            last = timestamp(record["last_at"])
            record["seconds"] += max(0.0, (now - last).total_seconds())
            record["last_at"] = now.isoformat()
    return sum(item["seconds"] for item in tracked.values()) / 3600 * HOURLY_USD


def save_state(path, state):
    path.parent.mkdir(parents=True, exist_ok=True)
    temporary = path.with_suffix(".tmp")
    fd = os.open(temporary, os.O_WRONLY | os.O_CREAT | os.O_TRUNC, 0o600)
    with os.fdopen(fd, "w") as output:
        json.dump(state, output, sort_keys=True)
        output.write("\n")
    os.replace(temporary, path)


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--dry-run", action="store_true")
    parser.add_argument("--threshold-usd", type=float, default=150.0)
    parser.add_argument("--state", type=Path, default=Path.home() / ".rsi/cloud/watchdog-state.json")
    args = parser.parse_args()
    if args.threshold_usd < 0:
        parser.error("threshold must be nonnegative")
    now = datetime.now(timezone.utc)
    lock = args.state.with_suffix(".lock")
    lock.parent.mkdir(parents=True, exist_ok=True)
    with lock.open("a+") as lease:
        fcntl.flock(lease, fcntl.LOCK_EX)
        state = json.loads(args.state.read_text()) if args.state.exists() else {}
        running = instances()
        estimated = update_cost(state, running, now)
        save_state(args.state, state)
        reason = "deadline" if now >= DEADLINE else "budget" if estimated >= args.threshold_usd else "none"
        ids = sorted(instance["InstanceId"] for instance in running)
        print(
            f"{now.isoformat()} reason={reason} dry_run={args.dry_run} "
            f"estimated_compute_usd={estimated:.2f} running={','.join(ids) or '-'}",
            flush=True,
        )
        if reason != "none" and ids and not args.dry_run:
            aws("ec2", "stop-instances", "--instance-ids", *ids)
            print(f"stop_requested={','.join(ids)}", flush=True)


if __name__ == "__main__":
    try:
        main()
    except Exception as error:
        print(f"cloud-watchdog failed: {error}", file=sys.stderr)
        raise SystemExit(1)
