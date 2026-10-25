use crate::error::{ChimneyError, Result};
use crate::matrix::Router;
use matrix_sdk::ruma::OwnedUserId;
use serde::Deserialize;
use std::env;
use std::fs;
use std::net::{IpAddr, SocketAddr};
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
    /// Answering forwarded emails from Matrix. Absent (the default) means off.
    #[serde(default)]
    pub reply: ReplyConfig,
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
    /// Optional room-routing rules, evaluated top to bottom. The first rule that
    /// matches an email's sender (`MAIL FROM`) and/or recipient (`RCPT TO`)
    /// decides the destination room; if none match, `room_id` is used as the
    /// catch-all. An empty list (the default) sends every email to `room_id`.
    #[serde(default)]
    pub routes: Vec<RouteConfig>,
    pub credentials: MatrixCredentials,
}

/// A single room-routing rule. At least one of `to`/`from` must be set; when
/// both are set, both must match (logical AND). Matching is case-insensitive.
#[derive(Clone, Debug, Deserialize)]
pub struct RouteConfig {
    /// Match when any recipient address (`RCPT TO`) equals this value.
    #[serde(default)]
    pub to: Option<String>,
    /// Match when the sender address (`MAIL FROM`) equals this value.
    #[serde(default)]
    pub from: Option<String>,
    /// Destination room for emails matched by this rule.
    pub room_id: String,
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

/// Answering forwarded emails from Matrix (`[reply]`): a Matrix reply to a
/// forwarded email, in the strict format below and from an allowed sender, is
/// sent back to that email's reply address through `[reply.smtp]`.
#[derive(Clone, Debug, Default, Deserialize)]
pub struct ReplyConfig {
    /// Off by default; nothing is synced from Matrix or sent by email unless set.
    #[serde(default)]
    pub enabled: bool,
    /// Matrix user ids whose replies are acted on. Everyone else is ignored.
    #[serde(default)]
    pub allowed_senders: Vec<String>,
    /// A Matrix reply is only treated as an email answer when its text starts
    /// with this word, so ordinary chat in the room never sends mail.
    #[serde(default = "default_reply_prefix")]
    pub prefix: String,
    /// Refuse replies sent from a device its owner has not cross-signed, so a
    /// stolen access token on a fresh device cannot send mail.
    #[serde(default = "default_true")]
    pub require_cross_signed_device: bool,
    /// How long after delivery a forwarded email can still be answered.
    #[serde(default = "default_reply_max_age_hours")]
    pub max_age_hours: u64,
    /// Longest answer accepted, in characters.
    #[serde(default = "default_reply_max_body_len")]
    pub max_body_len: usize,
    /// Most answers accepted in any rolling hour, across all rooms.
    #[serde(default = "default_reply_max_per_hour")]
    pub max_per_hour: u32,
    /// `From` address of outgoing answers.
    #[serde(default)]
    pub from: String,
    #[serde(default)]
    pub smtp: ReplySmtpConfig,
}

/// The SMTP server answers are submitted to.
#[derive(Clone, Default, Deserialize)]
pub struct ReplySmtpConfig {
    #[serde(default)]
    pub host: String,
    /// Defaults to the standard port for `security`: 465, 587 or 25.
    #[serde(default)]
    pub port: Option<u16>,
    #[serde(default)]
    pub security: SmtpSecurity,
    #[serde(default)]
    pub username: Option<String>,
    #[serde(default)]
    pub password: Option<String>,
}

impl ReplySmtpConfig {
    /// The configured port, or the standard one for the security mode.
    pub fn port(&self) -> u16 {
        self.port.unwrap_or(match self.security {
            SmtpSecurity::Tls => 465,
            SmtpSecurity::Starttls => 587,
            SmtpSecurity::None => 25,
        })
    }
}

impl std::fmt::Debug for ReplySmtpConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ReplySmtpConfig")
            .field("host", &self.host)
            .field("port", &self.port)
            .field("security", &self.security)
            .field("username", &self.username)
            .field("password", &self.password.as_ref().map(|_| "[REDACTED]"))
            .finish()
    }
}

/// Transport security for `[reply.smtp]`.
#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum SmtpSecurity {
    /// Implicit TLS from the first byte (port 465).
    Tls,
    /// Plain connection upgraded with STARTTLS, which is required (port 587).
    #[default]
    Starttls,
    /// No encryption. Only accepted for a loopback host, i.e. a local MTA.
    None,
}

fn default_true() -> bool {
    true
}

fn default_reply_prefix() -> String {
    "!reply".to_string()
}

fn default_reply_max_age_hours() -> u64 {
    168
}

fn default_reply_max_body_len() -> usize {
    4000
}

