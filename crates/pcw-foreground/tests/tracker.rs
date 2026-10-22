//! Spec §4.3: foreground session state machine (pure logic, no Win32).

use pcw_foreground::{LOCK_APP, OpenSession, SessionSignal, SessionTracker, TrackerOutput};
use pcw_types::{AppSession, WindowTitle};

fn fg(ts: u64, app: &str, title: &str) -> SessionSignal {
    SessionSignal::Foreground {
        ts_ms: ts,
        process_name: app.to_string(),
        window_title: title.to_string(),
    }
}

fn ts(ts: u64, app: &str, title: &str) -> SessionSignal {
    SessionSignal::Title {
        ts_ms: ts,
        process_name: app.to_string(),
        window_title: title.to_string(),
    }
}

fn locked(ts: u64) -> SessionSignal {
    SessionSignal::Locked { ts_ms: ts }
}

fn unlocked(ts: u64) -> SessionSignal {
    SessionSignal::Unlocked { ts_ms: ts }
}

fn suspended(ts: u64) -> SessionSignal {
    SessionSignal::Suspended { ts_ms: ts }
}

fn resumed(ts: u64) -> SessionSignal {
    SessionSignal::Resumed { ts_ms: ts }
}

fn display_off(ts: u64) -> SessionSignal {
    SessionSignal::DisplayOff { ts_ms: ts }
}

fn display_on(ts: u64) -> SessionSignal {
    SessionSignal::DisplayOn { ts_ms: ts }
}

fn paused(ts: u64) -> SessionSignal {
    SessionSignal::Paused { ts_ms: ts }
}

fn unpaused(ts: u64) -> SessionSignal {
    SessionSignal::Unpaused { ts_ms: ts }
}

fn sessions(outputs: Vec<TrackerOutput>) -> Vec<AppSession> {
    outputs
        .into_iter()
        .filter_map(|o| match o {
            TrackerOutput::Session(s) => Some(s),
            TrackerOutput::Title(_) => None,
        })
        .collect()
}

fn titles(outputs: Vec<TrackerOutput>) -> Vec<WindowTitle> {
    outputs
        .into_iter()
        .filter_map(|o| match o {
            TrackerOutput::Title(t) => Some(t),
            TrackerOutput::Session(_) => None,
        })
        .collect()
}

fn session(ts_start: u64, ts_end: u64, app: &str, title: &str) -> AppSession {
    AppSession {
        ts_start_ms: ts_start,
        ts_end_ms: ts_end,
        process_name: app.to_string(),
        window_title: title.to_string(),
    }
}

fn title(ts: u64, app: &str, title: &str) -> WindowTitle {
    WindowTitle {
        ts_ms: ts,
        process_name: app.to_string(),
        title: title.to_string(),
    }
}

fn open(ts: u64, app: &str, title: &str) -> Option<OpenSession> {
    Some(OpenSession {
        ts_start_ms: ts,
        process_name: app.to_string(),
        window_title: title.to_string(),
    })
}

#[test]
fn app_switch_ends_and_starts() {
    let mut tr = SessionTracker::new(true);
    let mut out = tr.on_signal(fg(0, "a.exe", ""));
    out.extend(tr.on_signal(fg(100, "b.exe", "")));
    assert_eq!(sessions(out), vec![session(0, 100, "a.exe", "")]);
    assert_eq!(tr.open_session(), open(100, "b.exe", ""));
}

#[test]
fn lock_closes_and_idle_does_not() {
    let mut tr = SessionTracker::new(true);
    let mut out = tr.on_signal(fg(0, "a.exe", ""));
    out.extend(tr.on_signal(locked(100)));
    // Lock ended the open session...
    assert_eq!(sessions(out.clone()), vec![session(0, 100, "a.exe", "")]);
    assert_eq!(tr.open_session(), None, "no session while locked");
    // ...and an hour of idle does NOT end the reopened one: finalize does.
    out.extend(tr.on_signal(unlocked(200)));
    assert_eq!(tr.open_session(), open(200, "a.exe", ""));
    out.extend(tr.finalize(3_600_200));
    assert_eq!(
        sessions(out),
        vec![
            session(0, 100, "a.exe", ""),
            session(200, 3_600_200, "a.exe", "")
        ]
    );
    assert_eq!(tr.open_session(), None);
}

