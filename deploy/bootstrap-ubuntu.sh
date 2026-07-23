#!/usr/bin/env bash
# Bootstrap a manually-created Ubuntu 24.04 EC2 host.
#
# Usage (run as root):
#   BRC_REPO_URL=https://github.com/YOUR_ACCOUNT/BrowserCoin.git \
#     BRC_REVISION=YOUR_COMMIT ./deploy/bootstrap-ubuntu.sh
#
# The repository must contain tools/headless-miner.ts and deploy/.
set -euo pipefail

: "${BRC_REPO_URL:?Set BRC_REPO_URL to your fork or a private Git remote}"
: "${BRC_REVISION:?Set BRC_REVISION to the tested commit SHA}"

export DEBIAN_FRONTEND=noninteractive
apt-get update
apt-get install -y ca-certificates curl git build-essential gnupg

install -d -m 0755 /etc/apt/keyrings
curl -fsSL https://deb.nodesource.com/gpgkey/nodesource-repo.gpg.key | gpg --batch --yes --dearmor -o /etc/apt/keyrings/nodesource.gpg
printf '%s\n' 'deb [signed-by=/etc/apt/keyrings/nodesource.gpg] https://deb.nodesource.com/node_22.x nodistro main' >/etc/apt/sources.list.d/nodesource.list
apt-get update
apt-get install -y nodejs
node --version
npm --version

if ! command -v cargo >/dev/null 2>&1 || ! rustc --version | awk '{ split($2, v, "."); exit !(v[1] > 1 || (v[1] == 1 && v[2] >= 85)) }'; then
  curl --proto '=https' --tlsv1.2 -fsSL https://sh.rustup.rs | sh -s -- -y --profile minimal
fi
export PATH="/root/.cargo/bin:$PATH"

if ! id -u browsercoin >/dev/null 2>&1; then
  useradd --system --create-home --home-dir /var/lib/browsercoin --shell /usr/sbin/nologin browsercoin
fi
install -d -o browsercoin -g browsercoin -m 0700 /var/lib/browsercoin

rm -rf /opt/browsercoin
git clone --no-checkout "$BRC_REPO_URL" /opt/browsercoin
git -C /opt/browsercoin checkout --detach "$BRC_REVISION"
git -C /opt/browsercoin status --porcelain
chown -R root:root /opt/browsercoin
chmod -R a-w /opt/browsercoin
cd /opt/browsercoin
npm ci
npm run build

install -d -m 0755 /etc/browsercoin
install -m 0755 deploy/browsercoin-miner /usr/local/bin/browsercoin-miner
install -m 0644 deploy/browsercoin-miner.service /etc/systemd/system/browsercoin-miner.service
systemctl daemon-reload
