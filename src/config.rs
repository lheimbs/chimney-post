use crate::error::{ChimneyError, Result};
use serde::Deserialize;
use std::env;
use std::fs;
use std::net::SocketAddr;
use std::path::Path;

pub const DEFAULT_MESSAGE_TEMPLATE: &str = "\
{%- if from %}From: {{ from }}
{% endif %}\
{%- if to %}To: {{ to }}
{% endif %}\
{%- if subject %}Subject: {{ subject }}{% else %}Subject: (none){% endif %}

{%- if body and body is string and body | trim %}
{{ body }}\
{% else %}
(empty message body)\
{% endif %}";

#[derive(Clone, Debug, Deserialize)]
pub struct Config {
    pub smtp: SmtpConfig,
    pub matrix: MatrixConfig,
    pub logging: LoggingConfig,
    pub queue: QueueConfig,
}

#[derive(Clone, Debug, Deserialize)]
pub struct SmtpConfig {
    pub bind: String,
    pub max_message_size: usize,
    pub timeout: u64,
    /// Maximum number of simultaneous SMTP connections. Excess connections are
    /// rejected with `421` so a flood cannot exhaust file descriptors or memory.
    #[serde(default = "default_max_connections")]
    pub max_connections: usize,
    /// Maximum lifetime of a single SMTP session in seconds, independent of the
    /// per-read `timeout`. Bounds slowloris-style connections that dribble bytes
    /// just often enough to keep the per-read timeout from firing.
    #[serde(default = "default_max_session_seconds")]
    pub max_session_seconds: u64,
}

fn default_max_connections() -> usize {
    100
}

fn default_max_session_seconds() -> u64 {
    300
}

#[derive(Clone, Debug, Deserialize)]
pub struct MatrixConfig {
    pub homeserver: String,
    pub user_id: String,
    pub device_name: String,
    pub room_id: String,
    pub store_path: String,
    #[serde(default = "default_require_e2ee")]
    pub require_e2ee: bool,
    #[serde(default = "default_message_template")]
    pub message_template: String,
    pub credentials: MatrixCredentials,
}

fn default_require_e2ee() -> bool {
    true
}

fn default_message_template() -> String {
    DEFAULT_MESSAGE_TEMPLATE.to_string()
}

#[derive(Clone, Deserialize)]
pub struct MatrixCredentials {
    pub password: Option<String>,
    pub access_token: Option<String>,
    pub device_id: Option<String>,
}

impl std::fmt::Debug for MatrixCredentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MatrixCredentials")
            .field("password", &self.password.as_ref().map(|_| "[REDACTED]"))
            .field(
                "access_token",
                &self.access_token.as_ref().map(|_| "[REDACTED]"),
            )
            .field("device_id", &self.device_id)
            .finish()
    }
}

#[derive(Clone, Debug, Deserialize)]
pub struct LoggingConfig {
    pub level: String,
    pub format: String,
}

#[derive(Clone, Debug, Deserialize)]
pub struct QueueConfig {
    pub max_retries: u32,
    pub retry_backoff: u64,
    /// Path to the persistent SQLite outbox database.
    #[serde(default = "default_queue_db_path")]
    pub db_path: String,
    /// Maximum number of messages held in the outbox before new mail is
    /// rejected with a temporary SMTP error (451), applying backpressure so the
    /// queue cannot grow without bound (e.g. while Matrix is unreachable).
    /// 0 means unlimited.
    #[serde(default = "default_queue_max_len")]
    pub max_len: usize,
}

fn default_queue_db_path() -> String {
    "/var/lib/chimney-post/queue.db".to_string()
}

fn default_queue_max_len() -> usize {
    10_000
}

impl Config {
    pub fn load_default() -> Result<Self> {
        let path = env::var("CHIMNEY_CONFIG").unwrap_or_else(|_| "config.toml".to_string());
        Self::load_from_path(path)
    }

    pub fn load_from_path<P: AsRef<Path>>(path: P) -> Result<Self> {
        let path = path.as_ref();
        let content = fs::read_to_string(path)
            .map_err(|e| ChimneyError::Config(format!("{}: {e}", path.display())))?;
        let mut config: Self = toml::from_str(&content)
            .map_err(|e| ChimneyError::Config(format!("{}: {e}", path.display())))?;
        config.apply_env_overrides()?;
        config.validate()?;
        Ok(config)
    }

