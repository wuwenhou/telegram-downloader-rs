//! Desktop notifications for finished jobs.
//!
//! Delivery is best-effort: a PowerShell balloon (NotifyIcon) is spawned
//! detached so the worker never blocks on it. If anything is missing the
//! notification is silently skipped — downloads must never fail because of a toast.

use std::process::{Command, Stdio};

use crate::config::Paths;
use crate::models::Download;

const BALLON_SCRIPT: &str = "tgdlrs-notify.ps1";

/// Notify that a job finished downloading (and was verified when applicable).
pub fn download_finished(paths: &Paths, download: &Download, verified: bool) {
    let lang = gui_lang(&paths.config_file);
    let bytes = download.bytes_total.or(download.file_size).unwrap_or(0);
    let (title, body) = message(&lang, true, &download.file_name, bytes, verified);
    spawn_balloon(paths, &title, &body);
}

/// Notify that a job failed.
pub fn download_failed(paths: &Paths, download: &Download, error: &str) {
    let lang = gui_lang(&paths.config_file);
    let bytes = download.bytes_total.or(download.file_size).unwrap_or(0);
    let (title, body) = failed_message(&lang, &download.file_name, bytes, error);
    spawn_balloon(paths, &title, &body);
}

fn gui_lang(config_file: &std::path::Path) -> String {
    std::fs::read_to_string(config_file)
        .ok()
        .and_then(|raw| serde_json::from_str::<serde_json::Value>(&raw).ok())
        .and_then(|value| {
            value
                .get("gui_lang")
                .and_then(|v| v.as_str().map(str::to_string))
        })
        .unwrap_or_else(|| "vi".to_string())
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

fn message(
    lang: &str,
    _finished: bool,
    name: &str,
    bytes: u64,
    verified: bool,
) -> (String, String) {
    match lang {
        "vi" => (
            format!("Tải xong: {name}"),
            if verified {
                format!("{} — đã xác minh SHA-256", human_bytes(bytes))
            } else {
                format!("{} — tải xong (chưa verify hash)", human_bytes(bytes))
            },
        ),
        _ => (
            format!("Download complete: {name}"),
            if verified {
                format!("{} — SHA-256 verified", human_bytes(bytes))
            } else {
                format!("{} — downloaded (hash not checked)", human_bytes(bytes))
            },
        ),
    }
}

fn failed_message(lang: &str, name: &str, bytes: u64, error: &str) -> (String, String) {
    match lang {
        "vi" => (
            format!("Tải THẤT BẠI: {name}"),
            format!("{} — {error}", human_bytes(bytes)),
        ),
        _ => (
            format!("Download FAILED: {name}"),
            format!("{} — {error}", human_bytes(bytes)),
        ),
    }
}

fn spawn_balloon(paths: &Paths, title: &str, body: &str) {
    let script = paths.home.join(BALLON_SCRIPT);
    if !script.is_file() {
        tracing::debug!(path = %script.display(), "notification script missing; skipping balloon");
        return;
    }

    let mut command = Command::new("powershell.exe");
    command
        .arg("-NoProfile")
        .arg("-ExecutionPolicy")
        .arg("Bypass")
        .arg("-File")
        .arg(&script)
        .arg("-Title")
        .arg(title)
        .arg("-Body")
        .arg(body)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());

    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    }

    if let Err(error) = command.spawn() {
        tracing::debug!(%error, "cannot spawn the notification balloon");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn english_success_message() {
        let (title, body) = message("en", true, "part.bin", 1_500_000_000, true);
        assert!(title.contains("part.bin"));
        assert!(body.contains("SHA-256"));
        assert!(body.contains("GiB"));
    }

    #[test]
    fn vietnamese_messages() {
        let (title, body) = message("vi", true, "CHINT.7z.021", 4_175_429_632, true);
        assert!(title.contains("CHINT.7z.021"));
        assert!(body.contains("xác minh"));
        let (title, body) = failed_message("vi", "CHINT.7z.022", 1000, "boom");
        assert!(title.contains("THẤT BẠI"));
        assert!(body.contains("boom"));
    }
}
