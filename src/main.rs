//! Who Ate My Tokens: a small, local, live dashboard for T3 Code threads
//! and Claude Code usage. Read-only on its sources; binds to 127.0.0.1.

mod agg;
mod codex;
mod drill;
mod feed;
mod http;
mod hub;
mod payload;
mod state;
mod sys;
mod t3;
mod tail;
mod timeutil;
mod transcript;

use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::net::TcpListener;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use crate::agg::DayAgg;
use crate::feed::FeedBuilder;
use crate::hub::Hub;
use crate::payload::{compose_days, compose_live, Shared, T3Part};
use crate::timeutil::{day_of, day_start_ms, now_ms, MS_PER_MIN};

const UI: &str = include_str!("../ui/index.html");

/// Fonts and images compiled into the binary. They live in read-only
/// pages, so they cost no heap memory.
static ASSETS: &[http::Asset] = &[
    http::Asset { name: "fonts/archivo.woff2", ctype: "font/woff2", bytes: include_bytes!("../ui/assets/fonts/archivo.woff2") },
    http::Asset { name: "fonts/caveat.woff2", ctype: "font/woff2", bytes: include_bytes!("../ui/assets/fonts/caveat.woff2") },
    http::Asset { name: "logo.png", ctype: "image/png", bytes: include_bytes!("../ui/assets/logo.png") },
    http::Asset { name: "logo-dark.png", ctype: "image/png", bytes: include_bytes!("../ui/assets/logo-dark.png") },
    http::Asset { name: "favicon.png", ctype: "image/png", bytes: include_bytes!("../ui/assets/favicon.png") },
];

struct Config {
    bind: String,
    t3_db: PathBuf,
    claude_root: PathBuf,
    codex_root: PathBuf,
    state_dir: PathBuf,
    ui: Option<PathBuf>,
    extra_hosts: Vec<String>,
    off: i32,
}

fn env_or(key: &str, default: impl Into<String>) -> String {
    std::env::var(key).ok().filter(|v| !v.is_empty()).unwrap_or_else(|| default.into())
}

fn config() -> Config {
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
    Config {
        bind: env_or("WAMT_BIND", "127.0.0.1:7317"),
        t3_db: env_or("WAMT_T3_DB", format!("{home}/.t3/userdata/statev2.sqlite")).into(),
        claude_root: env_or("WAMT_CLAUDE_ROOT", format!("{home}/.claude/projects")).into(),
        codex_root: env_or("WAMT_CODEX_ROOT", format!("{home}/.codex/sessions")).into(),
        state_dir: env_or("WAMT_STATE", ".state").into(),
        ui: std::env::var("WAMT_UI").ok().filter(|v| !v.is_empty()).map(PathBuf::from),
        extra_hosts: env_or("WAMT_ALLOW_HOSTS", "")
            .split(',')
            .map(|s| s.trim().to_ascii_lowercase())
            .filter(|s| !s.is_empty())
            .collect(),
        off: timeutil::local_offset_min(),
    }
}

fn log(msg: impl AsRef<str>) {
    eprintln!("{} {}", timeutil::iso_ms(now_ms()), msg.as_ref());
}

fn publish_live(shared: &Mutex<Shared>, hub: &Hub) {
    let json = {
        let mut sh = shared.lock().unwrap();
        compose_live(&mut sh, now_ms())
    };
    hub.set_live(json);
}

fn publish_days(shared: &Mutex<Shared>, hub: &Hub, force: bool) {
    let now = now_ms();
    let json = {
        let mut sh = shared.lock().unwrap();
        if !force && now - sh.last_days_json < 2000 {
            // Throttled: remember to publish on a later pass, never drop it.
            sh.days_pending = true;
            return;
        }
        sh.last_days_json = now;
        sh.days_pending = false;
        compose_days(&sh, now)
    };
    hub.set_days(json);
}

/// Publishes a days update that an earlier throttled call deferred.
fn flush_days(shared: &Mutex<Shared>, hub: &Hub) {
    let due = {
        let sh = shared.lock().unwrap();
        sh.days_pending && now_ms() - sh.last_days_json >= 2000
    };
    if due {
        publish_days(shared, hub, false);
    }
}

