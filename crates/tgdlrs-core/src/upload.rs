//! Archive-wide upload pipeline.
//!
//! Streams a (split) 7z archive straight into Telegram — one message per
//! `.edz` document — without extracting anything to disk:
//!
//! 1. `7z l -slt` gives the ordered entries (name + size + CRC-32).
//! 2. A single `7z x -so` child decodes the archive once; its stdout is
//!    de-multiplexed into per-file byte buffers using the listed sizes.
//! 3. Each file is CRC-32 checked against the listing (catches any
//!    misalignment before it ever reaches Telegram), SHA-256 hashed and
//!    uploaded with the exact caption template used by the OUMIJIA bot.
//! 4. Progress is checkpointed in SQLite (`upload_jobs`), so interrupting the
//!    process and running it again skips everything already posted. The
//!    destination channel is also scanned up front to avoid duplicates with
//!    files posted before this tool existed.
//!
//! Files that permanently fail are dumped to a failure directory for manual
//! handling; everything else keeps flowing.

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use grammers_client::media::{Document, Media};
use grammers_client::message::InputMessage;
use grammers_session::types::PeerRef;
use serde::Serialize;
use tokio::io::AsyncReadExt;

use crate::archive::{self, ArchiveEntry};
use crate::database::Store;
use crate::error::{parse_flood_wait, Error, Result};
use crate::telegram::{self, TelegramConnection};

/// What the destination pre-scan found already posted.
#[derive(Default, Debug)]
pub struct DestinationInventory {
    /// Lower-cased document file names.
    pub names: HashSet<String>,
    /// Lower-cased full SHA-256 digests parsed from captions.
    pub sha256s: HashSet<String>,
}

impl DestinationInventory {
    fn is_posted(&self, file_name: &str, sha256: Option<&str>) -> bool {
        if self.names.contains(&file_name.to_ascii_lowercase()) {
            return true;
        }
        match sha256 {
            Some(sha) => self.sha256s.contains(&sha.to_ascii_lowercase()),
            None => false,
        }
    }
}

/// Parse the `File:` / `SHA-256:` lines of an EDZ-style caption.
pub fn parse_caption(caption: &str) -> Option<(String, String)> {
    let mut file_name = None;
    let mut sha256 = None;
    for line in caption.lines() {
        let line = line.trim_end();
        if let Some(value) = line.strip_prefix("File: ") {
            let value = value.trim();
            if !value.is_empty() {
                file_name = Some(value.to_string());
            }
        } else if let Some(value) = line.strip_prefix("SHA-256: ") {
            let value = value.trim().to_ascii_lowercase();
            if value.len() == 64 && value.chars().all(|c| c.is_ascii_hexdigit()) {
                sha256 = Some(value);
            }
        }
    }
    match (file_name, sha256) {
        (Some(file), Some(sha)) => Some((file, sha)),
        _ => None,
    }
}

/// Build the caption exactly like the OUMIJIA bot does:
///
/// ```text
/// EPLAN EDZ | CHINT
/// Part: CHT.101041
/// File: CHT.101041.edz
/// SHA-256: <64 lowercase hex>
/// #CHINT #EPLAN #EDZ #EDZID_<first 16 hex>
/// ```
pub fn edz_caption(brand: &str, file_name: &str, sha256: &str) -> String {
    let part =
        if file_name.len() >= 4 && file_name[file_name.len() - 4..].eq_ignore_ascii_case(".edz") {
            &file_name[..file_name.len() - 4]
        } else {
            file_name
        };
    format!(
        "EPLAN EDZ | {brand}\n\
         Part: {part}\n\
         File: {file_name}\n\
         SHA-256: {sha256}\n\
         #{tag} #EPLAN #EDZ #EDZID_{identifier}",
        brand = brand,
        part = part,
        file_name = file_name,
        sha256 = sha256,
        tag = archive::tag(brand),
        identifier = &sha256[..16.min(sha256.len())],
    )
}

