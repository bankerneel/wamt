//! Aggregates built from transcript lines: per-day totals, per-minute weights
//! for the last 26 hours, limit hits, and the 5-hour window estimate.
//! Only numbers, model ids and session ids are stored here.

use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};

#[cfg(test)]
use crate::timeutil::now_ms;
use crate::timeutil::{day_of, day_start_ms, hour_of, MS_PER_HOUR, MS_PER_MIN};
use crate::transcript::{family, weight, Tokens};

pub const WINDOW_MS: i64 = 5 * MS_PER_HOUR;
/// Window starts snap to this grid (T3's recorded resets sit on 10-minute marks).
pub const WINDOW_GRID_MS: i64 = 10 * MS_PER_MIN;
const MINUTES_KEPT_MS: i64 = 26 * MS_PER_HOUR;
const HIT_DEDUPE_MS: i64 = 10 * MS_PER_MIN;
/// Local days kept in all: today plus six. Only the newest `DETAIL_DAYS`
/// keep per-session detail; older days are compacted to daily totals.
pub const WEEK_DAYS: i32 = 7;
pub const DETAIL_DAYS: i32 = 3;
/// Dedupe keys of compacted days are cut to their low 32 bits.
const SEEN_OLD_MASK: u64 = 0xFFFF_FFFF;

/// Usage for one key: tokens, number of API responses, weighted units.
#[derive(Default, Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Use {
    pub t: Tokens,
    pub n: u64,
    pub w: f64,
}

impl Use {
    pub fn add(&mut self, t: &Tokens, w: f64) {
        self.t.add(t);
        self.n += 1;
        self.w += w;
    }
    pub fn merge(&mut self, o: &Use) {
        self.t.add(&o.t);
        self.n += o.n;
        self.w += o.w;
    }
}

#[derive(Default, Clone, Debug, Serialize, Deserialize)]
pub struct SessionDay {
    pub models: BTreeMap<String, Use>,
    /// The part of this session's usage that came from its subagent files.
    pub sub: Use,
    pub first: i64,
    pub last: i64,
    /// Cold restarts: big cache rebuilds after the cache had expired.
    #[serde(default)]
    pub cold: ColdUse,
    /// Today only: weight (per model family) and output tokens per 10-minute
    /// slot of the local day. Sparse, and dropped when the day rolls.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub buckets: BTreeMap<u16, Bucket>,
}

/// Length of one timeline bucket.
pub const BUCKET_MS: i64 = 10 * MS_PER_MIN;

/// One 10-minute timeline bucket of a session: numbers only.
#[derive(Default, Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct Bucket {
    pub w: [f64; 4],
    pub out: u64,
}

/// Spoiled leftovers: requests that rebuilt over 50K tokens of cache after
/// the previous request's cache had expired.
#[derive(Default, Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct ColdUse {
    pub n: u32,
    /// Cache-creation tokens of those requests.
    pub tok: u64,
    /// Weight of just those cache-write tokens.
    pub w: f64,
}

impl ColdUse {
    pub fn merge(&mut self, o: &ColdUse) {
        self.n += o.n;
        self.tok += o.tok;
        self.w += o.w;
    }
}

/// Cache writes above this many tokens make a rebuild worth counting.
pub const COLD_MIN_TOKENS: u64 = 50_000;
const CACHE_TTL_5M_MS: i64 = 5 * MS_PER_MIN;
const CACHE_TTL_1H_MS: i64 = 60 * MS_PER_MIN;

