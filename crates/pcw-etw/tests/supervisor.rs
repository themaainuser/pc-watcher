use pcw_etw::source::SourceError;
use pcw_etw::supervisor::Supervisor;
use std::time::Duration;

fn err() -> SourceError {
    SourceError::Trace("test".to_string())
}

#[test]
fn restarts_with_doubling_backoff_capped_at_60s() {
    let mut s = Supervisor::new();
    let mut waits: Vec<u64> = Vec::new();
    for _ in 0..5 {
        waits.push(s.on_exit(Err(err()), Duration::from_secs(1)).as_secs());
    }
    assert_eq!(waits, vec![1, 2, 4, 8, 16]);
    for _ in 0..10 {
        waits.push(s.on_exit(Err(err()), Duration::from_secs(1)).as_secs());
    }
    assert_eq!(waits[5..], [32, 60, 60, 60, 60, 60, 60, 60, 60, 60]);
    assert_eq!(s.restarts, 15);
}

#[test]
fn backoff_resets_after_a_long_run() {
    let mut s = Supervisor::new();
    for _ in 0..3 {
        s.on_exit(Err(err()), Duration::from_secs(1));
    }
    // The next wait would have been 8 s.
    assert_eq!(
        s.on_exit(Err(err()), Duration::from_secs(360)),
        Duration::from_secs(1)
    );
    // Doubling resumes from the reset value.
    assert_eq!(
        s.on_exit(Err(err()), Duration::from_secs(1)),
        Duration::from_secs(2)
    );
}

#[test]
fn clean_exit_still_restarts() {
    let mut s = Supervisor::new();
    assert_eq!(s.restarts, 0);
    let wait = s.on_exit(Ok(()), Duration::from_secs(1));
    assert_eq!(wait, Duration::from_secs(1));
    assert_eq!(s.restarts, 1);
}

#[test]
fn loss_counts_increase_only() {
    let mut s = Supervisor::new();
    s.on_stats(5, 1);
    assert_eq!(s.events_lost, 5);
    s.on_stats(8, 1);
    assert_eq!(s.events_lost, 8);
    assert_eq!(s.buffers_lost, 1);
    // Session ends; per-session counters restart at zero.
    let _ = s.on_exit(Ok(()), Duration::from_secs(60));
    s.on_stats(2, 0);
    assert_eq!(s.events_lost, 10);
    assert_eq!(s.buffers_lost, 1);
}
