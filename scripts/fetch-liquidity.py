#!/usr/bin/env python3
"""
Fetches three FRED series that together make up the "Liquidity & Fed
Balance Sheet" section:

  WALCL    Fed Total Assets (H.4.1), weekly Wednesday level, in MILLIONS $
  WTREGEN  Treasury General Account (H.4.1), weekly Wednesday level, BILLIONS $
  WLRRAL   Reverse Repo liability (H.4.1), weekly Wednesday level, BILLIONS $

All three come from the same weekly H.4.1 release, so they land on the
same Wednesday dates — this merges them into one row per date, converting
each to a plain dollar figure (not millions/billions) so the front end
can format them uniformly.

Needs a FRED API key (free, https://fred.stlouisfed.org/docs/api/api_key.html)
passed via the FRED_API_KEY environment variable — set as a GitHub Actions
repo secret, never exposed client-side. Writes liquidity.json to the repo
root, same same-origin-static-file pattern as yields.json/tnx.json.
"""
import json
import os
import sys
import urllib.request
import urllib.parse
from datetime import datetime, timezone

FRED_BASE = "https://api.stlouisfed.org/fred/series/observations"
OUT_PATH = "liquidity.json"

# (series_id, output field name, multiplier to convert to plain dollars)
SERIES = [
    ("WALCL", "fedAssets", 1_000_000),   # FRED units: millions of $
    ("WTREGEN", "tga", 1_000_000_000),   # FRED units: billions of $
    ("WLRRAL", "rrp", 1_000_000_000),    # FRED units: billions of $
]


def fetch_series(series_id, api_key):
    params = {
        "series_id": series_id,
        "api_key": api_key,
        "file_type": "json",
        "sort_order": "asc",
        "observation_start": "2015-01-01",
    }
    url = FRED_BASE + "?" + urllib.parse.urlencode(params)
    req = urllib.request.Request(url, headers={"User-Agent": "edge-terminal-bot/1.0"})
    with urllib.request.urlopen(req, timeout=20) as resp:
        data = json.loads(resp.read().decode("utf-8"))
    obs = data.get("observations")
    if obs is None:
        raise RuntimeError("no 'observations' in FRED response for %s — %r" % (series_id, data))
    out = {}
    for o in obs:
        val = o.get("value")
        if val in (None, ".", ""):
            continue
        try:
            out[o["date"]] = float(val)
        except ValueError:
            continue
    if not out:
        raise RuntimeError("FRED returned zero usable observations for %s" % series_id)
    return out


def main():
    api_key = os.environ.get("FRED_API_KEY")
    if not api_key:
        print("fetch-liquidity.py failed: FRED_API_KEY env var not set", file=sys.stderr)
        sys.exit(1)

    try:
        by_series = {}
        for series_id, field, mult in SERIES:
            raw = fetch_series(series_id, api_key)
            by_series[field] = {d: v * mult for d, v in raw.items()}
    except Exception as e:
        print("fetch-liquidity.py failed: %s" % e, file=sys.stderr)
        sys.exit(1)

    all_dates = set()
    for field_map in by_series.values():
        all_dates.update(field_map.keys())

    rows = []
    for d in sorted(all_dates):
        row = {"d": d}
        ok = True
        for _, field, _ in SERIES:
            v = by_series[field].get(d)
            if v is None:
                ok = False
                break
            row[field] = v
        if ok:
            rows.append(row)

    if not rows:
        print("fetch-liquidity.py failed: no dates with all three series present", file=sys.stderr)
        sys.exit(1)

    obj = {"rows": rows, "fetchedAt": datetime.now(timezone.utc).isoformat()}

    with open(OUT_PATH, "w") as f:
        json.dump(obj, f)
        f.write("\n")

    print("wrote %s: %d rows, latest %s" % (OUT_PATH, len(rows), rows[-1]["d"]))


if __name__ == "__main__":
    main()
