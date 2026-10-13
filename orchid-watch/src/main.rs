use std::process::ExitCode;
use std::time::{Duration, Instant};

use orchid_watch::config::Config;
use orchid_watch::io::{read_secret, Ntfy, OrchiddHttp, Smtp, SystemClock};
use orchid_watch::watch::{log, Mail, Publish, State, Watcher};

const USAGE: &str = "usage: orchid-watch            run forever (the service)
       orchid-watch --once     one dry run: prints readings, what would be sent, and the saved state; sends nothing
       orchid-watch --test MSG send MSG through every configured channel; non-zero exit on failure

Environment: ORCHID_WATCH_URL, ORCHID_WATCH_STATE, ORCHID_NTFY_URL, ORCHID_NTFY_TOKEN_FILE,
ORCHID_WARMUP_MIN, ORCHID_REPEAT_MIN, ORCHID_HOT_F, ORCHID_INTERVAL_S,
ORCHID_SMTP_HOST, ORCHID_SMTP_PORT, ORCHID_SMTP_USER, ORCHID_SMTP_PASS_FILE, ORCHID_SMTP_FROM,
ORCHID_ADMIN_NTFY_URL, ORCHID_ADMIN_NTFY_TOKEN_FILE, ORCHID_ADMIN_TO, ORCHID_RELAY_STALE_MIN";

enum Cmd {
    Run,
    Once,
    Test(String),
}

fn parse_args() -> Result<Cmd, String> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args
        .iter()
        .map(String::as_str)
        .collect::<Vec<_>>()
        .as_slice()
    {
        [] => Ok(Cmd::Run),
        ["--once"] => Ok(Cmd::Once),
        ["--test"] => Ok(Cmd::Test(
            "Test from orchid-watch: alerts are working.".into(),
        )),
        ["--test", msg] => Ok(Cmd::Test(msg.to_string())),
        _ => Err(USAGE.into()),
    }
}

fn build(cfg: &Config, quiet: bool) -> Result<Watcher, String> {
    let sms: Option<Box<dyn Publish>> = match &cfg.ntfy_url {
        Some(url) => {
            let token = match &cfg.ntfy_token_file {
                Some(p) => Some(read_secret(p)?),
                None => {
                    log("no ntfy token file; publishing without auth");
                    None
                }
            };
            Some(Box::new(Ntfy::new(url, token)))
        }
        None => {
            log("ORCHID_NTFY_URL not set: text alerts are disabled");
            None
        }
    };
    let mail: Option<Box<dyn Mail>> = match &cfg.smtp {
        Some(s) => {
            let pass = read_secret(&s.pass_file)?;
            Some(Box::new(Smtp::new(
                &s.host, s.port, &s.user, &pass, &s.from,
            )?))
        }
        None => {
            log("email disabled (set ORCHID_SMTP_HOST, ORCHID_SMTP_USER, ORCHID_SMTP_PASS_FILE to enable)");
            None
        }
    };
    // John's channel: tells him when Rose Marie's relay (or orchidd) is down.
    let admin: Option<(Box<dyn Publish>, String)> = match &cfg.admin {
        Some(a) => {
            let token = a.token_file.as_deref().map(read_secret).transpose()?;
            Some((Box::new(Ntfy::new(&a.ntfy_url, token)), a.to.clone()))
        }
        None => None,
    };
    let state = match State::load(&cfg.state_path) {
        Ok(s) => s,
        Err(e) => {
            log(&format!("state unreadable, starting empty: {e}"));
            State::default()
        }
    };
    if !quiet {
        log(&format!(
            "watching {} every {} s (text: {}, email: {}, admin: {})",
            cfg.url,
            cfg.interval_s,
            if sms.is_some() { "on" } else { "off" },
            if mail.is_some() { "on" } else { "off" },
            if admin.is_some() { "on" } else { "off" },
        ));
    }
    Ok(Watcher {
        clock: Box::new(SystemClock),
        fetch: Box::new(OrchiddHttp::new(&cfg.url)),
        sms,
        mail,
        admin,
        cfg: cfg.watch.clone(),
        state,
        state_path: Some(cfg.state_path.clone()),
    })
}

fn main() -> ExitCode {
    let cmd = match parse_args() {
        Ok(c) => c,
        Err(u) => {
            eprintln!("{u}");
            return ExitCode::from(2);
        }
    };
    let cfg = match Config::from_lookup(|k| std::env::var(k).ok()) {
        Ok(c) => c,
        Err(e) => {
            log(&format!("config: {e}"));
            return ExitCode::from(2);
        }
    };
    let mut w = match build(&cfg, !matches!(cmd, Cmd::Run)) {
        Ok(w) => w,
        Err(e) => {
            log(&format!("startup: {e}"));
            return ExitCode::FAILURE;
        }
    };
    match cmd {
        Cmd::Once => {
            let report = w.tick(true);
            let out = serde_json::json!({ "report": report, "saved_state": w.state });
            println!("{}", serde_json::to_string_pretty(&out).unwrap_or_default());
            ExitCode::SUCCESS
        }
        Cmd::Test(msg) => match w.send_test(&msg) {
            Ok(lines) => {
                lines.iter().for_each(|l| println!("ok: {l}"));
                ExitCode::SUCCESS
            }
            Err(lines) => {
                lines.iter().for_each(|l| println!("FAILED/info: {l}"));
                ExitCode::FAILURE
            }
        },
        Cmd::Run => {
            let every = Duration::from_secs(cfg.interval_s);
            loop {
                let started = Instant::now();
                let r = w.tick(false);
                if let Some(e) = &r.fetch_error {
                    log(&format!("orchidd unreachable: {e}"));
                }
                std::thread::sleep(every.saturating_sub(started.elapsed()));
            }
        }
    }
}
