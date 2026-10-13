//! The watcher's state machine: what is wrong, for how long, and who has
//! already been told.
//!
//! Every outside dependency — the clock, orchidd's API, ntfy, SMTP — sits
//! behind a trait so the whole machine runs in tests against fakes.

use std::collections::BTreeMap;
use std::fmt;
use std::path::{Path, PathBuf};
use std::str::FromStr;

use chrono::{NaiveTime, TimeZone, Utc};
use chrono_tz::Tz;
use serde::{Deserialize, Serialize};

use crate::model::{Settings, Status};

// ---------------------------------------------------------------------------
// Seams
// ---------------------------------------------------------------------------

pub trait Clock {
    /// Unix seconds.
    fn now(&self) -> i64;
}

pub trait Fetch {
    fn status(&self) -> Result<Status, String>;
    fn settings(&self) -> Result<Settings, String>;
}

/// The SMS/iMessage path: one ntfy publish, picked up by the relay on a Mac.
pub trait Publish {
    /// `to` is an E.164 number; `None` lets the relay use its own fallback.
    fn publish(&self, title: &str, message: &str, to: Option<&str>) -> Result<(), String>;
}

pub trait Mail {
    fn send(&self, to: &str, subject: &str, body: &str) -> Result<(), String>;
}

// ---------------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq)]
pub struct WatchConfig {
    pub warmup_min: i64,
    pub repeat_min: i64,
    pub hot_f: f64,
    /// Minutes without a relay heartbeat before John is told.
    pub relay_stale_min: i64,
}

impl Default for WatchConfig {
    fn default() -> Self {
        WatchConfig {
            warmup_min: 120,
            repeat_min: 60,
            hot_f: 90.0,
            relay_stale_min: 10,
        }
    }
}

// ---------------------------------------------------------------------------
// Conditions
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Condition {
    OrchiddDown,
    SensorStale,
    PlugUnreachable,
    BelowSetpoint,
    TooHot,
    /// Rose Marie's iMessage relay has stopped checking in. Admin channel only.
    RelayDown,
}

impl Condition {
    /// The conditions read off orchidd's own alert state. `RelayDown` is not
    /// among them: it is evaluated only when the admin channel exists.
    pub const ALL: [Condition; 5] = [
        Condition::OrchiddDown,
        Condition::SensorStale,
        Condition::PlugUnreachable,
        Condition::BelowSetpoint,
        Condition::TooHot,
    ];

    /// How long the condition must hold, unbroken, before anyone is told.
    pub fn hold_secs(self) -> i64 {
        match self {
            Condition::BelowSetpoint => 10 * 60,
            // The staleness window is already the debounce.
            Condition::RelayDown => 0,
            _ => 5 * 60,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Condition::OrchiddDown => "orchidd_down",
            Condition::SensorStale => "sensor_stale",
            Condition::PlugUnreachable => "plug_unreachable",
            Condition::BelowSetpoint => "below_setpoint",
            Condition::TooHot => "too_hot",
            Condition::RelayDown => "relay_down",
        }
    }
}

