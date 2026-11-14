//! Filesystem helpers: safe names, atomic writes, free-space checks.

use std::ffi::OsStr;
use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use crate::error::{Error, Result};

const WINDOWS_RESERVED: &[&str] = &[
    "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8",
    "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
];

/// Turn an arbitrary Telegram file name into a safe single path component.
///
/// This never allows a path separator, drive letter, control character or
/// Windows reserved device name through, and keeps the extension intact.
pub fn safe_file_name(input: &str) -> String {
    let base = input
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or(input)
        .trim()
        .trim_matches('.');
    let base = if base.is_empty() {
        "download.bin"
    } else {
        base
    };

    let mut cleaned: String = base
        .chars()
        .map(|character| {
            if character.is_control()
                || matches!(
                    character,
                    '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*'
                )
            {
                '_'
            } else {
                character
            }
        })
        .collect();

    while cleaned.ends_with('.') || cleaned.ends_with(' ') {
        cleaned.pop();
    }
    if cleaned.is_empty() {
        cleaned = "download.bin".into();
    }

    let stem = cleaned
        .split('.')
        .next()
        .unwrap_or(&cleaned)
        .to_ascii_uppercase();
    if WINDOWS_RESERVED.contains(&stem.as_str()) {
        cleaned = format!("_{cleaned}");
    }

    if cleaned.chars().count() > 180 {
        let extension = Path::new(&cleaned)
            .extension()
            .and_then(OsStr::to_str)
            .map(|value| format!(".{value}"))
            .unwrap_or_default();
        let keep = 180usize.saturating_sub(extension.chars().count());
        cleaned = cleaned.chars().take(keep).collect::<String>() + &extension;
    }

    cleaned
}

/// Verify that `candidate` stays inside `root` after canonicalisation.
pub fn ensure_within(root: &Path, candidate: &Path) -> Result<()> {
    let root = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
    let candidate = candidate
        .canonicalize()
        .unwrap_or_else(|_| candidate.to_path_buf());
    if candidate.starts_with(&root) {
        Ok(())
    } else {
        Err(Error::UnsafePath(candidate))
    }
}

/// Write bytes to `path` atomically (temporary file + rename).
pub fn atomic_write(path: &Path, bytes: &[u8]) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let temporary = path.with_extension(format!(
        "{}.tmp",
        path.extension().and_then(OsStr::to_str).unwrap_or("data")
    ));
    {
        let mut file = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&temporary)?;
        file.write_all(bytes)?;
        file.sync_all()?;
    }
    // Windows rename does not overwrite; remove the old file only after the new
    // payload has been flushed.
    if path.exists() {
        std::fs::remove_file(path)?;
    }
    std::fs::rename(&temporary, path)?;
    Ok(())
}

/// Flush a file's data to stable storage.
pub fn sync_file(path: &Path) -> Result<()> {
    let file = OpenOptions::new().read(true).write(true).open(path)?;
    file.sync_all()?;
    Ok(())
}

/// Read the first `limit` bytes of a file.
pub fn read_prefix(path: &Path, limit: usize) -> Result<Vec<u8>> {
    let mut file = File::open(path)?;
    let mut buffer = vec![0u8; limit];
    let read = file.read(&mut buffer)?;
    buffer.truncate(read);
    Ok(buffer)
}

/// Free bytes available to the current user on the volume containing `path`.
#[cfg(windows)]
pub fn free_space(path: &Path) -> Result<u64> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::GetDiskFreeSpaceExW;

    let mut probe = path.to_path_buf();
    while !probe.exists() {
        if !probe.pop() {
            return Err(Error::Io(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "no existing ancestor for free-space probe",
            )));
        }
    }

    let wide: Vec<u16> = probe
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    let mut available: u64 = 0;
    let mut total: u64 = 0;
    let mut total_free: u64 = 0;
    let ok =
        unsafe { GetDiskFreeSpaceExW(wide.as_ptr(), &mut available, &mut total, &mut total_free) };
    if ok == 0 {
        return Err(Error::Io(std::io::Error::last_os_error()));
    }
    Ok(available)
}

#[cfg(not(windows))]
pub fn free_space(_path: &Path) -> Result<u64> {
    Ok(u64::MAX)
}

/// Ensure `needed` bytes can be written while preserving `reserve` bytes.
pub fn ensure_free_space(path: &Path, needed: u64, reserve: u64) -> Result<u64> {
    let available = free_space(path)?;
    let required = needed.saturating_add(reserve);
    if available < required {
        return Err(Error::Download(format!(
            "not enough disk space on {}: {} bytes available, {} bytes required ({} download + {} reserve)",
            path.display(),
            available,
            required,
            needed,
            reserve
        )));
    }
    Ok(available)
}

/// Return the size of a file, or zero when it does not exist.
pub fn file_len(path: &Path) -> Result<u64> {
    match std::fs::metadata(path) {
        Ok(metadata) => Ok(metadata.len()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(0),
        Err(error) => Err(error.into()),
    }
}

/// Move `from` to `to` without ever overwriting an existing destination.
pub fn rename_no_overwrite(from: &Path, to: &Path) -> Result<()> {
    if to.exists() {
        return Err(Error::Conflict(format!(
            "refusing to overwrite existing file {}",
            to.display()
        )));
    }
    std::fs::rename(from, to)?;
    Ok(())
}

/// Create a unique sibling path by appending a numeric suffix.
pub fn unique_sibling(path: &Path) -> PathBuf {
    if !path.exists() {
        return path.to_path_buf();
    }
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let stem = path
        .file_stem()
        .and_then(OsStr::to_str)
        .unwrap_or("download");
    let extension = path.extension().and_then(OsStr::to_str);
    for index in 1..10_000 {
        let name = match extension {
            Some(extension) => format!("{stem}.{index}.{extension}"),
            None => format!("{stem}.{index}"),
        };
        let candidate = parent.join(name);
        if !candidate.exists() {
            return candidate;
        }
    }
    path.to_path_buf()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_path_traversal() {
        assert_eq!(
            safe_file_name(r"..\..\Windows\system32\evil.dll"),
            "evil.dll"
        );
        assert_eq!(safe_file_name("../../etc/passwd"), "passwd");
        assert_eq!(safe_file_name("a/b/c.zip"), "c.zip");
        assert_eq!(safe_file_name("CON"), "_CON");
        assert_eq!(safe_file_name("  ...  "), "download.bin");
        assert_eq!(safe_file_name("normal.tar.gz"), "normal.tar.gz");
    }

    #[test]
    fn atomic_write_replaces_contents() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config.json");
        atomic_write(&path, b"one").unwrap();
        atomic_write(&path, b"two").unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"two");
    }

    #[test]
    fn rename_refuses_overwrite() {
        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("a");
        let destination = directory.path().join("b");
        std::fs::write(&source, b"a").unwrap();
        std::fs::write(&destination, b"b").unwrap();
        assert!(matches!(
            rename_no_overwrite(&source, &destination),
            Err(Error::Conflict(_))
        ));
        assert_eq!(std::fs::read(&destination).unwrap(), b"b");
    }
}
