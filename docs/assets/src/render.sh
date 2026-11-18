#!/bin/sh
set -eu

assets=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
ui="$assets/../../ui/assets"

# Fonts and logo marks come from the page's own assets at render time, so the
# repository keeps one copy of each.
for f in fonts/archivo.woff2 fonts/caveat.woff2 logo-mark.png logo-mark-dark.png; do
  cp "$ui/$f" "$assets/src/"
done
trap 'rm -f "$assets/src/archivo.woff2" "$assets/src/caveat.woff2" "$assets/src/logo-mark.png" "$assets/src/logo-mark-dark.png"' EXIT

render() {
  docker run --rm -v "$assets:/a" sb-shot \
    --force-device-scale-factor=1 --virtual-time-budget=2000 \
    --window-size="$1" --screenshot="/a/$2.png" "file:///a/src/$3.html"
}

render 1600,480 banner banner
render 1600,480 banner-dark banner-dark
render 1280,640 social-preview social
