//! Run-at-login registration.
//!
//! Windows takes this from `HKCU\Software\Microsoft\Windows\CurrentVersion\Run`,
//! which needs no elevation and no scheduled-task machinery. The command is the
//! executable's own path, so it keeps working after the installer puts the app
//! somewhere else — but it also means the value has to be refreshed when the
//! path changes, which is what [`sync`] is for.

use anyhow::{Context, Result};
use windows::Win32::Foundation::{ERROR_FILE_NOT_FOUND, ERROR_SUCCESS};
use windows::Win32::System::Registry::{
    HKEY, HKEY_CURRENT_USER, KEY_QUERY_VALUE, KEY_SET_VALUE, REG_OPTION_NON_VOLATILE, REG_SZ,
    RegCloseKey, RegDeleteValueW, RegOpenKeyExW, RegQueryValueExW, RegSetValueExW,
};

/// Where Windows keeps the per-user startup commands.
const RUN_KEY: &str = "Software\\Microsoft\\Windows\\CurrentVersion\\Run";
/// Value name; also what the user sees in Task Manager's startup list.
const VALUE_NAME: &str = "xshot";

/// The command line Windows should run at login.
fn command() -> Result<String> {
    let exe = std::env::current_exe().context("locating our own executable")?;
    Ok(format!("\"{}\"", exe.display()))
}

fn open_key(access: windows::Win32::System::Registry::REG_SAM_FLAGS) -> Result<HKEY> {
    let path = crate::win::util::wide(RUN_KEY);
    let mut key = HKEY::default();
    let status = unsafe {
        RegOpenKeyExW(
            HKEY_CURRENT_USER,
            windows::core::PCWSTR(path.as_ptr()),
            None,
            access,
            &mut key,
        )
    };
    if status != ERROR_SUCCESS {
        anyhow::bail!("opening HKCU\\{RUN_KEY} failed with {status:?}");
    }
    Ok(key)
}

/// The command currently registered, if any.
pub fn registered() -> Option<String> {
    let key = open_key(KEY_QUERY_VALUE).ok()?;
    let name = crate::win::util::wide(VALUE_NAME);
    let mut kind = windows::Win32::System::Registry::REG_VALUE_TYPE::default();
    let mut size = 0u32;
    let status = unsafe {
        RegQueryValueExW(
            key,
            windows::core::PCWSTR(name.as_ptr()),
            None,
            Some(&mut kind),
            None,
            Some(&mut size),
        )
    };
    if status != ERROR_SUCCESS || size == 0 {
        unsafe {
            let _ = RegCloseKey(key);
        }
        return None;
    }
    let mut buffer = vec![0u8; size as usize];
    let status = unsafe {
        RegQueryValueExW(
            key,
            windows::core::PCWSTR(name.as_ptr()),
            None,
            None,
            Some(buffer.as_mut_ptr()),
            Some(&mut size),
        )
    };
    unsafe {
        let _ = RegCloseKey(key);
    }
    if status != ERROR_SUCCESS {
        return None;
    }
    // UTF-16, with the terminating null included in the size.
    let units: Vec<u16> = buffer
        .chunks_exact(2)
        .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
        .take_while(|unit| *unit != 0)
        .collect();
    Some(String::from_utf16_lossy(&units))
}

pub fn is_enabled() -> bool {
    registered().is_some()
}

/// Adds or removes the startup entry.
pub fn set_enabled(enabled: bool) -> Result<()> {
    let key = open_key(KEY_SET_VALUE)?;
    let name = crate::win::util::wide(VALUE_NAME);
    let status = if enabled {
        let command = crate::win::util::wide(&command()?);
        let bytes = unsafe {
            std::slice::from_raw_parts(
                command.as_ptr() as *const u8,
                command.len() * std::mem::size_of::<u16>(),
            )
        };
        unsafe {
            RegSetValueExW(
                key,
                windows::core::PCWSTR(name.as_ptr()),
                None,
                REG_SZ,
                Some(bytes),
            )
        }
    } else {
        let status = unsafe { RegDeleteValueW(key, windows::core::PCWSTR(name.as_ptr())) };
        // Removing something that was never there is not a failure.
        if status == ERROR_FILE_NOT_FOUND {
            ERROR_SUCCESS
        } else {
            status
        }
    };
    unsafe {
        let _ = RegCloseKey(key);
    }
    if status != ERROR_SUCCESS {
        anyhow::bail!("writing the startup entry failed with {status:?}");
    }
    let _ = REG_OPTION_NON_VOLATILE;
    Ok(())
}

/// Makes the registry agree with the config, rewriting the path if it is stale.
///
/// Called once at startup: an installer that moved the executable would
/// otherwise leave a startup entry pointing at nothing.
pub fn sync(enabled: bool) {
    if !enabled {
        if is_enabled() {
            let _ = set_enabled(false);
        }
        return;
    }
    let wanted = command().unwrap_or_default();
    if registered().as_deref() != Some(wanted.as_str()) {
        if let Err(error) = set_enabled(true) {
            eprintln!("xshot: could not register startup: {error:#}");
        }
    }
}
