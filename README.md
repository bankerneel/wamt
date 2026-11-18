# Who Ate My Tokens

A small, local, live dashboard of your agent work: what HQ and every T3 thread
is doing right now, each subagent and the model it runs on, the last six hours,
today against yesterday and the day before, and an estimate of the 5-hour usage
window. One Rust binary, one page, nothing leaves the Mac.

Open it at <http://127.0.0.1:7317> once it is running.

## Use

```sh
scripts/wamt build     # compile (needs cargo; done once, and after updates)
scripts/wamt start     # run in the background
scripts/wamt status    # pid, memory, CPU
scripts/wamt open      # open the page in the browser
scripts/wamt logs      # last log lines
scripts/wamt stop
scripts/wamt update    # test in Docker, rebuild, restart and health-check
scripts/wamt update --no-docker   # skip the Docker test and Linux build
```

```sh
scripts/wamt login on      # start at login (a LaunchAgent, low priority)
scripts/wamt login off     # stop it and remove the login item
scripts/wamt login status
```

```sh
scripts/wamt statusline on      # wrap Claude Code's statusLine with WAMT
scripts/wamt statusline off     # put the original statusLine back
scripts/wamt statusline status  # show the active statusLine command
```

`statusline on` backs up `~/.claude/settings.json` to `.state/settings.json.bak`,
saves the original `statusLine` object in `.state/statusline-orig.json`, and
points `statusLine.command` at `scripts/statusline.sh`. The wrapper feeds your
original status line command the same stdin, appends the plate and week from
Claude Code's own numbers, and stores only `.rate_limits` in
`.state/cc-limits.json` for this dashboard. It adds "needs you" when a T3
thread is waiting and the dashboard is running. Errors in the wrapper are
swallowed so the status line never breaks. `statusline off` restores the
saved object exactly. Only the command is ever printed, never settings values.

With the login item on, `start`, `stop` and `restart` go through launchd;
`stop` keeps it stopped until the next login. Stopping loses nothing: the next
start reads only what changed since. The first start after an update that
changes the cache format reads the last 7 days once (about 6 s).

## What is on the page

- **Who's eating right now**: every active T3 thread with its subagents,
  model, current step, context fill and today's tokens. Click a row (or press
  Enter on it) for the thread's runs, tool counts, subagents and a 10-minute
  token timeline. Press `/` to filter by title, project, branch or model.
- **The 5-hour plate**: how much of the window is eaten, a run-out forecast
  from the last 30 minutes, and Codex's own limit line. When Claude Code's
  status line is installed, its `rate_limits` drive the plate and the weekly
  meter automatically: Claude's own percentage is the hero number, its reset
  is the reset time, and the local meter is scaled to match so bars, forecasts
  and alerts follow. Stale or missing reports fall back to the manual
  "Calibrate from /usage" (kept in this browser only). The muted line shows
  this Mac's own estimate.
- **Three days** and **the week so far**: daily totals, busy hours, model mix,
  runs, PRs with CI checks, review state and API-equivalent cost, and
  "spoiled leftovers" (requests that rebuilt over 50K tokens of cache after it
  expired). Below the week, a compact 30-day strip shows one thin column per
  day, stacked by model family, with a 30-day total; its tooltip gives the
  date, output tokens, requests and API-equivalent dollars. "Copy yesterday"
  and "Copy today" copy a plain-text standup; "Download CSV" saves every day
  available, up to 30.
- **Who ate when**: one day as horizontal lanes, one per T3 thread that ran
  that day (nested under its parent) and one per Claude Code session outside
  T3, with Today / Yesterday / day tabs. T3 runs are bars coloured by status
  with the model family as a top stripe, hatched waits where T3 records them,
  and 10-minute activity ticks for outside sessions on today (a first-to-last
  span on earlier days). A 00:00 to 24:00 axis, a "now" line for today and a
  busy-at-once strip on top show how many lanes were active at once. Click a
  thread lane for its runs on that day.
- **The bill**: tokens by model and by thread; "API prices" shows what they
  would cost at API list prices (an estimate; the plan is not billed this way).
- **Diet tips**: a Today and Yesterday tab of where tokens could be saved,
  computed from the day's aggregates: heavy contexts to start fresh, peak
  session concurrency, marathon sessions, Opus subagents, spoiled cache and
  the note-taker's share. Each tip names the threads and their share and
  API-equivalent cost. No message content is read.
- **Who ate it, by project**: a 30-day rollup by T3 project title, or by
  working-folder name outside T3. A table shows API-equivalent dollars, share,
  the last seven days, requests, output tokens, linked and merged PRs, a rough
  dollars-per-merged-PR figure and a small 30-day trend. Dollars are estimates
  at list prices, not what the plan bills. The month keeps at most twelve
  projects per day; the rest fold into "Other".
- **Alerts without looking**: the tab title and favicon show the plate and
  anything waiting on you. "Notify me" turns on desktop notifications for: a
  thread waiting on you, its prompt cache about to go cold, a thread passing
  85% context, one thread eating over 25% of the plate in 30 minutes, the
  plate passing 80% or running out before the reset, and a summary after
  20:00.

## What it reads, and what it never does

