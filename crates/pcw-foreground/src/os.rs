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
use windows::Win32::Foundation::{
    ERROR_CLASS_ALREADY_EXISTS, GetLastError, HANDLE, HINSTANCE, HMODULE, HWND, LPARAM, LRESULT,
    WPARAM,
};
use windows::Win32::Graphics::Gdi::HBRUSH;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Power::{
    HPOWERNOTIFY, POWERBROADCAST_SETTING, RegisterPowerSettingNotification,
    UnregisterPowerSettingNotification,
};
use windows::Win32::System::RemoteDesktop::{
    NOTIFY_FOR_THIS_SESSION, WTSRegisterSessionNotification, WTSUnRegisterSessionNotification,
};
use windows::Win32::System::SystemServices::GUID_CONSOLE_DISPLAY_STATE;
use windows::Win32::System::Threading::GetCurrentThreadId;
use windows::Win32::UI::Accessibility::{HWINEVENTHOOK, SetWinEventHook, UnhookWinEvent};
use windows::Win32::UI::WindowsAndMessaging::{
    CHILDID_SELF, CreateWindowExW, DEVICE_NOTIFY_WINDOW_HANDLE, DefWindowProcW, DestroyWindow,
    DispatchMessageW, EVENT_OBJECT_NAMECHANGE, EVENT_SYSTEM_FOREGROUND, GetForegroundWindow,
    GetMessageW, GetWindowTextW, GetWindowThreadProcessId, HCURSOR, HICON, MSG, OBJID_WINDOW,
    PBT_APMRESUMEAUTOMATIC, PBT_APMSUSPEND, PBT_POWERSETTINGCHANGE, PM_NOREMOVE, PeekMessageW,
    PostThreadMessageW, RegisterClassW, UnregisterClassW, WINEVENT_OUTOFCONTEXT,
    WINEVENT_SKIPOWNPROCESS, WM_POWERBROADCAST, WM_QUIT, WM_WTSSESSION_CHANGE, WNDCLASS_STYLES,
    WNDCLASSW, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_OVERLAPPED, WTS_SESSION_LOCK,
    WTS_SESSION_UNLOCK,
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
    /// Current `EVENT_OBJECT_NAMECHANGE` hook in its swap slot; unhooked here
    /// on the hook thread, both when re-scoped and in the post-loop teardown.
    title_hook: Arc<Mutex<Option<usize>>>,
}

thread_local! {
    /// Callback context of the hook thread. Callbacks are sequential on one
    /// thread; `with_ctx` uses `try_borrow_mut` so an unexpected re-entry
    /// drops the event rather than panicking.
    static HOOK_CTX: std::cell::RefCell<Option<HookCtx>> = const { std::cell::RefCell::new(None) };
}

/// Runs `f` with the hook-thread context; `None` when callbacks fire before
/// the context is installed or after it was removed, or when the borrow fails.
///
/// Uses `try_borrow_mut` so a re-entrant call (WinEvent callbacks can
/// re-enter per MSDN) bails out instead of panicking — a panic inside an
/// `extern "system"` callback aborts the process, so a dropped event is the
/// only safe response.
fn with_ctx<R>(f: impl FnOnce(&HookCtx) -> R) -> Option<R> {
    HOOK_CTX.with(|slot| {
        let slot = slot.try_borrow_mut().ok()?;
        slot.as_ref().map(f)
    })
}

/// Lock the shared title-hook slot, tolerating a poisoned mutex (the callbacks
/// never panic, so this only guards against external misuse).
fn lock_title_hook(slot: &Mutex<Option<usize>>) -> MutexGuard<'_, Option<usize>> {
    slot.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Everything installed in Win32 during [`setup`]: one handle per registered
/// resource plus the thread id. It lives on the hook thread only (the Win32
/// wrapper types are not `Send`); the readiness message carries just the
/// thread id across the channel.
#[derive(Clone, Copy)]
struct HookResources {
    thread_id: u32,
    hmodule: HMODULE,
    hwnd: HWND,
    fg_hook: usize,
    power_notify: usize,
}

