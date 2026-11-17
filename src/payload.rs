//! Joins T3 structure with transcript numbers and renders the two JSON
//! payloads the page uses: `live` (now, about once a second) and `days`
//! (today, yesterday, the day before; only when something changed).

use serde::Serialize;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Arc;

use crate::agg::{DayAgg, Use, WindowEst};
use crate::sys::{memory_bytes, Sys};
use crate::t3::{ActiveRun, Ctx, Pr, RunRow, Subagent, ThreadMeta, Waiting};
use crate::tail::{SessionInfo, TailStats};
use crate::timeutil::{day_label, day_of, day_start_ms, MS_PER_HOUR, MS_PER_MIN};
use crate::transcript::FAMILIES;

/// The T3 side, refreshed by the T3 worker.
#[derive(Default)]
pub struct T3Part {
    pub seq: i64,
    pub threads: Arc<HashMap<String, ThreadMeta>>,
    pub sessions: Arc<HashMap<String, String>>,
    pub active: Vec<ActiveRun>,
    pub subs: Arc<Vec<Subagent>>,
    pub ctx: Arc<HashMap<String, Ctx>>,
    pub waiting: Vec<Waiting>,
    pub runs: Arc<Vec<RunRow>>,
    pub last_event_ms: i64,
    pub launch_links: usize,
}

/// The transcript side, refreshed by the tail worker.
#[derive(Default)]
pub struct TxPart {
    pub days: Arc<Vec<(i32, DayAgg)>>,
    pub sessions: Arc<HashMap<String, SessionInfo>>,
    pub window: Option<WindowEst>,
    pub spark_start: i64,
    pub spark: Vec<[f64; 4]>,
    pub spark_obs: Vec<f64>,
    pub stats: TailStats,
    /// Limit hits (deduped) inside the 3-day window.
    pub hits: Vec<i64>,
    pub ready: bool,
}

pub struct Shared {
    pub off: i32,
    pub t3: Option<T3Part>,
    pub t3_error: Option<String>,
    pub t3_problems: Vec<String>,
    /// Payload keys T3 seems to have renamed (numbers may be off).
    pub t3_warnings: Vec<String>,
    pub tx: TxPart,
    pub sys: Sys,
    pub last_days_json: i64,
    /// A days update was throttled and still needs publishing.
    pub days_pending: bool,
}

impl Shared {
    pub fn new(off: i32) -> Self {
        Shared {
            off,
            t3: None,
            t3_error: None,
            t3_problems: Vec::new(),
            t3_warnings: Vec::new(),
            tx: TxPart::default(),
            sys: Sys::new(),
            last_days_json: 0,
            days_pending: false,
        }
    }

    pub fn anchors(&self) -> Vec<i64> {
        self.t3
            .as_ref()
            .map(|t| {
                let mut v: Vec<i64> = t.threads.values().filter_map(|m| m.reset_at).collect();
                v.sort_unstable();
                v.dedup();
                v
            })
            .unwrap_or_default()
    }
}

/// Where a Claude session came from.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum App {
    T3,
    /// A T3 Code session not (yet) matched to a T3 thread.
    T3code,
    Desktop,
    Cli,
    Sdk,
    Notetaker,
    Other,
}

fn app_of(session: &str, info: Option<&SessionInfo>, map: &HashMap<String, String>) -> App {
    if map.contains_key(session) {
        return App::T3;
    }
    let Some(i) = info else { return App::Other };
    if i.obs {
        return App::Notetaker;
    }
    match i.ep.as_deref() {
        Some("t3-code") => App::T3code,
        Some("claude-desktop") => App::Desktop,
        Some("cli") => App::Cli,
        Some(e) if e.starts_with("sdk") => App::Sdk,
        _ => App::Other,
    }
}

fn base_name(p: &str) -> &str {
    p.trim_end_matches('/').rsplit('/').next().unwrap_or(p)
}

fn session_label(session: &str, info: Option<&SessionInfo>) -> String {
    if let Some(i) = info {
        if let Some(t) = i.title.as_deref().filter(|t| !t.is_empty()) {
            return t.to_string();
        }
        if let Some(c) = i.cwd.as_deref() {
            return base_name(c).to_string();
        }
    }
    session.chars().take(8).collect()
}

// ---------------------------------------------------------------- live

