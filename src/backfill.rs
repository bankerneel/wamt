//! One-time backfill of daily totals from before the seven-day window.
//!
//! Runs on its own low-priority thread after the startup scan. It reads only
//! transcript files whose mtime is older than the window and newer than 31
//! days, counts only lines whose timestamp lands on a day still missing from
//! the month, and dedupes by message id in a set that is dropped at the end.
//! No message content is kept. Reads use F_NOCACHE so the month never pushes
//! the owner's apps out of memory.

use std::collections::{BTreeMap, HashSet};
use std::fs::{self, File};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::Instant;

use crate::agg::{fnv64, WEEK_DAYS};
use crate::codex::{self, CLine};
use crate::history::{HistDay, BACKFILL_NEWEST, BACKFILL_OLDEST};
use crate::tail::{drop_cache, mtime_ms, no_cache, CHUNK, MAX_LINE};
use crate::timeutil::{civil_from_days, day_of, day_start_ms, MS_PER_MIN};
use crate::transcript::{family, parse_line, weight, LineKind, Tokens};

pub struct BackfillResult {
    pub days: Vec<HistDay>,
    pub bytes: u64,
    pub ms: u128,
}

/// Nice the calling thread so the backfill never competes with live data.
pub fn set_low_priority() {
    #[cfg(unix)]
    {
        // SAFETY: setpriority with PRIO_PROCESS 0 changes this thread only.
        unsafe {
            libc::setpriority(libc::PRIO_PROCESS, 0, 10);
        }
    }
}

/// Days to look for: today - 30 up to today - 8. Today - 7 is moved in by
/// the day roll, so it is never read here.
pub fn missing_days(today: i32, have: &HashSet<i32>) -> Vec<i32> {
    ((today - BACKFILL_OLDEST)..=(today - BACKFILL_NEWEST)).filter(|d| !have.contains(d)).collect()
}

/// Scratch totals for one day. The bitset counts five-minute slots and is
/// turned into `active_min` when the day is finished.
#[derive(Default)]
struct Acc {
    t: [u64; 5],
    n: u64,
    w: f64,
    fam: [f64; 4],
    active: [u64; 5],
}

impl Acc {
    fn add(&mut self, tok: &Tokens, model: &str, fast: bool, ts_ms: i64, day: i32, off: i32) {
        self.t[0] += tok.input;
        self.t[1] += tok.output;
        self.t[2] += tok.cache_read;
        self.t[3] += tok.cache_w5m;
        self.t[4] += tok.cache_w1h;
        self.n += 1;
        let w = weight(model, tok, fast);
        self.w += w;
        self.fam[family(model)] += w;
        let slot = ((ts_ms - day_start_ms(day, off)) / (5 * MS_PER_MIN)).clamp(0, 287) as usize;
        self.active[slot / 64] |= 1u64 << (slot % 64);
    }

    fn into_day(self, day: i32, codex_tokens: u64) -> HistDay {
        HistDay {
            day,
            t: self.t,
            n: self.n,
            w: self.w,
            fam: self.fam,
            codex_tokens,
            active_min: self.active.iter().map(|w| w.count_ones()).sum::<u32>() * 5,
        }
    }
}

/// Reads whole finished files without caching them, handing every complete
/// line to `on_line`. A line split across chunks is kept for the next read.
fn read_all(path: &Path, bytes: &mut u64, mut on_line: impl FnMut(&[u8])) {
    let Ok(f) = File::open(path) else { return };
    no_cache(&f);
    let mut f = f;
    let mut buf: Vec<u8> = Vec::with_capacity(CHUNK);
    loop {
        let at = buf.len();
        buf.resize(at + CHUNK, 0);
        let n = match f.read(&mut buf[at..]) {
            Ok(n) => n,
            Err(_) => break,
        };
        buf.truncate(at + n);
        if n == 0 {
            break;
        }
        *bytes += n as u64;
        let data = std::mem::take(&mut buf);
        let mut pos = 0;
        for nl in memchr::memchr_iter(b'\n', &data) {
            if nl > pos {
                on_line(&data[pos..nl]);
            }
            pos = nl + 1;
        }
        buf = data;
        buf.drain(..pos);
        if buf.len() > MAX_LINE {
            buf.clear();
        }
    }
    drop_cache(&f);
}