/// Iterate the destination chat history and collect already-posted `.edz`
/// names.
///
/// Results are cached in the `dest_inventory` table of the state DB. The
/// first ever run does a full document scan; every later run only scans
/// messages NEWER than the stored watermark (`settings.dest_scan_watermark`)
/// and merges them into the cached inventory.
///
/// Note: the channel is treated as append-mostly. Files removed from it are
/// not noticed until the next full refresh — to force one, delete the
/// watermark setting (or the `dest_inventory` table) and re-run.
pub async fn scan_destination(
    connection: &TelegramConnection,
    peer: PeerRef,
    store: &Store,
    max_documents: usize,
) -> Result<DestinationInventory> {
    async fn collect(
        connection: &TelegramConnection,
        peer: PeerRef,
        resume_offset: Option<i32>,
        stop_below: Option<i32>,
        max_documents: usize,
    ) -> Result<CollectOutput> {
        let mut messages = connection.client().search_messages(peer).filter(
            grammers_tl_types::enums::MessagesFilter::InputMessagesFilterDocument,
        );
        if let Some(offset) = resume_offset {
            // 0 = start at the newest; otherwise fetch messages older than `offset`.
            messages = messages.offset_id(offset);
        }
        let mut rows = Vec::new();
        let mut highest: Option<i32> = None;
        let mut lowest: Option<i32> = None;
        let mut scanned = 0usize;
        let mut capped = false;
        while let Some(message) = messages.next().await.map_err(telegram::map_invocation)? {
            scanned += 1;
            let id = message.id();
            if id > highest.unwrap_or(0) {
                highest = Some(id);
            }
            if lowest.is_none_or(|current| id < current) {
                lowest = Some(id);
            }
            if let Some(mark) = stop_below {
                if id <= mark {
                    break;
                }
            }
            if scanned >= max_documents.clamp(1, 1_000_000) {
                capped = true;
                println!("destination scan: hit the {max_documents} document cap");
                break;
            }
            if scanned % 5000 == 0 {
                println!("destination scan: {scanned} document(s)…");
            }
            let Some(Media::Document(document)) = message.media() else {
                continue;
            };
            let document_name = file_name_of(&document);
            if !document_name.to_ascii_lowercase().ends_with(".edz") {
                continue;
            }
            let (caption_name, caption_sha) = parse_caption(message.text()).unwrap_or_default();
            let key = if caption_name.is_empty() {
                document_name
            } else {
                caption_name
            };
            let size = document.size().unwrap_or(0) as u64;
            let sha = if caption_sha.is_empty() {
                None
            } else {
                Some(caption_sha)
            };
            rows.push((key.to_ascii_lowercase(), size, sha, id));
        }
        Ok(CollectOutput {
            rows,
            highest,
            lowest,
            capped,
        })
    }

    struct CollectOutput {
        rows: Vec<(String, u64, Option<String>, i32)>,
        highest: Option<i32>,
        lowest: Option<i32>,
        capped: bool,
    }

    let complete = store.setting("dest_inventory_complete")?.as_deref() == Some("1");

    if complete {
        let mark = store.dest_watermark()?.unwrap_or(0);
        println!("destination scan: incremental refresh (watermark {mark})…");
        let out = collect(connection, peer, None, Some(mark as i32), max_documents).await?;
        store.dest_inventory_upsert(&out.rows)?;
        if let Some(highest) = out.highest {
            store.set_dest_watermark(mark.max(highest as i64))?;
        }
        println!(
            "destination scan: refreshed with {} new document(s)",
            out.rows.len()
        );
    } else {
        let offset = store
            .setting("dest_scan_resume")?
            .and_then(|value| value.parse::<i32>().ok())
            .unwrap_or(0);
        let highest_ever = store
            .setting("dest_scan_highest")?
            .and_then(|value| value.parse::<i64>().ok())
            .unwrap_or(0);
        println!(
            "destination scan: full scan in progress (resume offset {offset}, highest seen {highest_ever})…"
        );
        let out = collect(connection, peer, Some(offset), None, max_documents).await?;
        let added = out.rows.len();
        store.dest_inventory_upsert(&out.rows)?;
        // Remember how deep we have scanned, and the highest id found anywhere.
        let resume = out.lowest.unwrap_or(offset);
        store.set_setting("dest_scan_resume", &resume.to_string())?;
        let high = highest_ever.max(out.highest.map(|h| h as i64).unwrap_or(0));
        store.set_setting("dest_scan_highest", &high.to_string())?;
        if !out.capped {
            // The whole history has been consumed: this is now complete.
            store.set_setting("dest_inventory_complete", "1")?;
            store.set_dest_watermark(high)?;
            println!(
                "destination scan: full scan COMPLETE after resume pass, {added} new document(s) merged"
            );
        } else {
            println!(
                "destination scan: batch saved ({added} merged); restart continues from offset {resume}"
            );
        }
    }

    let names = store.dest_inventory_names()?;
    Ok(DestinationInventory {
        names,
        sha256s: HashSet::new(),
    })
}

