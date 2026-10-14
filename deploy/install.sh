#!/usr/bin/env bash
# Installs plant-server as a hardened systemd service on this machine.
# Run from the repository root after `./build.sh server`:
#   sudo deploy/install.sh
# Safe to re-run for upgrades; it never overwrites an existing config.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
BIN="$ROOT/server/target/release/plant-server"
CONF_DIR=/etc/plant-monitor
DATA_DIR=/var/lib/plant-monitor

log() { printf '\033[1;32m==>\033[0m %s\n' "$*"; }
die() { printf '\033[1;31merror:\033[0m %s\n' "$*" >&2; exit 1; }

[[ $EUID -eq 0 ]] || die "run as root (sudo $0)"
[[ -x "$BIN" ]] || die "build first: ./build.sh server"

if ! id plantmon >/dev/null 2>&1; then
  log "Creating system user plantmon"
  useradd --system --home-dir "$DATA_DIR" --shell /usr/sbin/nologin plantmon
fi

log "Installing binary to /usr/local/bin/plant-server"
install -m 0755 "$BIN" /usr/local/bin/plant-server
# Fedora: let systemd execute a binary placed in /usr/local/bin
command -v restorecon >/dev/null && restorecon /usr/local/bin/plant-server || true

install -d -m 0750 -o plantmon -g plantmon "$DATA_DIR"
install -d -m 0750 -o root -g plantmon "$CONF_DIR"

if [[ ! -f "$CONF_DIR/config.toml" ]]; then
  log "Creating $CONF_DIR/config.toml with a random device key"
  key="$(openssl rand -hex 24)"
  sed -e "s|^database = .*|database = \"$DATA_DIR/plants.db\"|" \
      -e "s|^device_key = .*|device_key = \"$key\"|" \
      "$ROOT/server/config.example.toml" > "$CONF_DIR/config.toml"
  echo "    device_key = $key   (put this in firmware/include/secrets.h)"
fi
chown root:plantmon "$CONF_DIR/config.toml"
chmod 0640 "$CONF_DIR/config.toml"

log "Installing systemd unit"
install -m 0644 "$ROOT/deploy/plant-server.service" /etc/systemd/system/plant-server.service
systemctl daemon-reload
systemctl enable plant-server
systemctl restart plant-server

log "Done. Next steps:"
cat <<EOF
  1. Review $CONF_DIR/config.toml (ntfy, public_url), then: systemctl restart plant-server
  2. Create your login:
       sudo -u plantmon plant-server -c $CONF_DIR/config.toml user-add <name>
  3. Set up Caddy for HTTPS: see deploy/Caddyfile
  4. Status / logs: systemctl status plant-server; journalctl -u plant-server -f
EOF
