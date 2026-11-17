//! Turns T3 events (and a few transcript signals) into feed items.
//!
//! Items have stable ids, so an update (a tool burst growing, a PR relinked)
//! replaces the earlier item in the browser instead of adding a new line.

use serde::Serialize;
use std::collections::{HashMap, HashSet, VecDeque};

use crate::t3::{short_tool, Ev, ThreadMeta};

/// Tool calls from one thread within this gap merge into one feed line.
const GROUP_GAP_MS: i64 = 20_000;
const ITEMS_REMEMBERED: usize = 1_500;

#[derive(Clone, Debug, Serialize, Default)]
pub struct FeedItem {
    pub id: String,
    pub ts: i64,
    /// run_start, run_end, waiting, sub_start, sub_end, tools, approval, pr,
    /// thread_new, handoff, settled, limit, session
    pub k: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub th: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tt: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub up: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dur: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub n: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub repo: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<(String, u32)>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub count: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub app: Option<String>,
}

struct Group {
    id: String,
    first: i64,
    last: i64,
    tools: Vec<(String, u32)>,
    count: u32,
}

#[derive(Default)]
pub struct FeedBuilder {
    run_status: HashMap<String, String>,
    sub_status: HashMap<String, String>,
    items_seen: HashSet<String>,
    items_order: VecDeque<String>,
    groups: HashMap<String, Group>,
    gid: u64,
}

fn title_of(threads: &HashMap<String, ThreadMeta>, id: &str) -> Option<String> {
    threads.get(id).map(|t| t.title.clone())
}

fn up_of(threads: &HashMap<String, ThreadMeta>, id: &str) -> Option<String> {
    let t = threads.get(id)?;
    let up = t.parent.as_deref().or(t.launcher.as_deref())?;
    title_of(threads, up)
}

const ENDED: [&str; 4] = ["completed", "failed", "interrupted", "cancelled"];

impl FeedBuilder {
    fn remember_item(&mut self, id: &str) -> bool {
        if self.items_seen.contains(id) {
            return false;
        }
        self.items_seen.insert(id.to_string());
        self.items_order.push_back(id.to_string());
        while self.items_order.len() > ITEMS_REMEMBERED {
            if let Some(old) = self.items_order.pop_front() {
                self.items_seen.remove(&old);
            }
        }
        true
    }

    fn base(&self, ev: &Ev, k: &'static str, threads: &HashMap<String, ThreadMeta>) -> FeedItem {
        FeedItem {
            ts: ev.ts,
            k,
            th: Some(ev.thread.clone()),
            tt: title_of(threads, &ev.thread),
            up: up_of(threads, &ev.thread),
            ..Default::default()
        }
    }

