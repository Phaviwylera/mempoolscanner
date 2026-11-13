#!/usr/bin/env bash
# Post-deploy verification for the read-only observer.
#
# Asserts the quality endpoint answers with a banner state, that the scanner
# service is running, and that at least one new observation is appended within
# the deadline. A failure here means the deploy is live but not proven healthy;
# nothing is rolled back automatically.
#
# Usage: verify_release.sh [deadline-seconds]
#
# Every path is overridable so the script can be exercised off-instance:
#   MEMPOLSCAN_ROOT, MEMPOLSCAN_UNIT, MEMPOLSCAN_QUALITY_URL,
#   MEMPOLSCAN_HITS_DEADLINE, MEMPOLSCAN_SKIP_SERVICE_CHECK
set -euo pipefail

ROOT="${MEMPOLSCAN_ROOT:-/opt/mempoolscanner}"
UNIT="${MEMPOLSCAN_UNIT:-mempolscan.service}"
QUALITY_URL="${MEMPOLSCAN_QUALITY_URL:-http://127.0.0.1/api/quality?limit=1}"
DEADLINE="${1:-${MEMPOLSCAN_HITS_DEADLINE:-180}}"
LOG="$ROOT/logs/hits.jsonl"
SKIP_SERVICE="${MEMPOLSCAN_SKIP_SERVICE_CHECK:-0}"

record_count() {
  if [[ -f "$LOG" ]]; then
    # grep -c prints 0 and exits 1 when there is nothing to count.
    grep -c . "$LOG" || true
  else
    echo 0
  fi
}

service_active() {
  if [[ "$SKIP_SERVICE" == "1" ]]; then
    return 0
  fi
  systemctl is-active --quiet "$UNIT"
}

dump_service() {
  if [[ "$SKIP_SERVICE" == "1" ]]; then
    return 0
  fi
  journalctl -u "$UNIT" -n 60 --no-pager >&2 || true
}

echo "== quality endpoint: $QUALITY_URL"
if ! quality="$(curl --silent --show-error --fail --max-time 10 "$QUALITY_URL")"; then
  echo "FAILED: /api/quality is not reachable, so the proxy location or the API is missing" >&2
  exit 1
fi
if ! echo "$quality" | jq -e . >/dev/null 2>&1; then
  echo "FAILED: /api/quality did not return JSON" >&2
  exit 1
fi

state="$(echo "$quality" | jq -r '.banner.state // empty')"
case "$state" in
  green | amber | red | unknown) ;;
  "")
    echo "FAILED: /api/quality returned no banner.state" >&2
    echo "$quality" | jq -c . >&2 || true
    exit 1
    ;;
  *)
    echo "FAILED: unexpected banner.state '$state'" >&2
    exit 1
    ;;
esac
echo "banner state: $state"
echo "$quality" | jq -c '{state: .banner.state, text: .banner.text, window: .window, rollup_available: .rollup.available}' || true

echo "== scanner service: $UNIT"
if ! service_active; then
  echo "FAILED: $UNIT is not active" >&2
  dump_service
  exit 1
fi

baseline="$(record_count)"
echo "records before waiting: $baseline"
echo "== waiting up to ${DEADLINE}s for a new observation"
deadline=$((SECONDS + DEADLINE))
count="$baseline"
while ((SECONDS < deadline)); do
  count="$(record_count)"
  if ((count > baseline)); then
    break
  fi
  if ! service_active; then
    echo "FAILED: $UNIT stopped while waiting for an observation" >&2
    dump_service
    exit 1
  fi
  sleep 5
done

if ((count <= baseline)); then
  echo "FAILED: no new observation in ${DEADLINE}s (record count stayed at ${baseline})" >&2
  curl --silent --show-error --max-time 10 "$QUALITY_URL" | jq -c '.banner' >&2 || true
  dump_service
  exit 1
fi

echo "new observation recorded: ${baseline} -> ${count}"
tail -n 1 "$LOG" | jq -c '{label, ts, fork: (.fork // {}), sim: (.sim // {})}' || true
echo "VERIFY OK"