| Source | How |
|---|---|
| `~/.t3/userdata/statev2.sqlite` | SQLite opened with `mode=ro`, 2 MB page cache, short read transactions |
| `~/.claude/projects/**/*.jsonl` | Read-only, only new bytes after the first pass, without filling the page cache |
| `~/.codex/sessions/` and `~/.codex/archived_sessions/` | Same as above, only the last few days' files; token counts and model names only. Nothing else in `~/.codex` is read |

- Never writes to, locks, or copies either source. Never reads
  `~/.t3/userdata/secrets` or any other T3 file.
- Shows metadata only: titles, models, statuses, tool names, times and token
  counts. Message text, tool inputs and outputs, commands and file paths never
  leave SQLite or the parser.
- Listens on 127.0.0.1 only and refuses requests whose `Host` is not localhost.
- The only files it writes are in `.state/`: `transcripts.json` (offsets and
  numbers, about 150 KB), `t3.json` (which thread launched which, about 10 KB),
  `history.json` (daily totals older than the week, about 5 KB),
  `cc-limits.json` (Claude's own two percentages and reset times) and the log.
  `statusline on` also writes backups of the statusLine settings there.
  Deleting `.state/` is always safe.

## How the numbers are made

- **Tokens** come from the transcripts, counted once per API response (Claude
  Code writes one response over several lines). An independent counter in
  `tools/crosscheck.py` matched the dashboard exactly for 17 Nov 2026.
- **Threads, runs, subagents, PRs and the live step** come from T3's database.
  T3 sessions are joined to transcripts by session id.
- **HQ and its threads**: T3 does not record which thread launched another, so
  the dashboard finds the thread id inside the `t3_thread_launch` or
  `create_threads` call that created it.
- **5-hour window (estimate)**: a window opens at the first request after the
  previous one closed (rounded down to 10 minutes) and lasts five hours. Resets
  T3 recorded override the guess. The meter compares this window's usage, with
  each model weighted by its list price, against the usage at your last limit
  hit. claude.ai chat and other devices count toward the real limit but are not
  visible here.
- **Run-out forecast**: weighted usage over the last 30 minutes (idle minutes
  included, never less than 5) projected to the cap; shown only as an estimate.
- **Claude's own limits** replace the estimate when `statusline on` is set:
  the status line writes only `.rate_limits` to the state dir, the tailer picks
  it up on its one-second tick when the file changes, and the page ages the
  report to decide whether it is still fresh.
- **Codex** usage counts in the totals as its own app and model family but not
  in the Claude plate. Its limit line is Codex's own number.
- **Days** are local calendar days on this Mac. Seven are kept in the live
  window: the last three in detail, days 4 to 7 as daily totals only. When a
  day ages out, its totals move to `.state/history.json`, which keeps up to 31
  days (no sessions, hours or dedupe keys). On a fresh start the older days
  are read once on a low-priority background thread, from transcript files
  last written inside that range only; the live data is never held up. The
  month lives in its own file, so a cache rebuild never wipes it.

## Footprint

About 11 to 18 MB of memory (physical footprint on the Mac, depending on how
busy T3 is) and well under 1% CPU while idle. With no page open it skips
building the page data altogether; a hidden tab keeps its alerts but does not
redraw. The page shows the process's own memory and CPU in the footer.

## If something looks wrong

- A red banner saying T3's layout changed means a T3 update renamed something
  this reads. Token figures keep working; live thread data pauses until the
  queries in `src/t3.rs` are updated.
- `scripts/wamt logs` shows startup timings and any read errors.

## Development

- `scripts/dtest.sh` runs `cargo test` (or any command) in a Rust container.
- `scripts/drun.sh` runs the Linux build in a container against the real data,
  mounted read-only, with T3's private folders hidden. Page on port 7318.
- `scripts/dshot.sh <name> <dark|light> <width> <height>` screenshots it from a
  headless Chromium container into `shots/`.
- `WAMT_UI=ui/index.html` serves the page from disk, so UI edits need no
  rebuild.
- `scripts/test-statusline.sh` tests the statusLine wrapper and installer with
  a throwaway HOME and a fake dashboard. It never touches the real
  `~/.claude`.
- Design: `docs/design.md`. Plan: `docs/plan.md`.

## Menu bar

`scripts/swiftbar/wamt.30s.sh` is a SwiftBar or xbar plugin (neither app is
installed here). Symlink it into your plugin folder, for example:

```sh
mkdir -p "$HOME/Library/Application Support/SwiftBar/Plugins"
ln -sf "$PWD/scripts/swiftbar/wamt.30s.sh" "$HOME/Library/Application Support/SwiftBar/Plugins/wamt.30s.sh"
```

It shows the plate as `🍪 23%` and a menu with the week percentage, threads
that need you, running threads and a link that opens the dashboard. It reads
only `http://127.0.0.1:7317/api/brief`; with the dashboard down it shows `?`.

## The look

Styled with the "dream" gradient, cream panels, plum ink,
hard offset shadows, tilted tickets, a highlighter, Archivo and Caveat (both
SIL Open Font License, bundled in `ui/assets/fonts/`). The pixel wordmark is
drawn in SVG by the page; the bitten-token logo (`ui/assets/logo-mark*.png`)
was generated by a Codex subagent. Fonts and images are compiled into the
binary, so the page still makes no outside requests. Design notes:
`docs/redesign.md`.
