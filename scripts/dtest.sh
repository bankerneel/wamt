#!/bin/sh
# Run a command (default: cargo test) inside the Rust dev container.
# The project is mounted read-write at /src; cargo caches live in named volumes.
set -eu
cd "$(dirname "$0")/.."
[ $# -eq 0 ] && set -- cargo test
exec docker run --rm \
  --memory 1536m --cpus 4 \
  -v "$PWD":/src -w /src \
  -v sb-cargo-registry:/usr/local/cargo/registry \
  -v "${SB_TARGET:-sb-target}":/target -e CARGO_TARGET_DIR=/target \
  -e CARGO_BUILD_JOBS=4 \
  rust:1-slim-bookworm "$@"
