# orchidd

A small Rust daemon that collects temperature & humidity from a **Shelly H&T
(Gen3)** sensor, serves a live dual-axis graph to your network, and installs as
a background service on macOS or Linux.

## How data flows

```
Shelly H&T  ──outbound WebSocket──▶  orchidd  ──HTTP──▶  browser / iPhone
 (192.168.0.39)                     (host machine)        (LAN, or WAN via tunnel)
```

The sensor is a "sleepy" device: it wakes periodically, connects **out** to
orchidd over a WebSocket, and pushes its status. orchidd is the WebSocket
*server* — when the sensor connects, orchidd also actively calls
`Shelly.GetStatus` so a reading is captured on every wake. Readings are stored
in SQLite.

Chosen over MQTT (needs a broker) and RPC-over-UDP (lossy) because it needs no
extra infrastructure and delivers reliably over TCP.

### Active polling (USB power)

orchidd also polls the sensor over HTTP every `sensor_poll_secs` (default 30):

```
GET <sensor_url>/rpc/Temperature.GetStatus?id=0   → {"id":0,"tC":21.9,"tF":71.4}
GET <sensor_url>/rpc/Humidity.GetStatus?id=0      → {"id":0,"rh":68.6}
```

Each request has a 3-second timeout. A successful poll is stored exactly like a
websocket push and wakes the thermostat controller at once, so it acts on the
fresh reading. A failed poll is skipped silently (set `ORCHIDD_DEBUG=1` to see
the skips); the websocket push keeps working as the fallback.

**The H&T only stays awake on USB power with the batteries removed.** On
batteries it sleeps between its ~5-minute pushes and almost every poll fails,
so resolution falls back to the push interval. Set `sensor_url` to an empty
string to turn polling off.

The push also fires when a reading changes by more than the sensor's report
threshold. To make pushes more frequent, lower it (run once against the
sensor while it is awake):

```sh
curl 'http://<sensor>/rpc/Temperature.SetConfig?id=0&config={"report_thr_C":0.1}'
```

`/api/status` reports `sensor_polling: true` while the last successful poll is
within two intervals. At 30-second polling the database grows by ~2,900 rows a
day; `/api/data` reads through the `ts` index and thins any range longer than
about 16 hours to at most ~2,000 points, so the graph stays fast.

## Install as a service

Both installers build the binary, install it outside the repo, and register a
service that starts on boot and restarts on crash. Re-running either one
upgrades in place.

### macOS (launchd)

```fish
bash deploy/install-macos.sh            # per-user agent, no sudo
bash deploy/install-macos.sh --system   # system daemon, starts before login
```

|                | user agent (default)                   | `--system` daemon              |
|----------------|----------------------------------------|--------------------------------|
| sudo required  | no                                     | yes                            |
| starts at      | login                                  | boot, no login needed          |
| binary         | `~/.local/bin/orchidd`                 | `/usr/local/bin/orchidd`       |
| database       | `~/Library/Application Support/orchidd`| `/usr/local/var/orchidd`       |
| log            | `~/Library/Logs/orchidd.log`           | `/usr/local/var/log/orchidd.log`|

Use `--system` on a machine that is only reached over the network — a user agent
will not start after an unattended reboot until somebody logs in.

```fish
launchctl print gui/(id -u)/com.jwmurray.orchidd     # status
tail -f ~/Library/Logs/orchidd.log                   # logs
launchctl bootout gui/(id -u)/com.jwmurray.orchidd   # stop
```

### Linux (systemd)

```bash
sudo bash deploy/install-linux.sh
```

Creates an unprivileged `orchidd` system user, installs the binary to
`/usr/local/bin/orchidd`, the database to `/var/lib/orchidd/orchidd.db`, and a
hardened unit at `/etc/systemd/system/orchidd.service`.

Requires cargo and a C toolchain (rusqlite builds SQLite from source):

```bash
sudo apt install build-essential      # Debian/Ubuntu
```

```bash
systemctl status orchidd
journalctl -u orchidd -f
sudo systemctl restart orchidd
```

### Run it in the foreground instead

```fish
cargo run --release
```

### Configuration (all optional, via env vars)

