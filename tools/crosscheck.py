"""Independent count of one local day's Claude Code tokens, for checking
wamt's numbers. Shares no code with the Rust reader.

Usage (inside a container with the transcripts mounted read-only):
    python3 -I crosscheck.py <projects_root> <utc_offset_minutes> <YYYY-MM-DD>

Counts assistant responses with usage, once per message id, whose timestamp
falls on the given local day. Prints JSON: model -> [input, output,
cache_read, cache_write, requests]. Reads metadata fields only.
"""

import json
import os
import sys
from datetime import datetime, timedelta, timezone


def main() -> None:
    root, off, day = sys.argv[1], int(sys.argv[2]), sys.argv[3]
    tz = timezone(timedelta(minutes=off))
    start = datetime.fromisoformat(day).replace(tzinfo=tz)
    end = start + timedelta(days=1)
    start_epoch = start.timestamp()
    seen = set()
    out: dict[str, list[int]] = {}
    for dirpath, _dirs, files in os.walk(root):
        for name in files:
            if not name.endswith(".jsonl"):
                continue
            path = os.path.join(dirpath, name)
            try:
                if os.path.getmtime(path) < start_epoch:
                    continue
                fh = open(path, "rb")
            except OSError:
                continue
            with fh:
                for raw in fh:
                    if b'"assistant"' not in raw or b'"usage"' not in raw:
                        continue
                    try:
                        o = json.loads(raw)
                    except ValueError:
                        continue
                    if o.get("type") != "assistant":
                        continue
                    msg = o.get("message") or {}
                    if not isinstance(msg, dict):
                        continue
                    usage = msg.get("usage") or {}
                    model = msg.get("model") or ""
                    if not usage or model in ("", "<synthetic>"):
                        continue
                    ts = o.get("timestamp")
                    if not ts:
                        continue
                    t = datetime.fromisoformat(ts.replace("Z", "+00:00"))
                    if not (start <= t < end):
                        continue
                    mid = msg.get("id")
                    if mid:
                        if mid in seen:
                            continue
                        seen.add(mid)
                    i = usage.get("input_tokens") or 0
                    o_ = usage.get("output_tokens") or 0
                    cr = usage.get("cache_read_input_tokens") or 0
                    cw = usage.get("cache_creation_input_tokens") or 0
                    if i + o_ + cr + cw == 0:
                        continue
                    row = out.setdefault(model, [0, 0, 0, 0, 0])
                    row[0] += i
                    row[1] += o_
                    row[2] += cr
                    row[3] += cw
                    row[4] += 1
    print(json.dumps(out, sort_keys=True))


if __name__ == "__main__":
    main()
