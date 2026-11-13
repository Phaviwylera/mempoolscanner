#!/usr/bin/env bash
set -euxo pipefail
exec > >(tee -a /var/log/mempolscan-bootstrap.log | logger -t mempolscan-bootstrap -s 2>/dev/console) 2>&1
export DEBIAN_FRONTEND=noninteractive

apt-get update
apt-get install -y --no-install-recommends ca-certificates curl git jq python3
id -u mempolscan >/dev/null 2>&1 || useradd --system --create-home --home-dir /home/mempolscan --shell /bin/bash mempolscan
install -d -o mempolscan -g mempolscan -m 0750 /opt/mempoolscanner

if ! systemctl list-unit-files --type=service | grep -q '^amazon-ssm-agent.service'; then
  command -v snap >/dev/null 2>&1 || apt-get install -y snapd
  snap install amazon-ssm-agent --classic
  systemctl enable --now snap.amazon-ssm-agent.amazon-ssm-agent.service
else
  systemctl enable --now amazon-ssm-agent.service
fi

cat >/usr/local/sbin/mempolscan-deploy <<'DEPLOY_SCRIPT'
#!/usr/bin/env bash
set -euo pipefail

sha="${1:?commit SHA required}"
[[ "$sha" =~ ^[0-9a-f]{40}$ ]] || { echo "Invalid commit SHA" >&2; exit 2; }
for _ in $(seq 1 120); do
  [[ -f /var/lib/mempolscan/bootstrap-ready ]] && break
  sleep 2
done
[[ -f /var/lib/mempolscan/bootstrap-ready ]] || { echo "Host bootstrap did not complete" >&2; exit 1; }

repo=/opt/mempoolscanner
if [[ ! -d "$repo/.git" ]]; then
  runuser -u mempolscan -- git clone --no-checkout https://github.com/Phaviwylera/mempoolscanner.git "$repo"
fi
runuser -u mempolscan -- git -C "$repo" fetch --depth=1 origin "$sha"
runuser -u mempolscan -- git -C "$repo" checkout --detach --force "$sha"
bash "$repo/deploy/setup_host.sh"
bash "$repo/deploy/deploy_release.sh"
DEPLOY_SCRIPT
chmod 0755 /usr/local/sbin/mempolscan-deploy
install -d -m 0755 /var/lib/mempolscan
touch /var/lib/mempolscan/bootstrap-ready
