#!/usr/bin/env bash
# A keyless, loopback-only SearXNG for the native web_search tool (#2315).
#
# Installs and starts SearXNG as a user-level systemd service
# (rsi-searxng.service), bound to 127.0.0.1 only, with the JSON API enabled,
# then prints the URL to put in the "Harness SearXNG URL" setting.
#
# Runtime, first usable match wins: podman, docker, else a Python venv built
# from the SearXNG sources (needs git, python3 with venv, and a C toolchain). A
# container runtime counts only when `<runtime> info` succeeds, so a docker the
# user cannot reach (no socket access) falls through to the venv. Override with
# SEARXNG_RUNTIME=podman|docker|venv.
#
# Public SearXNG instances usually block automated JSON queries, so a local
# instance is the supported path.
set -euo pipefail

usage() {
  cat <<'EOF'
usage: searxng-local.sh [--dry-run] <command>

commands:
  up       install and start the local instance (safe to re-run: it changes
           nothing, and restarts nothing, when the setup is already current)
  down     stop and disable the service (keeps the data directory)
  status   show the service, the URL and whether the JSON API answers
  url      print the URL for the "Harness SearXNG URL" setting
  run      run the instance in the foreground (the service's ExecStart; also
           the way to run it where systemd user services do not exist)

options:
  --dry-run   print what would change; write nothing and start nothing
  -h, --help  this text

environment (all optional):
  SEARXNG_PORT      loopback port (default 8888)
  SEARXNG_RUNTIME   podman | docker | venv (default: first usable one)
  SEARXNG_HOME      data directory (default $XDG_DATA_HOME/rsi/searxng)
  SEARXNG_IMAGE     container image (default docker.io/searxng/searxng:latest)

The instance listens on 127.0.0.1 only; there is no option to widen that.
After `up`, set the URL it prints: TUI Settings > Harness Tools > Harness
SearXNG URL, or
  rsi-rpc UpdateDaemonConfig --params '{"field":"harness_searxng_url","value":"URL"}'
The setting applies to the next Harness launch.
EOF
}

BIND=127.0.0.1
PORT="${SEARXNG_PORT:-8888}"
DATA_DIR="${SEARXNG_HOME:-${XDG_DATA_HOME:-$HOME/.local/share}/rsi/searxng}"
UNIT_DIR="${XDG_CONFIG_HOME:-$HOME/.config}/systemd/user"
UNIT=rsi-searxng.service
IMAGE="${SEARXNG_IMAGE:-docker.io/searxng/searxng:latest}"
SRC_URL="${SEARXNG_SRC_URL:-https://github.com/searxng/searxng.git}"
CONTAINER=rsi-searxng
DRY_RUN=0
CHANGED=0

say() { printf '%s\n' "$*"; }
die() { printf 'searxng-local: %s\n' "$1" >&2; exit "${2:-1}"; }
have() { command -v "$1" >/dev/null 2>&1; }

url() { printf 'http://%s:%s' "$BIND" "$PORT"; }

case "$PORT" in
  '' | *[!0-9]*) die "SEARXNG_PORT must be a number, got '$PORT'" 2 ;;
esac
if [ "$PORT" -lt 1024 ] || [ "$PORT" -gt 65535 ]; then
  die "SEARXNG_PORT must be between 1024 and 65535, got '$PORT'" 2
fi

