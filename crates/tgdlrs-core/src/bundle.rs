//! Bundle creation and publishing for the "post everything as a few big
//! 7z archives" path.
//!
//! The source is the same split, solid `CHINT.7z` archive used for the
//! direct-upload pipeline. We decode it **once** (the same de-multiplexer),
//! stage each remaining `.edz` into a per-bucket directory, and — once a
//! bucket accumulates ~1.8 GiB — pack it as `CHNedz<N>.7z` with `-mx=0`.
//! Packed bundles are handed off to a bounded worker pool (default 2)
//! that uploads them as no-caption documents and deletes each bundle after
//! Telegram confirms the post.
//!
//! Crash-resume: every bundle name is recorded in `upload_jobs`, so on the
//! next run we look up the first bundle index whose name is not recorded as
//! posted and continue from there (uploading leftover on-disk bundles first).
//! Bundles are published only once per message.
//!
//! Integrity: CRC-32 per archive entry (before staging), and SHA-256 per
//! bundle (recorded in `upload_jobs` for later verification passes).

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use grammers_client::message::InputMessage;
use grammers_session::types::PeerRef;
use serde::Serialize;
use std::sync::Mutex as StdMutex;
use tokio::io::AsyncReadExt;
use tokio::sync::{mpsc, Mutex as AsyncMutex};

use crate::archive::{self, ArchiveEntry};
use crate::database::Store;
use crate::error::{parse_flood_wait, Error, Result};
use crate::telegram::{self, TelegramConnection};

/// Default bucket size: 1.8 GiB (safely under the ~2 GB per-upload cap).
pub const DEFAULT_BUCKET_BYTES: u64 = 1_932_735_283; // 1.8 * 1024^3

/// Options for one bundle run.
pub struct BundleOptions {
    /// First volume of the split archive.
    pub archive_volume: PathBuf,
    /// Path to 7z.exe.
    pub seven_zip: PathBuf,
    /// Skip this many entries (archive order) before writing a single byte.
    pub skip_entries: usize,
    /// Directory for finished bundles (and temporary staging).
    pub output_dir: PathBuf,
    /// Bucket size cap in bytes.
    pub bucket_bytes: u64,
    /// Bundle file name prefix (no extension), e.g. `CHNedz`.
    pub name_prefix: String,
    /// Destination peer reference; `None` = pack only, do not upload.
    pub peer: Option<PeerRef>,
    /// Label for logs.
    pub dest_label: Option<String>,
    /// Pacing sleep after each successful post.
    pub delay: Duration,
    /// Retries per bundle before giving up (flood waits do not count).
    pub max_attempts: u32,
    /// Keep bundles on disk after they are posted (default: delete).
    pub keep_after_post: bool,
    /// Stop after publishing (or creating) this many bundles (0 = no limit).
    pub max_bundles: u64,
    /// Number of parallel upload workers (default 2).
    pub parallel: u32,
}

/// Final counters for one bundle run.
#[derive(Clone, Debug, Default, Serialize)]
pub struct BundleSummary {
    pub entries_processed: u64,
    pub bundles_created: u64,
    pub bundles_posted: u64,
    pub bytes_written: u64,
    pub elapsed_secs: u64,
}

/// Options for publishing a range of pre-built `.7z` bundles.
pub struct BundleRangeOptions {
    /// Directory containing the bundles.
    pub output_dir: PathBuf,
    /// File name prefix for bundles (no extension), e.g. `CHNedz`.
    pub name_prefix: String,
    /// Lowest bundle index (inclusive).
    pub from_index: u64,
    /// Highest bundle index (inclusive).
    pub to_index: u64,
    /// Keep bundles on disk after they are posted (default: delete).
    pub keep_after_post: bool,
    /// Pacing sleep after each successful post.
    pub delay: Duration,
    /// Retries per bundle before giving up (flood waits do not count).
    pub max_attempts: u32,
    /// Number of parallel upload workers.
    pub parallel: u32,
}

