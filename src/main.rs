//! orchidd — collects temperature/humidity from a Shelly H&T (Gen3) over an
//! outbound WebSocket and serves a live dual-axis graph to the local network.
//!
//! The Shelly device connects OUT to us (it is the WebSocket client); orchidd is
//! the server. On each wake the device opens the socket and pushes its status;
//! we also actively call `Shelly.GetStatus` on connect so a reading is always
//! captured. Readings are stored in SQLite and served as JSON to the web UI.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Query, State};
use axum::response::{Html, IntoResponse, Json};
use axum::routing::{any, get};
use axum::Router;
use rusqlite::Connection;
use serde_json::{json, Value};

const INDEX_HTML: &str = include_str!("../static/index.html");

/// Minimum seconds between stored rows, so a single wake (which may deliver both
/// a GetStatus response and a NotifyFullStatus) yields one row, not several.
const MIN_INSERT_INTERVAL_SECS: i64 = 5;

#[derive(Clone)]
struct AppState {
    db: Arc<Mutex<Connection>>,
    /// Last-known values, so a partial NotifyStatus can still form a full row.
    last: Arc<Mutex<LastReading>>,
    last_insert: Arc<AtomicI64>,
}

#[derive(Default)]
struct LastReading {
    temp_c: Option<f64>,
    humidity: Option<f64>,
}

fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn env_or(key: &str, default: &str) -> String {
    std::env::var(key).unwrap_or_else(|_| default.to_string())
}

/// Choose the IP address to advertise for the web UI and the Shelly connection.
/// Preference order (nothing hard-coded):
///   1. ORCHIDD_HOST env override.
///   2. The Wi-Fi interface's IP (this host may also have a wired NIC on the
///      same subnet; the sensor should target Wi-Fi since it isn't always wired).
///   3. Primary outbound IP via the route trick (correct on a single-NIC server
///      like Filio, where there is no Wi-Fi/wired distinction).
fn detect_advertise_ip(bind: &str) -> String {
    if let Ok(h) = std::env::var("ORCHIDD_HOST") {
        if !h.trim().is_empty() {
            return h.trim().to_string();
        }
    }
    if let Some(ip) = macos_wifi_ip() {
        return ip;
    }
    if let Some(ip) = detect_local_ip() {
        return ip.to_string();
    }
    bind.to_string()
}

/// On macOS, resolve the Wi-Fi hardware port's device name and read its IPv4.
/// Returns None on other platforms or if Wi-Fi has no address.
fn macos_wifi_ip() -> Option<String> {
    use std::process::Command;
    let out = Command::new("networksetup")
        .arg("-listallhardwareports")
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&out.stdout);
    let mut lines = text.lines();
    let mut device = None;
    while let Some(line) = lines.next() {
        if line.starts_with("Hardware Port:") && line.contains("Wi-Fi") {
            for l in lines.by_ref() {
                if let Some(d) = l.strip_prefix("Device:") {
                    device = Some(d.trim().to_string());
                    break;
                }
            }
            break;
        }
    }
    let device = device?;
    let out = Command::new("ipconfig")
        .arg("getifaddr")
        .arg(&device)
        .output()
        .ok()?;
    let ip = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if ip.is_empty() {
        None
    } else {
        Some(ip)
    }
}

/// Primary outbound IPv4 via the OS routing table: open a UDP socket "toward" a
/// public address and read which local interface the OS would route through.
/// No packets are sent.
fn detect_local_ip() -> Option<std::net::IpAddr> {
    let sock = std::net::UdpSocket::bind("0.0.0.0:0").ok()?;
    sock.connect("8.8.8.8:80").ok()?;
    sock.local_addr().ok().map(|a| a.ip())
}

