//! Persistent configuration and filesystem layout.
//!
//! The configuration lives next to the session file under `%APPDATA%\tgdlrs`
//! unless `TGDLRS_HOME` is set. Credentials may be provided with the
//! `TGDLRS_API_ID` / `TGDLRS_API_HASH` environment variables instead of being
//! written to disk.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};

/// Default download directory required by the project specification.
pub const DEFAULT_DOWNLOAD_DIR: &str = r"D:\Downloads\Brave";
/// Default disk reserve (10 GiB).
pub const DEFAULT_DISK_RESERVE: u64 = 10 * 1024 * 1024 * 1024;
/// Telegram serves file chunks aligned to this size.
pub const TELEGRAM_CHUNK_SIZE: u32 = 512 * 1024;
/// Default number of parallel download streams per file.
pub const DEFAULT_PARALLEL_STREAMS: u32 = 4;
fn default_parallel_streams() -> u32 {
    DEFAULT_PARALLEL_STREAMS
}

/// Filesystem locations used by every binary.
#[derive(Clone, Debug)]
pub struct Paths {
    pub home: PathBuf,
    pub config_file: PathBuf,
    pub database: PathBuf,
    pub session: PathBuf,
    pub log_dir: PathBuf,
    pub lock_file: PathBuf,
    pub pipe_name: String,
}

impl Paths {
    /// Resolve paths from `TGDLRS_HOME` or the platform data directory.
    pub fn discover() -> Result<Self> {
        let home = match std::env::var_os("TGDLRS_HOME") {
            Some(value) if !value.is_empty() => PathBuf::from(value),
            _ => dirs::data_dir()
                .ok_or_else(|| Error::Config("cannot determine %APPDATA%".into()))?
                .join("tgdlrs"),
        };

        let user = std::env::var("USERNAME")
            .or_else(|_| std::env::var("USER"))
            .unwrap_or_else(|_| "user".to_string());
        let pipe_user: String = user
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
            .collect();

        Ok(Self {
            config_file: home.join("config.json"),
            database: home.join("state.db"),
            session: home.join("telegram.session.json"),
            log_dir: home.join("logs"),
            lock_file: home.join("worker.lock"),
            pipe_name: format!(r"\\.\pipe\tgdlrs-{pipe_user}"),
            home,
        })
    }

    /// Create every directory that must exist before the database is opened.
    pub fn ensure(&self) -> Result<()> {
        std::fs::create_dir_all(&self.home)?;
        std::fs::create_dir_all(&self.log_dir)?;
        Ok(())
    }
}

/// User-tunable configuration. All fields have safe defaults.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub download_dir: PathBuf,
    pub max_concurrent_files: usize,
    /// How many parallel chunk streams to use per file (Telegram serves each
    /// independently). 1 = strictly serial.
    #[serde(default = "default_parallel_streams")]
    pub max_parallel_streams: u32,
    pub max_attempts: u32,
    pub verify_sha256: bool,
    pub skip_verified: bool,
    pub keep_partials: bool,
    pub disk_reserve_bytes: u64,
    pub auto_resume: bool,
    pub auto_start_worker: bool,
    pub api_id: Option<i32>,
    pub api_hash: Option<String>,
    pub phone: Option<String>,
    pub log_level: String,
    /// Base delay for exponential backoff, in milliseconds.
    pub retry_base_ms: u64,
    /// Maximum delay for exponential backoff, in milliseconds.
    pub retry_max_ms: u64,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            download_dir: PathBuf::from(DEFAULT_DOWNLOAD_DIR),
            max_concurrent_files: 1,
            max_parallel_streams: DEFAULT_PARALLEL_STREAMS,
            max_attempts: 10,
            verify_sha256: true,
            skip_verified: true,
            keep_partials: true,
            disk_reserve_bytes: DEFAULT_DISK_RESERVE,
            auto_resume: true,
            auto_start_worker: true,
            api_id: None,
            api_hash: None,
            phone: None,
            log_level: "info".to_string(),
            retry_base_ms: 1_000,
            retry_max_ms: 60_000,
        }
    }
}

