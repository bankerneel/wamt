# Architecture

wamt is one Rust binary with no async runtime. It reads three local sources, keeps a small in-memory picture of recent usage, and serves one HTML page over HTTP and Server-Sent Events.

## Threads

| Thread | Job | Pace |
|---|---|---|
| `main` | Binds the listener, builds the initial snapshot, owns the HTTP server | One connection per spawned `conn` thread |
| `t3` | Opens the T3 SQLite snapshot read-only, reloads threads, runs, subagents, PRs and launch links | About every 10 s, faster while busy, 60 s when idle |
| `tail` | Reads new bytes from Claude Code and Codex transcript files, updates day buckets and the plate | About once per second |
| `backfill` | On a fresh cache, reads older transcript files once, in the background | Low priority, never blocks the live threads |
| `conn` | One per HTTP request, capped at 24 open connections | Short lived |

All workers share an `Arc<Mutex<Shared>>`. Locks are short: workers compute outside the lock where possible, then publish. The HTTP server never reads source files on the page path except the on-demand drill-down and timeline builders.

## Data sources

- **T3 Code** (`~/.t3/userdata/statev2.sqlite`): opened with SQLite `mode=ro`, a 2 MB page cache and short read transactions. Modules in `src/t3.rs` query the projection tables for threads, runs, turn items, run attempts and events. If a T3 update renames a column, `schema_problems()` and `json_drift()` report it in the page banner; token counting continues.
- **Claude Code** (`~/.claude/projects/**/*.jsonl`): `src/tail.rs` tracks a byte offset per file and reads only appended bytes. `src/transcript.rs` parses assistant messages and usage objects. It uses `posix_fadvise`-style hints where available so scanning does not fill the page cache.
- **Codex** (`~/.codex/sessions`, `~/.codex/archived_sessions`): the same append-only reader, limited to recent files, extracting token counts and model names only. No auth or config files are opened.
- **Claude limits** (`.state/cc-limits.json`): written by the status line wrapper, read on the tailer's tick when the file changes.

Launch relationships are not recorded by T3 directly, so `src/t3.rs` looks for the thread id inside the `t3_thread_launch` or `create_threads` tool item that created the child. The result is cached in `.state/t3.json`.

## Aggregation

`src/agg.rs` folds parsed responses into token vectors of five numbers (input, output, cache read, cache write, other) weighted by list price. Weighting lets Opus, Sonnet, Haiku and GPT usage share one meter; GPT is counted in tokens and totals but not in the Claude plate.

Records are bucketed three ways:

- 10-minute buckets for the last six hours (the spark) and the 5-hour window.
- Hourly cells for each of the last seven days.
- Daily totals for days 8 to 31, moved to `.state/history.json` as they age out.

A "cold leftover" is a request that rebuilt more than 50K tokens of cache after a gap of over five minutes (or 60 minutes for 1-hour caching). Diet tips in `src/payload.rs` are derived from the same aggregates: heavy contexts, peak concurrency, long sessions, Opus subagents, cold cache and the note-taker's share. No message text feeds these calculations.

## Payloads

`src/payload.rs` composes three JSON documents:

- **`live`**: clock, offset, source health, today's totals, the 5-hour window, the six-hour spark, active threads and subagents, outside sessions, note-taker state, Codex limit and Claude limits.
- **`days`**: three detailed days, a seven-day week, week info, a 30-day strip, project rollups and model prices.
- **`feed`**: the last few hundred activity events (run start and end, tool batches, waiting, PRs, subagents, handoffs).

`/api/state` returns all three together. The drill-down (`src/drill.rs`) and the day timeline (`src/timeline.rs`) build their smaller JSON documents on demand from the shared state and the read-only database. `src/projects.rs` attributes each session to a T3 project title or, outside T3, to a working-folder basename; the month keeps at most twelve projects per day and folds the rest into "Other".

## HTTP and SSE

`src/http.rs` is a small HTTP/1.1 server on `std::net`. GET and HEAD only. Routes:

| Route | Response |
|---|---|
| `/` | The page, embedded or read from `WAMT_UI` |
| `/api/state` | All three payloads as JSON |
| `/api/brief` | A tiny summary for a status line or menu-bar plugin |
| `/api/thread?id=...` | One thread's day: runs, tools, items, subagents, token timeline |
| `/api/timeline?day=...` | One day as horizontal lanes plus a busy-at-once strip |
| `/events` | Server-Sent Events: `hello`, `live`, `days`, `feed` |
| `/healthz` | `ok` |
| `/assets/...` | Fonts, logo, favicon and demo fixtures |

Requests whose `Host` is not localhost (or an explicitly allowed extra host) get 403. Every response carries a strict CSP, `nosniff`, `no-referrer` and `frame-ancestors 'none'`. Assets are cached for a day; data is never cached.

## Memory design

The page is compiled into the binary as a `&'static str`, and fonts, images and demo fixtures as `&'static [u8]`, so they live in read-only pages and cost no heap. The heap holds token aggregates for seven days, a feed capped at 600 entries, a few hundred file offsets and a small T3 thread map. Cached files in `.state/` are JSON, not a database, and are rewritten atomically. Typical physical memory is 11 to 18 MB. When no page is open, workers still track sources but skip composing page data; a hidden tab keeps alerts without redrawing.
