#!/usr/bin/env python3
"""Build the synthetic fixtures served in demo mode.

Run from anywhere: python3 tools/make-demo.py
Output: ui/assets/demo/{state,thread,timeline}.json

Every project, thread, id and number below is invented. The keys and nesting
mirror the three live API payloads so the page can render them unchanged.
A fixed seed keeps the fixtures byte-for-byte identical on every run.
"""

import json
import os
import random
from datetime import datetime, timedelta, timezone

SEED = 20261118
rng = random.Random(SEED)

OFF = 0  # minutes east of UTC; the page formats everything with this
NOW = int(datetime(2026, 11, 18, 16, 20, tzinfo=timezone.utc).timestamp() * 1000)
MIN = 60_000
HOUR = 60 * MIN
DAY = 24 * HOUR
TODAY_START = NOW - (NOW % DAY)

PROJECTS = ["acme-api", "acme-web", "docs-site", "infra",
            "acme-mobile", "design-system", "data-pipeline", "edge-proxy"]
MODELS = ["claude-opus-5-5", "claude-sonnet-5-5", "claude-haiku-5-5"]
CODEX_MODEL = "gpt-6.1-sol"
FAMILIES = ["opus", "sonnet", "haiku", "other"]
FAM = {"claude-opus-5-5": 0, "claude-sonnet-5-5": 1, "claude-haiku-5-5": 2,
       CODEX_MODEL: 4}
PRICES = {"claude-haiku-5-5": [0.1, 0.5, 0.01],
          "claude-opus-5-5": [4.0, 20.0, 0.2],
          "claude-sonnet-5-5": [2.0, 10.0, 0.1]}

ORCH = "thread:acme:release-train"
DEMO_THREAD = "thread:acme-api:refactor-auth-middleware"


def usd(amount):
    """Weighted units: one dollar is one million units."""
    return round(amount * 1_000_000, 2)


def use(out, requests, dollars):
    """The five-number token vector plus request count and weighted usage."""
    inp = int(out * rng.uniform(0.04, 0.12))
    read = int(out * rng.uniform(18, 34))
    create = int(out * rng.uniform(0.4, 1.4))
    other = int(out * rng.uniform(0.6, 1.8))
    return {"t": [inp, int(out), read, create, other],
            "n": requests, "w": usd(dollars)}


def zero_use():
    return {"t": [0, 0, 0, 0, 0], "n": 0, "w": 0.0}


def fam4(values):
    assert len(values) == 4
    return [round(v, 2) for v in values]


def day_label(days_ago):
    return (datetime(2026, 11, 18, tzinfo=timezone.utc) - timedelta(days=days_ago)).strftime("%Y-%m-%d")


# ---------------------------------------------------------------- live page