#[test]
fn sleep_display_off_and_pause_each_close() {
    // Suspended closes it (and Resumed reopens at its ts).
    {
        let mut tr = SessionTracker::new(true);
        let mut out = tr.on_signal(fg(0, "a.exe", ""));
        out.extend(tr.on_signal(suspended(100)));
        assert_eq!(sessions(out), vec![session(0, 100, "a.exe", "")]);
        assert_eq!(tr.open_session(), None);
        tr.on_signal(resumed(200));
        assert_eq!(tr.open_session(), open(200, "a.exe", ""));
    }

    // DisplayOff closes it (and DisplayOn reopens).
    {
        let mut tr = SessionTracker::new(true);
        let mut out = tr.on_signal(fg(0, "a.exe", ""));
        out.extend(tr.on_signal(display_off(100)));
        assert_eq!(sessions(out), vec![session(0, 100, "a.exe", "")]);
        assert_eq!(tr.open_session(), None);
        tr.on_signal(display_on(200));
        assert_eq!(tr.open_session(), open(200, "a.exe", ""));
    }

    // Paused closes it (and Unpaused reopens).
    {
        let mut tr = SessionTracker::new(true);
        let mut out = tr.on_signal(fg(0, "a.exe", ""));
        out.extend(tr.on_signal(paused(100)));
        assert_eq!(sessions(out), vec![session(0, 100, "a.exe", "")]);
        assert_eq!(tr.open_session(), None);
        tr.on_signal(unpaused(200));
        assert_eq!(tr.open_session(), open(200, "a.exe", ""));
    }
}

#[test]
fn waking_or_display_on_while_locked_opens_nothing() {
    let mut tr = SessionTracker::new(true);
    let mut out = tr.on_signal(fg(0, "a.exe", ""));
    out.extend(tr.on_signal(locked(100)));
    out.extend(tr.on_signal(suspended(200)));
    out.extend(tr.on_signal(resumed(300)));
    out.extend(tr.on_signal(display_on(400)));
    // Resumed and DisplayOn while still Locked open nothing (flags are a set,
    // not a counter of set/clear pairs), so nothing can close either.
    assert_eq!(tr.open_session(), None, "no session while still Locked");
    assert_eq!(sessions(out.clone()), vec![session(0, 100, "a.exe", "")]);
    // Only Unlocked@500 finally reopens.
    out.extend(tr.on_signal(unlocked(500)));
    assert_eq!(tr.open_session(), open(500, "a.exe", ""));
    out.extend(tr.finalize(600));
    assert_eq!(
        sessions(out),
        vec![session(0, 100, "a.exe", ""), session(500, 600, "a.exe", "")]
    );
}

#[test]
fn lock_app_never_gets_a_session() {
    let mut tr = SessionTracker::new(true);
    let mut out = tr.on_signal(fg(0, "a.exe", ""));
    out.extend(tr.on_signal(locked(50)));
    out.extend(tr.on_signal(fg(60, LOCK_APP, "")));
    out.extend(tr.on_signal(unlocked(90)));
    // LockApp is the current foreground, so Unlocked reopens nothing...
    assert_eq!(tr.open_session(), None);
    // ...and it produced no session of its own: only a's [0,50] exists.
    assert_eq!(sessions(out.clone()), vec![session(0, 50, "a.exe", "")]);
    // Switching back to a.exe at 95 opens a's session at 95.
    out.extend(tr.on_signal(fg(95, "a.exe", "")));
    assert_eq!(tr.open_session(), open(95, "a.exe", ""));
    out.extend(tr.finalize(200));
    let all = sessions(out);
    assert_eq!(
        all,
        vec![session(0, 50, "a.exe", ""), session(95, 200, "a.exe", "")]
    );
    assert!(
        all.iter().all(|s| s.process_name != LOCK_APP),
        "LockApp must never get a session"
    );
}

