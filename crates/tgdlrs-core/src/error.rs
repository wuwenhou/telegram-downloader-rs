//! Error types shared by every crate in the workspace.

use std::path::PathBuf;

/// Errors returned by the downloader.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("configuration error: {0}")]
    Config(String),

    #[error("database error: {0}")]
    Database(String),

    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error("serialization error: {0}")]
    Serde(#[from] serde_json::Error),

    #[error("telegram error: {0}")]
    Telegram(String),

    #[error("telegram flood wait: retry after {0} seconds")]
    FloodWait(u64),

    #[error("download error: {0}")]
    Download(String),

    #[error("verification failed: {0}")]
    Verification(String),

    #[error("not found: {0}")]
    NotFound(String),

    #[error("invalid input: {0}")]
    InvalidInput(String),

    #[error("conflict: {0}")]
    Conflict(String),

    #[error("telegram session is not authorized; run `tgdlrs login`")]
    NotAuthorized,

    #[error("worker is not running")]
    WorkerUnavailable,

    #[error("IPC error: {0}")]
    Ipc(String),

    #[error("operation paused by user")]
    Paused,

    #[error("operation canceled by user")]
    Canceled,

    #[error("path escapes the configured download directory: {0}")]
    UnsafePath(PathBuf),

    #[error("{0}")]
    Other(String),
}

pub type Result<T> = std::result::Result<T, Error>;

impl Error {
    /// Whether the failure is transient and the download should be retried.
    pub fn is_retryable(&self) -> bool {
        match self {
            Error::Io(_) | Error::FloodWait(_) | Error::Telegram(_) => true,
            Error::Download(message) => {
                let lowered = message.to_ascii_lowercase();
                !lowered.contains("file reference")
                    && !lowered.contains("not found")
                    && !lowered.contains("unsupported")
            }
            _ => false,
        }
    }

    /// Extract a Telegram FLOOD_WAIT duration from an error message, if present.
    pub fn flood_wait_seconds(&self) -> Option<u64> {
        match self {
            Error::FloodWait(seconds) => Some(*seconds),
            other => parse_flood_wait(&other.to_string()),
        }
    }

    /// Stable machine-readable error kind, used by the IPC protocol.
    pub fn kind(&self) -> &'static str {
        match self {
            Error::Config(_) => "config",
            Error::Database(_) => "database",
            Error::Io(_) => "io",
            Error::Serde(_) => "serialization",
            Error::Telegram(_) => "telegram",
            Error::FloodWait(_) => "flood_wait",
            Error::Download(_) => "download",
            Error::Verification(_) => "verification",
            Error::NotFound(_) => "not_found",
            Error::InvalidInput(_) => "invalid_input",
            Error::Conflict(_) => "conflict",
            Error::NotAuthorized => "not_authorized",
            Error::WorkerUnavailable => "worker_unavailable",
            Error::Ipc(_) => "ipc",
            Error::Paused => "paused",
            Error::Canceled => "canceled",
            Error::UnsafePath(_) => "unsafe_path",
            Error::Other(_) => "other",
        }
    }

    /// Rebuild an error received over IPC.
    pub fn from_remote(kind: &str, message: &str) -> Error {
        let text = message
            .strip_prefix(&format!("{}: ", kind.replace('_', " ")))
            .unwrap_or(message)
            .to_string();
        match kind {
            "config" => Error::Config(text),
            "database" => Error::Database(text),
            "telegram" => Error::Telegram(text),
            "flood_wait" => Error::FloodWait(parse_flood_wait(message).unwrap_or(60)),
            "download" => Error::Download(text),
            "verification" => Error::Verification(text),
            "not_found" => Error::NotFound(text),
            "invalid_input" => Error::InvalidInput(text),
            "conflict" => Error::Conflict(text),
            "not_authorized" => Error::NotAuthorized,
            "worker_unavailable" => Error::WorkerUnavailable,
            "ipc" => Error::Ipc(text),
            "paused" => Error::Paused,
            "canceled" => Error::Canceled,
            _ => Error::Other(text),
        }
    }
}

/// Parse `FLOOD_WAIT_123` (also accepts `FLOOD_WAIT (123)` and `FLOOD_WAIT_123s`).
pub fn parse_flood_wait(message: &str) -> Option<u64> {
    let upper = message.to_ascii_uppercase();
    let marker = "FLOOD_WAIT";
    let start = upper.find(marker)? + marker.len();
    let rest = &upper[start..];
    let digits: String = rest
        .chars()
        .skip_while(|c| !c.is_ascii_digit())
        .take_while(|c| c.is_ascii_digit())
        .collect();
    digits.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_flood_wait_variants() {
        assert_eq!(parse_flood_wait("FLOOD_WAIT_42"), Some(42));
        assert_eq!(parse_flood_wait("rpc error FLOOD_WAIT (120)"), Some(120));
        assert_eq!(parse_flood_wait("FLOOD_WAIT_7 seconds"), Some(7));
        assert_eq!(parse_flood_wait("no wait here"), None);
    }

    #[test]
    fn retryability_is_conservative() {
        assert!(Error::FloodWait(3).is_retryable());
        assert!(!Error::NotAuthorized.is_retryable());
        assert!(!Error::Verification("bad hash".into()).is_retryable());
        assert!(Error::Telegram("connection reset".into()).is_retryable());
    }
}
