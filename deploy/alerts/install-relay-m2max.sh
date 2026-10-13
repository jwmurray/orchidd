#!/bin/sh
# Run a second copy of the firm's iMessage relay for the orchid topic.
# Run from your Mac, after setup-ntfy-gardenway.sh:
#
#   deploy/alerts/install-relay-m2max.sh            # on m2max
#   deploy/alerts/install-relay-m2max.sh imac       # on another Mac
#
# Uses the relay script already installed for leads,
#   ~/Library/Application Support/firm/lead-imessage-relay.py
# unmodified, with its own env file, seen-file, heartbeat, LaunchAgent and log:
#   ~/.config/firm/orchid-relay.env               (600; holds the read token)
#   ~/Library/LaunchAgents/org.jwmfirm.orchid-relay.plist
#   ~/Library/Logs/orchid-relay.log
#
# The token travels inside the script on ssh's stdin — never on a command line.
#
# Moving the relay to the iMac's jmurray session (next week):
#   1. Log in as jmurray at the iMac's screen (Messages must be signed in there).
#   2. Copy the seen-file so the iMac does not re-send the last few hours:
#        ssh m2max cat "'Library/Application Support/firm/orchid-relay.seen'" \
#          | ssh imac "mkdir -p 'Library/Application Support/firm' && cat > 'Library/Application Support/firm/orchid-relay.seen'"
#   3. deploy/alerts/install-relay-m2max.sh imac
#   4. ssh m2max 'launchctl bootout gui/$(id -u)/org.jwmfirm.orchid-relay'
# Only one relay may run: two would send every alert twice.
set -eu

HOST=${1:-m2max}
GARDENWAY=${GARDENWAY_HOST:-gardenway}
TOKEN_FILE="$HOME/.config/firm/orchid-relay.token"
FALLBACK_TO=${ORCHID_ALERT_TO:-+18016280795}

[ -s "$TOKEN_FILE" ] || { echo "no $TOKEN_FILE — run deploy/alerts/setup-ntfy-gardenway.sh first" >&2; exit 1; }
read -r TOKEN < "$TOKEN_FILE"
case "$TOKEN" in
  tk_*[!A-Za-z0-9_]*|"") echo "$TOKEN_FILE does not hold a ntfy token" >&2; exit 1 ;;
  tk_*) ;;
  *) echo "$TOKEN_FILE does not hold a ntfy token" >&2; exit 1 ;;
esac
case "$FALLBACK_TO" in
  +1[2-9][0-9][0-9][0-9][0-9][0-9][0-9][0-9][0-9][0-9]) ;;
  *) echo "fallback number must be +1 and 10 digits: $FALLBACK_TO" >&2; exit 2 ;;
esac

TOPIC=$(ssh "$GARDENWAY" 'sudo -n cat /etc/orchid-watch/topic')
case "$TOPIC" in
  orchid-alerts-????????) ;;
  *) echo "could not read the topic from $GARDENWAY:/etc/orchid-watch/topic" >&2; exit 1 ;;
esac

{
  printf "TOKEN='%s'\n" "$TOKEN"
  printf "TOPIC='%s'\n" "$TOPIC"
  printf "FALLBACK_TO='%s'\n" "$FALLBACK_TO"
  cat <<'REMOTE'
set -eu
H=$(hostname -s)
say() { echo "$H: $*"; }
die() { echo "$H: error: $*" >&2; exit 1; }

LABEL=org.jwmfirm.orchid-relay
FIRM="$HOME/Library/Application Support/firm"
SCRIPT="$FIRM/lead-imessage-relay.py"
ENVFILE="$HOME/.config/firm/orchid-relay.env"
PLIST="$HOME/Library/LaunchAgents/$LABEL.plist"
LOG="$HOME/Library/Logs/orchid-relay.log"
DOMAIN="gui/$(id -u)"

[ -f "$SCRIPT" ] || die "$SCRIPT is missing — install the lead relay here first"
grep -q 'to:' "$SCRIPT" \
  || say "warning: this relay script predates to:+1… tags; every alert goes to $FALLBACK_TO"

# Messages lives in the GUI session of whoever is at the screen.
console=$(stat -f %Su /dev/console)
[ "$console" = "$(whoami)" ] \
  || die "$(whoami) is not the console user ($console is); log in at the screen as $(whoami) first"

mkdir -p "$HOME/.config/firm" "$HOME/Library/LaunchAgents" "$HOME/Library/Logs" "$FIRM"

umask 077
cat > "$ENVFILE.tmp" <<EOF
# orchid alerts → iMessage. Sourced by $LABEL (sh, set -a).
LEAD_NTFY_URL=http://100.107.226.90:2586/$TOPIC
LEAD_NTFY_TOKEN=$TOKEN
LEAD_ALERT_TO=$FALLBACK_TO
LEAD_RELAY_SEEN="$FIRM/orchid-relay.seen"
LEAD_RELAY_ALIVE="$HOME/.config/firm/orchid-relay.alive"
EOF
chmod 600 "$ENVFILE.tmp"
mv "$ENVFILE.tmp" "$ENVFILE"
say "wrote $ENVFILE (600)"

umask 022
cat > "$PLIST" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<!-- Orchid room alerts relayed to iMessage. A second instance of the lead
     relay script with its own env file. Written by orchidd's
     deploy/alerts/install-relay-m2max.sh; no secrets in this file. -->
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>$LABEL</string>
    <key>ProgramArguments</key>
    <array>
        <string>/bin/sh</string>
        <string>-c</string>
        <string>set -a; . "\$HOME/.config/firm/orchid-relay.env"; set +a; exec /usr/bin/python3 "\$HOME/Library/Application Support/firm/lead-imessage-relay.py"</string>
    </array>
    <key>RunAtLoad</key>
    <true/>
    <key>KeepAlive</key>
    <true/>
    <key>ThrottleInterval</key>
    <integer>30</integer>
    <key>StandardOutPath</key>
    <string>$LOG</string>
    <key>StandardErrorPath</key>
    <string>$LOG</string>
</dict>
</plist>
EOF
plutil -lint "$PLIST" >/dev/null || die "generated plist is invalid"

launchctl bootout "$DOMAIN/$LABEL" </dev/null 2>/dev/null || true
sleep 1
launchctl bootstrap "$DOMAIN" "$PLIST" </dev/null
sleep 3
launchctl print "$DOMAIN/$LABEL" >/dev/null 2>&1 || die "bootstrap did not take"
say "orchid relay RUNNING — log: ~/Library/Logs/orchid-relay.log"
tail -n 3 "$LOG" 2>/dev/null | sed -e 's/tk_[A-Za-z0-9]*/tk_…/g' -e 's/orchid-alerts-[0-9a-f]*/orchid-alerts-…/g' || true
REMOTE
} | ssh "$HOST" 'sh -s'
