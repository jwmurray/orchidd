//! Keep the Wi-Fi H&T's own temperature and humidity offsets in line with
//! orchidd's settings. The sensor sleeps between reports and is reachable
//! only over the websocket it opens to us, so on each connect we ask for its
//! config and send a SetConfig only when an offset differs. The sensor then
//! applies the correction itself: its screen, the Shelly app and the readings
//! it sends here all show the corrected values.

use serde_json::{json, Value};

const GET_TEMP_ID: i64 = 2;
const GET_HUM_ID: i64 = 3;
const SET_TEMP_ID: i64 = 4;
const SET_HUM_ID: i64 = 5;

/// Requests sent on every connect, after Shelly.GetStatus.
pub(crate) fn config_requests() -> [String; 2] {
    [
        json!({"id": GET_TEMP_ID, "src": "orchidd", "method": "Temperature.GetConfig", "params": {"id": 0}})
            .to_string(),
        json!({"id": GET_HUM_ID, "src": "orchidd", "method": "Humidity.GetConfig", "params": {"id": 0}})
            .to_string(),
    ]
}

fn round2(v: f64) -> f64 {
    (v * 100.0).round() / 100.0
}

/// Given a frame from the sensor and the wanted offsets (°F, %RH), the
/// SetConfig to send when the sensor's offset differs, else None.
pub(crate) fn offset_fix(
    frame: &Value,
    temp_offset_f: f64,
    humidity_offset: f64,
) -> Option<String> {
    let id = frame.get("id")?.as_i64()?;
    let result = frame.get("result")?;
    let (have, want, method, key, set_id) = match id {
        GET_TEMP_ID => (
            result.get("offset_C")?.as_f64()?,
            round2(temp_offset_f * 5.0 / 9.0),
            "Temperature.SetConfig",
            "offset_C",
            SET_TEMP_ID,
        ),
        GET_HUM_ID => (
            result.get("offset")?.as_f64()?,
            round2(humidity_offset),
            "Humidity.SetConfig",
            "offset",
            SET_HUM_ID,
        ),
        _ => return None,
    };
    if (have - want).abs() < 0.005 {
        return None;
    }
    Some(
        json!({
            "id": set_id,
            "src": "orchidd",
            "method": method,
            "params": {"id": 0, "config": {key: want}},
        })
        .to_string(),
    )
}

/// A log line for the sensor's answer to one of our SetConfig calls.
pub(crate) fn set_result(frame: &Value) -> Option<String> {
    let id = frame.get("id")?.as_i64()?;
    let what = match id {
        SET_TEMP_ID => "temperature offset",
        SET_HUM_ID => "humidity offset",
        _ => return None,
    };
    Some(match frame.get("error") {
        Some(e) => format!("sensor rejected {what}: {e}"),
        None => format!("sensor {what} updated"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(s: &str) -> Value {
        serde_json::from_str(s).unwrap()
    }

    #[test]
    fn sets_the_temperature_offset_in_celsius_when_it_differs() {
        let frame = json!({"id": 2, "result": {"id": 0, "offset_C": 0.0, "report_thr_C": 1.0}});
        let fix = parse(&offset_fix(&frame, -1.4, 2.0).unwrap());
        assert_eq!(fix["method"], "Temperature.SetConfig");
        assert_eq!(fix["params"]["config"]["offset_C"], -0.78);
    }

    #[test]
    fn sets_the_humidity_offset_when_it_differs() {
        let frame = json!({"id": 3, "result": {"id": 0, "offset": 0.0}});
        let fix = parse(&offset_fix(&frame, -1.4, 2.0).unwrap());
        assert_eq!(fix["method"], "Humidity.SetConfig");
        assert_eq!(fix["params"]["config"]["offset"], 2.0);
    }

    #[test]
    fn leaves_a_matching_offset_alone() {
        // Avoids a flash write on every wake.
        let t = json!({"id": 2, "result": {"offset_C": -0.78}});
        let h = json!({"id": 3, "result": {"offset": 2.0}});
        assert_eq!(offset_fix(&t, -1.4, 2.0), None);
        assert_eq!(offset_fix(&h, -1.4, 2.0), None);
    }

    #[test]
    fn ignores_other_frames() {
        let status = json!({"id": 1, "result": {"temperature:0": {"tC": 22.0}}});
        let notify = json!({"method": "NotifyStatus", "params": {}});
        assert_eq!(offset_fix(&status, -1.4, 2.0), None);
        assert_eq!(offset_fix(&notify, -1.4, 2.0), None);
    }

    #[test]
    fn reports_set_results() {
        assert_eq!(
            set_result(&json!({"id": 4, "result": {"restart_required": false}})).as_deref(),
            Some("sensor temperature offset updated")
        );
        assert!(set_result(&json!({"id": 5, "error": {"code": -103}}))
            .unwrap()
            .starts_with("sensor rejected humidity offset"));
        assert_eq!(set_result(&json!({"id": 1, "result": {}})), None);
    }
}
