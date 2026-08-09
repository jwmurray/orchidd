# orchidd

A small Rust daemon that collects temperature & humidity from a **Shelly H&T
(Gen3)** sensor and serves a live, dual-axis graph to any computer on your
network.

## How data flows

```
Shelly H&T  ──outbound WebSocket──▶  orchidd  ──HTTP──▶  browser (graph)
 (192.168.0.39)                      (this host)          (any LAN computer)
```

The sensor is a battery/USB "sleepy" device: it wakes periodically, connects
**out** to orchidd over a WebSocket, and pushes its status. orchidd is the
WebSocket *server* — when the sensor connects, orchidd also actively calls
`Shelly.GetStatus` so a reading is captured on every wake. Readings are stored
in SQLite (`orchidd.db`).

Chosen over MQTT (needs a broker) and RPC-over-UDP (lossy) because it needs no
extra infrastructure and delivers reliably over TCP.

## Run it

```fish
cd ~/code/rust/orchidd
cargo run --release          # or ./target/release/orchidd
```

On start it prints the exact URLs, using this host's **Wi-Fi** IP (it prefers
the Wi-Fi interface over any wired NIC on the same subnet, and nothing is
hard-coded):

```
  web UI:   http://192.168.0.35:8000/
  Shelly:   ws://192.168.0.35:8000/temperature
```

### Configuration (all optional, via env vars)

| Var            | Default     | Meaning                                            |
|----------------|-------------|----------------------------------------------------|
| `ORCHIDD_BIND` | `0.0.0.0`   | Interface to bind (0.0.0.0 = all, incl. Wi-Fi).    |
| `ORCHIDD_PORT` | `8000`      | HTTP/WebSocket port.                               |
| `ORCHIDD_DB`   | `orchidd.db`| SQLite file path.                                  |
| `ORCHIDD_HOST` | *(auto)*    | Override the advertised IP (skip auto-detection).  |

Binding `0.0.0.0` means orchidd is reachable on the Wi-Fi IP automatically; the
advertised URL just *shows* the Wi-Fi address so you know what to type into the
Shelly.

## Configure the Shelly H&T (one time)

1. Open `http://192.168.0.39/` (USB-power the sensor so it stays awake).
2. **Settings → Outbound websocket.**
3. **Server:** `ws://192.168.0.35:8000/temperature`
4. **Connection type:** the **non-TLS** option (plain `ws://`, not `wss://`).
5. Check **Enable**, then **Save settings**.

The sensor reports on each wake (~every few minutes). The graph auto-refreshes
every minute.

## The web UI

- Temperature (°F) and humidity (%RH) on **one graph** with **separate scales**
  (temp = left axis, humidity = right axis), so both fill the height and show
  good variance.
- Each axis is auto-ranged to **10% beyond** the data's min/max.
- **"Show last"** selector: 24 hours through 7 days.
- Default window grows with your data: 24h until you have 2 days, then 2/3/4…
  up to **7 days** once ≥7 days are stored.

## Moving to the Filio server later

orchidd is host-agnostic. To relocate:

1. Copy the folder (and `orchidd.db` if you want history) to Filio.
2. `cargo run --release` there. On a single-NIC server the auto-detected IP is
   correct; otherwise set `ORCHIDD_HOST=<filio-ip>`.
3. Update the Shelly **Outbound websocket → Server** to
   `ws://<filio-ip>:8000/temperature`. Nothing else changes.
