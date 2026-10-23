// pcw-agent binary: `--install`/`--uninstall` CLI, otherwise the collector
// agent itself — the thread topology and tray wiring of plan Task 13.

#![windows_subsystem = "windows"]

mod tray;

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use pcw_agent::state;
use pcw_agent::timers::due;
use pcw_agent::writer::{WriterConfig, WriterMsg, run_writer};
use pcw_etw::source::{SourceConfig, query_loss, run_session, stop_session};
use pcw_etw::supervisor::Supervisor;
use pcw_etw::{NetEvent, SESSION_NAME};
use pcw_foreground::{
    HookHandle, SessionSignal, SessionTracker, TrackerOutput, attach_parent_console,
    free_disk_bytes,
};
use pcw_storage::{SharedStore, Store, WriteOp};
use pcw_types::{Counters, ProcessCache, SharedProcessCache};
use tao::event_loop::{EventLoop, EventLoopBuilder, EventLoopProxy};

/// All period constants in one place (spec/plan Task 13).
const HEARTBEAT: Duration = Duration::from_secs(60);
const TIMER_TICK: Duration = Duration::from_secs(60);
/// Sleep granularity of all polling loops (fast exit response).
const TICK_SLICE: Duration = Duration::from_millis(200);
const HOUR_MS: u64 = 3_600_000;
const SEVEN_DAYS_MS: u64 = 7 * 86_400_000;
/// Chunked prune (plan: `prune_step(retention_days, now, 50_000)`).
const PRUNE_MAX_ROWS: u32 = 50_000;
const PRUNE_PERIOD_MS: u64 = 3_600_000;
const BYTES_PER_GB: u64 = 1_073_741_824;
/// Marker for the export retry rule (retry hourly after a failed attempt).
const META_LAST_EXPORT_FAILURE: &str = "last_export_failure";
/// agent.log rotation: 5 MB, one old copy (spec §5).
const LOG_MAX_BYTES: u64 = 5 * 1024 * 1024;

fn main() {
    match std::env::args().nth(1).as_deref() {
        Some("--install") => report_cli(pcw_agent::install::install(), true),
        Some("--uninstall") => report_cli(pcw_agent::install::uninstall(), false),
        _ => run_agent(),
    }
}

/// `--install`/`--uninstall` CLI: attach to the calling console, print the
/// result, exit 0 on success / 1 on failure.
fn report_cli(result: Result<(), String>, installing: bool) {
    attach_parent_console();
    match result {
        Ok(()) => println!(
            "PC-Watcher task {}.",
            if installing {
                "installed"
            } else {
                "uninstalled"
            }
        ),
        Err(e) => {
            println!("error: {e}");
            std::process::exit(1);
        }
    }
}

/// Poison-tolerant lock helper for the store and the shared Supervisor: a
/// panic anywhere must not wedge the still-running agent.
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    match mutex.lock() {
        Ok(guard) => guard,
        Err(poisoned) => {
            log::error!("a lock was poisoned; recovering from it anyway");
            PoisonError::into_inner(poisoned)
        }
    }
}

/// Wall-clock epoch milliseconds (0 only before the epoch; timestamps are
/// epoch-ms per spec §4 and review flag F12).
fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Reads `meta[key]` as ms; a parse failure is the absent case.
fn meta_u64(store: &SharedStore, key: &str) -> Option<u64> {
    lock(store)
        .get_meta(key)
        .ok()
        .flatten()
        .and_then(|raw| raw.parse::<u64>().ok())
}

