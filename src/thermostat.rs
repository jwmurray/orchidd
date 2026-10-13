//! Thermostat: drives a heater on a Shelly Plus Plug US from the H&T readings.
//!
//! Readings are stored in °C (as the Shelly H&T reports them); everything in
//! this module — settings, decisions, the API — is in °F, converted with
//! [`c_to_f`] at the point a reading is read back out of SQLite.
//!
//! Safety model: every "on" command carries `toggle_after=900`, so the plug
//! turns itself off 15 minutes later unless orchidd refreshes it. If orchidd
//! dies, hangs or loses the network, the heater stops on its own.

use std::future::Future;
use std::str::FromStr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use chrono::{DateTime, NaiveTime, TimeZone, Utc};
use chrono_tz::Tz;
use rusqlite::{Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

pub const DEFAULT_PLUG_URL: &str = "http://192.168.0.30";
/// The Shelly H&T polled for readings; empty disables polling.
pub const DEFAULT_SENSOR_URL: &str = "http://192.168.0.39";
/// Seconds the plug keeps an "on" before switching itself off (dead-man timer).
pub const TOGGLE_AFTER_SECS: u32 = 900;
/// Re-send the self-expiring "on" at least this often while heating.
pub const REFRESH_INTERVAL_SECS: i64 = 300;
/// Controller loop period.
pub const LOOP_INTERVAL: Duration = Duration::from_secs(30);

pub fn c_to_f(c: f64) -> f64 {
    c * 9.0 / 5.0 + 32.0
}

fn round_to(v: f64, places: i32) -> f64 {
    let m = 10f64.powi(places);
    (v * m).round() / m
}

// ---------------------------------------------------------------------------
// Settings
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    Auto,
    Off,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Settings {
    pub mode: Mode,
    pub plug_url: String,
    pub timezone: String,
    pub day_start: String,
    pub day_end: String,
    pub day_setpoint_f: f64,
    pub night_setpoint_f: f64,
    pub hysteresis_f: f64,
    pub max_temp_f: f64,
    pub stale_minutes: u32,
    pub alert_below_f: f64,
    pub alert_phone: String,
    pub alert_email: String,
    pub alerts_enabled: bool,
    /// Sensor base URL for active polling; empty string disables polling.
    pub sensor_url: String,
    pub sensor_poll_secs: u32,
}

impl Settings {
    pub fn defaults(plug_url: &str) -> Self {
        Settings {
            mode: Mode::Auto,
            plug_url: plug_url.to_string(),
            timezone: "America/Denver".into(),
            day_start: "07:00".into(),
            day_end: "21:00".into(),
            day_setpoint_f: 75.0,
            night_setpoint_f: 60.0,
            hysteresis_f: 1.0,
            max_temp_f: 85.0,
            stale_minutes: 20,
            alert_below_f: 1.5,
            alert_phone: String::new(),
            alert_email: String::new(),
            alerts_enabled: true,
            sensor_url: DEFAULT_SENSOR_URL.into(),
            sensor_poll_secs: 30,
        }
    }

    pub fn validate(&self) -> Result<(), String> {
        for (name, v) in [
            ("day_setpoint_f", self.day_setpoint_f),
            ("night_setpoint_f", self.night_setpoint_f),
        ] {
            if !(32.0..=95.0).contains(&v) {
                return Err(format!("{name} must be between 32 and 95 °F"));
            }
        }
        if !(0.2..=5.0).contains(&self.hysteresis_f) {
            return Err("hysteresis_f must be between 0.2 and 5 °F".into());
        }
        let highest_off = self.day_setpoint_f.max(self.night_setpoint_f) + self.hysteresis_f;
        if self.max_temp_f <= highest_off || self.max_temp_f > 120.0 {
            return Err(format!(
                "max_temp_f must be above the highest setpoint plus hysteresis ({highest_off:.1} °F) and at most 120 °F"
            ));
        }
        let start = parse_hhmm(&self.day_start).map_err(|e| format!("day_start: {e}"))?;
        let end = parse_hhmm(&self.day_end).map_err(|e| format!("day_end: {e}"))?;
        if start == end {
            return Err("day_start and day_end must differ".into());
        }
        if Tz::from_str(&self.timezone).is_err() {
            return Err(format!(
                "timezone '{}' is not a known IANA zone (e.g. America/Denver)",
                self.timezone
            ));
        }
        if !is_http_url(&self.plug_url) {
            return Err("plug_url must look like http://192.168.0.30".into());
        }
        if !self.sensor_url.trim().is_empty() && !is_http_url(&self.sensor_url) {
            return Err(
                "sensor_url must look like http://192.168.0.39, or be empty to disable polling"
                    .into(),
            );
        }
        if !(10..=600).contains(&self.sensor_poll_secs) {
            return Err("sensor_poll_secs must be between 10 and 600".into());
        }
        if !(6..=240).contains(&self.stale_minutes) {
            return Err(
                "stale_minutes must be between 6 and 240 (the sensor reports every ~5 minutes)"
                    .into(),
            );
        }
        if !(0.0..=20.0).contains(&self.alert_below_f) {
            return Err("alert_below_f must be between 0 and 20 °F".into());
        }
        Ok(())
    }

    fn tz(&self) -> Tz {
        Tz::from_str(&self.timezone).unwrap_or(chrono_tz::America::Denver)
    }

    pub fn setpoint_for(&self, period: Period) -> f64 {
        match period {
            Period::Day => self.day_setpoint_f,
            Period::Night => self.night_setpoint_f,
        }
    }

    /// Day is `[day_start, day_end)` local time; a window that wraps midnight
    /// (start later than end) is honoured too.
    pub fn period_at(&self, t: NaiveTime) -> Result<Period, String> {
        let start = parse_hhmm(&self.day_start)?;
        let end = parse_hhmm(&self.day_end)?;
        let day = if start < end {
            t >= start && t < end
        } else {
            t >= start || t < end
        };
        Ok(if day { Period::Day } else { Period::Night })
    }
}

/// `http://host…` or `https://host…` with a non-empty host.
fn is_http_url(url: &str) -> bool {
    let url = url.trim();
    url.strip_prefix("http://")
        .or_else(|| url.strip_prefix("https://"))
        .is_some_and(|h| !h.trim_matches('/').is_empty())
}

/// Strict `HH:MM`, 24-hour, two digits each.
pub fn parse_hhmm(s: &str) -> Result<NaiveTime, String> {
    let b = s.as_bytes();
    let ok = b.len() == 5 && b[2] == b':' && [0, 1, 3, 4].iter().all(|&i| b[i].is_ascii_digit());
    if !ok {
        return Err(format!("'{s}' is not a HH:MM time"));
    }
    NaiveTime::parse_from_str(s, "%H:%M").map_err(|_| format!("'{s}' is not a valid HH:MM time"))
}

/// Merge a partial JSON object onto `current`, then validate the result.
pub fn apply_patch(current: &Settings, patch: &Value) -> Result<Settings, String> {
    let Some(patch) = patch.as_object() else {
        return Err("body must be a JSON object of settings".into());
    };
    let mut merged = match serde_json::to_value(current) {
        Ok(Value::Object(m)) => m,
        _ => return Err("internal: settings did not serialize".into()),
    };
    for (k, v) in patch {
        if !merged.contains_key(k) {
            return Err(format!("unknown setting '{k}'"));
        }
        merged.insert(k.clone(), v.clone());
    }
    let next: Settings =
        serde_json::from_value(Value::Object(merged)).map_err(|e| format!("invalid value: {e}"))?;
    next.validate()?;
    Ok(next)
}

pub fn init_schema(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS settings (
            key   TEXT PRIMARY KEY,
            value TEXT NOT NULL
        );
        CREATE TABLE IF NOT EXISTS heater_events (
            id        INTEGER PRIMARY KEY AUTOINCREMENT,
            ts        INTEGER NOT NULL,
            heater_on INTEGER NOT NULL,
            reason    TEXT    NOT NULL,
            temp_f    REAL,
            apower    REAL
        );
        CREATE INDEX IF NOT EXISTS idx_heater_events_ts ON heater_events(ts);",
    )
}

