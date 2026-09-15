#!/usr/bin/env python3
"""
Fetches ICE US Dollar Index (DXY) daily history from Stooq (server-side —
no CORS to work around here, unlike a client-side browser fetch) and
writes dxy.json in the same {rows:[...], fetchedAt} shape the Edge
Analyst tab's DXY section expects.

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
import urllib.request
from datetime import datetime, timezone

STOOQ_URL = "https://stooq.com/q/d/l/?s=dx.f&i=d"
OUTPUT_PATH = "dxy.json"
YEARS_BACK_KEPT = 3  # trim the JSON file to the last N years of daily closes


def fetch_dxy_rows():
    req = urllib.request.Request(STOOQ_URL, headers={"User-Agent": "Mozilla/5.0"})
    with urllib.request.urlopen(req, timeout=30) as resp:
        raw = resp.read().decode("utf-8")

    if raw.strip().lower().startswith("<!doctype") or "<html" in raw.lower():
        raise RuntimeError("Stooq returned HTML instead of CSV — endpoint may have changed")

    reader = csv.DictReader(io.StringIO(raw))
    rows = []
    for r in reader:
        date_key = r.get("Date") or r.get("date")
        close_key = r.get("Close") or r.get("close")
        if not date_key or not close_key:
            continue
        try:
            close = float(close_key)
        except ValueError:
            continue
        rows.append({"d": date_key, "c": close})

    if not rows:
        raise RuntimeError("parsed 0 rows from Stooq CSV — check STOOQ_URL / response format")

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
    }
    with open(OUTPUT_PATH, "w") as f:
        json.dump(payload, f, separators=(",", ":"))

    print(f"Wrote {len(rows)} rows to {OUTPUT_PATH} (latest: {rows[-1]['d']} = {rows[-1]['c']})")


if __name__ == "__main__":
    main()
