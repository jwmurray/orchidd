# Orchid room alerts

`orchid-watch` runs on gardenway, polls orchidd on the iMac every minute, and
texts Rose Marie (and optionally emails her) when the room needs a person:

| Condition | Fires after | Notes |
|---|---|---|
| `orchidd_down` — iMac/orchidd not answering | 5 min | uses the last-known phone/email |
| `sensor_stale` — sensor silent | 5 min of orchidd's alert | |
| `plug_unreachable` — heater plug not answering | 5 min of orchidd's alert | |
| `below_setpoint` | 10 min of orchidd's alert | silent for the first 2 h after `day_start` (morning warm-up) and when mode is Off |
| `too_hot` — 90 °F or more | 5 min | heater stuck on |

Each alert is sent once, repeated hourly while it lasts, and followed by one
"back to normal" when it clears. The phone number, email and the alerts on/off
switch come from orchidd's settings page.

Path of a text: orchid-watch → ntfy on gardenway (private topic, deny-all) →
the iMessage relay on m2max (a second copy of the lead relay) → Messages.

## Install (from your Mac, in this order)

```sh
deploy/alerts/setup-ntfy-gardenway.sh            # topic, ntfy users, tokens
deploy/alerts/install-watch-gardenway.sh         # build + systemd service
deploy/alerts/install-relay-m2max.sh             # relay LaunchAgent on m2max
deploy/alerts/install-watch-gardenway.sh --test  # end-to-end test text
```

Before this branch is merged, build it explicitly:
`ORCHID_REF=alerts deploy/alerts/install-watch-gardenway.sh`.

All three scripts are safe to re-run. None prints a token or the topic name.

What lands where:

- gardenway: `/etc/orchid-watch/{topic,ntfy.token,env}` (root 600),
  `/usr/local/bin/orchid-watch`, `orchid-watch.service`, state in
  `/var/lib/orchid-watch/state.json`.
- your Mac: `~/.config/firm/orchid-relay.token` (600).
- m2max: `~/.config/firm/orchid-relay.env` (600),
  `~/Library/LaunchAgents/org.jwmfirm.orchid-relay.plist`,
  `~/Library/Logs/orchid-relay.log`.

## Test end to end

1. `deploy/alerts/install-watch-gardenway.sh --test` — sends "Test from the
   orchid room monitor…" through ntfy (and email, if set up). Exit status is
   non-zero if any channel failed.
2. The phone in the settings page's alert phone field should get an iMessage
   within a few seconds. If not, look at the relay:
   `ssh m2max tail -n 20 Library/Logs/orchid-relay.log`.
3. What the watcher sees right now, without sending anything:

   ```sh
   ssh gardenway "sudo sh -c 'set -a; . /etc/orchid-watch/env; set +a; ORCHID_WATCH_STATE=/var/lib/orchid-watch/state.json orchid-watch --once'"
   ```

4. Live log: `ssh gardenway journalctl -u orchid-watch -f`.

A real alert can be provoked by unplugging the sensor's batteries (sensor
silent after orchidd's `stale_minutes` + 5 min) or stopping orchidd on the iMac
for 6 minutes.

## Move the relay to the iMac

When the iMac has a jmurray login session with Messages signed in:

```sh
# carry the seen-file so the iMac does not re-send recent alerts
ssh m2max cat "'Library/Application Support/firm/orchid-relay.seen'" \
  | ssh imac "mkdir -p 'Library/Application Support/firm' && cat > 'Library/Application Support/firm/orchid-relay.seen'"
deploy/alerts/install-relay-m2max.sh imac
ssh m2max 'launchctl bootout gui/$(id -u)/org.jwmfirm.orchid-relay'
```

The iMac needs `~/Library/Application Support/firm/lead-imessage-relay.py`
first (from mycase `deploy/`). Run only one relay: two send every alert twice.

## Add email

1. In the Google account that will send (2-Step Verification on), create an
   app password: Google Account → Security → App passwords.
2. On gardenway:

   ```sh
   ssh -t gardenway 'sudo sh -c "umask 077; cat > /etc/orchid-watch/smtp.pass"'
   # paste the 16-character app password, Enter, Ctrl-D
   ssh -t gardenway sudoedit /etc/orchid-watch/env
   # uncomment and fill ORCHID_SMTP_HOST=smtp.gmail.com, ORCHID_SMTP_PORT=587,
   # ORCHID_SMTP_USER=<the gmail address>, ORCHID_SMTP_FROM=<same>
   ```

3. `deploy/alerts/install-watch-gardenway.sh --test` — the installer sees
   `smtp.pass` and gives it to the service; the test sends to the alert email
   on the settings page.

## Settings (`/etc/orchid-watch/env`)

| Variable | Default |
|---|---|
| `ORCHID_WATCH_URL` | `http://192.168.0.15:8000` |
| `ORCHID_NTFY_URL` | set by the installer from `/etc/orchid-watch/topic` |
| `ORCHID_NTFY_TOKEN_FILE` | systemd credential `ntfy.token` (from `/etc/orchid-watch/ntfy.token`) |
| `ORCHID_WARMUP_MIN` | `120` |
| `ORCHID_REPEAT_MIN` | `60` |
| `ORCHID_HOT_F` | `90` |
| `ORCHID_INTERVAL_S` | `60` |
| `ORCHID_WATCH_STATE` | `/var/lib/orchid-watch/state.json` |
| `ORCHID_SMTP_HOST`, `ORCHID_SMTP_USER` | unset (email off) |
| `ORCHID_SMTP_PASS_FILE` | systemd credential `smtp.pass` |
| `ORCHID_SMTP_PORT` | `587` (STARTTLS; `465` = implicit TLS) |
| `ORCHID_SMTP_FROM` | `ORCHID_SMTP_USER` |

After editing: `ssh gardenway sudo systemctl restart orchid-watch`.
