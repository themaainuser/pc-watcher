use std::collections::HashMap;
use std::sync::{Arc, Mutex};

pub fn file_name_of(path: &str) -> String {
    path.rsplit(['\\', '/']).next().unwrap_or(path).to_string()
}

pub type SharedProcessCache = Arc<Mutex<ProcessCache>>;

/// Cap per map (`fallback`, `cache`). Exceeding it clears the map wholesale:
/// every entry is re-derivable from ETW Kernel-Process events and the Win32
/// query, so a clear only costs re-resolution and bounds the memory of the
/// pid-keyed maps (same shape as the store's `SEEN_IP_CAP`).
const CACHE_CAP: usize = 10_000;

pub struct ProcessCache {
    query: Box<dyn FnMut(u32) -> Option<(String, String)> + Send>,
    /// pid -> (name, path) for successful lookups.
    cache: HashMap<u32, (String, String)>,
    /// pid -> image path seen in process events, used when the query fails.
    fallback: HashMap<u32, String>,
}

impl ProcessCache {
    pub fn new(query: Box<dyn FnMut(u32) -> Option<(String, String)> + Send>) -> Self {
        Self {
            query,
            cache: HashMap::new(),
            fallback: HashMap::new(),
        }
    }

    /// Returns (process name, process path). Failed lookups are never cached
    /// and fall back to the image path recorded via `note_process`.
    pub fn get(&mut self, pid: u32) -> (String, String) {
        if let Some((name, path)) = self.cache.get(&pid) {
            return (name.clone(), path.clone());
        }
        match (*self.query)(pid) {
            Some((name, path)) => {
                self.cache.insert(pid, (name.clone(), path.clone()));
                if self.cache.len() > CACHE_CAP {
                    self.cache.clear();
                }
                (name, path)
            }
            None => match self.fallback.get(&pid) {
                Some(image) => (file_name_of(image), image.clone()),
                None => ("unknown".to_string(), String::new()),
            },
        }
    }

    /// Record an image path observed in a process event. When `started` is
    /// true the cached entry is dropped so the next `get` re-queries (the pid
    /// may have been reused by a different process). Start and stop events
    /// both record the image path as the fallback.
    pub fn note_process(&mut self, pid: u32, image_path: &str, started: bool) {
        if started {
            self.cache.remove(&pid);
        }
        self.fallback.insert(pid, image_path.to_string());
        if self.fallback.len() > CACHE_CAP {
            self.fallback.clear();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_name_of_both_path_styles() {
        assert_eq!(file_name_of("C:\\a\\b.exe"), "b.exe");
        assert_eq!(file_name_of("\\Device\\HarddiskVolume3\\x\\y.exe"), "y.exe");
        assert_eq!(file_name_of("forward/slash/exe"), "exe");
    }

    #[test]
    fn note_process_stop_does_not_invalidate() {
        let mut c = ProcessCache::new(Box::new(|_| Some(("a.exe".into(), "C:\\a.exe".into()))));
        assert_eq!(c.get(1).0, "a.exe");
        c.note_process(1, "C:\\gone.exe", false);
        // Still cached: a stop event must not drop the entry.
        assert_eq!(c.get(1).0, "a.exe");
    }

    #[test]
    fn fallback_not_used_when_query_succeeds() {
        let mut c = ProcessCache::new(Box::new(|_| Some(("a.exe".into(), "C:\\a.exe".into()))));
        c.note_process(2, "C:\\old.exe", false);
        assert_eq!(c.get(2), ("a.exe".to_string(), "C:\\a.exe".to_string()));
    }

    /// More inserts than the cap leave both maps at or below `CACHE_CAP`:
    /// exceeding a map clears it wholesale (entries re-derive from the next
    /// ETW Kernel-Process events / Win32 query).
    #[test]
    fn exceeding_the_cap_clears_wholesale() {
        let mut c = ProcessCache::new(Box::new(|pid| {
            Some((format!("p{pid}.exe"), format!(r"C:\p{pid}.exe")))
        }));
        // Drives `fallback` past the cap through stop events and `cache`
        // past it through successful queries.
        for pid in 0..(CACHE_CAP as u32 + 100) {
            c.note_process(pid, &format!(r"C:\p{pid}.exe"), false);
            c.get(pid);
            assert!(c.fallback.len() <= CACHE_CAP);
            assert!(c.cache.len() <= CACHE_CAP);
        }
        // A cleared map still answers: new entries accumulate again.
        c.cache.clear();
        c.fallback.clear();
        assert_eq!(c.get(CACHE_CAP as u32).0, format!("p{}.exe", CACHE_CAP));
    }
}
