#!/usr/bin/env bash
set -euo pipefail

# Render Codex prompts into active .agents skills. The prompt remains the
# source; this helper deliberately supports a narrow target/check mode so the
# command sync pipeline can validate its selected or complete canonical set.
repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
prompt_dir="$repo_root/.codex/prompts"
skill_dir="$repo_root/.agents/skills"

usage() {
    echo "usage: .codex/migrate-prompts-to-skills.sh [--check] [prompt-name ...]" >&2
}

content_equal() {
    local left="$1"
    local right="$2"
    cmp -s "$left" "$right"
}

render_skill() {
    local prompt_path="$1"
    local target_path="$2"
    local prompt_file skill_name description

    prompt_file="${prompt_path##*/}"
    skill_name="${prompt_file%.md}"
    skill_name="${skill_name//_/-}"
    description="$(awk '/^description: / { sub(/^description: /, ""); print; exit }' "$prompt_path")"

    if [[ -z "$description" ]]; then
        printf 'missing description: %s\n' "$prompt_path" >&2
        return 1
    fi

    {
        printf '%s\n' '---'
        printf 'name: %s\n' "$skill_name"
        printf 'description: %s\n' "$description"
        printf '%s\n' '---'
        awk 'BEGIN { frontmatter = 0 } /^---$/ { frontmatter++; next } frontmatter >= 2 { print }' "$prompt_path" |
            sed \
                -e 's/\$ARGUMENTS/the task supplied in the current user prompt/g' \
                -e 's/\$FILEPATH/the supplied plan path/g'
    } >"$target_path"
}

mode="sync"
if [[ "${1:-}" == "--check" ]]; then
    mode="--check"
    shift
fi

if [[ "${1:-}" == "--help" || "${1:-}" == "-h" ]]; then
    usage
    exit 0
fi

if [[ $# -gt 0 ]]; then
    prompt_names=("$@")
else
    shopt -s nullglob
    prompt_paths=("$prompt_dir"/*.md)
    prompt_names=()
    for prompt_path in "${prompt_paths[@]}"; do
        prompt_names+=("$(basename "$prompt_path" .md)")
    done
fi

tmpdir="$(mktemp -d)"
trap 'rm -rf "$tmpdir"' EXIT
status=0

for prompt_name in "${prompt_names[@]}"; do
    prompt_path="$prompt_dir/$prompt_name.md"
    if [[ ! -f "$prompt_path" ]]; then
        printf 'missing prompt: %s\n' "$prompt_path" >&2
        status=1
        continue
    fi

    skill_name="${prompt_name//_/-}"
    target_path="$skill_dir/$skill_name/SKILL.md"
    expected_path="$tmpdir/$skill_name.SKILL.md"
    render_skill "$prompt_path" "$expected_path" || {
        status=1
        continue
    }

    if [[ "$mode" == "--check" ]]; then
        if [[ ! -f "$target_path" ]]; then
            printf 'missing skill: %s\n' "$target_path" >&2
            status=1
        elif ! content_equal "$expected_path" "$target_path"; then
            printf 'out of sync: %s\n' "$target_path" >&2
            status=1
        fi
        continue
    fi

    if [[ -f "$target_path" ]] && content_equal "$expected_path" "$target_path"; then
        continue
    fi
    if [[ -e "$target_path" && ! -w "$target_path" ]]; then
        printf 'skipped read-only file: %s\n' "$target_path" >&2
        status=1
        continue
    fi
    mkdir -p "$(dirname "$target_path")"
    cp "$expected_path" "$target_path"
    printf 'synced %s\n' "${target_path#$repo_root/}"
done

exit "$status"
