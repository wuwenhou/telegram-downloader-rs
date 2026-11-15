//! Audit and repair of EDZ-style captions on already-posted documents.
//!
//! Scope: only files that exist in a known 7z archive (name + size match)
//! are eligible for repair. Non-archive files are listed in the report but
//! left untouched.
//!
//! The caption repair keeps the existing leading lines (`EPLAN EDZ | …`,
//! `Part:`, `File:`, …) byte-for-byte and only substitutes or inserts the
//! `SHA-256:` line and the `#EDZID_<sha16>` hashtag line.

use std::collections::{HashMap, HashSet};
use std::time::Duration;

use grammers_client::media::{Document, Media};
use grammers_client::message::InputMessage;
use grammers_session::types::PeerRef;
use serde::{Deserialize, Serialize};
use tokio::io::AsyncReadExt;

use crate::archive::{self, ArchiveEntry};
use crate::error::{Error, Result};
use crate::telegram::{self, TelegramConnection};

/// One document message found in the destination chat.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AuditEntry {
    pub message_id: i32,
    pub file_name: String,
    pub size_bytes: u64,
    pub has_sha_line: bool,
    pub has_edzid: bool,
    /// True when the file name + size matches an entry of the archive.
    pub in_archive: bool,
    /// Original caption (kept out of any report printed to the console).
    #[serde(skip)]
    pub caption: String,
}

/// Aggregate classification.
#[derive(Clone, Debug, Default, Serialize)]
pub struct AuditReport {
    pub total_documents: usize,
    pub ok: usize,
    pub flagged_total: usize,
    pub flagged_chint: usize,
    pub flagged_other: usize,
    pub entries: Vec<AuditEntry>,
}

/// True when the caption has a `SHA-256: <64 lowercase/uppercase hex>` line.
pub fn caption_has_valid_sha_line(caption: &str) -> bool {
    caption.lines().any(|line| {
        let line = line.trim_end();
        let value = match line.strip_prefix("SHA-256: ") {
            Some(value) => value.trim(),
            None => return false,
        };
        value.len() == 64 && value.chars().all(|c| c.is_ascii_hexdigit())
    })
}

/// True when the caption contains a `#EDZID_<hex>` hashtag (at least 16 hex).
pub fn caption_has_edzid_tag(caption: &str) -> bool {
    caption.split_whitespace().any(|token| {
        let lower = token.to_ascii_lowercase();
        let Some(tail) = lower.strip_prefix("#edzid_") else {
            return false;
        };
        tail.len() >= 16 && tail.chars().take(16).all(|c| c.is_ascii_hexdigit())
    })
}

/// Classify a caption; `(has_sha_line, has_edzid)`.
pub fn classify_caption(caption: &str) -> (bool, bool) {
    (
        caption_has_valid_sha_line(caption),
        caption_has_edzid_tag(caption),
    )
}

/// Repair a caption: keep the existing leading lines, substitute/insert the
/// `SHA-256:` line right after the `File:` line (or before the first hashtag
/// line, or at the end), and replace/append the `#EDZID_<sha16>` hashtag line.
///
/// The output is capped at 1024 characters like the OUMIJIA template.
pub fn repair_caption(old: &str, sha256: &str, brand: &str) -> String {
    let mut lines: Vec<String> = old
        .lines()
        .map(|line| line.trim_end().to_string())
        .collect();

    // Drop the old SHA-256 line and any hashtag line carrying EDZID.
    lines.retain(|line| {
        let lower = line.to_ascii_lowercase();
        !lower.starts_with("sha-256:") && !lower.contains("#edzid_")
    });

    let tag_line = format!(
        "#{} #EPLAN #EDZ #EDZID_{}",
        archive::tag(brand),
        &sha256[..16.min(sha256.len())]
    );
    match lines.iter().position(|line| line.starts_with('#')) {
        Some(index) => lines[index] = tag_line,
        None => lines.push(tag_line),
    }

    let sha_line = format!("SHA-256: {sha256}");
    match lines
        .iter()
        .position(|line| line.to_ascii_lowercase().starts_with("file:"))
    {
        Some(index) => lines.insert(index + 1, sha_line),
        None => match lines.iter().position(|line| line.starts_with('#')) {
            Some(index) => lines.insert(index, sha_line),
            None => lines.push(sha_line),
        },
    }

    let joined = lines.join("\n");
    joined.chars().take(1024).collect()
}

