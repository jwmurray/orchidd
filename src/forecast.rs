//! Today's forecast high from the National Weather Service, so a sunny day
//! can warm the room instead of the heater.
//!
//! A background task refreshes the forecast hourly into `forecasts`; the
//! controller reads today's high from there. No network in the control loop.

use std::time::Duration;

use chrono::{DateTime, NaiveDate};
use rusqlite::{Connection, OptionalExtension};
use serde_json::Value;

/// `high_f` is the latest forecast for the date; `first_high_f` the first
/// one seen, kept to judge how well a forecast predicts the room.
pub const SCHEMA: &str = "CREATE TABLE IF NOT EXISTS forecasts (
    date         TEXT PRIMARY KEY,
    high_f       REAL    NOT NULL,
    fetched_ts   INTEGER NOT NULL,
    first_high_f REAL
);";

/// Daytime periods of an NWS `/forecast` response as (local date, high °F).
pub fn parse_daytime_highs(forecast: &Value) -> Vec<(NaiveDate, f64)> {
    let Some(periods) = forecast["properties"]["periods"].as_array() else {
        return Vec::new();
    };
    periods
        .iter()
        .filter(|p| p["isDaytime"].as_bool() == Some(true))
        .filter_map(|p| {
            let start = DateTime::parse_from_rfc3339(p["startTime"].as_str()?).ok()?;
            let t = p["temperature"].as_f64()?;
            let f = match p["temperatureUnit"].as_str() {
                Some("C") => t * 9.0 / 5.0 + 32.0,
                _ => t,
            };
            Some((start.date_naive(), f))
        })
        .collect()
}

pub fn store_highs(
    conn: &Connection,
    highs: &[(NaiveDate, f64)],
    now: i64,
) -> rusqlite::Result<()> {
    for (date, high) in highs {
        conn.execute(
            "INSERT INTO forecasts (date, high_f, fetched_ts, first_high_f) VALUES (?1, ?2, ?3, ?2)
             ON CONFLICT(date) DO UPDATE SET high_f = excluded.high_f, fetched_ts = excluded.fetched_ts",
            rusqlite::params![date.to_string(), high, now],
        )?;
    }
    Ok(())
}

pub fn high_for(conn: &Connection, date: NaiveDate) -> Option<f64> {
    conn.query_row(
        "SELECT high_f FROM forecasts WHERE date = ?1",
        [date.to_string()],
        |r| r.get(0),
    )
    .optional()
    .ok()
    .flatten()
}

/// When NWS last answered, for spotting a fetch that has quietly stopped.
pub fn last_fetch_ts(conn: &Connection) -> Option<i64> {
    conn.query_row("SELECT MAX(fetched_ts) FROM forecasts", [], |r| r.get(0))
        .ok()
        .flatten()
}

/// One row per day: the forecast against what the room did.
#[derive(Debug, PartialEq, serde::Serialize)]
pub struct DayRecord {
    pub date: String,
    pub forecast_high_f: Option<f64>,
    pub first_forecast_high_f: Option<f64>,
    pub room_high_f: Option<f64>,
    pub room_low_f: Option<f64>,
}

