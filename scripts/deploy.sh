#!/usr/bin/env bash
#
# One-command deploy, run ON the EC2 host (as root, e.g. via SSM or
# `sudo /opt/mempoolscanner/scripts/deploy.sh`). It is the single source of
# truth for "get this box to the latest main", so it does not matter which
# laptop the code was pushed from.
#
#   ./deploy.sh            # pull main, rebuild dashboard, restart services
#   ./deploy.sh --backend  # ALSO rebuild the Rust binary (slow; needs cargo)
#
# It is idempotent and fails loud: any step that fails aborts the script.
set -euo pipefail

REPO=/opt/mempoolscanner
APP_USER=mempolscan          # owns the repo + the rustup/cargo toolchain
BRANCH=main
BUILD_BACKEND=0
[ "${1:-}" = "--backend" ] && BUILD_BACKEND=1

echo "==> [1/5] Sync $REPO to origin/$BRANCH"
git config --system --add safe.directory "$REPO" 2>/dev/null || true
cd "$REPO"
git fetch --prune origin
# The box is often left on a detached HEAD by the first deploy; force it onto
# the branch so future pulls are trivial.
git checkout -B "$BRANCH" "origin/$BRANCH"
git reset --hard "origin/$BRANCH"
echo "    now at $(git rev-parse --short HEAD) — $(git log -1 --pretty=%s)"

echo "==> [2/5] Build the dashboard (web-dash -> dist)"
# Node lives in the system PATH (installed via NodeSource). npm ci is clean +
# reproducible from package-lock.json.
export PATH="/usr/bin:/usr/local/bin:$PATH"
cd "$REPO/web-dash"
npm ci --no-audit --no-fund
npm run build
echo "    dist built: $(ls dist/assets/*.js 2>/dev/null | xargs -n1 basename | tr '\n' ' ')"

if [ "$BUILD_BACKEND" = "1" ]; then
  echo "==> [3/5] Build the Rust backend (as $APP_USER)"
  # This script runs as root and just rewrote the tree via git, so target/ can
  # end up root-owned. cargo runs as APP_USER (the toolchain owner) and must be
  # able to write target/, so hand it back first — otherwise the build dies
  # instantly with "Permission denied" on a fingerprint file.
  chown -R "$APP_USER:$APP_USER" "$REPO/target" 2>/dev/null || true
  # cargo belongs to APP_USER; -H gives a login shell so its cargo env loads.
  # -j2 matches the 2 vCPUs; a 6G swapfile covers the LTO link's memory spike.
  sudo -u "$APP_USER" -H bash -lc "cd '$REPO' && cargo build --release -j2"
  install -m755 "$REPO/target/release/mempool-scanner" "$REPO/bin/mempool-scanner"
  echo "    backend binary updated"
else
  echo "==> [3/5] Skipping Rust backend rebuild (pass --backend to include it)"
fi

echo "==> [4/5] Reload nginx + restart app services"
nginx -t && systemctl reload nginx
systemctl restart mempolscan.service mempolscan-api.service || true
systemctl restart mempolscan.service 2>/dev/null || true

echo "==> [5/5] Smoke test"
sleep 2
for ep in /api/opportunities /api/liquidations; do
  code=$(curl -s -m 8 -o /dev/null -w '%{http_code}' "http://127.0.0.1:80$ep" || echo "ERR")
  echo "    $ep -> HTTP $code"
done
echo "==> Deploy complete."
