#!/usr/bin/env bash
set -euo pipefail

usage() {
  echo "Usage: $0 <full-source-commit> <output-directory> [both|x86_64|arm64]" >&2
  echo "Run in a clean checkout at that commit. Requires cargo and cross for non-native targets." >&2
  exit 2
}

[[ $# -ge 2 && $# -le 3 ]] || usage
source_ref="$1"
output_dir="$2"
selected_architecture="${3:-both}"
[[ "$source_ref" =~ ^[0-9a-f]{40}$ ]] || usage
case "$selected_architecture" in
  both) architectures=(x86_64 arm64) ;;
  x86_64|arm64) architectures=("$selected_architecture") ;;
  *) usage ;;
esac
repo_dir="$(git rev-parse --show-toplevel)"
[[ "$(git -C "$repo_dir" rev-parse HEAD)" == "$source_ref" ]] || {
  echo 'Checkout HEAD does not match the requested source commit.' >&2
  exit 1
}
[[ -z "$(git -C "$repo_dir" status --porcelain)" ]] || {
  echo 'The source checkout must be clean before building artifacts.' >&2
  exit 1
}
mkdir -p "$output_dir"
output_dir="$(cd "$output_dir" && pwd -P)"
[[ "$output_dir" != "$repo_dir" && "$output_dir" != "$repo_dir/"* ]] || {
  echo 'Output directory must be outside the source checkout.' >&2
  exit 1
}
cd "$repo_dir"
target_dir="${CARGO_TARGET_DIR:-target}"

host_target=''
if [[ "$(uname -s)" == Linux ]]; then
  case "$(uname -m)" in
    x86_64) host_target=x86_64-unknown-linux-gnu ;;
    aarch64) host_target=aarch64-unknown-linux-gnu ;;
  esac
fi

for architecture in "${architectures[@]}"; do
  case "$architecture" in
    x86_64) target=x86_64-unknown-linux-gnu ;;
    arm64) target=aarch64-unknown-linux-gnu ;;
  esac
  if [[ "$target" == "$host_target" ]]; then
    builder=cargo
  else
    builder=cross
    command -v cross >/dev/null || {
      echo "cross is required to build $target on this machine." >&2
      exit 1
    }
  fi
  "$builder" build --locked --release --target "$target" \
    --bin rsi --bin rsid --bin rsi-rpc --bin rsi-agent-mcp
  staging="$(mktemp -d)"
  for binary in rsi rsid rsi-rpc rsi-agent-mcp; do
    install -m 755 "$target_dir/$target/release/$binary" "$staging/$binary"
  done
  printf '%s\n' "$source_ref" > "$staging/source-commit"
  printf '%s\n' "$architecture" > "$staging/architecture"
  artifact="$output_dir/rsi-$source_ref-$architecture.tar.gz"
  tar --sort=name --mtime='@0' --owner=0 --group=0 --numeric-owner -C "$staging" -cf - . | gzip -n > "$artifact"
  rm -rf "$staging"
  (cd "$output_dir" && sha256sum "$(basename "$artifact")") | tee "$artifact.sha256"
done