/// Index of archive entries by lower-case file name.
type SizeNameIndex = HashMap<String, Vec<(u64, usize)>>;

fn index_by_name(entries: &[ArchiveEntry]) -> SizeNameIndex {
    let mut map: SizeNameIndex = HashMap::new();
    for (index, entry) in entries.iter().enumerate() {
        map.entry(entry.file_name.to_ascii_lowercase())
            .or_default()
            .push((entry.size, index));
    }
    map
}

/// Scan every `.edz` document in the chat and classify its caption.
pub async fn collect_caption_audit(
    connection: &TelegramConnection,
    peer: PeerRef,
    entries: &[ArchiveEntry],
    max_documents: usize,
) -> Result<AuditReport> {
    let mut report = AuditReport::default();
    let by_name = index_by_name(entries);

    let mut messages = connection
        .client()
        .search_messages(peer)
        .filter(grammers_tl_types::enums::MessagesFilter::InputMessagesFilterDocument);
    let mut scanned = 0usize;
    while let Some(message) = messages.next().await.map_err(telegram::map_invocation)? {
        scanned += 1;
        if scanned > max_documents.clamp(1, 1_000_000) {
            break;
        }
        let Some(Media::Document(document)) = message.media() else {
            continue;
        };
        let document_name = file_name_of(&document);
        if !document_name.to_ascii_lowercase().ends_with(".edz") {
            continue;
        }
        report.total_documents += 1;
        let size = document.size().unwrap_or(0) as u64;
        let caption = message.text().to_string();
        let (has_sha_line, has_edzid) = classify_caption(&caption);
        if has_sha_line && has_edzid {
            report.ok += 1;
            continue;
        }
        let in_archive = by_name
            .get(&document_name.to_ascii_lowercase())
            .map(|sizes| sizes.iter().any(|(entry_size, _)| *entry_size == size))
            .unwrap_or(false);
        report.flagged_total += 1;
        if in_archive {
            report.flagged_chint += 1;
        } else {
            report.flagged_other += 1;
        }
        report.entries.push(AuditEntry {
            message_id: message.id(),
            file_name: document_name,
            size_bytes: size,
            has_sha_line,
            has_edzid,
            in_archive,
            caption,
        });
    }
    Ok(report)
}

fn file_name_of(document: &Document) -> String {
    document
        .name()
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .unwrap_or("")
        .to_string()
}

