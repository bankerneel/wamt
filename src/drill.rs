//! The thread drill-down behind `GET /api/thread?id=`. Built on demand, never
//! pushed. Numbers only: runs, tool and item counts by name, subagents, and
//! the 10-minute token buckets the aggregator keeps for today. A thread's
//! inputs, outputs, commands, paths and message text are never read.

use serde_json::json;
use std::path::Path;
use std::sync::Mutex;

use crate::agg::{Bucket, BUCKET_MS};
use crate::payload::Shared;
use crate::t3::T3;
use crate::timeutil::{civil_from_days, day_label, day_of, day_start_ms, days_from_civil, MS_PER_HOUR};

const MAX_ID: usize = 160;

/// T3 thread ids are plain ids ("mcp:<uuid>", "thread:subagent:<hex>") and,
/// for delegated tasks, carry percent-escapes ("command%3Amcp%3A..."). Allow
/// exactly those characters.
pub fn valid_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= MAX_ID && id.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | ':' | '%' | '.'))
}

/// Decodes `%XX` escapes of one query value (`+` stays a plus). Bad escapes
/// or non-UTF-8 results give None.
pub fn pct_decode(s: &str) -> Option<String> {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' {
            let h = std::str::from_utf8(b.get(i + 1..i + 3)?).ok()?;
            out.push(u8::from_str_radix(h, 16).ok()?);
            i += 3;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

/// The decoded, validated `id` of a query string, if there is exactly a good one.
pub fn id_from_query(q: &str) -> Option<String> {
    let raw = q.split('&').find_map(|kv| kv.strip_prefix("id="))?;
    let id = pct_decode(raw)?;
    valid_id(&id).then_some(id)
}

/// Parses a strict `YYYY-MM-DD` local calendar date into a day number.
/// Rejects short or padded fields, separators other than dashes and dates
/// that do not exist (including 29 February in non-leap years).
pub fn parse_day(s: &str) -> Option<i32> {
    let b = s.as_bytes();
    if b.len() != 10 || b[4] != b'-' || b[7] != b'-' {
        return None;
    }
    let num = |a: usize, z: usize| -> Option<i64> {
        if !b[a..z].iter().all(u8::is_ascii_digit) {
            return None;
        }
        s[a..z].parse().ok()
    };
    let (y, m, d) = (num(0, 4)?, num(5, 7)?, num(8, 10)?);
    let day = days_from_civil(y, m, d);
    let (y2, m2, d2) = civil_from_days(day);
    if (y2 as i64, m2 as i64, d2 as i64) != (y, m, d) {
        return None;
    }
    Some(day as i32)
}

/// Only the last seven local days (today and the six before it) can be
/// drilled into. Anything older, or in the future, is refused.
pub fn day_in_range(day: i32, now: i64, off: i32) -> bool {
    let today = day_of(now, off);
    (today - 6..=today).contains(&day)
}

/// The optional `day` of a query string: `Ok(None)` when absent,
/// `Ok(Some(day))` when strictly formatted, `Err(())` when malformed.
/// The range against "now" is checked later, where the offset is known.
pub fn day_from_query(q: &str) -> Result<Option<i32>, ()> {
    let Some(raw) = q.split('&').find_map(|kv| kv.strip_prefix("day=")) else {
        return Ok(None);
    };
    parse_day(&pct_decode(raw).ok_or(())?).map(Some).ok_or(())
}

/// Merges the buckets of several sessions into one timeline.
pub fn merge_buckets<'a>(sets: impl Iterator<Item = &'a std::collections::BTreeMap<u16, Bucket>>) -> std::collections::BTreeMap<u16, Bucket> {
    let mut out: std::collections::BTreeMap<u16, Bucket> = Default::default();
    for set in sets {
        for (slot, b) in set {
            let e = out.entry(*slot).or_default();
            for i in 0..4 {
                e.w[i] += b.w[i];
            }
            e.out += b.out;
        }
    }
    out
}

pub enum DrillErr {
    NotFound,
    BadDay,
    Unavailable,
    Db,
}

