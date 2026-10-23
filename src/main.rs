//! orchidd — collects temperature/humidity from a Shelly H&T (Gen3) over an
//! outbound WebSocket and serves a live dual-axis graph to the local network.
//!
//! The Shelly device connects OUT to us (it is the WebSocket client); orchidd is
//! the server. On each wake the device opens the socket and pushes its status;
//! we also actively call `Shelly.GetStatus` on connect so a reading is always
//! captured. Readings are stored in SQLite and served as JSON to the web UI.

mod blu;
mod sensor_config;
mod thermostat;

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::{Html, IntoResponse, Json};
use axum::routing::{any, get, post};
use axum::Router;
use rusqlite::Connection;
use serde_json::{json, Value};

const INDEX_HTML: &str = include_str!("../static/index.html");

// PWA assets are embedded too, so the binary stays self-contained and can be
// copied to another host without carrying a static/ directory alongside it.
const MANIFEST_JSON: &str = include_str!("../static/manifest.webmanifest");
const SERVICE_WORKER_JS: &str = include_str!("../static/sw.js");
const ICON_192_PNG: &[u8] = include_bytes!("../static/icon-192.png");
const ICON_512_PNG: &[u8] = include_bytes!("../static/icon-512.png");
const APPLE_TOUCH_ICON_PNG: &[u8] = include_bytes!("../static/apple-touch-icon.png");

/// Minimum seconds between stored rows, so a single wake (which may deliver both
/// a GetStatus response and a NotifyFullStatus) yields one row, not several.
const MIN_INSERT_INTERVAL_SECS: i64 = 5;

/// Readings are stored in °C, exactly as the Shelly H&T reports them.
pub(crate) const READINGS_SCHEMA: &str = "CREATE TABLE IF NOT EXISTS readings (
        id       INTEGER PRIMARY KEY AUTOINCREMENT,
        ts       INTEGER NOT NULL,
        temp_c   REAL    NOT NULL,
        humidity REAL    NOT NULL
    );
    CREATE INDEX IF NOT EXISTS idx_readings_ts ON readings(ts);";

#[derive(Clone)]
struct AppState {
    db: Arc<Mutex<Connection>>,
    /// Last-known values, so a partial NotifyStatus can still form a full row.
    last: Arc<Mutex<LastReading>>,
    last_insert: Arc<AtomicI64>,
    /// What the thermostat controller last saw and did.
    heater: Arc<Mutex<thermostat::ControllerStatus>>,
    /// Wakes the controller early, e.g. right after settings change.
    wake: Arc<tokio::sync::Notify>,
    /// ORCHIDD_RELAY_ALIVE: the heartbeat file the iMessage relay touches.
    /// Its age is reported in /api/status so the watcher can tell when the
    /// relay has died.
    relay_alive: Option<PathBuf>,
    /// Packet id of the last BLU broadcast stored: the sensor repeats each
    /// broadcast several times, and the plug forwards every copy it hears.
    blu_last_packet: Arc<Mutex<Option<u8>>>,
}

#[derive(Default)]
struct LastReading {
    temp_c: Option<f64>,
    humidity: Option<f64>,
}

pub(crate) fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn env_or(key: &str, default: &str) -> String {
    std::env::var(key).unwrap_or_else(|_| default.to_string())
}

