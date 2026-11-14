//! tgdlrs: command line interface for Telegram Downloader RS.
//!
//! The CLI never downloads anything itself; downloads run in the
//! `tgdlrs-worker` background process. Commands here either query/control
//! the worker (named pipe), or the SQLite state database directly.

mod autostart;

use std::collections::HashSet;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use clap::{Parser, Subcommand};

use tgdlrs_core::config::{Config, Paths};
use tgdlrs_core::database::Store;
use tgdlrs_core::error::{Error, Result};
use tgdlrs_core::ipc::{self, Request};
use tgdlrs_core::launch;
use tgdlrs_core::models::{DownloadState, FileEntry, NewDownload, VerifyReport, WorkerStatus};
use tgdlrs_core::storage;
use tgdlrs_core::telegram::{self, SignInOutcome};
use tgdlrs_core::verification;

#[derive(Parser)]
#[command(
    name = "tgdlrs",
    version,
    about = "Resumable Telegram downloads with a background worker",
    long_about = "Telegram Downloader RS: queue, pause, resume, verify and inspect \
                  resumable Telegram downloads. Works alongside the background \
                  worker (tgdlrs-worker) and the bundled MCP server."
)]
struct Cli {
    /// Emit machine-readable JSON.
    #[arg(long, global = true)]
    json: bool,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Sign in to Telegram interactively (OTP and optional 2FA password).
    Login {
        /// Re-authenticate even if the session looks valid.
        #[arg(long)]
        force: bool,
        /// Phone number, e.g. +1234567890. Defaults to the saved config value.
        #[arg(long)]
        phone: Option<String>,
    },
    /// Enqueue a t.me message link for background download.
    Download {
        /// t.me message link (public or private /c/ link).
        url: String,
        /// Store the job under this file name (until Telegram resolves the real one).
        #[arg(long)]
        file_name: Option<String>,
        /// Expected SHA-256 hex digest; verified after download.
        #[arg(long)]
        sha256: Option<String>,
        /// Queue priority (higher runs first).
        #[arg(long)]
        priority: Option<i64>,
        /// Override the download directory for this job.
        #[arg(long)]
        download_dir: Option<PathBuf>,
        /// Skip SHA-256 verification for this job.
        #[arg(long)]
        no_verify: bool,
        /// Do not start the background worker automatically.
        #[arg(long)]
        no_start: bool,
    },
    /// List downloadable documents in a chat (newest first).
    Scan {
        /// Bot API chat id (-100...), channel id, or a t.me link.
        chat: String,
        /// Maximum number of messages to inspect (default 500, cap 10000).
        #[arg(long)]
        limit: Option<usize>,
    },
    /// Show worker status and queue summary.
    Status,
    /// List downloads, optionally filtered by state.
    List {
        /// State filter: QUEUED, DOWNLOADING, PAUSED, RETRY_WAIT, COMPLETED,
        /// VERIFYING, VERIFIED, FAILED, CANCELED.
        #[arg(long)]
        state: Option<String>,
        /// Maximum number of rows (default 50, cap 500).
        #[arg(long)]
        limit: Option<usize>,
    },
    /// Pause a queued/running download.
    Pause {
        /// Download id (full or unique prefix).
        id: String,
    },
    /// Resume a paused download.
    Resume {
        /// Download id (full or unique prefix).
        id: String,
    },
    /// Cancel a download (the partial file is kept).
    Cancel {
        /// Download id (full or unique prefix).
        id: String,
    },
    /// Re-queue a failed, canceled or paused download from its partial file.
    Retry {
        /// Download id (full or unique prefix).
        id: String,
    },
    /// Verify size and SHA-256 of a downloaded file (or job).
    Verify {
        /// Download id (full or unique prefix) or a filesystem path.
        target: String,
        /// Force interpreting the target as a filesystem path.
        #[arg(long)]
        path: bool,
        /// Expected size in bytes.
        #[arg(long)]
        expected_size: Option<u64>,
        /// Expected SHA-256 hex digest.
        #[arg(long)]
        sha256: Option<String>,
    },
    /// Run installation diagnostics.
    Doctor,
    /// Manage the background worker and its autostart entry.
    Daemon {
        #[command(subcommand)]
        command: DaemonCommand,
    },
    /// Run the bundled MCP server on stdio.
    Mcp,
}

