//! 7-Zip archive support for archive-wide upload pipelines.
//!
//! Features:
//! - parse `7z l -slt` technical listings (file entries, ordered as in the archive),
//! - stream the (split) archive to stdout (`7z x -so`) and de-multiplex it
//!   back into the individual files using the sizes from the listing,
//! - CRC-32 (IEEE) and SHA-256 helpers to validate every de-multiplexed file.
//!
//! The split archive is a single LZMA2 solid stream, so the only way to read a
//! file is to decode everything before it. Decoding once and de-multiplexing
//! in memory keeps the pipeline O(total archive bytes) with zero temp files.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex};

use tokio::io::BufReader;

use crate::error::{Error, Result};

/// One `.edz` file entry inside a (possibly split) 7z archive.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ArchiveEntry {
    /// Path inside the archive, e.g. `CHINT\CHT.101041.edz`.
    pub archive_path: String,
    /// File name component (no directory part), e.g. `CHT.101041.edz`.
    pub file_name: String,
    /// Uncompressed size in bytes.
    pub size: u64,
    /// CRC-32 (IEEE) of the uncompressed bytes, as reported by `7z l -slt`.
    pub crc32: Option<u32>,
}

/// Parse the output of `7z l -slt <archive>` into the `.edz` entries in
/// archive order. Non-file entries (volumes, inner-archive headers, the
/// `CHINT` folder) are skipped.
pub fn parse_slt_listing(text: &str) -> Vec<ArchiveEntry> {
    fn finish(
        entries: &mut Vec<ArchiveEntry>,
        path: Option<String>,
        size: Option<u64>,
        crc: Option<u32>,
    ) {
        if let (Some(path), Some(size)) = (path, size) {
            let file_name = path
                .rsplit(['\\', '/'])
                .next()
                .unwrap_or(path.as_str())
                .to_string();
            // Keep only files (path contains a directory part) with .edz extension.
            let has_dir = path.contains('\\') || path.contains('/');
            if has_dir && file_name.to_ascii_lowercase().ends_with(".edz") {
                entries.push(ArchiveEntry {
                    archive_path: path,
                    file_name,
                    size,
                    crc32: crc,
                });
            }
        }
    }

    let mut entries = Vec::new();
    let mut path: Option<String> = None;
    let mut size: Option<u64> = None;
    let mut crc: Option<u32> = None;

    for raw in text.lines() {
        let line = raw.trim_end();
        if let Some(value) = line.strip_prefix("Path = ") {
            finish(&mut entries, path.take(), size.take(), crc.take());
            path = Some(value.trim().to_string());
        } else if let Some(value) = line.strip_prefix("Size = ") {
            if size.is_none() {
                size = value.trim().parse::<u64>().ok();
            }
        } else if let Some(value) = line.strip_prefix("CRC = ") {
            crc = u32::from_str_radix(value.trim(), 16).ok();
        }
    }
    finish(&mut entries, path.take(), size.take(), crc.take());
    entries
}

/// Run `7z l -slt <first_volume>` and return the `.edz` entries in archive order.
pub fn list_entries(seven_zip: &Path, first_volume: &Path) -> Result<Vec<ArchiveEntry>> {
    let output = std::process::Command::new(seven_zip)
        .arg("l")
        .arg("-slt")
        .arg(first_volume)
        .output()
        .map_err(|error| Error::Other(format!("cannot start 7-Zip: {error}")))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(Error::Other(format!(
            "7-Zip listing failed (exit {:?}): {}",
            output.status.code(),
            stderr.chars().take(500).collect::<String>()
        )));
    }
    // 7-Zip writes the listing in the console codepage; file names here are ASCII.
    let text = String::from_utf8_lossy(&output.stdout);
    Ok(parse_slt_listing(&text))
}

/// A running `7z x -so` child whose stdout is the de-multiplexed stream.
/// The stderr is drained in the background into a capped buffer so a noisy
/// 7-Zip cannot deadlock the pipeline.
///
/// Dropping without calling [`StreamingArchive::stop`] kills the child.
pub struct StreamingArchive {
    child: tokio::process::Child,
    /// Buffered stdout of the child (byte-faithful stream of file contents).
    pub stdout: BufReader<tokio::process::ChildStdout>,
    stderr: Arc<Mutex<String>>,
}

/// Spawn `7z x -so -y <first_volume>` with stdout piped.
pub async fn spawn_stream(seven_zip: &Path, first_volume: &Path) -> Result<StreamingArchive> {
    let mut command = tokio::process::Command::new(seven_zip);
    command
        .arg("x")
        .arg("-so")
        .arg("-y")
        .arg(first_volume)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let mut child = command
        .spawn()
        .map_err(|error| Error::Other(format!("cannot start 7-Zip stream: {error}")))?;

    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| Error::Other("7-Zip stdout is not piped".into()))?;
    let stderr_pipe = child
        .stderr
        .take()
        .ok_or_else(|| Error::Other("7-Zip stderr is not piped".into()))?;

    let stderr = Arc::new(Mutex::new(String::new()));
    let sink = Arc::clone(&stderr);
    tokio::spawn(async move {
        use tokio::io::AsyncReadExt;
        let mut pipe = BufReader::new(stderr_pipe);
        let mut buf = [0u8; 4096];
        loop {
            match pipe.read(&mut buf).await {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if let Ok(mut guard) = sink.lock() {
                        if guard.len() < 8192 {
                            // 7-Zip writes ASCII diagnostics to stderr.
                            guard.push_str(&String::from_utf8_lossy(&buf[..n]));
                        }
                    }
                }
            }
        }
    });

    Ok(StreamingArchive {
        child,
        stdout: BufReader::with_capacity(4 * 1024 * 1024, stdout),
        stderr,
    })
}

