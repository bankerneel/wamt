# Development

## Local setup

```sh
cargo build --release
cargo test
```

Never run the server against real data on the host during development. Use the Docker scripts below. On macOS, `orb start` starts the Docker engine if it is not already running; wait until `docker info` succeeds.

## Docker test scripts

All scripts live in `scripts/` and assume the project root as the working directory.

### `scripts/dtest.sh`

Runs a command in a Rust container with the project mounted read-write at `/src` and cargo caches in named volumes. With no arguments it runs `cargo test`.

```sh
scripts/dtest.sh
scripts/dtest.sh sh -c 'cargo test && cargo build --release'
```

`SB_TARGET` selects the cargo target volume (default `sb-target`), so parallel checkouts can build side by side.

### `scripts/drun.sh`

Builds nothing; it runs `target/release/wamt` from the shared `sb-target` volume inside a container against real data mounted read-only. Private T3 folders are hidden behind empty tmpfs mounts, and only `~/.codex/sessions` and `~/.codex/archived_sessions` are mounted. The page is served from `ui/index.html` on port 7318.

```sh
scripts/drun.sh          # container name sb-test, host port 7318
```

It accepts `SB_NAME`, `SB_PORT` and `SB_TARGET`:

```sh
SB_NAME=wamt-a SB_PORT=7319 SB_TARGET=sb-target-a scripts/drun.sh
```

Run `cargo build --release` through `scripts/dtest.sh` first, since the container uses the shared target volume.

### `scripts/dshot.sh`

Screenshots the running container's page with headless Chromium into `shots/`.

```sh
scripts/dshot.sh overview dark 1600 2600
EXTRA='&demo' scripts/dshot.sh overview-demo light 1400 1200
THREAD=thread:acme-api:refactor-auth-middleware EXTRA='&demo' scripts/dshot.sh drill light 1400 2200
```

Arguments are `<name> <light|dark> <width> <height>`. `SB_NAME` and `SB_PORT` select the target container. `EXTRA` appends extra query parameters and `THREAD` opens the drill-down panel.

Crop screenshots with `sips`:

```sh
sips -c <height> <width> --cropOffset <y> <x> shots/name.png --out docs/screenshots/name.png
```

Note that `sips` treats a `0 0` crop offset as unset and centers the crop; use a one-pixel offset to crop from the top. Keep images under about 400 KB.

## UI work

`WAMT_UI` serves the page from disk, so HTML edits show up on reload without a rebuild:

```sh
WAMT_UI=/absolute/path/to/ui/index.html target/release/wamt
```

In the container, `drun.sh` sets `WAMT_UI=/ui/index.html` and mounts `ui/` read-only. Under `Ui::Disk`, assets are read from `ui/assets/` on every request.

## Releasing an update

```sh
scripts/wamt update              # test in Docker, build release, restart, health-check
scripts/wamt update --no-docker  # skip the Docker test and Linux build
```

`scripts/test-statusline.sh` exercises the status line wrapper and installer with a throwaway `HOME` and a fake dashboard. It never touches the real `~/.claude`.

## Demo fixtures

`tools/make-demo.py` generates the three synthetic payloads used by demo mode:

```sh
python3 tools/make-demo.py
```

It writes `ui/assets/demo/state.json`, `thread.json` and `timeline.json`. The script is seeded, so output is byte-for-byte identical on every run. Keys and nesting mirror the live API payloads, but every project, thread, id and number is invented. The files are compiled into the binary through the `ASSETS` table in `src/main.rs`, so rebuild after changing them unless you are serving the UI from disk.

## Code layout

| Area | Files |
|---|---|
| Worker threads and configuration | `src/main.rs` |
| T3 database reader | `src/t3.rs` |
| Transcript tailing and parsing | `src/tail.rs`, `src/transcript.rs`, `src/codex.rs` |
| Aggregation and history | `src/agg.rs`, `src/backfill.rs`, `src/history.rs`, `src/projects.rs` |
| Page payloads | `src/payload.rs`, `src/feed.rs`, `src/drill.rs`, `src/timeline.rs` |
| HTTP, SSE, state cache | `src/http.rs`, `src/hub.rs`, `src/state.rs`, `src/cclimits.rs` |
| Page and assets | `ui/index.html`, `ui/assets/`, `ui/assets/demo/` |