#[derive(Subcommand)]
enum DaemonCommand {
    /// Start the background worker (detached, no console window).
    Start,
    /// Ask the worker to stop gracefully.
    Stop {
        /// Seconds to wait for the pipe to disappear (default 15).
        #[arg(long)]
        wait: Option<u64>,
    },
    /// Restart the background worker.
    Restart,
    /// Show worker status only.
    Status,
    /// Register tgdlrs-worker to start at Windows login (current user).
    InstallAutostart,
    /// Remove the Windows login autostart entry.
    RemoveAutostart,
}

#[tokio::main]
async fn main() -> ExitCode {
    match Cli::parse().run().await {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("error: {error}");
            if matches!(error, Error::NotAuthorized) {
                eprintln!("hint: run `tgdlrs login`");
            }
            ExitCode::FAILURE
        }
    }
}

impl Cli {
    async fn run(self) -> Result<()> {
        let json = self.json;
        match self.command {
            Command::Login { force, phone } => cmd_login(force, phone, json).await,
            Command::Download {
                url,
                file_name,
                sha256,
                priority,
                download_dir,
                no_verify,
                no_start,
            } => {
                cmd_download(
                    &url,
                    file_name,
                    sha256,
                    priority,
                    download_dir,
                    no_verify,
                    no_start,
                    json,
                )
                .await
            }
            Command::Scan { chat, limit } => cmd_scan(&chat, limit, json).await,
            Command::Status => cmd_status(json).await,
            Command::List { state, limit } => cmd_list(state, limit, json),
            Command::Pause { id } => cmd_action(&id, Action::Pause, json).await,
            Command::Resume { id } => cmd_action(&id, Action::Resume, json).await,
            Command::Cancel { id } => cmd_action(&id, Action::Cancel, json).await,
            Command::Retry { id } => cmd_action(&id, Action::Retry, json).await,
            Command::Verify {
                target,
                path,
                expected_size,
                sha256,
            } => cmd_verify(&target, path, expected_size, sha256, json),
            Command::Doctor => cmd_doctor(json).await,
            Command::Daemon { command } => cmd_daemon(command, json).await,
            Command::Mcp => tgdlrs_mcp::serve_stdio().await,
        }
    }
}

// ------------------------------------------------------------- helpers --

fn load() -> Result<(Paths, Config)> {
    let paths = Paths::discover()?;
    paths.ensure()?;
    let config = Config::load_or_create(&paths)?;
    Ok((paths, config))
}

fn open_store(paths: &Paths) -> Result<Store> {
    Store::open(paths)
}

fn validate_sha256(value: &str) -> Result<String> {
    let normalized = value.trim().to_ascii_lowercase();
    if normalized.len() == 64 && normalized.chars().all(|c| c.is_ascii_hexdigit()) {
        Ok(normalized)
    } else {
        Err(Error::InvalidInput(
            "sha256 must be 64 hexadecimal characters".into(),
        ))
    }
}