/// Seconds since `path` was last modified, never negative. None when there is
/// no path to watch or the file is missing or unreadable.
fn relay_age(path: Option<&Path>, now: SystemTime) -> Option<i64> {
    let mtime = std::fs::metadata(path?).and_then(|m| m.modified()).ok()?;
    Some(now.duration_since(mtime).map_or(0, |d| d.as_secs() as i64))
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
    conn.execute_batch(READINGS_SCHEMA).expect("init schema");
    conn.execute_batch(blu::BLU_SCHEMA)
        .expect("init blu schema");
    thermostat::init_schema(&conn).expect("init thermostat schema");
    // ORCHIDD_PLUG_URL only seeds the first run; afterwards plug_url lives in
    // the settings table and is edited through the API.
    let plug_url = env_or("ORCHIDD_PLUG_URL", thermostat::DEFAULT_PLUG_URL);
    thermostat::seed_settings(&conn, &thermostat::Settings::defaults(&plug_url))
        .expect("seed thermostat settings");

    let state = AppState {
        db: Arc::new(Mutex::new(conn)),
        last: Arc::new(Mutex::new(LastReading::default())),
        last_insert: Arc::new(AtomicI64::new(0)),
        heater: Arc::new(Mutex::new(thermostat::ControllerStatus::default())),
        wake: Arc::new(tokio::sync::Notify::new()),
        blu_last_packet: Arc::new(Mutex::new(None)),
        relay_alive: std::env::var("ORCHIDD_RELAY_ALIVE")
            .ok()
            .map(|p| p.trim().to_string())
            .filter(|p| !p.is_empty())
            .map(PathBuf::from),
    };

    let controller = thermostat::Controller::new(
        thermostat::ShellyPlug::new(),
        state.db.clone(),
        state.heater.clone(),
    );
    tokio::spawn(thermostat::run(controller, state.wake.clone()));

    let app = Router::new()
        .route("/", get(|| async { Html(INDEX_HTML) }))
        // The Shelly outbound-websocket URL points here. Any path works, but the
        // setup instructions use /temperature to match what's already entered.
        .route("/temperature", any(ws_handler))
        .route("/shelly", any(ws_handler))
        .route("/api/data", get(api_data))
        .route("/api/meta", get(api_meta))
        .route("/api/stats", get(api_stats))
        .route("/api/status", get(api_status))
        .route("/api/settings", get(api_settings_get).put(api_settings_put))
        .route("/api/heater", get(api_heater))
        .route("/api/blu", post(api_blu))
        // Progressive-web-app assets, so the graph installs to a phone's home
        // screen and runs full-screen like a native app.
        .route("/manifest.webmanifest", get(asset_manifest))
        .route("/sw.js", get(asset_sw))
        .route("/icon-192.png", get(asset_icon_192))
        .route("/icon-512.png", get(asset_icon_512))
        .route("/apple-touch-icon.png", get(asset_apple_icon))
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
    // Then check its offsets against the settings (see sensor_config).
    for r in sensor_config::config_requests() {
        let _ = socket.send(Message::Text(r.into())).await;
    }

    while let Some(Ok(msg)) = socket.recv().await {
        match msg {
            Message::Text(t) => {
                process_message(t.as_str(), &state);
                if let Some(fix) = offset_fix_for(t.as_str(), &state) {
                    let _ = socket.send(Message::Text(fix.into())).await;
                }
            }
            Message::Ping(p) => {
                let _ = socket.send(Message::Pong(p)).await;
            }
            Message::Close(_) => break,
            _ => {}
        }
    }
    println!("shelly disconnected");
}

/// The SetConfig to send when this frame shows a sensor offset that differs
/// from the settings; logs the sensor's answers to earlier SetConfigs.
fn offset_fix_for(text: &str, state: &AppState) -> Option<String> {
    let frame: Value = serde_json::from_str(text).ok()?;
    if let Some(line) = sensor_config::set_result(&frame) {
        println!("{line}");
        return None;
    }
    let settings = {
        let db = state.db.lock().unwrap();
        thermostat::load_settings(&db).ok()?
    };
    let fix = sensor_config::offset_fix(
        &frame,
        settings.sensor_temp_offset_f,
        settings.sensor_humidity_offset,
    )?;
    println!("sending sensor offset: {fix}");
    Some(fix)
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
// Static PWA assets (embedded at compile time).
// ---------------------------------------------------------------------------

/// Build a response with an explicit content type; browsers refuse to register
/// a service worker or parse a manifest served as the wrong type.
fn asset(content_type: &'static str, body: impl Into<axum::body::Body>) -> impl IntoResponse {
    (
        [(axum::http::header::CONTENT_TYPE, content_type)],
        body.into(),
    )
}

async fn asset_manifest() -> impl IntoResponse {
    asset("application/manifest+json", MANIFEST_JSON)
}
async fn asset_sw() -> impl IntoResponse {
    asset("text/javascript", SERVICE_WORKER_JS)
}
async fn asset_icon_192() -> impl IntoResponse {
    asset("image/png", ICON_192_PNG)
}
async fn asset_icon_512() -> impl IntoResponse {
    asset("image/png", ICON_512_PNG)
}
async fn asset_apple_icon() -> impl IntoResponse {
    asset("image/png", APPLE_TOUCH_ICON_PNG)
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
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, f64>(1)?,
                    r.get::<_, f64>(2)?,
                ))
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

    let blu_rows = {
        let db = state.db.lock().unwrap();
        blu::since(&db, cutoff).unwrap_or_default()
    };
    let blu_stride = (blu_rows.len() / MAX_POINTS).max(1);
    let (mut bt, mut btemp, mut bhum) = (Vec::new(), Vec::new(), Vec::new());
    for (i, (ts, tc, rh)) in blu_rows.iter().enumerate() {
        if i % blu_stride != 0 && i != blu_rows.len() - 1 {
            continue;
        }
        bt.push(ts * 1000);
        btemp.push(((tc * 9.0 / 5.0 + 32.0) * 100.0).round() / 100.0);
        bhum.push(rh.map(|h| (h * 100.0).round() / 100.0));
    }

    Json(json!({
        "unit": "F",
        "t": t,
        "tempF": temp_f,
        "hum": hum,
        "blu": { "t": bt, "tempF": btemp, "hum": bhum },
    }))
}

