#!/usr/bin/env python3
"""Fetch ERCOT (+ optional PJM) grid data, Henry Hub, weather -> energy.json.
Every section is independent: a failure is logged in out["errors"] and the rest still ships.
Env: FRED_API_KEY (Henry Hub; falls back to Yahoo NG=F), PJM_API_KEY (optional, enables PJM)."""
import os, json, sys, time, datetime as dt, traceback
import requests
import pandas as pd

OUT = os.environ.get("ENERGY_OUT", "energy.json")
NORMS = os.environ.get("ENERGY_NORMS", "energy_norms.json")
out = {"generatedAt": dt.datetime.now(dt.timezone.utc).isoformat(), "errors": {}}
CT = "America/Chicago"


def rget(*a, tries=3, **k):
    """requests.get with retries on timeouts, connection errors, 429 and 5xx"""
    last = None
    for i in range(tries):
        try:
            r = requests.get(*a, **k)
            if r.status_code == 429 or r.status_code >= 500:
                last = requests.HTTPError(f"HTTP {r.status_code}")
            else:
                return r
        except (requests.Timeout, requests.ConnectionError) as e:
            last = e
        if i < tries - 1:
            time.sleep(2 * (i + 1))
    raise last


def guard(name):
    def deco(fn):
        def run(*a, **k):
            try:
                return fn(*a, **k)
            except Exception as e:
                out["errors"][name] = f"{type(e).__name__}: {e}"[:300]
                traceback.print_exc(file=sys.stderr)
                return None
        return run
    return deco


def ercot_recent(fn, *a, **k):
    """ERCOT "today" reports are empty right after midnight CT; fall back to yesterday..now."""
    try:
        return fn("today", *a, **k)
    except Exception:
        now = pd.Timestamp.now(tz="US/Central")
        return fn(now.normalize() - pd.Timedelta(days=1), *a, end=now, **k)


def col(df, *needles, exclude=()):
    """first column whose lowercase name contains all needles"""
    for c in df.columns:
        lc = str(c).lower()
        if all(n in lc for n in needles) and not any(x in lc for x in exclude):
            return c
    return None


def iso(ts):
    return pd.Timestamp(ts).tz_convert("UTC").strftime("%Y-%m-%dT%H:%MZ")


def num(x):
    try:
        v = float(x)
        return None if v != v else round(v, 2)
    except Exception:
        return None


# ───────────────────────── ERCOT ─────────────────────────
ercot = None
try:
    import gridstatus
    ercot = gridstatus.Ercot()
except Exception as e:
    out["errors"]["gridstatus"] = str(e)[:300]