/// API credentials resolved from config and environment.
#[derive(Clone, Debug)]
pub struct Credentials {
    pub api_id: i32,
    pub api_hash: String,
}

impl Config {
    /// Load `config.json`, creating a default file when it does not exist.
    pub fn load_or_create(paths: &Paths) -> Result<Self> {
        paths.ensure()?;
        if !paths.config_file.exists() {
            let config = Config::default();
            config.save(paths)?;
            return Ok(config);
        }

        let raw = std::fs::read_to_string(&paths.config_file)?;
        let config: Config = serde_json::from_str(&raw).map_err(|error| {
            Error::Config(format!(
                "cannot parse {}: {error}",
                paths.config_file.display()
            ))
        })?;
        Ok(config)
    }

    /// Persist the configuration atomically.
    pub fn save(&self, paths: &Paths) -> Result<()> {
        paths.ensure()?;
        let raw = serde_json::to_string_pretty(self)?;
        crate::storage::atomic_write(&paths.config_file, raw.as_bytes())
    }

    /// Resolve credentials, preferring environment variables over the file.
    pub fn credentials(&self) -> Result<Credentials> {
        let api_id = std::env::var("TGDLRS_API_ID")
            .ok()
            .and_then(|value| value.trim().parse::<i32>().ok())
            .or(self.api_id)
            .ok_or_else(|| {
                Error::Config(
                    "Telegram api_id is missing; set TGDLRS_API_ID or add \"api_id\" to config.json"
                        .into(),
                )
            })?;
        let api_hash = std::env::var("TGDLRS_API_HASH")
            .ok()
            .filter(|value| !value.trim().is_empty())
            .or_else(|| self.api_hash.clone())
            .ok_or_else(|| {
                Error::Config(
                    "Telegram api_hash is missing; set TGDLRS_API_HASH or add \"api_hash\" to config.json"
                        .into(),
                )
            })?;

        if api_id <= 0 || api_hash.trim().is_empty() {
            return Err(Error::Config("Telegram credentials are invalid".into()));
        }

        Ok(Credentials {
            api_id,
            api_hash: api_hash.trim().to_string(),
        })
    }

    /// Ensure the configured download directory exists and is writable.
    pub fn ensure_download_dir(&self) -> Result<()> {
        std::fs::create_dir_all(&self.download_dir)?;
        let probe = self.download_dir.join(".tgdlrs-write-probe");
        std::fs::write(&probe, b"ok")?;
        std::fs::remove_file(&probe)?;
        Ok(())
    }

    /// Resolve a user-provided destination directory against the configured root.
    pub fn resolve_download_dir(&self, requested: Option<&Path>) -> Result<PathBuf> {
        let dir = match requested {
            Some(path) if path.is_absolute() => path.to_path_buf(),
            Some(path) => self.download_dir.join(path),
            None => self.download_dir.clone(),
        };
        std::fs::create_dir_all(&dir)?;
        Ok(dir)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_match_specification() {
        let config = Config::default();
        assert_eq!(config.max_concurrent_files, 1);
        assert_eq!(config.max_parallel_streams, DEFAULT_PARALLEL_STREAMS);
        assert_eq!(config.max_attempts, 10);
        assert!(config.verify_sha256);
        assert!(config.skip_verified);
        assert!(config.keep_partials);
        assert_eq!(config.disk_reserve_bytes, DEFAULT_DISK_RESERVE);
        assert_eq!(config.download_dir, PathBuf::from(DEFAULT_DOWNLOAD_DIR));
    }

    #[test]
    fn round_trips_config() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths {
            home: dir.path().to_path_buf(),
            config_file: dir.path().join("config.json"),
            database: dir.path().join("state.db"),
            session: dir.path().join("session.json"),
            log_dir: dir.path().join("logs"),
            lock_file: dir.path().join("worker.lock"),
            pipe_name: r"\\.\pipe\tgdlrs-test".into(),
        };
        let config = Config {
            max_attempts: 3,
            ..Config::default()
        };
        config.save(&paths).unwrap();
        let loaded = Config::load_or_create(&paths).unwrap();
        assert_eq!(loaded.max_attempts, 3);
    }
}
