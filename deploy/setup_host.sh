#!/usr/bin/env bash
set -euo pipefail

ROOT=/opt/mempoolscanner
REPO_SLUG="Phaviwylera/mempoolscanner"

id -u mempolscan >/dev/null 2>&1 || useradd --system --create-home --home-dir /home/mempolscan --shell /bin/bash mempolscan

export DEBIAN_FRONTEND=noninteractive
apt-get update
apt-get install -y --no-install-recommends build-essential pkg-config libssl-dev ca-certificates curl git jq nginx python3

install -d -o mempolscan -g mempolscan -m 0750 "$ROOT/logs"
install -d -o root -g root -m 0755 "$ROOT/bin"

if [[ ! -f "$ROOT/.env" ]]; then
  install -o mempolscan -g mempolscan -m 0600 "$ROOT/.env.example" "$ROOT/.env"
fi

SHA="$(git -C "$ROOT" rev-parse HEAD 2>/dev/null || echo unknown)"
BIN="$ROOT/bin/mempool-scanner"

need_install=true
if [[ -x "$BIN" ]] && "$BIN" --version 2>/dev/null | grep -q "$SHA"; then
  need_install=false
fi

if [[ "$need_install" == true ]]; then
  RELEASE_BASE="https://github.com/${REPO_SLUG}/releases/download/build-${SHA}"
  TMP_DIR="$(mktemp -d)"
  trap 'rm -rf "$TMP_DIR"' EXIT

  if [[ "$SHA" != "unknown" ]] && \
     curl -fsSL "${RELEASE_BASE}/mempool-scanner-linux-x86_64" -o "${TMP_DIR}/mempool-scanner" && \
     curl -fsSL "${RELEASE_BASE}/mempool-scanner-linux-x86_64.sha256" -o "${TMP_DIR}/mempool-scanner.sha256"; then
    (cd "$TMP_DIR" && sha256sum -c mempool-scanner.sha256)
    install -o root -g root -m 0755 "${TMP_DIR}/mempool-scanner" "$BIN"
    echo "Installed prebuilt mempool-scanner for ${SHA}"
  else
    echo "No prebuilt release artifact for ${SHA}; building from source with Cargo..."
    CARGO_BIN="/home/mempolscan/.cargo/bin/cargo"
    if [[ ! -x "$CARGO_BIN" ]]; then
      runuser -u mempolscan -- env HOME=/home/mempolscan bash -lc \
        'curl --proto "=https" --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal'
    fi
    runuser -u mempolscan -- env HOME=/home/mempolscan PATH="/home/mempolscan/.cargo/bin:$PATH" \
      CARGO_BUILD_JOBS=2 cargo build --manifest-path "$ROOT/Cargo.toml" --release
    install -o root -g root -m 0755 "$ROOT/target/release/mempool-scanner" "$BIN"
  fi
fi

"$BIN" --version

install -o root -g root -m 0644 "$ROOT/deploy/systemd/mempolscan.service" /etc/systemd/system/mempolscan.service
install -o root -g root -m 0644 "$ROOT/deploy/systemd/mempolscan-api.service" /etc/systemd/system/mempolscan-api.service
install -o root -g root -m 0644 "$ROOT/deploy/nginx-mempolscan.conf" /etc/nginx/sites-available/mempolscan
chmod 0755 "$ROOT" "$ROOT/web"
ln -sfn /etc/nginx/sites-available/mempolscan /etc/nginx/sites-enabled/mempolscan
rm -f /etc/nginx/sites-enabled/default
nginx -t

systemctl daemon-reload
