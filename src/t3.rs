//! Read-only access to T3 Code's state database (`statev2.sqlite`).
//!
//! T3's schema is internal and undocumented, so every query lives here and
//! `schema_problems` checks the tables and columns we depend on. Queries only
//! return ids, titles, statuses, timestamps, model ids, tool names and
//! numbers. Message text, tool inputs and outputs never leave SQLite.
//! Each poll runs inside one short read transaction so T3's WAL checkpoints
//! are never held back for long.

use rusqlite::{params, Connection, OpenFlags, OptionalExtension};
use serde::Serialize;
use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::time::Duration;

use crate::timeutil::{iso_ms, parse_ts_ms, MS_PER_MIN};

const LIVE_RUN_STATUSES: &str = "('queued','preparing','starting','running','waiting')";
const LAUNCH_TOOLS: &str = "('mcp__t3-code__t3_thread_launch','mcp__t3-code__create_threads')";
const STEP_SKIP: &str = "('checkpoint','system_notice','notification')";

/// Tables and columns Switchboard relies on.
const REQUIRED: &[(&str, &[&str])] = &[
    ("orchestration_events", &["sequence", "event_type", "stream_id", "occurred_at", "payload_json"]),
    (
        "orchestration_v2_projection_threads",
        &["thread_id", "project_id", "title", "created_at", "updated_at", "archived_at", "deleted_at", "payload_json", "default_provider"],
    ),
    ("projection_projects", &["project_id", "title"]),
    ("orchestration_v2_projection_runs", &["run_id", "thread_id", "ordinal", "status", "requested_at", "completed_at", "payload_json"]),
    ("orchestration_v2_projection_run_attempts", &["thread_id", "payload_json"]),
    ("orchestration_v2_projection_turn_items", &["thread_id", "run_id", "ordinal", "type", "status", "updated_at", "payload_json"]),
    (
        "orchestration_v2_projection_subagents",
        &["subagent_id", "thread_id", "run_id", "child_thread_id", "origin", "status", "started_at", "completed_at", "updated_at", "payload_json"],
    ),
    ("orchestration_v2_projection_provider_turns", &["thread_id", "started_at", "completed_at", "status", "payload_json"]),
    ("orchestration_v2_projection_runtime_requests", &["thread_id", "kind", "status", "created_at"]),
];

