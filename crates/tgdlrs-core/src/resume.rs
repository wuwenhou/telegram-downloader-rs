//! Pure resume planning logic.
//!
//! Telegram's `upload.getFile` requires offsets aligned to 4096 bytes; the
//! downloader uses 512 KiB chunks so it can also use grammers'
//! `skip_chunks` fast path. On restart we therefore keep the largest aligned
//! prefix that is guaranteed to be intact, truncate the partial file to that
//! offset and continue from there. Bytes between the aligned offset and the old
//! end are re-downloaded and reported honestly.

use crate::config::TELEGRAM_CHUNK_SIZE;

/// How the resume offset was chosen.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ResumeReason {
    /// The partial file was empty.
    Empty,
    /// The partial file ended exactly at a chunk boundary.
    Aligned,
    /// The partial file was truncated back to the previous chunk boundary.
    TruncatedToAlignment,
    /// The partial file was already complete.
    Complete,
    /// The partial file was larger than the expected size and is discarded.
    Oversized,
}

/// Plan for continuing a partial download.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ResumePlan {
    pub start_offset: u64,
    pub truncate_to: u64,
    pub redownload_bytes: u64,
    pub reason: ResumeReason,
}

/// Compute a resume plan for a partial file.
pub fn plan_resume(part_len: u64, expected_size: Option<u64>) -> ResumePlan {
    let chunk = TELEGRAM_CHUNK_SIZE as u64;

    if part_len == 0 {
        return ResumePlan {
            start_offset: 0,
            truncate_to: 0,
            redownload_bytes: 0,
            reason: ResumeReason::Empty,
        };
    }

    if let Some(total) = expected_size {
        if part_len > total {
            return ResumePlan {
                start_offset: 0,
                truncate_to: 0,
                redownload_bytes: part_len,
                reason: ResumeReason::Oversized,
            };
        }
        if part_len == total {
            return ResumePlan {
                start_offset: total,
                truncate_to: total,
                redownload_bytes: 0,
                reason: ResumeReason::Complete,
            };
        }
    }

    let aligned = part_len / chunk * chunk;
    ResumePlan {
        start_offset: aligned,
        truncate_to: aligned,
        redownload_bytes: part_len - aligned,
        reason: if aligned == part_len {
            ResumeReason::Aligned
        } else {
            ResumeReason::TruncatedToAlignment
        },
    }
}

/// Missing byte ranges for a partial file, as half-open intervals.
pub fn missing_ranges(plan: ResumePlan, expected_size: Option<u64>) -> Vec<(u64, u64)> {
    match expected_size {
        Some(total) if plan.start_offset < total => vec![(plan.start_offset, total)],
        _ => Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CHUNK: u64 = TELEGRAM_CHUNK_SIZE as u64;

    #[test]
    fn empty_file_starts_at_zero() {
        let plan = plan_resume(0, Some(1000));
        assert_eq!(plan.start_offset, 0);
        assert_eq!(plan.redownload_bytes, 0);
        assert_eq!(plan.reason, ResumeReason::Empty);
    }

    #[test]
    fn aligned_file_resumes_exactly() {
        let plan = plan_resume(CHUNK * 3, Some(CHUNK * 10));
        assert_eq!(plan.start_offset, CHUNK * 3);
        assert_eq!(plan.redownload_bytes, 0);
        assert_eq!(plan.reason, ResumeReason::Aligned);
    }

    #[test]
    fn unaligned_file_truncates_to_chunk_boundary() {
        let plan = plan_resume(CHUNK * 2 + 1234, Some(CHUNK * 5));
        assert_eq!(plan.start_offset, CHUNK * 2);
        assert_eq!(plan.truncate_to, CHUNK * 2);
        assert_eq!(plan.redownload_bytes, 1234);
        assert_eq!(plan.reason, ResumeReason::TruncatedToAlignment);
    }

    #[test]
    fn complete_file_is_detected() {
        let plan = plan_resume(500, Some(500));
        assert_eq!(plan.start_offset, 500);
        assert_eq!(plan.reason, ResumeReason::Complete);
    }

    #[test]
    fn oversized_file_is_discarded() {
        let plan = plan_resume(600, Some(500));
        assert_eq!(plan.start_offset, 0);
        assert_eq!(plan.truncate_to, 0);
        assert_eq!(plan.redownload_bytes, 600);
        assert_eq!(plan.reason, ResumeReason::Oversized);
    }

    #[test]
    fn small_unaligned_file_restarts_from_zero() {
        let plan = plan_resume(100, Some(1000));
        assert_eq!(plan.start_offset, 0);
        assert_eq!(plan.redownload_bytes, 100);
    }

    #[test]
    fn missing_ranges_cover_the_rest() {
        let plan = plan_resume(CHUNK, Some(CHUNK * 3));
        assert_eq!(
            missing_ranges(plan, Some(CHUNK * 3)),
            vec![(CHUNK, CHUNK * 3)]
        );
        assert!(missing_ranges(plan_resume(10, Some(10)), Some(10)).is_empty());
    }
}