#[tokio::main]
async fn main() {
    let bind = env_or("ORCHIDD_BIND", "0.0.0.0");
    let port: u16 = env_or("ORCHIDD_PORT", "8000").parse().unwrap_or(8000);
    let db_path = env_or("ORCHIDD_DB", "orchidd.db");

    let conn = Connection::open(&db_path).expect("open sqlite db");
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS readings (
            id       INTEGER PRIMARY KEY AUTOINCREMENT,
            ts       INTEGER NOT NULL,
            temp_c   REAL    NOT NULL,
            humidity REAL    NOT NULL
        );
        CREATE INDEX IF NOT EXISTS idx_readings_ts ON readings(ts);",
    )
    .expect("init schema");

    let state = AppState {
        db: Arc::new(Mutex::new(conn)),
        last: Arc::new(Mutex::new(LastReading::default())),
        last_insert: Arc::new(AtomicI64::new(0)),
    };

    let app = Router::new()
        .route("/", get(|| async { Html(INDEX_HTML) }))
        // The Shelly outbound-websocket URL points here. Any path works, but the
        // setup instructions use /temperature to match what's already entered.
        .route("/temperature", any(ws_handler))
        .route("/shelly", any(ws_handler))
        .route("/api/data", get(api_data))
        .route("/api/meta", get(api_meta))
        .with_state(state);

    let addr: SocketAddr = format!("{bind}:{port}").parse().expect("bind addr");
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .expect("bind listener");

    // Advertise the real Wi-Fi/primary IP so the URLs shown are usable as-is.
    // Binding 0.0.0.0 already serves on this IP; ORCHIDD_HOST can override the
    // advertised address if detection picks the wrong interface.
    let host_ip = detect_advertise_ip(&bind);

    println!("orchidd listening on {addr}  (db: {db_path})");
    println!("  web UI (view from any computer on the network):");
    println!("      http://{host_ip}:{port}/");
    println!("  Shelly outbound-websocket Server field:");
    println!("      ws://{host_ip}:{port}/temperature");
    axum::serve(listener, app).await.expect("serve");
}

// ---------------------------------------------------------------------------
// WebSocket: the Shelly device connects to us and pushes status.
// ---------------------------------------------------------------------------

async fn ws_handler(ws: WebSocketUpgrade, State(state): State<AppState>) -> impl IntoResponse {
    ws.on_upgrade(move |socket| handle_socket(socket, state))
}

async fn handle_socket(mut socket: WebSocket, state: AppState) {
    println!("shelly connected");
    // Proactively request full status so we capture a reading immediately,
    // regardless of what the device pushes on its own.
    let req = r#"{"id":1,"src":"orchidd","method":"Shelly.GetStatus"}"#;
    let _ = socket.send(Message::Text(req.into())).await;

    while let Some(Ok(msg)) = socket.recv().await {
        match msg {
            Message::Text(t) => process_message(t.as_str(), &state),
            Message::Ping(p) => {
                let _ = socket.send(Message::Pong(p)).await;
            }
            Message::Close(_) => break,
            _ => {}
        }
    }
    println!("shelly disconnected");
}

/// Parse a Shelly RPC frame and store a reading if we can form a complete row.
///
/// Frames of interest carry the sensor components under either `result`
/// (a response to Shelly.GetStatus) or `params` (NotifyFullStatus/NotifyStatus):
///   "temperature:0": { "tC": 22.5, "tF": 72.5 }
///   "humidity:0":    { "rh": 45.0 }
fn process_message(text: &str, state: &AppState) {
    let v: Value = match serde_json::from_str(text) {
        Ok(v) => v,
        Err(_) => return,
    };

    let obj = v.get("result").or_else(|| v.get("params"));
    let Some(obj) = obj else { return };

    let temp = obj
        .get("temperature:0")
        .and_then(|t| t.get("tC"))
        .and_then(Value::as_f64);
    let hum = obj
        .get("humidity:0")
        .and_then(|h| h.get("rh"))
        .and_then(Value::as_f64);

    if temp.is_none() && hum.is_none() {
        return;
    }

    // Merge with last-known values so a partial update can still form a row.
    let (temp_c, humidity) = {
        let mut last = state.last.lock().unwrap();
        if let Some(t) = temp {
            last.temp_c = Some(t);
        }
        if let Some(h) = hum {
            last.humidity = Some(h);
        }
        match (last.temp_c, last.humidity) {
            (Some(t), Some(h)) => (t, h),
            _ => return, // wait until we have both at least once
        }
    };

    let now = now_unix();
    let prev = state.last_insert.load(Ordering::Relaxed);
    if now - prev < MIN_INSERT_INTERVAL_SECS {
        return;
    }
    state.last_insert.store(now, Ordering::Relaxed);

    let db = state.db.lock().unwrap();
    if let Err(e) = db.execute(
        "INSERT INTO readings (ts, temp_c, humidity) VALUES (?1, ?2, ?3)",
        rusqlite::params![now, temp_c, humidity],
    ) {
        eprintln!("db insert error: {e}");
        return;
    }
    let temp_f = temp_c * 9.0 / 5.0 + 32.0;
    println!("stored  {temp_f:.1}°F  {humidity:.1}%RH");
}

