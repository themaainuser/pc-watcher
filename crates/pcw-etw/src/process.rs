//! PID → (process name, path) via the Win32 API.
//!
//! This is the only Win32 process lookup in the project, so `pcw-agent` can
//! build the one shared `ProcessCache` without importing `windows` itself.

use windows::Win32::System::Threading::{
    OpenProcess, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION, QueryFullProcessImageNameW,
};

/// Longest process image path is well under this (paths cap at 32767 chars,
/// and `QueryFullProcessImageNameW` fails rather than truncating).
const PATH_BUF_CHARS: usize = 1024;

/// Opens a process and returns its image path as `(file name, full path)`.
///
/// Service/exe paths resolve via `PROCESS_NAME_WIN32`, so paths are DOS-style
/// (`C:\…`) whenever the process still exists. Fails (→ `None`) when the
/// process is gone or access is denied (protected system processes).
pub fn query_process(pid: u32) -> Option<(String, String)> {
    // System idle/normal-priority placeholder processes can never be opened.
    if pid == 0 || pid == 4 {
        return None;
    }
    unsafe {
        let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()?;
        let mut buf = [0u16; PATH_BUF_CHARS];
        let mut len = buf.len() as u32;
        let result = QueryFullProcessImageNameW(
            handle,
            PROCESS_NAME_WIN32,
            windows::core::PWSTR(buf.as_mut_ptr()),
            &mut len,
        );
        let _ = windows::Win32::Foundation::CloseHandle(handle);
        result.ok()?;
        let path = String::from_utf16_lossy(&buf[..len as usize]);
        Some((pcw_types::file_name_of(&path), path))
    }
}
