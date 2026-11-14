//! SQLite persistence layer.

use std::path::{Path, PathBuf};

use chrono::{Duration, SecondsFormat, Utc};

use crate::config::Paths;
use crate::error::{Error, Result};
use crate::models::{Download, DownloadState, FileEntry, NewDownload, StatusSummary, VerifyReport};
use crate::params;
use crate::sqlite::{Database, Row, Value};

/// Current schema version.
const SCHEMA_VERSION: i64 = 2;

/// RFC 3339 timestamp with second precision.
pub fn now() -> String {
    Utc::now().to_rfc3339_opts(SecondsFormat::Secs, true)
}

/// RFC 3339 timestamp `seconds` in the future.
pub fn later(seconds: u64) -> String {
    (Utc::now() + Duration::seconds(seconds as i64)).to_rfc3339_opts(SecondsFormat::Secs, true)
}

fn row_to_download(row: &Row) -> Result<Download> {
    let state_text = row.require_str("state")?;
    let state = state_text.parse::<DownloadState>()?;
    Ok(Download {
        id: row.require_str("id")?.to_string(),
        source_url: row.get_string("source_url"),
        chat_id: row.get_i64("chat_id"),
        message_id: row.get_i64("message_id").map(|value| value as i32),
        document_id: row.get_i64("document_id"),
        file_name: row.require_str("file_name")?.to_string(),
        file_size: row.get_u64("file_size"),
        mime_type: row.get_string("mime_type"),
        download_dir: PathBuf::from(row.require_str("download_dir")?),
        part_path: PathBuf::from(row.require_str("part_path")?),
        final_path: PathBuf::from(row.require_str("final_path")?),
        state,
        bytes_downloaded: row.get_u64("bytes_downloaded").unwrap_or(0),
        bytes_total: row.get_u64("bytes_total"),
        attempt: row.get_i64("attempt").unwrap_or(0).max(0) as u32,
        max_attempts: row.get_i64("max_attempts").unwrap_or(10).max(1) as u32,
        error: row.get_string("error"),
        sha256_expected: row.get_string("sha256_expected"),
        sha256_actual: row.get_string("sha256_actual"),
        redownloaded_bytes: row.get_u64("redownloaded_bytes").unwrap_or(0),
        created_at: row.require_str("created_at")?.to_string(),
        updated_at: row.require_str("updated_at")?.to_string(),
        started_at: row.get_string("started_at"),
        completed_at: row.get_string("completed_at"),
        next_retry_at: row.get_string("next_retry_at"),
        priority: row.get_i64("priority").unwrap_or(0),
        source: row.get_string("source").unwrap_or_else(|| "cli".into()),
        verify: row.get_i64("verify").unwrap_or(1) != 0,
    })
}

/// A download lifecycle event.
#[derive(Clone, Debug)]
pub struct Event {
    pub id: i64,
    pub download_id: Option<String>,
    pub ts: String,
    pub level: String,
    pub event: String,
    pub details: Option<String>,
}

/// A Telegram chat/message source that has been resolved before.
#[derive(Clone, Debug)]
pub struct SourceRecord {
    pub id: i64,
    pub url: String,
    pub chat_id: Option<i64>,
    pub message_id: Option<i32>,
    pub title: Option<String>,
    pub file_name: Option<String>,
    pub file_size: Option<u64>,
    pub last_scanned_at: Option<String>,
    pub created_at: String,
}

/// Persistence facade.
pub struct Store {
    database: Database,
}

impl Store {
    /// Open (and migrate) the state database.
    pub fn open(paths: &Paths) -> Result<Self> {
        paths.ensure()?;
        let database = Database::open(&paths.database)?;
        let store = Self { database };
        store.migrate()?;
        Ok(store)
    }

    /// Open an arbitrary database path (tests, tools).
    pub fn open_path(path: &Path) -> Result<Self> {
        let database = Database::open(path)?;
        let store = Self { database };
        store.migrate()?;
        Ok(store)
    }

    pub fn library_version(&self) -> String {
        self.database.library_version()
    }

