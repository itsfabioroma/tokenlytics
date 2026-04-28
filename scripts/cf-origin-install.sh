#!/usr/bin/env bash
# Install Cloudflare Origin Certificate for tokenlytics.ultracontext.com.
# expects ~/cf-cert.crt and ~/cf-cert.key to already exist (you paste them in).
# moves them to /etc/ssl/cf-origin/ with correct perms, rewrites the nginx
# site to terminate HTTPS using those, and reloads nginx.
#
# usage:  sudo bash cf-origin-install.sh

set -euo pipefail

DOMAIN="tokenlytics.ultracontext.com"
PORT="${PORT:-6969}"
RUN_USER="${SUDO_USER:-$(whoami)}"
HOME_DIR="$(getent passwd "$RUN_USER" | cut -d: -f6)"

CERT_SRC="$HOME_DIR/cf-cert.crt"
KEY_SRC="$HOME_DIR/cf-cert.key"

if [[ "$EUID" -ne 0 ]]; then
  echo "run with sudo: sudo bash $0" >&2
  exit 1
fi

if [[ ! -f "$CERT_SRC" || ! -f "$KEY_SRC" ]]; then
  echo "missing cert or key. paste them at $CERT_SRC and $KEY_SRC first." >&2
  exit 1
fi

# 1. validate
echo ">> validating cert + key"
openssl x509 -in "$CERT_SRC" -noout -subject -issuer -dates
openssl pkey -in "$KEY_SRC" -noout && echo "   key: ok"

# 2. install to /etc/ssl/cf-origin/ (root-owned, key is 600)
echo ">> moving to /etc/ssl/cf-origin/"
mkdir -p /etc/ssl/cf-origin
mv "$CERT_SRC" /etc/ssl/cf-origin/tokenlytics.crt
mv "$KEY_SRC"  /etc/ssl/cf-origin/tokenlytics.key
chown root:root /etc/ssl/cf-origin/tokenlytics.crt /etc/ssl/cf-origin/tokenlytics.key
chmod 644 /etc/ssl/cf-origin/tokenlytics.crt
chmod 600 /etc/ssl/cf-origin/tokenlytics.key

# 3. rewrite nginx site with HTTPS listener
echo ">> updating nginx site"
cat > /etc/nginx/sites-available/tokenlytics <<NGINX
# HTTP → HTTPS redirect (in case CF ever talks to origin on :80)
server {
    listen 80;
    listen [::]:80;
    server_name $DOMAIN;
    return 301 https://\$host\$request_uri;
}

server {
    listen 443 ssl http2;
    listen [::]:443 ssl http2;
    server_name $DOMAIN;

    ssl_certificate     /etc/ssl/cf-origin/tokenlytics.crt;
    ssl_certificate_key /etc/ssl/cf-origin/tokenlytics.key;
    ssl_protocols TLSv1.2 TLSv1.3;

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

if ! nginx -t 2>&1 | grep -q "successful"; then
  nginx -t
  exit 1
fi
systemctl reload nginx
echo "   nginx: reloaded"

echo
echo "================================================="
echo "✓ origin certificate installed"
echo "================================================="
echo "  Cloudflare SSL/TLS mode can now be: Full or Full (strict)"
echo "  test:  curl https://$DOMAIN/api/version"
