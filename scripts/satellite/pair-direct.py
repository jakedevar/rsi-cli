#!/usr/bin/env python3
"""Pair a direct-mode satellite with this hub in one idempotent command (#2944).

Operator tool, run on the hub after `install-direct.sh NAME SSH_TARGET` has
installed the tunnel (`~/.rsi/satellites/NAME-direct.sock`). It drives the
operator-only RPCs on BOTH daemons through `rsi-rpc --socket`:

  hub        PutSatellitePeer -> PutSatelliteLink -> ProbeSatelliteLink
             -> PutSatellitePeer (paired, read/dispatch) -> PutSatellitePeerScope
  satellite  CreateProject (if missing) -> PutSatelliteInboundPolicy
             -> optional manager seat (LaunchSession + ConfigureHarnessManagerWithPolicy)

Agents must never run it: it refuses to start with RSI_SESSION_TOKEN set.
Rerunning converges: an existing peer (by label or installation), link (by
socket path), project (by path) and inbound entries are reused or merged, never
replaced, and a manager is appointed only when the project has none live.
Contract and wire shapes: docs/satellites.md ("Pair a direct satellite").
"""

import argparse
import json
import os
import re
import shlex
import subprocess
import sys
import uuid
from pathlib import Path

EXIT_FAILED = 1
EXIT_REFUSED = 2

# The Execute preset of rsi-common harness_manager_presets (no topology,
# git_effect, storage, daemon-setting or deploy grant: the hub restarts
# satellites, a satellite manager never deploys itself).
EXECUTE_CAPABILITIES = [
    "work_plan",
    "lead_control",
    "session_create",
    "lead_assign",
    "integration",
    "self_succession",
    "session_control",
    "issue_coordinate",
]
PROVIDERS = [
    "Claude",
    "Codex",
    "Pioneer",
    "OpenRouter",
    "Bedrock",
    "Local",
    "Antigravity",
    "CodexAppServer",
    "Harness",
]
UUID_RE = re.compile(r"^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$")
HERE = Path(__file__).resolve().parent
DEFAULT_BRIEF = HERE / "manager-brief.md"
DRY_PROJECT_ID = "<new-project-id>"


class Refused(Exception):
    """A precondition the operator must fix; nothing was changed by it."""


class Failed(Exception):
    """A call or check failed; rerunning after the fix converges."""


def canonical_provider(name):
    for known in PROVIDERS:
        if known.lower() == name.strip().lower():
            return known
    raise Refused(f"unknown provider {name!r}; one of {', '.join(PROVIDERS)}")


def parse_launch(text):
    parts = text.split(":", 2)
    if len(parts) < 2 or not parts[1].strip():
        raise Refused(f"--allowed-launch {text!r} must be PROVIDER:MODEL[:EFFORT]")
    choice = {"provider": canonical_provider(parts[0]), "model": parts[1].strip()}
    choice["effort"] = parts[2].strip() if len(parts) == 3 and parts[2].strip() else None
    return choice