/// Final counters for one range upload run.
#[derive(Clone, Debug, Default, Serialize)]
pub struct BundleRangeSummary {
    pub bundles_posted: u64,
    pub bundles_skipped: u64,
    pub bytes_written: u64,
    pub elapsed_secs: u64,
}

struct WorkerCounters {
    posted: AtomicU64,
    failed: AtomicU64,
    bytes_written: AtomicU64,
    errors: StdMutex<Vec<String>>,
}

impl WorkerCounters {
    fn new() -> Self {
        Self {
            posted: AtomicU64::new(0),
            failed: AtomicU64::new(0),
            bytes_written: AtomicU64::new(0),
            errors: StdMutex::new(Vec::new()),
        }
    }

    fn total_bytes(&self) -> u64 {
        self.bytes_written.load(Ordering::Relaxed)
    }
}

/// Output file name for a bucket: `<prefix><n>.7z`.
fn final_name(index: u64, prefix: &str) -> String {
    format!("{prefix}{index}.7z")
}

/// True when adding `entry_size` more bytes to a bucket can fit into `cap`.
/// An empty bucket accepts anything (an oversized first file still fits).
fn fits_in_bucket(bucket_bytes: u64, entry_size: u64, cap: u64) -> bool {
    bucket_bytes == 0 || bucket_bytes.saturating_add(entry_size) <= cap
}

/// First bundle index (1-based) whose file name is not in `posted`.
fn first_unposted_index(posted: &HashSet<String>, prefix: &str) -> u64 {
    let mut index = 1u64;
    loop {
        let name = final_name(index, prefix).to_ascii_lowercase();
        if !posted.contains(&name) {
            return index;
        }
        index += 1;
    }
}

/// SHA-256 of an entire file (single pass).
fn sha256_file(path: &Path) -> Result<String> {
    use sha2::Digest;
    let mut hasher = sha2::Sha256::new();
    let mut file = std::fs::File::open(path)?;
    std::io::copy(&mut file, &mut hasher)?;
    Ok(hex::encode(hasher.finalize()))
}

/// Stage one file into a bucket directory.
fn stage_file(dir: &Path, entry: &ArchiveEntry, bytes: &[u8]) -> Result<()> {
    std::fs::create_dir_all(dir)?;
    std::fs::write(dir.join(&entry.file_name), bytes)?;
    Ok(())
}

/// Pack the staged files inside `staging_dir` into `<output_dir>/<prefix><index>.7z`.
async fn pack_bucket(
    seven_zip: &Path,
    staging_dir: &Path,
    output_dir: &Path,
    prefix: &str,
    index: u64,
) -> Result<(PathBuf, u64)> {
    let bundle_path = output_dir.join(final_name(index, prefix));
    if bundle_path.exists() {
        std::fs::remove_file(&bundle_path)?;
    }
    let mut cmd = std::process::Command::new(seven_zip);
    cmd.arg("a")
        .arg("-mx=0") // content already LZMA2-compressed
        .arg("-y")
        .arg(&bundle_path);
    for file in std::fs::read_dir(staging_dir)? {
        let file = file?;
        cmd.arg(file.path());
    }

    let output = cmd.output().map_err(|error| {
        Error::Other(format!(
            "cannot start 7-Zip for {}: {error}",
            bundle_path.display()
        ))
    })?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let code = output.status.code();
        return Err(Error::Other(format!(
            "7-Zip failed to build {} (exit {code:?}): {}",
            bundle_path.display(),
            stderr.chars().take(500).collect::<String>()
        )));
    }
    let size = std::fs::metadata(&bundle_path)?.len();
    if size > 2_000_000_000 {
        // Telegram refuses files this large; never hand them to the uploader.
        std::fs::remove_file(&bundle_path).ok();
        return Err(Error::Other(format!(
            "bundle {} exceeds Telegram's 2 GB cap ({} bytes); removed",
            bundle_path.display(),
            size
        )));
    }
    Ok((bundle_path, size))
}