fn human_bytes(bytes: u64) -> String {
    const UNITS: &[&str] = &["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = bytes as f64;
    let mut unit = 0usize;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

fn print_json(value: &impl serde::Serialize) -> Result<()> {
    println!("{}", serde_json::to_string_pretty(value)?);
    Ok(())
}

fn prompt(prompt: &str) -> Result<String> {
    print!("{prompt}: ");
    std::io::stdout().flush()?;
    let mut line = String::new();
    std::io::stdin().read_line(&mut line)?;
    Ok(line.trim().to_string())
}

/// Ask on an interactive terminal; piped stdin cannot do hidden input.
fn prompt_hidden(prompt: &str) -> Result<String> {
    if std::io::IsTerminal::is_terminal(&std::io::stdin()) {
        rpassword::prompt_password(format!("{prompt}: "))
            .map_err(|error| Error::InvalidInput(format!("cannot read hidden input: {error}")))
    } else {
        Err(Error::InvalidInput(
            "this step needs an interactive terminal (no echo); run tgdlrs in a real console"
                .into(),
        ))
    }
}

async fn stop_worker(paths: &Paths) {
    if !ipc::ping(&paths.pipe_name).await {
        return;
    }
    eprintln!("stopping background worker...");
    let _ = ipc::request_value(&paths.pipe_name, &Request::Stop).await;
    for _ in 0..30 {
        if !ipc::ping(&paths.pipe_name).await {
            return;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    eprintln!("warning: worker still answers the pipe after 15s; continuing anyway");
}

fn annotate_downloaded(store: &Store, files: &mut [FileEntry]) {
    let mut known: HashSet<(String, u64)> = HashSet::new();
    if let Ok(completed) = store.completed_files() {
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

fn parse_state(value: &str) -> Result<DownloadState> {
    value.parse::<DownloadState>()
}

enum Action {
    Pause,
    Resume,
    Cancel,
    Retry,
}

// ------------------------------------------------------------ commands --

async fn cmd_login(force: bool, phone: Option<String>, _json: bool) -> Result<()> {
    let (paths, mut config) = load()?;
    let worker_was_running = ipc::ping(&paths.pipe_name).await;

    // Fail fast on configuration errors before touching the worker.
    let credentials = config.credentials()?;

    if worker_was_running && !force {
        let status: WorkerStatus = ipc::request(&paths.pipe_name, &Request::Status).await?;
        if status.authorized {
            let user = status.user.unwrap_or_else(|| "unknown user".into());
            println!("Already authorized as {user}.");
            println!("Use `tgdlrs login --force` to sign in again.");
            return Ok(());
        }
    }

    if worker_was_running {
        stop_worker(&paths).await;
    }
    if force && paths.session.exists() {
        let stamp = chrono::Utc::now().format("%Y%m%d-%H%M%S");
        let backup = paths.session.with_file_name(format!(
            "{}.bak-{stamp}",
            paths
                .session
                .file_name()
                .unwrap_or(std::ffi::OsStr::new("session.json"))
                .to_string_lossy()
        ));
        std::fs::copy(&paths.session, &backup)?;
        std::fs::remove_file(&paths.session)?;
        eprintln!("previous session backed up to {}", backup.display());
    }

    let session = Arc::new(telegram::session::FileSession::load(&paths.session)?);
    let connection = telegram::connect(session, credentials.api_id).await?;

    if connection.login_status().await?.authorized && !force {
        println!("Session is already authorized (it was not written before).");
        connection.shutdown().await;
        restart_worker(&paths).await?;
        return Ok(());
    }

    let phone = match phone.or_else(|| config.phone.clone()) {
        Some(phone) => phone,
        None => prompt("Phone number (international format, e.g. +1234567890)")?,
    };
    println!("Requesting a login code for {phone}...");
    let token = connection
        .request_login_code(&phone, &credentials.api_hash)
        .await?;
    println!("A login code was sent to your Telegram app.");
    let code = prompt_hidden("Enter the login code (hidden)")?;
    match connection.sign_in(&token, &code).await? {
        SignInOutcome::SignedIn { name } => {
            let name = name.unwrap_or_else(|| "your account".into());
            println!("Signed in as {name}.");
        }
        SignInOutcome::PasswordRequired(password_token) => {
            let hint = password_token
                .hint()
                .map(|hint| format!(" (hint from Telegram: {hint})"))
                .unwrap_or_default();
            let password = prompt_hidden(&format!("Enter your Telegram 2FA password{hint}"))?;
            match connection
                .check_password(password_token, password.as_bytes())
                .await?
            {
                Some(name) => println!("Signed in as {name}."),
                None => println!("Signed in."),
            }
        }
    }

    if config.phone.is_none() {
        config.phone = Some(phone);
        config.save(&paths)?;
    }

    connection.shutdown().await;
    println!("Session saved to {}", paths.session.display());
    restart_worker(&paths).await?;
    Ok(())
}

async fn restart_worker(paths: &Paths) -> Result<()> {
    if ipc::ping(&paths.pipe_name).await {
        println!("Background worker is running.");
        return Ok(());
    }
    match launch::spawn_worker_detached() {
        Ok(pid) => {
            if launch::wait_for_worker(&paths.pipe_name, 20).await {
                println!("Background worker started (pid {pid}).");
            } else {
                println!("Background worker spawned (pid {pid}) but the pipe is not answering yet; check `tgdlrs status`.");
            }
            Ok(())
        }
        Err(error) => {
            eprintln!("warning: could not start the background worker: {error}");
            eprintln!("start it manually with `tgdlrs daemon start`");
            Ok(())
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn cmd_download(
    url: &str,
    file_name: Option<String>,
    sha256: Option<String>,
    priority: Option<i64>,
    download_dir: Option<PathBuf>,
    no_verify: bool,
    no_start: bool,
    json: bool,
) -> Result<()> {
    let (paths, config) = load()?;
    tgdlrs_core::telegram::parse_link(url)?;
    let sha256 = sha256.as_deref().map(validate_sha256).transpose()?;
    let download_dir = download_dir.unwrap_or_else(|| config.download_dir.clone());
    std::fs::create_dir_all(&download_dir)?;

    let file_name = file_name
        .as_deref()
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| "download.bin".to_string());

    let store = open_store(&paths)?;
    let reused = store.find_active(Some(url), None, None)?.is_some();
    let mut job = NewDownload::new(file_name, download_dir, config.max_attempts);
    job.source_url = Some(url.to_string());
    job.sha256_expected = sha256;
    job.priority = priority.unwrap_or(0);
    job.source = "cli".into();
    job.verify = !no_verify && config.verify_sha256;
    let download = store.insert(job)?;

    let mut worker_started = false;
    if !no_start && !ipc::ping(&paths.pipe_name).await && launch::spawn_worker_detached().is_ok() {
        worker_started = true;
        launch::wait_for_worker(&paths.pipe_name, 20).await;
    }

    if json {
        return print_json(&serde_json::json!({
            "id": download.id,
            "state": download.state.as_str(),
            "file_name": download.file_name,
            "source_url": download.source_url,
            "final_path": download.final_path.to_string_lossy(),
            "reused_existing": reused,
            "worker_started": worker_started,
        }));
    }
    println!(
        "queued {} (id {}{})",
        download.file_name,
        download.id,
        if reused { ", already active" } else { "" }
    );
    if worker_started {
        println!("background worker started; it resumes automatically");
    }
    println!("check progress with `tgdlrs status` or `tgdlrs list`");
    Ok(())
}

async fn cmd_scan(chat: &str, limit: Option<usize>, json: bool) -> Result<()> {
    let (paths, _config) = load()?;
    let limit = limit.map(|value| value.clamp(1, 10_000));

    enum ChatArg {
        Direct(i64),
        Link(String),
    }
    let trimmed = chat.trim();
    let arg = if let Ok(id) = trimmed.parse::<i64>() {
        ChatArg::Direct(id)
    } else if trimmed.contains("t.me") || trimmed.starts_with("tg://") {
        ChatArg::Link(trimmed.to_string())
    } else {
        return Err(Error::InvalidInput(format!(
            "cannot interpret chat argument: {trimmed}"
        )));
    };

    if !ipc::ping(&paths.pipe_name).await {
        return Err(Error::WorkerUnavailable);
    }

    let chat_id = match arg {
        ChatArg::Direct(id) => id,
        ChatArg::Link(url) => {
            let resolved: tgdlrs_core::models::ResolvedLink =
                ipc::request(&paths.pipe_name, &Request::ResolveLink { url }).await?;
            println!(
                "resolved to chat {chat_id} (message {message_id})",
                chat_id = resolved.chat_id,
                message_id = resolved.message_id
            );
            resolved.chat_id
        }
    };

    let mut files: Vec<FileEntry> =
        ipc::request(&paths.pipe_name, &Request::ScanChat { chat_id, limit }).await?;
    let store = open_store(&paths)?;
    annotate_downloaded(&store, &mut files);

    if json {
        return print_json(&files);
    }
    if files.is_empty() {
        println!("no documents found (chat {chat_id})");
        return Ok(());
    }
    println!("{:>6} {:>12} {:>24} {:>4}  name", "msg", "size", "date", "");
    for file in &files {
        println!(
            "{:>6} {:>12} {:>24} {:>4}  {}",
            file.message_id,
            human_bytes(file.file_size),
            file.date.chars().take(19).collect::<String>(),
            if file.already_downloaded { "done" } else { "" },
            file.file_name
        );
    }
    println!(
        "{} file(s); `done` = already present and complete",
        files.len()
    );
    Ok(())
}

async fn cmd_status(json: bool) -> Result<()> {
    let (paths, config) = load()?;
    let store = open_store(&paths)?;
    let summary = store.summary()?;
    let worker: Option<WorkerStatus> = if ipc::ping(&paths.pipe_name).await {
        ipc::request(&paths.pipe_name, &Request::Status).await.ok()
    } else {
        None
    };

    if json {
        return print_json(&serde_json::json!({
            "worker": worker,
            "summary": summary,
            "download_dir": config.download_dir.to_string_lossy(),
            "disk_free_bytes": storage::free_space(&config.download_dir).unwrap_or(0),
            "disk_reserve_bytes": config.disk_reserve_bytes,
        }));
    }

    match &worker {
        Some(status) => {
            println!(
                "worker:    running (pid {:?}, version {})",
                status.pid, status.version
            );
            println!(
                "session:   {}",
                if status.authorized {
                    "authorized"
                } else {
                    "NOT authorized - run `tgdlrs login`"
                }
            );
            if let Some(user) = &status.user {
                println!("user:      {user}");
            }
            if let Some(active) = &status.active_download {
                println!("active:    {active}");
            }
            if let Some(error) = &status.last_error {
                println!("last error: {error}");
            }
        }
        None => {
            println!("worker:    not running - start it with `tgdlrs daemon start`");
            println!(
                "session:   {}",
                if paths.session.exists() {
                    "file present"
                } else {
                    "no session file - run `tgdlrs login`"
                }
            );
        }
    }
    println!(
        "downloads: total {}, queued {}, downloading {}, verifying {}, verified {}, failed {}",
        summary.total,
        summary.queued + summary.retry_wait,
        summary.downloading,
        summary.verifying,
        summary.verified,
        summary.failed
    );
    println!(
        "bytes:     {} of {} downloaded",
        human_bytes(summary.bytes_downloaded),
        human_bytes(summary.bytes_total)
    );
    println!(
        "dir:       {} ({} free, {} reserve)",
        config.download_dir.display(),
        human_bytes(storage::free_space(&config.download_dir).unwrap_or(0)),
        human_bytes(config.disk_reserve_bytes)
    );
    Ok(())
}

fn cmd_list(state: Option<String>, limit: Option<usize>, json: bool) -> Result<()> {
    let (paths, _config) = load()?;
    let state = state.as_deref().map(parse_state).transpose()?;
    let limit = limit.unwrap_or(50).clamp(1, 500);
    let store = open_store(&paths)?;
    let rows = store.list(state, limit)?;

    if json {
        return print_json(&rows);
    }
    if rows.is_empty() {
        println!("no downloads match.");
        return Ok(());
    }
    println!(
        "{:>8} {:>10} {:>22} {:>5}  name",
        "id", "state", "progress", "att"
    );
    for download in &rows {
        let progress = download
            .progress_fraction()
            .map(|fraction| format!("{:>3.0}%", fraction * 100.0))
            .unwrap_or_else(|| "   ?".to_string());
        println!(
            "{:>8} {:>10} {:>22} {:>5}  {}",
            &download.id[..download.id.len().min(8)],
            download.state.as_str(),
            progress,
            download.attempt,
            download.file_name
        );
        if let Some(error) = &download.error {
            println!("           error: {error}");
        }
    }
    Ok(())
}

async fn cmd_action(id: &str, action: Action, json: bool) -> Result<()> {
    let (paths, _config) = load()?;
    let store = open_store(&paths)?;
    let download = store.find(id)?;

    let (changed, state) = match action {
        Action::Pause => (store.pause(&download.id)?, "pause"),
        Action::Resume => (store.resume(&download.id)?, "resume"),
        Action::Cancel => (store.cancel(&download.id)?, "cancel"),
        Action::Retry => (store.retry(&download.id)?, "retry"),
    };
    let fresh = store.get(&download.id)?;

    if json {
        return print_json(&serde_json::json!({
            "id": fresh.id,
            "action": state,
            "changed": changed,
            "state": fresh.state.as_str(),
        }));
    }
    if changed {
        println!(
            "{state} ok for {} ({} -> {})",
            &fresh.id[..8],
            fresh.file_name,
            fresh.state.as_str()
        );
    } else {
        println!(
            "no change: {} is in state {} which does not accept {state}",
            &fresh.id[..8],
            fresh.state.as_str()
        );
    }
    Ok(())
}

fn cmd_verify(
    target: &str,
    force_path: bool,
    expected_size: Option<u64>,
    sha256: Option<String>,
    json: bool,
) -> Result<()> {
    let (paths, _config) = load()?;
    let sha256 = sha256.as_deref().map(validate_sha256).transpose()?;
    let store = open_store(&paths)?;

    let outcome: (Option<String>, VerifyReport) = if !force_path {
        match store.find(target) {
            Ok(download) => {
                let verified = verification::verify_job(
                    &store,
                    &download.id,
                    expected_size,
                    sha256.as_deref(),
                )?;
                (Some(verified.job_id), verified.report)
            }
            Err(Error::NotFound(_)) => {
                let report =
                    verification::verify_file(Path::new(target), expected_size, sha256.as_deref())?;
                (None, report)
            }
            Err(error) => return Err(error),
        }
    } else {
        let report =
            verification::verify_file(Path::new(target), expected_size, sha256.as_deref())?;
        (None, report)
    };
    let (job_id, report) = outcome;

    if json {
        return print_json(&serde_json::json!({
            "id": job_id,
            "path": report.path.to_string_lossy(),
            "exists": report.exists,
            "size_ok": report.size_ok,
            "hash_ok": report.hash_ok,
            "ok": report.ok,
            "expected_size": report.expected_size,
            "actual_size": report.actual_size,
            "sha256_expected": report.sha256_expected,
            "sha256_actual": report.sha256_actual,
        }));
    }

    println!("file:        {}", report.path.display());
    println!("exists:      {}", report.exists);
    println!(
        "size:        {} (expected {})",
        human_bytes(report.actual_size),
        report
            .expected_size
            .map(human_bytes)
            .unwrap_or_else(|| "?".into())
    );
    if let Some(actual) = &report.sha256_actual {
        println!("sha256:      {actual}");
    }
    println!("result:      {}", if report.ok { "OK" } else { "FAILED" });
    if report.ok {
        Ok(())
    } else {
        Err(Error::Verification(format!(
            "size_ok={} hash_ok={}",
            report.size_ok, report.hash_ok
        )))
    }
}

// -------------------------------------------------------------- doctor --

struct Check {
    name: &'static str,
    level: &'static str, // "ok" | "warn" | "fail"
    detail: String,
}

fn cmd_doctor_sync() -> Vec<Check> {
    let mut checks = Vec::new();
    let mut push = |name: &'static str, level: &'static str, detail: String| {
        checks.push(Check {
            name,
            level,
            detail,
        });
    };

    let paths = match Paths::discover() {
        Ok(paths) => paths,
        Err(error) => {
            return vec![Check {
                name: "paths",
                level: "fail",
                detail: format!("cannot resolve the data directory: {error}"),
            }];
        }
    };

    // Config
    match Config::load_or_create(&paths) {
        Ok(config) => {
            push(
                "config",
                "ok",
                format!(
                    "{} (max_concurrent={}, verify_sha256={}, skip_verified={}, keep_partials={})",
                    paths.config_file.display(),
                    config.max_concurrent_files,
                    config.verify_sha256,
                    config.skip_verified,
                    config.keep_partials
                ),
            );

            // Download directory
            let created = std::fs::create_dir_all(&config.download_dir).is_ok();
            let probe = config.download_dir.join(".tgdlrs-write-probe");
            let writable =
                std::fs::write(&probe, b"ok").is_ok() && std::fs::remove_file(&probe).is_ok();
            if created && writable {
                push(
                    "download dir",
                    "ok",
                    format!("{} is writable", config.download_dir.display()),
                );
            } else {
                push(
                    "download dir",
                    "fail",
                    format!("{} is not writable", config.download_dir.display()),
                );
            }

            // Disk space
            match storage::free_space(&config.download_dir) {
                Ok(free) => {
                    if free < config.disk_reserve_bytes {
                        push(
                            "disk space",
                            "warn",
                            format!(
                                "{} free, below the {} reserve",
                                human_bytes(free),
                                human_bytes(config.disk_reserve_bytes)
                            ),
                        );
                    } else {
                        push(
                            "disk space",
                            "ok",
                            format!(
                                "{} free (reserve {})",
                                human_bytes(free),
                                human_bytes(config.disk_reserve_bytes)
                            ),
                        );
                    }
                }
                Err(error) => push("disk space", "warn", format!("cannot measure: {error}")),
            }

            // Partials present
            if let Ok(entries) = std::fs::read_dir(&config.download_dir) {
                let mut partials = 0u64;
                let mut partial_bytes = 0u64;
                for entry in entries.flatten() {
                    if entry.file_name().to_string_lossy().ends_with(".part") {
                        partials += 1;
                        partial_bytes += entry.metadata().map(|m| m.len()).unwrap_or(0);
                    }
                }
                push(
                    "partials",
                    "ok",
                    format!("{partials} .part file(s), {}", human_bytes(partial_bytes)),
                );
            }
        }
        Err(error) => push(
            "config",
            "fail",
            format!("{}: {error}", paths.config_file.display()),
        ),
    }

    // Database
    match Store::open(&paths) {
        Ok(store) => {
            let version = store.library_version();
            match store.summary() {
                Ok(summary) => push(
                    "state database",
                    "ok",
                    format!(
                        "SQLite {version}; {} job(s), {} verified",
                        summary.total, summary.verified
                    ),
                ),
                Err(error) => push("state database", "fail", format!("query failed: {error}")),
            }
        }
        Err(error) => push("state database", "fail", format!("{error}")),
    }

    // Session
    if paths.session.exists() {
        push("session", "ok", format!("{}", paths.session.display()));
    } else {
        push(
            "session",
            "warn",
            format!("{} is missing; run `tgdlrs login`", paths.session.display()),
        );
    }

    // Worker binary
    match launch::worker_exe_path() {
        Some(path) => push("worker binary", "ok", format!("{}", path.display())),
        None => push(
            "worker binary",
            "warn",
            "tgdlrs-worker.exe not found next to tgdlrs.exe or on PATH".to_string(),
        ),
    }

    // Autostart
    match autostart::installed_command() {
        Ok(Some(command)) => push("autostart", "ok", format!("HKCU Run: {command}")),
        Ok(None) => push(
            "autostart",
            "warn",
            "not registered; run `tgdlrs daemon install-autostart`".to_string(),
        ),
        Err(error) => push(
            "autostart",
            "warn",
            format!("cannot read HKCU Run: {error}"),
        ),
    }

    checks
}

async fn cmd_doctor(json: bool) -> Result<()> {
    let checks = cmd_doctor_sync();
    let failed = checks.iter().filter(|check| check.level == "fail").count();
    let warned = checks.iter().filter(|check| check.level == "warn").count();

    if json {
        let items: Vec<serde_json::Value> = checks
            .iter()
            .map(|check| {
                serde_json::json!({
                    "name": check.name,
                    "level": check.level,
                    "detail": check.detail,
                })
            })
            .collect();
        let worker = if ipc::ping(&Paths::discover()?.pipe_name).await {
            match ipc::request::<WorkerStatus>(&Paths::discover()?.pipe_name, &Request::Status)
                .await
            {
                Ok(status) => serde_json::json!({
                    "reachable": true,
                    "authorized": status.authorized,
                    "user": status.user,
                    "pid": status.pid,
                    "version": status.version,
                }),
                Err(error) => serde_json::json!({ "reachable": true, "error": error.to_string() }),
            }
        } else {
            serde_json::json!({ "reachable": false })
        };
        print_json(&serde_json::json!({
            "ok": failed == 0,
            "failed": failed,
            "warned": warned,
            "checks": items,
            "worker": worker,
        }))?;
        std::process::exit(if failed == 0 { 0 } else { 1 });
    }

    for check in &checks {
        let marker = match check.level {
            "ok" => "ok  ",
            "warn" => "warn",
            _ => "FAIL",
        };
        println!("[{marker}] {:<18} {}", check.name, check.detail);
    }
    let paths = Paths::discover()?;
    if ipc::ping(&paths.pipe_name).await {
        match ipc::request::<WorkerStatus>(&paths.pipe_name, &Request::Status).await {
            Ok(status) => println!(
                "[ok  ] {:<18} reachable (pid {:?}, authorized {})",
                "worker", status.pid, status.authorized
            ),
            Err(error) => println!(
                "[warn] {:<18} answered ping but status failed: {error}",
                "worker"
            ),
        }
    } else {
        println!(
            "[warn] {:<18} not running (`tgdlrs daemon start`)",
            "worker"
        );
    }
    println!("summary: {failed} failed, {warned} warning(s)");
    std::process::exit(if failed == 0 { 0 } else { 1 });
}

// -------------------------------------------------------------- daemon --

async fn cmd_daemon(command: DaemonCommand, json: bool) -> Result<()> {
    let paths = Paths::discover()?;
    paths.ensure()?;

    match command {
        DaemonCommand::Start => {
            if ipc::ping(&paths.pipe_name).await {
                let status: WorkerStatus = ipc::request(&paths.pipe_name, &Request::Status).await?;
                if json {
                    return print_json(
                        &serde_json::json!({ "already_running": true, "pid": status.pid, "version": status.version }),
                    );
                }
                println!(
                    "worker already running (pid {:?}, version {})",
                    status.pid, status.version
                );
                return Ok(());
            }
            let pid = launch::spawn_worker_detached()?;
            let ready = launch::wait_for_worker(&paths.pipe_name, 20).await;
            if json {
                return print_json(
                    &serde_json::json!({ "started": true, "pid": pid, "ready": ready }),
                );
            }
            if ready {
                println!("worker started (pid {pid}) and the pipe is answering");
            } else {
                println!("worker spawned (pid {pid}) but the pipe is not answering yet");
                println!(
                    "check `tgdlrs status`; logs are in {}",
                    paths.log_dir.display()
                );
            }
            Ok(())
        }
        DaemonCommand::Stop { wait } => {
            if !ipc::ping(&paths.pipe_name).await {
                if json {
                    return print_json(
                        &serde_json::json!({ "stopped": false, "reason": "not running" }),
                    );
                }
                println!("worker is not running");
                return Ok(());
            }
            let _ = ipc::request_value(&paths.pipe_name, &Request::Stop).await;
            let wait = wait.unwrap_or(15);
            let mut gone = false;
            for _ in 0..(wait * 2) {
                if !ipc::ping(&paths.pipe_name).await {
                    gone = true;
                    break;
                }
                tokio::time::sleep(Duration::from_millis(500)).await;
            }
            if json {
                return print_json(&serde_json::json!({ "stopped": gone }));
            }
            if gone {
                println!("worker stopped");
            } else {
                println!("stop requested; a running download may take a while to finish (partials are kept)");
            }
            Ok(())
        }
        DaemonCommand::Restart => {
            if ipc::ping(&paths.pipe_name).await {
                let _ = ipc::request_value(&paths.pipe_name, &Request::Stop).await;
                for _ in 0..30 {
                    if !ipc::ping(&paths.pipe_name).await {
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(500)).await;
                }
            }
            let pid = launch::spawn_worker_detached()?;
            let ready = launch::wait_for_worker(&paths.pipe_name, 20).await;
            if json {
                return print_json(
                    &serde_json::json!({ "restarted": true, "pid": pid, "ready": ready }),
                );
            }
            println!(
                "worker restarted (pid {pid}{})",
                if ready { "" } else { ", not answering yet" }
            );
            Ok(())
        }
        DaemonCommand::Status => {
            if ipc::ping(&paths.pipe_name).await {
                let status: WorkerStatus = ipc::request(&paths.pipe_name, &Request::Status).await?;
                if json {
                    return print_json(
                        &serde_json::json!({ "running": true, "pid": status.pid, "version": status.version, "authorized": status.authorized, "user": status.user, "active_download": status.active_download }),
                    );
                }
                println!("running (pid {:?}, version {})", status.pid, status.version);
                println!("authorized: {}", status.authorized);
                if let Some(user) = status.user {
                    println!("user: {user}");
                }
                if let Some(active) = status.active_download {
                    println!("active: {active}");
                }
                Ok(())
            } else {
                if json {
                    return print_json(&serde_json::json!({ "running": false }));
                }
                println!("not running");
                std::process::exit(3);
            }
        }
        DaemonCommand::InstallAutostart => {
            let worker = launch::worker_exe_path().ok_or_else(|| {
                Error::Config(
                    "tgdlrs-worker.exe was not found next to tgdlrs.exe or on PATH".into(),
                )
            })?;
            let command = autostart::install(&worker).map_err(Error::Config)?;
            if json {
                return print_json(&serde_json::json!({ "installed": true, "command": command }));
            }
            println!("autostart installed for the current user:");
            println!("  HKCU\\Software\\Microsoft\\Windows\\CurrentVersion\\Run\\tgdlrs-worker");
            println!("  {command}");
            Ok(())
        }
        DaemonCommand::RemoveAutostart => {
            let removed = autostart::uninstall().map_err(Error::Config)?;
            if json {
                return print_json(&serde_json::json!({ "removed": removed }));
            }
            if removed {
                println!("autostart entry removed");
            } else {
                println!("no autostart entry was registered");
            }
            Ok(())
        }
    }
}