impl fmt::Display for Condition {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// What one tick learned about one condition.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Reading {
    /// It is happening.
    Raised,
    /// It is not happening.
    Clear,
    /// It is happening but is expected (morning warm-up, thermostat off):
    /// no new alert, no repeat, and no "back to normal" either.
    Held,
    /// Can't tell this tick (orchidd down, or masked by a worse alert):
    /// change nothing.
    Unknown,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Channel {
    Sms,
    Email,
    /// John, on the firm's leads topic through a different relay.
    Admin,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    Alert,
    Repeat,
    Recovery,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct CondState {
    /// When the current unbroken run of `Raised` began.
    #[serde(default)]
    pub first_seen: Option<i64>,
    /// Channels told about the current episode, and when they were last told.
    /// Non-empty means a "back to normal" is owed on that channel.
    #[serde(default)]
    pub sent: BTreeMap<Channel, i64>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct State {
    #[serde(default)]
    pub conditions: BTreeMap<Condition, CondState>,
    /// Last settings orchidd returned: recipients survive orchidd going down.
    #[serde(default)]
    pub settings: Option<Settings>,
    /// Last time orchidd answered /api/status.
    #[serde(default)]
    pub last_ok: Option<i64>,
}

impl State {
    pub fn load(path: &Path) -> Result<State, String> {
        match std::fs::read_to_string(path) {
            Ok(s) => serde_json::from_str(&s).map_err(|e| format!("{}: {e}", path.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(State::default()),
            Err(e) => Err(format!("{}: {e}", path.display())),
        }
    }

    /// Written to a temp file and renamed, so a crash never leaves half a file.
    pub fn save(&self, path: &Path) -> Result<(), String> {
        let json = serde_json::to_string_pretty(self).map_err(|e| e.to_string())?;
        if let Some(dir) = path.parent() {
            if !dir.as_os_str().is_empty() {
                std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
            }
        }
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, json).map_err(|e| format!("{}: {e}", tmp.display()))?;
        std::fs::rename(&tmp, path).map_err(|e| format!("{}: {e}", path.display()))
    }
}

// ---------------------------------------------------------------------------
// Pure helpers
// ---------------------------------------------------------------------------

/// E.164 for a phone number typed by a person. Ten digits are US (+1).
pub fn normalize_phone(raw: &str) -> Option<String> {
    let t = raw.trim();
    if t.is_empty()
        || !t
            .chars()
            .all(|c| c.is_ascii_digit() || " -().+".contains(c))
        || t.chars().skip(1).any(|c| c == '+')
    {
        return None;
    }
    let digits: String = t.chars().filter(char::is_ascii_digit).collect();
    if t.starts_with('+') {
        return (8..=15)
            .contains(&digits.len())
            .then(|| format!("+{digits}"));
    }
    match digits.len() {
        10 if !digits.starts_with(['0', '1']) => Some(format!("+1{digits}")),
        11 if digits.starts_with('1') => Some(format!("+{digits}")),
        _ => None,
    }
}

fn tz_of(s: &Settings) -> Tz {
    Tz::from_str(&s.timezone).unwrap_or(chrono_tz::America::Denver)
}

/// True in the first `warmup_min` minutes after `day_start`, local wall-clock
/// time in the settings' timezone.
pub fn in_warmup(now: i64, settings: &Settings, warmup_min: i64) -> bool {
    let Ok(start) = NaiveTime::parse_from_str(&settings.day_start, "%H:%M") else {
        return false;
    };
    let Some(utc) = Utc.timestamp_opt(now, 0).single() else {
        return false;
    };
    let local = utc.with_timezone(&tz_of(settings)).time();
    let since = (local - start).num_seconds().rem_euclid(86_400);
    since < warmup_min * 60
}

/// Read each condition off one fetch.
pub fn derive(
    now: i64,
    status: Option<&Status>,
    settings: Option<&Settings>,
    cfg: &WatchConfig,
) -> BTreeMap<Condition, Reading> {
    use Reading::*;
    let mut out = BTreeMap::new();
    let Some(s) = status else {
        out.insert(Condition::OrchiddDown, Raised);
        for c in &Condition::ALL[1..] {
            out.insert(*c, Unknown);
        }
        return out;
    };
    out.insert(Condition::OrchiddDown, Clear);

    let kind = s.alert_kind();
    let stale_limit = i64::from(settings.map_or(20, |x| x.stale_minutes)) * 60;
    // orchidd reports one alert at a time; a plug alert hides a stale sensor,
    // so look at the reading's age directly.
    let fresh = kind != Some("sensor_stale") && s.reading_age_s.is_some_and(|a| a < stale_limit);

    out.insert(
        Condition::PlugUnreachable,
        if kind == Some("plug_unreachable") {
            Raised
        } else {
            Clear
        },
    );
    out.insert(
        Condition::SensorStale,
        match kind {
            Some("sensor_stale") => Raised,
            Some("plug_unreachable") if !fresh => Raised,
            _ => Clear,
        },
    );

    let below = match kind {
        Some("below_setpoint") => Raised,
        // Masked by the plug alert, but the reading is still good: work it out.
        Some("plug_unreachable") if fresh => match (s.temp_f, s.setpoint_f, settings) {
            (Some(t), Some(sp), Some(st)) if s.mode == "auto" => {
                if t < sp - st.alert_below_f {
                    Raised
                } else {
                    Clear
                }
            }
            (Some(_), Some(_), Some(_)) => Clear,
            _ => Unknown,
        },
        Some("plug_unreachable") | Some("sensor_stale") => Unknown,
        _ => Clear,
    };
    let expected = s.mode == "off"
        || (s.period == "day" && settings.is_some_and(|st| in_warmup(now, st, cfg.warmup_min)));
    out.insert(
        Condition::BelowSetpoint,
        if below == Raised && expected {
            Held
        } else {
            below
        },
    );

    out.insert(
        Condition::TooHot,
        match s.temp_f {
            Some(t) if fresh => {
                if t >= cfg.hot_f {
                    Raised
                } else {
                    Clear
                }
            }
            _ => Unknown,
        },
    );
    out
}

/// Is the iMessage relay on the iMac checking in? Unknown while orchidd
/// itself is unreachable (that is `orchidd_down`'s job).
pub fn relay_reading(status: Option<&Status>, stale_min: i64) -> Reading {
    match status {
        None => Reading::Unknown,
        Some(s) if !s.relay_watched => Reading::Clear,
        Some(s) => match s.relay_age_s {
            Some(a) if a <= stale_min * 60 => Reading::Clear,
            _ => Reading::Raised,
        },
    }
}

// ---------------------------------------------------------------------------
// Wording — for Rose Marie, not for an engineer
// ---------------------------------------------------------------------------

fn deg(v: f64) -> String {
    if (v - v.round()).abs() < 0.05 {
        format!("{v:.0}")
    } else {
        format!("{v:.1}")
    }
}

fn heater_sentence(s: &Status) -> String {
    match (s.heater_on, s.apower_w) {
        (Some(true), Some(w)) => format!(" Heater is ON ({w:.0} W)."),
        (Some(true), None) => " Heater is ON.".into(),
        (Some(false), _) => " Heater is OFF.".into(),
        (None, _) => String::new(),
    }
}

fn mins(secs: i64) -> i64 {
    (secs.max(0) + 30) / 60
}

pub struct Ctx<'a> {
    pub now: i64,
    pub status: Option<&'a Status>,
    pub state: &'a State,
    pub cfg: &'a WatchConfig,
}

/// Title and body for one message.
pub fn compose(cond: Condition, kind: Kind, cx: &Ctx) -> (String, String) {
    if cond == Condition::RelayDown {
        return compose_relay(kind, cx);
    }
    let temp = cx.status.and_then(|s| s.temp_f);
    let temp_txt = temp.map_or("temperature unknown".to_string(), |t| format!("{t:.1}°F"));
    if kind == Kind::Recovery {
        let heater = match cx.status.and_then(|s| s.heater_on) {
            Some(true) => ", heater ON",
            Some(false) => ", heater OFF",
            None => "",
        };
        let lead = match cond {
            Condition::OrchiddDown => "Orchid monitor back online",
            Condition::SensorStale => "Sensor reporting again",
            Condition::PlugUnreachable => "Heater plug answering again",
            Condition::BelowSetpoint | Condition::TooHot => "Back to normal",
            Condition::RelayDown => unreachable!("composed by compose_relay"),
        };
        let mut msg = format!("{lead}: {temp_txt}{heater}.");
        if cx.status.is_some_and(|s| s.mode == "off") {
            msg.push_str(" The thermostat is set to Off.");
        }
        return ("Orchid room OK".into(), msg);
    }

    let first = cx
        .state
        .conditions
        .get(&cond)
        .and_then(|c| c.first_seen)
        .unwrap_or(cx.now);
    let (title, msg) = match cond {
        Condition::OrchiddDown => {
            let since = cx.state.last_ok.unwrap_or(first);
            (
                "Orchid monitor offline".to_string(),
                format!(
                    "Orchid monitor offline — no response from the iMac for {} min. \
                     Heater safety timer will switch the heater off.",
                    mins(cx.now - since)
                ),
            )
        }
        Condition::SensorStale => {
            let age = cx.status.and_then(|s| s.reading_age_s);
            let mut m = match age {
                Some(a) => format!(
                    "Orchid sensor silent for {} min — heater held off until it reports.",
                    mins(a)
                ),
                None => {
                    "Orchid sensor has not reported — heater held off until it reports.".to_string()
                }
            };
            if let Some(t) = temp {
                m.push_str(&format!(" Last reading {t:.1}°F."));
            }
            ("Orchid sensor silent".to_string(), m)
        }
        Condition::PlugUnreachable => {
            let mut m = format!(
                "The heater plug has not answered for {} min, so the heater can't be controlled. \
                 Check that it is plugged in.",
                mins(cx.now - first)
            );
            if let Some(t) = temp {
                m.push_str(&format!(" Room is {t:.1}°F."));
            }
            ("Orchid heater plug offline".to_string(), m)
        }
        Condition::BelowSetpoint => {
            let s = cx.status.expect("below_setpoint needs a status");
            let sp = s.setpoint_f.unwrap_or(0.0);
            let t = temp.unwrap_or(sp);
            (
                format!("Orchid room {t:.1}°F"),
                format!(
                    "{}° below the {}° {} setting.{}",
                    deg(sp - t),
                    deg(sp),
                    s.period,
                    heater_sentence(s)
                ),
            )
        }
        Condition::RelayDown => unreachable!("composed by compose_relay"),
        Condition::TooHot => {
            let s = cx.status.expect("too_hot needs a status");
            let t = temp.unwrap_or(cx.cfg.hot_f);
            (
                format!("Orchid room {t:.1}°F"),
                format!(
                    "Too hot — {}° or more.{} Unplug the heater if it stays on.",
                    deg(cx.cfg.hot_f),
                    heater_sentence(s)
                ),
            )
        }
    };
    if kind == Kind::Repeat {
        (format!("Still: {title}"), msg)
    } else {
        (title, msg)
    }
}

/// The relay-down message — for John, who has to go and fix it.
fn compose_relay(kind: Kind, cx: &Ctx) -> (String, String) {
    if kind == Kind::Recovery {
        return (
            "Orchid alerts restored".into(),
            "The iMessage relay on the iMac is checking in again; \
             Rose Marie's orchid alerts are back on."
                .into(),
        );
    }
    let n = cx
        .status
        .and_then(|s| s.relay_age_s)
        .map_or("an unknown number of".to_string(), |a| mins(a).to_string());
    let title = "Orchid alerts are down";
    let msg = format!(
        "Rose Marie will NOT get orchid texts: the iMessage relay on the iMac has not \
         checked in for {n} minutes. Usually the iMac restarted and nobody has logged \
         into jmurray.

Fix it from your laptop:
At home: Finder > Go > Connect to Server (Cmd-K), enter vnc://192.168.0.15, log in \
         as jmurray, then close the Screen Sharing window (do not log out).
Away from home: in Terminal run
  ssh -N -L 5901:192.168.0.15:5900 gardenway
then Cmd-K, enter vnc://localhost:5901, and log in as jmurray the same way.

You will get an \"Orchid alerts restored\" text when it is back."
    );
    if kind == Kind::Repeat {
        (format!("Still: {title}"), msg)
    } else {
        (title.into(), msg)
    }
}

// ---------------------------------------------------------------------------
// The watcher
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, Serialize)]
pub struct Outgoing {
    pub condition: Condition,
    pub kind: Kind,
    pub channel: Channel,
    pub to: Option<String>,
    pub title: String,
    pub message: String,
    /// None: not attempted (dry run or alerts disabled).
    pub result: Option<Result<(), String>>,
}

#[derive(Clone, Debug, Serialize)]
pub struct TickReport {
    pub now: i64,
    pub fetch_error: Option<String>,
    pub alerts_enabled: bool,
    pub readings: BTreeMap<Condition, Reading>,
    pub outgoing: Vec<Outgoing>,
}

pub struct Watcher {
    pub clock: Box<dyn Clock>,
    pub fetch: Box<dyn Fetch>,
    pub sms: Option<Box<dyn Publish>>,
    pub mail: Option<Box<dyn Mail>>,
    /// John's channel and his E.164 number, when configured.
    pub admin: Option<(Box<dyn Publish>, String)>,
    pub cfg: WatchConfig,
    pub state: State,
    pub state_path: Option<PathBuf>,
}

impl Watcher {
    /// Recipients from the last settings seen (live, or cached on disk).
    fn recipients(&self) -> Vec<(Channel, Option<String>)> {
        let settings = self.state.settings.as_ref();
        let mut out = Vec::new();
        if self.sms.is_some() {
            // An unusable or empty number still publishes, untagged: the
            // relay then sends it to its own fallback number.
            let raw = settings.map_or("", |s| s.alert_phone.as_str());
            let phone = normalize_phone(raw);
            if phone.is_none() && !raw.trim().is_empty() {
                log(&format!(
                    "alert_phone {raw:?} is not a phone number; using the relay's fallback"
                ));
            }
            out.push((Channel::Sms, phone));
        }
        if self.mail.is_some() {
            if let Some(addr) = settings
                .map(|s| s.alert_email.trim())
                .filter(|a| !a.is_empty())
            {
                out.push((Channel::Email, Some(addr.to_string())));
            }
        }
        out
    }

