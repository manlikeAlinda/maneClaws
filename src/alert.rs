//! Opt-in email alerting for events a human should be paged for: the bot going
//! dead, an unhandled tick error, auth cooldown engaging, or a panic-flatten
//! failure. Entirely no-op unless BOT_ALERT_SMTP_* env vars are configured —
//! matches the opt-in-via-env-var pattern used by telemetry.rs/pipeline's JSON
//! audit log elsewhere in this codebase.

use lettre::message::Message;
use lettre::transport::smtp::authentication::Credentials;
use lettre::{AsyncSmtpTransport, AsyncTransport, Tokio1Executor};

#[derive(Debug, Clone)]
pub struct AlertConfig {
    pub host: String,
    pub port: u16,
    pub username: String,
    pub password: String,
    pub from: String,
    pub to: String,
}

impl AlertConfig {
    pub fn from_env() -> Option<Self> {
        let host = std::env::var("BOT_ALERT_SMTP_HOST").ok()?;
        if host.trim().is_empty() {
            return None;
        }
        let port = std::env::var("BOT_ALERT_SMTP_PORT")
            .ok()
            .and_then(|v| v.parse::<u16>().ok())
            .unwrap_or(587);
        let username = std::env::var("BOT_ALERT_SMTP_USER").unwrap_or_default();
        let password = std::env::var("BOT_ALERT_SMTP_PASS").unwrap_or_default();
        let from = std::env::var("BOT_ALERT_EMAIL_FROM").unwrap_or_default();
        let to = std::env::var("BOT_ALERT_EMAIL_TO").unwrap_or_default();
        if from.trim().is_empty() || to.trim().is_empty() {
            return None;
        }
        Some(Self { host, port, username, password, from, to })
    }
}

/// Send an alert email. No-op (returns immediately) if BOT_ALERT_SMTP_HOST /
/// BOT_ALERT_EMAIL_FROM / BOT_ALERT_EMAIL_TO aren't all configured. Never
/// panics; logs a warning on send failure rather than propagating an error,
/// since a failed alert must not fail the trading tick that triggered it.
pub async fn notify(subject: &str, body: &str) {
    let Some(cfg) = AlertConfig::from_env() else {
        return;
    };

    let message = match Message::builder()
        .from(match cfg.from.parse() {
            Ok(addr) => addr,
            Err(e) => {
                tracing::warn!("Alert email: invalid BOT_ALERT_EMAIL_FROM: {e}");
                return;
            }
        })
        .to(match cfg.to.parse() {
            Ok(addr) => addr,
            Err(e) => {
                tracing::warn!("Alert email: invalid BOT_ALERT_EMAIL_TO: {e}");
                return;
            }
        })
        .subject(subject)
        .body(body.to_string())
    {
        Ok(m) => m,
        Err(e) => {
            tracing::warn!("Alert email: failed to build message: {e}");
            return;
        }
    };

    let creds = Credentials::new(cfg.username.clone(), cfg.password.clone());
    let transport = match AsyncSmtpTransport::<Tokio1Executor>::relay(&cfg.host) {
        Ok(t) => t.port(cfg.port).credentials(creds).build(),
        Err(e) => {
            tracing::warn!("Alert email: failed to build SMTP transport: {e}");
            return;
        }
    };

    if let Err(e) = transport.send(message).await {
        tracing::warn!("Alert email: send failed: {e}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn clear_env() {
        for k in [
            "BOT_ALERT_SMTP_HOST",
            "BOT_ALERT_SMTP_PORT",
            "BOT_ALERT_SMTP_USER",
            "BOT_ALERT_SMTP_PASS",
            "BOT_ALERT_EMAIL_FROM",
            "BOT_ALERT_EMAIL_TO",
        ] {
            unsafe {
                std::env::remove_var(k);
            }
        }
    }

    #[test]
    fn from_env_none_when_unconfigured() {
        clear_env();
        assert!(AlertConfig::from_env().is_none());
    }

    #[test]
    fn from_env_none_when_host_set_but_to_from_missing() {
        clear_env();
        unsafe {
            std::env::set_var("BOT_ALERT_SMTP_HOST", "smtp.example.com");
        }
        assert!(AlertConfig::from_env().is_none());
        clear_env();
    }

    #[test]
    fn from_env_some_when_fully_configured() {
        clear_env();
        unsafe {
            std::env::set_var("BOT_ALERT_SMTP_HOST", "smtp.example.com");
            std::env::set_var("BOT_ALERT_EMAIL_FROM", "bot@example.com");
            std::env::set_var("BOT_ALERT_EMAIL_TO", "me@example.com");
        }
        let cfg = AlertConfig::from_env().expect("should be configured");
        assert_eq!(cfg.host, "smtp.example.com");
        assert_eq!(cfg.port, 587);
        clear_env();
    }

    #[tokio::test]
    async fn notify_is_noop_when_unconfigured() {
        clear_env();
        // Must return promptly without attempting any network I/O.
        notify("test", "body").await;
    }
}
