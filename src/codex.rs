//! Codex usage, read-only and incremental, like the Claude tailer.
//!
//! Layout: `<root>/YYYY/MM/DD/rollout-<local time>-<uuid>.jsonl` (folders are
//! LOCAL dates). Only the day folders inside the kept window (plus one extra
//! day) are ever listed. A line is `{timestamp, type, payload}`; we use four
//! types and never look at anything else:
//!
//! - `session_meta`: session ids, thread source, working folder NAME.
//! - `turn_context`: the model in use.
//! - `token_usage_record`: one per model response (counted, deduped by id).
//! - `event_msg` `token_count`: only its `rate_limits.primary`, Codex's own
//!   limit meter (its usage copy is skipped so nothing is counted twice).
//!
//! Messages, reasoning, tool calls and `base_instructions` are skipped
//! unread. Codex usage has weight 0 and never reaches the Claude 5-hour
//! plate or the six-hour chart (see `UsageIn::codex`).

use memchr::memmem;
use serde::{Deserialize, Serialize};
use std::borrow::Cow;
use std::collections::HashMap;
use std::fs::{self, File};
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use crate::agg::{Agg, UsageIn};
use crate::tail::{drop_cache, ino, mtime_ms, no_cache, SessionInfo, CHUNK, HOT_MS, KEEP_BUF, MAX_LINE};
use crate::timeutil::{civil_from_days, day_of, parse_ts_ms, MS_PER_MIN};
use crate::transcript::{Lenient, Tokens};

pub const CODEX_CACHE_VERSION: u32 = 2;
/// The `ep` value Codex sessions carry in `SessionInfo`.
pub const EP: &str = "codex";

/// Codex's own limit meter (the latest one seen). Not Claude's limit.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CodexLimit {
    pub ts: i64,
    pub pct: f64,
    pub win_min: u64,
    pub resets_ms: i64,
    pub plan: Option<String>,
}

#[derive(Debug, PartialEq)]
pub enum CLine {
    Meta { id: Option<String>, session_id: Option<String>, subagent: bool, cwd: Option<String> },
    Model(String),
    Usage { ts_ms: i64, response_id: Option<String>, tokens: Tokens },
    Limit(CodexLimit),
}

#[derive(Deserialize)]
struct Raw<'a> {
    #[serde(rename = "type", borrow, default)]
    t: Option<Cow<'a, str>>,
    #[serde(borrow, default)]
    timestamp: Option<Cow<'a, str>>,
    // Never wrapped in `Lenient`: that would buffer the whole payload
    // (base instructions included). Unknown fields are skipped unread.
    #[serde(default)]
    payload: Option<Payload<'a>>,
}

#[derive(Deserialize)]
struct Payload<'a> {
    #[serde(rename = "type", borrow, default)]
    t: Option<Cow<'a, str>>,
    #[serde(default)]
    id: Option<Lenient<Cow<'a, str>>>,
    #[serde(default)]
    session_id: Option<Lenient<Cow<'a, str>>>,
    #[serde(default)]
    response_id: Option<Lenient<Cow<'a, str>>>,
    #[serde(default)]
    thread_source: Option<Lenient<Cow<'a, str>>>,
    #[serde(default)]
    cwd: Option<Lenient<Cow<'a, str>>>,
    #[serde(default)]
    model: Option<Lenient<Cow<'a, str>>>,
    #[serde(default)]
    usage: Option<Lenient<RawUsage>>,
    #[serde(default)]
    rate_limits: Option<Lenient<RawLimits<'a>>>,
}

#[derive(Deserialize, Default)]
struct RawUsage {
    #[serde(default)]
    input_tokens: Option<u64>,
    #[serde(default)]
    cached_input_tokens: Option<u64>,
    #[serde(default)]
    cache_write_input_tokens: Option<u64>,
    #[serde(default)]
    output_tokens: Option<u64>,
}