    /// Who hears about `cond`: Rose Marie's channels, plus John for
    /// `orchidd_down` (her relay runs on the same iMac), and only John for
    /// `relay_down`.
    fn recipients_for(
        &self,
        cond: Condition,
        base: &[(Channel, Option<String>)],
    ) -> Vec<(Channel, Option<String>)> {
        let admin = self
            .admin
            .as_ref()
            .map(|(_, to)| (Channel::Admin, Some(to.clone())));
        match cond {
            Condition::RelayDown => admin.into_iter().collect(),
            Condition::OrchiddDown => base.iter().cloned().chain(admin).collect(),
            _ => base.to_vec(),
        }
    }

    fn deliver(
        &self,
        channel: Channel,
        to: Option<&str>,
        title: &str,
        msg: &str,
    ) -> Result<(), String> {
        match channel {
            Channel::Sms => self
                .sms
                .as_ref()
                .ok_or("sms not configured")?
                .publish(title, msg, to),
            Channel::Email => self.mail.as_ref().ok_or("email not configured")?.send(
                to.ok_or("no email address")?,
                title,
                msg,
            ),
            Channel::Admin => self
                .admin
                .as_ref()
                .ok_or("admin channel not configured")?
                .0
                .publish(title, msg, to),
        }
    }

    /// One evaluation. With `dry_run`, nothing is sent and state is not
    /// changed or saved — the report says what would have happened.
    pub fn tick(&mut self, dry_run: bool) -> TickReport {
        let saved = dry_run.then(|| self.state.clone());
        let now = self.clock.now();
        let (status, fetch_error) = match self.fetch.status() {
            Ok(s) => (Some(s), None),
            Err(e) => (None, Some(e)),
        };
        if status.is_some() {
            self.state.last_ok = Some(now);
            match self.fetch.settings() {
                Ok(s) => self.state.settings = Some(s),
                Err(e) => log(&format!("settings unavailable, using last known: {e}")),
            }
        }
        let mut readings = derive(
            now,
            status.as_ref(),
            self.state.settings.as_ref(),
            &self.cfg,
        );
        if self.admin.is_some() {
            readings.insert(
                Condition::RelayDown,
                relay_reading(status.as_ref(), self.cfg.relay_stale_min),
            );
        }

        for (cond, r) in &readings {
            let cs = self.state.conditions.entry(*cond).or_default();
            match r {
                Reading::Raised => {
                    cs.first_seen.get_or_insert(now);
                }
                Reading::Clear | Reading::Held => cs.first_seen = None,
                Reading::Unknown => {}
            }
        }

        let enabled = self
            .state
            .settings
            .as_ref()
            .is_none_or(|s| s.alerts_enabled);
        let recipients = self.recipients();
        let repeat = self.cfg.repeat_min * 60;
        let mut outgoing = Vec::new();

        for (cond, r) in &readings {
            let cs = &self.state.conditions[cond];
            let qualified =
                *r == Reading::Raised && cs.first_seen.is_some_and(|f| now - f >= cond.hold_secs());
            for (ch, to) in &self.recipients_for(*cond, &recipients) {
                let kind = match (qualified, *r, cs.sent.get(ch)) {
                    (true, _, None) => Some(Kind::Alert),
                    (true, _, Some(&last)) if now - last >= repeat => Some(Kind::Repeat),
                    (_, Reading::Clear, Some(_)) => Some(Kind::Recovery),
                    _ => None,
                };
                if let Some(kind) = kind {
                    let cx = Ctx {
                        now,
                        status: status.as_ref(),
                        state: &self.state,
                        cfg: &self.cfg,
                    };
                    let (title, message) = compose(*cond, kind, &cx);
                    outgoing.push(Outgoing {
                        condition: *cond,
                        kind,
                        channel: *ch,
                        to: to.clone(),
                        title,
                        message,
                        result: None,
                    });
                }
            }
        }

        for o in &mut outgoing {
            if dry_run {
                continue;
            }
            if !enabled {
                log(&format!(
                    "alerts disabled, not sending {:?} {} via {:?}: {} / {}",
                    o.kind, o.condition, o.channel, o.title, o.message
                ));
                continue;
            }
            let res = self.deliver(o.channel, o.to.as_deref(), &o.title, &o.message);
            match &res {
                Ok(()) => {
                    log(&format!(
                        "sent {:?} {} via {:?}",
                        o.kind, o.condition, o.channel
                    ));
                    let cs = self
                        .state
                        .conditions
                        .get_mut(&o.condition)
                        .expect("present");
                    if o.kind == Kind::Recovery {
                        cs.sent.remove(&o.channel);
                    } else {
                        cs.sent.insert(o.channel, now);
                    }
                }
                Err(e) => log(&format!(
                    "FAILED {:?} {} via {:?} (retrying next tick): {e}",
                    o.kind, o.condition, o.channel
                )),
            }
            o.result = Some(res);
        }

        // A cleared condition owes nothing on a channel that no longer exists,
        // and nothing at all while alerts are switched off.
        for (cond, r) in &readings {
            if *r == Reading::Clear {
                let live: Vec<Channel> = self
                    .recipients_for(*cond, &recipients)
                    .iter()
                    .map(|r| r.0)
                    .collect();
                let cs = self.state.conditions.get_mut(cond).expect("present");
                if !enabled {
                    cs.sent.clear();
                } else {
                    cs.sent.retain(|ch, _| live.contains(ch));
                }
            }
        }

        if let Some(saved) = saved {
            self.state = saved;
        } else if let Some(p) = &self.state_path {
            if let Err(e) = self.state.save(p) {
                log(&format!("could not save state: {e}"));
            }
        }

        TickReport {
            now,
            fetch_error,
            alerts_enabled: enabled,
            readings,
            outgoing,
        }
    }