#[test]
fn title_changes_logged_after_2s_and_deduped() {
    let mut tr = SessionTracker::new(true);
    let mut out = tr.on_signal(fg(0, "a.exe", "T1"));
    out.extend(tr.on_signal(ts(1000, "a.exe", "T2")));
    out.extend(tr.on_signal(ts(5000, "a.exe", "T3")));
    // T1 lasted 1 s → not logged. T2 lasted 4 s → logged @1000.
    assert_eq!(
        titles(std::mem::take(&mut out)),
        vec![title(1000, "a.exe", "T2")]
    );
    // Identical title does not replace the pending entry: it stays anchored at 5000.
    out.extend(tr.on_signal(ts(6000, "a.exe", "T3")));
    assert!(titles(std::mem::take(&mut out)).is_empty());
    out.extend(tr.finalize(9000));
    // T3 was shown from 5000 through finalize@9000 → logged @5000, deduped once.
    assert_eq!(titles(out), vec![title(5000, "a.exe", "T3")]);
}

#[test]
fn same_process_foreground_change_is_title_not_session() {
    let mut tr = SessionTracker::new(true);
    let mut out = tr.on_signal(fg(0, "a.exe", ""));
    out.extend(tr.on_signal(fg(3000, "a.exe", "other")));
    out.extend(tr.finalize(9000));
    // The session keeps the title it OPENED with, not the latest one (spec
    // §4.3: "app_sessions.window_title is the title when the session opened").
    assert_eq!(sessions(out.clone()), vec![session(0, 9000, "a.exe", "")]);
    // The later Foreground was still a title change: logged at 3000.
    assert_eq!(titles(out), vec![title(3000, "a.exe", "other")]);
    assert_eq!(tr.open_session(), None);
}

#[test]
fn titles_disabled_logs_nothing() {
    let mut tr = SessionTracker::new(false);
    let mut out = tr.on_signal(fg(0, "a.exe", "T1"));
    out.extend(tr.on_signal(ts(1000, "a.exe", "T2")));
    out.extend(tr.on_signal(ts(5000, "a.exe", "T3")));
    out.extend(tr.on_signal(ts(6000, "a.exe", "T3")));
    out.extend(tr.on_signal(fg(7000, "b.exe", "T4")));
    out.extend(tr.finalize(9000));
    // Sessions are still produced, and they store EMPTY titles (spec §4.3:
    // "capture_window_titles = false stores empty titles and no title log")...
    assert_eq!(
        sessions(out.clone()),
        vec![
            session(0, 7000, "a.exe", ""),
            session(7000, 9000, "b.exe", "")
        ]
    );
    // ...but no Title outputs at all.
    assert_eq!(titles(out), vec![]);
}

#[test]
fn finalize_closes_open_session_at_given_ts() {
    let mut tr = SessionTracker::new(true);
    let mut out = tr.on_signal(fg(10, "a.exe", ""));
    out.extend(tr.finalize(70));
    assert_eq!(sessions(out), vec![session(10, 70, "a.exe", "")]);
    assert_eq!(tr.open_session(), None);
}

// Finding 3: dedupe key is (process_name, title), not a global title.

#[test]
fn same_title_in_a_different_process_is_logged() {
    let mut tr = SessionTracker::new(true);
    let mut out = tr.on_signal(fg(0, "a.exe", "Doc"));
    // App B shows the identical title; B's title IS logged.
    out.extend(tr.on_signal(fg(3000, "b.exe", "Doc")));
    out.extend(tr.finalize(7000));
    assert_eq!(
        titles(out.clone()),
        vec![title(0, "a.exe", "Doc"), title(3000, "b.exe", "Doc")]
    );
    assert_eq!(
        sessions(out),
        vec![
            session(0, 3000, "a.exe", "Doc"),
            session(3000, 7000, "b.exe", "Doc")
        ]
    );
}

#[test]
fn identical_title_after_lock_cycle_not_relogged() {
    let mut tr = SessionTracker::new(true);
    let mut out = tr.on_signal(fg(0, "a.exe", "Doc"));
    // Closing at 3000 flushes the pending title (dwell 3 s).
    out.extend(tr.on_signal(locked(3000)));
    out.extend(tr.on_signal(unlocked(4000)));
    out.extend(tr.finalize(6000));
    // The reopened session shows the same title for the same process:
    // logged once, even across the lock cycle.
    assert_eq!(titles(out.clone()), vec![title(0, "a.exe", "Doc")]);
    assert_eq!(
        sessions(out),
        vec![
            session(0, 3000, "a.exe", "Doc"),
            session(4000, 6000, "a.exe", "Doc")
        ]
    );
}

// Finding 4: a same-process Foreground while active reopens a session after
// finalize left the tracker session-less.

