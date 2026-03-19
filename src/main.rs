use std::collections::HashMap;
use std::env;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use bollard::query_parameters::{ListContainersOptions, LogsOptions};
use bollard::Docker;
use futures_util::StreamExt;
use regex::Regex;
use reqwest::Client;
use tokio::signal;
use tokio::time::sleep;
use tracing::{error, info, warn};

/// All configuration loaded from environment variables.
/// Using a struct rather than passing individual values keeps
/// function signatures clean as the config grows.
enum ContainerDiscovery {
    NamePrefix(String),
    Label { key: String, value: String },
}

struct Config {
    discovery: ContainerDiscovery,
    link_pattern: Regex,
    telegram_bot_token: Option<String>,
    telegram_chat_id: Option<String>,
    discovery_interval: Duration,
    reconnect_delay: Duration,
}

#[tokio::main]
async fn main() -> Result<()> {
    // Initialize structured logging. RUST_LOG env controls verbosity,
    // defaults to info level. In production you get JSON logs which
    // play nicely with log aggregators.
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "cap_link_notifier=info".into()),
        )
        .init();

    let cfg = Config::from_env()?;

    match &cfg.discovery {
        ContainerDiscovery::NamePrefix(prefix) => {
            info!(name_prefix = %prefix, "starting cap-link-notifier");
        }
        ContainerDiscovery::Label { key, value } => {
            info!(label_key = %key, label_value = %value, "starting cap-link-notifier");
        }
    }

    // tokio::select! races two futures: our main loop vs the shutdown
    // signal. Whichever completes first wins, the other is dropped.
    // This is the idiomatic way to handle graceful shutdown in tokio.
    tokio::select! {
        result = run(&cfg) => {
            if let Err(e) = result {
                error!(err = %e, "fatal error");
                std::process::exit(1);
            }
        }
        _ = shutdown_signal() => {
            info!("received shutdown signal");
        }
    }

    info!("shutting down");
    Ok(())
}

/// Listens for SIGINT or SIGTERM. On first signal, returns gracefully.
async fn shutdown_signal() {
    let ctrl_c = signal::ctrl_c();
    let mut sigterm = signal::unix::signal(signal::unix::SignalKind::terminate())
        .expect("failed to register SIGTERM handler");

    tokio::select! {
        _ = ctrl_c => {}
        _ = sigterm.recv() => {}
    }
}

/// Main loop: discover container → tail logs → reconnect on failure.
///
/// Note the pattern here: an infinite loop with early returns for
/// fatal errors, and `continue` for recoverable ones. This is common
/// in long-running Rust services — no recursion, no complex state machines.
async fn run(cfg: &Config) -> Result<()> {
    let docker = connect_docker().await?;
    info!("connected to Docker daemon");

    // reqwest::Client uses connection pooling internally, so we
    // create one and reuse it for all Telegram API calls.
    let http = Client::new();

    loop {
        match find_container(&docker, cfg).await? {
            Some(id) => {
                let short_id = &id[..12];
                info!(id = short_id, "attached to container");

                match tail_logs(&docker, &http, &id, cfg).await {
                    Ok(()) => {
                        // Stream ended cleanly (container stopped).
                        warn!(delay = ?cfg.reconnect_delay, "log stream ended, will reconnect");
                    }
                    Err(e) => {
                        warn!(
                            err = %e,
                            delay = ?cfg.reconnect_delay,
                            "log stream error, will reconnect"
                        );
                    }
                }
                sleep(cfg.reconnect_delay).await;
            }
            None => {
                warn!(
                    interval = ?cfg.discovery_interval,
                    "target container not found, retrying"
                );
                sleep(cfg.discovery_interval).await;
            }
        }
    }
}

