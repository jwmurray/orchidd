//! Active polling of the Shelly H&T over HTTP RPC.
//!
//! On batteries the H&T sleeps between its ~5-minute websocket pushes, so most
//! polls fail and are skipped quietly; the push keeps working as the fallback.
//! On USB power (batteries removed) it stays awake and answers every poll, so
//! readings arrive every `sensor_poll_secs`.

use std::future::Future;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use rusqlite::Connection;
use serde_json::Value;

use crate::thermostat::load_settings;

/// Per-request timeout; an asleep sensor should not hold the poller up.
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(3);

/// Read access to the temperature/humidity sensor. `base` is `sensor_url`.
pub trait Sensor: Send + Sync + 'static {
    /// Returns (temperature °C, humidity %RH).
    fn read(&self, base: &str) -> impl Future<Output = Result<(f64, f64), String>> + Send;
}

/// Shelly Gen3 RPC over plain HTTP (no auth).
pub struct ShellySensor {
    client: reqwest::Client,
}

impl ShellySensor {
    pub fn new() -> Self {
        let client = reqwest::Client::builder()
            .timeout(REQUEST_TIMEOUT)
            .build()
            .expect("http client");
        ShellySensor { client }
    }

    async fn rpc(&self, base: &str, path_and_query: &str) -> Result<Value, String> {
        let url = format!("{}{}", base.trim().trim_end_matches('/'), path_and_query);
        let res = self
            .client
            .get(&url)
            .send()
            .await
            .map_err(|e| format!("sensor unreachable: {e}"))?
            .error_for_status()
            .map_err(|e| format!("sensor error: {e}"))?;
        res.json::<Value>()
            .await
            .map_err(|e| format!("sensor sent bad JSON: {e}"))
    }
}

impl Default for ShellySensor {
    fn default() -> Self {
        Self::new()
    }
}

impl Sensor for ShellySensor {
    async fn read(&self, base: &str) -> Result<(f64, f64), String> {
        let t = self.rpc(base, "/rpc/Temperature.GetStatus?id=0").await?;
        let h = self.rpc(base, "/rpc/Humidity.GetStatus?id=0").await?;
        parse_readings(&t, &h)
    }
}

/// Pull `tC` and `rh` out of the two GetStatus replies. A null value (the
/// sensor reports one on a measurement error) is a failure, not a zero.
pub fn parse_readings(temp: &Value, hum: &Value) -> Result<(f64, f64), String> {
    let t = temp
        .get("tC")
        .and_then(Value::as_f64)
        .ok_or("Temperature.GetStatus has no numeric 'tC'")?;
    let h = hum
        .get("rh")
        .and_then(Value::as_f64)
        .ok_or("Humidity.GetStatus has no numeric 'rh'")?;
    Ok((t, h))
}

/// Outcome of the most recent poll; served by /api/status.
#[derive(Clone, Debug, Default)]
pub struct PollStatus {
    pub last_ok_ts: Option<i64>,
}

/// Polling counts as working when it is enabled and the last success is within
/// two intervals.
pub fn is_polling(status: &PollStatus, sensor_url: &str, poll_secs: u32, now: i64) -> bool {
    !sensor_url.trim().is_empty()
        && status
            .last_ok_ts
            .is_some_and(|t| now - t <= 2 * i64::from(poll_secs))
}

/// Debug-level log line, shown only when `ORCHIDD_DEBUG` is set.
fn debug(msg: &str) {
    if std::env::var_os("ORCHIDD_DEBUG").is_some() {
        eprintln!("sensor poll: {msg}");
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum PollOutcome {
    Disabled,
    Stored,
    Failed,
}

pub struct Poller<S: Sensor, R: Fn(f64, f64) + Send + Sync + 'static> {
    sensor: S,
    db: Arc<Mutex<Connection>>,
    status: Arc<Mutex<PollStatus>>,
    /// Stores a reading through the same path as the websocket push.
    record: R,
    /// Woken after each successful poll (the thermostat controller).
    wake: Arc<tokio::sync::Notify>,
}

impl<S: Sensor, R: Fn(f64, f64) + Send + Sync + 'static> Poller<S, R> {
    pub fn new(
        sensor: S,
        db: Arc<Mutex<Connection>>,
        status: Arc<Mutex<PollStatus>>,
        record: R,
        wake: Arc<tokio::sync::Notify>,
    ) -> Self {
        Poller {
            sensor,
            db,
            status,
            record,
            wake,
        }
    }

    /// Current (sensor_url, sensor_poll_secs); defaults if settings are unreadable.
    fn config(&self) -> (String, u32) {
        match load_settings(&self.db.lock().unwrap()) {
            Ok(s) => (s.sensor_url, s.sensor_poll_secs),
            Err(_) => (crate::thermostat::DEFAULT_SENSOR_URL.into(), 30),
        }
    }

    /// One poll at unix time `now`.
    pub async fn step(&self, now: i64) -> PollOutcome {
        let (url, _) = self.config();
        if url.trim().is_empty() {
            return PollOutcome::Disabled;
        }
        match self.sensor.read(&url).await {
            Ok((temp_c, humidity)) => {
                (self.record)(temp_c, humidity);
                self.status.lock().unwrap().last_ok_ts = Some(now);
                self.wake.notify_one();
                PollOutcome::Stored
            }
            Err(e) => {
                debug(&format!("skipped ({e})"));
                PollOutcome::Failed
            }
        }
    }
}

/// Poll forever, re-reading the interval from settings before each sleep so a
/// change takes effect on the next cycle.
pub async fn run<S: Sensor, R: Fn(f64, f64) + Send + Sync + 'static>(p: Poller<S, R>) {
    loop {
        p.step(crate::now_unix()).await;
        let (_, secs) = p.config();
        tokio::time::sleep(Duration::from_secs(u64::from(secs.clamp(10, 600)))).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parses_shelly_replies() {
        let t = json!({"id": 0, "tC": 21.9, "tF": 71.4});
        let h = json!({"id": 0, "rh": 68.6});
        assert_eq!(parse_readings(&t, &h).unwrap(), (21.9, 68.6));
        assert!(parse_readings(&json!({"id": 0, "tC": null}), &h).is_err());
        assert!(parse_readings(&t, &json!({"id": 0})).is_err());
    }

    #[test]
    fn polling_flag_needs_recent_success_and_a_url() {
        let ok = PollStatus {
            last_ok_ts: Some(1000),
        };
        assert!(is_polling(&ok, "http://x", 30, 1060));
        assert!(!is_polling(&ok, "http://x", 30, 1061));
        assert!(!is_polling(&ok, "", 30, 1001));
        assert!(!is_polling(&PollStatus::default(), "http://x", 30, 1000));
    }
}