fn default_reply_max_per_hour() -> u32 {
    20
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

        if self.queue.retry_backoff == 0 {
            return Err(ChimneyError::Config(
                "queue.retry_backoff must be greater than zero (a zero backoff would \
                 retry a failing message in a tight loop)"
                    .to_string(),
            ));
        }

        if is_blank(&self.matrix.homeserver) {
            return Err(ChimneyError::Config(
                "matrix.homeserver must not be empty".to_string(),
            ));
        }
        // Parsed here too (not just at connect time in MatrixClient::connect),
        // so a malformed homeserver URL fails `Config::load` synchronously at
        // startup instead of passing readiness and looping forever in the
        // background delivery worker's connect retries.
        self.matrix
            .homeserver
            .parse::<url::Url>()
            .map_err(|error| {
                ChimneyError::Config(format!("invalid matrix.homeserver URL: {error}"))
            })?;

        if is_blank(&self.matrix.user_id) {
            return Err(ChimneyError::Config(
                "matrix.user_id must not be empty".to_string(),
            ));
        }
        // Parsed here too (not just at connect time in MatrixClient::connect),
        // so a malformed user_id fails `Config::load` synchronously at startup
        // instead of passing readiness and looping forever in the background
        // delivery worker's connect retries.
        self.matrix
            .user_id
            .parse::<OwnedUserId>()
            .map_err(|error| ChimneyError::Config(format!("invalid matrix.user_id: {error}")))?;

        if is_blank(&self.matrix.store_path) {
            return Err(ChimneyError::Config(
                "matrix.store_path must not be empty".to_string(),
            ));
        }

        // Router::build() parses the default room id and every route's room id,
        // and enforces that each route sets at least one selector -- the same
        // structural checks MatrixClient::connect() relies on at connect time,
        // which runs in the background delivery worker after the SMTP listener
        // is already accepting mail. Running the same builder here makes a
        // malformed room id or selector-less route fail `Config::load`
        // synchronously at startup instead of surfacing later as an endless
        // connect-retry loop.
        Router::from_config(&self.matrix).map(|_| ())?;

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
                .is_none_or(|value| value.trim().is_empty())
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
                reply_to => "test@example.com",
            };
            tmpl.render(test_ctx).map_err(|error| {
                ChimneyError::Config(format!(
                    "matrix.message_template trial render failed: {error}"
                ))
            })?;
        }

        if self.reply.enabled {
            self.validate_reply()?;
        }

        Ok(())
    }

    fn validate_reply(&self) -> Result<()> {
        let reply = &self.reply;

        if reply.allowed_senders.is_empty() {
            return Err(ChimneyError::Config(
                "reply.allowed_senders must list at least one Matrix user when reply.enabled \
                 is set"
                    .to_string(),
            ));
        }
        for (index, sender) in reply.allowed_senders.iter().enumerate() {
            let user_id: OwnedUserId = sender.parse().map_err(|error| {
                ChimneyError::Config(format!("invalid reply.allowed_senders[{index}]: {error}"))
            })?;
            if user_id.as_str() == self.matrix.user_id {
                return Err(ChimneyError::Config(format!(
                    "reply.allowed_senders[{index}] is the bot's own matrix.user_id"
                )));
            }
        }

        if reply.prefix.is_empty()
            || reply.prefix.chars().count() > 32
            || reply
                .prefix
                .chars()
                .any(|c| c.is_whitespace() || c.is_control())
        {
            return Err(ChimneyError::Config(
                "reply.prefix must be a single word of at most 32 characters".to_string(),
            ));
        }

        for (name, value) in [
            ("max_age_hours", reply.max_age_hours as u128),
            ("max_body_len", reply.max_body_len as u128),
            ("max_per_hour", reply.max_per_hour as u128),
        ] {
            if value == 0 {
                return Err(ChimneyError::Config(format!(
                    "reply.{name} must be greater than zero"
                )));
            }
        }

        reply
            .from
            .parse::<lettre::Address>()
            .map_err(|error| ChimneyError::Config(format!("invalid reply.from: {error}")))?;

        let smtp = &reply.smtp;
        if is_blank(&smtp.host) {
            return Err(ChimneyError::Config(
                "reply.smtp.host must not be empty".to_string(),
            ));
        }
        if smtp.security == SmtpSecurity::None && !is_loopback_host(&smtp.host) {
            return Err(ChimneyError::Config(
                "reply.smtp.security = \"none\" is only allowed for a loopback host \
                 (a local MTA); use \"starttls\" or \"tls\" for a remote server"
                    .to_string(),
            ));
        }
        if smtp.username.is_some() != smtp.password.is_some() {
            return Err(ChimneyError::Config(
                "reply.smtp.username and reply.smtp.password must be set together".to_string(),
            ));
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

        if self.reply.smtp.username.as_deref() == Some("${REPLY_SMTP_USERNAME}") {
            self.reply.smtp.username = Some(env::var("REPLY_SMTP_USERNAME").map_err(|_| {
                ChimneyError::Config(
                    "config references ${REPLY_SMTP_USERNAME} but the env var is not set"
                        .to_string(),
                )
            })?);
        }

        if self.reply.smtp.password.as_deref() == Some("${REPLY_SMTP_PASSWORD}") {
            self.reply.smtp.password = Some(env::var("REPLY_SMTP_PASSWORD").map_err(|_| {
                ChimneyError::Config(
                    "config references ${REPLY_SMTP_PASSWORD} but the env var is not set"
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

/// True for `localhost` and loopback IP literals -- the only hosts a reply may
/// be submitted to without TLS. Names other than `localhost` are not resolved:
/// what a name points at can change after startup.
fn is_loopback_host(host: &str) -> bool {
    host.eq_ignore_ascii_case("localhost")
        || host
            .trim_start_matches('[')
            .trim_end_matches(']')
            .parse::<IpAddr>()
            .is_ok_and(|ip| ip.is_loopback())
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
                routes: Vec::new(),
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
            reply: ReplyConfig::default(),
        };

        let result = config.validate();
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("template"));
    }

    fn valid_config() -> Config {
        let text = include_str!("../config.example.toml");
        toml::from_str(text).expect("config.example.toml must parse")
    }

    #[test]
    fn validate_rejects_zero_retry_backoff() {
        let mut config = valid_config();
        config.queue.retry_backoff = 0;
        let err = config.validate().unwrap_err();
        assert!(err.to_string().contains("retry_backoff"));
    }

    #[test]
    fn validate_rejects_route_without_to_or_from() {
        // Selectors are also enforced in Router::build(), but that runs in the
        // background worker; validate() must reject it at load so startup fails
        // fast instead of looping on connect.
        let mut config = valid_config();
        config.matrix.routes = vec![RouteConfig {
            to: None,
            from: None,
            room_id: "!room:example.org".to_string(),
        }];
        let err = config.validate().unwrap_err();
        assert!(err.to_string().contains("at least one of"));
    }

    #[test]
    fn validate_treats_blank_to_or_from_as_unset() {
        // Whitespace-only selectors must not satisfy the "at least one" rule.
        let mut config = valid_config();
        config.matrix.routes = vec![RouteConfig {
            to: Some("   ".to_string()),
            from: Some(String::new()),
            room_id: "!room:example.org".to_string(),
        }];
        let err = config.validate().unwrap_err();
        assert!(err.to_string().contains("at least one of"));
    }

    #[test]
    fn validate_rejects_route_with_blank_room_id() {
        let mut config = valid_config();
        config.matrix.routes = vec![RouteConfig {
            to: Some("alerts@chimney".to_string()),
            from: None,
            room_id: "  ".to_string(),
        }];
        let err = config.validate().unwrap_err();
        assert!(err.to_string().contains("room_id"));
    }

    #[test]
    fn validate_accepts_well_formed_routes() {
        let mut config = valid_config();
        config.matrix.routes = vec![
            RouteConfig {
                to: Some("alerts@chimney".to_string()),
                from: None,
                room_id: "!alerts:example.org".to_string(),
            },
            RouteConfig {
                to: None,
                from: Some("nextcloud@server".to_string()),
                room_id: "!nextcloud:example.org".to_string(),
            },
        ];
        assert!(config.validate().is_ok());
    }

    #[test]
    fn validate_rejects_invalid_default_room_id() {
        let mut config = valid_config();
        config.matrix.room_id = "not-a-room-id".to_string();
        let err = config.validate().unwrap_err();
        assert!(err.to_string().contains("matrix.room_id"));
    }

    #[test]
    fn validate_rejects_invalid_route_room_id() {
        let mut config = valid_config();
        config.matrix.routes = vec![RouteConfig {
            to: Some("alerts@chimney".to_string()),
            from: None,
            room_id: "not-a-room-id".to_string(),
        }];
        let err = config.validate().unwrap_err();
        assert!(err.to_string().contains("matrix.routes[0].room_id"));
    }

    #[test]
    fn validate_rejects_invalid_homeserver_url() {
        // Mirrors validate_rejects_invalid_user_id: a malformed homeserver URL
        // must fail Config::load synchronously, not surface later as a
        // connect-retry loop in the background delivery worker (see
        // MatrixClient::connect).
        let mut config = valid_config();
        config.matrix.homeserver = "not a url".to_string();
        let err = config.validate().unwrap_err();
        assert!(err.to_string().contains("matrix.homeserver"));
    }

    #[test]
    fn validate_rejects_invalid_user_id() {
        // Mirrors the room_id checks above: a malformed user_id must fail
        // Config::load synchronously, not surface later as a connect-retry
        // loop in the background delivery worker (see MatrixClient::connect).
        let mut config = valid_config();
        config.matrix.user_id = "not-a-user-id".to_string();
        let err = config.validate().unwrap_err();
        assert!(err.to_string().contains("matrix.user_id"));
    }

    #[test]
    fn example_config_parses_and_validates() {
        // The shipped example must always deserialize and pass validation
        // (env-var placeholders are left as-is; only structure/template are checked).
        let text = include_str!("../config.example.toml");
        let config: Config = toml::from_str(text).expect("config.example.toml must parse");
        config
            .validate()
            .expect("config.example.toml must pass validation");
    }

    #[test]
    fn example_config_documents_all_tunables() {
        // Guard against config keys drifting out of the documented example.
        let text = include_str!("../config.example.toml");
        for key in [
            "max_message_size",
            "timeout",
            "max_connections",
            "max_session_seconds",
            "max_retries",
            "retry_backoff",
            "db_path",
            "max_len",
        ] {
            assert!(text.contains(key), "config.example.toml is missing `{key}`");
        }
    }

    fn valid_reply_config() -> Config {
        let mut config = valid_config();
        config.reply = ReplyConfig {
            enabled: true,
            allowed_senders: vec!["@me:example.org".to_string()],
            prefix: default_reply_prefix(),
            require_cross_signed_device: true,
            max_age_hours: 168,
            max_body_len: 4000,
            max_per_hour: 20,
            from: "chimney-post@example.org".to_string(),
            smtp: ReplySmtpConfig {
                host: "smtp.example.org".to_string(),
                ..Default::default()
            },
        };
        config
    }

    #[test]
    fn reply_is_off_and_unvalidated_by_default() {
        let config = valid_config();
        assert!(!config.reply.enabled);
        assert!(config.validate().is_ok());
    }

    #[test]
    fn validate_accepts_a_complete_reply_section() {
        assert!(valid_reply_config().validate().is_ok());
    }

    /// Breaks one setting of an otherwise valid config.
    type Breaker = fn(&mut Config);

    #[test]
    fn validate_reply_rejects_bad_settings() {
        let cases: Vec<(Breaker, &str)> = vec![
            (|c| c.reply.allowed_senders.clear(), "allowed_senders"),
            (
                |c| c.reply.allowed_senders = vec!["me".to_string()],
                "allowed_senders[0]",
            ),
            (
                |c| c.reply.allowed_senders = vec![c.matrix.user_id.clone()],
                "bot's own",
            ),
            (|c| c.reply.prefix = String::new(), "reply.prefix"),
            (|c| c.reply.prefix = "re ply".to_string(), "reply.prefix"),
            (|c| c.reply.max_age_hours = 0, "max_age_hours"),
            (|c| c.reply.max_body_len = 0, "max_body_len"),
            (|c| c.reply.max_per_hour = 0, "max_per_hour"),
            (|c| c.reply.from = "nope".to_string(), "reply.from"),
            (|c| c.reply.smtp.host = " ".to_string(), "reply.smtp.host"),
            (|c| c.reply.smtp.security = SmtpSecurity::None, "loopback"),
            (
                |c| c.reply.smtp.username = Some("user".to_string()),
                "set together",
            ),
        ];
        for (break_it, expected) in cases {
            let mut config = valid_reply_config();
            break_it(&mut config);
            let err = config.validate().unwrap_err().to_string();
            assert!(err.contains(expected), "expected `{expected}` in `{err}`");
        }
    }

    #[test]
    fn plaintext_reply_smtp_is_allowed_only_on_loopback() {
        for host in ["localhost", "127.0.0.1", "::1", "[::1]"] {
            let mut config = valid_reply_config();
            config.reply.smtp.host = host.to_string();
            config.reply.smtp.security = SmtpSecurity::None;
            assert!(config.validate().is_ok(), "{host}");
        }
        for host in ["10.0.0.1", "mail.localhost.example.com", "localhost.evil"] {
            assert!(!is_loopback_host(host), "{host}");
        }
    }

    #[test]
    fn reply_smtp_port_defaults_follow_security() {
        let mut smtp = ReplySmtpConfig::default();
        assert_eq!(smtp.port(), 587);
        smtp.security = SmtpSecurity::Tls;
        assert_eq!(smtp.port(), 465);
        smtp.security = SmtpSecurity::None;
        assert_eq!(smtp.port(), 25);
        smtp.port = Some(2525);
        assert_eq!(smtp.port(), 2525);
    }

    #[test]
    fn reply_smtp_password_is_redacted_in_debug() {
        let smtp = ReplySmtpConfig {
            password: Some("hunter2".to_string()),
            ..Default::default()
        };
        assert!(!format!("{smtp:?}").contains("hunter2"));
    }
}
