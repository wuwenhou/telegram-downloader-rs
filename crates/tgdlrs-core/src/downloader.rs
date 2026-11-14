//! The resumable download loop.
//!
//! The loop is deliberately independent from Telegram: it consumes a
//! [`ChunkSource`], so the resume behaviour can be tested against an in-memory
//! source with injected failures. The Telegram implementation lives in
//! `telegram::source`.

use std::fs::OpenOptions;
use std::io::{Seek, SeekFrom, Write};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;

use crate::config::TELEGRAM_CHUNK_SIZE;
use crate::error::{Error, Result};
use crate::resume::{plan_resume, ResumePlan, ResumeReason};

const SYNC_INTERVAL_BYTES: u64 = 8 * 1024 * 1024;

/// A source of aligned file chunks.
#[async_trait]
pub trait ChunkSource: Send {
    /// Total file size when known.
    fn size(&self) -> Option<u64>;
    /// Maximum chunk size returned by [`ChunkSource::next_chunk`].
    fn chunk_size(&self) -> u32;
    /// Fetch the next chunk at the source's current offset.
    async fn next_chunk(&mut self) -> Result<Option<Vec<u8>>>;
}

/// Signal returned by [`DownloadControl::check`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ControlSignal {
    Continue,
    Pause,
    Cancel,
}

/// Cross-task pause/cancel flags.
#[derive(Clone, Debug, Default)]
pub struct DownloadControl {
    paused: Arc<AtomicBool>,
    canceled: Arc<AtomicBool>,
}

impl DownloadControl {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn pause(&self) {
        self.paused.store(true, Ordering::SeqCst);
    }

    pub fn unpause(&self) {
        self.paused.store(false, Ordering::SeqCst);
    }

    pub fn cancel(&self) {
        self.canceled.store(true, Ordering::SeqCst);
    }

    pub fn is_paused(&self) -> bool {
        self.paused.load(Ordering::SeqCst)
    }

    pub fn is_canceled(&self) -> bool {
        self.canceled.load(Ordering::SeqCst)
    }

    pub fn check(&self) -> ControlSignal {
        if self.canceled.load(Ordering::SeqCst) {
            ControlSignal::Cancel
        } else if self.paused.load(Ordering::SeqCst) {
            ControlSignal::Pause
        } else {
            ControlSignal::Continue
        }
    }
}

/// Retry policy with exponential backoff and jitter.
#[derive(Clone, Debug)]
pub struct RetryPolicy {
    pub max_attempts: u32,
    pub base_delay: Duration,
    pub max_delay: Duration,
}

impl RetryPolicy {
    pub fn new(max_attempts: u32, base_ms: u64, max_ms: u64) -> Self {
        Self {
            max_attempts: max_attempts.max(1),
            base_delay: Duration::from_millis(base_ms.max(10)),
            max_delay: Duration::from_millis(max_ms.max(base_ms.max(10))),
        }
    }

    /// Exponential delay for `attempt` (1-based) with ±20% jitter.
    pub fn delay_for(&self, attempt: u32) -> Duration {
        let exponent = attempt.saturating_sub(1).min(16);
        let base = self
            .base_delay
            .as_millis()
            .saturating_mul(1u128 << exponent)
            .min(self.max_delay.as_millis());
        let jitter = 0.8 + rand::random::<f64>() * 0.4;
        Duration::from_millis((base as f64 * jitter) as u64)
    }
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self::new(10, 1_000, 60_000)
    }
}

/// Result of a completed download loop.
#[derive(Clone, Debug)]
pub struct DownloadOutcome {
    pub bytes_written: u64,
    pub redownloaded_bytes: u64,
    pub resumed_from: u64,
    pub retries: u32,
    pub plan: ResumePlan,
}