#[derive(serde::Deserialize)]
struct BluPost {
    /// BTHome service data (UUID 0xFCD2) as hex, as the plug script sends it.
    data: String,
}

/// A BLU sensor broadcast forwarded by the plug's script.
async fn api_blu(State(state): State<AppState>, Json(body): Json<BluPost>) -> impl IntoResponse {
    let Some(r) = blu::parse_bthome(&body.data) else {
        return (StatusCode::BAD_REQUEST, "not a plain BTHome v2 packet");
    };
    {
        let mut last = state.blu_last_packet.lock().unwrap();
        if r.packet_id.is_some() && *last == r.packet_id {
            return (StatusCode::OK, "duplicate");
        }
        *last = r.packet_id;
    }
    let db = state.db.lock().unwrap();
    match blu::store(&db, now_unix(), &r) {
        Ok(true) => {
            let tf = r.temp_c.unwrap_or_default() * 9.0 / 5.0 + 32.0;
            println!("blu     {tf:.1}°F  {:?}%RH", r.humidity);
            (StatusCode::OK, "stored")
        }
        Ok(false) => (StatusCode::OK, "no temperature"),
        Err(e) => {
            eprintln!("blu insert error: {e}");
            (StatusCode::INTERNAL_SERVER_ERROR, "db error")
        }
    }
}

/// High/low temperature and humidity over each summary window.
///
/// Windows that contain no readings yet report nulls rather than being omitted,
/// so the UI can list them consistently and just grey out the empty ones.
async fn api_stats(State(state): State<AppState>) -> impl IntoResponse {
    const WINDOWS: [(&str, f64); 3] = [("24h", 1.0), ("48h", 2.0), ("7d", 7.0)];

    let now = now_unix();
    let db = state.db.lock().unwrap();

    let windows: Vec<Value> = WINDOWS
        .iter()
        .map(|(key, days)| {
            let cutoff = now - (days * 86400.0) as i64;
            let row: (Option<f64>, Option<f64>, Option<f64>, Option<f64>, i64) = db
                .query_row(
                    // The BLU sensor: the one graphed and the one the thermostat follows.
                    "SELECT MIN(temp_c), MAX(temp_c), MIN(humidity), MAX(humidity), COUNT(*)
                     FROM blu_readings WHERE ts >= ?1",
                    [cutoff],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
                )
                .unwrap_or((None, None, None, None, 0));

            let (t_min, t_max, h_min, h_max, count) = row;
            // Convert °C to °F and round to one decimal, matching the graph.
            let f = |c: Option<f64>| c.map(|c| ((c * 9.0 / 5.0 + 32.0) * 10.0).round() / 10.0);
            let rh = |v: Option<f64>| v.map(|v| (v * 10.0).round() / 10.0);

            json!({
                "key": key,
                "days": days,
                "count": count,
                "tempMinF": f(t_min),
                "tempMaxF": f(t_max),
                "humMin": rh(h_min),
                "humMax": rh(h_max),
            })
        })
        .collect();

    Json(json!({ "unit": "F", "windows": windows }))
}

