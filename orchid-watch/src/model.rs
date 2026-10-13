//! What orchidd's HTTP API returns, as the watcher reads it.
//!
//! These are the watcher's own copies, not orchidd's types: they accept
//! unknown fields and tolerate missing optional ones, so orchidd can grow its
//! API without breaking the thing that watches it.

use serde::{Deserialize, Serialize};

/// `GET /api/status`.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Status {
    #[serde(default)]
    pub temp_f: Option<f64>,
    #[serde(default)]
    pub reading_age_s: Option<i64>,
    /// "day" or "night".
    pub period: String,
    #[serde(default)]
    pub setpoint_f: Option<f64>,
    #[serde(default)]
    pub heater_on: Option<bool>,
    #[serde(default)]
    pub apower_w: Option<f64>,
    /// "auto" or "off".
    pub mode: String,
    #[serde(default)]
    pub alert: Option<Alert>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Alert {
    /// "below_setpoint", "sensor_stale" or "plug_unreachable".
    pub kind: String,
    #[serde(default)]
    pub detail: String,
}

impl Status {
    pub fn alert_kind(&self) -> Option<&str> {
        self.alert.as_ref().map(|a| a.kind.as_str())
    }
}

/// `GET /api/settings` — only the fields the watcher uses.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Settings {
    pub timezone: String,
    pub day_start: String,
    pub day_end: String,
    pub day_setpoint_f: f64,
    pub night_setpoint_f: f64,
    #[serde(default = "default_alert_below")]
    pub alert_below_f: f64,
    #[serde(default = "default_stale_minutes")]
    pub stale_minutes: u32,
    #[serde(default)]
    pub alert_phone: String,
    #[serde(default)]
    pub alert_email: String,
    #[serde(default = "default_true")]
    pub alerts_enabled: bool,
    pub mode: String,
}

fn default_alert_below() -> f64 {
    1.5
}
fn default_stale_minutes() -> u32 {
    20
}
fn default_true() -> bool {
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_orchidd_status_and_ignores_unknown_fields() {
        let s: Status = serde_json::from_str(
            r#"{"temp_f":58.12,"humidity":40.1,"reading_ts":1,"reading_age_s":120,
                "period":"night","setpoint_f":60.0,"off_at_f":61.0,"heater_on":true,
                "apower_w":905.2,"controller_reason":"below setpoint","plug_error":null,
                "mode":"auto","alert":{"kind":"below_setpoint","detail":"x"},"new_field":1}"#,
        )
        .unwrap();
        assert_eq!(s.alert_kind(), Some("below_setpoint"));
        assert_eq!(s.heater_on, Some(true));
        let s: Status = serde_json::from_str(
            r#"{"temp_f":null,"reading_age_s":null,"period":"day","setpoint_f":75,
                "heater_on":null,"apower_w":null,"mode":"off","alert":null}"#,
        )
        .unwrap();
        assert_eq!(s.alert_kind(), None);
        assert_eq!(s.temp_f, None);
    }

    #[test]
    fn rejects_something_that_is_not_orchidd() {
        assert!(serde_json::from_str::<Status>(r#"{"hello":"world"}"#).is_err());
    }

    #[test]
    fn parses_orchidd_settings() {
        let s: Settings = serde_json::from_str(
            r#"{"mode":"auto","plug_url":"http://192.168.0.30","timezone":"America/Denver",
                "day_start":"07:00","day_end":"21:00","day_setpoint_f":75.0,
                "night_setpoint_f":60.0,"hysteresis_f":1.0,"max_temp_f":85.0,
                "stale_minutes":20,"alert_below_f":1.5,"alert_phone":"801 555 0101",
                "alert_email":"rm@example.com","alerts_enabled":true}"#,
        )
        .unwrap();
        assert_eq!(s.alert_phone, "801 555 0101");
        assert!(s.alerts_enabled);
    }
}
