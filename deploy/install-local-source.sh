#!/usr/bin/env bash
# Install a source tree copied to the instance with rsync or scp.
# Run as root: ./deploy/install-local-source.sh /tmp/browsercoin-src
set -euo pipefail

source_dir="${1:-/tmp/browsercoin-src}"
test -f "$source_dir/package.json"
test -f "$source_dir/tools/headless-miner.ts"

export DEBIAN_FRONTEND=noninteractive
apt-get update
apt-get install -y ca-certificates curl git build-essential gnupg
install -d -m 0755 /etc/apt/keyrings
curl -fsSL https://deb.nodesource.com/gpgkey/nodesource-repo.gpg.key | gpg --batch --yes --dearmor -o /etc/apt/keyrings/nodesource.gpg
printf '%s\n' 'deb [signed-by=/etc/apt/keyrings/nodesource.gpg] https://deb.nodesource.com/node_22.x nodistro main' >/etc/apt/sources.list.d/nodesource.list
apt-get update
apt-get install -y nodejs

if ! command -v cargo >/dev/null 2>&1 || ! rustc --version | awk '{ split($2, v, "."); exit !(v[1] > 1 || (v[1] == 1 && v[2] >= 85)) }'; then
  curl --proto '=https' --tlsv1.2 -fsSL https://sh.rustup.rs | sh -s -- -y --profile minimal
fi
export PATH="/root/.cargo/bin:$PATH"

if ! id -u browsercoin >/dev/null 2>&1; then
  useradd --system --create-home --home-dir /var/lib/browsercoin --shell /usr/sbin/nologin browsercoin
fi
install -d -o browsercoin -g browsercoin -m 0700 /var/lib/browsercoin

rm -rf /opt/browsercoin
mkdir -p /opt/browsercoin
cp -a "$source_dir"/. /opt/browsercoin/
chown -R root:root /opt/browsercoin
chmod -R a-w /opt/browsercoin
cd /opt/browsercoin
npm ci
npm run build

install -d -m 0755 /etc/browsercoin
install -m 0755 deploy/browsercoin-miner /usr/local/bin/browsercoin-miner
install -m 0644 deploy/browsercoin-miner.service /etc/systemd/system/browsercoin-miner.service
systemctl daemon-reload