    pub fn on_event(&mut self, ev: &Ev, threads: &HashMap<String, ThreadMeta>) -> Option<FeedItem> {
        // Bounded memory: thousands of runs is weeks of work; forgetting old
        // statuses only risks one repeated "started" line.
        if self.run_status.len() > 5000 {
            self.run_status.clear();
        }
        if self.sub_status.len() > 5000 {
            self.sub_status.clear();
        }
        if self.groups.len() > 500 {
            self.groups.clear();
        }
        match ev.kind.as_str() {
            "run.updated" | "run.created" => {
                let id = ev.id.clone()?;
                let s = ev.status.clone()?;
                let prev = self.run_status.insert(id.clone(), s.clone());
                if prev.as_deref() == Some(s.as_str()) {
                    return None;
                }
                let mut it = match s.as_str() {
                    "running" => self.base(ev, "run_start", threads),
                    "waiting" => self.base(ev, "waiting", threads),
                    x if ENDED.contains(&x) => {
                        let mut it = self.base(ev, "run_end", threads);
                        it.dur = match (ev.started, ev.completed) {
                            (Some(a), Some(b)) if b >= a => Some(b - a),
                            _ => None,
                        };
                        it
                    }
                    _ => return None,
                };
                it.id = format!("r:{id}:{s}");
                it.status = Some(s);
                it.model = ev.model.clone();
                it.n = ev.ordinal;
                Some(it)
            }
            "subagent.updated" => {
                let id = ev.id.clone()?;
                let s = ev.status.clone()?;
                let prev = self.sub_status.insert(id.clone(), s.clone());
                if prev.as_deref() == Some(s.as_str()) {
                    return None;
                }
                let k = match s.as_str() {
                    "running" | "starting" => "sub_start",
                    x if ENDED.contains(&x) => "sub_end",
                    _ => return None,
                };
                if k == "sub_start" && prev.as_deref().is_some_and(|p| p == "running" || p == "starting") {
                    return None;
                }
                let mut it = self.base(ev, k, threads);
                it.id = format!("s:{id}:{k}");
                it.status = Some(s);
                it.model = ev.model.clone();
                it.title = ev.title.clone();
                if k == "sub_end" {
                    it.dur = match (ev.started, ev.completed) {
                        (Some(a), Some(b)) if b >= a => Some(b - a),
                        _ => None,
                    };
                }
                Some(it)
            }
            "turn-item.updated" => {
                let ty = ev.item_type.as_deref()?;
                let item = ev.id.clone()?;
                if ty == "approval_request" || ty == "link_request" {
                    let s = ev.status.as_deref().unwrap_or("");
                    if ENDED.contains(&s) || !self.remember_item(&format!("{item}:ask")) {
                        return None;
                    }
                    let mut it = self.base(ev, "approval", threads);
                    it.id = format!("a:{item}");
                    it.title = Some(if ty == "approval_request" { "approval".into() } else { "link".into() });
                    return Some(it);
                }
                let label = match ty {
                    "command_execution" => "Bash".to_string(),
                    "dynamic_tool" => ev.tool.as_deref().map(short_tool).unwrap_or_else(|| "tool".into()),
                    "file_change" => "Edit".to_string(),
                    "web_search" => "WebSearch".to_string(),
                    "subagent" => "Agent".to_string(),
                    _ => return None,
                };
                if !self.remember_item(&item) {
                    return None;
                }
                let thread = ev.thread.clone();
                let open = self.groups.get(&thread).is_some_and(|g| ev.ts - g.last <= GROUP_GAP_MS);
                if !open {
                    self.gid += 1;
                    self.groups.insert(
                        thread.clone(),
                        Group { id: format!("g:{}", self.gid), first: ev.ts, last: ev.ts, tools: Vec::new(), count: 0 },
                    );
                }
                let g = self.groups.get_mut(&thread).unwrap();
                g.last = g.last.max(ev.ts);
                g.count += 1;
                match g.tools.iter_mut().find(|(n, _)| *n == label) {
                    Some(e) => e.1 += 1,
                    None => g.tools.push((label, 1)),
                }
                let mut tools = g.tools.clone();
                tools.sort_by(|a, b| b.1.cmp(&a.1));
                let (gid, first, count) = (g.id.clone(), g.first, g.count);
                let mut it = self.base(ev, "tools", threads);
                it.id = gid;
                it.ts = ev.ts;
                it.dur = Some(ev.ts - first);
                it.tools = Some(tools);
                it.count = Some(count);
                Some(it)
            }
            "thread.pull-request-link-updated" => {
                let n = ev.pr_num?;
                let mut it = self.base(ev, "pr", threads);
                let repo = ev.pr_repo.clone().unwrap_or_default();
                it.id = format!("p:{}:{}:{}", ev.thread, repo, n);
                it.n = Some(n);
                it.repo = Some(repo);
                Some(it)
            }
            "thread.created" => {
                let mut it = self.base(ev, "thread_new", threads);
                it.id = format!("t:{}", ev.thread);
                it.title = ev.title.clone().or_else(|| it.tt.clone());
                Some(it)
            }
            "context-transfer.created" => {
                let mut it = self.base(ev, "handoff", threads);
                it.id = format!("h:{}", ev.seq);
                Some(it)
            }
            "thread.settled" => {
                let mut it = self.base(ev, "settled", threads);
                it.id = format!("d:{}", ev.seq);
                Some(it)
            }
            _ => None,
        }
    }
}

pub fn limit_item(ts: i64) -> FeedItem {
    FeedItem { id: format!("l:{ts}"), ts, k: "limit", ..Default::default() }
}

pub fn session_item(session: &str, ts: i64, label: String, app: Option<String>, model: Option<String>) -> FeedItem {
    FeedItem {
        id: format!("x:{session}:{}", ts / 600_000),
        ts,
        k: "session",
        title: Some(label),
        app,
        model,
        ..Default::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(seq: i64, ts: i64, kind: &str, id: &str, status: &str) -> Ev {
        Ev {
            seq,
            ts,
            kind: kind.into(),
            thread: "t1".into(),
            id: Some(id.into()),
            status: Some(status.into()),
            ..Default::default()
        }
    }

    #[test]
    fn run_transitions_emit_once() {
        let mut f = FeedBuilder::default();
        let th = HashMap::new();
        assert_eq!(f.on_event(&ev(1, 0, "run.updated", "r1", "running"), &th).unwrap().k, "run_start");
        assert!(f.on_event(&ev(2, 1, "run.updated", "r1", "running"), &th).is_none());
        let mut end = ev(3, 2, "run.updated", "r1", "completed");
        end.started = Some(1000);
        end.completed = Some(61_000);
        let it = f.on_event(&end, &th).unwrap();
        assert_eq!(it.k, "run_end");
        assert_eq!(it.dur, Some(60_000));
    }

    #[test]
    fn tool_bursts_group_and_split() {
        let mut f = FeedBuilder::default();
        let th = HashMap::new();
        let mut a = ev(1, 0, "turn-item.updated", "i1", "running");
        a.item_type = Some("command_execution".into());
        let mut b = ev(2, 5_000, "turn-item.updated", "i2", "running");
        b.item_type = Some("dynamic_tool".into());
        b.tool = Some("Read".into());
        let mut b2 = b.clone();
        b2.seq = 3; // same item updated again: not a new call
        let mut c = ev(4, 60_000, "turn-item.updated", "i3", "running");
        c.item_type = Some("file_change".into());
        let first = f.on_event(&a, &th).unwrap();
        let second = f.on_event(&b, &th).unwrap();
        assert_eq!(first.id, second.id);
        assert_eq!(second.count, Some(2));
        assert!(f.on_event(&b2, &th).is_none());
        let third = f.on_event(&c, &th).unwrap();
        assert_ne!(third.id, first.id);
        assert_eq!(third.tools.unwrap()[0].0, "Edit");
    }
}
