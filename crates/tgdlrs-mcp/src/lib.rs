//! MCP server for Telegram Downloader RS.
//!
//! Runs over stdio. Every tool is either a quick SQLite query (queue
//! control, listings, diagnostics) or a single named-pipe round trip with
//! the background worker (Telegram queries). Nothing here downloads files:
//! downloads are enqueued and the worker performs them.
//!
//! Credentials (`api_id`, `api_hash`, OTP, 2FA password, session data) are
//! never sent to clients and never logged.

use std::collections::HashSet;
use std::path::PathBuf;
use std::str::FromStr;
use std::sync::Arc;

use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{Implementation, ServerCapabilities, ServerConfig};
use rmcp::{tool, tool_handler, tool_router, Json, ServerHandler, ServiceExt};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use tgdlrs_core::config::{Config, Paths};
use tgdlrs_core::database::Store;
use tgdlrs_core::error::{Error, Result as CoreResult};
use tgdlrs_core::ipc::{self, Request};
use tgdlrs_core::launch;
use tgdlrs_core::models::{Download, DownloadState, FileEntry, NewDownload};
use tgdlrs_core::storage;
use tgdlrs_core::verification;

/// Structured error object embedded in the text content of failed tools.
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct ToolError {
    /// Stable machine-readable error kind (e.g. `flood_wait`).
    pub kind: String,
    /// Human-readable description.
    pub message: String,
}

fn tool_error(error: &Error) -> String {
    serde_json::to_string(&ToolError {
        kind: error.kind().to_string(),
        message: error.to_string(),
    })
    .unwrap_or_else(|_| error.to_string())
}

