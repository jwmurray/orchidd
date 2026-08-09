#!/usr/bin/env bash
#
# Install orchidd as a systemd service on Linux.
#
#   sudo bash deploy/install-linux.sh
#
# Builds the binary, creates a dedicated unprivileged `orchidd` system user,
# installs the unit and enables it so the service comes back after a reboot.
#
# Requirements: rustup/cargo and a C toolchain (rusqlite builds SQLite from
# source). On Debian/Ubuntu: sudo apt install build-essential
set -euo pipefail

SERVICE="orchidd"
PORT="${ORCHIDD_PORT:-8000}"
RUN_USER="${ORCHIDD_USER:-orchidd}"
BIN="/usr/local/bin/orchidd"
DATADIR="/var/lib/orchidd"
DB="$DATADIR/orchidd.db"
UNIT="/etc/systemd/system/${SERVICE}.service"

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$REPO"

if [[ $EUID -ne 0 ]]; then
    echo "error: run with sudo (installs a system unit)" >&2
    exit 1
fi

# cargo usually belongs to the invoking user, not root, so build as them.
BUILD_USER="${SUDO_USER:-root}"
if ! sudo -u "$BUILD_USER" bash -lc 'command -v cargo' >/dev/null 2>&1; then
    echo "error: cargo not found for user $BUILD_USER. Install Rust: https://rustup.rs" >&2
    exit 1
fi

echo "==> Building release binary (as $BUILD_USER)"
sudo -u "$BUILD_USER" bash -lc "cd '$REPO' && cargo build --release"

if ! id -u "$RUN_USER" >/dev/null 2>&1; then
    echo "==> Creating system user $RUN_USER"
    useradd --system --no-create-home --shell /usr/sbin/nologin "$RUN_USER"
fi
GROUP="$(id -gn "$RUN_USER")"

echo "==> Installing to $BIN"
install -m 0755 target/release/orchidd "$BIN"
mkdir -p "$DATADIR"
chown "$RUN_USER:$GROUP" "$DATADIR"

echo "==> Writing $UNIT"
sed -e "s|@BIN@|$BIN|g" \
    -e "s|@DB@|$DB|g" \
    -e "s|@PORT@|$PORT|g" \
    -e "s|@DATADIR@|$DATADIR|g" \
    -e "s|@USER@|$RUN_USER|g" \
    -e "s|@GROUP@|$GROUP|g" \
    deploy/systemd/orchidd.service > "$UNIT"
chmod 0644 "$UNIT"

echo "==> Enabling and starting"
systemctl daemon-reload
systemctl enable "$SERVICE"
systemctl restart "$SERVICE"

sleep 2
if systemctl is-active --quiet "$SERVICE"; then
    echo
    echo "orchidd is running."
    echo "  database: $DB"
    echo "  status:   systemctl status $SERVICE"
    echo "  logs:     journalctl -u $SERVICE -f"
    echo "  stop:     sudo systemctl stop $SERVICE"
    echo
    systemctl --no-pager --lines=6 status "$SERVICE" || true
else
    echo "error: service failed to start" >&2
    journalctl -u "$SERVICE" --no-pager --lines=20 >&2 || true
    exit 1
fi