    /// `--test`: one message through every configured channel.
    pub fn send_test(&mut self, message: &str) -> Result<Vec<String>, Vec<String>> {
        match self.fetch.settings() {
            Ok(s) => self.state.settings = Some(s),
            Err(e) => log(&format!("settings unavailable, using last known: {e}")),
        }
        let mut ok = Vec::new();
        let mut failed = Vec::new();
        let mut recipients = self.recipients();
        if self.mail.is_some() && !recipients.iter().any(|r| r.0 == Channel::Email) {
            ok.push("email: skipped, no alert_email set on the settings page".to_string());
        }
        if let Some((_, to)) = &self.admin {
            recipients.push((Channel::Admin, Some(to.clone())));
        }
        if recipients.is_empty() {
            failed.push("no channel configured".to_string());
        }
        for (ch, to) in recipients {
            let who = to.clone().unwrap_or_else(|| "relay fallback".into());
            match self.deliver(ch, to.as_deref(), "Orchid alert test", message) {
                Ok(()) => ok.push(format!("{ch:?} to {who}: sent")),
                Err(e) => failed.push(format!("{ch:?} to {who}: {e}")),
            }
        }
        if failed.is_empty() {
            Ok(ok)
        } else {
            Err(failed.into_iter().chain(ok).collect())
        }
    }
}

pub fn log(msg: &str) {
    eprintln!("orchid-watch: {msg}");
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Alert;
    use std::cell::RefCell;
    use std::rc::Rc;

    // -- fakes --------------------------------------------------------------

    #[derive(Default)]
    struct World {
        now: i64,
        status: Option<Status>,
        settings: Option<Settings>,
        sms: Vec<(String, String, Option<String>)>,
        mail: Vec<(String, String, String)>,
        admin: Vec<(String, String, Option<String>)>,
        sms_fails: bool,
        mail_fails: bool,
    }
    type W = Rc<RefCell<World>>;

    struct FClock(W);
    impl Clock for FClock {
        fn now(&self) -> i64 {
            self.0.borrow().now
        }
    }
    struct FFetch(W);
    impl Fetch for FFetch {
        fn status(&self) -> Result<Status, String> {
            self.0
                .borrow()
                .status
                .clone()
                .ok_or("connection refused".into())
        }
        fn settings(&self) -> Result<Settings, String> {
            let w = self.0.borrow();
            if w.status.is_none() {
                return Err("connection refused".into());
            }
            w.settings.clone().ok_or("no settings".into())
        }
    }
    struct FSms(W);
    impl Publish for FSms {
        fn publish(&self, t: &str, m: &str, to: Option<&str>) -> Result<(), String> {
            let mut w = self.0.borrow_mut();
            if w.sms_fails {
                return Err("ntfy 502".into());
            }
            w.sms.push((t.into(), m.into(), to.map(String::from)));
            Ok(())
        }
    }
    struct FAdmin(W);
    impl Publish for FAdmin {
        fn publish(&self, t: &str, m: &str, to: Option<&str>) -> Result<(), String> {
            self.0
                .borrow_mut()
                .admin
                .push((t.into(), m.into(), to.map(String::from)));
            Ok(())
        }
    }
    struct FMail(W);
    impl Mail for FMail {
        fn send(&self, to: &str, s: &str, b: &str) -> Result<(), String> {
            let mut w = self.0.borrow_mut();
            if w.mail_fails {
                return Err("smtp 535".into());
            }
            w.mail.push((to.into(), s.into(), b.into()));
            Ok(())
        }
    }

    // 2026-01-15 00:00 America/Denver (MST, UTC-7) = 07:00 UTC.
    const MIDNIGHT_DENVER: i64 = 1_768_460_400;
    const H: i64 = 3600;
    const M: i64 = 60;

    fn settings() -> Settings {
        Settings {
            timezone: "America/Denver".into(),
            day_start: "07:00".into(),
            day_end: "21:00".into(),
            day_setpoint_f: 75.0,
            night_setpoint_f: 60.0,
            alert_below_f: 1.5,
            stale_minutes: 20,
            alert_phone: "(801) 555-0101".into(),
            alert_email: "rm@example.com".into(),
            alerts_enabled: true,
            mode: "auto".into(),
        }
    }

    fn ok_status(temp: f64, period: &str, setpoint: f64) -> Status {
        Status {
            temp_f: Some(temp),
            reading_age_s: Some(120),
            period: period.into(),
            setpoint_f: Some(setpoint),
            heater_on: Some(false),
            apower_w: Some(0.0),
            mode: "auto".into(),
            alert: None,
            relay_watched: false,
            relay_age_s: None,
        }
    }

    fn with_alert(mut s: Status, kind: &str) -> Status {
        s.alert = Some(Alert {
            kind: kind.into(),
            detail: String::new(),
        });
        s
    }

    fn below(temp: f64, period: &str, setpoint: f64) -> Status {
        let mut s = with_alert(ok_status(temp, period, setpoint), "below_setpoint");
        s.heater_on = Some(true);
        s.apower_w = Some(905.0);
        s
    }

    fn rig(start: i64, email: bool) -> (W, Watcher) {
        let w: W = Rc::new(RefCell::new(World {
            now: start,
            status: Some(ok_status(61.0, "night", 60.0)),
            settings: Some(settings()),
            ..Default::default()
        }));
        let watcher = Watcher {
            clock: Box::new(FClock(w.clone())),
            fetch: Box::new(FFetch(w.clone())),
            sms: Some(Box::new(FSms(w.clone()))),
            mail: email.then(|| Box::new(FMail(w.clone())) as Box<dyn Mail>),
            admin: None,
            cfg: WatchConfig::default(),
            state: State::default(),
            state_path: None,
        };
        (w, watcher)
    }

    /// Run ticks every minute for `minutes`, returning sms count afterwards.
    fn run(w: &W, watcher: &mut Watcher, minutes: i64) {
        for _ in 0..minutes {
            watcher.tick(false);
            w.borrow_mut().now += M;
        }
    }

    fn sms(w: &W) -> Vec<(String, String, Option<String>)> {
        w.borrow().sms.clone()
    }

    const JOHN: &str = "+18015550142";

    /// A rig with John's admin channel, orchidd watching a fresh relay.
    fn rig_admin(start: i64) -> (W, Watcher) {
        let (w, mut wt) = rig(start, false);
        wt.admin = Some((Box::new(FAdmin(w.clone())), JOHN.into()));
        relay(&w, true, Some(30));
        (w, wt)
    }

    fn relay(w: &W, watched: bool, age_s: Option<i64>) {
        let mut w = w.borrow_mut();
        let st = w.status.as_mut().expect("status");
        st.relay_watched = watched;
        st.relay_age_s = age_s;
    }

    fn admin(w: &W) -> Vec<(String, String, Option<String>)> {
        w.borrow().admin.clone()
    }

    // -- phone --------------------------------------------------------------

    #[test]
    fn phone_normalization() {
        assert_eq!(
            normalize_phone("801-555-0101").as_deref(),
            Some("+18015550101")
        );
        assert_eq!(
            normalize_phone("(801) 555-0101").as_deref(),
            Some("+18015550101")
        );
        assert_eq!(
            normalize_phone(" 801.555.0101 ").as_deref(),
            Some("+18015550101")
        );
        assert_eq!(
            normalize_phone("1 801 555 0101").as_deref(),
            Some("+18015550101")
        );
        assert_eq!(
            normalize_phone("+1 801 555 0101").as_deref(),
            Some("+18015550101")
        );
        assert_eq!(
            normalize_phone("+44 20 7946 0958").as_deref(),
            Some("+442079460958")
        );
        assert_eq!(normalize_phone(""), None);
        assert_eq!(normalize_phone("555-0101"), None);
        assert_eq!(normalize_phone("call me"), None);
        assert_eq!(normalize_phone("rm@example.com"), None);
        assert_eq!(normalize_phone("2 801 555 0101"), None);
        assert_eq!(normalize_phone("801+555+0101"), None);
        assert_eq!(normalize_phone("0801555010"), None);
    }

    // -- thresholds ---------------------------------------------------------

    #[test]
    fn below_setpoint_waits_ten_minutes_then_alerts_once() {
        let (w, mut wt) = rig(MIDNIGHT_DENVER + 2 * H, false);
        w.borrow_mut().status = Some(below(58.1, "night", 60.0));
        run(&w, &mut wt, 10); // minutes 0..9
        assert!(sms(&w).is_empty(), "nothing before 10 minutes");
        run(&w, &mut wt, 1); // minute 10
        let s = sms(&w);
        assert_eq!(s.len(), 1);
        assert_eq!(s[0].0, "Orchid room 58.1°F");
        assert_eq!(
            s[0].1,
            "1.9° below the 60° night setting. Heater is ON (905 W)."
        );
        assert_eq!(s[0].2.as_deref(), Some("+18015550101"));
        run(&w, &mut wt, 30);
        assert_eq!(sms(&w).len(), 1, "no repeat inside the repeat interval");
    }

    #[test]
    fn a_blip_resets_the_clock() {
        let (w, mut wt) = rig(MIDNIGHT_DENVER + 2 * H, false);
        w.borrow_mut().status = Some(below(58.1, "night", 60.0));
        run(&w, &mut wt, 9);
        w.borrow_mut().status = Some(ok_status(60.5, "night", 60.0));
        run(&w, &mut wt, 1);
        w.borrow_mut().status = Some(below(58.1, "night", 60.0));
        run(&w, &mut wt, 10);
        assert!(sms(&w).is_empty());
        run(&w, &mut wt, 1);
        assert_eq!(sms(&w).len(), 1);
    }

    #[test]
    fn stale_and_plug_alert_after_five_minutes() {
        for (kind, title) in [
            ("sensor_stale", "Orchid sensor silent"),
            ("plug_unreachable", "Orchid heater plug offline"),
        ] {
            let (w, mut wt) = rig(MIDNIGHT_DENVER + 2 * H, false);
            let mut st = with_alert(ok_status(61.0, "night", 60.0), kind);
            if kind == "sensor_stale" {
                st.reading_age_s = Some(25 * 60);
            }
            w.borrow_mut().status = Some(st);
            run(&w, &mut wt, 5);
            assert!(sms(&w).is_empty(), "{kind}");
            run(&w, &mut wt, 1);
            let s = sms(&w);
            assert_eq!(s.len(), 1, "{kind}");
            assert_eq!(s[0].0, title);
            if kind == "sensor_stale" {
                assert_eq!(
                    s[0].1,
                    "Orchid sensor silent for 25 min — heater held off until it reports. \
                     Last reading 61.0°F."
                );
            }
        }
    }

    #[test]
    fn too_hot_after_five_minutes() {
        let (w, mut wt) = rig(MIDNIGHT_DENVER + 2 * H, false);
        let mut st = ok_status(91.3, "night", 60.0);
        st.heater_on = Some(true);
        st.apower_w = Some(905.0);
        w.borrow_mut().status = Some(st);
        run(&w, &mut wt, 5);
        assert!(sms(&w).is_empty());
        run(&w, &mut wt, 1);
        let s = sms(&w);
        assert_eq!(s.len(), 1);
        assert_eq!(s[0].0, "Orchid room 91.3°F");
        assert!(s[0]
            .1
            .starts_with("Too hot — 90° or more. Heater is ON (905 W)."));
    }

    #[test]
    fn too_hot_ignores_a_stale_reading() {
        let (w, mut wt) = rig(MIDNIGHT_DENVER + 2 * H, false);
        let mut st = with_alert(ok_status(95.0, "night", 60.0), "sensor_stale");
        st.reading_age_s = Some(40 * 60);
        w.borrow_mut().status = Some(st);
        run(&w, &mut wt, 20);
        assert!(sms(&w).iter().all(|m| m.0 == "Orchid sensor silent"));
    }

    #[test]
    fn orchidd_down_after_five_minutes_uses_cached_recipients() {
        let (w, mut wt) = rig(MIDNIGHT_DENVER + 2 * H, true);
        run(&w, &mut wt, 1); // learns settings
        w.borrow_mut().status = None;
        run(&w, &mut wt, 5);
        assert!(sms(&w).is_empty());
        run(&w, &mut wt, 1);
        let s = sms(&w);
        assert_eq!(s.len(), 1);
        assert_eq!(s[0].0, "Orchid monitor offline");
        assert_eq!(
            s[0].1,
            "Orchid monitor offline — no response from the iMac for 6 min. \
             Heater safety timer will switch the heater off."
        );
        assert_eq!(s[0].2.as_deref(), Some("+18015550101"));
        assert_eq!(w.borrow().mail[0].0, "rm@example.com");
    }

    #[test]
    fn orchidd_down_does_not_fake_recoveries() {
        let (w, mut wt) = rig(MIDNIGHT_DENVER + 2 * H, false);
        w.borrow_mut().status = Some(below(58.1, "night", 60.0));
        run(&w, &mut wt, 11);
        assert_eq!(sms(&w).len(), 1);
        w.borrow_mut().status = None;
        run(&w, &mut wt, 3);
        assert_eq!(
            sms(&w).len(),
            1,
            "an unreachable orchidd is not 'back to normal'"
        );
        w.borrow_mut().status = Some(below(58.1, "night", 60.0));
        run(&w, &mut wt, 1);
        assert_eq!(sms(&w).len(), 1, "same episode continues, no new alert");
    }

    #[test]
    fn orchidd_down_with_no_cache_goes_to_relay_fallback() {
        let (w, mut wt) = rig(MIDNIGHT_DENVER, true);
        w.borrow_mut().status = None;
        run(&w, &mut wt, 6);
        let s = sms(&w);
        assert_eq!(s.len(), 1);
        assert_eq!(s[0].2, None, "untagged: relay uses LEAD_ALERT_TO");
        assert!(w.borrow().mail.is_empty(), "no address known");
    }

    // -- warm-up grace ------------------------------------------------------

    #[test]
    fn morning_warmup_is_silent_and_alerts_after_grace() {
        // 06:50 night, fine. 07:00 setpoint jumps to 75 and orchidd says below.
        let (w, mut wt) = rig(MIDNIGHT_DENVER + 6 * H + 50 * M, false);
        run(&w, &mut wt, 10);
        assert_eq!(w.borrow().now, MIDNIGHT_DENVER + 7 * H);
        w.borrow_mut().status = Some(below(62.0, "day", 75.0));
        run(&w, &mut wt, 120); // 07:00 .. 08:59
        assert!(sms(&w).is_empty(), "silent during the 2 h warm-up");
        // Grace ends at 09:00; still 10 minutes of continuous below needed.
        run(&w, &mut wt, 10);
        assert!(sms(&w).is_empty());
        run(&w, &mut wt, 1); // 09:10
        assert_eq!(sms(&w).len(), 1);
        assert_eq!(
            sms(&w)[0].1,
            "13° below the 75° day setting. Heater is ON (905 W)."
        );
    }

    #[test]
    fn warmup_boundary_is_exact() {
        let s = settings();
        let seven = MIDNIGHT_DENVER + 7 * H;
        assert!(!in_warmup(seven - 1, &s, 120));
        assert!(in_warmup(seven, &s, 120));
        assert!(in_warmup(seven + 2 * H - 1, &s, 120));
        assert!(!in_warmup(seven + 2 * H, &s, 120));
    }

    #[test]
    fn warmup_uses_the_settings_timezone_and_dst() {
        let mut s = settings();
        // Summer: Denver is UTC-6. 2026-07-15 07:00 MDT = 13:00 UTC.
        let july_7am = 1_784_120_400;
        assert!(in_warmup(july_7am, &s, 120));
        assert!(!in_warmup(july_7am - 1, &s, 120));
        // Same instant in New York is 09:00 — past a 2 h grace from 07:00.
        s.timezone = "America/New_York".into();
        assert!(!in_warmup(july_7am, &s, 120));
        assert!(in_warmup(july_7am - 2 * H, &s, 120));
    }

    #[test]
    fn warmup_wraps_midnight() {
        let mut s = settings();
        s.day_start = "23:30".into();
        assert!(in_warmup(MIDNIGHT_DENVER + 15 * M, &s, 120));
        assert!(!in_warmup(MIDNIGHT_DENVER + 2 * H, &s, 120));
    }

    #[test]
    fn night_below_is_not_graced_and_mode_off_is_held() {
        let st = settings();
        let cfg = WatchConfig::default();
        let at_730 = MIDNIGHT_DENVER + 7 * H + 30 * M;
        let r = derive(at_730, Some(&below(58.0, "night", 60.0)), Some(&st), &cfg);
        assert_eq!(r[&Condition::BelowSetpoint], Reading::Raised);
        let r = derive(at_730, Some(&below(58.0, "day", 75.0)), Some(&st), &cfg);
        assert_eq!(r[&Condition::BelowSetpoint], Reading::Held);
        let mut off = below(58.0, "night", 60.0);
        off.mode = "off".into();
        let r = derive(at_730, Some(&off), Some(&st), &cfg);
        assert_eq!(r[&Condition::BelowSetpoint], Reading::Held);
    }

    #[test]
    fn a_night_alert_is_not_falsely_cleared_by_morning_grace() {
        let (w, mut wt) = rig(MIDNIGHT_DENVER + 6 * H + 30 * M, false);
        w.borrow_mut().status = Some(below(55.0, "night", 60.0));
        run(&w, &mut wt, 30); // alert at 06:40, now 07:00
        assert_eq!(sms(&w).len(), 1);
        w.borrow_mut().status = Some(below(56.0, "day", 75.0));
        run(&w, &mut wt, 90);
        assert_eq!(
            sms(&w).len(),
            1,
            "no 'back to normal', no repeat while graced"
        );
        w.borrow_mut().status = Some(ok_status(75.5, "day", 75.0));
        run(&w, &mut wt, 1);
        assert_eq!(sms(&w).len(), 2);
        assert_eq!(sms(&w)[1].1, "Back to normal: 75.5°F, heater OFF.");
    }

    // -- repeat and recovery ------------------------------------------------

    #[test]
    fn repeats_every_interval_and_recovers_once() {
        let (w, mut wt) = rig(MIDNIGHT_DENVER + H, true);
        w.borrow_mut().status = Some(below(58.1, "night", 60.0));
        run(&w, &mut wt, 11); // alert at minute 10
        run(&w, &mut wt, 59);
        assert_eq!(sms(&w).len(), 1);
        run(&w, &mut wt, 1); // minute 70
        assert_eq!(sms(&w).len(), 2);
        assert_eq!(sms(&w)[1].0, "Still: Orchid room 58.1°F");
        run(&w, &mut wt, 60);
        assert_eq!(sms(&w).len(), 3);
        w.borrow_mut().status = Some(ok_status(61.2, "night", 60.0));
        run(&w, &mut wt, 10);
        let s = sms(&w);
        assert_eq!(s.len(), 4);
        assert_eq!(s[3].0, "Orchid room OK");
        assert_eq!(s[3].1, "Back to normal: 61.2°F, heater OFF.");
        let mail = w.borrow().mail.clone();
        assert_eq!(mail.len(), 4);
        assert_eq!(mail[3].1, "Orchid room OK");
    }

    #[test]
    fn no_recovery_without_a_sent_alert() {
        let (w, mut wt) = rig(MIDNIGHT_DENVER + H, false);
        w.borrow_mut().status = Some(below(58.1, "night", 60.0));
        run(&w, &mut wt, 9);
        w.borrow_mut().status = Some(ok_status(61.0, "night", 60.0));
        run(&w, &mut wt, 5);
        assert!(sms(&w).is_empty());
    }

    #[test]
    fn delivery_failure_is_retried_and_channels_are_independent() {
        let (w, mut wt) = rig(MIDNIGHT_DENVER + H, true);
        w.borrow_mut().status = Some(below(58.1, "night", 60.0));
        w.borrow_mut().sms_fails = true;
        run(&w, &mut wt, 11);
        assert!(sms(&w).is_empty());
        assert_eq!(w.borrow().mail.len(), 1);
        w.borrow_mut().sms_fails = false;
        run(&w, &mut wt, 1);
        assert_eq!(sms(&w).len(), 1, "retried on the next tick");
        assert_eq!(
            sms(&w)[0].0,
            "Orchid room 58.1°F",
            "first send is the alert, not a repeat"
        );
        assert_eq!(w.borrow().mail.len(), 1, "email not sent twice");
    }

    #[test]
    fn failed_recovery_is_retried() {
        let (w, mut wt) = rig(MIDNIGHT_DENVER + H, false);
        w.borrow_mut().status = Some(below(58.1, "night", 60.0));
        run(&w, &mut wt, 11);
        w.borrow_mut().status = Some(ok_status(61.0, "night", 60.0));
        w.borrow_mut().sms_fails = true;
        run(&w, &mut wt, 3);
        w.borrow_mut().sms_fails = false;
        run(&w, &mut wt, 3);
        let s = sms(&w);
        assert_eq!(s.len(), 2);
        assert_eq!(s[1].0, "Orchid room OK");
    }

    #[test]
    fn alerts_disabled_sends_nothing() {
        let (w, mut wt) = rig(MIDNIGHT_DENVER + H, true);
        w.borrow_mut().settings.as_mut().unwrap().alerts_enabled = false;
        w.borrow_mut().status = Some(below(58.1, "night", 60.0));
        let mut last = None;
        for _ in 0..30 {
            last = Some(wt.tick(false));
            w.borrow_mut().now += M;
        }
        assert!(sms(&w).is_empty());
        assert!(w.borrow().mail.is_empty());
        let r = last.unwrap();
        assert!(!r.alerts_enabled);
        assert!(!r.outgoing.is_empty(), "still evaluated and reported");
        // Recovers while disabled: nothing owed afterwards.
        w.borrow_mut().status = Some(ok_status(61.0, "night", 60.0));
        run(&w, &mut wt, 1);
        w.borrow_mut().settings.as_mut().unwrap().alerts_enabled = true;
        run(&w, &mut wt, 5);
        assert!(sms(&w).is_empty());
    }

    #[test]
    fn enabling_alerts_mid_episode_sends_the_alert() {
        let (w, mut wt) = rig(MIDNIGHT_DENVER + H, false);
        w.borrow_mut().settings.as_mut().unwrap().alerts_enabled = false;
        w.borrow_mut().status = Some(below(58.1, "night", 60.0));
        run(&w, &mut wt, 20);
        w.borrow_mut().settings.as_mut().unwrap().alerts_enabled = true;
        run(&w, &mut wt, 1);
        assert_eq!(sms(&w).len(), 1);
    }

    #[test]
    fn plug_alert_masking_below_is_worked_out_from_the_reading() {
        let st = settings();
        let cfg = WatchConfig::default();
        let s = with_alert(ok_status(57.0, "night", 60.0), "plug_unreachable");
        let r = derive(MIDNIGHT_DENVER, Some(&s), Some(&st), &cfg);
        assert_eq!(r[&Condition::PlugUnreachable], Reading::Raised);
        assert_eq!(r[&Condition::BelowSetpoint], Reading::Raised);
        assert_eq!(r[&Condition::SensorStale], Reading::Clear);
        let mut s2 = s.clone();
        s2.reading_age_s = Some(30 * 60);
        let r = derive(MIDNIGHT_DENVER, Some(&s2), Some(&st), &cfg);
        assert_eq!(r[&Condition::SensorStale], Reading::Raised);
        assert_eq!(r[&Condition::BelowSetpoint], Reading::Unknown);
        assert_eq!(r[&Condition::TooHot], Reading::Unknown);
    }

    #[test]
    fn mode_off_recovery_says_so() {
        let (w, mut wt) = rig(MIDNIGHT_DENVER + H, false);
        w.borrow_mut().status = Some(below(58.1, "night", 60.0));
        run(&w, &mut wt, 11);
        let mut off = ok_status(58.1, "night", 60.0);
        off.mode = "off".into();
        w.borrow_mut().status = Some(off);
        run(&w, &mut wt, 1);
        assert_eq!(
            sms(&w)[1].1,
            "Back to normal: 58.1°F, heater OFF. The thermostat is set to Off."
        );
    }

    #[test]
    fn dry_run_changes_nothing() {
        let (w, mut wt) = rig(MIDNIGHT_DENVER + H, false);
        w.borrow_mut().status = Some(below(58.1, "night", 60.0));
        run(&w, &mut wt, 10);
        let before = wt.state.clone();
        let r = wt.tick(true);
        assert_eq!(r.outgoing.len(), 1);
        assert!(r.outgoing[0].result.is_none());
        assert!(sms(&w).is_empty());
        assert_eq!(wt.state, before);
    }

    #[test]
    fn email_follows_the_settings_page() {
        let (w, mut wt) = rig(MIDNIGHT_DENVER + H, true);
        w.borrow_mut().settings.as_mut().unwrap().alert_email = "  ".into();
        w.borrow_mut().settings.as_mut().unwrap().alert_phone = "801 555 0199".into();
        w.borrow_mut().status = Some(below(58.1, "night", 60.0));
        run(&w, &mut wt, 11);
        assert!(w.borrow().mail.is_empty());
        assert_eq!(sms(&w)[0].2.as_deref(), Some("+18015550199"));
    }

    // -- relay down (admin channel) ----------------------------------------

    #[test]
    fn fresh_relay_sends_nothing() {
        let (w, mut wt) = rig_admin(MIDNIGHT_DENVER + H);
        run(&w, &mut wt, 30);
        assert!(admin(&w).is_empty());
        assert!(sms(&w).is_empty());
    }

    #[test]
    fn stale_relay_tells_john_repeats_and_recovers_once() {
        let (w, mut wt) = rig_admin(MIDNIGHT_DENVER + H);
        relay(&w, true, Some(10 * M));
        run(&w, &mut wt, 5);
        assert!(admin(&w).is_empty(), "exactly the threshold is not stale");
        relay(&w, true, Some(12 * M));
        run(&w, &mut wt, 1);
        let a = admin(&w);
        assert_eq!(a.len(), 1);
        assert_eq!(a[0].0, "Orchid alerts are down");
        assert_eq!(
            a[0].1,
            "Rose Marie will NOT get orchid texts: the iMessage relay on the iMac has not \
             checked in for 12 minutes. Usually the iMac restarted and nobody has logged \
             into jmurray.\n\
             \n\
             Fix it from your laptop:\n\
             At home: Finder > Go > Connect to Server (Cmd-K), enter vnc://192.168.0.15, \
             log in as jmurray, then close the Screen Sharing window (do not log out).\n\
             Away from home: in Terminal run\n  \
             ssh -N -L 5901:192.168.0.15:5900 gardenway\n\
             then Cmd-K, enter vnc://localhost:5901, and log in as jmurray the same way.\n\
             \n\
             You will get an \"Orchid alerts restored\" text when it is back."
        );
        assert_eq!(a[0].2.as_deref(), Some(JOHN));
        assert!(sms(&w).is_empty(), "never on Rose Marie's channel");

        run(&w, &mut wt, 59);
        assert_eq!(admin(&w).len(), 1, "no repeat inside the repeat interval");
        run(&w, &mut wt, 1);
        assert_eq!(admin(&w).len(), 2);
        assert_eq!(admin(&w)[1].0, "Still: Orchid alerts are down");

        relay(&w, true, Some(20));
        run(&w, &mut wt, 5);
        let a = admin(&w);
        assert_eq!(a.len(), 3, "one recovery");
        assert_eq!(a[2].0, "Orchid alerts restored");
        assert_eq!(
            a[2].1,
            "The iMessage relay on the iMac is checking in again; \
             Rose Marie's orchid alerts are back on."
        );
        assert_eq!(a[2].2.as_deref(), Some(JOHN));
        assert!(sms(&w).is_empty());
    }

    #[test]
    fn relay_that_never_checked_in_is_down() {
        let (w, mut wt) = rig_admin(MIDNIGHT_DENVER + H);
        relay(&w, true, None);
        run(&w, &mut wt, 1);
        let a = admin(&w);
        assert_eq!(a.len(), 1);
        assert!(a[0]
            .1
            .contains("has not checked in for an unknown number of minutes."));
    }

    #[test]
    fn unwatched_relay_is_ignored() {
        let (w, mut wt) = rig_admin(MIDNIGHT_DENVER + H);
        relay(&w, false, None);
        run(&w, &mut wt, 30);
        assert!(admin(&w).is_empty());
    }

    #[test]
    fn without_the_admin_channel_relay_down_is_not_evaluated() {
        let (w, mut wt) = rig(MIDNIGHT_DENVER + H, false);
        relay(&w, true, None);
        let r = wt.tick(false);
        run(&w, &mut wt, 30);
        assert!(!r.readings.contains_key(&Condition::RelayDown));
        assert!(!wt.state.conditions.contains_key(&Condition::RelayDown));
        assert!(sms(&w).is_empty());
    }

    #[test]
    fn orchidd_down_is_mirrored_to_john() {
        let (w, mut wt) = rig_admin(MIDNIGHT_DENVER + H);
        run(&w, &mut wt, 1);
        w.borrow_mut().status = None;
        run(&w, &mut wt, 6);
        let a = admin(&w);
        assert_eq!(
            a.len(),
            1,
            "relay_down is unknown, not raised, while orchidd is down"
        );
        assert_eq!(a[0].0, "Orchid monitor offline");
        assert_eq!(a[0].2.as_deref(), Some(JOHN));
        assert_eq!(sms(&w).len(), 1, "Rose Marie is still told");
        w.borrow_mut().status = Some(ok_status(61.0, "night", 60.0));
        relay(&w, true, Some(30));
        run(&w, &mut wt, 1);
        let a = admin(&w);
        assert_eq!(a.len(), 2);
        assert_eq!(a[1].0, "Orchid room OK");
        assert_eq!(sms(&w).len(), 2);
    }

    #[test]
    fn send_test_includes_the_admin_channel() {
        let (w, mut wt) = rig_admin(MIDNIGHT_DENVER);
        let lines = wt.send_test("hello").unwrap();
        assert_eq!(admin(&w)[0].2.as_deref(), Some(JOHN));
        assert!(lines.contains(&format!("Admin to {JOHN}: sent")));
    }

    // -- persistence --------------------------------------------------------

    #[test]
    fn state_round_trips_and_a_restart_does_not_re_alert() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sub/state.json");
        let (w, mut wt) = rig(MIDNIGHT_DENVER + H, true);
        wt.state_path = Some(path.clone());
        w.borrow_mut().status = Some(below(58.1, "night", 60.0));
        run(&w, &mut wt, 11);
        assert_eq!(sms(&w).len(), 1);

        let loaded = State::load(&path).unwrap();
        assert_eq!(loaded, wt.state);
        assert_eq!(
            loaded.settings.as_ref().unwrap().alert_phone,
            "(801) 555-0101"
        );

        // "Restart": a fresh watcher from disk, orchidd now down.
        let (_, mut wt2) = rig(0, true);
        wt2.clock = Box::new(FClock(w.clone()));
        wt2.fetch = Box::new(FFetch(w.clone()));
        wt2.sms = Some(Box::new(FSms(w.clone())));
        wt2.mail = Some(Box::new(FMail(w.clone())));
        wt2.state = loaded;
        wt2.state_path = Some(path.clone());
        run(&w, &mut wt2, 5);
        assert_eq!(sms(&w).len(), 1, "no re-alert after restart");
        w.borrow_mut().status = None;
        run(&w, &mut wt2, 6);
        let s = sms(&w);
        assert_eq!(s.last().unwrap().0, "Orchid monitor offline");
        assert_eq!(s.last().unwrap().2.as_deref(), Some("+18015550101"));
    }

    #[test]
    fn missing_state_file_is_empty_and_corrupt_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(
            State::load(&dir.path().join("nope.json")).unwrap(),
            State::default()
        );
        let bad = dir.path().join("bad.json");
        std::fs::write(&bad, "{not json").unwrap();
        assert!(State::load(&bad).is_err());
    }

    // -- --test -------------------------------------------------------------

    #[test]
    fn send_test_hits_every_channel_and_reports_failure() {
        let (w, mut wt) = rig(MIDNIGHT_DENVER, true);
        assert!(wt.send_test("hello").is_ok());
        assert_eq!(sms(&w)[0].0, "Orchid alert test");
        assert_eq!(w.borrow().mail[0].2, "hello");
        w.borrow_mut().mail_fails = true;
        assert!(wt.send_test("hello").is_err());
    }

    #[test]
    fn deg_formatting() {
        assert_eq!(deg(60.0), "60");
        assert_eq!(deg(1.94), "1.9");
        assert_eq!(deg(13.0), "13");
    }
}
