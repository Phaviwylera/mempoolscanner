#!/usr/bin/env bash
set -euo pipefail

nginx -t
systemctl enable --now nginx

# Retire the legacy external Anvil daemon if present: simulations now run in-process via Rust Revm.
systemctl disable --now mempolscan-anvil.service 2>/dev/null || true
rm -f /etc/systemd/system/mempolscan-anvil.service
systemctl daemon-reload

systemctl reset-failed mempolscan-api.service || true
systemctl enable --now mempolscan-api.service
systemctl restart mempolscan-api.service
systemctl reload nginx

if ! curl --retry 10 --retry-delay 1 --retry-all-errors --silent --show-error --fail http://127.0.0.1/ >/dev/null; then
  journalctl -u nginx.service -n 80 --no-pager
  exit 1
fi

if ! curl --retry 10 --retry-delay 1 --retry-all-errors --silent --show-error --fail 'http://127.0.0.1/api/hits?limit=1' >/dev/null; then
  journalctl -u mempolscan-api.service -n 80 --no-pager
  exit 1
fi

if ! curl --retry 10 --retry-delay 1 --retry-all-errors --silent --show-error --fail 'http://127.0.0.1/api/quality?limit=1' >/dev/null; then
  journalctl -u mempolscan-api.service -n 80 --no-pager
  exit 1
fi

if ! curl --retry 10 --retry-delay 1 --retry-all-errors --silent --show-error --fail 'http://127.0.0.1/liquidations.html' >/dev/null; then
  journalctl -u nginx.service -n 80 --no-pager
  exit 1
fi

if ! curl --retry 10 --retry-delay 1 --retry-all-errors --silent --show-error --fail 'http://127.0.0.1/api/liquidations?limit=5' >/dev/null; then
  journalctl -u mempolscan-api.service -n 80 --no-pager
  exit 1
fi

systemctl reset-failed mempolscan.service || true
systemctl enable mempolscan.service
systemctl restart mempolscan.service
sleep 4
if ! systemctl is-active --quiet mempolscan.service; then
  journalctl -u mempolscan.service -n 80 --no-pager
  exit 1
fi

systemctl --no-pager --full status nginx.service mempolscan-api.service mempolscan.service