def parse_args(argv):
    p = argparse.ArgumentParser(
        prog="pair-direct.py",
        description="Pair a direct-mode satellite with this hub (idempotent, operator-only).",
    )
    p.add_argument("name", metavar="NAME", help="satellite name (the install-direct.sh NAME; peer label)")
    p.add_argument("--repo", required=True, help="repository path ON THE SATELLITE (the project path)")
    p.add_argument("--project-name", help="satellite project name (default: the repo directory name)")
    p.add_argument("--dispatch", action="store_true", help="enable hub-manager dispatch to this peer")
    p.add_argument("--appoint-manager", action="store_true", help="launch and appoint a satellite manager seat")
    p.add_argument("--manager-brief", help=f"first prompt for the seat (default: {DEFAULT_BRIEF.name} next to this tool)")
    p.add_argument("--manager-provider", default="Claude")
    p.add_argument("--manager-model", help="required with --appoint-manager")
    p.add_argument("--manager-effort", help="seat effort, e.g. high")
    p.add_argument(
        "--manager-session",
        help="appoint this existing satellite session instead of launching a new seat",
    )
    p.add_argument(
        "--allowed-launch",
        action="append",
        default=[],
        metavar="PROVIDER:MODEL[:EFFORT]",
        help="a launch choice the seat may use for its workers (repeatable; default: the seat's own)",
    )
    p.add_argument("--max-active-sessions", type=int, help="seat concurrency cap (default 4)")
    p.add_argument("--manager-policy-file", help="ManagerPolicyV2 JSON; replaces the conservative defaults")
    p.add_argument("--env-file", help="direct-tunnel env file (default ~/.config/rsi/satellites/NAME-direct.env)")
    p.add_argument("--hub-socket", help="hub daemon socket (default $RSI_DAEMON_SOCKET_PATH or ~/.rsi/daemon.sock)")
    p.add_argument("--rsi-rpc", default=os.environ.get("RSI_PAIR_RSI_RPC", "rsi-rpc"))
    p.add_argument("--ssh", default=os.environ.get("RSI_PAIR_SSH", "ssh"))
    p.add_argument("--ssh-keygen", default=os.environ.get("RSI_PAIR_SSH_KEYGEN", "ssh-keygen"))
    p.add_argument("--trust-reference", help="use this trust reference text instead of computing it")
    p.add_argument(
        "--update-trust-reference",
        action="store_true",
        help="accept a changed host key fingerprint on an existing link",
    )
    p.add_argument("--no-repo-check", action="store_true", help="skip the ssh check that --repo exists")
    p.add_argument("--dry-run", action="store_true", help="read and print every call; change nothing")
    args = p.parse_args(argv)
    if args.appoint_manager and not (args.manager_model or args.manager_session):
        p.error("--appoint-manager needs --manager-model (or --manager-session)")
    if args.max_active_sessions is not None and not 1 <= args.max_active_sessions <= 100:
        p.error("--max-active-sessions must be 1-100")
    if args.manager_session and not args.appoint_manager:
        p.error("--manager-session needs --appoint-manager")
    if not os.path.isabs(args.repo):
        p.error("--repo must be an absolute path on the satellite")
    return args


def load_env_file(path):
    values = {}
    try:
        text = Path(path).read_text()
    except OSError as error:
        raise Refused(f"cannot read the tunnel env file {path}: {error}; run install-direct.sh first")
    for line in text.splitlines():
        line = line.strip()
        if not line or line.startswith("#") or "=" not in line:
            continue
        key, _, value = line.partition("=")
        values[key.strip()] = value.strip().strip("'\"")
    for key in ("RSI_SATELLITE_TARGET", "RSI_SATELLITE_LOCAL_SOCKET"):
        if not values.get(key):
            raise Refused(f"{path} lacks {key}")
    return values


# SessionStatus values (crates/rsi-common/src/types.rs): running ones need no wake;
# Completed and Interrupted are live only while a wake armed to resume that same
# session exists (agent_fresh launches a separate session, so it does not count).
RUNNING_STATUSES = ("Starting", "Running", "WaitingApproval")
RESUMABLE_STATUSES = ("Completed", "Interrupted")
SCHEDULED_JOB_PAGE_SIZE = 200
SCHEDULED_JOB_PAGES = 20


def armed_wake_for(job, seat):
    if not isinstance(job, dict) or not job.get("enabled"):
        return False
    if job.get("wake_session_id") != seat:
        return False
    mode = job.get("wake_mode")
    return mode == "resume" or (isinstance(mode, dict) and "on_terminal" in mode)


class Rpc:
    """`rsi-rpc --socket SOCKET METHOD --params JSON` with a printed call log."""

    def __init__(self, binary, socket, label, dry_run, out):
        self.binary, self.socket, self.label = binary, socket, label
        self.dry_run, self.out = dry_run, out

    def _show(self, params):
        shown = dict(params) if isinstance(params, dict) else params
        if isinstance(shown, dict) and isinstance(shown.get("query"), str):
            shown["query"] = f"<{len(shown['query'].encode())} byte brief>"
        return "" if params is None else " " + json.dumps(shown, separators=(",", ":"))

    def call(self, method, params=None, write=False):
        """The result, or None for a write skipped by --dry-run."""
        if write and self.dry_run:
            self.out(f"  would call [{self.label}] {method}{self._show(params)}")
            return None
        self.out(f"  call [{self.label}] {method}{self._show(params)}")
        cmd = [self.binary, "--socket", self.socket, method]
        if params is not None:
            cmd += ["--params", json.dumps(params, separators=(",", ":"))]
        try:
            proc = subprocess.run(cmd, capture_output=True, text=True, timeout=120)
        except (OSError, subprocess.TimeoutExpired) as error:
            raise Failed(f"[{self.label}] {method}: cannot run {self.binary}: {error}")
        try:
            response = json.loads(proc.stdout) if proc.stdout.strip() else None
        except json.JSONDecodeError:
            response = None
        if isinstance(response, dict) and response.get("error"):
            error = response["error"]
            message = error.get("message", error) if isinstance(error, dict) else error
            raise Failed(f"[{self.label}] {method} refused: {message}")
        if proc.returncode != 0 or not isinstance(response, dict):
            detail = (proc.stderr or proc.stdout).strip()[:300]
            raise Failed(f"[{self.label}] {method} failed (exit {proc.returncode}): {detail}")
        return response.get("result")