/// Upload one bundle as a no-caption document; returns `(message_id, attempts_used)`.
async fn publish_bundle(
    connection: &TelegramConnection,
    peer: PeerRef,
    bundle_path: &Path,
    max_attempts: u32,
) -> std::result::Result<(i32, u32), Error> {
    let mut attempt = 0u32;
    let mut flood_waits = 0u32;
    const MAX_FLOOD_WAITS: u32 = 8;
    loop {
        // Step 1: upload the bytes to Telegram's file store.
        let uploaded = match connection.client().upload_file(bundle_path).await {
            Ok(uploaded) => uploaded,
            Err(error) => {
                if let Some(seconds) = parse_flood_wait(&error.to_string()) {
                    flood_waits += 1;
                    if flood_waits > MAX_FLOOD_WAITS {
                        return Err(Error::FloodWait(seconds));
                    }
                    eprintln!(
                        "flood wait {seconds}s for {} (flood #{flood_waits})",
                        bundle_path.display()
                    );
                    tokio::time::sleep(Duration::from_secs(seconds)).await;
                    continue;
                }
                return Err(Error::Telegram(error.to_string()));
            }
        };

        // Step 2: attach as a no-caption document and send it.
        let message = InputMessage::new().file(uploaded);
        match connection.client().send_message(peer, message).await {
            Ok(sent) => return Ok((sent.id(), attempt + flood_waits)),
            Err(invocation) => {
                let error = telegram::map_invocation(invocation);
                if let Some(seconds) = error.flood_wait_seconds() {
                    flood_waits += 1;
                    if flood_waits > MAX_FLOOD_WAITS {
                        return Err(error);
                    }
                    eprintln!(
                        "flood wait {seconds}s for {} (flood #{flood_waits})",
                        bundle_path.display()
                    );
                    tokio::time::sleep(Duration::from_secs(seconds)).await;
                    continue;
                }
                attempt += 1;
                if attempt < max_attempts && error.is_retryable() {
                    let seconds = 2u64.saturating_pow(attempt.min(6)).min(120);
                    eprintln!(
                        "retry {attempt}/{max_attempts} for {} in {seconds}s ({error})",
                        bundle_path.display()
                    );
                    tokio::time::sleep(Duration::from_secs(seconds)).await;
                    continue;
                }
                return Err(error);
            }
        }
    }
}

/// Upload one bundle with crash-resume handling and DB checkpointing.
async fn worker_upload_one(
    connection: &TelegramConnection,
    store: &Store,
    peer: PeerRef,
    bundle_path: &Path,
    options: &BundleOptions,
) -> Result<()> {
    let name = bundle_path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("bundle.7z")
        .to_string();
    let lower = name.to_ascii_lowercase();
    let size = std::fs::metadata(bundle_path)?.len();

    let posted = store.upload_posted_names()?;
    if posted.contains(&lower) {
        println!("[worker] {name} already posted (recorded); deleting leftover file");
        std::fs::remove_file(bundle_path)?;
        return Ok(());
    }

    let bundle_sha = sha256_file(bundle_path)?;
    match publish_bundle(connection, peer, bundle_path, options.max_attempts).await {
        Ok((message_id, attempts)) => {
            store.upload_record_posted(&name, size, &bundle_sha, message_id, attempts)?;
            println!("[worker] posted {name} -> message {message_id} ({} MiB)", size / (1024 * 1024));
            if !options.keep_after_post {
                std::fs::remove_file(bundle_path)?;
            }
            Ok(())
        }
        Err(error) => Err(Error::Other(format!("bundle upload failed for {name}: {error}"))),
    }
}

/// Discard exactly `size` bytes from the stream.
async fn discard_bytes(reader: &mut tokio::io::BufReader<tokio::process::ChildStdout>, size: u64) -> Result<u64> {
    use tokio::io::AsyncReadExt;
    let mut taken = reader.take(size);
    let mut sink = tokio::io::sink();
    let copied = tokio::io::copy(&mut taken, &mut sink).await?;
    Ok(copied)
}

