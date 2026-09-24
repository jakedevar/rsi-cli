#!/usr/bin/env python3
"""Per-session benchmark metrics from the daemon DB, read-only (benchmark #694).

Usage: bench-session-metrics.py [--db PATH] [--over USD] <runs.tsv | session-id>...

runs.tsv lines are `arm<TAB>session-id` (a prefix of 8+ hex chars is enough);
`#` lines are ignored. Bare session ids get an empty arm.

Tokens come from the session row totals, NOT from model_invocations sums:
failed embedding-index invocations copy session usage (Issue #700), so
invocation sums double count. USD is tokens x the OpenRouter list prices below
and is [inferred] (the daemon records no OpenRouter cost, #585).

--over USD: also exit 3 if any running or finished session exceeds USD at list
price; the lead uses it as a runaway guard and halts those sessions.
"""
import argparse
import sqlite3
import sys
from pathlib import Path

# USD per 1M tokens: (input, output, cache_read). Source: GET
# https://openrouter.ai/api/v1/models, fetched 2026-09-24T02:48Z.
PRICES = {
    "z-ai/glm-5.3-flashx": (0.37, 1.25, 0.09),
    "deepseek/deepseek-v4.1-flash": (0.14, 0.42, 0.0042),
    "qwen/qwen3.8-flash": (0.15, 0.47, 0.016),
    "minimax/minimax-m3": (0.30, 1.20, 0.06),
    "qwen/qwen3-coder-next": (0.12, 0.80, 0.07),
}

QUERY = """
SELECT s.id, s.provider, s.model, s.status, COALESCE(s.stop_reason, ''),
       COALESCE(s.terminal_reason, ''), s.created_at, s.updated_at,
       COALESCE(s.total_input_tokens, 0), COALESCE(s.total_output_tokens, 0),
       COALESCE(s.total_cache_creation_tokens, 0), COALESCE(s.total_cache_read_tokens, 0),
       (SELECT count(*) FROM conversation_events e
         WHERE e.session_id = s.id AND e.event_type = 'ToolUse'),
       (SELECT count(*) FROM conversation_events e
         WHERE e.session_id = s.id AND e.content LIKE '**Process Error%')
FROM sessions s WHERE s.id LIKE ? || '%'
"""


def parse_ts(ts):
    from datetime import datetime

    # RFC3339 with nanoseconds; Python accepts at most microseconds.
    ts = ts.replace("Z", "+00:00")
    if "." not in ts:
        return datetime.fromisoformat(ts)
    head, _, rest = ts.partition(".")
    digits = len(rest) - len(rest.lstrip("0123456789"))
    frac, tz = rest[:digits][:6] or "0", rest[digits:]
    return datetime.fromisoformat(f"{head}.{frac}{tz}")


def list_usd(model, inp, out, cc, cr):
    p = PRICES.get(model)
    if p is None:
        return None
    return ((inp + cc) * p[0] + out * p[1] + cr * p[2]) / 1e6


def load_runs(args):
    runs = []
    for item in args:
        path = Path(item)
        if path.is_file():
            for line in path.read_text().splitlines():
                line = line.strip()
                if line and not line.startswith("#"):
                    arm, _, sid = line.partition("\t")
                    runs.append((arm, sid.strip()))
        else:
            runs.append(("", item))
    return runs


def main():
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    ap.add_argument("--db", default=str(Path.home() / ".rsi/rsi.db"))
    ap.add_argument("--over", type=float, default=None)
    ap.add_argument("runs", nargs="+")
    a = ap.parse_args()
    db = sqlite3.connect(f"file:{a.db}?mode=ro", uri=True)
    print("arm|session|model|status|stop|wall_s|tool_calls|process_errors|in|out|cache_create|cache_read|list_usd")
    total, over = 0.0, []
    for arm, sid in load_runs(a.runs):
        rows = db.execute(QUERY, (sid,)).fetchall()
        if len(rows) != 1:
            print(f"{arm}|{sid}|NOT_FOUND_OR_AMBIGUOUS({len(rows)})|||||||||||")
            continue
        (full, _prov, model, status, stop, term, created, updated,
         inp, out, cc, cr, tools, perr) = rows[0]
        wall = (parse_ts(updated) - parse_ts(created)).total_seconds()
        usd = list_usd(model, inp, out, cc, cr)
        total += usd or 0.0
        if a.over is not None and usd is not None and usd > a.over:
            over.append(full)
        usd_s = "unpriced" if usd is None else f"{usd:.4f}"
        print(f"{arm}|{full[:8]}|{model}|{status}|{stop or term}|{wall:.0f}|{tools}|{perr}|"
              f"{inp}|{out}|{cc}|{cr}|{usd_s}")
    print(f"TOTAL list_usd [inferred] = {total:.4f}")
    if over:
        print("OVER " + " ".join(over))
        return 3
    return 0


if __name__ == "__main__":
    sys.exit(main())