// ---------------------------------------------------------------- DTOs --

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct LoginStatusOut {
    pub worker_reachable: bool,
    pub worker_running: bool,
    pub worker_version: Option<String>,
    pub authorized: bool,
    pub user: Option<String>,
    pub session_present: bool,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct ResolveLinkOut {
    pub url: String,
    pub chat_id: i64,
    pub message_id: i32,
    pub title: Option<String>,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct ResolveLinkParams {
    /// t.me message link, e.g. https://t.me/c/1188194766/1914
    pub url: String,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct ScanChatParams {
    /// Bot API chat id (-100...) or channel peer id.
    pub chat_id: i64,
    /// Maximum number of messages to inspect (default 500, cap 10000).
    pub limit: Option<usize>,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct SearchFilesParams {
    /// Bot API chat id (-100...) or channel peer id.
    pub chat_id: i64,
    /// File name query text; empty string falls back to a plain listing.
    pub query: String,
    /// Maximum number of messages to inspect (default 500, cap 10000).
    pub limit: Option<usize>,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct FileEntryOut {
    pub message_id: i32,
    pub file_name: String,
    pub file_size: u64,
    pub mime_type: Option<String>,
    pub document_id: Option<i64>,
    pub date: String,
    pub already_downloaded: bool,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct ScanOut {
    pub chat_id: i64,
    pub count: usize,
    pub files: Vec<FileEntryOut>,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct DownloadParams {
    /// t.me message link. Exactly one of `url` or (`chat_id` and `message_id`) is required.
    pub url: Option<String>,
    /// Bot API chat id (-100...) or channel peer id.
    pub chat_id: Option<i64>,
    /// Message id containing the document.
    pub message_id: Option<i32>,
    /// Override the stored file name (used until Telegram resolves the real one).
    pub file_name: Option<String>,
    /// Expected SHA-256 hex digest; verified after download.
    pub sha256: Option<String>,
    /// Queue priority (higher runs first).
    pub priority: Option<i64>,
    /// Override the download directory for this job.
    pub download_dir: Option<String>,
    /// Set to false to skip SHA-256 verification for this job.
    pub verify: Option<bool>,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct DownloadJobOut {
    pub id: String,
    pub state: String,
    pub file_name: String,
    pub file_size: Option<u64>,
    pub source_url: Option<String>,
    pub final_path: String,
    pub reused_existing: bool,
    pub worker_reachable: bool,
    pub worker_started: bool,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct BatchDownloadParams {
    /// t.me message links to enqueue (max 200).
    pub urls: Vec<String>,
    /// Queue priority applied to every job.
    pub priority: Option<i64>,
    /// Override the download directory for every job.
    pub download_dir: Option<String>,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct BatchFailure {
    pub url: String,
    pub error: String,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct BatchOut {
    pub requested: usize,
    pub created_count: usize,
    pub reused_count: usize,
    pub created: Vec<DownloadJobOut>,
    pub failed: Vec<BatchFailure>,
    pub worker_started: bool,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct ListDownloadsParams {
    /// Filter by state (QUEUED, DOWNLOADING, PAUSED, RETRY_WAIT, COMPLETED,
    /// VERIFYING, VERIFIED, FAILED, CANCELED). Case-insensitive.
    pub state: Option<String>,
    /// Maximum number of rows (default 50, cap 500).
    pub limit: Option<usize>,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct DownloadSummaryOut {
    pub id: String,
    pub state: String,
    pub file_name: String,
    pub bytes_downloaded: u64,
    pub bytes_total: Option<u64>,
    pub progress: Option<f64>,
    pub attempt: u32,
    pub max_attempts: u32,
    pub error: Option<String>,
    pub sha256_expected: Option<String>,
    pub sha256_actual: Option<String>,
    pub source_url: Option<String>,
    pub final_path: String,
    pub part_path: String,
    pub redownloaded_bytes: u64,
    pub created_at: String,
    pub updated_at: String,
    pub completed_at: Option<String>,
    pub next_retry_at: Option<String>,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct ListOut {
    pub returned: usize,
    pub downloads: Vec<DownloadSummaryOut>,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct DownloadStatusParams {
    /// Full id or unique prefix.
    pub id: String,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct IdParams {
    /// Full id or unique prefix.
    pub id: String,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct ActionOut {
    pub id: String,
    pub state: String,
    pub changed: bool,
    pub message: Option<String>,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct VerifyParams {
    /// Verify the file tracked by this download job (full id or prefix).
    pub id: Option<String>,
    /// Or verify an arbitrary path on disk.
    pub path: Option<String>,
    /// Expected size in bytes (defaults to the recorded size for `id`).
    pub expected_size: Option<u64>,
    /// Expected SHA-256 hex digest (defaults to the recorded hash for `id`).
    pub sha256: Option<String>,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct VerifyOut {
    pub id: Option<String>,
    pub path: String,
    pub exists: bool,
    pub size_ok: bool,
    pub hash_ok: bool,
    pub ok: bool,
    pub expected_size: Option<u64>,
    pub actual_size: u64,
    pub sha256_expected: Option<String>,
    pub sha256_actual: Option<String>,
    pub recorded: bool,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct MissingPartsParams {
    /// Directory to scan; defaults to the configured download directory.
    pub directory: Option<String>,
    /// File name prefix, e.g. `CHINT.7z.`
    pub prefix: String,
    /// First index to check (default 1).
    pub start: Option<u32>,
    /// Last index to check (default: highest index found, or `start`).
    pub end: Option<u32>,
    /// Number of digits in each index (default 3, e.g. `.001`).
    pub padding: Option<u32>,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct FoundPart {
    pub index: u32,
    pub name: String,
    pub size_bytes: u64,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct MissingPartsOut {
    pub directory: String,
    pub prefix: String,
    pub checked_from: u32,
    pub checked_to: u32,
    pub found: Vec<FoundPart>,
    pub missing: Vec<u32>,
    pub complete: bool,
    pub total_bytes: u64,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct DiagnosticsOut {
    pub version: String,
    pub platform: String,
    pub home: String,
    pub config_file: String,
    pub database: String,
    pub session_file: String,
    pub download_dir: String,
    pub download_dir_writable: bool,
    pub disk_free_bytes: u64,
    pub disk_reserve_bytes: u64,
    pub sqlite_version: Option<String>,
    pub database_ok: bool,
    pub database_error: Option<String>,
    pub worker_reachable: bool,
    pub worker_running: bool,
    pub worker_authorized: bool,
    pub worker_last_error: Option<String>,
    pub session_present: bool,
    pub max_concurrent_files: usize,
    pub verify_sha256: bool,
    pub skip_verified: bool,
    pub keep_partials: bool,
    pub auto_start_worker: bool,
    pub total_jobs: u64,
    pub verified_jobs: u64,
    pub failed_jobs: u64,
    pub config_error: Option<String>,
}

// ------------------------------------------------------------ the server --

#[derive(Clone)]
pub struct DownloadMcp {
    paths: Paths,
    config: Option<Config>,
    store: Arc<Store>,
    tool_router: ToolRouter<Self>,
    allow_worker_spawn: bool,
}

impl DownloadMcp {
    pub fn new() -> CoreResult<Self> {
        let paths = Paths::discover()?;
        paths.ensure()?;
        let store = Arc::new(Store::open(&paths)?);
        let config = Config::load_or_create(&paths).ok();
        Ok(Self {
            paths,
            config,
            store,
            tool_router: Self::tool_router(),
            allow_worker_spawn: true,
        })
    }

    #[cfg(test)]
    fn for_test(paths: Paths, config: Config, store: Arc<Store>) -> Self {
        Self {
            paths,
            config: Some(config),
            store,
            tool_router: Self::tool_router(),
            allow_worker_spawn: false,
        }
    }

    fn require_config(&self) -> CoreResult<&Config> {
        self.config.as_ref().ok_or_else(|| {
            Error::Config(format!(
                "cannot load {}; fix or remove it and try again",
                self.paths.config_file.display()
            ))
        })
    }

    /// Mark which entries are already present in the verified set.
    fn annotate_downloaded(&self, files: &mut [FileEntry]) {
        let mut known: HashSet<(String, u64)> = HashSet::new();
        if let Ok(completed) = self.store.completed_files() {
            for download in completed {
                let size = download.bytes_total.or(download.file_size).unwrap_or(0);
                known.insert((download.file_name.to_ascii_lowercase(), size));
            }
        }
        for entry in files.iter_mut() {
            entry.already_downloaded = entry.file_size > 0
                && known.contains(&(entry.file_name.to_ascii_lowercase(), entry.file_size));
        }
    }

    fn file_entry_out(entry: &FileEntry) -> FileEntryOut {
        FileEntryOut {
            message_id: entry.message_id,
            file_name: entry.file_name.clone(),
            file_size: entry.file_size,
            mime_type: entry.mime_type.clone(),
            document_id: entry.document_id,
            date: entry.date.clone(),
            already_downloaded: entry.already_downloaded,
        }
    }

    fn summary_out(download: &Download) -> DownloadSummaryOut {
        DownloadSummaryOut {
            id: download.id.clone(),
            state: download.state.as_str().to_string(),
            file_name: download.file_name.clone(),
            bytes_downloaded: download.bytes_downloaded,
            bytes_total: download.bytes_total.or(download.file_size),
            progress: download.progress_fraction(),
            attempt: download.attempt,
            max_attempts: download.max_attempts,
            error: download.error.clone(),
            sha256_expected: download.sha256_expected.clone(),
            sha256_actual: download.sha256_actual.clone(),
            source_url: download.source_url.clone(),
            final_path: download.final_path.to_string_lossy().into_owned(),
            part_path: download.part_path.to_string_lossy().into_owned(),
            redownloaded_bytes: download.redownloaded_bytes,
            created_at: download.created_at.clone(),
            updated_at: download.updated_at.clone(),
            completed_at: download.completed_at.clone(),
            next_retry_at: download.next_retry_at.clone(),
        }
    }

    fn job_out(
        &self,
        download: &Download,
        reused: bool,
        worker_reachable: bool,
        worker_started: bool,
    ) -> DownloadJobOut {
        DownloadJobOut {
            id: download.id.clone(),
            state: download.state.as_str().to_string(),
            file_name: download.file_name.clone(),
            file_size: download.file_size,
            source_url: download.source_url.clone(),
            final_path: download.final_path.to_string_lossy().into_owned(),
            reused_existing: reused,
            worker_reachable,
            worker_started,
        }
    }

    /// Ensure the background worker is running; report (reachable, started).
    async fn ensure_worker(&self) -> (bool, bool) {
        if !self.allow_worker_spawn {
            return (false, false);
        }
        if ipc::ping(&self.paths.pipe_name).await {
            return (true, false);
        }
        match launch::spawn_worker_detached() {
            Ok(_) => {
                let ready = launch::wait_for_worker(&self.paths.pipe_name, 20).await;
                (ready, true)
            }
            Err(_) => (false, true),
        }
    }
}

// ------------------------------------------------------------------ tools --

/// Unique placeholder for jobs whose file name is only known after Telegram resolution.
fn placeholder_file_name(
    url: Option<&str>,
    chat_id: Option<i64>,
    message_id: Option<i32>,
) -> String {
    if let (Some(chat), Some(message)) = (chat_id, message_id) {
        return format!("telegram-{chat}-{message}.bin");
    }
    if let Some(url) = url {
        if let Ok(parsed) = tgdlrs_core::telegram::parse_link(url) {
            return format!("download-{}.bin", parsed.message_id);
        }
    }
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0);
    format!("download-{nanos}.bin")
}

/// Look up one download by full id or unique prefix.
fn find_download(store: &Store, id: &str) -> CoreResult<Download> {
    let trimmed = id.trim();
    if trimmed.is_empty() {
        return Err(Error::InvalidInput("download id must not be empty".into()));
    }
    store.find(trimmed)
}

fn validate_sha256(value: &str) -> CoreResult<String> {
    let normalized = value.trim().to_ascii_lowercase();
    if normalized.len() == 64 && normalized.chars().all(|c| c.is_ascii_hexdigit()) {
        Ok(normalized)
    } else {
        Err(Error::InvalidInput(
            "sha256 must be 64 hexadecimal characters".into(),
        ))
    }
}

fn resolve_download_dir(config: &Config, requested: Option<&str>) -> CoreResult<PathBuf> {
    match requested.map(str::trim) {
        None | Some("") => Ok(config.download_dir.clone()),
        Some(path) => {
            let candidate = PathBuf::from(path);
            if candidate.is_absolute() {
                Ok(candidate)
            } else {
                Ok(config.download_dir.join(candidate))
            }
        }
    }
}

#[tool_router]
impl DownloadMcp {
    /// Check whether the background worker is running and the Telegram session is authorized.
    #[tool(
        name = "telegram_login_status",
        description = "Check background worker reachability and whether the Telegram session is authorized. Never returns credentials or session data."
    )]
    async fn telegram_login_status(&self) -> std::result::Result<Json<LoginStatusOut>, String> {
        let outcome: CoreResult<LoginStatusOut> = async {
            let session_present = self.paths.session.exists();
            if !ipc::ping(&self.paths.pipe_name).await {
                return Ok(LoginStatusOut {
                    worker_reachable: false,
                    worker_running: false,
                    worker_version: None,
                    authorized: false,
                    user: None,
                    session_present,
                });
            }
            let status: tgdlrs_core::models::WorkerStatus = tokio::time::timeout(
                std::time::Duration::from_secs(10),
                ipc::request(&self.paths.pipe_name, &Request::Status),
            )
            .await
            .map_err(|_| Error::Ipc("worker did not answer in time".into()))??;
            Ok(LoginStatusOut {
                worker_reachable: true,
                worker_running: status.running,
                worker_version: Some(status.version),
                authorized: status.authorized,
                user: status.user,
                session_present: status.session_present,
            })
        }
        .await;
        outcome.map(Json).map_err(|error| tool_error(&error))
    }

    /// Resolve a t.me message link into chat id, message id and title.
    #[tool(
        name = "telegram_resolve_link",
        description = "Resolve a t.me message link (public or private /c/ link) into a chat id, message id and title. Requires the worker to be running."
    )]
    async fn telegram_resolve_link(
        &self,
        Parameters(params): Parameters<ResolveLinkParams>,
    ) -> std::result::Result<Json<ResolveLinkOut>, String> {
        let outcome: CoreResult<ResolveLinkOut> = async {
            let resolved: tgdlrs_core::models::ResolvedLink = tokio::time::timeout(
                std::time::Duration::from_secs(60),
                ipc::request(
                    &self.paths.pipe_name,
                    &Request::ResolveLink {
                        url: params.url.clone(),
                    },
                ),
            )
            .await
            .map_err(|_| Error::Ipc("worker did not answer in time".into()))??;
            Ok(ResolveLinkOut {
                url: resolved.url,
                chat_id: resolved.chat_id,
                message_id: resolved.message_id,
                title: resolved.title,
            })
        }
        .await;
        outcome.map(Json).map_err(|error| tool_error(&error))
    }

    /// List downloadable documents in a chat, newest first.
    #[tool(
        name = "telegram_scan_chat",
        description = "List documents with sizes in a Telegram chat, newest first. Requires the worker to be running."
    )]
    async fn telegram_scan_chat(
        &self,
        Parameters(params): Parameters<ScanChatParams>,
    ) -> std::result::Result<Json<ScanOut>, String> {
        let outcome = self.scan_files(params.chat_id, None, params.limit).await;
        outcome.map(Json).map_err(|error| tool_error(&error))
    }

    /// Search documents in a chat by file name.
    #[tool(
        name = "telegram_search_files",
        description = "Search documents in a Telegram chat by file name. Requires the worker to be running."
    )]
    async fn telegram_search_files(
        &self,
        Parameters(params): Parameters<SearchFilesParams>,
    ) -> std::result::Result<Json<ScanOut>, String> {
        let outcome = self
            .scan_files(params.chat_id, Some(params.query), params.limit)
            .await;
        outcome.map(Json).map_err(|error| tool_error(&error))
    }

    /// Enqueue one Telegram file for background download.
    #[tool(
        name = "telegram_download",
        description = "Enqueue one Telegram document for background download. Returns immediately; the worker performs the actual download with resume support. Exactly one of `url` or (`chat_id` and `message_id`) is required."
    )]
    async fn telegram_download(
        &self,
        Parameters(params): Parameters<DownloadParams>,
    ) -> std::result::Result<Json<DownloadJobOut>, String> {
        let outcome: CoreResult<DownloadJobOut> = async {
            let (url, chat_id, message_id) = match (&params.url, params.chat_id, params.message_id)
            {
                (Some(url), None, None) => {
                    let trimmed = url.trim();
                    if trimmed.is_empty() {
                        return Err(Error::InvalidInput("url must not be empty".into()));
                    }
                    (Some(trimmed.to_string()), None, None)
                }
                (None, Some(chat), Some(message)) => (None, Some(chat), Some(message)),
                (None, None, None) => {
                    return Err(Error::InvalidInput(
                        "either url or (chat_id and message_id) is required".into(),
                    ));
                }
                _ => {
                    return Err(Error::InvalidInput(
                        "pass either url alone, or chat_id with message_id".into(),
                    ));
                }
            };

            if let Some(url) = &url {
                tgdlrs_core::telegram::parse_link(url)?;
            }

            let sha256 = params.sha256.as_deref().map(validate_sha256).transpose()?;

            let config = self.require_config()?.clone();
            let download_dir = resolve_download_dir(&config, params.download_dir.as_deref())?;
            std::fs::create_dir_all(&download_dir)?;

            let file_name = params
                .file_name
                .as_deref()
                .map(str::trim)
                .filter(|name| !name.is_empty())
                .map(str::to_string)
                .unwrap_or_else(|| {
                    placeholder_file_name(params.url.as_deref(), chat_id, message_id)
                });

            let reused = self
                .store
                .find_active(url.as_deref(), chat_id, message_id)?
                .is_some();

            let mut job = NewDownload::new(file_name, download_dir, config.max_attempts);
            job.source_url = url;
            job.chat_id = chat_id;
            job.message_id = message_id;
            job.sha256_expected = sha256;
            job.priority = params.priority.unwrap_or(0);
            job.source = "mcp".into();
            job.verify = params.verify.unwrap_or(config.verify_sha256);
            let download = self.store.insert(job)?;

            let (worker_reachable, worker_started) = self.ensure_worker().await;
            Ok(self.job_out(&download, reused, worker_reachable, worker_started))
        }
        .await;
        outcome.map(Json).map_err(|error| tool_error(&error))
    }

    /// Enqueue many Telegram files for background download.
    #[tool(
        name = "telegram_batch_download",
        description = "Enqueue up to 200 t.me message links for background download. Returns per-link results; failures do not abort the batch."
    )]
    async fn telegram_batch_download(
        &self,
        Parameters(params): Parameters<BatchDownloadParams>,
    ) -> std::result::Result<Json<BatchOut>, String> {
        let outcome: CoreResult<BatchOut> = async {
            if params.urls.is_empty() {
                return Err(Error::InvalidInput("urls must not be empty".into()));
            }
            if params.urls.len() > 200 {
                return Err(Error::InvalidInput("at most 200 urls per batch".into()));
            }
            let config = self.require_config()?.clone();
            let download_dir = resolve_download_dir(&config, params.download_dir.as_deref())?;
            std::fs::create_dir_all(&download_dir)?;

            let mut created = Vec::new();
            let mut failed = Vec::new();
            let mut reused_count = 0usize;
            for url in &params.urls {
                let trimmed = url.trim().to_string();
                let single = DownloadParams {
                    url: Some(trimmed.clone()),
                    chat_id: None,
                    message_id: None,
                    file_name: None,
                    sha256: None,
                    priority: params.priority,
                    download_dir: Some(download_dir.to_string_lossy().into_owned()),
                    verify: None,
                };
                match self.enqueue(single, &config).await {
                    Ok((job, reused)) => {
                        if reused {
                            reused_count += 1;
                        }
                        created.push(job);
                    }
                    Err(error) => failed.push(BatchFailure {
                        url: trimmed,
                        error: error.to_string(),
                    }),
                }
            }
            let (worker_reachable, worker_started) = self.ensure_worker().await;
            for job in created.iter_mut() {
                job.worker_reachable = worker_reachable;
                job.worker_started = worker_started;
            }
            Ok(BatchOut {
                requested: params.urls.len(),
                created_count: created.len(),
                reused_count,
                created,
                failed,
                worker_started,
            })
        }
        .await;
        outcome.map(Json).map_err(|error| tool_error(&error))
    }

    /// List downloads in the queue and history.
    #[tool(
        name = "telegram_list_downloads",
        description = "List downloads known to the queue, optionally filtered by state. Newest activity first."
    )]
    async fn telegram_list_downloads(
        &self,
        Parameters(params): Parameters<ListDownloadsParams>,
    ) -> std::result::Result<Json<ListOut>, String> {
        let outcome: CoreResult<ListOut> = (|| {
            let state = params
                .state
                .as_deref()
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(DownloadState::from_str)
                .transpose()?;
            let limit = params.limit.unwrap_or(50).clamp(1, 500);
            let downloads = self
                .store
                .list(state, limit)?
                .iter()
                .map(Self::summary_out)
                .collect::<Vec<_>>();
            Ok(ListOut {
                returned: downloads.len(),
                downloads,
            })
        })();
        outcome.map(Json).map_err(|error| tool_error(&error))
    }

    /// Show one download job in detail.
    #[tool(
        name = "telegram_download_status",
        description = "Show one download job: state, bytes, attempts, hashes, paths and timestamps. Accepts a full id or unique prefix."
    )]
    async fn telegram_download_status(
        &self,
        Parameters(params): Parameters<DownloadStatusParams>,
    ) -> std::result::Result<Json<DownloadSummaryOut>, String> {
        let outcome: CoreResult<DownloadSummaryOut> =
            find_download(&self.store, &params.id).map(|download| Self::summary_out(&download));
        outcome.map(Json).map_err(|error| tool_error(&error))
    }

    /// Pause a download.
    #[tool(
        name = "telegram_pause_download",
        description = "Pause a queued/running download. The worker observes the pause within seconds and keeps the partial file."
    )]
    async fn telegram_pause_download(
        &self,
        Parameters(params): Parameters<IdParams>,
    ) -> std::result::Result<Json<ActionOut>, String> {
        self.transition(&params.id, Action::Pause)
            .await
            .map(Json)
            .map_err(|error| tool_error(&error))
    }

    /// Resume a paused download.
    #[tool(
        name = "telegram_resume_download",
        description = "Resume a paused download from its partial file."
    )]
    async fn telegram_resume_download(
        &self,
        Parameters(params): Parameters<IdParams>,
    ) -> std::result::Result<Json<ActionOut>, String> {
        self.transition(&params.id, Action::Resume)
            .await
            .map(Json)
            .map_err(|error| tool_error(&error))
    }

    /// Cancel a download.
    #[tool(
        name = "telegram_cancel_download",
        description = "Cancel a download. The partial file is kept on disk; the job moves to CANCELED and can be re-queued with telegram_retry_download."
    )]
    async fn telegram_cancel_download(
        &self,
        Parameters(params): Parameters<IdParams>,
    ) -> std::result::Result<Json<ActionOut>, String> {
        self.transition(&params.id, Action::Cancel)
            .await
            .map(Json)
            .map_err(|error| tool_error(&error))
    }

    /// Retry a failed, canceled or paused download.
    #[tool(
        name = "telegram_retry_download",
        description = "Re-queue a failed, canceled or paused download. Attempt counter and error are reset; the partial file is kept and resumed."
    )]
    async fn telegram_retry_download(
        &self,
        Parameters(params): Parameters<IdParams>,
    ) -> std::result::Result<Json<ActionOut>, String> {
        self.transition(&params.id, Action::Retry)
            .await
            .map(Json)
            .map_err(|error| tool_error(&error))
    }

    /// Verify size and SHA-256 of a downloaded file.
    #[tool(
        name = "telegram_verify_file",
        description = "Verify the size and SHA-256 hash of a downloaded file. Pass either a job id (defaults to recorded expectations) or an explicit path (with optional expected size/hash)."
    )]
    async fn telegram_verify_file(
        &self,
        Parameters(params): Parameters<VerifyParams>,
    ) -> std::result::Result<Json<VerifyOut>, String> {
        let sha256_override = match &params.sha256 {
            Some(value) => Some(validate_sha256(value).map_err(|error| tool_error(&error))?),
            None => None,
        };
        let outcome: CoreResult<VerifyOut> = (|| {
            if let Some(id) = &params.id {
                let verified = verification::verify_job(
                    &self.store,
                    id,
                    params.expected_size,
                    sha256_override.as_deref(),
                )?;
                let report = verified.report;
                return Ok(VerifyOut {
                    id: Some(verified.job_id),
                    path: report.path.to_string_lossy().into_owned(),
                    exists: report.exists,
                    size_ok: report.size_ok,
                    hash_ok: report.hash_ok,
                    ok: report.ok,
                    expected_size: report.expected_size,
                    actual_size: report.actual_size,
                    sha256_expected: report.sha256_expected,
                    sha256_actual: report.sha256_actual,
                    recorded: true,
                });
            }
            let Some(path) = params.path.as_deref() else {
                return Err(Error::InvalidInput("pass either id or path".into()));
            };
            let path = PathBuf::from(path);
            let report =
                verification::verify_file(&path, params.expected_size, sha256_override.as_deref())?;
            Ok(VerifyOut {
                id: None,
                path: path.to_string_lossy().into_owned(),
                exists: report.exists,
                size_ok: report.size_ok,
                hash_ok: report.hash_ok,
                ok: report.ok,
                expected_size: report.expected_size,
                actual_size: report.actual_size,
                sha256_expected: report.sha256_expected,
                sha256_actual: report.sha256_actual,
                recorded: false,
            })
        })();
        outcome.map(Json).map_err(|error| tool_error(&error))
    }

    /// Find missing numbered parts (e.g. archive volumes) in a directory.
    #[tool(
        name = "telegram_find_missing_parts",
        description = "Scan a directory for numbered files like `CHINT.7z.001` and report which indices are missing between `start` and `end`."
    )]
    async fn telegram_find_missing_parts(
        &self,
        Parameters(params): Parameters<MissingPartsParams>,
    ) -> std::result::Result<Json<MissingPartsOut>, String> {
        let outcome: CoreResult<MissingPartsOut> = (|| {
            let config = self.require_config()?;
            let directory = match params
                .directory
                .as_deref()
                .map(str::trim)
                .filter(|value| !value.is_empty())
            {
                Some(path) => {
                    let candidate = PathBuf::from(path);
                    if candidate.is_absolute() {
                        candidate
                    } else {
                        config.download_dir.join(candidate)
                    }
                }
                None => config.download_dir.clone(),
            };
            if !directory.is_dir() {
                return Err(Error::NotFound(format!(
                    "directory {}",
                    directory.display()
                )));
            }
            let prefix = params.prefix.trim().to_string();
            if prefix.is_empty() {
                return Err(Error::InvalidInput("prefix must not be empty".into()));
            }
            let padding = params.padding.unwrap_or(3) as usize;
            if !(1..=8).contains(&padding) {
                return Err(Error::InvalidInput(
                    "padding must be between 1 and 8".into(),
                ));
            }
            let start = params.start.unwrap_or(1);

            let mut found = Vec::new();
            for entry in std::fs::read_dir(&directory)? {
                let entry = entry?;
                let name = entry.file_name().to_string_lossy().into_owned();
                let Some(rest) = name.strip_prefix(&prefix) else {
                    continue;
                };
                if rest.len() != padding || !rest.bytes().all(|b| b.is_ascii_digit()) {
                    continue;
                }
                let Ok(index) = rest.parse::<u32>() else {
                    continue;
                };
                let size_bytes = entry.metadata().map(|m| m.len()).unwrap_or(0);
                found.push(FoundPart {
                    index,
                    name,
                    size_bytes,
                });
            }
            found.sort_by_key(|part| part.index);

            let end = params
                .end
                .unwrap_or_else(|| found.iter().map(|part| part.index).max().unwrap_or(start));
            if start > end {
                return Err(Error::InvalidInput(
                    "start must not be greater than end".into(),
                ));
            }

            let present: HashSet<u32> = found
                .iter()
                .filter(|part| part.index >= start && part.index <= end)
                .map(|part| part.index)
                .collect();
            let missing: Vec<u32> = (start..=end)
                .filter(|index| !present.contains(index))
                .collect();
            let in_range: Vec<&FoundPart> = found
                .iter()
                .filter(|part| part.index >= start && part.index <= end)
                .collect();
            let total_bytes = in_range.iter().map(|part| part.size_bytes).sum();
            Ok(MissingPartsOut {
                directory: directory.to_string_lossy().into_owned(),
                prefix,
                checked_from: start,
                checked_to: end,
                found: found
                    .into_iter()
                    .filter(|part| part.index >= start && part.index <= end)
                    .collect(),
                missing: missing.clone(),
                complete: missing.is_empty(),
                total_bytes,
            })
        })();
        outcome.map(Json).map_err(|error| tool_error(&error))
    }

    /// Run system diagnostics for this installation.
    #[tool(
        name = "telegram_system_diagnostics",
        description = "Report configuration, filesystem, disk space, SQLite, worker and session status. Never includes credentials."
    )]
    async fn telegram_system_diagnostics(
        &self,
    ) -> std::result::Result<Json<DiagnosticsOut>, String> {
        let outcome: CoreResult<DiagnosticsOut> = async {
            let config_error = Config::load_or_create(&self.paths)
                .err()
                .map(|error| error.to_string());
            let config = self.config.clone();
            let download_dir_writable = config
                .as_ref()
                .map(|cfg| {
                    let probe = cfg.download_dir.join(".tgdlrs-write-probe");
                    std::fs::write(&probe, b"ok").is_ok() && std::fs::remove_file(&probe).is_ok()
                })
                .unwrap_or(false);
            let disk_free_bytes = config
                .as_ref()
                .map(|cfg| storage::free_space(&cfg.download_dir).unwrap_or(0))
                .unwrap_or(0);
            let disk_reserve_bytes = config
                .as_ref()
                .map(|cfg| cfg.disk_reserve_bytes)
                .unwrap_or(0);

            let (database_ok, sqlite_version, database_error, verified, failed, total) = {
                match self.store.summary() {
                    Ok(summary) => (
                        true,
                        Some(self.store.library_version()),
                        None,
                        summary.verified,
                        summary.failed,
                        summary.total,
                    ),
                    Err(error) => (false, None, Some(error.to_string()), 0, 0, 0),
                }
            };

            let worker_reachable = ipc::ping(&self.paths.pipe_name).await;
            let (worker_running, worker_authorized, worker_last_error) = if worker_reachable {
                let status: CoreResult<tgdlrs_core::models::WorkerStatus> = tokio::time::timeout(
                    std::time::Duration::from_secs(5),
                    ipc::request(&self.paths.pipe_name, &Request::Status),
                )
                .await
                .map_err(|_| Error::Ipc("worker did not answer in time".into()))
                .and_then(|result| result);
                match status {
                    Ok(worker) => (worker.running, worker.authorized, worker.last_error),
                    Err(error) => (true, false, Some(error.to_string())),
                }
            } else {
                (false, false, None)
            };

            let (max_concurrent_files, verify_sha256, skip_verified, keep_partials, auto_start) =
                config
                    .as_ref()
                    .map(|cfg| {
                        (
                            cfg.max_concurrent_files,
                            cfg.verify_sha256,
                            cfg.skip_verified,
                            cfg.keep_partials,
                            cfg.auto_start_worker,
                        )
                    })
                    .unwrap_or((1, true, true, true, true));

            Ok(DiagnosticsOut {
                version: env!("CARGO_PKG_VERSION").to_string(),
                platform: std::env::consts::OS.to_string(),
                home: self.paths.home.to_string_lossy().into_owned(),
                config_file: self.paths.config_file.to_string_lossy().into_owned(),
                database: self.paths.database.to_string_lossy().into_owned(),
                session_file: self.paths.session.to_string_lossy().into_owned(),
                download_dir: config
                    .as_ref()
                    .map(|cfg| cfg.download_dir.to_string_lossy().into_owned())
                    .unwrap_or_default(),
                download_dir_writable,
                disk_free_bytes,
                disk_reserve_bytes,
                sqlite_version,
                database_ok,
                database_error,
                worker_reachable,
                worker_running,
                worker_authorized,
                worker_last_error,
                session_present: self.paths.session.exists(),
                max_concurrent_files,
                verify_sha256,
                skip_verified,
                keep_partials,
                auto_start_worker: auto_start,
                total_jobs: total,
                verified_jobs: verified,
                failed_jobs: failed,
                config_error,
            })
        }
        .await;
        outcome.map(Json).map_err(|error| tool_error(&error))
    }
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for DownloadMcp {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new("tgdlrs-mcp", env!("CARGO_PKG_VERSION")))
            .with_instructions(
                "Telegram Downloader RS: enqueues and controls resumable Telegram downloads. \
                 Downloads never run inside this server; they run in the tgdlrs-worker process. \
                 Pause/resume/cancel/retry/verify operate on queued jobs immediately. \
                 Scan/search/resolve require the worker and an authorized Telegram session. \
                 Errors are JSON objects with `kind` and `message` fields.",
            )
    }
}