def make_threads():
    def thr(tid, title, project, parent, rel, model, status, step, ctx_used,
            dollars, pr=None, waiting=None):
        return {
            "id": tid, "title": title, "project": project, "parent": parent,
            "launcher": None, "rel": rel, "model": model, "effort": None,
            "fast": False, "source": "mcp",
            "created": NOW - rng.randint(20, 190) * MIN,
            "updated": NOW - rng.randint(1, 12) * MIN,
            "settled": None, "snoozed": None, "reset_at": None,
            "branch": None, "archived": False,
            "prs": 1 if pr else 0, "pr": pr,
            "run": {"run": "run:" + tid, "thread": tid, "ordinal": rng.randint(1, 8),
                    "status": status, "requested": NOW - 9 * MIN,
                    "started": NOW - 8 * MIN, "model": model, "effort": None,
                    "delegated": rel == "subagent",
                    "step": {"label": step, "kind": "tool", "since": NOW - 2 * MIN},
                    "tools": rng.randint(8, 60)} if status in ("running", "pending", "waiting") else None,
            "ctx": {"used": ctx_used, "max": 1_000_000,
                    "cache_exp": NOW + rng.randint(10, 50) * MIN,
                    "model": model, "at": NOW - MIN} if ctx_used else
                   {"used": 0, "max": 0, "cache_exp": None, "model": None, "at": 0},
            "waiting": waiting,
            "today": use(rng.randint(3_000, 90_000), rng.randint(20, 400), dollars),
            "eat30": round(usd(dollars * rng.uniform(0.02, 0.2)), 2),
            "last_req": NOW - rng.randint(1, 25) * MIN,
        }

    pr = {"n": 241, "repo": "acme/api", "linked": NOW - 42 * MIN, "state": "open",
          "title": "Refactor auth middleware onto the shared session store",
          "draft": False, "checks": "passing", "review": "review_required"}
    threads = [
        thr(ORCH, "Acme release train 2.4", "infra", None, None,
            "claude-opus-5-5", "running", "Bash", 412_000, 26.4),
        thr(DEMO_THREAD, "Refactor auth middleware", "acme-api", ORCH, None,
            "claude-opus-5-5", "running", "Edit", 857_000, 18.7, pr=pr),
        thr("thread:acme-api:billing-webhooks", "Add billing webhooks", "acme-api", ORCH, None,
            "claude-sonnet-5-5", "running", "Read", 366_000, 9.2),
        thr("thread:acme-web:flaky-e2e", "Fix flaky e2e tests", "acme-web", ORCH, None,
            "claude-haiku-5-5", "waiting", "Bash", 0, 3.1,
            waiting={"thread": "thread:acme-web:flaky-e2e", "kind": "approval", "since": NOW - 6 * MIN}),
        thr("thread:acme-web:cache-headers", "Tune cache headers", "acme-web", ORCH, None,
            "claude-opus-5-5", "completed", "Read", 0, 5.8),
        thr("thread:docs-site:launch-notes", "Write launch notes", "docs-site", ORCH, None,
            "claude-sonnet-5-5", "completed", "Edit", 0, 2.3),
        thr("thread:sub:repo-map", "Map the service boundaries", "acme-api", DEMO_THREAD, "subagent",
            "claude-haiku-5-5", "completed", "Read", 0, 1.1),
        thr("thread:sub:migration-check", "Check the migration plan", "infra",
            "thread:acme-api:billing-webhooks", "subagent",
            CODEX_MODEL, "running", "Bash", 0, 4.6),
    ]
    return threads


def make_subs():
    rows = []
    specs = [
        ("Map the service boundaries", "claude-haiku-5-5", "completed", DEMO_THREAD),
        ("Draft webhook handlers", "claude-sonnet-5-5", "completed", "thread:acme-api:billing-webhooks"),
        ("Check the migration plan", CODEX_MODEL, "running", "thread:acme-api:billing-webhooks"),
        ("Isolate the flaky spec", "claude-haiku-5-5", "completed", "thread:acme-web:flaky-e2e"),
        ("Review header policy", "claude-opus-5-5", "completed", "thread:acme-web:cache-headers"),
        ("Collect release notes", "claude-sonnet-5-5", "running", "thread:docs-site:launch-notes"),
    ]
    for i, (title, model, status, parent) in enumerate(specs):
        start = NOW - rng.randint(30, 150) * MIN
        rows.append({"id": "sub:acme:%02d" % i, "thread": ORCH,
                     "run": "run:thread:acme:%02d" % i,
                     "child": "thread:sub:acme:%02d" % i, "origin": "app_owned",
                     "status": status, "model": model, "title": title,
                     "started": start,
                     "completed": start + rng.randint(2, 24) * MIN if status != "running" else None,
                     "updated": NOW - rng.randint(1, 30) * MIN})
    return rows


