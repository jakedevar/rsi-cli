#!/usr/bin/env python3
"""Opt-in, single-owner unverified intake and bounded batch QA. No landing authority."""

import argparse
import contextlib
import fcntl
import hashlib
import json
import os
from pathlib import Path
import re
import selectors
import signal
import shutil
import subprocess
import sys
import tempfile
import time
import uuid

sys.dont_write_bytecode = True
REF = "refs/agent-dev-pilot/"
STATE = REF + "journal"
OID = re.compile(r"(?:[0-9a-f]{40}|[0-9a-f]{64})\Z")
KEY = re.compile(r"[A-Za-z0-9_-]{1,80}\Z")
SCRIPT = Path(__file__).resolve()
LIMITS = {"probe_seconds": 30, "qa_seconds": 1800, "runner_seconds": 1860,
          "probe_bytes": 64 * 1024, "qa_bytes": 16 * 1024 * 1024,
          "log_bytes": 17 * 1024 * 1024}
REAP_SECONDS = 5
CONTROL_SECONDS = 10
TERMINAL_BYTES = 1024 * 1024


class Refusal(Exception):
    pass


def limits(config):
    selected = config.get("limits", {})
    if not isinstance(selected, dict) or set(selected) - set(LIMITS):
        raise Refusal("unknown runtime/output limit")
    result = dict(LIMITS, **selected)
    for name, maximum in LIMITS.items():
        if type(result[name]) is not int or not 1 <= result[name] <= maximum:
            raise Refusal(f"{name} must be 1..{maximum}")
    return result


def resources(config):
    """Effective requested scope values, in bytes; never a live enforcement claim."""
    selected = config.get("memory", {})
    if not isinstance(selected, dict) or set(selected) - {"high_gib", "max_gib"}:
        raise Refusal("memory must contain only high_gib and max_gib")
    memory = {"high_gib": 6, "max_gib": 8}
    memory.update(selected)
    for name, maximum in (("high_gib", 8), ("max_gib", 10)):
        if type(memory[name]) is not int or not 1 <= memory[name] <= maximum:
            raise Refusal(f"memory {name} must be an integer in 1..{maximum} GiB")
    if memory["high_gib"] > memory["max_gib"]:
        raise Refusal("memory high_gib must be <= max_gib")
    return {"MemoryHigh": memory["high_gib"] * 1024 ** 3,
            "MemoryMax": memory["max_gib"] * 1024 ** 3,
            "MemorySwapMax": 0, "CPUWeight": 50}


def terminal_json(path):
    with Path(path).open("rb") as stream:
        data = stream.read(TERMINAL_BYTES + 1)
    if len(data) > TERMINAL_BYTES:
        raise Refusal("terminal evidence exceeds size limit")
    return json.loads(data)


def bounded_process(argv, *, cwd, env, seconds, byte_limit, sink=None):
    """Drain both pipes without unbounded buffers; kill groups and bound reaping.

    A scope check remains mandatory: a descendant can create a new process group.
    No caller may interpret a missing reap or a limit as success.
    """
    proc = subprocess.Popen(argv, cwd=cwd, env=env, stdin=subprocess.DEVNULL,
                            stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                            start_new_session=True)
    deadline = time.monotonic() + seconds
    output = {"stdout": bytearray(), "stderr": bytearray()}
    kept = 0
    reason = None
    reaped = False

    def kill_group():
        try:
            os.killpg(proc.pid, signal.SIGKILL)
        except ProcessLookupError:
            pass

    try:
        with selectors.DefaultSelector() as selector:
            for name in output:
                pipe = getattr(proc, name)
                os.set_blocking(pipe.fileno(), False)
                selector.register(pipe, selectors.EVENT_READ, name)
            while selector.get_map() or proc.poll() is None:
                remaining = deadline - time.monotonic()
                if remaining <= 0:
                    reason = "runtime_limit"
                    break
                for key, _ in selector.select(min(remaining, 0.05)):
                    block = os.read(key.fd, 65536)
                    if not block:
                        selector.unregister(key.fileobj)
                        continue
                    accepted = block[:max(0, byte_limit - kept)]
                    if sink is None:
                        output[key.data].extend(accepted)
                    else:
                        sink.write(accepted)
                        sink.flush()
                    kept += len(accepted)
                    if len(accepted) != len(block):
                        reason = "output_limit"
                        break
                if reason:
                    break
            # An exited leader can leave children even after both pipes closed.
            if reason is None:
                try:
                    os.killpg(proc.pid, 0)
                    reason = "remaining_process_group"
                except ProcessLookupError:
                    pass
    finally:
        # Also runs on interruption/I/O errors; never wait indefinitely on pipes
        # inherited by grandchildren, or on an uninterruptible process.
        kill_group()
        for pipe in (proc.stdout, proc.stderr):
            pipe.close()
        try:
            proc.wait(timeout=REAP_SECONDS)
            reaped = True
        except subprocess.TimeoutExpired:
            pass
    return {"exit": proc.returncode, "limit": reason, "reaped": reaped,
            "kept_bytes": kept, **{name: data.decode(errors="replace") for name, data in output.items()}}