#[derive(Deserialize)]
struct RawLimits<'a> {
    #[serde(default)]
    primary: Option<RawPrimary>,
    #[serde(default)]
    plan_type: Option<Lenient<Cow<'a, str>>>,
}

#[derive(Deserialize)]
struct RawPrimary {
    #[serde(default)]
    used_percent: Option<f64>,
    #[serde(default)]
    window_minutes: Option<u64>,
    #[serde(default)]
    resets_at: Option<i64>,
}

fn has(line: &[u8], needle: &[u8]) -> bool {
    memmem::find(line, needle).is_some()
}

fn folder_name(c: &str) -> String {
    c.trim_end_matches('/').rsplit('/').next().unwrap_or(c).to_string()
}

/// OpenAI counts cached and cache-written tokens inside `input_tokens`, and
/// reasoning tokens inside `output_tokens`. Uncached input is what is left.
fn map_usage(u: &RawUsage) -> Tokens {
    let input = u.input_tokens.unwrap_or(0);
    let cached = u.cached_input_tokens.unwrap_or(0);
    let write = u.cache_write_input_tokens.unwrap_or(0);
    Tokens {
        input: input.saturating_sub(cached).saturating_sub(write),
        output: u.output_tokens.unwrap_or(0),
        cache_read: cached,
        cache_w5m: write,
        cache_w1h: 0,
    }
}