/// The last `days` local days (today included, newest first), from the
/// forecasts table and the BLU readings.
pub fn history(conn: &Connection, tz: chrono_tz::Tz, now: i64, days: u32) -> Vec<DayRecord> {
    use chrono::{Days, TimeZone, Utc};
    let today = Utc
        .timestamp_opt(now, 0)
        .single()
        .map_or_else(|| Utc::now().with_timezone(&tz), |t| t.with_timezone(&tz))
        .date_naive();
    let bounds = |d: NaiveDate| {
        let start = tz
            .from_local_datetime(&d.and_hms_opt(0, 0, 0)?)
            .earliest()?;
        let next = d.checked_add_days(Days::new(1))?;
        let end = tz
            .from_local_datetime(&next.and_hms_opt(0, 0, 0)?)
            .earliest()?;
        Some((start.timestamp(), end.timestamp()))
    };
    (0..days)
        .filter_map(|i| today.checked_sub_days(Days::new(u64::from(i))))
        .map(|d| {
            let (fc, first) = conn
                .query_row(
                    "SELECT high_f, first_high_f FROM forecasts WHERE date = ?1",
                    [d.to_string()],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .optional()
                .ok()
                .flatten()
                .unwrap_or((None, None));
            let (hi, lo) = bounds(d)
                .and_then(|(a, b)| {
                    conn.query_row(
                        "SELECT MAX(temp_c), MIN(temp_c) FROM blu_readings
                         WHERE ts >= ?1 AND ts < ?2 AND humidity IS NOT NULL",
                        [a, b],
                        |r| Ok((r.get::<_, Option<f64>>(0)?, r.get::<_, Option<f64>>(1)?)),
                    )
                    .ok()
                })
                .unwrap_or((None, None));
            let f =
                |c: Option<f64>| c.map(|c| (crate::thermostat::c_to_f(c) * 10.0).round() / 10.0);
            DayRecord {
                date: d.to_string(),
                forecast_high_f: fc,
                first_forecast_high_f: first,
                room_high_f: f(hi),
                room_low_f: f(lo),
            }
        })
        .collect()
}

async fn fetch(
    client: &reqwest::Client,
    lat: f64,
    lon: f64,
) -> Result<Vec<(NaiveDate, f64)>, String> {
    let get = |url: String| async move {
        client
            .get(&url)
            .send()
            .await
            .and_then(|r| r.error_for_status())
            .map_err(|e| format!("{url}: {e}"))?
            .json::<Value>()
            .await
            .map_err(|e| format!("{url}: {e}"))
    };
    let points = get(format!("https://api.weather.gov/points/{lat:.4},{lon:.4}")).await?;
    let url = points["properties"]["forecast"]
        .as_str()
        .ok_or("points response has no forecast URL")?
        .to_string();
    Ok(parse_daytime_highs(&get(url).await?))
}

/// Refresh the forecast now and then hourly, for as long as orchidd runs.
pub async fn run(db: std::sync::Arc<std::sync::Mutex<Connection>>) {
    let client = match reqwest::Client::builder()
        .user_agent("orchidd (john@gardenway.org)")
        .timeout(Duration::from_secs(30))
        .build()
    {
        Ok(c) => c,
        Err(e) => return eprintln!("forecast: client: {e}"),
    };
    let mut tick = tokio::time::interval(Duration::from_secs(3600));
    loop {
        tick.tick().await;
        let settings = crate::thermostat::load_settings(&db.lock().unwrap());
        let Ok(s) = settings else { continue };
        if !s.forecast_enabled {
            continue;
        }
        match fetch(&client, s.forecast_lat, s.forecast_lon).await {
            Ok(highs) if !highs.is_empty() => {
                let res = store_highs(&db.lock().unwrap(), &highs, crate::now_unix());
                match res {
                    Ok(()) => println!(
                        "forecast: {} daytime highs, first {:?}",
                        highs.len(),
                        highs[0]
                    ),
                    Err(e) => eprintln!("forecast: store: {e}"),
                }
            }
            Ok(_) => eprintln!("forecast: no daytime periods in the response"),
            Err(e) => eprintln!("forecast: {e}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn keeps_daytime_highs_by_local_date() {
        let v = json!({"properties": {"periods": [
            {"startTime": "2026-10-24T09:00:00-06:00", "isDaytime": true,
             "temperature": 78, "temperatureUnit": "F"},
            {"startTime": "2026-10-24T18:00:00-06:00", "isDaytime": false,
             "temperature": 50, "temperatureUnit": "F"},
            {"startTime": "2026-10-25T06:00:00-06:00", "isDaytime": true,
             "temperature": 20, "temperatureUnit": "C"}
        ]}});
        let d = |s: &str| NaiveDate::parse_from_str(s, "%Y-%m-%d").unwrap();
        assert_eq!(
            parse_daytime_highs(&v),
            vec![(d("2026-10-24"), 78.0), (d("2026-10-25"), 68.0)]
        );
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(SCHEMA).unwrap();
        store_highs(&conn, &parse_daytime_highs(&v), 1).unwrap();
        store_highs(&conn, &[(d("2026-10-24"), 80.0)], 2).unwrap();
        assert_eq!(high_for(&conn, d("2026-10-24")), Some(80.0));
        assert_eq!(high_for(&conn, d("2026-10-26")), None);
    }

    #[test]
    fn history_pairs_the_first_forecast_with_the_rooms_high_and_low() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(SCHEMA).unwrap();
        conn.execute_batch(crate::blu::BLU_SCHEMA).unwrap();
        let d = |s: &str| NaiveDate::parse_from_str(s, "%Y-%m-%d").unwrap();
        store_highs(&conn, &[(d("2026-10-24"), 76.0)], 1).unwrap();
        store_highs(&conn, &[(d("2026-10-24"), 79.0)], 2).unwrap();
        // 2026-10-24 09:00 and 15:00 MDT.
        for (ts, c) in [(1_792_854_000_i64, 17.5), (1_792_875_600, 27.0)] {
            conn.execute(
                "INSERT INTO blu_readings (ts, temp_c, humidity) VALUES (?1, ?2, 50.0)",
                rusqlite::params![ts, c],
            )
            .unwrap();
        }
        let h = history(&conn, chrono_tz::America::Denver, 1_792_890_000, 2);
        assert_eq!(h.len(), 2);
        assert_eq!(
            h[0],
            DayRecord {
                date: "2026-10-24".into(),
                forecast_high_f: Some(79.0),
                first_forecast_high_f: Some(76.0),
                room_high_f: Some(80.6),
                room_low_f: Some(63.5),
            }
        );
        assert_eq!(h[1].date, "2026-10-23");
        assert_eq!(h[1].room_high_f, None);
    }
}