/// Read exactly one entry from the stream and return its bytes.
async fn read_exact_entry(
    stream: &mut archive::StreamingArchive,
    entry: &ArchiveEntry,
) -> Result<Vec<u8>> {
    let mut buffer = vec![0u8; entry.size as usize];
    stream.stdout.read_exact(&mut buffer).await.map_err(|error| {
        Error::Other(format!(
            "archive stream ended early at {}: {error}; {}",
            entry.file_name,
            stream.stderr_text()
        ))
    })?;
    Ok(buffer)
}

/// One bucket staging area.
struct Bucket {
    staging: PathBuf,
    bytes: u64,
    files: u64,
}

impl Bucket {
    fn new(staging_root: &Path, index: u64) -> Result<Self> {
        let staging = staging_root.join(format!("{index}"));
        if staging.exists() {
            std::fs::remove_dir_all(&staging)?;
        }
        std::fs::create_dir_all(&staging)?;
        Ok(Self {
            staging,
            bytes: 0,
            files: 0,
        })
    }
}

/// Run the full bundle pipeline.
pub async fn run_bundle(
    connection: TelegramConnection,
    store: Store,
    options: BundleOptions,
) -> Result<BundleSummary> {
    let started = Instant::now();
    let entries = archive::list_entries(&options.seven_zip, &options.archive_volume)?;
    let total = entries.len();
    let skip = options.skip_entries.min(total);
    println!(
        "bundle: {total} entries, skipping the first {skip}; {} remaining",
        total.saturating_sub(skip)
    );

    let posted_names = store.upload_posted_names()?;
    let mut bundle_index = first_unposted_index(&posted_names, &options.name_prefix);
    println!("bundle: first unposted index is {bundle_index}");

    std::fs::create_dir_all(&options.output_dir)?;
    let staging_root = options.output_dir.join("_staging");
    if staging_root.exists() {
        std::fs::remove_dir_all(&staging_root)?;
    }
    std::fs::create_dir_all(&staging_root)?;

    // Upload path: spawn a worker pool fed by an mpsc channel of packed bundles.
    let peer_ref = options.peer;
    let options = Arc::new(options);
    let connection = connection;
    let store = Arc::new(store);

    let mut worker_handles = Vec::new();
    let tx = if let Some(peer) = peer_ref {
        let (tx, rx) = mpsc::channel::<PathBuf>(4);
        let mutex_rx = Arc::new(AsyncMutex::new(rx));
        let connection = Arc::new(connection);
        let store = Arc::clone(&store);
        let options = Arc::clone(&options);
        let counters = Arc::new(WorkerCounters::new());
        let abort = Arc::new(AtomicBool::new(false));
        for _ in 0..options.parallel.max(1) {
            let connection = Arc::clone(&connection);
            let store = Arc::clone(&store);
            let options = Arc::clone(&options);
            let counters = Arc::clone(&counters);
            let abort = Arc::clone(&abort);
            let rx = Arc::clone(&mutex_rx);
            worker_handles.push(tokio::spawn(async move {
                // Workers share one receiver via a mutex — each worker polls.
                loop {
                    let next = {
                        let mut guard = rx.lock().await;
                        guard.recv().await
                    };
                    let Some(bundle_path) = next else {
                        return;
                    };
                    if abort.load(Ordering::Relaxed) {
                        return;
                    }
                    let bytes_before = std::fs::metadata(&bundle_path).map(|m| m.len()).unwrap_or(0);
                    match worker_upload_one(
                        connection.as_ref(),
                        store.as_ref(),
                        peer,
                        &bundle_path,
                        options.as_ref(),
                    )
                    .await
                    {
                        Ok(()) => {
                            counters.posted.fetch_add(1, Ordering::Relaxed);
                            counters.bytes_written.fetch_add(bytes_before, Ordering::Relaxed);
                        }
                        Err(error) => {
                            eprintln!("[worker] FAILED bundle {}: {error}", bundle_path.display());
                            counters.failed.fetch_add(1, Ordering::Relaxed);
                            if let Ok(mut guard) = counters.errors.lock() {
                                guard.push(error.to_string());
                            }
                            abort.store(true, Ordering::Relaxed);
                            return;
                        }
                    }
                    tokio::time::sleep(options.delay).await;
                }
            }));
        }
        Some((tx, counters, abort))
    } else {
        // Pack-only: no upload workers.
        None
    };

    let mut stream = archive::spawn_stream(&options.seven_zip, &options.archive_volume).await?;
    let mut summary = BundleSummary::default();
    let pack_only = options.peer.is_none();

    // Drain leftover bundles from the previous run (recorded as posted skip).
    let mut bucket = Bucket::new(&staging_root, bundle_index)?;
    let mut result: Result<()> = Ok(());
    let mut ended_early = false;

    for (entry_index, entry) in entries.iter().enumerate() {
        if entry_index < skip {
            if let Err(error) = discard_bytes(&mut stream.stdout, entry.size).await {
                result = Err(error);
                ended_early = true;
                break;
            }
            continue;
        }
        if options.max_bundles > 0 && summary.bundles_created >= options.max_bundles {
            println!("reached --max-bundles {}; stopping here", options.max_bundles);
            ended_early = true;
            break;
        }
        let bytes = match read_exact_entry(&mut stream, entry).await {
            Ok(bytes) => bytes,
            Err(error) => {
                result = Err(error);
                ended_early = true;
                break;
            }
        };

        if let Some(expected) = entry.crc32 {
            let actual = archive::crc32(&bytes);
            if actual != expected {
                result = Err(Error::Verification(format!(
                    "archive entry {} CRC mismatch (expected {:08x}, got {:08x}); aborting before packing or uploading anything",
                    entry.file_name, expected, actual
                )));
                ended_early = true;
                break;
            }
        }

        if !fits_in_bucket(bucket.bytes, entry.size, options.bucket_bytes) && bucket.files > 0 {
            // The current bucket is full: pack it and hand the result to the workers.
            match pack_and_handoff(
                &options,
                &bucket,
                bundle_index,
                tx.as_ref().map(|(tx, _, _)| tx),
                pack_only,
                &mut summary,
            )
            .await
            {
                Ok(()) => {
                    bundle_index += 1;
                    bucket = Bucket::new(&staging_root, bundle_index)?;
                }
                Err(error) => {
                    result = Err(error);
                    ended_early = true;
                    break;
                }
            }
        }

        stage_file(&bucket.staging, entry, &bytes)?;
        bucket.bytes += entry.size;
        bucket.files += 1;
        summary.entries_processed += 1;
    }

    // Final full bucket (or the partial trailing one, if not asked to stop early).
    if result.is_ok() && bucket.files > 0
        && (options.max_bundles == 0 || summary.bundles_created < options.max_bundles)
    {
        result = pack_and_handoff(
            &options,
            &bucket,
            bundle_index,
            tx.as_ref().map(|(tx, _, _)| tx),
            pack_only,
            &mut summary,
        )
        .await;
    } else if bucket.files > 0 {
        println!("reached --max-bundles {} — leaving partial staging bucket", options.max_bundles);
    }

    // Drain the channel: wait for all workers to finish uploading.
    if let Some((tx, counters, _abort)) = tx {
        drop(tx);
        for handle in worker_handles {
            let _ = handle.await;
        }
        summary.bundles_posted = counters.posted.load(Ordering::Relaxed);
        summary.bytes_written = counters.total_bytes();
        let failed = counters.failed.load(Ordering::Relaxed);
        let errors: Vec<String> = counters.errors.lock().ok().map(|g| g.clone()).unwrap_or_default();
        if failed > 0 {
            return Err(Error::Other(format!(
                "{failed} bundles failed to publish; last error: {}",
                errors.last().cloned().unwrap_or_default()
            )));
        }
    }

    let _ = std::fs::remove_dir_all(&staging_root);

    if ended_early {
        stream.stop().await;
    } else {
        if let Ok(Some(code)) = stream.wait().await {
            if code != 0 {
                return Err(Error::Other(format!(
                    "7-Zip stream exited with code {code}"
                )));
            }
        }
    }

    result?;
    summary.elapsed_secs = started.elapsed().as_secs();
    Ok(summary)
}