fn run_agent() {
    // Diagnostics when started from a terminal; a scheduled start has no
    // console and the attach silently does nothing (spec §5).
    attach_parent_console();

    // Data dir and log file first: logfile::init does not create the parent
    // dir, and config load both needs and creates the dir (review flag F15).
    let Ok(data_dir) = pcw_config::default_data_dir() else {
        eprintln!(
            "PC-Watcher: cannot determine the data directory (PCW_DATA_DIR/LOCALAPPDATA unset)"
        );
        std::process::exit(1);
    };
    if let Err(e) = std::fs::create_dir_all(&data_dir) {
        eprintln!("PC-Watcher: cannot create {}: {e}", data_dir.display());
        std::process::exit(1);
    }
    pcw_agent::logfile::init(&data_dir.join("agent.log"), LOG_MAX_BYTES);
    log::info!("agent starting, data dir {}", data_dir.display());

    let cfg = match pcw_config::Config::load_or_create(&data_dir) {
        Ok(cfg) => cfg,
        Err(e) => {
            log::error!("cannot load config: {e}");
            std::process::exit(1);
        }
    };
    let store: SharedStore = match Store::open(&data_dir.join("data.db")) {
        Ok(store) => Arc::new(Mutex::new(store)),
        Err(e) => {
            log::error!("cannot open the database: {e}");
            std::process::exit(1);
        }
    };
    let counters = Arc::new(Counters::default());
    {
        let store_guard = lock(&store);
        if let Err(e) = state::load_counters(&store_guard, &counters) {
            log::error!("loading counters failed: {e}");
        }
    }
    // Cross-crash recovery BEFORE any thread starts (review flag F15).
    {
        let mut store_guard = lock(&store);
        match state::recover_open_session(&mut store_guard) {
            Ok(Some(session)) => log::info!(
                "recovered open session: {} since ms {}",
                session.process_name,
                session.ts_start_ms
            ),
            Ok(None) => {}
            Err(e) => log::error!("recovering the open session failed: {e}"),
        }
    }

    // One process cache shared by ETW and the foreground hooks, backed by the
    // single Win32 lookup in pcw-etw.
    let cache: SharedProcessCache = Arc::new(Mutex::new(ProcessCache::new(Box::new(
        pcw_etw::process::query_process,
    ))));

    let paused = Arc::new(AtomicBool::new(false));
    let exit_flag = Arc::new(AtomicBool::new(false));
    let supervisor = Arc::new(Mutex::new(Supervisor::new()));
    let export_running = Arc::new(AtomicBool::new(false));
    let db_path = data_dir.join("data.db");
    let min_free_bytes = u64::from(cfg.min_free_disk_gb).saturating_mul(BYTES_PER_GB);

    // ---- writer thread -------------------------------------------------
    let (tx_writer, rx_writer) = std::sync::mpsc::channel::<WriterMsg>();
    let writer_thread = {
        let store = Arc::clone(&store);
        let counters = Arc::clone(&counters);
        let probe_dir = data_dir.clone();
        thread::Builder::new()
            .name("pcw-writer".into())
            .spawn(move || {
                run_writer(
                    rx_writer,
                    store,
                    WriterConfig {
                        min_free_bytes,
                        free_bytes: Box::new(move || free_disk_bytes(&probe_dir)),
                        disk_check_every: HEARTBEAT,
                    },
                    counters,
                );
            })
            .expect("spawn writer thread")
    };

    // ---- tracker thread ------------------------------------------------
    let (tx_signals, rx_signals) = std::sync::mpsc::channel::<SessionSignal>();
    let tracker_thread = {
        let tx_writer = tx_writer.clone();
        thread::Builder::new()
            .name("pcw-tracker".into())
            .spawn(move || tracker_loop(rx_signals, tx_writer, cfg.capture_window_titles))
            .expect("spawn tracker thread")
    };

    // ---- net pump thread ------------------------------------------------
    // `run_session` blocks its own thread decoding ETW, so the last hop
    // (NetEvent → WriteOp) is its own worker; the session sink is dropped
    // inside run_session on every restart, which drains/parks this loop.
    let (tx_net, rx_net) = std::sync::mpsc::channel::<NetEvent>();
    let pump_thread = {
        let tx_writer = tx_writer.clone();
        thread::Builder::new()
            .name("pcw-net-pump".into())
            .spawn(move || {
                for event in rx_net {
                    let op = match event {
                        NetEvent::Connection(ev) => WriteOp::Connection(ev),
                        NetEvent::Dns(answer) => WriteOp::Dns(answer),
                    };
                    if tx_writer.send(WriterMsg::Op(op)).is_err() {
                        return;
                    }
                }
            })
            .expect("spawn net pump thread")
    };

    // ---- ETW thread ------------------------------------------------------
    let etw_thread = {
        let source_cfg = SourceConfig {
            capture_dns: cfg.capture_dns_names,
            udp_rate_cap: cfg.udp_rate_cap,
        };
        let cache = Arc::clone(&cache);
        let counters = Arc::clone(&counters);
        let exit_flag = Arc::clone(&exit_flag);
        let paused = Arc::clone(&paused);
        let supervisor = Arc::clone(&supervisor);
        thread::Builder::new()
            .name("pcw-etw".into())
            .spawn(move || {
                etw_loop(
                    source_cfg, tx_net, cache, counters, paused, supervisor, exit_flag,
                );
            })
            .expect("spawn etw thread")
    };

    // ---- hooks thread (spawned inside start_hooks) -----------------------
    let hooks = match pcw_foreground::start_hooks(
        tx_signals.clone(),
        cache.clone(),
        cfg.capture_window_titles,
    ) {
        Ok(handle) => Some(handle),
        Err(e) => {
            // Session/title tracking degrades; collection keeps running.
            log::error!("foreground hooks unavailable: {e}");
            None
        }
    };

    // ---- PTR worker (review flag F11) -------------------------------------
    let ptr_stop = Arc::new(AtomicBool::new(false));
    let ptr_thread = if cfg.resolve_dns {
        let store = Arc::clone(&store);
        let stop = Arc::clone(&ptr_stop);
        Some(
            thread::Builder::new()
                .name("pcw-resolve".into())
                .spawn(move || pcw_resolve::run_worker(store, stop, Duration::from_secs(30), 50))
                .expect("spawn resolve worker"),
        )
    } else {
        None
    };

    // ---- main-thread tray: the loop first; its proxy feeds the timer ------
    let event_loop: EventLoop<tray::TraySignal> =
        EventLoopBuilder::<tray::TraySignal>::with_user_event().build();
    let tray_proxy = event_loop.create_proxy();

    // ---- timer thread -----------------------------------------------------
    let timer_state = TimerState {
        store: Arc::clone(&store),
        counters: Arc::clone(&counters),
        supervisor: Arc::clone(&supervisor),
        exit_flag: Arc::clone(&exit_flag),
        paused: Arc::clone(&paused),
        proxy: tray_proxy.clone(),
        data_dir: data_dir.clone(),
        db_path: db_path.clone(),
        export_folder: cfg.export_folder.clone(),
        export_running: Arc::clone(&export_running),
        retention_days: cfg.retention_days,
        min_free_bytes,
    };
    let timer_thread = thread::Builder::new()
        .name("pcw-timer".into())
        .spawn(move || timer_loop(timer_state))
        .expect("spawn timer thread");

    // ---- menu handlers ----------------------------------------------------
    let last_export_initial = meta_u64(&store, "last_export");
    let low_disk_initial = free_disk_bytes(&data_dir).is_some_and(|free| free < min_free_bytes);
    let handlers = tray::MenuHandlers {
        open_dashboard: Arc::new({
            let exe_dir = exe_dir();
            move || {
                if let Err(e) = Command::new(exe_dir.join("pcw-dash.exe")).spawn() {
                    log::error!("starting pcw-dash.exe failed: {e}");
                }
            }
        }),
        toggle_pause: Arc::new({
            let paused = Arc::clone(&paused);
            let tx_signals = tx_signals.clone();
            move || {
                let now_paused = !paused.load(Ordering::Relaxed);
                paused.store(now_paused, Ordering::Relaxed);
                // Note (F14e): pausing before the first ETW snapshot has been
                // sent discards that one-shot snapshot — known/accepted.
                let signal = if now_paused {
                    SessionSignal::Paused { ts_ms: now_ms() }
                } else {
                    SessionSignal::Unpaused { ts_ms: now_ms() }
                };
                let _ = tx_signals.send(signal);
                log::info!("collection paused={now_paused}");
                now_paused
            }
        }),
        export_now: Arc::new({
            let db_path = db_path.clone();
            let store = Arc::clone(&store);
            let running = Arc::clone(&export_running);
            let folder = cfg.export_folder.clone();
            move || {
                if !running.swap(true, Ordering::Relaxed) {
                    spawn_export(
                        db_path.clone(),
                        Arc::clone(&store),
                        folder.clone(),
                        running.clone(),
                    );
                }
            }
        }),
        open_data_folder: Arc::new({
            let dir = data_dir.clone();
            move || {
                if let Err(e) = Command::new("explorer.exe").arg(&dir).spawn() {
                    log::error!("opening {} failed: {e}", dir.display());
                }
            }
        }),
        quit: Box::new({
            let store = Arc::clone(&store);
            let counters = Arc::clone(&counters);
            move || {
                let threads = AgentThreads {
                    exit_flag,
                    hooks,
                    tx_signals,
                    ptr_stop,
                    timer: timer_thread,
                    ptr: ptr_thread,
                    etw: etw_thread,
                    pump: pump_thread,
                    tracker: tracker_thread,
                    writer: writer_thread,
                };
                shutdown(threads, store, counters);
            }
        }),
    };

    tray::run_main_loop(
        event_loop,
        handlers,
        &tray::tooltip_text(false, last_export_initial, now_ms(), low_disk_initial),
    );
}

