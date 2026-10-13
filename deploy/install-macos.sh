#!/usr/bin/env bash
#
# Install orchidd as a launchd service on macOS.
#
#   bash deploy/install-macos.sh              # per-user agent (no sudo)
#   bash deploy/install-macos.sh --system     # system daemon (sudo, starts at boot)
#
# The agent form needs no administrator rights but only starts once someone logs
# in. The daemon form starts at boot even with nobody logged in, which is what
# you want on a machine that is only ever reached over the network — it just
# needs sudo to install.
#
# Re-running is safe: it rebuilds, replaces the binary and restarts the service.
#
# Optional, read at install time and written into the plist:
#   ORCHIDD_PORT          port to serve on (default 8000)
#   ORCHIDD_RELAY_ALIVE   heartbeat file the iMessage relay touches; orchidd
#                         reports its age in /api/status so orchid-watch can
#                         tell John when the relay is down. Left out when unset.
#
#   ORCHIDD_RELAY_ALIVE=/Users/jmurray/.config/firm/orchid-relay.alive \
#       bash deploy/install-macos.sh --system
set -euo pipefail

LABEL="com.jwmurray.orchidd"
PORT="${ORCHIDD_PORT:-8000}"
RELAY_ALIVE="${ORCHIDD_RELAY_ALIVE:-}"
SYSTEM=0
[[ "${1:-}" == "--system" ]] && SYSTEM=1

# The path goes into sed and XML: absolute, and nothing either would mangle.
if [[ -n "$RELAY_ALIVE" ]]; then
    if [[ "$RELAY_ALIVE" != /* || "$RELAY_ALIVE" == *[\|\&\<\>\\]* ]]; then
        echo "error: ORCHIDD_RELAY_ALIVE must be an absolute path without | & < > \\" >&2
        exit 2
    fi
    RELAY_ENV="<key>ORCHIDD_RELAY_ALIVE</key><string>$RELAY_ALIVE</string>"
fi

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$REPO"

if ! command -v cargo >/dev/null 2>&1; then
    echo "error: cargo not found. Install Rust from https://rustup.rs" >&2
    exit 1
fi

echo "==> Building release binary"
cargo build --release

if [[ $SYSTEM -eq 1 ]]; then
    BIN="/usr/local/bin/orchidd"
    DATADIR="/usr/local/var/orchidd"
    LOG="/usr/local/var/log/orchidd.log"
    PLIST="/Library/LaunchDaemons/${LABEL}.plist"
    SUDO="sudo"
    DOMAIN="system"
else
    BIN="$HOME/.local/bin/orchidd"
    DATADIR="$HOME/Library/Application Support/orchidd"
    LOG="$HOME/Library/Logs/orchidd.log"
    PLIST="$HOME/Library/LaunchAgents/${LABEL}.plist"
    SUDO=""
    DOMAIN="gui/$(id -u)"
fi
DB="$DATADIR/orchidd.db"

echo "==> Installing to $BIN"
$SUDO mkdir -p "$(dirname "$BIN")" "$DATADIR" "$(dirname "$LOG")" "$(dirname "$PLIST")"
$SUDO cp target/release/orchidd "$BIN"

# Stop an existing instance before swapping the plist, so the port is free.
# Querying the system domain requires root, so these go through $SUDO too.
if $SUDO launchctl print "$DOMAIN/$LABEL" >/dev/null 2>&1; then
    echo "==> Stopping running service"
    $SUDO launchctl bootout "$DOMAIN/$LABEL" 2>/dev/null || true
fi

echo "==> Writing $PLIST"
if [[ -n "$RELAY_ALIVE" ]]; then
    RELAY_SED="s|@RELAY_ENV@|$RELAY_ENV|"
else
    RELAY_SED="/@RELAY_ENV@/d"
fi
sed -e "s|@BIN@|$BIN|g" \
    -e "s|@DB@|$DB|g" \
    -e "s|@LOG@|$LOG|g" \
    -e "s|@PORT@|$PORT|g" \
    -e "s|@DATADIR@|$DATADIR|g" \
    -e "$RELAY_SED" \
    "deploy/launchd/${LABEL}.plist" | $SUDO tee "$PLIST" >/dev/null

if [[ $SYSTEM -eq 1 ]]; then
    # launchd refuses to load a daemon that is group- or world-writable.
    $SUDO chown root:wheel "$PLIST"
    $SUDO chmod 644 "$PLIST"
fi

echo "==> Starting service"
$SUDO launchctl bootstrap "$DOMAIN" "$PLIST"
$SUDO launchctl enable "$DOMAIN/$LABEL"

sleep 2
if $SUDO launchctl print "$DOMAIN/$LABEL" >/dev/null 2>&1; then
    echo
    echo "orchidd is running."
    echo "  database: $DB"
    echo "  log:      $LOG"
    [[ -n "$RELAY_ALIVE" ]] && echo "  relay:    watching $RELAY_ALIVE"
    echo "  status:   ${SUDO:+sudo }launchctl print $DOMAIN/$LABEL"
    echo "  stop:     ${SUDO:+sudo }launchctl bootout $DOMAIN/$LABEL"
    echo
    $SUDO tail -n 6 "$LOG" 2>/dev/null || true
else
    echo "error: service did not start; check $LOG" >&2
    exit 1
fi