def make_live():
    cap_used = 200_000_000.0
    used = 124_000_000.0
    families = fam4([74_000_000, 32_000_000, 12_000_000, 6_000_000])
    start = NOW - 2 * HOUR
    end = start + 5 * HOUR
    spark_w, spark_obs = [], []
    for i in range(72):
        wave = max(0.0, 1.0 - abs(i - 38) / 26.0)
        base = [usd(rng.uniform(0.1, 1.4) * (0.6 + wave)),
                usd(rng.uniform(0.05, 0.7) * (0.5 + wave)),
                usd(rng.uniform(0.01, 0.3) * wave), 0.0]
        spark_w.append(fam4(base))
        spark_obs.append(round(rng.uniform(0, 900), 2))

    return {
        "now": NOW, "off": OFF, "families": FAMILIES,
        "health": {"t3": "ok", "t3_error": None, "problems": [], "warnings": [],
                   "seq": 128_442, "last_event": NOW - MIN, "tx_ready": True,
                   "files": 42, "bytes_read": 11_800_000, "scan_ms": 46,
                   "mem": 13_900_000, "cpu": 1.1, "uptime": 3 * HOUR // MIN,
                   "launch_links": 9},
        "today": {"tokens": 118_420_000, "output": 2_940_000, "calls": 1642,
                  "active_min": 412, "runs": 28, "runs_live": 3, "threads": 9,
                  "subagents": 14, "subagents_live": 2, "prs": 6, "hits": 0,
                  "waiting": 1},
        "window": {"active": True, "start_ms": start, "end_ms": end,
                   "anchored": False, "used": used, "by_family": families,
                   "observer": 28_400.0, "calls": 74,
                   "cap": {"used": cap_used, "hit_ms": start - 26 * HOUR,
                           "start_ms": start - 28 * HOUR},
                   "rate_per_min": 620_000.0, "eta_ms": NOW + 118 * MIN,
                   "pct": round(used / cap_used, 4)},
        "spark": {"start": NOW - 6 * HOUR, "step": 5 * MIN, "w": spark_w, "obs": spark_obs},
        "threads": make_threads(),
        "subs": make_subs(),
        "outside": [
            {"id": "outside:cli:1", "label": "acme-api CLI session", "app": "cli",
             "cwd": "acme-api", "model": "claude-opus-5-5", "active": True,
             "last": NOW - 2 * MIN, "today": use(52_000, 31, 6.4)},
            {"id": "outside:cli:2", "label": "docs-site CLI session", "app": "cli",
             "cwd": "docs-site", "model": CODEX_MODEL, "active": False,
             "last": NOW - 24 * MIN, "today": use(11_000, 9, 0.9)},
        ],
        "notetaker": {"last": NOW - 22 * MIN, "active": False,
                      "model": "claude-haiku-5-5",
                      "today": use(78_000, 55, 0.6)},
        "codex_limit": {"ts": NOW - 6 * HOUR, "pct": 18.0, "win_min": 10080,
                        "resets_ms": NOW + 4 * DAY, "plan": "plus"},
        "cc_limits": None,
    }


# -------------------------------------------------------------- detailed days

def make_sources(day_i):
    n = 8 - day_i
    out = []
    labels = [
        ("Refactor auth middleware", "acme-api", "claude-opus-5-5"),
        ("Add billing webhooks", "acme-api", "claude-sonnet-5-5"),
        ("Fix flaky e2e tests", "acme-web", "claude-haiku-5-5"),
        ("Tune cache headers", "acme-web", "claude-opus-5-5"),
        ("Write launch notes", "docs-site", "claude-sonnet-5-5"),
        ("Migrate to Postgres 16", "infra", "claude-opus-5-5"),
        ("Refresh design tokens", "design-system", CODEX_MODEL),
        ("Backfill event exports", "data-pipeline", "claude-haiku-5-5"),
    ]
    for i, (label, project, model) in enumerate(labels[:n]):
        big = 14.0 - i * 1.3 - day_i * 1.1
        first = TODAY_START - day_i * DAY + rng.randint(8, 11) * HOUR
        last = min(NOW, first + rng.randint(2, 7) * HOUR)
        out.append({
            "key": "t:acme:%02d" % i, "app": "t3", "label": label,
            "thread": "thread:acme:%02d" % i,
            "up": "Acme release train 2.4" if i else None,
            "project": project,
            "total": use(rng.randint(2_000, 60_000), rng.randint(20, 320), big),
            "sub": use(rng.randint(400, 12_000), rng.randint(5, 70), big * 0.18),
            "cold": {"n": rng.randint(0, 3), "tok": rng.randint(0, 900_000),
                     "w": usd(rng.uniform(0, 2.4))},
            "models": {model: use(rng.randint(2_000, 60_000), rng.randint(20, 320), big)},
            "first": first, "last": last, "sessions": rng.randint(1, 3),
        })
    return out