/// Finds a running container matching the configured compose service label.
///
/// Returns the container ID or None if not found. If multiple containers
/// match (e.g., during a rolling update), picks the most recently created.
async fn find_container(docker: &Docker, cfg: &Config) -> Result<Option<String>> {
    let mut filters = HashMap::new();
    filters.insert("status".to_string(), vec!["running".to_string()]);

    match &cfg.discovery {
        ContainerDiscovery::NamePrefix(prefix) => {
            filters.insert("name".to_string(), vec![prefix.clone()]);
        }
        ContainerDiscovery::Label { key, value } => {
            filters.insert("label".to_string(), vec![format!("{}={}", key, value)]);
        }
    }

    let containers = docker
        .list_containers(Some(ListContainersOptions {
            filters: Some(filters),
            ..Default::default()
        }))
        .await
        .context("failed to list containers")?;

    let best = containers
        .iter()
        .max_by_key(|c| c.created.unwrap_or(0));

    Ok(best.and_then(|c| c.id.clone()))
}

/// Tails container logs as an async stream and scans for login links.
///
/// This is where bollard shines — ContainerLogs returns a Stream of
/// LogOutput items. We use StreamExt::next() to consume them one at
/// a time without buffering the entire log history.
async fn tail_logs(docker: &Docker, http: &Client, container_id: &str, cfg: &Config) -> Result<()> {
    let opts = LogsOptions {
        follow: true,
        stdout: true,
        stderr: true,
        tail: "0".to_string(), // Only new lines from now.
        ..Default::default()
    };

    // .logs() returns a Stream<Item = Result<LogOutput>>. Unlike the
    // Go Docker client which gives you a raw io.Reader with multiplexed
    // frames, bollard already demultiplexes stdout/stderr for you —
    // each LogOutput variant (StdOut/StdErr) contains clean text.
    let mut stream = docker.logs(container_id, Some(opts));
    let mut scanner = LogScanner::new(cfg.link_pattern.clone());

    while let Some(result) = stream.next().await {
        let output = result.context("error reading log stream")?;
        let bytes = output.into_bytes();
        let line = String::from_utf8_lossy(&bytes);

        if line.trim().is_empty() {
            continue;
        }

        for capture in scanner.feed(&line) {
            if let Err(e) = send_notification(http, cfg, &capture).await {
                error!(err = %e, "notification send failed");
            }
        }
    }

    Ok(())
}

async fn connect_docker() -> Result<Docker> {
    if let Ok(docker) = Docker::connect_with_local_defaults() {
        if docker.ping().await.is_ok() {
            return Ok(docker);
        }
    }

    let home = env::var("HOME").unwrap_or_default();
    let candidates = [
        format!("{}/.rd/docker.sock", home),
        format!("{}/.colima/default/docker.sock", home),
        "/var/run/docker.sock".to_string(),
    ];

    for path in &candidates {
        if !std::path::Path::new(path).exists() {
            continue;
        }
        let url = format!("unix://{}", path);
        if let Ok(docker) = Docker::connect_with_socket(&url, 120, bollard::API_DEFAULT_VERSION) {
            if docker.ping().await.is_ok() {
                info!(socket = %path, "connected via alternative socket");
                return Ok(docker);
            }
        }
    }

    bail!("no Docker socket found — set DOCKER_HOST or check your Docker/Rancher Desktop installation")
}

async fn send_notification(http: &Client, cfg: &Config, capture: &Capture) -> Result<()> {
    let text = match capture {
        Capture::LoginLink(link) => {
            info!(link = %link, "captured login link");
            format!("🔐 *Cap Login Link*\n\n`{}`", link)
        }
        Capture::VerificationCode { email, code } => {
            info!(email = %email, code = %code, "captured verification code");
            format!("🔐 *Cap Login*\n\n📧 `{}`\n🔢 Code: `{}`", email, code)
        }
    };

    let (Some(token), Some(chat_id)) = (&cfg.telegram_bot_token, &cfg.telegram_chat_id) else {
        println!("{}", text);
        return Ok(());
    };

    let url = format!("https://api.telegram.org/bot{}/sendMessage", token);

    let params = [
        ("chat_id", chat_id.as_str()),
        ("text", &text),
        ("parse_mode", "Markdown"),
        ("disable_web_page_preview", "true"),
    ];

    let resp = http
        .post(&url)
        .form(&params)
        .send()
        .await
        .context("telegram request failed")?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body: serde_json::Value = resp.json().await.unwrap_or_default();
        bail!("telegram API {}: {}", status, body);
    }

    info!("telegram notification sent");
    Ok(())
}

