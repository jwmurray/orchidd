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

> **The sensor sleeps, and that is not configurable.** On USB power the H&T Gen3
> wakes every 5 minutes, reports, and sleeps again. Shelly's firmware has no
> always-on mode even when USB-powered, so ~5 minutes is the best resolution
> available. Gaps longer than that mean the sensor or the host was unreachable.

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
- Theme, time window and Hi/Lo choice are remembered **per browser** in
  `localStorage`, so your phone, laptop and desktop each keep their own settings.

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
