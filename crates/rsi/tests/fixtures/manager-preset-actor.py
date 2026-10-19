#!/usr/bin/python3
"""Finite, local-only Codex protocol fixture; never calls an inference API."""
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
    sys.exit("unsupported manager fixture invocation")


def emit(event):
    print(json.dumps(event), flush=True)


prompt = sys.stdin.read(262145)
if len(prompt) > 262144:
    sys.exit("manager fixture prompt exceeded its bound")
emit({"type": "thread.started", "thread_id": "manager-fixture-" + os.environ.get("RSI_SESSION_ID", "unknown")})
if "MANAGER_PRESET_E2E_ACTOR" in prompt:
    request_path = fixture_bin / "manager-action.json"
    deadline = time.monotonic() + 90
    while not request_path.exists() and time.monotonic() < deadline:
        time.sleep(0.02)
    if not request_path.exists():
        sys.exit("manager fixture timed out awaiting UI save")
    socket_path = Path(os.environ.get("RSI_DAEMON_SOCKET_PATH", "")).resolve()
    if not socket_path.is_relative_to(fixture_root) or not os.environ.get("RSI_SESSION_TOKEN"):
        sys.exit("manager fixture refused a non-isolated or unattributed socket")
    def rpc(method, params):
        request = {
            "jsonrpc": "2.0", "id": 1, "method": method, "params": params,
            "session_token": os.environ["RSI_SESSION_TOKEN"],
        }
        with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as conn:
            conn.settimeout(15)
            conn.connect(str(socket_path))
            conn.sendall(json.dumps(request).encode() + b"\n")
            return json.loads(conn.makefile("rb").readline())

    reply = rpc("AgentManagerControl", json.loads(request_path.read_text()))
    # Never retain or print the transport credential or original request frame.
    (fixture_bin / "manager-action-result.json").write_text(json.dumps(reply))
    if "error" in reply:
        sys.exit("manager fixture action was refused; inspect its saved receipt")
    # The operator Actions projection has no live admission reason. Read it
    # with this fixture manager's own transport identity, never save its token.
    deadline = time.monotonic() + 45
    while time.monotonic() < deadline:
        view = rpc("AgentManagerGetAction", {
            "operation_id": reply["result"]["operation_id"],
        })
        pending = fixture_bin / "manager-action-view.pending"
        pending.write_text(json.dumps(view))
        pending.replace(fixture_bin / "manager-action-view.json")
        if "error" in view or view["result"]["state"] not in ("queued", "running"):
            break
        time.sleep(0.1)

emit({"type": "item.completed", "item": {"type": "agent_message", "text": "scripted manager fixture finished"}})
emit({"type": "turn.completed", "usage": {"input_tokens": 20, "cached_input_tokens": 0, "output_tokens": 8}})