    fn migrate(&self) -> Result<()> {
        self.database.with(|connection| {
            connection.execute_batch(
                r#"
                CREATE TABLE IF NOT EXISTS schema_migrations(
                    version     INTEGER PRIMARY KEY,
                    applied_at  TEXT NOT NULL
                );

                CREATE TABLE IF NOT EXISTS downloads(
                    id                 TEXT PRIMARY KEY,
                    source_url         TEXT,
                    chat_id            INTEGER,
                    message_id         INTEGER,
                    document_id        INTEGER,
                    file_name          TEXT NOT NULL,
                    file_size          INTEGER,
                    mime_type          TEXT,
                    download_dir       TEXT NOT NULL,
                    part_path          TEXT NOT NULL UNIQUE,
                    final_path         TEXT NOT NULL,
                    state              TEXT NOT NULL,
                    bytes_downloaded   INTEGER NOT NULL DEFAULT 0,
                    bytes_total        INTEGER,
                    attempt            INTEGER NOT NULL DEFAULT 0,
                    max_attempts       INTEGER NOT NULL DEFAULT 10,
                    error              TEXT,
                    sha256_expected    TEXT,
                    sha256_actual      TEXT,
                    redownloaded_bytes INTEGER NOT NULL DEFAULT 0,
                    priority           INTEGER NOT NULL DEFAULT 0,
                    source             TEXT NOT NULL DEFAULT 'cli',
                    verify             INTEGER NOT NULL DEFAULT 1,
                    created_at         TEXT NOT NULL,
                    updated_at         TEXT NOT NULL,
                    started_at         TEXT,
                    completed_at       TEXT,
                    next_retry_at      TEXT
                );

                CREATE INDEX IF NOT EXISTS idx_downloads_state ON downloads(state);
                CREATE INDEX IF NOT EXISTS idx_downloads_created ON downloads(created_at);
                CREATE INDEX IF NOT EXISTS idx_downloads_source ON downloads(chat_id, message_id);

                CREATE TABLE IF NOT EXISTS download_parts(
                    download_id   TEXT NOT NULL,
                    part_index    INTEGER NOT NULL,
                    start_offset  INTEGER NOT NULL,
                    end_offset    INTEGER,
                    bytes_written INTEGER NOT NULL DEFAULT 0,
                    sha256        TEXT,
                    status        TEXT NOT NULL,
                    updated_at    TEXT NOT NULL,
                    PRIMARY KEY(download_id, part_index)
                );

                CREATE TABLE IF NOT EXISTS download_events(
                    id          INTEGER PRIMARY KEY AUTOINCREMENT,
                    download_id TEXT,
                    ts          TEXT NOT NULL,
                    level       TEXT NOT NULL,
                    event       TEXT NOT NULL,
                    details     TEXT
                );

                CREATE INDEX IF NOT EXISTS idx_events_download ON download_events(download_id, id);

                CREATE TABLE IF NOT EXISTS telegram_sources(
                    id              INTEGER PRIMARY KEY AUTOINCREMENT,
                    url             TEXT NOT NULL UNIQUE,
                    chat_id         INTEGER,
                    message_id      INTEGER,
                    title           TEXT,
                    file_name       TEXT,
                    file_size       INTEGER,
                    last_scanned_at TEXT,
                    created_at      TEXT NOT NULL
                );

                CREATE TABLE IF NOT EXISTS verification_results(
                    id              INTEGER PRIMARY KEY AUTOINCREMENT,
                    download_id     TEXT NOT NULL,
                    ts              TEXT NOT NULL,
                    expected_size   INTEGER,
                    actual_size     INTEGER,
                    sha256_expected TEXT,
                    sha256_actual   TEXT,
                    ok              INTEGER NOT NULL,
                    details         TEXT
                );

                CREATE INDEX IF NOT EXISTS idx_verification_download
                    ON verification_results(download_id, id);

                CREATE TABLE IF NOT EXISTS settings(
                    key        TEXT PRIMARY KEY,
                    value      TEXT NOT NULL,
                    updated_at TEXT NOT NULL
                );
                "#,
            )?;

            // Older databases predate the `verify` column; add it in place.
            let columns = connection.query("PRAGMA table_info(downloads)", &[])?;
            let has_verify = columns
                .iter()
                .any(|row| row.get_string("name").as_deref() == Some("verify"));
            if !has_verify {
                connection.execute(
                    "ALTER TABLE downloads ADD COLUMN verify INTEGER NOT NULL DEFAULT 1",
                    &[],
                )?;
            }

            let applied = connection.query(
                "SELECT version FROM schema_migrations WHERE version = ?1",
                &params![SCHEMA_VERSION],
            )?;
            if applied.is_empty() {
                connection.execute(
                    "INSERT INTO schema_migrations(version, applied_at) VALUES (?1, ?2)",
                    &params![SCHEMA_VERSION, now()],
                )?;
            }
            Ok(())
        })
    }

