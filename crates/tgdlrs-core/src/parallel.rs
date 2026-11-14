//! Parallel range downloader.
//!
//! Splits the file into N chunk-aligned contiguous ranges and downloads them
//! concurrently, each with its own streaming source (own Telegram connection).
//! Every range resumes independently from a `<file>.part.ranges.json` sidecar,
//! so killing the worker mid-flight loses at most [`STATE_SYNC_BYTES`] of
//! progress per range.
//!
//! Bytes from different Telegram messages are never concatenated; each range
//! seeks to its absolute offset before writing. Integrity is enforced at the
//! end by the caller (SHA-256), exactly as for the serial path.

use std::fs::OpenOptions;
use std::io::{Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::config::TELEGRAM_CHUNK_SIZE;
use crate::downloader::{
    download_part, ChunkSource, ControlSignal, DownloadControl, DownloadOutcome, RetryPolicy,
};
use crate::error::{Error, Result};
use crate::resume::{plan_resume, ResumeReason};

const STATE_SYNC_BYTES: u64 = 8 * 1024 * 1024;
const STATE_SYNC_INTERVAL: Duration = Duration::from_secs(2);
const PROGRESS_INTERVAL: Duration = Duration::from_millis(250);

fn truncate_and_sync(part_path: &Path, len: u64) -> Result<()> {
    let file = std::fs::OpenOptions::new().write(true).open(part_path)?;
    file.set_len(len)?;
    file.sync_all()?;
    Ok(())
}

#[derive(Serialize, Deserialize)]
struct RangeState {
    ranges: Vec<u64>,
}

fn state_path(part_path: &Path) -> PathBuf {
    let mut path = part_path.as_os_str().to_os_string();
    path.push(".ranges.json");
    PathBuf::from(path)
}

fn load_state(path: &Path, expected_ranges: u32) -> Option<Vec<u64>> {
    let raw = std::fs::read(path).ok()?;
    let parsed: RangeState = serde_json::from_slice(&raw).ok()?;
    if parsed.ranges.len() != expected_ranges as usize {
        return None;
    }
    Some(parsed.ranges)
}

fn save_state_atomic(path: &Path, ranges: &[u64]) -> Result<()> {
    let tmp = path.with_extension("tmp");
    std::fs::write(
        &tmp,
        serde_json::to_vec(&RangeState {
            ranges: ranges.to_vec(),
        })?,
    )?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

struct Shared<F, G>
where
    F: FnMut(u64, Option<u64>) -> Result<()> + Send,
    G: FnMut(u32, Duration, &Error) + Send,
{
    bytes: AtomicU64,
    saved_at_bytes: AtomicU64,
    saved_at: Mutex<Instant>,
    progress_at: Mutex<Instant>,
    ranges: Mutex<Vec<u64>>,
    failed: AtomicBool,
    first_error: Mutex<Option<Error>>,
    on_progress: Mutex<F>,
    on_retry: Mutex<G>,
}

struct RangeTask<F, G>
where
    F: FnMut(u64, Option<u64>) -> Result<()> + Send,
    G: FnMut(u32, Duration, &Error) + Send,
{
    index: usize,
    range_start: u64,
    range_end: u64,
    resume_local: u64,
    part_path: PathBuf,
    state_path: PathBuf,
    total: u64,
    shared: Arc<Shared<F, G>>,
    control: DownloadControl,
    retry: RetryPolicy,
    make_source: Arc<dyn Fn(u64) -> Result<Box<dyn ChunkSource>> + Send + Sync>,
}

struct RangeCtx<'a, F, G>
where
    F: FnMut(u64, Option<u64>) -> Result<()> + Send,
    G: FnMut(u32, Duration, &Error) + Send,
{
    index: usize,
    range_start: u64,
    range_end: u64,
    resume_local: u64,
    part_path: &'a Path,
    state_path: &'a Path,
    total: u64,
    shared: &'a Shared<F, G>,
    control: &'a DownloadControl,
    retry: &'a RetryPolicy,
    make_source: &'a Arc<dyn Fn(u64) -> Result<Box<dyn ChunkSource>> + Send + Sync>,
}

async fn range_task<F, G>(args: RangeTask<F, G>)
where
    F: FnMut(u64, Option<u64>) -> Result<()> + Send,
    G: FnMut(u32, Duration, &Error) + Send,
{
    let shared = Arc::clone(&args.shared);
    let outcome = {
        let ctx = RangeCtx {
            index: args.index,
            range_start: args.range_start,
            range_end: args.range_end,
            resume_local: args.resume_local,
            part_path: &args.part_path,
            state_path: &args.state_path,
            total: args.total,
            shared: &args.shared,
            control: &args.control,
            retry: &args.retry,
            make_source: &args.make_source,
        };
        range_task_inner(ctx).await
    };
    if let Err(error) = outcome {
        if !matches!(error, Error::Paused | Error::Canceled) {
            shared.failed.store(true, Ordering::SeqCst);
            let mut slot = shared.first_error.lock().unwrap();
            if slot.is_none() {
                *slot = Some(error);
            }
        }
    }
}

async fn range_task_inner<F, G>(ctx: RangeCtx<'_, F, G>) -> Result<()>
where
    F: FnMut(u64, Option<u64>) -> Result<()> + Send,
    G: FnMut(u32, Duration, &Error) + Send,
{
    let RangeCtx {
        index,
        range_start,
        range_end,
        resume_local,
        part_path,
        state_path,
        total,
        shared,
        control,
        retry,
        make_source,
    } = ctx;

    let mut file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(part_path)?;
    file.seek(SeekFrom::Start(range_start + resume_local))?;

    let mut local = resume_local;
    let mut flood_waits = 0u32;
    let mut retries = 0u32;
    let mut source = (make_source)(range_start + local)?;

    loop {
        match control.check() {
            ControlSignal::Pause => {
                file.sync_all()?;
                return Err(Error::Paused);
            }
            ControlSignal::Cancel => {
                file.sync_all()?;
                return Err(Error::Canceled);
            }
            ControlSignal::Continue => {}
        }
        if shared.failed.load(Ordering::SeqCst) {
            return Err(Error::Download("another range failed".into()));
        }

        let remaining = range_end - (range_start + local);
        if remaining == 0 {
            break;
        }

        match source.next_chunk().await {
            Ok(None) => {
                // The iterator ended before the range was complete. This can
                // happen when Telegram terminates a response early (for
                // example, an expired file reference that ends the stream
                // instead of erroring). Re-fetch a fresh reference at the
                // current offset instead of pretending the range is done.
                if retries >= retry.max_attempts {
                    return Err(Error::Download(format!(
                        "connection ended at offset {} with {} bytes remaining",
                        range_start + local,
                        range_end - (range_start + local)
                    )));
                }
                retries += 1;
                let delay = retry.delay_for(retries);
                shared.on_retry.lock().unwrap()(
                    retries,
                    delay,
                    &Error::Download(format!(
                        "connection ended at offset {} with {} bytes remaining",
                        range_start + local,
                        range_end - (range_start + local)
                    )),
                );
                tokio::time::sleep(delay).await;
                source = (make_source)(range_start + local)?;
                continue;
            }
            Ok(Some(mut chunk)) => {
                if chunk.is_empty() {
                    file.sync_all()?;
                    return Err(Error::Download(format!(
                        "Telegram returned an empty chunk at offset {}",
                        range_start + local
                    )));
                }
                if chunk.len() as u64 > remaining {
                    chunk.truncate(remaining as usize);
                }
                file.write_all(&chunk)?;
                let bytes = chunk.len() as u64;
                local += bytes;
                let now_total = shared.bytes.fetch_add(bytes, Ordering::SeqCst) + bytes;
                {
                    let mut ranges = shared.ranges.lock().unwrap();
                    ranges[index] = local;
                }

                let due = now_total.saturating_sub(shared.saved_at_bytes.load(Ordering::SeqCst))
                    >= STATE_SYNC_BYTES
                    || shared.saved_at.lock().unwrap().elapsed() >= STATE_SYNC_INTERVAL;
                if due {
                    let ranges = shared.ranges.lock().unwrap().clone();
                    save_state_atomic(state_path, &ranges)?;
                    shared.saved_at_bytes.store(now_total, Ordering::SeqCst);
                    *shared.saved_at.lock().unwrap() = Instant::now();
                    file.sync_all()?;
                }

                let mut progress = shared.progress_at.lock().unwrap();
                if progress.elapsed() >= PROGRESS_INTERVAL {
                    *progress = Instant::now();
                    let mut cb = shared.on_progress.lock().unwrap();
                    cb(now_total, Some(total))?;
                }
            }
            Err(error) => {
                let _ = file.sync_all();
                if let Some(seconds) = error.flood_wait_seconds() {
                    flood_waits += 1;
                    if flood_waits > 100 {
                        return Err(error);
                    }
                    tracing::warn!(seconds, "Telegram flood wait (parallel range)");
                    tokio::time::sleep(Duration::from_secs(seconds.saturating_add(1))).await;
                } else {
                    if !error.is_retryable() {
                        return Err(error);
                    }
                    retries += 1;
                    if retries > retry.max_attempts {
                        return Err(error);
                    }
                    let delay = retry.delay_for(retries);
                    shared.on_retry.lock().unwrap()(retries, delay, &error);
                    tokio::time::sleep(delay).await;
                }
                source = (make_source)(range_start + local)?;
            }
        }
    }

    // Persist this range's completion before returning.
    file.sync_all()?;
    let ranges = shared.ranges.lock().unwrap().clone();
    save_state_atomic(state_path, &ranges)?;
    Ok(())
}

/// Download (or resume) a file in parallel ranges.
///
/// Falls back to the serial [`download_part`] when the total size is unknown
/// or only one range is requested.
#[allow(clippy::too_many_arguments)]
pub async fn download_part_parallel(
    part_path: &Path,
    expected_size: Option<u64>,
    ranges: u32,
    make_source: impl Fn(u64) -> Result<Box<dyn ChunkSource>> + Send + Sync + 'static,
    on_progress: impl FnMut(u64, Option<u64>) -> Result<()> + Send + 'static,
    control: &DownloadControl,
    retry: &RetryPolicy,
    on_retry: impl FnMut(u32, Duration, &Error) + Send + 'static,
) -> Result<DownloadOutcome> {
    let Some(total) = expected_size else {
        return download_part(
            part_path,
            None,
            &make_source,
            on_progress,
            control,
            retry,
            on_retry,
        )
        .await;
    };
    let n_ranges = ranges.clamp(1, 16);
    if n_ranges == 1 {
        return download_part(
            part_path,
            Some(total),
            &make_source,
            on_progress,
            control,
            retry,
            on_retry,
        )
        .await;
    }

    if let Some(parent) = part_path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let chunk = TELEGRAM_CHUNK_SIZE as u64;
    let part_len = std::fs::metadata(part_path).map(|m| m.len()).unwrap_or(0);
    let plan = plan_resume(part_len, Some(total));
    if plan.truncate_to != part_len {
        truncate_and_sync(part_path, plan.truncate_to)?;
    }
    if plan.reason == ResumeReason::Complete {
        let mut on_progress = on_progress;
        on_progress(total, Some(total))?;
        return Ok(DownloadOutcome {
            bytes_written: total,
            redownloaded_bytes: plan.redownload_bytes,
            resumed_from: plan.start_offset,
            retries: 0,
            plan,
        });
    }

    let num_chunks = total.div_ceil(chunk);
    let mut ranges_abs: Vec<(u64, u64)> = Vec::with_capacity(n_ranges as usize);
    for i in 0..n_ranges as u64 {
        let start_chunk = i * num_chunks / n_ranges as u64;
        let end_chunk = (i + 1) * num_chunks / n_ranges as u64;
        ranges_abs.push((start_chunk * chunk, (end_chunk * chunk).min(total)));
    }
    let range_lens: Vec<u64> = ranges_abs.iter().map(|&(s, e)| e - s).collect();

    // Initialize or load resume state. A missing or inconsistent state file
    // re-initializes all ranges from the global aligned prefix, exactly like
    // the serial resume does (untrusted tail bytes are re-downloaded).
    let state_path = state_path(part_path);
    let mut redownloaded_bytes = 0u64;
    let resume_local: Vec<u64> = match load_state(&state_path, n_ranges) {
        Some(state) => {
            // Guard against a stale state file: if the sidecar claims more
            // bytes than the .part file actually contains, the .part is a
            // leftover from a different download and must be discarded.
            let claimed: u64 = state.iter().sum();
            if claimed > part_len {
                let _ = std::fs::remove_file(&state_path);
                let _ = save_state_atomic(&state_path, &vec![0; n_ranges as usize]);
                truncate_and_sync(part_path, 0)
                    .map_err(|error| Error::Other(format!("guard truncate .part: {error}")))?;
                vec![0; n_ranges as usize]
            } else {
                state
                    .iter()
                    .enumerate()
                    .map(|(i, &state)| {
                        let aligned = state - (state % chunk);
                        redownloaded_bytes += state - aligned;
                        aligned.min(range_lens[i])
                    })
                    .collect()
            }
        }
        None => {
            let vec: Vec<u64> = ranges_abs
                .iter()
                .map(|&(s, e)| plan.start_offset.saturating_sub(s).min(e - s))
                .collect();
            let _ = save_state_atomic(&state_path, &vec);
            vec
        }
    };

    let initial_bytes: u64 = resume_local.iter().sum();
    let shared = Arc::new(Shared {
        bytes: AtomicU64::new(initial_bytes),
        saved_at_bytes: AtomicU64::new(initial_bytes),
        saved_at: Mutex::new(Instant::now()),
        progress_at: Mutex::new(Instant::now() - PROGRESS_INTERVAL),
        ranges: Mutex::new(resume_local.clone()),
        failed: AtomicBool::new(false),
        first_error: Mutex::new(None),
        on_progress: Mutex::new(on_progress),
        on_retry: Mutex::new(on_retry),
    });

    let make_source: Arc<dyn Fn(u64) -> Result<Box<dyn ChunkSource>> + Send + Sync> =
        Arc::new(make_source);
    let mut handles = Vec::with_capacity(n_ranges as usize);
    for (index, &(start, end)) in ranges_abs.iter().enumerate() {
        let resume = resume_local[index];
        handles.push(tokio::spawn(range_task(RangeTask {
            index,
            range_start: start,
            range_end: end,
            resume_local: resume,
            part_path: part_path.to_path_buf(),
            state_path: state_path.clone(),
            total,
            shared: Arc::clone(&shared),
            control: control.clone(),
            retry: retry.clone(),
            make_source: Arc::clone(&make_source),
        })));
    }

    let mut first_error: Option<Error> = None;
    for handle in handles {
        if let Err(join_error) = handle.await {
            if first_error.is_none() {
                first_error = Some(Error::Other(format!(
                    "download range task failed: {join_error}"
                )));
            }
        }
    }
    if let Some(error) = first_error.or_else(|| shared.first_error.lock().unwrap().take()) {
        return Err(error);
    }

    // All ranges complete. The caller renames .part to the final path, so a
    // stale sidecar file must not be left behind.
    let _ = std::fs::remove_file(&state_path);
    Ok(DownloadOutcome {
        bytes_written: total,
        redownloaded_bytes,
        resumed_from: plan.start_offset,
        retries: 0,
        plan,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicU64;
    use std::sync::Mutex;

    struct MemorySource {
        data: Arc<Vec<u8>>,
        offset: u64,
        chunk: u32,
        fail_after: Option<u32>,
        calls: u32,
        served: Arc<AtomicU64>,
    }

    impl MemorySource {
        fn new(
            data: Arc<Vec<u8>>,
            offset: u64,
            fail_after: Option<u32>,
            served: Arc<AtomicU64>,
        ) -> Self {
            Self {
                data,
                offset,
                chunk: TELEGRAM_CHUNK_SIZE,
                fail_after,
                calls: 0,
                served,
            }
        }
    }

    #[async_trait::async_trait]
    impl ChunkSource for MemorySource {
        fn size(&self) -> Option<u64> {
            Some(self.data.len() as u64)
        }

        fn chunk_size(&self) -> u32 {
            self.chunk
        }

        async fn next_chunk(&mut self) -> Result<Option<Vec<u8>>> {
            self.calls += 1;
            if let Some(limit) = self.fail_after {
                if self.calls > limit {
                    return Err(Error::Download("injected network failure".into()));
                }
            }
            if self.offset >= self.data.len() as u64 {
                return Ok(None);
            }
            let end = (self.offset + self.chunk as u64).min(self.data.len() as u64);
            let chunk = self.data[self.offset as usize..end as usize].to_vec();
            self.offset = end;
            self.served.fetch_add(chunk.len() as u64, Ordering::SeqCst);
            Ok(Some(chunk))
        }
    }

    fn sample(size: usize) -> Arc<Vec<u8>> {
        Arc::new((0..size).map(|index| (index % 251) as u8).collect())
    }

    fn no_retry() -> RetryPolicy {
        RetryPolicy::new(1, 10, 20)
    }

    #[tokio::test]
    async fn parallel_download_matches_data_and_resumes() {
        let directory = tempfile::tempdir().unwrap();
        let part = directory.path().join("file.part");
        let data = sample(TELEGRAM_CHUNK_SIZE as usize * 8);
        let served = Arc::new(AtomicU64::new(0));

        // Every range source fails before producing any chunk on the first call.
        let served_fail = Arc::clone(&served);
        let data_first = data.clone();
        let first = download_part_parallel(
            &part,
            Some(data.len() as u64),
            4,
            move |offset| {
                Ok(Box::new(MemorySource::new(
                    data_first.clone(),
                    offset,
                    Some(0),
                    Arc::clone(&served_fail),
                )))
            },
            |_, _| Ok(()),
            &DownloadControl::new(),
            &RetryPolicy::new(0, 10, 20),
            |_, _, _| {},
        )
        .await;
        assert!(first.is_err());

        let served_total = Arc::new(AtomicU64::new(0));
        let data_second = data.clone();
        let outcome = download_part_parallel(
            &part,
            Some(data.len() as u64),
            4,
            move |offset| {
                Ok(Box::new(MemorySource::new(
                    data_second.clone(),
                    offset,
                    None,
                    Arc::clone(&served_total),
                )))
            },
            |_, _| Ok(()),
            &DownloadControl::new(),
            &RetryPolicy::new(0, 10, 20),
            |_, _, _| {},
        )
        .await
        .unwrap();

        assert_eq!(outcome.bytes_written, data.len() as u64);
        assert_eq!(std::fs::read(&part).unwrap(), *data);
        assert!(
            !std::fs::metadata(format!("{}.ranges.json", part.display())).is_ok()
                || std::fs::metadata(format!("{}.ranges.json", part.display()))
                    .map(|m| m.len() == 0)
                    .unwrap_or(false)
        );
    }

    #[tokio::test]
    async fn stale_state_file_discards_false_claims() {
        let directory = tempfile::tempdir().unwrap();
        let part = directory.path().join("file.part");
        let data = sample(TELEGRAM_CHUNK_SIZE as usize * 6);

        // A tiny garbage .part file, but a sidecar claiming the whole file is done.
        std::fs::write(&part, vec![0xAAu8; TELEGRAM_CHUNK_SIZE as usize]).unwrap();
        std::fs::write(
            format!("{}.ranges.json", part.display()),
            serde_json::to_vec(&serde_json::json!({ "ranges": vec![data.len() as u64; 4] }))
                .unwrap(),
        )
        .unwrap();

        let data_stale = data.clone();
        let outcome = download_part_parallel(
            &part,
            Some(data.len() as u64),
            4,
            move |offset| {
                Ok(Box::new(MemorySource::new(
                    data_stale.clone(),
                    offset,
                    None,
                    Arc::new(AtomicU64::new(0)),
                )))
            },
            |_, _| Ok(()),
            &DownloadControl::new(),
            &no_retry(),
            |_, _, _| {},
        )
        .await
        .unwrap();

        assert_eq!(outcome.bytes_written, data.len() as u64);
        assert_eq!(std::fs::read(&part).unwrap(), *data);
    }

    #[tokio::test]
    async fn resume_skips_already_downloaded_prefix() {
        let directory = tempfile::tempdir().unwrap();
        let part = directory.path().join("file.part");
        let chunk = TELEGRAM_CHUNK_SIZE as u64;
        let data = sample(chunk as usize * 8);
        let served = Arc::new(AtomicU64::new(0));
        let offsets = Arc::new(Mutex::new(Vec::new()));
        let offsets_clone = Arc::clone(&offsets);

        // .part already contains the first two aligned chunks; the sidecar
        // reports exactly that.
        std::fs::write(&part, &data[..2 * chunk as usize]).unwrap();
        let ranges_abs: Vec<(u64, u64)> = {
            let n_ranges = 4u64;
            let n_chunks = data.len() as u64 / chunk;
            (0..n_ranges)
                .map(|i| {
                    let s = (i * n_chunks / n_ranges) * chunk;
                    let e = ((i + 1) * n_chunks / n_ranges) * chunk;
                    (s, e.min(data.len() as u64))
                })
                .collect()
        };
        let claimed: Vec<u64> = ranges_abs
            .iter()
            .map(|&(s, e)| (2 * chunk).saturating_sub(s).min(e - s))
            .collect();
        std::fs::write(
            format!("{}.ranges.json", part.display()),
            serde_json::to_vec(&serde_json::json!({ "ranges": claimed })).unwrap(),
        )
        .unwrap();

        let offsets_worker = Arc::clone(&offsets_clone);
        let served_worker = Arc::clone(&served);
        let data_resume = data.clone();
        let outcome = download_part_parallel(
            &part,
            Some(data.len() as u64),
            4,
            move |offset| {
                offsets_worker.lock().unwrap().push(offset);
                Ok(Box::new(MemorySource::new(
                    data_resume.clone(),
                    offset,
                    None,
                    Arc::clone(&served_worker),
                )) as Box<dyn ChunkSource>)
            },
            |_, _| Ok(()),
            &DownloadControl::new(),
            &no_retry(),
            |_, _, _| {},
        )
        .await
        .unwrap();

        assert_eq!(outcome.bytes_written, data.len() as u64);
        assert_eq!(std::fs::read(&part).unwrap(), *data);
        // The first two chunks must not have been re-fetched.
        assert_eq!(served.load(Ordering::SeqCst), data.len() as u64 - 2 * chunk);
    }
}
