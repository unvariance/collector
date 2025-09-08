use thiserror::Error;

#[derive(Debug, Error)]
pub enum NriError {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("Command failed: {0}")]
    CommandFailed(String),
    #[error("Parse error: {0}")]
    Parse(String),
    #[error("Unsupported version: {0}")]
    VersionUnsupported(String),
    #[error("Config not found: {0}")]
    ConfigNotFound(String),
    #[error("Insufficient privileges: {0}")]
    Privilege(String),
    #[error("Restart unavailable: {0}")]
    RestartUnavailable(String),
    #[error("Timed out: {0}")]
    Timeout(String),
    #[error("Verification failed: {0}")]
    VerificationFailed(String),
    #[error("Detection failed: {0}")]
    DetectionFailed(String),
    #[error("TOML mutation failed: {0}")]
    TomlMutation(String),
}

pub type Result<T> = std::result::Result<T, NriError>;
