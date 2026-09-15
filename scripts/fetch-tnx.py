#!/usr/bin/env python3
"""
Fetches CBOE's $TNX (10-Year Treasury Yield Index) via Yahoo Finance's
public chart endpoint (server-side — no CORS restriction here, unlike a
client-side fetch from index.html) and writes tnx.json to the repo root.

$TNX is quoted at 10x the actual yield (e.g. 42.50 -> 4.250%), so this
divides by 10 before writing. Meant to run on a schedule via GitHub
Actions (see .github/workflows/update-tnx.yml) — every run overwrites
tnx.json with the latest quote.
"""
import json
import sys
import urllib.request
from datetime import datetime, timezone

YAHOO_URL = "https://query1.finance.yahoo.com/v8/finance/chart/%5ETNX?interval=1m&range=1d"
OUT_PATH = "tnx.json"


def fetch_tnx():
    req = urllib.request.Request(
        YAHOO_URL,
        headers={"User-Agent": "Mozilla/5.0 (compatible; edge-terminal-bot/1.0)"},
    )
    with urllib.request.urlopen(req, timeout=15) as resp:
        data = json.loads(resp.read().decode("utf-8"))

    result = data.get("chart", {}).get("result")
    if not result:
        raise RuntimeError("no 'result' in Yahoo response — %r" % (data.get("chart", {}).get("error"),))

    meta = result[0].get("meta", {})
    price = meta.get("regularMarketPrice")
    if price is None:
        raise RuntimeError("no regularMarketPrice in Yahoo response meta")

    market_time_epoch = meta.get("regularMarketTime")
    market_time_iso = (
        datetime.fromtimestamp(market_time_epoch, tz=timezone.utc).isoformat()
        if market_time_epoch
        else None
    )

    return {
        "yield": round(price / 10.0, 3),
        "rawPrice": price,
        "marketTime": market_time_iso,
        "fetchedAt": datetime.now(timezone.utc).isoformat(),
    }


def main():
    try:
        obj = fetch_tnx()
    except Exception as e:
        print("fetch-tnx.py failed: %s" % e, file=sys.stderr)
        sys.exit(1)

    with open(OUT_PATH, "w") as f:
        json.dump(obj, f)
        f.write("\n")

    print("wrote %s: %s" % (OUT_PATH, obj))


if __name__ == "__main__":
    main()