/// Directory of the running agent exe (the dashboard binary's home; an empty
/// path only leads to a logged spawn failure).
fn exe_dir() -> PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(Path::to_path_buf))
        .unwrap_or_default()
}

/// The ETW watchdog loop: stop leftovers, run one session to its end
/// (erroring or not), back off, repeat — until the process is exiting.
fn etw_loop(
    source_cfg: SourceConfig,
    sink: std::sync::mpsc::Sender<NetEvent>,
    cache: SharedProcessCache,
    counters: Arc<Counters>,
    paused: Arc<AtomicBool>,
    supervisor: Arc<Mutex<Supervisor>>,
    exit_flag: Arc<AtomicBool>,
) {
    loop {
        if exit_flag.load(Ordering::Relaxed) {
            return;
        }
        // Review flag F14(b): a leftover session from a crashed run makes
        // every start fail forever; stop BEFORE every attempt (stopping a
        // missing session is the normal no-op startup case).
        stop_session(SESSION_NAME);
        let started = Instant::now();
        // Note (review flag F14(a), brief line-15 deviation): run_session
        // already emits the startup tcp_snapshot internally, so NO
        // tcp_snapshot_events are sent from this loop — sending them twice
        // would double-record every already-open connection.
        let result = run_session(
            &source_cfg,
            cache.clone(),
            counters.clone(),
            paused.clone(),
            sink.clone(),
        );
        let wait = {
            let mut supervisor = lock(&supervisor);
            supervisor.on_exit(result, started.elapsed())
        };
        sleep_in_slices(wait, &exit_flag);
    }
}