/// Insert any setting not yet stored. Existing values are never overwritten.
pub fn seed_settings(conn: &Connection, defaults: &Settings) -> rusqlite::Result<()> {
    if let Ok(Value::Object(m)) = serde_json::to_value(defaults) {
        for (k, v) in m {
            conn.execute(
                "INSERT OR IGNORE INTO settings (key, value) VALUES (?1, ?2)",
                rusqlite::params![k, v.to_string()],
            )?;
        }
    }
    Ok(())
}

/// Load settings, layered over the built-in defaults so a key added in a later
/// version still has a value. Unknown stored keys are ignored.
pub fn load_settings(conn: &Connection) -> Result<Settings, String> {
    let mut merged = match serde_json::to_value(Settings::defaults(DEFAULT_PLUG_URL)) {
        Ok(Value::Object(m)) => m,
        _ => return Err("internal: settings did not serialize".into()),
    };
    let mut stmt = conn
        .prepare("SELECT key, value FROM settings")
        .map_err(|e| e.to_string())?;
    let rows = stmt
        .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))
        .map_err(|e| e.to_string())?;
    for (k, v) in rows.filter_map(Result::ok) {
        if merged.contains_key(&k) {
            if let Ok(v) = serde_json::from_str::<Value>(&v) {
                merged.insert(k, v);
            }
        }
    }
    serde_json::from_value(Value::Object(merged)).map_err(|e| format!("stored settings: {e}"))
}