#[derive(Serialize)]
struct ThreadOut<'a> {
    id: &'a str,
    title: &'a str,
    project: Option<&'a str>,
    parent: Option<&'a str>,
    launcher: Option<&'a str>,
    rel: Option<&'a str>,
    model: Option<&'a str>,
    effort: Option<&'a str>,
    fast: bool,
    source: Option<&'a str>,
    created: i64,
    updated: i64,
    settled: Option<i64>,
    snoozed: Option<i64>,
    reset_at: Option<i64>,
    branch: Option<&'a str>,
    archived: bool,
    prs: usize,
    pr: Option<&'a Pr>,
    run: Option<&'a ActiveRun>,
    ctx: Option<&'a Ctx>,
    waiting: Option<&'a Waiting>,
    today: Option<Use>,
    last_req: i64,
}

#[derive(Serialize)]
struct OutsideOut {
    id: String,
    label: String,
    app: App,
    cwd: Option<String>,
    last: i64,
    model: Option<String>,
    active: bool,
    today: Use,
}

#[derive(Serialize)]
struct NotetakerOut {
    last: i64,
    active: bool,
    model: Option<String>,
    today: Use,
}

#[derive(Serialize)]
struct WindowOut<'a> {
    #[serde(flatten)]
    est: &'a WindowEst,
    /// used / cap.used, when a cap reference exists.
    pct: Option<f64>,
}

#[derive(Serialize, Default)]
struct TodayOut {
    tokens: u64,
    output: u64,
    calls: u64,
    active_min: u32,
    runs: usize,
    runs_live: usize,
    threads: usize,
    subagents: usize,
    subagents_live: usize,
    prs: usize,
    hits: usize,
    waiting: usize,
}

#[derive(Serialize)]
struct Health<'a> {
    t3: &'a str,
    t3_error: Option<&'a str>,
    problems: &'a [String],
    warnings: &'a [String],
    seq: i64,
    last_event: i64,
    tx_ready: bool,
    files: usize,
    bytes_read: u64,
    scan_ms: u64,
    mem: u64,
    cpu: f64,
    uptime: u64,
    launch_links: usize,
}

#[derive(Serialize)]
struct Live<'a> {
    now: i64,
    off: i32,
    families: [&'static str; 4],
    health: Health<'a>,
    today: TodayOut,
    window: Option<WindowOut<'a>>,
    spark: SparkOut<'a>,
    threads: Vec<ThreadOut<'a>>,
    subs: Vec<&'a Subagent>,
    outside: Vec<OutsideOut>,
    notetaker: Option<NotetakerOut>,
}

#[derive(Serialize)]
struct SparkOut<'a> {
    start: i64,
    step: i64,
    w: &'a [[f64; 4]],
    obs: &'a [f64],
}

fn today_by_thread(day: Option<&DayAgg>, map: &HashMap<String, String>) -> HashMap<String, Use> {
    let mut out: HashMap<String, Use> = HashMap::new();
    if let Some(d) = day {
        for (sid, s) in &d.sessions {
            if let Some(th) = map.get(sid) {
                let u = out.entry(th.clone()).or_default();
                for m in s.models.values() {
                    u.merge(m);
                }
            }
        }
    }
    out
}

const LIVE_RUN_LIKE: [&str; 4] = ["running", "starting", "pending", "waiting"];

