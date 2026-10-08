#!/usr/bin/python3
"""Finite, local-only Claude print-mode fixture; never calls an inference API.

It answers one turn with a scripted message and exits, so a fresh-install
`:blank` session reaches a finished state without any paid model run.
"""
import json
import select
import sys

args = sys.argv[1:]
if args == ["--version"]:
    print("2.1.0 (Claude Code)")
    sys.exit(0)

# Read whatever prompt the daemon sends (bounded, never waits on a tty).
prompt = ""
while select.select([sys.stdin], [], [], 0.5)[0]:
    chunk = sys.stdin.readline()
    if not chunk:
        break
    prompt += chunk
    if len(prompt) > 262144:
        sys.exit("claude fixture prompt exceeded its bound")


def emit(event):
    print(json.dumps(event), flush=True)


sid = "fresh-install-fixture-session"
usage = {"input_tokens": 12, "output_tokens": 6, "cache_read_input_tokens": 0, "cache_creation_input_tokens": 0}
emit({"type": "system", "subtype": "init", "session_id": sid, "model": "claude-sonnet-5-5", "tools": [], "cwd": "."})
emit({"type": "assistant", "session_id": sid, "message": {"id": "msg_fixture", "role": "assistant", "model": "claude-sonnet-5-5", "content": [{"type": "text", "text": "scripted fresh-install fixture ready"}], "usage": usage}})
emit({"type": "result", "subtype": "success", "is_error": False, "result": "scripted fresh-install fixture ready", "session_id": sid, "total_cost_usd": 0, "duration_ms": 1, "num_turns": 1, "usage": usage})
