#!/usr/bin/env python3
"""Write data/tnx_monthly.json: monthly ^TNX OHLC, full history from Yahoo."""
import json, math, os, time, urllib.request
from datetime import datetime, timezone

BASE = "https://query1.finance.yahoo.com/v8/finance/chart/%5ETNX"
OUT = os.path.join(os.path.dirname(__file__), "..", "data", "tnx_monthly.json")
NOW = int(time.time())


def fetch(interval):
    # explicit period1/period2: range=max can silently truncate the history
    url = f"{BASE}?interval={interval}&period1=-631152000&period2={NOW}"
    req = urllib.request.Request(url, headers={"User-Agent": "Mozilla/5.0"})
    res = json.load(urllib.request.urlopen(req, timeout=60))["chart"]["result"][0]
    q = res["indicators"]["quote"][0]
    out = []
    for i, t in enumerate(res["timestamp"]):
        o, h, l, c = (q[k][i] for k in ("open", "high", "low", "close"))
        if all(isinstance(x, (int, float)) and math.isfinite(x) for x in (o, h, l, c)):
            out.append((datetime.fromtimestamp(t, timezone.utc).strftime("%Y-%m-%d"), o, h, l, c))
    return out


def monthly(raw):
    rows, cur = [], None
    for d, o, h, l, c in raw:
        m = d[:7] + "-01"
        if cur and cur["d"] == m:
            cur["h"] = max(cur["h"], h); cur["l"] = min(cur["l"], l); cur["c"] = c
        else:
            cur = {"d": m, "o": o, "h": h, "l": l, "c": c}
            rows.append(cur)
    return rows


rows = monthly(fetch("1mo"))
print(f"1mo: {len(rows)} rows from {rows[0]['d'] if rows else '-'}")
if len(rows) < 500:  # truncated -> build months from daily bars instead
    rows = monthly(fetch("1d"))
    print(f"1d rolled up: {len(rows)} rows from {rows[0]['d'] if rows else '-'}")
for r in rows:
    for k in ("o", "h", "l", "c"):
        r[k] = round(r[k], 4)
assert len(rows) > 500, f"only {len(rows)} rows"
os.makedirs(os.path.dirname(OUT), exist_ok=True)
with open(OUT, "w") as f:
    json.dump({"fetchedAt": datetime.now(timezone.utc).isoformat(), "rows": rows}, f, separators=(",", ":"))
print(f"wrote {len(rows)} rows, {rows[0]['d']} -> {rows[-1]['d']}")
