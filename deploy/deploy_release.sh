#!/usr/bin/env bash
set -euo pipefail

ROOT=/opt/mempoolscanner
systemctl enable mempolscan-anvil.service
systemctl restart mempolscan-anvil.service

ready=false
for _ in $(seq 1 90); do
  if curl --silent --show-error --fail http://127.0.0.1:8545 \
      -H 'Content-Type: application/json' \
      --data '{"jsonrpc":"2.0","method":"eth_chainId","params":[],"id":1}' >/dev/null 2>&1; then
    ready=true
    break
  fi
  sleep 2
done
if [[ "$ready" != true ]]; then
  journalctl -u mempolscan-anvil.service -n 80 --no-pager
  exit 1
fi

systemctl enable mempolscan.service
systemctl restart mempolscan.service
sleep 2
if ! systemctl is-active --quiet mempolscan.service; then
  journalctl -u mempolscan.service -n 80 --no-pager
  exit 1
fi
systemctl --no-pager --full status mempolscan-anvil.service mempolscan.service

