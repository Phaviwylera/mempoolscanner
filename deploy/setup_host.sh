#!/usr/bin/env bash
set -euo pipefail

ROOT=/opt/mempoolscanner
id -u mempolscan >/dev/null 2>&1 || useradd --system --create-home --home-dir /home/mempolscan --shell /bin/bash mempolscan

export DEBIAN_FRONTEND=noninteractive
apt-get update
apt-get install -y --no-install-recommends build-essential ca-certificates curl git jq nginx python3 python3-dev python3-venv python3-pip

FOUNDRY_BIN=/home/mempolscan/.foundry/bin/anvil
if [[ ! -x "$FOUNDRY_BIN" ]]; then
  runuser -u mempolscan -- env HOME=/home/mempolscan bash -lc 'curl -L https://foundry.paradigm.xyz | bash && /home/mempolscan/.foundry/bin/foundryup'
fi

install -d -o mempolscan -g mempolscan -m 0750 "$ROOT/logs"
if [[ ! -f "$ROOT/.env" ]]; then
  install -o mempolscan -g mempolscan -m 0600 "$ROOT/.env.example" "$ROOT/.env"
fi

install -o root -g root -m 0644 "$ROOT/deploy/systemd/mempolscan-anvil.service" /etc/systemd/system/mempolscan-anvil.service
install -o root -g root -m 0644 "$ROOT/deploy/systemd/mempolscan.service" /etc/systemd/system/mempolscan.service
install -o root -g root -m 0644 "$ROOT/deploy/systemd/mempolscan-api.service" /etc/systemd/system/mempolscan-api.service
install -o root -g root -m 0644 "$ROOT/deploy/nginx-mempolscan.conf" /etc/nginx/sites-available/mempolscan
ln -sfn /etc/nginx/sites-available/mempolscan /etc/nginx/sites-enabled/mempolscan
rm -f /etc/nginx/sites-enabled/default
nginx -t

if [[ ! -d "$ROOT/venv" ]]; then
  runuser -u mempolscan -- python3 -m venv "$ROOT/venv"
fi
runuser -u mempolscan -- "$ROOT/venv/bin/pip" install --disable-pip-version-check -r "$ROOT/requirements.txt"
systemctl daemon-reload

