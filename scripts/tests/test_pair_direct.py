"""#2944: scripts/satellite/pair-direct.py against a stub `rsi-rpc` (no live daemon).

The stub is a small fake of both daemons: it keeps a JSON world (hub registry,
satellite projects/policy/manager), enforces the revision fence and the
`deny_unknown_fields` request shapes the real RPCs have, and records every call.
"""

import json
import os
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

REPO = Path(__file__).resolve().parents[2]
TOOL = REPO / "scripts" / "satellite" / "pair-direct.py"
BRIEF = REPO / "scripts" / "satellite" / "manager-brief.md"

HUB_ID = "6b0bdc21-e34b-423e-aeda-090a2e3ab123"
SAT_ID = "a3f9d71b-20b4-4a48-bcec-b36a8734f022"
OTHER_ID = "2d37f97a-f8a2-441f-89f8-ef22949252b2"
FP = "SHA256:ZfKpeO26fJCnGNOkFkCnDgl3On5KFLesSaq3uA9gf/I"
HUB_SOCKET = "/hub/daemon.sock"
TUNNEL = "/hub/satellites/mac-mini-direct.sock"
WRITES = {
    "PutSatellitePeer",
    "PutSatelliteLink",
    "PutSatellitePeerScope",
    "CreateProject",
    "PutSatelliteInboundPolicy",
    "LaunchSession",
    "ConfigureHarnessManagerWithPolicy",
}

