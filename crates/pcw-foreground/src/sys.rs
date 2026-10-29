//! Small Win32 helpers for the agent layers: console attach, elevation check,
//! free-disk-space query, the `TaskbarCreated` message filter and the
//! manual-start guards (single instance, UAC self-relaunch, message box).
//! Each helper maps a Win32 failure to `None`/`false`/no-op rather than
//! panicking.

use std::ffi::OsStr;
use std::os::windows::ffi::OsStrExt;
use std::path::Path;

use windows::Win32::Foundation::{CloseHandle, ERROR_ALREADY_EXISTS, GetLastError, HANDLE};
use windows::Win32::Security::{GetTokenInformation, TOKEN_ELEVATION, TOKEN_QUERY, TokenElevation};
use windows::Win32::Storage::FileSystem::GetDiskFreeSpaceExW;
use windows::Win32::System::Console::{ATTACH_PARENT_PROCESS, AttachConsole};
use windows::Win32::System::Threading::{CreateMutexW, GetCurrentProcess, OpenProcessToken};
use windows::Win32::UI::Shell::ShellExecuteW;
use windows::Win32::UI::WindowsAndMessaging::{
    ChangeWindowMessageFilter, MB_ICONWARNING, MB_OK, MB_SETFOREGROUND, MSGFLT_ADD, MessageBoxW,
    RegisterWindowMessageW, SW_SHOWNORMAL,
};
use windows::core::{PCWSTR, w};

/// Attaches this process to the parent process's console so a console
/// launched from a terminal can receive diagnostics output. Returns `true`
/// only when THIS call newly attached a console: `false` also means "already
/// attached" (`AttachConsole` fails with `ERROR_ACCESS_DENIED` for a process
/// that already has a console) as well as the no-parent-console case, and a
/// tray app without a console keeps going silently either way.
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
        let mut elevation = TOKEN_ELEVATION::default();
        let mut return_length = 0u32;
        let ok = GetTokenInformation(
            token,
            TokenElevation,
            Some(&mut elevation as *mut TOKEN_ELEVATION as *mut core::ffi::c_void),
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

/// Named-mutex guard proving "this is the only running copy": [`acquire`]
/// returns `None` when another process already holds `name`. The kernel
/// closes the handle on any exit — including a crash — so the lock can
/// never outlive its process.
pub struct InstanceLock(HANDLE);

impl InstanceLock {
    /// Acquires `name`, or `None` when another copy already holds it. A
    /// `CreateMutexW` failure (e.g. out of memory) fails OPEN — refusing to
    /// start the only copy would be worse than risking a duplicate.
    #[must_use]
    pub fn acquire(name: &str) -> Option<Self> {
        let wide = null_terminated_utf16(OsStr::new(name));
        unsafe {
            let Ok(mutex) = CreateMutexW(None, false, PCWSTR::from_raw(wide.as_ptr())) else {
                return Some(Self(HANDLE(std::ptr::null_mut())));
            };
            if GetLastError() == ERROR_ALREADY_EXISTS {
                let _ = CloseHandle(mutex);
                return None;
            }
            Some(Self(mutex))
        }
    }
}

impl Drop for InstanceLock {
    fn drop(&mut self) {
        if !self.0.is_invalid() {
            unsafe {
                let _ = CloseHandle(self.0);
            }
        }
    }
}

/// Relaunches the current exe elevated via the UAC prompt (`runas`), for the
/// manual-start flow: ETW capture needs an admin token and there is no
/// scheduled task to supply one. `Err` covers the user declining the prompt
/// (`ERROR_CANCELLED`) and every other `ShellExecuteW` failure.
pub fn relaunch_elevated() -> Result<(), String> {
    let exe = std::env::current_exe().map_err(|e| format!("cannot locate the running exe: {e}"))?;
    let file = null_terminated_utf16(exe.as_os_str());
    let verb = null_terminated_utf16(OsStr::new("runas"));
    unsafe {
        let result = ShellExecuteW(
            None,
            PCWSTR::from_raw(verb.as_ptr()),
            PCWSTR::from_raw(file.as_ptr()),
            None,
            None,
            SW_SHOWNORMAL,
        );
        // ShellExecuteW reports failure as a return value <= 32.
        let code = result.0 as isize;
        if code <= 32 {
            return Err(format!("ShellExecuteW(runas) failed with code {code}"));
        }
    }
    Ok(())
}

/// Best-effort modal warning for a GUI-subsystem app (no console): used for
/// "already running" and "elevation declined". A failed box is ignored —
/// there is no better fallback than the tray/log the caller already has.
pub fn warn_user(message: &str) {
    let wide = null_terminated_utf16(OsStr::new(message));
    unsafe {
        let _ = MessageBoxW(
            None,
            PCWSTR::from_raw(wide.as_ptr()),
            w!("PC-Watcher"),
            MB_OK | MB_ICONWARNING | MB_SETFOREGROUND,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::InstanceLock;

    #[test]
    fn second_lock_on_the_same_name_is_refused() {
        let name = format!("PC-Watcher-Test-{}", std::process::id());
        let first = InstanceLock::acquire(&name);
        assert!(first.is_some(), "fresh name must be acquirable");
        assert!(
            InstanceLock::acquire(&name).is_none(),
            "a held name must refuse a second lock"
        );
        drop(first);
        assert!(
            InstanceLock::acquire(&name).is_some(),
            "a released name must be acquirable again"
        );
    }
}
