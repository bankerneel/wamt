//! Latest JSON payloads plus a condition variable that wakes SSE clients.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

const FEED_KEEP: usize = 400;

#[derive(Default)]
struct State {
    live: Arc<String>,
    live_v: u64,
    /// When the stored payloads were composed; an older one never replaces
    /// a newer one (two workers and a reconnecting client compose at once).
    live_at: i64,
    days: Arc<String>,
    days_v: u64,
    days_at: i64,
    feed: VecDeque<(u64, String, Arc<String>)>,
    feed_v: u64,
}

#[derive(Default)]
pub struct Hub {
    st: Mutex<State>,
    cv: Condvar,
    /// Open event streams. With none, the workers skip building the live
    /// and days JSON (they only mark it stale) and a new client rebuilds it.
    clients: AtomicUsize,
    stale: AtomicBool,
}

/// Counts one open event stream for as long as it lives.
pub struct ClientGuard<'a>(&'a Hub);

impl Drop for ClientGuard<'_> {
    fn drop(&mut self) {
        self.0.clients.fetch_sub(1, Ordering::Relaxed);
    }
}

/// Versions a client has already seen.
#[derive(Clone, Copy, Default, Debug)]
pub struct Seen {
    pub live: u64,
    pub days: u64,
    pub feed: u64,
}

pub struct Update {
    pub live: Option<Arc<String>>,
    pub days: Option<Arc<String>>,
    pub feed: Vec<Arc<String>>,
    pub seen: Seen,
}

impl Hub {
    pub fn new() -> Arc<Hub> {
        let h = Hub::default();
        {
            let mut s = h.st.lock().unwrap();
            s.live = Arc::new("{}".into());
            s.days = Arc::new("{}".into());
        }
        Arc::new(h)
    }

    pub fn client(&self) -> ClientGuard<'_> {
        self.clients.fetch_add(1, Ordering::Relaxed);
        ClientGuard(self)
    }

    /// True when no event stream is open: building payloads can wait.
    /// Marks them stale so the next reader rebuilds them first.
    pub fn idle(&self) -> bool {
        let idle = self.clients.load(Ordering::Relaxed) == 0;
        if idle {
            self.stale.store(true, Ordering::Relaxed);
        }
        idle
    }

    pub fn take_stale(&self) -> bool {
        self.stale.swap(false, Ordering::Relaxed)
    }

    pub fn set_live(&self, json: String) {
        self.set_live_at(json, i64::MAX);
    }

    /// Stores a live payload composed at `at`, unless a newer one is stored.
    pub fn set_live_at(&self, json: String, at: i64) {
        let mut s = self.st.lock().unwrap();
        if at != i64::MAX {
            if at < s.live_at {
                return;
            }
            s.live_at = at;
        }
        s.live = Arc::new(json);
        s.live_v += 1;
        self.cv.notify_all();
    }

    pub fn set_days(&self, json: String) {
        self.set_days_at(json, i64::MAX);
    }

    pub fn set_days_at(&self, json: String, at: i64) {
        let mut s = self.st.lock().unwrap();
        if at != i64::MAX {
            if at < s.days_at {
                return;
            }
            s.days_at = at;
        }
        s.days = Arc::new(json);
        s.days_v += 1;
        self.cv.notify_all();
    }

    /// Adds or replaces feed items by id.
    pub fn push_feed(&self, items: Vec<(String, String)>) {
        if items.is_empty() {
            return;
        }
        let mut s = self.st.lock().unwrap();
        for (id, json) in items {
            s.feed.retain(|x| x.1 != id);
            s.feed_v += 1;
            let v = s.feed_v;
            s.feed.push_back((v, id, Arc::new(json)));
            while s.feed.len() > FEED_KEEP {
                s.feed.pop_front();
            }
        }
        self.cv.notify_all();
    }

    /// Everything, as one JSON object: `{"live":..,"days":..,"feed":[..]}`.
    pub fn snapshot(&self) -> (String, Seen) {
        let s = self.st.lock().unwrap();
        let mut out = String::with_capacity(s.live.len() + s.days.len() + s.feed.len() * 200 + 64);
        out.push_str("{\"live\":");
        out.push_str(&s.live);
        out.push_str(",\"days\":");
        out.push_str(&s.days);
        out.push_str(",\"feed\":[");
        for (i, (_, _, j)) in s.feed.iter().enumerate() {
            if i > 0 {
                out.push(',');
            }
            out.push_str(j);
        }
        out.push_str("]}");
        (out, Seen { live: s.live_v, days: s.days_v, feed: s.feed_v })
    }

    /// Waits up to `timeout` for anything newer than `seen`.
    pub fn wait(&self, seen: Seen, timeout: Duration) -> Update {
        let s = self.st.lock().unwrap();
        let (s, _) = self
            .cv
            .wait_timeout_while(s, timeout, |s| s.live_v == seen.live && s.days_v == seen.days && s.feed_v == seen.feed)
            .unwrap();
        Update {
            live: (s.live_v != seen.live).then(|| s.live.clone()),
            days: (s.days_v != seen.days).then(|| s.days.clone()),
            feed: s.feed.iter().filter(|x| x.0 > seen.feed).map(|x| x.2.clone()).collect(),
            seen: Seen { live: s.live_v, days: s.days_v, feed: s.feed_v },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_older_payload_never_replaces_a_newer_one() {
        let h = Hub::new();
        h.set_live_at("new".into(), 200);
        h.set_live_at("old".into(), 100);
        assert_eq!(h.snapshot().0.contains("new"), true);
        h.set_days_at("d2".into(), 50);
        h.set_days_at("d1".into(), 40);
        assert!(h.snapshot().0.contains("d2"));
    }

    #[test]
    fn idle_marks_stale_until_a_client_connects() {
        let h = Hub::new();
        assert!(h.idle());
        assert!(h.take_stale());
        assert!(!h.take_stale());
        {
            let _g = h.client();
            assert!(!h.idle());
            assert!(!h.take_stale());
        }
        assert!(h.idle());
    }

    #[test]
    fn feed_upserts_and_wait_returns_changes() {
        let h = Hub::new();
        h.push_feed(vec![("a".into(), "{\"id\":\"a\",\"v\":1}".into())]);
        h.push_feed(vec![("a".into(), "{\"id\":\"a\",\"v\":2}".into())]);
        let (snap, seen) = h.snapshot();
        assert!(snap.contains("\"v\":2") && !snap.contains("\"v\":1"));
        let none = h.wait(seen, Duration::from_millis(10));
        assert!(none.live.is_none() && none.feed.is_empty());
        h.set_live("{\"x\":1}".into());
        let up = h.wait(seen, Duration::from_millis(10));
        assert_eq!(up.live.as_deref().map(String::as_str), Some("{\"x\":1}"));
    }
}