/// Sleeps at most `total`, in small slices, cut short by the exit flag.
fn sleep_in_slices(total: Duration, exit_flag: &AtomicBool) {
    let mut remaining = total;
    while remaining > Duration::ZERO && !exit_flag.load(Ordering::Relaxed) {
        let slice = remaining.min(TICK_SLICE);
        std::thread::sleep(slice);
        remaining = remaining.saturating_sub(slice);
    }
}

/// Owns the tracker; receives Win32 session signals and the heartbeat.
fn tracker_loop(
    rx: std::sync::mpsc::Receiver<SessionSignal>,
    tx_writer: std::sync::mpsc::Sender<WriterMsg>,
    capture_titles: bool,
) {
    let mut tracker = SessionTracker::new(capture_titles);
    let mut last_heartbeat = Instant::now();
    loop {
        match rx.recv_timeout(HEARTBEAT) {
            Ok(signal) => {
                for out in tracker.on_signal(signal) {
                    if let Err(e) = tx_writer.send(tracker_op(out)) {
                        log::error!("writer channel closed: {e}");
                        return;
                    }
                }
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
            // All signal sources gone (hook thread stopped, main dropped its
            // sender): finalize ONCE (review flag F12), then the final
            // heartbeat below clears meta.open_session.
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                for out in tracker.finalize(now_ms()) {
                    let _ = tx_writer.send(tracker_op(out));
                }
                break;
            }
        }
        if last_heartbeat.elapsed() >= HEARTBEAT {
            last_heartbeat = Instant::now();
            let _ = tx_writer.send(WriterMsg::OpenSession {
                session: tracker.open_session(),
                now_ms: now_ms(),
            });
        }
    }
    // Clean shutdown complete: `finalize` closed the session above, so this
    // heartbeat is `None` and clears meta.open_session (no stale crash
    // marker); then the writer flushes its queue and exits.
    let _ = tx_writer.send(WriterMsg::OpenSession {
        session: tracker.open_session(),
        now_ms: now_ms(),
    });
    let _ = tx_writer.send(WriterMsg::Shutdown);
}