/// Pack one staging bucket and hand the result off to the upload workers
/// (or just print it when in pack-only mode).
async fn pack_and_handoff(
    options: &Arc<BundleOptions>,
    bucket: &Bucket,
    bundle_index: u64,
    tx: Option<&mpsc::Sender<PathBuf>>,
    pack_only: bool,
    summary: &mut BundleSummary,
) -> Result<()> {
    let name = final_name(bundle_index, &options.name_prefix);
    println!(
        "packing bucket {bundle_index} -> {name} ({} files, {} MiB)…",
        bucket.files,
        bucket.bytes / (1024 * 1024)
    );
    let (bundle_path, size) =
        pack_bucket(&options.seven_zip, &bucket.staging, &options.output_dir, &options.name_prefix, bundle_index)
            .await?;
    summary.bundles_created += 1;
    let _ = std::fs::remove_dir_all(&bucket.staging);
    if pack_only {
        println!("created {name} ({} MiB)", size / (1024 * 1024));
        summary.bytes_written += size;
        return Ok(());
    }
    if let Some(tx) = tx {
        tx.send(bundle_path).await.map_err(|error| {
            Error::Other(format!("upload channel closed before sending {name}: {error}"))
        })?;
        summary.bytes_written += size;
    }
    Ok(())
}

