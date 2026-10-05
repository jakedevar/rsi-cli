#!/usr/bin/env bash
# Export this repository to the public rsi-cli repository.
#
# Usage: scripts/export-rsi-cli.sh [--rev REV] [--dest DIR] [--push] [--skip-check]
#
#   --rev REV      commit to export (default: origin/rolling)
#   --dest DIR     rsi-cli checkout to sync into (default: a fresh clone in a
#                  temp dir)
#   --push         commit and push to rsi-cli main after the checks pass
#                  (without it the synced checkout is left for review)
#   --skip-check   skip `cargo check --workspace --all-targets`
#
# The export is the tree at REV minus private material, with deployment
# identifiers scrubbed and packaging/rsi-cli/ (public-only GitHub files)
# overlaid. Checks: tools/check_personal_paths.py and cargo check.
set -euo pipefail

ROOT="$(git rev-parse --show-toplevel)"
# Override with RSI_CLI_REMOTE (for example an SSH URL) to change transport.
REMOTE_URL="${RSI_CLI_REMOTE:-https://github.com/jakedevar/rsi-cli.git}"
REV="origin/rolling"
DEST=""
PUSH=0
CHECK=1

while [ $# -gt 0 ]; do
    case "$1" in
        --rev) REV="$2"; shift 2 ;;
        --dest) DEST="$2"; shift 2 ;;
        --push) PUSH=1; shift ;;
        --skip-check) CHECK=0; shift ;;
        -h|--help) sed -n '2,16p' "$0"; exit 0 ;;
        *) echo "unknown argument: $1" >&2; exit 2 ;;
    esac
done

# Private or internal-only paths, relative to the repository root.
EXCLUDES=(
    thoughts
    .rsi-task-artifacts
    .github
    LICENSING-DECISION.md
    packaging/rsi-cli
    docs/field-issues
    docs/plans
    docs/prompt-grading
    docs/gastwn_flywheel_features
    docs/symphony_capability_docs
    docs/memory_system_features
    docs/reference
    docs/design
    docs/tui-design-prompt.md
    docs/v111-branched-lineage-recovery.md
    docs/rsi-roadmap.md
    docs/recursive-master-implement-gate.md
)

SHA="$(git -C "$ROOT" rev-parse --verify "$REV^{commit}")"
SHORT="$(git -C "$ROOT" rev-parse --short=9 "$SHA")"

WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT
TREE="$WORK/tree"
mkdir -p "$TREE"
git -C "$ROOT" archive "$SHA" | tar -x -C "$TREE"

for p in "${EXCLUDES[@]}"; do rm -rf "${TREE:?}/$p"; done
# Loose screenshots at the repository root are local scratch.
find "$TREE" -maxdepth 1 -name '*.png' -delete

# Scrub deployment identifiers.
for f in "$TREE"/infra/aws/*/versions.tf; do
    if [ -f "$f" ]; then
        sed -i -E 's/(allowed_account_ids[[:space:]]*=[[:space:]]*\[)[^]]*\]/\1"000000000000"]/' "$f"
    fi
done
for f in "$TREE"/infra/aws/*/*.tf; do
    if [ -f "$f" ]; then
        sed -i -E 's/(budget_email[[:space:]]*=[[:space:]]*)"[^"]*"/\1"operator@example.com"/' "$f"
    fi
done

# Public-only files from the exported revision (its README describes the overlay).
git -C "$ROOT" archive "$SHA" packaging/rsi-cli \
    | { mkdir -p "$WORK/overlay"; tar -x -C "$WORK/overlay" --strip-components=2; }
rm -f "$WORK/overlay/README.md"
cp -a "$WORK/overlay/." "$TREE/"
printf '\n# Private planning notes live only in the private rsi repo\nthoughts/\n' >> "$TREE/.gitignore"

# Checks.
(cd "$TREE" && git init -q && git add -A && python3 tools/check_personal_paths.py)
if grep -lE 'allowed_account_ids.*"[1-9][0-9]{11}"' "$TREE"/infra/aws/*/versions.tf; then
    echo "export: unscrubbed AWS account id" >&2; exit 1
fi
rm -rf "$TREE/.git"
if [ "$CHECK" = 1 ]; then
    (cd "$TREE" && CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-$ROOT/target}" cargo check --workspace --all-targets --locked)
fi

if [ -z "$DEST" ]; then
    DEST="$(mktemp -d)/rsi-cli"
    git clone -q "$REMOTE_URL" "$DEST"
fi
rsync -a --delete --exclude=.git "$TREE/" "$DEST/"
git -C "$DEST" add -A
if git -C "$DEST" diff --cached --quiet; then
    echo "export: rsi-cli already matches $SHORT"
    exit 0
fi
git -C "$DEST" diff --cached --shortstat

if [ "$PUSH" = 1 ]; then
    git -C "$DEST" \
        -c user.name="$(git -C "$ROOT" config user.name)" \
        -c user.email="$(git -C "$ROOT" config user.email)" \
        commit -q -m "chore: sync rsi ($SHORT)" \
        -m "Public export of rsi $SHA via scripts/export-rsi-cli.sh."
    git -C "$DEST" push -q origin HEAD:main
    echo "export: pushed $(git -C "$DEST" rev-parse --short HEAD) from rsi $SHORT"
else
    echo "export: staged in $DEST (re-run with --push to publish)"
fi