def run_tool(binary, argv, what):
    try:
        proc = subprocess.run([binary, *argv], capture_output=True, text=True, timeout=60)
    except (OSError, subprocess.TimeoutExpired) as error:
        raise Failed(f"{what}: cannot run {binary}: {error}")
    return proc


def ssh_options(env):
    options = []
    if env.get("RSI_SATELLITE_SSH_PORT"):
        options += ["-p", env["RSI_SATELLITE_SSH_PORT"]]
    if env.get("RSI_SATELLITE_SSH_JUMP"):
        options += ["-J", env["RSI_SATELLITE_SSH_JUMP"]]
    if env.get("RSI_SATELLITE_SSH_CONFIG"):
        options += ["-F", env["RSI_SATELLITE_SSH_CONFIG"]]
    return options


def tilde(path):
    home = str(Path.home())
    return "~" + path[len(home):] if path == home or path.startswith(home + "/") else path


def compute_trust_reference(args, env):
    """`ssh ALIAS = user@host[:port] with KEY; host key TYPE SHA256:... (known_hosts)`."""
    alias = env["RSI_SATELLITE_TARGET"]
    proc = run_tool(args.ssh, [*ssh_options(env), "-G", alias], "ssh -G")
    if proc.returncode != 0:
        raise Failed(f"ssh -G {alias} failed: {proc.stderr.strip()[:200]}")
    config = {}
    identities, known_hosts = [], []
    for line in proc.stdout.splitlines():
        key, _, value = line.partition(" ")
        key = key.lower()
        if key == "identityfile":
            identities.append(value.strip())
        elif key == "userknownhostsfile":
            known_hosts += value.split()
        else:
            config.setdefault(key, value.strip())
    host, user, port = config.get("hostname"), config.get("user"), config.get("port", "22")
    if not host or not user:
        raise Failed(f"ssh -G {alias} reported no hostname or user")
    expanded = [os.path.expanduser(item) for item in identities]
    existing = [item for item in expanded if os.path.exists(item)]
    identity = tilde((existing or expanded or [""])[0])
    lookup = host if port == "22" else f"[{host}]:{port}"
    keygen_args = ["-F", lookup, "-l"]
    for candidate in known_hosts:
        candidate = os.path.expanduser(candidate)
        if os.path.exists(candidate):
            keygen_args += ["-f", candidate]
            break
    scan = run_tool(args.ssh_keygen, keygen_args, "ssh-keygen -F")
    keys = []
    for line in scan.stdout.splitlines():
        parts = line.split()
        if line.startswith("#") or len(parts) < 3 or not parts[2].startswith("SHA256:"):
            continue
        keys.append((parts[1], parts[2]))
    if not keys:
        raise Refused(
            f"no host key for {lookup} in known_hosts: connect once with `ssh {alias}` and verify "
            "the fingerprint, then rerun"
        )
    key_type, fingerprint = next((k for k in keys if k[0] == "ED25519"), keys[0])
    where = f"{user}@{host}" + ("" if port == "22" else f":{port}")
    with_key = f" with {identity}" if identity else ""
    text = f"ssh {alias} = {where}{with_key}; host key {key_type} {fingerprint} (known_hosts)"
    if len(text.encode()) > 256:
        raise Refused(f"trust reference exceeds 256 bytes: {text}")
    return text


def fingerprint_of(trust_reference):
    match = re.search(r"SHA256:\S+", trust_reference or "")
    return match.group(0) if match else None


