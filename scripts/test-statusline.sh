#!/bin/sh
# Shell tests for scripts/statusline.sh and scripts/wamt statusline on|off.
# Everything happens in a throwaway HOME: the real ~/.claude is never read
# or written, and its contents are never printed.
set -eu
ROOT=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
TMP=$(mktemp -d)
trap 'rm -rf "$TMP"' EXIT INT TERM
H="$TMP/home"; STATE="$TMP/state"; BIN="$TMP/bin"
mkdir -p "$H/.claude" "$STATE" "$BIN"
fail() { echo "FAIL: $*" >&2; exit 1; }
ms() { python3 -c 'import time; print(int(time.time() * 1000))'; }

cat >"$H/.claude/settings.json" <<'JSON'
{
  "theme": "dark",
  "statusLine": {
    "type": "command",
    "command": "printf ORIG",
    "padding": 0
  },
  "apiKeyHelper": "secret-value-never-printed"
}
JSON
cat >"$STATE/statusline-orig.json" <<'JSON'
{"type":"command","command":"printf ORIG","padding":0}
JSON

RL='{"session_id":"topsecret","cwd":"/private/secret","version":"2.1.80","rate_limits":{"five_hour":{"used_percentage":23.5,"resets_at":1990000000},"seven_day":{"used_percentage":41.2,"resets_at":1990100000}}}'
PLAIN='{"session_id":"topsecret","cwd":"/private/secret","version":"2.1.80"}'

# 1. With rate_limits: original output plus the plate and week from stdin.
printf '%s' "$RL" | WAMT_STATE="$STATE" sh "$ROOT/scripts/statusline.sh" >"$TMP/with"
grep -q '^ORIG | plate 24% | week 41%$' "$TMP/with" || fail "with rate_limits: $(cat "$TMP/with")"
grep -q 'five_hour' "$STATE/cc-limits.json" || fail "limits not stored"
grep -q 'seven_day' "$STATE/cc-limits.json" || fail "week not stored"
grep -q '"at"' "$STATE/cc-limits.json" || fail "timestamp missing"
if grep -Eq 'session_id|topsecret|/private|version' "$STATE/cc-limits.json"; then fail "stored more than .rate_limits"; fi
if ls "$STATE"/cc-limits.json.tmp.* >/dev/null 2>&1; then fail "temporary file left behind"; fi

# 2. Without rate_limits: original output only.
printf '%s' "$PLAIN" | WAMT_STATE="$STATE" sh "$ROOT/scripts/statusline.sh" >"$TMP/without"
grep -qx 'ORIG' "$TMP/without" || fail "without rate_limits: $(cat "$TMP/without")"

# 2b. Only the weekly window: no plate segment, no empty-field mix-up.
ONLYW='{"rate_limits":{"seven_day":{"used_percentage":41.2,"resets_at":1990100000}}}'
printf '%s' "$ONLYW" | WAMT_STATE="$STATE" sh "$ROOT/scripts/statusline.sh" >"$TMP/weekonly"
grep -qx 'ORIG | week 41%' "$TMP/weekonly" || fail "week only: $(cat "$TMP/weekonly")"

# 3. Dashboard down: no "needs you" text.
t0=$(ms)
printf '%s' "$RL" | WAMT_STATE="$STATE" sh "$ROOT/scripts/statusline.sh" >"$TMP/down"
t1=$(ms)
grep -q 'needs you' "$TMP/down" && fail "dashboard down but needs reported"
echo "statusline with dashboard unreachable: $((t1 - t0)) ms"

# 4. Fake brief with needs=1: the third segment is appended.
cat >"$BIN/curl" <<'SH'
#!/bin/sh
printf '%s\n' '{"plate_pct":23.5,"week_pct":41.2,"needs":1,"eating":2,"plate_resets_ms":1990000000000}'
SH
chmod +x "$BIN/curl"
printf '%s' "$RL" | PATH="$BIN:$PATH" WAMT_STATE="$STATE" sh "$ROOT/scripts/statusline.sh" >"$TMP/needs"
grep -q '^ORIG | plate 24% | week 41% | 1 needs you$' "$TMP/needs" || fail "needs: $(cat "$TMP/needs")"

# 5. Installer: backup, original saved, command changed, exact restore.
HOME="$H" WAMT_STATE="$STATE" sh "$ROOT/scripts/wamt" statusline on >"$TMP/on"
grep -q 'scripts/statusline.sh' "$TMP/on" || fail "on did not print the command"
[ -f "$STATE/settings.json.bak" ] || fail "no backup"
cmp -s "$STATE/settings.json.bak" "$H/.claude/settings.json" && fail "backup is not the original"
installed=$(/opt/homebrew/bin/jq -r '.statusLine.command' "$H/.claude/settings.json")
[ "$installed" = "sh $ROOT/scripts/statusline.sh" ] || fail "wrong installed command: $installed"
[ "$(/opt/homebrew/bin/jq -r '.statusLine.type' "$H/.claude/settings.json")" = command ] || fail "type changed"
[ "$(/opt/homebrew/bin/jq -r '.statusLine.padding' "$H/.claude/settings.json")" = 0 ] || fail "extra key dropped"
[ "$(/opt/homebrew/bin/jq -r '.apiKeyHelper' "$H/.claude/settings.json")" = secret-value-never-printed ] || fail "settings changed"
HOME="$H" WAMT_STATE="$STATE" sh "$ROOT/scripts/wamt" statusline status | grep -q 'Active:' || fail "status says inactive"
HOME="$H" WAMT_STATE="$STATE" sh "$ROOT/scripts/wamt" statusline off >"$TMP/off"
[ "$(/opt/homebrew/bin/jq -S -c '.statusLine' "$H/.claude/settings.json")" = '{"command":"printf ORIG","padding":0,"type":"command"}' ] || fail "statusLine not restored exactly"
HOME="$H" WAMT_STATE="$STATE" sh "$ROOT/scripts/wamt" statusline status | grep -q 'Inactive' || fail "off still active"
if grep -Rq 'secret-value-never-printed' "$TMP/on" "$TMP/off"; then fail "installer printed a setting value"; fi

echo "shell tests: 6 checks passed"