/// Tracker outputs → the writer's messages.
fn tracker_op(out: TrackerOutput) -> WriterMsg {
    match out {
        TrackerOutput::Session(session) => WriterMsg::Op(WriteOp::Session(session)),
        TrackerOutput::Title(title) => WriterMsg::Op(WriteOp::Title(title)),
    }
}

/// Everything the 60 s timer loop needs (shared Arcs only — the loop polls
/// the exit flag; it holds no thread handles).
struct TimerState {
    store: SharedStore,
    counters: Arc<Counters>,
    supervisor: Arc<Mutex<Supervisor>>,
    exit_flag: Arc<AtomicBool>,
    paused: Arc<AtomicBool>,
    proxy: EventLoopProxy<tray::TraySignal>,
    data_dir: PathBuf,
    db_path: PathBuf,
    export_folder: PathBuf,
    export_running: Arc<AtomicBool>,
    retention_days: u32,
    min_free_bytes: u64,
}

/// Ticks immediately at start, then every 60 s while the agent runs.
fn timer_loop(state: TimerState) {
    // Last prune tick in ms: `None` makes the first tick due — each run
    // catches a backlog left by downtime; the watermark guard keeps the
    // sweep safe either way.
    let mut last_prune: Option<u64> = None;
    let mut next_tick = Instant::now();
    loop {
        if state.exit_flag.load(Ordering::Relaxed) {
            return;
        }
        if Instant::now() < next_tick {
            std::thread::sleep(TICK_SLICE);
            continue;
        }
        next_tick += TIMER_TICK;
        timer_tick(&state, &mut last_prune);
    }
}

/// One 60 s tick: loss query → supervisor → counters → persist → prune →
/// export schedule → tray tooltip.
fn timer_tick(state: &TimerState, last_prune: &mut Option<u64>) {
    let now = now_ms();

    // Review flag F14(d): query_loss only while the session is alive, and
    // only from this thread (run_session blocks its own).
    if let Some((events_lost, buffers_lost)) = query_loss(SESSION_NAME) {
        lock(&state.supervisor).on_stats(events_lost, buffers_lost);
    }
    // Review flag F14(c): Supervisor totals are CUMULATIVE — publish with
    // .set, never .add. `udp_capped` is added live by the ETW callbacks and
    // the two writer counters by the writer thread; they are left alone here.
    {
        let supervisor = lock(&state.supervisor);
        state
            .counters
            .set("etw_events_lost", supervisor.events_lost);
        state
            .counters
            .set("etw_buffers_lost", supervisor.buffers_lost);
        state.counters.set("watchdog_restarts", supervisor.restarts);
    }
    {
        let mut store = lock(&state.store);
        if let Err(e) = state::persist_counters(&mut store, &state.counters) {
            log::error!("persisting counters failed: {e}");
        }
    }

    // Hourly prune: loop until the chunker returns 0, releasing the store
    // lock between the transactions (brief line 15, step 6).
    if due(now, *last_prune, PRUNE_PERIOD_MS) {
        loop {
            let deleted = {
                let mut store = lock(&state.store);
                match store.prune_step(state.retention_days, now, PRUNE_MAX_ROWS) {
                    Ok(count) => count,
                    Err(e) => {
                        log::error!("prune failed: {e}");
                        break;
                    }
                }
            };
            if deleted == 0 {
                break;
            }
        }
        *last_prune = Some(now);
    }

    // Export schedule: absent `last_export` or ≥ 7 days, retried hourly after
    // a failed attempt (review flag F10: a failed run_export may still have
    // published the folder — the retry rule re-runs on its own).
    let last_export = meta_u64(&state.store, "last_export");
    let last_failure = meta_u64(&state.store, META_LAST_EXPORT_FAILURE);
    if due(now, last_export, SEVEN_DAYS_MS)
        && due(now, last_failure, HOUR_MS)
        && !state.export_running.swap(true, Ordering::Relaxed)
    {
        spawn_export(
            state.db_path.clone(),
            Arc::clone(&state.store),
            state.export_folder.clone(),
            Arc::clone(&state.export_running),
        );
    }

    // Tray status line.
    let paused_now = state.paused.load(Ordering::Relaxed);
    let low_disk = free_disk_bytes(&state.data_dir).is_some_and(|free| free < state.min_free_bytes);
    let text = tray::tooltip_text(paused_now, last_export, now, low_disk);
    let _ = state.proxy.send_event(tray::TraySignal::Tooltip(text));
}

