//! Environment configuration.

use std::path::PathBuf;

use crate::watch::WatchConfig;

pub const DEFAULT_URL: &str = "http://192.168.0.15:8000";
pub const DEFAULT_STATE: &str = "/var/lib/orchid-watch/state.json";

#[derive(Clone, Debug, PartialEq)]
pub struct Smtp {
    pub host: String,
    pub port: u16,
    pub user: String,
    pub pass_file: PathBuf,
    pub from: String,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Config {
    pub url: String,
    pub state_path: PathBuf,
    pub interval_s: u64,
    pub watch: WatchConfig,
    pub ntfy_url: Option<String>,
    pub ntfy_token_file: Option<PathBuf>,
    pub smtp: Option<Smtp>,
}

impl Config {
    /// `get` looks up one variable (std::env::var in production).
    pub fn from_lookup(get: impl Fn(&str) -> Option<String>) -> Result<Config, String> {
        let var = |k: &str| {
            get(k)
                .map(|v| v.trim().to_string())
                .filter(|v| !v.is_empty())
        };
        let num = |k: &str, default: i64, lo: i64, hi: i64| -> Result<i64, String> {
            match var(k) {
                None => Ok(default),
                Some(v) => v
                    .parse::<i64>()
                    .ok()
                    .filter(|n| (lo..=hi).contains(n))
                    .ok_or(format!("{k}={v:?}: expected a whole number {lo}..={hi}")),
            }
        };
        let hot_f = match var("ORCHID_HOT_F") {
            None => 90.0,
            Some(v) => v
                .parse::<f64>()
                .ok()
                .filter(|f| (70.0..=120.0).contains(f))
                .ok_or(format!("ORCHID_HOT_F={v:?}: expected 70..=120"))?,
        };
        let creds = var("CREDENTIALS_DIRECTORY");
        let smtp = match (var("ORCHID_SMTP_HOST"), var("ORCHID_SMTP_USER")) {
            (Some(host), Some(user)) => {
                crate::io::secret_path(var("ORCHID_SMTP_PASS_FILE"), creds.clone(), "smtp.pass")
                    .map(|pass_file| -> Result<Smtp, String> {
                        Ok(Smtp {
                            port: num("ORCHID_SMTP_PORT", 587, 1, 65535)? as u16,
                            from: var("ORCHID_SMTP_FROM").unwrap_or_else(|| user.clone()),
                            host,
                            user,
                            pass_file,
                        })
                    })
                    .transpose()?
            }
            _ => None,
        };
        Ok(Config {
            url: var("ORCHID_WATCH_URL").unwrap_or_else(|| DEFAULT_URL.into()),
            state_path: var("ORCHID_WATCH_STATE")
                .unwrap_or_else(|| DEFAULT_STATE.into())
                .into(),
            interval_s: num("ORCHID_INTERVAL_S", 60, 5, 3600)? as u64,
            watch: WatchConfig {
                warmup_min: num("ORCHID_WARMUP_MIN", 120, 0, 1440)?,
                repeat_min: num("ORCHID_REPEAT_MIN", 60, 1, 10080)?,
                hot_f,
            },
            ntfy_url: var("ORCHID_NTFY_URL"),
            ntfy_token_file: crate::io::secret_path(
                var("ORCHID_NTFY_TOKEN_FILE"),
                creds,
                "ntfy.token",
            ),
            smtp,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn cfg(pairs: &[(&str, &str)]) -> Result<Config, String> {
        let m: HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        Config::from_lookup(|k| m.get(k).cloned())
    }

    #[test]
    fn defaults() {
        let c = cfg(&[]).unwrap();
        assert_eq!(c.url, DEFAULT_URL);
        assert_eq!(c.state_path, PathBuf::from(DEFAULT_STATE));
        assert_eq!(c.interval_s, 60);
        assert_eq!(c.watch, WatchConfig::default());
        assert_eq!(c.ntfy_url, None);
        assert_eq!(c.smtp, None);
    }

    #[test]
    fn overrides_and_smtp() {
        let c = cfg(&[
            ("ORCHID_WARMUP_MIN", "90"),
            ("ORCHID_REPEAT_MIN", "30"),
            ("ORCHID_NTFY_URL", "http://h:2586/orchid-alerts-x"),
            ("ORCHID_NTFY_TOKEN_FILE", "/etc/orchid-watch/ntfy.token"),
            ("ORCHID_SMTP_HOST", "smtp.gmail.com"),
            ("ORCHID_SMTP_USER", "me@gmail.com"),
            ("ORCHID_SMTP_PASS_FILE", "/etc/orchid-watch/smtp.pass"),
        ])
        .unwrap();
        assert_eq!(c.watch.warmup_min, 90);
        assert_eq!(c.watch.repeat_min, 30);
        assert_eq!(
            c.ntfy_token_file,
            Some(PathBuf::from("/etc/orchid-watch/ntfy.token"))
        );
        let s = c.smtp.unwrap();
        assert_eq!(s.port, 587);
        assert_eq!(s.from, "me@gmail.com");
    }

    #[test]
    fn smtp_needs_all_three() {
        let c = cfg(&[
            ("ORCHID_SMTP_HOST", "smtp.gmail.com"),
            ("ORCHID_SMTP_USER", "u"),
        ])
        .unwrap();
        assert_eq!(c.smtp, None);
    }

    #[test]
    fn bad_numbers_are_errors() {
        assert!(cfg(&[("ORCHID_REPEAT_MIN", "0")]).is_err());
        assert!(cfg(&[("ORCHID_WARMUP_MIN", "two hours")]).is_err());
        assert!(cfg(&[("ORCHID_HOT_F", "200")]).is_err());
    }
}