def scope_settlement(control, unit, *, cwd, env):
    """Confirm the exact scope is inactive. Bounded stop failure retains custody."""
    calls = []

    def invoke(*args):
        argv = [control, "--user", *args, unit]
        result = bounded_process(argv, cwd=cwd, env=env, seconds=CONTROL_SECONDS, byte_limit=4096)
        calls.append({"argv": argv, "result": result})
        return result

    def inactive(result):
        fields = dict(line.split("=", 1) for line in result["stdout"].splitlines() if "=" in line)
        return (result["exit"] == 0 and result["reaped"] and result["limit"] is None and
                fields.get("ActiveState") == "inactive" and
                fields.get("LoadState") in ("loaded", "not-found"))

    before = invoke("show", "--property=ActiveState", "--property=LoadState")
    if inactive(before):
        return {"confirmed_inactive": True, "needed_stop": False, "calls": calls}
    invoke("stop")
    after = invoke("show", "--property=ActiveState", "--property=LoadState")
    return {"confirmed_inactive": inactive(after), "needed_stop": True, "calls": calls}



def encoded(value):
    return (json.dumps(value, sort_keys=True, separators=(",", ":")) + "\n").encode()


def digest(data):
    return hashlib.sha256(data).hexdigest()


def file_digest(path):
    with open(path, "rb") as stream:
        return hashlib.file_digest(stream, "sha256").hexdigest()


def git(repo, *args, data=None, check=True):
    env = {k: v for k, v in os.environ.items() if not k.startswith(("GIT_", "RSI_"))}
    env.update(GIT_CONFIG_NOSYSTEM="1", GIT_CONFIG_GLOBAL=os.devnull,
               GIT_AUTHOR_NAME="agent-dev pilot", GIT_AUTHOR_EMAIL="pilot@invalid",
               GIT_COMMITTER_NAME="agent-dev pilot", GIT_COMMITTER_EMAIL="pilot@invalid")
    result = subprocess.run(
        ["git", "-C", str(repo), "--no-optional-locks", "-c", "core.hooksPath=/dev/null",
         "-c", "commit.gpgSign=false", *args], input=data, capture_output=True, env=env,
        check=False,
    )
    if check and result.returncode:
        raise Refusal(f"git {args[0]}: {result.stderr.decode(errors='replace').strip()}")
    return result


def out(repo, *args):
    return git(repo, *args).stdout.decode().strip()


def exact_commit(repo, sha):
    if not isinstance(sha, str) or not OID.fullmatch(sha):
        raise Refusal("expected full lowercase commit SHA")
    if out(repo, "rev-parse", f"{sha}^{{commit}}") != sha:
        raise Refusal("expected exact commit")


def clean(repo, sha):
    if out(repo, "rev-parse", "HEAD") != sha:
        raise Refusal(f"stale HEAD at {repo}; expected {sha}")
    # Ignored files may affect builds too. Cache is deliberately outside the tree.
    entries = git(repo, "ls-files", "--stage", "-z").stdout.split(b"\0")
    for entry in filter(None, entries):
        mode, rest = entry.split(b" ", 1)
        if mode == b"160000":
            raise Refusal("submodules require separate custody and are unsupported by this pilot")
        if mode == b"120000":
            path = Path(repo) / os.fsdecode(rest.split(b"\t", 1)[1])
            if not path.resolve().is_relative_to(Path(repo).resolve()):
                raise Refusal("external symlink build inputs are unsupported")
    if git(repo, "status", "--porcelain=v1", "--untracked-files=all", "--ignored").stdout:
        raise Refusal(f"dirty input at {repo}; preserve it for its owner")
    if git(repo, "ls-files", "-v").stdout.splitlines() != \
            git(repo, "ls-files", "-t").stdout.splitlines():
        raise Refusal("assume-unchanged index entries are unsupported")
    if any(line.startswith(b"S ") for line in git(repo, "ls-files", "-t").stdout.splitlines()):
        raise Refusal("sparse/skip-worktree inputs are unsupported")