def check_repo(args, env):
    if args.no_repo_check:
        return
    proc = run_tool(
        args.ssh,
        ["-o", "BatchMode=yes", *ssh_options(env), env["RSI_SATELLITE_TARGET"],
         "test -d " + shlex.quote(args.repo)],
        "repo check",
    )
    if proc.returncode != 0:
        raise Refused(
            f"{args.repo} is not a directory on {env['RSI_SATELLITE_TARGET']} (or ssh failed: "
            f"{proc.stderr.strip()[:120]}); fix --repo or pass --no-repo-check"
        )


def valid_uuid(value, what):
    if not isinstance(value, str) or not UUID_RE.match(value):
        raise Failed(f"{what} is not a canonical lowercase uuid: {value!r}")
    return value


class Pairing:
    def __init__(self, args, env, out):
        self.args, self.env, self.out = args, env, out
        self.name = args.name
        hub_socket = args.hub_socket or os.environ.get("RSI_DAEMON_SOCKET_PATH") or str(
            Path.home() / ".rsi" / "daemon.sock"
        )
        self.local_socket = env["RSI_SATELLITE_LOCAL_SOCKET"]
        self.hub = Rpc(args.rsi_rpc, hub_socket, "hub", args.dry_run, out)
        self.sat = Rpc(args.rsi_rpc, self.local_socket, "satellite", args.dry_run, out)
        self.dry = args.dry_run
        self.revision = None
        self.summary = {}
        self.dispatch_on = False

    def step(self, text):
        self.out(f"\n== {text}")

    # -- hub registry helpers -------------------------------------------------

    def registry(self):
        view = self.hub.call("ListSatellitePeers")
        if not isinstance(view, dict) or "peers" not in view:
            raise Failed("ListSatellitePeers returned an unexpected shape")
        self.revision = view["revision"]
        return view

    def hub_write(self, method, params):
        result = self.hub.call(method, params, write=True)
        if result is None:
            self.revision += 1
        else:
            self.revision = result["revision"]

    def find_peer(self, view, satellite_id):
        by_label = [p for p in view["peers"] if p["config"]["label"] == self.name]
        by_installation = [
            p for p in view["peers"] if p["config"].get("expected_installation_id") == satellite_id
        ]
        if by_label and by_installation and by_label[0] is not by_installation[0]:
            raise Refused(
                f"label {self.name!r} is peer {by_label[0]['config']['peer_id']} but installation "
                f"{satellite_id} is already paired as {by_installation[0]['config']['label']!r} "
                f"({by_installation[0]['config']['peer_id']}); resolve in the TUI satellite registry"
            )
        peer = (by_label or by_installation or [None])[0]
        if peer is not None:
            expected = peer["config"].get("expected_installation_id")
            if expected and expected != satellite_id:
                raise Refused(
                    f"peer {self.name!r} is paired with installation {expected} but the tunnel "
                    f"{self.local_socket} answers as {satellite_id}: the tunnel points at another "
                    "machine, or the satellite was reinstalled. Not changing anything; repair the "
                    "peer in the TUI (repair quarantine) if the reinstall is intended"
                )
        return peer

    # -- steps ----------------------------------------------------------------

    def identities(self):
        self.step("identities")
        satellite = self.sat.call("GetSatelliteIdentity", {})
        hub = self.hub.call("GetSatelliteIdentity", {})
        sat_id = valid_uuid((satellite or {}).get("installation_id"), "satellite installation id")
        hub_id = valid_uuid((hub or {}).get("installation_id"), "hub installation id")
        if sat_id == hub_id:
            raise Refused(f"the tunnel {self.local_socket} answers with the hub's own installation id")
        self.out(f"  satellite installation {sat_id}\n  hub installation       {hub_id}")
        return sat_id, hub_id

    def pair_hub(self, satellite_id):
        self.step("hub: peer, link, probe")
        view = self.registry()
        peer = self.find_peer(view, satellite_id)
        trust = self.args.trust_reference or compute_trust_reference(self.args, self.env)
        self.out(f"  trust_reference: {trust}")
        if peer is None:
            peer_id = str(uuid.uuid4())
            config = {
                "peer_id": peer_id,
                "label": self.name,
                "expected_installation_id": None,
                "enabled": True,
                "read_enabled": False,
                "dispatch_enabled": False,
            }
            self.hub_write(
                "PutSatellitePeer",
                {"expected_registry_revision": self.revision, "peer": config, "repair_quarantine": False},
            )
            links = []
        else:
            config = dict(peer["config"])
            peer_id = config["peer_id"]
            if not config["enabled"]:
                config["enabled"] = True
                self.hub_write(
                    "PutSatellitePeer",
                    {"expected_registry_revision": self.revision, "peer": config, "repair_quarantine": False},
                )
            else:
                self.out(f"  peer {peer_id} already registered")
            links = peer["links"]
        link = next((l for l in links if l["socket_path"] == self.local_socket), None)
        if link is None:
            link_cfg = {
                "link_id": str(uuid.uuid4()),
                "direction": "direct_local_forward",
                "socket_path": self.local_socket,
                "ssh_target": self.env["RSI_SATELLITE_TARGET"],
                "trust_reference": trust,
                "enabled": True,
                "priority": 0,
            }
        else:
            old, new = fingerprint_of(link["trust_reference"]), fingerprint_of(trust)
            if old and new and old != new and not self.args.update_trust_reference:
                raise Refused(
                    f"the host key fingerprint changed for {self.name}: link has {old}, ssh now "
                    f"trusts {new}. Verify the machine, then rerun with --update-trust-reference"
                )
            link_cfg = dict(link)
            link_cfg.update(
                direction="direct_local_forward",
                ssh_target=self.env["RSI_SATELLITE_TARGET"],
                trust_reference=trust,
                enabled=True,
            )
        if link_cfg == link:
            self.out(f"  link {link_cfg['link_id']} already registered")
        else:
            self.hub_write(
                "PutSatelliteLink",
                {"expected_registry_revision": self.revision, "peer_id": peer_id, "link": link_cfg},
            )
        settled = (
            peer is not None
            and link is not None
            and peer["config"]["enabled"]
            and link_cfg == link
        )
        probe = None
        if settled or not self.dry:
            probe = self.hub.call(
                "ProbeSatelliteLink", {"peer_id": peer_id, "link_id": link_cfg["link_id"]}
            )
        if probe is None:
            self.out("  probe skipped: the peer or link does not exist yet (dry run)")
        else:
            probed = probe["identity"]["installation_id"]
            if probed != satellite_id or probe.get("matches_expected") is False:
                raise Refused(
                    f"installation mismatch: the hub's link probe saw {probed} but the tunnel "
                    f"identity read directly is {satellite_id}. Peer left unpaired and disabled for "
                    "read and dispatch; check the tunnel unit and the peer in the TUI"
                )
            self.out(f"  probe ok: link reaches installation {probed}")
        self.summary.update(peer_id=peer_id, link_id=link_cfg["link_id"])
        return peer_id, config

    def enable_peer(self, peer_id, config, satellite_id, project_id):
        self.step("hub: enable the verified peer, declare scope")
        want = dict(config)
        want.update(
            enabled=True,
            read_enabled=True,
            expected_installation_id=satellite_id,
            dispatch_enabled=bool(config.get("dispatch_enabled")) or self.args.dispatch,
        )
        if want == config:
            self.out("  peer already paired, read-enabled" + (", dispatch-enabled" if want["dispatch_enabled"] else ""))
        else:
            self.hub_write(
                "PutSatellitePeer",
                {"expected_registry_revision": self.revision, "peer": want, "repair_quarantine": False},
            )
        self.dispatch_on = want["dispatch_enabled"]
        simulated = self.revision
        view = self.registry()
        if self.dry:
            self.revision = simulated
        live = next((p for p in view["peers"] if p["config"]["peer_id"] == peer_id), None)
        sessions = list((live or {}).get("dispatch_scope", []))
        projects = list((live or {}).get("dispatch_project_scope", []))
        if project_id in projects:
            self.out(f"  project {project_id} already in the peer scope")
            return
        projects.append(project_id)
        self.hub_write(
            "PutSatellitePeerScope",
            {
                "expected_registry_revision": self.revision,
                "peer_id": peer_id,
                "remote_session_ids": sessions,
                "remote_project_ids": projects,
            },
        )

    def ensure_project(self):
        self.step("satellite: project")
        projects = self.sat.call("ListProjects") or []
        repo = self.args.repo.rstrip("/")
        name = self.args.project_name or Path(repo).name
        by_path = next((p for p in projects if (p.get("path") or "").rstrip("/") == repo), None)
        if by_path:
            self.out(f"  project {by_path['name']!r} {by_path['id']} already covers {repo}")
            return by_path["id"], by_path["name"]
        clash = next((p for p in projects if p.get("name") == name), None)
        if clash:
            raise Refused(
                f"a project named {name!r} already exists at {clash.get('path')!r}, not {repo}; "
                "pass --project-name or fix --repo"
            )
        created = self.sat.call("CreateProject", {"name": name, "path": repo}, write=True)
        return (valid_uuid(created["id"], "created project id") if created else DRY_PROJECT_ID), name

    def ensure_policy(self, hub_id, project_id):
        self.step("satellite: inbound policy")
        current = self.sat.call("GetSatelliteInboundPolicy") or {}
        policy = {
            key: list(current.get(key) or [])
            for key in ("allowed_hub_installations", "scope_roots", "scope_projects")
        }
        before = json.dumps(policy, sort_keys=True)
        for key, value in (("allowed_hub_installations", hub_id), ("scope_projects", project_id)):
            if value not in policy[key]:
                policy[key].append(value)
        if json.dumps(policy, sort_keys=True) == before:
            self.out("  hub and project already allowed")
            return
        self.sat.call("PutSatelliteInboundPolicy", {"policy": policy}, write=True)

    def preflight_manager(self):
        """Build and check the seat's policy and brief before anything is written."""
        args = self.args
        self.seat_choice = {
            "provider": canonical_provider(args.manager_provider),
            "model": args.manager_model or "",
            "effort": args.manager_effort,
        }
        self.policy = self.manager_policy(self.seat_choice)
        launches = self.policy.get("allowed_launches") or []
        if not launches or any(not item.get("model") for item in launches):
            raise Refused("the seat policy has no usable allowed launch: pass --manager-model or --allowed-launch")
        if not args.manager_session:
            self.render_brief("<project-id>", "<project>", "<hub>")

    def manager_policy(self, seat):
        args = self.args
        if args.manager_policy_file:
            try:
                policy = json.loads(Path(args.manager_policy_file).read_text())
            except (OSError, ValueError) as error:
                raise Refused(f"cannot read --manager-policy-file: {error}")
            if not isinstance(policy, dict):
                raise Refused("--manager-policy-file must hold a JSON object")
        else:
            policy = {
                "mode": "execute",
                "paused": False,
                "paused_epic_ids": [],
                "capabilities": list(EXECUTE_CAPABILITIES),
                "group_ids": [],
                "allow_create_groups": False,
                "max_created_containers": 0,
                "max_created_sessions": 32,
                "max_active_sessions": 4,
                "provider_limits": [],
                "allowed_launches": [],
                "max_recovery_attempts": 3,
                "retry_delay_seconds": 60,
                "request_timeout_seconds": 900,
                "max_spend_usd": None,
                "minimum_tier2_reviewer_tier": "standard",
                "daemon_setting_bounds": [],
            }
        if args.max_active_sessions is not None:
            policy["max_active_sessions"] = args.max_active_sessions
        if args.allowed_launch:
            policy["allowed_launches"] = [parse_launch(item) for item in args.allowed_launch]
        elif not policy.get("allowed_launches"):
            policy["allowed_launches"] = [seat]
        if "deploy" in policy.get("capabilities", []):
            self.out("  WARNING: the policy grants deploy; the hub restarts satellites, a seat should not")
        return policy

    def render_brief(self, project_id, project_name, hub_id):
        path = Path(self.args.manager_brief) if self.args.manager_brief else DEFAULT_BRIEF
        try:
            text = path.read_text()
        except OSError as error:
            raise Refused(f"cannot read the manager brief {path}: {error}")
        values = {
            "SATELLITE_NAME": self.name,
            "REPO": self.args.repo,
            "PROJECT_NAME": project_name,
            "PROJECT_ID": project_id,
            "HUB_INSTALLATION_ID": hub_id,
        }
        for key, value in values.items():
            text = text.replace("{{" + key + "}}", value)
        if not text.strip():
            raise Refused(f"the manager brief {path} is empty")
        return text

    def seat_state(self, project_id):
        """The satellite's own verdict on the project's seat (GetSatelliteScopeStatus).

        `inbound` lists each declared scope entry as {kind, id, state, seat_session_id?}
        with `state` one of live, unappointed, revoked, no_live_seat
        (crates/rsi-common/src/satellite_dispatch.rs). A project missing from the
        declared scope is reported as `not_in_scope`, never as live.
        """
        status = self.sat.call("GetSatelliteScopeStatus") or {}
        for entry in status.get("inbound") or []:
            if entry.get("kind") == "project" and entry.get("id") == project_id:
                return entry.get("state") or "unknown"
        return "not_in_scope"

    def dead_seat_message(self, seat, state, project_id):
        return (
            f"the appointed manager {seat} is not live on {self.name} (seat status: {state}); "
            "no manager is running there. Recover with either: "
            f"(1) reseat from the hub with AgentReseatSatelliteManager (peer {self.name}, "
            f"remote_session_id {project_id}), or "
            f"(2) rerun this command with --appoint-manager --manager-session <live-session-uuid> "
            "to appoint a session that is running on the satellite"
        )

    def seat_session_problem(self, seat):
        """Why a scope-live seat is not really running, or None when it is.

        The scope resolver calls every seat live unless the session is Archived or
        Deleted (resolve_satellite_scope_entry), so a Failed seat, or a Completed
        one with nothing armed to resume it, passes as live. Read the session's own
        status (GetSession) and, for a session that is not running, whether an
        enabled wake or pending continuation will bring it back (ListScheduledJobs).
        Interrupted is treated like Completed: live only while a wake is armed.
        """
        session = self.sat.call("GetSession", {"session_id": seat}) or {}
        status = session.get("status") or "unknown"
        if status in RUNNING_STATUSES:
            return None
        if status in RESUMABLE_STATUSES and self.seat_has_armed_wake(seat):
            return None
        return f"seat session {status}" + (" with no armed wake" if status in RESUMABLE_STATUSES else "")

    def seat_has_armed_wake(self, seat):
        """True when an enabled resume or when/on_terminal wake targets the seat.

        An agent_fresh wake launches a separate session and never resumes the seat,
        so it does not count.

        ListScheduledJobs (operator read) returns {jobs, next_cursor}; each job has
        `enabled`, `wake_session_id` and `wake_mode`: "resume" or
        {"on_terminal": <watched-session-uuid>} (rsi_common::types::ScheduledJob).
        """
        cursor = None
        for _ in range(SCHEDULED_JOB_PAGES):
            params = {"limit": SCHEDULED_JOB_PAGE_SIZE}
            if cursor:
                params["cursor"] = cursor
            page = self.sat.call("ListScheduledJobs", params) or {}
            if any(armed_wake_for(job, seat) for job in page.get("jobs") or []):
                return True
            cursor = page.get("next_cursor")
            if not cursor:
                return False
        return False

    def live_seat(self, existing, project_id):
        """The appointed seat when it is live; None when the seat must be (re)appointed.

        Raises Failed for an appointed seat that is not live and no replacement was
        given: reporting "already appointed" for an archived seat leaves no manager.
        """
        if not existing:
            return None
        revoked = not (
            existing.get("scope_mode") == "project"
            or existing.get("group_ids")
            or existing.get("epic_ids")
        )
        seat = existing.get("current_session_id")
        if revoked or not seat:
            return None
        state = self.seat_state(project_id)
        if state == "live":
            state = self.seat_session_problem(seat) or "live"
        if state == "live":
            self.out(f"  manager {seat} already appointed and live; not touching it")
            return seat
        if not self.args.manager_session:
            raise Failed(self.dead_seat_message(seat, state, project_id))
        self.out(f"  manager {seat} is appointed but not live ({state}); re-appointing")
        return None

    def ensure_manager(self, project_id, project_name, hub_id):
        self.step("satellite: manager seat")
        args = self.args
        existing = None
        if project_id != DRY_PROJECT_ID:
            existing = self.sat.call("GetHarnessManager", {"project_id": project_id})
        live_seat = self.live_seat(existing, project_id)
        if live_seat:
            return live_seat
        policy = self.policy
        stored = (
            self.sat.call("GetHarnessManagerPolicy", {"project_id": project_id})
            if project_id != DRY_PROJECT_ID
            else None
        )
        session_id = args.manager_session
        if session_id:
            valid_uuid(session_id, "--manager-session")
        else:
            launch = {
                "query": self.render_brief(project_id, project_name, hub_id),
                "title": f"{self.name} manager",
                "working_dir": args.repo,
                "provider": self.seat_choice["provider"],
                "model": self.seat_choice["model"],
                "project_id": project_id,
                "tags": ["satellite-manager"],
            }
            if self.seat_choice["effort"]:
                launch["effort"] = self.seat_choice["effort"]
            launched = self.sat.call("LaunchSession", launch, write=True)
            session_id = valid_uuid(launched["session_id"], "launched session id") if launched else "<new-session-id>"
        request = {
            "scope": {
                "project_id": project_id,
                "session_id": session_id,
                "expected_row_version": (existing or {}).get("row_version", 0),
            },
            "expected_policy_version": (stored or {}).get("row_version", 0),
            "idempotency_key": f"pair-direct:{self.name}:{project_id}:{session_id}",
            "policy": policy,
        }
        try:
            self.sat.call("ConfigureHarnessManagerWithPolicy", request, write=True)
        except Failed as error:
            raise Failed(
                f"{error}. Session {session_id} was launched but is not appointed: rerun with "
                f"--appoint-manager --manager-session {session_id}"
            )
        return session_id


