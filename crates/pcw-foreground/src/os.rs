//! Win32 adapter for the foreground tracker (spec §4.3).
//!
//! [`start_hooks`] runs on one dedicated thread: it registers a window class,
//! creates a hidden *top-level* window (required because WTS session
//! notifications and power notifications must target a real message window),
//! subscribes to session lock/unlock, power and display-state messages, sets
//! an `EVENT_SYSTEM_FOREGROUND` win-event hook, and then pumps messages. Each
//! foreground change re-scopes an `EVENT_OBJECT_NAMECHANGE` hook to the
//! foreground process so browser-tab title changes are detected. Events are
//! sent as [`SessionSignal`]s into the channel supplied by the tracker owner
//! (Task 12).
//!
//! Callback bodies stay minimal and panic-free: everything unsafe in the
//! callbacks runs inside `with_ctx`, which is a no-op while the context is
//! unset, and only sends on a channel.

use std::sync::mpsc::{Sender, channel};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::{self, JoinHandle};
use std::time::{SystemTime, UNIX_EPOCH};

use pcw_types::SharedProcessCache;
use windows::Win32::Foundation::{HANDLE, HINSTANCE, HMODULE, HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::Graphics::Gdi::HBRUSH;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Power::{
    HPOWERNOTIFY, POWERBROADCAST_SETTING, RegisterPowerSettingNotification,
    UnregisterPowerSettingNotification,
};
use windows::Win32::System::RemoteDesktop::{
    NOTIFY_FOR_THIS_SESSION, WTSRegisterSessionNotification,
};
use windows::Win32::System::SystemServices::GUID_CONSOLE_DISPLAY_STATE;
use windows::Win32::System::Threading::GetCurrentThreadId;
use windows::Win32::UI::Accessibility::{HWINEVENTHOOK, SetWinEventHook, UnhookWinEvent};
use windows::Win32::UI::WindowsAndMessaging::{
    CHILDID_SELF, CreateWindowExW, DEVICE_NOTIFY_WINDOW_HANDLE, DefWindowProcW, DispatchMessageW,
    EVENT_OBJECT_NAMECHANGE, EVENT_SYSTEM_FOREGROUND, GetForegroundWindow, GetMessageW,
    GetWindowTextW, GetWindowThreadProcessId, HCURSOR, HICON, MSG, OBJID_WINDOW,
    PBT_APMRESUMEAUTOMATIC, PBT_APMSUSPEND, PBT_POWERSETTINGCHANGE, PostThreadMessageW,
    RegisterClassW, WINEVENT_OUTOFCONTEXT, WINEVENT_SKIPOWNPROCESS, WM_POWERBROADCAST, WM_QUIT,
    WM_WTSSESSION_CHANGE, WNDCLASS_STYLES, WNDCLASSW, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW,
    WS_OVERLAPPED, WTS_SESSION_LOCK, WTS_SESSION_UNLOCK,
};
use windows::core::PCWSTR;

use crate::tracker::SessionSignal;

/// Hidden window class used by the hook thread.
const CLASS_NAME: &str = "PcwForegroundWatch";
/// Window title of the hidden window (invisible, but required to be set).
const WINDOW_TITLE: &str = "pcw-foreground";
/// Buffer for `GetWindowTextW` (long enough for any realistic window title).
const TITLE_BUFFER_LEN: usize = 512;

/// Why [`start_hooks`] failed: a Win32 call returned an error, or the hook
/// thread itself failed.
#[derive(Debug)]
pub enum OsError {
    Win32(windows::core::Error),
    Thread(String),
}

impl std::fmt::Display for OsError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Win32(e) => write!(f, "win32 error: {e}"),
            Self::Thread(msg) => write!(f, "hook thread error: {msg}"),
        }
    }
}

impl core::error::Error for OsError {}

impl From<windows::core::Error> for OsError {
    fn from(value: windows::core::Error) -> Self {
        Self::Win32(value)
    }
}