STUB = r'''#!{python}
import json, sys, uuid
state_path, calls_path = "{state}", "{calls}"
argv = sys.argv[1:]
socket = argv[argv.index("--socket") + 1]
method = [a for i, a in enumerate(argv) if a not in ("--socket", "--params") and not (i and argv[i-1] in ("--socket", "--params"))][0]
params = json.loads(argv[argv.index("--params") + 1]) if "--params" in argv else None
world = json.load(open(state_path))
side = "hub" if socket == "{hub}" else "sat" if socket == "{tunnel}" else None
open(calls_path, "a").write(json.dumps({{"side": side, "method": method, "params": params}}) + "\n")

def fail(message):
    print(json.dumps({{"jsonrpc": "2.0", "id": 1, "error": {{"code": -32000, "message": message}}}}))
    sys.exit(2)

def exact(value, keys, what):
    if not isinstance(value, dict) or set(value) != set(keys):
        fail("invalid %s: unknown or missing field in %s" % (what, sorted(value) if isinstance(value, dict) else value))

def save():
    json.dump(world, open(state_path, "w"))

def reply(result):
    print(json.dumps({{"jsonrpc": "2.0", "id": 1, "result": result}}))
    sys.exit(0)

def peer_by_id(pid):
    for p in world["hub"]["peers"]:
        if p["config"]["peer_id"] == pid:
            return p
    fail("satellite peer does not exist")

def bump(expected):
    if expected != world["hub"]["revision"]:
        fail("registry revision conflict")
    world["hub"]["revision"] += 1
    return world["hub"]["revision"]

if side is None:
    fail("unexpected socket " + socket)
if method == "GetSatelliteIdentity":
    ident = world["hub_id"] if side == "hub" else world["tunnel_id"]
    reply({{"wire_version": 1, "installation_id": ident, "daemon_incarnation_id": str(uuid.uuid4()),
            "protocol": {{"major": 1, "minor": 0}}, "capabilities": {{}}}})
if side == "hub":
    hub = world["hub"]
    if method == "ListSatellitePeers":
        reply(hub)
    if method == "PutSatellitePeer":
        exact(params, ["expected_registry_revision", "peer", "repair_quarantine"], "satellite peer")
        cfg = params["peer"]
        exact(cfg, ["peer_id", "label", "expected_installation_id", "enabled", "read_enabled", "dispatch_enabled"], "peer")
        if (cfg["read_enabled"] or cfg["dispatch_enabled"]) and (not cfg["enabled"] or not cfg["expected_installation_id"]):
            fail("active policy requires an enabled paired peer")
        if cfg["dispatch_enabled"] and not cfg["read_enabled"]:
            fail("dispatch requires read access to the peer")
        rev = bump(params["expected_registry_revision"])
        for p in hub["peers"]:
            if p["config"]["peer_id"] == cfg["peer_id"]:
                p["config"] = cfg
                p["row_version"] += 1
                break
        else:
            hub["peers"].append({{"config": cfg, "row_version": 1, "links": [], "observation": None,
                                 "dispatch_scope": [], "dispatch_project_scope": []}})
        save(); reply({{"revision": rev}})
    if method == "PutSatelliteLink":
        exact(params, ["expected_registry_revision", "peer_id", "link"], "satellite link")
        exact(params["link"], ["link_id", "direction", "socket_path", "ssh_target", "trust_reference", "enabled", "priority"], "link")
        if params["link"]["direction"] != "direct_local_forward" or not params["link"]["ssh_target"]:
            fail("direct link requires a configured SSH target")
        rev = bump(params["expected_registry_revision"])
        peer = peer_by_id(params["peer_id"])
        peer["links"] = [l for l in peer["links"] if l["link_id"] != params["link"]["link_id"]] + [params["link"]]
        save(); reply({{"revision": rev}})
    if method == "ProbeSatelliteLink":
        exact(params, ["peer_id", "link_id"], "satellite probe")
        peer = peer_by_id(params["peer_id"])
        link = [l for l in peer["links"] if l["link_id"] == params["link_id"]]
        if not link:
            fail("satellite link does not exist")
        if not peer["config"]["enabled"] or not link[0]["enabled"]:
            fail("satellite peer or link is disabled")
        expected = peer["config"]["expected_installation_id"]
        probed = world["probe_id"]
        reply({{"peer_id": params["peer_id"], "link_id": params["link_id"], "direction": link[0]["direction"],
                "trust_reference": link[0]["trust_reference"],
                "identity": {{"installation_id": probed}},
                "matches_expected": None if expected is None else expected == probed}})
    if method == "PutSatellitePeerScope":
        exact(params, ["expected_registry_revision", "peer_id", "remote_session_ids", "remote_project_ids"], "satellite scope")
        rev = bump(params["expected_registry_revision"])
        peer = peer_by_id(params["peer_id"])
        peer["dispatch_scope"] = params["remote_session_ids"]
        peer["dispatch_project_scope"] = params["remote_project_ids"]
        save(); reply({{"revision": rev}})
    fail("unknown hub method " + method)

sat = world["sat"]
if method == "ListProjects":
    reply(sat["projects"])
if method == "CreateProject":
    if not set(params) <= {{"name", "path", "description", "color"}}:
        fail("Invalid params")
    project = {{"id": str(uuid.uuid4()), "name": params["name"], "path": params.get("path")}}
    sat["projects"].append(project)
    save(); reply(project)
if method == "GetSatelliteInboundPolicy":
    reply(sat["policy"])
if method == "PutSatelliteInboundPolicy":
    exact(params, ["policy"], "satellite inbound policy")
    exact(params["policy"], ["allowed_hub_installations", "scope_roots", "scope_projects"], "policy")
    sat["policy"] = params["policy"]
    save(); reply(params["policy"])
if method == "GetHarnessManager":
    reply(sat["manager"])
if method == "GetSatelliteScopeStatus":
    if params is not None:
        fail("GetSatelliteScopeStatus takes no parameters")
    live = sat.get("seat_state", "live") == "live" and (sat["manager"] or {{}}).get("current_session_id")
    reply({{"inbound": [dict(kind="project", id=pid, state=sat.get("seat_state", "live"),
                           **({{"seat_session_id": sat["manager"]["current_session_id"]}} if live else {{}}))
                       for pid in sat["policy"]["scope_projects"]]}})
if method == "GetSession":
    exact(params, ["session_id"], "GetSession")
    status = sat.get("session_status", "Running")
    reply({{"id": params["session_id"], "status": status}})
if method == "ListScheduledJobs":
    if not set(params or {{}}) <= {{"include_history", "limit", "cursor"}}:
        fail("Invalid params")
    reply({{"jobs": sat.get("jobs", []), "next_cursor": None, "include_history": False}})
if method == "GetHarnessManagerPolicy":
    reply(sat["manager_policy"])
if method == "LaunchSession":
    if not params.get("tags") or not params.get("query"):
        fail("tags required")
    sid = str(uuid.uuid4())
    sat["launched"].append(dict(params, session_id=sid))
    save(); reply({{"session_id": sid}})
if method == "ConfigureHarnessManagerWithPolicy":
    exact(params, ["scope", "expected_policy_version", "idempotency_key", "policy"], "manager_invalid_request")
    exact(params["scope"], ["project_id", "session_id", "expected_row_version"], "scope")
    if params["scope"]["expected_row_version"] != (sat["manager"] or {{}}).get("row_version", 0):
        fail("manager_stale_scope")
    sat["manager"] = {{"project_id": params["scope"]["project_id"], "manager_session_id": params["scope"]["session_id"],
                      "current_session_id": params["scope"]["session_id"], "epic_ids": [], "scope_mode": "project",
                      "group_ids": [], "row_version": 1}}
    sat["manager_policy"] = {{"row_version": 1, "policy": params["policy"]}}
    save(); reply({{"config": sat["manager"], "policy": sat["manager_policy"]}})
fail("unknown satellite method " + method)
'''

