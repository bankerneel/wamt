# Configuration

wamt is configured entirely through environment variables. The launcher reads the same variables and adds a few of its own. Defaults that use `~` are expanded by the binary from `$HOME`.

## Binary settings

| Variable | Default | Description |
|---|---|---|
| `WAMT_BIND` | `127.0.0.1:7317` | Listen address and port. Keep this on loopback. |
| `WAMT_T3_DB` | `~/.t3/userdata/statev2.sqlite` | T3 Code SQLite snapshot, opened read-only. |
| `WAMT_CLAUDE_ROOT` | `~/.claude/projects` | Root of Claude Code JSONL transcripts. |
| `WAMT_CODEX_ROOT` | `~/.codex/sessions` | Root of Codex session files. Only `sessions` is read. |
| `WAMT_STATE` | `.state` | Directory for cache files and the log. |
| `WAMT_UI` | unset | Path to `index.html` to serve from disk instead of the embedded copy. Used for UI work. |
| `WAMT_ALLOW_HOSTS` | empty | Comma-separated extra `Host` header values to accept, needed when a container publishes the port. |
| `WAMT_UTC_OFFSET_MIN` | machine local offset | Override the UTC offset in minutes. Mainly for tests and containers. |

An empty value is treated as unset. To start the server with non-default paths:

```sh
WAMT_BIND=127.0.0.1:7319 \
WAMT_STATE=~/.wamt-state \
WAMT_UTC_OFFSET_MIN=0 \
target/release/wamt
```

## Launcher settings

`scripts/wamt` is a macOS convenience wrapper. It reads:

| Variable | Default | Description |
|---|---|---|
| `WAMT_STATE` | `$PWD/.state` | State directory, also passed to the binary. |
| `WAMT_URL` | `http://127.0.0.1:7317` | URL used for health checks and `open`. |
| `WAMT_DRY_RUN` | `0` | Set to `1` to print each step without running it. |

The launcher installs a launchd agent labelled `io.github.bankerneel.wamt` when `login on` is used, with a low process priority and logs in `.state/wamt.log`. `login off` removes it.

## Docker settings

The test scripts use their own overrides:

| Variable | Used by | Description |
|---|---|---|
| `SB_NAME` | `drun.sh`, `dshot.sh` | Container name, default `sb-test`. |
| `SB_PORT` | `drun.sh` | Host port, default `7318`. |
| `SB_TARGET` | `drun.sh`, `dtest.sh` | Cargo target volume, default `sb-target`. |
| `WAMT_CODEX_SRC` | `drun.sh` | Alternate Codex sessions folder to mount read-only. |
| `WAMT_UI` | `drun.sh` | Set to `/ui/index.html` by the script for live UI edits. |
| `EXTRA` | `dshot.sh` | Extra query string, for example `&demo`. |
| `THREAD` | `dshot.sh` | Thread id to open the drill-down in a screenshot. |

See [development.md](development.md) for the full Docker workflow.