def atomic_json(path, value):
    path = Path(path)
    fd, temporary = tempfile.mkstemp(prefix=".pending-", dir=path.parent)
    try:
        with os.fdopen(fd, "wb") as stream:
            stream.write(encoded(value))
            stream.flush()
            os.fsync(stream.fileno())
        os.replace(temporary, path)
        directory = os.open(path.parent, os.O_RDONLY | os.O_DIRECTORY)
        try:
            os.fsync(directory)
        finally:
            os.close(directory)
    finally:
        if os.path.exists(temporary):
            os.unlink(temporary)


class Queue:
    """Call only while locked. Git journal is authoritative, files are evidence."""

    def __init__(self, repo):
        self.repo = Path(repo).resolve()
        self.common = Path(out(repo, "rev-parse", "--path-format=absolute", "--git-common-dir"))
        self.directory = self.common / "agent-dev-pilot"
        self.directory.mkdir(mode=0o700, exist_ok=True)
        self.sha = None
        self.state = None

    @contextlib.contextmanager
    def locked(self):
        with (self.directory / "lock").open("a+b") as lock:
            try:
                fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
            except BlockingIOError as error:
                raise Refusal("queue/QA owner is busy") from error
            result = git(self.repo, "rev-parse", "--verify", STATE, check=False)
            self.sha = result.stdout.decode().strip() if result.returncode == 0 else None
            self.state = json.loads(out(self.repo, "show", f"{self.sha}:state.json")) if self.sha else None
            try:
                yield self
            finally:
                fcntl.flock(lock, fcntl.LOCK_UN)

    def save(self, refs=(), verify=()):
        """One ref transaction: state, retention and base fences cannot diverge."""
        blob = git(self.repo, "hash-object", "-w", "--stdin", data=encoded(self.state)).stdout.decode().strip()
        tree = git(self.repo, "mktree", data=f"100644 blob {blob}\tstate.json\n".encode()).stdout.decode().strip()
        parents = ["-p", self.sha] if self.sha else []
        new = git(self.repo, "commit-tree", tree, *parents, data=b"agent-dev pilot journal\n").stdout.decode().strip()
        zero = "0" * len(new)
        lines = ["start", f"update {STATE} {new} {self.sha or zero}"]
        lines += [f"create {ref} {value}" for ref, value in refs]
        lines += [f"verify {ref} {value}" for ref, value in verify]
        lines += ["prepare", "commit", ""]
        git(self.repo, "update-ref", "--stdin", data="\n".join(lines).encode())
        self.sha = new

    def configured(self):
        if self.state is None:
            raise Refusal("pilot is not initialized; explicit init is required")
        return self.state["config"]

    def owner(self, owner):
        if owner != self.configured()["qa_owner"]:
            raise Refusal("QA owner does not match explicit custody record")

    def init(self, config):
        required = {"base_ref", "base", "qa_owner", "workspace", "max_wip", "batch_size",
                    "oldest_seconds", "max_batches", "environment", "command", "toolchain_command",
                    "external_inputs"}
        if set(config) - {"limits", "memory"} != required:
            raise Refusal(f"config fields must be {sorted(required)} plus optional limits and memory")
        limits(config)
        resources(config)
        exact_commit(self.repo, config["base"])
        if not config["base_ref"].startswith("refs/heads/"):
            raise Refusal("base_ref must be an explicit local branch ref")
        if out(self.repo, "rev-parse", config["base_ref"]) != config["base"]:
            raise Refusal("stale base")
        for key, maximum in (("max_wip", 64), ("batch_size", 16), ("oldest_seconds", 86400),
                             ("max_batches", 100)):
            if type(config[key]) is not int or not 1 <= config[key] <= maximum:
                raise Refusal(f"{key} must be 1..{maximum}")
        if config["batch_size"] > config["max_wip"] or not config["qa_owner"]:
            raise Refusal("invalid batch bound or owner")
        for key in ("command", "toolchain_command"):
            if not isinstance(config[key], list) or not config[key] or \
                    any(not isinstance(arg, str) or not arg or "\0" in arg for arg in config[key]):
                raise Refusal(f"{key} must be nonempty argv")
        env = config["environment"]
        if not isinstance(env, dict) or not env.get("PATH") or not env.get("HOME") or any(
                not isinstance(k, str) or not re.fullmatch(r"[A-Za-z_][A-Za-z0-9_]*", k)
                or not isinstance(v, str) or "\0" in v or k.startswith(("GIT_", "RSI_"))
                for k, v in env.items()):
            raise Refusal("explicit environment with PATH and HOME required; no GIT_/RSI_ credentials")
        for key in ("HOME", "CARGO_HOME", "RUSTUP_HOME"):
            if key in env and not Path(env[key]).is_absolute():
                raise Refusal(f"{key} must be absolute")
        if any(not Path(part).is_absolute() for part in env["PATH"].split(os.pathsep)):
            raise Refusal("PATH must have only absolute entries")
        workspace = Path(config["workspace"])
        if not workspace.is_absolute() or workspace.resolve() != workspace:
            raise Refusal("workspace must be a canonical absolute path")
        if workspace.is_relative_to(self.repo) or workspace.is_relative_to(self.common):
            raise Refusal("QA workspace must be outside source and Git directories")
        for path in config["external_inputs"]:
            if not Path(path).is_absolute() or not Path(path).is_file():
                raise Refusal("external_inputs must name existing absolute files")
        if self.state:
            if self.state["config"] != config:
                raise Refusal("pilot already initialized with different immutable config")
            return self.state
        if workspace.exists():
            raise Refusal("QA workspace must be a new pilot-owned path")
        self.state = {"version": 1, "id": str(uuid.uuid4()), "config": config,
                      "members": [], "batches": [], "active": None, "accepted_base": config["base"]}
        self.save(refs=[(REF + "base", config["base"])],
                  verify=[(config["base_ref"], config["base"])])
        return self.state

    def intake(self, request):
        config = self.configured()
        required = {"key", "source", "source_path", "owner", "custody", "checks", "tests"}
        if set(request) != required or not KEY.fullmatch(request["key"]):
            raise Refusal("invalid intake fields/key")
        for field in ("owner", "custody", "checks", "tests"):
            if not isinstance(request[field], str) or not request[field].strip():
                raise Refusal(f"nonempty {field} declaration required")
        for member in self.state["members"]:
            if member["request"]["key"] == request["key"]:
                if member["request"] != request:
                    raise Refusal("intake key reused with different inputs")
                self.retained(member)
                return member
            if member["request"]["source"] == request["source"]:
                raise Refusal("source already retained under another intake key")
        if len([m for m in self.state["members"] if m["status"] in ("unverified", "batched")]) >= config["max_wip"]:
            raise Refusal("unverified WIP limit reached")
        if len(self.state["batches"]) >= config["max_batches"]:
            raise Refusal("pilot batch budget exhausted")
        exact_commit(self.repo, request["source"])
        source = Path(request["source_path"])
        if not source.is_absolute() or source.resolve() != source:
            raise Refusal("source_path must be canonical absolute path")
        if Path(out(source, "rev-parse", "--path-format=absolute", "--git-common-dir")) != self.common:
            raise Refusal("source must belong to this repository")
        clean(source, request["source"])
        if git(self.repo, "merge-base", "--is-ancestor", config["base"], request["source"], check=False).returncode:
            raise Refusal("source is not descended from pilot base")
        member = {"request": request, "tree": out(self.repo, "rev-parse", request["source"] + "^{tree}"),
                  "ref": REF + "sources/" + request["key"], "received": time.time(),
                  "status": "unverified"}
        self.state["members"].append(member)
        self.save(refs=[(member["ref"], request["source"])])
        return member

    def retained(self, member):
        sha = member["request"]["source"]
        if out(self.repo, "rev-parse", member["ref"]) != sha or \
                out(self.repo, "rev-parse", sha + "^{tree}") != member["tree"]:
            raise Refusal("source retention ref/tree changed")

    def ready(self):
        config = self.configured()
        pending = [m for m in self.state["members"] if m["status"] == "unverified"]
        age = max(0, time.time() - pending[0]["received"]) if pending else 0
        return {"ready": bool(pending) and (len(pending) >= config["batch_size"] or
                                          age >= config["oldest_seconds"]),
                "count": len(pending), "oldest_age_seconds": age}

    def freeze(self, owner):
        self.owner(owner)
        if self.state["active"] is not None:
            return self.batch()
        config = self.configured()
        if len(self.state["batches"]) >= config["max_batches"]:
            raise Refusal("pilot batch budget exhausted")
        if not self.ready()["ready"]:
            raise Refusal("batch is below count and oldest-age readiness bounds")
        members = [m for m in self.state["members"] if m["status"] == "unverified"][:config["batch_size"]]
        base = self.state["accepted_base"]
        if out(self.repo, "rev-parse", config["base_ref"]) != base:
            raise Refusal("stale accepted base; re-admission requires a new reviewed pilot decision")
        candidate = base
        batch = {"id": str(uuid.uuid4()), "base": base, "members": [m["request"]["key"] for m in members],
                 "status": "frozen", "qa_owner": owner}
        for member in members:
            self.retained(member)
            sha = member["request"]["source"]
            merged = git(self.repo, "merge-tree", "--write-tree", candidate, sha, check=False)
            if merged.returncode:
                batch.update(status="conflict", conflict=merged.stdout.decode(errors="replace"),
                             error=merged.stderr.decode(errors="replace"))
                break
            tree = merged.stdout.decode().splitlines()[0]
            candidate = git(self.repo, "commit-tree", tree, "-p", candidate, "-p", sha,
                            data=f"Unverified agent-dev batch {batch['id']}\n".encode()).stdout.decode().strip()
        batch.update(candidate=candidate, tree=out(self.repo, "rev-parse", candidate + "^{tree}"))
        batch["ref"] = REF + "batches/" + batch["id"]
        for member in members:
            member["status"] = "batched"
        self.state["batches"].append(batch)
        self.state["active"] = batch["id"]
        self.save(refs=[(batch["ref"], candidate), (batch["ref"] + "-base", base)],
                  verify=[(config["base_ref"], base)] + [(m["ref"], m["request"]["source"]) for m in members])
        return batch

    def batch(self):
        self.configured()
        for batch in self.state["batches"]:
            if batch["id"] == self.state["active"]:
                return batch
        raise Refusal("no active frozen batch")

    def members(self, batch):
        return [m for m in self.state["members"] if m["request"]["key"] in batch["members"]]

    def fences(self, batch, current_base=True):
        config = self.configured()
        refs = [(batch["ref"], batch["candidate"]), (batch["ref"] + "-base", batch["base"])]
        refs += [(m["ref"], m["request"]["source"]) for m in self.members(batch)]
        if current_base:
            refs.append((config["base_ref"], batch["base"]))
        for ref, sha in refs:
            if out(self.repo, "rev-parse", ref) != sha:
                raise Refusal(f"stale input ref: {ref}")
        return refs

    def workspace(self, batch):
        workspace = Path(self.configured()["workspace"])
        marker = self.directory / "workspace.json"
        claim = {"queue": self.state["id"], "workspace": str(workspace)}
        if not marker.exists():
            if workspace.exists():
                raise Refusal("unclaimed workspace exists; preserve for custody owner")
            atomic_json(marker, claim)  # Intent first: crash after worktree add is recoverable.
        if json.loads(marker.read_text()) != claim:
            raise Refusal("workspace custody marker mismatch")
        if not workspace.exists():
            git(self.repo, "worktree", "add", "--detach", str(workspace), batch["candidate"])
        self.workspace_custody()
        old = out(workspace, "rev-parse", "HEAD")
        clean(workspace, old)
        known = [b["candidate"] for b in self.state["batches"]]
        if old not in known:
            raise Refusal("workspace HEAD is outside recorded custody")
        if old != batch["candidate"]:
            git(workspace, "checkout", "--detach", batch["candidate"])
        clean(workspace, batch["candidate"])
        return workspace

    def workspace_custody(self):
        workspace = Path(self.configured()["workspace"])
        marker = self.directory / "workspace.json"
        if (workspace.resolve() != workspace or json.loads(marker.read_text()) !=
                {"queue": self.state["id"], "workspace": str(workspace)}):
            raise Refusal("workspace custody changed")
        if (Path(out(workspace, "rev-parse", "--path-format=absolute", "--git-common-dir")) != self.common
                or Path(out(workspace, "rev-parse", "--show-toplevel")) != workspace):
            raise Refusal("workspace repository/path mismatch")
        if git(workspace, "symbolic-ref", "-q", "HEAD", check=False).returncode == 0:
            raise Refusal("QA workspace must remain detached")

    def inputs(self, batch):
        config = self.configured()
        environment = dict(config["environment"])
        environment.update(CARGO_BUILD_JOBS="1", CARGO_PROFILE_DEV_DEBUG="line-tables-only",
                           CARGO_TARGET_DIR=str(self.directory / "cache"))
        executables = {}
        for name in ("systemd-run", "systemctl", config["command"][0], config["toolchain_command"][0], sys.executable):
            path = shutil.which(name, path=environment["PATH"])
            if not path:
                raise Refusal(f"required executable unavailable: {name}; no unbounded fallback")
            executables[name] = {"path": str(Path(path).absolute()), "sha256": file_digest(path)}
        external = {path: file_digest(path) for path in config["external_inputs"]}
        # Cargo discovers configuration through the working directory's ancestors,
        # CARGO_HOME and HOME; record absence too, so later additions invalidate QA.
        workspace = Path(config["workspace"])
        discovered = set()
        for directory in (workspace, *workspace.parents):
            discovered.update(directory / ".cargo" / name for name in ("config", "config.toml"))
        home = environment.get("HOME")
        cargo_home = environment.get("CARGO_HOME") or (str(Path(home) / ".cargo") if home else None)
        if cargo_home:
            discovered.update(Path(cargo_home) / name for name in ("config", "config.toml"))
        rustup_home = environment.get("RUSTUP_HOME") or (str(Path(home) / ".rustup") if home else None)
        if rustup_home:
            discovered.add(Path(rustup_home) / "settings.toml")
        discovered_inputs = {str(path): file_digest(path) if path.exists() else None
                             for path in sorted(discovered)}
        return {"candidate": batch["candidate"], "tree": batch["tree"], "base": batch["base"],
                "members": [{"request": m["request"], "tree": m["tree"], "ref": m["ref"]}
                            for m in self.members(batch)], "config": config, "environment": environment,
                "executables": executables, "external_inputs": external,
                "discovered_config_inputs": discovered_inputs, "limits": limits(config),
                "resources": resources(config),
                "driver_sha256": file_digest(SCRIPT),
                "git_config_sha256": digest(git(self.repo, "config", "--local", "--null", "--list").stdout),
                "tracked_inputs": out(self.repo, "ls-tree", "-r", batch["candidate"])}

    def run(self, owner):
        self.owner(owner)
        batch = self.batch()
        if batch["status"] in ("passed", "failed"):
            return self.verify_receipt(batch)
        if batch["status"] != "frozen":
            raise Refusal(f"batch {batch['status']}; no automatic retry; repair owners: {self.repairs(batch)}")
        self.fences(batch)
        workspace = self.workspace(batch)
        inputs = self.inputs(batch)
        run_dir = self.directory / batch["id"]
        run_dir.mkdir(exist_ok=True)
        if any((run_dir / name).exists() for name in ("terminal.json", "receipt.json", "qa.log")):
            raise Refusal("unexpected prior runner artifacts; preserve for custody owner")
        job = {"command": inputs["config"]["command"], "toolchain_command": inputs["config"]["toolchain_command"],
               "workspace": str(workspace), "environment": inputs["environment"],
               "result": str(run_dir / "terminal.json"), "limits": inputs["limits"],
               "resources": inputs["resources"]}
        runner = inputs["executables"]["systemd-run"]["path"]
        unit = f"rsi-agent-dev-{batch['id']}.scope"
        control = inputs["executables"]["systemctl"]["path"]
        argv = [runner, "--user", "--scope", "--quiet", f"--unit=rsi-agent-dev-{batch['id']}",
                *[f"--property={name}={value}" for name, value in inputs["resources"].items()],
                f"--property=RuntimeMaxSec={inputs['limits']['runner_seconds']}",
                "--property=KillMode=control-group", "--property=KillSignal=SIGKILL",
                "--property=SendSIGKILL=yes", "--same-dir", "--", sys.executable, str(SCRIPT),
                "_worker", str(run_dir / "job.json")]
        job["runner_argv"] = argv
        atomic_json(run_dir / "job.json", job)
        batch.update(status="running", inputs=inputs, runner_argv=argv, job=job)
        self.save(verify=self.fences(batch))  # Persist before launch: retry can never double-run.
        log = run_dir / "qa.log"
        try:
            with log.open("xb") as stream:
                try:
                    result = bounded_process(argv, cwd=workspace, env=inputs["environment"],
                                             seconds=inputs["limits"]["runner_seconds"],
                                             byte_limit=inputs["limits"]["log_bytes"], sink=stream)
                finally:
                    scope = scope_settlement(control, unit, cwd=workspace, env=inputs["environment"])
                    stream.flush()
                    os.fsync(stream.fileno())
            terminal_path = run_dir / "terminal.json"
            terminal = terminal_json(terminal_path) if terminal_path.exists() else None
            valid = (terminal is not None and terminal["exit"] == result["exit"] and
                     terminal["job_sha256"] == digest(encoded(job)) and
                     terminal.get("resources") == inputs["resources"] and
                     terminal.get("runner_argv") == argv)
            clean(workspace, batch["candidate"])
            self.fences(batch)
            if self.inputs(batch) != inputs:
                raise Refusal("QA inputs changed during run")
            quiescent = result["reaped"] and scope["confirmed_inactive"]
            passed = (valid and quiescent and not scope["needed_stop"] and result["limit"] is None
                      and result["exit"] == 0 and terminal["toolchain_exit"] == 0
                      and terminal.get("limit") is None)
            receipt = {"batch": batch["id"], "inputs": inputs, "runner_argv": argv,
                       "runner_exit": result["exit"], "runner_execution": result,
                       "scope_settlement": scope, "terminal": terminal,
                       "log_sha256": file_digest(log), "passed": bool(passed),
                       "repair_owners": [] if passed else self.repairs(batch)}
            batch.update(status=("passed" if passed else "failed") if quiescent else "running", receipt=receipt)
            self.save(verify=self.fences(batch))
            atomic_json(run_dir / "receipt.json", receipt)
            if not quiescent:
                raise Refusal("scope/runner termination unconfirmed; custody stays running")
            return receipt
        except (OSError, ValueError, KeyError, Refusal) as error:
            # 'running' is the durable at-most-once fence even if journal settlement fails.
            raise Refusal(f"QA outcome uncertain/invalid; preserve workspace/log; {error}; "
                          f"repair owners: {self.repairs(batch)}") from error

    def recover(self, owner):
        """Recreate only a missing receipt mirror from committed terminal evidence."""
        self.owner(owner)
        batch = self.batch()
        if batch["status"] not in ("passed", "failed"):
            raise Refusal("no committed terminal evidence; uncertain runs require external custody resolution")
        path = self.directory / batch["id"] / "receipt.json"
        self.verify_receipt(batch, missing_mirror=True)
        if not path.exists():
            atomic_json(path, batch["receipt"])
        return self.verify_receipt(batch)

    def verify_receipt(self, batch, current_base=True, missing_mirror=False):
        if batch["status"] not in ("passed", "failed"):
            raise Refusal(f"no terminal receipt: {batch['status']}; repair owners: {self.repairs(batch)}")
        self.fences(batch, current_base=current_base)
        self.workspace_custody()
        clean(Path(self.configured()["workspace"]), batch["candidate"])
        receipt = batch["receipt"]
        run_dir = self.directory / batch["id"]
        mirror = run_dir / "receipt.json"
        if ((json.loads(mirror.read_text()) != receipt if mirror.exists() else not missing_mirror) or
                receipt["inputs"] != self.inputs(batch) or
                file_digest(run_dir / "qa.log") != receipt["log_sha256"] or
                json.loads((run_dir / "job.json").read_text()) != batch["job"] or
                (terminal_json(run_dir / "terminal.json")
                 if (run_dir / "terminal.json").exists() else None) != receipt["terminal"]):
            raise Refusal("tampered/stale receipt or modified inputs")
        return receipt

    def repairs(self, batch):
        return [{"source": m["request"]["source"], "owner": m["request"]["owner"],
                 "custody": m["request"]["custody"]} for m in self.members(batch)]

    def eligible(self):
        batch = self.batch()
        receipt = self.verify_receipt(batch)
        return {"eligible_for_reviewed_admission": receipt["passed"], "rolling_mutated": False,
                "batch": batch["id"], "candidate": batch["candidate"], "base": batch["base"],
                "repair_owners": [] if receipt["passed"] else self.repairs(batch)}

    def landed(self, owner):
        self.owner(owner)
        batch = self.batch()
        receipt = self.verify_receipt(batch, current_base=False)
        target = out(self.repo, "rev-parse", self.configured()["base_ref"])
        if not receipt["passed"] or git(self.repo, "merge-base", "--is-ancestor",
                                        batch["candidate"], target, check=False).returncode:
            raise Refusal("exact successful candidate has not landed through external gates")
        for member in self.members(batch):
            member["status"] = "landed"
        batch.update(status="landed", landed_target=target)
        self.state["accepted_base"] = target
        self.state["active"] = None
        self.save(verify=self.fences(batch, current_base=False) + [(self.configured()["base_ref"], target)])
        return {"observed_landed": batch["candidate"], "target": target}

    def retire(self, owner, reason):
        self.owner(owner)
        batch = self.batch()
        if batch["status"] not in ("conflict", "failed") or not reason.strip():
            raise Refusal("retire requires a conflict/terminal failure and explicit repair reason")
        if batch["status"] == "failed":
            self.verify_receipt(batch, current_base=False)
        for member in self.members(batch):
            member["status"] = "retired"
        batch.update(status="retired", repair_reason=reason)
        self.state["active"] = None
        self.save(verify=self.fences(batch, current_base=False))
        return {"retired": batch["id"], "repair_owners": self.repairs(batch),
                "retention_preserved": True, "cleanup_authorized": False}

    def cleanup(self):
        evidence = []
        for member in self.state["members"]:
            self.retained(member)
            request = member["request"]
            try:
                clean(Path(request["source_path"]), request["source"])
                source_clean = True
            except (Refusal, OSError):
                source_clean = False
            evidence.append({"source": request["source"], "owner": request["owner"],
                             "custody": request["custody"], "source_path": request["source_path"],
                             "retention_ref": member["ref"], "source_clean": source_clean,
                             "landed": member["status"] == "landed",
                             "candidate_for_daemon_custody_check": source_clean and member["status"] == "landed"})
        return {"cleanup_authorized": False, "members": evidence,
                "required_daemon_checks": ["terminal", "no active custody", "clean exact source", "no seal",
                                           "no review", "no pin", "no wake", "retention and custody identity"],
                "preserve_qa_workspace_and_cache": True}