| Var            | Default     | Meaning                                           |
|----------------|-------------|---------------------------------------------------|
| `ORCHIDD_BIND` | `0.0.0.0`   | Interface to bind (0.0.0.0 = all, incl. Wi-Fi).   |
| `ORCHIDD_PORT` | `8000`      | HTTP/WebSocket port.                              |
| `ORCHIDD_DB`   | `orchidd.db`| SQLite file path.                                 |
| `ORCHIDD_HOST` | *(auto)*    | Override the advertised IP (skip auto-detection). |
| `ORCHIDD_PLUG_URL` | `http://192.168.0.30` | Heater plug URL, used only to seed `plug_url` on first run. |
| `ORCHIDD_SENSOR_URL` | `http://192.168.0.39` | Sensor URL for polling, used only to seed `sensor_url` on first run. |
| `ORCHIDD_DEBUG` | *(unset)* | Set to log skipped sensor polls. |

The installers set `ORCHIDD_DB` and `ORCHIDD_PORT` in the service definition;
set `ORCHIDD_PORT` before running an installer to change the port.

## Configure the Shelly H&T (one time)

1. Open the sensor's IP (e.g. `http://192.168.0.39/`). Press its button first so
   it stays awake for ~3 minutes.
2. **Settings → Outbound websocket.**
3. **Server:** `ws://<host-ip>:8000/temperature`
4. **Connection type:** the **non-TLS** option (plain `ws://`, not `wss://`).
5. Check **Enable**, then **Save settings**.

The sensor reports on each wake. The graph auto-refreshes every minute.

> Give the host machine a **DHCP reservation** on your router. The sensor stores
> a literal IP, so a new lease silently ends reporting.

## The web UI

- Temperature (°F) and humidity (%RH) on **one graph** with **separate scales**
  (temp = left axis, humidity = right axis), each auto-ranged to 10% beyond its
  data so both fill the height.
- **"Show last"**: 1, 2, 4 and 12 hours, then whole days up to 7. The list of day
  options grows as history accumulates.
- **"Hi/Lo"**: draws high and low reference lines for the last 24 hours, 48 hours
  or 7 days — dashed, labelled, in each metric's colour, with a distinct dash
  pattern per window. `All three` shows all of them at once. The axes expand to
  include whichever lines are shown, so a 7-day high stays visible even while
  viewing the last hour.
- A status strip above the graph: temperature, Day/Night and setpoint, heater
  ON/OFF with watts, reading age, and any alert in red. Heater-on periods are
  shaded on the graph.
- Theme, time window and Hi/Lo choice are remembered **per browser** in
  `localStorage`, so your phone, laptop and desktop each keep their own settings.

## Thermostat

orchidd can drive a heater plugged into a **Shelly Plus Plug US** (Gen2 RPC, no
auth) from the H&T readings. A controller task runs every 30 seconds (and
immediately after a settings change):

1. `GET <plug>/rpc/Switch.GetStatus?id=0` to read the real output and watts.
2. Decide, in this order: mode `off` → off; no reading newer than
   `stale_minutes` → off (`sensor_stale`); temperature at/above `max_temp_f` →
   off; otherwise hysteresis — on at or below the setpoint, off at or above
   setpoint + `hysteresis_f`, and inside that band keep the current state.
3. Heating: send `Switch.Set?id=0&on=true&toggle_after=900` whenever the plug is
   off or 5 minutes have passed since the last refresh. Not heating and the plug
   is on: send `Switch.Set?id=0&on=false`.

**Dead-man safety.** Every "on" expires on the plug itself after 15 minutes.
If orchidd crashes, hangs, or loses the network, the heater turns off within 15
minutes without anything else having to work.

Day is `[day_start, day_end)` in `timezone` local time (wall clock, so DST
changes need nothing); the rest is night. Each change of heater state is written
to the `heater_events` table and drawn as shaded bands on the graph.

Readings stay stored in °C exactly as the sensor sends them; the thermostat, its
settings and its API are all °F, converted when a reading is read back.

### Settings

Stored in the SQLite `settings` table, seeded with defaults on first run, and
edited from the web page's **Thermostat settings** panel or the API.

