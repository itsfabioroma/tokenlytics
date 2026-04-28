#!/usr/bin/env bash
# tokenlytics VPS bootstrap — installs Rust + tokenlytics + systemd + nginx site.
# assumes nginx is already on the box (typical Ubuntu VPS w/ certbot setup).
# pairs with Cloudflare proxy in Flexible SSL mode (or Full w/ Origin Cert).
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
echo "   port   : $PORT (loopback only, fronted by nginx)"
echo "   user   : $RUN_USER"
echo

# 1. base build deps (Rust needs these for rusqlite bundled + openssl-sys etc)
echo ">> apt: build deps"
apt-get update -qq
apt-get install -y -qq curl build-essential pkg-config libssl-dev ca-certificates

# 2. Rust (as the run user, not root)
if ! sudo -u "$RUN_USER" bash -lc 'command -v cargo' >/dev/null 2>&1; then
  echo ">> installing Rust for $RUN_USER"
  sudo -u "$RUN_USER" sh -c "curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --default-toolchain stable"
fi

# 3. build + install tokenlytics
echo ">> cargo install tokenlytics (~3-5min first time, faster on re-runs)"
sudo -u "$RUN_USER" bash -lc "source $HOME_DIR/.cargo/env && cargo install --git https://github.com/ultracontext/tokenlytics --force --quiet"

BIN_PATH="$HOME_DIR/.cargo/bin/tokenlytics"
if [[ ! -x "$BIN_PATH" ]]; then
  echo "tokenlytics binary not found at $BIN_PATH" >&2
  exit 1
fi

# 4. systemd unit — daemon under the run user, restart on failure
echo ">> writing systemd unit"
cat > /etc/systemd/system/tokenlytics.service <<UNIT
[Unit]
Description=tokenlytics global leaderboard
After=network.target

[Service]
Type=simple
User=$RUN_USER
Environment="LEADERBOARD=1"
Environment="LEADERBOARD_SERVER=1"
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

# 5. nginx site (HTTP only — Cloudflare in Flexible mode terminates TLS)
echo ">> writing nginx site"
cat > /etc/nginx/sites-available/tokenlytics <<NGINX
server {
    listen 80;
    listen [::]:80;
    server_name $DOMAIN;

    # SSE-friendly: no buffering, long read timeout
    proxy_buffering off;
    proxy_read_timeout 24h;

    location / {
        proxy_pass http://127.0.0.1:$PORT;
        proxy_http_version 1.1;
        proxy_set_header Host              \$host;
        proxy_set_header X-Real-IP         \$remote_addr;
        proxy_set_header X-Forwarded-For   \$proxy_add_x_forwarded_for;
        proxy_set_header X-Forwarded-Proto \$scheme;
    }
}
NGINX

ln -sf /etc/nginx/sites-available/tokenlytics /etc/nginx/sites-enabled/tokenlytics

if ! nginx -t 2>&1 | grep -q "successful"; then
  echo "nginx config test failed:" >&2
  nginx -t
  exit 1
fi
systemctl reload nginx
echo "   nginx: reloaded"

echo
echo "================================================="
echo "✓ tokenlytics deployed"
echo "================================================="
echo
echo "  domain   : https://$DOMAIN  (TLS via Cloudflare)"
echo "  origin   : nginx :80 -> tokenlytics :$PORT (loopback)"
echo "  logs     : journalctl -u tokenlytics -f"
echo "  status   : systemctl status tokenlytics"
echo "  upgrade  : sudo bash $0 $DOMAIN"
echo
echo "Cloudflare SSL/TLS mode for ultracontext.com:"
echo "  Flexible        (CF<->client HTTPS, CF<->origin HTTP)  ← simplest"
echo "  Full / Strict   (need Cloudflare Origin Certificate on this VPS)"
echo
echo "smoke test:"
echo "  curl https://$DOMAIN/api/version"