def make_prs():
    specs = [
        (241, "acme/api", "Refactor auth middleware onto the shared store", "open", None, "review_required", False),
        (238, "acme/api", "Add idempotency keys to billing webhooks", "merged", "passing", "approved", False),
        (236, "acme/web", "Stabilise the checkout e2e suite", "merged", "passing", None, False),
        (233, "acme/infra", "Bump Postgres to 16", "merged", "failing", None, False),
        (229, "acme/docs-site", "Draft the 2.4 launch notes", "open", "pending", None, True),
        (224, "acme/design-system", "Refresh the token palette", "merged", "passing", "changes_requested", False),
    ]
    return [{"n": n, "repo": repo, "title": title, "state": state, "draft": draft,
             "checks": checks, "review": review,
             "thread": repo.split("/")[-1], "thread_id": "00000000-0000-4000-8000-%012d" % (i + 1),
             "linked": NOW - rng.randint(30, 600) * MIN}
            for i, (n, repo, title, state, checks, review, draft) in enumerate(specs)]


def make_tips(day_i):
    total = usd(96.0 - day_i * 12.0)
    return [
        {"kind": "concurrency", "label": "Too many at once",
         "text": "7 sessions were active at once at 14:20; together they ate 82% of the day, about $79.00",
         "who": "Refactor auth middleware, Add billing webhooks, Fix flaky e2e tests, Migrate to Postgres 16",
         "w": total, "share": 0.82, "usd": total / 1e6,
         "tip": "<b>Run fewer sessions at once.</b><br>This is the busiest stretch of the day. Each extra session adds context that has to be rebuilt."},
        {"kind": "cold", "label": "Cold cache",
         "text": "5 requests rebuilt over 50K tokens of cache, about $8.40 of extra work",
         "who": "Tune cache headers, Refresh design tokens",
         "w": usd(8.4), "share": 0.09, "usd": 8.4,
         "tip": "<b>Keep related turns together.</b><br>A gap over five minutes lets the prompt cache go cold, so the next request pays to rebuild it."},
        {"kind": "context", "label": "Heavy context",
         "text": "One session carried 857K tokens of context, about $18.70",
         "who": "Refactor auth middleware",
         "w": usd(18.7), "share": 0.19, "usd": 18.7,
         "tip": "<b>Start a fresh thread for a new task.</b><br>A long context raises the cost of every later request in that session."},
    ]


