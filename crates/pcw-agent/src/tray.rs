//! Tray icon and main-thread event loop (plan Task 13, spec §5.1).
//!
//! The menu is exactly the five items of the spec. Menu clicks are forwarded
//! into the tao loop as user events via tray-icon's event handler (the loop
//! has no other window of its own), and the timer thread refreshes the
//! tooltip every 60 s through the same user-event channel.

use std::sync::Arc;

use tao::event::Event;
use tao::event_loop::{ControlFlow, EventLoop};
use time::OffsetDateTime;
use time::macros::format_description;
use tray_icon::menu::{Menu, MenuEvent, MenuItem};
use tray_icon::{Icon, TrayIconBuilder};

/// User events delivered on the loop: menu clicks forwarded from tray-icon's
/// handler, and tooltip refreshes sent by the timer thread.
pub enum TraySignal {
    Menu(MenuEvent),
    Tooltip(String),
}

/// Menu item ids (stable strings, matched on incoming [`MenuEvent`]s).
pub const ID_OPEN_DASHBOARD: &str = "pcw-open-dashboard";
pub const ID_PAUSE_RESUME: &str = "pcw-pause-resume";
pub const ID_EXPORT_NOW: &str = "pcw-export-now";
pub const ID_OPEN_DATA_FOLDER: &str = "pcw-open-data-folder";
pub const ID_EXIT: &str = "pcw-exit";

const PAUSE_LABEL: &str = "Pause";
const RESUME_LABEL: &str = "Resume";
const DAY_MS: u64 = 86_400_000;
/// `export overdue` warns strictly after this long without an export;
/// `never` (no export yet) counts as overdue.
const OVERDUE_MS: u64 = 8 * DAY_MS;

/// The five menu actions of spec §5.1, provided by main.rs where the data
/// they need lives.
pub struct MenuHandlers {
    pub open_dashboard: Arc<dyn Fn() + Send + Sync>,
    /// Toggles the paused flag, sends `Paused`/`Unpaused` to the tracker and
    /// returns the new paused state (the tray flips the item label).
    pub toggle_pause: Arc<dyn Fn() -> bool + Send + Sync>,
    pub export_now: Arc<dyn Fn() + Send + Sync>,
    pub open_data_folder: Arc<dyn Fn() + Send + Sync>,
    /// Graceful shutdown: stops and joins every thread. Run once, then the
    /// loop exits.
    pub quit: Box<dyn FnOnce() + Send>,
}

/// Builds the tray icon with the five-item menu and runs the tao loop on the
/// calling thread (must be the main thread). Never returns.
///
/// # Panics
/// When the menu or tray icon cannot be built — there is no meaningful
/// agent without the tray.
#[allow(clippy::needless_pass_by_value)] // consumes the loop (tao `run`)
pub fn run_main_loop(
    event_loop: EventLoop<TraySignal>,
    handlers: MenuHandlers,
    initial_tooltip: &str,
) -> ! {
    // The agent runs elevated: allow the (usually filtered) TaskbarCreated
    // message so the icon re-registers when Explorer restarts (spec §5.1).
    // tray-icon already re-adds on that message internally.
    pcw_foreground::allow_taskbar_created_message();

    MenuEvent::set_event_handler(Some({
        let proxy = event_loop.create_proxy();
        move |event: MenuEvent| {
            let _ = proxy.send_event(TraySignal::Menu(event));
        }
    }));

    let item_open = MenuItem::with_id(ID_OPEN_DASHBOARD, "Open Dashboard", true, None);
    let item_pause = MenuItem::with_id(ID_PAUSE_RESUME, PAUSE_LABEL, true, None);
    let item_export = MenuItem::with_id(ID_EXPORT_NOW, "Export now", true, None);
    let item_folder = MenuItem::with_id(ID_OPEN_DATA_FOLDER, "Open data folder", true, None);
    let item_exit = MenuItem::with_id(ID_EXIT, "Exit", true, None);
    let menu = Menu::new();
    for item in [
        &item_open,
        &item_pause,
        &item_export,
        &item_folder,
        &item_exit,
    ] {
        menu.append(item).expect("append tray menu item");
    }

    let tray = TrayIconBuilder::new()
        .with_menu(Box::new(menu))
        .with_tooltip(initial_tooltip)
        .with_icon(tray_icon_image())
        .build()
        .expect("tray icon");

    let MenuHandlers {
        open_dashboard,
        toggle_pause,
        export_now,
        open_data_folder,
        quit,
    } = handlers;
    let mut quit = Some(quit);

    event_loop.run(move |event, _, control_flow| {
        *control_flow = ControlFlow::Wait;
        match event {
            Event::UserEvent(TraySignal::Tooltip(text)) => {
                let _ = tray.set_tooltip(Some(text.as_str()));
            }
            Event::UserEvent(TraySignal::Menu(click)) => match click.id().as_ref() {
                ID_OPEN_DASHBOARD => open_dashboard(),
                ID_PAUSE_RESUME => {
                    let paused = toggle_pause();
                    item_pause.set_text(if paused { RESUME_LABEL } else { PAUSE_LABEL });
                }
                ID_EXPORT_NOW => export_now(),
                ID_OPEN_DATA_FOLDER => open_data_folder(),
                ID_EXIT => {
                    if let Some(shutdown) = quit.take() {
                        shutdown();
                    }
                    *control_flow = ControlFlow::Exit;
                }
                _ => {}
            },
            _ => {}
        }
    });
}