SSH = r'''#!{python}
import os, sys
args = sys.argv[1:]
if "-G" in args:
    print("hostname 192.0.2.7\nuser tester\nport 22\nidentityfile ~/.ssh/id_ed25519_rsi\n"
          "userknownhostsfile ~/.ssh/known_hosts ~/.ssh/known_hosts2")
    sys.exit(0)
if args and args[-1].startswith("test -d"):
    sys.exit(1 if os.environ.get("FAKE_REPO_MISSING") else 0)
sys.exit(3)
'''

KEYGEN = r'''#!{python}
import os, sys
if "-F" not in sys.argv:
    sys.exit(1)
fp = os.environ.get("FAKE_HOSTKEY_FP", "{fp}")
if fp == "none":
    sys.exit(1)
print("# Host 192.0.2.7 found: line 3")
print("192.0.2.7 ED25519 " + fp)
sys.exit(0)
'''


def fresh_world():
    return {
        "hub_id": HUB_ID,
        "tunnel_id": SAT_ID,
        "probe_id": SAT_ID,
        "hub": {"revision": 4, "peers": []},
        "sat": {
            "projects": [],
            "policy": {"allowed_hub_installations": [], "scope_roots": [], "scope_projects": []},
            "manager": None,
            "manager_policy": None,
            "launched": [],
        },
    }


