//! The real clock, orchidd client, ntfy publisher and SMTP mailer.

use std::path::{Path, PathBuf};
use std::time::Duration;

use lettre::message::header::ContentType;
use lettre::transport::smtp::authentication::Credentials;
use lettre::{Message, SmtpTransport, Transport};

use crate::model::{Settings, Status};
use crate::watch::{Clock, Fetch, Mail, Publish};

pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> i64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_secs() as i64)
    }
}

fn agent(timeout: Duration) -> ureq::Agent {
    ureq::Agent::config_builder()
        .timeout_global(Some(timeout))
        .build()
        .into()
}

pub struct OrchiddHttp {
    base: String,
    agent: ureq::Agent,
}

impl OrchiddHttp {
    pub fn new(base: &str) -> Self {
        OrchiddHttp {
            base: base.trim().trim_end_matches('/').to_string(),
            agent: agent(Duration::from_secs(5)),
        }
    }

    fn get<T: serde::de::DeserializeOwned>(&self, path: &str) -> Result<T, String> {
        let url = format!("{}{path}", self.base);
        self.agent
            .get(&url)
            .call()
            .map_err(|e| format!("GET {url}: {e}"))?
            .body_mut()
            .read_json::<T>()
            .map_err(|e| format!("GET {url}: unexpected reply: {e}"))
    }
}

impl Fetch for OrchiddHttp {
    fn status(&self) -> Result<Status, String> {
        self.get("/api/status")
    }
    fn settings(&self) -> Result<Settings, String> {
        self.get("/api/settings")
    }
}

/// A header carries ASCII only; ntfy decodes RFC 2047 encoded words in
/// `Title`, so "58.1°F" survives the trip. Same encoding the firm's lead
/// alerts use against the same server.
pub fn header_text(raw: &str) -> String {
    if raw.is_ascii() {
        return raw.to_string();
    }
    let mut out = String::from("=?UTF-8?Q?");
    for b in raw.bytes() {
        match b {
            b' ' => out.push('_'),
            b'=' | b'?' | b'_' => out.push_str(&format!("={b:02X}")),
            0x21..=0x7e => out.push(b as char),
            _ => out.push_str(&format!("={b:02X}")),
        }
    }
    out.push_str("?=");
    out
}

/// The ntfy tag the iMessage relay reads as the recipient.
pub fn to_tag(phone: &str) -> String {
    format!("to:{phone}")
}

pub struct Ntfy {
    url: String,
    token: Option<String>,
    agent: ureq::Agent,
}

impl Ntfy {
    pub fn new(url: &str, token: Option<String>) -> Self {
        Ntfy {
            url: url.trim().trim_end_matches('/').to_string(),
            token: token
                .map(|t| t.trim().to_string())
                .filter(|t| !t.is_empty()),
            agent: agent(Duration::from_secs(10)),
        }
    }
}

impl Publish for Ntfy {
    fn publish(&self, title: &str, message: &str, to: Option<&str>) -> Result<(), String> {
        let mut req = self
            .agent
            .post(&self.url)
            .header("Title", header_text(title));
        if let Some(to) = to {
            req = req.header("Tags", to_tag(to));
        }
        if let Some(t) = &self.token {
            req = req.header("Authorization", format!("Bearer {t}"));
        }
        // The URL holds the secret topic name: keep it out of the error.
        req.send(message)
            .map(|_| ())
            .map_err(|e| format!("ntfy publish failed: {e}"))
    }
}

pub struct Smtp {
    transport: SmtpTransport,
    from: String,
}

impl Smtp {
    pub fn new(host: &str, port: u16, user: &str, pass: &str, from: &str) -> Result<Self, String> {
        // 465 is TLS from the first byte; anything else (587) is STARTTLS.
        let builder = if port == 465 {
            SmtpTransport::relay(host)
        } else {
            SmtpTransport::starttls_relay(host)
        }
        .map_err(|e| format!("smtp {host}: {e}"))?;
        let transport = builder
            .port(port)
            .credentials(Credentials::new(user.to_string(), pass.to_string()))
            .timeout(Some(Duration::from_secs(20)))
            .build();
        Ok(Smtp {
            transport,
            from: from.to_string(),
        })
    }
}

impl Mail for Smtp {
    fn send(&self, to: &str, subject: &str, body: &str) -> Result<(), String> {
        let msg = Message::builder()
            .from(
                self.from
                    .parse()
                    .map_err(|e| format!("from {:?}: {e}", self.from))?,
            )
            .to(to.parse().map_err(|e| format!("to {to:?}: {e}"))?)
            .subject(subject)
            .header(ContentType::TEXT_PLAIN)
            .body(body.to_string())
            .map_err(|e| e.to_string())?;
        self.transport
            .send(&msg)
            .map(|_| ())
            .map_err(|e| format!("smtp send failed: {e}"))
    }
}

/// Read a one-line secret file.
pub fn read_secret(path: &Path) -> Result<String, String> {
    std::fs::read_to_string(path)
        .map(|s| s.trim().to_string())
        .map_err(|e| format!("{}: {e}", path.display()))
}

/// A secret's path: the named env var, else systemd's credentials directory
/// (`LoadCredential=`) when the file is there.
pub fn secret_path(
    explicit: Option<String>,
    credentials_dir: Option<String>,
    name: &str,
) -> Option<PathBuf> {
    if let Some(p) = explicit.filter(|p| !p.trim().is_empty()) {
        return Some(PathBuf::from(p));
    }
    let p = PathBuf::from(credentials_dir?).join(name);
    p.exists().then_some(p)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn titles_with_degree_signs_are_encoded() {
        assert_eq!(
            header_text("Orchid monitor offline"),
            "Orchid monitor offline"
        );
        assert_eq!(
            header_text("Orchid room 58.1°F"),
            "=?UTF-8?Q?Orchid_room_58.1=C2=B0F?="
        );
        assert!(header_text("Still: a=b?").is_ascii());
    }

    #[test]
    fn tag_names_the_recipient() {
        assert_eq!(to_tag("+18015550101"), "to:+18015550101");
    }

    #[test]
    fn secret_path_prefers_explicit_then_credentials_dir() {
        let dir = tempfile::tempdir().unwrap();
        let cred = dir.path().to_string_lossy().to_string();
        assert_eq!(
            secret_path(Some("/x/y".into()), Some(cred.clone()), "ntfy.token"),
            Some(PathBuf::from("/x/y"))
        );
        assert_eq!(secret_path(None, Some(cred.clone()), "ntfy.token"), None);
        std::fs::write(dir.path().join("ntfy.token"), "tk_abc\n").unwrap();
        let p = secret_path(None, Some(cred), "ntfy.token").unwrap();
        assert_eq!(read_secret(&p).unwrap(), "tk_abc");
        assert_eq!(secret_path(None, None, "ntfy.token"), None);
    }
}