// ------------------------------------------------------------ T3 worker

#[derive(Serialize, Deserialize, Default)]
struct LaunchCache {
    v: u32,
    launch: HashMap<String, String>,
    checked: HashSet<String>,
    /// The one full launch scan has run (even if it found nothing).
    #[serde(default)]
    scanned: bool,
}

fn t3_worker(cfg_db: PathBuf, state_dir: PathBuf, shared: Arc<Mutex<Shared>>, hub: Arc<Hub>) {
    let cache_path = state_dir.join("t3.json");
    let mut first_open = true;
    loop {
        let mut t3 = match t3::T3::open(&cfg_db) {
            Ok(t) => t,
            Err(e) => {
                if first_open {
                    log(format!("T3 database unavailable: {e}"));
                    first_open = false;
                }
                shared.lock().unwrap().t3_error = Some(e);
                publish_live(&shared, &hub);
                thread::sleep(Duration::from_secs(10));
                continue;
            }
        };
        first_open = true;
        let problems = t3.schema_problems();
        if !problems.is_empty() {
            log(format!("T3 schema changed: {}", problems.join("; ")));
            {
                let mut sh = shared.lock().unwrap();
                sh.t3_problems = problems;
                sh.t3 = None;
            }
            publish_live(&shared, &hub);
            publish_days(&shared, &hub, true);
            thread::sleep(Duration::from_secs(60));
            continue;
        }
        {
            let mut sh = shared.lock().unwrap();
            sh.t3_problems.clear();
            sh.t3_warnings = t3.json_drift();
            if !sh.t3_warnings.is_empty() {
                log(format!("T3 payload keys changed: {}", sh.t3_warnings.join("; ")));
            }
        }
        if let Err(e) = run_t3(&mut t3, &cache_path, &shared, &hub) {
            log(format!("T3 read error, reconnecting: {e}"));
            {
                // Show "unavailable", never stale runs that look live.
                let mut sh = shared.lock().unwrap();
                sh.t3_error = Some(e.to_string());
                sh.t3 = None;
            }
            publish_live(&shared, &hub);
            publish_days(&shared, &hub, true);
            thread::sleep(Duration::from_secs(3));
        }
    }
}