class PairDirectTest(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        root = Path(self.tmp.name)
        (root / "home").mkdir()
        self.state = root / "state.json"
        self.calls = root / "calls.jsonl"
        self.calls.write_text("")
        self.write_world(fresh_world())
        python = sys.executable
        self.rpc = self.script(
            root / "rsi-rpc",
            STUB.format(python=python, state=self.state, calls=self.calls, hub=HUB_SOCKET, tunnel=TUNNEL),
        )
        self.ssh = self.script(root / "ssh", SSH.format(python=python))
        self.keygen = self.script(root / "ssh-keygen", KEYGEN.format(python=python, fp=FP))
        self.env_file = root / "mac-mini-direct.env"
        self.env_file.write_text(
            "RSI_SATELLITE_TARGET=mac-mini\nRSI_SATELLITE_SSH_PORT=\nRSI_SATELLITE_SSH_JUMP=\n"
            f"RSI_SATELLITE_SSH_CONFIG=\nRSI_SATELLITE_LOCAL_SOCKET={TUNNEL}\n"
            "RSI_SATELLITE_REMOTE_SOCKET=/Users/example/.rsi/daemon.sock\n"
        )

    def script(self, path, text):
        path.write_text(text)
        path.chmod(0o755)
        return path

    def write_world(self, world):
        self.state.write_text(json.dumps(world))

    def world(self):
        return json.loads(self.state.read_text())

    def recorded(self):
        return [json.loads(line) for line in self.calls.read_text().splitlines() if line]

    def writes(self):
        return [c["method"] for c in self.recorded() if c["method"] in WRITES]

    def run_tool(self, *extra, env=None):
        environ = {k: v for k, v in os.environ.items() if k != "RSI_SESSION_TOKEN"}
        environ["HOME"] = str(Path(self.tmp.name) / "home")
        environ.update(env or {})
        cmd = [
            sys.executable,
            str(TOOL),
            "mac-mini",
            "--repo",
            "/Users/example/rsi",
            "--project-name",
            "Rsi",
            "--env-file",
            str(self.env_file),
            "--hub-socket",
            HUB_SOCKET,
            "--rsi-rpc",
            str(self.rpc),
            "--ssh",
            str(self.ssh),
            "--ssh-keygen",
            str(self.keygen),
            *extra,
        ]
        return subprocess.run(cmd, capture_output=True, text=True, env=environ, timeout=60)

    def peer(self):
        peers = self.world()["hub"]["peers"]
        self.assertEqual(len(peers), 1)
        return peers[0]

    def test_fresh_pairing_registers_verifies_and_scopes_without_dispatch(self):
        result = self.run_tool()
        self.assertEqual(result.returncode, 0, result.stderr + result.stdout)
        world = self.world()
        peer = self.peer()
        config = peer["config"]
        self.assertEqual(config["label"], "mac-mini")
        self.assertEqual(config["expected_installation_id"], SAT_ID)
        self.assertTrue(config["enabled"] and config["read_enabled"])
        self.assertFalse(config["dispatch_enabled"])
        (link,) = peer["links"]
        self.assertEqual(link["direction"], "direct_local_forward")
        self.assertEqual(link["socket_path"], TUNNEL)
        self.assertEqual(link["ssh_target"], "mac-mini")
        self.assertEqual(
            link["trust_reference"],
            f"ssh mac-mini = tester@192.0.2.7 with ~/.ssh/id_ed25519_rsi; host key ED25519 {FP} (known_hosts)",
        )
        (project,) = world["sat"]["projects"]
        self.assertEqual((project["name"], project["path"]), ("Rsi", "/Users/example/rsi"))
        self.assertEqual(peer["dispatch_project_scope"], [project["id"]])
        self.assertEqual(world["sat"]["policy"]["allowed_hub_installations"], [HUB_ID])
        self.assertEqual(world["sat"]["policy"]["scope_projects"], [project["id"]])
        self.assertIsNone(world["sat"]["manager"])
        # The probe ran before read was enabled, and the satellite was verified first.
        methods = [c["method"] for c in self.recorded()]
        self.assertLess(methods.index("ProbeSatelliteLink"), len(methods) - 1 - methods[::-1].index("PutSatellitePeer"))
        out = result.stdout
        for needle in (config["peer_id"], link["link_id"], project["id"], "registry_revision:  8", "dispatch:           off"):
            self.assertIn(needle, out)
        self.assertIn("--dispatch", out.split("Dispatch is off")[1])
        self.assertIn("scripts/satellite/pair-direct.py mac-mini --repo /Users/example/rsi --project-name Rsi --dispatch", out)

    def test_dispatch_and_manager_seat_with_conservative_policy(self):
        result = self.run_tool(
            "--dispatch",
            "--appoint-manager",
            "--manager-model",
            "claude-opus-5-5",
            "--manager-effort",
            "high",
        )
        self.assertEqual(result.returncode, 0, result.stderr + result.stdout)
        world = self.world()
        self.assertTrue(self.peer()["config"]["dispatch_enabled"])
        (launch,) = world["sat"]["launched"]
        self.assertEqual(launch["provider"], "Claude")
        self.assertEqual(launch["model"], "claude-opus-5-5")
        self.assertEqual(launch["effort"], "high")
        self.assertEqual(launch["working_dir"], "/Users/example/rsi")
        self.assertEqual(launch["project_id"], world["sat"]["projects"][0]["id"])
        self.assertIn("Satellite manager brief: mac-mini", launch["query"])
        self.assertIn("AgentReportToHub", launch["query"])
        self.assertNotIn("{{", launch["query"])
        policy = world["sat"]["manager_policy"]["policy"]
        self.assertEqual(policy["mode"], "execute")
        self.assertEqual(policy["max_active_sessions"], 4)
        self.assertNotIn("deploy", policy["capabilities"])
        self.assertNotIn("git_effect", policy["capabilities"])
        self.assertEqual(
            policy["allowed_launches"],
            [{"provider": "Claude", "model": "claude-opus-5-5", "effort": "high"}],
        )
        self.assertEqual(world["sat"]["manager"]["manager_session_id"], launch["session_id"])
        self.assertIn(launch["session_id"], result.stdout)
        self.assertNotIn("Dispatch is off", result.stdout)

    def test_rerun_converges_to_a_noop(self):
        args = ("--dispatch", "--appoint-manager", "--manager-model", "claude-opus-5-5")
        first = self.run_tool(*args)
        self.assertEqual(first.returncode, 0, first.stderr)
        before = self.world()
        self.calls.write_text("")
        second = self.run_tool(*args)
        self.assertEqual(second.returncode, 0, second.stderr + second.stdout)
        self.assertEqual(self.writes(), [])
        self.assertEqual(self.world(), before)
        self.assertEqual(len(before["sat"]["launched"]), 1)
        self.assertIn(before["sat"]["manager"]["manager_session_id"], second.stdout)

    def test_partial_prior_state_is_completed_and_foreign_entries_are_kept(self):
        world = fresh_world()
        peer_id = "f27d5d4e-35f5-4df3-b1b7-52ac41fdd67f"
        world["hub"]["peers"].append(
            {
                "config": {
                    "peer_id": peer_id,
                    "label": "mac-mini",
                    "expected_installation_id": None,
                    "enabled": True,
                    "read_enabled": False,
                    "dispatch_enabled": False,
                },
                "row_version": 2,
                "links": [],
                "observation": None,
                "dispatch_scope": [OTHER_ID],
                "dispatch_project_scope": [],
            }
        )
        world["sat"]["projects"].append({"id": "bf71fc49-04c6-4109-96a5-4ea4895a16ed", "name": "Rsi", "path": "/Users/example/rsi/"})
        world["sat"]["policy"] = {
            "allowed_hub_installations": [OTHER_ID],
            "scope_roots": ["11111111-1111-4111-8111-111111111111"],
            "scope_projects": [],
        }
        self.write_world(world)
        result = self.run_tool()
        self.assertEqual(result.returncode, 0, result.stderr + result.stdout)
        after = self.world()
        peer = self.peer()
        self.assertEqual(peer["config"]["peer_id"], peer_id)
        self.assertEqual(peer["config"]["expected_installation_id"], SAT_ID)
        self.assertEqual(len(peer["links"]), 1)
        self.assertEqual(peer["dispatch_scope"], [OTHER_ID])
        self.assertEqual(peer["dispatch_project_scope"], ["bf71fc49-04c6-4109-96a5-4ea4895a16ed"])
        self.assertEqual(len(after["sat"]["projects"]), 1)
        self.assertNotIn("CreateProject", self.writes())
        policy = after["sat"]["policy"]
        self.assertEqual(policy["allowed_hub_installations"], [OTHER_ID, HUB_ID])
        self.assertEqual(policy["scope_roots"], ["11111111-1111-4111-8111-111111111111"])
        self.assertEqual(policy["scope_projects"], ["bf71fc49-04c6-4109-96a5-4ea4895a16ed"])

    def test_peer_paired_with_another_installation_is_refused_before_any_write(self):
        world = fresh_world()
        world["hub"]["peers"].append(
            {
                "config": {
                    "peer_id": "f27d5d4e-35f5-4df3-b1b7-52ac41fdd67f",
                    "label": "mac-mini",
                    "expected_installation_id": OTHER_ID,
                    "enabled": True,
                    "read_enabled": True,
                    "dispatch_enabled": False,
                },
                "row_version": 3,
                "links": [],
                "observation": None,
                "dispatch_scope": [],
                "dispatch_project_scope": [],
            }
        )
        self.write_world(world)
        result = self.run_tool("--dispatch")
        self.assertEqual(result.returncode, 2, result.stdout)
        self.assertIn(OTHER_ID, result.stderr)
        self.assertIn(SAT_ID, result.stderr)
        self.assertEqual(self.writes(), [])

    def test_probe_identity_differing_from_the_tunnel_identity_never_enables_the_peer(self):
        world = fresh_world()
        world["probe_id"] = OTHER_ID
        self.write_world(world)
        result = self.run_tool("--dispatch")
        self.assertEqual(result.returncode, 2, result.stdout)
        self.assertIn("installation mismatch", result.stderr)
        config = self.peer()["config"]
        self.assertIsNone(config["expected_installation_id"])
        self.assertFalse(config["read_enabled"] or config["dispatch_enabled"])
        self.assertEqual(self.world()["sat"]["projects"], [])
        self.assertNotIn("PutSatellitePeerScope", self.writes())
        self.assertNotIn("PutSatelliteInboundPolicy", self.writes())

    def test_tunnel_answering_as_the_hub_is_refused(self):
        world = fresh_world()
        world["tunnel_id"] = HUB_ID
        self.write_world(world)
        result = self.run_tool()
        self.assertEqual(result.returncode, 2)
        self.assertEqual(self.writes(), [])

    def test_session_token_in_the_environment_is_refused_before_any_call(self):
        result = self.run_tool(env={"RSI_SESSION_TOKEN": "not-a-real-token"})
        self.assertEqual(result.returncode, 2)
        self.assertIn("RSI_SESSION_TOKEN", result.stderr)
        self.assertNotIn("not-a-real-token", result.stderr + result.stdout)
        self.assertEqual(self.recorded(), [])

    def test_dry_run_prints_every_write_and_changes_nothing(self):
        before = self.world()
        result = self.run_tool(
            "--dry-run", "--dispatch", "--appoint-manager", "--manager-model", "claude-opus-5-5"
        )
        self.assertEqual(result.returncode, 0, result.stderr + result.stdout)
        self.assertEqual(self.writes(), [])
        self.assertEqual(self.world(), before)
        for method in (
            "PutSatellitePeer",
            "PutSatelliteLink",
            "CreateProject",
            "PutSatelliteInboundPolicy",
            "LaunchSession",
            "ConfigureHarnessManagerWithPolicy",
            "PutSatellitePeerScope",
        ):
            self.assertIn(f"would call", result.stdout)
            self.assertIn(f"{method}", result.stdout, method)
        self.assertIn("dry run", result.stdout)
        self.assertNotIn("Satellite manager brief", result.stdout)

    def test_changed_host_key_on_an_existing_link_needs_an_explicit_flag(self):
        self.assertEqual(self.run_tool().returncode, 0)
        self.calls.write_text("")
        changed = {"FAKE_HOSTKEY_FP": "SHA256:ChangedChangedChangedChangedChangedChang"}
        refused = self.run_tool(env=changed)
        self.assertEqual(refused.returncode, 2, refused.stdout)
        self.assertIn("fingerprint changed", refused.stderr)
        self.assertEqual(self.writes(), [])
        accepted = self.run_tool("--update-trust-reference", env=changed)
        self.assertEqual(accepted.returncode, 0, accepted.stderr)
        (link,) = self.peer()["links"]
        self.assertIn("ChangedChanged", link["trust_reference"])

    def test_unknown_host_key_and_missing_repo_are_refused_without_writes(self):
        missing_key = self.run_tool(env={"FAKE_HOSTKEY_FP": "none"})
        self.assertEqual(missing_key.returncode, 2)
        self.assertIn("known_hosts", missing_key.stderr)
        missing_repo = self.run_tool(env={"FAKE_REPO_MISSING": "1"})
        self.assertEqual(missing_repo.returncode, 2)
        self.assertIn("not a directory", missing_repo.stderr)
        self.assertEqual(self.writes(), [])

    def test_project_name_clash_at_another_path_is_refused(self):
        world = fresh_world()
        world["sat"]["projects"].append({"id": "bf71fc49-04c6-4109-96a5-4ea4895a16ed", "name": "Rsi", "path": "/elsewhere"})
        self.write_world(world)
        result = self.run_tool()
        self.assertEqual(result.returncode, 2)
        self.assertIn("already exists", result.stderr)
        self.assertNotIn("CreateProject", self.writes())

    def test_manager_launch_choices_and_cap_come_from_flags(self):
        result = self.run_tool(
            "--appoint-manager",
            "--manager-model",
            "claude-opus-5-5",
            "--allowed-launch",
            "claude:claude-sonnet-5-5:high",
            "--allowed-launch",
            "Codex:gpt-6-astra",
            "--max-active-sessions",
            "3",
        )
        self.assertEqual(result.returncode, 0, result.stderr + result.stdout)
        policy = self.world()["sat"]["manager_policy"]["policy"]
        self.assertEqual(policy["max_active_sessions"], 3)
        self.assertEqual(
            policy["allowed_launches"],
            [
                {"provider": "Claude", "model": "claude-sonnet-5-5", "effort": "high"},
                {"provider": "Codex", "model": "gpt-6-astra", "effort": None},
            ],
        )

    def test_manager_is_not_replaced_when_one_is_live(self):
        world = fresh_world()
        world["sat"]["projects"].append({"id": "bf71fc49-04c6-4109-96a5-4ea4895a16ed", "name": "Rsi", "path": "/Users/example/rsi"})
        seat = "22222222-2222-4222-8222-222222222222"
        world["sat"]["manager"] = {
            "project_id": "bf71fc49-04c6-4109-96a5-4ea4895a16ed",
            "manager_session_id": seat,
            "current_session_id": seat,
            "epic_ids": [],
            "scope_mode": "project",
            "group_ids": [],
            "row_version": 5,
        }
        self.write_world(world)
        result = self.run_tool("--appoint-manager", "--manager-model", "claude-opus-5-5")
        self.assertEqual(result.returncode, 0, result.stderr + result.stdout)
        self.assertNotIn("LaunchSession", self.writes())
        self.assertNotIn("ConfigureHarnessManagerWithPolicy", self.writes())
        self.assertIn(seat, result.stdout)

    def seed_seat(self, seat_state, scope_mode="project", session_status="Running", jobs=()):
        """A satellite whose project already has an appointed seat in `seat_state`.

        `session_status` is the seat session's own SessionStatus and `jobs` the
        scheduled wakes ListScheduledJobs returns.
        """
        world = fresh_world()
        world["sat"]["session_status"] = session_status
        world["sat"]["jobs"] = list(jobs)
        project = "bf71fc49-04c6-4109-96a5-4ea4895a16ed"
        seat = "22222222-2222-4222-8222-222222222222"
        world["sat"]["projects"].append({"id": project, "name": "Rsi", "path": "/Users/example/rsi"})
        world["sat"]["seat_state"] = seat_state
        world["sat"]["manager"] = {
            "project_id": project,
            "manager_session_id": seat,
            "current_session_id": seat,
            "epic_ids": [],
            "scope_mode": scope_mode,
            "group_ids": [],
            "row_version": 5,
        }
        self.write_world(world)
        return project, seat

    def test_live_seat_is_reported_live_by_the_scope_status_and_left_alone(self):
        _, seat = self.seed_seat("live")
        result = self.run_tool("--appoint-manager", "--manager-model", "claude-opus-5-5")
        self.assertEqual(result.returncode, 0, result.stderr + result.stdout)
        self.assertIn("GetSatelliteScopeStatus", [c["method"] for c in self.recorded()])
        self.assertIn(f"manager {seat} already appointed and live", result.stdout)
        self.assertEqual(self.writes().count("ConfigureHarnessManagerWithPolicy"), 0)
        self.assertNotIn("LaunchSession", self.writes())

    SEAT = "22222222-2222-4222-8222-222222222222"

    def wake(self, **fields):
        job = {
            "id": "44444444-4444-4444-8444-444444444444",
            "name": "park",
            "enabled": True,
            "wake_mode": "resume",
            "wake_session_id": self.SEAT,
        }
        return dict(job, **fields)

    def assert_left_alone(self, result):
        self.assertEqual(result.returncode, 0, result.stderr + result.stdout)
        self.assertIn(f"manager {self.SEAT} already appointed and live", result.stdout)
        self.assertNotIn("ConfigureHarnessManagerWithPolicy", self.writes())
        self.assertNotIn("LaunchSession", self.writes())

    def assert_dead_seat_refused(self, result, status):
        self.assertEqual(result.returncode, 1, result.stderr + result.stdout)
        self.assertNotIn("already appointed", result.stdout)
        for needle in (self.SEAT, "not live", status, "AgentReseatSatelliteManager", "--manager-session"):
            self.assertIn(needle, result.stderr)
        self.assertNotIn("ConfigureHarnessManagerWithPolicy", self.writes())
        self.assertNotIn("LaunchSession", self.writes())
        self.assertEqual(self.world()["sat"]["manager"]["current_session_id"], self.SEAT)

    def test_running_seat_stays_live_without_reading_wakes(self):
        for status in ("Starting", "Running", "WaitingApproval"):
            with self.subTest(status=status):
                self.seed_seat("live", session_status=status)
                self.assert_left_alone(self.run_tool("--appoint-manager", "--manager-model", "claude-opus-5-5"))
                methods = [c["method"] for c in self.recorded()]
                self.assertIn("GetSession", methods)
                self.assertNotIn("ListScheduledJobs", methods)

    def test_failed_seat_is_not_live_even_with_a_wake_armed(self):
        self.seed_seat("live", session_status="Failed", jobs=[self.wake()])
        result = self.run_tool("--appoint-manager", "--manager-model", "claude-opus-5-5")
        self.assert_dead_seat_refused(result, "Failed")

    def test_failed_seat_is_replaced_when_a_live_session_is_named(self):
        self.seed_seat("live", session_status="Failed")
        successor = "33333333-3333-4333-8333-333333333333"
        result = self.run_tool("--appoint-manager", "--manager-model", "claude-opus-5-5", "--manager-session", successor)
        self.assertEqual(result.returncode, 0, result.stderr + result.stdout)
        self.assertNotIn("already appointed", result.stdout)
        self.assertNotIn("LaunchSession", self.writes())
        self.assertIn("ConfigureHarnessManagerWithPolicy", self.writes())
        self.assertEqual(self.world()["sat"]["manager"]["current_session_id"], successor)

    def test_completed_seat_with_nothing_armed_is_not_live(self):
        other = "55555555-5555-4555-8555-555555555555"
        cases = {
            "no jobs": [],
            "disabled wake": [self.wake(enabled=False)],
            "another session's wake": [self.wake(wake_session_id=other)],
            "fresh launch wake": [self.wake(wake_mode="fresh")],
            "agent_fresh wake (separate session)": [self.wake(wake_mode="agent_fresh")],
        }
        for name, jobs in cases.items():
            with self.subTest(name):
                self.seed_seat("live", session_status="Completed", jobs=jobs)
                result = self.run_tool("--appoint-manager", "--manager-model", "claude-opus-5-5")
                self.assert_dead_seat_refused(result, "Completed")
                self.assertIn("ListScheduledJobs", [c["method"] for c in self.recorded()])

    def test_completed_seat_with_an_armed_wake_stays_live(self):
        watched = "66666666-6666-4666-8666-666666666666"
        for name, job in {
            "resume": self.wake(),
            "when/on_terminal": self.wake(wake_mode={"on_terminal": watched}),
        }.items():
            with self.subTest(name):
                self.seed_seat("live", session_status="Completed", jobs=[job])
                self.assert_left_alone(self.run_tool("--appoint-manager", "--manager-model", "claude-opus-5-5"))

    def test_archived_seat_is_not_reported_as_appointed_and_names_the_recovery(self):
        project, seat = self.seed_seat("no_live_seat")
        result = self.run_tool("--appoint-manager", "--manager-model", "claude-opus-5-5")
        self.assertEqual(result.returncode, 1, result.stderr + result.stdout)
        self.assertNotIn("already appointed", result.stdout)
        self.assertNotIn("== summary", result.stdout)
        for needle in (seat, "not live", "no_live_seat", "AgentReseatSatelliteManager", "--manager-session", project):
            self.assertIn(needle, result.stderr)
        self.assertNotIn("LaunchSession", self.writes())
        self.assertNotIn("ConfigureHarnessManagerWithPolicy", self.writes())
        # Nothing was changed: the dead seat is still the stored appointment.
        self.assertEqual(self.world()["sat"]["manager"]["current_session_id"], seat)

    def test_archived_seat_is_replaced_when_a_live_session_is_named(self):
        project, _ = self.seed_seat("no_live_seat")
        successor = "33333333-3333-4333-8333-333333333333"
        result = self.run_tool("--appoint-manager", "--manager-model", "claude-opus-5-5", "--manager-session", successor)
        self.assertEqual(result.returncode, 0, result.stderr + result.stdout)
        self.assertNotIn("already appointed", result.stdout)
        self.assertIn("ConfigureHarnessManagerWithPolicy", self.writes())
        self.assertNotIn("LaunchSession", self.writes())
        self.assertEqual(self.world()["sat"]["manager"]["current_session_id"], successor)

    def test_revoked_scope_is_reappointed_without_consulting_the_seat(self):
        _, seat = self.seed_seat("revoked", scope_mode="selected")
        result = self.run_tool("--appoint-manager", "--manager-model", "claude-opus-5-5")
        self.assertEqual(result.returncode, 0, result.stderr + result.stdout)
        self.assertNotIn("already appointed", result.stdout)
        self.assertIn("ConfigureHarnessManagerWithPolicy", self.writes())
        (launch,) = self.world()["sat"]["launched"]
        manager = self.world()["sat"]["manager"]
        self.assertEqual(manager["current_session_id"], launch["session_id"])
        self.assertNotEqual(manager["current_session_id"], seat)

    def test_shipped_brief_is_short_and_cites_instead_of_copying(self):
        text = BRIEF.read_text()
        self.assertLess(len(text.splitlines()), 80)
        for needle in (
            ".claude/skills/rsi-project-manager/SKILL.md",
            "docs/satellites.md",
            "AgentReportToHub",
            "enqueue",
            "--stage-dir",
            "AgentRequestDeploy",
            "3 concurrent workers",
            "-j 6",
            "#1229",
            "origin",
        ):
            self.assertIn(needle, text)


if __name__ == "__main__":
    unittest.main()
