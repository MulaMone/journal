#!/usr/bin/env python3
"""Fetch the U.S. Treasury daily par yield curve CSV and write a compact
yields.json for the trading journal's Analyst tab to read same-origin.

Runs server-side (in a GitHub Action), so there's no CORS to work around —
this is the whole reason to do it this way instead of a client-side proxy.

Pulls a rolling multi-year window (YEARS_BACK) every run rather than just
the current year, so the chart has real history (like FRED's 5Y view)
instead of resetting every January. Treasury's per-year CSVs are small —
a few hundred KB each at most — so re-fetching the whole window each run
is trivial for a GitHub Action and self-heals if any single day was ever
parsed wrong, no need to read back the previous output and merge.

Stdlib only (urllib/csv/json) — no pip install needed in the workflow.
"""
import csv
import io
import json
import sys
import urllib.error
import urllib.request
from datetime import datetime, timezone

# How many years of history to keep, including the current year.
YEARS_BACK = 5

# ── Adjust this to wherever index.html actually lives in the repo ──
# e.g. "journal/yields.json" if index.html is under a journal/ folder.
OUTPUT_PATH = "yields.json"

# Treasury CSV column name -> compact key used in yields.json
FIELD_MAP = {"3 Mo": "m3", "2 Yr": "y2", "5 Yr": "y5", "10 Yr": "y10", "30 Yr": "y30"}


def year_url(year):
    return (
        "https://home.treasury.gov/resource-center/data-chart-center/interest-rates/"
        f"daily-treasury-rates.csv/{year}/all?type=daily_treasury_yield_curve"
        f"&field_tdr_date_value={year}&page&_format=csv"
    )


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
    return rows


def main():
    current_year = datetime.now().year
    years = range(current_year - YEARS_BACK + 1, current_year + 1)
    by_date = {}
    years_ok = 0
    for year in years:
        try:
            text = fetch_csv(year_url(year))
        except (urllib.error.URLError, TimeoutError) as e:
            # One bad year (network hiccup, or a very old year Treasury
            # doesn't serve this way) shouldn't block updating the rest —
            # just skip it and keep whatever years did come through.
            print(f"Skipping {year}: {e}", file=sys.stderr)
            continue
        rows = parse(text)
        if not rows:
            print(f"Skipping {year}: no rows parsed", file=sys.stderr)
            continue
        years_ok += 1
        for r in rows:
            by_date[r["d"]] = r  # dedupe by date; later years win on overlap

    if not by_date:
        # Don't overwrite a good file with an empty one if every fetch
        # failed or Treasury's page format changed.
        print("No rows parsed for any year — aborting without touching " + OUTPUT_PATH, file=sys.stderr)
        sys.exit(1)

    rows = sorted(by_date.values(), key=lambda r: r["d"])
    payload = {"fetchedAt": datetime.now(timezone.utc).isoformat(), "rows": rows}
    with open(OUTPUT_PATH, "w") as f:
        json.dump(payload, f, separators=(",", ":"))
    print(f"Wrote {len(rows)} rows across {years_ok}/{len(years)} years to {OUTPUT_PATH}")


if __name__ == "__main__":
    main()
