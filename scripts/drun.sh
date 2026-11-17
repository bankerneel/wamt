#!/bin/sh
# Run the Linux build inside a container against the real data, read-only.
# Private T3 folders are hidden behind empty mounts. Page: http://127.0.0.1:7318
set -eu
cd "$(dirname "$0")/.."
mkdir -p .state-docker
docker rm -f sb-test >/dev/null 2>&1 || true
docker network inspect sb-net >/dev/null 2>&1 || docker network create sb-net >/dev/null
exec docker run -d --name sb-test --network sb-net --memory 256m --cpus 1 \
  -p 127.0.0.1:7318:7317 \
  -v "$HOME/.t3/userdata":/data/t3:ro \
  --mount type=tmpfs,destination=/data/t3/secrets,tmpfs-size=1k \
  --mount type=tmpfs,destination=/data/t3/clerk,tmpfs-size=1k \
  --mount type=tmpfs,destination=/data/t3/providers,tmpfs-size=1k \
  --mount type=tmpfs,destination=/data/t3/device,tmpfs-size=1k \
  -v /dev/null:/data/t3/shell-environment-cache.json:ro \
  -v "$HOME/.claude/projects":/data/claude:ro \
  -v sb-target:/target:ro \
  -v "$PWD/.state-docker":/state \
  -v "$PWD/ui":/ui:ro \
  -e WAMT_BIND=0.0.0.0:7317 \
  -e WAMT_T3_DB=/data/t3/statev2.sqlite \
  -e WAMT_CLAUDE_ROOT=/data/claude \
  -e WAMT_STATE=/state \
  -e WAMT_UTC_OFFSET_MIN=330 \
  -e WAMT_UI=/ui/index.html \
  -e WAMT_ALLOW_HOSTS=localhost:7318,127.0.0.1:7318,sb-test:7317 \
  rust:1-slim-bookworm /target/release/wamt