// ---------------------------------------------------------------------------
// HTTP API for the web UI.
// ---------------------------------------------------------------------------

#[derive(serde::Deserialize)]
struct DataQuery {
    days: Option<f64>,
}

/// Return readings within the last `days` (default 7) as compact arrays.
/// Temperatures are converted to °F for display. Downsampled to keep payloads
/// small if there are a lot of points.
async fn api_data(State(state): State<AppState>, Query(q): Query<DataQuery>) -> impl IntoResponse {
    let days = q.days.unwrap_or(7.0).clamp(0.01, 3650.0);
    let cutoff = now_unix() - (days * 86400.0) as i64;

    let rows: Vec<(i64, f64, f64)> = {
        let db = state.db.lock().unwrap();
        let mut stmt = db
            .prepare("SELECT ts, temp_c, humidity FROM readings WHERE ts >= ?1 ORDER BY ts")
            .unwrap();
        let iter = stmt
            .query_map([cutoff], |r| {
                Ok((r.get::<_, i64>(0)?, r.get::<_, f64>(1)?, r.get::<_, f64>(2)?))
            })
            .unwrap();
        iter.filter_map(Result::ok).collect()
    };

    // Downsample by striding if there are more points than we want to render.
    const MAX_POINTS: usize = 2000;
    let stride = (rows.len() / MAX_POINTS).max(1);

    let mut t = Vec::new();
    let mut temp_f = Vec::new();
    let mut hum = Vec::new();
    for (i, (ts, tc, rh)) in rows.iter().enumerate() {
        if i % stride != 0 && i != rows.len() - 1 {
            continue;
        }
        t.push(ts * 1000); // ms epoch for JS Date
        let tf = tc * 9.0 / 5.0 + 32.0;
        temp_f.push((tf * 100.0).round() / 100.0);
        hum.push((rh * 100.0).round() / 100.0);
    }

    Json(json!({ "unit": "F", "t": t, "tempF": temp_f, "hum": hum }))
}

/// Report the span of stored data and the default window to show, per the
/// growth rule: start at 1 day, grow one whole day at a time as data
/// accumulates, cap at 7 days.
async fn api_meta(State(state): State<AppState>) -> impl IntoResponse {
    let (min_ts, max_ts, count): (Option<i64>, Option<i64>, i64) = {
        let db = state.db.lock().unwrap();
        db.query_row(
            "SELECT MIN(ts), MAX(ts), COUNT(*) FROM readings",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .unwrap_or((None, None, 0))
    };

    let span_days = match (min_ts, max_ts) {
        (Some(a), Some(b)) if b > a => (b - a) as f64 / 86400.0,
        _ => 0.0,
    };
    // floor(span) days, clamped to [1, 7].
    let default_days = (span_days.floor() as i64).clamp(1, 7);

    Json(json!({
        "minTs": min_ts.map(|x| x * 1000),
        "maxTs": max_ts.map(|x| x * 1000),
        "count": count,
        "spanDays": span_days,
        "defaultDays": default_days,
        "maxDays": 7
    }))
}