fn file_name_of(document: &Document) -> String {
    document
        .name()
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .unwrap_or("")
        .to_string()
}

/// Options for one upload run.
pub struct UploadOptions {
    /// First volume of the split archive (all of them stay untouched).
    pub archive_volume: PathBuf,
    /// Path to 7z.exe.
    pub seven_zip: PathBuf,
    /// Peer reference of the destination chat/channel.
    pub peer: PeerRef,
    /// Human-readable destination label for logs.
    pub dest_label: String,
    /// Brand used in the caption (e.g. `CHINT`).
    pub brand: String,
    /// Pacing sleep after each successful post.
    pub delay: Duration,
    /// Retries per file before declaring failure (flood waits are not capped by this).
    pub max_attempts: u32,
    /// Process only the first N entries of the archive (0 = all).
    pub limit: u64,
    /// Hash and log, but do not send anything.
    pub dry_run: bool,
    /// Directory for files that failed permanently.
    pub failure_dir: PathBuf,
    /// Skip the destination scan (DB-only dedup). Not recommended.
    pub skip_scan: bool,
    /// If set, only these file names (lower-cased) are processed; everything
    /// else is decompressed and discarded without any output or counters.
    pub only: Option<HashSet<String>>,
    /// Number of concurrent upload workers (1 = classic sequential mode).
    pub parallel: u32,
}

/// Final counters for one run.
#[derive(Clone, Debug, Default, Serialize)]
pub struct UploadSummary {
    pub entries: usize,
    pub uploaded: u64,
    pub skipped: u64,
    pub failed: u64,
    pub bytes_uploaded: u64,
    pub dry_run: bool,
    pub elapsed_secs: u64,
}

/// Run the full pipeline: list entries, scan destination, stream, upload, checkpoint.
pub async fn run_upload(
    connection: &TelegramConnection,
    store: &Store,
    options: UploadOptions,
) -> Result<UploadSummary> {
    let started = Instant::now();
    let entries = archive::list_entries(&options.seven_zip, &options.archive_volume)?;
    let total = entries.len();
    println!("archive: {total} .edz entries");
    if options.limit > 0 {
        println!("limit: processing first {} entrie(s) only", options.limit);
    }

    let posted_names = store.upload_posted_names()?;
    let inventory = if options.skip_scan {
        println!("destination scan: skipped (--skip-scan)");
        None
    } else {
        println!("scanning destination ({})…", options.dest_label);
        let inventory = scan_destination(connection, options.peer, store, 500_000).await?;
        println!(
            "destination scan: {} document name(s) known",
            inventory.names.len()
        );
        Some(inventory)
    };

    let mut stream = archive::spawn_stream(&options.seven_zip, &options.archive_volume).await?;

    let mut summary = UploadSummary {
        entries: total,
        dry_run: options.dry_run,
        ..Default::default()
    };

    let consumed = demux_and_upload(
        connection,
        store,
        &mut stream,
        &entries,
        &posted_names,
        inventory.as_ref(),
        &options,
        &mut summary,
    )
    .await?;

    if consumed < entries.len() {
        // The loop broke early (`--limit`); the child is stuck writing to a
        // pipe nobody reads anymore. Kill it instead of waiting for a
        // natural exit that never comes.
        stream.stop().await;
    } else {
        // The stream ran to completion; make sure the child reaped.
        let stderr = stream.stderr_text();
        let code = stream.wait().await?;
        if let Some(code) = code {
            if code != 0 {
                return Err(Error::Other(format!(
                    "7-Zip stream exited with code {code}: {stderr}"
                )));
            }
        }
    }

    summary.elapsed_secs = started.elapsed().as_secs();
    Ok(summary)
}