/// Publish a numbered range of pre-built `.7z` bundles to Telegram.
/// Returns counters; the destination must already be resolved/created.
pub async fn upload_bundles_range(
    connection: TelegramConnection,
    store: Store,
    peer: PeerRef,
    options: BundleRangeOptions,
) -> Result<BundleRangeSummary> {
    let started = Instant::now();

    // 1) Gather every bundle file in the requested range.
    let mut candidate: Vec<(u64, PathBuf)> = Vec::new();
    for index in options.from_index..=options.to_index {
        let name = final_name(index, &options.name_prefix);
        let path = options.output_dir.join(&name);
        if path.is_file() {
            candidate.push((index, path));
        } else {
            eprintln!("bundle {index}: {} not found, skipping", path.display());
        }
    }
    if candidate.is_empty() {
        println!("no bundles found for range {}–{}", options.from_index, options.to_index);
        return Ok(BundleRangeSummary {
            elapsed_secs: started.elapsed().as_secs(),
            ..Default::default()
        });
    }
    println!(
        "bundle-upload: {} candidate(s) in range CHNedz{}..CHNedz{}",
        candidate.len(),
        options.from_index,
        options.to_index
    );

    // 2) Filter out everything already recorded as posted.
    let posted = store.upload_posted_names()?;
    let mut work: Vec<(u64, PathBuf)> = Vec::with_capacity(candidate.len());
    let mut skipped = 0u64;
    for (index, path) in candidate {
        let name = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("bundle.7z")
            .to_string();
        if posted.contains(&name.to_ascii_lowercase()) {
            println!("skip {} — already posted", name);
            skipped += 1;
        } else {
            work.push((index, path));
        }
    }
    if work.is_empty() {
        println!("everything in the requested range is already posted");
        return Ok(BundleRangeSummary {
            bundles_skipped: skipped,
            elapsed_secs: started.elapsed().as_secs(),
            ..Default::default()
        });
    }
    println!("bundle-upload: uploading {} bundle(s), {skipped} skipped", work.len());

    // 3) Spawn a small worker pool; workers share the receiver behind a mutex.
    let parallel = options.parallel.max(1);
    let connection = Arc::new(connection);
    let store = Arc::new(store);
    let options = Arc::new(options);

    let (tx, rx) = mpsc::channel::<(u64, PathBuf)>(4);
    let rx = Arc::new(AsyncMutex::new(rx));
    let counters = Arc::new(WorkerCounters::new());
    let abort = Arc::new(AtomicBool::new(false));

    let mut handles = Vec::with_capacity(parallel as usize);
    for _ in 0..parallel {
        let connection = Arc::clone(&connection);
        let store = Arc::clone(&store);
        let options = Arc::clone(&options);
        let counters = Arc::clone(&counters);
        let abort = Arc::clone(&abort);
        let rx = Arc::clone(&rx);
        handles.push(tokio::spawn(async move {
            loop {
                let next = {
                    let mut guard = rx.lock().await;
                    guard.recv().await
                };
                let Some((_index, path)) = next else {
                    return;
                };
                if abort.load(Ordering::Relaxed) {
                    return;
                }
                let name = path
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or("bundle.7z")
                    .to_string();
                let size = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
                // Hash to record the digest in the database.
                let bundle_sha = match sha256_file(&path) {
                    Ok(sha) => sha,
                    Err(error) => {
                        eprintln!("FAILED to hash {name}: {error}");
                        counters.failed.fetch_add(1, Ordering::Relaxed);
                        abort.store(true, Ordering::Relaxed);
                        return;
                    }
                };
                match publish_bundle(connection.as_ref(), peer, &path, options.max_attempts).await {
                    Ok((message_id, attempts)) => {
                        if let Err(error) = store.upload_record_posted(&name, size, &bundle_sha, message_id, attempts) {
                            eprintln!("FAILED to record {name}: {error}");
                        } else {
                            println!("posted {name} -> message {message_id} ({} MiB)", size / (1024 * 1024));
                        }
                        counters.posted.fetch_add(1, Ordering::Relaxed);
                        counters.bytes_written.fetch_add(size, Ordering::Relaxed);
                        if !options.keep_after_post {
                            let _ = std::fs::remove_file(&path);
                        }
                    }
                    Err(error) => {
                        eprintln!("FAILED to publish {name}: {error}");
                        counters.failed.fetch_add(1, Ordering::Relaxed);
                        if let Ok(mut guard) = counters.errors.lock() {
                            guard.push(error.to_string());
                        }
                        abort.store(true, Ordering::Relaxed);
                        return;
                    }
                }
                tokio::time::sleep(options.delay).await;
            }
        }));
    }

    for (index, path) in work {
        if tx.send((index, path)).await.is_err() {
            break;
        }
    }
    drop(tx);
    for handle in handles {
        let _ = handle.await;
    }

    let posted_count = counters.posted.load(Ordering::Relaxed);
    let failed = counters.failed.load(Ordering::Relaxed);
    let bytes_total = counters.total_bytes();
    let errors: Vec<String> = counters.errors.lock().ok().map(|g| g.clone()).unwrap_or_default();

    if failed > 0 {
        return Err(Error::Other(format!(
            "{failed} bundle(s) failed to publish; last error: {}",
            errors.last().cloned().unwrap_or_default()
        )));
    }

    Ok(BundleRangeSummary {
        bundles_posted: posted_count,
        bundles_skipped: skipped,
        bytes_written: bytes_total,
        elapsed_secs: started.elapsed().as_secs(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn final_name_matches_expected() {
        assert_eq!(final_name(1, "CHNedz"), "CHNedz1.7z");
        assert_eq!(final_name(42, "CHNedz"), "CHNedz42.7z");
    }

    #[test]
    fn first_unposted_skips_only_posted() {
        let posted: HashSet<String> = ["chnedz1.7z", "chnedz2.7z", "chnedz3.7z"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert_eq!(first_unposted_index(&posted, "CHNedz"), 4);
        assert_eq!(first_unposted_index(&HashSet::new(), "CHNedz"), 1);
    }

    #[test]
    fn bucket_fit_boundary() {
        assert!(fits_in_bucket(0, 1, 10)); // oversized into an empty bucket is fine
        assert!(fits_in_bucket(5, 5, 10)); // exactly fills the bucket
        assert!(!fits_in_bucket(6, 5, 10)); // would overflow
        assert!(!fits_in_bucket(1, u64::MAX, 10)); // saturates, still false
    }
}
