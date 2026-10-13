#!/bin/sh
# Create the orchid alert topic and its two ntfy accounts on gardenway.
# Run from your Mac:
#
#   deploy/alerts/setup-ntfy-gardenway.sh
#
# Safe to re-run. On gardenway (as root, via passwordless sudo):
#   /etc/orchid-watch/topic       orchid-alerts-<8 hex>, made once and reused
#   ntfy user orchid-pub          write-only on that topic  (orchid-watch)
#   ntfy user orchid-relay        read-only on that topic   (the iMessage relay)
#   /etc/orchid-watch/ntfy.token  orchid-pub's token, root 600, made once
#
# On this Mac: ~/.config/firm/orchid-relay.token (600), orchid-relay's token,
# made only if that file is missing. No token or topic is printed.
#
# If ntfy does not run from the Debian package (e.g. Docker), say how to reach
# its CLI: NTFY_CMD='docker exec ntfy ntfy' deploy/alerts/setup-ntfy-gardenway.sh
set -eu

HOST=${GARDENWAY_HOST:-gardenway}
TOKEN_FILE="$HOME/.config/firm/orchid-relay.token"

case "${NTFY_CMD:-}" in
  *"'"*) echo "NTFY_CMD may not contain a single quote" >&2; exit 2 ;;
esac

need_relay=1
if [ -s "$TOKEN_FILE" ]; then
  need_relay=0
  echo "relay token already at $TOKEN_FILE; not making another" >&2
fi

mkdir -p "$(dirname "$TOKEN_FILE")"
umask 077
tmp=$(mktemp "$(dirname "$TOKEN_FILE")/.orchid-relay.token.XXXXXX")
trap 'rm -f "$tmp"' EXIT

# The remote script reads its two settings from the first lines of stdin; its
# stdout is the relay token (or nothing), and it talks to you on stderr.
{
  printf 'NEED_RELAY=%s\n' "$need_relay"
  printf "NTFY_CMD='%s'\n" "${NTFY_CMD:-}"
  cat <<'REMOTE'
set -eu
say() { echo "gardenway: $*" >&2; }
die() { say "error: $*"; exit 1; }
umask 077
DIR=/etc/orchid-watch
mkdir -p "$DIR"
chmod 755 "$DIR"

if [ -s "$DIR/topic" ]; then
  TOPIC=$(cat "$DIR/topic")
  say "reusing the existing topic ($DIR/topic)"
else
  TOPIC="orchid-alerts-$(od -An -N4 -tx1 /dev/urandom | tr -d ' \n')"
  printf '%s\n' "$TOPIC" > "$DIR/topic"
  chmod 600 "$DIR/topic"
  say "created a new topic ($DIR/topic)"
fi
case "$TOPIC" in
  orchid-alerts-????????) ;;
  *) die "$DIR/topic does not hold an orchid-alerts-<8 hex> name" ;;
esac

# How to run the ntfy CLI. It must run as whoever owns the auth database, or
# sqlite leaves root-owned journal files the server then cannot write.
if [ -n "$NTFY_CMD" ]; then
  NTFY=$NTFY_CMD
else
  command -v ntfy >/dev/null 2>&1 || die "no ntfy CLI on PATH; rerun with NTFY_CMD='…'"
  CONF=/etc/ntfy/server.yml
  [ -r "$CONF" ] || die "$CONF not found; rerun with NTFY_CMD='…'"
  AUTH=$(sed -n "s/^[[:space:]]*auth-file:[[:space:]]*[\"']\{0,1\}\([^\"' #]*\).*/\1/p" "$CONF" | head -n 1)
  [ -n "$AUTH" ] || die "no auth-file in $CONF — access control is not on"
  [ -f "$AUTH" ] || die "auth-file $AUTH does not exist"
  OWNER=$(stat -c %U "$AUTH")
  NTFY="sudo -u $OWNER --preserve-env=NTFY_PASSWORD ntfy"
fi

# `ntfy user add` reads NTFY_PASSWORD instead of prompting. The password is
# never used again — both accounts work by token — so it is random and kept
# nowhere. stdin is /dev/null so a prompt can never eat this script.
add_user() {
  NTFY_PASSWORD=$(od -An -N24 -tx1 /dev/urandom | tr -d ' \n')
  export NTFY_PASSWORD
  if out=$($NTFY user add "$1" </dev/null 2>&1); then
    say "ntfy user $1 created"
  else
    case "$out" in
      *"already exists"*) say "ntfy user $1 already exists" ;;
      *) unset NTFY_PASSWORD; die "ntfy user add $1: $out" ;;
    esac
  fi
  unset NTFY_PASSWORD
}

# Prints only the token itself.
new_token() {
  out=$($NTFY token add --label="$2" "$1" </dev/null 2>&1) || die "ntfy token add $1 failed"
  tok=$(printf '%s\n' "$out" | grep -o 'tk_[A-Za-z0-9]*' | head -n 1)
  [ -n "$tok" ] || die "ntfy token add $1 printed no token"
  printf '%s\n' "$tok"
}

add_user orchid-pub
add_user orchid-relay
$NTFY access orchid-pub "$TOPIC" write-only </dev/null >/dev/null 2>&1 \
  || die "ntfy access orchid-pub failed"
$NTFY access orchid-relay "$TOPIC" read-only </dev/null >/dev/null 2>&1 \
  || die "ntfy access orchid-relay failed"
say "access: orchid-pub write-only, orchid-relay read-only on the topic"

if [ -s "$DIR/ntfy.token" ]; then
  say "publisher token already in $DIR/ntfy.token"
else
  new_token orchid-pub orchid-watch > "$DIR/ntfy.token.tmp"
  chmod 600 "$DIR/ntfy.token.tmp"
  mv "$DIR/ntfy.token.tmp" "$DIR/ntfy.token"
  say "publisher token written to $DIR/ntfy.token (root 600)"
fi

if [ "$NEED_RELAY" = 1 ]; then
  new_token orchid-relay orchid-imessage-relay
  say "relay token sent to your Mac"
fi
REMOTE
} | ssh "$HOST" 'sudo -n sh -s' > "$tmp"

if [ "$need_relay" = 1 ]; then
  if ! grep -q '^tk_[A-Za-z0-9]*$' "$tmp"; then
    echo "error: gardenway did not return a relay token" >&2
    exit 1
  fi
  chmod 600 "$tmp"
  mv "$tmp" "$TOKEN_FILE"
  echo "relay token saved to $TOKEN_FILE (600)"
fi
echo "ntfy is set up for orchid alerts on $HOST."