/// Partial-hash pass for already-posted files that need repair.
///
/// Computes SHA-256 only for the requested `(lower-case name, size)` keys.
/// Entries are still consumed sequentially (solid archive), but skipped
/// entries are discarded with a fast drain instead of being buffered, and
/// the pass stops as soon as every requested hash has been computed.
/// The result is merged into the JSON cache at `cache_path`.
pub async fn archive_hashes_subset(
    cache_path: &std::path::Path,
    seven_zip: &std::path::Path,
    first_volume: &std::path::Path,
    entries: &[ArchiveEntry],
    keys: &HashSet<(String, u64)>,
) -> Result<HashMap<(String, u64), String>> {
    // Try the cache first.
    if let Ok(text) = std::fs::read_to_string(cache_path) {
        if let Ok(map) = serde_json::from_str::<HashMap<String, Vec<(u64, String)>>>(&text) {
            let mut found = HashMap::new();
            let mut complete = true;
            for (name, size) in keys.iter() {
                match map
                    .get(name)
                    .and_then(|list| list.iter().find(|(s, _)| s == size))
                {
                    Some((_, sha)) => {
                        found.insert((name.clone(), *size), sha.clone());
                    }
                    None => {
                        complete = false;
                        break;
                    }
                }
            }
            if complete {
                println!("archive hashes: loaded from {}", cache_path.display());
                return Ok(found);
            }
            println!("archive subset cache incomplete, recomputing…");
        }
    }

    if keys.is_empty() {
        return Ok(HashMap::new());
    }

    let last_index = entries
        .iter()
        .rposition(|entry| keys.contains(&(entry.file_name.to_ascii_lowercase(), entry.size)))
        .map(|index| index + 1)
        .unwrap_or(entries.len());

    let mut stream = archive::spawn_stream(seven_zip, first_volume).await?;
    let mut found: HashMap<(String, u64), String> = HashMap::new();
    let mut finished = false;
    let mut errored: Option<Error> = None;
    for entry in entries.iter().take(last_index) {
        let key = (entry.file_name.to_ascii_lowercase(), entry.size);
        if !keys.contains(&key) {
            if let Err(error) = archive::discard_bytes(&mut stream.stdout, entry.size).await {
                errored = Some(Error::Other(format!(
                    "archive stream discard failed at {}: {error}",
                    entry.file_name
                )));
                break;
            }
            continue;
        }
        let mut buffer = vec![0u8; entry.size as usize];
        if let Err(error) = stream.stdout.read_exact(&mut buffer).await {
            errored = Some(Error::Other(format!(
                "archive stream ended early at {}: {error}",
                entry.file_name
            )));
            break;
        }
        if let Some(expected) = entry.crc32 {
            let actual = archive::crc32(&buffer);
            if actual != expected {
                errored = Some(Error::Verification(format!(
                    "archive entry {} CRC mismatch (expected {:08x}, got {:08x})",
                    entry.file_name, expected, actual
                )));
                break;
            }
        }
        found.insert(key, archive::sha256_hex(&buffer));
        if found.len() >= keys.len() {
            finished = true;
            break;
        }
    }
    let fully_read = errored.is_none() && finished;
    if fully_read {
        stream.stop().await;
    } else {
        match stream.wait().await {
            Ok(Some(code)) if code != 0 => {
                return Err(Error::Other(format!(
                    "7-Zip stream exited with code {code}"
                )));
            }
            Ok(_) => {}
            Err(_) => {}
        }
    }
    if let Some(error) = errored {
        return Err(error);
    }
    if found.len() < keys.len() {
        eprintln!(
            "warning: {} of {} requested entries were not found in the archive (they will be reported as failed)",
            keys.len() - found.len(),
            keys.len()
        );
    }

    // Merge into the cache (read-modify-write).
    let mut map: HashMap<String, Vec<(u64, String)>> = match std::fs::read_to_string(cache_path) {
        Ok(text) => serde_json::from_str(&text).unwrap_or_default(),
        Err(_) => HashMap::new(),
    };
    for ((name, size), sha) in &found {
        map.entry(name.clone())
            .or_default()
            .push((*size, sha.clone()));
    }
    if let Some(parent) = cache_path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let tmp = cache_path.with_extension("tmp");
    std::fs::write(&tmp, serde_json::to_string(&map)?)?;
    std::fs::rename(&tmp, cache_path)?;
    println!(
        "archive hashes: subset cached ({} entries) in {}",
        found.len(),
        cache_path.display()
    );
    Ok(found)
}

/// What happened to one edited message (for the report).
#[derive(Clone, Debug, Serialize)]
pub struct FixResult {
    pub message_id: i32,
    pub file_name: String,
    pub status: String,
}

