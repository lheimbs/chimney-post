use thiserror::Error;

pub type Result<T> = std::result::Result<T, ChimneyError>;

#[derive(Debug, Error)]
pub enum ChimneyError {
    #[error("SMTP error: {0}")]
    Smtp(String),

    #[error("SMTP message size exceeded")]
    SmtpSizeExceeded,

    #[error("Matrix error: {0}")]
    Matrix(String),

    #[error("Configuration error: {0}")]
    Config(String),

    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    #[error("TOML error: {0}")]
    Toml(#[from] toml::de::Error),
}
