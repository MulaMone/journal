#!/usr/bin/env python3
"""Write data/tnx_monthly.json: monthly ^TNX OHLC, full history (Yahoo, interval=1mo, range=max)."""
import json, math, os, urllib.request
from datetime import datetime, timezone

URL = "https://query1.finance.yahoo.com/v8/finance/chart/%5ETNX?interval=1mo&range=max"
OUT = os.path.join(os.path.dirname(__file__), "..", "data", "tnx_monthly.json")

req = urllib.request.Request(URL, headers={"User-Agent": "Mozilla/5.0"})
res = json.load(urllib.request.urlopen(req, timeout=30))["chart"]["result"][0]
q = res["indicators"]["quote"][0]
rows, seen = [], {}
for i, t in enumerate(res["timestamp"]):
    o, h, l, c = (q[k][i] for k in ("open", "high", "low", "close"))
    if not all(isinstance(x, (int, float)) and math.isfinite(x) for x in (o, h, l, c)):
        continue
    d = datetime.fromtimestamp(t, timezone.utc).strftime("%Y-%m-01")
    row = {"d": d, "o": round(o, 4), "h": round(h, 4), "l": round(l, 4), "c": round(c, 4)}
    if d in seen:
        rows[seen[d]] = row
    else:
        seen[d] = len(rows)
        rows.append(row)
assert len(rows) > 500, f"only {len(rows)} rows"
os.makedirs(os.path.dirname(OUT), exist_ok=True)
with open(OUT, "w") as f:
    json.dump({"fetchedAt": datetime.now(timezone.utc).isoformat(), "rows": rows}, f, separators=(",", ":"))
print(f"wrote {len(rows)} rows, {rows[0]['d']} -> {rows[-1]['d']}")
