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
use crate::timeutil::{day_of, day_start_ms, MS_PER_HOUR};

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
    Unavailable,
    Db,
}

/// The JSON body for one thread. Opens its own short-lived read-only
/// connection (dropped on return) so the T3 worker's connection and its
/// read transactions are never shared across threads.
pub fn build(shared: &Mutex<Shared>, db: &Path, id: &str, now: i64) -> Result<String, DrillErr> {
    let (off, meta, nsess, buckets) = {
        let sh = shared.lock().unwrap_or_else(|e| e.into_inner());
        let Some(t3) = sh.t3.as_ref() else { return Err(DrillErr::Unavailable) };
        let Some(m) = t3.threads.get(id) else { return Err(DrillErr::NotFound) };
        let today = day_of(now, sh.off);
        let sids: Vec<&String> = t3.sessions.iter().filter(|(_, th)| th.as_str() == id).map(|(s, _)| s).collect();
        let day = sh.tx.days.iter().find(|(d, _)| *d == today).map(|(_, a)| a);
        let sets = sids.iter().filter_map(|s| day.and_then(|a| a.sessions.get(*s))).map(|s| &s.buckets);
        (sh.off, m.clone(), sids.len(), merge_buckets(sets))
    };
    let start = day_start_ms(day_of(now, off), off);
    let detail = {
        let t3 = T3::open(db).map_err(|_| DrillErr::Db)?;
        t3.begin().map_err(|_| DrillErr::Db)?;
        let d = t3.thread_detail(id, start, start + 24 * MS_PER_HOUR);
        t3.end();
        d.map_err(|_| DrillErr::Db)?
    };
    let runs: Vec<_> = detail
        .runs
        .iter()
        .map(|r| {
            let begin = r.started.unwrap_or(r.requested);
            let ms = match (r.started, r.completed) {
                (Some(a), Some(b)) if b > a => Some(b - a),
                (Some(a), None) if matches!(r.status.as_str(), "running" | "waiting") => Some((now - a).max(0)),
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
        "title": meta.title,
        "project": meta.project,
        "model": meta.model,
        "now": now,
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