/// Report the span of stored data and the default window to show, per the
/// growth rule: start at 1 day, grow one whole day at a time as data
/// accumulates, cap at 7 days.
async fn api_meta(State(state): State<AppState>) -> impl IntoResponse {
    let (min_ts, max_ts, count): (Option<i64>, Option<i64>, i64) = {
        let db = state.db.lock().unwrap();
        db.query_row("SELECT MIN(ts), MAX(ts), COUNT(*) FROM readings", [], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?))
        })
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

// ---------------------------------------------------------------------------
// Thermostat API.
// ---------------------------------------------------------------------------

/// Current temperature, period, setpoint, heater state and any alert, plus
/// the age of the iMessage relay's heartbeat when one is watched.
async fn api_status(State(state): State<AppState>) -> impl IntoResponse {
    let ctrl = state.heater.lock().unwrap().clone();
    let mut v = {
        let db = state.db.lock().unwrap();
        thermostat::status_json(&db, &ctrl, now_unix())
    };
    let watched = state.relay_alive.as_deref();
    v["relay_watched"] = json!(watched.is_some());
    v["relay_age_s"] = json!(relay_age(watched, SystemTime::now()));
    Json(v)
}

async fn api_settings_get(State(state): State<AppState>) -> axum::response::Response {
    let loaded = thermostat::load_settings(&state.db.lock().unwrap());
    match loaded {
        Ok(s) => Json(s).into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": e })),
        )
            .into_response(),
    }
}

/// Partial update: any subset of settings as a JSON object. The merged result
/// is validated as a whole; on error nothing is saved and the reply is a 400
/// with `{"error": "..."}`.
async fn api_settings_put(
    State(state): State<AppState>,
    body: axum::body::Bytes,
) -> axum::response::Response {
    let bad =
        |msg: String| (StatusCode::BAD_REQUEST, Json(json!({ "error": msg }))).into_response();
    let patch: Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(e) => return bad(format!("body is not valid JSON: {e}")),
    };
    let result = {
        let db = state.db.lock().unwrap();
        thermostat::load_settings(&db)
            .and_then(|current| thermostat::apply_patch(&current, &patch))
            .and_then(|next| thermostat::save_settings(&db, &next).map(|()| next))
    };
    match result {
        Ok(next) => {
            state.wake.notify_one();
            Json(next).into_response()
        }
        Err(e) => bad(e),
    }
}

/// Heater-on intervals for the graph, over the same `days` window as
/// /api/data. Times are ms epoch, matching /api/data's `t`.
async fn api_heater(
    State(state): State<AppState>,
    Query(q): Query<DataQuery>,
) -> impl IntoResponse {
    let days = q.days.unwrap_or(7.0).clamp(0.01, 3650.0);
    let now = now_unix();
    let cutoff = now - (days * 86400.0) as i64;
    let intervals = thermostat::heater_intervals(&state.db.lock().unwrap(), cutoff, now);
    let on: Vec<[i64; 2]> = intervals
        .iter()
        .map(|(a, b)| [a * 1000, b * 1000])
        .collect();
    Json(json!({ "intervals": on }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn relay_age_is_seconds_since_the_heartbeat() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("orchid-relay.alive");
        std::fs::write(&path, "").unwrap();
        let mtime = std::fs::metadata(&path).unwrap().modified().unwrap();
        let later = mtime + Duration::from_secs(125);
        assert_eq!(relay_age(Some(&path), later), Some(125));
        // A clock behind the file's mtime is clamped, not negative.
        let earlier = mtime - Duration::from_secs(30);
        assert_eq!(relay_age(Some(&path), earlier), Some(0));
    }

    #[test]
    fn relay_age_is_none_when_unwatched_or_missing() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(relay_age(None, SystemTime::now()), None);
        let missing = dir.path().join("nope.alive");
        assert_eq!(relay_age(Some(&missing), SystemTime::now()), None);
    }
}