    /// Insert a download, de-duplicating identical active jobs.
    pub fn insert(&self, new: NewDownload) -> Result<Download> {
        let safe_name = crate::storage::safe_file_name(&new.file_name);
        let mut download_dir = new.download_dir.clone();
        if download_dir.as_os_str().is_empty() {
            download_dir = PathBuf::from(crate::config::DEFAULT_DOWNLOAD_DIR);
        }

        let inserted_id = self.database.with(|connection| {
            if let (Some(chat_id), Some(message_id)) = (new.chat_id, new.message_id) {
                let existing = connection.query(
                    "SELECT id FROM downloads
                     WHERE chat_id = ?1 AND message_id = ?2
                       AND state NOT IN ('FAILED','CANCELED','VERIFIED')
                     LIMIT 1",
                    &params![chat_id, message_id],
                )?;
                if let Some(row) = existing.first() {
                    return Ok(ExistingOrNew::Existing(
                        row.require_str("id")?.to_string(),
                    ));
                }
            }

            // Same URL re-enqueued while a job is still active: reuse it.
            if let Some(url) = &new.source_url {
                let existing = connection.query(
                    "SELECT id FROM downloads
                     WHERE source_url = ?1
                       AND state NOT IN ('FAILED','CANCELED','VERIFIED')
                     LIMIT 1",
                    &params![url],
                )?;
                if let Some(row) = existing.first() {
                    return Ok(ExistingOrNew::Existing(
                        row.require_str("id")?.to_string(),
                    ));
                }
            }

            let id = uuid::Uuid::new_v4().to_string();
            let final_path = download_dir.join(&safe_name);
            let mut part_path = PathBuf::from(format!("{}.part", final_path.display()));
            if !connection
                .query(
                    "SELECT 1 FROM downloads WHERE part_path = ?1 LIMIT 1",
                    &params![part_path.to_string_lossy().to_string()],
                )?
                .is_empty()
            {
                part_path = download_dir.join(format!("{safe_name}.{}.part", &id[..8]));
            }

            let timestamp = now();
            connection.execute(
                r#"INSERT INTO downloads(
                    id, source_url, chat_id, message_id, document_id, file_name, file_size,
                    mime_type, download_dir, part_path, final_path, state, bytes_downloaded,
                    bytes_total, attempt, max_attempts, sha256_expected, priority, source,
                    verify, created_at, updated_at
                ) VALUES (
                    ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, 'QUEUED', 0, ?7, 0, ?12,
                    ?13, ?14, ?15, ?16, ?17, ?17
                )"#,
                &params![
                    id.clone(),
                    new.source_url.clone(),
                    new.chat_id,
                    new.message_id,
                    new.document_id,
                    safe_name.clone(),
                    new.file_size,
                    new.mime_type.clone(),
                    download_dir.to_string_lossy().to_string(),
                    part_path.to_string_lossy().to_string(),
                    final_path.to_string_lossy().to_string(),
                    new.max_attempts.max(1) as i64,
                    new.sha256_expected.clone(),
                    new.priority,
                    new.source.clone(),
                    new.verify,
                    &timestamp,
                ],
            )?;

            connection.execute(
                "INSERT INTO download_parts(download_id, part_index, start_offset, end_offset, bytes_written, status, updated_at)
                 VALUES (?1, 0, 0, ?2, 0, 'PENDING', ?3)",
                &params![id.clone(), new.file_size, &timestamp],
            )?;

            Ok(ExistingOrNew::New(id))
        })?;

        let id = match inserted_id {
            ExistingOrNew::Existing(id) => return self.get(&id),
            ExistingOrNew::New(id) => id,
        };

