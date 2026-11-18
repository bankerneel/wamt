//! Joins T3 structure with transcript numbers and renders the two JSON
//! payloads the page uses: `live` (now, about once a second) and `days`
//! (today, yesterday, the day before; only when something changed).

use serde::Serialize;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Arc;

use crate::agg::{ColdUse, DayAgg, Use, WeekDay, WindowEst};
use crate::history::{History, MONTH_DAYS};
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
    /// The 7-day strip: daily totals only.
    pub week: WeekPart,
    /// The 30-day strip: history days plus the seven kept days.
    pub month: Vec<MonthDayOut>,
    pub sessions: Arc<HashMap<String, SessionInfo>>,
    pub window: Option<WindowEst>,
    pub spark_start: i64,
    pub spark: Vec<[f64; 4]>,
    pub spark_obs: Vec<f64>,
    pub stats: TailStats,
    /// Limit hits (deduped) inside the 3-day window.
    pub hits: Vec<i64>,
    /// Codex's own limit meter (the latest seen).
    pub codex_limit: Option<crate::codex::CodexLimit>,
    pub ready: bool,
}

/// Daily totals for one of the last 7 days (no per-session detail).
#[derive(Serialize, Clone, Debug, Default, PartialEq)]
pub struct WeekDayOut {
    pub day: String,
    pub start: i64,
    /// Claude totals: tokens, requests, weighted units.
    pub total: Use,
    /// Weighted units per model family (opus, sonnet, haiku, other).
    pub fam: [f64; 4],
    /// Codex tokens that day (Codex has no weight).
    pub codex_tokens: u64,
    /// Weighted units per local hour (all Claude families).
    pub hours: Vec<f64>,
    pub active_min: u32,
}

/// The week as a whole.
#[derive(Serialize, Clone, Debug, Default, PartialEq)]
pub struct WeekInfo {
    /// Weighted units over the 7 days.
    pub total_w: f64,
    pub codex_tokens: u64,
    /// Weekly-limit hits in the window. The page anchors a cap to a hit that
    /// falls inside the calibrated weekly window; no cap is computed here.
    pub hits: Vec<i64>,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct WeekPart {
    pub days: Vec<WeekDayOut>,
    pub info: WeekInfo,
}

/// One day of the month strip: the same totals-only fields the history file
/// keeps, with the day as a label the page can read directly.
#[derive(Serialize, Clone, Debug, Default, PartialEq)]
pub struct MonthDayOut {
    pub day: String,
    /// input, output, cache read, cache write 5m, cache write 1h.
    pub t: [u64; 5],
    pub n: u64,
    pub w: f64,
    /// Weighted units per model family: opus, sonnet, haiku, other.
    pub fam: [f64; 4],
    pub codex_tokens: u64,
    pub active_min: u32,
}

/// History days plus the seven kept days, oldest first, at most a month.
/// The seven-day part is built from the same summaries as `week`, so the
/// month and the strip always agree on today's numbers.
pub fn build_month(history: &History, week: &WeekPart, today: i32) -> Vec<MonthDayOut> {
    let mut out: Vec<MonthDayOut> = history
        .days
        .iter()
        .filter(|d| d.day < today - (crate::agg::WEEK_DAYS - 1))
        .map(|d| MonthDayOut {
            day: day_label(d.day),
            t: d.t,
            n: d.n,
            w: round2(d.w),
            fam: d.fam.map(round2),
            codex_tokens: d.codex_tokens,
            active_min: d.active_min,
        })
        .collect();
    for d in &week.days {
        out.push(MonthDayOut {
            day: d.day.clone(),
            t: [d.total.t.input, d.total.t.output, d.total.t.cache_read, d.total.t.cache_w5m, d.total.t.cache_w1h],
            n: d.total.n,
            w: round2(d.total.w),
            fam: d.fam,
            codex_tokens: d.codex_tokens,
            active_min: d.active_min,
        });
    }
    out.sort_by(|a, b| a.day.cmp(&b.day));
    out.dedup_by(|a, b| a.day == b.day);
    if out.len() > MONTH_DAYS as usize {
        let extra = out.len() - MONTH_DAYS as usize;
        out.drain(..extra);
    }
    out
}

fn round2(x: f64) -> f64 {
    (x * 100.0).round() / 100.0
}

/// Short display forms for the tip lines: "612K", "1.2M".
fn big_num(n: f64) -> String {
    let n = n.max(0.0);
    let (v, unit) = if n >= 1e6 { (n / 1e6, "M") } else if n >= 1e3 { (n / 1e3, "K") } else { (n, "") };
    let s = if v >= 10.0 || unit.is_empty() { (v.round() as u64).to_string() } else { format!("{v:.1}") };
    s + unit
}
/// Weights are list-price micro-dollars, so dollars are weight / 1e6.
fn usd_s(w: f64) -> String {
    format!("${:.2}", w / 1e6)
}
fn pct_s(share: f64) -> String {
    format!("{}%", (share * 100.0).round() as i64)
}
/// Names on a muted line, folded so eight copies of one observer read "x8".
fn join_names(names: &[String]) -> String {
    let mut counts: Vec<(&String, usize)> = Vec::new();
    for n in names {
        if let Some(e) = counts.iter_mut().find(|(k, _)| *k == n) {
            e.1 += 1;
        } else {
            counts.push((n, 1));
        }
    }
    counts.into_iter().map(|(n, c)| if c > 1 { format!("{n} x{c}") } else { n.clone() }).collect::<Vec<_>>().join(", ")
}

/// Builds the 7-day payload from compact day summaries (oldest first).
/// `codex` maps a local day to Codex tokens that day.
pub fn build_week(claude: &[WeekDay], codex: &HashMap<i32, u64>, hits: &[i64], off: i32) -> WeekPart {
    let mut info = WeekInfo { hits: hits.to_vec(), ..Default::default() };
    let days: Vec<WeekDayOut> = claude
        .iter()
        .map(|d| {
            let cx = codex.get(&d.day).copied().unwrap_or(0);
            info.total_w += d.total.w;
            info.codex_tokens += cx;
            WeekDayOut {
                day: day_label(d.day),
                start: day_start_ms(d.day, off),
                total: d.total.clone(),
                fam: d.fam.map(round2),
                codex_tokens: cx,
                hours: d.hours.iter().map(|h| round2(*h)).collect(),
                active_min: d.active_min,
            }
        })
        .collect();
    WeekPart { days, info }
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
    /// A Codex session T3 does not know (the ones it knows count as T3).
    Codex,
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
        Some(crate::codex::EP) => App::Codex,
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
    /// Weighted usage in roughly the last 30 minutes (today's 10-minute
    /// buckets that overlap it), for the runaway alert.
    #[serde(skip_serializing_if = "is_zero")]
    eat30: f64,
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
    /// Codex's own limit meter, when Codex is read and has reported one.
    codex_limit: Option<&'a crate::codex::CodexLimit>,
}

#[derive(Serialize)]
struct SparkOut<'a> {
    start: i64,
    step: i64,
    w: &'a [[f64; 4]],
    obs: &'a [f64],
}

