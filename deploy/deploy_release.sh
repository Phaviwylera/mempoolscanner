#!/usr/bin/env bash
set -euo pipefail

nginx -t
systemctl enable --now nginx

# Retire the legacy external Anvil daemon if present: simulations now run in-process via Rust Revm.
if systemctl list-unit-files --type=service | grep -q '^mempolscan-anvil.service'; then
  systemctl disable --now mempolscan-anvil.service || true
fi

systemctl enable --now mempolscan-api.service
systemctl restart mempolscan-api.service
systemctl reload nginx

if ! curl --silent --show-error --fail http://127.0.0.1/ >/dev/null; then
  journalctl -u nginx.service -n 80 --no-pager
  exit 1
fi

if ! curl --silent --show-error --fail 'http://127.0.0.1/api/hits?limit=1' >/dev/null; then
  journalctl -u mempolscan-api.service -n 80 --no-pager
  exit 1
fi

if ! curl --silent --show-error --fail 'http://127.0.0.1/api/quality?limit=1' >/dev/null; then
  journalctl -u mempolscan-api.service -n 80 --no-pager
  exit 1
fi

systemctl enable mempolscan.service
systemctl restart mempolscan.service
sleep 3
if ! systemctl is-active --quiet mempolscan.service; then
  journalctl -u mempolscan.service -n 80 --no-pager
  exit 1
fi

systemctl --no-pager --full status nginx.service mempolscan-api.service mempolscan.service
