//! Thirty days of daily totals, kept apart from the seven-day window.
//!
//! When a day ages out of `Agg.days` it is moved here with totals only: no
//! sessions, no hours, no dedupe keys. The file has its own version number,
//! so a `CACHE_VERSION` bump that rebuilds the seven-day window never wipes
//! the month. About 5 KB for a full set of entries.

use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};
use std::path::Path;

use crate::agg::DayAgg;
use crate::projects::{self, ProjectMap};
use crate::transcript::{family, Tokens};

/// Version of `.state/history.json`, independent of `CACHE_VERSION`.
/// Bumped when days gained per-project maps: an older file is discarded and
/// the low-priority backfill reads those days again once.
pub const HISTORY_VERSION: u32 = 2;
/// At most 31 entries: a full month plus one day of slack.
pub const HISTORY_MAX: usize = 31;
/// The month is the most recent 30 local days, today included.
pub const MONTH_DAYS: i32 = 30;
/// Oldest day a fresh start backfills: today - 30.
pub const BACKFILL_OLDEST: i32 = 30;
/// Newest day a fresh start backfills: today - 8. The day before that
/// (today - 7) arrives from the day roll, so it is not read twice.
pub const BACKFILL_NEWEST: i32 = 8;

/// One day of the month: totals only.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct HistDay {
    pub day: i32,
    /// input, output, cache read, cache write 5m, cache write 1h.
    pub t: [u64; 5],
    pub n: u64,
    /// Weighted units (list-price micro-dollars). Codex has weight 0.
    pub w: f64,
    /// Weighted units per model family: opus, sonnet, haiku, other.
    pub fam: [f64; 4],
    pub codex_tokens: u64,
    pub active_min: u32,
    /// Top twelve projects by weight on that day, rest folded into "Other".
    #[serde(default)]
    pub projects: ProjectMap,
}

impl HistDay {
    /// Takes a day that is about to be pruned from the seven-day window.
    pub fn from_agg(day: i32, agg: &DayAgg, codex_tokens: u64) -> Self {
        let mut out = HistDay {
            day,
            codex_tokens,
            active_min: agg.active_slots() * 5,
            projects: projects::fold(&agg.projects),
            ..Default::default()
        };
        out.add_agg(agg);
        out
    }

    pub fn add_agg(&mut self, agg: &DayAgg) {
        for (m, u) in &agg.models {
            add_tokens(&mut self.t, &u.t);
            self.n += u.n;
            self.w += u.w;
            self.fam[family(m)] += u.w;
        }
    }

}