        self.add_event(
            Some(&id),
            "info",
            "QUEUED",
            Some(&format!("queued {safe_name}")),
        )?;
        self.get(&id)
    }

    /// Update identity and paths after a URL-only job has been resolved.
    #[allow(clippy::too_many_arguments)]
    pub fn apply_resolution(
        &self,
        id: &str,
        chat_id: i64,
        message_id: i32,
        document_id: Option<i64>,
        file_name: &str,
        file_size: Option<u64>,
        mime_type: Option<&str>,
    ) -> Result<Download> {
        let safe_name = crate::storage::safe_file_name(file_name);
        self.database.with(|connection| {
            let rows = connection.query("SELECT * FROM downloads WHERE id = ?1", &params![id])?;
            let current = rows
                .first()
                .ok_or_else(|| Error::NotFound(format!("download {id}")))
                .and_then(row_to_download)?;
            let final_path = current.download_dir.join(&safe_name);
            let mut part_path = PathBuf::from(format!("{}.part", final_path.display()));
            if !connection
                .query(
                    "SELECT 1 FROM downloads WHERE part_path = ?1 AND id <> ?2 LIMIT 1",
                    &params![part_path.to_string_lossy().to_string(), id],
                )?
                .is_empty()
            {
                part_path = current
                    .download_dir
                    .join(format!("{safe_name}.{}.part", &id[..8]));
            }
            connection.execute(
                "UPDATE downloads
                 SET chat_id = ?1, message_id = ?2, document_id = COALESCE(?3, document_id),
                     file_name = ?4, file_size = COALESCE(?5, file_size),
                     bytes_total = COALESCE(?5, bytes_total), mime_type = COALESCE(?6, mime_type),
                     part_path = ?7, final_path = ?8, updated_at = ?9
                 WHERE id = ?10",
                &params![
                    chat_id,
                    message_id,
                    document_id,
                    safe_name,
                    file_size,
                    mime_type,
                    part_path.to_string_lossy().to_string(),
                    final_path.to_string_lossy().to_string(),
                    now(),
                    id,
                ],
            )?;
            Ok(())
        })?;
        self.get(id)
    }

    /// Fetch one download by full id.
    pub fn get(&self, id: &str) -> Result<Download> {
        self.database.with(|connection| {
            let rows = connection.query("SELECT * FROM downloads WHERE id = ?1", &params![id])?;
            rows.first()
                .ok_or_else(|| Error::NotFound(format!("download {id}")))
                .and_then(row_to_download)
        })
    }

    /// Resolve a full id or a unique prefix.
    pub fn find(&self, prefix: &str) -> Result<Download> {
        if let Ok(download) = self.get(prefix) {
            return Ok(download);
        }
        let rows = self.database.with(|connection| {
            connection.query(
                "SELECT * FROM downloads WHERE id LIKE ?1 ORDER BY created_at DESC LIMIT 2",
                &params![format!("{prefix}%")],
            )
        })?;
        match rows.len() {
            0 => Err(Error::NotFound(format!("download {prefix}"))),
            1 => row_to_download(&rows[0]),
            _ => Err(Error::InvalidInput(format!(
                "download id prefix {prefix} is ambiguous"
            ))),
        }
    }

    pub fn list(&self, state: Option<DownloadState>, limit: usize) -> Result<Vec<Download>> {
        let limit = limit.clamp(1, 1000) as i64;
        self.database.with(|connection| {
            let rows = match state {
                Some(state) => connection.query(
                    "SELECT * FROM downloads WHERE state = ?1
                     ORDER BY created_at DESC LIMIT ?2",
                    &params![state.as_str(), limit],
                )?,
                None => connection.query(
                    "SELECT * FROM downloads ORDER BY created_at DESC LIMIT ?1",
                    &params![limit],
                )?,
            };
            rows.iter().map(row_to_download).collect()
        })
    }

    /// Downloads the worker may start now.
    pub fn runnable(&self, limit: usize) -> Result<Vec<Download>> {
        let limit = limit.clamp(1, 100) as i64;
        self.database.with(|connection| {
            let rows = connection.query(
                "SELECT * FROM downloads
                 WHERE state IN ('QUEUED','RETRY_WAIT')
                   AND (next_retry_at IS NULL OR next_retry_at <= ?1)
                 ORDER BY priority DESC, created_at ASC
                 LIMIT ?2",
                &params![now(), limit],
            )?;
            rows.iter().map(row_to_download).collect()
        })
    }

    pub fn has_active(&self) -> Result<bool> {
        self.database.with(|connection| {
            let rows = connection.query(
                "SELECT 1 AS present FROM downloads
                 WHERE state IN ('RESOLVING','DOWNLOADING','VERIFYING') LIMIT 1",
                &[],
            )?;
            Ok(!rows.is_empty())
        })
    }

    /// Transition state, optionally recording an error message.
    pub fn set_state(&self, id: &str, state: DownloadState, error: Option<&str>) -> Result<()> {
        let timestamp = now();
        self.database.with(|connection| {
            let changed = connection.execute(
                "UPDATE downloads SET state = ?1, error = ?2, updated_at = ?3 WHERE id = ?4",
                &params![state.as_str(), error, timestamp, id],
            )?;
            if changed == 0 {
                return Err(Error::NotFound(format!("download {id}")));
            }
            Ok(())
        })?;
        self.add_event(
            Some(id),
            if state == DownloadState::Failed {
                "error"
            } else {
                "info"
            },
            state.as_str(),
            error,
        )
    }

    pub fn set_metadata(
        &self,
        id: &str,
        size: Option<u64>,
        mime: Option<&str>,
        document_id: Option<i64>,
    ) -> Result<()> {
        self.database.with(|connection| {
            connection.execute(
                "UPDATE downloads
                 SET file_size = COALESCE(?1, file_size),
                     bytes_total = COALESCE(?1, bytes_total),
                     mime_type = COALESCE(?2, mime_type),
                     document_id = COALESCE(?3, document_id),
                     updated_at = ?4
                 WHERE id = ?5",
                &params![size, mime, document_id, now(), id],
            )?;
            connection.execute(
                "UPDATE download_parts SET end_offset = COALESCE(?1, end_offset), updated_at = ?2
                 WHERE download_id = ?3 AND part_index = 0",
                &params![size, now(), id],
            )?;
            Ok(())
        })
    }

    pub fn set_progress(&self, id: &str, downloaded: u64, total: Option<u64>) -> Result<()> {
        let timestamp = now();
        self.database.with(|connection| {
            connection.execute(
                "UPDATE downloads
                 SET bytes_downloaded = ?1, bytes_total = COALESCE(?2, bytes_total), updated_at = ?3
                 WHERE id = ?4",
                &params![downloaded, total, &timestamp, id],
            )?;
            connection.execute(
                "UPDATE download_parts SET bytes_written = ?1, status = 'DOWNLOADING', updated_at = ?2
                 WHERE download_id = ?3 AND part_index = 0",
                &params![downloaded, &timestamp, id],
            )?;
            Ok(())
        })
    }

    pub fn set_redownloaded(&self, id: &str, bytes: u64) -> Result<()> {
        self.database.with(|connection| {
            connection.execute(
                "UPDATE downloads SET redownloaded_bytes = ?1, updated_at = ?2 WHERE id = ?3",
                &params![bytes, now(), id],
            )?;
            Ok(())
        })
    }

    pub fn begin_attempt(&self, id: &str) -> Result<()> {
        self.database.with(|connection| {
            connection.execute(
                "UPDATE downloads
                 SET attempt = attempt + 1, state = 'DOWNLOADING',
                     started_at = COALESCE(started_at, ?1), updated_at = ?1,
                     next_retry_at = NULL, error = NULL
                 WHERE id = ?2",
                &params![now(), id],
            )?;
            Ok(())
        })?;
        self.add_event(Some(id), "info", "ATTEMPT", None)
    }

    pub fn schedule_retry(&self, id: &str, error: &str, delay_seconds: u64) -> Result<()> {
        let timestamp = now();
        let retry_at = later(delay_seconds);
        self.database.with(|connection| {
            connection.execute(
                "UPDATE downloads SET state = 'RETRY_WAIT', error = ?1, next_retry_at = ?2,
                 updated_at = ?3 WHERE id = ?4",
                &params![error, retry_at, timestamp, id],
            )?;
            Ok(())
        })?;
        self.add_event(
            Some(id),
            "warn",
            "RETRY_WAIT",
            Some(&format!("retry in {delay_seconds}s: {error}")),
        )
    }

    pub fn mark_verified(&self, id: &str, sha256: &str) -> Result<()> {
        let timestamp = now();
        self.database.with(|connection| {
            connection.execute(
                "UPDATE downloads
                 SET state = 'VERIFIED', sha256_actual = ?1, completed_at = ?2, updated_at = ?2,
                     error = NULL
                 WHERE id = ?3",
                &params![sha256, &timestamp, id],
            )?;
            connection.execute(
                "UPDATE download_parts SET status = 'COMPLETE', updated_at = ?1
                 WHERE download_id = ?2 AND part_index = 0",
                &params![&timestamp, id],
            )?;
            Ok(())
        })?;
        self.add_event(Some(id), "info", "VERIFIED", Some(sha256))
    }

    pub fn mark_completed(&self, id: &str, sha256: Option<&str>) -> Result<()> {
        let timestamp = now();
        self.database.with(|connection| {
            connection.execute(
                "UPDATE downloads
                 SET state = 'COMPLETED', sha256_actual = COALESCE(?1, sha256_actual),
                     completed_at = ?2, updated_at = ?2, error = NULL
                 WHERE id = ?3",
                &params![sha256, &timestamp, id],
            )?;
            Ok(())
        })?;
        self.add_event(Some(id), "info", "COMPLETED", None)
    }

    pub fn record_verification(&self, id: &str, report: &VerifyReport) -> Result<()> {
        self.database.with(|connection| {
            connection.execute(
                r#"INSERT INTO verification_results(
                    download_id, ts, expected_size, actual_size, sha256_expected,
                    sha256_actual, ok, details
                ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)"#,
                &params![
                    id,
                    now(),
                    report.expected_size,
                    report.actual_size,
                    report.sha256_expected.as_deref(),
                    report.sha256_actual.as_deref(),
                    report.ok,
                    if report.ok { "ok" } else { "mismatch" },
                ],
            )?;
            Ok(())
        })
    }

    pub fn add_event(
        &self,
        download_id: Option<&str>,
        level: &str,
        event: &str,
        details: Option<&str>,
    ) -> Result<()> {
        self.database.with(|connection| {
            connection.execute(
                "INSERT INTO download_events(download_id, ts, level, event, details)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                &params![download_id, now(), level, event, details],
            )?;
            Ok(())
        })
    }

    pub fn events(&self, download_id: Option<&str>, limit: usize) -> Result<Vec<Event>> {
        let limit = limit.clamp(1, 1000) as i64;
        self.database.with(|connection| {
            let rows = match download_id {
                Some(id) => connection.query(
                    "SELECT * FROM download_events WHERE download_id = ?1
                     ORDER BY id DESC LIMIT ?2",
                    &params![id, limit],
                )?,
                None => connection.query(
                    "SELECT * FROM download_events ORDER BY id DESC LIMIT ?1",
                    &params![limit],
                )?,
            };
            rows.iter()
                .map(|row| {
                    Ok(Event {
                        id: row.require_i64("id")?,
                        download_id: row.get_string("download_id"),
                        ts: row.require_str("ts")?.to_string(),
                        level: row.require_str("level")?.to_string(),
                        event: row.require_str("event")?.to_string(),
                        details: row.get_string("details"),
                    })
                })
                .collect()
        })
    }

    pub fn summary(&self) -> Result<StatusSummary> {
        self.database.with(|connection| {
            let rows = connection.query(
                "SELECT state, COUNT(*) AS count, COALESCE(SUM(bytes_downloaded),0) AS bytes,
                        COALESCE(SUM(COALESCE(bytes_total, file_size)),0) AS total
                 FROM downloads GROUP BY state",
                &[],
            )?;
            let mut summary = StatusSummary::default();
            for row in rows {
                let count = row.require_u64("count")?;
                summary.total += count;
                summary.bytes_downloaded += row.get_u64("bytes").unwrap_or(0);
                summary.bytes_total += row.get_u64("total").unwrap_or(0);
                match row.require_str("state")?.parse::<DownloadState>()? {
                    DownloadState::Queued => summary.queued = count,
                    DownloadState::Resolving => summary.resolving = count,
                    DownloadState::Downloading => summary.downloading = count,
                    DownloadState::Paused => summary.paused = count,
                    DownloadState::RetryWait => summary.retry_wait = count,
                    DownloadState::Completed => summary.completed = count,
                    DownloadState::Verifying => summary.verifying = count,
                    DownloadState::Verified => summary.verified = count,
                    DownloadState::Failed => summary.failed = count,
                    DownloadState::Canceled => summary.canceled = count,
                }
            }
            Ok(summary)
        })
    }

    /// Reset interrupted states after a restart so the queue can continue.
    pub fn recover_interrupted(&self) -> Result<usize> {
        self.database.with(|connection| {
            let changed = connection.execute(
                "UPDATE downloads
                 SET state = 'QUEUED', error = 'recovered after worker restart',
                     updated_at = ?1
                 WHERE state IN ('RESOLVING','DOWNLOADING','VERIFYING')",
                &params![now()],
            )?;
            Ok(changed)
        })
    }

    pub fn pause(&self, id: &str) -> Result<bool> {
        self.transition(
            id,
            &["QUEUED", "RESOLVING", "DOWNLOADING", "RETRY_WAIT"],
            DownloadState::Paused,
            None,
        )
    }

    /// Find one still-active download job by URL or chat/message identity.
    pub fn find_active(
        &self,
        source_url: Option<&str>,
        chat_id: Option<i64>,
        message_id: Option<i32>,
    ) -> Result<Option<Download>> {
        self.database.with(|connection| {
            let row = if let (Some(chat), Some(message)) = (chat_id, message_id) {
                connection
                    .query(
                        "SELECT * FROM downloads
                         WHERE chat_id = ?1 AND message_id = ?2
                           AND state NOT IN ('FAILED','CANCELED','VERIFIED')
                         LIMIT 1",
                        &params![chat, message],
                    )?
                    .into_iter()
                    .next()
            } else if let Some(url) = source_url {
                connection
                    .query(
                        "SELECT * FROM downloads
                         WHERE source_url = ?1
                           AND state NOT IN ('FAILED','CANCELED','VERIFIED')
                         LIMIT 1",
                        &params![url],
                    )?
                    .into_iter()
                    .next()
            } else {
                None
            };
            row.map(|row| row_to_download(&row)).transpose()
        })
    }

    pub fn resume(&self, id: &str) -> Result<bool> {
        let changed = self.transition(id, &["PAUSED"], DownloadState::Queued, None)?;
        if changed {
            self.database.with(|connection| {
                connection.execute(
                    "UPDATE downloads SET next_retry_at = NULL, updated_at = ?1 WHERE id = ?2",
                    &params![now(), id],
                )?;
                Ok(())
            })?;
        }
        Ok(changed)
    }

    pub fn cancel(&self, id: &str) -> Result<bool> {
        self.transition(
            id,
            &[
                "QUEUED",
                "RESOLVING",
                "DOWNLOADING",
                "PAUSED",
                "RETRY_WAIT",
                "VERIFYING",
            ],
            DownloadState::Canceled,
            None,
        )
    }

    pub fn retry(&self, id: &str) -> Result<bool> {
        let changed = self.transition(
            id,
            &["FAILED", "CANCELED", "PAUSED"],
            DownloadState::Queued,
            None,
        )?;
        if changed {
            self.database.with(|connection| {
                connection.execute(
                    "UPDATE downloads SET attempt = 0, next_retry_at = NULL, error = NULL,
                     updated_at = ?1 WHERE id = ?2",
                    &params![now(), id],
                )?;
                Ok(())
            })?;
        }
        Ok(changed)
    }

    fn transition(
        &self,
        id: &str,
        from: &[&str],
        to: DownloadState,
        error: Option<&str>,
    ) -> Result<bool> {
        let placeholders = (3..3 + from.len())
            .map(|index| format!("?{index}"))
            .collect::<Vec<_>>()
            .join(",");
        let sql = format!(
            "UPDATE downloads SET state = ?1, error = ?2, updated_at = ?{} WHERE id = ?{} AND state IN ({placeholders})",
            from.len() + 3,
            from.len() + 4
        );
        let mut values: Vec<Value> = vec![
            Value::Text(to.as_str().to_string()),
            error
                .map(|message| Value::Text(message.to_string()))
                .unwrap_or(Value::Null),
        ];
        values.extend(from.iter().map(|state| Value::Text((*state).to_string())));
        values.push(Value::Text(now()));
        values.push(Value::Text(id.to_string()));

        let changed = self
            .database
            .with(|connection| connection.execute(&sql, &values))?;
        if changed > 0 {
            self.add_event(
                Some(id),
                "info",
                to.as_str(),
                error.or(Some("state changed by user")),
            )?;
        }
        Ok(changed > 0)
    }

    pub fn set_setting(&self, key: &str, value: &str) -> Result<()> {
        self.database.with(|connection| {
            connection.execute(
                "INSERT INTO settings(key, value, updated_at) VALUES (?1, ?2, ?3)
                 ON CONFLICT(key) DO UPDATE SET value = excluded.value,
                 updated_at = excluded.updated_at",
                &params![key, value, now()],
            )?;
            Ok(())
        })
    }

    pub fn setting(&self, key: &str) -> Result<Option<String>> {
        self.database.with(|connection| {
            let rows =
                connection.query("SELECT value FROM settings WHERE key = ?1", &params![key])?;
            Ok(rows.first().and_then(|row| row.get_string("value")))
        })
    }

    pub fn upsert_source(
        &self,
        url: &str,
        chat_id: Option<i64>,
        message_id: Option<i32>,
        title: Option<&str>,
    ) -> Result<()> {
        self.database.with(|connection| {
            connection.execute(
                r#"INSERT INTO telegram_sources(url, chat_id, message_id, title, created_at)
                   VALUES (?1, ?2, ?3, ?4, ?5)
                   ON CONFLICT(url) DO UPDATE SET
                       chat_id = COALESCE(excluded.chat_id, telegram_sources.chat_id),
                       message_id = COALESCE(excluded.message_id, telegram_sources.message_id),
                       title = COALESCE(excluded.title, telegram_sources.title)"#,
                &params![url, chat_id, message_id, title, now()],
            )?;
            Ok(())
        })
    }

    pub fn touch_source_scan(&self, url: &str, files: &[FileEntry]) -> Result<()> {
        let first = files.first();
        self.database.with(|connection| {
            connection.execute(
                "UPDATE telegram_sources SET last_scanned_at = ?1,
                 file_name = COALESCE(?2, file_name), file_size = COALESCE(?3, file_size)
                 WHERE url = ?4",
                &params![
                    now(),
                    first.map(|file| file.file_name.clone()),
                    first.map(|file| file.file_size),
                    url
                ],
            )?;
            Ok(())
        })
    }

    pub fn sources(&self, limit: usize) -> Result<Vec<SourceRecord>> {
        let limit = limit.clamp(1, 1000) as i64;
        self.database.with(|connection| {
            let rows = connection.query(
                "SELECT * FROM telegram_sources ORDER BY id DESC LIMIT ?1",
                &params![limit],
            )?;
            rows.iter()
                .map(|row| {
                    Ok(SourceRecord {
                        id: row.require_i64("id")?,
                        url: row.require_str("url")?.to_string(),
                        chat_id: row.get_i64("chat_id"),
                        message_id: row.get_i64("message_id").map(|value| value as i32),
                        title: row.get_string("title"),
                        file_name: row.get_string("file_name"),
                        file_size: row.get_u64("file_size"),
                        last_scanned_at: row.get_string("last_scanned_at"),
                        created_at: row.require_str("created_at")?.to_string(),
                    })
                })
                .collect()
        })
    }

    /// All known downloads' final paths, used for `find_missing_parts`.
    pub fn completed_files(&self) -> Result<Vec<Download>> {
        self.database.with(|connection| {
            let rows = connection.query(
                "SELECT * FROM downloads WHERE state IN ('COMPLETED','VERIFIED')
                 ORDER BY created_at ASC",
                &[],
            )?;
            rows.iter().map(row_to_download).collect()
        })
    }

    /// Purge old events while keeping the most recent 50 per download.
    pub fn trim_events(&self) -> Result<()> {
        self.database.with(|connection| {
            connection.execute(
                "DELETE FROM download_events WHERE id NOT IN (
                     SELECT id FROM download_events ORDER BY id DESC LIMIT 5000
                 )",
                &[],
            )?;
            Ok(())
        })
    }
}