fn run_t3(t3: &mut t3::T3, cache_path: &PathBuf, shared: &Arc<Mutex<Shared>>, hub: &Arc<Hub>) -> rusqlite::Result<()> {
    let off = shared.lock().unwrap().off;
    let t0 = std::time::Instant::now();
    let mut cache_dirty = false;
    let mut scanned = false;
    if let Some(c) = state::load::<LaunchCache>(cache_path) {
        if c.v == 1 {
            t3.launch = c.launch;
            t3.launch_checked = c.checked;
            scanned = c.scanned || !t3.launch.is_empty();
        }
    }
    t3.begin()?;
    t3.load_threads(None)?;
    t3.load_sessions()?;
    t3.end();
    if !scanned {
        log("first start: matching launched threads to their launchers (one pass, a few seconds)");
        t3.begin()?;
        let n = t3.full_launch_scan()?;
        t3.load_threads(None)?;
        t3.end();
        log(format!("found {n} launch links"));
        cache_dirty = true;
    }

    // Seed the feed from recent history.
    let mut feed = FeedBuilder::default();
    t3.begin()?;
    let max = t3.max_seq()?;
    let seed = t3.events_since((max - 6000).max(0), 6000)?;
    t3.end();
    let items: Vec<(String, String)> = seed
        .iter()
        .filter_map(|ev| feed.on_event(ev, &t3.threads))
        .map(|it| (it.id.clone(), serde_json::to_string(&it).unwrap_or_default()))
        .collect();
    hub.push_feed(items);
    let mut last_event_ms = seed.last().map(|e| e.ts).unwrap_or(0);

    let mut threads_arc = Arc::new(t3.threads.clone());
    let mut sessions_arc = Arc::new(t3.session_thread.clone());
    // Shared with the composer by Arc, so each pass hands over pointers,
    // not copies; they are rebuilt only when refreshed.
    let mut runs: Arc<Vec<t3::RunRow>> = Arc::new(Vec::new());
    let mut subs: Arc<Vec<t3::Subagent>> = Arc::new(Vec::new());
    let mut active = Vec::new();
    let mut waiting = Vec::new();
    let mut ctx: HashMap<String, t3::Ctx> = HashMap::new();
    let mut ctx_arc: Arc<HashMap<String, t3::Ctx>> = Arc::new(HashMap::new());
    let mut last_drift = now_ms();
    let (mut last_full, mut last_subs, mut last_runs, mut last_live, mut last_launch) = (0i64, 0i64, 0i64, 0i64, 0i64);
    let mut force = true;
    let mut runs_pending = true;
    let mut ctx_dirty: HashSet<String> = HashSet::new();
    let mut last_ctx_full = 0i64;
    log(format!("T3 ready in {} ms: {} threads, {} sessions mapped", t0.elapsed().as_millis(), t3.threads.len(), t3.session_thread.len()));

    loop {
        let now = now_ms();
        let window_start = day_start_ms(day_of(now, off) - 2, off);
        t3.begin()?;
        let seq = t3.max_seq()?;
        let mut live_dirty = force;
        let mut days_dirty = force;
        let mut threads_dirty = false;
        let mut subs_dirty = force;
        if seq != t3.last_seq {
            let evs = t3.events_since(t3.last_seq, 5000)?;
            let mut refresh: HashSet<String> = HashSet::new();
            let mut sessions_dirty = false;
            for ev in &evs {
                match ev.kind.as_str() {
                    k if k.starts_with("thread.") => {
                        refresh.insert(ev.thread.clone());
                    }
                    "subagent.updated" => subs_dirty = true,
                    "provider-turn.updated" => {
                        ctx_dirty.insert(ev.thread.clone());
                    }
                    "run-attempt.created" | "run-attempt.updated" | "provider-session.attached" => sessions_dirty = true,
                    "run.updated" | "run.created" => days_dirty = true,
                    _ => {}
                }
            }
            for id in &refresh {
                t3.load_threads(Some(id))?;
                threads_dirty = true;
            }
            for ev in evs.iter().filter(|e| e.kind == "thread.created") {
                let mcp_root = t3
                    .threads
                    .get(&ev.thread)
                    .is_some_and(|t| t.source.as_deref() == Some("mcp") && t.parent.is_none());
                if mcp_root {
                    t3.note_created(&ev.thread, ev.seq, now);
                }
            }
            if sessions_dirty {
                t3.load_sessions()?;
                sessions_arc = Arc::new(t3.session_thread.clone());
            }
            let items: Vec<(String, String)> = evs
                .iter()
                .filter_map(|ev| feed.on_event(ev, &t3.threads))
                .map(|it| (it.id.clone(), serde_json::to_string(&it).unwrap_or_default()))
                .collect();
            hub.push_feed(items);
            if let Some(l) = evs.last() {
                last_event_ms = l.ts;
            }
            live_dirty = true;
        }
        if now - last_launch > 5000 {
            last_launch = now;
            if t3.resolve_launches(now) {
                threads_dirty = true;
                cache_dirty = true;
            }
        }
        if now - last_full > 60_000 {
            last_full = now;
            t3.load_threads(None)?;
            t3.load_sessions()?;
            sessions_arc = Arc::new(t3.session_thread.clone());
            threads_dirty = true;
            // Forget launch checks for threads that no longer exist.
            let before = t3.launch_checked.len();
            let known: HashSet<String> = t3.threads.keys().cloned().collect();
            t3.launch_checked.retain(|id| known.contains(id));
            cache_dirty |= before != t3.launch_checked.len();
        }
        if now - last_drift > 10 * 60_000 {
            last_drift = now;
            let drift = t3.json_drift();
            shared.lock().unwrap().t3_warnings = drift;
        }
        if subs_dirty || now - last_subs > 15_000 {
            last_subs = now;
            subs = Arc::new(t3.subagents(window_start)?);
            live_dirty = true;
            days_dirty = true;
        }
        if live_dirty || now - last_live > 10_000 {
            last_live = now;
            active = t3.active_runs()?;
            waiting = t3.waiting(now)?;
            // Context fill for threads that are live or recently touched.
            // Re-read only threads whose provider turn changed, plus a full
            // pass once a minute.
            let mut want: HashSet<&str> = active.iter().map(|r| r.thread.as_str()).collect();
            for t in t3.threads.values() {
                if now - t.updated < 3 * 60 * MS_PER_MIN {
                    want.insert(&t.id);
                }
            }
            let full_ctx = now - last_ctx_full > 60_000;
            if full_ctx {
                last_ctx_full = now;
            }
            let before = ctx.len();
            ctx.retain(|id, _| want.contains(id.as_str()));
            let mut ctx_changed = before != ctx.len();
            for id in want {
                if full_ctx || ctx_dirty.contains(id) || !ctx.contains_key(id) {
                    ctx_changed = true;
                    match t3.ctx(id)? {
                        Some(c) => {
                            ctx.insert(id.to_string(), c);
                        }
                        None => {
                            ctx.remove(id);
                        }
                    }
                }
            }
            ctx_dirty.clear();
            if ctx_changed {
                ctx_arc = Arc::new(ctx.clone());
            }
        }
        runs_pending |= days_dirty;
        days_dirty = subs_dirty || force;
        if (runs_pending && now - last_runs > 3000) || now - last_runs > 60_000 {
            last_runs = now;
            runs = Arc::new(t3.runs_since(window_start)?);
            runs_pending = false;
            days_dirty = true;
        }
        t3.end();
        if threads_dirty {
            threads_arc = Arc::new(t3.threads.clone());
        }
        {
            let mut sh = shared.lock().unwrap();
            sh.t3_error = None;
            sh.t3 = Some(T3Part {
                seq,
                threads: threads_arc.clone(),
                sessions: sessions_arc.clone(),
                active: active.clone(),
                subs: subs.clone(),
                ctx: ctx_arc.clone(),
                waiting: waiting.clone(),
                runs: runs.clone(),
                last_event_ms,
                launch_links: t3.launch.len(),
            });
        }
        if live_dirty || threads_dirty {
            publish_live(shared, hub);
        }
        if days_dirty || threads_dirty || force {
            publish_days(shared, hub, force);
        } else {
            flush_days(shared, hub);
        }
        if cache_dirty {
            let c = LaunchCache { v: 1, launch: t3.launch.clone(), checked: t3.launch_checked.clone(), scanned: true };
            if state::save(cache_path, &c).is_ok() {
                cache_dirty = false;
            }
        }
        force = false;
        // Poll faster while things are happening.
        let busy = now - last_event_ms < 30_000;
        thread::sleep(Duration::from_millis(if busy { 700 } else { 1500 }));
    }
}

