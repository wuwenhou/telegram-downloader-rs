//! Autostart management via HKCU\Software\Microsoft\Windows\CurrentVersion\Run.
//!
//! Runs for the current user only; no administrator rights required.

use std::ffi::OsStr;
use std::os::windows::ffi::OsStrExt;

use windows_sys::Win32::Foundation::ERROR_SUCCESS;
use windows_sys::Win32::System::Registry::{
    RegCloseKey, RegDeleteValueW, RegOpenKeyExW, RegQueryValueExW, RegSetValueExW, HKEY,
    HKEY_CURRENT_USER, KEY_READ, KEY_WRITE, REG_SZ,
};

const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
const VALUE_NAME: &str = "tgdlrs-worker";

fn wide(text: &str) -> Vec<u16> {
    OsStr::new(text)
        .encode_wide()
        .chain(std::iter::once(0))
        .collect()
}

fn open(write: bool) -> Result<HKEY, String> {
    let mut handle: HKEY = std::ptr::null_mut();
    let access = if write { KEY_WRITE } else { KEY_READ };
    let key = wide(RUN_KEY);
    let result = unsafe { RegOpenKeyExW(HKEY_CURRENT_USER, key.as_ptr(), 0, access, &mut handle) };
    if result != ERROR_SUCCESS {
        return Err(format!(
            "cannot open HKCU\\{RUN_KEY}: Windows error {result}"
        ));
    }
    Ok(handle)
}

/// Install the autostart entry for `worker_exe`; returns the quoted command line.
pub fn install(worker_exe: &std::path::Path) -> Result<String, String> {
    let command = format!("\"{}\"", worker_exe.display());
    let key = open(true)?;
    let name = wide(VALUE_NAME);
    let data = wide(&command);
    let bytes = (data.len() * std::mem::size_of::<u16>()) as u32;
    let result = unsafe {
        RegSetValueExW(
            key,
            name.as_ptr(),
            0,
            REG_SZ,
            data.as_ptr() as *const u8,
            bytes,
        )
    };
    unsafe { RegCloseKey(key) };
    if result != ERROR_SUCCESS {
        return Err(format!("cannot set the Run value: Windows error {result}"));
    }
    Ok(command)
}

/// Remove the autostart entry. Returns true when an entry was removed.
pub fn uninstall() -> Result<bool, String> {
    let key = open(true)?;
    let name = wide(VALUE_NAME);
    let result = unsafe { RegDeleteValueW(key, name.as_ptr()) };
    unsafe { RegCloseKey(key) };
    match result {
        ERROR_SUCCESS => Ok(true),
        2 => Ok(false), // ERROR_FILE_NOT_FOUND: nothing was installed
        other => Err(format!(
            "cannot delete the Run value: Windows error {other}"
        )),
    }
}

/// Read the configured command line, when present.
pub fn installed_command() -> Result<Option<String>, String> {
    let key = open(false)?;
    let name = wide(VALUE_NAME);
    let mut kind = 0u32;
    let mut size = 0u32;
    let mut result = unsafe {
        RegQueryValueExW(
            key,
            name.as_ptr(),
            std::ptr::null_mut(),
            &mut kind,
            std::ptr::null_mut(),
            &mut size,
        )
    };
    if result != ERROR_SUCCESS {
        unsafe { RegCloseKey(key) };
        return match result {
            2 => Ok(None),
            other => Err(format!("cannot read the Run value: Windows error {other}")),
        };
    }
    let mut buffer = vec![0u16; (size as usize / 2) + 1];
    result = unsafe {
        RegQueryValueExW(
            key,
            name.as_ptr(),
            std::ptr::null_mut(),
            &mut kind,
            buffer.as_mut_ptr() as *mut u8,
            &mut size,
        )
    };
    unsafe { RegCloseKey(key) };
    if result != ERROR_SUCCESS {
        return Err(format!("cannot read the Run value: Windows error {result}"));
    }
    while matches!(buffer.last(), Some(0)) {
        buffer.pop();
    }
    Ok(Some(String::from_utf16_lossy(&buffer)))
}