fn add_tokens(into: &mut [u64; 5], t: &Tokens) {
    into[0] += t.input;
    into[1] += t.output;
    into[2] += t.cache_read;
    into[3] += t.cache_w5m;
    into[4] += t.cache_w1h;
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct History {
    pub v: u32,
    pub days: Vec<HistDay>,
    /// Backfill days already looked at, including days with no usage. Without
    /// this, an empty day always counts as missing and every start reads the
    /// same gigabyte again. No version bump: old files simply start with an
    /// empty list and keep all their existing days.
    #[serde(default)]
    pub checked: Vec<i32>,
}

impl History {
    /// Days that need no backfill: those with data and those already checked.
    pub fn have(&self) -> std::collections::HashSet<i32> {
        self.days.iter().map(|d| d.day).chain(self.checked.iter().copied()).collect()
    }

    /// Loads the month, or starts a fresh one when the file is missing or
    /// carries another version. The seven-day cache is never consulted.
    pub fn load(path: &Path) -> Self {
        match crate::state::load::<History>(path) {
            Some(h) if h.v == HISTORY_VERSION => h,
            _ => History { v: HISTORY_VERSION, days: Vec::new(), checked: Vec::new() },
        }
    }

    /// Inserts or replaces one day. Returns true only when something changed.
    pub fn upsert(&mut self, day: HistDay) -> bool {
        if let Some(e) = self.days.iter_mut().find(|d| d.day == day.day) {
            if *e == day {
                return false;
            }
            *e = day;
        } else {
            self.days.push(day);
        }
        self.days.sort_by_key(|d| d.day);
        if self.days.len() > HISTORY_MAX {
            let extra = self.days.len() - HISTORY_MAX;
            self.days.drain(..extra);
        }
        true
    }

    /// Keeps at most `MONTH_DAYS` recent days and at most `HISTORY_MAX`
    /// entries, oldest first. Returns true when anything was dropped.
    pub fn trim(&mut self, today: i32) -> bool {
        let before = self.days.len();
        self.days.retain(|d| d.day >= today - MONTH_DAYS && d.day <= today);
        if self.days.len() > HISTORY_MAX {
            let extra = self.days.len() - HISTORY_MAX;
            self.days.drain(..extra);
        }
        self.checked.retain(|d| *d >= today - MONTH_DAYS && *d <= today);
        self.days.len() != before
    }

    /// Remember backfill days, empty ones included, so the next start skips
    /// them. Returns true only when a day was newly recorded.
    pub fn mark_checked(&mut self, days: impl IntoIterator<Item = i32>) -> bool {
        let mut changed = false;
        for day in days {
            if !self.checked.contains(&day) {
                self.checked.push(day);
                changed = true;
            }
        }
        if changed {
            self.checked.sort_unstable();
        }
        changed
    }

    /// Moves every day that `prune` is about to drop (older than `min_day`)
    /// into the month. Codex days join as a token count only.
    pub fn capture_expired(&mut self, days: &BTreeMap<i32, DayAgg>, min_day: i32, today: i32, codex: &HashMap<i32, u64>) -> bool {
        let mut changed = false;
        for (day, agg) in days {
            if *day >= min_day {
                continue;
            }
            let cx = codex.get(day).copied().unwrap_or(0);
            if agg.models.is_empty() && cx == 0 {
                continue;
            }
            changed |= self.upsert(HistDay::from_agg(*day, agg, cx));
        }
        if changed {
            self.trim(today);
        }
        changed
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agg::{Agg, UsageIn, WEEK_DAYS};
    use crate::state;
    use crate::tail::TailCache;
    use crate::timeutil::{day_start_ms, MS_PER_HOUR};
    use std::fs;

    const OFF: i32 = 330;
    const TODAY: i32 = 20_774; // 2026-11-17 in local days

    fn a_day(day: i32, out: u64) -> DayAgg {
        let mut a = Agg::default();
        let tok = Tokens { output: out, ..Default::default() };
        a.add_usage_at(
            UsageIn {
                session: "s1",
                sub: false,
                observer: false,
                ts_ms: day_start_ms(day, OFF) + 5 * MS_PER_HOUR,
                model: "claude-opus-5-5",
                tokens: &tok,
                fast: false,
                msg_id: Some("m1"),
                prev_ms: 0,
                codex: false,
            },
            OFF,
            day_start_ms(TODAY, OFF) + 12 * MS_PER_HOUR,
        );
        a.days.remove(&day).unwrap()
    }

    #[test]
    fn a_day_moves_from_the_window_into_history() {
        let mut agg = Agg::default();
        let day = TODAY - (WEEK_DAYS - 1);
        agg.days.insert(day, a_day(day, 4321));
        let mut h = History { v: HISTORY_VERSION, days: Vec::new(), checked: Vec::new() };
        assert!(h.capture_expired(&agg.days, day + 1, TODAY, &HashMap::new()));
        let moved = &h.days[0];
        assert_eq!(moved.day, day);
        assert_eq!(moved.t[1], 4321);
        assert_eq!(moved.n, 1);
        assert!(moved.w > 0.0 && moved.fam[0] > 0.0);
        // The same call again does not rewrite the file: no change.
        assert!(!h.capture_expired(&agg.days, day + 1, TODAY, &HashMap::new()));
        // Pruning the window drops the day from the live aggregates, but the
        // totals survive in the month.
        agg.prune(day + 1, day_start_ms(TODAY, OFF) + 12 * MS_PER_HOUR, OFF);
        assert!(!agg.days.contains_key(&day));
        assert_eq!(h.days[0].t[1], 4321);
    }

    #[test]
    fn history_is_capped_at_31_entries() {
        let mut h = History { v: HISTORY_VERSION, days: Vec::new(), checked: Vec::new() };
        for back in 0..40 {
            assert!(h.upsert(HistDay { day: TODAY - back, n: 1, ..Default::default() }));
        }
        assert_eq!(h.days.len(), HISTORY_MAX);
        assert_eq!(h.days[0].day, TODAY - MONTH_DAYS);
        assert_eq!(h.days[HISTORY_MAX - 1].day, TODAY);
        assert!(h.days.windows(2).all(|w| w[0].day < w[1].day));
    }

    #[test]
    fn history_file_round_trips_and_survives_a_cache_version_change() {
        let dir = std::env::temp_dir().join(format!("sb-history-{}-{TODAY}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let hp = dir.join("history.json");
        let mut h = History { v: HISTORY_VERSION, days: Vec::new(), checked: Vec::new() };
        for back in 8..=30 {
            h.upsert(HistDay { day: TODAY - back, n: back as u64, t: [1, 2, 3, 4, 5], w: 12.5, ..Default::default() });
        }
        let bytes = state::save(&hp, &h).unwrap();
        assert!(bytes < 8_000, "history.json is {bytes} bytes");
        assert_eq!(state::load::<History>(&hp).unwrap(), h);
        // A cache-version bump rebuilds transcripts.json. The month lives in
        // its own file and is untouched by it.
        let bumped = TailCache { v: crate::tail::CACHE_VERSION + 1, off: OFF, files: HashMap::new(), sessions: HashMap::new(), agg: Agg::default(), quiet_before: 0 };
        state::save(&dir.join("transcripts.json"), &bumped).unwrap();
        assert_eq!(History::load(&hp), h);
        let mut t = crate::tail::Tailer::new(dir.clone(), OFF);
        assert!(!t.restore(bumped), "the bumped cache is refused");
        fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn a_history_version_change_discards_the_old_file() {
        let dir = std::env::temp_dir().join(format!("sb-histver-{}-{TODAY}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let hp = dir.join("history.json");
        // A file written by the previous version holds totals but no projects.
        state::save(&hp, &serde_json::json!({
            "v": HISTORY_VERSION - 1,
            "days": [{ "day": TODAY - 10, "t": [1, 2, 3, 4, 5], "n": 1, "w": 12.5 }],
        })).unwrap();
        let loaded = History::load(&hp);
        assert_eq!(loaded.v, HISTORY_VERSION);
        assert!(loaded.days.is_empty(), "the old days are discarded so backfill refills them");
        // A current file round trips its per-project map.
        let mut fresh = History { v: HISTORY_VERSION, days: Vec::new(), checked: Vec::new() };
        let mut p = ProjectMap::new();
        projects::add(&mut p, "demo", 2.0, 3, 40);
        assert!(fresh.upsert(HistDay { day: TODAY - 10, projects: p.clone(), ..Default::default() }));
        state::save(&hp, &fresh).unwrap();
        assert_eq!(History::load(&hp), fresh);
        fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn checked_days_survive_a_save_and_keep_existing_days_on_upgrade() {
        let dir = std::env::temp_dir().join(format!("sb-checked-{}-{TODAY}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let hp = dir.join("history.json");
        // An old file has days but no checked list: it loads without a version
        // bump and keeps every day it already had.
        state::save(&hp, &serde_json::json!({
            "v": HISTORY_VERSION,
            "days": [{ "day": TODAY - 10, "t": [1, 2, 3, 4, 5], "n": 1, "w": 12.5, "fam": [12.5, 0, 0, 0], "codex_tokens": 0, "active_min": 5, "projects": {} }],
        })).unwrap();
        let mut h = History::load(&hp);
        assert_eq!(h.days.len(), 1);
        assert!(h.checked.is_empty());
        assert_eq!(h.have().len(), 1);

        let days: Vec<i32> = (TODAY - BACKFILL_OLDEST..=TODAY - BACKFILL_NEWEST).collect();
        assert!(h.mark_checked(days.clone()));
        assert!(!h.mark_checked(days), "marking twice changes nothing");
        assert_eq!(h.have().len(), 23, "the existing day is inside the checked range");
        let bytes = state::save(&hp, &h).unwrap();
        assert!(bytes < 10_000, "history.json is {bytes} bytes");
        assert_eq!(History::load(&hp), h);

        // Days that fall out of the month drop out of the checked list too.
        h.checked.push(TODAY - 40);
        h.trim(TODAY);
        assert!(!h.checked.contains(&(TODAY - 40)));
        fs::remove_dir_all(dir).ok();
    }
}