@guard("ercot_load")
def ercot_load():
    d = ercot.get_load("today")
    c = col(d, "load") or d.columns[-1]
    actual = d[["Interval Start", c]].dropna()
    fc = None
    try:
        f = ercot.get_load_forecast("today")
        tc = col(f, "system total") or col(f, "total")
        if tc is not None:
            f = f.copy()
            f["_t"] = f["Interval Start"] if "Interval Start" in f else f.iloc[:, 0]
            fc = f[["_t", tc]].dropna()
    except Exception as e:
        out["errors"]["ercot_load_forecast"] = str(e)[:200]
    # hourly buckets; forecast = nearest hourly value
    a = actual.set_index("Interval Start")[c].resample("1h").mean()
    rows = []
    fser = None
    if fc is not None and len(fc):
        fser = fc.set_index("_t")[fc.columns[1]].resample("1h").mean()
    for t, v in a.items():
        fv = num(fser.get(t)) if fser is not None and t in fser.index else None
        rows.append({"t": iso(t), "actual": num(v), "forecast": fv})
    # include remaining forecast hours of today
    if fser is not None:
        end = a.index.max() + pd.Timedelta(hours=18)
        for t, v in fser.items():
            if a.index.max() < t <= end:
                rows.append({"t": iso(t), "actual": None, "forecast": num(v)})
    # committed capacity (ERCOT Supply & Demand dashboard feed): hourly mean, per row
    try:
        sd = rget("https://www.ercot.com/api/1/services/read/dashboards/supply-demand.json", timeout=30).json().get("data", [])
        cap = {}
        for x in sd:
            if x.get("capacity") is not None and x.get("epoch") is not None:
                h = int(x["epoch"]) // 3600000
                cap.setdefault(h, []).append(float(x["capacity"]))
        for r in rows:
            h = int(pd.Timestamp(r["t"]).timestamp() // 3600)
            if h in cap:
                r["committed"] = round(sum(cap[h]) / len(cap[h]), 1)
    except Exception as e:
        out["errors"]["ercot_committed"] = f"{type(e).__name__}: {e}"[:200]
    out.setdefault("ercot", {})["load"] = rows


@guard("ercot_fuel")
def ercot_fuel():
    d = ercot.get_fuel_mix("today")
    d = d.set_index("Time") if "Time" in d.columns else d.set_index(d.columns[0])
    d = d.resample("1h").mean(numeric_only=True).dropna(how="all")
    rows = []
    for t, r in d.iterrows():
        g = {"gasCC": 0.0, "gasPeaker": 0.0, "gas": 0.0, "solar": 0.0, "wind": 0.0, "nuclear": 0.0, "coal": 0.0, "other": 0.0}
        for k, v in r.items():
            lk, v = str(k).lower(), (num(v) or 0.0)
            if "gas" in lk and ("cc" in lk or "combined" in lk):
                g["gasCC"] += v
            elif "gas" in lk and ("peak" in lk or "gt" in lk or "turbine" in lk):
                g["gasPeaker"] += v
            elif "gas" in lk:
                g["gas"] += v
            elif "solar" in lk:
                g["solar"] += v
            elif "wind" in lk:
                g["wind"] += v
            elif "nuclear" in lk:
                g["nuclear"] += v
            elif "coal" in lk or "lignite" in lk:
                g["coal"] += v
            else:
                g["other"] += v
        g["t"] = iso(t)
        rows.append({k: (round(v, 1) if k != "t" else v) for k, v in g.items()})
    out.setdefault("ercot", {})["fuel"] = rows[-48:]


@guard("ercot_renew")
def ercot_renew():
    res = {}
    for key, fn, fcn in (("wind", ercot.get_wind_actual_and_forecast_hourly, "stwpf"),
                         ("solar", ercot.get_solar_actual_and_forecast_hourly, "stppf")):
        d = ercot_recent(fn)
        ac = col(d, "gen", "system")
        fc = col(d, fcn, "system") or col(d, "cop", "system")
        tc = col(d, "interval start") or col(d, "time") or d.columns[0]
        d = d.dropna(subset=[ac]) if ac else d
        rows = []
        for _, r in d.iterrows():
            a, f = num(r.get(ac)), num(r.get(fc))
            if a is None or f is None:
                continue
            rows.append({"t": iso(r[tc]), "actual": a, "forecast": f, "err": round(a - f, 1)})
        res[key] = rows[-24:]
    out.setdefault("ercot", {})["renew"] = res


@guard("ercot_prices")
def ercot_prices():
    from gridstatus import Markets
    hubs = ["HB_NORTH", "HB_HOUSTON", "HB_SOUTH", "HB_WEST", "HB_PAN", "HB_BUSAVG"]
    rt = ercot_recent(ercot.get_spp, market=Markets.REAL_TIME_15_MIN, locations=hubs, location_type="Trading Hub")
    da = ercot_recent(ercot.get_spp, market=Markets.DAY_AHEAD_HOURLY, locations=hubs, location_type="Trading Hub")
    pc = col(rt, "spp") or rt.columns[-1]
    dc = col(da, "spp") or da.columns[-1]
    rt = rt.sort_values("Interval Start")
    da = da.sort_values("Interval Start")
    avg = rt[rt.Location == "HB_BUSAVG"].set_index("Interval Start")[pc]
    res = []
    for h in hubs:
        if h == "HB_BUSAVG":
            continue
        s = rt[rt.Location == h].set_index("Interval Start")[pc].dropna()
        if s.empty:
            continue
        last_t = s.index[-1]
        rtv = num(s.iloc[-1])
        dsub = da[da.Location == h].set_index("Interval Start")[dc].dropna()
        dav = None
        if len(dsub):
            hr = last_t.floor("1h")
            dav = num(dsub.get(hr, dsub.iloc[-1]))
        base = num(avg.get(last_t)) if len(avg) else None
        res.append({
            "iso": "ERCOT", "name": h.replace("HB_", "") + " Hub", "id": h,
            "rt": rtv, "da": dav, "spread": None if rtv is None or dav is None else round(dav - rtv, 2),
            "energy": base, "mcc": None if base is None or rtv is None else round(rtv - base, 2),
            "mccProxy": True, "t": iso(last_t),
            "series": [num(v) for v in s.resample("1h").mean().dropna().tolist()][-24:],
        })
    out["hubs"] = out.get("hubs", []) + res


# ───────────────────────── PJM (optional) ─────────────────────────
@guard("pjm")
def pjm():
    key = os.environ.get("PJM_API_KEY")
    if not key:  # fall back to the public key PJM's own Data Miner web app uses
        key = rget("https://dataminer2.pjm.com/config/settings.json", timeout=20).json()["subscriptionKey"]
    import gridstatus
    from gridstatus import Markets
    p = gridstatus.PJM(api_key=key)
    rt = p.get_lmp("today", market=Markets.REAL_TIME_5_MIN, locations="hubs")
    da = p.get_lmp("today", market=Markets.DAY_AHEAD_HOURLY, locations="hubs")
    LC = "Location Name" if "Location Name" in rt.columns else "Location"
    res = []
    for loc, g in rt.groupby(LC):
        g = g.sort_values("Interval Start")
        last = g.iloc[-1]
        dsub = da[da[LC] == loc].set_index("Interval Start")["LMP"]
        dav = num(dsub.get(pd.Timestamp(last["Interval Start"]).floor("1h"), dsub.iloc[-1] if len(dsub) else None))
        rtv = num(last["LMP"])
        res.append({
            "iso": "PJM", "name": str(loc).title(), "id": str(loc),
            "rt": rtv, "da": dav, "spread": None if rtv is None or dav is None else round(dav - rtv, 2),
            "energy": num(last.get("Energy")), "mcc": num(last.get("Congestion")),
            "mlc": num(last.get("Loss")), "mccProxy": False, "t": iso(last["Interval Start"]),
            "series": [num(v) for v in g.set_index("Interval Start")["LMP"].resample("1h").mean().dropna().tolist()][-24:],
        })
    out["hubs"] = out.get("hubs", []) + res
    try:
        L = p.get_load("today")
        F = p.get_load_forecast("today")
        lc = col(L, "load") or L.columns[-1]
        fc = col(F, "load") or F.columns[-1]
        a = L.set_index("Interval Start")[lc].resample("1h").mean()
        f = F.set_index("Interval Start")[fc].resample("1h").mean()
        a = a.dropna()
        if len(a):
            lo, hi = a.index.min(), a.index.max() + pd.Timedelta(hours=18)
            ts = sorted(t for t in set(a.index) | set(f.index) if lo <= t <= hi)
        else:
            ts = sorted(f.index)[:24]
        out.setdefault("pjm", {})["load"] = [{"t": iso(t), "actual": num(a.get(t)), "forecast": num(f.get(t))} for t in ts]
        if not len(a):
            out["errors"]["pjm_load"] = "no PJM actual load returned"
    except Exception as e:
        out["errors"]["pjm_load"] = str(e)[:200]
    try:
        try:
            fm = p.get_fuel_mix("today")
        except Exception:  # PJM posts gen_by_fuel late; empty right after midnight ET
            now = pd.Timestamp.now(tz="US/Eastern")
            fm = p.get_fuel_mix(now.normalize() - pd.Timedelta(days=1), end=now)
        m = fm.set_index("Interval Start").resample("1h").mean(numeric_only=True).dropna(how="all")
        rows = []
        for t, r in m.iterrows():
            g = {"gas": 0.0, "solar": 0.0, "wind": 0.0, "nuclear": 0.0, "coal": 0.0, "other": 0.0}
            for k, v in r.items():
                lk, v = str(k).lower(), (num(v) or 0.0)
                tgt = "gas" if "gas" in lk else "solar" if "solar" in lk else "wind" if "wind" in lk else "nuclear" if "nuclear" in lk else "coal" if "coal" in lk else "other"
                g[tgt] += v
            rows.append({"t": iso(t), **{k: round(v, 1) for k, v in g.items()}})
        out.setdefault("pjm", {})["fuel"] = rows[-48:]
    except Exception as e:
        out["errors"]["pjm_fuel"] = str(e)[:200]
    try:  # Economic Max (daily total MW offered, posted ~5 AM ET): latest row + prior day for the delta
        j = rget("https://api.pjm.com/api/v1/day_gen_capacity", params={"rowCount": 3, "startRow": 1, "sort": "bid_datetime_beginning_ept", "order": "Desc"},
                 headers={"Ocp-Apim-Subscription-Key": key}, timeout=30).json()
        items = (j.get("items") if isinstance(j, dict) else j) or []
        if not items:
            raise ValueError("no day_gen_capacity rows")
        out.setdefault("pjm", {})["capacity"] = {
            "economicMax": num(items[0].get("eco_max")),
            "prev": num(items[1].get("eco_max")) if len(items) > 1 else None,
            "date": str(items[0].get("bid_datetime_beginning_ept"))}
    except Exception as e:
        out["errors"]["pjm_econmax"] = f"{type(e).__name__}: {e}"[:200]
    try:  # Primary (contingency) operational reserves — latest 15-second reading, RTO level
        j = rget("https://api.pjm.com/api/v1/operational_reserves", params={"rowCount": 60, "startRow": 1, "sort": "datetime_beginning_ept", "order": "Desc"},
                 headers={"Ocp-Apim-Subscription-Key": key}, timeout=30).json()
        items = (j.get("items") if isinstance(j, dict) else j) or []
        def pick(name):
            for r in items:
                if str(r.get("reserve_name", "")).strip() == name:
                    return r
            return None
        tot = pick("RTO Contingency (Primary) Reserve Total")
        req = pick("RTO Contingency (Primary) Reserve Requirement")
        if not tot:
            raise ValueError("no RTO primary reserve row")
        out.setdefault("pjm", {})["reserves"] = {"primary": num(tot.get("reserve_mw")),
            "requirement": num(req.get("reserve_mw")) if req else None,
            "t": str(tot.get("datetime_beginning_ept"))}
    except Exception as e:
        out["errors"]["pjm_reserves"] = f"{type(e).__name__}: {e}"[:200]


# ───────────────────────── Outages ─────────────────────────
@guard("outages")
def outages():
    d = ercot.get_hourly_resource_outage_capacity("today")
    tot = col(d, "total", "resource") or None
    cols = [c for c in d.columns if "total" in str(c).lower() and "mw" in str(c).lower()]
    if not cols:
        cols = [c for c in d.columns if pd.api.types.is_numeric_dtype(d[c])]
    r = d.iloc[-1]
    parts = {str(c): num(r[c]) for c in cols}
    out["outages"] = {"ercot": {"asOf": iso(r.get("Time", d.index[-1])) if "Time" in d.columns else None,
                                "totalMW": round(sum(v for v in parts.values() if v), 1), "parts": parts}}


# ───────────────────────── Henry Hub ─────────────────────────
@guard("gas")
def gas():
    key = os.environ.get("FRED_API_KEY")
    if key:
        r = rget("https://api.stlouisfed.org/fred/series/observations",
                         params={"series_id": "DHHNGSP", "api_key": key, "file_type": "json", "sort_order": "desc", "limit": 30}, timeout=30)
        r.raise_for_status()
        obs = [(o["date"], float(o["value"])) for o in r.json()["observations"] if o["value"] not in (".", "")]
        if obs:
            out["gas"] = {"source": "FRED DHHNGSP (spot)", "price": obs[0][1], "asOf": obs[0][0], "series": [v for _, v in reversed(obs)][-20:]}
            return
    r = rget("https://query1.finance.yahoo.com/v8/finance/chart/NG%3DF", params={"range": "1mo", "interval": "1d"},
                     headers={"User-Agent": "Mozilla/5.0"}, timeout=30)
    r.raise_for_status()
    res = r.json()["chart"]["result"][0]
    closes = [c for c in res["indicators"]["quote"][0]["close"] if c is not None]
    out["gas"] = {"source": "Yahoo NG=F (front-month futures, FRED key not set)", "price": round(res["meta"].get("regularMarketPrice", closes[-1]), 3),
                  "asOf": dt.date.today().isoformat(), "series": [round(c, 3) for c in closes][-20:]}


@guard("spark")
def spark():
    g = out.get("gas")
    if not g:
        return
    px = g["price"]
    res = []
    for h in out.get("hubs", []):
        if h.get("rt") is None:
            continue
        item = {"iso": h["iso"], "name": h["name"], "power": h["rt"]}
        for hr in (7.0, 10.0):
            sp = round(h["rt"] - px * hr, 2)
            item[f"s{int(hr)}"] = sp
            ser = h.get("series") or []
            hist = [round(v - px * hr, 2) for v in ser if v is not None]
            item[f"s{int(hr)}Series"] = hist
            if len(hist) >= 6:
                prior = sum(hist[-6:-1]) / 5
                diff = sp - prior
                item[f"s{int(hr)}Trend"] = "expanding" if diff > max(1.0, abs(prior) * 0.05) else "contracting" if diff < -max(1.0, abs(prior) * 0.05) else "flat"
            else:
                item[f"s{int(hr)}Trend"] = "n/a"
        res.append(item)
    out["spark"] = {"gas": px, "hubs": res}


# ───────────────────────── Weather ─────────────────────────
CITIES = {"Dallas": (32.78, -96.80), "Houston": (29.76, -95.37), "Chicago": (41.88, -87.63), "Philadelphia": (39.95, -75.17),
          "New York City": (40.71, -74.01), "Atlanta": (33.75, -84.39)}
WIND_PTS = {"West TX (Midland)": (31.99, -102.08), "TX Panhandle": (35.22, -101.83), "Coastal Bend TX": (27.8, -97.4),
            "Iowa": (41.6, -93.6), "Kansas": (37.75, -100.0), "Illinois": (40.48, -88.99)}


def norms():
    n = {"v": 1, "cities": {}}
    try:
        with open(NORMS) as f:
            c = json.load(f)
        if c.get("v") == 1:
            n = c
    except Exception:
        pass
    changed = False
    for name, (la, lo) in CITIES.items():
        if name in n["cities"]:
            continue
        changed = True
        r = rget("https://archive-api.open-meteo.com/v1/archive", params={
            "latitude": la, "longitude": lo, "start_date": "1991-01-01", "end_date": "2020-12-31",
            "daily": "temperature_2m_mean", "temperature_unit": "fahrenheit", "timezone": "auto"}, timeout=120)
        r.raise_for_status()
        dd = r.json()["daily"]
        buck = {}
        for d, t in zip(dd["time"], dd["temperature_2m_mean"]):
            if t is None:
                continue
            buck.setdefault(d[5:], []).append(t)
        n["cities"][name] = {k: round(sum(v) / len(v), 1) for k, v in buck.items()}
    if changed:
        with open(NORMS, "w") as f:
            json.dump(n, f)
    return n


@guard("weather")
def weather():
    nm = norms()
    res = []
    for name, (la, lo) in CITIES.items():
        r = rget("https://api.open-meteo.com/v1/forecast", params={
            "latitude": la, "longitude": lo, "daily": "temperature_2m_mean", "temperature_unit": "fahrenheit",
            "forecast_days": 7, "timezone": "auto"}, timeout=30)
        r.raise_for_status()
        dd = r.json()["daily"]
        days = []
        for d, t in zip(dd["time"], dd["temperature_2m_mean"]):
            n = nm["cities"][name].get(d[5:])
            if t is None or n is None:
                continue
            cdd, hdd = max(t - 65, 0), max(65 - t, 0)
            ncdd, nhdd = max(n - 65, 0), max(65 - n, 0)
            days.append({"d": d, "t": round(t, 1), "norm": n, "cdd": round(cdd, 1), "hdd": round(hdd, 1),
                         "cddA": round(cdd - ncdd, 1), "hddA": round(hdd - nhdd, 1)})
        res.append({"city": name, "days": days,
                    "cddA7": round(sum(x["cddA"] for x in days), 1), "hddA7": round(sum(x["hddA"] for x in days), 1)})
    out["weather"] = res


@guard("wind")
def wind():
    res = []
    for name, (la, lo) in WIND_PTS.items():
        r = rget("https://api.open-meteo.com/v1/forecast", params={
            "latitude": la, "longitude": lo, "hourly": "wind_speed_80m,wind_speed_100m", "wind_speed_unit": "mph",
            "forecast_days": 2, "timezone": "UTC"}, timeout=30)
        r.raise_for_status()
        hh = r.json()["hourly"]
        res.append({"name": name, "t": hh["time"][:48:3], "v80": hh["wind_speed_80m"][:48:3], "v100": hh["wind_speed_100m"][:48:3]})
    out["windCorridors"] = res


if __name__ == "__main__":
    if ercot is not None:
        ercot_load(); ercot_fuel(); ercot_renew(); ercot_prices(); outages()
    pjm()
    gas(); spark(); weather(); wind()
    out["links"] = [
        {"label": "ERCOT constraints / outages", "url": "https://www.ercot.com/gridmktinfo/dashboards"},
        {"label": "ERCOT transmission outages (MIS)", "url": "https://www.ercot.com/mp/data-products/data-product-details?id=NP3-233-CD"},
        {"label": "PJM transmission outages", "url": "https://www.pjm.com/markets-and-operations/transmission-service/transmission-outages"},
        {"label": "PJM Data Miner", "url": "https://dataminer2.pjm.com"},
    ]
    if not any(k in out for k in ("ercot", "hubs", "gas", "weather")):
        print("no data at all - not overwriting", file=sys.stderr)
        sys.exit(1)
    with open(OUT, "w") as f:
        json.dump(out, f, separators=(",", ":"))
    print("wrote", OUT, "errors:", out["errors"])
