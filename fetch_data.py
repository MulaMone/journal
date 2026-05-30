import yfinance as yf
import pandas as pd
from supabase import create_client
from datetime import datetime
import schedule, time

SUPABASE_URL = "SUPABASE_URL"
SUPABASE_KEY = "SUPABASE_KEY"


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
                    print(f"✗ {sym_name} {tf_name} — no data returned")
                    continue

                rows = []
                for ts, row in df.iterrows():
                    rows.append({
                        "symbol":    sym_name,
                        "timeframe": tf_name,
                        "time":      ts.isoformat(),
                        "open":      round(float(row["Open"]),   2),
                        "high":      round(float(row["High"]),   2),
                        "low":       round(float(row["Low"]),    2),
                        "close":     round(float(row["Close"]),  2),
                        "volume":    int(row["Volume"])
                    })

                supabase.table("chart_data").upsert(
                    rows,
                    on_conflict="symbol,timeframe,time"
                ).execute()

                print(f"✓ {sym_name} {tf_name} — {len(rows)} candles pushed")

            except Exception as e:
                print(f"✗ {sym_name} {tf_name} error: {e}")

# Run immediately then every 5 minutes
fetch_and_push()
schedule.every(5).minutes.do(fetch_and_push)

while True:
    schedule.run_pending()
    time.sleep(30)
