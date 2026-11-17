#!/bin/sh
# Screenshot the test container's page: dshot.sh <name> <theme> <width> <height>
set -eu
cd "$(dirname "$0")/.."
mkdir -p shots
docker run --rm --network sb-net --memory 1g -v "$PWD/shots":/out sb-shot \
  --window-size="${3:-1600},${4:-3000}" --screenshot="/out/$1.png" --virtual-time-budget=8000 \
  "http://sb-test:7317/?snapshot&theme=${2:-dark}${EXTRA:-}${THREAD:+&thread=$THREAD}" >/dev/null 2>&1
echo "shots/$1.png"
