//! The background worker.
//!
//! The worker is the only process that owns a live Telegram session. It
//! recovers interrupted jobs, downloads one file at a time by default, writes
//! progress to SQLite, watches the database for pause/cancel requests, and
//! exposes read-only Telegram queries over the local named pipe.

use std::collections::HashSet;
use std::fs::{File, OpenOptions};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use chrono::Utc;
use fs2::FileExt;
use tokio::sync::watch;
use tokio::task::{JoinError, JoinSet};

use crate::config::{Config, Paths};
use crate::database::Store;
use crate::downloader::{download_part, ChunkSource, DownloadControl, RetryPolicy};
use crate::error::{Error, Result};
use crate::ipc::{self, Request, RequestHandler, Response};
use crate::models::{Download, DownloadState, WorkerStatus};
use crate::storage;
use crate::telegram::{self, LoginStatus, TelegramConnection};
use crate::verification::{self, FinalizeOutcome};

/// How often the worker looks for new queued jobs.
const QUEUE_POLL: Duration = Duration::from_secs(2);
/// How often a running download checks the database for pause/cancel.
const CONTROL_POLL: Duration = Duration::from_secs(2);
/// How often progress is written to SQLite.
const PROGRESS_POLL: Duration = Duration::from_secs(1);
/// How long a graceful shutdown waits for active downloads.
const SHUTDOWN_GRACE: Duration = Duration::from_secs(120);

/// Shared state used by the worker loop and the IPC handlers.
pub struct WorkerContext {
    pub paths: Paths,
    pub config: Config,
    pub store: Arc<Store>,
    pub telegram: Option<Arc<TelegramConnection>>,
    active: Mutex<HashSet<String>>,
    stop: Arc<AtomicBool>,
    last_error: Mutex<Option<String>>,
    started: Instant,
}

impl WorkerContext {
    fn telegram(&self) -> Result<&TelegramConnection> {
        self.telegram.as_deref().ok_or_else(|| {
            Error::Config(
                "the worker has no Telegram connection; set api_id/api_hash and run `tgdlrs login`"
                    .into(),
            )
        })
    }

    fn active_download(&self) -> Option<String> {
        self.active
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .iter()
            .next()
            .cloned()
    }

    fn set_last_error(&self, error: Option<String>) {
        *self
            .last_error
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = error;
    }

