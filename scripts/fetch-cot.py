#!/usr/bin/env python3
"""Fetch the newest CFTC COT reports (ES + NQ) straight from cftc.gov's static
report pages and write cot.json at the repo root.

Why: the CFTC's Socrata API (publicreporting.cftc.gov) can lag the static
pages by hours after the Friday 3:30pm ET release. Index.html reads cot.json
only when the API is behind.

Sources (both updated at the release):
  Legacy futures-only, CME short form : /dea/futures/deacmesf.htm
  TFF futures-only, short form        : /dea/futures/financial_lf.htm
Stdlib only. Exits non-zero if a page layout changes so the workflow goes red.
"""
import html
import json
import os
import re
import sys
import time
import urllib.request
from datetime import datetime, timezone

LEGACY_URL = "https://www.cftc.gov/dea/futures/deacmesf.htm"
TFF_URL = "https://www.cftc.gov/dea/futures/financial_lf.htm"
CODES = {"ES": "13874A", "NQ": "209742"}  # E-MINI S&P 500 / NASDAQ MINI (CME)
OUT = os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "cot.json")
NUM = re.compile(r"-?\d[\d,]*")


def fetch(url):
    req = urllib.request.Request(
        url + "?t=" + str(int(time.time())),
        headers={
            "User-Agent": "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/124.0 Safari/537.36",
            "Cache-Control": "no-cache",
            "Pragma": "no-cache",
        },
    )
    with urllib.request.urlopen(req, timeout=60) as r:
        raw = r.read().decode("utf-8", "replace")
    return html.unescape(re.sub(r"<[^>]+>", "", raw))


def ints(line, expect):
    vals = [int(x.replace(",", "")) for x in NUM.findall(line)]
    if len(vals) != expect:
        raise ValueError("expected %d numbers, got %d in %r" % (expect, len(vals), line))
    return vals


def num(s):
    s = s.strip()
    return int(s.replace(",", "")) if NUM.fullmatch(s) else 0


def next_line_after(body, marker):
    lines = body.splitlines()
    for i, ln in enumerate(lines):
        if marker in ln:
            for nxt in lines[i + 1:]:
                if nxt.strip():
                    return nxt
    raise ValueError("marker %r not found" % marker)


def parse_legacy(text, code):
    heads = list(re.finditer(r"^(\S.*?)\s+Code-([0-9A-Z+]+)\s*$", text, re.M))
    for i, h in enumerate(heads):
        if h.group(2) != code:
            continue
        end = heads[i + 1].start() if i + 1 < len(heads) else len(text)
        body = text[h.end():end]
        d = re.search(r"AS OF (\d\d)/(\d\d)/(\d\d)", body)
        oi = re.search(r"OPEN INTEREST:\s+([\d,]+)", body)
        chg = re.search(r"CHANGE IN OPEN INTEREST:\s+(-?[\d,]+)", body)
        if not (d and oi and chg):
            raise ValueError("legacy block %s missing date/OI" % code)
        c = ints(next_line_after(body, "COMMITMENTS"), 9)
        return {
            "report_date_as_yyyy_mm_dd": "20%s-%s-%s" % (d.group(3), d.group(1), d.group(2)),
            "open_interest_all": num(oi.group(1)),
            "change_in_open_interest_all": num(chg.group(1)),
            "noncomm_positions_long_all": c[0],
            "noncomm_positions_short_all": c[1],
            "comm_positions_long_all": c[3],
            "comm_positions_short_all": c[4],
            "nonrept_positions_long_all": c[7],
            "nonrept_positions_short_all": c[8],
        }
    raise ValueError("legacy code %s not found" % code)


def parse_tff(text, code):
    chunks = re.split(r"(?=^Traders in Financial Futures - Futures Only Positions as of )", text, flags=re.M)
    for ch in chunks:
        m = re.search(r"CFTC Code #(\S+)", ch)
        if not m or m.group(1) != code:
            continue
        d = re.search(r"Positions as of ([A-Za-z]+) (\d{1,2}), (\d{4})", ch)
        oi = re.search(r"Open Interest is\s*([\d,]+)", ch)
        chg = re.search(r"Total Change is:\s+(-?[\d,]+|\.)", ch)
        if not (d and oi):
            raise ValueError("tff block %s missing date/OI" % code)
        dt = datetime.strptime("%s %s %s" % (d.group(1), d.group(2), d.group(3)), "%B %d %Y")
        lines = ch.splitlines()
        idx = next(i for i, ln in enumerate(lines) if ln.strip() == "Positions")
        p = ints(next(l for l in lines[idx + 1:] if l.strip()), 14)
        return {
            "report_date_as_yyyy_mm_dd": dt.strftime("%Y-%m-%d"),
            "open_interest_all": num(oi.group(1)),
            "change_in_open_interest_all": num(chg.group(1)) if chg else 0,
            "dealer_positions_long_all": p[0],
            "dealer_positions_short_all": p[1],
            "asset_mgr_positions_long": p[3],
            "asset_mgr_positions_short": p[4],
            "lev_money_positions_long": p[6],
            "lev_money_positions_short": p[7],
            "other_rept_positions_long": p[9],
            "other_rept_positions_short": p[10],
        }
    raise ValueError("tff code %s not found" % code)


def main():
    legacy_txt = fetch(LEGACY_URL)
    tff_txt = fetch(TFF_URL)
    data = {
        "legacy": {s: parse_legacy(legacy_txt, c) for s, c in CODES.items()},
        "tff": {s: parse_tff(tff_txt, c) for s, c in CODES.items()},
    }
    try:
        with open(OUT, encoding="utf-8") as f:
            old = json.load(f)
    except Exception:
        old = {}
    if old.get("legacy") == data["legacy"] and old.get("tff") == data["tff"]:
        print("no change:", data["legacy"]["ES"]["report_date_as_yyyy_mm_dd"])
        return
    data["fetchedAt"] = datetime.now(timezone.utc).isoformat()
    with open(OUT, "w", encoding="utf-8") as f:
        json.dump(data, f, indent=1)
    print("wrote cot.json:", data["legacy"]["ES"]["report_date_as_yyyy_mm_dd"], data["tff"]["ES"]["report_date_as_yyyy_mm_dd"])


if __name__ == "__main__":
    try:
        main()
    except Exception as e:  # fail loudly so the workflow goes red
        print("fetch-cot failed:", e, file=sys.stderr)
        sys.exit(1)
