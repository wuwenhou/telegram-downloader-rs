//! Telegram implementation of [`ChunkSource`].
//!
//! Telegram serves file chunks aligned to 512 KiB. Grammers can skip whole
//! chunks cheaply, so resuming only re-downloads at most the unaligned tail of
//! the local partial file.
//!
//! Grammers is built with `cdn_supported = false`; if Telegram nevertheless
//! answers with `File::CdnRedirect`, Grammers panics. The panic is caught here
//! and converted into a clear, non-retryable error instead of killing the
//! worker.

use std::panic::AssertUnwindSafe;

use async_trait::async_trait;
use futures_util::FutureExt;
use grammers_client::client::DownloadIter;
use grammers_client::media::Document;
use grammers_client::Client;

use crate::config::TELEGRAM_CHUNK_SIZE;
use crate::downloader::ChunkSource;
use crate::error::{Error, Result};
use crate::telegram::map_invocation;

pub struct TelegramChunkSource {
    client: Client,
    document: Document,
    size: Option<u64>,
    /// Absolute file offset of the next byte to return.
    offset: u64,
    /// Bytes to discard from the first chunk because `offset` was unaligned.
    pending_skip: u64,
    iterator: Option<DownloadIter>,
}

impl TelegramChunkSource {
    pub fn new(client: Client, document: Document, offset: u64) -> Self {
        let size = document.size().map(|size| size as u64);
        let chunk = TELEGRAM_CHUNK_SIZE as u64;
        let aligned = offset - (offset % chunk);
        Self {
            client,
            document,
            size,
            offset,
            pending_skip: offset - aligned,
            iterator: None,
        }
    }

    fn build_iterator(&self) -> DownloadIter {
        let chunk = TELEGRAM_CHUNK_SIZE as u64;
        let aligned = self.offset - (self.offset % chunk);
        let skip_chunks = (aligned / chunk) as i32;
        self.client
            .iter_download(&self.document)
            .chunk_size(TELEGRAM_CHUNK_SIZE as i32)
            .skip_chunks(skip_chunks)
    }
}

fn panic_text(payload: Box<dyn std::any::Any + Send>) -> String {
    if let Some(message) = payload.downcast_ref::<&str>() {
        (*message).to_string()
    } else if let Some(message) = payload.downcast_ref::<String>() {
        message.clone()
    } else {
        "unknown panic".to_string()
    }
}

#[async_trait]
impl ChunkSource for TelegramChunkSource {
    fn size(&self) -> Option<u64> {
        self.size
    }

    fn chunk_size(&self) -> u32 {
        TELEGRAM_CHUNK_SIZE
    }

    async fn next_chunk(&mut self) -> Result<Option<Vec<u8>>> {
        if let Some(size) = self.size {
            if self.offset >= size {
                return Ok(None);
            }
        }

        if self.iterator.is_none() {
            self.iterator = Some(self.build_iterator());
        }

        let iterator = self.iterator.as_mut().expect("iterator was just created");
        let outcome = AssertUnwindSafe(iterator.next()).catch_unwind().await;

        let mut chunk = match outcome {
            Ok(Ok(Some(chunk))) => chunk,
            Ok(Ok(None)) => {
                self.iterator = None;
                return Ok(None);
            }
            Ok(Err(error)) => {
                self.iterator = None;
                return Err(map_invocation(error));
            }
            Err(payload) => {
                self.iterator = None;
                let message = panic_text(payload);
                return Err(Error::Download(format!(
                    "Telegram file download aborted: {message}. \
                     This usually means the server redirected the file to its CDN, \
                     which this Grammers build does not support."
                )));
            }
        };

        if chunk.is_empty() {
            self.iterator = None;
            return Ok(None);
        }

        if self.pending_skip > 0 {
            let skip = (self.pending_skip as usize).min(chunk.len());
            chunk.drain(..skip);
            self.pending_skip -= skip as u64;
            if chunk.is_empty() {
                // Ask again on the next call; the skip is consumed.
                return self.next_chunk().await;
            }
        }

        self.offset += chunk.len() as u64;
        Ok(Some(chunk))
    }
}
