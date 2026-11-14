//! tgdlrs-worker: the background download worker.
//!
//! Runs independently of OpenCode. Recovers interrupted jobs on startup,
//! downloads one file at a time by default, and serves read-only Telegram
//! queries over the local named pipe.

use std::path::PathBuf;

use tgdlrs_core::config::{Config, Paths};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let paths = Paths::discover()?;
    paths.ensure()?;

    let config = Config::load_or_create(&paths)?;

    // Log to a file next to the session so the worker can run detached; if
    // the file cannot be opened, fall back to stderr.
    let log_file = paths.log_dir.join("worker.log");
    let file_appender = tracing_appender::rolling::never(&paths.log_dir, "worker.log");
    let (non_blocking, guard) = tracing_appender::non_blocking(file_appender);
    let _guard = guard;

    let filter = tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| {
        let level = if config.log_level.trim().is_empty() {
            "info"
        } else {
            config.log_level.trim()
        };
        tracing_subscriber::EnvFilter::new(format!("tgdlrs={level}"))
    });

    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_ansi(false)
        .with_writer(non_blocking)
        .init();

    tracing::info!(
        version = env!("CARGO_PKG_VERSION"),
        download_dir = %PathBuf::from(&config.download_dir).display(),
        log = %log_file.display(),
        "tgdlrs-worker starting"
    );

    tgdlrs_core::worker::run(paths, config).await?;
    Ok(())
}