def make_day(day_i):
    start = TODAY_START - day_i * DAY
    factor = 1.0 - day_i * 0.18
    dollars = 96.0 * factor
    models = {
        "claude-haiku-5-5": use(int(78_000 * factor), 55, 0.6 * factor),
        "claude-opus-5-5": use(int(2_860_000 * factor), 940, dollars * 0.72),
        "claude-sonnet-5-5": use(int(1_140_000 * factor), 410, dollars * 0.26),
    }
    total = use(int(4_080_000 * factor), 1405, dollars)
    hours = [fam4([usd(rng.uniform(0.4, 3.4) * factor * (0.4 + max(0, 1 - abs(h - 14) / 9))),
                   usd(rng.uniform(0.2, 1.6) * factor),
                   usd(rng.uniform(0.02, 0.4) * factor), 0.0]) for h in range(24)]
    hours_n = [rng.randint(0, 150) for _ in range(24)]
    threads = [
        {"id": "thread:acme-api:refactor-auth-middleware", "title": "Refactor auth middleware",
         "up": "Acme release train 2.4", "runs": 19, "failed": 0,
         "run_ms": 8_900_000},
        {"id": "thread:acme-api:billing-webhooks", "title": "Add billing webhooks",
         "up": "Acme release train 2.4", "runs": 12, "failed": 1, "run_ms": 5_200_000},
        {"id": "thread:acme-web:flaky-e2e", "title": "Fix flaky e2e tests",
         "up": "Acme release train 2.4", "runs": 8, "failed": 0, "run_ms": 3_100_000},
        {"id": "thread:docs-site:launch-notes", "title": "Write launch notes",
         "up": "Acme release train 2.4", "runs": 5, "failed": 0, "run_ms": 1_700_000},
    ][:4 - day_i]
    return {
        "day": day_label(day_i), "start": start, "offset": OFF,
        "models": models, "total": total, "hours": hours, "hours_n": hours_n,
        "active_min": int(410 * factor),
        "apps": {"notetaker": use(int(78_000 * factor), 55, 0.6 * factor),
                 "t3": use(int(4_000_000 * factor), 1350, dollars * 0.99)},
        "sources": make_sources(day_i), "sources_more": 0,
        "t3": {"runs": int(28 * factor),
               "by_status": {"completed": int(25 * factor), "failed": 1,
                             "interrupted": 0, "running": 2},
               "run_ms": int(24_000_000 * factor), "threads_touched": 9 - day_i,
               "threads": threads, "subagents": int(14 * factor), "sub_failed": 1,
               "sub_ms": int(15_000_000 * factor),
               "sub_models": {"claude-opus-5-5": 5, "claude-sonnet-5-5": 4,
                              "claude-haiku-5-5": 3, CODEX_MODEL: 2},
               "prs": make_prs() if day_i == 0 else make_prs()[:3], "created": 9 - day_i},
        "outside_subs": {}, "hits": [], "resets": [],
        "by_now": usd(dollars * 0.97),
        "cold": {"n": 5, "tok": 480_000, "w": usd(8.4)},
        "tips": make_tips(day_i) if day_i == 0 else make_tips(day_i)[:2],
    }


def make_week():
    out = []
    for i in range(6, -1, -1):
        start = TODAY_START - i * DAY
        dollars = 96.0 * (1.0 - i * 0.11)
        hours = [fam4([usd(rng.uniform(0.2, 2.8) * (1 - i * 0.1)),
                       usd(rng.uniform(0.1, 1.2)), usd(rng.uniform(0, 0.3)), 0.0])
                 for _ in range(24)]
        out.append({"day": day_label(i), "start": start,
                    "total": use(int(4_000_000 * (1 - i * 0.1)), 1400 - i * 90, dollars),
                    "fam": fam4([usd(dollars * 0.72), usd(dollars * 0.25),
                                 usd(dollars * 0.03), 0.0]),
                    "codex_tokens": rng.randint(0, 120_000),
                    "hours": hours, "active_min": 400 - i * 30})
    return out


def make_month():
    out = []
    for i in range(29, -1, -1):
        dollars = rng.uniform(18, 105)
        out.append({"day": day_label(i),
                    "t": [rng.randint(200, 6000), rng.randint(1000, 90_000),
                          rng.randint(20_000, 2_000_000), rng.randint(100, 9000),
                          rng.randint(200, 12_000)],
                    "n": rng.randint(40, 600), "w": usd(dollars),
                    "fam": fam4([usd(dollars * 0.7), usd(dollars * 0.25),
                                 usd(dollars * 0.05), 0.0]),
                    "codex_tokens": rng.randint(0, 90_000),
                    "active_min": rng.randint(60, 420)})
    return out