impl HookResources {
    fn empty() -> Self {
        Self {
            thread_id: 0,
            hmodule: HMODULE(std::ptr::null_mut()),
            hwnd: HWND(std::ptr::null_mut()),
            fg_hook: 0,
            power_notify: 0,
        }
    }
}

/// Handle to the running hook thread; dropping it (not just calling
/// [`HookHandle::stop`]) also shuts the thread down.
pub struct HookHandle {
    thread_id: u32,
    join: Option<JoinHandle<()>>,
}

impl HookHandle {
    /// Posts `WM_QUIT` to the hook thread and joins it. All teardown happens
    /// ON the hook thread, after its message loop returns: unhooking both
    /// win-event hooks (`UnhookWinEvent` is only valid on the installing
    /// thread), `WTSUnRegisterSessionNotification`,
    /// `UnregisterPowerSettingNotification`, `DestroyWindow` and
    /// `UnregisterClassW`. `stop` merely wakes the loop and waits; it is
    /// idempotent, and the same shutdown runs when the handle is dropped.
    pub fn stop(mut self) {
        self.shutdown();
    }

    fn shutdown(&mut self) {
        // Wake the message loop FIRST so the thread exits even when it is
        // blocked in GetMessageW; the thread then tears everything down
        // before exiting. Any unhooking from this thread would be a no-op
        // per the MSDN thread rule, so none happens here.
        unsafe {
            if self.thread_id != 0 {
                let _ = PostThreadMessageW(self.thread_id, WM_QUIT, WPARAM(0), LPARAM(0));
                self.thread_id = 0;
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
    let (ready_tx, ready_rx) = channel::<Result<u32, String>>();
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
        Ok(Ok(thread_id)) => Ok(HookHandle {
            thread_id,
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
/// class, hidden window and hooks, report readiness, pump messages, then tear
/// everything down (still on this thread) and drop the context.
fn hook_thread(
    tx: Sender<SessionSignal>,
    cache: SharedProcessCache,
    capture_titles: bool,
    title_hook: Arc<Mutex<Option<usize>>>,
    ready: Sender<Result<u32, String>>,
) {
    HOOK_CTX.with(|slot| {
        *slot.borrow_mut() = Some(HookCtx {
            tx,
            cache,
            capture_titles,
            title_hook: title_hook.clone(),
        });
    });
    match unsafe { setup() } {
        Ok(res) => {
            // The thread must own a message queue before readiness is
            // reported: `PostThreadMessageW(WM_QUIT)` from the spawner fails
            // until a queue exists. A non-removing peek creates it and
            // returns immediately, so `stop` can never miss the thread.
            let mut peek = MSG::default();
            let _ = unsafe {
                PeekMessageW(
                    &mut peek,
                    Some(HWND(std::ptr::null_mut())),
                    0,
                    0,
                    PM_NOREMOVE,
                )
            };
            // Readiness goes out before the message loop, as required; the
            // resources stay here because teardown must run on this thread.
            let _ = ready.send(Ok(res.thread_id));
            unsafe { pump_messages() };
            // Authoritative teardown, on the same thread that installed
            // everything: hooks, session/power registrations, window and
            // window class. This is what makes `start_hooks` restartable
            // in the same process.
            if let Some(hook) = lock_title_hook(&title_hook).take() {
                let _ = unsafe { UnhookWinEvent(HWINEVENTHOOK(hook as *mut core::ffi::c_void)) };
            }
            unsafe { teardown(&res) };
        }
        Err(msg) => {
            let _ = ready.send(Err(msg));
        }
    }
    HOOK_CTX.with(|slot| *slot.borrow_mut() = None);
}

/// Registers everything; failures are returned to the spawner as a string
/// that includes the Win32 error code. Anything registered before a failure
/// is torn down before the error is reported.
///
/// # Safety
/// Must run on the hook thread before the message loop; creates real Win32
/// resources on that thread.
unsafe fn setup() -> Result<HookResources, String> {
    unsafe {
        let mut res = HookResources::empty();
        match register_all(&mut res) {
            Ok(()) => Ok(res),
            Err(msg) => {
                // Unroll the partial setup so a failed start cannot leak
                // resources (e.g. the window class) into the process.
                teardown(&res);
                Err(msg)
            }
        }
    }
}

/// One registration step after another, recording each handle in `res` as it
/// is created.
///
/// # Safety
/// Must run on the hook thread; see [`setup`].
unsafe fn register_all(res: &mut HookResources) -> Result<(), String> {
    unsafe {
        res.thread_id = GetCurrentThreadId();
        let hmodule: HMODULE =
            GetModuleHandleW(None).map_err(|e| format!("GetModuleHandleW failed: {e}"))?;
        res.hmodule = hmodule;
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
            // The class is registered under our name and our module, so an
            // existing registration can only be a leftover from an earlier
            // start attempt in this process: ours to reuse instead of
            // failing the restart.
            let code = GetLastError();
            if code != ERROR_CLASS_ALREADY_EXISTS {
                return Err(format!("RegisterClassW failed ({})", code.0));
            }
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
        res.hwnd = hwnd;

        WTSRegisterSessionNotification(hwnd, NOTIFY_FOR_THIS_SESSION)
            .map_err(|e| format!("WTSRegisterSessionNotification failed: {e}"))?;

        let power_notification = RegisterPowerSettingNotification(
            HANDLE(hwnd.0),
            &GUID_CONSOLE_DISPLAY_STATE,
            DEVICE_NOTIFY_WINDOW_HANDLE,
        )
        .map_err(|e| format!("RegisterPowerSettingNotification failed: {e}"))?;
        res.power_notify = power_notification.0 as usize;

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
        res.fg_hook = fg_hook.0 as usize;

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

        Ok(())
    }
}

/// Full teardown of the resources in `res`. Runs on the hook thread:
/// `UnhookWinEvent` must be called from the thread that installed the hook,
/// and the window/class live on it too. This is the authoritative cleanup —
/// what makes `start_hooks` restartable in the same process (otherwise
/// `RegisterClassW` fails with `ERROR_CLASS_ALREADY_EXISTS` and the restart
/// is dead). The scoped title hook is unhooked by the caller first, from the
/// same slot [`scope_title_hook`] swaps into.
///
/// # Safety
/// Must run on the hook thread; destroys real Win32 resources. Safe with an
/// empty/zeroed `res` (nothing was registered yet) for the partial-setup case.
unsafe fn teardown(res: &HookResources) {
    unsafe {
        if res.fg_hook != 0 {
            let _ = UnhookWinEvent(HWINEVENTHOOK(res.fg_hook as *mut core::ffi::c_void));
        }
        if !res.hwnd.0.is_null() {
            // Errors ignored: a failed unregister call only means the
            // process exit cleans the resource up instead.
            if res.power_notify != 0 {
                let _ = UnregisterPowerSettingNotification(HPOWERNOTIFY(res.power_notify as isize));
            }
            let _ = WTSUnRegisterSessionNotification(res.hwnd);
            let _ = DestroyWindow(res.hwnd);
        }
        if !res.hmodule.0.is_null() {
            let class_wide = null_terminated_utf16(CLASS_NAME);
            let _ = UnregisterClassW(
                PCWSTR::from_raw(class_wide.as_ptr()),
                Some(HINSTANCE(res.hmodule.0)),
            );
        }
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
        // Runs on the hook thread, inside the WinEvent callback: on a cache
        // miss the `query` closure owned by `ProcessCache` executes HERE.
        // Constraints the cache owner must honour:
        // - the query must be bounded and non-blocking (quick syscalls such
        //   as OpenProcess + QueryFullProcessImageNameW); a slow or blocked
        //   lookup would stall this thread's message pump and delay
        //   lock/unlock, power and display-state processing;
        // - the query must never block on another thread that might be
        //   waiting for this one.
        // Keeping the query inside the shared `ProcessCache` necessarily
        // holds the cache mutex for the lookup's duration: `ProcessCache`
        // offers no API to resolve a miss without the mutex, and
        // `SessionSignal` carries already-resolved names, so there is no way
        // to defer resolution past this callback without changing the
        // `start_hooks` signature (pre-defined interface). Cache hits — the
        // common case — take an uncontended mutex and a hashmap read only.
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
