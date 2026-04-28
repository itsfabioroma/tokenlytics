#!/usr/bin/env bash
# tokenlytics VPS bootstrap — installs Rust + tokenlytics + systemd unit + Caddy reverse proxy.
#
# usage:  sudo bash vps-deploy.sh [DOMAIN]
# example: sudo bash vps-deploy.sh tokenlytics.ultracontext.com
#
# idempotent — safe to re-run for upgrades.

set -euo pipefail

DOMAIN="${1:-tokenlytics.ultracontext.com}"
PORT="${PORT:-6969}"
RUN_USER="${SUDO_USER:-$(whoami)}"
HOME_DIR="$(getent passwd "$RUN_USER" | cut -d: -f6)"

if [[ "$EUID" -ne 0 ]]; then
  echo "run with sudo: sudo bash $0 $DOMAIN" >&2
  exit 1
fi

echo ">> tokenlytics deploy"
echo "   domain : $DOMAIN"
echo "   port   : $PORT (localhost only, fronted by Caddy)"
echo "   user   : $RUN_USER"
echo

# 1. system deps
echo ">> apt update + base deps"
apt-get update -qq
apt-get install -y -qq curl build-essential pkg-config libssl-dev \
  debian-keyring debian-archive-keyring apt-transport-https ca-certificates gnupg

# 2. Caddy (reverse proxy + auto TLS via Let's Encrypt)
if ! command -v caddy >/dev/null 2>&1; then
  echo ">> installing Caddy"
  curl -1sLf 'https://dl.cloudsmith.io/public/caddy/stable/gpg.key' \
    | gpg --dearmor -o /usr/share/keyrings/caddy-stable-archive-keyring.gpg
  curl -1sLf 'https://dl.cloudsmith.io/public/caddy/stable/debian.deb.txt' \
    > /etc/apt/sources.list.d/caddy-stable.list
  apt-get update -qq
  apt-get install -y -qq caddy
fi

# 3. Rust toolchain (as the run user, not root)
if ! sudo -u "$RUN_USER" bash -c 'command -v cargo' >/dev/null 2>&1; then
  echo ">> installing Rust for $RUN_USER"
  sudo -u "$RUN_USER" sh -c "curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --default-toolchain stable"
fi

# 4. build + install tokenlytics
echo ">> cargo install tokenlytics (~3-5min first time, faster on re-runs)"
sudo -u "$RUN_USER" bash -lc "source $HOME_DIR/.cargo/env && cargo install --git https://github.com/ultracontext/tokenlytics --force --quiet"

BIN_PATH="$HOME_DIR/.cargo/bin/tokenlytics"
if [[ ! -x "$BIN_PATH" ]]; then
  echo "tokenlytics binary not found at $BIN_PATH" >&2
  exit 1
fi

# 5. systemd unit — daemon under the run user, restart on failure
echo ">> writing systemd unit"
cat > /etc/systemd/system/tokenlytics.service <<UNIT
[Unit]
Description=tokenlytics global leaderboard
After=network.target

[Service]
Type=simple
User=$RUN_USER
Environment="LEADERBOARD=1"
Environment="TOKENLYTICS_NAME=server"
Environment="PORT=$PORT"
ExecStart=$BIN_PATH serve --no-setup
Restart=always
RestartSec=3

[Install]
WantedBy=multi-user.target
UNIT

systemctl daemon-reload
systemctl enable --now tokenlytics
sleep 2
if ! systemctl is-active --quiet tokenlytics; then
  echo "tokenlytics failed to start" >&2
  journalctl -u tokenlytics -n 30 --no-pager
  exit 1
fi
echo "   tokenlytics: active"

# 6. Caddy reverse proxy
echo ">> writing Caddyfile"
cat > /etc/caddy/Caddyfile <<CADDY
$DOMAIN {
    reverse_proxy localhost:$PORT
}
CADDY

systemctl restart caddy
sleep 2
if ! systemctl is-active --quiet caddy; then
  echo "caddy failed to start" >&2
  journalctl -u caddy -n 30 --no-pager
  exit 1
fi
echo "   caddy: active"

# 7. summary
PUBLIC_IP="$(curl -fsS https://api.ipify.org 2>/dev/null || echo 'unknown')"

echo
echo "================================================="
echo "✓ tokenlytics deployed"
echo "================================================="
echo
echo "  url     : https://$DOMAIN"
echo "  origin  : http://localhost:$PORT (firewalled, only via Caddy)"
echo "  logs    : journalctl -u tokenlytics -f"
echo "  status  : systemctl status tokenlytics"
echo "  upgrade : re-run this script (cargo install --force)"
echo
echo "DNS check — add this A record if you haven't:"
echo "  $DOMAIN  ->  $PUBLIC_IP"
echo
echo "smoke test:"
echo "  curl https://$DOMAIN/api/version"