/// The JSON body for one thread, optionally for a past local day. Opens its
/// own short-lived read-only connection (dropped on return) so the T3
/// worker's connection and its read transactions are never shared across
/// threads. The shared state is locked only while copying in-memory numbers;
/// every SQLite read happens after the lock is released.
pub fn build(shared: &Mutex<Shared>, db: &Path, id: &str, now: i64, day: Option<i32>) -> Result<String, DrillErr> {
    let (off, live_meta, nsess, buckets, target, today) = {
        let sh = shared.lock().unwrap_or_else(|e| e.into_inner());
        let today = day_of(now, sh.off);
        let target = day.unwrap_or(today);
        if !day_in_range(target, now, sh.off) {
            return Err(DrillErr::BadDay);
        }
        let Some(t3) = sh.t3.as_ref() else { return Err(DrillErr::Unavailable) };
        let sids: Vec<&String> = t3.sessions.iter().filter(|(_, th)| th.as_str() == id).map(|(s, _)| s).collect();
        // Buckets are kept for today only; merge them while the borrowed
        // day aggregate is still alive, into an owned map.
        let buckets = if target == today {
            let day = sh.tx.days.iter().find(|(d, _)| *d == today).map(|(_, a)| a);
            let sets = sids.iter().filter_map(|s| day.and_then(|a| a.sessions.get(*s))).map(|s| &s.buckets);
            merge_buckets(sets)
        } else {
            Default::default()
        };
        (sh.off, t3.threads.get(id).cloned(), sids.len(), buckets, target, today)
    };
    let start = day_start_ms(target, off);
    let (detail, db_meta) = {
        let t3 = T3::open(db).map_err(|_| DrillErr::Db)?;
        t3.begin().map_err(|_| DrillErr::Db)?;
        // Metadata is read from SQLite, not from the live board: a thread
        // idle for hours may no longer be listed there.
        let meta = t3.thread_meta(id).map_err(|_| DrillErr::Db)?;
        let d = t3.thread_detail(id, start, start + 24 * MS_PER_HOUR);
        t3.end();
        (d.map_err(|_| DrillErr::Db)?, meta)
    };
    if db_meta.is_none() && live_meta.is_none() && detail.runs.is_empty() && detail.tools.is_empty() && detail.items.is_empty() && detail.subs.is_empty() {
        return Err(DrillErr::NotFound);
    }
    // Prefer the database row; fall back to the live board, then to the id.
    let (title, project, model) = match db_meta {
        Some((title, project, model)) => (title, project, model),
        None => match live_meta {
            Some(m) => (m.title, m.project, m.model),
            None => (id.to_string(), None, None),
        },
    };
    let runs: Vec<_> = detail
        .runs
        .iter()
        .map(|r| {
            let begin = r.started.unwrap_or(r.requested);
            let ms = match (r.started, r.completed) {
                (Some(a), Some(b)) if b > a => Some(b - a),
                (Some(a), None) if matches!(r.status.as_str(), "running" | "waiting") && target == today => Some((now - a).max(0)),
                _ => None,
            };
            json!({"n": r.n, "status": r.status, "at": begin, "ms": ms, "model": r.model, "delegated": r.delegated})
        })
        .collect();
    let subs: Vec<_> = detail
        .subs
        .iter()
        .map(|s| {
            let ms = match (s.started, s.completed) {
                (Some(a), Some(b)) if b > a => Some(b - a),
                _ => None,
            };
            json!({"title": s.title, "model": s.model, "status": s.status, "origin": s.origin, "at": s.started.unwrap_or(s.updated), "ms": ms})
        })
        .collect();
    let tl: Vec<_> = buckets.iter().map(|(slot, b)| json!({"t": start + *slot as i64 * BUCKET_MS, "w": b.w, "out": b.out})).collect();
    let body = json!({
        "id": id,
        "title": title,
        "project": project,
        "model": model,
        "now": now,
        "day": day_label(target),
        "today": day_label(today),
        "is_today": target == today,
        "day_start": start,
        "runs": runs,
        "tools": detail.tools.iter().map(|(n, c)| json!({"name": n, "n": c})).collect::<Vec<_>>(),
        "items": detail.items.iter().map(|(n, c)| json!({"type": n, "n": c})).collect::<Vec<_>>(),
        "subagents": subs,
        "timeline": {"bucket_ms": BUCKET_MS, "sessions": nsess, "buckets": tl},
    });
    Ok(body.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::timeutil::parse_ts_ms;

    #[test]
    fn days_are_parsed_strictly() {
        assert_eq!(parse_day("2026-11-18"), Some(days_from_civil(2026, 11, 18) as i32));
        assert_eq!(parse_day("2024-02-29"), Some(days_from_civil(2024, 2, 29) as i32));
        for bad in [
            "", "2026-11-18T00", "2026-1-18", "2026-11-8", "2026-13-01", "2026-00-10",
            "2026-11-00", "2026-02-30", "2023-02-29", "2026/11/18", "2026-11-18 ",
            " 2026-11-18", "abcdefghij", "2026-11-1x",
        ] {
            assert_eq!(parse_day(bad), None, "{bad}");
        }
    }

    #[test]
    fn day_query_is_decoded_then_range_checked() {
        let day = parse_day("2026-11-18").unwrap();
        assert_eq!(day_from_query("day=2026-11-18"), Ok(Some(day)));
        assert_eq!(day_from_query("id=mcp%3Aabc&day=2026%2D11%2D18"), Ok(Some(day)));
        assert_eq!(day_from_query("id=mcp:abc"), Ok(None));
        assert_eq!(day_from_query(""), Ok(None));
        for bad in ["day=", "day=2026-13-01", "day=2026-11-18x", "day=%zz", "day=2026%2F11%2F18"] {
            assert!(day_from_query(bad).is_err(), "{bad}");
        }
        let now = parse_ts_ms("2026-11-18T12:00:00Z").unwrap();
        assert!(day_in_range(day, now, 0));
        assert!(day_in_range(parse_day("2026-11-12").unwrap(), now, 0));
        assert!(!day_in_range(parse_day("2026-11-11").unwrap(), now, 0));
        assert!(!day_in_range(parse_day("2026-11-19").unwrap(), now, 0));
        // An offset shifts which local day an instant belongs to.
        let later = parse_ts_ms("2026-11-18T20:00:00Z").unwrap();
        assert!(day_in_range(parse_day("2026-11-19").unwrap(), later, 330));
        assert!(!day_in_range(parse_day("2026-11-12").unwrap(), later, 330));
    }

    #[test]
    fn ids_are_validated_strictly() {
        assert!(valid_id("mcp:00000000-0000-4000-8000-0000000000a1"));
        assert!(valid_id("thread:subagent:0000000000000b01"));
        assert!(valid_id("thread:delegated-task:command%3Amcp%3Aa0b6%3Adelegate-task%3Ae9f7"));
        for bad in ["", "a b", "a/b", "../x", "a'b", "a\"b", "a;b", "a<b", "a\nb", "a\\b", "é", &"a".repeat(161)] {
            assert!(!valid_id(bad), "{bad:?}");
        }
    }

    #[test]
    fn query_id_is_decoded_then_checked() {
        assert_eq!(id_from_query("id=mcp:abc-1").as_deref(), Some("mcp:abc-1"));
        assert_eq!(id_from_query("x=1&id=mcp%3Aabc").as_deref(), Some("mcp:abc"));
        // A literal percent escape inside an id arrives double-encoded.
        assert_eq!(id_from_query("id=command%253Amcp").as_deref(), Some("command%3Amcp"));
        assert_eq!(id_from_query("id=a%2Fb"), None);
        assert_eq!(id_from_query("id=a%20b"), None);
        assert_eq!(id_from_query("id=a%zz"), None);
        assert_eq!(id_from_query("id=a%"), None);
        assert_eq!(id_from_query("id=%00"), None);
        assert_eq!(id_from_query("id="), None);
        assert_eq!(id_from_query("other=1"), None);
    }

    #[test]
    fn buckets_merge_across_sessions() {
        let mut a = std::collections::BTreeMap::new();
        a.insert(3u16, Bucket { w: [1.0, 0.0, 0.0, 0.0], out: 5 });
        let mut b = std::collections::BTreeMap::new();
        b.insert(3u16, Bucket { w: [0.5, 2.0, 0.0, 0.0], out: 1 });
        b.insert(4u16, Bucket { w: [0.0, 0.0, 0.0, 1.0], out: 2 });
        let m = merge_buckets([&a, &b].into_iter());
        assert_eq!(m.len(), 2);
        assert_eq!(m[&3], Bucket { w: [1.5, 2.0, 0.0, 0.0], out: 6 });
        assert_eq!(m[&4].out, 2);
    }
}