def worker(path):
    """Executed only inside the explicit runner; never used as an automatic fallback."""
    job = json.loads(Path(path).read_text())
    if Path(job["result"]).exists():
        raise Refusal("terminal result already exists")
    selected = limits({"limits": job["limits"]})
    probe = bounded_process(job["toolchain_command"], cwd=job["workspace"], env=job["environment"],
                            seconds=selected["probe_seconds"], byte_limit=selected["probe_bytes"])
    qa = None
    limited = probe["limit"]
    code = probe["exit"]
    if not probe["reaped"]:
        limited = limited or "probe_reap_unconfirmed"
    if code == 0 and limited is None:
        qa = bounded_process(job["command"], cwd=job["workspace"], env=job["environment"],
                             seconds=selected["qa_seconds"], byte_limit=selected["qa_bytes"],
                             sink=sys.stdout.buffer)
        code, limited = qa["exit"], qa["limit"]
        if not qa["reaped"]:
            limited = limited or "qa_reap_unconfirmed"
    # Limit failure is explicit even when a process races exit(0) at its deadline.
    code = 124 if limited else (code if code >= 0 else 128 - code)
    atomic_json(Path(job["result"]), {"job_sha256": digest(encoded(job)),
                                      "toolchain": {"stdout": probe["stdout"], "stderr": probe["stderr"]},
                                      "resources": job["resources"], "runner_argv": job["runner_argv"],
                                      "probe_execution": probe, "qa_execution": qa,
                                      "toolchain_exit": probe["exit"], "exit": code, "limit": limited})
    return code


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--repo", type=Path, default=Path.cwd())
    commands = parser.add_subparsers(dest="action", required=True)
    for name in ("init", "intake"):
        commands.add_parser(name).add_argument("json", type=Path)
    for name in ("freeze", "run", "landed", "recover"):
        commands.add_parser(name).add_argument("--owner", required=True)
    retire = commands.add_parser("retire")
    retire.add_argument("--owner", required=True)
    retire.add_argument("--reason", required=True)
    for name in ("status", "eligible", "cleanup-evidence"):
        commands.add_parser(name)
    args = parser.parse_args()
    try:
        with Queue(args.repo).locked() as queue:
            if args.action in ("init", "intake"):
                result = getattr(queue, args.action)(json.loads(args.json.read_text()))
            elif args.action in ("freeze", "run", "landed", "recover"):
                result = getattr(queue, args.action)(args.owner)
            elif args.action == "retire":
                result = queue.retire(args.owner, args.reason)
            elif args.action == "eligible":
                result = queue.eligible()
            elif args.action == "cleanup-evidence":
                queue.configured()
                result = queue.cleanup()
            else:
                queue.configured()
                result = {"state": queue.state, "readiness": queue.ready()}
            print(json.dumps(result, indent=2))
        return 1 if (result.get("passed") is False or
                     result.get("eligible_for_reviewed_admission") is False or
                     result.get("status") == "conflict") else 0
    except (Refusal, OSError, ValueError, KeyError, TypeError) as error:
        print(f"agent-dev queue: {error}", file=sys.stderr)
        return 2


if __name__ == "__main__":
    if len(sys.argv) == 3 and sys.argv[1] == "_worker":
        sys.exit(worker(sys.argv[2]))
    sys.exit(main())
