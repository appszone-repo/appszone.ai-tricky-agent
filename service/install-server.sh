#!/usr/bin/env bash
# Run ON THE SERVER, as root, from the project folder you copied there (it needs
# dist/tricky-agent-linux-x86_64, config.toml, tricky-agent.env and service/).
#   sudo ./service/install-server.sh              install or update, enable at boot, start
#   sudo ./service/install-server.sh uninstall    stop and remove the service (keeps /etc/tricky-agent and the user)
set -euo pipefail

[ "$(id -u)" = 0 ] || { echo "run as root: sudo $0 ${1:-}"; exit 1; }
cd "$(dirname "$0")/.."

SVC_USER=tricky
HOME_DIR=/var/lib/tricky-agent
UNIT=tricky-agent.service
BIN=dist/tricky-agent-linux-x86_64

case "${1:-install}" in
  install)
    [ -x "$BIN" ] || { echo "missing $BIN"; exit 1; }
    [ -f tricky-agent.env ] || [ -f /etc/tricky-agent/env ] || { echo "missing tricky-agent.env (a line: TRICKY_API_KEY=<long random secret>)"; exit 1; }
    "./$BIN" --version >/dev/null || { echo "the binary does not run on this machine (wrong CPU architecture?)"; exit 1; }

    id -u "$SVC_USER" >/dev/null 2>&1 \
      || useradd --system --home-dir "$HOME_DIR" --create-home --shell /usr/sbin/nologin "$SVC_USER"

    install -d -m 755 /opt/tricky-agent /etc/tricky-agent
    install -m 755 "$BIN" /opt/tricky-agent/tricky-agent        # replaces the file, safe while running
    # config and key are never overwritten on update
    [ -f /etc/tricky-agent/config.toml ] || install -m 644 config.toml /etc/tricky-agent/config.toml
    [ -f /etc/tricky-agent/env ] || install -m 640 -o root -g "$SVC_USER" tricky-agent.env /etc/tricky-agent/env
    grep -q '^TRICKY_API_KEY=.\{16,\}' /etc/tricky-agent/env \
      || { echo "TRICKY_API_KEY in /etc/tricky-agent/env is missing or shorter than 16 characters"; exit 1; }

    install -m 644 "service/$UNIT" "/etc/systemd/system/$UNIT"
    systemctl daemon-reload
    systemctl enable "$UNIT"
    systemctl restart "$UNIT"
    sleep 1
    systemctl --no-pager status "$UNIT" | head -12 || true

    PORT=$(sed -n 's/^port *= *\([0-9]*\).*/\1/p' /etc/tricky-agent/config.toml | head -1); PORT=${PORT:-8010}
    if curl -fsS "localhost:$PORT/health" >/dev/null 2>&1; then
      echo "health check: ok (port $PORT)"
    else
      echo "health check FAILED: journalctl -u tricky-agent -n 30"; exit 1
    fi
    cat <<EOF

Next, as the service user (the agents are not installed or logged in yet for it):
  sudo -u $SVC_USER -H agent login        # Cursor Agent: prints a link; or put CURSOR_API_KEY=... in /etc/tricky-agent/env
  sudo -u $SVC_USER -H cmd                # Command Code: log in the same way you did on your own machine
  sudo -u $SVC_USER -H env PATH=$HOME_DIR/.local/bin:/usr/local/bin:/usr/bin:/bin /opt/tricky-agent/tricky-agent --check agent
  sudo -u $SVC_USER -H env PATH=$HOME_DIR/.local/bin:/usr/local/bin:/usr/bin:/bin /opt/tricky-agent/tricky-agent --check cmd
Startup banner (shows "NOT FOUND in PATH" for a missing agent):  journalctl -u tricky-agent -n 20
EOF
    ;;
  uninstall)
    systemctl disable --now "$UNIT" 2>/dev/null || true
    rm -f "/etc/systemd/system/$UNIT" /opt/tricky-agent/tricky-agent
    systemctl daemon-reload
    echo "removed the service and binary; kept /etc/tricky-agent and the '$SVC_USER' user ($HOME_DIR)"
    ;;
  *)
    echo "usage: $0 [install|uninstall]"; exit 2 ;;
esac