pub fn save_settings(conn: &Connection, s: &Settings) -> Result<(), String> {
    let Ok(Value::Object(m)) = serde_json::to_value(s) else {
        return Err("internal: settings did not serialize".into());
    };
    for (k, v) in m {
        conn.execute(
            "INSERT INTO settings (key, value) VALUES (?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            rusqlite::params![k, v.to_string()],
        )
        .map_err(|e| e.to_string())?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Decision
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Period {
    Day,
    Night,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Decision {
    pub want_on: bool,
    pub reason: &'static str,
    pub period: Period,
    pub setpoint_f: f64,
}

/// Pure heater decision. Precedence: mode off → stale/no reading → max temp →
/// hysteresis (on at or below setpoint, off at or above setpoint + hysteresis,
/// otherwise keep the current state).
pub fn decide(
    temp_f: Option<f64>,
    reading_age: Option<Duration>,
    s: &Settings,
    now_local: &DateTime<Tz>,
    currently_on: bool,
) -> Decision {
    let Ok(period) = s.period_at(now_local.time()) else {
        return Decision {
            want_on: false,
            reason: "invalid_settings",
            period: Period::Night,
            setpoint_f: s.night_setpoint_f,
        };
    };
    let setpoint_f = s.setpoint_for(period);
    let out = |want_on, reason| Decision {
        want_on,
        reason,
        period,
        setpoint_f,
    };

    if s.mode == Mode::Off {
        return out(false, "mode_off");
    }
    let stale_after = Duration::from_secs(u64::from(s.stale_minutes) * 60);
    let temp = match (temp_f, reading_age) {
        (Some(t), Some(age)) if age < stale_after => t,
        _ => return out(false, "sensor_stale"),
    };
    if temp >= s.max_temp_f {
        return out(false, "max_temp");
    }
    if temp <= setpoint_f {
        return out(true, "below_setpoint");
    }
    if temp >= setpoint_f + s.hysteresis_f {
        return out(false, "above_off_point");
    }
    if currently_on {
        out(true, "deadband_keep_on")
    } else {
        out(false, "deadband_keep_off")
    }
}

// ---------------------------------------------------------------------------
// Plug access
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PlugStatus {
    pub output: bool,
    pub apower: Option<f64>,
}

/// Access to the heater's smart plug. `base` is the plug URL from settings.
pub trait Plug: Send + Sync + 'static {
    fn get_status(&self, base: &str) -> impl Future<Output = Result<PlugStatus, String>> + Send;
    fn set_on(
        &self,
        base: &str,
        toggle_after_secs: u32,
    ) -> impl Future<Output = Result<(), String>> + Send;
    fn set_off(&self, base: &str) -> impl Future<Output = Result<(), String>> + Send;
}

/// Shelly Gen2 RPC over plain HTTP (no auth).
pub struct ShellyPlug {
    client: reqwest::Client,
}

impl ShellyPlug {
    pub fn new() -> Self {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(5))
            .build()
            .expect("http client");
        ShellyPlug { client }
    }

    async fn rpc(&self, base: &str, path_and_query: &str) -> Result<Value, String> {
        let url = format!("{}{}", base.trim().trim_end_matches('/'), path_and_query);
        let res = self.client.get(&url).send().await.map_err(|e| {
            // reqwest's Display omits the cause ("connection refused", "timed out").
            let cause = std::error::Error::source(&e)
                .and_then(std::error::Error::source)
                .map(|c| format!(" ({c})"))
                .unwrap_or_default();
            format!("plug unreachable: {e}{cause}")
        })?;
        let res = res
            .error_for_status()
            .map_err(|e| format!("plug error: {e}"))?;
        res.json::<Value>()
            .await
            .map_err(|e| format!("plug sent bad JSON: {e}"))
    }
}

impl Default for ShellyPlug {
    fn default() -> Self {
        Self::new()
    }
}

impl Plug for ShellyPlug {
    async fn get_status(&self, base: &str) -> Result<PlugStatus, String> {
        let v = self.rpc(base, "/rpc/Switch.GetStatus?id=0").await?;
        let output = v
            .get("output")
            .and_then(Value::as_bool)
            .ok_or("plug status has no 'output'")?;
        Ok(PlugStatus {
            output,
            apower: v.get("apower").and_then(Value::as_f64),
        })
    }

    async fn set_on(&self, base: &str, toggle_after_secs: u32) -> Result<(), String> {
        self.rpc(
            base,
            &format!("/rpc/Switch.Set?id=0&on=true&toggle_after={toggle_after_secs}"),
        )
        .await
        .map(|_| ())
    }

    async fn set_off(&self, base: &str) -> Result<(), String> {
        self.rpc(base, "/rpc/Switch.Set?id=0&on=false")
            .await
            .map(|_| ())
    }
}

// ---------------------------------------------------------------------------
// Controller
// ---------------------------------------------------------------------------

/// What the controller last saw and did; served by /api/status.
#[derive(Clone, Debug, Default, Serialize)]
pub struct ControllerStatus {
    pub heater_on: Option<bool>,
    pub apower_w: Option<f64>,
    pub controller_reason: Option<String>,
    pub plug_error: Option<String>,
    pub last_run_ts: Option<i64>,
}

/// Newest reading: (unix ts, temp °C, humidity %RH).
pub fn latest_reading(conn: &Connection) -> Option<(i64, f64, f64)> {
    conn.query_row(
        "SELECT ts, temp_c, humidity FROM readings ORDER BY ts DESC LIMIT 1",
        [],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
    )
    .optional()
    .ok()
    .flatten()
}

fn last_event_state(conn: &Connection) -> Option<bool> {
    conn.query_row(
        "SELECT heater_on FROM heater_events ORDER BY ts DESC, id DESC LIMIT 1",
        [],
        |r| r.get::<_, bool>(0),
    )
    .optional()
    .ok()
    .flatten()
}

pub struct Controller<P: Plug> {
    plug: P,
    db: Arc<Mutex<Connection>>,
    status: Arc<Mutex<ControllerStatus>>,
    /// When the self-expiring "on" was last sent.
    last_refresh: Option<i64>,
    /// Last state written to heater_events.
    last_logged: Option<bool>,
}

impl<P: Plug> Controller<P> {
    pub fn new(plug: P, db: Arc<Mutex<Connection>>, status: Arc<Mutex<ControllerStatus>>) -> Self {
        let last_logged = last_event_state(&db.lock().unwrap());
        Controller {
            plug,
            db,
            status,
            last_refresh: None,
            last_logged,
        }
    }

    /// One control iteration at unix time `now`.
    pub async fn step(&mut self, now: i64) {
        let (settings, reading) = {
            let db = self.db.lock().unwrap();
            (load_settings(&db), latest_reading(&db))
        };
        let settings = match settings {
            Ok(s) => s,
            Err(e) => {
                let mut st = self.status.lock().unwrap();
                st.controller_reason = Some("invalid_settings".into());
                st.plug_error = Some(e);
                st.last_run_ts = Some(now);
                return;
            }
        };
        let now_local = local_time(&settings, now);
        let temp_f = reading.map(|r| c_to_f(r.1));
        let age = reading.map(|r| Duration::from_secs((now - r.0).max(0) as u64));
        let base = settings.plug_url.clone();

        let observed = match self.plug.get_status(&base).await {
            Ok(s) => s,
            Err(e) => {
                let d = decide(temp_f, age, &settings, &now_local, false);
                eprintln!("thermostat: {e}");
                let mut st = self.status.lock().unwrap();
                *st = ControllerStatus {
                    heater_on: None,
                    apower_w: None,
                    controller_reason: Some(d.reason.into()),
                    plug_error: Some(e),
                    last_run_ts: Some(now),
                };
                return;
            }
        };

        let d = decide(temp_f, age, &settings, &now_local, observed.output);
        let mut error = None;
        let mut commanded = false;
        if d.want_on {
            let due = !observed.output
                || self
                    .last_refresh
                    .is_none_or(|t| now - t >= REFRESH_INTERVAL_SECS);
            if due {
                match self.plug.set_on(&base, TOGGLE_AFTER_SECS).await {
                    Ok(()) => {
                        self.last_refresh = Some(now);
                        commanded = true;
                    }
                    Err(e) => error = Some(e),
                }
            }
        } else {
            self.last_refresh = None;
            if observed.output {
                match self.plug.set_off(&base).await {
                    Ok(()) => commanded = true,
                    Err(e) => error = Some(e),
                }
            }
        }

        // Read back the real state after any command.
        let real = if commanded {
            match self.plug.get_status(&base).await {
                Ok(s) => s,
                Err(e) => {
                    error = Some(e);
                    PlugStatus {
                        output: d.want_on,
                        apower: None,
                    }
                }
            }
        } else {
            observed
        };

        if self.last_logged != Some(real.output) {
            let db = self.db.lock().unwrap();
            if let Err(e) = db.execute(
                "INSERT INTO heater_events (ts, heater_on, reason, temp_f, apower)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                rusqlite::params![now, real.output, d.reason, temp_f, real.apower],
            ) {
                eprintln!("thermostat: heater_events insert: {e}");
            }
            self.last_logged = Some(real.output);
            println!(
                "heater {}  ({}, temp {})",
                if real.output { "ON" } else { "OFF" },
                d.reason,
                temp_f.map_or("none".into(), |t| format!("{t:.1}°F"))
            );
        }

        if let Some(e) = &error {
            eprintln!("thermostat: {e}");
        }
        let mut st = self.status.lock().unwrap();
        *st = ControllerStatus {
            heater_on: Some(real.output),
            apower_w: real.apower,
            controller_reason: Some(d.reason.into()),
            plug_error: error,
            last_run_ts: Some(now),
        };
    }
}

fn local_time(s: &Settings, now: i64) -> DateTime<Tz> {
    let utc = Utc
        .timestamp_opt(now, 0)
        .single()
        .unwrap_or_else(|| Utc.timestamp_opt(0, 0).unwrap());
    utc.with_timezone(&s.tz())
}

/// Run the controller forever: every [`LOOP_INTERVAL`], or immediately when
/// `wake` is notified (e.g. after settings change).
pub async fn run<P: Plug>(mut c: Controller<P>, wake: Arc<tokio::sync::Notify>) {
    let mut tick = tokio::time::interval(LOOP_INTERVAL);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            _ = tick.tick() => {}
            _ = wake.notified() => {}
        }
        c.step(crate::now_unix()).await;
    }
}

