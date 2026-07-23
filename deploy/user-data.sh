#!/usr/bin/env bash
# Paste this into EC2 user data only after publishing this tested branch to a
# private fork or other authenticated Git remote. Do not put a private key,
# wallet seed, or BRC_KEY_FILE contents in user data.
set -euo pipefail

export BRC_REPO_URL="https://github.com/REPLACE_WITH_YOUR_ACCOUNT/BrowserCoin.git"
export BRC_REVISION="REPLACE_WITH_TESTED_COMMIT_SHA"

workdir="$(mktemp -d)"
trap 'rm -rf "$workdir"' EXIT
git clone --depth 1 "$BRC_REPO_URL" "$workdir/browsercoin"
git -C "$workdir/browsercoin" fetch --depth 1 origin "$BRC_REVISION"
git -C "$workdir/browsercoin" checkout --detach "$BRC_REVISION"
"$workdir/browsercoin/deploy/bootstrap-ubuntu.sh"
