#!/usr/bin/env python3
"""Spend guard and cache-stat summary for the AWS gate ledger (Issue #1010).

  cloud-spend.py check [--ledger PATH]
      Exit 0 when the ledger's cumulative compute estimate is below the stop
      line and today's (UTC) estimate is below the daily cap, 4 when either is
      reached (or the ledger or caps file cannot be read: the guard fails
      closed). The operator sets both caps through the rsid daemon settings
      `cloud_spend_stop_line_usd` and `cloud_spend_daily_cap_usd` (Settings
      row, UpdateDaemonConfig); rsid mirrors them into spend-caps.json beside
      the ledger and this script reads that file. Without the file the stop
      line falls back to the ledger header ("Stop and report by $90
      cumulative ...") and the daily cap to $15. The estimate is the sum of
      every "est compute $X" figure in the ledger, so a new grant starts a new
      ledger; a line counts toward the UTC day of its "stop <timestamp>" (else
      its first timestamp), the same rule as rsid's GetCloudSpend.

  cloud-spend.py cache [--json-out PATH] < `sccache --show-stats --stats-format json`
      Print one line, "sccache_hits=N sccache_misses=M sccache_hit_rate=P%",
      and optionally write the same numbers as JSON.

AWS Budgets (rsi-cloud-us-west-1-daily $15, -monthly $100) stay the external
backstop.
"""
import argparse
import datetime
import json
import os
import re
import sys

DEFAULT_LEDGER = os.path.join(os.path.expanduser("~"), ".rsi", "cloud", "spend.md")
GRANT_RE = re.compile(r"Operator grant:\s*\$([0-9]+(?:\.[0-9]+)?)")
STOP_RE = re.compile(r"Stop and report by\s*\$([0-9]+(?:\.[0-9]+)?)\s+cumulative")
EST_RE = re.compile(r"est compute\s*\$([0-9]+(?:\.[0-9]+)?)")
DATE_RE = re.compile(r"(\d{4}-\d{2}-\d{2})[T ]")
DEFAULT_DAILY_CAP = 15.0
CAPS_FILE = "spend-caps.json"


def line_date(line):
    at = line.find("stop ")
    for text in ((line[at:],) if at >= 0 else ()) + (line,):
        match = DATE_RE.search(text)
        if match:
            return match.group(1)
    return None


def read_ledger(path):
    """Return (grant, header stop line, cumulative spend, spend by UTC day)."""
    with open(path, encoding="utf-8") as handle:
        text = handle.read()
    grant = GRANT_RE.search(text)
    stop = STOP_RE.search(text)
    if not grant or not stop:
        raise ValueError("ledger header lacks 'Operator grant: $N' or 'Stop and report by $N cumulative'")
    spent = 0.0
    by_day = {}
    for line in text.splitlines():
        if not EST_RE.search(line):
            continue
        est = sum(float(m.group(1)) for m in EST_RE.finditer(line))
        spent += est
        day = line_date(line)
        if day:
            by_day[day] = by_day.get(day, 0.0) + est
    return float(grant.group(1)), float(stop.group(1)), spent, by_day


def read_caps(path):
    """Return (stop line or None, daily cap) from the daemon-written caps file."""
    try:
        with open(path, encoding="utf-8") as handle:
            caps = json.load(handle)
        return float(caps["stop_line_usd"]), float(caps["daily_cap_usd"])
    except FileNotFoundError:
        return None, DEFAULT_DAILY_CAP
    except (OSError, ValueError, KeyError, TypeError) as err:
        raise ValueError(f"unreadable {path}: {err}")


def check(args):
    today = args.today or datetime.datetime.now(datetime.timezone.utc).strftime("%Y-%m-%d")
    caps_path = args.caps or os.path.join(os.path.dirname(os.path.abspath(args.ledger)), CAPS_FILE)
    try:
        grant, header_stop, spent, by_day = read_ledger(args.ledger)
        caps_stop, daily_cap = read_caps(caps_path)
    except (OSError, ValueError) as err:
        print(f"cloud-spend: refusing to start a remote run: {err}", file=sys.stderr)
        return 4
    stop = header_stop if caps_stop is None else caps_stop
    day_spent = by_day.get(today, 0.0)
    line = (
        f"cloud-spend: cumulative estimate ${spent:.2f} of ${grant:.2f} grant, stop line ${stop:.2f}; "
        f"{today} UTC ${day_spent:.2f} of ${daily_cap:.2f} daily cap"
    )
    if spent >= stop:
        print(f"{line}; at or above the stop line, refusing to start a remote run", file=sys.stderr)
        return 4
    if day_spent >= daily_cap:
        print(f"{line}; at or above the daily cap, refusing to start a remote run", file=sys.stderr)
        return 4
    print(line, file=sys.stderr)
    return 0


def summarize(stats):
    body = stats.get("stats", stats)

    def total(key):
        counts = (body.get(key) or {}).get("counts") or {}
        return sum(int(v) for v in counts.values())

    hits, misses = total("cache_hits"), total("cache_misses")
    rate = 100.0 * hits / (hits + misses) if hits + misses else 0.0
    return {"hits": hits, "misses": misses, "hit_rate_percent": round(rate, 1)}


def cache(args):
    try:
        summary = summarize(json.load(sys.stdin))
    except (ValueError, AttributeError, TypeError) as err:
        print(f"cloud-spend: unreadable sccache stats: {err}", file=sys.stderr)
        return 1
    if args.json_out:
        with open(args.json_out, "w", encoding="utf-8") as handle:
            json.dump(summary, handle)
            handle.write("\n")
    print(f"sccache_hits={summary['hits']} sccache_misses={summary['misses']} sccache_hit_rate={summary['hit_rate_percent']}%")
    return 0


def main():
    parser = argparse.ArgumentParser(description=__doc__.split("\n")[0])
    sub = parser.add_subparsers(dest="command", required=True)
    p_check = sub.add_parser("check")
    p_check.add_argument("--ledger", default=DEFAULT_LEDGER)
    p_check.add_argument("--caps", help="caps file (default: spend-caps.json beside the ledger)")
    p_check.add_argument("--today", help="UTC day YYYY-MM-DD (tests)")
    p_check.set_defaults(func=check)
    p_cache = sub.add_parser("cache")
    p_cache.add_argument("--json-out")
    p_cache.set_defaults(func=cache)
    args = parser.parse_args()
    return args.func(args)


if __name__ == "__main__":
    sys.exit(main())
