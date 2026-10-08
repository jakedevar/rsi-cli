#!/usr/bin/python3
"""Finite, local-only Codex protocol fixture; never calls an inference API.

Used by the agent-created project E2E (#1626). Once its prompt carries
AGENT_PROJECT_E2E_ACTOR it waits for request files `agent-request-<n>.json`
({"method": ..., "params": ...}) next to this script, sends each to the
isolated daemon socket with its own daemon-minted token, and writes the reply
to `agent-result-<n>.json`. The token is used only as RPC transport.
"""
import json
import os
from pathlib import Path
import socket
import sys
import time

fixture_bin = Path(__file__).resolve().parent
fixture_root = fixture_bin.parent
args = sys.argv[1:]
if args == ["--version"]:
    print("codex-cli 0.155.1")
    sys.exit(0)
if args == ["debug", "models", "--bundled"]:
    print((fixture_bin / "codex-models.json").read_text())
    sys.exit(0)
if not args or args[0] != "exec":
    sys.exit("unsupported agent fixture invocation")


def emit(event):
    print(json.dumps(event), flush=True)


prompt = sys.stdin.read(262145)
if len(prompt) > 262144:
    sys.exit("agent fixture prompt exceeded its bound")
emit({"type": "thread.started", "thread_id": "agent-fixture-" + os.environ.get("RSI_SESSION_ID", "unknown")})
if "AGENT_PROJECT_E2E_ACTOR" in prompt:
    socket_path = Path(os.environ.get("RSI_DAEMON_SOCKET_PATH", "")).resolve()
    if not socket_path.is_relative_to(fixture_root) and not str(socket_path).startswith("/tmp/rsi-"):
        sys.exit("agent fixture refused a non-isolated socket")
    if not os.environ.get("RSI_SESSION_TOKEN"):
        sys.exit("agent fixture refused an unattributed session")
    deadline = time.monotonic() + 240
    served = 0
    while time.monotonic() < deadline and served < 4:
        request_path = fixture_bin / f"agent-request-{served}.json"
        if not request_path.exists():
            time.sleep(0.02)
            continue
        body = json.loads(request_path.read_text())
        request = {
            "jsonrpc": "2.0", "id": 1, "method": body["method"],
            "params": body["params"],
            "session_token": os.environ["RSI_SESSION_TOKEN"],
        }
        with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as conn:
            conn.settimeout(15)
            conn.connect(str(socket_path))
            conn.sendall(json.dumps(request).encode() + b"\n")
            reply = json.loads(conn.makefile("rb").readline())
        # Never retain or print the transport credential or request frame.
        tmp = fixture_bin / f"agent-result-{served}.tmp"
        tmp.write_text(json.dumps(reply))
        tmp.rename(fixture_bin / f"agent-result-{served}.json")
        served += 1

emit({"type": "item.completed", "item": {"type": "agent_message", "text": "scripted agent fixture finished"}})
emit({"type": "turn.completed", "usage": {"input_tokens": 20, "cached_input_tokens": 0, "output_tokens": 8}})
