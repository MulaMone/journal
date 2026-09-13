#!/usr/bin/env python3
"""Fetch the U.S. Treasury daily par yield curve CSV and write a compact
yields.json for the trading journal's Analyst tab to read same-origin.

Runs server-side (in a GitHub Action), so there's no CORS to work around —
this is the whole reason to do it this way instead of a client-side proxy.

Stdlib only (urllib/csv/json) — no pip install needed in the workflow.
"""
import csv
import io
import json
import sys
import urllib.request
from datetime import datetime, timezone

YEAR = datetime.now().year
URL = (
    "https://home.treasury.gov/resource-center/data-chart-center/interest-rates/"
    f"daily-treasury-rates.csv/{YEAR}/all?type=daily_treasury_yield_curve"
    f"&field_tdr_date_value={YEAR}&page&_format=csv"
)

# ── Adjust this to wherever index.html actually lives in the repo ──
# e.g. "journal/yields.json" if index.html is under a journal/ folder.
OUTPUT_PATH = "yields.json"

# Treasury CSV column name -> compact key used in yields.json
FIELD_MAP = {"3 Mo": "m3", "2 Yr": "y2", "5 Yr": "y5", "10 Yr": "y10", "30 Yr": "y30"}


def fetch_csv(url):
    req = urllib.request.Request(url, headers={"User-Agent": "Mozilla/5.0"})
    with urllib.request.urlopen(req, timeout=30) as resp:
        return resp.read().decode("utf-8")


def parse(text):
    reader = csv.DictReader(io.StringIO(text))
    rows = []
    for row in reader:
        date_str = row.get("Date", "")
        try:
            d = datetime.strptime(date_str, "%m/%d/%Y").date()
        except ValueError:
            continue
        out = {"d": d.isoformat()}
        ok = True
        for src, dst in FIELD_MAP.items():
            val = (row.get(src) or "").strip()
            if not val:
                ok = False
                break
            try:
                out[dst] = float(val)
            except ValueError:
                ok = False
                break
        if ok:
            rows.append(out)
    rows.sort(key=lambda r: r["d"])
    return rows


def main():
    text = fetch_csv(URL)
    rows = parse(text)
    if not rows:
        # Don't overwrite a good file with an empty one if Treasury's page
        # format ever changes or a request fails oddly.
        print("No rows parsed — aborting without touching " + OUTPUT_PATH, file=sys.stderr)
        sys.exit(1)
    payload = {"fetchedAt": datetime.now(timezone.utc).isoformat(), "rows": rows}
    with open(OUTPUT_PATH, "w") as f:
        json.dump(payload, f, separators=(",", ":"))
    print(f"Wrote {len(rows)} rows to {OUTPUT_PATH}")


if __name__ == "__main__":
    main()