fn is_zero(v: &f64) -> bool {
    *v == 0.0
}

/// Weight per thread in the 10-minute buckets overlapping the last 30 minutes.
fn recent_by_thread(day: Option<&DayAgg>, map: &HashMap<String, String>, day_start: i64, now: i64) -> HashMap<String, f64> {
    let mut out: HashMap<String, f64> = HashMap::new();
    let Some(d) = day else { return out };
    let from = now - 30 * MS_PER_MIN;
    for (sid, s) in &d.sessions {
        let Some(th) = map.get(sid) else { continue };
        let w: f64 = s
            .buckets
            .iter()
            .filter(|(slot, _)| day_start + (**slot as i64 + 1) * 10 * MS_PER_MIN > from)
            .map(|(_, b)| b.w.iter().sum::<f64>())
            .sum();
        if w > 0.0 {
            *out.entry(th.clone()).or_default() += w;
        }
    }
    out
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
    let recent = recent_by_thread(tday, smap, day_start_ms(today, off), now);

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
            eat30: recent.get(&t.id).copied().unwrap_or(0.0),
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
        codex_limit: sh.tx.codex_limit.as_ref(),
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
    cold: ColdUse,
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

/// One line of live advice for a day, ordered by the weight it accounts for.
#[derive(Serialize, Clone, Debug, Default, PartialEq)]
pub struct TipOut {
    pub kind: &'static str,
    pub label: &'static str,
    /// The one-line message, including the share and the dollar estimate.
    pub text: String,
    /// Muted second line naming the threads or sessions involved.
    pub who: String,
    /// Weighted units this tip is about; tips are ordered by this.
    pub w: f64,
    pub share: f64,
    pub usd: f64,
    /// Rule explained in plain text plus <b> and <br> only.
    pub tip: &'static str,
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
    /// Spoiled leftovers: cache rebuilt after the cache expired.
    cold: ColdUse,
    /// Where this day could be trimmed, most costly first, at most five.
    tips: Vec<TipOut>,
}

#[derive(Serialize)]
struct Days<'a> {
    now: i64,
    days: Vec<DayOut<'a>>,
    week: &'a [WeekDayOut],
    week_info: &'a WeekInfo,
    /// Oldest first: history days plus the seven kept days, at most 30.
    month: &'a [MonthDayOut],
    /// USD per million tokens (input, output, cache read) for each priced
    /// model in these days; the bill's API-equivalent view explains itself
    /// with these. Unpriced (Codex) models are left out.
    prices: BTreeMap<String, [f64; 3]>,
}

