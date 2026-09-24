#!/usr/bin/env bash
set -euo pipefail

# Claude commands are canonical. Every top-level command is cross-harness and
# this script renders its Codex adapters, Gemini wrappers, and Codex-derived
# skills. Discovering the catalog prevents a new Claude command from silently
# lacking a Codex skill.
ROOT="$(git rev-parse --show-toplevel)"
CLAUDE_COMMAND_DIR="$ROOT/.claude/commands"
CLAUDE_SKILL_DIR="$ROOT/.claude/skills"
CODEX_PROMPT_DIR="$ROOT/.codex/prompts"
CODEX_SKILL_DIR="$ROOT/.agents/skills"
GEMINI_COMMAND_DIR="$ROOT/.gemini/commands"
SKILL_RENDERER="$ROOT/.codex/migrate-prompts-to-skills.sh"

CANONICAL_COMMANDS=()
while IFS= read -r command; do
  CANONICAL_COMMANDS+=("$command")
done < <(
  find "$CLAUDE_COMMAND_DIR" -maxdepth 1 -type f -name '*.md' -exec basename {} .md \; |
    LC_ALL=C sort
)

usage() {
  echo "usage: scripts/sync-agent-commands.sh [--check] [canonical-command ...]" >&2
}

is_canonical_command() {
  local candidate="$1"
  local command
  for command in "${CANONICAL_COMMANDS[@]}"; do
    [[ "$command" == "$candidate" ]] && return 0
  done
  return 1
}

content_equal() {
  local left="$1"
  local right="$2"
  local normalized_left
  local normalized_right

  cmp -s "$left" "$right" && return 0
  normalized_left="$(mktemp)"
  normalized_right="$(mktemp)"
  awk 'NR > 1 { print previous } { previous = $0 } END { printf "%s", previous }' "$left" >"$normalized_left"
  awk 'NR > 1 { print previous } { previous = $0 } END { printf "%s", previous }' "$right" >"$normalized_right"
  cmp -s "$normalized_left" "$normalized_right"
  local status=$?
  rm -f "$normalized_left" "$normalized_right"
  return "$status"
}

render_codex_prompt() {
  local src="$1"
  local command="$2"
  local dst="$3"
  local argument_hint=""
  case "$command" in
    create_worktree) argument_hint='FILEPATH=' ;;
    drive_plan|ship_small) argument_hint='[TASK]' ;;
  esac

  # Only the first YAML block is provider routing metadata. Body text such as
  # the debug handoff's `capability_class:` line must remain byte-for-byte.
  awk -v argument_hint="$argument_hint" '
    NR == 1 && $0 == "---" { frontmatter = 1; print; next }
    frontmatter == 1 {
      if ($0 == "---") {
        if (argument_hint != "") print "argument-hint: " argument_hint
        print
        frontmatter = 2
        next
      }
      if ($0 ~ /^(model|capability_class):[[:space:]]*/) next
    }
    { print }
  ' "$src" >"$dst"
}

command_description() {
  local src="$1"
  local description

  description="$(sed -n 's/^description:[[:space:]]*//p' "$src" | head -n 1)"
  if [[ -z "$description" ]]; then
    description="$(basename "$src" .md)"
  fi

  printf '%s' "$description" | sed 's/\\/\\\\/g; s/"/\\"/g'
}

render_gemini_command() {
  local src="$1"
  local dst="$2"

  if grep -q "'''" "$src"; then
    echo "cannot encode Gemini prompt containing TOML literal delimiter: $src" >&2
    return 1
  fi

  {
    printf 'description = "%s"\n' "$(command_description "$src")"
    printf "prompt = '''"
    cat "$src"
    printf "'''\n"
  } >"$dst"
}

write_if_changed() {
  local expected="$1"
  local destination="$2"

  if [[ -f "$destination" ]] && content_equal "$expected" "$destination"; then
    return 0
  fi
  if [[ -e "$destination" && ! -w "$destination" ]]; then
    echo "skipped read-only file: $destination" >&2
    return 1
  fi
  mkdir -p "$(dirname "$destination")"
  if [[ ! -w "$(dirname "$destination")" ]]; then
    echo "skipped read-only directory: $(dirname "$destination")" >&2
    return 1
  fi
  cp "$expected" "$destination"
  echo "synced ${destination#$ROOT/}"
}

check_if_changed() {
  local expected="$1"
  local destination="$2"

  if [[ ! -f "$destination" ]]; then
    echo "missing: ${destination#$ROOT/}" >&2
    return 1
  fi
  if ! content_equal "$expected" "$destination"; then
    echo "out of sync: ${destination#$ROOT/}" >&2
    return 1
  fi
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

full_catalog=true
if [[ $# -gt 0 ]]; then
  full_catalog=false
  selected_commands=("$@")
else
  selected_commands=("${CANONICAL_COMMANDS[@]}")
fi

for command in "${selected_commands[@]}"; do
  if ! is_canonical_command "$command"; then
    echo "not a canonical command: $command" >&2
    usage
    exit 2
  fi
done

tmpdir="$(mktemp -d)"
trap 'rm -rf "$tmpdir"' EXIT
status=0

for command in "${selected_commands[@]}"; do
  src="$CLAUDE_COMMAND_DIR/$command.md"
  codex_expected="$tmpdir/$command.codex.md"
  gemini_expected="$tmpdir/$command.gemini.toml"

  if [[ ! -f "$src" ]]; then
    echo "missing canonical Claude command: ${src#$ROOT/}" >&2
    status=1
    continue
  fi

  render_codex_prompt "$src" "$command" "$codex_expected"
  render_gemini_command "$src" "$gemini_expected" || {
    status=1
    continue
  }

  if [[ "$mode" == "--check" ]]; then
    check_if_changed "$codex_expected" "$CODEX_PROMPT_DIR/$command.md" || status=1
    check_if_changed "$gemini_expected" "$GEMINI_COMMAND_DIR/$command.toml" || status=1
    "$SKILL_RENDERER" --check "$command" || status=1
  else
    write_if_changed "$codex_expected" "$CODEX_PROMPT_DIR/$command.md" || status=1
    write_if_changed "$gemini_expected" "$GEMINI_COMMAND_DIR/$command.toml" || status=1
    "$SKILL_RENDERER" "$command" || status=1
  fi
done

native_skill_files=0
if [[ "$full_catalog" == true && -d "$CLAUDE_SKILL_DIR" ]]; then
  while IFS= read -r src; do
    relative="${src#$CLAUDE_SKILL_DIR/}"
    destination="$CODEX_SKILL_DIR/$relative"
    native_skill_files=$((native_skill_files + 1))
    if [[ "$mode" == "--check" ]]; then
      check_if_changed "$src" "$destination" || status=1
    else
      write_if_changed "$src" "$destination" || status=1
    fi
  done < <(find "$CLAUDE_SKILL_DIR" -type f -print | LC_ALL=C sort)
fi

if [[ "$mode" == "--check" && "$status" -eq 0 ]]; then
  echo "agent commands are in sync: ${#selected_commands[@]} canonical command(s), derived skills, and $native_skill_files native skill file(s) checked"
fi

exit "$status"
