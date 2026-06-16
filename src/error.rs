use thiserror::Error;

pub type Result<T> = std::result::Result<T, ChimneyError>;

#[derive(Debug, Error)]
pub enum ChimneyError {
    #[error("SMTP error: {0}")]
    Smtp(String),

    #[error("SMTP message size exceeded")]
    SmtpSizeExceeded,

    #[error("SMTP line exceeded the maximum length")]
    SmtpLineTooLong,

    #[error("Matrix error: {0}")]
    Matrix(String),

    #[error("Configuration error: {0}")]
    Config(String),

    #[error("Template error: {0}")]
    Template(String),

    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    #[error("TOML error: {0}")]
    Toml(#[from] toml::de::Error),

    #[error("Queue error: {0}")]
    Queue(String),

    #[error("Storage error: {0}")]
    Storage(#[from] rusqlite::Error),
}