def make_projects():
    out = []
    for i, name in enumerate(PROJECTS):
        w30 = usd(210 - i * 21)
        out.append({"name": name, "w30": w30, "w7": round(w30 * rng.uniform(0.2, 0.4), 2),
                    "n30": 1800 - i * 170, "out30": 1_700_000 - i * 150_000,
                    "prs": 32 - i * 3, "merged": 28 - i * 3,
                    "days": [round(rng.uniform(0, 1) * (w30 / 30), 2) for _ in range(30)]})
    return out


def make_feed():
    kinds = ["run_start", "run_end", "tools", "waiting", "pr", "sub_start",
             "sub_end", "handoff", "thread_new", "settled"]
    titles = ["Refactor auth middleware", "Add billing webhooks",
              "Fix flaky e2e tests", "Tune cache headers", "Write launch notes",
              "Migrate to Postgres 16"]
    out = []
    for i in range(64):
        k = kinds[i % len(kinds)]
        title = titles[i % len(titles)]
        item = {"id": "demo:feed:%03d" % i, "ts": NOW - i * rng.randint(2, 9) * MIN,
                "k": k, "th": "thread:acme:%02d" % (i % 6), "tt": title,
                "up": "Acme release train 2.4", "dur": rng.randint(20_000, 900_000),
                "tools": [["Bash", rng.randint(1, 12)], ["Read", rng.randint(1, 9)],
                          ["Edit", rng.randint(1, 6)]],
                "count": rng.randint(2, 14), "model": MODELS[i % 3],
                "status": "completed" if i % 5 else "waiting",
                "n": rng.randint(1, 30), "title": title,
                "repo": "acme/api"}
        out.append(item)
    return out


def make_state():
    week = make_week()
    return {"live": make_live(),
            "days": {"now": NOW, "days": [make_day(i) for i in range(3)],
                     "week": week,
                     "week_info": {"total_w": round(sum(d["total"]["w"] for d in week), 2),
                                   "codex_tokens": 286_000, "hits": []},
                     "month": make_month(), "projects": make_projects(),
                     "prices": PRICES},
            "feed": make_feed()}


# -------------------------------------------------------------- drill-down

def make_thread_fixture():
    day_start = TODAY_START
    runs = [{"at": day_start + h * HOUR + rng.randint(0, 50) * MIN,
             "delegated": False, "model": MODELS[i % 3],
             "ms": rng.randint(60_000, 2_600_000), "n": i + 1,
             "status": "completed" if i else "running"}
            for i, h in enumerate([8, 9, 10, 11, 13, 14, 15])]
    buckets = [{"out": rng.randint(1_000, 9_000),
                "t": day_start + h * HOUR,
                "w": fam4([usd(rng.uniform(1, 9)), usd(rng.uniform(0.2, 2)),
                           usd(rng.uniform(0, 0.6)), 0.0])}
               for h in range(8, 16)]
    return {
        "day": day_label(0), "day_start": day_start, "id": DEMO_THREAD,
        "is_today": True,
        "items": [{"n": 48, "type": "command_execution"},
                  {"n": 31, "type": "reasoning"},
                  {"n": 24, "type": "assistant_message"},
                  {"n": 18, "type": "dynamic_tool"},
                  {"n": 6, "type": "user_message"}],
        "model": "claude-opus-5-5", "now": NOW, "project": "acme-api",
        "runs": runs,
        "subagents": [
            {"title": "Map the service boundaries", "model": "claude-haiku-5-5",
             "at": day_start + 9 * HOUR, "ms": 420_000, "status": "completed",
             "origin": "app_owned"},
            {"title": "Draft the migration check", "model": CODEX_MODEL,
             "at": day_start + 13 * HOUR, "ms": 980_000, "status": "running",
             "origin": "provider_native"},
        ],
        "timeline": {"bucket_ms": HOUR, "buckets": buckets, "sessions": 2},
        "title": "Refactor auth middleware", "today": day_label(0),
        "tools": [{"n": 48, "name": "Bash"}, {"n": 24, "name": "Read"},
                  {"n": 18, "name": "Edit"}, {"n": 11, "name": "Grep"}],
    }