#[derive(Clone, Debug, Default, Serialize)]
pub struct Pr {
    pub n: i64,
    pub repo: String,
    pub linked: i64,
    pub state: Option<String>,
    pub title: Option<String>,
    pub draft: bool,
    pub checks: Option<String>,
    pub review: Option<String>,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct ThreadMeta {
    pub id: String,
    pub title: String,
    pub project: Option<String>,
    /// Lineage parent (T3 subagent threads).
    pub parent: Option<String>,
    pub rel: Option<String>,
    /// The thread whose `t3_thread_launch` / `create_threads` call created this one.
    pub launcher: Option<String>,
    pub model: Option<String>,
    pub effort: Option<String>,
    pub fast: bool,
    pub source: Option<String>,
    pub provider: String,
    pub created: i64,
    pub updated: i64,
    pub archived: bool,
    pub settled: Option<i64>,
    pub snoozed_until: Option<i64>,
    pub reset_at: Option<i64>,
    pub branch: Option<String>,
    pub prs: Vec<Pr>,
}

#[derive(Clone, Debug, Serialize)]
pub struct Step {
    pub label: String,
    /// tool, think, write, wait, agent, error, other
    pub kind: &'static str,
    pub since: i64,
}

#[derive(Clone, Debug, Serialize)]
pub struct ActiveRun {
    pub run: String,
    pub thread: String,
    pub ordinal: i64,
    pub status: String,
    pub requested: i64,
    pub started: Option<i64>,
    pub model: Option<String>,
    pub effort: Option<String>,
    pub delegated: bool,
    pub step: Option<Step>,
    pub tools: i64,
}

#[derive(Clone, Debug, Serialize)]
pub struct Subagent {
    pub id: String,
    pub thread: String,
    pub run: Option<String>,
    pub child: Option<String>,
    pub origin: String,
    pub status: String,
    pub model: Option<String>,
    pub title: Option<String>,
    pub started: Option<i64>,
    pub completed: Option<i64>,
    pub updated: i64,
}

#[derive(Clone, Debug, Serialize)]
pub struct Ctx {
    pub used: i64,
    pub max: i64,
    pub cache_exp: Option<i64>,
    pub model: Option<String>,
    pub at: i64,
}

#[derive(Clone, Debug, Serialize)]
pub struct RunRow {
    pub thread: String,
    pub status: String,
    pub requested: i64,
    pub started: Option<i64>,
    pub completed: Option<i64>,
    pub model: Option<String>,
    pub delegated: bool,
}

/// One run of a thread, for the drill-down.
#[derive(Clone, Debug, Serialize)]
pub struct ThreadRun {
    pub n: i64,
    pub status: String,
    pub requested: i64,
    pub started: Option<i64>,
    pub completed: Option<i64>,
    pub model: Option<String>,
    pub delegated: bool,
}

/// Counts only: what one thread did inside a time range.
#[derive(Clone, Debug, Default, Serialize)]
pub struct ThreadDetail {
    pub runs: Vec<ThreadRun>,
    /// Tool name -> calls, biggest first.
    pub tools: Vec<(String, i64)>,
    /// Turn-item type -> items, biggest first.
    pub items: Vec<(String, i64)>,
    pub subs: Vec<Subagent>,
}

#[derive(Clone, Debug, Serialize)]
pub struct Waiting {
    pub thread: String,
    pub kind: String,
    pub since: i64,
}

/// One row of T3's event log, reduced to safe fields.
#[derive(Clone, Debug, Default)]
pub struct Ev {
    pub seq: i64,
    pub ts: i64,
    pub kind: String,
    pub thread: String,
    pub status: Option<String>,
    pub id: Option<String>,
    pub item_type: Option<String>,
    pub tool: Option<String>,
    pub model: Option<String>,
    pub title: Option<String>,
    pub started: Option<i64>,
    pub completed: Option<i64>,
    pub ordinal: Option<i64>,
    pub pr_num: Option<i64>,
    pub pr_repo: Option<String>,
}

pub struct T3 {
    conn: Connection,
    pub threads: HashMap<String, ThreadMeta>,
    /// launched thread -> launcher thread
    pub launch: HashMap<String, String>,
    /// mcp-created threads already searched for a launcher.
    pub launch_checked: HashSet<String>,
    pending_launch: Vec<(String, i64, i64)>,
    pub session_thread: HashMap<String, String>,
    pub last_seq: i64,
}

fn ts(s: Option<String>) -> Option<i64> {
    s.as_deref().and_then(parse_ts_ms)
}

fn uri_path(p: &Path) -> String {
    let s = p.to_string_lossy();
    let mut out = String::with_capacity(s.len() + 16);
    for c in s.chars() {
        match c {
            '%' => out.push_str("%25"),
            '?' => out.push_str("%3f"),
            '#' => out.push_str("%23"),
            ' ' => out.push_str("%20"),
            _ => out.push(c),
        }
    }
    out
}

/// Short, human label for a T3 tool name. `mcp__t3-code__t3_thread_send`
/// becomes `t3 thread_send`; plain tools (Read, Bash) stay as they are.
pub fn short_tool(name: &str) -> String {
    let Some(rest) = name.strip_prefix("mcp__") else { return name.to_string() };
    let mut parts = rest.splitn(2, "__");
    let server = parts.next().unwrap_or("");
    let tool = parts.next().unwrap_or("");
    let server = if let Some(p) = server.strip_prefix("plugin_") {
        p.split('_').next().unwrap_or(p)
    } else if let Some(p) = server.strip_prefix("claude_ai_") {
        p
    } else {
        server
    };
    let server = server.strip_suffix("-code").unwrap_or(server);
    let tool = tool.strip_prefix(&format!("{server}_")).unwrap_or(tool);
    format!("{server} {tool}")
}

/// What a turn item says about the current step.
pub fn step_of(item_type: &str, tool: Option<&str>, status: &str) -> (String, &'static str) {
    let finished = matches!(status, "completed" | "failed" | "interrupted" | "cancelled");
    let (label, kind): (String, &'static str) = match item_type {
        "command_execution" => ("Bash".into(), "tool"),
        "dynamic_tool" => (tool.map(short_tool).unwrap_or_else(|| "tool".into()), "tool"),
        "file_change" => ("Edit".into(), "tool"),
        "web_search" => ("WebSearch".into(), "tool"),
        "subagent" => ("Subagent".into(), "agent"),
        "reasoning" => ("thinking".into(), "think"),
        "assistant_message" => ("writing".into(), "write"),
        "user_message" => ("reading prompt".into(), "other"),
        "approval_request" => ("needs approval".into(), "wait"),
        "link_request" => ("needs a link".into(), "wait"),
        "error" => ("error".into(), "error"),
        other => (other.replace('_', " "), "other"),
    };
    // A finished tool means the model is working on its result.
    if finished && matches!(kind, "tool" | "agent") {
        return ("thinking".into(), "think");
    }
    (label, kind)
}

impl T3 {
    pub fn open(db: &Path) -> Result<T3, String> {
        if !db.exists() {
            return Err(format!("{} not found", db.display()));
        }
        let uri = format!("file:{}?mode=ro", uri_path(db));
        let flags = OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_URI | OpenFlags::SQLITE_OPEN_NO_MUTEX;
        let conn = Connection::open_with_flags(&uri, flags).map_err(|e| e.to_string())?;
        conn.busy_timeout(Duration::from_millis(2000)).map_err(|e| e.to_string())?;
        // Keep our own footprint small: 2 MB page cache, no mmap.
        conn.execute_batch("PRAGMA cache_size=-512; PRAGMA mmap_size=0; PRAGMA query_only=1; PRAGMA temp_store=MEMORY;")
            .map_err(|e| e.to_string())?;
        conn.set_prepared_statement_cache_capacity(48);
        Ok(T3 {
            conn,
            threads: HashMap::new(),
            launch: HashMap::new(),
            launch_checked: HashSet::new(),
            pending_launch: Vec::new(),
            session_thread: HashMap::new(),
            last_seq: 0,
        })
    }

    pub fn begin(&self) -> rusqlite::Result<()> {
        self.conn.execute_batch("BEGIN")
    }

    pub fn end(&self) {
        let _ = self.conn.execute_batch("COMMIT");
    }

    /// Missing tables or columns, as readable strings. Empty means all good.
    pub fn schema_problems(&self) -> Vec<String> {
        let mut out = Vec::new();
        for (table, cols) in REQUIRED {
            let have: HashSet<String> = match self.conn.prepare("SELECT name FROM pragma_table_info(?1)") {
                Ok(mut st) => st
                    .query_map([table], |r| r.get::<_, String>(0))
                    .map(|rows| rows.flatten().collect())
                    .unwrap_or_default(),
                Err(e) => {
                    out.push(format!("{table}: {e}"));
                    continue;
                }
            };
            if have.is_empty() {
                out.push(format!("table {table} is missing"));
                continue;
            }
            for c in *cols {
                if !have.contains(*c) {
                    out.push(format!("{table}.{c} is missing"));
                }
            }
        }
        out
    }

