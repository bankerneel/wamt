//! `GET /api/timeline?day=YYYY-MM-DD`, built on demand like the thread
//! drill-down. One lane per T3 thread that ran that day (with its lineage
//! parent for nesting) and one per Claude Code session outside T3. Bars for
//! runs, hatched waits, 10-minute ticks or a first-to-last span for outside
//! sessions, a 00:00 to 24:00 axis, a now line and a per-10-minute lane count.
//! Only ids, titles, statuses, models and times ever reach the response.

use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::Path;
use std::sync::Mutex;

use crate::agg::BUCKET_MS;
use crate::drill::{day_from_query, day_in_range};
use crate::payload::Shared;
use crate::t3::T3;
use crate::timeutil::{day_label, day_of, day_start_ms, MS_PER_DAY};
use crate::transcript::family;

/// At most this many lanes are sent. The page is told how many were left out.
pub const MAX_LANES: usize = 40;
const SLOTS: usize = (MS_PER_DAY / BUCKET_MS) as usize;

#[derive(Debug)]
pub enum TimelineErr {
    BadDay,
    Db,
}

/// The `day` of a timeline query is required and strictly validated. Parsing
/// and the seven-day range check are shared with the thread drill-down.
pub fn day_from_timeline_query(q: &str) -> Result<i32, ()> {
    match day_from_query(q) {
        Ok(Some(day)) => Ok(day),
        Ok(None) | Err(()) => Err(()),
    }
}

struct OutRun {
    n: i64,
    status: String,
    at: i64,
    to: i64,
    model: Option<String>,
    fam: usize,
}

struct OutWait {
    at: i64,
    to: i64,
    kind: &'static str,
}

struct OutLane {
    id: String,
    title: String,
    parent: Option<String>,
    model: Option<String>,
    kind: &'static str,
    fam: usize,
    runs: Vec<OutRun>,
    waits: Vec<OutWait>,
    ticks: Vec<u16>,
    span: [i64; 2],
    score: i64,
}

/// Marks every 10-minute slot a time span touches, once per lane.
fn mark(occ: &mut [bool; SLOTS], day_start: i64, a: i64, b: i64) {
    let lo = (a - day_start).max(0).min(MS_PER_DAY - 1);
    let hi = (b - day_start).max(0).min(MS_PER_DAY - 1);
    if hi < lo {
        return;
    }
    for i in (lo / BUCKET_MS) as usize..=(hi / BUCKET_MS) as usize {
        occ[i] = true;
    }
}