/// Data the WinEvent callbacks and the window procedure need. They only ever
/// run on the hook thread (win-event hooks without a target thread deliver on
/// the installing thread while it pumps messages), so the context lives in a
/// thread-local owned by that thread.
struct HookCtx {
    tx: Sender<SessionSignal>,
    cache: SharedProcessCache,
    capture_titles: bool,
    /// Current `EVENT_OBJECT_NAMECHANGE` hook, shared with [`HookHandle`]
    /// so `stop` can unhook it from another thread. Raw pointer bits to keep
    /// the type shareable (`HWINEVENTHOOK` is not `Send`/`Sync`).
    title_hook: Arc<Mutex<Option<usize>>>,
}

thread_local! {
    /// Callback context of the hook thread. Callbacks are sequential on one
    /// thread and never re-enter each other, so a short `borrow_mut` is safe.
    static HOOK_CTX: std::cell::RefCell<Option<HookCtx>> = const { std::cell::RefCell::new(None) };
}

/// Runs `f` with the hook-thread context; `None` when callbacks fire before
/// the context is installed (should never happen) or after it was removed.
fn with_ctx<R>(f: impl FnOnce(&HookCtx) -> R) -> Option<R> {
    HOOK_CTX.with(|slot| {
        let slot = slot.borrow_mut();
        slot.as_ref().map(f)
    })
}

/// Lock the shared title-hook slot, tolerating a poisoned mutex (the callbacks
/// never panic, so this only guards against external misuse).
fn lock_title_hook(slot: &Mutex<Option<usize>>) -> MutexGuard<'_, Option<usize>> {
    slot.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// What `stop` shuts down: posted messages target this thread id, unhooking
/// removes the hooks created on it.
struct ReadyMsg {
    thread_id: u32,
    fg_hook: usize,
    power_notify: usize,
}

/// Handle to the running hook thread; dropping it (not just calling
/// [`HookHandle::stop`]) also shuts the thread down.
pub struct HookHandle {
    fg_hook: usize,
    title_hook: Arc<Mutex<Option<usize>>>,
    power_notify: usize,
    thread_id: u32,
    join: Option<JoinHandle<()>>,
}

impl HookHandle {
    /// Posts `WM_QUIT` to the hook thread, removes both win-event hooks and
    /// unregisters the display-state notification, then joins the thread.
    /// Idempotent; the same shutdown runs when the handle is dropped.
    pub fn stop(mut self) {
        self.shutdown();
    }