fn list_jsonl(dir: &Path, out: &mut Vec<PathBuf>, depth: usize) {
    if depth > 6 {
        return;
    }
    let Ok(rd) = fs::read_dir(dir) else { return };
    for e in rd.flatten() {
        let Ok(ft) = e.file_type() else { continue };
        let p = e.path();
        if ft.is_dir() {
            list_jsonl(&p, out, depth + 1);
        } else if ft.is_file() && p.extension().is_some_and(|x| x == "jsonl") {
            out.push(p);
        }
    }
}

fn list_rollouts(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(rd) = fs::read_dir(dir) else { return };
    for e in rd.flatten() {
        let name = e.file_name();
        let name = name.to_string_lossy();
        if name.starts_with("rollout-") && name.ends_with(".jsonl") {
            out.push(e.path());
        }
    }
}

fn in_file_range(mtime: i64, from: i64, to: i64) -> bool {
    mtime >= from && mtime < to
}

/// Fills every missing day. `codex_root` is optional; when present, its day
/// folders and the sibling archived sessions are read the same way.
pub fn backfill(root: &Path, codex_root: Option<&Path>, off: i32, today: i32, have: &HashSet<i32>) -> BackfillResult {
    let t0 = Instant::now();
    let mut out = BackfillResult { days: Vec::new(), bytes: 0, ms: 0 };
    let missing = missing_days(today, have);
    if missing.is_empty() {
        out.ms = t0.elapsed().as_millis();
        return out;
    }
    let want: HashSet<i32> = missing.iter().copied().collect();
    let from = day_start_ms(today - BACKFILL_OLDEST, off);
    let to = day_start_ms(today - (WEEK_DAYS - 1), off);

    let mut acc: BTreeMap<i32, Acc> = BTreeMap::new();
    let mut seen: HashSet<u64> = HashSet::new();
    let mut paths = Vec::new();
    list_jsonl(root, &mut paths, 0);
    for path in paths {
        let Ok(meta) = fs::metadata(&path) else { continue };
        if !in_file_range(mtime_ms(&meta), from, to) {
            continue;
        }
        read_all(&path, &mut out.bytes, |line| {
            let Some(l) = parse_line(line, false) else { return };
            if l.kind != LineKind::Usage {
                return;
            }
            let day = day_of(l.ts_ms, off);
            if !want.contains(&day) {
                return;
            }
            let Some(id) = l.msg_id.as_deref() else { return };
            if !seen.insert(fnv64(id)) {
                return;
            }
            let model = l.model.as_deref().unwrap_or("unknown");
            acc.entry(day).or_default().add(&l.tokens, model, l.fast, l.ts_ms, day, off);
        });
    }
    // The temporary dedupe set goes out of scope here, before Codex is read.
    drop(seen);

    let mut cx_totals: BTreeMap<i32, u64> = BTreeMap::new();
    if let Some(cr) = codex_root {
        let base = cr.parent().unwrap_or(cr);
        let mut cx_paths = Vec::new();
        for &day in &missing {
            let (y, m, d) = civil_from_days(day as i64);
            list_rollouts(&cr.join(format!("{y:04}/{m:02}/{d:02}")), &mut cx_paths);
        }
        list_rollouts(&base.join("archived_sessions"), &mut cx_paths);
        let mut cx_seen: HashSet<u64> = HashSet::new();
        for path in cx_paths {
            let Ok(meta) = fs::metadata(&path) else { continue };
            if !in_file_range(mtime_ms(&meta), from, to) {
                continue;
            }
            let tag = fnv64(&path.to_string_lossy());
            read_all(&path, &mut out.bytes, |line| {
                let Some(CLine::Usage { ts_ms, response_id, tokens }) = codex::parse_line(line) else { return };
                let day = day_of(ts_ms, off);
                if !want.contains(&day) {
                    return;
                }
                let key = match response_id {
                    Some(id) => fnv64(&id),
                    None => fnv64(&format!("{}|{}|{}", tag, ts_ms, tokens.total())),
                };
                if !cx_seen.insert(key) {
                    return;
                }
                *cx_totals.entry(day).or_default() += tokens.total();
            });
        }
    }

    for (day, a) in acc {
        out.days.push(a.into_day(day, cx_totals.get(&day).copied().unwrap_or(0)));
    }
    for (day, cx) in cx_totals {
        if !out.days.iter().any(|d| d.day == day) {
            out.days.push(HistDay { day, codex_tokens: cx, ..Default::default() });
        }
    }
    out.days.sort_by_key(|d| d.day);
    out.ms = t0.elapsed().as_millis();
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::timeutil::{days_from_civil, parse_ts_ms, MS_PER_HOUR};
    use std::io::Write;

    const OFF: i32 = 330;
    // 17 Nov 2026, local day number 20774.
    const TODAY: i32 = 20_774;

    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("sb-backfill-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(d.join("-Users-x-projects-demo")).unwrap();
        d
    }

    fn line(id: &str, ts: &str, out: u64) -> String {
        format!(
            r#"{{"type":"assistant","timestamp":"{ts}","message":{{"id":"{id}","model":"claude-opus-5-5","content":[{{"type":"text","text":"x"}}],"usage":{{"input_tokens":1,"output_tokens":{out}}}}}}}"#
        ) + "\n"
    }

    fn set_mtime(p: &Path, ms: i64) {
        let when = std::time::UNIX_EPOCH + std::time::Duration::from_millis(ms as u64);
        File::options().write(true).open(p).unwrap().set_modified(when).unwrap();
    }

    fn append(p: &Path, s: &str) {
        let mut f = fs::OpenOptions::new().create(true).append(true).open(p).unwrap();
        f.write_all(s.as_bytes()).unwrap();
    }

    #[test]
    fn backfill_counts_only_missing_days_and_dedupes() {
        assert_eq!(TODAY, days_from_civil(2026, 11, 17) as i32);
        let root = tmp("missing");
        let f = root.join("-Users-x-projects-demo/s1.jsonl");
        let stamp = |day: i32| crate::timeutil::iso_ms(day_start_ms(day, OFF) + 5 * MS_PER_HOUR);
        let missing = TODAY - 10;
        let present = TODAY - 9;
        append(
            &f,
            &(line("a", &stamp(missing), 100)
                + &line("a", &stamp(missing), 100)
                + &line("b", &stamp(present), 200)
                + &line("c", &stamp(TODAY - 31), 300)
                + &line("d", &stamp(TODAY - 7), 400)),
        );
        // mtime inside [today - 30, today - 6): old enough to be outside the
        // live window, recent enough to be worth reading.
        set_mtime(&f, day_start_ms(TODAY - 8, OFF) + MS_PER_HOUR);

        let mut have = HashSet::new();
        have.insert(present); // history already has today - 9
        let res = backfill(&root, None, OFF, TODAY, &have);
        assert_eq!(res.days.len(), 1, "{:?}", res.days);
        assert_eq!(res.days[0].day, missing);
        assert_eq!(res.days[0].t[1], 100);
        assert_eq!(res.days[0].n, 1);
        assert!(res.days[0].w > 0.0);
        assert!(res.bytes > 0);

        // A file written today is never read, even if it holds old lines.
        set_mtime(&f, day_start_ms(TODAY, OFF) + MS_PER_HOUR);
        let res = backfill(&root, None, OFF, TODAY, &have);
        assert!(res.days.is_empty());
    }

    #[test]
    fn backfill_missing_range_stops_at_eight_days_back() {
        let have: HashSet<i32> = HashSet::new();
        let m = missing_days(TODAY, &have);
        assert_eq!(m.first().copied(), Some(TODAY - BACKFILL_OLDEST));
        assert_eq!(m.last().copied(), Some(TODAY - BACKFILL_NEWEST));
        assert_eq!(m.len(), (BACKFILL_OLDEST - BACKFILL_NEWEST + 1) as usize);
        assert_eq!(parse_ts_ms("2026-11-17T00:00:00Z").is_some(), true);
    }
}
