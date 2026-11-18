#!/bin/sh
# Claude Code statusLine wrapper for Who Ate My Tokens.
#
# stdin is the statusLine JSON. We read it once, store only .rate_limits for
# the dashboard, run the owner's original status line with the same stdin,
# then append a short plate summary. Every wamt step is best effort: an error
# here must never cost the owner their status line. No python, no network for
# the percentages.

HERE=$(CDPATH= cd -- "$(dirname -- "$0")" 2>/dev/null && pwd) || exit 0
ROOT=$(CDPATH= cd -- "$HERE/.." 2>/dev/null && pwd) || ROOT=.
STATE=${WAMT_STATE:-$ROOT/.state}
JQ=/opt/homebrew/bin/jq
[ -x "$JQ" ] || JQ=$(command -v jq 2>/dev/null) || JQ=

input=
while IFS= read -r line || [ -n "$line" ]; do input="$input$line"; done

# One jq pass gives the file contents and the two display percentages. Only
# .rate_limits crosses into the file: paths, ids and session data never do.
# jq's own clock supplies the timestamp, so no extra process is needed.
origfile="$STATE/statusline-orig.json"
[ -f "$origfile" ] || origfile=/dev/null
parsed=
if [ -n "$JQ" ]; then
  # One jq pass for the file, the display percentages and the saved command.
  parsed=$(printf '%s' "$input" | "$JQ" -r --rawfile orig "$origfile" '
    (.rate_limits // {}) as $r |
    [ (if ($r | length) > 0 then
        (({ at: (now * 1000 | floor) }
          + (if $r.five_hour then { five_hour: $r.five_hour } else {} end)
          + (if $r.seven_day then { seven_day: $r.seven_day } else {} end)) | tojson)
      else "" end),
      (if $r.five_hour and ($r.five_hour.used_percentage != null)
        then ((($r.five_hour.used_percentage + 0.5) | floor) | tostring) else "" end),
      (if $r.seven_day and ($r.seven_day.used_percentage != null)
        then ((($r.seven_day.used_percentage + 0.5) | floor) | tostring) else "" end),
      (try ($orig | fromjson | .command) catch "" | tostring)
    ] | join("\u001f")
  ' 2>/dev/null) || parsed=
fi

doc=; p5=; p7=; orig=
if [ -n "$parsed" ]; then
  oldifs=$IFS; IFS=$(printf '\037'); set -- $parsed
  doc=${1:-}; p5=${2:-}; p7=${3:-}; orig=${4:-}
  IFS=$oldifs
fi

if [ -n "$doc" ]; then
  tmp="$STATE/cc-limits.json.tmp.$$"
  [ -d "$STATE" ] || mkdir -p "$STATE" 2>/dev/null
  if printf '%s\n' "$doc" >"$tmp" 2>/dev/null; then
    mv -f "$tmp" "$STATE/cc-limits.json" 2>/dev/null || rm -f "$tmp" 2>/dev/null
  else
    rm -f "$tmp" 2>/dev/null
  fi
fi

# The original status line always runs, even when the dashboard is absent.
if [ -z "$JQ" ] && [ -f "$origfile" ]; then
  orig=$(sed -n 's/.*"command"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p' "$origfile" 2>/dev/null) || orig=
fi
out=
if [ -n "$orig" ]; then
  out=$(printf '%s\n' "$input" | eval "$orig" 2>/dev/null) || out=
fi

extra=
add() { if [ -n "$1" ]; then if [ -n "$extra" ]; then extra="$extra | $1"; else extra="$1"; fi; fi; }
[ -n "$p5" ] && add "plate ${p5}%"
[ -n "$p7" ] && add "week ${p7}%"

# The "needs you" count comes from the local dashboard, and is skipped when
# it is down. A fifth of a second is the hard ceiling for that call.
if command -v curl >/dev/null 2>&1; then
  brief=$(curl -s --max-time 0.2 http://127.0.0.1:7317/api/brief 2>/dev/null) || brief=
  # Our brief is compact JSON, so the count needs no jq process here.
  needs=0
  case "$brief" in
    *'"needs":'*)
      n=${brief#*'"needs":'}
      n=${n%%[!0-9]*}
      [ -n "$n" ] && needs=$n
      ;;
  esac
  case "$needs" in
    0|'') ;;
    *) add "$needs needs you" ;;
  esac
fi

if [ -n "$out" ]; then
  printf '%s%s\n' "$out" "${extra:+ | $extra}"
else
  printf '%s\n' "$extra"
fi
