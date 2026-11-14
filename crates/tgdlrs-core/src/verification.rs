//! SHA-256 and size verification.

use std::fs::File;
use std::io::{BufReader, Read};
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

use crate::database::Store;
use crate::error::{Error, Result};
use crate::models::VerifyReport;

/// Hash a file with SHA-256, reading it in bounded chunks.
pub fn sha256_file(path: &Path) -> Result<String> {
    sha256_file_with_progress(path, |_, _| {})
}

/// Hash a file, reporting `(bytes_hashed, total_bytes)` after each chunk.
pub fn sha256_file_with_progress(
    path: &Path,
    mut progress: impl FnMut(u64, u64),
) -> Result<String> {
    let file = File::open(path)?;
    let total = file.metadata()?.len();
    let mut reader = BufReader::with_capacity(1024 * 1024, file);
    let mut hasher = Sha256::new();
    let mut buffer = vec![0u8; 1024 * 1024];
    let mut hashed = 0u64;

    loop {
        let read = reader.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
        hashed += read as u64;
        progress(hashed, total);
    }

    Ok(hex::encode(hasher.finalize()))
}

/// Verify an existing file against its expected size and optional digest.
pub fn verify_file(
    path: &Path,
    expected_size: Option<u64>,
    expected_sha256: Option<&str>,
) -> Result<VerifyReport> {
    let path = path.to_path_buf();
    let metadata = match std::fs::metadata(&path) {
        Ok(metadata) => Some(metadata),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => return Err(error.into()),
    };

    let actual_size = metadata
        .as_ref()
        .map(|metadata| metadata.len())
        .unwrap_or(0);
    let size_ok = match expected_size {
        Some(expected) => metadata.is_some() && actual_size == expected,
        None => metadata.is_some(),
    };

    let actual_sha = if metadata.is_some() && (expected_sha256.is_some() || size_ok) {
        Some(sha256_file(&path)?)
    } else {
        None
    };

    let hash_ok = match (expected_sha256, actual_sha.as_deref()) {
        (Some(expected), Some(actual)) => expected.eq_ignore_ascii_case(actual),
        (Some(_), None) => false,
        (None, _) => true,
    };

    Ok(VerifyReport {
        exists: metadata.is_some(),
        path,
        expected_size,
        actual_size,
        sha256_expected: expected_sha256.map(str::to_string),
        sha256_actual: actual_sha,
        size_ok,
        hash_ok,
        ok: metadata.is_some() && size_ok && hash_ok,
    })
}

/// Verify that `part` is complete, then rename it to `final_path`.
///
/// If the destination already exists it is validated first; a valid file is
/// kept and reported as `already_present`. An invalid destination is never
/// overwritten.
pub enum FinalizeOutcome {
    Renamed(VerifyReport),
    AlreadyPresent(VerifyReport),
}

pub fn finalize_part(
    part_path: &Path,
    final_path: &Path,
    expected_size: Option<u64>,
    expected_sha256: Option<&str>,
    verify: bool,
) -> Result<FinalizeOutcome> {
    if final_path.exists() {
        let report = verify_file(final_path, expected_size, expected_sha256)?;
        if report.ok {
            return Ok(FinalizeOutcome::AlreadyPresent(report));
        }
        return Err(Error::Conflict(format!(
            "{} already exists but does not match the expected size/hash; refusing to overwrite",
            final_path.display()
        )));
    }

    let part_len = crate::storage::file_len(part_path)?;
    if let Some(expected) = expected_size {
        if part_len != expected {
            return Err(Error::Verification(format!(
                "partial file {} has {part_len} bytes, expected {expected}",
                part_path.display()
            )));
        }
    }
    if part_len == 0 {
        return Err(Error::Verification(format!(
            "partial file {} is empty",
            part_path.display()
        )));
    }

    let report = if verify {
        let report = verify_file(part_path, expected_size, expected_sha256)?;
        if !report.ok {
            return Err(Error::Verification(format!(
                "verification failed for {}: size_ok={}, hash_ok={}",
                part_path.display(),
                report.size_ok,
                report.hash_ok
            )));
        }
        report
    } else {
        verify_file(part_path, expected_size, None)?
    };

    if let Some(parent) = final_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    crate::storage::rename_no_overwrite(part_path, final_path)?;

    Ok(FinalizeOutcome::Renamed(VerifyReport {
        path: final_path.to_path_buf(),
        ..report
    }))
}