impl StreamingArchive {
    /// Captured (capped) stderr output, for diagnostics.
    pub fn stderr_text(&self) -> String {
        self.stderr
            .lock()
            .map(|guard| guard.clone())
            .unwrap_or_default()
    }

    /// Kill the child and reap it. Always call this when stopping early.
    pub async fn stop(mut self) {
        let _ = self.child.start_kill();
        let _ = self.child.wait().await;
    }

    /// Wait for a naturally finished child; returns its exit code.
    pub async fn wait(mut self) -> Result<Option<i32>> {
        let status = self
            .child
            .wait()
            .await
            .map_err(|error| Error::Other(format!("cannot wait for 7-Zip: {error}")))?;
        Ok(status.code())
    }
}

impl Drop for StreamingArchive {
    fn drop(&mut self) {
        let _ = self.child.start_kill();
    }
}

/// Discard exactly `size` bytes from the stream (used for already-posted files).
pub async fn discard_bytes(
    reader: &mut BufReader<tokio::process::ChildStdout>,
    size: u64,
) -> Result<u64> {
    use tokio::io::AsyncReadExt;
    let mut taken = reader.take(size);
    let mut sink = tokio::io::sink();
    let copied = tokio::io::copy(&mut taken, &mut sink).await?;
    Ok(copied)
}

/// Compute the CRC-32 (IEEE) of `data`.
pub fn crc32(data: &[u8]) -> u32 {
    const fn build_table() -> [u32; 256] {
        let mut table = [0u32; 256];
        let mut i = 0usize;
        while i < 256 {
            let mut c = i as u32;
            let mut k = 0;
            while k < 8 {
                c = if c & 1 != 0 {
                    0xEDB8_8320 ^ (c >> 1)
                } else {
                    c >> 1
                };
                k += 1;
            }
            table[i] = c;
            i += 1;
        }
        table
    }

    static TABLE: [u32; 256] = build_table();

    let mut c = 0xFFFF_FFFFu32;
    for &byte in data {
        c = TABLE[((c ^ u32::from(byte)) & 0xFF) as usize] ^ (c >> 8);
    }
    !c
}

/// SHA-256 hex digest of `data`.
pub fn sha256_hex(data: &[u8]) -> String {
    use sha2::Digest;
    let mut hasher = sha2::Sha256::new();
    hasher.update(data);
    hex::encode(hasher.finalize())
}

/// Build a `#TAG` like the OUMIJIA metadata tagger:
/// non-word characters become `_`, edges trimmed, capped at 50 chars.
pub fn tag(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    let mut pending_underscore = false;
    for ch in value.chars() {
        if ch.is_alphanumeric() || ch == '_' {
            if pending_underscore && !out.is_empty() {
                out.push('_');
            }
            pending_underscore = false;
            out.push(ch);
        } else {
            pending_underscore = true;
        }
    }
    let trimmed = out.trim_matches('_');
    let capped: String = trimmed.chars().take(50).collect();
    if capped.is_empty() {
        "Unknown".to_string()
    } else {
        capped
    }
}

/// Best-effort detection of a usable 7-Zip binary.
pub fn detect_seven_zip() -> Option<PathBuf> {
    let candidates = [
        PathBuf::from(r"C:\Program Files\7-Zip\7z.exe"),
        PathBuf::from(r"C:\Program Files (x86)\7-Zip\7z.exe"),
        PathBuf::from(r"E:\VScode_Opencode\Tools\7-Zip\7z.exe"),
    ];
    candidates.into_iter().find(|path| path.is_file())
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "\
7-Zip 23.01 (x64) : Copyright (c) 1999-2023 Igor Pavlov : 2023-06-20

Listing archive: D:\\Downloads\\Brave\\CHINT.7z.001

--
Path = D:\\Downloads\\Brave\\CHINT.7z.001
Type = Split
Physical Size = 4175429632
Volumes = 26
Total Physical Size = 105800826003
----
Path = CHINT.7z
Size = 105800826003
--
Path = CHINT.7z
Type = 7z
Physical Size = 105800826003
Headers Size = 228397
Method = LZMA2:26
Blocks = 15

Path = CHINT
Folder = +
Size = 0
Attributes = D

Path = CHINT\\CHT.101041.edz
Size = 8029872
Packed Size = 13089786481
Modified = 2024-11-03 16:54:36.3803696
Attributes = A
CRC = 969D3838
Encrypted = -
Method = LZMA2:26
Block = 0

Path = CHINT\\CHT.101042.edz
Size = 7195768
Packed Size = 1234
CRC = AABBCCDD

Path = CHINT\\readme.txt
Size = 12
CRC = 00000000
";

    #[test]
    fn parses_edz_entries_in_order() {
        let entries = parse_slt_listing(SAMPLE);
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].file_name, "CHT.101041.edz");
        assert_eq!(entries[0].archive_path, "CHINT\\CHT.101041.edz");
        assert_eq!(entries[0].size, 8029872);
        assert_eq!(entries[0].crc32, Some(0x969D3838));
        assert_eq!(entries[1].file_name, "CHT.101042.edz");
    }

    #[test]
    fn crc32_matches_known_values() {
        assert_eq!(crc32(b""), 0);
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
        assert_eq!(
            crc32(b"The quick brown fox jumps over the lazy dog"),
            0x414F_A339
        );
    }

    #[test]
    fn tag_matches_python_behaviour() {
        assert_eq!(tag("CHINT"), "CHINT");
        assert_eq!(tag("AB CD-EF"), "AB_CD_EF");
        assert_eq!(tag("___"), "Unknown");
    }
}