pub fn compose_live(sh: &mut Shared, now: i64) -> String {
    sh.sys.sample();
    let off = sh.off;
    let today = day_of(now, off);
    let empty_threads = HashMap::new();
    let empty_map = HashMap::new();
    let t3 = sh.t3.as_ref();
    let threads = t3.map(|t| t.threads.as_ref()).unwrap_or(&empty_threads);
    let smap = t3.map(|t| t.sessions.as_ref()).unwrap_or(&empty_map);
    let tday = sh.tx.days.iter().find(|(d, _)| *d == today).map(|(_, a)| a);
    let by_thread = today_by_thread(tday, smap);

    // Last request per thread, from transcript sessions.
    let mut last_req: HashMap<&str, i64> = HashMap::new();
    for (sid, info) in sh.tx.sessions.iter() {
        if let Some(th) = smap.get(sid) {
            let e = last_req.entry(th.as_str()).or_default();
            *e = (*e).max(info.last);
        }
    }

    // Which threads to show: anything live or recent, plus their ancestors.
    let mut show: HashSet<&str> = HashSet::new();
    if let Some(t) = t3 {
        for r in &t.active {
            show.insert(&r.thread);
        }
        for s in t.subs.iter() {
            let recent = LIVE_RUN_LIKE.contains(&s.status.as_str()) || now - s.updated < 3 * MS_PER_HOUR;
            if recent {
                show.insert(&s.thread);
                if let Some(c) = &s.child {
                    show.insert(c);
                }
            }
        }
        for w in &t.waiting {
            show.insert(&w.thread);
        }
    }
    for (id, t) in threads {
        if !t.archived && now - t.updated < 3 * MS_PER_HOUR {
            show.insert(id);
        }
    }
    for (th, last) in &last_req {
        if now - last < 2 * MS_PER_HOUR {
            show.insert(th);
        }
    }
    let mut frontier: Vec<&str> = show.iter().copied().collect();
    while let Some(id) = frontier.pop() {
        if let Some(t) = threads.get(id) {
            for up in [t.parent.as_deref(), t.launcher.as_deref()].into_iter().flatten() {
                if threads.contains_key(up) && show.insert(up) {
                    frontier.push(up);
                }
            }
        }
    }

    let active_by_thread: HashMap<&str, &ActiveRun> =
        t3.map(|t| t.active.iter().map(|r| (r.thread.as_str(), r)).collect()).unwrap_or_default();
    let waiting_by_thread: HashMap<&str, &Waiting> =
        t3.map(|t| t.waiting.iter().map(|w| (w.thread.as_str(), w)).collect()).unwrap_or_default();

    let mut out_threads: Vec<ThreadOut> = show
        .iter()
        .filter_map(|id| threads.get(*id))
        .map(|t| ThreadOut {
            id: &t.id,
            title: &t.title,
            project: t.project.as_deref(),
            parent: t.parent.as_deref(),
            launcher: t.launcher.as_deref(),
            rel: t.rel.as_deref(),
            model: t.model.as_deref(),
            effort: t.effort.as_deref(),
            fast: t.fast,
            source: t.source.as_deref(),
            created: t.created,
            updated: t.updated,
            settled: t.settled,
            snoozed: t.snoozed_until,
            reset_at: t.reset_at,
            branch: t.branch.as_deref(),
            archived: t.archived,
            prs: t.prs.len(),
            pr: t.prs.first(),
            run: active_by_thread.get(t.id.as_str()).copied(),
            ctx: t3.and_then(|x| x.ctx.get(&t.id)),
            waiting: waiting_by_thread.get(t.id.as_str()).copied(),
            today: by_thread.get(&t.id).cloned(),
            last_req: last_req.get(t.id.as_str()).copied().unwrap_or(0),
        })
        .collect();
    out_threads.sort_by(|a, b| b.updated.cmp(&a.updated));

    let subs: Vec<&Subagent> = t3
        .map(|t| {
            t.subs
                .iter()
                .filter(|s| LIVE_RUN_LIKE.contains(&s.status.as_str()) || now - s.updated < 3 * MS_PER_HOUR)
                .collect()
        })
        .unwrap_or_default();

    // Sessions outside T3 that were active in the last 30 minutes.
    let mut outside = Vec::new();
    let mut note: Option<NotetakerOut> = None;
    for (sid, info) in sh.tx.sessions.iter() {
        if smap.contains_key(sid) || now - info.last > 30 * MS_PER_MIN {
            continue;
        }
        let today_use = tday
            .and_then(|d| d.sessions.get(sid))
            .map(|s| {
                let mut u = Use::default();
                for m in s.models.values() {
                    u.merge(m);
                }
                u
            })
            .unwrap_or_default();
        let app = app_of(sid, Some(info), smap);
        if app == App::Notetaker {
            continue;
        }
        outside.push(OutsideOut {
            id: sid.clone(),
            label: session_label(sid, Some(info)),
            app,
            cwd: info.cwd.as_deref().map(|c| base_name(c).to_string()),
            last: info.last,
            model: info.model.clone(),
            active: now - info.last < 2 * MS_PER_MIN,
            today: today_use,
        });
    }
    outside.sort_by(|a, b| b.last.cmp(&a.last));
    // The note-taker as one aggregated row.
    if let Some(d) = tday {
        let mut u = Use::default();
        let mut last = 0;
        let mut model = None;
        for (sid, s) in &d.sessions {
            if let Some(info) = sh.tx.sessions.get(sid).filter(|i| i.obs) {
                for m in s.models.values() {
                    u.merge(m);
                }
                if s.last > last {
                    last = s.last;
                    model = info.model.clone();
                }
            }
        }
        if u.n > 0 {
            note = Some(NotetakerOut { last, active: now - last < 2 * MS_PER_MIN, model, today: u });
        }
    }

    // Header counters for today.
    let mut td = TodayOut::default();
    if let Some(d) = tday {
        let u = d.total();
        td.tokens = u.t.total();
        td.output = u.t.output;
        td.calls = u.n;
        td.active_min = d.active_slots() * 5;
    }
    let day0 = day_start_ms(today, off);
    td.hits = sh.tx.hits.iter().filter(|h| **h >= day0).count();
    if let Some(t) = t3 {
        let runs_today: Vec<&RunRow> = t.runs.iter().filter(|r| r.requested >= day0).collect();
        td.runs = runs_today.len();
        td.threads = runs_today.iter().map(|r| r.thread.as_str()).collect::<HashSet<_>>().len();
        td.runs_live = t.active.len();
        td.subagents = t.subs.iter().filter(|s| s.started.unwrap_or(s.updated) >= day0).count();
        td.subagents_live = t.subs.iter().filter(|s| LIVE_RUN_LIKE.contains(&s.status.as_str())).count();
        // One PR linked from several threads counts once.
        td.prs = t
            .threads
            .values()
            .flat_map(|th| th.prs.iter())
            .filter(|p| p.linked >= day0)
            .map(|p| (p.repo.as_str(), p.n))
            .collect::<HashSet<_>>()
            .len();
        // Threads waiting on you, each counted once.
        td.waiting = t
            .waiting
            .iter()
            .map(|w| w.thread.as_str())
            .chain(t.active.iter().filter(|r| r.status == "waiting").map(|r| r.thread.as_str()))
            .collect::<HashSet<_>>()
            .len();
    }
    let window = sh.tx.window.as_ref().map(|w| WindowOut {
        est: w,
        pct: w.cap.as_ref().filter(|c| c.used > 0.0 && w.active).map(|c| w.used / c.used),
    });

    let live = Live {
        now,
        off,
        families: FAMILIES,
        health: Health {
            // "ok" only when T3 data is current; the page shows
            // "unavailable" for anything else rather than zeros.
            t3: if !sh.t3_problems.is_empty() {
                "schema"
            } else if sh.t3_error.is_some() || sh.t3.is_none() {
                "down"
            } else {
                "ok"
            },
            t3_error: sh.t3_error.as_deref(),
            problems: &sh.t3_problems,
            warnings: &sh.t3_warnings,
            seq: t3.map(|t| t.seq).unwrap_or(0),
            last_event: t3.map(|t| t.last_event_ms).unwrap_or(0),
            tx_ready: sh.tx.ready,
            files: sh.tx.stats.files,
            bytes_read: sh.tx.stats.bytes_read,
            scan_ms: sh.tx.stats.scan_ms,
            mem: memory_bytes(),
            cpu: (sh.sys.cpu_pct * 10.0).round() / 10.0,
            uptime: sh.sys.started.elapsed().as_secs(),
            launch_links: t3.map(|t| t.launch_links).unwrap_or(0),
        },
        today: td,
        window,
        spark: SparkOut { start: sh.tx.spark_start, step: 5 * MS_PER_MIN, w: &sh.tx.spark, obs: &sh.tx.spark_obs },
        threads: out_threads,
        subs,
        outside,
        notetaker: note,
    };
    serde_json::to_string(&live).unwrap_or_else(|_| "{}".into())
}

