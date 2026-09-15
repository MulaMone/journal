#!/usr/bin/env python3
"""
Fetches the Fed's Nominal Broad U.S. Dollar Index (FRED series DTWEXBGS)
server-side and writes dxy.json in the same {rows:[...], fetchedAt} shape
the Edge Analyst tab's "Intermarket & Cross-Asset Correlations" section
expects.

NOTE — this is the Fed's own broad trade-weighted dollar index, not the
ICE DXY futures index. First choice was Stooq's DX.F continuous futures
contract (the literal DXY proxy), but Stooq blocks requests from cloud/
datacenter IP ranges — which is exactly what GitHub Actions runners use —
so that endpoint returns an HTML block page instead of CSV when run here.
FRED's plain CSV endpoint has no such bot-blocking and is well-suited to
CI. DTWEXBGS is arguably a MORE complete dollar-strength gauge than DXY
anyway (broader trade-partner basket vs. DXY's EUR-heavy six-currency
basket) — just note it's a different index on a different scale (~120
vs. DXY's ~90-105) if you ever compare the two directly.

Mirrors the existing update_yields.py / update-yields.yml pattern already
used for yields.json in this repo: a scheduled GitHub Action writes a
small static JSON file, and index.html only ever reads it same-origin.

Usage: python3 scripts/update_dxy.py
Writes: dxy.json (repo root)
"""
import csv
import io
import json
import sys
import time
import urllib.request
import urllib.error
from datetime import datetime, timezone

FRED_SERIES_ID = "DTWEXBGS"  # Nominal Broad U.S. Dollar Index
FRED_URL = f"https://fred.stlouisfed.org/graph/fredgraph.csv?id={FRED_SERIES_ID}"
OUTPUT_PATH = "dxy.json"
YEARS_BACK_KEPT = 3  # trim the JSON file to the last N years of daily closes
REQUEST_TIMEOUT = 60
MAX_ATTEMPTS = 3
RETRY_DELAY_SECONDS = 5


def fetch_csv_text():
    req = urllib.request.Request(FRED_URL, headers={"User-Agent": "Mozilla/5.0"})
    last_err = None
    for attempt in range(1, MAX_ATTEMPTS + 1):
        try:
            with urllib.request.urlopen(req, timeout=REQUEST_TIMEOUT) as resp:
                return resp.read().decode("utf-8")
        except (urllib.error.URLError, TimeoutError, OSError) as e:
            last_err = e
            print(f"attempt {attempt}/{MAX_ATTEMPTS} failed: {e}", file=sys.stderr)
            if attempt < MAX_ATTEMPTS:
                time.sleep(RETRY_DELAY_SECONDS)
    raise RuntimeError(f"all {MAX_ATTEMPTS} attempts failed, last error: {last_err}")


def fetch_dxy_rows():
    raw = fetch_csv_text()

    if raw.strip().lower().startswith("<!doctype") or "<html" in raw.lower():
        raise RuntimeError("FRED returned HTML instead of CSV — endpoint may have changed")

    reader = csv.DictReader(io.StringIO(raw))
    rows = []
    for r in reader:
        date_key = r.get("observation_date") or r.get("DATE")
        close_key = r.get(FRED_SERIES_ID)
        if not date_key or not close_key or close_key == ".":
            continue  # "." marks a missing/holiday observation in FRED CSVs
        try:
            close = float(close_key)
        except ValueError:
            continue
        rows.append({"d": date_key, "c": close})

    if not rows:
        raise RuntimeError("parsed 0 rows from FRED CSV — check FRED_SERIES_ID / response format")

    rows.sort(key=lambda r: r["d"])
    cutoff_year = datetime.now(timezone.utc).year - YEARS_BACK_KEPT
    rows = [r for r in rows if int(r["d"][:4]) >= cutoff_year]
    return rows


def main():
    try:
        rows = fetch_dxy_rows()
    except Exception as e:
        print(f"ERROR: {e}", file=sys.stderr)
        sys.exit(1)

    payload = {
        "rows": rows,
        "fetchedAt": datetime.now(timezone.utc).isoformat(),
        "source": f"FRED:{FRED_SERIES_ID}",
    }
    with open(OUTPUT_PATH, "w") as f:
        json.dump(payload, f, separators=(",", ":"))

    print(f"Wrote {len(rows)} rows to {OUTPUT_PATH} (latest: {rows[-1]['d']} = {rows[-1]['c']})")


if __name__ == "__main__":
    main()
