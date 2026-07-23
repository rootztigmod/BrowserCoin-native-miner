#!/usr/bin/env bash
# Build a portable Linux x86_64 BrowserCoin miner executable. Bun embeds the
# controller/runtime; this script embeds the Rust worker and JS fallback worker.
set -euo pipefail

readonly root_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
readonly assets_dir="$root_dir/tools/standalone-assets"
readonly output_dir="${BRC_RELEASE_DIR:-$root_dir/release}"
readonly output="$output_dir/browsercoin-miner-linux-x64"

command -v bun >/dev/null 2>&1 || {
  printf 'bun is required to build the standalone release. Install it from https://bun.sh/.\n' >&2
  exit 1
}

mkdir -p "$assets_dir" "$output_dir"
trap 'rm -f "$assets_dir/browsercoin-native-miner.asset" "$assets_dir/headless-miner.worker.asset"' EXIT

(
  cd "$root_dir/native"
  RUSTFLAGS="${BRC_STANDALONE_RUSTFLAGS:--C target-cpu=x86-64}" \
    cargo build --release -p browsercoin-native-miner
)

(
  cd "$root_dir"
  ./node_modules/.bin/tsx tools/build-headless-worker.ts
)

cp "$root_dir/native/target/release/browsercoin-native-miner" "$assets_dir/browsercoin-native-miner.asset"
cp "$root_dir/tools/dist/headless-miner.worker.mjs" "$assets_dir/headless-miner.worker.asset"
chmod 700 "$assets_dir/browsercoin-native-miner.asset"

(
  cd "$root_dir"
  bun build tools/standalone-entry.ts \
    --compile \
    --target=bun-linux-x64 \
    --outfile "$output"
)

chmod 755 "$output"
(
  cd "$output_dir"
  sha256sum "$(basename "$output")" >"$(basename "$output").sha256"
)
printf 'Built %s\n' "$output"