impl Config {
    /// Loads configuration from environment variables.
    ///
    /// This pattern — a `from_env()` constructor on the config struct —
    /// is common in Rust services. It centralises all env parsing and
    /// validation so main() stays clean.
    fn from_env() -> Result<Self> {
        let telegram_bot_token = env::var("TELEGRAM_BOT_TOKEN").ok();
        let telegram_chat_id = env::var("TELEGRAM_CHAT_ID").ok();

        if telegram_bot_token.is_none() || telegram_chat_id.is_none() {
            warn!("TELEGRAM_BOT_TOKEN or TELEGRAM_CHAT_ID not set — notifications will print to stdout");
        }

        let discovery = if let Ok(prefix) = env::var("CONTAINER_NAME_PREFIX") {
            ContainerDiscovery::NamePrefix(prefix)
        } else {
            let key = env::var("SERVICE_LABEL")
                .unwrap_or_else(|_| "com.docker.compose.service".to_string());
            let value = env::var("SERVICE_VALUE").unwrap_or_else(|_| "cap".to_string());
            ContainerDiscovery::Label { key, value }
        };

        let pattern_str = env::var("LINK_PATTERN").unwrap_or_else(|_| {
            r#"https?://[^\s"'\]}>]+(?:callback|verify|token|magic|signin|auth)[^\s"'\]}>]*"#
                .to_string()
        });
        let link_pattern =
            Regex::new(&pattern_str).context("invalid LINK_PATTERN regex")?;

        let discovery_interval = parse_duration_or("DISCOVERY_INTERVAL", Duration::from_secs(5));
        let reconnect_delay = parse_duration_or("RECONNECT_DELAY", Duration::from_secs(3));

        Ok(Self {
            discovery,
            link_pattern,
            telegram_bot_token,
            telegram_chat_id,
            discovery_interval,
            reconnect_delay,
        })
    }
}

/// Parses a Go-style duration string (e.g., "5s", "100ms") from an env var.
/// Falls back to the provided default if unset or unparseable.
///
/// Note: this is a standalone function, not a method, because it doesn't
/// need access to any struct state. In Rust, free functions are perfectly
/// normal — not everything needs to be a method.
enum Capture {
    LoginLink(String),
    VerificationCode { email: String, code: String },
}

struct LogScanner {
    link_pattern: Regex,
    pending_email: Option<String>,
}

impl LogScanner {
    fn new(link_pattern: Regex) -> Self {
        Self {
            link_pattern,
            pending_email: None,
        }
    }

    fn feed(&mut self, line: &str) -> Vec<Capture> {
        let mut captures = Vec::new();

        if let Some(email) = line.split("Email:").nth(1).map(|s| s.trim().to_string()) {
            if !email.is_empty() {
                self.pending_email = Some(email);
            }
        }

        if let Some(code) = line.split("Code:").nth(1).map(|s| s.trim().to_string()) {
            if let Some(email) = self.pending_email.take() {
                if !code.is_empty() {
                    captures.push(Capture::VerificationCode { email, code });
                }
            }
        }

        for mat in self.link_pattern.find_iter(line) {
            captures.push(Capture::LoginLink(mat.as_str().to_string()));
        }

        captures
    }
}

fn parse_duration_or(key: &str, default: Duration) -> Duration {
    let Ok(val) = env::var(key) else {
        return default;
    };

    // Quick and dirty duration parser: strip the suffix, parse the number.
    // Supports s, ms, m formats which covers the common cases.
    let val = val.trim();
    if let Some(secs) = val.strip_suffix('s') {
        if let Some(ms) = secs.strip_suffix('m') {
            // "ms" suffix
            if let Ok(n) = ms.parse::<u64>() {
                return Duration::from_millis(n);
            }
        } else if let Ok(n) = secs.parse::<u64>() {
            return Duration::from_secs(n);
        }
    } else if let Some(mins) = val.strip_suffix('m') {
        if let Ok(n) = mins.parse::<u64>() {
            return Duration::from_secs(n * 60);
        }
    }

    warn!(
        key,
        value = val,
        ?default,
        "invalid duration, using default"
    );
    default
}