/// Download (or resume) `part_path` until `expected_size` bytes are present.
///
/// `make_source` is called with the current aligned offset, both initially and
/// after every transient error. It must not block for long; the Telegram
/// implementation only constructs an iterator.
pub async fn download_part(
    part_path: &Path,
    expected_size: Option<u64>,
    mut make_source: impl FnMut(u64) -> Result<Box<dyn ChunkSource>>,
    mut on_progress: impl FnMut(u64, Option<u64>) -> Result<()>,
    control: &DownloadControl,
    retry: &RetryPolicy,
    mut on_retry: impl FnMut(u32, Duration, &Error),
) -> Result<DownloadOutcome> {
    if let Some(parent) = part_path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let mut file = OpenOptions::new()
        .create(true)
        .truncate(false) // the partial file holds bytes we are resuming; never truncate
        .read(true)
        .write(true)
        .open(part_path)?;

    let part_len = file.metadata()?.len();
    let plan = plan_resume(part_len, expected_size);

    if plan.truncate_to != part_len {
        file.set_len(plan.truncate_to)?;
        file.sync_all()?;
    }

    let mut offset = plan.start_offset;

    if plan.reason == ResumeReason::Complete {
        on_progress(offset, expected_size)?;
        return Ok(DownloadOutcome {
            bytes_written: offset,
            redownloaded_bytes: plan.redownload_bytes,
            resumed_from: plan.start_offset,
            retries: 0,
            plan,
        });
    }

    file.seek(SeekFrom::Start(offset))?;
    let mut source = make_source(offset)?;
    let mut retries = 0u32;
    let mut flood_waits = 0u32;

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

        match source.next_chunk().await {
            Ok(Some(mut chunk)) => {
                if chunk.is_empty() {
                    return Err(Error::Download(format!(
                        "Telegram returned an empty chunk at offset {offset}"
                    )));
                }

                if let Some(total) = expected_size {
                    let remaining = total.saturating_sub(offset);
                    if remaining == 0 {
                        break;
                    }
                    if chunk.len() as u64 > remaining {
                        chunk.truncate(remaining as usize);
                    }
                }

                file.write_all(&chunk)?;
                offset += chunk.len() as u64;
                on_progress(offset, expected_size)?;

                if offset % SYNC_INTERVAL_BYTES < chunk.len() as u64 {
                    file.sync_data()?;
                }

                if let Some(total) = expected_size {
                    if offset >= total {
                        break;
                    }
                } else if chunk.len() < source.chunk_size() as usize {
                    // A short chunk signals the end of an unknown-size file.
                    break;
                }
            }
            Ok(None) => {
                // The iterator ended early. Telegram may terminate a response
                // without an error (for example when a file reference expires
                // mid-stream). Ending here would mark the bytes written so far
                // as complete, leaving a corrupt .part behind — so treat it as
                // a transient failure: recreate the iterator at the current
                // offset with a fresh reference.
                if let Some(total) = expected_size {
                    if offset < total {
                        let remaining = total - offset;
                        if retries >= retry.max_attempts {
                            return Err(Error::Download(format!(
                                "connection ended at offset {offset} with {remaining} bytes remaining"
                            )));
                        }
                        retries += 1;
                        let delay = retry.delay_for(retries);
                        on_retry(
                            retries,
                            delay,
                            &Error::Download(format!(
                                "connection ended at offset {offset} with {remaining} bytes remaining"
                            )),
                        );
                        tokio::time::sleep(delay).await;
                        source = make_source(offset)?;
                        continue;
                    }
                }
                break;
            }
            Err(error) => {
                let _ = file.sync_data();
                if let Some(seconds) = error.flood_wait_seconds() {
                    flood_waits += 1;
                    if flood_waits > 100 {
                        return Err(error);
                    }
                    tracing::warn!(seconds, "Telegram flood wait");
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
                    on_retry(retries, delay, &error);
                    tokio::time::sleep(delay).await;
                }
                source = make_source(offset)?;
            }
        }
    }

    file.sync_all()?;
    let final_len = file.metadata()?.len();
    if let Some(total) = expected_size {
        if final_len != total {
            return Err(Error::Verification(format!(
                "download stopped at {final_len} bytes, expected {total}"
            )));
        }
    }

    Ok(DownloadOutcome {
        bytes_written: final_len,
        redownloaded_bytes: plan.redownload_bytes,
        resumed_from: plan.start_offset,
        retries,
        plan,
    })
}

