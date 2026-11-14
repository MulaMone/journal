#!/usr/bin/env python3
"""Writes data/dxy_monthly.json and data/cl_monthly.json (monthly OHLC, full history).
DXY: Yahoo DX-Y.NYB (back to 1971).  Oil: FRED DCOILWTICO daily spot (1986+) rolled to months,
then Yahoo CL=F (2000+) wins for every month it covers."""
import datetime, json, os, sys, time, urllib.request

UA = {"User-Agent": "Mozilla/5.0"}
OUT = os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "data")


def get(url):
    return urllib.request.urlopen(urllib.request.Request(url, headers=UA), timeout=60).read().decode()


def yahoo_monthly(sym):
    j = json.loads(get("https://query1.finance.yahoo.com/v8/finance/chart/%s?interval=1mo&range=max" % sym))
    r = j["chart"]["result"][0]
    q = r["indicators"]["quote"][0]
    rows = {}
    for i, t in enumerate(r["timestamp"]):
        v = [q[k][i] for k in ("open", "high", "low", "close")]
        if any(x is None for x in v):
            continue
        # +12h keeps a 00:00Z stamp on the 1st inside its own month
        d = datetime.datetime.utcfromtimestamp(t + 43200).strftime("%Y-%m-01")
        rows[d] = {"d": d, "o": v[0], "h": v[1], "l": v[2], "c": v[3]}
    return rows


def fred_monthly(series):
    # fredgraph.csv sometimes stalls or refuses cloud runners: try a few URL forms, 3 attempts each
    urls = [
        "https://fred.stlouisfed.org/graph/fredgraph.csv?id=%s&cosd=1986-01-01" % series,
        "https://fred.stlouisfed.org/graph/fredgraph.csv?id=%s" % series,
        "https://fred.stlouisfed.org/data/%s.txt" % series,
    ]
    text, err = None, None
    for u in urls:
        for _ in range(3):
            try:
                text = get(u)
                break
            except Exception as e:
                err = e
                time.sleep(3)
        if text:
            break
    if not text:
        raise RuntimeError("all FRED URLs failed: %s" % err)
    rows = {}
    for line in text.splitlines():
        rec = line.replace(",", " ").split()
        try:
            day, val = rec[0], float(rec[1])
            datetime.date.fromisoformat(day)
        except (ValueError, IndexError):
            continue  # header / notes / '.' (no print)
        d = day[:7] + "-01"
        m = rows.get(d)
        if m is None:
            rows[d] = {"d": d, "o": val, "h": val, "l": val, "c": val}
        else:
            m["h"] = max(m["h"], val)
            m["l"] = min(m["l"], val)
            m["c"] = val
    if not rows:
        raise RuntimeError("FRED returned no rows")
    return rows


def write(name, rows):
    os.makedirs(OUT, exist_ok=True)
    out = sorted(rows.values(), key=lambda x: x["d"])
    with open(os.path.join(OUT, name), "w") as f:
        json.dump({"updated": datetime.datetime.utcnow().isoformat() + "Z", "rows": out}, f, separators=(",", ":"))
    print(name, len(out), "rows", out[0]["d"], "->", out[-1]["d"])


write("dxy_monthly.json", yahoo_monthly("DX-Y.NYB"))
cl = {}
fred_err = None
try:
    cl.update(fred_monthly("DCOILWTICO"))
except Exception as e:
    fred_err = e
cl.update(yahoo_monthly("CL%3DF"))
write("cl_monthly.json", cl)
if fred_err:  # still commit what we have, but turn the run red so it is noticed
    sys.exit("FRED failed, oil history starts at 2000: %s" % fred_err)