    pub fn validate(&self) -> Result<()> {
        let _addr: SocketAddr = self.smtp.bind.parse().map_err(|_| {
            ChimneyError::Config("smtp.bind must be a valid IP:PORT address".to_string())
        })?;

        if self.smtp.max_message_size == 0 {
            return Err(ChimneyError::Config(
                "smtp.max_message_size must be greater than zero".to_string(),
            ));
        }

        if self.smtp.max_connections == 0 {
            return Err(ChimneyError::Config(
                "smtp.max_connections must be greater than zero".to_string(),
            ));
        }

        if self.smtp.max_session_seconds == 0 {
            return Err(ChimneyError::Config(
                "smtp.max_session_seconds must be greater than zero".to_string(),
            ));
        }

        if is_blank(&self.queue.db_path) {
            return Err(ChimneyError::Config(
                "queue.db_path must not be empty".to_string(),
            ));
        }

        if is_blank(&self.matrix.homeserver) {
            return Err(ChimneyError::Config(
                "matrix.homeserver must not be empty".to_string(),
            ));
        }

        if is_blank(&self.matrix.user_id) {
            return Err(ChimneyError::Config(
                "matrix.user_id must not be empty".to_string(),
            ));
        }

        if is_blank(&self.matrix.room_id) {
            return Err(ChimneyError::Config(
                "matrix.room_id must not be empty".to_string(),
            ));
        }

        if is_blank(&self.matrix.store_path) {
            return Err(ChimneyError::Config(
                "matrix.store_path must not be empty".to_string(),
            ));
        }

        if self.matrix.credentials.password.is_none()
            && self.matrix.credentials.access_token.is_none()
        {
            return Err(ChimneyError::Config(
                "matrix credentials require password or access_token".to_string(),
            ));
        }

        if self.matrix.credentials.password.is_some()
            && self.matrix.credentials.access_token.is_some()
        {
            return Err(ChimneyError::Config(
                "matrix credentials must use either password or access_token, not both".to_string(),
            ));
        }

        if let Some(access_token) = self.matrix.credentials.access_token.as_deref() {
            if access_token.trim().is_empty() {
                return Err(ChimneyError::Config(
                    "matrix.credentials.access_token must not be empty".to_string(),
                ));
            }

            if self
                .matrix
                .credentials
                .device_id
                .as_deref()
                .map_or(true, |value| value.trim().is_empty())
            {
                return Err(ChimneyError::Config(
                    "matrix.credentials.device_id is required when using access_token".to_string(),
                ));
            }
        }

        // Validate the message template by parsing and trial-rendering it
        {
            let mut env = minijinja::Environment::new();
            env.set_auto_escape_callback(|_| minijinja::AutoEscape::None);
            env.add_template("message", &self.matrix.message_template)
                .map_err(|error| {
                    ChimneyError::Config(format!(
                        "matrix.message_template failed to parse: {error}"
                    ))
                })?;
            let tmpl = env.get_template("message").map_err(|error| {
                ChimneyError::Config(format!("matrix.message_template internal error: {error}"))
            })?;
            let test_ctx = minijinja::context! {
                from => "test@example.com",
                to => "recipient@example.com",
                subject => "Test Subject",
                body => "Test body",
            };
            tmpl.render(test_ctx).map_err(|error| {
                ChimneyError::Config(format!(
                    "matrix.message_template trial render failed: {error}"
                ))
            })?;
        }

        Ok(())
    }

    fn apply_env_overrides(&mut self) -> Result<()> {
        if self.matrix.credentials.password.as_deref() == Some("${MATRIX_PASSWORD}") {
            self.matrix.credentials.password = Some(env::var("MATRIX_PASSWORD").map_err(|_| {
                ChimneyError::Config(
                    "config references ${MATRIX_PASSWORD} but the env var is not set".to_string(),
                )
            })?);
        }

        if self.matrix.credentials.access_token.as_deref() == Some("${MATRIX_ACCESS_TOKEN}") {
            self.matrix.credentials.access_token =
                Some(env::var("MATRIX_ACCESS_TOKEN").map_err(|_| {
                    ChimneyError::Config(
                        "config references ${MATRIX_ACCESS_TOKEN} but the env var is not set"
                            .to_string(),
                    )
                })?);
        }

        if self.matrix.credentials.device_id.as_deref() == Some("${MATRIX_DEVICE_ID}") {
            self.matrix.credentials.device_id =
                Some(env::var("MATRIX_DEVICE_ID").map_err(|_| {
                    ChimneyError::Config(
                        "config references ${MATRIX_DEVICE_ID} but the env var is not set"
                            .to_string(),
                    )
                })?);
        }

        Ok(())
    }
}

fn is_blank(value: &str) -> bool {
    value.trim().is_empty()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validate_rejects_invalid_template() {
        let config = Config {
            smtp: SmtpConfig {
                bind: "127.0.0.1:2525".to_string(),
                max_message_size: 1024,
                timeout: 30,
                max_connections: 100,
                max_session_seconds: 300,
            },
            matrix: MatrixConfig {
                homeserver: "https://example.org".to_string(),
                user_id: "@bot:example.org".to_string(),
                device_name: "chimney-post".to_string(),
                room_id: "!room:example.org".to_string(),
                store_path: "/tmp/matrix".to_string(),
                require_e2ee: true,
                message_template: "{{ unclosed".to_string(),
                credentials: MatrixCredentials {
                    password: Some("test".to_string()),
                    access_token: None,
                    device_id: None,
                },
            },
            logging: LoggingConfig {
                level: "info".to_string(),
                format: "json".to_string(),
            },
            queue: QueueConfig {
                max_retries: 5,
                retry_backoff: 60,
                db_path: "/tmp/queue.db".to_string(),
                max_len: 0,
            },
        };

        let result = config.validate();
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("template"));
    }
}