impl DownloadMcp {
    async fn scan_files(
        &self,
        chat_id: i64,
        query: Option<String>,
        limit: Option<usize>,
    ) -> CoreResult<ScanOut> {
        let capped = limit.unwrap_or(500).clamp(1, 10_000);
        let request = match query {
            Some(query) => Request::SearchFiles {
                chat_id,
                query,
                limit: Some(capped),
            },
            None => Request::ScanChat {
                chat_id,
                limit: Some(capped),
            },
        };
        let files: Vec<FileEntry> = tokio::time::timeout(
            std::time::Duration::from_secs(120),
            ipc::request(&self.paths.pipe_name, &request),
        )
        .await
        .map_err(|_| Error::Ipc("worker did not answer in time".into()))??;
        let mut files = files;
        self.annotate_downloaded(&mut files);
        Ok(ScanOut {
            chat_id,
            count: files.len(),
            files: files.iter().map(Self::file_entry_out).collect(),
        })
    }

    async fn enqueue(
        &self,
        params: DownloadParams,
        config: &Config,
    ) -> CoreResult<(DownloadJobOut, bool)> {
        let outcome: CoreResult<(DownloadJobOut, bool)> = async {
            let url = params
                .url
                .as_deref()
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(str::to_string);
            if let Some(url) = &url {
                tgdlrs_core::telegram::parse_link(url)?;
            }
            let sha256 = params.sha256.as_deref().map(validate_sha256).transpose()?;
            let download_dir = resolve_download_dir(config, params.download_dir.as_deref())?;
            std::fs::create_dir_all(&download_dir)?;

            let file_name = params
                .file_name
                .as_deref()
                .map(str::trim)
                .filter(|name| !name.is_empty())
                .map(str::to_string)
                .unwrap_or_else(|| placeholder_file_name(params.url.as_deref(), None, None));

            let reused = self
                .store
                .find_active(url.as_deref(), None, None)?
                .is_some();
            let mut job = NewDownload::new(file_name, download_dir, config.max_attempts);
            job.source_url = url;
            job.sha256_expected = sha256;
            job.priority = params.priority.unwrap_or(0);
            job.source = "mcp".into();
            job.verify = params.verify.unwrap_or(config.verify_sha256);
            let download = self.store.insert(job)?;
            Ok((self.job_out(&download, reused, false, false), reused))
        }
        .await;
        outcome
    }

