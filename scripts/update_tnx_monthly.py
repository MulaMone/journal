#!/usr/bin/env python3
"""Write data/tnx_monthly.json (Yahoo ^TNX monthly OHLC) and data/tnx_yearly.json
(yearly OHLC: Yahoo where it has full years, FRED DGS10 daily closes for the years before)."""
import csv, io, json, math, os, time, urllib.request
from datetime import datetime, timezone

BASE = "https://query1.finance.yahoo.com/v8/finance/chart/%5ETNX"
FRED = "https://fred.stlouisfed.org/graph/fredgraph.csv?id=DGS10"
DATA = os.path.join(os.path.dirname(__file__), "..", "data")
NOW = int(time.time())
UA = {"User-Agent": "Mozilla/5.0"}


def fetch_yahoo(interval):
    # explicit period1/period2: range=max can silently truncate the history
    url = f"{BASE}?interval={interval}&period1=-631152000&period2={NOW}"
    res = json.load(urllib.request.urlopen(urllib.request.Request(url, headers=UA), timeout=60))["chart"]["result"][0]
    q = res["indicators"]["quote"][0]
    out = []
    for i, t in enumerate(res["timestamp"]):
        o, h, l, c = (q[k][i] for k in ("open", "high", "low", "close"))
        if all(isinstance(x, (int, float)) and math.isfinite(x) for x in (o, h, l, c)):
            out.append((datetime.fromtimestamp(t, timezone.utc).strftime("%Y-%m-%d"), o, h, l, c))
    return out


def roll(raw, n):
    """Roll (date,o,h,l,c) tuples up by the first n chars of the date (7 = month, 4 = year)."""
    rows, cur = [], None
    for d, o, h, l, c in raw:
        k = d[:n]
        if cur and cur["k"] == k:
            cur["h"] = max(cur["h"], h); cur["l"] = min(cur["l"], l); cur["c"] = c
        else:
            cur = {"k": k, "d": (d[:7] + "-01") if n == 7 else (d[:4] + "-01-01"), "o": o, "h": h, "l": l, "c": c}
            rows.append(cur)
    return rows


def clean(rows):
    return [{"d": r["d"], **{k: round(r[k], 4) for k in "ohlc"}} for r in rows]


def write(name, rows):
    os.makedirs(DATA, exist_ok=True)
    with open(os.path.join(DATA, name), "w") as f:
        json.dump({"fetchedAt": datetime.now(timezone.utc).isoformat(), "rows": rows}, f, separators=(",", ":"))
    print(f"wrote {name}: {len(rows)} rows, {rows[0]['d']} -> {rows[-1]['d']}")


# ---- monthly (Yahoo only) ----
raw = fetch_yahoo("1mo")
monthly = roll(raw, 7)
print(f"yahoo 1mo: {len(monthly)} rows from {monthly[0]['d'] if monthly else '-'}")
if len(monthly) < 300:  # truncated -> build months from daily bars instead
    raw = fetch_yahoo("1d")
    monthly = roll(raw, 7)
    print(f"yahoo 1d rolled up: {len(monthly)} rows from {monthly[0]['d'] if monthly else '-'}")
assert len(monthly) > 300, f"only {len(monthly)} monthly rows"
write("tnx_monthly.json", clean(monthly))

# ---- yearly: FRED closes for years before Yahoo's first full year ----
yahoo_years = roll(raw, 4)
# first year Yahoo has from January on (earlier years are partial)
first_full = next(int(y["d"][:4]) for y in yahoo_years
                  if any(m["d"] == f"{y['d'][:4]}-01-01" for m in monthly))
yearly = [y for y in yahoo_years if int(y["d"][:4]) >= first_full]
MIRROR = "https://raw.githubusercontent.com/datasets/bond-yields-us-10y/main/data/monthly.csv"


def older_rows(url, timeout):
    """(date,v,v,v,v) rows from a 2-column CSV (date, value) for years before Yahoo starts."""
    txt = urllib.request.urlopen(urllib.request.Request(url, headers=UA), timeout=timeout).read().decode()
    out = []
    for row in list(csv.reader(io.StringIO(txt)))[1:]:
        try:
            v = float(row[1])
        except (ValueError, IndexError):
            continue  # "." = no print that day
        if int(row[0][:4]) < first_full:
            out.append((row[0], v, v, v, v))
    return out


older = []
for label, url, timeout in (("FRED DGS10 daily closes", FRED, 20),
                            ("GitHub datasets mirror of FRED GS10 (monthly averages)", MIRROR, 30)):
    try:
        older = roll(older_rows(url, timeout), 4)
        print(f"{label}: {len(older)} years before {first_full}")
        break
    except Exception as e:
        print(f"WARNING: {label} failed ({e})")
if older:
    yearly = older + yearly
else:
    print("WARNING: no pre-Yahoo source worked; yearly file is Yahoo-only")
write("tnx_yearly.json", clean(yearly))
