# Privacy and security

wamt is built to observe without interfering. It treats every local agent directory as read-only input and never makes a network request outward.

## What it reads

| Source | How it is read |
|---|---|
| `~/.t3/userdata/statev2.sqlite` | SQLite `mode=ro`, 2 MB page cache, short read transactions |
| `~/.claude/projects/**/*.jsonl` | Read-only, only new bytes after the first pass, without filling the page cache |
| `~/.codex/sessions`, `~/.codex/archived_sessions` | Same append-only reader, recent files only, token counts and model names only |

It never reads `~/.t3/userdata/secrets` or any other private T3 folder. In the Docker test container those folders are hidden behind empty tmpfs mounts. No source is written to, locked or copied.

## What it keeps

Only metadata: titles, models, statuses, tool names, times and token counts. Message text, tool inputs and outputs, commands and file paths never leave the parser. The page never contains a message body.

The only files the binary writes are in `.state/`:

- `transcripts.json`: byte offsets and per-day numbers, about 150 KB.
- `t3.json`: which thread launched which, about 10 KB.
- `history.json`: daily totals older than the live week, about 5 KB.
- `cc-limits.json`: the two Claude Code percentages and their reset times.
- `wamt.log`: startup timings and read errors.

`statusline on` additionally writes `settings.json.bak` and `statusline-orig.json` in the same directory so the original Claude Code `statusLine` can be restored exactly. Deleting `.state/` is always safe; the next start rebuilds what it needs.

## Network posture

- The listener binds to 127.0.0.1 by default. `WAMT_BIND` can point elsewhere, but doing so is your choice.
- Requests whose `Host` header is not localhost (or an explicitly allowed `WAMT_ALLOW_HOSTS` value) are refused with 403. This blocks DNS-rebinding pages from reading the dashboard.
- Every response sets `Content-Security-Policy: default-src 'none'`, restricts scripts and styles to inline, restricts connections to `self`, and sets `frame-ancestors 'none'`, `nosniff` and `no-referrer`.
- There are no outgoing HTTP clients in the binary. Fonts, the logo, the favicon and the demo fixtures are served from the binary itself.

## Demo mode

Opening `/?demo` loads synthetic fixtures compiled into the binary. In that mode the page makes no calls to `/api/state`, `/events`, `/api/thread`, `/api/timeline` or `/api/brief`, so a screenshot or a screen share cannot expose real usage.