#[cfg(test)]
mod tests {
    use super::*;

    fn default_link_pattern() -> Regex {
        Regex::new(
            r#"https?://[^\s"'\]}>]+(?:callback|verify|token|magic|signin|auth)[^\s"'\]}>]*"#,
        )
        .unwrap()
    }

    fn scanner() -> LogScanner {
        LogScanner::new(default_link_pattern())
    }

    #[test]
    fn captures_verification_code_from_cap_logs() {
        let mut s = scanner();
        let lines = [
            "2026-Mar-19 06:32:10 📧 Email: foo@bar.com",
            "2026-Mar-19 06:32:10 🔢 Code: 523803",
        ];

        let caps = lines.iter().flat_map(|l| s.feed(l)).collect::<Vec<_>>();

        assert_eq!(caps.len(), 1);
        match &caps[0] {
            Capture::VerificationCode { email, code } => {
                assert_eq!(email, "foo@bar.com");
                assert_eq!(code, "523803");
            }
            _ => panic!("expected VerificationCode"),
        }
    }

    #[test]
    fn ignores_code_without_preceding_email() {
        let mut s = scanner();
        let caps = s.feed("2026-Mar-19 06:32:10 🔢 Code: 523803");
        assert!(caps.is_empty());
    }

    #[test]
    fn email_without_code_does_not_emit() {
        let mut s = scanner();
        let caps = s.feed("2026-Mar-19 06:32:10 📧 Email: foo@bar.com");
        assert!(caps.is_empty());
    }

    #[test]
    fn captures_url_based_magic_links() {
        let mut s = scanner();
        let caps = s.feed("Visit https://app.cap.so/api/auth/callback/credentials?token=abc123 to login");

        assert_eq!(caps.len(), 1);
        match &caps[0] {
            Capture::LoginLink(url) => {
                assert!(url.contains("callback"));
                assert!(url.contains("token=abc123"));
            }
            _ => panic!("expected LoginLink"),
        }
    }

    #[test]
    fn ignores_unrelated_log_lines() {
        let mut s = scanner();
        let lines = [
            "2026-Mar-18 11:47:29 [ShareVideoPage] Starting transcription for video: 1reec20z43k6p7m",
            "2026-Mar-19 04:24:09 auth header:  Bearer 2f8503c2-ab18-4747-aa61-88b09e5f8878",
            "2026-Mar-19 04:24:09 Using API key auth",
        ];

        let caps: Vec<_> = lines.iter().flat_map(|l| s.feed(l)).collect();
        assert!(caps.is_empty());
    }

    #[test]
    fn does_not_match_nextjs_error_urls() {
        let mut s = scanner();
        let caps = s.feed("Read more: https://nextjs.org/docs/messages/failed-to-find-server-action");
        assert!(caps.is_empty());
    }

    #[test]
    fn handles_multiple_verification_blocks() {
        let mut s = scanner();
        let lines = [
            "📧 Email: alice@example.com",
            "🔢 Code: 111111",
            "📧 Email: bob@example.com",
            "🔢 Code: 222222",
        ];

        let caps: Vec<_> = lines.iter().flat_map(|l| s.feed(l)).collect();
        assert_eq!(caps.len(), 2);

        match &caps[0] {
            Capture::VerificationCode { email, code } => {
                assert_eq!(email, "alice@example.com");
                assert_eq!(code, "111111");
            }
            _ => panic!("expected VerificationCode"),
        }
        match &caps[1] {
            Capture::VerificationCode { email, code } => {
                assert_eq!(email, "bob@example.com");
                assert_eq!(code, "222222");
            }
            _ => panic!("expected VerificationCode"),
        }
    }

    #[test]
    fn stale_email_replaced_by_newer_one() {
        let mut s = scanner();
        s.feed("📧 Email: stale@example.com");
        s.feed("📧 Email: fresh@example.com");
        let caps = s.feed("🔢 Code: 999999");

        assert_eq!(caps.len(), 1);
        match &caps[0] {
            Capture::VerificationCode { email, .. } => {
                assert_eq!(email, "fresh@example.com");
            }
            _ => panic!("expected VerificationCode"),
        }
    }
}
