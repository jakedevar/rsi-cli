#!/usr/bin/env bash
set -euo pipefail

usage() {
  echo "Usage: $0 <full-source-commit> <output-directory> [both|x86_64|arm64]" >&2
  echo "Run in a clean checkout at that commit. Requires cargo and cross for non-native targets." >&2
  echo "Set RSI_ARTIFACT_BUILDER=zigbuild to build both targets with cargo-zigbuild against" >&2
  echo "glibc RSI_ARTIFACT_GLIBC (default 2.34, Amazon Linux 2023) from any Linux host." >&2
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
builder_mode="${RSI_ARTIFACT_BUILDER:-auto}"
glibc_version="${RSI_ARTIFACT_GLIBC:-2.34}"
case "$builder_mode" in
  auto) ;;
  zigbuild)
    [[ "$glibc_version" =~ ^2\.[0-9]+$ ]] || {
      echo 'RSI_ARTIFACT_GLIBC must look like 2.34.' >&2
      exit 1
    }
    command -v zig >/dev/null && cargo zigbuild --help >/dev/null 2>&1 || {
      echo 'RSI_ARTIFACT_BUILDER=zigbuild requires zig and cargo-zigbuild.' >&2
      exit 1
    }
    ;;
  *)
    echo 'RSI_ARTIFACT_BUILDER must be auto or zigbuild.' >&2
    exit 1
    ;;
esac

host_target=''
if [[ "$(uname -s)" == Linux ]]; then
  case "$(uname -m)" in
    x86_64) host_target=x86_64-unknown-linux-gnu ;;
    aarch64) host_target=aarch64-unknown-linux-gnu ;;
  esac
fi

# The test seam must never ship in a release bundle (#1021 S4).
"$(dirname "${BASH_SOURCE[0]}")/check-release-seam.sh"

for architecture in "${architectures[@]}"; do
  case "$architecture" in
    x86_64) target=x86_64-unknown-linux-gnu ;;
    arm64) target=aarch64-unknown-linux-gnu ;;
  esac
  if [[ "$builder_mode" == zigbuild ]]; then
    # zig links against the requested glibc symbol versions, so a bundle built
    # on a newer distribution still loads on the host's older glibc.
    cargo zigbuild --locked --release --target "$target.$glibc_version" \
      --bin rsi --bin rsid --bin rsi-rpc --bin rsi-agent-mcp
  else
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
  fi
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