fn app_key(a: App) -> &'static str {
    match a {
        App::T3 => "t3",
        App::T3code => "t3code",
        App::Desktop => "desktop",
        App::Cli => "cli",
        App::Sdk => "sdk",
        App::Notetaker => "notetaker",
        App::Codex => "codex",
        App::Other => "other",
    }
}

/// Builds the per-day "diet tips" from aggregates already held in memory.
/// No new storage and no re-reading of transcripts. `sources` is the day's
/// displayed source list, `apps` the full per-app totals (not truncated), and
/// `labels` maps a session id to a readable thread or session name.
fn compose_tips(
    agg: &DayAgg,
    sources: &[SourceOut],
    apps: &BTreeMap<&'static str, Use>,
    labels: &HashMap<String, String>,
    day_w: f64,
    is_today: bool,
    // claude-mem note-taker sessions: tiny, many and short-lived, so they
    // must not count as "too many at once" or as marathons.
    is_obs: &dyn Fn(&str) -> bool,
) -> Vec<TipOut> {
    let share = |w: f64| if day_w > 0.0 { w / day_w } else { 0.0 };
    let name = |sid: &str| labels.get(sid).cloned().unwrap_or_else(|| sid.chars().take(8).collect());
    let mut out: Vec<TipOut> = Vec::new();

    // 1. Heavy context: input and cache are resent on every request.
    let mut heavy: Vec<(f64, f64, String)> = Vec::new();
    for s in sources {
        if s.total.n == 0 {
            continue;
        }
        let ctx = s.total.t.input + s.total.t.cache_read + s.total.t.cache_w5m + s.total.t.cache_w1h;
        let avg = ctx as f64 / s.total.n as f64;
        if avg > 300_000.0 {
            heavy.push((s.total.w, avg, s.label.clone()));
        }
    }
    heavy.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
    heavy.truncate(3);
    if !heavy.is_empty() {
        let w: f64 = heavy.iter().map(|h| h.0).sum();
        let avg = heavy.iter().map(|h| h.1).sum::<f64>() / heavy.len() as f64;
        out.push(TipOut {
            kind: "context",
            label: "Heavy context",
            text: format!("start fresh when you switch tasks: {} context tokens per request on average, {} of the day, about {}", big_num(avg), pct_s(share(w)), usd_s(w)),
            who: heavy.iter().map(|h| format!("{}: {} per request", h.2, big_num(h.1))).collect::<Vec<_>>().join(", "),
            w,
            share: share(w),
            usd: w / 1e6,
            tip: "<b>Start fresh when you switch tasks.</b><br>These sources averaged over 300K input and cache tokens per request. A long context is resent on every call, so a fresh thread or a compaction makes every later request cheaper. The biggest lever here.",
        });
    }

    // 2. Too many at once: sessions with a request in the same 10-minute slot.
    // Buckets exist for today only.
    if is_today {
        let mut slots: BTreeMap<u16, Vec<String>> = BTreeMap::new();
        let mut slot_w: HashMap<u16, f64> = HashMap::new();
        for (sid, s) in agg.sessions.iter().filter(|(sid, _)| !is_obs(sid)) {
            let w: f64 = s.models.values().map(|u| u.w).sum();
            for slot in s.buckets.keys() {
                slots.entry(*slot).or_default().push(sid.clone());
                *slot_w.entry(*slot).or_default() += w;
            }
        }
        if let Some((&slot, sids)) = slots.iter().max_by(|a, b| a.1.len().cmp(&b.1.len()).then(b.0.cmp(a.0))) {
            if sids.len() >= 6 {
                let w = slot_w.get(&slot).copied().unwrap_or(0.0);
                let min = slot as i64 * 10;
                out.push(TipOut {
                    kind: "concurrency",
                    label: "Too many at once",
                    text: format!("{} sessions were active at once at {:02}:{:02}; together they ate {} of the day, about {}", sids.len(), min / 60, min % 60, pct_s(share(w)), usd_s(w)),
                    who: join_names(&sids.iter().map(|s| name(s)).collect::<Vec<_>>()),
                    w,
                    share: share(w),
                    usd: w / 1e6,
                    tip: "<b>Run fewer sessions at once.</b><br>This is the most sessions with a request in the same 10-minute slot today. Parallel sessions each carry their own context, so overlapping them multiplies what gets resent.",
                });
            }
        }
    }

    // 3. Marathon sessions: eight distinct hours today, or an eight-hour span
    // on older days whose 10-minute buckets have been dropped.
    let mut marathon: Vec<(f64, String, String)> = Vec::new();
    for (sid, s) in agg.sessions.iter().filter(|(sid, _)| !is_obs(sid)) {
        let w: f64 = s.models.values().map(|u| u.w).sum();
        if w <= 0.0 {
            continue;
        }
        if is_today {
            let hours: std::collections::BTreeSet<u16> = s.buckets.keys().map(|slot| slot / 6).collect();
            if hours.len() >= 8 {
                marathon.push((w, name(sid), format!("in {} hours", hours.len())));
            }
        } else if s.last > s.first && (s.last - s.first) >= 8 * MS_PER_HOUR as i64 {
            let h = (s.last - s.first) / MS_PER_HOUR as i64;
            marathon.push((w, name(sid), format!("spanning {h} hours")));
        }
    }
    if !marathon.is_empty() {
        let w: f64 = marathon.iter().map(|m| m.0).sum();
        let pre = if is_today { "in" } else { "spanning" };
        out.push(TipOut {
            kind: "marathon",
            label: "Marathon sessions",
            text: format!("{} sessions with requests {} 8 or more hours, {} of the day, about {}", marathon.len(), pre, pct_s(share(w)), usd_s(w)),
            who: marathon.iter().map(|m| format!("{} {}", m.1, m.2)).collect::<Vec<_>>().join(", "),
            w,
            share: share(w),
            usd: w / 1e6,
            tip: "<b>Review sessions active over many hours.</b><br>A session that keeps working across the day resends its growing context each time. Starting a fresh thread at a task change keeps contexts short.",
        });
    }

    // 4. Opus subagents. `SessionDay.sub` is a total across models, so the
    // subagent share of each session's weight is applied to its Opus models
    // to estimate the Opus subagent tokens and their weight.
    let mut opus_sub_w = 0.0;
    let mut opus_sub_t = crate::transcript::Tokens::default();
    let mut opus_who: Vec<String> = Vec::new();
    for (sid, s) in &agg.sessions {
        let sw: f64 = s.models.values().map(|u| u.w).sum();
        if sw <= 0.0 || s.sub.w <= 0.0 {
            continue;
        }
        let frac = (s.sub.w / sw).min(1.0);
        let mut hit = false;
        for (m, u) in &s.models {
            if crate::transcript::family(m) != 0 {
                continue;
            }
            hit = true;
            opus_sub_w += u.w * frac;
            let t = &u.t;
            opus_sub_t.input += (t.input as f64 * frac).round() as u64;
            opus_sub_t.output += (t.output as f64 * frac).round() as u64;
            opus_sub_t.cache_read += (t.cache_read as f64 * frac).round() as u64;
            opus_sub_t.cache_w5m += (t.cache_w5m as f64 * frac).round() as u64;
            opus_sub_t.cache_w1h += (t.cache_w1h as f64 * frac).round() as u64;
        }
        if hit {
            opus_who.push(name(sid));
        }
    }
    if day_w > 0.0 && opus_sub_w / day_w > 0.05 {
        let sonnet_w = crate::transcript::weight("claude-sonnet-5-5", &opus_sub_t, false);
        let save = (opus_sub_w - sonnet_w).max(0.0);
        out.push(TipOut {
            kind: "opus",
            label: "Opus subagents",
            text: format!("subagents used Opus for {} of the day, about {} less on Sonnet", pct_s(share(opus_sub_w)), usd_s(save)),
            who: opus_who.join(", "),
            w: opus_sub_w,
            share: share(opus_sub_w),
            usd: opus_sub_w / 1e6,
            tip: "<b>Check which models your subagents use.</b><br>This is the weight of subagent requests on Opus models, and what the same tokens would have cost at Sonnet list prices. Explore and review agents rarely need Opus.",
        });
    }

    // 5. Spoiled leftovers: cache rebuilt after it had expired.
    let mut cold: Vec<(f64, u32, u64, String)> = Vec::new();
    for s in sources {
        if s.cold.n > 0 && s.cold.w > 0.0 {
            cold.push((s.cold.w, s.cold.n, s.cold.tok, s.label.clone()));
        }
    }
    cold.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
    cold.truncate(2);
    if !cold.is_empty() {
        let w: f64 = cold.iter().map(|c| c.0).sum();
        let n: u32 = cold.iter().map(|c| c.1).sum();
        let tok: u64 = cold.iter().map(|c| c.2).sum();
        out.push(TipOut {
            kind: "cold",
            label: "Spoiled leftovers",
            text: format!("{} cold restart{} rebuilt {} tokens, {} of the day, about {}", n, if n == 1 { "" } else { "s" }, big_num(tok as f64), pct_s(share(w)), usd_s(w)),
            who: cold.iter().map(|c| format!("{}: {} rebuild{}", c.3, c.1, if c.1 == 1 { "" } else { "s" })).collect::<Vec<_>>().join(", "),
            w,
            share: share(w),
            usd: w / 1e6,
            tip: "<b>Keep the prompt cache warm.</b><br>These requests rebuilt more than 50K tokens of cache after it had expired. Finishing a task before the cache goes cold, or resuming sooner, avoids paying twice.",
        });
    }

    // 6. The note-taker's own share of the day.
    let nw = apps.get("notetaker").map(|u| u.w).unwrap_or(0.0);
    if day_w > 0.0 && nw / day_w > 0.03 {
        out.push(TipOut {
            kind: "notetaker",
            label: "Note-taker",
            text: format!("the claude-mem note-taker used {} of the day, about {}", pct_s(share(nw)), usd_s(nw)),
            who: "claude-mem note-taker".into(),
            w: nw,
            share: share(nw),
            usd: nw / 1e6,
            tip: "<b>The note-taker counts against your plan too.</b><br>claude-mem's observer sessions run through the same login. Its share is shown here so you can decide whether to trim it.",
        });
    }

    out.sort_by(|a, b| b.w.partial_cmp(&a.w).unwrap_or(std::cmp::Ordering::Equal));
    out.truncate(5);
    out
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
        let total = agg.total();

        // Sources: T3 sessions roll up per thread; the note-taker is one row.
        let mut src: HashMap<String, SourceOut> = HashMap::new();
        let mut apps: BTreeMap<&'static str, Use> = BTreeMap::new();
        let mut cold_day = ColdUse::default();
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
                cold: ColdUse::default(),
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
            e.cold.merge(&s.cold);
            cold_day.merge(&s.cold);
            e.first = if e.first == 0 { s.first } else { e.first.min(s.first) };
            e.last = e.last.max(s.last);
        }
        // Readable names for the sessions behind a tip: the T3 thread title
        // when the session was matched, otherwise its title, folder or id.
        let mut labels: HashMap<String, String> = HashMap::new();
        for sid in agg.sessions.keys() {
            let info = sh.tx.sessions.get(sid);
            let label = match smap.get(sid) {
                Some(th) => threads.get(th).map(|m| m.title.clone()).unwrap_or_else(|| session_label(sid, info)),
                None => session_label(sid, info),
            };
            labels.insert(sid.clone(), label);
        }

        let mut sources: Vec<SourceOut> = src.into_values().collect();
        sources.sort_by(|a, b| {
            b.total.w.partial_cmp(&a.total.w).unwrap_or(std::cmp::Ordering::Equal).then(b.total.t.output.cmp(&a.total.t.output))
        });
        // Rows with no weight (Codex) sort last; keep the biggest two so
        // a busy Claude day cannot push them off the list.
        let (mut sources, unweighted): (Vec<SourceOut>, Vec<SourceOut>) = sources.into_iter().partition(|s| s.total.w > 0.0);
        let total_sources = sources.len() + unweighted.len();
        sources.truncate(14);
        sources.extend(unweighted.into_iter().take(if sources.len() < 14 { 16 - sources.len() } else { 2 }));
        let more = total_sources - sources.len();

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

        let is_obs = |sid: &str| sh.tx.sessions.get(sid).is_some_and(|i| i.obs);
        let tips = compose_tips(agg, &sources, &apps, &labels, total.w, back == 0, &is_obs);
        days.push(DayOut {
            day: day_label(day),
            start,
            offset: off,
            models: agg.models.clone(),
            total: total.clone(),
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
            cold: cold_day,
            tips,
        });
    }
    let mut prices = BTreeMap::new();
    for d in &days {
        for (m, u) in &d.models {
            if u.w > 0.0 && !prices.contains_key(m) {
                let (i, o, r) = crate::transcript::price(m);
                prices.insert(m.clone(), [i, o, r]);
            }
        }
    }
    serde_json::to_string(&Days { now, days, week: &sh.tx.week.days, week_info: &sh.tx.week.info, month: &sh.tx.month, prices }).unwrap_or_else(|_| "{}".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recent_by_thread_counts_buckets_overlapping_30_minutes() {
        use crate::agg::{Bucket, SessionDay};
        let mut d = DayAgg::default();
        let mut sd = SessionDay::default();
        // Slots are 10 minutes from the local day start (0 here).
        for (slot, w) in [(0u16, 1.0), (3, 2.0), (4, 4.0), (5, 8.0)] {
            sd.buckets.insert(slot, Bucket { w: [w, 0.0, 0.0, 0.0], out: 0 });
        }
        d.sessions.insert("s1".into(), sd);
        let map: HashMap<String, String> = [("s1".to_string(), "th".to_string())].into();
        // Now 00:55: the last 30 minutes start 00:25, so slots 2 (00:20) to 5 count.
        let r = recent_by_thread(Some(&d), &map, 0, 55 * MS_PER_MIN);
        assert_eq!(r["th"], 14.0);
    }
    use crate::agg::{Agg, UsageIn, WEEK_DAYS};
    use crate::timeutil::now_ms;
    use crate::transcript::Tokens;

    const OFF: i32 = 330;

    fn add(a: &mut Agg, ts: i64, model: &str, out: u64, id: &str) {
        let t = Tokens { output: out, ..Default::default() };
        a.add_usage(
            UsageIn { session: "s", sub: false, observer: false, ts_ms: ts, model, tokens: &t, fast: false, msg_id: Some(id), prev_ms: 0, codex: false },
            OFF,
        );
    }

    #[test]
    fn week_payload_has_seven_days_oldest_first() {
        let mut a = Agg::default();
        let now = now_ms();
        let today = day_of(now, OFF);
        add(&mut a, day_start_ms(today, OFF) + 5 * MS_PER_HOUR, "claude-opus-5-5", 1000, "t1");
        add(&mut a, day_start_ms(today - 5, OFF) + 2 * MS_PER_HOUR, "claude-sonnet-5-5", 500, "t2");
        add(&mut a, day_start_ms(today - 5, OFF) + 3 * MS_PER_HOUR, "claude-haiku-5-5", 500, "t3");
        let mut cx = HashMap::new();
        cx.insert(today - 1, 777u64);
        let w = build_week(&a.week(today), &cx, &[], OFF);
        assert_eq!(w.days.len() as i32, WEEK_DAYS);
        assert_eq!(w.days[6].start, day_start_ms(today, OFF));
        assert_eq!(w.days[0].start, day_start_ms(today - 6, OFF));
        assert_eq!(w.days[6].total.n, 1);
        assert_eq!(w.days[1].total.n, 2);
        assert!(w.days[1].fam[1] > 0.0 && w.days[1].fam[2] > 0.0 && w.days[1].fam[0] == 0.0);
        assert_eq!(w.days[5].codex_tokens, 777);
        assert_eq!(w.info.codex_tokens, 777);
        assert_eq!(w.days[0].hours.len(), 24);
        assert!(w.info.hits.is_empty());
        let sum: f64 = w.days.iter().map(|d| d.total.w).sum();
        assert!((w.info.total_w - sum).abs() < 1e-9 && sum > 0.0);
        // Serialises with the keys the page reads.
        let v: serde_json::Value = serde_json::to_value(&w.days[6]).unwrap();
        for k in ["day", "total", "fam", "codex_tokens", "hours", "active_min"] {
            assert!(v.get(k).is_some(), "{k}");
        }
        assert!(v["total"].get("t").is_some() && v["total"].get("n").is_some() && v["total"].get("w").is_some());
    }

    #[test]
    fn weekly_hits_are_sent_without_a_server_cap() {
        let mut a = Agg::default();
        let now = now_ms();
        let today = day_of(now, OFF);
        add(&mut a, day_start_ms(today, OFF) + MS_PER_HOUR, "claude-opus-5-5", 1000, "b");
        // A weekly hit is kept apart from the 5-hour hits; no cap is measured
        // on the server because the weekly limit has a fixed reset, not a
        // rolling 7-day window.
        assert!(a.add_weekly_hit(day_start_ms(today, OFF) + 3 * MS_PER_HOUR));
        assert!(a.hits.is_empty());
        let w = build_week(&a.week(today), &HashMap::new(), &a.week_hits, OFF);
        assert_eq!(w.info.hits.len(), 1);
        let v: serde_json::Value = serde_json::to_value(&w.info).unwrap();
        assert!(v.get("hits").is_some());
        assert!(v.get("cap").is_none() && v.get("cap_hit").is_none());
    }

    #[test]
    fn month_has_history_plus_kept_days_capped_at_thirty() {
        use crate::history::{HistDay, History, HISTORY_VERSION, MONTH_DAYS};
        let mut a = Agg::default();
        let now = now_ms();
        let today = day_of(now, OFF);
        add(&mut a, day_start_ms(today, OFF) + MS_PER_HOUR, "claude-opus-5-5", 10, "t");
        let week = build_week(&a.week(today), &HashMap::new(), &[], OFF);
        let mut h = History { v: HISTORY_VERSION, days: Vec::new() };
        for back in 8..=32 {
            h.upsert(HistDay {
                day: today - back,
                n: back as u64,
                t: [1, 2, 3, 4, 5],
                w: 7.0,
                fam: [1.0, 0.0, 0.0, 0.0],
                codex_tokens: 3,
                active_min: 5,
            });
        }
        let m = build_month(&h, &week, today);
        assert_eq!(m.len(), MONTH_DAYS as usize);
        assert_eq!(m[0].day, day_label(today - 30));
        assert_eq!(m[m.len() - 1].day, day_label(today));
        assert_eq!(m[m.len() - 1].t[1], 10);
        assert_eq!(m[0].codex_tokens, 3);
        let v: serde_json::Value = serde_json::to_value(&m[0]).unwrap();
        for k in ["day", "t", "n", "w", "fam", "codex_tokens", "active_min"] {
            assert!(v.get(k).is_some(), "{k}");
        }
    }

    // ---------------------------------------------------------------- tips
    use crate::agg::{Bucket, SessionDay};

    fn tip_source(label: &str, ctx: u64, n: u64, w: f64, cold: (u32, u64, f64)) -> SourceOut {
        SourceOut {
            key: label.into(),
            app: App::T3,
            label: label.into(),
            thread: None,
            up: None,
            project: None,
            total: Use { t: Tokens { input: ctx, ..Default::default() }, n, w },
            sub: Use::default(),
            cold: ColdUse { n: cold.0, tok: cold.1, w: cold.2 },
            models: BTreeMap::new(),
            first: 0,
            last: 0,
            sessions: 1,
        }
    }

    #[test]
    fn tips_heavy_context_lists_three_biggest_over_threshold() {
        let sources = vec![
            tip_source("A", 700_000 * 10, 10, 500.0, (0, 0, 0.0)),
            tip_source("B", 500_000 * 10, 10, 400.0, (0, 0, 0.0)),
            tip_source("C", 400_000 * 10, 10, 300.0, (0, 0, 0.0)),
            tip_source("D", 350_000 * 10, 10, 200.0, (0, 0, 0.0)),
            tip_source("E", 100_000 * 10, 10, 900.0, (0, 0, 0.0)),
        ];
        let tips = compose_tips(&DayAgg::default(), &sources, &BTreeMap::new(), &HashMap::new(), 1000.0, true, &|_| false);
        assert_eq!(tips.len(), 1);
        assert_eq!(tips[0].kind, "context");
        assert!(tips[0].who.contains("A") && tips[0].who.contains("B") && tips[0].who.contains("C"));
        assert!(!tips[0].who.contains("D") && !tips[0].who.contains("E"));
        assert!((tips[0].w - 1200.0).abs() < 1e-9);
        assert!(tips[0].text.contains("533K") && tips[0].text.contains("$"), "{}", tips[0].text);
        assert!(tips[0].who.contains("700K per request"), "{}", tips[0].who);
    }

    #[test]
    fn tips_concurrency_needs_six_sessions_in_one_slot() {
        let mut d = DayAgg::default();
        for i in 0..5 {
            let mut s = SessionDay::default();
            s.buckets.insert(3, Bucket { w: [1.0, 0.0, 0.0, 0.0], out: 0 });
            d.sessions.insert(format!("s{i}"), s);
        }
        let tips = compose_tips(&d, &[], &BTreeMap::new(), &HashMap::new(), 100.0, true, &|_| false);
        assert!(tips.iter().all(|t| t.kind != "concurrency"));
        let mut s6 = SessionDay::default();
        s6.buckets.insert(3, Bucket::default());
        d.sessions.insert("s5".into(), s6);
        let tips = compose_tips(&d, &[], &BTreeMap::new(), &HashMap::new(), 100.0, true, &|_| false);
        let t = tips.iter().find(|t| t.kind == "concurrency").expect("six sessions in one slot");
        assert!(t.text.contains("6 sessions") && t.text.contains("00:30"), "{}", t.text);
        let tips = compose_tips(&d, &[], &BTreeMap::new(), &HashMap::new(), 100.0, false, &|_| false);
        assert!(tips.iter().all(|t| t.kind != "concurrency"));
        // Note-taker sessions do not count toward the six.
        let tips = compose_tips(&d, &[], &BTreeMap::new(), &HashMap::new(), 100.0, true, &|sid| sid == "s5");
        assert!(tips.iter().all(|t| t.kind != "concurrency"));
    }

    #[test]
    fn tips_marathon_uses_hours_today_and_span_for_earlier_days() {
        let mut d = DayAgg::default();
        let mut s = SessionDay::default();
        for slot in [0u16, 6, 12, 18, 24, 30, 36, 42] {
            s.buckets.insert(slot, Bucket::default());
        }
        s.models.insert("claude-opus-5-5".into(), Use { w: 50.0, ..Default::default() });
        d.sessions.insert("s1".into(), s);
        let labels: HashMap<String, String> = [("s1".to_string(), "Alpha".to_string())].into();
        let tips = compose_tips(&d, &[], &BTreeMap::new(), &labels, 100.0, true, &|_| false);
        let m = tips.iter().find(|t| t.kind == "marathon").unwrap();
        assert!(m.who.contains("Alpha") && m.who.contains("in 8 hours"), "{}", m.who);
        for sd in d.sessions.values_mut() {
            sd.buckets.clear();
            sd.first = 1_000_000;
            sd.last = 1_000_000 + 9 * MS_PER_HOUR;
        }
        let tips = compose_tips(&d, &[], &BTreeMap::new(), &labels, 100.0, false, &|_| false);
        let m = tips.iter().find(|t| t.kind == "marathon").unwrap();
        assert!(m.who.contains("spanning 9 hours"), "{}", m.who);
    }

    #[test]
    fn tips_opus_subagents_only_over_five_percent() {
        let mut d = DayAgg::default();
        let mut s = SessionDay::default();
        let mut u = Use::default();
        u.t.input = 1_000_000;
        u.w = crate::transcript::weight("claude-opus-5-5", &u.t, false);
        s.models.insert("claude-opus-5-5".into(), u.clone());
        s.sub = u.clone();
        d.sessions.insert("s1".into(), s);
        let tips = compose_tips(&d, &[], &BTreeMap::new(), &HashMap::new(), u.w * 100.0, true, &|_| false);
        assert!(tips.iter().all(|t| t.kind != "opus"));
        let tips = compose_tips(&d, &[], &BTreeMap::new(), &HashMap::new(), u.w * 10.0, true, &|_| false);
        let t = tips.iter().find(|t| t.kind == "opus").unwrap();
        assert!(t.text.contains("less on Sonnet") && t.usd > 0.0, "{}", t.text);
    }

    #[test]
    fn tips_notetaker_and_cold_have_thresholds() {
        let apps: BTreeMap<&'static str, Use> = [("notetaker", Use { w: 40.0, ..Default::default() })].into();
        let sources = vec![
            tip_source("Cold A", 10_000, 1, 100.0, (3, 150_000, 10.0)),
            tip_source("Cold B", 10_000, 1, 90.0, (1, 60_000, 5.0)),
        ];
        let tips = compose_tips(&DayAgg::default(), &sources, &apps, &HashMap::new(), 1000.0, true, &|_| false);
        let cold = tips.iter().find(|t| t.kind == "cold").unwrap();
        assert!(cold.who.contains("Cold A") && cold.who.contains("Cold B"));
        assert!(cold.text.contains("4 cold restarts"), "{}", cold.text);
        assert!(tips.iter().any(|t| t.kind == "notetaker"));
        let apps2: BTreeMap<&'static str, Use> = [("notetaker", Use { w: 20.0, ..Default::default() })].into();
        let tips = compose_tips(&DayAgg::default(), &[], &apps2, &HashMap::new(), 1000.0, true, &|_| false);
        assert!(tips.iter().all(|t| t.kind != "notetaker"));
    }

    #[test]
    fn tips_are_capped_at_five_and_ordered_by_cost() {
        let mut d = DayAgg::default();
        for i in 0..6 {
            let mut s = SessionDay::default();
            s.buckets.insert(3, Bucket { w: [10.0, 0.0, 0.0, 0.0], out: 0 });
            if i == 0 {
                for slot in [0u16, 6, 12, 18, 24, 30, 36, 42] {
                    s.buckets.insert(slot, Bucket::default());
                }
                let mut u = Use::default();
                u.t.input = 500_000;
                u.w = 60.0;
                s.models.insert("claude-opus-5-5".into(), u.clone());
                s.sub = u;
            }
            d.sessions.insert(format!("s{i}"), s);
        }
        let sources = vec![
            tip_source("Heavy A", 400_000, 1, 300.0, (2, 100_000, 20.0)),
            tip_source("Heavy B", 350_000, 1, 200.0, (0, 0, 0.0)),
        ];
        let apps: BTreeMap<&'static str, Use> = [("notetaker", Use { w: 40.0, ..Default::default() })].into();
        let tips = compose_tips(&d, &sources, &apps, &HashMap::new(), 1000.0, true, &|_| false);
        assert_eq!(tips.len(), 5, "{:?}", tips.iter().map(|t| t.kind).collect::<Vec<_>>());
        assert_eq!(tips[0].kind, "context");
        assert!(tips.iter().all(|t| t.kind != "cold"));
        for w in tips.windows(2) {
            assert!(w[0].w >= w[1].w);
        }
        let none = compose_tips(&DayAgg::default(), &[], &BTreeMap::new(), &HashMap::new(), 1000.0, true, &|_| false);
        assert!(none.is_empty());
    }
}
