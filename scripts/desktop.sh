#!/usr/bin/env bash
# Build and run the RSI desktop UI (Tauri). Requires a running rsid
# (./scripts/dev-daemon.sh or the installed daemon); override the socket with
# RSI_DAEMON_SOCKET. `scripts/desktop.sh release` builds an optimized binary.
set -euo pipefail
cd "$(dirname "$0")/../desktop"
[ -d node_modules ] || npm install --no-audit --no-fund
npm run build
cd src-tauri
# build.rs watches ../dist, so cargo re-embeds frontend changes by itself.
if [ "${1:-}" = "release" ]; then
  cargo build --release
  echo "built: $(cargo metadata --format-version 1 --no-deps | python3 -c 'import json,sys;print(json.load(sys.stdin)["target_directory"])')/release/rsi-desktop"
else
  exec cargo run
fi