// ---------------------------------------------------------------- days

#[derive(Serialize)]
struct SourceOut {
    key: String,
    app: App,
    label: String,
    thread: Option<String>,
    up: Option<String>,
    project: Option<String>,
    total: Use,
    sub: Use,
    models: BTreeMap<String, Use>,
    first: i64,
    last: i64,
    sessions: usize,
}

#[derive(Serialize, Default)]
struct ThreadDay {
    id: String,
    title: String,
    up: Option<String>,
    runs: usize,
    failed: usize,
    run_ms: i64,
}

#[derive(Serialize)]
struct PrOut<'a> {
    n: i64,
    repo: &'a str,
    title: Option<&'a str>,
    state: Option<&'a str>,
    draft: bool,
    checks: Option<&'a str>,
    review: Option<&'a str>,
    thread: &'a str,
    linked: i64,
}

#[derive(Serialize, Default)]
struct T3Day<'a> {
    runs: usize,
    by_status: BTreeMap<String, usize>,
    run_ms: i64,
    threads_touched: usize,
    threads: Vec<ThreadDay>,
    subagents: usize,
    sub_failed: usize,
    sub_ms: i64,
    sub_models: BTreeMap<String, usize>,
    prs: Vec<PrOut<'a>>,
    created: usize,
}

#[derive(Serialize)]
struct DayOut<'a> {
    day: String,
    start: i64,
    offset: i32,
    models: BTreeMap<String, Use>,
    total: Use,
    hours: Vec<[f64; 4]>,
    hours_n: Vec<u32>,
    active_min: u32,
    apps: BTreeMap<&'static str, Use>,
    sources: Vec<SourceOut>,
    sources_more: usize,
    t3: T3Day<'a>,
    outside_subs: BTreeMap<String, u32>,
    hits: Vec<i64>,
    resets: Vec<i64>,
    /// Weighted usage up to the current local time of day.
    by_now: f64,
}