/// `PC-Watcher — collecting|paused · last export <date|never>` plus the
/// warnings: `· export overdue` (> 8 days, or no export yet) and
/// `· low disk`.
#[must_use]
pub fn tooltip_text(
    paused: bool,
    last_export_ms: Option<u64>,
    now_ms: u64,
    low_disk: bool,
) -> String {
    let status = if paused { "paused" } else { "collecting" };
    let last = match last_export_ms {
        Some(ms) => local_date(ms),
        None => "never".to_string(),
    };
    let mut text = format!("PC-Watcher — {status} · last export {last}");
    if last_export_ms.is_none_or(|ms| now_ms.saturating_sub(ms) > OVERDUE_MS) {
        text.push_str(" · export overdue");
    }
    if low_disk {
        text.push_str(" · low disk");
    }
    text
}

/// `ms` as `YYYY-MM-DD` in local time (UTC when the offset is undeterminable;
/// epoch on implausible values, as in the export module).
fn local_date(ts_ms: u64) -> String {
    let fmt = format_description!("[year]-[month]-[day]");
    let nanos = i128::from(i64::try_from(ts_ms).map_or(0i64, |ms| ms.saturating_mul(1_000_000)));
    let time =
        OffsetDateTime::from_unix_timestamp_nanos(nanos).unwrap_or(OffsetDateTime::UNIX_EPOCH);
    let offset = time::UtcOffset::current_local_offset().unwrap_or(time::UtcOffset::UTC);
    time.to_offset(offset).format(&fmt).unwrap_or_default()
}

/// A 16×16 blue disc on a transparent buffer: a real (blank-default-free)
/// icon built from static data, since the repo carries no icon asset.
fn tray_icon_image() -> Icon {
    const PIXELS: u32 = 16;
    let count = usize::try_from(PIXELS * PIXELS).map_or(0, usize::from);
    let mut rgba = vec![0u8; count * 4];
    let center = (PIXELS as f32 - 1.0) / 2.0;
    let radius = PIXELS as f32 * 0.42;
    for (index, slot) in rgba.chunks_exact_mut(4).enumerate() {
        let x = (index as u32 % PIXELS) as f32;
        let y = (index as u32 / PIXELS) as f32;
        if (x - center).hypot(y - center) <= radius {
            slot[0] = 25;
            slot[1] = 89;
            slot[2] = 200;
            slot[3] = 255;
        }
    }
    Icon::from_rgba(rgba, PIXELS, PIXELS).expect("valid static RGBA icon")
}