/// Builds the compact JSON body for one local day. Shared state is locked
/// only while small metadata is copied; every SQLite read happens afterwards
/// on a fresh read-only connection, so no lock is held during a read.
pub fn build(shared: &Mutex<Shared>, db: &Path, now: i64, target: i32) -> Result<String, TimelineErr> {
    // Copy the in-memory metadata we need, then drop the lock before any
    // SQLite work. Titles, models and session id mappings are all safe to
    // copy; no transcript text is read here.
    let (off, today, launchers, outside, sessions_available) = {
        let sh = shared.lock().unwrap_or_else(|e| e.into_inner());
        let today = day_of(now, sh.off);
        if !day_in_range(target, now, sh.off) {
            return Err(TimelineErr::BadDay);
        }
        let (session_to_thread, launchers) = match sh.t3.as_ref() {
            Some(t3) => (
                (*t3.sessions).clone(),
                t3.threads
                    .iter()
                    .filter_map(|(id, m)| m.launcher.as_ref().map(|l| (id.clone(), l.clone())))
                    .collect::<HashMap<_, _>>(),
            ),
            None => (HashMap::new(), HashMap::new()),
        };
        let infos = (*sh.tx.sessions).clone();
        let mut outside: Vec<(i64, OutLane)> = Vec::new();
        let mut sessions_available = false;
        if let Some((_, agg)) = sh.tx.days.iter().find(|(d, _)| *d == target) {
            sessions_available = !agg.is_compact();
            for (sid, sd) in &agg.sessions {
                // A session mapped to a T3 thread already has its own lane.
                if session_to_thread.contains_key(sid) {
                    continue;
                }
                let info = infos.get(sid);
                let title = info.and_then(|i| i.title.clone()).unwrap_or_else(|| sid.clone());
                let model = info.and_then(|i| i.model.clone());
                let ticks: Vec<u16> = if target == today {
                    sd.buckets
                        .iter()
                        .filter(|(_, b)| b.out > 0 || b.w.iter().any(|w| *w > 0.0))
                        .map(|(slot, _)| *slot)
                        .collect()
                } else {
                    Vec::new()
                };
                let score = if target == today {
                    ticks.len() as i64 * BUCKET_MS
                } else {
                    (sd.last - sd.first).max(0)
                };
                let lane = OutLane {
                    id: sid.clone(),
                    title,
                    parent: None,
                    model: model.clone(),
                    kind: "session",
                    fam: family(model.as_deref().unwrap_or("")),
                    runs: Vec::new(),
                    waits: Vec::new(),
                    ticks,
                    span: [sd.first, sd.last],
                    score,
                };
                outside.push((score, lane));
            }
        }
        (sh.off, today, launchers, outside, sessions_available)
    };

    // SQLite reads, after the lock is gone. A large limit keeps ranking and
    // the 40-lane cap in one place, across T3 and outside lanes alike.
    let start = day_start_ms(target, off);
    let end = start + MS_PER_DAY;
    let t3_lanes = {
        let t3 = T3::open(db).map_err(|_| TimelineErr::Db)?;
        t3.begin().map_err(|_| TimelineErr::Db)?;
        let lanes = t3.timeline_lanes(start, end, now, usize::MAX);
        t3.end();
        lanes.map_err(|_| TimelineErr::Db)?
    };

    let mut ranked: Vec<(i64, OutLane)> = Vec::with_capacity(t3_lanes.len() + outside.len());
    for l in t3_lanes {
        let parent = l.parent.clone().or_else(|| launchers.get(&l.thread).cloned());
        let fam = family(l.model.as_deref().unwrap_or(""));
        ranked.push((
            l.score,
            OutLane {
                id: l.thread,
                title: l.title,
                parent,
                model: l.model,
                kind: "t3",
                fam,
                runs: l
                    .runs
                    .into_iter()
                    .map(|r| OutRun {
                        n: r.n,
                        status: r.status,
                        at: r.at,
                        to: r.to,
                        fam: family(r.model.as_deref().unwrap_or("")),
                        model: r.model,
                    })
                    .collect(),
                waits: l
                    .waits
                    .into_iter()
                    .map(|w| OutWait { at: w.at, to: w.to, kind: w.kind })
                    .collect(),
                ticks: Vec::new(),
                span: [0, 0],
                score: l.score,
            },
        ));
    }
    ranked.extend(outside);
    let total = ranked.len();
    ranked.sort_by(|a, b| b.0.cmp(&a.0).then(b.1.id.cmp(&a.1.id)));
    ranked.truncate(MAX_LANES);
    let omitted = total.saturating_sub(ranked.len());
    let wait_available = ranked.iter().any(|(_, l)| !l.waits.is_empty());

    // One lane contributes at most one count per 10-minute slot.
    let mut peak = vec![0u16; SLOTS];
    for (_, lane) in &ranked {
        let mut occ = [false; SLOTS];
        if lane.kind == "t3" {
            for r in &lane.runs {
                mark(&mut occ, start, r.at, r.to);
            }
            for w in &lane.waits {
                mark(&mut occ, start, w.at, w.to);
            }
        } else if target == today {
            for slot in &lane.ticks {
                let i = (*slot as usize).min(SLOTS - 1);
                occ[i] = true;
            }
        } else {
            mark(&mut occ, start, lane.span[0], lane.span[1]);
        }
        for i in 0..SLOTS {
            if occ[i] {
                peak[i] += 1;
            }
        }
    }

    let lanes: Vec<Value> = ranked
        .into_iter()
        .map(|(_, l)| {
            let mut v = json!({
                "id": l.id,
                "title": l.title,
                "parent": l.parent,
                "model": l.model,
                "kind": l.kind,
                "fam": l.fam,
                "score": l.score,
            });
            let obj = v.as_object_mut().unwrap();
            obj.insert(
                "runs".into(),
                json!(l.runs
                    .iter()
                    .map(|r| json!({"n": r.n, "status": r.status, "at": r.at, "to": r.to, "model": r.model, "fam": r.fam}))
                    .collect::<Vec<_>>()),
            );
            obj.insert(
                "waits".into(),
                json!(l.waits.iter().map(|w| json!({"at": w.at, "to": w.to, "kind": w.kind})).collect::<Vec<_>>()),
            );
            if l.kind == "session" {
                obj.insert("ticks".into(), json!(l.ticks));
                obj.insert("span".into(), json!(l.span));
            }
            v
        })
        .collect();

    let body = json!({
        "day": day_label(target),
        "today": day_label(today),
        "is_today": target == today,
        "day_start": start,
        "day_end": end,
        "now": now,
        "off": off,
        "bucket_ms": BUCKET_MS,
        "slots": SLOTS,
        "cap": MAX_LANES,
        "omitted": omitted,
        "wait_available": wait_available,
        "sessions_available": sessions_available,
        "peak": peak,
        "peak_max": peak.iter().copied().max().unwrap_or(0),
        "lanes": lanes,
    });
    Ok(body.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::payload::Shared;
    use crate::timeutil::parse_ts_ms;

    #[test]
    fn day_is_required_and_reuses_drill_validation() {
        assert_eq!(day_from_timeline_query("day=2026-11-18"), Ok(parse_day_num("2026-11-18")));
        assert_eq!(day_from_timeline_query("day=2026%2D11%2D18"), Ok(parse_day_num("2026-11-18")));
        for bad in ["", "x=1", "day=", "day=2026-13-01", "day=2026-11-18x", "day=2026%2F11%2F18", "day=%zz"] {
            assert!(day_from_timeline_query(bad).is_err(), "{bad}");
        }
    }

    fn parse_day_num(s: &str) -> i32 {
        crate::drill::parse_day(s).unwrap()
    }

    #[test]
    fn peak_marks_each_slot_once_per_lane() {
        let day = parse_day_num("2026-11-17");
        let start = day_start_ms(day, 0);
        let mut occ = [false; SLOTS];
        // Two runs in the same slot and a wait crossing into the next one.
        mark(&mut occ, start, start, start + 60_000);
        mark(&mut occ, start, start + 120_000, start + 180_000);
        mark(&mut occ, start, start + BUCKET_MS - 1, start + BUCKET_MS + 60_000);
        assert_eq!(occ.iter().filter(|v| **v).count(), 2);
    }

    #[test]
    fn build_refuses_an_out_of_range_day() {
        let shared = Mutex::new(Shared::new(0));
        let now = parse_ts_ms("2026-11-18T12:00:00Z").unwrap();
        let old = parse_day_num("2026-11-01");
        assert!(matches!(build(&shared, Path::new("/nonexistent/x.sqlite"), now, old), Err(TimelineErr::BadDay)));
    }
}