// ---------------------------------------------------------------------------
// Status + graph helpers for the API
// ---------------------------------------------------------------------------

/// Pick the single most important alert, or None.
pub fn compute_alert(
    s: &Settings,
    temp_f: Option<f64>,
    reading_age_s: Option<i64>,
    setpoint_f: f64,
    plug_error: Option<&str>,
) -> Value {
    if let Some(e) = plug_error {
        return json!({ "kind": "plug_unreachable", "detail": e });
    }
    let stale = reading_age_s.is_none_or(|a| a >= i64::from(s.stale_minutes) * 60);
    if stale {
        let detail = match reading_age_s {
            None => "no sensor reading yet".to_string(),
            Some(a) => format!("last sensor reading {} minutes ago", a / 60),
        };
        return json!({ "kind": "sensor_stale", "detail": detail });
    }
    if let Some(t) = temp_f {
        if s.mode == Mode::Auto && t < setpoint_f - s.alert_below_f {
            return json!({
                "kind": "below_setpoint",
                "detail": format!("{t:.1}°F is more than {:.1}°F below the {setpoint_f:.1}°F setpoint", s.alert_below_f),
            });
        }
    }
    Value::Null
}

pub fn status_json(conn: &Connection, ctrl: &ControllerStatus, now: i64) -> Value {
    let settings = load_settings(conn).unwrap_or_else(|_| Settings::defaults(DEFAULT_PLUG_URL));
    let reading = latest_reading(conn);
    let temp_f = reading.map(|r| c_to_f(r.1));
    let age = reading.map(|r| (now - r.0).max(0));
    let period = settings
        .period_at(local_time(&settings, now).time())
        .unwrap_or(Period::Night);
    let setpoint_f = settings.setpoint_for(period);
    let alert = compute_alert(
        &settings,
        temp_f,
        age,
        setpoint_f,
        ctrl.plug_error.as_deref(),
    );
    json!({
        "temp_f": temp_f.map(|t| round_to(t, 2)),
        "humidity": reading.map(|r| round_to(r.2, 2)),
        "reading_ts": reading.map(|r| r.0),
        "reading_age_s": age,
        "period": period,
        "setpoint_f": setpoint_f,
        "off_at_f": round_to(setpoint_f + settings.hysteresis_f, 2),
        "heater_on": ctrl.heater_on,
        "apower_w": ctrl.apower_w,
        "controller_reason": ctrl.controller_reason,
        "plug_error": ctrl.plug_error,
        "mode": settings.mode,
        "alert": alert,
    })
}