| Setting            | Default               | Rule                                          |
|--------------------|-----------------------|-----------------------------------------------|
| `mode`             | `auto`                | `auto` or `off` (off keeps the heater off)    |
| `plug_url`         | `http://192.168.0.30` | `http://…` or `https://…`                     |
| `timezone`         | `America/Denver`      | IANA zone name                                |
| `day_start`        | `07:00`               | `HH:MM`, must differ from `day_end`           |
| `day_end`          | `21:00`               | `HH:MM`                                       |
| `day_setpoint_f`   | `75.0`                | 32–95                                         |
| `night_setpoint_f` | `60.0`                | 32–95                                         |
| `hysteresis_f`     | `1.0`                 | 0.2–5                                         |
| `max_temp_f`       | `85.0`                | above both setpoints + hysteresis, ≤ 120      |
| `stale_minutes`    | `20`                  | 6–240                                         |
| `alert_below_f`    | `1.5`                 | 0–20                                          |
| `alert_phone`      | `""`                  | stored only                                   |
| `alert_email`      | `""`                  | stored only                                   |
| `alerts_enabled`   | `true`                | stored only                                   |
| `sensor_url`       | `http://192.168.0.39` | `http://…`/`https://…`, or `""` to disable polling |
| `sensor_poll_secs` | `30`                  | 10–600                                        |

Alert delivery (text/email) is not built yet; `/api/status` reports the current
alert so something else can act on it.

### API

| Route                    | Purpose                                                     |
|--------------------------|-------------------------------------------------------------|
| `GET /api/status`        | `temp_f, humidity, reading_ts, reading_age_s, period, setpoint_f, off_at_f, heater_on, apower_w, controller_reason, plug_error, mode, alert, sensor_polling` |
| `GET /api/settings`      | All settings.                                               |
| `PUT /api/settings`      | JSON object with any subset of settings. Validated as a whole; `400 {"error": "..."}` and nothing saved on failure. |
| `GET /api/heater?days=N` | Heater-on intervals, `{"intervals": [[startMs, endMs], ...]}`. |

`alert` is `null` or `{kind, detail}` with `kind` one of `plug_unreachable`,
`sensor_stale`, or `below_setpoint` (temperature more than `alert_below_f`
under the setpoint, mode `auto` only), in that order of precedence.

```fish
curl -s localhost:8000/api/status
curl -s -X PUT localhost:8000/api/settings -H 'content-type: application/json' -d '{"night_setpoint_f": 58}'
```

## Install on an iPhone (or any phone)

orchidd serves a progressive web app — a real home-screen icon, launching
full-screen with no browser chrome.

1. Open the orchidd URL in **Safari** (must be Safari; Chrome on iOS cannot
   install web apps).
2. **Share → Add to Home Screen → Add.**

It caches its shell and the last readings, so opening it with no signal shows the
last known data instead of an error. To reach it from outside the house, put it
behind a tunnel first — see below — and install from that public URL.

## Remote access with Cloudflare Tunnel

Exposes orchidd on a hostname without opening a router port or having a static IP.

```bash
brew install cloudflared            # macOS
cloudflared tunnel login            # opens a browser; pick your zone
cloudflared tunnel create orchidd
cloudflared tunnel route dns orchidd orchid.example.com
```

Write `~/.cloudflared/config.yml` (see `deploy/cloudflared/config.yml.example`),
then install it as a service so it also survives reboot:

```bash
sudo cloudflared service install
```

**Gate it.** The tunnel is on the public internet; a live feed of a home is worth
protecting. In the Cloudflare dashboard: **Zero Trust → Access → Applications →
Add a self-hosted application** for the hostname, with a policy allowing only
your email addresses. Sign-in is a one-time emailed PIN and the session cookie
lasts as long as you configure, so the phone rarely re-prompts.

The web UI understands Access: when a session expires the page shows
"Session expired — Sign in again" instead of a broken graph, and its service
worker refuses to cache the login page in place of your data.

> Leave the **sensor** pointed at the host's LAN address. The Shelly cannot pass
> an Access login, and its readings should never leave the house. Only the
> browser-facing UI goes through the tunnel.

## Moving to another host

1. Install the service on the new host (above).
2. Copy `orchidd.db` into the new host's data directory to keep history.
3. Repoint the Shelly's **Outbound websocket → Server** at the new host's IP.
4. If a tunnel is in use, update its `service:` target to the new host.

## Regenerating the app icons

Icons are committed; `deploy/make-icons.py` (pure stdlib) regenerates them if you
want to restyle:

```fish
python3 deploy/make-icons.py
```
