# Switchboard

A small, local, live dashboard of your agent work: what HQ and every T3 thread
is doing right now, each subagent and the model it runs on, the last six hours,
today against yesterday and the day before, and an estimate of the 5-hour usage
window. One Rust binary, one page, nothing leaves the Mac.

Open it at <http://127.0.0.1:7317> once it is running.

## Use

```sh
scripts/switchboard build     # compile (needs cargo; done once, and after updates)
scripts/switchboard start     # run in the background
scripts/switchboard status    # pid, memory, CPU
scripts/switchboard open      # open the page in the browser
scripts/switchboard logs      # last log lines
scripts/switchboard stop
```

It is not started at login. Start it when you want it; stopping it loses
nothing, because the next start reads only what changed since.

## What it reads, and what it never does

| Source | How |
|---|---|
| `~/.t3/userdata/statev2.sqlite` | SQLite opened with `mode=ro`, 2 MB page cache, short read transactions |
| `~/.claude/projects/**/*.jsonl` | Read-only, only new bytes after the first pass, without filling the page cache |

- Never writes to, locks, or copies either source. Never reads
  `~/.t3/userdata/secrets` or any other T3 file.
- Shows metadata only: titles, models, statuses, tool names, times and token
  counts. Message text, tool inputs and outputs, commands and file paths never
  leave SQLite or the parser.
- Listens on 127.0.0.1 only and refuses requests whose `Host` is not localhost.
- The only files it writes are in `.state/`: `transcripts.json` (offsets and
  numbers, about 150 KB), `t3.json` (which thread launched which, about 10 KB),
  and the log. Deleting `.state/` is always safe.

## How the numbers are made

- **Tokens** come from the transcripts, counted once per API response (Claude
  Code writes one response over several lines). An independent counter in
  `tools/crosscheck.py` matched Switchboard exactly for 17 Nov 2026.
- **Threads, runs, subagents, PRs and the live step** come from T3's database.
  T3 sessions are joined to transcripts by session id.
- **HQ and its threads**: T3 does not record which thread launched another, so
  Switchboard finds the thread id inside the `t3_thread_launch` or
  `create_threads` call that created it.
- **5-hour window (estimate)**: a window opens at the first request after the
  previous one closed (rounded down to 10 minutes) and lasts five hours. Resets
  T3 recorded override the guess. The meter compares this window's usage, with
  each model weighted by its list price, against the usage at your last limit
  hit. claude.ai chat and other devices count toward the real limit but are not
  visible here.
- **Days** are local calendar days on this Mac.

## Footprint

About 10 to 15 MB of memory and well under 1% CPU while idle (measured in the
test container). The page shows its own memory and CPU in the footer.

## If something looks wrong

- A red banner saying T3's layout changed means a T3 update renamed something
  this reads. Token figures keep working; live thread data pauses until the
  queries in `src/t3.rs` are updated.
- `scripts/switchboard logs` shows startup timings and any read errors.

## Development

- `scripts/dtest.sh` runs `cargo test` (or any command) in a Rust container.
- `scripts/drun.sh` runs the Linux build in a container against the real data,
  mounted read-only, with T3's private folders hidden. Page on port 7318.
- `scripts/dshot.sh <name> <dark|light> <width> <height>` screenshots it from a
  headless Chromium container into `shots/`.
- `SWITCHBOARD_UI=ui/index.html` serves the page from disk, so UI edits need no
  rebuild.
- Design: `docs/design.md`. Plan: `docs/plan.md`.