/// Runs one export cycle on its own thread (it may stream a lot and must not
/// block the timer tick); `running` is the shared in-flight guard so the
/// scheduled run and "Export now" can never overlap.
fn spawn_export(
    db_path: PathBuf,
    store: SharedStore,
    export_folder: PathBuf,
    running: Arc<AtomicBool>,
) {
    let _ = thread::Builder::new()
        .name("pcw-export".into())
        .spawn(move || {
            let now = now_ms();
            match pcw_export::run_export(&db_path, &store, &export_folder, now) {
                Ok(report) => log::info!(
                    "export finished: {} ({} connections, {} sessions, {} titles)",
                    report.folder.display(),
                    report.connection_rows,
                    report.session_rows,
                    report.title_rows
                ),
                Err(e) => {
                    log::error!("export failed: {e}");
                    let mut store = lock(&store);
                    if let Err(e) = store.set_meta(META_LAST_EXPORT_FAILURE, &now.to_string()) {
                        log::error!("recording the export failure failed: {e}");
                    }
                }
            }
            running.store(false, Ordering::Relaxed);
        });
}

/// Everything a clean shutdown must own: the two stop flags, the shared
/// signal sender, and the join handles of every thread (owned by the Exit
/// menu action, which runs [`shutdown`]).
struct AgentThreads {
    exit_flag: Arc<AtomicBool>,
    hooks: Option<HookHandle>,
    tx_signals: std::sync::mpsc::Sender<SessionSignal>,
    ptr_stop: Arc<AtomicBool>,
    timer: JoinHandle<()>,
    ptr: Option<JoinHandle<()>>,
    etw: JoinHandle<()>,
    pump: JoinHandle<()>,
    tracker: JoinHandle<()>,
    writer: JoinHandle<()>,
}

/// The Exit handler (tray, brief ordering): stop the ETW session, stop the
/// hooks, wake the tracker (its finalize runs exactly once), stop the
/// workers, flush the writer, persist the counters, join everything.
fn shutdown(threads: AgentThreads, store: SharedStore, counters: Arc<Counters>) {
    let AgentThreads {
        exit_flag,
        hooks,
        tx_signals,
        ptr_stop,
        timer: timer_thread,
        ptr: ptr_thread,
        etw: etw_thread,
        pump: pump_thread,
        tracker: tracker_thread,
        writer: writer_thread,
    } = threads;
    log::info!("shutting down");
    exit_flag.store(true, Ordering::Relaxed);

    // Make run_session return so the ETW loop can wind down.
    stop_session(SESSION_NAME);
    if let Some(hooks) = hooks {
        // Joins the hook thread; its signal sender dies with it.
        hooks.stop();
    }
    // Drop the main thread's last sender: the tracker wakes on Disconnect,
    // finalizes once and posts its outputs + `OpenSession {None}` +
    // `WriterMsg::Shutdown` to the writer.
    drop(tx_signals);

    // Timer first (it polls the flag in 200 ms slices), then the PTR worker.
    let _ = timer_thread.join();
    ptr_stop.store(true, Ordering::Relaxed);
    if let Some(ptr_thread) = ptr_thread {
        let _ = ptr_thread.join();
    }
    // The ETW loop only breaks after run_session returns; its sink is dropped
    // with the session, which drains the pump before it is joined here.
    let _ = etw_thread.join();
    let _ = pump_thread.join();
    // Tracker sends the last `WriterMsg::OpenSession {None}` + `Shutdown`.
    let _ = tracker_thread.join();
    // Writer flushes its queue and exits after `Shutdown`.
    let _ = writer_thread.join();

    let mut store_guard = lock(&store);
    if let Err(e) = state::persist_counters(&mut store_guard, &counters) {
        log::error!("persisting counters at shutdown failed: {e}");
    }
    log::info!("shutdown complete");
}