// ------------------------------------------------------- transcript worker

/// Adds a Codex day to a Claude day. Session ids never collide, and Codex
/// has no hourly or active-time data, so only models, sessions and subagent
/// counts are merged.
fn merge_codex_day(into: &mut DayAgg, cx: &DayAgg) {
    for (m, u) in &cx.models {
        into.models.entry(m.clone()).or_default().merge(u);
    }
    for (sid, s) in &cx.sessions {
        into.sessions.insert(sid.clone(), s.clone());
    }
    for (p, m) in &cx.subs {
        into.subs.entry(p.clone()).or_default().extend(m.clone());
    }
}

fn tail_worker(root: PathBuf, codex_root: PathBuf, state_dir: PathBuf, shared: Arc<Mutex<Shared>>, hub: Arc<Hub>) {
    let cache_path = state_dir.join("transcripts.json");
    let off = shared.lock().unwrap().off;
    let mut t = tail::Tailer::new(root.clone(), off);
    if let Some(c) = state::load::<tail::TailCache>(&cache_path) {
        if t.restore(c) {
            log("transcript cache restored; reading only new bytes");
        } else {
            log("transcript cache ignored (version or time zone changed)");
        }
    }
    // Codex is optional: with no folder the feature is silently off.
    let codex_path = state_dir.join("codex.json");
    let mut cx = codex_root.is_dir().then(|| codex::CodexTail::new(codex_root.clone(), off));
    if let Some(c) = cx.as_mut() {
        if let Some(cache) = state::load::<codex::CodexCache>(&codex_path) {
            if c.restore(cache) {
                log("codex cache restored; reading only new bytes");
            }
        }
    }
    let mut cx_saved = now_ms();
    let mut today = day_of(now_ms(), off);
    let (mut last_full, mut last_pub, mut last_save, mut last_prune) = (0i64, 0i64, now_ms(), 0i64);
    let mut last_tracked = 0i64;
    let mut days_arc: Arc<Vec<(i32, DayAgg)>> = Arc::new(Vec::new());
    let mut first = true;
    loop {
        let now = now_ms();
        let day = day_of(now, off);
        let window_start = day_start_ms(day - 2, off);
        let mut force = first;
        if day != today || now - last_prune > 60_000 {
            if day != today {
                log("new day: rolling the three-day window");
                force = true;
                today = day;
            }
            t.agg.prune(day - 2, now, off);
            t.prune_sessions(window_start);
            if let Some(c) = cx.as_mut() {
                c.prune(day, window_start, now);
            }
            last_prune = now;
        }
        let t0 = std::time::Instant::now();
        // Hot files every second, tracked files and folders every 5 s, and a
        // full sweep (which also catches resumed old sessions) every minute.
        let mut tier = 0u8;
        let changed = if now - last_full >= 60_000 {
            tier = 2;
            last_full = now;
            last_tracked = now;
            let c = t.full_scan(window_start, now);
            // Give back pages freed by parsing (most after the first pass).
            let released = sys::release_free_memory();
            if first && released > 0 {
                log(format!("returned {} MB of freed memory to macOS", released / 1_048_576));
            }
            c
        } else if now - last_tracked >= 5000 {
            tier = 1;
            last_tracked = now;
            t.poll_tracked(window_start, now)
        } else {
            t.poll_hot(window_start, now)
        };
        let mut changed = changed;
        if let Some(c) = cx.as_mut() {
            changed |= match tier {
                2 => c.full_scan(window_start, now),
                1 => c.poll_tracked(window_start, now),
                _ => c.poll_hot(window_start, now),
            };
        }
        if first {
            log(format!(
                "transcripts ready in {} ms: {} files tracked, {} MB read",
                t0.elapsed().as_millis(),
                t.stats.files,
                t.stats.bytes_read / 1_000_000
            ));
        }
        if changed || force || now - last_pub >= 5000 {
            last_pub = now;
            let anchors = shared.lock().unwrap().anchors();
            let window = t.agg.window(now, &anchors);
            if changed || force {
                let mut v: Vec<(i32, DayAgg)> = Vec::with_capacity(3);
                for d in [day, day - 1, day - 2] {
                    let mut a = t.agg.days.get(&d).cloned();
                    // Codex joins the day's totals and lists (weight 0).
                    if let Some(cd) = cx.as_ref().and_then(|c| c.agg.days.get(&d)) {
                        merge_codex_day(a.get_or_insert_with(DayAgg::default), cd);
                    }
                    if let Some(a) = a {
                        v.push((d, a));
                    }
                }
                days_arc = Arc::new(v);
            }
            // Last 6 hours in 5-minute buckets.
            let step = 5 * MS_PER_MIN;
            let end = now - now.rem_euclid(step) + step;
            let start = end - 72 * step;
            let mut spark = vec![[0.0f64; 4]; 72];
            let mut spark_obs = vec![0.0f64; 72];
            for (m, c) in t.agg.mins.range(start / MS_PER_MIN..) {
                let i = ((m * MS_PER_MIN - start) / step) as usize;
                if i < 72 {
                    for f in 0..4 {
                        spark[i][f] += c.w[f];
                    }
                    spark_obs[i] += c.o;
                }
            }
            let sessions: HashMap<String, tail::SessionInfo> = t
                .sessions
                .iter()
                .chain(cx.iter().flat_map(|c| c.sessions.iter()))
                .filter(|(_, s)| s.last >= window_start)
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect();
            let woke: Vec<String> = std::mem::take(&mut t.woke);
            let hits: Vec<i64> = std::mem::take(&mut t.new_hits);
            let mut feed_items = Vec::new();
            {
                let mut sh = shared.lock().unwrap();
                sh.tx.days = days_arc.clone();
                sh.tx.sessions = Arc::new(sessions);
                sh.tx.window = Some(window);
                sh.tx.spark_start = start;
                sh.tx.spark = spark;
                sh.tx.spark_obs = spark_obs;
                sh.tx.stats = t.stats.clone();
                sh.tx.hits = t.agg.hits.clone();
                sh.tx.codex_limit = cx.as_ref().and_then(|c| c.limit.clone());
                sh.tx.ready = true;
                let mapped: HashSet<String> = sh.t3.as_ref().map(|p| p.sessions.keys().cloned().collect()).unwrap_or_default();
                if !first {
                    for s in woke {
                        if mapped.contains(&s) {
                            continue;
                        }
                        let info = t.sessions.get(&s);
                        let label = info
                            .and_then(|i| i.title.clone().or_else(|| i.cwd.as_deref().map(|c| c.rsplit('/').next().unwrap_or(c).to_string())))
                            .unwrap_or_else(|| s.chars().take(8).collect());
                        let app = info.and_then(|i| i.ep.clone());
                        let model = info.and_then(|i| i.model.clone());
                        feed_items.push(feed::session_item(&s, now, label, app, model));
                    }
                    for h in hits {
                        feed_items.push(feed::limit_item(h));
                    }
                }
            }
            hub.push_feed(feed_items.into_iter().map(|it| (it.id.clone(), serde_json::to_string(&it).unwrap_or_default())).collect());
            publish_live(&shared, &hub);
            if changed || force {
                publish_days(&shared, &hub, force);
            }
        }
        flush_days(&shared, &hub);
        if t.agg.dirty && now - last_save >= 60_000 {
            last_save = now;
            match state::save(&cache_path, &t.to_cache()) {
                Ok(n) => {
                    t.agg.dirty = false;
                    if first || n > 400_000 {
                        log(format!("cache saved: {} KB", n / 1024));
                    }
                }
                Err(e) => log(format!("cache save failed: {e}")),
            }
        }
        if let Some(c) = cx.as_mut() {
            if c.agg.dirty && now - cx_saved >= 60_000 {
                cx_saved = now;
                match state::save(&codex_path, &c.to_cache()) {
                    Ok(_) => c.agg.dirty = false,
                    Err(e) => log(format!("codex cache save failed: {e}")),
                }
            }
        }
        first = false;
        thread::sleep(Duration::from_millis(1000));
    }
}

