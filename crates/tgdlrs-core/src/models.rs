//! Domain models shared by the database, CLI, worker and MCP server.

use std::fmt;
use std::path::PathBuf;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};

/// Lifecycle of a download job.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum DownloadState {
    Queued,
    Resolving,
    Downloading,
    Paused,
    RetryWait,
    Completed,
    Verifying,
    Verified,
    Failed,
    Canceled,
}

impl DownloadState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Queued => "QUEUED",
            Self::Resolving => "RESOLVING",
            Self::Downloading => "DOWNLOADING",
            Self::Paused => "PAUSED",
            Self::RetryWait => "RETRY_WAIT",
            Self::Completed => "COMPLETED",
            Self::Verifying => "VERIFYING",
            Self::Verified => "VERIFIED",
            Self::Failed => "FAILED",
            Self::Canceled => "CANCELED",
        }
    }

    /// Terminal states never transition again without an explicit retry.
    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Verified | Self::Failed | Self::Canceled)
    }

    /// States that occupy a download slot.
    pub fn is_active(self) -> bool {
        matches!(self, Self::Resolving | Self::Downloading | Self::Verifying)
    }

    /// States the worker may pick up.
    pub fn is_runnable(self) -> bool {
        matches!(self, Self::Queued | Self::RetryWait)
    }
}

impl fmt::Display for DownloadState {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl FromStr for DownloadState {
    type Err = Error;

    fn from_str(value: &str) -> Result<Self> {
        match value.trim().to_ascii_uppercase().as_str() {
            "QUEUED" => Ok(Self::Queued),
            "RESOLVING" => Ok(Self::Resolving),
            "DOWNLOADING" => Ok(Self::Downloading),
            "PAUSED" => Ok(Self::Paused),
            "RETRY_WAIT" => Ok(Self::RetryWait),
            "COMPLETED" => Ok(Self::Completed),
            "VERIFYING" => Ok(Self::Verifying),
            "VERIFIED" => Ok(Self::Verified),
            "FAILED" => Ok(Self::Failed),
            "CANCELED" | "CANCELLED" => Ok(Self::Canceled),
            other => Err(Error::InvalidInput(format!("unknown state: {other}"))),
        }
    }
}

/// A queued or completed download.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Download {
    pub id: String,
    pub source_url: Option<String>,
    pub chat_id: Option<i64>,
    pub message_id: Option<i32>,
    pub document_id: Option<i64>,
    pub file_name: String,
    pub file_size: Option<u64>,
    pub mime_type: Option<String>,
    pub download_dir: PathBuf,
    pub part_path: PathBuf,
    pub final_path: PathBuf,
    pub state: DownloadState,
    pub bytes_downloaded: u64,
    pub bytes_total: Option<u64>,
    pub attempt: u32,
    pub max_attempts: u32,
    pub error: Option<String>,
    pub sha256_expected: Option<String>,
    pub sha256_actual: Option<String>,
    pub redownloaded_bytes: u64,
    pub created_at: String,
    pub updated_at: String,
    pub started_at: Option<String>,
    pub completed_at: Option<String>,
    pub next_retry_at: Option<String>,
    pub priority: i64,
    pub source: String,
    pub verify: bool,
}

impl Download {
    /// Fraction completed in the range `0.0..=1.0` when the total size is known.
    pub fn progress_fraction(&self) -> Option<f64> {
        let total = self.bytes_total.or(self.file_size)?;
        if total == 0 {
            return Some(1.0);
        }
        Some((self.bytes_downloaded as f64 / total as f64).clamp(0.0, 1.0))
    }

    pub fn is_verified(&self) -> bool {
        self.state == DownloadState::Verified
    }
}

/// Fields required to create a new download row.
#[derive(Clone, Debug)]
pub struct NewDownload {
    pub source_url: Option<String>,
    pub chat_id: Option<i64>,
    pub message_id: Option<i32>,
    pub document_id: Option<i64>,
    pub file_name: String,
    pub file_size: Option<u64>,
    pub mime_type: Option<String>,
    pub download_dir: PathBuf,
    pub sha256_expected: Option<String>,
    pub max_attempts: u32,
    pub priority: i64,
    pub source: String,
    pub verify: bool,
}

impl NewDownload {
    pub fn new(file_name: impl Into<String>, download_dir: PathBuf, max_attempts: u32) -> Self {
        Self {
            source_url: None,
            chat_id: None,
            message_id: None,
            document_id: None,
            file_name: file_name.into(),
            file_size: None,
            mime_type: None,
            download_dir,
            sha256_expected: None,
            max_attempts,
            priority: 0,
            source: "cli".into(),
            verify: true,
        }
    }
}

/// A file discovered while scanning a chat or running a search.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct FileEntry {
    pub message_id: i32,
    pub file_name: String,
    pub file_size: u64,
    pub mime_type: Option<String>,
    pub document_id: Option<i64>,
    pub date: String,
    pub already_downloaded: bool,
}

/// Result of resolving a Telegram link.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ResolvedLink {
    pub url: String,
    pub chat_id: i64,
    pub message_id: i32,
    pub title: Option<String>,
}

/// Result of a SHA-256/size verification.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct VerifyReport {
    pub path: PathBuf,
    pub exists: bool,
    pub expected_size: Option<u64>,
    pub actual_size: u64,
    pub sha256_expected: Option<String>,
    pub sha256_actual: Option<String>,
    pub size_ok: bool,
    pub hash_ok: bool,
    pub ok: bool,
}

/// Aggregate queue counters.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct StatusSummary {
    pub total: u64,
    pub queued: u64,
    pub resolving: u64,
    pub downloading: u64,
    pub paused: u64,
    pub retry_wait: u64,
    pub completed: u64,
    pub verifying: u64,
    pub verified: u64,
    pub failed: u64,
    pub canceled: u64,
    pub bytes_downloaded: u64,
    pub bytes_total: u64,
}

/// State reported by the background worker.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct WorkerStatus {
    pub running: bool,
    pub pid: Option<u32>,
    pub version: String,
    pub authorized: bool,
    pub session_present: bool,
    pub user: Option<String>,
    pub active_download: Option<String>,
    pub download_dir: PathBuf,
    pub disk_free_bytes: u64,
    pub disk_reserve_bytes: u64,
    pub summary: StatusSummary,
    pub last_error: Option<String>,
}

impl WorkerStatus {
    pub fn stopped() -> Self {
        Self {
            running: false,
            pid: None,
            version: env!("CARGO_PKG_VERSION").to_string(),
            authorized: false,
            session_present: false,
            user: None,
            active_download: None,
            download_dir: PathBuf::new(),
            disk_free_bytes: 0,
            disk_reserve_bytes: 0,
            summary: StatusSummary::default(),
            last_error: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn states_round_trip() {
        for state in [
            DownloadState::Queued,
            DownloadState::Resolving,
            DownloadState::Downloading,
            DownloadState::Paused,
            DownloadState::RetryWait,
            DownloadState::Completed,
            DownloadState::Verifying,
            DownloadState::Verified,
            DownloadState::Failed,
            DownloadState::Canceled,
        ] {
            assert_eq!(DownloadState::from_str(state.as_str()).unwrap(), state);
        }
        assert_eq!(
            DownloadState::from_str("cancelled").unwrap(),
            DownloadState::Canceled
        );
        assert!(DownloadState::Failed.is_terminal());
        assert!(!DownloadState::RetryWait.is_terminal());
    }
}