/// Is this request a cold restart? `prev_ms` is the previous request in the
/// same transcript stream (0 when none was seen).
pub fn is_cold(t: &Tokens, ts_ms: i64, prev_ms: i64) -> bool {
    if prev_ms <= 0 {
        return false;
    }
    let writes = t.cache_w5m + t.cache_w1h;
    if writes <= COLD_MIN_TOKENS {
        return false;
    }
    let ttl = if t.cache_w1h > t.cache_w5m { CACHE_TTL_1H_MS } else { CACHE_TTL_5M_MS };
    ts_ms - prev_ms > ttl
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DayAgg {
    pub models: BTreeMap<String, Use>,
    /// Weighted units per local hour and model family.
    pub hours: Vec<[f64; 4]>,
    /// API responses per local hour.
    pub hours_n: Vec<u32>,
    pub sessions: HashMap<String, SessionDay>,
    /// 288 five-minute slots with at least one request (bitset).
    pub active: [u64; 5],
    /// Subagent transcript files first seen this day: parent session ->
    /// "agentType|model" -> count.
    pub subs: BTreeMap<String, BTreeMap<String, u32>>,
}

impl Default for DayAgg {
    fn default() -> Self {
        DayAgg {
            models: BTreeMap::new(),
            hours: vec![[0.0; 4]; 24],
            hours_n: vec![0; 24],
            sessions: HashMap::new(),
            active: [0; 5],
            subs: BTreeMap::new(),
        }
    }
}

/// One day of the weekly view: totals only.
#[derive(Clone, Debug, PartialEq)]
pub struct WeekDay {
    pub day: i32,
    pub total: Use,
    /// Weighted units per model family.
    pub fam: [f64; 4],
    /// Weighted units per local hour (all families).
    pub hours: Vec<f64>,
    pub active_min: u32,
}

impl DayAgg {
    /// Drops per-session detail, keeping models, hours and active time.
    pub fn compact(&mut self) {
        self.sessions = HashMap::new();
        self.subs = BTreeMap::new();
    }
    pub fn is_compact(&self) -> bool {
        self.sessions.is_empty() && self.subs.is_empty()
    }
    pub fn week_summary(&self, day: i32) -> WeekDay {
        let mut fam = [0.0; 4];
        for (m, u) in &self.models {
            fam[family(m)] += u.w;
        }
        WeekDay {
            day,
            total: self.total(),
            fam,
            hours: self.hours.iter().map(|h| h.iter().sum()).collect(),
            active_min: self.active_slots() * 5,
        }
    }
    pub fn active_slots(&self) -> u32 {
        self.active.iter().map(|w| w.count_ones()).sum()
    }
    pub fn total(&self) -> Use {
        let mut u = Use::default();
        for m in self.models.values() {
            u.merge(m);
        }
        u
    }
}

/// One minute of activity: weighted units per family, observer share, calls.
#[derive(Default, Clone, Debug, Serialize, Deserialize)]
pub struct MinCell {
    pub w: [f64; 4],
    /// Weighted units from claude-mem's note-taker (included in `w`).
    pub o: f64,
    pub n: u32,
}

/// Weighted usage of the most recent window that ended in a limit hit.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CapRef {
    pub used: f64,
    pub hit_ms: i64,
    pub start_ms: i64,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct WindowEst {
    /// False when no request happened in the last 5 hours.
    pub active: bool,
    pub start_ms: i64,
    pub end_ms: i64,
    /// True when start/end come from a reset T3 recorded, not a guess.
    pub anchored: bool,
    pub used: f64,
    pub by_family: [f64; 4],
    pub observer: f64,
    pub calls: u32,
    pub cap: Option<CapRef>,
    /// Weighted usage per minute over the last 30 minutes of this window.
    pub rate_per_min: f64,
    /// When the plate reaches the cap at that rate, if before the reset.
    pub eta_ms: Option<i64>,
}

/// Minutes of recent eating the forecast looks at.
const RATE_SPAN_MIN: i64 = 30;

#[derive(Default, Serialize, Deserialize)]
pub struct Agg {
    pub days: BTreeMap<i32, DayAgg>,
    /// Minute index (epoch ms / 60000) -> cell, for the last 26 hours.
    pub mins: BTreeMap<i64, MinCell>,
    /// Limit hits (deduped), last 7 days.
    pub hits: Vec<i64>,
    /// Weekly-limit hits (deduped), last 7 days. Kept apart from `hits` so a
    /// weekly hit never sets the 5-hour plate's reference.
    #[serde(default)]
    pub week_hits: Vec<i64>,
    /// Kept past the 3-day window on purpose (one number).
    pub cap_ref: Option<CapRef>,
    /// The latest window worked out (start, end, from a recorded reset).
    #[serde(default)]
    pub win_seed: Option<(i64, i64, bool)>,
    /// Message-id hashes already counted -> local day, for dedupe across lines
    /// and across forked session files.
    #[serde(with = "seen_codec")]
    pub seen: HashMap<u64, i32>,
    #[serde(skip)]
    pub dirty: bool,
}

/// 64-bit FNV-1a: stable across Rust versions, unlike `DefaultHasher`.
pub fn fnv64(s: &str) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in s.as_bytes() {
        h ^= *b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

pub struct UsageIn<'a> {
    pub session: &'a str,
    pub sub: bool,
    pub observer: bool,
    pub ts_ms: i64,
    pub model: &'a str,
    pub tokens: &'a Tokens,
    pub fast: bool,
    pub msg_id: Option<&'a str>,
    /// Previous request time in the same transcript file (0 = none seen).
    pub prev_ms: i64,
    /// A Codex response: weight 0, and kept out of the Claude minute cells,
    /// hourly strip and active time (Codex does not count toward Claude's limit).
    pub codex: bool,
}

impl Agg {
    /// Adds one API response. Returns false when it was already counted.
    #[cfg(test)]
    pub fn add_usage(&mut self, u: UsageIn, off: i32) -> bool {
        self.add_usage_at(u, off, now_ms())
    }

    /// Like `add_usage`, with the current time given (the tailer's clock).
    pub fn add_usage_at(&mut self, u: UsageIn, off: i32, now: i64) -> bool {
        let day = day_of(u.ts_ms, off);
        // Days past the detail window keep a 32-bit dedupe key (see `prune`):
        // enough to count a forked copy of old history once, at half the size.
        let old = day <= day_of(now, off) - DETAIL_DAYS;
        if let Some(id) = u.msg_id {
            let h = fnv64(id);
            let key = if old { h & SEEN_OLD_MASK } else { h };
            if self.seen.contains_key(&h) || self.seen.contains_key(&key) {
                return false;
            }
            self.seen.insert(key, day);
        }
        let w = if u.codex { 0.0 } else { weight(u.model, u.tokens, u.fast) };
        let fam = family(u.model);
        let d = self.days.entry(day).or_default();
        d.models.entry(u.model.to_string()).or_default().add(u.tokens, w);
        // Days older than the detail window keep totals only.
        let detail = day > day_of(now, off) - DETAIL_DAYS;
        if !u.codex {
            let hr = hour_of(u.ts_ms, off);
            d.hours[hr][fam] += w;
            d.hours_n[hr] += 1;
            let slot = ((u.ts_ms - day_start_ms(day, off)) / (5 * MS_PER_MIN)).clamp(0, 287) as usize;
            d.active[slot / 64] |= 1u64 << (slot % 64);
        }
        if !detail {
            // Older than the minute cells anyway.
            self.dirty = true;
            return true;
        }
        let s = d.sessions.entry(u.session.to_string()).or_default();
        s.models.entry(u.model.to_string()).or_default().add(u.tokens, w);
        if u.sub {
            s.sub.add(u.tokens, w);
        }
        // Timeline buckets are kept for the current local day only.
        if day == day_of(now, off) {
            let slot = ((u.ts_ms - day_start_ms(day, off)) / BUCKET_MS).clamp(0, 143) as u16;
            let b = s.buckets.entry(slot).or_default();
            b.w[fam] += w;
            b.out += u.tokens.output;
        }
        if is_cold(u.tokens, u.ts_ms, u.prev_ms) {
            let writes = Tokens { cache_w5m: u.tokens.cache_w5m, cache_w1h: u.tokens.cache_w1h, ..Default::default() };
            s.cold.n += 1;
            s.cold.tok += writes.total();
            s.cold.w += weight(u.model, &writes, u.fast);
        }
        if s.first == 0 || u.ts_ms < s.first {
            s.first = u.ts_ms;
        }
        s.last = s.last.max(u.ts_ms);
        if !u.codex {
            let m = self.mins.entry(u.ts_ms.div_euclid(MS_PER_MIN)).or_default();
            m.w[fam] += w;
            m.n += 1;
            if u.observer {
                m.o += w;
            }
        }
        self.dirty = true;
        true
    }

    pub fn add_subagent_file(&mut self, day: i32, parent: &str, label: &str, off: i32, now: i64) {
        if day <= day_of(now, off) - DETAIL_DAYS {
            return;
        }
        *self
            .days
            .entry(day)
            .or_default()
            .subs
            .entry(parent.to_string())
            .or_default()
            .entry(label.to_string())
            .or_default() += 1;
        self.dirty = true;
    }

    /// Records a rate-limit hit unless one was recorded within 10 minutes.
    pub fn add_limit_hit(&mut self, ts_ms: i64) -> bool {
        if self.hits.iter().any(|h| (h - ts_ms).abs() < HIT_DEDUPE_MS) {
            return false;
        }
        self.hits.push(ts_ms);
        self.hits.sort_unstable();
        self.dirty = true;
        true
    }

    /// Records a weekly-limit hit unless one was recorded within 10 minutes.
    pub fn add_weekly_hit(&mut self, ts_ms: i64) -> bool {
        if self.week_hits.iter().any(|h| (h - ts_ms).abs() < HIT_DEDUPE_MS) {
            return false;
        }
        self.week_hits.push(ts_ms);
        self.week_hits.sort_unstable();
        self.dirty = true;
        true
    }

    /// Weighted usage per local day for the last 7 days, oldest first.
    pub fn week(&self, today: i32) -> Vec<WeekDay> {
        (today - (WEEK_DAYS - 1)..=today)
            .map(|d| self.days.get(&d).map_or_else(|| DayAgg::default().week_summary(d), |a| a.week_summary(d)))
            .collect()
    }

    /// Drops everything older than local day `min_day`, and minute cells
    /// older than 26 hours. Days past the detail window are compacted to
    /// totals (sessions and subagent counts dropped).
    pub fn prune(&mut self, min_day: i32, now_ms: i64, off: i32) {
        let before = (self.days.len(), self.mins.len(), self.seen.len(), self.hits.len() + self.week_hits.len());
        for (d, a) in self.days.iter_mut() {
            if *d <= day_of(now_ms, off) - DETAIL_DAYS && !a.is_compact() {
                a.compact();
                self.dirty = true;
            }
        }
        // Day roll: yesterday's timeline buckets go (only today's are kept).
        let today = day_of(now_ms, off);
        for (d, a) in self.days.iter_mut() {
            if *d < today {
                for s in a.sessions.values_mut() {
                    if !s.buckets.is_empty() {
                        s.buckets = BTreeMap::new();
                        self.dirty = true;
                    }
                }
            }
        }
        self.days.retain(|d, _| *d >= min_day);
        self.seen.retain(|_, d| *d >= min_day);
        let old_cut = today - DETAIL_DAYS;
        if self.seen.iter().any(|(k, d)| *d <= old_cut && *k > SEEN_OLD_MASK) {
            let seen = std::mem::take(&mut self.seen);
            self.seen = seen.into_iter().map(|(k, d)| (if d <= old_cut { k & SEEN_OLD_MASK } else { k }, d)).collect();
            self.dirty = true;
        }
        let min_minute = (now_ms - MINUTES_KEPT_MS).div_euclid(MS_PER_MIN);
        self.mins.retain(|m, _| *m >= min_minute);
        let from = day_start_ms(min_day, off);
        self.hits.retain(|h| *h >= from);
        self.week_hits.retain(|h| *h >= from);
        if before != (self.days.len(), self.mins.len(), self.seen.len(), self.hits.len() + self.week_hits.len()) {
            self.dirty = true;
        }
    }

    /// Splits recorded activity into 5-hour windows. A window opens at the
    /// first request after the previous one closed, snapped down to the
    /// 10-minute grid, unless a recorded reset (`anchors`) covers it.
    fn windows(&self, anchors: &[i64]) -> Vec<(i64, i64, bool)> {
        let mut out: Vec<(i64, i64, bool)> = Vec::new();
        // Continue from the window remembered last time, so pruning old
        // minutes can never shift where the current window seems to start.
        let mut from_minute = i64::MIN;
        if let Some((s, e, a)) = self.win_seed {
            let reset = anchors.iter().copied().filter(|r| *r > s && r - WINDOW_MS < e).max();
            let w = match reset {
                Some(r) => (r - WINDOW_MS, r, true),
                None => (s, e, a),
            };
            from_minute = w.0.div_euclid(MS_PER_MIN);
            out.push(w);
        }
        for &m in self.mins.range(from_minute..).map(|(k, _)| k) {
            let t = m * MS_PER_MIN;
            if out.last().map_or(true, |w| t >= w.1) {
                let anchored = anchors.iter().copied().filter(|r| r - WINDOW_MS <= t && t < *r).max();
                let (s, a) = match anchored {
                    Some(r) => (r - WINDOW_MS, true),
                    None => (t - t.rem_euclid(WINDOW_GRID_MS), false),
                };
                out.push((s, s + WINDOW_MS, a));
            }
        }
        out
    }

    fn sum(&self, from_ms: i64, to_ms: i64) -> ([f64; 4], f64, u32) {
        let mut w = [0.0; 4];
        let mut o = 0.0;
        let mut n = 0;
        let a = from_ms.div_euclid(MS_PER_MIN);
        let b = to_ms.div_euclid(MS_PER_MIN);
        for (_, c) in self.mins.range(a..=b) {
            for i in 0..4 {
                w[i] += c.w[i];
            }
            o += c.o;
            n += c.n;
        }
        (w, o, n)
    }

    /// The current 5-hour window and the reference it is compared against.
    /// Also refreshes `cap_ref` from limit hits inside the kept minutes.
    pub fn window(&mut self, now_ms: i64, anchors: &[i64]) -> WindowEst {
        let wins = self.windows(anchors);
        if let Some(last) = wins.last().copied() {
            if self.win_seed != Some(last) {
                self.win_seed = Some(last);
                self.dirty = true;
            }
        }
        // Reference: the latest window containing a limit hit, measured up
        // to its first hit.
        for &(s, e, _) in wins.iter().rev() {
            if let Some(&h) = self.hits.iter().find(|h| **h >= s && **h < e) {
                let (w, _, _) = self.sum(s, h);
                let used: f64 = w.iter().sum();
                if used > 0.0 {
                    let cand = CapRef { used, hit_ms: h, start_ms: s };
                    if self.cap_ref.as_ref().map_or(true, |c| c.hit_ms < h) {
                        self.cap_ref = Some(cand);
                        self.dirty = true;
                    }
                }
                break;
            }
        }
        match wins.last() {
            Some(&(s, e, anchored)) if e > now_ms => {
                let (w, o, n) = self.sum(s, now_ms);
                let used: f64 = w.iter().sum();
                let (rate_per_min, eta_ms) = self.forecast(now_ms, s, e, used);
                WindowEst {
                    active: true,
                    start_ms: s,
                    end_ms: e,
                    anchored,
                    used,
                    by_family: w,
                    observer: o,
                    calls: n,
                    cap: self.cap_ref.clone(),
                    rate_per_min,
                    eta_ms,
                }
            }
            _ => WindowEst {
                active: false,
                start_ms: 0,
                end_ms: 0,
                anchored: false,
                used: 0.0,
                by_family: [0.0; 4],
                observer: 0.0,
                calls: 0,
                cap: self.cap_ref.clone(),
                rate_per_min: 0.0,
                eta_ms: None,
            },
        }
    }

    /// Eating rate over the last 30 minutes (or since the window opened, if
    /// later, but never less than 5 minutes so one burst does not dominate)
    /// and the time the plate would reach the last cap at that rate. The
    /// time is None when there is no cap, no recent eating, or the reset
    /// comes first. Already over the cap means "now".
    fn forecast(&self, now_ms: i64, start_ms: i64, end_ms: i64, used: f64) -> (f64, Option<i64>) {
        let from = (now_ms - RATE_SPAN_MIN * MS_PER_MIN).max(start_ms);
        let span_min = ((now_ms - from) as f64 / MS_PER_MIN as f64).max(5.0);
        let (w, _, _) = self.sum(from, now_ms);
        let rate = w.iter().sum::<f64>() / span_min;
        let Some(cap) = self.cap_ref.as_ref() else { return (rate, None) };
        let left = cap.used - used;
        if left <= 0.0 {
            return (rate, Some(now_ms));
        }
        if rate <= 0.0 {
            return (rate, None);
        }
        let eta = now_ms + (left / rate * MS_PER_MIN as f64) as i64;
        (rate, (eta < end_ms).then_some(eta))
    }
}

/// Serializes the dedupe set compactly: day -> base64 of little-endian u64s.
mod seen_codec {
    use serde::{Deserialize, Deserializer, Serialize, Serializer};
    use std::collections::{BTreeMap, HashMap};

    const ALPHA: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

    pub fn encode(bytes: &[u8]) -> String {
        let mut s = String::with_capacity(bytes.len() / 3 * 4 + 4);
        for ch in bytes.chunks(3) {
            let b = [ch[0], *ch.get(1).unwrap_or(&0), *ch.get(2).unwrap_or(&0)];
            let n = (b[0] as u32) << 16 | (b[1] as u32) << 8 | b[2] as u32;
            for i in 0..4 {
                if i <= ch.len() {
                    s.push(ALPHA[(n >> (18 - 6 * i) & 63) as usize] as char);
                } else {
                    s.push('=');
                }
            }
        }
        s
    }

    pub fn decode(s: &str) -> Option<Vec<u8>> {
        let mut out = Vec::with_capacity(s.len() / 4 * 3);
        let val = |c: u8| ALPHA.iter().position(|&a| a == c).map(|p| p as u32);
        for ch in s.as_bytes().chunks(4) {
            if ch.len() != 4 {
                return None;
            }
            let mut n = 0u32;
            let mut pad = 0;
            for &c in ch {
                n <<= 6;
                if c == b'=' {
                    pad += 1;
                } else {
                    n |= val(c)?;
                }
            }
            let b = [(n >> 16) as u8, (n >> 8) as u8, n as u8];
            out.extend_from_slice(&b[..3 - pad]);
        }
        Some(out)
    }

    /// A day whose keys all fit 32 bits (a compacted day) is written as "4:"
    /// plus 4-byte entries; any other day as 8-byte entries.
    pub fn serialize<S: Serializer>(m: &HashMap<u64, i32>, s: S) -> Result<S::Ok, S::Error> {
        let mut by_day: BTreeMap<i32, Vec<u64>> = BTreeMap::new();
        for (h, d) in m {
            by_day.entry(*d).or_default().push(*h);
        }
        let enc: BTreeMap<i32, String> = by_day
            .into_iter()
            .map(|(d, keys)| {
                if keys.iter().all(|k| *k <= u32::MAX as u64) {
                    let b: Vec<u8> = keys.iter().flat_map(|k| (*k as u32).to_le_bytes()).collect();
                    (d, format!("4:{}", encode(&b)))
                } else {
                    let b: Vec<u8> = keys.iter().flat_map(|k| k.to_le_bytes()).collect();
                    (d, encode(&b))
                }
            })
            .collect();
        enc.serialize(s)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<HashMap<u64, i32>, D::Error> {
        let enc: BTreeMap<i32, String> = BTreeMap::deserialize(d)?;
        let mut m = HashMap::new();
        for (day, s) in enc {
            let (narrow, body) = match s.strip_prefix("4:") {
                Some(rest) => (true, rest),
                None => (false, s.as_str()),
            };
            let bytes = decode(body).ok_or_else(|| serde::de::Error::custom("bad seen encoding"))?;
            if narrow {
                for c in bytes.chunks_exact(4) {
                    m.insert(u32::from_le_bytes(c.try_into().unwrap()) as u64, day);
                }
            } else {
                for c in bytes.chunks_exact(8) {
                    m.insert(u64::from_le_bytes(c.try_into().unwrap()), day);
                }
            }
        }
        Ok(m)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::timeutil::parse_ts_ms;

    const OFF: i32 = 330;

    fn usage(a: &mut Agg, ts: &str, model: &str, out: u64, id: &str) -> bool {
        let t = Tokens { output: out, ..Default::default() };
        a.add_usage(
            UsageIn {
                session: "s1",
                sub: false,
                observer: false,
                ts_ms: parse_ts_ms(ts).unwrap(),
                model,
                tokens: &t,
                fast: false,
                msg_id: Some(id),
                prev_ms: 0,
                codex: false,
            },
            OFF,
        )
    }

    #[test]
    fn cold_restart_rules() {
        let w5 = |n| Tokens { cache_w5m: n, ..Default::default() };
        let w1 = |n| Tokens { cache_w1h: n, ..Default::default() };
        let m = MS_PER_MIN;
        // No previous request: never cold.
        assert!(!is_cold(&w5(60_000), 100 * m, 0));
        // 5m writes: 6 minutes idle is cold, 4 is not.
        assert!(is_cold(&w5(60_000), 106 * m, 100 * m));
        assert!(!is_cold(&w5(60_000), 104 * m, 100 * m));
        // 1h writes: 6 minutes is warm, 61 is cold.
        assert!(!is_cold(&w1(60_000), 106 * m, 100 * m));
        assert!(is_cold(&w1(60_000), 161 * m, 100 * m));
        // Small rebuilds do not count; exactly 50K does not.
        assert!(!is_cold(&w5(50_000), 200 * m, 100 * m));
        assert!(is_cold(&w5(50_001), 200 * m, 100 * m));
        // The dominant kind picks the TTL.
        let mixed = Tokens { cache_w5m: 10_000, cache_w1h: 50_000, ..Default::default() };
        assert!(!is_cold(&mixed, 110 * m, 100 * m));
    }

    #[test]
    fn cold_restart_sums_and_dedupes() {
        let mut a = Agg::default();
        let mk = |ts: &str, id: &'static str, prev: i64, t: &'static Tokens| UsageIn {
            session: "s1",
            sub: false,
            observer: false,
            ts_ms: parse_ts_ms(ts).unwrap(),
            model: "claude-opus-5-5",
            tokens: t,
            fast: false,
            msg_id: Some(id),
            prev_ms: prev,
            codex: false,
        };
        static T: Tokens = Tokens { input: 0, output: 5, cache_read: 0, cache_w5m: 80_000, cache_w1h: 0 };
        let prev = parse_ts_ms("2026-11-17T10:00:00Z").unwrap();
        assert!(a.add_usage(mk("2026-11-17T10:20:00Z", "c1", prev, &T), OFF));
        // Same message id again (split lines) must not count twice.
        assert!(!a.add_usage(mk("2026-11-17T10:20:00Z", "c1", prev, &T), OFF));
        // A warm request does not count.
        assert!(a.add_usage(mk("2026-11-17T10:22:00Z", "c2", prev + 20 * MS_PER_MIN, &T), OFF));
        let c = a.days.values().next().unwrap().sessions["s1"].cold;
        assert_eq!((c.n, c.tok), (1, 80_000));
        assert!((c.w - 80_000.0 * 4.0 * 1.25).abs() < 1e-6, "{}", c.w);
    }

    fn at(a: &mut Agg, ts_ms: i64, model: &str, out: u64, id: &str) {
        let t = Tokens { output: out, ..Default::default() };
        a.add_usage(
            UsageIn { session: "s1", sub: false, observer: false, ts_ms, model, tokens: &t, fast: false, msg_id: Some(id), prev_ms: 0, codex: false },
            OFF,
        );
    }

    #[test]
    fn ten_minute_buckets_for_today_only() {
        let mut a = Agg::default();
        let now = now_ms();
        let today = day_of(now, OFF);
        let start = day_start_ms(today, OFF);
        // 03:05 and 03:09 share a slot; 03:10 starts the next one.
        at(&mut a, start + 3 * MS_PER_HOUR + 5 * MS_PER_MIN, "claude-opus-5-5", 100, "a");
        at(&mut a, start + 3 * MS_PER_HOUR + 9 * MS_PER_MIN, "claude-haiku-5-5", 10, "b");
        at(&mut a, start + 3 * MS_PER_HOUR + 10 * MS_PER_MIN, "claude-opus-5-5", 7, "c");
        // Yesterday's request leaves no buckets.
        at(&mut a, start - MS_PER_HOUR, "claude-opus-5-5", 5, "d");
        let b = &a.days[&today].sessions["s1"].buckets;
        assert_eq!(b.keys().copied().collect::<Vec<_>>(), vec![18, 19]);
        assert_eq!(b[&18].out, 110);
        assert!(b[&18].w[0] > 0.0 && b[&18].w[2] > 0.0 && b[&18].w[1] == 0.0);
        assert_eq!(b[&19].out, 7);
        assert!(a.days[&(today - 1)].sessions["s1"].buckets.is_empty());
        // Totals still add up with and without buckets.
        let total: f64 = b.values().map(|x| x.w.iter().sum::<f64>()).sum();
        assert!((total - a.days[&today].sessions["s1"].models.values().map(|u| u.w).sum::<f64>()).abs() < 1e-9);
    }

    #[test]
    fn buckets_dropped_at_day_roll() {
        let mut a = Agg::default();
        let now = now_ms();
        let today = day_of(now, OFF);
        at(&mut a, day_start_ms(today, OFF) + MS_PER_HOUR, "claude-opus-5-5", 1, "a");
        assert!(!a.days[&today].sessions["s1"].buckets.is_empty());
        // A day later the same data is yesterday's: buckets go, totals stay.
        a.prune(today - 1, now + 24 * MS_PER_HOUR, OFF);
        let s = &a.days[&today].sessions["s1"];
        assert!(s.buckets.is_empty());
        assert_eq!(s.models["claude-opus-5-5"].t.output, 1);
    }

    #[test]
    fn duplicate_message_ids_count_once() {
        let mut a = Agg::default();
        assert!(usage(&mut a, "2026-11-17T10:00:00Z", "claude-opus-5-5", 100, "m1"));
        assert!(!usage(&mut a, "2026-11-17T10:00:01Z", "claude-opus-5-5", 100, "m1"));
        let d = a.days.values().next().unwrap();
        assert_eq!(d.models["claude-opus-5-5"].t.output, 100);
        assert_eq!(d.models["claude-opus-5-5"].n, 1);
    }

    #[test]
    fn rollover_drops_oldest_day() {
        let mut a = Agg::default();
        usage(&mut a, "2026-11-14T10:00:00Z", "claude-opus-5-5", 1, "a");
        usage(&mut a, "2026-11-15T10:00:00Z", "claude-opus-5-5", 1, "b");
        usage(&mut a, "2026-11-17T10:00:00Z", "claude-opus-5-5", 1, "c");
        let now = parse_ts_ms("2026-11-17T10:00:00Z").unwrap();
        let today = day_of(now, OFF);
        a.prune(today - 2, now, OFF);
        // 14 Nov (three days ago) goes; 15 Nov (day before yesterday) and today stay.
        assert_eq!(a.days.len(), 2);
        assert_eq!(a.seen.len(), 2);
        assert!(!a.days.contains_key(&(today - 3)));
        assert!(a.days.contains_key(&(today - 2)));
        assert!(a.days.contains_key(&today));
    }

    #[test]
    fn hours_and_active_slots_use_local_time() {
        let mut a = Agg::default();
        // 18:35 UTC is 00:05 IST the next day.
        usage(&mut a, "2026-11-16T18:35:00Z", "claude-haiku-5-5", 10, "x");
        let d = a.days.values().next().unwrap();
        assert!(d.hours[0][2] > 0.0);
        assert_eq!(d.hours_n[0], 1);
        assert_eq!(d.active_slots(), 1);
        assert_eq!(d.active[0] & 0b10, 0b10); // slot 1 (00:05 to 00:10)
    }

    #[test]
    fn windows_chain_and_snap_to_grid() {
        let mut a = Agg::default();
        usage(&mut a, "2026-11-17T08:07:00Z", "claude-opus-5-5", 10, "1");
        usage(&mut a, "2026-11-17T09:07:00Z", "claude-opus-5-5", 10, "2");
        usage(&mut a, "2026-11-17T14:07:00Z", "claude-opus-5-5", 10, "3");
        let w = a.windows(&[]);
        assert_eq!(w.len(), 2);
        assert_eq!(w[0].0, parse_ts_ms("2026-11-17T08:00:00Z").unwrap());
        assert_eq!(w[1].0, parse_ts_ms("2026-11-17T14:00:00Z").unwrap());
        let now = parse_ts_ms("2026-11-17T14:30:00Z").unwrap();
        let est = a.window(now, &[]);
        assert!(est.active);
        assert_eq!(est.calls, 1);
    }

    #[test]
    fn recorded_reset_overrides_the_guess() {
        let mut a = Agg::default();
        usage(&mut a, "2026-11-17T10:15:00Z", "claude-opus-5-5", 10, "1");
        let reset = parse_ts_ms("2026-11-17T15:10:00Z").unwrap();
        let w = a.windows(&[reset]);
        assert_eq!(w[0].0, parse_ts_ms("2026-11-17T10:10:00Z").unwrap());
        assert!(w[0].2);
    }

    #[test]
    fn cap_reference_measures_up_to_first_hit() {
        let mut a = Agg::default();
        usage(&mut a, "2026-11-17T10:10:00Z", "claude-opus-5-5", 1000, "1");
        usage(&mut a, "2026-11-17T11:00:00Z", "claude-opus-5-5", 1000, "2");
        let hit = parse_ts_ms("2026-11-17T12:56:00Z").unwrap();
        assert!(a.add_limit_hit(hit));
        assert!(!a.add_limit_hit(hit + 60_000));
        usage(&mut a, "2026-11-17T13:30:00Z", "claude-opus-5-5", 1000, "3");
        let now = parse_ts_ms("2026-11-17T13:31:00Z").unwrap();
        let est = a.window(now, &[]);
        let cap = est.cap.unwrap();
        assert_eq!(cap.hit_ms, hit);
        let per = weight("claude-opus-5-5", &Tokens { output: 1000, ..Default::default() }, false);
        assert!((cap.used - 2.0 * per).abs() < 1e-6);
        assert!((est.used - 3.0 * per).abs() < 1e-6);
    }

    #[test]
    fn window_start_survives_pruning_under_continuous_use() {
        let mut a = Agg::default();
        // A request every 20 minutes from 01:07 to 10:47: no 5-hour gap.
        let t0 = parse_ts_ms("2026-11-17T01:07:00Z").unwrap();
        for i in 0..30 {
            let ts = crate::timeutil::iso_ms(t0 + i * 20 * MS_PER_MIN);
            usage(&mut a, &ts, "claude-opus-5-5", 10, &format!("c{i}"));
        }
        let now = t0 + 29 * 20 * MS_PER_MIN + MS_PER_MIN;
        let first = a.window(now, &[]);
        // Chain: 01:00-06:00, then 06:07 snaps to 06:00-11:00.
        assert_eq!(first.start_ms, parse_ts_ms("2026-11-17T06:00:00Z").unwrap());
        // Drop the oldest three hours of minutes, as pruning would.
        let cut = (t0 + 3 * MS_PER_HOUR) / MS_PER_MIN;
        a.mins.retain(|m, _| *m >= cut);
        let again = a.window(now, &[]);
        assert_eq!(again.start_ms, first.start_ms);
    }

    #[test]
    fn forecast_projects_empty_time_or_makes_it_to_reset() {
        let mut a = Agg::default();
        let per = weight("claude-opus-5-5", &Tokens { output: 1000, ..Default::default() }, false);
        // Window opens 10:00 UTC; cap reference worth 100 calls.
        a.cap_ref = Some(CapRef { used: 100.0 * per, hit_ms: 0, start_ms: 0 });
        let t0 = parse_ts_ms("2026-11-17T10:00:00Z").unwrap();
        // One call a minute for 60 minutes: 60 used, 1 per minute.
        for i in 0..60 {
            let ts = crate::timeutil::iso_ms(t0 + i * MS_PER_MIN);
            usage(&mut a, &ts, "claude-opus-5-5", 1000, &format!("f{i}"));
        }
        let now = t0 + 60 * MS_PER_MIN;
        let est = a.window(now, &[]);
        assert!((est.rate_per_min - per).abs() < 1e-6);
        // 40 calls left at 1 a minute: empty 40 minutes from now.
        assert_eq!(est.eta_ms, Some(now + 40 * MS_PER_MIN));

        // A big cap: the reset at 15:00 comes first.
        a.cap_ref = Some(CapRef { used: 10_000.0 * per, hit_ms: 0, start_ms: 0 });
        assert_eq!(a.window(now, &[]).eta_ms, None);

        // Idle for the last 30 minutes: no rate, no forecast.
        a.cap_ref = Some(CapRef { used: 100.0 * per, hit_ms: 0, start_ms: 0 });
        let later = now + 31 * MS_PER_MIN;
        let idle = a.window(later, &[]);
        assert_eq!(idle.rate_per_min, 0.0);
        assert_eq!(idle.eta_ms, None);

        // Already past the cap: empty now.
        a.cap_ref = Some(CapRef { used: 10.0 * per, hit_ms: 0, start_ms: 0 });
        assert_eq!(a.window(now, &[]).eta_ms, Some(now));
    }

    fn req(a: &mut Agg, session: &str, ts_ms: i64, model: &str, id: &str) -> bool {
        let t = Tokens { output: 10, cache_w5m: 80_000, ..Default::default() };
        a.add_usage(
            UsageIn { session, sub: true, observer: false, ts_ms, model, tokens: &t, fast: false, msg_id: Some(id), prev_ms: ts_ms - 20 * MS_PER_MIN, codex: false },
            OFF,
        )
    }

    #[test]
    fn days_four_to_seven_keep_totals_but_no_sessions() {
        let mut a = Agg::default();
        let now = now_ms();
        let today = day_of(now, OFF);
        for back in 0..7 {
            let ts = day_start_ms(today - back, OFF) + 4 * MS_PER_HOUR + 7 * MS_PER_MIN;
            assert!(req(&mut a, "s1", ts, "claude-opus-5-5", &format!("m{back}")));
            a.add_subagent_file(today - back, "s1", "Explore|claude-haiku-5-5", OFF, now);
        }
        for back in 0..7 {
            let d = &a.days[&(today - back)];
            // Totals, hourly cells and active time exist for every day.
            assert_eq!(d.models["claude-opus-5-5"].n, 1);
            assert!(d.hours[4][0] > 0.0 && d.hours_n[4] == 1 && d.active_slots() == 1);
            if back < 3 {
                assert_eq!(d.sessions["s1"].cold.n, 1, "day {back}");
                assert_eq!(d.subs.len(), 1);
            } else {
                assert!(d.sessions.is_empty() && d.subs.is_empty(), "day {back}");
            }
        }
        // Dropping detail never changes what the week says.
        let w = a.week(today);
        assert_eq!(w.len(), 7);
        assert!(w.iter().all(|d| d.total.n == 1 && d.active_min == 5 && d.hours[4] > 0.0));
    }

    #[test]
    fn a_day_is_compacted_when_it_ages_past_day_three() {
        let mut a = Agg::default();
        let now = now_ms();
        let today = day_of(now, OFF);
        let ts = day_start_ms(today, OFF) + 2 * MS_PER_HOUR;
        req(&mut a, "s1", ts, "claude-sonnet-5-5", "x");
        req(&mut a, "s2", ts + MS_PER_MIN, "claude-haiku-5-5", "y");
        let before = a.days[&today].total();
        assert_eq!(a.days[&today].sessions.len(), 2);
        // Two days later it is day 3: still detailed. Three days later: compact.
        a.prune(today - 4, now + 2 * 24 * MS_PER_HOUR, OFF);
        assert_eq!(a.days[&today].sessions.len(), 2);
        a.prune(today - 3, now + 3 * 24 * MS_PER_HOUR, OFF);
        let d = &a.days[&today];
        assert!(d.sessions.is_empty() && d.subs.is_empty());
        assert_eq!(d.total(), before);
        assert_eq!(d.models.len(), 2);
        assert!(d.hours[2][1] > 0.0 && d.hours[2][2] > 0.0);
        assert_eq!(d.active_slots(), 1);
        assert!(a.dirty);
    }

    #[test]
    fn seven_day_prune_keeps_today_and_six_back() {
        let mut a = Agg::default();
        let now = now_ms();
        let today = day_of(now, OFF);
        for back in 0..9 {
            let ts = day_start_ms(today - back, OFF) + 5 * MS_PER_HOUR;
            req(&mut a, "s1", ts, "claude-opus-5-5", &format!("p{back}"));
        }
        let h_old = day_start_ms(today - 8, OFF);
        let h_ok = day_start_ms(today - 6, OFF) + MS_PER_HOUR;
        a.hits = vec![h_old, h_ok];
        a.week_hits = vec![h_old, h_ok];
        a.prune(today - (WEEK_DAYS - 1), now, OFF);
        assert_eq!(a.days.len(), 7);
        assert!(a.days.contains_key(&(today - 6)) && !a.days.contains_key(&(today - 7)));
        assert_eq!(a.seen.len(), 7);
        assert_eq!(a.hits, vec![h_ok]);
        assert_eq!(a.week_hits, vec![h_ok]);
    }

    #[test]
    fn four_extra_busy_days_add_little_to_the_cache() {
        let busy = |days: i32| {
            let mut a = Agg::default();
            let now = now_ms();
            let today = day_of(now, OFF);
            // Days of 3,000 requests each, over 12 sessions a day.
            for back in 0..days {
                for i in 0..3000i64 {
                    let ts = day_start_ms(today - back, OFF) + (i % 1400) * MS_PER_MIN;
                    req(&mut a, &format!("sess{}", i % 12), ts, "claude-opus-5-5", &format!("d{back}-{i}"));
                }
            }
            a.prune(today - 6, now, OFF);
            (serde_json::to_string(&a).unwrap().len(), seen_codec_probe(&a).values().sum::<usize>(), a, today)
        };
        let (three, _, _, _) = busy(3);
        let (seven, seen, a, today) = busy(7);
        // Four compact days of 3,000 requests: 4-byte keys (x 4/3 in base64)
        // and almost nothing else.
        let extra = seven - three;
        assert!(extra < 75_000, "days 4 to 7 add {extra} bytes");
        assert!(extra > 4 * 3000 * 4 * 4 / 3, "sanity: {extra}");
        // With 8-byte keys they would have added about 130 KB for the keys alone.
        assert!(seen < 3 * 3000 * 11 + 4 * 3000 * 6, "dedupe sets are {seen} bytes");
        let compact = serde_json::to_string(&a.days[&(today - 5)]).unwrap().len();
        assert!(compact < 3_000, "a compacted day is {compact} bytes");
    }

    /// Just the dedupe set, as the cache writes it.
    fn seen_codec_probe(a: &Agg) -> BTreeMap<i32, usize> {
        let mut m = BTreeMap::new();
        let js = serde_json::to_value(a).unwrap();
        for (d, v) in js["seen"].as_object().unwrap() {
            m.insert(d.parse().unwrap(), v.as_str().unwrap().len());
        }
        m
    }

    #[test]
    fn compacted_days_dedupe_forked_copies_at_half_the_size() {
        let mut a = Agg::default();
        let now = now_ms();
        let today = day_of(now, OFF);
        let old = day_start_ms(today - 5, OFF) + 3 * MS_PER_HOUR;
        let recent = day_start_ms(today, OFF) + 3 * MS_PER_HOUR;
        assert!(req(&mut a, "s1", old, "claude-opus-5-5", "old-1"));
        assert!(req(&mut a, "s1", recent, "claude-opus-5-5", "new-1"));
        a.prune(today - 6, now, OFF);
        // A forked file replays the same ids: counted once, before and after a cache round trip.
        let js = serde_json::to_string(&a).unwrap();
        let mut b: Agg = serde_json::from_str(&js).unwrap();
        assert!(!req(&mut b, "s2", old, "claude-opus-5-5", "old-1"));
        assert!(!req(&mut b, "s2", recent, "claude-opus-5-5", "new-1"));
        assert!(req(&mut b, "s2", old, "claude-opus-5-5", "old-2"));
        assert_eq!(b.days[&(today - 5)].models["claude-opus-5-5"].n, 2);
        // The old day's key is 4 bytes wide on disk (4:xxxx), the recent one 8.
        let v: serde_json::Value = serde_json::from_str(&js).unwrap();
        let o = v["seen"][(today - 5).to_string()].as_str().unwrap();
        let r = v["seen"][today.to_string()].as_str().unwrap();
        assert!(o.starts_with("4:") && o.len() == 2 + 8 && r.len() == 12, "{o} {r}");
    }

    #[test]
    fn weekly_hits_dedupe_and_stay_apart_from_five_hour_hits() {
        let mut a = Agg::default();
        assert!(a.add_weekly_hit(1_000_000_000_000));
        assert!(!a.add_weekly_hit(1_000_000_000_000 + MS_PER_MIN));
        assert!(a.hits.is_empty());
    }

    #[test]
    fn seen_codec_round_trips() {
        let mut a = Agg::default();
        for i in 0..50 {
            usage(&mut a, "2026-11-17T10:00:00Z", "claude-opus-5-5", 1, &format!("id{i}"));
        }
        let js = serde_json::to_string(&a).unwrap();
        let b: Agg = serde_json::from_str(&js).unwrap();
        assert_eq!(a.seen, b.seen);
        assert_eq!(seen_codec::decode(&seen_codec::encode(b"ab")).unwrap(), b"ab");
        assert_eq!(seen_codec::decode(&seen_codec::encode(b"abcd")).unwrap(), b"abcd");
    }
}