/// Heater-on intervals overlapping `[from, to]`, as unix-second pairs. An
/// interval still open at `to` ends at `to`.
pub fn heater_intervals(conn: &Connection, from: i64, to: i64) -> Vec<(i64, i64)> {
    let before: Option<bool> = conn
        .query_row(
            "SELECT heater_on FROM heater_events WHERE ts < ?1 ORDER BY ts DESC, id DESC LIMIT 1",
            [from],
            |r| r.get(0),
        )
        .optional()
        .ok()
        .flatten();
    let mut on_since = if before == Some(true) {
        Some(from)
    } else {
        None
    };
    let mut out = Vec::new();
    let Ok(mut stmt) = conn.prepare(
        "SELECT ts, heater_on FROM heater_events WHERE ts >= ?1 AND ts <= ?2 ORDER BY ts, id",
    ) else {
        return out;
    };
    let Ok(rows) = stmt.query_map([from, to], |r| {
        Ok((r.get::<_, i64>(0)?, r.get::<_, bool>(1)?))
    }) else {
        return out;
    };
    for (ts, on) in rows.filter_map(Result::ok) {
        match (on, on_since) {
            (true, None) => on_since = Some(ts),
            (false, Some(start)) => {
                out.push((start, ts));
                on_since = None;
            }
            _ => {}
        }
    }
    if let Some(start) = on_since {
        out.push((start, to));
    }
    out
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use chrono_tz::America::Denver;

    fn s() -> Settings {
        Settings::defaults(DEFAULT_PLUG_URL)
    }

    fn at(y: i32, mo: u32, d: u32, h: u32, mi: u32) -> DateTime<Tz> {
        Denver
            .with_ymd_and_hms(y, mo, d, h, mi, 0)
            .single()
            .unwrap()
    }

    fn fresh() -> Option<Duration> {
        Some(Duration::from_secs(60))
    }

    // An ordinary October day: day setpoint 75, night 60, hysteresis 1.
    fn day() -> DateTime<Tz> {
        at(2026, 10, 12, 12, 0)
    }
    fn night() -> DateTime<Tz> {
        at(2026, 10, 12, 23, 0)
    }

    #[test]
    fn hysteresis_turns_on_at_or_below_setpoint() {
        let d = decide(Some(75.0), fresh(), &s(), &day(), false);
        assert!(d.want_on);
        assert_eq!(d.reason, "below_setpoint");
        assert!(decide(Some(74.2), fresh(), &s(), &day(), false).want_on);
    }

    #[test]
    fn hysteresis_turns_off_at_setpoint_plus_band() {
        let d = decide(Some(76.0), fresh(), &s(), &day(), true);
        assert!(!d.want_on);
        assert_eq!(d.reason, "above_off_point");
    }

    #[test]
    fn hysteresis_keeps_state_inside_band() {
        // Rising through the band while heating: stay on.
        let d = decide(Some(75.5), fresh(), &s(), &day(), true);
        assert!(d.want_on);
        assert_eq!(d.reason, "deadband_keep_on");
        // Falling through the band while off: stay off.
        let d = decide(Some(75.5), fresh(), &s(), &day(), false);
        assert!(!d.want_on);
        assert_eq!(d.reason, "deadband_keep_off");
    }

    #[test]
    fn night_uses_night_setpoint() {
        let d = decide(Some(65.0), fresh(), &s(), &night(), false);
        assert_eq!(d.period, Period::Night);
        assert_eq!(d.setpoint_f, 60.0);
        assert!(!d.want_on);
        assert!(decide(Some(59.9), fresh(), &s(), &night(), false).want_on);
    }

    #[test]
    fn day_night_boundaries() {
        // 65 °F: wants heat in the day (75), not at night (60).
        let cases = [
            ((6, 59), Period::Night, false),
            ((7, 0), Period::Day, true),
            ((20, 59), Period::Day, true),
            ((21, 0), Period::Night, false),
        ];
        for ((h, m), period, want_on) in cases {
            let d = decide(Some(65.0), fresh(), &s(), &at(2026, 10, 12, h, m), false);
            assert_eq!(d.period, period, "{h:02}:{m:02}");
            assert_eq!(d.want_on, want_on, "{h:02}:{m:02}");
        }
    }

    #[test]
    fn dst_transition_days_use_local_wall_clock() {
        let settings = s();
        let local = |utc_h: u32, utc_m: u32, y, mo, d| {
            Utc.with_ymd_and_hms(y, mo, d, utc_h, utc_m, 0)
                .single()
                .unwrap()
                .with_timezone(&Denver)
        };
        // Spring forward 2026-03-08: 07:00 MDT is 13:00 UTC.
        let d = decide(
            Some(65.0),
            fresh(),
            &settings,
            &local(13, 0, 2026, 3, 8),
            false,
        );
        assert_eq!(d.period, Period::Day);
        let d = decide(
            Some(65.0),
            fresh(),
            &settings,
            &local(12, 59, 2026, 3, 8),
            false,
        );
        assert_eq!(d.period, Period::Night);
        // The same UTC instant the day before is 06:00 MST: still night.
        let d = decide(
            Some(65.0),
            fresh(),
            &settings,
            &local(13, 0, 2026, 3, 7),
            false,
        );
        assert_eq!(d.period, Period::Night);
        // Fall back 2026-11-01: 07:00 MST is 14:00 UTC; 21:00 MST is 04:00 UTC next day.
        let d = decide(
            Some(65.0),
            fresh(),
            &settings,
            &local(13, 59, 2026, 11, 1),
            false,
        );
        assert_eq!(d.period, Period::Night);
        let d = decide(
            Some(65.0),
            fresh(),
            &settings,
            &local(14, 0, 2026, 11, 1),
            false,
        );
        assert_eq!(d.period, Period::Day);
        let d = decide(
            Some(65.0),
            fresh(),
            &settings,
            &local(3, 59, 2026, 11, 2),
            false,
        );
        assert_eq!(d.period, Period::Day);
        let d = decide(
            Some(65.0),
            fresh(),
            &settings,
            &local(4, 0, 2026, 11, 2),
            false,
        );
        assert_eq!(d.period, Period::Night);
    }

    #[test]
    fn stale_or_missing_reading_forces_off() {
        let d = decide(
            Some(50.0),
            Some(Duration::from_secs(20 * 60)),
            &s(),
            &day(),
            true,
        );
        assert!(!d.want_on);
        assert_eq!(d.reason, "sensor_stale");
        let d = decide(None, None, &s(), &day(), true);
        assert!(!d.want_on);
        assert_eq!(d.reason, "sensor_stale");
        // Just under the limit is still fresh.
        let d = decide(
            Some(50.0),
            Some(Duration::from_secs(20 * 60 - 1)),
            &s(),
            &day(),
            false,
        );
        assert!(d.want_on);
    }

    #[test]
    fn max_temp_cutoff_beats_everything_but_mode_and_stale() {
        let mut settings = s();
        settings.day_setpoint_f = 90.0;
        settings.max_temp_f = 85.0; // deliberately inconsistent: decide must still cut off
        let d = decide(Some(85.0), fresh(), &settings, &day(), true);
        assert!(!d.want_on);
        assert_eq!(d.reason, "max_temp");
    }

    #[test]
    fn mode_off_forces_off() {
        let mut settings = s();
        settings.mode = Mode::Off;
        let d = decide(Some(40.0), fresh(), &settings, &day(), true);
        assert!(!d.want_on);
        assert_eq!(d.reason, "mode_off");
    }

    // ---- settings validation ------------------------------------------------

    #[test]
    fn defaults_are_valid() {
        s().validate().unwrap();
    }

    fn patch(v: Value) -> Result<Settings, String> {
        apply_patch(&s(), &v)
    }

    #[test]
    fn validation_rejects_bad_values() {
        assert!(patch(json!({"day_setpoint_f": 31.9})).is_err());
        assert!(patch(json!({"night_setpoint_f": 95.1})).is_err());
        assert!(patch(json!({"hysteresis_f": 0.1})).is_err());
        assert!(patch(json!({"hysteresis_f": 5.1})).is_err());
        // max must exceed the highest setpoint + hysteresis (75 + 1).
        assert!(patch(json!({"max_temp_f": 76.0})).is_err());
        assert!(patch(json!({"max_temp_f": 76.1})).is_ok());
        assert!(patch(json!({"day_start": "7:00"})).is_err());
        assert!(patch(json!({"day_end": "24:00"})).is_err());
        assert!(patch(json!({"day_start": "21:00"})).is_err()); // equals day_end
        assert!(patch(json!({"timezone": "Mars/Olympus"})).is_err());
        assert!(patch(json!({"mode": "heat"})).is_err());
        assert!(patch(json!({"plug_url": "192.168.0.30"})).is_err());
        assert!(patch(json!({"stale_minutes": 2})).is_err());
        assert!(patch(json!({"bogus": 1})).is_err());
        assert!(patch(json!({"sensor_url": "192.168.0.39"})).is_err());
        assert!(patch(json!({"sensor_url": "http://"})).is_err());
        assert!(patch(json!({"sensor_poll_secs": 9})).is_err());
        assert!(patch(json!({"sensor_poll_secs": 601})).is_err());
        assert!(patch(json!({"sensor_poll_secs": -1})).is_err());
        assert!(patch(json!([1, 2])).is_err());
    }

    #[test]
    fn sensor_settings_defaults_and_accepted_values() {
        assert_eq!(s().sensor_url, "http://192.168.0.39");
        assert_eq!(s().sensor_poll_secs, 30);
        // Empty (or blank) disables polling and is valid.
        assert_eq!(patch(json!({"sensor_url": ""})).unwrap().sensor_url, "");
        assert!(patch(json!({"sensor_url": "  "})).is_ok());
        assert!(patch(json!({"sensor_url": "https://ht.local/"})).is_ok());
        assert_eq!(
            patch(json!({"sensor_poll_secs": 10}))
                .unwrap()
                .sensor_poll_secs,
            10
        );
        assert!(patch(json!({"sensor_poll_secs": 600})).is_ok());
    }

    #[test]
    fn settings_stored_before_sensor_keys_load_with_defaults() {
        // A database from before sensor polling has no sensor_* rows.
        let conn = Connection::open_in_memory().unwrap();
        init_schema(&conn).unwrap();
        seed_settings(&conn, &s()).unwrap();
        conn.execute(
            "DELETE FROM settings WHERE key IN ('sensor_url', 'sensor_poll_secs')",
            [],
        )
        .unwrap();
        let loaded = load_settings(&conn).unwrap();
        assert_eq!(loaded.sensor_url, DEFAULT_SENSOR_URL);
        assert_eq!(loaded.sensor_poll_secs, 30);
    }

    #[test]
    fn partial_update_changes_only_named_fields() {
        let next = patch(json!({"day_setpoint_f": 72.5, "mode": "off"})).unwrap();
        assert_eq!(next.day_setpoint_f, 72.5);
        assert_eq!(next.mode, Mode::Off);
        assert_eq!(next.night_setpoint_f, 60.0);
    }

    #[test]
    fn settings_round_trip_through_sqlite() {
        let conn = Connection::open_in_memory().unwrap();
        init_schema(&conn).unwrap();
        seed_settings(&conn, &Settings::defaults("http://10.0.0.5")).unwrap();
        assert_eq!(load_settings(&conn).unwrap().plug_url, "http://10.0.0.5");
        // Re-seeding never clobbers a stored value.
        seed_settings(&conn, &Settings::defaults("http://10.0.0.9")).unwrap();
        assert_eq!(load_settings(&conn).unwrap().plug_url, "http://10.0.0.5");
        let mut next = load_settings(&conn).unwrap();
        next.night_setpoint_f = 58.0;
        save_settings(&conn, &next).unwrap();
        assert_eq!(load_settings(&conn).unwrap(), next);
    }

    // ---- alerts ---------------------------------------------------------------

    #[test]
    fn alert_precedence() {
        let st = s();
        assert_eq!(
            compute_alert(&st, Some(70.0), Some(60), 75.0, Some("down"))["kind"],
            "plug_unreachable"
        );
        assert_eq!(
            compute_alert(&st, Some(70.0), Some(20 * 60), 75.0, None)["kind"],
            "sensor_stale"
        );
        assert_eq!(
            compute_alert(&st, Some(73.4), Some(60), 75.0, None)["kind"],
            "below_setpoint"
        );
        assert!(compute_alert(&st, Some(73.5), Some(60), 75.0, None).is_null());
    }

    // ---- controller with a fake plug -----------------------------------------

    #[derive(Default)]
    struct FakeState {
        output: bool,
        calls: Vec<String>,
        unreachable: bool,
    }

    #[derive(Clone, Default)]
    struct FakePlug(Arc<Mutex<FakeState>>);

    impl FakePlug {
        fn commands(&self) -> Vec<String> {
            self.0
                .lock()
                .unwrap()
                .calls
                .iter()
                .filter(|c| c.as_str() != "status")
                .cloned()
                .collect()
        }
    }

    impl Plug for FakePlug {
        async fn get_status(&self, _base: &str) -> Result<PlugStatus, String> {
            let mut st = self.0.lock().unwrap();
            if st.unreachable {
                return Err("plug unreachable: fake".into());
            }
            st.calls.push("status".into());
            Ok(PlugStatus {
                output: st.output,
                apower: Some(if st.output { 1400.0 } else { 0.0 }),
            })
        }
        async fn set_on(&self, _base: &str, toggle_after_secs: u32) -> Result<(), String> {
            let mut st = self.0.lock().unwrap();
            st.calls.push(format!("on:{toggle_after_secs}"));
            st.output = true;
            Ok(())
        }
        async fn set_off(&self, _base: &str) -> Result<(), String> {
            let mut st = self.0.lock().unwrap();
            st.calls.push("off".into());
            st.output = false;
            Ok(())
        }
    }

    fn test_db() -> Arc<Mutex<Connection>> {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(crate::READINGS_SCHEMA).unwrap();
        init_schema(&conn).unwrap();
        seed_settings(&conn, &s()).unwrap();
        Arc::new(Mutex::new(conn))
    }

    fn add_reading(db: &Arc<Mutex<Connection>>, ts: i64, temp_f: f64) {
        let temp_c = (temp_f - 32.0) * 5.0 / 9.0;
        db.lock()
            .unwrap()
            .execute(
                "INSERT INTO readings (ts, temp_c, humidity) VALUES (?1, ?2, 50.0)",
                rusqlite::params![ts, temp_c],
            )
            .unwrap();
    }

    // 2026-10-12 12:00 MDT (daytime, setpoint 75).
    const NOON: i64 = 1_791_828_000;

    #[tokio::test]
    async fn controller_refreshes_self_expiring_on_and_cuts_off_when_stale() {
        assert_eq!(
            Utc.timestamp_opt(NOON, 0)
                .unwrap()
                .with_timezone(&Denver)
                .format("%H:%M")
                .to_string(),
            "12:00"
        );
        let db = test_db();
        let plug = FakePlug::default();
        let status = Arc::new(Mutex::new(ControllerStatus::default()));
        let mut c = Controller::new(plug.clone(), db.clone(), status.clone());

        add_reading(&db, NOON, 70.0);
        c.step(NOON).await;
        assert_eq!(plug.commands(), ["on:900"]);
        assert_eq!(status.lock().unwrap().heater_on, Some(true));
        assert_eq!(status.lock().unwrap().apower_w, Some(1400.0));

        // 30 s and 4.5 min later: plug is on and the refresh is not due.
        c.step(NOON + 30).await;
        c.step(NOON + 270).await;
        assert_eq!(plug.commands(), ["on:900"]);

        // 5 min after the last "on": refresh it.
        c.step(NOON + 300).await;
        assert_eq!(plug.commands(), ["on:900", "on:900"]);

        // Plug expired on its own (e.g. a missed refresh): re-send immediately.
        plug.0.lock().unwrap().output = false;
        c.step(NOON + 330).await;
        assert_eq!(plug.commands(), ["on:900", "on:900", "on:900"]);

        // No new reading for 20 minutes: sensor stale → off.
        c.step(NOON + 20 * 60).await;
        assert_eq!(plug.commands(), ["on:900", "on:900", "on:900", "off"]);
        let st = status.lock().unwrap().clone();
        assert_eq!(st.heater_on, Some(false));
        assert_eq!(st.controller_reason.as_deref(), Some("sensor_stale"));

        // Events: on at NOON, (the external expiry at +330 was re-asserted within
        // the same step, so it never appears), off at +1200.
        let conn = db.lock().unwrap();
        assert_eq!(
            heater_intervals(&conn, NOON - 3600, NOON + 3600),
            vec![(NOON, NOON + 20 * 60)]
        );
    }

    #[tokio::test]
    async fn controller_turns_off_at_off_point_and_reports_unreachable() {
        let db = test_db();
        let plug = FakePlug::default();
        let status = Arc::new(Mutex::new(ControllerStatus::default()));
        let mut c = Controller::new(plug.clone(), db.clone(), status.clone());

        add_reading(&db, NOON, 74.0);
        c.step(NOON).await;
        add_reading(&db, NOON + 300, 76.0);
        c.step(NOON + 330).await;
        assert_eq!(plug.commands(), ["on:900", "off"]);

        plug.0.lock().unwrap().unreachable = true;
        c.step(NOON + 360).await;
        let st = status.lock().unwrap().clone();
        assert_eq!(st.heater_on, None);
        assert!(st.plug_error.unwrap().contains("unreachable"));
        let json = status_json(&db.lock().unwrap(), &status.lock().unwrap(), NOON + 360);
        assert_eq!(json["alert"]["kind"], "plug_unreachable");
    }

    #[test]
    fn intervals_carry_state_across_window_start() {
        let conn = Connection::open_in_memory().unwrap();
        init_schema(&conn).unwrap();
        for (ts, on) in [(100, true), (200, false), (300, true)] {
            conn.execute(
                "INSERT INTO heater_events (ts, heater_on, reason) VALUES (?1, ?2, 'x')",
                rusqlite::params![ts, on],
            )
            .unwrap();
        }
        assert_eq!(
            heater_intervals(&conn, 150, 400),
            vec![(150, 200), (300, 400)]
        );
        assert_eq!(heater_intervals(&conn, 0, 250), vec![(100, 200)]);
    }
}
