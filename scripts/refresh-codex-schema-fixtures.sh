#!/usr/bin/env bash
# Regenerate the CodexAppServer wire-conformance fixtures from the installed codex CLI (#2754).
#
#   scripts/refresh-codex-schema-fixtures.sh <version>     e.g. 0.163.0
#
# Runs `codex app-server generate-json-schema` twice (stable, and --experimental) and writes, under
# crates/rsid/src/codex_app_server/fixtures/:
#   schema-<version>/{stable,experimental}/  the request schemas RSI builds (initialize, initialized,
#                                            thread/start, thread/resume, turn/start) plus the method
#                                            inventory, each with a provenance.json
#   approval-<version>/                      the server-request approval fixtures (stable schema)
# The script refuses when the installed CLI is not <version>, so a fixture directory always names the
# CLI that generated it. After a refresh, run the codex_app_server tests; a wire drift fails there.
set -euo pipefail

version="${1:-}"
if [[ -z "$version" || "$version" == -* ]]; then
    echo "usage: $0 <codex-cli-version>   (the installed version, e.g. 0.163.0)" >&2
    exit 2
fi
codex_bin="${CODEX_BIN:-codex}"
installed="$("$codex_bin" --version 2>/dev/null | awk '{print $NF}')" || installed=""
if [[ "$installed" != "$version" ]]; then
    echo "installed codex-cli is '${installed:-unavailable}', not '$version'; install $version or pass the installed version" >&2
    exit 1
fi

repo="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
dest="$repo/crates/rsid/src/codex_app_server/fixtures"
work="$(mktemp -d "${TMPDIR:-/tmp}/codex-schema-XXXXXX")"
trap 'rm -rf "$work"' EXIT

"$codex_bin" app-server generate-json-schema --out "$work/stable" >/dev/null
"$codex_bin" app-server generate-json-schema --experimental --out "$work/experimental" >/dev/null

python3 -I - "$version" "$work" "$dest" <<'PY'
import hashlib, json, os, shutil, sys

version, work, dest = sys.argv[1:4]

def sha(path):
    with open(path, "rb") as f:
        return hashlib.sha256(f.read()).hexdigest()

def dump(path, value):
    os.makedirs(os.path.dirname(path), exist_ok=True)
    with open(path, "w") as f:
        json.dump(value, f, indent=2)
        f.write("\n")

def methods(client_request):
    return sorted(o["properties"]["method"]["enum"][0] for o in client_request["oneOf"])

COPIED = ["v1/InitializeParams.json", "ClientNotification.json",
          "v2/ThreadStartParams.json", "v2/ThreadResumeParams.json", "v2/TurnStartParams.json"]

for variant, flag in (("stable", ""), ("experimental", " --experimental")):
    src = os.path.join(work, variant)
    out = os.path.join(dest, f"schema-{version}", variant)
    shutil.rmtree(out, ignore_errors=True)
    os.makedirs(out)
    digests = {}
    for rel in COPIED:
        shutil.copyfile(os.path.join(src, rel), os.path.join(out, os.path.basename(rel)))
        digests[rel] = sha(os.path.join(src, rel))
    with open(os.path.join(src, "ClientRequest.json")) as f:
        inventory = methods(json.load(f))
    dump(os.path.join(out, "ClientRequest-methods.json"), inventory)
    digests["ClientRequest.json"] = sha(os.path.join(src, "ClientRequest.json"))
    dump(os.path.join(out, "provenance.json"), {
        "provider": f"codex-cli {version}",
        "generation_command": f"codex app-server generate-json-schema{flag} --out <dir>",
        "variant": variant,
        "source_sha256": digests,
        "excerpts": {"ClientRequest-methods.json": "Every ClientRequest method name, sorted. The full ClientRequest.json is not stored."},
    })

# Approval fixtures: stable schema, same projections as approval-0.153.4.
src = os.path.join(work, "stable")
out = os.path.join(dest, f"approval-{version}")
shutil.rmtree(out, ignore_errors=True)
os.makedirs(out)
copied = ["CommandExecutionRequestApprovalResponse.json", "FileChangeRequestApprovalResponse.json", "RequestId.json"]
for name in copied:
    shutil.copyfile(os.path.join(src, name), os.path.join(out, name))
with open(os.path.join(src, "ServerRequest.json")) as f:
    server_request = json.load(f)
dump(os.path.join(out, "ServerRequest-methods.json"), {
    o["properties"]["method"]["enum"][0]: o["properties"]["params"]["$ref"] for o in server_request["oneOf"]})
required = {}
for name in ("CommandExecutionRequestApprovalParams.json", "FileChangeRequestApprovalParams.json"):
    with open(os.path.join(src, name)) as f:
        schema = json.load(f)
    keys = schema["required"]
    required[name] = {"required": keys, "properties": {k: schema["properties"][k] for k in keys}}
dump(os.path.join(out, "ApprovalParams-required.json"), required)
sources = copied + ["ServerRequest.json", "CommandExecutionRequestApprovalParams.json",
                    "FileChangeRequestApprovalParams.json", "ExecCommandApprovalResponse.json",
                    "ApplyPatchApprovalResponse.json"]
dump(os.path.join(out, "provenance.json"), {
    "provider": f"codex-cli {version}",
    "generation_command": "codex app-server generate-json-schema --out <dir>",
    "source_sha256": {name: sha(os.path.join(src, name)) for name in sources},
    "excerpts": {
        "ServerRequest-methods.json": "All oneOf method enum values and their params references, in source order.",
        "ApprovalParams-required.json": "Exact required keys and their property schemas. Optional properties and definitions omitted.",
    },
    "availableDecisions": "Not declared by the generated params schemas. If supplied by a backend, constrain replies to its explicit array of offered decisions; do not add a permissive default for malformed or empty arrays.",
})
print(f"wrote schema-{version}/{{stable,experimental}} and approval-{version} under {dest}")
PY