#[test]
fn session_reopens_after_finalize_on_foreground() {
    let mut tr = SessionTracker::new(true);
    let mut out = tr.on_signal(fg(0, "a.exe", "T"));
    out.extend(tr.finalize(1000));
    assert_eq!(sessions(out.clone()), vec![session(0, 1000, "a.exe", "T")]);
    // Still active, still a.exe in the foreground: Foreground reopens.
    out.clear();
    out.extend(tr.on_signal(fg(3000, "a.exe", "T")));
    assert_eq!(
        sessions(std::mem::take(&mut out)),
        vec![],
        "no session closes on reopen"
    );
    assert_eq!(tr.open_session(), open(3000, "a.exe", "T"));
    out.extend(tr.finalize(5000));
    assert_eq!(sessions(out), vec![session(3000, 5000, "a.exe", "T")]);
}

// Finding 6: edge tests.

#[test]
fn title_dwell_exactly_min_ms_is_logged() {
    let mut tr = SessionTracker::new(true);
    let mut out = tr.on_signal(fg(0, "a.exe", "T1"));
    out.extend(tr.on_signal(ts(1000, "a.exe", "T2")));
    out.extend(tr.finalize(3000));
    // T2's dwell is exactly TITLE_MIN_MS (3000 - 1000 = 2000) → counted.
    assert_eq!(titles(out.clone()), vec![title(1000, "a.exe", "T2")]);
    assert_eq!(sessions(out), vec![session(0, 3000, "a.exe", "T1")]);
}

#[test]
fn foreground_to_lock_app_closes_session_and_opens_nothing() {
    let mut tr = SessionTracker::new(true);
    let mut out = tr.on_signal(fg(0, "a.exe", "T1"));
    out.extend(tr.on_signal(ts(1000, "a.exe", "T2")));
    out.extend(tr.on_signal(fg(3000, LOCK_APP, "lock screen")));
    assert_eq!(tr.open_session(), None, "LockApp opens nothing");
    // The open session closed at 3000 (with its opening title), and the
    // pending T2 (dwell exactly 2 s) was flushed.
    assert_eq!(sessions(out.clone()), vec![session(0, 3000, "a.exe", "T1")]);
    assert_eq!(titles(out), vec![title(1000, "a.exe", "T2")]);
}

#[test]
fn title_for_other_process_is_ignored() {
    let mut tr = SessionTracker::new(true);
    let mut out = tr.on_signal(fg(0, "a.exe", "T1"));
    out.extend(tr.on_signal(fg(500, "b.exe", "T2")));
    // A title that names a different process than the tracked foreground is
    // unexpected input: ignored, no corrupt rows.
    out.extend(tr.on_signal(ts(1000, "a.exe", "T3")));
    out.extend(tr.finalize(6000));
    assert_eq!(titles(out.clone()), vec![title(500, "b.exe", "T2")]);
    assert_eq!(
        sessions(out),
        vec![
            session(0, 500, "a.exe", "T1"),
            session(500, 6000, "b.exe", "T2")
        ],
        "stray title must not corrupt the open session"
    );
}

#[test]
fn double_finalize_does_not_duplicate_session() {
    let mut tr = SessionTracker::new(true);
    let mut out = tr.on_signal(fg(0, "a.exe", "T"));
    out.extend(tr.finalize(1000));
    out.extend(tr.finalize(2000));
    assert_eq!(sessions(out), vec![session(0, 1000, "a.exe", "T")]);
    assert_eq!(tr.open_session(), None);
}

#[test]
fn locked_then_unlocked_uses_current_foreground_rules() {
    let mut tr = SessionTracker::new(true);
    let mut out = tr.on_signal(locked(0));
    // Locked arrives before any foreground: tracked, but opens nothing.
    out.extend(tr.on_signal(fg(100, "a.exe", "Doc")));
    assert_eq!(tr.open_session(), None);
    out.extend(tr.on_signal(unlocked(200)));
    // Becoming active opens a session for the (now) current foreground.
    assert_eq!(tr.open_session(), open(200, "a.exe", "Doc"));
    // Dwell since 200; finalize exactly TITLE_MIN_MS later logs the title.
    out.extend(tr.finalize(2200));
    assert_eq!(
        sessions(out.clone()),
        vec![session(200, 2200, "a.exe", "Doc")]
    );
    assert_eq!(titles(out), vec![title(200, "a.exe", "Doc")]);
}