def follow_up_command(args, argv0):
    parts = [argv0, args.name, "--repo", args.repo]
    if args.project_name:
        parts += ["--project-name", args.project_name]
    if args.appoint_manager:
        parts += ["--appoint-manager"]
        if args.manager_model:
            parts += ["--manager-provider", args.manager_provider, "--manager-model", args.manager_model]
        if args.manager_effort:
            parts += ["--manager-effort", args.manager_effort]
    parts.append("--dispatch")
    return " ".join(shlex.quote(part) for part in parts)


def pair(args, out=print):
    if os.environ.get("RSI_SESSION_TOKEN"):
        raise Refused(
            "RSI_SESSION_TOKEN is set: this is an operator tool (hard rule 4: agents never call "
            "operator-only RPCs). Run it from the operator's own shell"
        )
    env_path = args.env_file or str(Path.home() / ".config" / "rsi" / "satellites" / f"{args.name}-direct.env")
    env = load_env_file(env_path)
    pairing = Pairing(args, env, out)
    out(f"Pairing {args.name} via {pairing.local_socket}" + (" (dry run: nothing will change)" if args.dry_run else ""))
    if args.appoint_manager:
        pairing.preflight_manager()
    check_repo(args, env)
    satellite_id, hub_id = pairing.identities()
    peer_id, config = pairing.pair_hub(satellite_id)
    project_id, project_name = pairing.ensure_project()
    pairing.ensure_policy(hub_id, project_id)
    manager_id = None
    if args.appoint_manager:
        manager_id = pairing.ensure_manager(project_id, project_name, hub_id)
    pairing.enable_peer(peer_id, config, satellite_id, project_id)
    revision = pairing.revision if args.dry_run else pairing.registry()["revision"]
    out("\n== summary" + (" (dry run, nothing changed)" if args.dry_run else ""))
    out(f"  peer_id:            {peer_id}")
    out(f"  link_id:            {pairing.summary['link_id']}")
    out(f"  project_id:         {project_id}  ({project_name} at {args.repo})")
    out(f"  manager_session_id: {manager_id or 'none (--appoint-manager not given)'}")
    out(f"  registry_revision:  {revision}")
    out(f"  dispatch:           {'enabled' if pairing.dispatch_on else 'off'}")
    if not pairing.dispatch_on:
        out("\nDispatch is off. To enable hub-manager dispatch to this peer, rerun:")
        out("  " + follow_up_command(args, "scripts/satellite/pair-direct.py"))
    return 0


def main(argv=None):
    try:
        args = parse_args(sys.argv[1:] if argv is None else argv)
        return pair(args)
    except Refused as error:
        print(f"pair-direct: refused: {error}", file=sys.stderr)
        return EXIT_REFUSED
    except Failed as error:
        print(f"pair-direct: failed: {error}", file=sys.stderr)
        return EXIT_FAILED


if __name__ == "__main__":
    sys.exit(main())
