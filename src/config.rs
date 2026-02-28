use crate::error::{ChimneyError, Result};
use serde::Deserialize;
use std::env;
use std::fs;
use std::net::SocketAddr;
use std::path::Path;

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
}

#[derive(Clone, Debug, Deserialize)]
pub struct MatrixConfig {
    pub homeserver: String,
    pub user_id: String,
    pub device_name: String,
    pub room_id: String,
    pub store_path: String,
    pub credentials: MatrixCredentials,
}

#[derive(Clone, Debug, Deserialize)]
pub struct MatrixCredentials {
    pub password: Option<String>,
    pub access_token: Option<String>,
    pub device_id: Option<String>,
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
    pub capacity: usize,
}

impl Config {
    pub fn load_default() -> Result<Self> {
        let path = env::var("CHIMNEY_CONFIG").unwrap_or_else(|_| "config.toml".to_string());
        Self::load_from_path(path)
    }

    pub fn load_from_path<P: AsRef<Path>>(path: P) -> Result<Self> {
        let content = fs::read_to_string(&path)?;
        let mut config: Self = toml::from_str(&content)?;
        config.apply_env_overrides();
        config.validate()?;
        Ok(config)
    }

    pub fn validate(&self) -> Result<()> {
        let addr: SocketAddr = self.smtp.bind.parse().map_err(|_| {
            ChimneyError::Config("smtp.bind must be an IP:PORT on localhost".to_string())
        })?;

        if !addr.ip().is_loopback() {
            return Err(ChimneyError::Config(
                "smtp.bind must be a loopback address".to_string(),
            ));
        }

        if self.smtp.max_message_size == 0 {
            return Err(ChimneyError::Config(
                "smtp.max_message_size must be greater than zero".to_string(),
            ));
        }

        if self.queue.capacity == 0 {
            return Err(ChimneyError::Config(
                "queue.capacity must be greater than zero".to_string(),
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

        Ok(())
    }

    fn apply_env_overrides(&mut self) {
        if self.matrix.credentials.password.as_deref() == Some("${MATRIX_PASSWORD}") {
            self.matrix.credentials.password = env::var("MATRIX_PASSWORD").ok();
        }

        if self.matrix.credentials.access_token.as_deref() == Some("${MATRIX_ACCESS_TOKEN}") {
            self.matrix.credentials.access_token = env::var("MATRIX_ACCESS_TOKEN").ok();
        }

        if self.matrix.credentials.device_id.as_deref() == Some("${MATRIX_DEVICE_ID}") {
            self.matrix.credentials.device_id = env::var("MATRIX_DEVICE_ID").ok();
        }

        if self.matrix.credentials.password.is_none() {
            if let Ok(value) = env::var("MATRIX_PASSWORD") {
                self.matrix.credentials.password = Some(value);
            }
        }

        if self.matrix.credentials.access_token.is_none() {
            if let Ok(value) = env::var("MATRIX_ACCESS_TOKEN") {
                self.matrix.credentials.access_token = Some(value);
            }
        }

        if self.matrix.credentials.device_id.is_none() {
            if let Ok(value) = env::var("MATRIX_DEVICE_ID") {
                self.matrix.credentials.device_id = Some(value);
            }
        }
    }
}

fn is_blank(value: &str) -> bool {
    value.trim().is_empty()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validate_rejects_non_loopback() {
        let config = Config {
            smtp: SmtpConfig {
                bind: "0.0.0.0:2525".to_string(),
                max_message_size: 1024,
                timeout: 30,
            },
            matrix: MatrixConfig {
                homeserver: "https://example.org".to_string(),
                user_id: "@bot:example.org".to_string(),
                device_name: "chimney-post".to_string(),
                room_id: "!room:example.org".to_string(),
                store_path: "/tmp/matrix".to_string(),
                credentials: MatrixCredentials {
                    password: None,
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
                capacity: 10,
            },
        };

        let result = config.validate();
        assert!(result.is_err());
    }
}
