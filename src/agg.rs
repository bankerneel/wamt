//! Aggregates built from transcript lines: per-day totals, per-minute weights
//! for the last 26 hours, limit hits, and the 5-hour window estimate.
//! Only numbers, model ids and session ids are stored here.

use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};

use crate::timeutil::{day_of, day_start_ms, hour_of, MS_PER_HOUR, MS_PER_MIN};
use crate::transcript::{family, weight, Tokens};

pub const WINDOW_MS: i64 = 5 * MS_PER_HOUR;
/// Window starts snap to this grid (T3's recorded resets sit on 10-minute marks).
pub const WINDOW_GRID_MS: i64 = 10 * MS_PER_MIN;
const MINUTES_KEPT_MS: i64 = 26 * MS_PER_HOUR;
const HIT_DEDUPE_MS: i64 = 10 * MS_PER_MIN;

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

impl DayAgg {
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
    /// Limit hits (deduped), last 3 days.
    pub hits: Vec<i64>,
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
}

impl Agg {
    /// Adds one API response. Returns false when it was already counted.
    pub fn add_usage(&mut self, u: UsageIn, off: i32) -> bool {
        let day = day_of(u.ts_ms, off);
        if let Some(id) = u.msg_id {
            let h = fnv64(id);
            if self.seen.contains_key(&h) {
                return false;
            }
            self.seen.insert(h, day);
        }
        let w = weight(u.model, u.tokens, u.fast);
        let fam = family(u.model);
        let d = self.days.entry(day).or_default();
        d.models.entry(u.model.to_string()).or_default().add(u.tokens, w);
        let hr = hour_of(u.ts_ms, off);
        d.hours[hr][fam] += w;
        d.hours_n[hr] += 1;
        let slot = ((u.ts_ms - day_start_ms(day, off)) / (5 * MS_PER_MIN)).clamp(0, 287) as usize;
        d.active[slot / 64] |= 1u64 << (slot % 64);
        let s = d.sessions.entry(u.session.to_string()).or_default();
        s.models.entry(u.model.to_string()).or_default().add(u.tokens, w);
        if u.sub {
            s.sub.add(u.tokens, w);
        }
        if s.first == 0 || u.ts_ms < s.first {
            s.first = u.ts_ms;
        }
        s.last = s.last.max(u.ts_ms);
        let m = self.mins.entry(u.ts_ms.div_euclid(MS_PER_MIN)).or_default();
        m.w[fam] += w;
        m.n += 1;
        if u.observer {
            m.o += w;
        }
        self.dirty = true;
        true
    }

    pub fn add_subagent_file(&mut self, day: i32, parent: &str, label: &str) {
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

    /// Drops everything older than local day `min_day`, and minute cells
    /// older than 26 hours.
    pub fn prune(&mut self, min_day: i32, now_ms: i64, off: i32) {
        let before = (self.days.len(), self.mins.len(), self.seen.len(), self.hits.len());
        self.days.retain(|d, _| *d >= min_day);
        self.seen.retain(|_, d| *d >= min_day);
        let min_minute = (now_ms - MINUTES_KEPT_MS).div_euclid(MS_PER_MIN);
        self.mins.retain(|m, _| *m >= min_minute);
        let from = day_start_ms(min_day, off);
        self.hits.retain(|h| *h >= from);
        if before != (self.days.len(), self.mins.len(), self.seen.len(), self.hits.len()) {
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

    pub fn serialize<S: Serializer>(m: &HashMap<u64, i32>, s: S) -> Result<S::Ok, S::Error> {
        let mut by_day: BTreeMap<i32, Vec<u8>> = BTreeMap::new();
        for (h, d) in m {
            by_day.entry(*d).or_default().extend_from_slice(&h.to_le_bytes());
        }
        let enc: BTreeMap<i32, String> = by_day.into_iter().map(|(d, b)| (d, encode(&b))).collect();
        enc.serialize(s)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<HashMap<u64, i32>, D::Error> {
        let enc: BTreeMap<i32, String> = BTreeMap::deserialize(d)?;
        let mut m = HashMap::new();
        for (day, s) in enc {
            let bytes = decode(&s).ok_or_else(|| serde::de::Error::custom("bad seen encoding"))?;
            for c in bytes.chunks_exact(8) {
                m.insert(u64::from_le_bytes(c.try_into().unwrap()), day);
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
            },
            OFF,
        )
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