    /// Payload keys we rely on, checked on recent rows. A table with rows
    /// but none carrying the key means T3 renamed it: the numbers built on
    /// it would be wrong, so this is reported instead of silently zeroed.
    pub fn json_drift(&self) -> Vec<String> {
        const PROBES: &[(&str, &str, &str)] = &[
            ("orchestration_v2_projection_threads", "$.modelSelection.model", "1"),
            ("orchestration_v2_projection_run_attempts", "$.nativeThreadId", "1"),
            ("orchestration_v2_projection_provider_turns", "$.tokenUsage.maxTokens", "status = 'completed'"),
            ("orchestration_v2_projection_subagents", "$.model", "1"),
            ("orchestration_v2_projection_turn_items", "$.toolName", "type = 'dynamic_tool'"),
            ("orchestration_v2_projection_runs", "$.startedAt", "status = 'completed'"),
        ];
        let mut out = Vec::new();
        for (table, key, filter) in PROBES {
            let sql = format!(
                "SELECT count(*), count(json_extract(payload_json, '{key}'))
                 FROM (SELECT payload_json FROM {table} WHERE {filter} ORDER BY rowid DESC LIMIT 20)"
            );
            match self.conn.query_row(&sql, [], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?))) {
                Ok((rows, with_key)) if rows > 0 && with_key == 0 => out.push(format!("{table} rows no longer carry {key}")),
                Ok(_) => {}
                Err(e) => out.push(format!("{table}: {e}")),
            }
        }
        out
    }

    pub fn max_seq(&self) -> rusqlite::Result<i64> {
        self.conn
            .prepare_cached("SELECT coalesce(max(sequence), 0) FROM orchestration_events")?
            .query_row([], |r| r.get(0))
    }

    /// Loads all threads, or one thread when `only` is set.
    pub fn load_threads(&mut self, only: Option<&str>) -> rusqlite::Result<()> {
        let sql = "SELECT t.thread_id, t.title, p.title, t.created_at, t.updated_at, t.archived_at, t.default_provider,
              json_extract(t.payload_json,'$.lineage.parentThreadId'),
              json_extract(t.payload_json,'$.lineage.relationshipToParent'),
              json_extract(t.payload_json,'$.modelSelection.model'),
              (SELECT json_extract(o.value,'$.value') FROM json_each(t.payload_json,'$.modelSelection.options') o
                 WHERE json_extract(o.value,'$.id')='effort'),
              (SELECT CAST(json_extract(o.value,'$.value') AS INTEGER) FROM json_each(t.payload_json,'$.modelSelection.options') o
                 WHERE json_extract(o.value,'$.id')='fastMode'),
              json_extract(t.payload_json,'$.creationSource'),
              json_extract(t.payload_json,'$.settledAt'),
              json_extract(t.payload_json,'$.snoozedUntil'),
              json_extract(t.payload_json,'$.limitRecovery.resetAt'),
              json_extract(t.payload_json,'$.branch')
            FROM orchestration_v2_projection_threads t
            LEFT JOIN projection_projects p ON p.project_id = t.project_id
            WHERE t.deleted_at IS NULL AND (?1 IS NULL OR t.thread_id = ?1)";
        let rows: Vec<ThreadMeta> = {
            let mut st = self.conn.prepare_cached(sql)?;
            let it = st.query_map(params![only], |r| {
                Ok(ThreadMeta {
                    id: r.get(0)?,
                    title: r.get(1)?,
                    project: r.get(2)?,
                    created: ts(r.get(3)?).unwrap_or(0),
                    updated: ts(r.get(4)?).unwrap_or(0),
                    archived: r.get::<_, Option<String>>(5)?.is_some(),
                    provider: r.get(6)?,
                    parent: r.get(7)?,
                    rel: r.get(8)?,
                    model: r.get(9)?,
                    effort: r.get::<_, Option<rusqlite::types::Value>>(10)?.and_then(|v| match v {
                        rusqlite::types::Value::Text(s) => Some(s),
                        _ => None,
                    }),
                    fast: r.get::<_, Option<i64>>(11)?.unwrap_or(0) != 0,
                    source: r.get(12)?,
                    settled: ts(r.get(13)?),
                    snoozed_until: ts(r.get(14)?),
                    reset_at: ts(r.get(15)?),
                    branch: r.get(16)?,
                    launcher: None,
                    prs: Vec::new(),
                })
            })?;
            it.collect::<rusqlite::Result<_>>()?
        };
        let prs_sql = "SELECT t.thread_id, json_extract(p.value,'$.number'), json_extract(p.value,'$.repository'),
              json_extract(p.value,'$.linkedAt'), json_extract(p.value,'$.snapshot.state'),
              json_extract(p.value,'$.snapshot.title'), json_extract(p.value,'$.snapshot.isDraft'),
              json_extract(p.value,'$.snapshot.checksState'), json_extract(p.value,'$.snapshot.reviewDecision')
            FROM orchestration_v2_projection_threads t, json_each(t.payload_json,'$.pullRequests') p
            WHERE t.deleted_at IS NULL AND (?1 IS NULL OR t.thread_id = ?1)";
        let mut prs: HashMap<String, Vec<Pr>> = HashMap::new();
        {
            let mut st = self.conn.prepare_cached(prs_sql)?;
            let it = st.query_map(params![only], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    Pr {
                        n: r.get::<_, Option<i64>>(1)?.unwrap_or(0),
                        repo: r.get::<_, Option<String>>(2)?.unwrap_or_default(),
                        linked: ts(r.get(3)?).unwrap_or(0),
                        state: r.get(4)?,
                        title: r.get(5)?,
                        draft: r.get::<_, Option<i64>>(6)?.unwrap_or(0) != 0,
                        checks: r.get(7)?,
                        review: r.get(8)?,
                    },
                ))
            })?;
            for row in it.flatten() {
                prs.entry(row.0).or_default().push(row.1);
            }
        }
        if only.is_none() {
            self.threads.clear();
        }
        for mut t in rows {
            t.prs = prs.remove(&t.id).unwrap_or_default();
            t.prs.sort_by_key(|p| std::cmp::Reverse(p.linked));
            // Same PR linked twice (relinks) shows once.
            let mut seen = HashSet::new();
            t.prs.retain(|p| seen.insert((p.repo.clone(), p.n)));
            t.launcher = self.launch.get(&t.id).cloned();
            self.threads.insert(t.id.clone(), t);
        }
        Ok(())
    }

    /// Maps Claude session ids (transcript file names) to T3 threads.
    pub fn load_sessions(&mut self) -> rusqlite::Result<()> {
        let mut st = self.conn.prepare_cached(
            "SELECT DISTINCT json_extract(payload_json,'$.nativeThreadId'), thread_id
             FROM orchestration_v2_projection_run_attempts
             WHERE json_extract(payload_json,'$.nativeThreadId') IS NOT NULL",
        )?;
        let rows = st.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?;
        for (s, t) in rows.flatten() {
            self.session_thread.insert(s, t);
        }
        Ok(())
    }

    /// One full pass matching launched thread ids inside launch tool items.
    /// Reads most of the database once (about 2 s); the result is cached.
    pub fn full_launch_scan(&mut self) -> rusqlite::Result<usize> {
        let sql = format!(
            "SELECT li.thread_id, t.thread_id
             FROM orchestration_v2_projection_turn_items li
             JOIN orchestration_v2_projection_threads s ON s.thread_id = li.thread_id
             JOIN orchestration_v2_projection_threads t
               ON t.thread_id <> li.thread_id AND instr(li.payload_json, t.thread_id) > 0
             WHERE li.type = 'dynamic_tool' AND json_extract(li.payload_json,'$.toolName') IN {LAUNCH_TOOLS}
               -- A launcher is older than what it launched, and the call
               -- happens around the moment the new thread is created.
               AND t.created_at >= s.created_at
               AND abs(julianday(li.updated_at) - julianday(t.created_at)) < 0.02
             ORDER BY li.updated_at"
        );
        let mut st = self.conn.prepare(&sql)?;
        let rows = st.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?;
        let mut n = 0;
        for (launcher, child) in rows.flatten() {
            if !self.launch.contains_key(&child) {
                self.launch.insert(child, launcher);
                n += 1;
            }
        }
        for t in self.threads.values() {
            if t.source.as_deref() == Some("mcp") {
                self.launch_checked.insert(t.id.clone());
            }
        }
        Ok(n)
    }

    /// Queue a newly created thread for a launcher search.
    pub fn note_created(&mut self, thread: &str, seq: i64, now: i64) {
        if !self.launch.contains_key(thread) && !self.launch_checked.contains(thread) {
            self.pending_launch.push((thread.to_string(), seq, now));
        }
    }

    /// Looks for launchers of recently created threads in the event log.
    /// Returns true when a link was found.
    pub fn resolve_launches(&mut self, now: i64) -> bool {
        if self.pending_launch.is_empty() {
            return false;
        }
        let sql = format!(
            "SELECT stream_id FROM orchestration_events
             WHERE sequence > ?1 AND event_type = 'turn-item.updated' AND stream_id <> ?2
               AND instr(payload_json, ?2) > 0
               AND json_extract(payload_json,'$.toolName') IN {LAUNCH_TOOLS}
             ORDER BY sequence LIMIT 1"
        );
        let pending = std::mem::take(&mut self.pending_launch);
        let mut found = false;
        for (thread, seq, first) in pending {
            let hit: Option<String> = self
                .conn
                .prepare_cached(&sql)
                .and_then(|mut st| st.query_row(params![seq - 500, thread], |r| r.get(0)).optional())
                .unwrap_or(None);
            match hit {
                Some(l) => {
                    if let Some(t) = self.threads.get_mut(&thread) {
                        t.launcher = Some(l.clone());
                    }
                    self.launch.insert(thread.clone(), l);
                    self.launch_checked.insert(thread);
                    found = true;
                }
                None if now - first > 5 * MS_PER_MIN => {
                    self.launch_checked.insert(thread);
                }
                None => self.pending_launch.push((thread, seq, first)),
            }
        }
        found
    }

    /// New event rows since the last call, reduced to safe fields.
    pub fn events_since(&mut self, from_seq: i64, limit: i64) -> rusqlite::Result<Vec<Ev>> {
        let sql = "SELECT sequence, event_type, stream_id, occurred_at,
              CASE WHEN event_type IN ('run.updated','run.created','subagent.updated','turn-item.updated','runtime-request.updated')
                   THEN json_extract(payload_json,'$.status') END,
              CASE WHEN event_type IN ('run.updated','run.created','subagent.updated','turn-item.updated')
                   THEN json_extract(payload_json,'$.id') END,
              CASE WHEN event_type = 'turn-item.updated' THEN json_extract(payload_json,'$.type')
                   WHEN event_type = 'runtime-request.updated' THEN json_extract(payload_json,'$.kind') END,
              CASE WHEN event_type = 'turn-item.updated' THEN json_extract(payload_json,'$.toolName') END,
              CASE WHEN event_type = 'subagent.updated' THEN json_extract(payload_json,'$.model')
                   WHEN event_type IN ('run.updated','run.created') THEN json_extract(payload_json,'$.modelSelection.model') END,
              CASE WHEN event_type IN ('subagent.updated','thread.created') THEN json_extract(payload_json,'$.title') END,
              CASE WHEN event_type IN ('run.updated','subagent.updated','turn-item.updated') THEN json_extract(payload_json,'$.startedAt') END,
              CASE WHEN event_type IN ('run.updated','subagent.updated','turn-item.updated') THEN json_extract(payload_json,'$.completedAt') END,
              CASE WHEN event_type IN ('run.updated','run.created') THEN json_extract(payload_json,'$.ordinal') END,
              CASE WHEN event_type = 'thread.pull-request-link-updated' THEN json_extract(payload_json,'$.number') END,
              CASE WHEN event_type = 'thread.pull-request-link-updated' THEN json_extract(payload_json,'$.repository') END
            FROM orchestration_events WHERE sequence > ?1 ORDER BY sequence LIMIT ?2";
        let mut st = self.conn.prepare_cached(sql)?;
        let rows = st.query_map(params![from_seq, limit], |r| {
            Ok(Ev {
                seq: r.get(0)?,
                kind: r.get(1)?,
                thread: r.get(2)?,
                ts: ts(r.get(3)?).unwrap_or(0),
                status: r.get(4)?,
                id: r.get(5)?,
                item_type: r.get(6)?,
                tool: r.get(7)?,
                model: r.get(8)?,
                title: r.get(9)?,
                started: ts(r.get(10)?),
                completed: ts(r.get(11)?),
                ordinal: r.get(12)?,
                pr_num: r.get(13)?,
                pr_repo: r.get(14)?,
            })
        })?;
        let out: Vec<Ev> = rows.collect::<rusqlite::Result<_>>()?;
        if let Some(last) = out.last() {
            self.last_seq = self.last_seq.max(last.seq);
        }
        Ok(out)
    }

    /// Runs that are queued, starting, running or waiting, with their step.
    pub fn active_runs(&self) -> rusqlite::Result<Vec<ActiveRun>> {
        let sql = format!(
            "SELECT r.run_id, r.thread_id, r.ordinal, r.status, r.requested_at,
                    json_extract(r.payload_json,'$.startedAt'),
                    json_extract(r.payload_json,'$.modelSelection.model'),
                    (SELECT json_extract(o.value,'$.value') FROM json_each(r.payload_json,'$.modelSelection.options') o
                       WHERE json_extract(o.value,'$.id')='effort'),
                    json_extract(r.payload_json,'$.delegatedTaskId') IS NOT NULL
             FROM orchestration_v2_projection_runs r WHERE r.status IN {LIVE_RUN_STATUSES}"
        );
        let mut runs: Vec<ActiveRun> = {
            let mut st = self.conn.prepare_cached(&sql)?;
            let it = st.query_map([], |r| {
                Ok(ActiveRun {
                    run: r.get(0)?,
                    thread: r.get(1)?,
                    ordinal: r.get(2)?,
                    status: r.get(3)?,
                    requested: ts(r.get(4)?).unwrap_or(0),
                    started: ts(r.get(5)?),
                    model: r.get(6)?,
                    effort: r.get::<_, Option<rusqlite::types::Value>>(7)?.and_then(|v| match v {
                        rusqlite::types::Value::Text(s) => Some(s),
                        _ => None,
                    }),
                    delegated: r.get::<_, i64>(8)? != 0,
                    step: None,
                    tools: 0,
                })
            })?;
            it.collect::<rusqlite::Result<_>>()?
        };
        let step_sql = format!(
            "SELECT type, status, json_extract(payload_json,'$.toolName'),
                    coalesce(json_extract(payload_json,'$.startedAt'), updated_at), updated_at
             FROM orchestration_v2_projection_turn_items
             WHERE run_id = ?1 AND type NOT IN {STEP_SKIP}
             ORDER BY ordinal DESC LIMIT 1"
        );
        let count_sql = "SELECT count(*) FROM orchestration_v2_projection_turn_items
             WHERE run_id = ?1 AND type IN ('command_execution','dynamic_tool','file_change','web_search','subagent')";
        for run in &mut runs {
            let step = self
                .conn
                .prepare_cached(&step_sql)?
                .query_row([&run.run], |r| {
                    let ty: String = r.get(0)?;
                    let status: String = r.get(1)?;
                    let tool: Option<String> = r.get(2)?;
                    let started = ts(r.get(3)?).unwrap_or(0);
                    let updated = ts(r.get(4)?).unwrap_or(0);
                    Ok((ty, status, tool, started, updated))
                })
                .optional()?;
            if let Some((ty, status, tool, started, updated)) = step {
                let (label, kind) = step_of(&ty, tool.as_deref(), &status);
                let finished = matches!(status.as_str(), "completed" | "failed" | "interrupted" | "cancelled");
                run.step = Some(Step { label, kind, since: if finished { updated } else { started } });
            }
            run.tools = self.conn.prepare_cached(count_sql)?.query_row([&run.run], |r| r.get(0))?;
        }
        Ok(runs)
    }

    /// Subagents updated since `since_ms`, plus any still live.
    pub fn subagents(&self, since_ms: i64) -> rusqlite::Result<Vec<Subagent>> {
        let mut st = self.conn.prepare_cached(
            "SELECT subagent_id, thread_id, run_id, child_thread_id, origin, status,
                    json_extract(payload_json,'$.model'), json_extract(payload_json,'$.title'),
                    started_at, completed_at, updated_at
             FROM orchestration_v2_projection_subagents
             WHERE updated_at >= ?1 OR status NOT IN ('completed','failed','interrupted','cancelled')",
        )?;
        let rows = st.query_map([iso_ms(since_ms)], |r| {
            Ok(Subagent {
                id: r.get(0)?,
                thread: r.get(1)?,
                run: r.get(2)?,
                child: r.get(3)?,
                origin: r.get(4)?,
                status: r.get(5)?,
                model: r.get(6)?,
                title: r.get(7)?,
                started: ts(r.get(8)?),
                completed: ts(r.get(9)?),
                updated: ts(r.get(10)?).unwrap_or(0),
            })
        })?;
        rows.collect()
    }

    /// One thread's runs, tool and item counts, and subagents in
    /// `[from_ms, to_ms)`. Every query starts from the thread id (indexed)
    /// and only reads the tool name and item type out of the JSON: inputs,
    /// outputs and message text are never selected.
    pub fn thread_detail(&self, thread: &str, from_ms: i64, to_ms: i64) -> rusqlite::Result<ThreadDetail> {
        let (from, to) = (iso_ms(from_ms), iso_ms(to_ms));
        let mut d = ThreadDetail::default();
        let mut st = self.conn.prepare_cached(
            "SELECT ordinal, status, requested_at, json_extract(payload_json,'$.startedAt'), completed_at,
                    json_extract(payload_json,'$.modelSelection.model'),
                    json_extract(payload_json,'$.delegatedTaskId') IS NOT NULL
             FROM orchestration_v2_projection_runs
             WHERE thread_id = ?1 AND requested_at >= ?2 AND requested_at < ?3 ORDER BY ordinal",
        )?;
        d.runs = st
            .query_map(params![thread, from, to], |r| {
                Ok(ThreadRun {
                    n: r.get(0)?,
                    status: r.get(1)?,
                    requested: ts(r.get(2)?).unwrap_or(0),
                    started: ts(r.get(3)?),
                    completed: ts(r.get(4)?),
                    model: r.get(5)?,
                    delegated: r.get::<_, i64>(6)? != 0,
                })
            })?
            .collect::<rusqlite::Result<_>>()?;
        let mut st = self.conn.prepare_cached(
            "SELECT type, CASE WHEN type = 'dynamic_tool' THEN json_extract(payload_json,'$.toolName') END, count(*)
             FROM orchestration_v2_projection_turn_items
             WHERE thread_id = ?1 AND updated_at >= ?2 AND updated_at < ?3 GROUP BY 1, 2",
        )?;
        let rows: Vec<(String, Option<String>, i64)> =
            st.query_map(params![thread, from, to], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?.collect::<rusqlite::Result<_>>()?;
        let mut tools: HashMap<String, i64> = HashMap::new();
        let mut items: HashMap<String, i64> = HashMap::new();
        for (ty, tool, n) in rows {
            *items.entry(ty.clone()).or_default() += n;
            if matches!(ty.as_str(), "command_execution" | "dynamic_tool" | "file_change" | "web_search" | "subagent") {
                *tools.entry(step_of(&ty, tool.as_deref(), "running").0).or_default() += n;
            }
        }
        let sorted = |m: HashMap<String, i64>| {
            let mut v: Vec<(String, i64)> = m.into_iter().collect();
            v.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
            v
        };
        d.tools = sorted(tools);
        d.items = sorted(items);
        let mut st = self.conn.prepare_cached(
            "SELECT subagent_id, thread_id, run_id, child_thread_id, origin, status,
                    json_extract(payload_json,'$.model'), json_extract(payload_json,'$.title'),
                    started_at, completed_at, updated_at
             FROM orchestration_v2_projection_subagents
             WHERE thread_id = ?1 AND ((coalesce(started_at, updated_at) >= ?2 AND coalesce(started_at, updated_at) < ?3)
                   OR status NOT IN ('completed','failed','interrupted','cancelled'))
             ORDER BY coalesce(started_at, updated_at)",
        )?;
        d.subs = st
            .query_map(params![thread, from, to], |r| {
                Ok(Subagent {
                    id: r.get(0)?,
                    thread: r.get(1)?,
                    run: r.get(2)?,
                    child: r.get(3)?,
                    origin: r.get(4)?,
                    status: r.get(5)?,
                    model: r.get(6)?,
                    title: r.get(7)?,
                    started: ts(r.get(8)?),
                    completed: ts(r.get(9)?),
                    updated: ts(r.get(10)?).unwrap_or(0),
                })
            })?
            .collect::<rusqlite::Result<_>>()?;
        Ok(d)
    }

    /// Latest context fill and prompt-cache expiry for a thread.
    pub fn ctx(&self, thread: &str) -> rusqlite::Result<Option<Ctx>> {
        self.conn
            .prepare_cached(
                "SELECT json_extract(payload_json,'$.tokenUsage.usedTokens'),
                        json_extract(payload_json,'$.tokenUsage.maxTokens'),
                        json_extract(payload_json,'$.tokenUsage.promptCacheExpiresAt'),
                        json_extract(payload_json,'$.model'),
                        coalesce(completed_at, started_at)
                 FROM orchestration_v2_projection_provider_turns
                 WHERE thread_id = ?1 ORDER BY started_at DESC LIMIT 1",
            )?
            .query_row([thread], |r| {
                Ok(Ctx {
                    used: r.get::<_, Option<i64>>(0)?.unwrap_or(0),
                    max: r.get::<_, Option<i64>>(1)?.unwrap_or(0),
                    cache_exp: ts(r.get(2)?),
                    model: r.get(3)?,
                    at: ts(r.get(4)?).unwrap_or(0),
                })
            })
            .optional()
    }

    /// Open requests for the user (approvals, input) from the last day.
    /// T3 marks them `pending` until resolved or cancelled.
    pub fn waiting(&self, now: i64) -> rusqlite::Result<Vec<Waiting>> {
        let mut st = self.conn.prepare_cached(
            "SELECT thread_id, kind, created_at FROM orchestration_v2_projection_runtime_requests
             WHERE status = 'pending' AND created_at >= ?1",
        )?;
        let rows = st.query_map([iso_ms(now - 24 * 60 * MS_PER_MIN)], |r| {
            Ok(Waiting { thread: r.get(0)?, kind: r.get(1)?, since: ts(r.get(2)?).unwrap_or(0) })
        })?;
        rows.collect()
    }

    /// Runs requested since `since_ms` (for the three-day view).
    pub fn runs_since(&self, since_ms: i64) -> rusqlite::Result<Vec<RunRow>> {
        let mut st = self.conn.prepare_cached(
            "SELECT thread_id, status, requested_at, json_extract(payload_json,'$.startedAt'), completed_at,
                    json_extract(payload_json,'$.modelSelection.model'),
                    json_extract(payload_json,'$.delegatedTaskId') IS NOT NULL
             FROM orchestration_v2_projection_runs WHERE requested_at >= ?1",
        )?;
        let rows = st.query_map([iso_ms(since_ms)], |r| {
            Ok(RunRow {
                thread: r.get(0)?,
                status: r.get(1)?,
                requested: ts(r.get(2)?).unwrap_or(0),
                started: ts(r.get(3)?),
                completed: ts(r.get(4)?),
                model: r.get(5)?,
                delegated: r.get::<_, i64>(6)? != 0,
            })
        })?;
        rows.collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Builds a tiny database with the subset of T3's schema we read. The
    /// rows are synthetic; no real data is copied.
    fn fixture(name: &str, drop_col: bool) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("sb-t3-{}-{}", name, std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("statev2.sqlite");
        let c = Connection::open(&p).unwrap();
        let threads_cols = if drop_col { "thread_id, project_id, title, created_at, updated_at, archived_at, deleted_at, default_provider" } else { "thread_id, project_id, title, created_at, updated_at, archived_at, deleted_at, payload_json, default_provider" };
        c.execute_batch(&format!(
            "CREATE TABLE orchestration_events (sequence INTEGER PRIMARY KEY, event_type TEXT, stream_id TEXT, occurred_at TEXT, payload_json TEXT);
             CREATE TABLE orchestration_v2_projection_threads ({threads_cols});
             CREATE TABLE projection_projects (project_id, title);
             CREATE TABLE orchestration_v2_projection_runs (run_id, thread_id, ordinal, status, requested_at, completed_at, payload_json);
             CREATE TABLE orchestration_v2_projection_run_attempts (thread_id, payload_json);
             CREATE TABLE orchestration_v2_projection_turn_items (turn_item_id, thread_id, run_id, ordinal, type, status, updated_at, payload_json);
             CREATE TABLE orchestration_v2_projection_subagents (subagent_id, thread_id, run_id, child_thread_id, origin, status, started_at, completed_at, updated_at, payload_json);
             CREATE TABLE orchestration_v2_projection_provider_turns (thread_id, started_at, completed_at, status, payload_json);
             CREATE TABLE orchestration_v2_projection_runtime_requests (thread_id, kind, status, created_at);"
        ))
        .unwrap();
        if !drop_col {
            c.execute_batch(
                r#"INSERT INTO projection_projects VALUES ('p1','demo');
                 INSERT INTO orchestration_v2_projection_threads VALUES
                   ('hq','p1','HQ','2026-11-17T08:00:00.000Z','2026-11-17T09:00:00.000Z',NULL,NULL,
                    '{"modelSelection":{"model":"claude-opus-5-5","options":[{"id":"effort","value":"xhigh"},{"id":"fastMode","value":false}]},"creationSource":"web","pullRequests":[{"number":7,"repository":"o/r","linkedAt":"2026-11-17T09:00:00.000Z","snapshot":{"state":"open","title":"Fix"}}]}','claudeAgent'),
                   ('kid','p1','Backend','2026-11-17T08:30:00.000Z','2026-11-17T09:00:00.000Z',NULL,NULL,
                    '{"modelSelection":{"model":"claude-opus-5-5","options":[]},"creationSource":"mcp"}','claudeAgent');
                 INSERT INTO orchestration_v2_projection_turn_items VALUES
                   ('i1','hq','r1',1,'dynamic_tool','completed','2026-11-17T08:30:01.000Z','{"toolName":"mcp__t3-code__t3_thread_launch","output":{"threadId":"kid","text":"secret"}}'),
                   ('i2','kid','r2',1,'reasoning','completed','2026-11-17T09:00:00.000Z','{}'),
                   ('i3','kid','r2',2,'command_execution','running','2026-11-17T09:00:05.000Z','{"startedAt":"2026-11-17T09:00:04.000Z","input":"rm -rf secret"}');
                 INSERT INTO orchestration_v2_projection_runs VALUES
                   ('r2','kid',3,'running','2026-11-17T09:00:00.000Z',NULL,'{"startedAt":"2026-11-17T09:00:00.500Z","modelSelection":{"model":"claude-opus-5-5","options":[{"id":"effort","value":"high"}]}}');
                 INSERT INTO orchestration_v2_projection_run_attempts VALUES ('kid','{"nativeThreadId":"sess-1"}');
                 INSERT INTO orchestration_events VALUES
                   (1,'run.updated','kid','2026-11-17T09:00:00.000Z','{"id":"r2","status":"running","ordinal":3,"modelSelection":{"model":"claude-opus-5-5"}}'),
                   (2,'turn-item.updated','kid','2026-11-17T09:00:05.000Z','{"id":"i3","type":"command_execution","status":"running","input":"secret"}');"#,
            )
            .unwrap();
        }
        p
    }

    #[test]
    fn schema_check_reports_missing() {
        let t = T3::open(&fixture("schema", true)).unwrap();
        let probs = t.schema_problems();
        assert!(probs.iter().any(|p| p.contains("orchestration_v2_projection_threads.payload_json")));
        let ok = T3::open(&fixture("schema-ok", false)).unwrap();
        assert!(ok.schema_problems().is_empty(), "{:?}", ok.schema_problems());
        assert!(ok.json_drift().is_empty(), "{:?}", ok.json_drift());
    }

    #[test]
    fn json_drift_reports_renamed_keys() {
        let p = fixture("drift", false);
        {
            let c = Connection::open(&p).unwrap();
            for _ in 0..20 {
                c.execute("INSERT INTO orchestration_v2_projection_run_attempts VALUES ('kid', '{\"nativeId\":\"x\"}')", []).unwrap();
            }
        }
        let t = T3::open(&p).unwrap();
        let d = t.json_drift();
        assert_eq!(d.len(), 1, "{d:?}");
        assert!(d[0].contains("nativeThreadId"));
    }

    #[test]
    fn launch_link_found_via_tool_item() {
        let mut t = T3::open(&fixture("launch", false)).unwrap();
        t.load_threads(None).unwrap();
        assert_eq!(t.full_launch_scan().unwrap(), 1);
        assert_eq!(t.launch.get("kid").map(String::as_str), Some("hq"));
        t.load_threads(None).unwrap();
        assert_eq!(t.threads["kid"].launcher.as_deref(), Some("hq"));
        let hq = &t.threads["hq"];
        assert_eq!(hq.effort.as_deref(), Some("xhigh"));
        assert_eq!(hq.prs.len(), 1);
        assert_eq!(hq.prs[0].title.as_deref(), Some("Fix"));
    }

    #[test]
    fn current_step_maps_types() {
        let t = T3::open(&fixture("step", false)).unwrap();
        let runs = t.active_runs().unwrap();
        assert_eq!(runs.len(), 1);
        let s = runs[0].step.as_ref().unwrap();
        assert_eq!(s.label, "Bash");
        assert_eq!(s.kind, "tool");
        assert_eq!(runs[0].tools, 1);
        assert_eq!(runs[0].effort.as_deref(), Some("high"));
        assert_eq!(step_of("reasoning", None, "running").0, "thinking");
        assert_eq!(step_of("dynamic_tool", Some("mcp__t3-code__t3_thread_send"), "running").0, "t3 thread_send");
        assert_eq!(step_of("dynamic_tool", Some("Read"), "completed").0, "thinking");
        assert_eq!(short_tool("mcp__plugin_claude-mem_mcp-search__work_state_write"), "claude-mem work_state_write");
    }

    #[test]
    fn thread_detail_counts_names_only() {
        let p = fixture("detail", false);
        {
            let c = Connection::open(&p).unwrap();
            c.execute_batch(
                r#"INSERT INTO orchestration_v2_projection_turn_items VALUES
                   ('i4','kid','r2',3,'dynamic_tool','completed','2026-11-17T09:01:00.000Z','{"toolName":"Read","input":{"path":"/secret"}}'),
                   ('i5','kid','r2',4,'dynamic_tool','completed','2026-11-17T09:02:00.000Z','{"toolName":"Read","input":{"path":"/other"}}'),
                   ('i6','kid','r2',5,'dynamic_tool','completed','2026-11-17T09:03:00.000Z','{"toolName":"mcp__t3-code__t3_thread_send"}'),
                   ('i7','kid','r2',6,'assistant_message','completed','2026-11-17T09:04:00.000Z','{"text":"hello secret"}'),
                   ('old','kid','r1',7,'file_change','completed','2026-11-16T09:04:00.000Z','{}'),
                   ('oth','hq','r9',1,'command_execution','completed','2026-11-17T09:04:00.000Z','{}');
                 INSERT INTO orchestration_v2_projection_runs VALUES
                   ('r1','kid',1,'completed','2026-11-17T08:30:00.000Z','2026-11-17T08:40:00.000Z','{"startedAt":"2026-11-17T08:31:00.000Z","modelSelection":{"model":"claude-sonnet-5-5"},"delegatedTaskId":"d1"}'),
                   ('r0','kid',0,'completed','2026-11-16T08:30:00.000Z','2026-11-16T08:40:00.000Z','{}');
                 INSERT INTO orchestration_v2_projection_subagents VALUES
                   ('sa1','kid','r2',NULL,'app_owned','completed','2026-11-17T08:50:00.000Z','2026-11-17T08:55:00.000Z','2026-11-17T08:55:00.000Z','{"model":"claude-haiku-5-5","title":"Scout"}'),
                   ('sa0','kid','r0',NULL,'app_owned','completed','2026-11-16T08:50:00.000Z','2026-11-16T08:55:00.000Z','2026-11-16T08:55:00.000Z','{"model":"x"}');"#,
            )
            .unwrap();
        }
        let t = T3::open(&p).unwrap();
        let from = parse_ts_ms("2026-11-17T00:00:00Z").unwrap();
        let d = t.thread_detail("kid", from, from + 24 * 3_600_000).unwrap();
        assert_eq!(d.runs.iter().map(|r| r.n).collect::<Vec<_>>(), vec![1, 3]);
        assert!(d.runs[0].delegated && d.runs[0].model.as_deref() == Some("claude-sonnet-5-5"));
        assert_eq!(d.runs[1].status, "running");
        assert_eq!(d.tools[0], ("Read".to_string(), 2));
        assert!(d.tools.contains(&("t3 thread_send".to_string(), 1)));
        assert!(d.tools.contains(&("Bash".to_string(), 1)));
        // Yesterday's file_change and another thread's items are not counted.
        assert!(!d.tools.iter().any(|(n, _)| n == "Edit"));
        let reasoning = d.items.iter().find(|(n, _)| n == "reasoning").unwrap().1;
        assert_eq!(reasoning, 1);
        assert_eq!(d.items.iter().find(|(n, _)| n == "assistant_message").unwrap().1, 1);
        assert_eq!(d.subs.len(), 1);
        assert_eq!(d.subs[0].title.as_deref(), Some("Scout"));
        let json = serde_json::to_string(&d).unwrap();
        assert!(!json.contains("secret"), "{json}");
        // An unknown thread is empty, not an error.
        assert!(t.thread_detail("nope", from, from + 1000).unwrap().runs.is_empty());
    }

    #[test]
    fn events_since_advances() {
        let mut t = T3::open(&fixture("events", false)).unwrap();
        let ev = t.events_since(0, 100).unwrap();
        assert_eq!(ev.len(), 2);
        assert_eq!(ev[0].status.as_deref(), Some("running"));
        assert_eq!(ev[1].item_type.as_deref(), Some("command_execution"));
        assert_eq!(t.last_seq, 2);
        assert!(t.events_since(2, 100).unwrap().is_empty());
        t.load_sessions().unwrap();
        assert_eq!(t.session_thread.get("sess-1").map(String::as_str), Some("kid"));
    }
}