    async fn transition(&self, id: &str, action: Action) -> CoreResult<ActionOut> {
        let download = find_download(&self.store, id)?;
        let (changed, verb) = match action {
            Action::Pause => (
                self.store.pause(&download.id)?,
                "pause requested; the worker observes it within seconds",
            ),
            Action::Resume => (
                self.store.resume(&download.id)?,
                "resumed; the worker will pick the job up",
            ),
            Action::Cancel => (
                self.store.cancel(&download.id)?,
                "canceled; the partial file is kept",
            ),
            Action::Retry => (
                self.store.retry(&download.id)?,
                "re-queued; the worker will resume from the partial file",
            ),
        };
        let fresh = self.store.get(&download.id)?;
        Ok(ActionOut {
            id: fresh.id,
            state: fresh.state.as_str().to_string(),
            changed,
            message: Some(if changed {
                verb.to_string()
            } else {
                "the job was not in a state that accepts this action".to_string()
            }),
        })
    }
}

enum Action {
    Pause,
    Resume,
    Cancel,
    Retry,
}

/// Run the MCP server on stdio until the client closes the pipe.
pub async fn serve_stdio() -> CoreResult<()> {
    let server = DownloadMcp::new()?;
    let service = server
        .serve(rmcp::transport::stdio())
        .await
        .map_err(|error| Error::Ipc(format!("cannot start the MCP server: {error}")))?;
    service
        .waiting()
        .await
        .map_err(|error| Error::Ipc(format!("MCP server stopped with an error: {error}")))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tgdlrs_core::config::Config;
    use tgdlrs_core::database::Store;

    fn test_env() -> (DownloadMcp, tempfile::TempDir) {
        let home = tempfile::tempdir().unwrap();
        let root = home.path().to_path_buf();
        let paths = Paths {
            home: root.clone(),
            config_file: root.join("config.json"),
            database: root.join("state.db"),
            session: root.join("telegram.session.json"),
            log_dir: root.join("logs"),
            lock_file: root.join("worker.lock"),
            pipe_name: format!(r"\\.\pipe\tgdlrs-test-{}", std::process::id()),
        };
        paths.ensure().unwrap();
        let config = Config {
            download_dir: root.join("downloads"),
            ..Config::default()
        };
        config.save(&paths).unwrap();
        std::fs::create_dir_all(&config.download_dir).unwrap();
        let store = Arc::new(Store::open(&paths).unwrap());
        (DownloadMcp::for_test(paths, config, store), home)
    }

    fn download_params(url: Option<&str>) -> DownloadParams {
        DownloadParams {
            url: url.map(str::to_string),
            chat_id: None,
            message_id: None,
            file_name: None,
            sha256: None,
            priority: None,
            download_dir: None,
            verify: None,
        }
    }

    #[tokio::test]
    async fn find_missing_parts_reports_gaps() {
        let (server, _home) = test_env();
        let dir = server.config.as_ref().unwrap().download_dir.clone();
        std::fs::write(dir.join("CHINT.7z.001"), b"a").unwrap();
        std::fs::write(dir.join("CHINT.7z.002"), b"b").unwrap();
        std::fs::write(dir.join("CHINT.7z.004"), vec![0u8; 100]).unwrap();
        std::fs::write(dir.join("CHINT.7z.006"), b"f").unwrap();
        std::fs::write(dir.join("CHINT.7z.other"), b"x").unwrap();

        let report = server
            .telegram_find_missing_parts(Parameters(MissingPartsParams {
                directory: None,
                prefix: "CHINT.7z.".into(),
                start: Some(1),
                end: None,
                padding: None,
            }))
            .await
            .unwrap()
            .0;
        assert_eq!(report.checked_from, 1);
        assert_eq!(report.checked_to, 6);
        assert_eq!(report.found.len(), 4);
        assert_eq!(report.missing, vec![3, 5]);
        assert!(!report.complete);
        assert_eq!(report.total_bytes, 103);
    }

    #[tokio::test]
    async fn find_missing_parts_complete_when_all_present() {
        let (server, _home) = test_env();
        let dir = server.config.as_ref().unwrap().download_dir.clone();
        for index in 1..=3u32 {
            std::fs::write(dir.join(format!("data.7z.{index:03}")), b"x").unwrap();
        }
        let report = server
            .telegram_find_missing_parts(Parameters(MissingPartsParams {
                directory: None,
                prefix: "data.7z.".into(),
                start: Some(1),
                end: Some(3),
                padding: None,
            }))
            .await
            .unwrap()
            .0;
        assert!(report.complete);
        assert!(report.missing.is_empty());
        assert_eq!(report.found.len(), 3);
    }

    #[tokio::test]
    async fn download_lifecycle_through_tools() {
        let (server, _home) = test_env();
        let queued = server
            .telegram_download(Parameters(download_params(Some(
                "https://t.me/c/1188194766/1914",
            ))))
            .await
            .unwrap()
            .0;
        assert_eq!(queued.state, "QUEUED");
        assert!(!queued.reused_existing);
        // The test server never spawns the worker and the pipe does not exist.
        assert!(!queued.worker_reachable);
        assert!(!queued.worker_started);

        let again = server
            .telegram_download(Parameters(download_params(Some(
                "https://t.me/c/1188194766/1914",
            ))))
            .await
            .unwrap()
            .0;
        assert!(again.reused_existing);
        assert_eq!(again.id, queued.id);

        let list = server
            .telegram_list_downloads(Parameters(ListDownloadsParams {
                state: None,
                limit: Some(10),
            }))
            .await
            .unwrap()
            .0;
        assert_eq!(list.returned, 1);

        let paused = server
            .telegram_pause_download(Parameters(IdParams {
                id: queued.id[..8].to_string(),
            }))
            .await
            .unwrap()
            .0;
        assert!(paused.changed);
        assert_eq!(paused.state, "PAUSED");

        let resumed = server
            .telegram_resume_download(Parameters(IdParams {
                id: queued.id.clone(),
            }))
            .await
            .unwrap()
            .0;
        assert!(resumed.changed);
        assert_eq!(resumed.state, "QUEUED");

        let canceled = server
            .telegram_cancel_download(Parameters(IdParams {
                id: queued.id.clone(),
            }))
            .await
            .unwrap()
            .0;
        assert_eq!(canceled.state, "CANCELED");

        let retried = server
            .telegram_retry_download(Parameters(IdParams {
                id: queued.id.clone(),
            }))
            .await
            .unwrap()
            .0;
        assert_eq!(retried.state, "QUEUED");

        let status = server
            .telegram_download_status(Parameters(DownloadStatusParams {
                id: queued.id.clone(),
            }))
            .await
            .unwrap()
            .0;
        assert_eq!(status.state, "QUEUED");
        assert_eq!(
            status.source_url.as_deref(),
            Some("https://t.me/c/1188194766/1914")
        );
    }

    #[tokio::test]
    async fn download_rejects_ambiguous_arguments() {
        let (server, _home) = test_env();
        let params = DownloadParams {
            url: Some("https://t.me/c/1188194766/1914".into()),
            chat_id: Some(-1001188194766),
            message_id: Some(1914),
            file_name: None,
            sha256: None,
            priority: None,
            download_dir: None,
            verify: None,
        };
        let error = server
            .telegram_download(Parameters(params))
            .await
            .err()
            .expect("the call must fail");
        assert!(error.contains("invalid_input"));

        let error = server
            .telegram_download(Parameters(download_params(None)))
            .await
            .err()
            .expect("the call must fail");
        assert!(error.contains("invalid_input"));
    }

    #[tokio::test]
    async fn verify_tool_records_and_fails_job() {
        let (server, _home) = test_env();
        let dir = server.config.as_ref().unwrap().download_dir.clone();
        let final_path = dir.join("payload.bin");
        std::fs::write(&final_path, b"abc").unwrap();
        let sha = tgdlrs_core::verification::sha256_file(&final_path).unwrap();

        let mut job = tgdlrs_core::models::NewDownload::new("payload.bin", dir, 3);
        job.file_size = Some(3);
        job.sha256_expected = Some(sha);
        job.source_url = Some("https://t.me/c/1188194766/1914".into());
        let inserted = server.store.insert(job).unwrap();

        let outcome = server
            .telegram_verify_file(Parameters(VerifyParams {
                id: Some(inserted.id.clone()),
                path: None,
                expected_size: None,
                sha256: None,
            }))
            .await
            .unwrap()
            .0;
        assert!(outcome.ok);
        assert!(outcome.recorded);
        assert_eq!(
            server.store.get(&inserted.id).unwrap().state,
            DownloadState::Verified
        );

        // Corrupt the file: verification now fails and the job becomes FAILED.
        std::fs::write(&final_path, b"abd").unwrap();
        let outcome = server
            .telegram_verify_file(Parameters(VerifyParams {
                id: Some(inserted.id.clone()),
                path: None,
                expected_size: None,
                sha256: None,
            }))
            .await
            .unwrap()
            .0;
        assert!(!outcome.ok);
        assert_eq!(
            server.store.get(&inserted.id).unwrap().state,
            DownloadState::Failed
        );
    }

    #[tokio::test]
    async fn verify_rejects_bad_sha256_input() {
        let (server, _home) = test_env();
        let error = server
            .telegram_verify_file(Parameters(VerifyParams {
                id: None,
                path: Some("nope.bin".into()),
                expected_size: None,
                sha256: Some("zz".into()),
            }))
            .await
            .err()
            .expect("the call must fail");
        assert!(error.contains("invalid_input"));
    }

    #[tokio::test]
    async fn diagnostics_reports_without_secrets() {
        let (server, _home) = test_env();
        let diag = server.telegram_system_diagnostics().await.unwrap().0;
        assert!(diag.database_ok);
        assert!(diag.download_dir_writable);
        assert!(!diag.worker_reachable);
        assert!(!diag.session_present);
        let json = serde_json::to_string(&diag).unwrap();
        assert!(!json.contains("api_hash"));
        assert!(!json.contains("api_id"));
    }
}