pub fn parse_line(line: &[u8]) -> Option<CLine> {
    let wanted = has(line, b"\"token_usage_record\"")
        || has(line, b"\"session_meta\"")
        || has(line, b"\"turn_context\"")
        || has(line, b"\"rate_limits\"");
    if !wanted {
        return None;
    }
    let raw: Raw = serde_json::from_slice(line).ok()?;
    let p = raw.payload?;
    let ts_ms = raw.timestamp.as_deref().and_then(parse_ts_ms).unwrap_or(0);
    let s = |v: Option<Lenient<Cow<str>>>| v.and_then(Lenient::get).map(Cow::into_owned).filter(|x| !x.is_empty());
    match raw.t.as_deref()? {
        "session_meta" => Some(CLine::Meta {
            id: s(p.id),
            session_id: s(p.session_id),
            subagent: p.thread_source.and_then(Lenient::get).is_some_and(|t| t == "subagent"),
            cwd: s(p.cwd).map(|c| folder_name(&c)),
        }),
        "turn_context" => s(p.model).map(CLine::Model),
        "token_usage_record" => {
            let tokens = map_usage(&p.usage.and_then(Lenient::get)?);
            if tokens.is_zero() || ts_ms <= 0 {
                return None;
            }
            Some(CLine::Usage { ts_ms, response_id: s(p.response_id), tokens })
        }
        "event_msg" if p.t.as_deref() == Some("token_count") => {
            let rl = p.rate_limits.and_then(Lenient::get)?;
            let prim = rl.primary?;
            if ts_ms <= 0 {
                return None;
            }
            Some(CLine::Limit(CodexLimit {
                ts: ts_ms,
                pct: prim.used_percent?,
                win_min: prim.window_minutes.unwrap_or(0),
                resets_ms: prim.resets_at.unwrap_or(0) * 1000,
                plan: rl.plan_type.and_then(Lenient::get).map(Cow::into_owned),
            }))
        }
        _ => None,
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct CFile {
    pub off: u64,
    pub size: u64,
    pub mtime: i64,
    pub ino: u64,
    /// Session this file's usage belongs to (the file's own id, or the parent
    /// session for a subagent thread).
    pub sid: String,
    #[serde(default)]
    pub sub: bool,
    /// Model from the latest `turn_context` seen in this file.
    #[serde(default)]
    pub model: Option<String>,
}

#[derive(Default, Serialize, Deserialize)]
pub struct CodexCache {
    pub v: u32,
    pub off: i32,
    pub files: HashMap<String, CFile>,
    pub sessions: HashMap<String, SessionInfo>,
    pub agg: Agg,
    #[serde(default)]
    pub limit: Option<CodexLimit>,
}

#[derive(Serialize)]
pub struct CodexCacheRef<'a> {
    pub v: u32,
    pub off: i32,
    pub files: HashMap<&'a str, &'a CFile>,
    pub sessions: &'a HashMap<String, SessionInfo>,
    pub agg: &'a Agg,
    pub limit: &'a Option<CodexLimit>,
}

pub struct CodexTail {
    root: PathBuf,
    /// Codex moves finished sessions to a flat `archived_sessions` folder
    /// next to `sessions`; read it too (same shape, same read-only rules).
    archive: PathBuf,
    /// Cache paths are relative to the folder holding both.
    base: PathBuf,
    off: i32,
    files: HashMap<PathBuf, CFile>,
    pub sessions: HashMap<String, SessionInfo>,
    pub agg: Agg,
    pub limit: Option<CodexLimit>,
    buf: Vec<u8>,
    dirs: HashMap<PathBuf, i64>,
    pub bytes_read: u64,
}

/// The session id in `rollout-<time>-<uuid>.jsonl` (the last 36 characters).
fn file_id(path: &Path) -> Option<String> {
    let stem = path.file_stem()?.to_str()?;
    let stem = stem.strip_prefix("rollout-")?;
    (stem.len() >= 36).then(|| stem[stem.len() - 36..].to_string())
}

impl CodexTail {
    pub fn new(root: PathBuf, off: i32) -> Self {
        let base = root.parent().map(Path::to_path_buf).unwrap_or_else(|| root.clone());
        CodexTail {
            archive: base.join("archived_sessions"),
            base,
            root,
            off,
            files: HashMap::new(),
            sessions: HashMap::new(),
            agg: Agg::default(),
            limit: None,
            buf: Vec::new(),
            dirs: HashMap::new(),
            bytes_read: 0,
        }
    }

    #[cfg(test)]
    pub fn files_tracked(&self) -> usize {
        self.files.len()
    }

    pub fn restore(&mut self, c: CodexCache) -> bool {
        if c.v != CODEX_CACHE_VERSION || c.off != self.off {
            return false;
        }
        for (p, rec) in c.files {
            self.files.insert(self.base.join(p), rec);
        }
        self.sessions = c.sessions;
        self.agg = c.agg;
        self.limit = c.limit;
        true
    }

    pub fn to_cache(&self) -> CodexCacheRef<'_> {
        CodexCacheRef {
            v: CODEX_CACHE_VERSION,
            off: self.off,
            files: self
                .files
                .iter()
                .filter_map(|(p, t)| p.strip_prefix(&self.base).ok().and_then(|r| r.to_str()).map(|s| (s, t)))
                .collect(),
            sessions: &self.sessions,
            agg: &self.agg,
            limit: &self.limit,
        }
    }

    fn day_dir(&self, day: i32) -> PathBuf {
        let (y, m, d) = civil_from_days(day as i64);
        self.root.join(format!("{y:04}")).join(format!("{m:02}")).join(format!("{d:02}"))
    }

    fn list_day(dir: &Path, out: &mut Vec<PathBuf>) {
        let Ok(rd) = fs::read_dir(dir) else { return };
        for e in rd.flatten() {
            let name = e.file_name();
            let name = name.to_string_lossy();
            if name.starts_with("rollout-") && name.ends_with(".jsonl") {
                out.push(e.path());
            }
        }
    }

    fn track(&mut self, path: &Path) {
        if self.files.contains_key(path) {
            return;
        }
        let Some(sid) = file_id(path) else { return };
        self.files.insert(path.to_path_buf(), CFile { sid, ..Default::default() });
    }

    /// Lists the day folders from three days before the window's first day
    /// up to today (the window is the last three local days).
    pub fn full_scan(&mut self, window_start_ms: i64, now_ms: i64) -> bool {
        let today = day_of(now_ms, self.off);
        let mut changed = false;
        let mut present = std::collections::HashSet::new();
        let mut files = Vec::new();
        for day in (today - 3)..=today {
            let dir = self.day_dir(day);
            if let Ok(m) = fs::metadata(&dir) {
                self.dirs.insert(dir.clone(), mtime_ms(&m));
                Self::list_day(&dir, &mut files);
            }
        }
        // Archived sessions: one flat folder; the mtime check below skips
        // everything older than the window without opening it.
        Self::list_day(&self.archive.clone(), &mut files);
        {
            for path in files {
                let Ok(meta) = fs::metadata(&path) else { continue };
                present.insert(path.clone());
                if !self.files.contains_key(&path) && mtime_ms(&meta) < window_start_ms {
                    continue;
                }
                self.track(&path);
                changed |= self.read_if_grown(&path, &meta, window_start_ms, now_ms);
            }
        }
        let before = self.files.len();
        self.files.retain(|p, t| present.contains(p) && t.mtime >= window_start_ms);
        changed | (before != self.files.len())
    }

    /// Re-stats tracked files, and re-lists today's and yesterday's folders
    /// when their mtime changed (a new session file changes its folder).
    pub fn poll_tracked(&mut self, window_start_ms: i64, now_ms: i64) -> bool {
        let today = day_of(now_ms, self.off);
        let mut changed = false;
        for day in [today - 1, today] {
            let dir = self.day_dir(day);
            let Ok(m) = fs::metadata(&dir) else { continue };
            let mt = mtime_ms(&m);
            if self.dirs.get(&dir).copied() == Some(mt) {
                continue;
            }
            self.dirs.insert(dir.clone(), mt);
            let mut files = Vec::new();
            Self::list_day(&dir, &mut files);
            for path in files {
                let Ok(meta) = fs::metadata(&path) else { continue };
                if !self.files.contains_key(&path) && mtime_ms(&meta) < window_start_ms {
                    continue;
                }
                self.track(&path);
                changed |= self.read_if_grown(&path, &meta, window_start_ms, now_ms);
            }
        }
        let tracked: Vec<PathBuf> = self.files.keys().cloned().collect();
        for path in tracked {
            if let Ok(meta) = fs::metadata(&path) {
                changed |= self.read_if_grown(&path, &meta, window_start_ms, now_ms);
            }
        }
        changed
    }

    pub fn poll_hot(&mut self, window_start_ms: i64, now_ms: i64) -> bool {
        let hot: Vec<PathBuf> = self.files.iter().filter(|(_, t)| now_ms - t.mtime < HOT_MS).map(|(p, _)| p.clone()).collect();
        let mut changed = false;
        for path in hot {
            if let Ok(meta) = fs::metadata(&path) {
                changed |= self.read_if_grown(&path, &meta, window_start_ms, now_ms);
            }
        }
        changed
    }

    /// Forgets sessions outside the window that no tracked file feeds.
    pub fn prune(&mut self, day: i32, window_start_ms: i64, now_ms: i64) {
        self.agg.prune(day - 2, now_ms, self.off);
        let tracked: std::collections::HashSet<&str> = self.files.values().map(|t| t.sid.as_str()).collect();
        self.sessions.retain(|id, s| s.last >= window_start_ms || tracked.contains(id.as_str()));
        if self.limit.as_ref().is_some_and(|l| l.ts < window_start_ms) {
            self.limit = None;
            self.agg.dirty = true;
        }
    }

    fn read_if_grown(&mut self, path: &Path, meta: &fs::Metadata, window_start_ms: i64, now_ms: i64) -> bool {
        let size = meta.len();
        let file_ino = ino(meta);
        let mt = mtime_ms(meta);
        let Some(t) = self.files.get_mut(path) else { return false };
        if (t.ino != 0 && t.ino != file_ino) || size < t.off {
            // Replaced or truncated: start over; response-id dedupe prevents
            // double counting.
            t.off = 0;
        }
        t.ino = file_ino;
        t.mtime = mt;
        if size == t.off {
            t.size = size;
            return false;
        }
        let mut rec = self.files.remove(path).unwrap();
        let res = self.read_new(path, &mut rec, size, window_start_ms, now_ms);
        rec.size = size;
        self.files.insert(path.to_path_buf(), rec);
        res.unwrap_or(false)
    }

    fn read_new(&mut self, path: &Path, t: &mut CFile, size: u64, window_start_ms: i64, now_ms: i64) -> std::io::Result<bool> {
        let mut f = File::open(path)?;
        no_cache(&f);
        f.seek(SeekFrom::Start(t.off))?;
        let mut remaining = size - t.off;
        let mut changed = false;
        self.buf.clear();
        while remaining > 0 {
            let want = (remaining as usize).min(CHUNK);
            let len = self.buf.len();
            self.buf.resize(len + want, 0);
            let n = f.read(&mut self.buf[len..])?;
            self.buf.truncate(len + n);
            if n == 0 {
                break;
            }
            remaining -= n as u64;
            self.bytes_read += n as u64;
            let mut start = 0;
            let buf = std::mem::take(&mut self.buf);
            for nl in memchr::memchr_iter(b'\n', &buf) {
                let line = &buf[start..nl];
                if !line.is_empty() {
                    changed |= self.handle_line(line, t, window_start_ms, now_ms);
                }
                start = nl + 1;
            }
            t.off += start as u64;
            let mut buf = buf;
            buf.drain(..start);
            if buf.len() > MAX_LINE {
                t.off += buf.len() as u64;
                buf.clear();
            }
            self.buf = buf;
        }
        drop_cache(&f);
        self.buf.clear();
        if self.buf.capacity() > KEEP_BUF {
            self.buf.shrink_to(KEEP_BUF / 4);
        }
        Ok(changed)
    }

    fn handle_line(&mut self, line: &[u8], t: &mut CFile, window_start_ms: i64, now_ms: i64) -> bool {
        let Some(l) = parse_line(line) else { return false };
        match l {
            CLine::Meta { id, session_id, subagent, cwd } => {
                // A subagent thread's usage belongs to its parent session.
                if subagent {
                    if let (Some(own), Some(parent)) = (id.as_deref(), session_id) {
                        if own != parent {
                            t.sid = parent;
                            t.sub = true;
                        }
                    }
                }
                let s = self.sessions.entry(t.sid.clone()).or_default();
                s.ep = Some(EP.to_string());
                if s.cwd.is_none() {
                    s.cwd = cwd;
                }
                false
            }
            CLine::Model(m) => {
                t.model = Some(m);
                false
            }
            CLine::Limit(l) => {
                if self.limit.as_ref().map_or(true, |o| l.ts >= o.ts) && self.limit.as_ref() != Some(&l) {
                    self.limit = Some(l);
                    self.agg.dirty = true;
                    true
                } else {
                    false
                }
            }
            CLine::Usage { ts_ms, response_id, tokens } => {
                if ts_ms < window_start_ms || ts_ms > now_ms + 10 * MS_PER_MIN {
                    return false;
                }
                let model = t.model.clone().unwrap_or_else(|| "gpt".to_string());
                // Without a response id, a stand-in keeps re-reads from double counting.
                let key = response_id.unwrap_or_else(|| format!("{}|{}|{}", t.sid, ts_ms, tokens.total()));
                let counted = self.agg.add_usage(
                    UsageIn {
                        session: &t.sid,
                        sub: t.sub,
                        observer: false,
                        ts_ms,
                        model: &model,
                        tokens: &tokens,
                        fast: false,
                        msg_id: Some(&key),
                        prev_ms: 0,
                        codex: true,
                    },
                    self.off,
                );
                if counted {
                    let s = self.sessions.entry(t.sid.clone()).or_default();
                    s.ep = Some(EP.to_string());
                    if ts_ms >= s.last {
                        s.last = ts_ms;
                        s.model = Some(model);
                    }
                }
                counted
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    const UUID: &str = "00000000-0000-4000-8000-000000000001";
    const NOW: &str = "2026-11-17T20:00:00Z";
    const START: &str = "2026-11-14T18:30:00Z";

    fn meta() -> String {
        r#"{"timestamp":"2026-11-17T18:10:47.000Z","type":"session_meta","payload":{"id":"00000000-0000-4000-8000-000000000001","session_id":"00000000-0000-4000-8000-000000000001","originator":"T3 Code","source":"vscode","thread_source":"user","model_provider":"openai","cwd":"/Users/x/projects/demo/","base_instructions":{"text":"SECRET TEXT"}}}"#.to_string() + "\n"
    }

    fn ctx(model: &str) -> String {
        format!(r#"{{"timestamp":"2026-11-17T18:10:48.000Z","type":"turn_context","payload":{{"model":"{model}","effort":"medium"}}}}"#) + "\n"
    }

    fn rec(id: &str, ts: &str, input: u64, cached: u64, write: u64, out: u64) -> String {
        format!(
            r#"{{"timestamp":"{ts}","type":"token_usage_record","payload":{{"response_id":"{id}","session_id":"{UUID}","thread_id":"{UUID}","usage":{{"input_tokens":{input},"cached_input_tokens":{cached},"cache_write_input_tokens":{write},"output_tokens":{out},"reasoning_output_tokens":3,"total_tokens":{}}}}}}}"#,
            input + out
        ) + "\n"
    }

    fn count(ts: &str, pct: f64) -> String {
        format!(
            r#"{{"timestamp":"{ts}","type":"event_msg","payload":{{"type":"token_count","info":{{"last_token_usage":{{"input_tokens":99999,"output_tokens":99999}}}},"rate_limits":{{"limit_id":"codex","primary":{{"used_percent":{pct},"window_minutes":43200,"resets_at":1792236244}},"secondary":null,"plan_type":"go"}}}}}}"#
        ) + "\n"
    }

    fn tmp(name: &str) -> (PathBuf, PathBuf) {
        let top = std::env::temp_dir().join(format!("sb-codex-{}-{}", name, std::process::id()));
        let _ = fs::remove_dir_all(&top);
        let d = top.join("sessions");
        let day = d.join("2026/11/17");
        fs::create_dir_all(&day).unwrap();
        let f = day.join(format!("rollout-2026-11-17T23-40-47-{UUID}.jsonl"));
        (d, f)
    }

    fn append(p: &Path, s: &str) {
        let mut f = fs::OpenOptions::new().create(true).append(true).open(p).unwrap();
        f.write_all(s.as_bytes()).unwrap();
    }

    fn scan(t: &mut CodexTail) -> bool {
        t.full_scan(parse_ts_ms(START).unwrap(), parse_ts_ms(NOW).unwrap())
    }

    #[test]
    fn usage_subtracts_cached_and_writes_without_double_counting_reasoning() {
        let l = parse_line(rec("r1", "2026-11-17T18:11:00.000Z", 22_335, 9_984, 100, 7).as_bytes()).unwrap();
        let CLine::Usage { tokens, response_id, ts_ms } = l else { panic!("not usage") };
        assert_eq!(response_id.as_deref(), Some("r1"));
        assert_eq!(ts_ms, parse_ts_ms("2026-11-17T18:11:00Z").unwrap());
        // 22335 - 9984 - 100 uncached; reasoning (3) is already inside output.
        assert_eq!(tokens, Tokens { input: 12_251, output: 7, cache_read: 9_984, cache_w5m: 100, cache_w1h: 0 });
        // Never negative when the parts exceed the whole.
        let l = parse_line(rec("r2", "2026-11-17T18:11:00.000Z", 10, 9, 9, 1).as_bytes()).unwrap();
        let CLine::Usage { tokens, .. } = l else { panic!() };
        assert_eq!(tokens.input, 0);
    }

    #[test]
    fn meta_keeps_folder_name_only() {
        let CLine::Meta { id, cwd, subagent, .. } = parse_line(meta().as_bytes()).unwrap() else { panic!() };
        assert_eq!(id.as_deref(), Some(UUID));
        assert_eq!(cwd.as_deref(), Some("demo"));
        assert!(!subagent);
        assert_eq!(parse_line(ctx("gpt-6.1-sol").as_bytes()), Some(CLine::Model("gpt-6.1-sol".into())));
    }

    #[test]
    fn rate_limit_parse() {
        let CLine::Limit(l) = parse_line(count("2026-11-17T18:12:00.000Z", 12.5).as_bytes()).unwrap() else { panic!() };
        assert_eq!(l.pct, 12.5);
        assert_eq!(l.win_min, 43_200);
        assert_eq!(l.resets_ms, 1_792_236_244_000);
        assert_eq!(l.plan.as_deref(), Some("go"));
        // A token_count line with no meter is not a limit line.
        let none = r#"{"timestamp":"2026-11-17T18:12:00Z","type":"event_msg","payload":{"type":"token_count","info":null,"rate_limits":null}}"#;
        assert!(parse_line(none.as_bytes()).is_none());
    }

    #[test]
    fn archived_sessions_are_read_and_a_move_is_not_counted_twice() {
        let (root, f) = tmp("archive");
        append(&f, &meta());
        append(&f, &ctx("gpt-6.1-sol"));
        append(&f, &rec("r1", "2026-11-17T18:11:00.000Z", 1000, 400, 0, 50));
        let mut t = CodexTail::new(root.clone(), 330);
        assert!(scan(&mut t));
        // Codex archives the finished session: same file, new folder.
        let arch = root.parent().unwrap().join("archived_sessions");
        fs::create_dir_all(&arch).unwrap();
        let moved = arch.join(f.file_name().unwrap());
        fs::rename(&f, &moved).unwrap();
        append(&moved, &rec("r2", "2026-11-17T18:12:00.000Z", 500, 0, 0, 20));
        scan(&mut t);
        let u = t.agg.days.values().next().unwrap().total();
        assert_eq!((u.n, u.t.output), (2, 70));
        // The cache keeps archived paths too.
        let js = serde_json::to_string(&t.to_cache()).unwrap();
        assert!(js.contains("archived_sessions/rollout-"));
    }

    #[test]
    fn other_lines_are_skipped() {
        let s = r#"{"timestamp":"2026-11-17T18:12:00Z","type":"response_item","payload":{"type":"message","content":[{"type":"output_text","text":"mentions token_usage_record in text"}]}}"#;
        assert!(parse_line(s.as_bytes()).is_none());
    }

    #[test]
    fn tail_counts_once_and_tracks_session() {
        let (root, f) = tmp("once");
        append(&f, &meta());
        append(&f, &ctx("gpt-6.1-sol"));
        append(&f, &rec("r1", "2026-11-17T18:11:00.000Z", 1000, 400, 0, 50));
        // The same response twice (and a second distinct one): counted once each.
        append(&f, &rec("r1", "2026-11-17T18:11:00.000Z", 1000, 400, 0, 50));
        append(&f, &rec("r2", "2026-11-17T18:12:00.000Z", 500, 0, 0, 20));
        append(&f, &count("2026-11-17T18:12:01.000Z", 3.0));
        let mut t = CodexTail::new(root.clone(), 330);
        assert!(scan(&mut t));
        let day = t.agg.days.values().next().unwrap();
        let u = day.total();
        assert_eq!((u.n, u.t.input, u.t.cache_read, u.t.output), (2, 600 + 500, 400, 70));
        assert_eq!(u.w, 0.0);
        assert!(day.models.contains_key("gpt-6.1-sol"));
        let s = &t.sessions[UUID];
        assert_eq!((s.cwd.as_deref(), s.ep.as_deref(), s.model.as_deref()), (Some("demo"), Some("codex"), Some("gpt-6.1-sol")));
        assert_eq!(t.limit.as_ref().map(|l| l.pct), Some(3.0));
        // A restart from the cache reads only new bytes and does not recount.
        let js = serde_json::to_string(&t.to_cache()).unwrap();
        append(&f, &rec("r3", "2026-11-17T18:13:00.000Z", 100, 0, 0, 1));
        append(&f, &rec("r2", "2026-11-17T18:12:00.000Z", 500, 0, 0, 20));
        let mut t2 = CodexTail::new(root.clone(), 330);
        assert!(t2.restore(serde_json::from_str(&js).unwrap()));
        scan(&mut t2);
        assert_eq!(t2.agg.days.values().next().unwrap().total().n, 3);
        assert!(!CodexTail::new(root, 0).restore(serde_json::from_str(&js).unwrap()));
    }

    #[test]
    fn codex_never_reaches_the_five_hour_window() {
        let (root, f) = tmp("window");
        append(&f, &meta());
        append(&f, &ctx("gpt-6.1-sol"));
        append(&f, &rec("r1", "2026-11-17T19:50:00.000Z", 1_000_000, 0, 0, 100_000));
        let mut t = CodexTail::new(root, 330);
        scan(&mut t);
        assert_eq!(t.agg.days.values().next().unwrap().total().t.output, 100_000);
        assert!(t.agg.mins.is_empty());
        let now = parse_ts_ms(NOW).unwrap();
        let w = t.agg.window(now, &[]);
        assert!(!w.active);
        assert_eq!((w.used, w.calls), (0.0, 0));
        let d = t.agg.days.values().next().unwrap();
        assert_eq!(d.active_slots(), 0);
        assert!(d.hours.iter().all(|h| h.iter().all(|v| *v == 0.0)) && d.hours_n.iter().all(|n| *n == 0));
    }

    #[test]
    fn old_lines_and_old_folders_are_ignored() {
        let (root, f) = tmp("old");
        append(&f, &meta());
        append(&f, &rec("old", "2026-11-01T18:00:00.000Z", 10, 0, 0, 10));
        append(&f, &rec("new", "2026-11-17T18:00:00.000Z", 10, 0, 0, 1));
        // A folder far outside the window is never listed.
        let far = root.join("2026/05/12");
        fs::create_dir_all(&far).unwrap();
        append(&far.join(format!("rollout-2026-05-12T10-00-00-{UUID}.jsonl")), &rec("far", "2026-11-17T18:00:00.000Z", 5, 0, 0, 5));
        let mut t = CodexTail::new(root, 330);
        scan(&mut t);
        assert_eq!(t.agg.days.values().map(|d| d.total().t.output).sum::<u64>(), 1);
        assert_eq!(t.files_tracked(), 1);
    }

    #[test]
    fn subagent_thread_belongs_to_parent_session() {
        let (root, _) = tmp("sub");
        let sub = root.join("2026/11/17/rollout-2026-11-17T23-41-00-0000aaaa-0000-0000-0000-000000000001.jsonl");
        append(
            &sub,
            r#"{"timestamp":"2026-11-17T18:11:00.000Z","type":"session_meta","payload":{"id":"0000aaaa-0000-0000-0000-000000000001","session_id":"parent-1","thread_source":"subagent","cwd":"/a/b"}}"#,
        );
        append(&sub, "\n");
        append(&sub, &rec("s1", "2026-11-17T18:12:00.000Z", 10, 0, 0, 5));
        let mut t = CodexTail::new(root, 330);
        scan(&mut t);
        let d = t.agg.days.values().next().unwrap();
        assert_eq!(d.sessions["parent-1"].sub.t.output, 5);
    }
}
