#!/bin/sh
# Build and install orchid-watch as a systemd service on gardenway.
# Run from your Mac, after setup-ntfy-gardenway.sh:
#
#   deploy/alerts/install-watch-gardenway.sh            # install / upgrade
#   deploy/alerts/install-watch-gardenway.sh --test     # ... then send a test alert
#
# Safe to re-run: it pulls, rebuilds, reinstalls and restarts. It never
# overwrites /etc/orchid-watch/env once that exists (it only adds
# ORCHID_NTFY_URL if missing), so your edits there survive.
#
# Builds from origin/main of https://github.com/jwmurray/orchidd.git in
# ~/code/orchidd on gardenway. To build a branch: ORCHID_REF=alerts …
set -eu

HOST=${GARDENWAY_HOST:-gardenway}
REF=${ORCHID_REF:-main}
RUN_TEST=0
case "${1:-}" in
  "") ;;
  --test) RUN_TEST=1 ;;
  *) echo "usage: $0 [--test]" >&2; exit 2 ;;
esac
case "$REF" in
  *[!A-Za-z0-9._/-]*|"") echo "ORCHID_REF has odd characters: $REF" >&2; exit 2 ;;
esac

{
  printf 'RUN_TEST=%s\n' "$RUN_TEST"
  printf "REF='%s'\n" "$REF"
  cat <<'REMOTE'
set -eu
say() { echo "gardenway: $*"; }
die() { echo "gardenway: error: $*" >&2; exit 1; }

CARGO="$HOME/.cargo/bin/cargo"
REPO="$HOME/code/orchidd"
DIR=/etc/orchid-watch
UNIT=/etc/systemd/system/orchid-watch.service
DROPIN=/etc/systemd/system/orchid-watch.service.d

[ -x "$CARGO" ] || die "no cargo at $CARGO (install rustup)"
command -v cc >/dev/null 2>&1 || die "no C compiler (sudo apt install build-essential)"
sudo -n true 2>/dev/null || die "needs passwordless sudo"
sudo test -s "$DIR/topic" || die "no $DIR/topic — run deploy/alerts/setup-ntfy-gardenway.sh first"
sudo test -s "$DIR/ntfy.token" || die "no $DIR/ntfy.token — run deploy/alerts/setup-ntfy-gardenway.sh first"

if [ -d "$REPO/.git" ]; then
  git -C "$REPO" fetch --quiet origin </dev/null
else
  mkdir -p "$(dirname "$REPO")"
  git clone --quiet https://github.com/jwmurray/orchidd.git "$REPO" </dev/null
fi
git -C "$REPO" checkout --quiet --detach "origin/$REF" </dev/null
say "building orchid-watch at $(git -C "$REPO" rev-parse --short HEAD) ($REF)"
(cd "$REPO" && "$CARGO" build --release --locked -p orchid-watch) </dev/null

sudo install -m 0755 "$REPO/target/release/orchid-watch" /usr/local/bin/orchid-watch
say "installed /usr/local/bin/orchid-watch"

# Every command below that could read stdin gets </dev/null: stdin is this
# script, and a command that read it would swallow the rest.

# The env file holds the topic URL (half the secret), so it is root 600 too.
if ! sudo test -f "$DIR/env"; then
  TOPIC=$(sudo cat "$DIR/topic")
  sudo sh -c "umask 077; cat > '$DIR/env'" <<EOF
# orchid-watch settings. Edit, then: sudo systemctl restart orchid-watch
ORCHID_WATCH_URL=http://192.168.0.15:8000
ORCHID_NTFY_URL=http://100.107.226.90:2586/$TOPIC
#ORCHID_WARMUP_MIN=120
#ORCHID_REPEAT_MIN=60
#ORCHID_HOT_F=90

# Email (optional). Put the password in $DIR/smtp.pass (root 600), uncomment
# these, and rerun deploy/alerts/install-watch-gardenway.sh.
#ORCHID_SMTP_HOST=smtp.gmail.com
#ORCHID_SMTP_PORT=587
#ORCHID_SMTP_USER=you@gmail.com
#ORCHID_SMTP_FROM=you@gmail.com
EOF
  say "wrote $DIR/env"
elif ! sudo grep -q '^ORCHID_NTFY_URL=' "$DIR/env"; then
  TOPIC=$(sudo cat "$DIR/topic")
  printf 'ORCHID_NTFY_URL=http://100.107.226.90:2586/%s\n' "$TOPIC" \
    | sudo sh -c "cat >> '$DIR/env'"
  say "added ORCHID_NTFY_URL to $DIR/env"
else
  say "kept existing $DIR/env"
fi
sudo chmod 600 "$DIR/env"

sudo install -m 0644 "$REPO/deploy/systemd/orchid-watch.service" "$UNIT"
if sudo test -s "$DIR/smtp.pass"; then
  sudo chmod 600 "$DIR/smtp.pass"
  sudo mkdir -p "$DROPIN"
  printf '[Service]\nLoadCredential=smtp.pass:%s/smtp.pass\n' "$DIR" \
    | sudo sh -c "cat > '$DROPIN/smtp.conf'"
  say "email password found; service will load it"
else
  sudo rm -f "$DROPIN/smtp.conf"
fi

sudo systemctl daemon-reload </dev/null
sudo systemctl enable --quiet orchid-watch </dev/null
sudo systemctl restart orchid-watch </dev/null
sleep 3
if sudo systemctl is-active --quiet orchid-watch; then
  say "orchid-watch is running"
  sudo journalctl -u orchid-watch --no-pager -n 8 -o cat </dev/null || true
else
  sudo journalctl -u orchid-watch --no-pager -n 30 -o cat </dev/null >&2 || true
  die "orchid-watch failed to start"
fi

if [ "$RUN_TEST" = 1 ]; then
  say "sending a test alert through every configured channel"
  sudo sh -c '
    set -a; . /etc/orchid-watch/env; set +a
    export ORCHID_WATCH_STATE=/var/lib/orchid-watch/state.json
    export ORCHID_NTFY_TOKEN_FILE=/etc/orchid-watch/ntfy.token
    if [ -s /etc/orchid-watch/smtp.pass ]; then
      export ORCHID_SMTP_PASS_FILE=/etc/orchid-watch/smtp.pass
    fi
    exec /usr/local/bin/orchid-watch --test "Test from the orchid room monitor. If you can read this, alerts reach you."
  ' </dev/null
fi
REMOTE
} | ssh "$HOST" 'sh -s'