    fn last_error(&self) -> Option<String> {
        self.last_error
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    async fn login_status(&self) -> LoginStatus {
        match &self.telegram {
            Some(connection) => connection.login_status().await.unwrap_or(LoginStatus {
                authorized: false,
                user: None,
                session_present: connection.session().has_persisted_data(),
            }),
            None => LoginStatus {
                authorized: false,
                user: None,
                session_present: self.paths.session.exists(),
            },
        }
    }

    async fn status(&self) -> WorkerStatus {
        let authorized = match &self.telegram {
            Some(connection) => {
                match tokio::time::timeout(
                    Duration::from_secs(10),
                    connection.client().is_authorized(),
                )
                .await
                {
                    Ok(Ok(value)) => value,
                    _ => false,
                }
            }
            None => false,
        };
        let user = if authorized {
            match &self.telegram {
                Some(connection) => {
                    tokio::time::timeout(Duration::from_secs(5), connection.client().get_me())
                        .await
                        .ok()
                        .and_then(|result| result.ok())
                        .and_then(|user| user.first_name().map(str::to_string))
                }
                None => None,
            }
        } else {
            None
        };
        let session_present = self
            .telegram
            .as_ref()
            .map(|connection| connection.session().has_persisted_data())
            .unwrap_or_else(|| self.paths.session.exists());
        let summary = self.store.summary().unwrap_or_default();
        let disk_free_bytes = storage::free_space(&self.config.download_dir).unwrap_or(0);
        WorkerStatus {
            running: true,
            pid: Some(std::process::id()),
            version: env!("CARGO_PKG_VERSION").to_string(),
            authorized,
            session_present,
            user,
            active_download: self.active_download(),
            download_dir: self.config.download_dir.clone(),
            disk_free_bytes,
            disk_reserve_bytes: self.config.disk_reserve_bytes,
            summary,
            last_error: self.last_error(),
        }
    }
}

#[async_trait]
impl RequestHandler for WorkerContext {
    async fn handle(&self, request: Request) -> Response {
        let result: Result<serde_json::Value> = match request {
            Request::Ping => Ok(serde_json::json!({
                "pong": true,
                "pid": std::process::id(),
                "version": env!("CARGO_PKG_VERSION"),
                "uptime_seconds": self.started.elapsed().as_secs(),
            })),
            Request::Status => to_value(self.status().await),
            Request::LoginStatus => to_value(self.login_status().await),
            Request::ResolveLink { url } => match self.telegram() {
                Ok(connection) => match connection.resolve_link(&url).await {
                    Ok(resolved) => {
                        let _ = self.store.upsert_source(
                            &url,
                            Some(resolved.chat_id),
                            Some(resolved.message_id),
                            resolved.title.as_deref(),
                        );
                        to_value(resolved)
                    }
                    Err(error) => Err(error),
                },
                Err(error) => Err(error),
            },
            Request::ScanChat { chat_id, limit } => match self.telegram() {
                Ok(connection) => match connection.scan_chat(chat_id, limit.unwrap_or(500)).await {
                    Ok(files) => {
                        let _ = self.store.add_event(
                            None,
                            "info",
                            "SCAN",
                            Some(&format!("chat {chat_id}: {} files", files.len())),
                        );
                        to_value(files)
                    }
                    Err(error) => Err(error),
                },
                Err(error) => Err(error),
            },
            Request::SearchFiles {
                chat_id,
                query,
                limit,
            } => match self.telegram() {
                Ok(connection) => match connection
                    .search_files(chat_id, &query, limit.unwrap_or(500))
                    .await
                {
                    Ok(files) => to_value(files),
                    Err(error) => Err(error),
                },
                Err(error) => Err(error),
            },
            Request::Stop => {
                self.stop.store(true, Ordering::SeqCst);
                let _ = self.store.add_event(
                    None,
                    "info",
                    "WORKER_STOP_REQUESTED",
                    Some("stop requested over IPC"),
                );
                Ok(serde_json::json!({ "stopping": true }))
            }
        };

        match result {
            Ok(data) => Response::Ok { data },
            Err(error) => Response::error(&error),
        }
    }
}

fn to_value<T: serde::Serialize>(value: T) -> Result<serde_json::Value> {
    serde_json::to_value(value).map_err(Error::from)
}

/// Acquire the single-instance worker lock.
pub fn acquire_worker_lock(paths: &Paths) -> Result<File> {
    paths.ensure()?;
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(&paths.lock_file)?;
    file.try_lock_exclusive().map_err(|_| {
        Error::Conflict(format!(
            "another tgdlrs worker already owns {}",
            paths.lock_file.display()
        ))
    })?;
    Ok(file)
}

/// Connect using the persisted session, when credentials are available.
async fn connect_telegram(paths: &Paths, config: &Config) -> Result<TelegramConnection> {
    let credentials = config.credentials()?;
    let session = Arc::new(telegram::session::FileSession::load(&paths.session)?);
    telegram::connect(session, credentials.api_id).await
}

/// Run the worker until `stop` is requested.
pub async fn run(paths: Paths, config: Config) -> Result<()> {
    paths.ensure()?;
    config.ensure_download_dir()?;

    let _lock = acquire_worker_lock(&paths)?;
    let store = Arc::new(Store::open(&paths)?);
    let recovered = store.recover_interrupted()?;
    store.add_event(
        None,
        "info",
        "WORKER_START",
        Some(&format!(
            "pid {}, recovered {recovered} interrupted job(s)",
            std::process::id()
        )),
    )?;

    let telegram = match connect_telegram(&paths, &config).await {
        Ok(connection) => Some(Arc::new(connection)),
        Err(error) => {
            tracing::warn!(%error, "worker started without a Telegram connection");
            let _ = store.add_event(
                None,
                "warn",
                "TELEGRAM_UNAVAILABLE",
                Some(&error.to_string()),
            );
            None
        }
    };

    let context = Arc::new(WorkerContext {
        paths: paths.clone(),
        config: config.clone(),
        store: Arc::clone(&store),
        telegram,
        active: Mutex::new(HashSet::new()),
        stop: Arc::new(AtomicBool::new(false)),
        last_error: Mutex::new(None),
        started: Instant::now(),
    });

    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let pipe_name = paths.pipe_name.clone();
    let server = tokio::spawn({
        let context = Arc::clone(&context);
        async move {
            if let Err(error) = ipc::serve(pipe_name, context, shutdown_rx).await {
                tracing::error!(%error, "IPC server stopped");
            }
        }
    });

    let mut jobs: JoinSet<(String, Result<()>)> = JoinSet::new();
    let mut ticker = tokio::time::interval(QUEUE_POLL);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    loop {
        while let Some(joined) = jobs.try_join_next() {
            report_job(&context, joined);
        }

        if context.stop.load(Ordering::SeqCst) {
            break;
        }

        let max_files = config.max_concurrent_files.max(1);
        if jobs.len() < max_files {
            let active: HashSet<String> = context
                .active
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .clone();
            match store.runnable(100) {
                Ok(candidates) => {
                    for download in candidates {
                        if jobs.len() >= max_files {
                            break;
                        }
                        if active.contains(&download.id) {
                            continue;
                        }
                        context
                            .active
                            .lock()
                            .unwrap_or_else(|poisoned| poisoned.into_inner())
                            .insert(download.id.clone());
                        let job_context = Arc::clone(&context);
                        jobs.spawn(async move {
                            let id = download.id.clone();
                            let result = run_job(Arc::clone(&job_context), download).await;
                            job_context
                                .active
                                .lock()
                                .unwrap_or_else(|poisoned| poisoned.into_inner())
                                .remove(&id);
                            (id, result)
                        });
                    }
                }
                Err(error) => {
                    tracing::error!(%error, "cannot read the queue");
                    context.set_last_error(Some(error.to_string()));
                }
            }
        }

        tokio::select! {
            _ = tokio::signal::ctrl_c() => {
                context.stop.store(true, Ordering::SeqCst);
            }
            _ = ticker.tick() => {}
            Some(joined) = jobs.join_next(), if !jobs.is_empty() => {
                report_job(&context, joined);
            }
        }
    }

    let deadline = Instant::now() + SHUTDOWN_GRACE;
    while !jobs.is_empty() && Instant::now() < deadline {
        match tokio::time::timeout(Duration::from_secs(5), jobs.join_next()).await {
            Ok(Some(joined)) => report_job(&context, joined),
            Ok(None) => break,
            Err(_) => {}
        }
    }
    if !jobs.is_empty() {
        tracing::warn!(
            "aborting {} download(s) after shutdown grace period",
            jobs.len()
        );
        jobs.abort_all();
    }

    let _ = shutdown_tx.send(true);
    let _ = tokio::time::timeout(Duration::from_secs(5), server).await;
    if let Some(connection) = &context.telegram {
        connection.shutdown().await;
    }
    store.add_event(None, "info", "WORKER_STOP", None)?;
    Ok(())
}

fn report_job(
    context: &WorkerContext,
    joined: std::result::Result<(String, Result<()>), JoinError>,
) {
    match joined {
        Ok((_, Ok(()))) => {}
        Ok((id, Err(error))) => {
            if !matches!(error, Error::Paused | Error::Canceled) {
                tracing::error!(download = %id, %error, "download job failed");
                context.set_last_error(Some(format!("{id}: {error}")));
            }
        }
        Err(error) => {
            tracing::error!(%error, "download task panicked or was aborted");
            context.set_last_error(Some(error.to_string()));
        }
    }
}

async fn run_job(context: Arc<WorkerContext>, mut download: Download) -> Result<()> {
    let id = download.id.clone();
    let telegram = context.telegram()?;

    if download.chat_id.is_none() || download.message_id.is_none() {
        let url = download
            .source_url
            .clone()
            .ok_or_else(|| Error::InvalidInput(format!("download {id} has no source")))?;
        context
            .store
            .set_state(&id, DownloadState::Resolving, None)?;
        let resolved = telegram.resolve_link(&url).await?;
        context.store.upsert_source(
            &url,
            Some(resolved.chat_id),
            Some(resolved.message_id),
            resolved.title.as_deref(),
        )?;
        download = context.store.apply_resolution(
            &id,
            resolved.chat_id,
            resolved.message_id,
            None,
            &download.file_name,
            download.file_size,
            download.mime_type.as_deref(),
        )?;
    }

    let chat_id = download
        .chat_id
        .ok_or_else(|| Error::InvalidInput(format!("download {id} has no chat id")))?;
    let message_id = download
        .message_id
        .ok_or_else(|| Error::InvalidInput(format!("download {id} has no message id")))?;

    context
        .store
        .set_state(&id, DownloadState::Resolving, None)?;
    let file = telegram.fetch_file(chat_id, message_id).await?;

    if let Some(stored) = download.document_id {
        if stored != file.document_id {
            return Err(Error::Conflict(format!(
                "message {message_id} now contains document {} instead of the queued document {stored}; \
                 refusing to mix bytes from different sources",
                file.document_id
            )));
        }
    }
    if let Some(stored) = download.file_size {
        if stored != file.file_size {
            return Err(Error::Conflict(format!(
                "message {message_id} now has size {} instead of the queued size {stored}; \
                 refusing to overwrite or append",
                file.file_size
            )));
        }
    }

    download = context.store.apply_resolution(
        &id,
        chat_id,
        message_id,
        Some(file.document_id),
        &file.file_name,
        Some(file.file_size),
        file.mime_type.as_deref(),
    )?;

    let verify = download.verify && context.config.verify_sha256;
    let expected_sha = if verify {
        download.sha256_expected.as_deref()
    } else {
        None
    };

    if download.final_path.exists() {
        let report =
            verification::verify_file(&download.final_path, Some(file.file_size), expected_sha)?;
        context.store.record_verification(&id, &report)?;
        if report.ok {
            if verify {
                context
                    .store
                    .mark_verified(&id, report.sha256_actual.as_deref().unwrap_or(""))?;
            } else {
                context
                    .store
                    .mark_completed(&id, report.sha256_actual.as_deref())?;
            }
            return Ok(());
        }
        return Err(Error::Conflict(format!(
            "{} already exists but does not match the expected size/hash; refusing to overwrite",
            download.final_path.display()
        )));
    }

    let part_len = storage::file_len(&download.part_path)?;
    let remaining = file.file_size.saturating_sub(part_len);
    storage::ensure_free_space(
        &download.download_dir,
        remaining,
        context.config.disk_reserve_bytes,
    )?;

    context.store.begin_attempt(&id)?;

    let remaining_attempts = download
        .max_attempts
        .saturating_sub(download.attempt)
        .max(1);
    let retry = RetryPolicy::new(
        remaining_attempts,
        context.config.retry_base_ms,
        context.config.retry_max_ms,
    );
    let control = DownloadControl::new();
    let mut last_progress = Instant::now() - PROGRESS_POLL;
    let mut last_poll = Instant::now() - CONTROL_POLL;

    let store = Arc::clone(&context.store);
    let stop = Arc::clone(&context.stop);
    let progress_control = control.clone();
    let progress_id = id.clone();
    let total = file.file_size;
    let part_path = download.part_path.clone();
    let document = file.document.clone();
    let source_client = telegram;

    let outcome = download_part(
        &part_path,
        Some(total),
        |offset| {
            let source = source_client.source(&document, offset);
            Ok(Box::new(source) as Box<dyn ChunkSource>)
        },
        |written, total| {
            if stop.load(Ordering::SeqCst) {
                progress_control.pause();
            }
            if last_poll.elapsed() >= CONTROL_POLL {
                last_poll = Instant::now();
                if let Ok(current) = store.get(&progress_id) {
                    match current.state {
                        DownloadState::Paused => progress_control.pause(),
                        DownloadState::Canceled => progress_control.cancel(),
                        _ => {}
                    }
                }
            }
            if last_progress.elapsed() >= PROGRESS_POLL {
                last_progress = Instant::now();
                store.set_progress(&progress_id, written, total)?;
            }
            Ok(())
        },
        &control,
        &retry,
        |_attempt, delay, error| {
            let seconds = delay.as_secs().max(1);
            let _ = store.schedule_retry(&progress_id, &error.to_string(), seconds);
        },
    )
    .await;

    match outcome {
        Ok(result) => {
            context
                .store
                .set_redownloaded(&id, result.redownloaded_bytes)?;
            context
                .store
                .set_progress(&id, result.bytes_written, Some(total))?;
            context
                .store
                .set_state(&id, DownloadState::Verifying, None)?;

            match verification::finalize_part(
                &download.part_path,
                &download.final_path,
                Some(total),
                expected_sha,
                verify,
            ) {
                Ok(outcome) => {
                    let report = match outcome {
                        FinalizeOutcome::Renamed(report) => report,
                        FinalizeOutcome::AlreadyPresent(report) => report,
                    };
                    context.store.record_verification(&id, &report)?;
                    if verify {
                        context
                            .store
                            .mark_verified(&id, report.sha256_actual.as_deref().unwrap_or(""))?;
                    } else {
                        context
                            .store
                            .mark_completed(&id, report.sha256_actual.as_deref())?;
                    }
                    Ok(())
                }
                Err(Error::Verification(message)) => {
                    // Keep the bytes for inspection, but move them out of the
                    // way so a retry can start from zero.
                    quarantine(&download.part_path);
                    Err(Error::Verification(message))
                }
                Err(error) => Err(error),
            }
        }
        Err(Error::Paused) => {
            if context.stop.load(Ordering::SeqCst) {
                let current = context.store.get(&id)?;
                if current.state != DownloadState::Paused {
                    context.store.set_state(
                        &id,
                        DownloadState::Queued,
                        Some("worker stopped; will resume"),
                    )?;
                }
            }
            Ok(())
        }
        Err(Error::Canceled) => {
            context
                .store
                .set_state(&id, DownloadState::Canceled, None)?;
            Ok(())
        }
        Err(error) => {
            let attempt = context
                .store
                .get(&id)
                .map(|current| current.attempt)
                .unwrap_or(download.attempt + 1);
            if error.is_retryable() && attempt < download.max_attempts {
                let delay = retry.delay_for(attempt).as_secs().max(1);
                context
                    .store
                    .schedule_retry(&id, &error.to_string(), delay)?;
                Ok(())
            } else {
                context
                    .store
                    .set_state(&id, DownloadState::Failed, Some(&error.to_string()))?;
                Err(error)
            }
        }
    }
}

/// Rename a corrupt complete part to `<part>.bad-<timestamp>`.
fn quarantine(part_path: &Path) {
    if !part_path.exists() {
        return;
    }
    let stamp = Utc::now().format("%Y%m%d-%H%M%S");
    let destination = PathBuf::from(format!("{}.bad-{stamp}", part_path.display()));
    if let Err(error) = storage::rename_no_overwrite(part_path, &destination) {
        tracing::warn!(
            %error,
            path = %part_path.display(),
            "cannot quarantine a corrupt partial file"
        );
    } else {
        tracing::warn!(path = %destination.display(), "quarantined a corrupt partial file");
    }
}
