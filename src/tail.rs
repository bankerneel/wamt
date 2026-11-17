//! Incremental, read-only tailing of Claude Code transcripts.
//!
//! Layout: `<root>/<project dir>/<session>.jsonl` and, for built-in
//! subagents, `<root>/<project dir>/<session>/subagents/agent-<id>.jsonl`
//! with a sibling `agent-<id>.meta.json`. Each file is read once up to its
//! last complete line; afterwards only new bytes are read.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs::{self, File};
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use crate::agg::{Agg, UsageIn};
use crate::timeutil::{day_of, MS_PER_MIN};
use crate::transcript::{parse_line, LineKind};

const CHUNK: usize = 256 * 1024;
const KEEP_BUF: usize = 512 * 1024;
const MAX_LINE: usize = 64 * 1024 * 1024;
const HOT_MS: i64 = 15 * MS_PER_MIN;

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct FileRec {
    pub off: u64,
    pub size: u64,
    pub mtime: i64,
    pub ino: u64,
    /// Subagent file already counted as one subagent run.
    #[serde(default)]
    pub sc: bool,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct SessionInfo {
    pub cwd: Option<String>,
    pub ep: Option<String>,
    pub title: Option<String>,
    pub obs: bool,
    pub last: i64,
    /// Model of the most recent request.
    pub model: Option<String>,
}

#[derive(Default, Serialize, Deserialize)]
pub struct TailCache {
    pub v: u32,
    pub off: i32,
    pub files: HashMap<String, FileRec>,
    pub sessions: HashMap<String, SessionInfo>,
    pub agg: Agg,
}

pub const CACHE_VERSION: u32 = 2;

#[derive(Default, Clone, Debug, Serialize)]
pub struct TailStats {
    pub files: usize,
    pub bytes_read: u64,
    pub lines: u64,
    pub scans: u64,
    pub last_full_scan_ms: i64,
    pub scan_ms: u64,
}

struct Tracked {
    rec: FileRec,
    session: String,
    sub: bool,
    obs: bool,
    agent_type: Option<String>,
}

pub struct Tailer {
    root: PathBuf,
    off: i32,
    files: HashMap<PathBuf, Tracked>,
    pub sessions: HashMap<String, SessionInfo>,
    pub agg: Agg,
    buf: Vec<u8>,
    pub stats: TailStats,
    /// Sessions that turned active in the last poll (for the feed).
    pub woke: Vec<String>,
    /// Limit hits recorded in the last poll (for the feed).
    pub new_hits: Vec<i64>,
    /// Folders seen in the last listing, with their mtimes.
    dirs: HashMap<PathBuf, i64>,
}

/// What a path is, judged from where it sits under the root.
fn classify(root: &Path, path: &Path) -> Option<(String, bool, bool)> {
    let rel = path.strip_prefix(root).ok()?;
    let parts: Vec<&str> = rel.iter().filter_map(|p| p.to_str()).collect();
    let obs = parts.first().is_some_and(|p| p.contains("claude-mem-observer"));
    match parts.as_slice() {
        [_, f] if f.ends_with(".jsonl") => Some((f.trim_end_matches(".jsonl").to_string(), false, obs)),
        [_, s, "subagents", f] if f.starts_with("agent-") && f.ends_with(".jsonl") => Some((s.to_string(), true, obs)),
        _ => None,
    }
}

fn mtime_ms(m: &fs::Metadata) -> i64 {
    m.modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

#[cfg(unix)]
fn ino(m: &fs::Metadata) -> u64 {
    use std::os::unix::fs::MetadataExt;
    m.ino()
}

/// Ask the OS not to keep what we read in its page cache: a backfill must not
/// push the user's apps out of memory.
fn no_cache(f: &File) {
    #[cfg(target_os = "macos")]
    {
        use std::os::unix::io::AsRawFd;
        // SAFETY: plain fcntl on a descriptor we own.
        unsafe {
            libc::fcntl(f.as_raw_fd(), libc::F_NOCACHE, 1);
        }
    }
    #[cfg(not(target_os = "macos"))]
    let _ = f;
}

fn drop_cache(f: &File) {
    #[cfg(target_os = "linux")]
    {
        use std::os::unix::io::AsRawFd;
        // SAFETY: advisory call on a descriptor we own.
        unsafe {
            libc::posix_fadvise(f.as_raw_fd(), 0, 0, libc::POSIX_FADV_DONTNEED);
        }
    }
    #[cfg(not(target_os = "linux"))]
    let _ = f;
}

fn agent_type(jsonl: &Path) -> Option<String> {
    #[derive(Deserialize)]
    struct Meta {
        #[serde(rename = "agentType")]
        agent_type: Option<String>,
    }
    let meta = jsonl.with_extension("meta.json");
    let bytes = fs::read(meta).ok()?;
    if bytes.len() > 64 * 1024 {
        return None;
    }
    serde_json::from_slice::<Meta>(&bytes).ok()?.agent_type
}

impl Tailer {
    pub fn new(root: PathBuf, off: i32) -> Self {
        Tailer {
            root,
            off,
            files: HashMap::new(),
            sessions: HashMap::new(),
            agg: Agg::default(),
            buf: Vec::new(),
            stats: TailStats::default(),
            woke: Vec::new(),
            new_hits: Vec::new(),
            dirs: HashMap::new(),
        }
    }

    /// Restores offsets and aggregates from a cache written by `to_cache`.
    pub fn restore(&mut self, c: TailCache) -> bool {
        if c.v != CACHE_VERSION || c.off != self.off {
            return false;
        }
        for (p, rec) in c.files {
            // Paths are stored relative to the transcripts folder.
            let path = self.root.join(&p);
            if let Some((session, sub, obs)) = classify(&self.root, &path) {
                let agent_type = if sub { agent_type(&path) } else { None };
                self.files.insert(path, Tracked { rec, session, sub, obs, agent_type });
            }
        }
        self.sessions = c.sessions;
        self.agg = c.agg;
        true
    }

    pub fn to_cache(&self) -> TailCacheRef<'_> {
        TailCacheRef {
            v: CACHE_VERSION,
            off: self.off,
            files: self
                .files
                .iter()
                .filter_map(|(p, t)| p.strip_prefix(&self.root).ok().and_then(|r| r.to_str()).map(|s| (s, &t.rec)))
                .collect(),
            sessions: &self.sessions,
            agg: &self.agg,
        }
    }

    /// Agent transcripts inside one `subagents` folder.
    fn list_subagents(dir: &Path, files: &mut Vec<PathBuf>) {
        if let Ok(se) = fs::read_dir(dir) {
            for s in se.flatten() {
                let name = s.file_name();
                let name = name.to_string_lossy();
                if name.starts_with("agent-") && name.ends_with(".jsonl") {
                    files.push(s.path());
                }
            }
        }
    }

    /// Session transcripts in one project folder, plus the agent transcripts
    /// of its sessions. Records every folder visited with its mtime.
    fn list_project(dir: &Path, files: &mut Vec<PathBuf>, dirs: &mut Vec<(PathBuf, i64)>) {
        let Ok(entries) = fs::read_dir(dir) else { return };
        if let Ok(m) = fs::metadata(dir) {
            dirs.push((dir.to_path_buf(), mtime_ms(&m)));
        }
        for e in entries.flatten() {
            let Ok(ft) = e.file_type() else { continue };
            let path = e.path();
            if ft.is_file() && path.extension().is_some_and(|x| x == "jsonl") {
                files.push(path);
            } else if ft.is_dir() {
                let subs = path.join("subagents");
                if let Ok(m) = fs::metadata(&subs) {
                    dirs.push((subs.clone(), mtime_ms(&m)));
                    Self::list_subagents(&subs, files);
                }
            }
        }
    }

    fn list_files(&mut self) -> Vec<PathBuf> {
        let mut out = Vec::new();
        let mut dirs = Vec::new();
        if let Ok(m) = fs::metadata(&self.root) {
            dirs.push((self.root.clone(), mtime_ms(&m)));
        }
        if let Ok(projects) = fs::read_dir(&self.root) {
            for p in projects.flatten() {
                if p.file_type().is_ok_and(|ft| ft.is_dir()) {
                    Self::list_project(&p.path(), &mut out, &mut dirs);
                }
            }
        }
        self.dirs = dirs.into_iter().collect();
        out
    }

    /// Cheaper than a full scan: re-stats the tracked files, and re-lists
    /// only folders whose mtime changed (a new file changes its folder's
    /// mtime). Falls back to a full scan when a new project folder appears.
    pub fn poll_tracked(&mut self, window_start_ms: i64, now_ms: i64) -> bool {
        let root_m = fs::metadata(&self.root).map(|m| mtime_ms(&m)).unwrap_or(0);
        if self.dirs.get(&self.root).copied() != Some(root_m) {
            return self.full_scan(window_start_ms, now_ms);
        }
        let mut changed = false;
        let stale: Vec<PathBuf> = self
            .dirs
            .iter()
            .filter(|(d, mt)| **d != self.root && fs::metadata(d).map(|m| mtime_ms(&m)).unwrap_or(0) != **mt)
            .map(|(d, _)| d.clone())
            .collect();
        for dir in stale {
            let mut files = Vec::new();
            let mut dirs = Vec::new();
            if dir.ends_with("subagents") {
                if let Ok(m) = fs::metadata(&dir) {
                    dirs.push((dir.clone(), mtime_ms(&m)));
                }
                Self::list_subagents(&dir, &mut files);
            } else {
                Self::list_project(&dir, &mut files, &mut dirs);
            }
            for (d, mt) in dirs {
                self.dirs.insert(d, mt);
            }
            for path in files {
                if self.files.contains_key(&path) {
                    continue;
                }
                let Ok(meta) = fs::metadata(&path) else { continue };
                if mtime_ms(&meta) < window_start_ms {
                    continue;
                }
                let Some((session, sub, obs)) = classify(&self.root, &path) else { continue };
                let agent_type = if sub { agent_type(&path) } else { None };
                self.files.insert(path.clone(), Tracked { rec: FileRec::default(), session, sub, obs, agent_type });
                changed |= self.read_if_grown(&path, &meta, window_start_ms, now_ms);
            }
        }
        let tracked: Vec<PathBuf> = self.files.keys().cloned().collect();
        for path in tracked {
            if let Ok(meta) = fs::metadata(&path) {
                changed |= self.read_if_grown(&path, &meta, window_start_ms, now_ms);
            }
        }
        self.stats.files = self.files.len();
        changed
    }

    /// Walks every transcript, starts tracking files touched inside the
    /// window and reads anything new. Returns true when aggregates changed.
    pub fn full_scan(&mut self, window_start_ms: i64, now_ms: i64) -> bool {
        let t0 = std::time::Instant::now();
        let mut changed = false;
        let listed = self.list_files();
        let mut present = std::collections::HashSet::with_capacity(listed.len());
        for path in listed {
            let Ok(meta) = fs::metadata(&path) else { continue };
            let mt = mtime_ms(&meta);
            present.insert(path.clone());
            if !self.files.contains_key(&path) {
                if mt < window_start_ms {
                    continue;
                }
                let Some((session, sub, obs)) = classify(&self.root, &path) else { continue };
                let agent_type = if sub { agent_type(&path) } else { None };
                self.files.insert(path.clone(), Tracked { rec: FileRec::default(), session, sub, obs, agent_type });
            }
            changed |= self.read_if_grown(&path, &meta, window_start_ms, now_ms);
        }
        // Forget files that vanished or fell out of the window.
        let before = self.files.len();
        self.files.retain(|p, t| present.contains(p) && t.rec.mtime >= window_start_ms);
        changed |= before != self.files.len();
        self.stats.files = self.files.len();
        self.stats.scans += 1;
        self.stats.last_full_scan_ms = now_ms;
        self.stats.scan_ms = t0.elapsed().as_millis() as u64;
        changed
    }

    /// Re-stats files written in the last 15 minutes and reads new bytes.
    pub fn poll_hot(&mut self, window_start_ms: i64, now_ms: i64) -> bool {
        let hot: Vec<PathBuf> = self
            .files
            .iter()
            .filter(|(_, t)| now_ms - t.rec.mtime < HOT_MS)
            .map(|(p, _)| p.clone())
            .collect();
        let mut changed = false;
        for path in hot {
            if let Ok(meta) = fs::metadata(&path) {
                changed |= self.read_if_grown(&path, &meta, window_start_ms, now_ms);
            }
        }
        changed
    }

    fn read_if_grown(&mut self, path: &Path, meta: &fs::Metadata, window_start_ms: i64, now_ms: i64) -> bool {
        let size = meta.len();
        let file_ino = ino(meta);
        let mt = mtime_ms(meta);
        let Some(t) = self.files.get_mut(path) else { return false };
        if (t.rec.ino != 0 && t.rec.ino != file_ino) || size < t.rec.off {
            // Replaced or truncated: start over. Message-id dedupe prevents
            // double counting of lines we saw before.
            t.rec.off = 0;
        }
        t.rec.ino = file_ino;
        t.rec.mtime = mt;
        if size == t.rec.off {
            t.rec.size = size;
            return false;
        }
        let mut tracked = self.files.remove(path).unwrap();
        let res = self.read_new(path, &mut tracked, size, window_start_ms, now_ms);
        tracked.rec.size = size;
        self.files.insert(path.to_path_buf(), tracked);
        res.unwrap_or(false)
    }

    fn read_new(
        &mut self,
        path: &Path,
        t: &mut Tracked,
        size: u64,
        window_start_ms: i64,
        now_ms: i64,
    ) -> std::io::Result<bool> {
        let mut f = File::open(path)?;
        no_cache(&f);
        f.seek(SeekFrom::Start(t.rec.off))?;
        let mut remaining = size - t.rec.off;
        let mut changed = false;
        self.buf.clear();
        while remaining > 0 {
            let want = (remaining as usize).min(CHUNK);
            // Read straight into the reusable buffer's tail.
            let len = self.buf.len();
            self.buf.resize(len + want, 0);
            let n = f.read(&mut self.buf[len..])?;
            self.buf.truncate(len + n);
            if n == 0 {
                break;
            }
            remaining -= n as u64;
            self.stats.bytes_read += n as u64;
            // Process every complete line in the buffer.
            let mut start = 0;
            let buf = std::mem::take(&mut self.buf);
            for nl in memchr::memchr_iter(b'\n', &buf) {
                let line = &buf[start..nl];
                if !line.is_empty() {
                    changed |= self.handle_line(line, t, window_start_ms, now_ms);
                }
                start = nl + 1;
            }
            t.rec.off += start as u64;
            let mut buf = buf;
            buf.drain(..start);
            if buf.len() > MAX_LINE {
                // A runaway line: skip past it rather than hold it in memory.
                t.rec.off += buf.len() as u64;
                buf.clear();
            }
            self.buf = buf;
        }
        drop_cache(&f);
        // The tail (a line still being written) is re-read next time.
        self.buf.clear();
        if self.buf.capacity() > KEEP_BUF {
            self.buf.shrink_to(KEEP_BUF / 4);
        }
        Ok(changed)
    }

    fn handle_line(&mut self, line: &[u8], t: &mut Tracked, window_start_ms: i64, now_ms: i64) -> bool {
        self.stats.lines += 1;
        let want_meta = !t.sub && self.sessions.get(&t.session).map_or(true, |s| s.cwd.is_none());
        let Some(l) = parse_line(line, want_meta) else { return false };
        let s = self.sessions.entry(t.session.clone()).or_default();
        s.obs |= t.obs;
        if !t.sub {
            if s.cwd.is_none() {
                s.cwd = l.cwd.clone();
            }
            if s.ep.is_none() {
                s.ep = l.entrypoint.clone();
            }
        }
        let in_window = l.ts_ms >= window_start_ms && l.ts_ms <= now_ms + 10 * MS_PER_MIN;
        match l.kind {
            LineKind::Title => {
                s.title = l.title;
                true
            }
            LineKind::RateLimit => {
                if in_window && self.agg.add_limit_hit(l.ts_ms) {
                    self.new_hits.push(l.ts_ms);
                    true
                } else {
                    false
                }
            }
            LineKind::Usage => {
                if !in_window {
                    return false;
                }
                let model = l.model.as_deref().unwrap_or("unknown");
                let was_idle = now_ms - s.last > 10 * MS_PER_MIN;
                let counted = self.agg.add_usage(
                    UsageIn {
                        session: &t.session,
                        sub: t.sub,
                        observer: t.obs,
                        ts_ms: l.ts_ms,
                        model,
                        tokens: &l.tokens,
                        fast: l.fast,
                        msg_id: l.msg_id.as_deref(),
                    },
                    self.off,
                );
                if counted {
                    if l.ts_ms >= s.last {
                        s.last = l.ts_ms;
                        s.model = Some(model.to_string());
                    }
                    if was_idle && now_ms - l.ts_ms < 2 * MS_PER_MIN && !t.obs {
                        self.woke.push(t.session.clone());
                    }
                    if t.sub && !t.rec.sc {
                        t.rec.sc = true;
                        let label = format!("{}|{}", t.agent_type.as_deref().unwrap_or("agent"), model);
                        self.agg.add_subagent_file(day_of(l.ts_ms, self.off), &t.session, &label);
                    }
                }
                counted
            }
            LineKind::Meta => false,
        }
    }
}

/// Borrowed view of the cache, so saving never clones the aggregates.
#[derive(Serialize)]
pub struct TailCacheRef<'a> {
    pub v: u32,
    pub off: i32,
    pub files: HashMap<&'a str, &'a FileRec>,
    pub sessions: &'a HashMap<String, SessionInfo>,
    pub agg: &'a Agg,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::timeutil::parse_ts_ms;
    use std::io::Write;

    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("sb-tail-{}-{}", name, std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(d.join("-Users-x-projects-demo")).unwrap();
        d
    }

    fn line(id: &str, ts: &str, out: u64) -> String {
        format!(
            r#"{{"type":"assistant","timestamp":"{ts}","cwd":"/Users/x/projects/demo","entrypoint":"cli","message":{{"id":"{id}","model":"claude-opus-5-5","content":[{{"type":"text","text":"x"}}],"usage":{{"input_tokens":1,"output_tokens":{out}}}}}}}"#
        ) + "\n"
    }

    fn append(p: &Path, s: &str) {
        let mut f = fs::OpenOptions::new().create(true).append(true).open(p).unwrap();
        f.write_all(s.as_bytes()).unwrap();
    }

    fn out_today(t: &Tailer) -> u64 {
        t.agg.days.values().map(|d| d.total().t.output).sum()
    }

    const NOW: &str = "2026-11-17T16:00:00Z";
    const START: &str = "2026-11-14T18:30:00Z";

    #[test]
    fn partial_line_is_deferred() {
        let root = tmp("partial");
        let f = root.join("-Users-x-projects-demo/s1.jsonl");
        let full = line("m1", "2026-11-17T15:00:00Z", 10);
        let (a, b) = full.split_at(40);
        append(&f, a);
        let mut t = Tailer::new(root.clone(), 330);
        let (ws, now) = (parse_ts_ms(START).unwrap(), parse_ts_ms(NOW).unwrap());
        t.full_scan(ws, now);
        assert_eq!(out_today(&t), 0);
        append(&f, b);
        t.full_scan(ws, now);
        assert_eq!(out_today(&t), 10);
        assert_eq!(t.sessions["s1"].cwd.as_deref(), Some("/Users/x/projects/demo"));
    }

    #[test]
    fn truncated_file_resets() {
        let root = tmp("trunc");
        let f = root.join("-Users-x-projects-demo/s1.jsonl");
        append(&f, &line("m1", "2026-11-17T15:00:00Z", 10));
        append(&f, &line("m2", "2026-11-17T15:01:00Z", 20));
        let mut t = Tailer::new(root.clone(), 330);
        let (ws, now) = (parse_ts_ms(START).unwrap(), parse_ts_ms(NOW).unwrap());
        t.full_scan(ws, now);
        assert_eq!(out_today(&t), 30);
        // Rewrite shorter, with one old and one new message.
        fs::write(&f, line("m1", "2026-11-17T15:00:00Z", 10) + &line("m3", "2026-11-17T15:02:00Z", 5)).unwrap();
        t.full_scan(ws, now);
        assert_eq!(out_today(&t), 35);
    }

    #[test]
    fn split_message_counted_once() {
        let root = tmp("split");
        let f = root.join("-Users-x-projects-demo/s1.jsonl");
        for _ in 0..3 {
            append(&f, &line("m1", "2026-11-17T15:00:00Z", 7));
        }
        let mut t = Tailer::new(root.clone(), 330);
        t.full_scan(parse_ts_ms(START).unwrap(), parse_ts_ms(NOW).unwrap());
        assert_eq!(out_today(&t), 7);
    }

    #[test]
    fn subagent_file_attributes_to_parent() {
        let root = tmp("sub");
        let dir = root.join("-Users-x-projects-demo/s1/subagents");
        fs::create_dir_all(&dir).unwrap();
        append(&root.join("-Users-x-projects-demo/s1.jsonl"), &line("m1", "2026-11-17T15:00:00Z", 1));
        append(&dir.join("agent-abc.jsonl"), &line("m2", "2026-11-17T15:00:30Z", 100));
        fs::write(dir.join("agent-abc.meta.json"), r#"{"agentType":"Explore","description":"x"}"#).unwrap();
        let mut t = Tailer::new(root.clone(), 330);
        t.full_scan(parse_ts_ms(START).unwrap(), parse_ts_ms(NOW).unwrap());
        let d = t.agg.days.values().next().unwrap();
        assert_eq!(d.sessions.len(), 1);
        assert_eq!(d.sessions["s1"].sub.t.output, 100);
        assert_eq!(d.subs["s1"]["Explore|claude-opus-5-5"], 1);
    }

    #[test]
    fn lines_outside_window_are_ignored() {
        let root = tmp("window");
        let f = root.join("-Users-x-projects-demo/s1.jsonl");
        append(&f, &line("old", "2026-11-10T15:00:00Z", 99));
        append(&f, &line("new", "2026-11-17T15:00:00Z", 1));
        let mut t = Tailer::new(root.clone(), 330);
        t.full_scan(parse_ts_ms(START).unwrap(), parse_ts_ms(NOW).unwrap());
        assert_eq!(out_today(&t), 1);
    }

    #[test]
    fn poll_tracked_finds_new_files_by_folder_change() {
        let root = tmp("tracked");
        let proj = root.join("-Users-x-projects-demo");
        append(&proj.join("s1.jsonl"), &line("m1", "2026-11-17T15:00:00Z", 1));
        let (ws, now) = (parse_ts_ms(START).unwrap(), parse_ts_ms(NOW).unwrap());
        let mut t = Tailer::new(root.clone(), 330);
        t.full_scan(ws, now);
        std::thread::sleep(std::time::Duration::from_millis(20));
        // A new session file, and a subagent folder for the first session.
        append(&proj.join("s2.jsonl"), &line("m2", "2026-11-17T15:01:00Z", 10));
        fs::create_dir_all(proj.join("s1/subagents")).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(20));
        assert!(t.poll_tracked(ws, now));
        assert_eq!(out_today(&t), 11);
        std::thread::sleep(std::time::Duration::from_millis(20));
        append(&proj.join("s1/subagents/agent-a.jsonl"), &line("m3", "2026-11-17T15:02:00Z", 100));
        assert!(t.poll_tracked(ws, now));
        assert_eq!(out_today(&t), 111);
        // Nothing new: nothing changes.
        assert!(!t.poll_tracked(ws, now));
    }

    #[test]
    fn cache_round_trip_resumes_offsets() {
        let root = tmp("cache");
        let f = root.join("-Users-x-projects-demo/s1.jsonl");
        append(&f, &line("m1", "2026-11-17T15:00:00Z", 10));
        let (ws, now) = (parse_ts_ms(START).unwrap(), parse_ts_ms(NOW).unwrap());
        let mut t = Tailer::new(root.clone(), 330);
        t.full_scan(ws, now);
        let js = serde_json::to_string(&t.to_cache()).unwrap();
        append(&f, &line("m2", "2026-11-17T15:01:00Z", 5));
        let mut t2 = Tailer::new(root.clone(), 330);
        assert!(t2.restore(serde_json::from_str(&js).unwrap()));
        t2.full_scan(ws, now);
        assert_eq!(out_today(&t2), 15);
        // A cache written with another UTC offset is refused.
        let mut t3 = Tailer::new(root.clone(), 0);
        assert!(!t3.restore(serde_json::from_str(&js).unwrap()));
    }
}
