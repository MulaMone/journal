import yfinance as yf
from supabase import create_client
from datetime import datetime
import os
try:
    from dotenv import load_dotenv
    load_dotenv()
except ImportError:
    pass

SUPABASE_URL = os.getenv("SUPABASE_URL")
SUPABASE_KEY = os.getenv("SUPABASE_KEY")

supabase = create_client(SUPABASE_URL, SUPABASE_KEY)

SYMBOLS = {
    "ES":  "ES=F",
    "NQ":  "NQ=F",
    "MES": "MES=F",
    "MNQ": "MNQ=F"
}

INTERVALS = {
    "1m":  {"period": "1d",  "interval": "1m"},
    "5m":  {"period": "5d",  "interval": "5m"},
    "15m": {"period": "5d",  "interval": "15m"},
    "1h":  {"period": "1mo", "interval": "1h"},
}

def fetch_and_push():
    print(f"[{datetime.now().strftime('%H:%M:%S')}] Fetching...")
    for sym_name, sym_ticker in SYMBOLS.items():
        for tf_name, tf_opts in INTERVALS.items():
            try:
                df = yf.download(
                    sym_ticker,
                    period=tf_opts["period"],
                    interval=tf_opts["interval"],
                    progress=False
                )
                if df.empty:
                    print(f"  ✗ {sym_name} {tf_name} — no data")
                    continue

                rows = []
                for ts, row in df.iterrows():
                    rows.append({
                        "symbol":    sym_name,
                        "timeframe": tf_name,
                        "time":      ts.isoformat(),
                        "open":      round(float(row["Open"]),  2),
                        "high":      round(float(row["High"]),  2),
                        "low":       round(float(row["Low"]),   2),
                        "close":     round(float(row["Close"]), 2),
                        "volume":    int(row["Volume"])
                    })

                supabase.table("chart_data").upsert(
                    rows,
                    on_conflict="symbol,timeframe,time"
                ).execute()

                print(f"  ✓ {sym_name} {tf_name} — {len(rows)} candles")

            except Exception as e:
                print(f"  ✗ {sym_name} {tf_name} error: {e}")

fetch_and_push()
print("Done.")