fn main() {
    let cfg = config();
    let listener = match TcpListener::bind(&cfg.bind) {
        Ok(l) => l,
        Err(e) => {
            log(format!("cannot listen on {}: {e}", cfg.bind));
            std::process::exit(1);
        }
    };
    let port = listener.local_addr().map(|a| a.port()).unwrap_or(7317);
    log(format!(
        "who ate my tokens? on http://{} (utc offset {} min, T3 {}, transcripts {}, state {})",
        cfg.bind,
        cfg.off,
        cfg.t3_db.display(),
        cfg.claude_root.display(),
        cfg.state_dir.display()
    ));
    let shared = Arc::new(Mutex::new(Shared::new(cfg.off)));
    let hub = Hub::new();
    publish_live(&shared, &hub);
    publish_days(&shared, &hub, true);
    {
        let (s, h, db, sd) = (shared.clone(), hub.clone(), cfg.t3_db.clone(), cfg.state_dir.clone());
        thread::Builder::new().name("t3".into()).stack_size(1 << 20).spawn(move || t3_worker(db, sd, s, h)).unwrap();
    }
    {
        let (s, h, root, sd) = (shared.clone(), hub.clone(), cfg.claude_root.clone(), cfg.state_dir.clone());
        let cx = cfg.codex_root.clone();
        thread::Builder::new().name("tail".into()).stack_size(1 << 20).spawn(move || tail_worker(root, cx, sd, s, h)).unwrap();
    }
    let ui = match cfg.ui {
        Some(p) => http::Ui::Disk(p),
        None => http::Ui::Embedded(UI),
    };
    let server = Arc::new(http::Server { hub, shared: shared.clone(), t3_db: cfg.t3_db.clone(), ui, assets: ASSETS, port, extra_hosts: cfg.extra_hosts });
    server.run(listener);
}