/// Edit one message caption with retries (flood waits always honored).
pub async fn edit_caption(
    connection: &TelegramConnection,
    peer: PeerRef,
    message_id: i32,
    new_caption: &str,
) -> Result<u32> {
    let max_attempts = 5u32;
    let mut attempt = 0u32;
    let mut flood_waits = 0u32;
    const MAX_FLOOD_WAITS: u32 = 8;
    loop {
        match connection
            .client()
            .edit_message(
                peer,
                message_id,
                InputMessage::new().text(new_caption.to_string()),
            )
            .await
        {
            Ok(()) => return Ok(attempt + flood_waits),
            Err(error) => {
                let error = telegram::map_invocation(error);
                if let Some(seconds) = error.flood_wait_seconds() {
                    flood_waits += 1;
                    if flood_waits > MAX_FLOOD_WAITS {
                        return Err(error);
                    }
                    eprintln!(
                        "flood wait {seconds}s for message {message_id} (flood #{flood_waits})"
                    );
                    tokio::time::sleep(Duration::from_secs(seconds)).await;
                    continue;
                }
                attempt += 1;
                if attempt < max_attempts && error.is_retryable() {
                    let seconds = 2u64.saturating_pow(attempt.min(6)).min(120);
                    eprintln!("retry {attempt}/{max_attempts} for message {message_id} in {seconds}s ({error})");
                    tokio::time::sleep(Duration::from_secs(seconds)).await;
                    continue;
                }
                return Err(error);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_valid_and_invalid_sha_lines() {
        let good = format!("SHA-256: {}", "a".repeat(64));
        assert!(caption_has_valid_sha_line(&good));
        assert!(!caption_has_valid_sha_line("SHA-256: deadbeef"));
        assert!(!caption_has_valid_sha_line("file: x.edz"));
        assert!(!caption_has_valid_sha_line(""));
    }

    #[test]
    fn detects_edzid_tag() {
        assert!(caption_has_edzid_tag(
            "#CHINT #EPLAN #EDZ #EDZID_7db6e24aa9a21f6e"
        ));
        assert!(caption_has_edzid_tag("#EDZID_0123456789abcdef0123"));
        assert!(!caption_has_edzid_tag("#CHINT #EPLAN #EDZ"));
        assert!(!caption_has_edzid_tag("EDZID_7db6e24aa9a21f6e")); // no # required? we require #
        assert!(!caption_has_edzid_tag(""));
    }

    #[test]
    fn repair_replaces_sha_and_tag_keeping_header() {
        let old = "EPLAN EDZ | CHINT\nPart: CHT.101041\nFile: CHT.101041.edz\n#CHINT #EPLAN";
        let sha = "7db6e24aa9a21f6ede74022243849aba47571e9a5c954f70d6be16f8dcc864b6";
        let fixed = repair_caption(old, sha, "CHINT");
        assert!(fixed.contains("EPLAN EDZ | CHINT"));
        assert!(fixed.contains("Part: CHT.101041"));
        assert!(fixed.contains("File: CHT.101041.edz"));
        assert!(fixed.contains(&format!("SHA-256: {sha}")));
        assert!(fixed.contains("#EDZID_7db6e24aa9a21f6e"));
        // the old hashtag line without EDZID was replaced, not duplicated
        assert_eq!(fixed.matches("#EPLAN").count(), 1);
    }

    #[test]
    fn repair_when_nothing_but_header_exists() {
        let old = "EPLAN EDZ | CHINT\nFile: CHT.101041.edz";
        let sha = "a".repeat(64);
        let fixed = repair_caption(old, &sha, "CHINT");
        assert!(fixed.contains(&format!("SHA-256: {sha}")));
        assert!(fixed.contains("#EPLAN #EDZ #EDZID_aaaaaaaaaaaaaaaa"));
        assert!(fixed.matches("SHA-256:").count() == 1);
    }

    #[test]
    fn repair_replaces_bad_sha_line() {
        let old = "EPLAN EDZ | CHINT\nFile: CHT.101041.edz\nSHA-256: deadbeef\n#CHINT #EDZID_0123456789abcdef00";
        let sha = "b".repeat(64);
        let fixed = repair_caption(old, &sha, "CHINT");
        assert!(fixed.contains(&format!("SHA-256: {sha}")));
        assert!(!fixed.contains("deadbeef"));
        assert!(!fixed.contains("#EDZID_0123456789abcdef00"));
        assert!(fixed.contains("#EDZID_bbbbbbbbbbbbbbbb"));
    }
}