enum ExistingOrNew {
    Existing(String),
    New(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_store() -> (Store, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open_path(&dir.path().join("state.db")).unwrap();
        (store, dir)
    }

    fn queued_job(store: &Store) -> Download {
        let mut new = NewDownload::new("file.bin", PathBuf::from("D:\\Downloads\\Brave"), 3);
        new.chat_id = Some(-1001188194766);
        new.message_id = Some(1914);
        new.source_url = Some("https://t.me/c/1188194766/1914".into());
        store.insert(new).unwrap()
    }

    #[test]
    fn pause_resume_cancel_retry_round_trip() {
        let (store, _dir) = test_store();
        let job = queued_job(&store);

        assert!(store.pause(&job.id).unwrap());
        assert_eq!(store.get(&job.id).unwrap().state, DownloadState::Paused);
        // Pause is idempotent in the state machine's favor: no transition, no change.
        assert!(!store.pause(&job.id).unwrap());

        assert!(store.resume(&job.id).unwrap());
        let resumed = store.get(&job.id).unwrap();
        assert_eq!(resumed.state, DownloadState::Queued);
        assert_eq!(resumed.error, None);

        assert!(store.cancel(&job.id).unwrap());
        assert_eq!(store.get(&job.id).unwrap().state, DownloadState::Canceled);

        assert!(store.retry(&job.id).unwrap());
        let retried = store.get(&job.id).unwrap();
        assert_eq!(retried.state, DownloadState::Queued);
        assert_eq!(retried.attempt, 0);
        assert!(!store.retry(&job.id).unwrap());
    }

    #[test]
    fn transition_records_and_clears_error_text() {
        let (store, _dir) = test_store();
        let job = queued_job(&store);

        store
            .set_state(&job.id, DownloadState::Failed, Some("boom"))
            .unwrap();
        assert_eq!(store.get(&job.id).unwrap().error.as_deref(), Some("boom"));

        // Resume/retry transitions clear the previous error message.
        assert!(store.retry(&job.id).unwrap());
        assert_eq!(store.get(&job.id).unwrap().error, None);
    }
}