/// Convenience wrapper with the standard 512 KiB chunk size.
pub fn telegram_chunk_size() -> u32 {
    TELEGRAM_CHUNK_SIZE
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicU64;

    struct MemorySource {
        data: Arc<Vec<u8>>,
        offset: u64,
        chunk: u32,
        fail_after: Option<u32>,
        permanent: bool,
        calls: u32,
        served: Arc<AtomicU64>,
    }

    impl MemorySource {
        fn new(data: Arc<Vec<u8>>, offset: u64, fail_after: Option<u32>) -> Self {
            Self {
                data,
                offset,
                chunk: TELEGRAM_CHUNK_SIZE,
                fail_after,
                permanent: false,
                calls: 0,
                served: Arc::new(AtomicU64::new(0)),
            }
        }

        /// Like [`MemorySource::new`], but the injected failure is not
        /// retryable, so `download_part` returns the error immediately.
        fn new_permanent(data: Arc<Vec<u8>>, offset: u64, fail_after: Option<u32>) -> Self {
            Self {
                permanent: true,
                ..Self::new(data, offset, fail_after)
            }
        }
    }

    #[async_trait]
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
                    return Err(if self.permanent {
                        Error::InvalidInput("injected permanent failure".into())
                    } else {
                        Error::Download("injected network failure".into())
                    });
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
    async fn downloads_from_zero_and_verifies_content() {
        let directory = tempfile::tempdir().unwrap();
        let part = directory.path().join("file.part");
        let data = sample(TELEGRAM_CHUNK_SIZE as usize * 3 + 1234);

        let outcome = download_part(
            &part,
            Some(data.len() as u64),
            |offset| {
                let mut source = MemorySource::new(data.clone(), offset, None);
                source.served = Arc::new(AtomicU64::new(0));
                Ok(Box::new(source))
            },
            |_, _| Ok(()),
            &DownloadControl::new(),
            &no_retry(),
            |_, _, _| {},
        )
        .await
        .unwrap();

        assert_eq!(outcome.bytes_written, data.len() as u64);
        assert_eq!(outcome.resumed_from, 0);
        assert_eq!(std::fs::read(&part).unwrap(), *data);
    }

    #[tokio::test]
    async fn resumes_after_interruption_at_chunk_boundary() {
        let directory = tempfile::tempdir().unwrap();
        let part = directory.path().join("file.part");
        let data = sample(TELEGRAM_CHUNK_SIZE as usize * 5);

        // First run fails after three chunks with a non-retryable error, so
        // the error is returned to the caller immediately.
        let first = download_part(
            &part,
            Some(data.len() as u64),
            |offset| {
                Ok(Box::new(MemorySource::new_permanent(
                    data.clone(),
                    offset,
                    Some(3),
                )))
            },
            |_, _| Ok(()),
            &DownloadControl::new(),
            &RetryPolicy::new(0, 10, 20),
            |_, _, _| {},
        )
        .await;
        assert!(first.is_err());
        assert_eq!(
            std::fs::metadata(&part).unwrap().len(),
            TELEGRAM_CHUNK_SIZE as u64 * 3
        );

        let second = download_part(
            &part,
            Some(data.len() as u64),
            |offset| {
                assert_eq!(offset, TELEGRAM_CHUNK_SIZE as u64 * 3);
                Ok(Box::new(MemorySource::new(data.clone(), offset, None)))
            },
            |_, _| Ok(()),
            &DownloadControl::new(),
            &no_retry(),
            |_, _, _| {},
        )
        .await
        .unwrap();

        assert_eq!(second.resumed_from, TELEGRAM_CHUNK_SIZE as u64 * 3);
        assert_eq!(second.redownloaded_bytes, 0);
        assert_eq!(std::fs::read(&part).unwrap(), *data);
    }

    #[tokio::test]
    async fn truncates_unaligned_tail_before_resuming() {
        let directory = tempfile::tempdir().unwrap();
        let part = directory.path().join("file.part");
        let data = sample(TELEGRAM_CHUNK_SIZE as usize * 4);

        let mut prefix = data[..TELEGRAM_CHUNK_SIZE as usize * 2].to_vec();
        prefix.extend(std::iter::repeat_n(0xEE, 777));
        std::fs::write(&part, &prefix).unwrap();

        let outcome = download_part(
            &part,
            Some(data.len() as u64),
            |offset| {
                assert_eq!(offset, TELEGRAM_CHUNK_SIZE as u64 * 2);
                Ok(Box::new(MemorySource::new(data.clone(), offset, None)))
            },
            |_, _| Ok(()),
            &DownloadControl::new(),
            &no_retry(),
            |_, _, _| {},
        )
        .await
        .unwrap();

        assert_eq!(outcome.redownloaded_bytes, 777);
        assert_eq!(outcome.resumed_from, TELEGRAM_CHUNK_SIZE as u64 * 2);
        assert_eq!(std::fs::read(&part).unwrap(), *data);
    }

    #[tokio::test]
    async fn oversized_partial_restarts_from_zero() {
        let directory = tempfile::tempdir().unwrap();
        let part = directory.path().join("file.part");
        let data = sample(TELEGRAM_CHUNK_SIZE as usize);
        std::fs::write(&part, vec![0xAA; data.len() + 100]).unwrap();

        let outcome = download_part(
            &part,
            Some(data.len() as u64),
            |offset| {
                assert_eq!(offset, 0);
                Ok(Box::new(MemorySource::new(data.clone(), offset, None)))
            },
            |_, _| Ok(()),
            &DownloadControl::new(),
            &no_retry(),
            |_, _, _| {},
        )
        .await
        .unwrap();

        assert_eq!(outcome.redownloaded_bytes, data.len() as u64 + 100);
        assert_eq!(std::fs::read(&part).unwrap(), *data);
    }

    #[tokio::test]
    async fn pause_and_cancel_are_observed() {
        let directory = tempfile::tempdir().unwrap();
        let part = directory.path().join("file.part");
        let data = sample(TELEGRAM_CHUNK_SIZE as usize * 4);

        let control = DownloadControl::new();
        control.pause();
        let paused = download_part(
            &part,
            Some(data.len() as u64),
            |offset| Ok(Box::new(MemorySource::new(data.clone(), offset, None))),
            |_, _| Ok(()),
            &control,
            &no_retry(),
            |_, _, _| {},
        )
        .await;
        assert!(matches!(paused, Err(Error::Paused)));

        let control = DownloadControl::new();
        control.cancel();
        let canceled = download_part(
            &part,
            Some(data.len() as u64),
            |offset| Ok(Box::new(MemorySource::new(data.clone(), offset, None))),
            |_, _| Ok(()),
            &control,
            &no_retry(),
            |_, _, _| {},
        )
        .await;
        assert!(matches!(canceled, Err(Error::Canceled)));
    }

    #[tokio::test]
    async fn retries_transient_errors_without_losing_bytes() {
        let directory = tempfile::tempdir().unwrap();
        let part = directory.path().join("file.part");
        let data = sample(TELEGRAM_CHUNK_SIZE as usize * 4);
        let attempts = Arc::new(AtomicU64::new(0));

        let outcome = download_part(
            &part,
            Some(data.len() as u64),
            |offset| {
                attempts.fetch_add(1, Ordering::SeqCst);
                Ok(Box::new(MemorySource::new(data.clone(), offset, Some(1))))
            },
            |_, _| Ok(()),
            &DownloadControl::new(),
            &RetryPolicy::new(10, 10, 20),
            |_, _, _| {},
        )
        .await
        .unwrap();

        assert!(attempts.load(Ordering::SeqCst) > 1);
        assert_eq!(std::fs::read(&part).unwrap(), *data);
        assert!(outcome.retries > 0);
    }

    #[test]
    fn backoff_is_bounded() {
        let policy = RetryPolicy::new(10, 1_000, 60_000);
        for attempt in 1..20 {
            let delay = policy.delay_for(attempt);
            assert!(delay <= Duration::from_millis(72_000));
        }
    }
}
