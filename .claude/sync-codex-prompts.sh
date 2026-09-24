#!/usr/bin/env bash
set -euo pipefail

# Render Codex prompt catalog into Claude Code slash commands.
# Claude-only routing metadata is preserved for existing commands and supplied
# by the map below for new commands.
repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
prompt_dir="$repo_root/.codex/prompts"
command_dir="$repo_root/.claude/commands"

usage() {
    echo "usage: .claude/sync-codex-prompts.sh [--check] [prompt-name ...]" >&2
}

model_for_capability_class() {
    case "$1" in
        architect) printf '%s\n' opus ;;
        implementer) printf '%s\n' sonnet ;;
        lookup_fast) printf '%s\n' haiku ;;
        *)
            printf 'unknown capability class: %s\n' "$1" >&2
            return 1 ;;
    esac
}

metadata_for_new_command() {
    case "$1" in
        auto_build|auto_plan|auto_research|brainstorm|cannibalize_research|compile_delegation|create_plan|drive_plan|iterate_plan|master_implement|master_orchestrate|master_ui_implement|mwp_incorporate|mwp_incorporate_standalone|oneshot|opus5_effort_program|orchestration_router|plan|project_spec|ralph_plan|ralph_research|research|research_codebase|review_plan|team_implement|team_plan|team_research|termwright_e2e|v1_burndown)
            printf '%s\n%s\n' opus architect ;;
        auto_implement|create_handoff|debug|feature_status|implement|implement_plan|oneshot_plan|ralph_impl|resume_handoff|ship_small|validate_plan|verify_phase)
            printf '%s\n%s\n' sonnet implementer ;;
        ci_commit|ci_describe_pr|commit|create_worktree|db|describe_pr|describe_pr_nt|linear|local_review)
            printf '%s\n%s\n' haiku lookup_fast ;;
        *)
            printf 'missing Claude routing metadata for: %s\n' "$1" >&2
            return 1 ;;
    esac
}

frontmatter_value() {
    local path="$1"
    local key="$2"
    awk -v key="$key" '
        /^---$/ { delimiters++; next }
        delimiters == 1 && index($0, key ": ") == 1 {
            sub("^" key ": ", "")
            print
            exit
        }
    ' "$path"
}

render_command() {
    local prompt_path="$1"
    local target_path="$2"
    local prompt_name description argument_hint model capability_class

    prompt_name="$(basename "$prompt_path" .md)"
    description="$(frontmatter_value "$prompt_path" description)"
    argument_hint="$(frontmatter_value "$prompt_path" argument-hint)"
    model="$(frontmatter_value "$target_path" model 2>/dev/null || true)"
    capability_class="$(frontmatter_value "$target_path" capability_class 2>/dev/null || true)"

    if [[ -z "$description" ]]; then
        printf 'missing description: %s\n' "$prompt_path" >&2
        return 1
    fi
    if [[ -z "$capability_class" ]]; then
        local metadata
        metadata="$(metadata_for_new_command "$prompt_name")" || return 1
        model="${metadata%%$'\n'*}"
        capability_class="${metadata#*$'\n'}"
        capability_class="${capability_class%%$'\n'*}"
    elif [[ -z "$model" ]]; then
        model="$(model_for_capability_class "$capability_class")" || return 1
    fi

    {
        printf '%s\n' '---'
        printf 'description: %s\n' "$description"
        [[ -z "$argument_hint" ]] || printf 'argument-hint: %s\n' "$argument_hint"
        printf 'model: %s\n' "$model"
        printf 'capability_class: %s\n' "$capability_class"
        printf '%s\n' '---'
        awk 'BEGIN { frontmatter = 0 } /^---$/ && frontmatter < 2 { frontmatter++; next } frontmatter >= 2 { print }' "$prompt_path"
    }
}

mode=sync
if [[ "${1:-}" == '--check' ]]; then
    mode=check
    shift
fi
if [[ "${1:-}" == '--help' || "${1:-}" == '-h' ]]; then
    usage
    exit 0
fi

if [[ $# -gt 0 ]]; then
    prompt_names=("$@")
else
    shopt -s nullglob
    prompt_names=()
    for prompt_path in "$prompt_dir"/*.md; do
        prompt_names+=("$(basename "$prompt_path" .md)")
    done
fi

temp_dir="$(mktemp -d)"
trap 'rm -rf "$temp_dir"' EXIT
status=0

for prompt_name in "${prompt_names[@]}"; do
    prompt_path="$prompt_dir/$prompt_name.md"
    target_path="$command_dir/$prompt_name.md"
    expected_path="$temp_dir/$prompt_name.md"
    if [[ ! -f "$prompt_path" ]]; then
        printf 'missing Codex prompt: %s\n' "$prompt_path" >&2
        status=1
        continue
    fi
    render_command "$prompt_path" "$target_path" >"$expected_path" || {
        status=1
        continue
    }
    if [[ "$mode" == check ]]; then
        if [[ ! -f "$target_path" ]]; then
            printf 'missing Claude command: %s\n' "$target_path" >&2
            status=1
        elif ! cmp -s "$expected_path" "$target_path"; then
            printf 'out of sync: %s\n' "$target_path" >&2
            status=1
        fi
    elif [[ ! -f "$target_path" ]] || ! cmp -s "$expected_path" "$target_path"; then
        cp "$expected_path" "$target_path"
        printf 'synced %s\n' "${target_path#$repo_root/}"
    fi
done

if [[ $# -eq 0 ]]; then
    for command_path in "$command_dir"/*.md; do
        command_name="$(basename "$command_path" .md)"
        if [[ ! -f "$prompt_dir/$command_name.md" ]]; then
            printf 'Claude-only command: %s\n' "$command_path" >&2
            status=1
        fi
    done
fi

exit "$status"