# The configured values reach a systemd unit file and a container command line.
# Control characters (a newline would start a new directive) are refused; the
# data directory must be absolute (it names the unit's ExecStart); the image may
# not look like an option or hold whitespace. Everything else is escaped when
# the unit is rendered (see unit_quote).
reject_control() {
  case "$2" in
    *[[:cntrl:]]*) die "$1 must not contain line breaks or control characters" 2 ;;
  esac
}
reject_control SEARXNG_HOME "$DATA_DIR"
reject_control SEARXNG_IMAGE "$IMAGE"
case "$DATA_DIR" in
  /*) ;;
  *) die "SEARXNG_HOME must be an absolute path, got '$DATA_DIR'" 2 ;;
esac
case "$IMAGE" in
  '' | -* | *[[:space:]]*) die "SEARXNG_IMAGE must be a single image reference, got '$IMAGE'" 2 ;;
esac

# usable_runtime NAME: the runtime is installed and its daemon/socket answers
# quickly. Returns 1 and prints the reason on stderr otherwise.
usable_runtime() {
  local rt="$1" reason
  have "$rt" || return 1
  if have timeout; then
    reason="$(timeout 15 "$rt" info 2>&1 >/dev/null)" && return 0
  else
    reason="$("$rt" info 2>&1 >/dev/null)" && return 0
  fi
  reason="$(printf '%s' "$reason" | head -n 1)"
  printf 'searxng-local: %s is installed but not usable (%s info failed%s%s); skipping it\n' \
    "$rt" "$rt" "${reason:+: }" "$reason" >&2
  return 1
}

runtime() {
  local chosen="${SEARXNG_RUNTIME:-}"
  if [ -z "$chosen" ]; then
    if usable_runtime podman; then chosen=podman
    elif usable_runtime docker; then chosen=docker
    else chosen=venv
    fi
  fi
  case "$chosen" in
    podman | docker)
      have "$chosen" || die "SEARXNG_RUNTIME=$chosen but '$chosen' is not installed" 2
      ;;
    venv) ;;
    *) die "SEARXNG_RUNTIME must be podman, docker or venv, got '$chosen'" 2 ;;
  esac
  printf '%s' "$chosen"
}

# write_if_changed PATH MODE: stdin becomes the file unless it already holds
# exactly that. Sets CHANGED=1 when it wrote (or, in --dry-run, would write).
write_if_changed() {
  local path="$1" mode="$2" tmp
  tmp="$(mktemp)"
  cat >"$tmp"
  if [ -f "$path" ] && cmp -s "$tmp" "$path"; then
    rm -f "$tmp"
    return 0
  fi
  CHANGED=1
  if [ "$DRY_RUN" = 1 ]; then
    say "would write $path"
    rm -f "$tmp"
    return 0
  fi
  mkdir -p "$(dirname "$path")"
  install -m "$mode" "$tmp" "$path"
  rm -f "$tmp"
  say "wrote $path"
}

ensure_secret() {
  local file="$DATA_DIR/secret"
  if [ -f "$file" ]; then
    return 0
  fi
  if [ "$DRY_RUN" = 1 ]; then
    say "would create $file (a random instance secret, mode 600)"
    return 0
  fi
  mkdir -p "$DATA_DIR"
  (umask 077 && head -c 32 /dev/urandom | od -An -tx1 | tr -d ' \n' >"$file")
  say "created $file"
}

secret() {
  if [ -f "$DATA_DIR/secret" ]; then
    cat "$DATA_DIR/secret"
  else
    printf 'dry-run-secret'
  fi
}

render_settings() {
  local rt="$1" inner_bind inner_port
  if [ "$rt" = venv ]; then
    inner_bind="$BIND"
    inner_port="$PORT"
  else
    # Inside the container the server must listen on all interfaces; the
    # published port is bound to the host's loopback only (see `run`).
    inner_bind=0.0.0.0
    inner_port=8080
  fi
  cat <<EOF
# Generated by scripts/searxng-local.sh; \`up\` rewrites this file.
use_default_settings: true
general:
  instance_name: "rsi local search"
  enable_metrics: false
search:
  # The JSON API is what the native web_search tool calls.
  formats:
    - html
    - json
server:
  bind_address: "$inner_bind"
  port: $inner_port
  secret_key: "$(secret)"
  limiter: false
  public_instance: false
  image_proxy: false
EOF
}

# The unit runs a copy of this script from the data directory, so it keeps
# working when the checkout it was installed from goes away.
installed_script() { printf '%s/searxng-local.sh' "$DATA_DIR"; }

# unit_quote VALUE [exec]: VALUE as a double-quoted systemd word. Inside the
# quotes backslash and the quote are escaped; `%` would start a specifier, so it
# is doubled; in an Exec* line `$` would start a variable, so it is doubled too.
unit_quote() {
  local v="$1"
  v="${v//\\/\\\\}"
  v="${v//\"/\\\"}"
  v="${v//%/%%}"
  if [ "${2:-}" = exec ]; then
    v="${v//\$/\$\$}"
  fi
  printf '"%s"' "$v"
}

render_unit() {
  local rt="$1"
  cat <<EOF
# Generated by scripts/searxng-local.sh; \`up\` rewrites this file.
[Unit]
Description=Local SearXNG for the rsi web_search tool (127.0.0.1 only)
After=network-online.target

[Service]
Type=simple
Environment=$(unit_quote "SEARXNG_PORT=$PORT")
Environment=$(unit_quote "SEARXNG_HOME=$DATA_DIR")
Environment=$(unit_quote "SEARXNG_RUNTIME=$rt")
Environment=$(unit_quote "SEARXNG_IMAGE=$IMAGE")
ExecStart=$(unit_quote "$(installed_script)" exec) run
Restart=on-failure
RestartSec=5

[Install]
WantedBy=default.target
EOF
}

need_systemd_user() {
  have systemctl && systemctl --user show-environment >/dev/null 2>&1 \
    || die "'up' needs systemd user services (Linux). Run '$0 run' in a terminal or tmux instead." 3
}

prepare_runtime() {
  local rt="$1"
  case "$rt" in
    podman | docker)
      if "$rt" image inspect "$IMAGE" >/dev/null 2>&1; then
        return 0
      fi
      if [ "$DRY_RUN" = 1 ]; then
        say "would pull $IMAGE with $rt"
      else
        say "pulling $IMAGE with $rt"
        "$rt" pull "$IMAGE"
        CHANGED=1
      fi
      ;;
    venv)
      local src="$DATA_DIR/src" venv="$DATA_DIR/venv" stamp want=""
      # SearXNG's setup.py imports the package, which imports its own
      # requirements (msgspec, ...), so they go in before the editable install.
      # `.installed` holds a checksum of requirements.txt; a changed file
      # re-runs the install, an unchanged one does nothing.
      if [ -f "$src/requirements.txt" ]; then
        want="$(cksum <"$src/requirements.txt")"
      fi
      stamp="$(cat "$DATA_DIR/.installed" 2>/dev/null || true)"
      if [ -f "$DATA_DIR/.installed" ] && [ -n "$want" ] && [ "$stamp" = "$want" ]; then
        return 0
      fi
      if [ "$DRY_RUN" = 1 ]; then
        say "would clone $SRC_URL, install the requirements from $src/requirements.txt, then install SearXNG into $venv"
        return 0
      fi
      have git || die "the venv runtime needs git" 2
      have python3 || die "the venv runtime needs python3" 2
      mkdir -p "$DATA_DIR"
      [ -d "$src/.git" ] || git clone --depth 1 "$SRC_URL" "$src"
      [ -f "$src/requirements.txt" ] || die "$src/requirements.txt is missing; remove $src and retry" 1
      [ -x "$venv/bin/python" ] || python3 -m venv "$venv"
      "$venv/bin/pip" install -U pip setuptools wheel pyyaml
      "$venv/bin/pip" install -r "$src/requirements.txt"
      (cd "$src" && "$venv/bin/pip" install --use-pep517 --no-build-isolation -e .)
      cksum <"$src/requirements.txt" >"$DATA_DIR/.installed"
      CHANGED=1
      ;;
  esac
}

wait_ready() {
  have curl || { say "curl is not installed; skipping the readiness check"; return 0; }
  local i
  for i in $(seq 1 60); do
    if curl -fsS --max-time 3 "$(url)/healthz" >/dev/null 2>&1; then
      return 0
    fi
    sleep 1
  done
  return 1
}

json_status() {
  curl -s -o /dev/null -w '%{http_code}' --max-time 30 "$(url)/search?q=rsi&format=json" 2>/dev/null || true
}

report_json() {
  have curl || return 0
  local code
  code="$(json_status)"
  case "$code" in
    200) say "json api:  enabled (HTTP 200)" ;;
    403) say "json api:  REFUSED (HTTP 403): the instance has the json format disabled; check $DATA_DIR/config/settings.yml" ;;
    *) say "json api:  HTTP ${code:-none} (the instance may still be starting)" ;;
  esac
}

cmd_up() {
  [ "$DRY_RUN" = 1 ] || need_systemd_user
  local rt
  rt="$(runtime)"
  say "runtime: $rt"
  ensure_secret
  prepare_runtime "$rt"
  CHANGED_BEFORE=$CHANGED
  CHANGED=0
  # Redirects from process substitution (not pipes) keep CHANGED in this shell.
  write_if_changed "$DATA_DIR/config/settings.yml" 600 < <(render_settings "$rt")
  local settings_changed=$CHANGED
  CHANGED=0
  write_if_changed "$(installed_script)" 755 <"${BASH_SOURCE[0]}"
  write_if_changed "$UNIT_DIR/$UNIT" 644 < <(render_unit "$rt")
  local unit_changed=$CHANGED
  if [ "$DRY_RUN" = 1 ]; then
    say "would start $UNIT and print $(url)"
    return 0
  fi
  if [ "$unit_changed" = 1 ]; then
    systemctl --user daemon-reload
  fi
  systemctl --user enable "$UNIT" >/dev/null 2>&1
  if [ "$unit_changed" = 1 ] || [ "$settings_changed" = 1 ] || [ "$CHANGED_BEFORE" = 1 ]; then
    systemctl --user restart "$UNIT"
  else
    systemctl --user start "$UNIT"
  fi
  if wait_ready; then
    report_json
  else
    say "the instance did not answer on $(url) within 60s; see: journalctl --user -u $UNIT"
  fi
  say ""
  say "url:       $(url)"
  say "Set it in the TUI (Settings > Harness Tools > Harness SearXNG URL) or with:"
  say "  rsi-rpc UpdateDaemonConfig --params '{\"field\":\"harness_searxng_url\",\"value\":\"$(url)\"}'"
}

cmd_down() {
  if [ "$DRY_RUN" = 1 ]; then
    say "would stop and disable $UNIT"
    return 0
  fi
  need_systemd_user
  systemctl --user disable --now "$UNIT" >/dev/null 2>&1 || true
  say "stopped $UNIT (data kept in $DATA_DIR)"
}

cmd_status() {
  say "url:       $(url)"
  say "data:      $DATA_DIR"
  if have systemctl && systemctl --user show-environment >/dev/null 2>&1; then
    say "service:   $(systemctl --user is-active "$UNIT" 2>/dev/null || true)"
  else
    say "service:   (no systemd user services)"
  fi
  report_json
}

cmd_run() {
  local rt
  rt="$(runtime)"
  ensure_secret
  write_if_changed "$DATA_DIR/config/settings.yml" 600 < <(render_settings "$rt")
  case "$rt" in
    podman | docker)
      "$rt" rm -f "$CONTAINER" >/dev/null 2>&1 || true
      exec "$rt" run --rm --name "$CONTAINER" \
        -p "$BIND:$PORT:8080" \
        -v "$DATA_DIR/config:/etc/searxng:Z" \
        "$IMAGE"
      ;;
    venv)
      [ -f "$DATA_DIR/.installed" ] || die "not installed yet; run '$0 up' first" 2
      cd "$DATA_DIR/src"
      SEARXNG_SETTINGS_PATH="$DATA_DIR/config/settings.yml" exec "$DATA_DIR/venv/bin/python" searx/webapp.py
      ;;
  esac
}

main() {
  local cmd=""
  while [ "$#" -gt 0 ]; do
    case "$1" in
      -h | --help) usage; exit 0 ;;
      --dry-run) DRY_RUN=1 ;;
      up | down | status | url | run)
        [ -z "$cmd" ] || die "one command at a time" 2
        cmd="$1"
        ;;
      *) usage >&2; die "unknown argument '$1'" 2 ;;
    esac
    shift
  done
  [ -n "$cmd" ] || { usage >&2; exit 2; }
  case "$cmd" in
    up) cmd_up ;;
    down) cmd_down ;;
    status) cmd_status ;;
    url) url; printf '\n' ;;
    run) cmd_run ;;
  esac
}

main "$@"