#[derive(Serialize)]
struct Days<'a> {
    now: i64,
    days: Vec<DayOut<'a>>,
}

fn app_key(a: App) -> &'static str {
    match a {
        App::T3 => "t3",
        App::T3code => "t3code",
        App::Desktop => "desktop",
        App::Cli => "cli",
        App::Sdk => "sdk",
        App::Notetaker => "notetaker",
        App::Other => "other",
    }
}

pub fn compose_days(sh: &Shared, now: i64) -> String {
    let off = sh.off;
    let today = day_of(now, off);
    let empty_threads = HashMap::new();
    let empty_map = HashMap::new();
    let t3 = sh.t3.as_ref();
    let threads = t3.map(|t| t.threads.as_ref()).unwrap_or(&empty_threads);
    let smap = t3.map(|t| t.sessions.as_ref()).unwrap_or(&empty_map);
    let blank = DayAgg::default();
    let tod_ms = now - day_start_ms(today, off);
    let mut days = Vec::new();
    for back in 0..3 {
        let day = today - back;
        let start = day_start_ms(day, off);
        let end = start + 24 * MS_PER_HOUR;
        let agg = sh.tx.days.iter().find(|(d, _)| *d == day).map(|(_, a)| a).unwrap_or(&blank);

        // Sources: T3 sessions roll up per thread; the note-taker is one row.
        let mut src: HashMap<String, SourceOut> = HashMap::new();
        let mut apps: BTreeMap<&'static str, Use> = BTreeMap::new();
        for (sid, s) in &agg.sessions {
            let info = sh.tx.sessions.get(sid);
            let app = app_of(sid, info, smap);
            let (key, label, thread, up, project) = match app {
                App::T3 => {
                    let th = smap.get(sid).cloned().unwrap_or_default();
                    let meta = threads.get(&th);
                    let up = meta
                        .and_then(|m| m.parent.as_deref().or(m.launcher.as_deref()))
                        .and_then(|u| threads.get(u))
                        .map(|m| m.title.clone());
                    (
                        format!("t:{th}"),
                        meta.map(|m| m.title.clone()).unwrap_or_else(|| "T3 thread".into()),
                        Some(th),
                        up,
                        meta.and_then(|m| m.project.clone()),
                    )
                }
                App::Notetaker => ("notetaker".into(), "claude-mem note-taker".into(), None, None, None),
                _ => (format!("s:{sid}"), session_label(sid, info), None, None, info.and_then(|i| i.cwd.as_deref().map(|c| base_name(c).to_string()))),
            };
            let e = src.entry(key.clone()).or_insert_with(|| SourceOut {
                key,
                app,
                label,
                thread,
                up,
                project,
                total: Use::default(),
                sub: Use::default(),
                models: BTreeMap::new(),
                first: 0,
                last: 0,
                sessions: 0,
            });
            e.sessions += 1;
            for (m, u) in &s.models {
                e.models.entry(m.clone()).or_default().merge(u);
                e.total.merge(u);
                apps.entry(app_key(app)).or_default().merge(u);
            }
            e.sub.merge(&s.sub);
            e.first = if e.first == 0 { s.first } else { e.first.min(s.first) };
            e.last = e.last.max(s.last);
        }
        let mut sources: Vec<SourceOut> = src.into_values().collect();
        sources.sort_by(|a, b| b.total.w.partial_cmp(&a.total.w).unwrap_or(std::cmp::Ordering::Equal));
        let more = sources.len().saturating_sub(16);
        sources.truncate(16);

        // T3 runs, subagents, PRs for the day.
        let mut td = T3Day::default();
        if let Some(t) = t3 {
            let mut per_thread: HashMap<&str, ThreadDay> = HashMap::new();
            for r in t.runs.iter().filter(|r| r.requested >= start && r.requested < end) {
                td.runs += 1;
                *td.by_status.entry(r.status.clone()).or_default() += 1;
                let ms = match (r.started, r.completed) {
                    (Some(a), Some(b)) if b > a => b - a,
                    (Some(a), None) if r.status == "running" => (now - a).max(0),
                    _ => 0,
                };
                td.run_ms += ms;
                let e = per_thread.entry(&r.thread).or_insert_with(|| {
                    let meta = threads.get(&r.thread);
                    ThreadDay {
                        id: r.thread.clone(),
                        title: meta.map(|m| m.title.clone()).unwrap_or_else(|| "T3 thread".into()),
                        up: meta
                            .and_then(|m| m.parent.as_deref().or(m.launcher.as_deref()))
                            .and_then(|u| threads.get(u))
                            .map(|m| m.title.clone()),
                        ..Default::default()
                    }
                });
                e.runs += 1;
                e.run_ms += ms;
                if r.status == "failed" {
                    e.failed += 1;
                }
            }
            td.threads_touched = per_thread.len();
            let mut tl: Vec<ThreadDay> = per_thread.into_values().collect();
            tl.sort_by(|a, b| b.run_ms.cmp(&a.run_ms).then(b.runs.cmp(&a.runs)));
            tl.truncate(12);
            td.threads = tl;
            for s in t.subs.iter().filter(|s| {
                let st = s.started.unwrap_or(s.updated);
                st >= start && st < end
            }) {
                td.subagents += 1;
                if s.status == "failed" {
                    td.sub_failed += 1;
                }
                if let (Some(a), Some(b)) = (s.started, s.completed) {
                    td.sub_ms += (b - a).max(0);
                }
                *td.sub_models.entry(s.model.clone().unwrap_or_else(|| "unknown".into())).or_default() += 1;
            }
            for th in t.threads.values() {
                if th.created >= start && th.created < end {
                    td.created += 1;
                }
                for p in th.prs.iter().filter(|p| p.linked >= start && p.linked < end) {
                    td.prs.push(PrOut {
                        n: p.n,
                        repo: &p.repo,
                        title: p.title.as_deref(),
                        state: p.state.as_deref(),
                        draft: p.draft,
                        checks: p.checks.as_deref(),
                        review: p.review.as_deref(),
                        thread: &th.title,
                        linked: p.linked,
                    });
                }
            }
            td.prs.sort_by(|a, b| b.linked.cmp(&a.linked));
        }

        // Subagent files from sessions T3 does not know.
        let mut outside_subs: BTreeMap<String, u32> = BTreeMap::new();
        for (parent, m) in &agg.subs {
            if smap.contains_key(parent) {
                continue;
            }
            for (label, n) in m {
                *outside_subs.entry(label.clone()).or_default() += n;
            }
        }

        let hits: Vec<i64> = sh.tx.hits.iter().copied().filter(|h| *h >= start && *h < end).collect();
        let resets: Vec<i64> = sh.anchors().into_iter().filter(|r| *r >= start && *r < end).collect();

        // Usage up to the same local time of day, for comparisons.
        let full_hours = (tod_ms / MS_PER_HOUR) as usize;
        let frac = (tod_ms % MS_PER_HOUR) as f64 / MS_PER_HOUR as f64;
        let mut by_now = 0.0;
        for (h, w) in agg.hours.iter().enumerate() {
            let s: f64 = w.iter().sum();
            if h < full_hours {
                by_now += s;
            } else if h == full_hours {
                by_now += s * frac;
            }
        }

        days.push(DayOut {
            day: day_label(day),
            start,
            offset: off,
            models: agg.models.clone(),
            total: agg.total(),
            hours: agg.hours.clone(),
            hours_n: agg.hours_n.clone(),
            active_min: agg.active_slots() * 5,
            apps,
            sources,
            sources_more: more,
            t3: td,
            outside_subs,
            hits,
            resets,
            by_now,
        });
    }
    serde_json::to_string(&Days { now, days }).unwrap_or_else(|_| "{}".into())
}
