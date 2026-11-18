#!/bin/sh
# SwiftBar/xbar plugin for Who Ate My Tokens. The 30s in the file name is the
# refresh interval. Install by symlinking this file into your plugin folder.
# Reads only the local dashboard's tiny /api/brief payload.

JQ=/opt/homebrew/bin/jq
[ -x "$JQ" ] || JQ=$(command -v jq 2>/dev/null) || JQ=

brief=
if command -v curl >/dev/null 2>&1; then
  brief=$(curl -s --max-time 0.5 http://127.0.0.1:7317/api/brief 2>/dev/null) || brief=
fi

if [ -z "$brief" ] || [ -z "$JQ" ]; then
  printf '%s\n' '🍪 ?'
  printf '%s\n' '---'
  printf '%s\n' 'Dashboard is not running.'
  exit 0
fi

vals=$(printf '%s' "$brief" | "$JQ" -r '[((.plate_pct // 0) + 0.5 | floor), (.week_pct // 0), (.needs // 0), (.eating // 0)] | @tsv' 2>/dev/null) || vals=
if [ -z "$vals" ]; then
  printf '%s\n' '🍪 ?'
  printf '%s\n' '---'
  printf '%s\n' 'Dashboard is not answering.'
  exit 0
fi

plate=$(printf '%s' "$vals" | cut -f1)
week=$(printf '%s' "$vals" | cut -f2)
needs=$(printf '%s' "$vals" | cut -f3)
eating=$(printf '%s' "$vals" | cut -f4)

printf '🍪 %s%%\n' "${plate:-?}"
printf '%s\n' '---'
printf 'Week: %s%%\n' "${week:-?}"
printf 'Needs you: %s\n' "${needs:-?}"
printf 'Eating: %s\n' "${eating:-?}"
printf '%s\n' 'Open dashboard | href=http://127.0.0.1:7317'