def make_timeline_fixture():
    lanes = []
    specs = [
        (DEMO_THREAD, "Refactor auth middleware", ORCH, "t3", "claude-opus-5-5", 0, 95),
        ("thread:acme:release-train", "Acme release train 2.4", None, "t3", "claude-opus-5-5", 0, 88),
        ("thread:acme-api:billing-webhooks", "Add billing webhooks", ORCH, "t3", "claude-sonnet-5-5", 1, 74),
        ("thread:acme-web:flaky-e2e", "Fix flaky e2e tests", ORCH, "t3", "claude-haiku-5-5", 2, 63),
        ("thread:acme-web:cache-headers", "Tune cache headers", ORCH, "t3", "claude-opus-5-5", 0, 51),
        ("thread:docs-site:launch-notes", "Write launch notes", ORCH, "t3", "claude-sonnet-5-5", 1, 40),
        ("thread:infra:postgres-16", "Migrate to Postgres 16", ORCH, "t3", "claude-opus-5-5", 0, 35),
        ("thread:infra:edge-proxy", "Tune edge proxy rules", ORCH, "t3", CODEX_MODEL, 3, 22),
    ]
    for i, (tid, title, parent, kind, model, fam_i, score) in enumerate(specs):
        runs = []
        for j in range(3):
            at = TODAY_START + rng.randint(8, 15) * HOUR + rng.randint(0, 50) * MIN
            runs.append({"at": at, "fam": fam_i, "model": model,
                         "n": rng.randint(1, 4),
                         "status": "completed" if j else "running",
                         "to": min(NOW, at + rng.randint(8, 70) * MIN)})
        waits = []
        if i == 3:
            waits = [{"at": TODAY_START + 10 * HOUR, "kind": "approval",
                      "to": TODAY_START + 10 * HOUR + 6 * MIN}]
        lanes.append({"fam": fam_i, "id": tid, "kind": kind, "model": model,
                      "parent": parent, "runs": runs, "score": score,
                      "title": title, "waits": waits})
    for i, (label, model) in enumerate([("acme-api CLI session", "claude-opus-5-5"),
                                        ("docs-site CLI session", CODEX_MODEL)]):
        slots = [rng.randint(0, 143) for _ in range(5)]
        lanes.append({"fam": FAM[model], "id": "outside:cli:%d" % i, "kind": "outside",
                      "model": model, "parent": None, "ticks": slots,
                      "runs": [{"at": TODAY_START + s * 10 * MIN, "fam": FAM[model],
                                "model": model, "n": 1, "status": "completed",
                                "to": TODAY_START + (s + rng.randint(2, 12)) * 10 * MIN}
                               for s in slots],
                      "score": 18 - i * 4, "title": label, "waits": []})
    peak = [rng.randint(0, 9) for _ in range(144)]
    peak[84] = 9
    return {"bucket_ms": 10 * MIN, "cap": 40, "day": day_label(0),
            "day_end": TODAY_START + DAY, "day_start": TODAY_START,
            "is_today": True, "lanes": lanes, "now": NOW, "off": OFF,
            "omitted": 0, "peak": peak, "peak_max": 9,
            "sessions_available": True, "slots": 144, "today": day_label(0),
            "wait_available": True}


def main():
    root = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
    out_dir = os.path.join(root, "ui", "assets", "demo")
    os.makedirs(out_dir, exist_ok=True)
    fixtures = {"state.json": make_state(),
                "thread.json": make_thread_fixture(),
                "timeline.json": make_timeline_fixture()}
    for name, data in fixtures.items():
        path = os.path.join(out_dir, name)
        with open(path, "w", encoding="utf-8") as f:
            json.dump(data, f, ensure_ascii=False, separators=(",", ":"))
            f.write("\n")
        print("%s (%d bytes)" % (path, os.path.getsize(path)))


if __name__ == "__main__":
    main()
