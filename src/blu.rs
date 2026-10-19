//! Second sensor: a Shelly BLU H&T Display that broadcasts BTHome v2 over
//! Bluetooth. It has no Wi-Fi, so a script on the heater plug listens for its
//! broadcasts and POSTs each one here as hex (`deploy/blu-forward.js`).
//!
//! These readings are graphed beside the Wi-Fi sensor's for comparison only;
//! the thermostat still follows the Wi-Fi sensor.

use rusqlite::Connection;

pub(crate) const BLU_SCHEMA: &str = "CREATE TABLE IF NOT EXISTS blu_readings (
        id       INTEGER PRIMARY KEY AUTOINCREMENT,
        ts       INTEGER NOT NULL,
        temp_c   REAL    NOT NULL,
        humidity REAL,
        battery  INTEGER
    );
    CREATE INDEX IF NOT EXISTS idx_blu_readings_ts ON blu_readings(ts);";

/// One decoded BTHome v2 advertisement.
#[derive(Debug, Default, PartialEq)]
pub(crate) struct BtHome {
    pub packet_id: Option<u8>,
    pub battery: Option<u8>,
    pub temp_c: Option<f64>,
    pub humidity: Option<f64>,
}

fn i16_le(b: &[u8]) -> f64 {
    i16::from_le_bytes([b[0], b[1]]) as f64
}

fn u16_le(b: &[u8]) -> f64 {
    u16::from_le_bytes([b[0], b[1]]) as f64
}

/// Decode BTHome v2 service data (UUID 0xFCD2), given as hex. None when the
/// hex is malformed, the packet is encrypted, or it is not version 2. Parsing
/// stops at the first object id whose size is unknown, keeping what came
/// before it.
pub(crate) fn parse_bthome(hex: &str) -> Option<BtHome> {
    let hex = hex.trim();
    if !hex.len().is_multiple_of(2) || hex.is_empty() {
        return None;
    }
    let bytes: Vec<u8> = (0..hex.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).ok())
        .collect::<Option<_>>()?;
    let info = bytes[0];
    if info & 0x01 != 0 || info >> 5 != 2 {
        return None; // encrypted, or not BTHome v2
    }
    let mut out = BtHome::default();
    let mut i = 1;
    while i < bytes.len() {
        let id = bytes[i];
        let size = match id {
            0x00 | 0x01 | 0x15 | 0x1E | 0x2E | 0x3A | 0x64 => 1,
            0x02 | 0x03 | 0x45 | 0xF0 => 2,
            0xF2 => 3,
            0xF1 => 4,
            _ => break,
        };
        let Some(v) = bytes.get(i + 1..i + 1 + size) else {
            break;
        };
        match id {
            0x00 => out.packet_id = Some(v[0]),
            0x01 => out.battery = Some(v[0]),
            0x02 => out.temp_c = Some(i16_le(v) * 0.01),
            0x03 => out.humidity = Some(u16_le(v) * 0.01),
            0x2E => out.humidity = Some(v[0] as f64),
            0x45 => out.temp_c = Some(i16_le(v) * 0.1),
            _ => {}
        }
        i += 1 + size;
    }
    Some(out)
}

/// Store a reading that carries a temperature. Button and device-ID packets
/// carry none and are skipped. Returns whether a row was written.
pub(crate) fn store(conn: &Connection, ts: i64, r: &BtHome) -> rusqlite::Result<bool> {
    let Some(temp_c) = r.temp_c else {
        return Ok(false);
    };
    conn.execute(
        "INSERT INTO blu_readings (ts, temp_c, humidity, battery) VALUES (?1, ?2, ?3, ?4)",
        rusqlite::params![ts, temp_c, r.humidity, r.battery],
    )?;
    Ok(true)
}

/// Readings at or after `cutoff`, oldest first: (ts, temp_c, humidity).
pub(crate) fn since(
    conn: &Connection,
    cutoff: i64,
) -> rusqlite::Result<Vec<(i64, f64, Option<f64>)>> {
    let mut stmt =
        conn.prepare("SELECT ts, temp_c, humidity FROM blu_readings WHERE ts >= ?1 ORDER BY ts")?;
    let rows = stmt.query_map([cutoff], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
    rows.collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Captured from the orchid-room sensor through the plug on 2026-10-18.
    const BEACON: &str = "40003201641e012e3645fe00";

    #[test]
    fn decodes_the_display_beacon() {
        let r = parse_bthome(BEACON).unwrap();
        assert_eq!(r.packet_id, Some(0x32));
        assert_eq!(r.battery, Some(100));
        assert_eq!(r.humidity, Some(54.0));
        assert!((r.temp_c.unwrap() - 25.4).abs() < 1e-9);
    }

    #[test]
    fn negative_and_hundredths_temperatures() {
        // 0x02 = -5.25 °C (0xFDF3), 0x03 = 45.67 %
        let r = parse_bthome("4002f3fd03d711").unwrap();
        assert!((r.temp_c.unwrap() + 5.25).abs() < 1e-9);
        assert!((r.humidity.unwrap() - 45.67).abs() < 1e-9);
    }

    #[test]
    fn rejects_encrypted_bad_hex_and_other_versions() {
        assert_eq!(parse_bthome("41003201"), None);
        assert_eq!(parse_bthome("4g"), None);
        assert_eq!(parse_bthome("200032"), None);
        assert_eq!(parse_bthome(""), None);
    }

    #[test]
    fn stops_at_an_unknown_object_but_keeps_earlier_ones() {
        let r = parse_bthome("400164ee0145fe00").unwrap();
        assert_eq!(r.battery, Some(100));
        assert_eq!(r.temp_c, None);
    }

    #[test]
    fn button_packets_are_not_stored() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(BLU_SCHEMA).unwrap();
        let press = parse_bthome("40016415003a01").unwrap();
        assert!(!store(&conn, 10, &press).unwrap());
        assert!(store(&conn, 20, &parse_bthome(BEACON).unwrap()).unwrap());
        let rows = since(&conn, 0).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].0, 20);
        assert_eq!(rows[0].2, Some(54.0));
    }
}