/// Outcome of verifying the file tracked by a download job and recording it.
pub struct VerifyJobOutcome {
    pub job_id: String,
    pub report: VerifyReport,
}

/// Verify one download job end to end: hash/size check, record the result in
/// `verification_results`, and reflect the outcome in the job state.
///
/// A complete file that fails verification moves the job to FAILED; a file
/// that is still incomplete (size mismatch) leaves the job state untouched.
pub fn verify_job(
    store: &Store,
    id: &str,
    expected_size: Option<u64>,
    sha256: Option<&str>,
) -> Result<VerifyJobOutcome> {
    let job = store.find(id)?;
    let expected_size = expected_size.or(job.file_size);
    let expected_sha = match sha256 {
        Some(value) => Some(value.trim().to_ascii_lowercase()),
        None => job.sha256_expected.clone(),
    };
    let report = verify_file(&job.final_path, expected_size, expected_sha.as_deref())?;
    store.record_verification(&job.id, &report)?;
    if report.ok {
        if job.verify {
            store.mark_verified(&job.id, report.sha256_actual.as_deref().unwrap_or(""))?;
        } else {
            store.mark_completed(&job.id, report.sha256_actual.as_deref())?;
        }
    } else if report.exists
        && (report.expected_size == Some(report.actual_size)
            || (report.expected_size.is_none() && !report.hash_ok))
    {
        store.set_state(
            &job.id,
            crate::models::DownloadState::Failed,
            Some("verification failed; see verification_results"),
        )?;
    }
    Ok(VerifyJobOutcome {
        job_id: job.id,
        report,
    })
}

/// Report for a file that does not exist yet.
pub fn missing_report(path: &Path, expected_size: Option<u64>) -> VerifyReport {
    VerifyReport {
        path: path.to_path_buf(),
        exists: false,
        expected_size,
        actual_size: 0,
        sha256_expected: None,
        sha256_actual: None,
        size_ok: false,
        hash_ok: false,
        ok: false,
    }
}

/// Path used for human-readable verification output.
pub fn display_path(path: &Path) -> PathBuf {
    path.canonicalize().unwrap_or_else(|_| path.to_path_buf())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hashes_known_content() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("sample.bin");
        std::fs::write(&path, b"abc").unwrap();
        assert_eq!(
            sha256_file(&path).unwrap(),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn verifies_size_and_hash() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("sample.bin");
        std::fs::write(&path, b"abc").unwrap();
        let hash = sha256_file(&path).unwrap();

        assert!(verify_file(&path, Some(3), Some(&hash)).unwrap().ok);
        assert!(!verify_file(&path, Some(4), Some(&hash)).unwrap().ok);
        assert!(!verify_file(&path, Some(3), Some("deadbeef")).unwrap().ok);
        assert!(verify_file(&path, Some(3), None).unwrap().ok);
    }

    #[test]
    fn finalize_renames_and_refuses_overwrite() {
        let directory = tempfile::tempdir().unwrap();
        let part = directory.path().join("file.part");
        let final_path = directory.path().join("file.bin");
        std::fs::write(&part, b"payload").unwrap();

        match finalize_part(&part, &final_path, Some(7), None, true).unwrap() {
            FinalizeOutcome::Renamed(report) => assert!(report.ok),
            FinalizeOutcome::AlreadyPresent(_) => panic!("unexpected"),
        }
        assert!(final_path.exists());
        assert!(!part.exists());

        // A second finalize must validate the existing destination instead of
        // overwriting it.
        std::fs::write(&part, b"different").unwrap();
        match finalize_part(&part, &final_path, Some(7), None, true).unwrap() {
            FinalizeOutcome::AlreadyPresent(report) => assert!(report.ok),
            FinalizeOutcome::Renamed(_) => panic!("unexpected"),
        }
        assert_eq!(std::fs::read(&final_path).unwrap(), b"payload");
    }
}
