//! Small Win32 helpers for the agent layers: console attach, elevation check,
//! free-disk-space query and the `TaskbarCreated` message filter. Each helper
//! maps a Win32 failure to `None`/`false`/no-op rather than panicking.

use std::ffi::OsStr;
use std::os::windows::ffi::OsStrExt;
use std::path::Path;

use windows::Win32::Foundation::{CloseHandle, HANDLE};
use windows::Win32::Security::{GetTokenInformation, TOKEN_ELEVATION, TOKEN_QUERY, TokenElevation};
use windows::Win32::Storage::FileSystem::GetDiskFreeSpaceExW;
use windows::Win32::System::Console::{ATTACH_PARENT_PROCESS, AttachConsole};
use windows::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
use windows::Win32::UI::WindowsAndMessaging::{
    ChangeWindowMessageFilter, MSGFLT_ADD, RegisterWindowMessageW,
};
use windows::core::{PCWSTR, w};

/// Attaches this process to the parent process's console so a console
/// launched from a terminal can receive diagnostics output. Returns `true`
/// when a console was attached (a tray app without a console keeps going
/// silently).
pub fn attach_parent_console() -> bool {
    unsafe { AttachConsole(ATTACH_PARENT_PROCESS).is_ok() }
}

/// `true` when the current process runs with an elevated (admin) token.
pub fn is_elevated() -> bool {
    unsafe {
        let mut token = HANDLE(std::ptr::null_mut());
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token).is_err() {
            return false;
        }
        let elevation = TOKEN_ELEVATION::default();
        let mut return_length = 0u32;
        let ok = GetTokenInformation(
            token,
            TokenElevation,
            Some(
                &elevation as *const TOKEN_ELEVATION as *const core::ffi::c_void
                    as *mut core::ffi::c_void,
            ),
            std::mem::size_of::<TOKEN_ELEVATION>() as u32,
            &mut return_length,
        )
        .is_ok();
        let _ = CloseHandle(token);
        ok && elevation.TokenIsElevated != 0
    }
}

/// Bytes still available to the user on the volume holding `path`, or `None`
/// when the volume cannot be queried (e.g. the drive is gone).
pub fn free_disk_bytes(path: &Path) -> Option<u64> {
    let wide = null_terminated_utf16(path.as_os_str());
    let mut free_to_caller = 0u64;
    unsafe {
        GetDiskFreeSpaceExW(
            PCWSTR::from_raw(wide.as_ptr()),
            Some(&mut free_to_caller),
            None,
            None,
        )
        .ok()?;
    }
    Some(free_to_caller)
}

/// Allow the (blocked) `TaskbarCreated` window message so a tray icon can be
/// re-registered after explorer restarts. Errors are ignored: the worst case
/// is that the tray icon does not reappear until the agent restarts.
pub fn allow_taskbar_created_message() {
    unsafe {
        let message = RegisterWindowMessageW(w!("TaskbarCreated"));
        let _ = ChangeWindowMessageFilter(message, MSGFLT_ADD);
    }
}

/// `OsStr` as a NUL-terminated UTF-16 buffer.
fn null_terminated_utf16(value: &OsStr) -> Vec<u16> {
    value.encode_wide().chain(std::iter::once(0)).collect()
}