#[allow(clippy::too_many_arguments)]
async fn demux_and_upload(
    connection: &TelegramConnection,
    store: &Store,
    stream: &mut archive::StreamingArchive,
    entries: &[ArchiveEntry],
    posted_names: &HashSet<String>,
    inventory: Option<&DestinationInventory>,
    options: &UploadOptions,
    summary: &mut UploadSummary,
) -> Result<usize> {
    if options.parallel <= 1 {
        return demux_serial(
            connection,
            store,
            stream,
            entries,
            posted_names,
            inventory,
            options,
            summary,
        )
        .await;
    }
    demux_parallel(
        connection,
        store,
        stream,
        entries,
        posted_names,
        inventory,
        options,
        summary,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
async fn demux_serial(
    connection: &TelegramConnection,
    store: &Store,
    stream: &mut archive::StreamingArchive,
    entries: &[ArchiveEntry],
    posted_names: &HashSet<String>,
    inventory: Option<&DestinationInventory>,
    options: &UploadOptions,
    summary: &mut UploadSummary,
) -> Result<usize> {
    let total = entries.len();
    let mut processed = 0u64;
    let mut consumed = 0usize;
    let mut discarded_only = 0usize;

    for (index, entry) in entries.iter().enumerate() {
        if options.limit > 0 && processed >= options.limit {
            break;
        }
        let position = index + 1;
        let lower = entry.file_name.to_ascii_lowercase();
        let size = entry.size as usize;

        if let Some(only) = &options.only {
            if !only.contains(&lower) {
                archive::discard_bytes(&mut stream.stdout, entry.size).await?;
                consumed += 1;
                discarded_only += 1;
                if discarded_only % 1000 == 0 {
                    println!("  … {discarded_only} non-selected entries discarded");
                }
                continue;
            }
        }

        let already = posted_names.contains(&lower)
            || inventory
                .map(|inv| inv.is_posted(&entry.file_name, None))
                .unwrap_or(false);
        if already {
            archive::discard_bytes(&mut stream.stdout, entry.size).await?;
            consumed += 1;
            summary.skipped += 1;
            println!(
                "[{position}/{total}] skip (already posted) {}",
                entry.file_name
            );
            continue;
        }
        processed += 1;

        let mut buffer = vec![0u8; size];
        stream
            .stdout
            .read_exact(&mut buffer)
            .await
            .map_err(|error| {
                Error::Other(format!(
                    "archive stream ended early at {} (uncompressed so far looks fine, \
                 but this file could not be read): {error}; {}",
                    entry.file_name,
                    stream.stderr_text()
                ))
            })?;
        consumed += 1;

        // CRC-32 alignment guard: proves the de-multiplexer is perfectly in
        // sync before any bytes leave the machine.
        if let Some(expected) = entry.crc32 {
            let actual = archive::crc32(&buffer);
            if actual != expected {
                return Err(Error::Verification(format!(
                    "archive entry {} decompressed with CRC mismatch (listing CRC {:08x}, got {:08x}); aborting before any damage to the destination channel",
                    entry.file_name, expected, actual
                )));
            }
        }

        let sha256 = archive::sha256_hex(&buffer);
        let caption = edz_caption(&options.brand, &entry.file_name, &sha256);

        if options.dry_run {
            summary.uploaded += 1;
            summary.bytes_uploaded += entry.size;
            println!(
                "[{position}/{total}] DRY-RUN would upload {} ({} B, sha256 {}…)",
                entry.file_name,
                entry.size,
                &sha256[..16]
            );
            continue;
        }

        match upload_one(connection, options, entry, &buffer, &caption).await {
            Ok((message_id, attempts)) => {
                store.upload_record_posted(
                    &entry.file_name,
                    entry.size,
                    &sha256,
                    message_id,
                    attempts,
                )?;
                summary.uploaded += 1;
                summary.bytes_uploaded += entry.size;
                println!(
                    "[{position}/{total}] posted {} -> message {message_id} (attempt {attempts})",
                    entry.file_name
                );
                tokio::time::sleep(options.delay).await;
            }
            Err(error) => {
                summary.failed += 1;
                let attempts = error.attempts();
                let message = error.into_inner().to_string();
                dump_failed(options, entry, &buffer, &message);
                store.upload_record_failed(
                    &entry.file_name,
                    entry.size,
                    &sha256,
                    attempts,
                    &message,
                )?;
                println!(
                    "[{position}/{total}] FAILED {} -> dumped to {}",
                    entry.file_name,
                    options.failure_dir.display()
                );
            }
        }
    }

    Ok(consumed)
}

// ------------------------------------------------------- parallel mode --

struct DispatchJob {
    position: usize,
    entry: ArchiveEntry,
    buffer: Vec<u8>,
}

#[derive(Default)]
struct SharedCounters {
    uploaded: AtomicU64,
    skipped: AtomicU64,
    failed: AtomicU64,
    bytes_uploaded: AtomicU64,
}

#[allow(clippy::too_many_arguments)]
async fn demux_parallel(
    connection: &TelegramConnection,
    store: &Store,
    stream: &mut archive::StreamingArchive,
    entries: &[ArchiveEntry],
    posted_names: &HashSet<String>,
    inventory: Option<&DestinationInventory>,
    options: &UploadOptions,
    summary: &mut UploadSummary,
) -> Result<usize> {
    let total = entries.len();
    let workers = options.parallel.max(2) as usize;
    let (tx, rx) = tokio::sync::mpsc::channel::<DispatchJob>(3);
    let rx = Arc::new(tokio::sync::Mutex::new(rx));
    let abort = Arc::new(AtomicBool::new(false));
    let counters = Arc::new(SharedCounters::default());

    let decoder = async {
        let mut consumed = 0usize;
        let mut processed = 0u64;
        let mut discarded_only = 0usize;
        for (index, entry) in entries.iter().enumerate() {
            if options.limit > 0 && processed >= options.limit {
                break;
            }
            let position = index + 1;
            let lower = entry.file_name.to_ascii_lowercase();
            let size = entry.size as usize;

            if let Some(only) = &options.only {
                if !only.contains(&lower) {
                    archive::discard_bytes(&mut stream.stdout, entry.size).await?;
                    consumed += 1;
                    discarded_only += 1;
                    if discarded_only % 1000 == 0 {
                        println!("  … {discarded_only} non-selected entries discarded");
                    }
                    continue;
                }
            }

            let already = posted_names.contains(&lower)
                || inventory
                    .map(|inv| inv.is_posted(&entry.file_name, None))
                    .unwrap_or(false);
            if already {
                archive::discard_bytes(&mut stream.stdout, entry.size).await?;
                consumed += 1;
                counters.skipped.fetch_add(1, Ordering::Relaxed);
                println!("[{position}/{total}] skip (already posted) {}", entry.file_name);
                continue;
            }
            processed += 1;

            let mut buffer = vec![0u8; size];
            if let Err(error) = stream.stdout.read_exact(&mut buffer).await {
                abort.store(true, Ordering::SeqCst);
                return Err(Error::Other(format!(
                    "archive stream ended early at {} (uncompressed so far looks fine, \
                     but this file could not be read): {error}; {}",
                    entry.file_name,
                    stream.stderr_text()
                )));
            }
            consumed += 1;

            // CRC-32 alignment guard: nothing leaves the machine until this
            // file's bytes are proven to match the archive listing.
            if let Some(expected) = entry.crc32 {
                let actual = archive::crc32(&buffer);
                if actual != expected {
                    abort.store(true, Ordering::SeqCst);
                    return Err(Error::Verification(format!(
                        "archive entry {} decompressed with CRC mismatch (listing CRC {:08x}, got {:08x}); aborting before any damage to the destination channel",
                        entry.file_name, expected, actual
                    )));
                }
            }

            if tx
                .send(DispatchJob {
                    position,
                    entry: entry.clone(),
                    buffer,
                })
                .await
                .is_err()
            {
                abort.store(true, Ordering::SeqCst);
                return Err(Error::Other(
                    "upload workers terminated unexpectedly".into(),
                ));
            }
        }
        Ok(consumed)
    };

    let worker = |_id: usize| async {
        loop {
            if abort.load(Ordering::SeqCst) {
                return;
            }
            let job = {
                let mut guard = rx.lock().await;
                guard.recv().await
            };
            let Some(DispatchJob {
                position,
                entry,
                buffer,
            }) = job
            else {
                return;
            };
            if abort.load(Ordering::SeqCst) {
                return;
            }

            let sha256 = archive::sha256_hex(&buffer);
            let caption = edz_caption(&options.brand, &entry.file_name, &sha256);

            if options.dry_run {
                counters.uploaded.fetch_add(1, Ordering::Relaxed);
                counters
                    .bytes_uploaded
                    .fetch_add(buffer.len() as u64, Ordering::Relaxed);
                println!(
                    "[{position}/{total}] DRY-RUN would upload {} ({} B, sha256 {}…)",
                    entry.file_name,
                    entry.size,
                    &sha256[..16]
                );
                continue;
            }

            match upload_one(connection, options, &entry, &buffer, &caption).await {
                Ok((message_id, attempts)) => {
                    if let Err(error) = store.upload_record_posted(
                        &entry.file_name,
                        entry.size,
                        &sha256,
                        message_id,
                        attempts,
                    ) {
                        eprintln!("cannot record posted job for {}: {error}", entry.file_name);
                    }
                    counters.uploaded.fetch_add(1, Ordering::Relaxed);
                    counters
                        .bytes_uploaded
                        .fetch_add(entry.size, Ordering::Relaxed);
                    println!(
                        "[{position}/{total}] posted {} -> message {message_id} (attempt {attempts})",
                        entry.file_name
                    );
                    tokio::time::sleep(options.delay).await;
                }
                Err(error) => {
                    let attempts = error.attempts();
                    let message = error.into_inner().to_string();
                    dump_failed(options, &entry, &buffer, &message);
                    if let Err(record_error) = store.upload_record_failed(
                        &entry.file_name,
                        entry.size,
                        &sha256,
                        attempts,
                        &message,
                    ) {
                        eprintln!(
                            "cannot record failed job for {}: {record_error}",
                            entry.file_name
                        );
                    }
                    counters.failed.fetch_add(1, Ordering::Relaxed);
                    println!(
                        "[{position}/{total}] FAILED {} -> dumped to {}",
                        entry.file_name,
                        options.failure_dir.display()
                    );
                }
            }
        }
    };

    let workers_future =
        futures_util::future::join_all((0..workers).map(worker));
    let (decoder_result, _worker_outputs) =
        futures_util::future::join(decoder, workers_future).await;
    let consumed = decoder_result?;

    summary.uploaded = counters.uploaded.load(Ordering::Relaxed);
    summary.skipped = counters.skipped.load(Ordering::Relaxed);
    summary.failed = counters.failed.load(Ordering::Relaxed);
    summary.bytes_uploaded = counters.bytes_uploaded.load(Ordering::Relaxed);

    Ok(consumed)
}

/// Upload one file with bounded retries. Flood waits always honored and do
/// not consume the regular retry budget (a separate, generous cap applies).
async fn upload_one(
    connection: &TelegramConnection,
    options: &UploadOptions,
    entry: &ArchiveEntry,
    buffer: &[u8],
    caption: &str,
) -> std::result::Result<(i32, u32), UploadFailure> {
    let mut attempt = 0u32;
    let mut flood_waits = 0u32;
    const MAX_FLOOD_WAITS: u32 = 8;
    loop {
        let result = send_file(connection, options, entry, buffer, caption).await;
        match result {
            Ok(message_id) => return Ok((message_id, attempt + flood_waits)),
            Err(error) => {
                if let Some(seconds) = error.flood_wait_seconds() {
                    flood_waits += 1;
                    if flood_waits > MAX_FLOOD_WAITS {
                        return Err(UploadFailure {
                            inner: error,
                            attempts: attempt + flood_waits,
                        });
                    }
                    eprintln!(
                        "flood wait {seconds}s for {} (flood #{flood_waits})",
                        entry.file_name
                    );
                    tokio::time::sleep(Duration::from_secs(seconds)).await;
                    continue;
                }
                attempt += 1;
                if attempt < options.max_attempts && error.is_retryable() {
                    let seconds = backoff_seconds(attempt);
                    eprintln!(
                        "retry {attempt}/{} for {} in {seconds}s ({error})",
                        options.max_attempts, entry.file_name
                    );
                    tokio::time::sleep(Duration::from_secs(seconds)).await;
                    continue;
                }
                return Err(UploadFailure {
                    inner: error,
                    attempts: attempt + flood_waits,
                });
            }
        }
    }
}

async fn send_file(
    connection: &TelegramConnection,
    options: &UploadOptions,
    entry: &ArchiveEntry,
    buffer: &[u8],
    caption: &str,
) -> Result<i32> {
    let mut slice: &[u8] = buffer;
    let uploaded = connection
        .client()
        .upload_stream(&mut slice, buffer.len(), entry.file_name.clone())
        .await
        .map_err(|error| {
            let message = error.to_string();
            match parse_flood_wait(&message) {
                Some(seconds) => Error::FloodWait(seconds),
                None => Error::Telegram(message),
            }
        })?;

    let message = InputMessage::new()
        .mime_type("application/octet-stream")
        .text(caption.to_string())
        .file(uploaded);

    let sent = connection
        .client()
        .send_message(options.peer, message)
        .await
        .map_err(telegram::map_invocation)?;
    Ok(sent.id())
}

/// Dump a permanently failing file for later manual handling.
fn dump_failed(options: &UploadOptions, entry: &ArchiveEntry, buffer: &[u8], error: &str) {
    if let Err(create_error) = std::fs::create_dir_all(&options.failure_dir) {
        eprintln!("cannot create failure directory: {create_error}");
        return;
    }
    let safe_name: String = entry
        .file_name
        .chars()
        .map(|c| match c {
            '\\' | '/' | ':' | '*' | '?' | '"' | '<' | '>' | '|' => '_',
            _ => c,
        })
        .collect();
    let target = options.failure_dir.join(&safe_name);
    if let Err(write_error) = std::fs::write(&target, buffer) {
        eprintln!(
            "cannot dump failed file {}: {write_error}",
            target.display()
        );
        return;
    }
    let _ = std::fs::write(target.with_extension("error.txt"), error);
}

/// Exponential backoff with cap, used when the server gives no hint.
fn backoff_seconds(attempt: u32) -> u64 {
    let base = 2u64.saturating_pow(attempt.min(6));
    base.min(120)
}

/// A failure after all retries were exhausted.
struct UploadFailure {
    inner: Error,
    attempts: u32,
}

impl UploadFailure {
    fn attempts(&self) -> u32 {
        self.attempts
    }

    fn into_inner(self) -> Error {
        self.inner
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn caption_matches_oumijia_template() {
        let sha = "7db6e24aa9a21f6ede74022243849aba47571e9a5c954f70d6be16f8dcc864b6";
        let caption = edz_caption("CHINT", "CHT.101041.edz", sha);
        let expected = "EPLAN EDZ | CHINT\n\
                        Part: CHT.101041\n\
                        File: CHT.101041.edz\n\
                        SHA-256: 7db6e24aa9a21f6ede74022243849aba47571e9a5c954f70d6be16f8dcc864b6\n\
                        #CHINT #EPLAN #EDZ #EDZID_7db6e24aa9a21f6e";
        assert_eq!(caption, expected);
    }

    #[test]
    fn caption_round_trips_through_parser() {
        let sha = "a".repeat(64);
        let caption = edz_caption("CHINT", "CHT.101041.edz", &sha);
        let (file, parsed) = parse_caption(&caption).unwrap();
        assert_eq!(file, "CHT.101041.edz");
        assert_eq!(parsed, sha);
    }

    #[test]
    fn caption_for_name_with_spaces_and_dots() {
        let sha = "b".repeat(64);
        let name = "CHT.710020100002139 . .edz";
        let caption = edz_caption("CHINT", name, &sha);
        let expected_part = &name[..name.len() - 4]; // file_name minus the ".edz" suffix
        assert!(caption.contains(&format!("Part: {expected_part}")));
        assert!(caption.contains(&format!("File: {name}")));
        assert!(caption.ends_with("#CHINT #EPLAN #EDZ #EDZID_bbbbbbbbbbbbbbbb"));
    }

    #[test]
    fn parser_rejects_garbage() {
        assert!(parse_caption("hello").is_none());
        assert!(parse_caption("File: x.edz\nSHA-256: xyz").is_none());
        assert!(parse_caption("File: \nSHA-256: ").is_none());
    }

    #[test]
    fn tag_and_identifier() {
        assert_eq!(archive::tag("CHINT"), "CHINT");
        let sha = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
        assert_eq!(&sha[..16], "0123456789abcdef");
    }
}
