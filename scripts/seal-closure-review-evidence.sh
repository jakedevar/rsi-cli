#!/usr/bin/env bash
set -euo pipefail

usage() {
  echo "usage: scripts/seal-closure-review-evidence.sh --source-head SHA --source-ref REF --source-worktree PATH --reviewer-session-id UUID --model-invocation-id UUID --review-policy-digest sha256:HEX --review PATH --manifest PATH" >&2
}

source_head=""
source_ref=""
source_worktree=""
reviewer_session_id=""
model_invocation_id=""
review_policy_digest=""
review_path=""
manifest_path=""

while [[ $# -gt 0 ]]; do
  case "$1" in
    --source-head) source_head="${2:-}"; shift 2 ;;
    --source-ref) source_ref="${2:-}"; shift 2 ;;
    --source-worktree) source_worktree="${2:-}"; shift 2 ;;
    --reviewer-session-id) reviewer_session_id="${2:-}"; shift 2 ;;
    --model-invocation-id) model_invocation_id="${2:-}"; shift 2 ;;
    --review-policy-digest) review_policy_digest="${2:-}"; shift 2 ;;
    --review) review_path="${2:-}"; shift 2 ;;
    --manifest) manifest_path="${2:-}"; shift 2 ;;
    *) usage; exit 2 ;;
  esac
done

for required in source_head source_ref source_worktree reviewer_session_id model_invocation_id review_policy_digest review_path manifest_path; do
  if [[ -z "${!required}" ]]; then
    usage
    exit 2
  fi
done

if [[ "${RSI_SESSION_ID:-}" != "$reviewer_session_id" ]] ||
   [[ "${RSI_MODEL_INVOCATION_ID:-}" != "$model_invocation_id" ]]; then
  echo "reviewer session/model invocation environment does not match the sealed review request" >&2
  exit 1
fi
case "$source_ref" in refs/heads/?*) ;; *) echo "source ref must be a local branch" >&2; exit 1 ;; esac
case "$review_path" in thoughts/shared/reviews/closure/?*/?*-review-v1.json) ;; *) echo "noncanonical Closure review path" >&2; exit 1 ;; esac
case "$manifest_path" in thoughts/shared/verification/closure/?*/?*-manifest-v2.md) ;; *) echo "noncanonical Closure manifest path" >&2; exit 1 ;; esac
case "$review_path$manifest_path" in *..*) echo "Closure evidence paths must not traverse" >&2; exit 1 ;; esac

evidence_root="$(git rev-parse --show-toplevel)"
if [[ "$(git rev-parse HEAD)" != "$source_head" ]]; then
  echo "evidence worktree must start at the sealed source head" >&2
  exit 1
fi
if [[ "$(git rev-parse "$source_ref")" != "$source_head" ]] ||
   [[ "$(git -C "$source_worktree" rev-parse HEAD)" != "$source_head" ]]; then
  echo "source ref/worktree drifted before evidence sealing" >&2
  exit 1
fi
for artifact in "$review_path" "$manifest_path"; do
  if [[ ! -f "$evidence_root/$artifact" ]] || [[ -L "$evidence_root/$artifact" ]]; then
    echo "Closure evidence must be two regular files" >&2
    exit 1
  fi
done

actual_paths="$(git status --porcelain --untracked-files=all | cut -c4- | LC_ALL=C sort)"
expected_paths="$(printf '%s\n%s\n' "$manifest_path" "$review_path" | LC_ALL=C sort)"
if [[ "$actual_paths" != "$expected_paths" ]]; then
  echo "evidence worktree contains changes outside the two canonical artifacts" >&2
  exit 1
fi

validator="${RSI_CLOSURE_EVIDENCE_VALIDATOR_BIN:-}"
if [[ -n "$validator" ]]; then
  "$validator" \
    --source-head "$source_head" \
    --reviewer-session-id "$reviewer_session_id" \
    --model-invocation-id "$model_invocation_id" \
    --review-policy-digest "$review_policy_digest" \
    --review "$review_path" \
    --manifest "$manifest_path"
else
  cargo run -q -p rsi-common --bin rsi-closure-evidence-validate -- \
    --source-head "$source_head" \
    --reviewer-session-id "$reviewer_session_id" \
    --model-invocation-id "$model_invocation_id" \
    --review-policy-digest "$review_policy_digest" \
    --review "$review_path" \
    --manifest "$manifest_path"
fi

git add -- "$review_path" "$manifest_path"
staged_paths="$(git diff --cached --name-only | LC_ALL=C sort)"
if [[ "$staged_paths" != "$expected_paths" ]]; then
  echo "Closure evidence commit must contain exactly the two canonical paths" >&2
  exit 1
fi
git commit -qm "Closure review evidence"
evidence_commit="$(git rev-parse HEAD)"
if [[ "$(git rev-parse HEAD^)" != "$source_head" ]] ||
   [[ "$(git diff-tree --no-commit-id --name-only -r HEAD | LC_ALL=C sort)" != "$expected_paths" ]]; then
  echo "Closure evidence commit parent/diff is not sealed-source plus two artifacts" >&2
  exit 1
fi
if [[ "$(git rev-parse "$source_ref")" != "$source_head" ]] ||
   [[ "$(git -C "$source_worktree" rev-parse HEAD)" != "$source_head" ]] ||
   [[ -n "$(git -C "$source_worktree" status --porcelain)" ]]; then
  echo "source ref/worktree changed while evidence was committed" >&2
  exit 1
fi
if [[ -n "$(git status --porcelain)" ]]; then
  echo "evidence worktree is not clean after commit" >&2
  exit 1
fi

printf '%s\n' "$evidence_commit"