    fn shutdown(&mut self) {
        // Order: wake the message loop FIRST so the thread exits even when it
        // is blocked in GetMessageW; then unhook; then join.
        unsafe {
            if self.thread_id != 0 {
                let _ = PostThreadMessageW(self.thread_id, WM_QUIT, WPARAM(0), LPARAM(0));
            }
            if self.fg_hook != 0 {
                let _ = UnhookWinEvent(HWINEVENTHOOK(self.fg_hook as *mut core::ffi::c_void));
                self.fg_hook = 0;
            }
            if let Some(hook) = lock_title_hook(&self.title_hook).take() {
                let _ = UnhookWinEvent(HWINEVENTHOOK(hook as *mut core::ffi::c_void));
            }
            if self.power_notify != 0 {
                // Errors ignored: process exit cleans the registration up.
                let _ =
                    UnregisterPowerSettingNotification(HPOWERNOTIFY(self.power_notify as isize));
                self.power_notify = 0;
            }
        }
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

impl Drop for HookHandle {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// Starts the hook thread. The returned [`HookHandle`] controls its lifetime.
///
/// `capture_titles == false` disables title capture entirely: no
/// `EVENT_OBJECT_NAMECHANGE` hook is registered, no title text is read.
pub fn start_hooks(
    tx: Sender<SessionSignal>,
    cache: SharedProcessCache,
    capture_titles: bool,
) -> Result<HookHandle, OsError> {
    let (ready_tx, ready_rx) = channel::<Result<ReadyMsg, String>>();
    let title_hook = Arc::new(Mutex::new(None));
    let title_hook_for_thread = title_hook.clone();
    let join = thread::Builder::new()
        .name("pcw-foreground-hooks".into())
        .spawn(move || hook_thread(tx, cache, capture_titles, title_hook_for_thread, ready_tx))
        .map_err(|e| OsError::Thread(format!("failed to spawn hook thread: {e}")))?;
    // The thread reports itself once everything is registered, right before
    // the message loop starts. That message carries the real thread id from
    // `GetCurrentThreadId` — never guessed on the spawning side.
    match ready_rx.recv() {
        Ok(Ok(ready)) => Ok(HookHandle {
            fg_hook: ready.fg_hook,
            title_hook,
            power_notify: ready.power_notify,
            thread_id: ready.thread_id,
            join: Some(join),
        }),
        Ok(Err(msg)) => {
            let _ = join.join();
            Err(OsError::Thread(msg))
        }
        Err(_) => {
            let _ = join.join();
            Err(OsError::Thread(
                "hook thread exited before signaling readiness".into(),
            ))
        }
    }
}

/// Body of the hook thread: install the callback context, register the window
/// class, hidden window and hooks, report readiness, then pump messages.
fn hook_thread(
    tx: Sender<SessionSignal>,
    cache: SharedProcessCache,
    capture_titles: bool,
    title_hook: Arc<Mutex<Option<usize>>>,
    ready: Sender<Result<ReadyMsg, String>>,
) {
    HOOK_CTX.with(|slot| {
        *slot.borrow_mut() = Some(HookCtx {
            tx,
            cache,
            capture_titles,
            title_hook,
        });
    });
    match unsafe { setup() } {
        Ok(ready_msg) => {
            // Readiness goes out before the message loop, as required.
            let _ = ready.send(Ok(ready_msg));
            unsafe { pump_messages() };
        }
        Err(msg) => {
            let _ = ready.send(Err(msg));
        }
    }
    HOOK_CTX.with(|slot| *slot.borrow_mut() = None);
}

/// Registers everything; each failure is returned to the spawner as a string.
///
/// # Safety
/// Must run on the hook thread before the message loop; creates real Win32
/// resources on that thread.
unsafe fn setup() -> Result<ReadyMsg, String> {
    unsafe {
        let hmodule: HMODULE =
            GetModuleHandleW(None).map_err(|e| format!("GetModuleHandleW failed: {e}"))?;
        let hinstance = HINSTANCE(hmodule.0);

        // NUL-terminated UTF-16 class/title; must outlive the API calls below.
        let class_wide = null_terminated_utf16(CLASS_NAME);
        let title_wide = null_terminated_utf16(WINDOW_TITLE);
        let class_pc = PCWSTR::from_raw(class_wide.as_ptr());
        let title_pc = PCWSTR::from_raw(title_wide.as_ptr());

        if RegisterClassW(&WNDCLASSW {
            style: WNDCLASS_STYLES(0),
            lpfnWndProc: Some(wnd_proc),
            cbClsExtra: 0,
            cbWndExtra: 0,
            hInstance: hinstance,
            hIcon: HICON(std::ptr::null_mut()),
            hCursor: HCURSOR(std::ptr::null_mut()),
            hbrBackground: HBRUSH(std::ptr::null_mut()),
            lpszMenuName: PCWSTR::null(),
            lpszClassName: class_pc,
        }) == 0
        {
            return Err("RegisterClassW failed".into());
        }

        // A hidden but real top-level window: HWND_MESSAGE children cannot
        // carry WTS or power notifications.
        let hwnd = CreateWindowExW(
            WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE,
            class_pc,
            title_pc,
            WS_OVERLAPPED,
            0,
            0,
            0,
            0,
            None,
            None,
            Some(hinstance),
            None,
        )
        .map_err(|e| format!("CreateWindowExW failed: {e}"))?;

        WTSRegisterSessionNotification(hwnd, NOTIFY_FOR_THIS_SESSION)
            .map_err(|e| format!("WTSRegisterSessionNotification failed: {e}"))?;

        let power_notification = RegisterPowerSettingNotification(
            HANDLE(hwnd.0),
            &GUID_CONSOLE_DISPLAY_STATE,
            DEVICE_NOTIFY_WINDOW_HANDLE,
        )
        .map_err(|e| format!("RegisterPowerSettingNotification failed: {e}"))?;

        let fg_hook = SetWinEventHook(
            EVENT_SYSTEM_FOREGROUND,
            EVENT_SYSTEM_FOREGROUND,
            None,
            Some(fg_win_event),
            0,
            0,
            WINEVENT_OUTOFCONTEXT | WINEVENT_SKIPOWNPROCESS,
        );
        if fg_hook.0.is_null() {
            return Err("SetWinEventHook(EVENT_SYSTEM_FOREGROUND) failed".into());
        }

        // Deliver the currently focused window as the first Foreground
        // signal, then scope the name-change hook to it.
        let current = GetForegroundWindow();
        if !current.0.is_null() {
            with_ctx(|ctx| {
                if let Some((pid, process, title)) = scan_window(current, ctx) {
                    let _ = ctx.tx.send(SessionSignal::Foreground {
                        ts_ms: now_ms(),
                        process_name: process,
                        window_title: title,
                    });
                    scope_title_hook(ctx, pid);
                }
            });
        }

        Ok(ReadyMsg {
            thread_id: GetCurrentThreadId(),
            fg_hook: fg_hook.0 as usize,
            power_notify: power_notification.0 as usize,
        })
    }
}

/// `GetMessageW` loop; `WM_QUIT` from [`HookHandle::stop`] or a `GetMessage`
/// error (-1) ends it.
///
/// # Safety
/// The message window and hooks must belong to the calling thread.
unsafe fn pump_messages() {
    let mut msg = MSG::default();
    loop {
        let result = unsafe { GetMessageW(&mut msg, None, 0, 0) };
        if result.0 <= 0 {
            break;
        }
        unsafe { DispatchMessageW(&msg) };
    }
}

/// Examines `hwnd` the one way all signal senders do it: pid via
/// `GetWindowThreadProcessId`, process name from the shared cache, title via
/// `GetWindowTextW` (empty when titles are not captured). `None` when the
/// window or its process cannot be identified.
///
/// # Safety
/// `hwnd` must be a live top-level window handle.
unsafe fn scan_window(hwnd: HWND, ctx: &HookCtx) -> Option<(u32, String, String)> {
    let mut pid = 0u32;
    unsafe { GetWindowThreadProcessId(hwnd, Some(&mut pid)) };
    if pid == 0 {
        return None;
    }
    let process = {
        let mut cache = ctx
            .cache
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        cache.get(pid).0
    };
    let title = if ctx.capture_titles {
        unsafe { window_title(hwnd) }
    } else {
        // Titles are not captured at all when disabled.
        String::new()
    };
    Some((pid, process, title))
}

/// Current text of `hwnd` (`""` when the text is empty or unreadable).
///
/// # Safety
/// `hwnd` must be a live window handle.
unsafe fn window_title(hwnd: HWND) -> String {
    let mut buffer = [0u16; TITLE_BUFFER_LEN];
    let copied = unsafe { GetWindowTextW(hwnd, &mut buffer) };
    if copied <= 0 {
        return String::new();
    }
    String::from_utf16_lossy(&buffer[..(copied as usize).min(TITLE_BUFFER_LEN)])
}

/// Swaps the `EVENT_OBJECT_NAMECHANGE` hook to `pid`. Called on foreground
/// changes and once for the window focused at start-up; unscoped (and not
/// registered again) when titles are not captured.
///
/// # Safety
/// Current thread must be the hook thread (it will receive the callbacks).
unsafe fn scope_title_hook(ctx: &HookCtx, pid: u32) {
    let mut slot = lock_title_hook(&ctx.title_hook);
    if let Some(old) = slot.take() {
        let _ = unsafe { UnhookWinEvent(HWINEVENTHOOK(old as *mut core::ffi::c_void)) };
    }
    if !ctx.capture_titles || pid == 0 {
        return;
    }
    let hook = unsafe {
        SetWinEventHook(
            EVENT_OBJECT_NAMECHANGE,
            EVENT_OBJECT_NAMECHANGE,
            None,
            Some(title_win_event),
            pid,
            0,
            WINEVENT_OUTOFCONTEXT | WINEVENT_SKIPOWNPROCESS,
        )
    };
    if !hook.0.is_null() {
        *slot = Some(hook.0 as usize);
    }
}

/// `EVENT_SYSTEM_FOREGROUND`: a new window got the focus.
unsafe extern "system" fn fg_win_event(
    hook: HWINEVENTHOOK,
    event: u32,
    hwnd: HWND,
    idobject: i32,
    idchild: i32,
    ideventthread: u32,
    dwmseventtime: u32,
) {
    // Callback params beyond the window handle are unused.
    let _ = (hook, event, idobject, idchild, ideventthread, dwmseventtime);
    unsafe {
        if hwnd.0.is_null() {
            return;
        }
        with_ctx(|ctx| {
            let Some((pid, process, title)) = scan_window(hwnd, ctx) else {
                return;
            };
            let _ = ctx.tx.send(SessionSignal::Foreground {
                ts_ms: now_ms(),
                process_name: process,
                window_title: title,
            });
            scope_title_hook(ctx, pid);
        });
    }
}

/// `EVENT_OBJECT_NAMECHANGE` (scoped to the foreground pid): the foreground
/// window changed its title (browser tab, document, …).
unsafe extern "system" fn title_win_event(
    hook: HWINEVENTHOOK,
    event: u32,
    hwnd: HWND,
    idobject: i32,
    idchild: i32,
    ideventthread: u32,
    dwmseventtime: u32,
) {
    let _ = (hook, event, ideventthread, dwmseventtime);
    unsafe {
        // Ignore anything that is not exactly the foreground window's own
        // title change; the pid-scoped hook filters most noise already, but
        // windows in the process may change other object titles.
        if hwnd.0.is_null() || hwnd.0 != GetForegroundWindow().0 {
            return;
        }
        if idobject != OBJID_WINDOW.0 || idchild != CHILDID_SELF as i32 {
            return;
        }
        with_ctx(|ctx| {
            let Some((_, process, title)) = scan_window(hwnd, ctx) else {
                return;
            };
            let _ = ctx.tx.send(SessionSignal::Title {
                ts_ms: now_ms(),
                process_name: process,
                window_title: title,
            });
        });
    }
}

/// Window procedure of the hidden window: session lock/unlock and power /
/// display-state transitions become the paired tracker signals.
unsafe extern "system" fn wnd_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match msg {
        WM_WTSSESSION_CHANGE => {
            with_ctx(|ctx| {
                let sig = match wparam.0 as u32 {
                    WTS_SESSION_LOCK => SessionSignal::Locked { ts_ms: now_ms() },
                    WTS_SESSION_UNLOCK => SessionSignal::Unlocked { ts_ms: now_ms() },
                    _ => return,
                };
                let _ = ctx.tx.send(sig);
            });
            LRESULT(0)
        }
        WM_POWERBROADCAST => {
            with_ctx(|ctx| {
                let sig = match wparam.0 as u32 {
                    PBT_APMSUSPEND => Some(SessionSignal::Suspended { ts_ms: now_ms() }),
                    PBT_APMRESUMEAUTOMATIC => Some(SessionSignal::Resumed { ts_ms: now_ms() }),
                    PBT_POWERSETTINGCHANGE => unsafe { display_setting_signal(lparam) },
                    _ => None,
                };
                if let Some(sig) = sig {
                    let _ = ctx.tx.send(sig);
                }
            });
            LRESULT(1) // TRUE: the power message was handled.
        }
        _ => unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
    }
}

/// `WM_POWERBROADCAST` with `PBT_POWERSETTINGCHANGE`: the registered
/// `GUID_CONSOLE_DISPLAY_STATE` notification. Data 0 = display off,
/// 1 (Console) / 2 (Monitor) = display on.
unsafe fn display_setting_signal(lparam: LPARAM) -> Option<SessionSignal> {
    if lparam.0 == 0 {
        return None;
    }
    let setting = unsafe { &*(lparam.0 as *const POWERBROADCAST_SETTING) };
    if setting.PowerSetting != GUID_CONSOLE_DISPLAY_STATE || setting.DataLength < 1 {
        return None;
    }
    let ts_ms = now_ms();
    match setting.Data[0] {
        0 => Some(SessionSignal::DisplayOff { ts_ms }),
        1 | 2 => Some(SessionSignal::DisplayOn { ts_ms }),
        _ => None,
    }
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// NUL-terminated UTF-16 buffer (as Win32 wants it for class/window names).
fn null_terminated_utf16(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(std::iter::once(0)).collect()
}
