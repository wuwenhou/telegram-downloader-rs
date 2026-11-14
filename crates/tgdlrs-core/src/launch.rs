//! Locating and starting the background worker process.
//!
//! This is the only place in the codebase that spawns `tgdlrs-worker.exe`.
//! It never uses a shell; it executes the known binary directly with its
//! standard streams detached.

use std::path::PathBuf;
use std::process::{Command, Stdio};

use crate::error::{Error, Result};

/// `CREATE_NO_WINDOW` on Windows; ignored elsewhere.
#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// Find `tgdlrs-worker.exe` next to the current binary or on `PATH`.
pub fn worker_exe_path() -> Option<PathBuf> {
    if let Ok(current) = std::env::current_exe() {
        if let Some(directory) = current.parent() {
            let candidate = directory.join(worker_exe_name());
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    if let Some(path) = std::env::var_os("PATH") {
        for directory in std::env::split_paths(&path) {
            let candidate = directory.join(worker_exe_name());
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    None
}

fn worker_exe_name() -> &'static str {
    if cfg!(windows) {
        "tgdlrs-worker.exe"
    } else {
        "tgdlrs-worker"
    }
}

/// Spawn the worker detached (no console window, stdio discarded).
///
/// Returns the child's process id. The caller is responsible for waiting for
/// the IPC pipe to become available.
pub fn spawn_worker_detached() -> Result<u32> {
    let exe = worker_exe_path().ok_or_else(|| {
        Error::Config("tgdlrs-worker.exe was not found next to this binary or on PATH".into())
    })?;

    let mut command = Command::new(&exe);
    command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());

    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(CREATE_NO_WINDOW);
    }

    let child = command
        .spawn()
        .map_err(|error| Error::Config(format!("cannot start {}: {error}", exe.display())))?;
    Ok(child.id())
}

/// Wait until the IPC pipe answers a ping, or give up after `attempts` × 250 ms.
pub async fn wait_for_worker(pipe_name: &str, attempts: u32) -> bool {
    for _ in 0..attempts {
        if crate::ipc::ping(pipe_name).await {
            return true;
        }
        tokio::time::sleep(std::time::Duration::from_millis(250)).await;
    }
    false
}
