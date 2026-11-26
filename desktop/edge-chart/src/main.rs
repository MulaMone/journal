#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]
//! EDGE native chart: candles from the same OANDA feed as the web chart, drawn through the GPU.
//! Launched by the EDGE desktop shell. Reads EDGE_OANDA_TOKEN (and optional EDGE_OANDA_BASE) from the environment.

use chrono::{DateTime, Datelike, Offset, TimeZone, Timelike};
use chrono_tz::America::New_York;
use eframe::egui::{self, Align2, Color32, FontId, Pos2, Rect, Sense, Stroke, Vec2};
use std::collections::{BTreeMap, HashMap};
use std::io::{BufRead, BufReader};
use std::net::TcpListener;
use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

// ───────────────────────── style (edit to taste) ─────────────────────────
const BG: Color32 = Color32::BLACK;
const GRID: Color32 = Color32::from_rgb(0x1a, 0x32, 0x60); // web chart grid #1a3260
const AXIS_TXT: Color32 = Color32::from_rgb(0xe8, 0xf0, 0xff);
const UP_FILL: Color32 = Color32::from_rgb(0, 230, 118);
const UP_LINE: Color32 = Color32::from_rgb(4, 54, 31);
const DN_FILL: Color32 = Color32::from_rgb(255, 68, 68);
const DN_LINE: Color32 = Color32::from_rgb(74, 10, 10);
const BODY_RATIO: f32 = 0.70; // body width as a share of the bar spacing (NinjaTrader is ~0.70)
const WICK_PTS: f32 = 1.0; // wick + outline thickness in points (1px at 100% scaling, 2px at 150-200%)
const RIGHT_GAP_BARS: f32 = 8.0;
const RANGE_TICKS: f64 = 400.0; // the "400R" chart: every bar spans this many ticks (NQ/ES tick = 0.25 -> 100 points)
const EMA_FAST: usize = 14;
const EMA_SLOW: usize = 50;
const BIG_SWING: usize = 5; // 400R: big swings (NinjaTrader's Swing indicator, strength 5) = solid lines
const SMALL_SWING: usize = 2; // 400R: small swings = stars
const AXIS_W: f32 = 96.0;
const AXIS_H: f32 = 28.0;
// ─────────────────────────────────────────────────────────────────────────


#[derive(Clone, Copy, Debug)]
struct Candle {
    t: i64, // unix seconds, UTC
    e: i64, // ET wall-clock seconds (same as the web chart's "chart time")
    o: f64,
    h: f64,
    l: f64,
    c: f64,
    done: bool,
}

const SYMS: [(&str, &str); 7] = [
    ("ES", "SPX500_USD"),
    ("NQ", "NAS100_USD"),
    ("GC", "XAU_USD"),
    ("CL", "WTICO_USD"),
    ("NG", "NATGAS_USD"),
    ("DXY", ""), // NinjaTrader only (no OANDA feed): shown while EdgeFeed is connected for it
    ("BTC", ""),
];
const TFS: [(&str, &str, i64); 9] = [
    ("2m", "M2", 120),
    ("5m", "M5", 300),
    ("30m", "M30", 1800),
    ("1h", "H1", 3600),
    ("4h", "H4", 14400),
    ("8h", "H8", 28800),
    ("D", "D", 86400),
    ("W", "W", 604800),
    ("400R", "RANGE", 300), // range bars built from 1-minute candles (300 = treated like an intraday chart for sessions/levels)
];
const INTRA_TFS: [&str; 4] = ["2m", "5m", "30m", "1h"];
const HTFS: [(&str, &str, u32); 3] = [("1H", "H1", 1500), ("4H", "H4", 1000), ("D", "D", 500)];
const FUTURES: [&str; 7] = ["ES", "NQ", "CL", "GC", "NG", "DXY", "BTC"]; // sessions open 18:00 ET (the web chart's FUTURES_ALIGN_SYMS)

fn inst_for(sym: &str) -> &'static str {
    SYMS.iter().find(|s| s.0 == sym).map(|s| s.1).unwrap_or("NAS100_USD")
}
fn tick_size(sym: &str) -> f64 {
    match sym {
        "GC" => 0.1,
        "CL" => 0.01,
        "NG" => 0.001,
        "DXY" => 0.005,
        "BTC" => 5.0,
        _ => 0.25,
    }
}
fn align_for(sym: &str) -> i64 {
    if FUTURES.contains(&sym) { 18 } else { 17 }
}

/// EDGE symbol names that map onto the symbols this chart knows.
fn canon_sym(s: &str) -> String {
    let s = s.to_uppercase();
    match s.as_str() {
        "MNQ" | "NNQ" => "NQ",
        "MES" | "NES" => "ES",
        "MGC" | "1OZ" | "QO" => "GC",
        "MCL" | "QM" => "CL",
        "MNG" | "QG" => "NG",
        "GOLD" => "GC",
        "OIL" => "CL",
        "NATGAS" => "NG",
        "DX" | "USDX" => "DXY",
        "MBT" => "BTC",
        other => other,
    }
    .to_string()
}

/// EDGE timeframe names -> this chart's labels
fn canon_tf(s: &str) -> String {
    let t = s.to_lowercase();
    let m = match t.as_str() {
        "1d" | "d" | "1y" | "1m" => "D",
        "1w" | "w" => "W",
        "400r" => "400R",
        other => other,
    };
    if TFS.iter().any(|x| x.0 == m) { m.to_string() } else { "5m".to_string() }
}

fn off(t: i64) -> i64 {
    New_York.timestamp_opt(t, 0).single().map(|d| d.offset().fix().local_minus_utc() as i64).unwrap_or(-18000)
}

/// Alt+E puts the stop this many points past your entry.
const BE_PTS: f64 = 10.0;

/// One working NinjaTrader order on the chart's symbol.
#[derive(Clone, PartialEq)]
struct LiveOrd {
    id: String,
    is_stop: bool,                 // stop-type price (stop order) vs limit-type (limit / target)
    px: f64,
    val: Option<(f64, f64)>,       // exit orders only: (position average price, signed dollars per point)
    entry: bool,                   // a waiting ENTRY order (no position yet, or it adds to the position), not a stop / target
    buy: bool,
    qty: f64,
    acct: String,
    inst: String,                  // contract the order is on (NQ, MNQ, NNQ ...)
}

#[derive(Default)]
struct Shared {
    main: Vec<Candle>,
    daily: Vec<Candle>,
    m5: Vec<Candle>,
    htf: Vec<Candle>,
    status: String,
    version: u64, // bumps when a fresh full load replaces the main data
    nt: HashMap<String, Vec<Candle>>, // 1-minute futures bars pushed by NinjaTrader, per symbol
    nt_r: HashMap<String, Vec<Candle>>, // NinjaTrader's own 400-range bars (exact tick fills), per symbol; the 400R chart uses these when present
    nt_d: HashMap<String, Vec<Candle>>, // NinjaTrader's own daily bars (long history for D / W / levels)
    nt_conns: HashMap<String, usize>, // open NinjaTrader connections per symbol
    nt_owner: HashMap<String, String>, // canonical symbol -> raw NinjaTrader symbol that feeds it (MNQ and NQ must not both write the same bars)
    src: String,                      // "NT" or "OANDA" (shown in the toolbar)
    ord_sym: String,                  // chart symbol whose live orders the poller fetches
    entries: Vec<(f64, f64, f64, f64, String, String)>, // open positions on this symbol: (average price, +1 long / -1 short, contracts, dollars per point, account, contract e.g. NQ / NNQ)
    orders: Vec<LiveOrd>, // NinjaTrader orders working right now - futures prices, same as the NT candles
    toast: Option<(String, bool, std::time::Instant)>, // short message on the chart (text, ok, when)
    link_state: u8,                   // 2nd+ window: 1 = receiving NinjaTrader from the first window, 2 = first window is not sharing it
    force_cfd: bool,                  // the FUT | CFD switch is on CFD: ignore NinjaTrader and use the OANDA feed
}

enum Req {
    Load { sym: String, tf: String, htf: String }, // htf is an HTFS label, fetched in parallel with the main data
    Htf { sym: String, tf: String }, // tf is an HTFS label
    Nt { sym: String },              // NinjaTrader pushed new bars
    NtGone { sym: String },          // a NinjaTrader connection closed
}

fn fetch(base: &str, tok: &str, inst: &str, gran: &str, count: u32, align: i64) -> Result<Vec<Candle>, String> {
    fetch_ex(base, tok, inst, gran, count, align, None)
}

fn fetch_ex(base: &str, tok: &str, inst: &str, gran: &str, count: u32, align: i64, to: Option<i64>) -> Result<Vec<Candle>, String> {
    if inst.is_empty() {
        return Err("DXY and BTC only work while NinjaTrader is live (EdgeFeed on a DX / BTC chart)".into());
    }
    let to_q = to
        .and_then(|t| DateTime::from_timestamp(t, 0))
        .map(|d| format!("&to={}", d.format("%Y-%m-%dT%H:%M:%SZ")))
        .unwrap_or_default();
    let wk = if gran == "W" { "&weeklyAlignment=Sunday" } else { "" };
    let url = format!(
        "{base}/v3/instruments/{inst}/candles?granularity={gran}&price=M&count={count}&alignmentTimezone=America%2FNew_York&dailyAlignment={align}{wk}{to_q}"
    );
    let resp = ureq::get(&url)
        .set("Authorization", &format!("Bearer {tok}"))
        .timeout(Duration::from_secs(20))
        .call()
        .map_err(|e| match e {
            ureq::Error::Status(code, _) => format!("OANDA returned HTTP {code} (check the token)"),
            other => format!("network error: {other}"),
        })?;
    let v: serde_json::Value = resp.into_json().map_err(|e| format!("bad JSON: {e}"))?;
    let arr = v["candles"].as_array().ok_or("no candles in response")?;
    let mut out: Vec<Candle> = Vec::with_capacity(arr.len());
    for c in arr {
        let t = c["time"].as_str().and_then(|s| DateTime::parse_from_rfc3339(s).ok());
        let m = &c["mid"];
        let p = |k: &str| m[k].as_str().and_then(|s| s.parse::<f64>().ok());
        if let (Some(t), Some(o), Some(h), Some(l), Some(cl)) = (t, p("o"), p("h"), p("l"), p("c")) {
            let t = t.timestamp();
            if out.last().map_or(true, |x| t > x.t) {
                out.push(Candle { t, e: t + off(t), o, h, l, c: cl, done: c["complete"].as_bool() != Some(false) });
            }
        }
    }
    Ok(out)
}

fn merge(dst: &mut Vec<Candle>, new: Vec<Candle>) {
    for c in new {
        match dst.last().map(|l| l.t) {
            Some(lt) if c.t == lt => *dst.last_mut().unwrap() = c,
            Some(lt) if c.t > lt => dst.push(c),
            None => dst.push(c),
            _ => {
                if let Some(x) = dst.iter_mut().rev().take(6).find(|x| x.t == c.t) {
                    *x = c;
                }
            }
        }
    }
}

/// Range bars from 1-minute candles: each bar spans exactly `r` price units. Inside a minute the price is assumed to
/// travel open -> low -> high -> close (up candle) or open -> high -> low -> close (down candle), so bar edges are
/// approximate; real tick data would be exact.
fn range_bars(m1: &[Candle], r: f64) -> Vec<Candle> {
    let mut out: Vec<Candle> = vec![];
    let Some(f) = m1.first() else { return out };
    let mut price = f.o;
    let mut cur = Candle { t: f.t, e: f.e, o: price, h: price, l: price, c: price, done: false };
    let mut guard = 0u32;
    for k in m1 {
        let path = if k.c >= k.o { [k.o, k.l, k.h, k.c] } else { [k.o, k.h, k.l, k.c] };
        for &target in &path {
            loop {
                guard += 1;
                if guard > 5_000_000 {
                    return out;
                }
                if target > price {
                    let cap = cur.l + r;
                    if target > cap + 1e-9 {
                        cur.h = cap;
                        cur.c = cap;
                        cur.done = true;
                        out.push(cur);
                        price = cap;
                        cur = Candle { t: k.t, e: k.e, o: cap, h: cap, l: cap, c: cap, done: false };
                        continue;
                    }
                    price = target;
                    cur.h = cur.h.max(price);
                    cur.c = price;
                } else if target < price {
                    let cap = cur.h - r;
                    if target < cap - 1e-9 {
                        cur.l = cap;
                        cur.c = cap;
                        cur.done = true;
                        out.push(cur);
                        price = cap;
                        cur = Candle { t: k.t, e: k.e, o: cap, h: cap, l: cap, c: cap, done: false };
                        continue;
                    }
                    price = target;
                    cur.l = cur.l.min(price);
                    cur.c = price;
                }
                break;
            }
        }
    }
    out.push(cur);
    out
}

/// ~5 trading days of 1-minute candles (several pages of 5000)
fn fetch_m1(base: &str, tok: &str, inst: &str, align: i64) -> Result<Vec<Candle>, String> {
    let mut all: Vec<Candle> = vec![];
    let mut to: Option<i64> = None;
    for _ in 0..4 {
        let page = fetch_ex(base, tok, inst, "M1", 5000, align, to)?;
        let Some(first) = page.first().map(|c| c.t) else { break };
        if all.first().map_or(false, |a| first >= a.t) {
            break; // no older candles came back
        }
        let keep_until = all.first().map_or(i64::MAX, |a| a.t);
        let mut merged: Vec<Candle> = page.into_iter().filter(|c| c.t < keep_until).collect();
        merged.extend(all);
        all = merged;
        to = Some(first);
    }
    if all.is_empty() { Err("no 1-minute data".into()) } else { Ok(all) }
}

// ───────────────────────── NinjaTrader feed ─────────────────────────
/// Bucket 1-minute bars into `secs` buckets aligned to the session start (align hour ET); weekly buckets start Sunday.
fn agg_m1(m1: &[Candle], secs: i64, align: i64, weekly: bool) -> Vec<Candle> {
    let woff = if weekly { 3 * 86400 } else { 0 };
    let mut out: Vec<Candle> = vec![];
    let mut last_key = i64::MIN;
    for k in m1 {
        let key = (k.e - align * 3600 - woff).div_euclid(secs);
        if key == last_key {
            let c = out.last_mut().unwrap();
            c.h = c.h.max(k.h);
            c.l = c.l.min(k.l);
            c.c = k.c;
        } else {
            let e0 = key * secs + align * 3600 + woff;
            out.push(Candle { t: k.t - (k.e - e0), e: e0, o: k.o, h: k.h, l: k.l, c: k.c, done: true });
            last_key = key;
        }
    }
    if let Some(l) = out.last_mut() {
        l.done = false;
    }
    out
}

fn nt_ready(shared: &Arc<Mutex<Shared>>, sym: &str) -> bool {
    let s = shared.lock().unwrap();
    if s.force_cfd && !inst_for(sym).is_empty() {
        return false; // CFD selected (symbols with no OANDA feed, DXY / BTC, stay on NinjaTrader)
    }
    s.nt_conns.get(sym).copied().unwrap_or(0) > 0 && s.nt.get(sym).map_or(0, |v| v.len()) >= 2
}

/// Which NinjaTrader series exist for `sym`: (own 400-range bars, own daily bars). The main series is built from them, so when one arrives the bar count changes.
fn nt_sig(shared: &Arc<Mutex<Shared>>, sym: &str) -> (bool, bool) {
    let s = shared.lock().unwrap();
    (s.nt_r.get(sym).map_or(0, |v| v.len()) >= 2, s.nt_d.get(sym).map_or(0, |v| v.len()) >= 2)
}

fn htf_secs(gran: &str) -> i64 {
    match gran { "H1" => 3600, "H4" => 14400, _ => 86400 }
}

/// Rebuild every feed (main, 5m, daily, HTF) from the NinjaTrader 1-minute bars. False when NinjaTrader has nothing for `sym`.
fn nt_apply(shared: &Arc<Mutex<Shared>>, sym: &str, tf: &str, htf_gran: Option<&str>) -> bool {
    if !nt_ready(shared, sym) {
        return false;
    }
    let m1 = shared.lock().unwrap().nt.get(sym).cloned().unwrap_or_default();
    let al = align_for(sym);
    let nd = shared.lock().unwrap().nt_d.get(sym).cloned().unwrap_or_default();
    // daily / weekly come from NinjaTrader's daily series when it sent one (years of history), else from the 1-minute bars
    let daily_src: &[Candle] = if nd.len() >= 2 { &nd } else { &m1 };
    let nr = shared.lock().unwrap().nt_r.get(sym).cloned().unwrap_or_default();
    let main = if tf == "400R" {
        if nr.len() >= 2 {
            let mut v = nr;
            if let Some(l) = v.last_mut() {
                l.done = false;
            }
            v
        } else {
            range_bars(&m1, RANGE_TICKS * tick_size(sym))
        }
    } else if tf == "D" || tf == "W" {
        agg_m1(daily_src, if tf == "W" { 604800 } else { 86400 }, al, tf == "W")
    } else {
        let secs = TFS.iter().find(|t| t.0 == tf).map(|t| t.2).unwrap_or(300);
        agg_m1(&m1, secs, al, false)
    };
    let m5 = agg_m1(&m1, 300, al, false);
    let daily = agg_m1(daily_src, 86400, al, false);
    let htf = htf_gran.map(|g| agg_m1(if g == "D" { daily_src } else { &m1 }, htf_secs(g), al, false));
    let mut s = shared.lock().unwrap();
    s.main = main;
    s.m5 = m5;
    s.daily = daily;
    if let Some(h) = htf {
        s.htf = h;
    }
    s.src = "NT".into();
    true
}

/// NinjaTrader feed. The first chart window owns the NinjaTrader port and relays everything it accepts (127.0.0.1:7880) to the other
/// chart windows, so every window can show futures. If the owner closes, another window takes the port over by itself.
/// Wire format, one JSON object per line: {"sym":"NQ","reset":true,"bars":[[unix_utc_open,o,h,l,c],...]} (history, then one-bar updates);
/// the relay adds {"sym":"NQ","gone":true} when NinjaTrader drops a symbol.
static RELAY: Mutex<Vec<mpsc::Sender<Arc<String>>>> = Mutex::new(Vec::new()); // one writer queue per joined window

fn relay_send(line: &str) {
    let msg = Arc::new(format!("{line}\n"));
    RELAY.lock().unwrap().retain(|c| c.send(msg.clone()).is_ok()); // a window that is gone has a dead queue: dropped here
}

/// A window that joined as a relay client gets a "relay ok" answer, then everything the owner already holds (history), then the live lines.
/// Each joined window has its own writer thread and queue: a slow window can never stall NinjaTrader's feed or the other windows,
/// and no socket timeout is used (Windows applies timeouts per socket handle, not per connection, which cut links on a quiet feed).
fn relay_attach(shared: &Arc<Mutex<Shared>>, mut st: std::net::TcpStream) {
    use std::io::Write;
    let _ = st.set_nodelay(true);
    let (qtx, qrx) = mpsc::channel::<Arc<String>>();
    let mut clients = RELAY.lock().unwrap(); // held while the snapshot is queued, so no live line slips between snapshot and stream
    let _ = qtx.send(Arc::new("{\"relay\":\"ok\"}\n".to_string()));
    {
        let s = shared.lock().unwrap();
        for (sym, n) in &s.nt_conns {
            if *n == 0 {
                continue;
            }
            let raw = s.nt_owner.get(sym).cloned().unwrap_or_else(|| sym.clone());
            for (is_d, src) in [(false, s.nt.get(sym)), (true, s.nt_d.get(sym))] {
                let Some(v) = src else { continue };
                for (k, chunk) in v.chunks(20000).enumerate() {
                    let bars: Vec<serde_json::Value> = chunk.iter().map(|c| serde_json::json!([c.t, c.o, c.h, c.l, c.c])).collect();
                    let mut o = serde_json::json!({ "sym": raw, "bars": bars });
                    if k == 0 {
                        o["reset"] = serde_json::json!(true);
                    }
                    if is_d {
                        o["tf"] = serde_json::json!("D");
                    }
                    let _ = qtx.send(Arc::new(format!("{o}\n")));
                }
            }
        }
    }
    {
        let s = shared.lock().unwrap();
        for (sym, n) in &s.nt_conns {
            if *n == 0 {
                continue;
            }
            let raw = s.nt_owner.get(sym).cloned().unwrap_or_else(|| sym.clone());
            if let Some(v) = s.nt_r.get(sym) {
                for (k, chunk) in v.chunks(20000).enumerate() {
                    let bars: Vec<serde_json::Value> = chunk.iter().map(|c| serde_json::json!([c.t, c.o, c.h, c.l, c.c])).collect();
                    let o = serde_json::json!({ "sym": raw, "tf": "R", "from": k * 20000, "bars": bars });
                    let _ = qtx.send(Arc::new(format!("{o}\n")));
                }
            }
        }
    }
    clients.push(qtx);
    drop(clients);
    std::thread::spawn(move || {
        for msg in qrx {
            if st.write_all(msg.as_bytes()).is_err() {
                ilog("relay: write to a joined window failed, dropping it");
                break;
            }
        }
    });
}

/// One NinjaTrader (or relay) line. Returns true when it was accepted. `relay` = the line came from the owner window, not from NinjaTrader.
fn nt_line(sh: &Arc<Mutex<Shared>>, tx: &mpsc::Sender<Req>, ctx: &egui::Context, line: &str, mine: &mut Vec<String>, relay: bool) {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else { return };
    let sym = canon_sym(v["sym"].as_str().unwrap_or(""));
    if !SYMS.iter().any(|x| x.0 == sym) {
        return;
    }
    if relay && v["gone"].as_bool() == Some(true) {
        if let Some(i) = mine.iter().position(|m| *m == sym) {
            mine.remove(i);
            nt_drop(sh, tx, ctx, &sym, false);
        }
        return;
    }
    let bars: Vec<Candle> = v["bars"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|b| {
                    let f = |i: usize| b.get(i).and_then(|x| x.as_f64());
                    let t = f(0)? as i64;
                    Some(Candle { t, e: t + off(t), o: f(1)?, h: f(2)?, l: f(3)?, c: f(4)?, done: true })
                })
                .collect()
        })
        .unwrap_or_default();
    let raw = v["sym"].as_str().unwrap_or("").to_uppercase();
    {
        let mut s = sh.lock().unwrap();
        // one raw symbol per chart symbol: a second feed (e.g. MNQ next to NQ) would overwrite the forming bar with slightly different prices
        if s.nt_owner.entry(sym.clone()).or_insert_with(|| raw.clone()) != &raw {
            return;
        }
        if !mine.contains(&sym) {
            *s.nt_conns.entry(sym.clone()).or_insert(0) += 1;
            mine.push(sym.clone());
        }
        if v["tf"].as_str() == Some("R") {
            // 400-range bars: replace from bar index `from` on (several range bars can share one second, so no merge by time)
            let from = v["from"].as_u64().unwrap_or(0) as usize;
            let e = s.nt_r.entry(sym.clone()).or_default();
            if from <= e.len() {
                e.truncate(from);
                e.extend(bars);
                if e.len() > 60000 {
                    let cut = e.len() - 60000;
                    e.drain(..cut);
                }
            }
            drop(s);
            if !relay {
                relay_send(line);
            }
            let _ = tx.send(Req::Nt { sym });
            ctx.request_repaint();
            return;
        }
        let is_d = v["tf"].as_str() == Some("D");
        let e = if is_d { s.nt_d.entry(sym.clone()).or_default() } else { s.nt.entry(sym.clone()).or_default() };
        if v["reset"].as_bool() == Some(true) {
            e.clear();
        }
        merge(e, bars);
        if e.len() > 150000 {
            let cut = e.len() - 150000;
            e.drain(..cut);
        }
    }
    if !relay {
        relay_send(line);
    }
    let _ = tx.send(Req::Nt { sym });
    ctx.request_repaint();
}

/// Link diagnostics: %LOCALAPPDATA%\\EDGE\\chart-cache\\chart-link.log (kept small). Shows when a window joins / loses the NinjaTrader feed and why.
fn ilog(msg: &str) {
    use std::io::Write;
    let dir = cache_dir();
    let _ = std::fs::create_dir_all(&dir);
    let p = dir.join("chart-link.log");
    if std::fs::metadata(&p).map_or(false, |m| m.len() > 200_000) {
        let _ = std::fs::remove_file(&p);
    }
    let now = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs_f64()).unwrap_or(0.0);
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(p) {
        let _ = writeln!(f, "{:.2} pid{} {}", now, std::process::id(), msg);
    }
}

/// This window lost its NinjaTrader link for `sym`. The drop only counts after 4 s with no reconnect, so a brief
/// reconnect doesn't make the FUT | CFD switch flicker or push a futures chart back onto CFD. The owner tells the other windows once the last connection for it is gone.
fn nt_drop(sh: &Arc<Mutex<Shared>>, tx: &mpsc::Sender<Req>, ctx: &egui::Context, sym: &str, owner: bool) {
    let (sh, tx, ctx, sym) = (sh.clone(), tx.clone(), ctx.clone(), sym.to_string());
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_secs(4));
        let mut gone = false;
        {
            let mut s = sh.lock().unwrap();
            if let Some(n) = s.nt_conns.get_mut(&sym) {
                *n = n.saturating_sub(1);
                if *n == 0 {
                    s.nt_owner.remove(&sym);
                    gone = true;
                }
            }
        }
        ilog(&format!("{sym}: link gone for good={gone} owner={owner}"));
        if gone && owner {
            relay_send(&format!("{{\"sym\":\"{sym}\",\"gone\":true}}"));
        }
        let _ = tx.send(Req::NtGone { sym });
        ctx.request_repaint();
    });
}

fn nt_listener(shared: Arc<Mutex<Shared>>, tx: mpsc::Sender<Req>, ctx: egui::Context) {
    let port: u16 = std::env::var("EDGE_NT_PORT").ok().and_then(|p| p.parse().ok()).unwrap_or(7878);
    let mut first = true;
    loop {
        let bound = TcpListener::bind(("127.0.0.1", port));
        if first && bound.is_err() {
            shared.lock().unwrap().force_cfd = true; // 2nd, 3rd... window: starts on CFD, the FUT | CFD switch can still move it to futures
        }
        first = false;
        if let Ok(l) = bound {
            // owner window: takes NinjaTrader's connections, and relays them to the other chart windows that join on the same port
            shared.lock().unwrap().link_state = 0;
            for st in l.incoming() {
                let Ok(st) = st else { continue };
                let (sh, tx, ctx) = (shared.clone(), tx.clone(), ctx.clone());
                std::thread::spawn(move || {
                    let mut mine: Vec<String> = vec![];
                    let Ok(wr) = st.try_clone() else { return };
                    let mut n = 0;
                    ilog("owner: connection accepted");
                    for line in BufReader::new(st).lines() {
                        let line = match line {
                            Ok(l) => l,
                            Err(e) => {
                                ilog(&format!("owner: connection read error: {e}"));
                                break;
                            }
                        };
                        n += 1;
                        if n == 1 && line.len() < 64 && line.contains("\"relay\"") {
                            relay_attach(&sh, wr); // another chart window, not NinjaTrader
                            return;
                        }
                        nt_line(&sh, &tx, &ctx, &line, &mut mine, false);
                    }
                    ilog(&format!("owner: connection ended, symbols {:?}", mine));
                    for sym in mine {
                        nt_drop(&sh, &tx, &ctx, &sym, true);
                    }
                });
            }
            return;
        }
        // another window owns the port: join it and read the same feed
        let mut ok = false;
        if let Ok(mut st) = std::net::TcpStream::connect_timeout(&std::net::SocketAddr::from(([127, 0, 0, 1], port)), Duration::from_secs(2)) {
            use std::io::Write;
            let _ = st.set_nodelay(true);
            let _ = st.set_read_timeout(Some(Duration::from_secs(3)));
            let Ok(rs) = st.try_clone() else { continue };
            let mut rd = BufReader::new(rs);
            if st.write_all(b"{\"relay\":true}\n").is_ok() {
                let mut hello = String::new();
                // an older chart build never answers: that shows as "not shared"
                let hello_ok = rd.read_line(&mut hello).is_ok() && hello.contains("ok");
                // Windows keeps the 3 s timeout on the handle that reads (the clone), so clear it there, not only on `st`
                let _ = rd.get_ref().set_read_timeout(None);
                let _ = st.set_read_timeout(None);
                if hello_ok {
                    ok = true;
                    shared.lock().unwrap().link_state = 1;
                    ctx.request_repaint();
                    let mut mine: Vec<String> = vec![];
                    ilog("client: joined the first window");
                    for line in rd.lines() {
                        let line = match line {
                            Ok(l) => l,
                            Err(e) => {
                                ilog(&format!("client: read error: {e}"));
                                break;
                            }
                        };
                        nt_line(&shared, &tx, &ctx, &line, &mut mine, true);
                    }
                    ilog(&format!("client: link ended, symbols {:?}", mine));
                    for sym in mine {
                        nt_drop(&shared, &tx, &ctx, &sym, false);
                    }
                }
            }
        }
        if !ok {
            ilog("client: could not join the first window");
        }
        shared.lock().unwrap().link_state = if ok { 0 } else { 2 };
        ctx.request_repaint();
        std::thread::sleep(Duration::from_millis(800)); // owner gone or not ready yet: try to take over / reconnect
    }
}

/// Polls EdgeFeed (127.0.0.1:7881/orders) for the orders NinjaTrader has working on the chart's symbol.
/// An order is live while its latest recorded event is "W" (working); fills and cancels end it.
/// With an open position in the order's account, an order on the opposite side is a stop / target; anything else is a waiting entry order.
/// Wakes order_poll early: EdgeFeed's push stream and a command that just succeeded both call this, so the lines redraw at once instead of after the next 1.5 s tick.
static WAKE: (Mutex<bool>, std::sync::Condvar) = (Mutex::new(false), std::sync::Condvar::new());
fn wake_poll() {
    let (m, c) = &WAKE;
    *m.lock().unwrap() = true;
    c.notify_all();
}
fn wait_poll(ms: u64) {
    let (m, c) = &WAKE;
    let g = m.lock().unwrap();
    let (mut g, _) = c.wait_timeout_while(g, Duration::from_millis(ms), |w| !*w).unwrap();
    *g = false;
}

/// Listens to EdgeFeed's server-sent events (127.0.0.1:7881/stream): a tiny message arrives the moment NinjaTrader reports an order or a fill.
/// Reconnects by itself; while EdgeFeed is not running it just retries every second (the 1.5 s poll keeps working on its own).
fn order_stream() {
    loop {
        let agent = ureq::AgentBuilder::new().timeout_connect(Duration::from_secs(2)).timeout_read(Duration::from_secs(30)).build();
        if let Ok(r) = agent.get("http://127.0.0.1:7881/stream").call() {
            for line in BufReader::new(r.into_reader()).lines() {
                let Ok(line) = line else { break };
                if line.starts_with("data:") {
                    wake_poll();
                }
            }
        }
        std::thread::sleep(Duration::from_millis(1000));
    }
}

fn order_poll(shared: Arc<Mutex<Shared>>, ctx: egui::Context) {
    loop {
        wait_poll(1500);
        let sym = shared.lock().unwrap().ord_sym.clone();
        if sym.is_empty() {
            continue;
        }
        let now = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0);
        let url = format!("http://127.0.0.1:7881/orders?sym={sym}&from={}&to={}", now - 259200, now + 120);
        let res = ureq::get(&url).timeout(Duration::from_secs(2)).call().ok().and_then(|r| r.into_json::<serde_json::Value>().ok());
        let mut live: Vec<LiveOrd> = vec![];
        let mut pos_e: Vec<(f64, f64, f64, f64, String, String)> = vec![];
        if let Some(v) = res {
            let pos: Vec<(String, f64, f64, f64, f64, String)> = v["pos"]
                .as_array()
                .map(|a| a.iter().filter_map(|q| Some((q[0].as_str()?.to_string(), q[1].as_f64()?, q[2].as_f64()?, q[3].as_f64()?, q[4].as_f64()?, q[5].as_str().unwrap_or("").to_string()))).collect())
                .unwrap_or_default();
            pos_e = pos.iter().map(|q| (q.3, q.1, q.2, q.4, q.0.clone(), q.5.clone())).collect();
            let mut last: HashMap<String, (i64, bool, f64, String, f64, bool, String)> = HashMap::new();
            if let Some(a) = v["orders"].as_array() {
                for e in a {
                    let (Some(t), Some(id), Some(k), Some(px), Some(st)) = (e[0].as_i64(), e[1].as_str(), e[2].as_str(), e[4].as_f64(), e[6].as_str()) else { continue };
                    if last.get(id).map_or(true, |o| t >= o.0) {
                        last.insert(id.to_string(), (t, k == "S", px, st.to_string(), e[5].as_f64().unwrap_or(0.0), e[3].as_str() == Some("B"), e[7].as_str().unwrap_or("").to_string()));
                    }
                }
            }
            for (id, (_, is_stop, px, st, qty, buy, inst)) in last {
                if st == "W" {
                    let acct = id.split('|').next().unwrap_or("").to_string();
                    let pz = pos.iter().find(|q| q.0 == acct && (inst.is_empty() || q.5 == inst));
                    // long position + a sell order (or short + buy) = it exits the trade
                    let exit = pz.map_or(false, |q| (q.1 > 0.0) != buy);
                    // what an exit order is worth: (price - average) x this, with this = direction x dollars per point x the order's contracts
                    let val = if exit { pz.map(|q| (q.3, q.1 * q.4 * if qty > 0.0 { qty } else { q.2 })) } else { None };
                    live.push(LiveOrd { id: id.clone(), is_stop, px, val, entry: !exit, buy, qty: if qty > 0.0 { qty } else { 1.0 }, acct, inst });
                }
            }
            live.sort_by(|a, b| a.px.partial_cmp(&b.px).unwrap_or(std::cmp::Ordering::Equal));
        }
        let ents: Vec<(f64, f64, f64, f64, String, String)> = pos_e;
        let changed = {
            let mut s = shared.lock().unwrap();
            let c = s.orders != live || s.entries != ents;
            if c {
                s.orders = live;
                s.entries = ents;
            }
            c
        };
        if changed {
            ctx.request_repaint();
        }
    }
}
/// Pushes labels apart so none overlap (each keeps at least 28 px of its own).
fn spread_y(ys: &mut [f32]) {
    let mut idx: Vec<usize> = (0..ys.len()).collect();
    idx.sort_by(|a, b| ys[*a].partial_cmp(&ys[*b]).unwrap_or(std::cmp::Ordering::Equal));
    for w in 1..idx.len() {
        let prev = ys[idx[w - 1]];
        if ys[idx[w]] < prev + 28.0 {
            ys[idx[w]] = prev + 28.0;
        }
    }
}

/// The secret key EdgeFeed made on its first run (%APPDATA%\EdgeFeed\key.txt). Orders are only moved / closed for requests that carry it.
fn edge_key() -> String {
    let base = std::env::var("APPDATA").unwrap_or_default();
    if base.is_empty() {
        return String::new();
    }
    std::fs::read_to_string(std::path::Path::new(&base).join("EdgeFeed").join("key.txt")).map(|s| s.trim().to_string()).unwrap_or_default()
}
fn urlenc(s: &str) -> String {
    let mut o = String::new();
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || b == b'-' || b == b'_' || b == b'.' || b == b'~' {
            o.push(b as char);
        } else {
            o.push_str(&format!("%{:02X}", b));
        }
    }
    o
}
/// Sends a move / close command to EdgeFeed (NinjaTrader). On a refusal the reason shows on the chart; on success the order lines simply
/// redraw from NinjaTrader's real order state a moment later.
fn send_cmd(shared: Arc<Mutex<Shared>>, ctx: egui::Context, path: String) {
    std::thread::spawn(move || {
        let key = edge_key();
        let url = format!("http://127.0.0.1:7881{path}");
        let mut rq = ureq::post(&url).timeout(Duration::from_secs(4));
        if !key.is_empty() {
            rq = rq.set("X-Edge-Key", &key);
        }
        let (ok, msg) = match rq.call() {
            Ok(r) => {
                let v = r.into_json::<serde_json::Value>().unwrap_or_default();
                (v["ok"].as_bool().unwrap_or(false), v["err"].as_str().unwrap_or("NinjaTrader did not answer").to_string())
            }
            Err(ureq::Error::Status(_, r)) => {
                let v = r.into_json::<serde_json::Value>().unwrap_or_default();
                (false, v["err"].as_str().unwrap_or("NinjaTrader link refused the command").to_string())
            }
            Err(_) => (false, "Can't reach EdgeFeed in NinjaTrader".to_string()),
        };
        if ok {
            // NinjaTrader acted (EdgeFeed already waited for the fill): re-read orders and positions now
            wake_poll();
            std::thread::sleep(Duration::from_millis(300));
            wake_poll();
        }
        if !ok {
            let msg: String = msg.chars().take(110).collect();
            shared.lock().unwrap().toast = Some((msg, false, std::time::Instant::now()));
            ctx.request_repaint();
        }
    });
}

// ───────────────────────── candle cache (disk + memory, delta refresh) ─────────────────────────
type Cache = Arc<Mutex<HashMap<(String, String), Vec<Candle>>>>;

fn htf_count(g: &str) -> usize {
    HTFS.iter().find(|h| h.1 == g).map(|h| h.2 as usize).unwrap_or(500)
}
fn tail_n(mut v: Vec<Candle>, n: usize) -> Vec<Candle> {
    if v.len() > n {
        let cut = v.len() - n;
        v.drain(..cut);
    }
    v
}
fn gran_secs(g: &str) -> i64 {
    match g { "M1" => 60, "M2" => 120, "M5" => 300, "M30" => 1800, "H1" => 3600, "H4" => 14400, "H8" => 28800, "D" => 86400, "W" => 604800, _ => 300 }
}
fn cache_dir() -> PathBuf {
    let base = std::env::var_os("LOCALAPPDATA")
        .or_else(|| std::env::var_os("XDG_CACHE_HOME"))
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cache")))
        .unwrap_or_else(std::env::temp_dir);
    base.join("EDGE").join("chart-cache")
}
fn cache_file(sym: &str, g: &str) -> PathBuf {
    cache_dir().join(format!("{sym}_{g}.bin"))
}
const REC: usize = 41; // t i64, o h l c f64, done u8
fn cache_load(sym: &str, g: &str) -> Vec<Candle> {
    let Ok(b) = std::fs::read(cache_file(sym, g)) else { return vec![] };
    if b.len() < 8 || &b[..4] != b"EDC1" {
        return vec![];
    }
    let n = u32::from_le_bytes([b[4], b[5], b[6], b[7]]) as usize;
    if b.len() != 8 + n * REC {
        return vec![];
    }
    let f = |c: &[u8], i: usize| f64::from_le_bytes(c[i..i + 8].try_into().unwrap());
    b[8..]
        .chunks_exact(REC)
        .map(|c| {
            let t = i64::from_le_bytes(c[0..8].try_into().unwrap());
            Candle { t, e: t + off(t), o: f(c, 8), h: f(c, 16), l: f(c, 24), c: f(c, 32), done: c[40] != 0 }
        })
        .collect()
}
fn cache_save(sym: &str, g: &str, cs: &[Candle]) {
    let _ = std::fs::create_dir_all(cache_dir());
    let mut b: Vec<u8> = Vec::with_capacity(8 + cs.len() * REC);
    b.extend_from_slice(b"EDC1");
    b.extend_from_slice(&(cs.len() as u32).to_le_bytes());
    for c in cs {
        b.extend_from_slice(&c.t.to_le_bytes());
        for v in [c.o, c.h, c.l, c.c] {
            b.extend_from_slice(&v.to_le_bytes());
        }
        b.push(c.done as u8);
    }
    let p = cache_file(sym, g);
    static SEQ: AtomicU64 = AtomicU64::new(0); // two threads can save the same series at once: never share a temp file
    let tmp = p.with_extension(format!("tmp{}", SEQ.fetch_add(1, Ordering::SeqCst)));
    if std::fs::write(&tmp, b).is_ok() {
        if std::fs::rename(&tmp, &p).is_err() {
            let _ = std::fs::remove_file(&tmp);
        }
    }
}
/// Memory first, then disk.
fn cached_get(cache: &Cache, sym: &str, g: &str) -> Vec<Candle> {
    let key = (sym.to_string(), g.to_string());
    if let Some(v) = cache.lock().unwrap().get(&key) {
        return v.clone();
    }
    let v = cache_load(sym, g);
    if !v.is_empty() {
        cache.lock().unwrap().insert(key, v.clone());
    }
    v
}
/// Latest `full_n` candles. With a cached copy that is deep enough, only the candles since its last bar are fetched
/// (one small request) and appended; otherwise a full fetch. The result is saved to disk.
fn load_series(base: &str, tok: &str, sym: &str, g: &str, full_n: u32, al: i64, seed: Vec<Candle>) -> Result<Vec<Candle>, String> {
    let inst = inst_for(sym);
    let mut cs = seed;
    if cs.len() as u32 >= full_n * 9 / 10 && cs.len() >= 50 {
        let now = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0);
        let last = cs[cs.len() - 1].t;
        let missing = ((now - last) / gran_secs(g)).max(0) + 4;
        if missing <= 2500 {
            let new = fetch(base, tok, inst, g, missing as u32, al)?;
            if let Some(f) = new.first().map(|c| c.t) {
                cs.retain(|c| c.t < f);
                cs.extend(new);
                if cs.len() > 5000 {
                    let cut = cs.len() - 5000;
                    cs.drain(..cut);
                }
                cache_save(sym, g, &cs);
                return Ok(cs);
            }
        }
    }
    let full = fetch(base, tok, inst, g, full_n, al)?;
    cache_save(sym, g, &full);
    Ok(full)
}
/// Quietly warm the cache for the other timeframes (two at a time) so switching to them is instant.
fn prefetch(base: &str, tok: &str, sym: &str, skip: &str, cache: &Cache, pgen: &Arc<AtomicU64>, my: u64) {
    let grans: Vec<&'static str> = ["M2", "M5", "M30", "H1", "H4", "H8", "D", "W"].into_iter().filter(|g| *g != skip).collect();
    for lane in 0..2usize {
        let (b, t, sy, ca, ge) = (base.to_string(), tok.to_string(), sym.to_string(), cache.clone(), pgen.clone());
        let mine: Vec<&'static str> = grans.iter().enumerate().filter(|(i, _)| i % 2 == lane).map(|(_, g)| *g).collect();
        std::thread::spawn(move || {
            for g in mine {
                if ge.load(Ordering::SeqCst) != my {
                    return; // symbol changed: stop
                }
                let seed = cached_get(&ca, &sy, g);
                if let Ok(cs) = load_series(&b, &t, &sy, g, 3000, align_for(&sy), seed) {
                    ca.lock().unwrap().insert((sy.clone(), g.to_string()), cs);
                }
                std::thread::sleep(Duration::from_millis(200));
            }
        });
    }
}

fn worker(rx: mpsc::Receiver<Req>, tx_self: mpsc::Sender<Req>, shared: Arc<Mutex<Shared>>, ctx: egui::Context, base: String, tok: String) {
    let mut cur_tf = String::new();
    let mut src_sig = (false, false); // nt_sig() the main series was last built from
    let mut nt_active = false;
    let mut cur: Option<(String, String)> = None; // (sym, main gran)
    let mut htf_cur: Option<(String, String)> = None; // (sym, htf gran)
    let mut tick = 0u32;
    let mut last_cfd = false; // feed the aux data (daily / 5m / HTF) was loaded from
    let mut data_sym = String::new(); // symbol the daily / 5m / HTF data currently belong to
    let cache: Cache = Arc::new(Mutex::new(HashMap::new())); // candles per (symbol, OANDA granularity), backed by disk
    let pf_gen = Arc::new(AtomicU64::new(0));
    let mut pf_done: HashSet<String> = HashSet::new();
    let mut m1raw: Vec<Candle> = vec![];
    let set_status = |s: &str| {
        shared.lock().unwrap().status = s.to_string();
        ctx.request_repaint();
    };
    loop {
        match rx.recv_timeout(Duration::from_secs(2)) {
            Ok(Req::Load { sym, tf, htf }) => {
                // daily / 5m / HTF data belong to the symbol, not the timeframe: keep them when only the timeframe changes
                let cfd_now = shared.lock().unwrap().force_cfd;
                let feed_changed = cfd_now != last_cfd; // futures prices and CFD prices differ: nothing may carry over
                last_cfd = cfd_now;
                let same_sym = data_sym == sym && !feed_changed;
                if !same_sym {
                    pf_gen.fetch_add(1, Ordering::SeqCst);
                    pf_done.clear();
                    let mut s = shared.lock().unwrap();
                    if feed_changed {
                        s.main.clear();
                    }
                    s.daily.clear();
                    s.m5.clear();
                    s.htf.clear();
                }
                let gran = TFS.iter().find(|t| t.0 == tf).map(|t| t.1).unwrap_or("M5");
                let al = align_for(&sym);
                cur_tf = tf.clone();
                let hg: Option<&'static str> = HTFS.iter().find(|h| h.0 == htf).map(|h| h.1);
                if nt_apply(&shared, &sym, &tf, hg) {
                    nt_active = true;
                    src_sig = nt_sig(&shared, &sym);
                    data_sym = sym.clone();
                    htf_cur = hg.map(|g| (sym.clone(), g.to_string()));
                    cur = Some((sym.clone(), gran.to_string()));
                    {
                        let mut s = shared.lock().unwrap();
                        s.status.clear();
                        s.version += 1;
                    }
                    ctx.request_repaint();
                    continue;
                }
                nt_active = false;
                shared.lock().unwrap().src = "OANDA".into();
                let is_range = gran == "RANGE";
                // paint from the memory / disk cache straight away, then catch up with a small request
                let seed_main = if is_range { vec![] } else { cached_get(&cache, &sym, gran) };
                let shown = !seed_main.is_empty();
                if shown {
                    {
                        let mut s = shared.lock().unwrap();
                        s.main = seed_main.clone();
                        s.status.clear();
                        s.version += 1;
                    }
                    cur = Some((sym.clone(), gran.to_string()));
                    ctx.request_repaint();
                } else {
                    set_status("loading…");
                }
                if !same_sym {
                    let sd = cached_get(&cache, &sym, "D");
                    let sm = cached_get(&cache, &sym, "M5");
                    let sh = hg.map(|g| cached_get(&cache, &sym, g)).unwrap_or_default();
                    let mut s = shared.lock().unwrap();
                    if !sd.is_empty() { s.daily = sd; }
                    if !sm.is_empty() { s.m5 = sm; }
                    if !sh.is_empty() { s.htf = tail_n(sh, hg.map_or(500, htf_count)); }
                }
                // levels (D), 5m and the HTF gaps load at the same time as the main candles
                let need_aux = {
                    let s = shared.lock().unwrap();
                    !same_sym || s.daily.is_empty() || s.m5.is_empty()
                };
                let need_htf = hg.is_some() && (!same_sym || htf_cur.as_ref().map_or(true, |h| Some(h.1.as_str()) != hg) || shared.lock().unwrap().htf.is_empty());
                let job = |g: &'static str, n: u32, seed: Vec<Candle>| {
                    let (b, t, sy) = (base.clone(), tok.clone(), sym.clone());
                    std::thread::spawn(move || load_series(&b, &t, &sy, g, n, al, seed))
                };
                let h_d = if need_aux { Some(job("D", 800, cached_get(&cache, &sym, "D"))) } else { None };
                let h_m5 = if need_aux && gran != "M5" { Some(job("M5", 3000, cached_get(&cache, &sym, "M5"))) } else { None };
                let h_htf = if need_htf {
                    hg.map(|g| job(g, HTFS.iter().find(|h| h.1 == g).map(|h| h.2).unwrap_or(500), cached_get(&cache, &sym, g)))
                } else {
                    None
                };
                let res = if is_range {
                    set_status("building range bars…");
                    fetch_m1(&base, &tok, inst_for(&sym), al).map(|m| {
                        let bars = range_bars(&m, RANGE_TICKS * tick_size(&sym));
                        m1raw = m;
                        bars
                    })
                } else {
                    load_series(&base, &tok, &sym, gran, 3000, al, seed_main)
                };
                match res {
                    Ok(cs) => {
                        if !is_range {
                            cache.lock().unwrap().insert((sym.clone(), gran.to_string()), cs.clone());
                        }
                        let mut s = shared.lock().unwrap();
                        s.main = cs;
                        s.status.clear();
                        if !shown {
                            s.version += 1; // a cached view was already shown (and zoomed); the refresh just swaps the data in
                        }
                        cur = Some((sym.clone(), gran.to_string()));
                    }
                    Err(e) => {
                        cur = Some((sym.clone(), gran.to_string())); // so a NinjaTrader feed that connects later is picked up
                        set_status(&e);
                        continue;
                    }
                }
                ctx.request_repaint();
                if need_aux && gran == "M5" {
                    let m = shared.lock().unwrap().main.clone(); // the main 5m candles double as the EMA / NY-open feed
                    shared.lock().unwrap().m5 = m;
                }
                if let Some(Ok(d)) = h_d.map(|h| h.join()).and_then(|r| r.ok()) {
                    cache.lock().unwrap().insert((sym.clone(), "D".to_string()), d.clone());
                    shared.lock().unwrap().daily = d;
                }
                if let Some(Ok(m)) = h_m5.map(|h| h.join()).and_then(|r| r.ok()) {
                    cache.lock().unwrap().insert((sym.clone(), "M5".to_string()), m.clone());
                    shared.lock().unwrap().m5 = m;
                }
                if let (Some(h), Some(g)) = (h_htf, hg) {
                    if let Ok(Ok(b)) = h.join() {
                        cache.lock().unwrap().insert((sym.clone(), g.to_string()), b.clone());
                        shared.lock().unwrap().htf = tail_n(b, htf_count(g));
                        htf_cur = Some((sym.clone(), g.to_string()));
                    }
                }
                data_sym = sym.clone();
                ctx.request_repaint();
                if !pf_done.contains(&sym) {
                    pf_done.insert(sym.clone());
                    prefetch(&base, &tok, &sym, gran, &cache, &pf_gen, pf_gen.load(Ordering::SeqCst));
                }
            }
            Ok(Req::Htf { sym, tf }) => {
                if let Some(h) = HTFS.iter().find(|h| h.0 == tf) {
                    if htf_cur.as_ref().map_or(false, |c| c.0 == sym && c.1 == h.1) && !shared.lock().unwrap().htf.is_empty() {
                        continue; // already loaded together with the main data
                    }
                    if nt_active && cur.as_ref().map_or(false, |c| c.0 == sym) && nt_ready(&shared, &sym) {
                        let m1 = shared.lock().unwrap().nt.get(&sym).cloned().unwrap_or_default();
                        shared.lock().unwrap().htf = agg_m1(&m1, htf_secs(h.1), align_for(&sym), false);
                        htf_cur = Some((sym, h.1.to_string()));
                        ctx.request_repaint();
                    } else if let Ok(b) = fetch(&base, &tok, inst_for(&sym), h.1, h.2, align_for(&sym)) {
                        shared.lock().unwrap().htf = b;
                        htf_cur = Some((sym, h.1.to_string()));
                        ctx.request_repaint();
                    }
                }
            }
            Ok(Req::Nt { sym }) => {
                if let Some((cs, _)) = cur.clone() {
                    if cs == sym {
                        let hg = htf_cur.as_ref().filter(|h| h.0 == sym).map(|h| h.1.clone());
                        let sig = nt_sig(&shared, &sym);
                        if nt_apply(&shared, &sym, &cur_tf, hg.as_deref()) {
                            // NinjaTrader's own 400R / daily bars arrived after the first 1-minute data: the main series is replaced by one with a
                            // different bar count, so the old scroll position would point at bars from a day ago. Re-apply the default view.
                            let swapped = (cur_tf == "400R" && sig.0 != src_sig.0) || ((cur_tf == "D" || cur_tf == "W") && sig.1 != src_sig.1);
                            src_sig = sig;
                            if !nt_active {
                                nt_active = true;
                                let mut s = shared.lock().unwrap();
                                s.version += 1;
                                s.status.clear();
                            } else if swapped {
                                shared.lock().unwrap().version += 1;
                            }
                            ctx.request_repaint();
                        }
                    }
                }
            }
            Ok(Req::NtGone { sym }) => {
                if nt_active && cur.as_ref().map_or(false, |c| c.0 == sym) && !nt_ready(&shared, &sym) {
                    nt_active = false;
                    shared.lock().unwrap().src = "OANDA".into();
                    let hl = htf_cur.as_ref().and_then(|h| HTFS.iter().find(|x| x.1 == h.1)).map(|x| x.0).unwrap_or("4H");
                    let _ = tx_self.send(Req::Load { sym: sym.clone(), tf: cur_tf.clone(), htf: hl.to_string() });
                    if let Some(h) = htf_cur.as_ref().and_then(|h| HTFS.iter().find(|x| x.1 == h.1)) {
                        let _ = tx_self.send(Req::Htf { sym, tf: h.0.to_string() });
                    }
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                if nt_active {
                    continue; // NinjaTrader is the live source; no OANDA polling
                }
                tick += 1;
                if let Some((sym, gran)) = &cur {
                    let al = align_for(sym);
                    if gran == "RANGE" {
                        if let Ok(cs) = fetch(&base, &tok, inst_for(sym), "M1", 3, al) {
                            merge(&mut m1raw, cs);
                            let bars = range_bars(&m1raw, RANGE_TICKS * tick_size(sym));
                            shared.lock().unwrap().main = bars;
                        }
                    } else if let Ok(cs) = fetch(&base, &tok, inst_for(sym), gran, 3, al) {
                        merge(&mut shared.lock().unwrap().main, cs);
                    }
                    if tick % 15 == 0 {
                        if let Ok(cs) = fetch(&base, &tok, inst_for(sym), "M5", 3, al) {
                            merge(&mut shared.lock().unwrap().m5, cs);
                        }
                        if let Ok(cs) = fetch(&base, &tok, inst_for(sym), "D", 2, al) {
                            merge(&mut shared.lock().unwrap().daily, cs);
                        }
                        if let Some((hs, hg)) = &htf_cur {
                            if let Ok(cs) = fetch(&base, &tok, inst_for(hs), hg, 3, align_for(hs)) {
                                merge(&mut shared.lock().unwrap().htf, cs);
                            }
                        }
                    }
                    ctx.request_repaint();
                }
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => return,
        }
    }
}

// ───────────────────────── helpers ─────────────────────────
fn nice_step(raw: f64) -> f64 {
    let p = 10f64.powf(raw.abs().max(1e-9).log10().floor());
    let f = raw / p;
    let n = if f <= 1.0 { 1.0 } else if f <= 2.0 { 2.0 } else if f <= 2.5 { 2.5 } else if f <= 5.0 { 5.0 } else { 10.0 };
    n * p
}

fn fmt_price(p: f64, step: f64) -> String {
    let d = if step >= 0.1 { 2 } else { 4 };
    format!("{p:.d$}")
}

fn et(t: i64) -> DateTime<chrono_tz::Tz> {
    New_York.timestamp_opt(t, 0).single().unwrap_or_else(|| New_York.timestamp_opt(0, 0).unwrap())
}

fn hexc(h: u32, alpha: f32) -> Color32 {
    Color32::from_rgba_unmultiplied((h >> 16) as u8, (h >> 8) as u8, h as u8, (alpha * 255.0) as u8)
}
fn rgb(h: u32) -> Color32 {
    hexc(h, 1.0)
}

fn ymd(secs: i64) -> (i64, i64, i64) {
    let d = DateTime::from_timestamp(secs, 0).map(|d| d.naive_utc()).unwrap_or_default();
    (d.year() as i64, d.month0() as i64, d.day() as i64)
}
fn weekday(day_idx: i64) -> i64 {
    (day_idx + 4).rem_euclid(7) // 0 = Sunday
}

fn dash_h(p: &egui::Painter, x0: f32, x1: f32, y: f32, on: f32, offl: f32, s: Stroke) {
    let mut x = x0;
    while x < x1 {
        p.hline(x..=(x + on).min(x1), y, s);
        x += on + offl;
    }
}
fn dash_v(p: &egui::Painter, x: f32, y0: f32, y1: f32, on: f32, offl: f32, s: Stroke) {
    let mut y = y0;
    while y < y1 {
        p.vline(x, y..=(y + on).min(y1), s);
        y += on + offl;
    }
}
/// Dashes laid out on the chart's own x axis, so they move with the candles instead of crawling while you pan.
fn dashed_by_x(p: &egui::Painter, pts: &[Pos2], anchor: f32, on: f32, offl: f32, st: Stroke) {
    let per = on + offl;
    for w in pts.windows(2) {
        let (a, b) = (w[0], w[1]);
        if b.x - a.x < 1e-3 {
            continue;
        }
        let mut x = a.x;
        let mut guard = 0;
        while x < b.x && guard < 64 {
            guard += 1;
            let u = (x - anchor).rem_euclid(per);
            let (is_on, rem) = if u < on { (true, on - u) } else { (false, per - u) };
            let nx = (x + rem.max(0.01)).min(b.x);
            if is_on {
                let t0 = (x - a.x) / (b.x - a.x);
                let t1 = (nx - a.x) / (b.x - a.x);
                p.line_segment([Pos2::new(x, a.y + (b.y - a.y) * t0), Pos2::new(nx, a.y + (b.y - a.y) * t1)], st);
            }
            x = nx;
        }
    }
}
fn text_w(p: &egui::Painter, s: &str, f: &FontId) -> f32 {
    p.layout_no_wrap(s.to_string(), f.clone(), Color32::WHITE).size().x
}
/// Soft drop shadow + faint light rim so tags lift off the black background and off the candles behind them.
fn tag_box(p: &egui::Painter, r: Rect, fill: Color32) {
    p.add(egui::Shadow { offset: [0, 2], blur: 8, spread: 0, color: Color32::from_black_alpha(150) }.as_shape(r, 4.0));
    p.rect_filled(r, 4.0, fill);
    p.rect_stroke(r, 4.0, Stroke::new(1.0, Color32::from_white_alpha(28)), egui::StrokeKind::Inside);
}
/// Order label: dark plate, bright accent edge + stripe, large light text - readable on any candle colour.
fn order_tag(p: &egui::Painter, r: Rect, accent: Color32, txt: &str, tcol: Color32, f: FontId) {
    tag_box(p, r, rgb(0x0a1424));
    p.rect_stroke(r, 4.0, Stroke::new(1.5, accent), egui::StrokeKind::Inside);
    p.rect_filled(Rect::from_min_size(r.min, Vec2::new(6.0, r.height())), 4.0, accent);
    p.text(Pos2::new(r.left() + 14.0, r.center().y), Align2::LEFT_CENTER, txt, f, tcol);
}
fn pill(p: &egui::Painter, r: Rect, fill: Color32, text: &str, tcol: Color32, f: FontId) {
    tag_box(p, r, fill);
    p.text(r.center(), Align2::CENTER_CENTER, text, f, tcol);
}
/// Four-corner colour gradient rectangle (GPU mesh).
fn grad_rect(p: &egui::Painter, r: Rect, tl: Color32, tr: Color32, br: Color32, bl: Color32) {
    let mut m = egui::Mesh::default();
    m.colored_vertex(r.left_top(), tl);
    m.colored_vertex(r.right_top(), tr);
    m.colored_vertex(r.right_bottom(), br);
    m.colored_vertex(r.left_bottom(), bl);
    m.add_triangle(0, 1, 2);
    m.add_triangle(0, 2, 3);
    p.add(egui::Shape::mesh(m));
}
/// Soft fill between a line and the bottom of the plot, fading to nothing.
fn fill_under(p: &egui::Painter, pts: &[Pos2], col: u32, bottom: f32) {
    let mut m = egui::Mesh::default();
    for (k, pt) in pts.iter().enumerate() {
        m.colored_vertex(*pt, hexc(col, 0.12));
        m.colored_vertex(Pos2::new(pt.x, bottom), hexc(col, 0.0));
        if k > 0 {
            let b = (k * 2) as u32;
            m.add_triangle(b - 2, b - 1, b);
            m.add_triangle(b - 1, b + 1, b);
        }
    }
    p.add(egui::Shape::mesh(m));
}
/// Nicer fonts when available: fonts next to the exe (font_mono.ttf / font_ui.ttf) win, else Windows' Consolas / Segoe UI.
fn setup_fonts(ctx: &egui::Context) {
    use std::sync::Arc;
    let dir = std::env::current_exe().ok().and_then(|p| p.parent().map(|d| d.to_path_buf()));
    let find = |own: &str, sys: &[&str]| -> Option<Vec<u8>> {
        if let Some(d) = &dir {
            if let Ok(b) = std::fs::read(d.join(own)) {
                return Some(b);
            }
        }
        sys.iter().find_map(|p| std::fs::read(p).ok())
    };
    let mut fonts = egui::FontDefinitions::default();
    if let Some(b) = find("font_mono.ttf", &["C:\\Windows\\Fonts\\consola.ttf"]) {
        fonts.font_data.insert("edge_mono".into(), Arc::new(egui::FontData::from_owned(b)));
        fonts.families.entry(egui::FontFamily::Monospace).or_default().insert(0, "edge_mono".into());
    }
    if let Some(b) = find("font_ui.ttf", &["C:\\Windows\\Fonts\\segoeui.ttf"]) {
        fonts.font_data.insert("edge_ui".into(), Arc::new(egui::FontData::from_owned(b)));
        fonts.families.entry(egui::FontFamily::Proportional).or_default().insert(0, "edge_ui".into());
    }
    ctx.set_fonts(fonts);
}
fn lum_text(col: u32) -> Color32 {
    let l = 0.299 * ((col >> 16) & 255) as f32 + 0.587 * ((col >> 8) & 255) as f32 + 0.114 * (col & 255) as f32;
    if l > 150.0 { rgb(0x041018) } else { Color32::WHITE }
}

// ───────────────────────── FVG ─────────────────────────
struct Zone {
    i0: usize,
    lo: f64,
    hi: f64,
}
struct DoneZone {
    i0: usize,
    i1: usize,
    lo: f64,
    hi: f64,
}

/// 3-candle fair value gaps. Open ones shrink as price trades back into them; fully filled ones are dropped (last 5 kept grey).
fn fvg(cs: &[Candle]) -> (Vec<Zone>, Vec<DoneZone>) {
    let (mut zones, mut done) = (vec![], vec![]);
    let mut n = cs.len();
    while n > 0 && !cs[n - 1].done {
        n -= 1; // only completed candles form gaps
    }
    for i in 2..n {
        let (a, c) = (&cs[i - 2], &cs[i]);
        let (mut lo, mut hi, up);
        if c.l > a.h {
            lo = a.h;
            hi = c.l;
            up = true;
        } else if c.h < a.l {
            lo = c.h;
            hi = a.l;
            up = false;
        } else {
            continue;
        }
        let (lo0, hi0) = (lo, hi);
        let mut closed = None;
        for j in (i + 1)..cs.len() {
            let k = &cs[j];
            if up {
                if k.l <= lo {
                    closed = Some(j);
                    break;
                }
                if k.l < hi {
                    hi = k.l;
                }
            } else {
                if k.h >= hi {
                    closed = Some(j);
                    break;
                }
                if k.h > lo {
                    lo = k.h;
                }
            }
        }
        match closed {
            None => zones.push(Zone { i0: i - 2, lo, hi }),
            Some(j) => done.push(DoneZone { i0: i - 2, i1: j, lo: lo0, hi: hi0 }),
        }
    }
    done.sort_by_key(|d| d.i1);
    let k = done.len().saturating_sub(5);
    (zones, done.split_off(k))
}

struct HZone {
    e0: i64,
    lo0: f64,
    hi0: f64,
    lo: f64,
    hi: f64,
    up: bool,
}

/// Open higher-timeframe gaps (orange); the part price already filled is kept so it can be drawn periwinkle.
fn htf_zones(bars: &[Candle]) -> Vec<HZone> {
    let mut n = bars.len();
    while n > 0 && !bars[n - 1].done {
        n -= 1;
    }
    let mut out = vec![];
    for i in 2..n {
        let (a, c) = (&bars[i - 2], &bars[i]);
        let (lo, hi, up);
        if c.l > a.h {
            lo = a.h;
            hi = c.l;
            up = true;
        } else if c.h < a.l {
            lo = c.h;
            hi = a.l;
            up = false;
        } else {
            continue;
        }
        let mut z = HZone { e0: a.e, lo0: lo, hi0: hi, lo, hi, up };
        let mut shut = false;
        for k in &bars[(i + 1)..] {
            if up {
                if k.l <= z.lo0 {
                    shut = true;
                    break;
                }
                if k.l < z.hi {
                    z.hi = k.l;
                }
            } else {
                if k.h >= z.hi0 {
                    shut = true;
                    break;
                }
                if k.h > z.lo {
                    z.lo = k.h;
                }
            }
        }
        if !shut {
            out.push(z);
        }
    }
    out
}

// ───────────────────────── previous-period levels ─────────────────────────
#[derive(Clone, Copy, PartialEq)]
enum Per {
    Day,
    Week,
    Month,
    Quarter,
    Year,
}
fn key_of(p: Per, e: i64, anchor: i64) -> i64 {
    let s = e + anchor;
    let day = s.div_euclid(86400);
    let (y, m, _) = ymd(s);
    match p {
        Per::Day => day,
        Per::Week => (day + 3).div_euclid(7), // Monday-start trading week (Sunday's open shifts into Monday)
        Per::Month => y * 12 + m,
        Per::Quarter => y * 4 + m / 3,
        Per::Year => y,
    }
}

#[derive(Clone, Copy)]
struct Hlc {
    h: f64,
    l: f64,
    c: f64,
    first: i64,
}
fn agg(ds: &[Candle], p: Per, anchor: i64) -> BTreeMap<i64, Hlc> {
    let mut m: BTreeMap<i64, Hlc> = BTreeMap::new();
    for d in ds {
        let k = key_of(p, d.e, anchor);
        let x = m.entry(k).or_insert(Hlc { h: f64::MIN, l: f64::MAX, c: d.c, first: d.e });
        x.h = x.h.max(d.h);
        x.l = x.l.min(d.l);
        x.c = d.c;
    }
    m
}

struct Lv {
    rank: i32,
    k: String,
    price: f64,
    col: u32,
}

/// Intraday mode: previous day / week / month / quarter / year high, low and close.
fn intraday_levels(ds: &[Candle], cur_e: i64, anchor: i64) -> Vec<Lv> {
    let mut lv = vec![];
    if ds.len() < 3 {
        return lv;
    }
    let mut di = ds.len() - 1;
    while di > 0 && ds[di].e > cur_e {
        di -= 1;
    }
    const DAYC: [u32; 3] = [0x00b862, 0xff4444, 0xb8c4d9];
    const MONC: [u32; 3] = [0xa3e635, 0xff7a59, 0x8a3ffc];
    let mut add = |rank: i32, pre: &str, h: f64, l: f64, c: f64, cols: [u32; 3]| {
        for (s, p, col) in [("H", h, cols[0]), ("L", l, cols[1]), ("C", c, cols[2])] {
            if p.is_finite() {
                lv.push(Lv { rank, k: format!("{pre}{s}"), price: p, col });
            }
        }
    };
    for (rank, pre, per, cols) in [(5, "PY", Per::Year, MONC), (4, "PQ", Per::Quarter, DAYC), (3, "PM", Per::Month, MONC), (2, "PW", Per::Week, DAYC)] {
        let m = agg(ds, per, anchor);
        let ck = key_of(per, ds[di].e, anchor);
        if let Some(x) = m.get(&(ck - 1)) {
            if x.first > ds[0].e {
                add(rank, pre, x.h, x.l, x.c, cols);
            }
        }
    }
    if di >= 1 {
        let d = &ds[di - 1];
        add(1, "PD", d.h, d.l, d.c, DAYC);
    }
    lv.sort_by(|a, b| b.rank.cmp(&a.rank));
    lv
}

struct Group {
    i0: usize,
    i1: usize,
    h: f64,
    l: f64,
    c: f64,
}
/// Normal mode: each period shows the prior period's high / low / close (day for <=30m, week for 1h/4h, month for D).
fn period_groups(cs: &[Candle], ds: &[Candle], per: Per, anchor: i64, extra: i64) -> Vec<Group> {
    let m = agg(ds, per, anchor);
    let mut out: Vec<Group> = vec![];
    let mut cur_k = i64::MIN;
    for (i, c) in cs.iter().enumerate() {
        let k = key_of(per, c.e + extra, anchor);
        if k != cur_k {
            cur_k = k;
            if let Some((_, x)) = m.range(..k).next_back() {
                out.push(Group { i0: i, i1: i, h: x.h, l: x.l, c: x.c });
            } else {
                out.push(Group { i0: i, i1: i, h: f64::NAN, l: f64::NAN, c: f64::NAN });
            }
        } else if let Some(g) = out.last_mut() {
            g.i1 = i;
        }
    }
    out
}

fn ema_of(src: &[f64], len: usize) -> Vec<Option<f64>> {
    let k = 2.0 / (len as f64 + 1.0);
    let (mut s, mut v) = (0.0, 0.0);
    let mut out = vec![None; src.len()];
    for i in 0..src.len() {
        if i + 1 < len {
            s += src[i];
        } else if i + 1 == len {
            s += src[i];
            v = s / len as f64;
            out[i] = Some(v);
        } else {
            v = src[i] * k + v * (1.0 - k);
            out[i] = Some(v);
        }
    }
    out
}
/// last index i with bars[i].e < lim
fn last_before(bars: &[Candle], lim: i64) -> Option<usize> {
    let p = bars.partition_point(|b| b.e < lim);
    if p == 0 { None } else { Some(p - 1) }
}
/// 5m EMA(len) sampled per chart candle (steps on 5m+ charts, smoothed per candle below 5m)
fn ema_points(cs: &[Candle], m5: &[Candle], len: usize, per: i64) -> Vec<(usize, f64)> {
    let closes: Vec<f64> = m5.iter().map(|b| b.c).collect();
    let ema = ema_of(&closes, len);
    let mut pts = vec![];
    if cs.is_empty() {
        return pts;
    }
    if per < 300 {
        let a = 1.0 - (1.0 - 2.0 / (len as f64 + 1.0)).powf(per as f64 / 300.0);
        let mut e = last_before(m5, cs[0].e + 1).and_then(|j| ema[j]).unwrap_or(cs[0].c);
        for (i, c) in cs.iter().enumerate() {
            e += a * (c.c - e);
            pts.push((i, e));
        }
        return pts;
    }
    for (i, c) in cs.iter().enumerate() {
        if let Some(j) = last_before(m5, c.e + per) {
            if m5[j].e < c.e - 300 {
                continue;
            }
            if let Some(v) = ema[j] {
                pts.push((i, v));
            }
        }
    }
    pts
}

/// NinjaTrader's Swing indicator (same rules as its source), run on the completed bars. Prices within `eps` count as equal, like ApproxCompare.
/// Returns (high line per bar, low line per bar, confirmed swing-high bars, confirmed swing-low bars).
/// The lines follow NinjaTrader's plot: only the latest swing is shown, a newer swing replaces it, and a bar that trades through it clears it.
fn nt_swing(cs: &[Candle], s: usize, eps: f64) -> (Vec<Option<f64>>, Vec<Option<f64>>, Vec<usize>, Vec<usize>) {
    let mut n = cs.len();
    while n > 0 && !cs[n - 1].done {
        n -= 1; // the forming bar is not processed (Calculate = OnBarClose)
    }
    let k = 2 * s + 1;
    let mut hp: Vec<Option<f64>> = vec![None; cs.len()];
    let mut lp: Vec<Option<f64>> = vec![None; cs.len()];
    let (mut ph, mut pl): (Vec<usize>, Vec<usize>) = (vec![], vec![]);
    let (mut ch, mut cl) = (0.0f64, 0.0f64);
    for i in 0..n {
        if i + 1 < k {
            continue;
        }
        let w = &cs[i + 1 - k..=i];
        let cand = w[s].h;
        let is_h = (0..s).all(|j| w[j].h < cand - eps) && (s + 1..k).all(|j| w[j].h <= cand + eps);
        if is_h {
            ch = cand;
            ph.push(i - s);
            for j in 0..=s {
                hp[i - j] = Some(ch);
            }
        } else if cs[i].h > ch + eps || ch == 0.0 {
            ch = 0.0;
            hp[i] = None;
        } else {
            hp[i] = Some(ch);
        }
        let cand = w[s].l;
        let is_l = (0..s).all(|j| w[j].l > cand + eps) && (s + 1..k).all(|j| w[j].l >= cand - eps);
        if is_l {
            cl = cand;
            pl.push(i - s);
            for j in 0..=s {
                lp[i - j] = Some(cl);
            }
        } else if cs[i].l < cl - eps || cl == 0.0 {
            cl = f64::MAX;
            lp[i] = None;
        } else {
            lp[i] = Some(cl);
        }
    }
    (hp, lp, ph, pl)
}
/// Runs of equal levels -> (first bar, last bar, price)
fn swing_runs(pl: &[Option<f64>]) -> Vec<(usize, usize, f64)> {
    let mut out: Vec<(usize, usize, f64)> = vec![];
    for (i, v) in pl.iter().enumerate() {
        let Some(v) = *v else { continue };
        match out.last_mut() {
            Some(r) if r.1 + 1 == i && (r.2 - v).abs() < 1e-9 => r.1 = i,
            _ => out.push((i, i, v)),
        }
    }
    out
}
/// Five-point star (filled mesh)
fn star_shape(p: &egui::Painter, c: Pos2, r: f32, col: Color32) {
    let mut m = egui::Mesh::default();
    m.colored_vertex(c, col);
    for k in 0..10 {
        let a = -std::f32::consts::FRAC_PI_2 + k as f32 * std::f32::consts::PI / 5.0;
        let rr = if k % 2 == 0 { r } else { r * 0.45 };
        m.colored_vertex(Pos2::new(c.x + rr * a.cos(), c.y + rr * a.sin()), col);
    }
    for k in 0..10u32 {
        m.add_triangle(0, 1 + k, 1 + (k + 1) % 10);
    }
    p.add(egui::Shape::mesh(m));
}

// ── chart linking: native chart windows sharing the same colour group mirror zoom/scroll (by TIME) and the crosshair ──
const LINK_COLORS: [(u8, u8, u8); 5] = [(0, 212, 212), (0, 230, 118), (251, 191, 36), (255, 68, 68), (168, 85, 247)];
const LINK_PORT: u16 = 7879;

// Tiny local hub: the first chart window to start owns 127.0.0.1:7879 and relays JSON lines between all windows.
// If the hub window closes, the others reconnect and one of them takes over.
fn link_thread(out_rx: mpsc::Receiver<String>, in_tx: mpsc::Sender<String>, ctx: egui::Context) {
    use std::io::Write;
    let port: u16 = std::env::var("EDGE_LINK_PORT").ok().and_then(|v| v.parse().ok()).unwrap_or(LINK_PORT);
    loop {
        if let Ok(l) = TcpListener::bind(("127.0.0.1", port)) {
            let clients: Arc<Mutex<Vec<std::net::TcpStream>>> = Arc::new(Mutex::new(vec![]));
            {
                let (cl, tx, ctx) = (clients.clone(), in_tx.clone(), ctx.clone());
                std::thread::spawn(move || {
                    for st in l.incoming().flatten() {
                        let _ = st.set_nodelay(true);
                        if let Ok(w) = st.try_clone() {
                            cl.lock().unwrap().push(w);
                        }
                        let (cl2, tx2, ctx2) = (cl.clone(), tx.clone(), ctx.clone());
                        std::thread::spawn(move || {
                            for line in BufReader::new(st).lines().map_while(Result::ok) {
                                let msg = format!("{line}\n");
                                cl2.lock().unwrap().retain_mut(|c| c.write_all(msg.as_bytes()).is_ok());
                                let _ = tx2.send(line);
                                ctx2.request_repaint();
                            }
                        });
                    }
                });
            }
            // hub's own messages go to every client and straight to the local window's inbox is not needed (own id is ignored)
            while let Ok(m) = out_rx.recv() {
                let msg = format!("{m}\n");
                clients.lock().unwrap().retain_mut(|c| c.write_all(msg.as_bytes()).is_ok());
            }
            return;
        }
        match std::net::TcpStream::connect(("127.0.0.1", port)) {
            Ok(mut w) => {
                let _ = w.set_nodelay(true);
                let Ok(r) = w.try_clone() else { continue };
                let (tx, ctx2) = (in_tx.clone(), ctx.clone());
                let reader = std::thread::spawn(move || {
                    for line in BufReader::new(r).lines().map_while(Result::ok) {
                        let _ = tx.send(line);
                        ctx2.request_repaint();
                    }
                });
                loop {
                    match out_rx.recv_timeout(Duration::from_millis(500)) {
                        Ok(m) => {
                            if w.write_all(format!("{m}\n").as_bytes()).is_err() {
                                break;
                            }
                        }
                        Err(mpsc::RecvTimeoutError::Timeout) => {
                            if reader.is_finished() {
                                break;
                            }
                        }
                        Err(_) => return,
                    }
                }
            }
            Err(_) => std::thread::sleep(Duration::from_millis(400)),
        }
    }
}

struct App {
    link_tx: mpsc::Sender<String>,
    link_rx: mpsc::Receiver<String>,
    link_id: String,
    link_g: Option<usize>,          // colour group (None = unlinked)
    link_peers: HashMap<String, std::time::Instant>,
    link_last: Option<(i64, i64)>,  // last shared visible time range (unix secs)
    link_sent: std::time::Instant,
    link_xh_sent: bool,
    link_xh: Option<(i64, f64, std::time::Instant)>, // other window's crosshair: time, price
    bar_over: bool,
    bar_x: f32,
    sym: String,
    tf: String,
    shared: Arc<Mutex<Shared>>,
    tx: mpsc::Sender<Req>,
    spacing: f32,
    right_idx: f32, // fractional index shown at the right edge of the plot
    seen_version: u64,
    has_token: bool,
    intraday: bool,
    show_fvg: bool,
    show_htf: bool,
    htf_i: usize,
    hide_nums: bool,
    note: String,
    show_pw: bool, // 30m: previous-week lines (off by default, like the web chart)
    show_pm: bool, // 8h: previous-month lines
    show_pq: bool, // D: previous-quarter lines
    show_py: bool, // W: previous-year lines
    manual: Option<(f64, f64)>, // manual price range (after dragging the price axis / panning vertically); None = auto-fit
    drag_mode: u8,              // 1 = plot drag, 2 = price-axis drag, 3 = measure (shift + drag)
    ord_drag: Option<(String, bool, f64)>, // a stop / target being dragged: (order id, is stop, price under the pointer)
    ord_pend: Option<(String, f64, std::time::Instant)>, // a move sent to NinjaTrader, held on screen until NinjaTrader shows it
    close_sent: Option<std::time::Instant>, // last X click (ignores a double click)
    rev_sent: Option<std::time::Instant>, // last reverse click (own timer, so Close right after a reverse still works)
    meas_p0: Option<Pos2>,      // where the measure drag started (screen)
    meas: Option<(f32, f64, f32, f64)>, // measure: (bar idx, price) start and end
    meas_w: f32,                // widest label so far (keeps the label from jittering as the text changes)
    vdrag: f32,
    fling: f32,                 // pan momentum, bars per second
    zoom_t: Option<(f32, f32)>, // eased wheel zoom: (target spacing, anchor x)
    xh_last: Option<Pos2>,      // last crosshair position, kept while it fades out
    xh_off: bool,               // a click on the chart hides / shows the crosshair lines (like the web chart)
    show_wk_d: bool,            // D: a vertical line at the start of each week (off by default)
    show_mo_w: bool,            // W: a vertical line at the start of each month (off by default)
    swing_ext: bool,            // 400R: big swing lines run on until price hits them (off by default = NinjaTrader's own Swing plot)
    title_green: bool,          // window title bar currently painted green (futures feed live)
}

impl App {
    fn new(cc: &eframe::CreationContext<'_>, sym: String, tf: String, base: String, tok: String) -> Self {
        setup_fonts(&cc.egui_ctx);
        let mut v = egui::Visuals::dark();
        v.panel_fill = BG;
        v.window_fill = BG;
        cc.egui_ctx.set_visuals(v);
        let shared = Arc::new(Mutex::new(Shared::default()));
        let (tx, rx) = mpsc::channel();
        let has_token = !tok.is_empty();
        {
            let (sh, ctx) = (shared.clone(), cc.egui_ctx.clone());
            let (tx2, ctx2, sh2) = (tx.clone(), ctx.clone(), sh.clone());
            std::thread::spawn(move || nt_listener(sh2, tx2, ctx2));
            let (sh4, ctx4) = (sh.clone(), ctx.clone());
            std::thread::spawn(move || order_poll(sh4, ctx4));
            std::thread::spawn(order_stream);
            let tx3 = tx.clone();
            std::thread::spawn(move || worker(rx, tx3, sh, ctx, base, tok));
        }
        let (link_tx, out_rx) = mpsc::channel();
        let (in_tx, link_rx) = mpsc::channel();
        {
            let ctx = cc.egui_ctx.clone();
            std::thread::spawn(move || link_thread(out_rx, in_tx, ctx));
        }
        let link_id = format!("{}-{}", std::process::id(), SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.subsec_nanos()).unwrap_or(0));
        let app = App {
            link_tx,
            link_rx,
            link_id,
            link_g: None,
            link_peers: HashMap::new(),
            link_last: None,
            link_sent: std::time::Instant::now(),
            link_xh: None,
            link_xh_sent: false,
            bar_over: false,
            sym,
            tf,
            shared,
            tx,
            spacing: 13.0,
            right_idx: 0.0,
            seen_version: 0,
            has_token,
            intraday: false,
            show_fvg: true,
            show_htf: true,
            htf_i: 1,
            hide_nums: false,
            bar_x: 0.0,
            note: String::new(),
            show_pw: false,
            show_pm: true,
            show_pq: true,
            show_py: true,
            manual: None,
            drag_mode: 0,
            ord_drag: None,
            ord_pend: None,
            close_sent: None,
            rev_sent: None,
            meas_p0: None,
            meas: None,
            meas_w: 0.0,
            vdrag: 0.0,
            fling: 0.0,
            zoom_t: None,
            xh_last: None,
            xh_off: false,
            show_wk_d: false,
            show_mo_w: false,
            swing_ext: false,
            title_green: false,
        };
        app.reload();
        app
    }

    fn reload(&self) {
        let _ = self.tx.send(Req::Load { sym: self.sym.clone(), tf: self.tf.clone(), htf: HTFS[self.htf_i].0.to_string() });
        let _ = self.tx.send(Req::Htf { sym: self.sym.clone(), tf: HTFS[self.htf_i].0.to_string() });
    }
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        // the title bar turns green while the futures (NinjaTrader) feed is the one on screen
        let nt_on = self.shared.lock().unwrap().src == "NT";
        if nt_on != self.title_green {
            self.title_green = nt_on;
            set_title_green(frame, nt_on);
        }
        {
            let mut s = self.shared.lock().unwrap();
            if s.ord_sym != self.sym {
                s.ord_sym = self.sym.clone(); // the order poller follows the symbol on screen
                s.orders.clear();
                s.entries.clear();
            }
        }
        // ── toolbar ──
        egui::Panel::top("bar").show(ui, |ui| {
            let avail = ui.available_width();
            ui.horizontal(|ui| {
                let over = self.bar_over;
                if over && ui.small_button("‹").clicked() {
                    self.bar_x = (self.bar_x - 300.0).max(0.0);
                }
                if over && ui.rect_contains_pointer(ui.max_rect()) {
                    let d = ui.input(|i| i.smooth_scroll_delta.y);
                    if d != 0.0 {
                        self.bar_x = (self.bar_x - d).max(0.0); // plain mouse wheel over the toolbar scrolls it sideways
                    }
                }
                let out = egui::ScrollArea::horizontal()
                    .id_salt("barscroll")
                    .max_width((avail - if over { 60.0 } else { 14.0 } - if self.link_g.is_some() || !self.link_peers.is_empty() { 52.0 } else { 0.0 }).max(100.0))
                    .scroll_bar_visibility(egui::scroll_area::ScrollBarVisibility::AlwaysHidden)
                    .horizontal_scroll_offset(self.bar_x)
                    .show(ui, |ui| {
                ui.horizontal(|ui| {
                let mut changed = false;
                // FUT | CFD: shown only while NinjaTrader is feeding this symbol
                if self.shared.lock().unwrap().nt_conns.get(&self.sym).copied().unwrap_or(0) > 0 {
                    let is_fut = !self.shared.lock().unwrap().force_cfd || inst_for(&self.sym).is_empty();
                    let fut = egui::RichText::new("FUT").strong().color(if is_fut { Color32::from_rgb(0, 230, 118) } else { Color32::from_rgb(150, 165, 195) });
                    let cfd = egui::RichText::new("CFD").strong().color(if !is_fut { Color32::from_rgb(77, 163, 255) } else { Color32::from_rgb(150, 165, 195) });
                    if ui.selectable_label(is_fut, fut).clicked() && !is_fut {
                        self.shared.lock().unwrap().force_cfd = false;
                        changed = true;
                    }
                    if ui.selectable_label(!is_fut, cfd).clicked() && is_fut {
                        if inst_for(&self.sym).is_empty() {
                            self.shared.lock().unwrap().toast = Some((format!("{} has no CFD feed", self.sym), false, std::time::Instant::now()));
                        } else {
                            self.shared.lock().unwrap().force_cfd = true;
                            changed = true;
                        }
                    }
                    ui.separator();
                }
                for (s, inst) in SYMS.iter() {
                    // NinjaTrader-only symbols appear while their feed is connected (or when already selected)
                    if inst.is_empty() && self.sym != *s && self.shared.lock().unwrap().nt_conns.get(*s).copied().unwrap_or(0) == 0 {
                        continue;
                    }
                    if ui.selectable_label(self.sym == *s, *s).clicked() && self.sym != *s {
                        self.sym = s.to_string();
                        changed = true;
                    }
                }
                ui.separator();
                for (t, _, _) in TFS.iter() {
                    if ui.selectable_label(self.tf == *t, *t).clicked() && self.tf != *t {
                        self.tf = t.to_string();
                        changed = true;
                    }
                }
                if !INTRA_TFS.contains(&self.tf.as_str()) {
                    self.intraday = false;
                }
                ui.separator();
                if INTRA_TFS.contains(&self.tf.as_str()) && ui.selectable_label(self.intraday, "INTRADAY").clicked() {
                    self.intraday = !self.intraday;
                    self.seen_version = u64::MAX; // re-apply the default zoom for the new mode
                }
                if ui.selectable_label(self.show_fvg, "FVG").clicked() {
                    self.show_fvg = !self.show_fvg;
                }
                if self.tf == "400R" {
                    let on = self.swing_ext;
                    let ext_txt = egui::RichText::new("EXTEND").strong().color(if on { Color32::from_rgb(26, 18, 0) } else { Color32::from_rgb(150, 165, 195) });
                    let resp = if on {
                        ui.add(egui::Button::new(ext_txt).fill(Color32::from_rgb(255, 176, 32)))
                    } else {
                        ui.selectable_label(false, ext_txt)
                    };
                    if resp.on_hover_text("Big swing lines keep running until price hits them (off: only the latest swing line, like NinjaTrader)").clicked() {
                        self.swing_ext = !self.swing_ext;
                    }
                }
                if self.intraday {
                    if ui.selectable_label(self.show_htf, "HTF").clicked() {
                        self.show_htf = !self.show_htf;
                    }
                    if self.show_htf {
                        for (i, h) in HTFS.iter().enumerate() {
                            if ui.selectable_label(self.htf_i == i, h.0).clicked() && self.htf_i != i {
                                self.htf_i = i;
                                let _ = self.tx.send(Req::Htf { sym: self.sym.clone(), tf: h.0.to_string() });
                            }
                        }
                    }
                }
                if !self.intraday {
                    let (lbl, flag) = match self.tf.as_str() {
                        "30m" => ("PW", Some(&mut self.show_pw)),
                        "8h" => ("PM", Some(&mut self.show_pm)),
                        "D" => ("PQ", Some(&mut self.show_pq)),
                        "W" => ("PY", Some(&mut self.show_py)),
                        _ => ("", None),
                    };
                    if let Some(f) = flag {
                        if ui.selectable_label(*f, lbl).clicked() {
                            *f = !*f;
                        }
                    }
                }
                if !self.intraday {
                    let (lbl, flag) = match self.tf.as_str() {
                        "D" => ("WEEK", Some(&mut self.show_wk_d)),
                        "W" => ("MONTH", Some(&mut self.show_mo_w)),
                        _ => ("", None),
                    };
                    if let Some(f) = flag {
                        if ui.selectable_label(*f, lbl).on_hover_text(if lbl == "WEEK" { "Line at the start of each week" } else { "Line at the start of each month" }).clicked() {
                            *f = !*f;
                        }
                    }
                }
                if self.sym == "NQ" && ui.selectable_label(!self.hide_nums, "#").clicked() {
                    self.hide_nums = !self.hide_nums;
                }
                if self.shared.lock().unwrap().src == "NT" {
                    ui.colored_label(Color32::from_rgb(0, 230, 118), "NT live");
                }
                if changed {
                    self.reload();
                }
                if changed {
                    self.note.clear();
                }
                if self.shared.lock().unwrap().link_state == 2 {
                    ui.separator();
                    ui.colored_label(Color32::from_rgb(255, 191, 0), "futures: first chart isn't sharing NinjaTrader");
                }
                let st = self.shared.lock().unwrap().status.clone();
                let st = if st.is_empty() { self.note.clone() } else { st };
                if !st.is_empty() {
                    ui.separator();
                    ui.colored_label(Color32::from_rgb(255, 191, 0), st);
                }
                });
                });
                self.bar_x = out.state.offset().x;
                let max_x = (out.content_size.x - out.inner_rect.width()).max(0.0);
                self.bar_over = max_x > 2.0 || (over && out.content_size.x + 60.0 > avail);
                if over && ui.small_button("›").clicked() {
                    self.bar_x = (self.bar_x + 300.0).min(max_x);
                }
                {
                    let now = std::time::Instant::now();
                    self.link_peers.retain(|_, t| now.duration_since(*t) < Duration::from_secs(6));
                    if self.link_g.is_some() || !self.link_peers.is_empty() {
                        ui.separator();
                        let col = self.link_g.map(|g| LINK_COLORS[g]).map(|(r, g, b)| Color32::from_rgb(r, g, b));
                        let txt = egui::RichText::new("LINK").color(col.unwrap_or(Color32::from_rgb(140, 150, 170)));
                        if ui.selectable_label(col.is_some(), txt).on_hover_text("Link charts: windows with the same colour share zoom, scroll and crosshair").clicked() {
                            self.link_g = match self.link_g { None => Some(0), Some(g) if g + 1 < LINK_COLORS.len() => Some(g + 1), _ => None };
                            self.link_last = None;
                            self.link_xh = None;
                        }
                    }
                }
            });
        });

        let bg = BG;
        let grid_col = GRID;
        egui::CentralPanel::default().frame(egui::Frame::NONE.fill(bg)).show(ui, |ui| {
            let full = ui.available_rect_before_wrap();
            let src_nt = self.shared.lock().unwrap().src == "NT";
            if !self.has_token && !src_nt {
                ui.painter().text(
                    full.center(),
                    Align2::CENTER_CENTER,
                    "No OANDA token received.\nOpen this chart from the GPU button in EDGE (it passes your saved token).",
                    FontId::proportional(16.0),
                    AXIS_TXT,
                );
                return;
            }
            let (resp, painter) = ui.allocate_painter(full.size(), Sense::click_and_drag());
            let ppp = ctx.pixels_per_point();
            let snapc = |v: f32| ((v * ppp).floor() + 0.5) / ppp; // centre of a physical pixel: 1px lines stay crisp
            let hair = 1.0 / ppp;
            let plot = Rect::from_min_max(full.min, Pos2::new(full.max.x - AXIS_W, full.max.y - AXIS_H));

            let (cs, ds, m5, hb, ver) = {
                let s = self.shared.lock().unwrap();
                (s.main.clone(), s.daily.clone(), s.m5.clone(), s.htf.clone(), s.version)
            };
            if cs.is_empty() {
                painter.text(plot.center(), Align2::CENTER_CENTER, "loading…", FontId::proportional(16.0), AXIS_TXT);
                return;
            }
            let n = cs.len();
            // INTRADAY mode opens close up on the latest candles, like the web chart (desktop counts), with a ~150px gap for the price tags
            let default_view = |tf: &str, intra: bool, sym: &str| -> (f32, f32) {
                if intra {
                    let base = match tf { "2m" => 54.0f32, "5m" => 43.0, "30m" => 30.0, "1h" => 32.0, _ => 0.0 }; // exactly the web chart's intraday counts
                    if base > 0.0 {
                        let bars = if sym == "BTC" { (base * 0.7).round() } else { base };
                        let sp = ((plot.width() - 150.0) / bars).clamp(3.0, 140.0);
                        return (sp, 150.0 / sp);
                    }
                }
                // same counts as the web chart on desktop (its bars-per-timeframe x 0.45; BTC x 0.7), ~58px kept free on the right
                let base = match tf { "2m" => 112.0f32, "5m" => 90.0, "30m" => 112.0, "1h" => 135.0, "4h" => 90.0, "8h" => 68.0, "D" => 68.0, "W" => 34.0, _ => 112.0 };
                let bars = if sym == "BTC" { (base * 0.7).round() } else { base };
                let sp = ((plot.width() - 58.0) / bars).clamp(3.0, 140.0);
                (sp, 58.0 / sp)
            };
            let psecs = TFS.iter().find(|t| t.0 == self.tf).map(|t| t.2).unwrap_or(300) as f64;
            // time <-> fractional bar index through this chart's own bars (extrapolated past both ends)
            let t_to_l = |t: f64| -> f64 {
                let (f, l) = (cs[0].t as f64, cs[n - 1].t as f64);
                if t <= f { return (t - f) / psecs; }
                if t >= l { return (n - 1) as f64 + (t - l) / psecs; }
                let i = cs.partition_point(|c| (c.t as f64) <= t).clamp(1, n - 1);
                let (a, b) = (cs[i - 1].t as f64, cs[i].t as f64);
                (i - 1) as f64 + if b > a { (t - a) / (b - a) } else { 0.0 }
            };
            let l_to_t = |l: f64| -> f64 {
                if l <= 0.0 { return cs[0].t as f64 + l * psecs; }
                if l >= (n - 1) as f64 { return cs[n - 1].t as f64 + (l - (n - 1) as f64) * psecs; }
                let i = l.floor() as usize;
                cs[i].t as f64 + (l - i as f64) * (cs[i + 1].t - cs[i].t) as f64
            };
            let mut apply_range = |app: &mut App, from: i64, to: i64| {
                let (lf, lt) = (t_to_l(from as f64), t_to_l(to as f64));
                if lt > lf + 0.5 {
                    app.zoom_t = None;
                    app.fling = 0.0;
                    app.spacing = ((plot.width() as f64 / (lt - lf)) as f32).clamp(3.0, 140.0);
                    app.right_idx = lt as f32;
                }
            };
            if ver != self.seen_version {
                self.seen_version = ver;
                self.manual = None;
                self.zoom_t = None;
                self.fling = 0.0;
                let (sp0, gap0) = default_view(&self.tf, self.intraday, &self.sym);
                self.spacing = sp0;
                self.right_idx = (n - 1) as f32 + gap0;
                if let (Some(_), Some((f, t))) = (self.link_g, self.link_last) {
                    apply_range(self, f, t); // reload / timeframe switch keeps the shared view
                }
            }
            // incoming link messages
            let mut got_range: Option<(i64, i64)> = None;
            while let Ok(line) = self.link_rx.try_recv() {
                let Ok(v) = serde_json::from_str::<serde_json::Value>(&line) else { continue };
                let id = v["id"].as_str().unwrap_or("").to_string();
                if id == self.link_id { continue; }
                match v["t"].as_str().unwrap_or("") {
                    "alive" => { self.link_peers.insert(id, std::time::Instant::now()); }
                    "bye" => { self.link_peers.remove(&id); }
                    t => {
                        if self.link_g.is_none() || v["g"].as_u64().map(|g| g as usize) != self.link_g { continue; }
                        match t {
                            "range" => {
                                if let (Some(f), Some(to)) = (v["from"].as_i64(), v["to"].as_i64()) { got_range = Some((f, to)); }
                            }
                            "xh" => {
                                if let (Some(tm), Some(pr)) = (v["time"].as_i64(), v["price"].as_f64()) { self.link_xh = Some((tm, pr, std::time::Instant::now())); }
                            }
                            "xhc" => self.link_xh = None,
                            "xhoff" => {
                                self.xh_off = v["off"].as_bool() == Some(true);
                                self.link_xh = None;
                            }
                            "hello" => { self.link_sent = std::time::Instant::now() - Duration::from_secs(1); self.link_last = None; got_range = None; }
                            _ => {}
                        }
                    }
                }
            }
            if let Some((f, t)) = got_range {
                self.link_last = Some((f, t));
                apply_range(self, f, t);
            }
            if self.link_xh.map_or(false, |x| x.2.elapsed() > Duration::from_secs(2)) { self.link_xh = None; }
            {
                // heartbeat so the ⛓ button shows up when another chart window exists
                static LAST_ALIVE: Mutex<Option<std::time::Instant>> = Mutex::new(None);
                let mut la = LAST_ALIVE.lock().unwrap();
                if la.map_or(true, |t| t.elapsed() > Duration::from_secs(2)) {
                    *la = Some(std::time::Instant::now());
                    let _ = self.link_tx.send(format!("{{\"id\":\"{}\",\"t\":\"alive\"}}", self.link_id));
                }
                ctx.request_repaint_after(Duration::from_secs(2));
            }
            let (pre_sp, pre_ri) = (self.spacing, self.right_idx);

            // ── interaction ──
            if resp.drag_started() {
                let o = ctx.input(|i| i.pointer.press_origin());
                let shift = ctx.input(|i| i.modifiers.shift);
                self.drag_mode = if o.map_or(false, |p| p.x > plot.right()) { 2 } else if shift { 3 } else { 1 };
                self.meas = None;
                self.meas_w = 0.0;
                self.meas_p0 = if self.drag_mode == 3 { o } else { None };
                self.vdrag = 0.0;
                self.fling = 0.0;
                self.zoom_t = None;
            }
            // the measure stays on screen after you let go; a click or Esc clears it
            if resp.drag_stopped() {
                if self.drag_mode == 1 {
                    let v = ctx.input(|i| i.pointer.velocity().x);
                    let f = (-v / self.spacing).clamp(-600.0, 600.0);
                    self.fling = if f.abs() > 3.0 { f } else { 0.0 };
                }
                self.meas_p0 = None;
                if self.drag_mode == 3 {
                    self.drag_mode = 0;
                    if let Some((i0, p0, i1, p1)) = self.meas { if i0 == i1 && p0 == p1 { self.meas = None; } }
                }
            }
            if resp.clicked() && self.meas.is_none() {
                // a plain click on the chart hides / shows the crosshair lines (a double click flips twice = no change)
                if ctx.input(|i| i.pointer.interact_pos()).map_or(false, |p| p.x <= plot.right()) {
                    self.xh_off = !self.xh_off;
                    if let Some(g) = self.link_g {
                        let _ = self.link_tx.send(format!("{{\"id\":\"{}\",\"g\":{},\"t\":\"xhoff\",\"off\":{}}}", self.link_id, g, self.xh_off));
                    }
                }
            }
            if resp.clicked() || ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
                self.fling = 0.0;
                self.meas = None;
                self.meas_p0 = None;
            }
            if resp.dragged() && self.drag_mode == 1 {
                self.right_idx -= resp.drag_delta().x / self.spacing;
            }
            if resp.double_clicked() {
                let on_axis = ctx.input(|i| i.pointer.hover_pos()).map_or(false, |p| p.x > plot.right());
                self.manual = None; // back to auto-fit
                self.zoom_t = None;
                self.fling = 0.0;
                if !on_axis {
                    let (sp0, gap0) = default_view(&self.tf, self.intraday, &self.sym);
                    self.spacing = sp0;
                    self.right_idx = (n - 1) as f32 + gap0;
                }
            }
            {
                let (l, r, sh) = ctx.input(|i| (i.key_pressed(egui::Key::ArrowLeft), i.key_pressed(egui::Key::ArrowRight), i.modifiers.shift));
                let k = if sh { 5.0 } else { 1.0 };
                if l {
                    self.right_idx -= k;
                }
                if r {
                    self.right_idx += k;
                }
            }
            if self.fling != 0.0 && !resp.dragged() {
                if self.fling.abs() > 0.5 {
                    let dt = ctx.input(|i| i.stable_dt).min(0.05);
                    self.right_idx += self.fling * dt;
                    self.fling *= (-dt * 3.5).exp();
                    ctx.request_repaint();
                } else {
                    self.fling = 0.0;
                }
            }
            let mut axis_scroll = 0.0f32;
            if resp.hovered() {
                let scroll = ctx.input(|i| i.smooth_scroll_delta.y);
                let over_axis = ctx.input(|i| i.pointer.hover_pos()).map_or(false, |p| p.x > plot.right());
                if scroll != 0.0 && over_axis {
                    axis_scroll = scroll;
                } else if scroll != 0.0 {
                    let anchor = ctx.input(|i| i.pointer.hover_pos()).map(|p| p.x).unwrap_or(plot.right());
                    self.fling = 0.0;
                    let cur = self.zoom_t.map_or(self.spacing, |t| t.0);
                    self.zoom_t = Some(((cur * (scroll * 0.0025).exp()).clamp(3.0, 140.0), anchor));
                }
            }
            if let Some((tgt, anchor)) = self.zoom_t {
                let dt = ctx.input(|i| i.stable_dt).min(0.05);
                let k = 1.0 - (-dt * 22.0).exp();
                let idx_at = self.right_idx - (plot.right() - anchor) / self.spacing;
                let ns = self.spacing + (tgt - self.spacing) * k;
                self.spacing = if (tgt - ns).abs() < 0.02 { tgt } else { ns };
                self.right_idx = idx_at + (plot.right() - anchor) / self.spacing;
                if self.spacing == tgt {
                    self.zoom_t = None;
                } else {
                    ctx.request_repaint();
                }
            }
            self.right_idx = self.right_idx.clamp(2.0f32.min((n - 1) as f32), (n - 1) as f32 + (plot.width() / self.spacing - 2.0).max(RIGHT_GAP_BARS)); // keep at least ~2 bars on screen, like the web chart
            // a view change made HERE (drag, wheel, arrows, double-click) goes out to linked windows
            if let Some(g) = self.link_g {
                if (self.spacing - pre_sp).abs() > 1e-4 || (self.right_idx - pre_ri).abs() > 1e-4 {
                    if got_range.is_none() {
                        let (a, b) = (l_to_t((self.right_idx - plot.width() / self.spacing) as f64) as i64, l_to_t(self.right_idx as f64) as i64);
                        self.link_last = Some((a, b));
                        self.link_sent = std::time::Instant::now();
                        let _ = self.link_tx.send(format!("{{\"id\":\"{}\",\"g\":{},\"t\":\"range\",\"from\":{},\"to\":{}}}", self.link_id, g, a, b));
                    }
                }
            }
            let (sp, ri) = (self.spacing, self.right_idx);
            let x_of = move |i: f32| plot.right() - (ri - i) * sp;
            let first_vis = ((ri - (plot.width() / sp)).floor() as i64 - 1).max(0) as usize;
            let last_vis = ((ri).ceil() as i64 + 1).min(n as i64 - 1).max(0) as usize;

            // ── price range ──
            let vis = &cs[first_vis.min(n - 1)..=last_vis.max(first_vis).min(n - 1)];
            let mut lo = vis.iter().map(|c| c.l).fold(f64::MAX, f64::min);
            let mut hi = vis.iter().map(|c| c.h).fold(f64::MIN, f64::max);
            if !(lo.is_finite() && hi.is_finite()) || hi <= lo {
                lo = cs[n - 1].l - 1.0;
                hi = cs[n - 1].h + 1.0;
            }
            let pad = (hi - lo) * 0.08;
            lo -= pad;
            hi += pad;
            if let Some((ml, mh)) = self.manual {
                lo = ml;
                hi = mh;
            }
            // price scale: drag the axis (or wheel over it) to stretch the range, drag the chart vertically to shift it
            if resp.dragged() {
                let dy = resp.drag_delta().y;
                if self.drag_mode == 2 {
                    let c = (lo + hi) / 2.0;
                    let half = ((hi - lo) / 2.0 * (dy as f64 * 0.006).exp()).max(1e-6);
                    lo = c - half;
                    hi = c + half;
                    self.manual = Some((lo, hi));
                } // dragging the chart body only moves time (like the web chart); price is moved/stretched from the price axis
            }
            if self.manual.is_some() {
                let c = (lo + hi) / 2.0;
                let half = ((hi - lo) / 2.0).clamp(1e-4, 1e7);
                lo = c - half;
                hi = c + half;
                self.manual = Some((lo, hi));
            }
            if axis_scroll != 0.0 {
                let c = (lo + hi) / 2.0;
                let half = ((hi - lo) / 2.0 * (-(axis_scroll as f64) * 0.0025).exp()).max(1e-6);
                lo = c - half;
                hi = c + half;
                self.manual = Some((lo, hi));
            }
            let y_of = move |p: f64| plot.bottom() - ((p - lo) / (hi - lo)) as f32 * plot.height();
            let p_of = move |y: f32| lo + ((plot.bottom() - y) / plot.height()) as f64 * (hi - lo);

            // ── grid + price axis ──
            let step = nice_step((hi - lo) / (plot.height() as f64 / 70.0).max(2.0));
            let mut p = (lo / step).ceil() * step;
            let axis_font = FontId::monospace(13.0);
            while p < hi {
                let y = snapc(y_of(p));
                painter.text(Pos2::new(plot.right() + 8.0, y), Align2::LEFT_CENTER, fmt_price(p, step), axis_font.clone(), AXIS_TXT);
                p += step;
            }

            // ── time axis ──
            let secs = TFS.iter().find(|t| t.0 == self.tf).map(|t| t.2).unwrap_or(300);
            let every = ((95.0 / sp).ceil() as usize).max(1);
            let mut i = (first_vis / every) * every;
            while i <= last_vis + every {
                if i < n {
                    let x = snapc(x_of(i as f32));
                    if x > plot.left() && x < plot.right() {
                        let d = et(cs[i].t);
                        let label = if secs >= 86400 {
                            format!("{} {}", d.format("%b"), d.day())
                        } else if d.hour() == 0 && d.minute() == 0 {
                            format!("{} {}", d.format("%a"), d.day())
                        } else {
                            d.format("%H:%M").to_string()
                        };
                        painter.text(Pos2::new(x, plot.bottom() + 6.0), Align2::CENTER_TOP, label, axis_font.clone(), AXIS_TXT);
                    }
                }
                i += every;
            }
            painter.rect_stroke(plot, 0.0, Stroke::new(hair, grid_col), egui::StrokeKind::Inside);
            let clip = painter.with_clip_rect(plot);
            let small = FontId::monospace(10.0);
            let tiny = FontId::monospace(9.0);
            clip.text(plot.center(), Align2::CENTER_CENTER, &self.sym, FontId::monospace(140.0), Color32::from_rgba_unmultiplied(255, 255, 255, 18));

            let align = align_for(&self.sym);
            let anchor = (24 - align) * 3600;
            let tf = self.tf.as_str();
            let intraday = self.intraday;
            let is_range = tf == "400R";

            // ═════════ session lines: YEAR / MONTH / QUARTER / WEEK / ETH ═════════
            let mut marks: Vec<(usize, &str, u32)> = vec![];
            {
                let mut add = |i: usize, l: &'static str, c: u32, m: &mut Vec<(usize, &str, u32)>| {
                    if !m.iter().any(|x| x.0 == i) {
                        m.push((i, l, c));
                    }
                };
                let (mut yb, mut mb, mut qb) = (vec![], vec![], vec![]);
                if tf == "W" {
                    let wq = |e: i64| {
                        let (y, m, _) = ymd(e + anchor + 2 * 86400);
                        y * 4 + m / 3
                    };
                    for i in 1..n {
                        let (a, b) = (wq(cs[i].e), wq(cs[i - 1].e));
                        if a != b {
                            if a.div_euclid(4) != b.div_euclid(4) { yb.push(i) } else { qb.push(i) }
                        }
                    }
                } else {
                    for i in 1..n {
                        let (ya, ma, _) = ymd(cs[i].e + anchor);
                        let (yp, mp, _) = ymd(cs[i - 1].e + anchor);
                        if ma != mp || ya != yp {
                            if tf == "D" && ya != yp {
                                yb.push(i)
                            } else if tf == "D" && ma % 3 == 0 {
                                qb.push(i)
                            } else {
                                mb.push(i)
                            }
                        }
                    }
                }
                for &i in &yb { add(i, "YEAR", 0xe8f0ff, &mut marks); }
                for &i in &mb { add(i, "MONTH", 0xb06aff, &mut marks); }
                for &i in &qb { add(i, "QUARTER", 0xffc83d, &mut marks); }
                if tf != "D" && secs < 604800 {
                    for i in 1..n {
                        if cs[i].t - cs[i - 1].t > 2 * 86400 { add(i, "WEEK", 0x2ec4b6, &mut marks); }
                    }
                }
                // optional (buttons): D gets a line at the start of each week, W at the start of each month (same colours as the lines above)
                if tf == "D" && self.show_wk_d {
                    for i in 1..n {
                        if key_of(Per::Week, cs[i].e, anchor) != key_of(Per::Week, cs[i - 1].e, anchor) { add(i, "WEEK", 0x2ec4b6, &mut marks); }
                    }
                }
                if tf == "W" && self.show_mo_w {
                    let mk = |e: i64| {
                        let (y, m, _) = ymd(e + anchor + 2 * 86400);
                        y * 12 + m
                    };
                    for i in 1..n {
                        if mk(cs[i].e) != mk(cs[i - 1].e) { add(i, "MONTH", 0xb06aff, &mut marks); }
                    }
                }
                if secs < 3600 && FUTURES.contains(&self.sym.as_str()) {
                    for i in 1..n {
                        let (tod, ptod) = (cs[i].e.rem_euclid(86400), cs[i - 1].e.rem_euclid(86400));
                        if tod >= 18 * 3600 && (ptod < 18 * 3600 || cs[i].e.div_euclid(86400) != cs[i - 1].e.div_euclid(86400)) {
                            add(i, "ETH", 0xffb020, &mut marks);
                        }
                    }
                }
            }
            // day groups (trading days; boundary = session anchor)
            let mut dg: Vec<(usize, usize, i64)> = vec![];
            if secs <= 1800 {
                for i in 0..n {
                    let dk = (cs[i].e + anchor).div_euclid(86400);
                    match dg.last_mut() {
                        Some(g) if g.2 == dk => g.1 = i,
                        _ => dg.push((i, i, dk)),
                    }
                }
            }
            let now_dk = dg.last().map(|g| g.2).unwrap_or(0);
            // 30m: shade completed Wednesdays
            if tf == "30m" {
                for g in dg.iter().filter(|g| weekday(g.2) == 3 && g.2 != now_dk) {
                    let (x0, x1) = (x_of(g.0 as f32), x_of(g.1 as f32) + sp);
                    if x1 < plot.left() || x0 > plot.right() {
                        continue;
                    }
                    let (bx0, bx1) = (x0.max(plot.left()), x1.min(plot.right()));
                    clip.rect_filled(Rect::from_min_max(Pos2::new(bx0, plot.top()), Pos2::new(bx1, plot.bottom())), 0.0, hexc(0xffb020, 0.07));
                    let w = x1 - x0;
                    let txt = if w >= 26.0 { "WED" } else if w >= 9.0 { "W" } else { "" };
                    if !txt.is_empty() {
                        clip.text(Pos2::new(bx0 + 4.0, plot.bottom() - 3.0), Align2::LEFT_BOTTOM, txt, tiny.clone(), hexc(0xffb020, 0.9));
                    }
                }
            }
            // vertical lines + labels
            {
                let vis_marks: Vec<(f32, &str, u32)> = marks
                    .iter()
                    .filter_map(|m| {
                        let x = snapc(x_of(m.0 as f32));
                        if x < plot.left() || x > plot.right() { None } else { Some((x, m.1, m.2)) }
                    })
                    .collect();
                for (x, l, c) in &vis_marks {
                    dash_v(&clip, *x, plot.top(), plot.bottom(), 4.0, 4.0, Stroke::new(hair, hexc(*c, if *l == "ETH" { 0.3 } else { 0.6 })));
                }
                let obst: Vec<f32> = vis_marks.iter().filter(|m| m.1 != "ETH").map(|m| m.0).collect();
                let mut rows: Vec<Vec<(f32, f32)>> = vec![];
                for (x, l, c) in vis_marks.iter().filter(|m| m.1 != "ETH") {
                    let mut pick: Option<(String, f32, f32)> = None;
                    for txt in [l.to_string(), l.chars().next().unwrap().to_string()] {
                        let lw = text_w(&painter, &txt, &tiny);
                        for (l0, l1) in [(x + 4.0, x + 4.0 + lw), (x - 4.0 - lw, x - 4.0)] {
                            if pick.is_some() || l0 < plot.left() || l1 > plot.right() {
                                continue;
                            }
                            if obst.iter().any(|ox| ox != x && *ox > l0 - 3.0 && *ox < l1 + 3.0) {
                                continue;
                            }
                            pick = Some((txt.clone(), l0, l1));
                        }
                    }
                    let (txt, l0, l1) = pick.unwrap_or_else(|| (l.chars().next().unwrap().to_string(), x + 4.0, x + 11.0));
                    let mut r = 0;
                    while rows.get(r).map_or(false, |row| row.iter().any(|s| l0 < s.1 + 3.0 && l1 > s.0 - 3.0)) {
                        r += 1;
                    }
                    if rows.len() <= r {
                        rows.push(vec![]);
                    }
                    rows[r].push((l0, l1));
                    clip.text(Pos2::new(l0, plot.top() + 2.0 + r as f32 * 12.0), Align2::LEFT_TOP, txt, tiny.clone(), rgb(*c));
                }
            }
            // weekday names for the current week (<=30m)
            if secs <= 1800 && !dg.is_empty() {
                let wk0 = marks.iter().filter(|m| m.1 == "WEEK").map(|m| m.0).max().unwrap_or(0);
                for g in dg.iter().filter(|g| g.1 >= wk0) {
                    let name = ["SUN", "MON", "TUE", "WED", "THU", "FRI", "SAT"][weekday(g.2) as usize];
                    if tf == "30m" && name == "WED" && g.2 != now_dk {
                        continue;
                    }
                    let (x0, x1) = (x_of(g.0 as f32), x_of(g.1 as f32));
                    if x1 < plot.left() || x0 > plot.right() || x1 - x0 < 26.0 {
                        continue;
                    }
                    clip.text(Pos2::new(x0.max(plot.left()) + 4.0, plot.bottom() - 3.0), Align2::LEFT_BOTTOM, name, tiny.clone(), hexc(0xffb020, 0.85));
                }
            }

            // ═════════ RTH shading (<=30m) ═════════
            let weekday_of = |e: i64| weekday(e.div_euclid(86400));
            // <=30m: the regular trading session (9:30-16:00 ET) as a faint blue band, in place of the old 9:30 line
            if secs <= 1800 {
                let mut run: Option<usize> = None;
                let mut bands: Vec<(usize, usize)> = vec![];
                for i in first_vis..=last_vis.min(n - 1) {
                    let sod = cs[i].e.rem_euclid(86400);
                    let dk = (cs[i].e + anchor).div_euclid(86400);
                    let wed_done = tf == "30m" && weekday(dk) == 3 && dk != now_dk; // completed Wednesdays are amber: just a line at the open there
                    if wed_done && sod == 34200 {
                        dash_v(&clip, snapc(x_of(i as f32) - sp / 2.0), plot.top(), plot.bottom(), 2.0, 4.0, Stroke::new(hair, hexc(0x4aa3ff, 0.9)));
                    }
                    let rth = sod >= 34200 && sod < 57600 && !wed_done && !matches!(weekday_of(cs[i].e), 0 | 6);
                    match (rth, run) {
                        (true, None) => run = Some(i),
                        (false, Some(s0)) => {
                            bands.push((s0, i - 1));
                            run = None;
                        }
                        _ => {}
                    }
                }
                if let Some(s0) = run {
                    bands.push((s0, last_vis.min(n - 1)));
                }
                for (i0, i1) in bands {
                    let (x0, x1) = (x_of(i0 as f32) - sp / 2.0, x_of(i1 as f32) + sp / 2.0);
                    clip.rect_filled(Rect::from_min_max(Pos2::new(x0.max(plot.left()), plot.top()), Pos2::new(x1.min(plot.right()), plot.bottom())), 0.0, hexc(0x4aa3ff, 0.07));
                }
            }
            // 1h: every second trading day gets a faint band, with the weekday name and a dashed line at each day start
            if tf == "1h" {
                let mut days: Vec<(usize, usize, i64)> = vec![];
                for i in 0..n {
                    let dk = (cs[i].e + anchor).div_euclid(86400);
                    match days.last_mut() {
                        Some(g) if g.2 == dk => g.1 = i,
                        _ => days.push((i, i, dk)),
                    }
                }
                for (gi, g) in days.iter().enumerate() {
                    let (x0, x1) = (x_of(g.0 as f32) - sp / 2.0, x_of(g.1 as f32) + sp / 2.0);
                    if x1 < plot.left() || x0 > plot.right() {
                        continue;
                    }
                    if gi % 2 == 1 {
                        clip.rect_filled(Rect::from_min_max(Pos2::new(x0.max(plot.left()), plot.top()), Pos2::new(x1.min(plot.right()), plot.bottom())), 0.0, hexc(0x6b8cff, 0.07));
                    }
                    if gi > 0 && x0 > plot.left() {
                        dash_v(&clip, snapc(x0), plot.top(), plot.bottom(), 4.0, 4.0, Stroke::new(hair, hexc(0xffb020, 0.3)));
                    }
                    if x1 - x0 >= 26.0 {
                        let name = ["SUN", "MON", "TUE", "WED", "THU", "FRI", "SAT"][weekday(g.2) as usize];
                        clip.text(Pos2::new(x0.max(plot.left()) + 4.0, plot.bottom() - 3.0), Align2::LEFT_BOTTOM, name, tiny.clone(), hexc(0xffb020, 0.85));
                    }
                }
            }

            // ═════════ FVG (3-candle) ═════════
            if self.show_fvg {
                let (zones, done) = fvg(&cs);
                for z in &done {
                    let (x0, x1) = (x_of(z.i0 as f32), x_of(z.i1 as f32));
                    let (y1, y2) = (y_of(z.hi), y_of(z.lo));
                    if x1 < plot.left() || x0 > plot.right() {
                        continue;
                    }
                    let x0 = x0.max(plot.left());
                    clip.rect_filled(Rect::from_min_max(Pos2::new(x0, y1), Pos2::new(x1, y2)), 0.0, hexc(0xb4b2a9, 0.2));
                    let e = Stroke::new(hair, hexc(0xb4b2a9, 0.8));
                    clip.hline(x0..=x1, snapc(y1), e);
                    clip.hline(x0..=x1, snapc(y2), e);
                    clip.vline(snapc(x1), y1..=y2, e);
                }
                for z in &zones {
                    let x0 = x_of(z.i0 as f32);
                    if x0 > plot.right() {
                        continue;
                    }
                    let x0 = x0.max(plot.left());
                    let (y1, y2) = (y_of(z.hi), y_of(z.lo));
                    grad_rect(&clip, Rect::from_min_max(Pos2::new(x0, y1), Pos2::new(plot.right(), y2)), hexc(0xff2bd6, 0.30), hexc(0xff2bd6, 0.05), hexc(0xff2bd6, 0.05), hexc(0xff2bd6, 0.30));
                    let e = Stroke::new(hair, hexc(0xff2bd6, 0.8));
                    clip.hline(x0..=plot.right(), snapc(y1), e);
                    clip.hline(x0..=plot.right(), snapc(y2), e);
                }
            }

            // ═════════ HTF FVG (intraday mode) ═════════
            if intraday && self.show_htf && !hb.is_empty() {
                let tag = format!("{} FVG", HTFS[self.htf_i].0);
                let tfont = FontId::monospace(10.0);
                for z in htf_zones(&hb) {
                    let ci = cs.partition_point(|c| c.e < z.e0);
                    if ci >= n {
                        continue;
                    }
                    let x0 = if ci == 0 && z.e0 < cs[0].e { plot.left() } else { x_of(ci as f32) };
                    if x0 > plot.right() {
                        continue;
                    }
                    let x0 = x0.max(plot.left());
                    let (yt, yb) = (y_of(z.hi0), y_of(z.lo0));
                    grad_rect(&clip, Rect::from_min_max(Pos2::new(x0, yt), Pos2::new(plot.right(), yb)), hexc(0xff7a1a, 0.30), hexc(0xff7a1a, 0.05), hexc(0xff7a1a, 0.05), hexc(0xff7a1a, 0.30));
                    let (bt, bb, edge) = if z.up && z.hi < z.hi0 {
                        (Some(yt), Some(y_of(z.hi)), Some(y_of(z.hi)))
                    } else if !z.up && z.lo > z.lo0 {
                        (Some(y_of(z.lo)), Some(yb), Some(y_of(z.lo)))
                    } else {
                        (None, None, None)
                    };
                    if let (Some(bt), Some(bb)) = (bt, bb) {
                        clip.rect_filled(Rect::from_min_max(Pos2::new(x0, bt), Pos2::new(plot.right(), bb)), 0.0, hexc(0x8b9cff, 0.2));
                        if let Some(ed) = edge {
                            clip.hline(x0..=plot.right(), snapc(ed), Stroke::new(hair, hexc(0x8b9cff, 0.9)));
                        }
                    }
                    let o = Stroke::new(hair, hexc(0xff7a1a, 0.85));
                    clip.hline(x0..=plot.right(), snapc(yt), o);
                    clip.hline(x0..=plot.right(), snapc(yb), o);
                    let tw = text_w(&painter, &tag, &tfont);
                    let tx = plot.right() - tw - 5.0;
                    let ty = (yt + yb) / 2.0;
                    if tx >= x0 + 4.0 && ty > plot.top() && ty < plot.bottom() {
                        clip.rect_filled(Rect::from_min_max(Pos2::new(tx - 3.0, ty - 7.0), Pos2::new(tx + tw + 3.0, ty + 7.0)), 0.0, hexc(0x040d1a, 0.75));
                        clip.text(Pos2::new(tx, ty), Align2::LEFT_CENTER, &tag, tfont.clone(), rgb(0xff7a1a));
                    }
                }
            }

            // ═════════ previous-period levels ═════════
            let last = cs[n - 1];
            let lv_font = FontId::monospace(10.0);
            if intraday {
                let lv = intraday_levels(&ds, last.e, anchor);
                for l in &lv {
                    let y = y_of(l.price);
                    if y > plot.top() && y < plot.bottom() {
                        dash_h(&clip, plot.left(), plot.right(), snapc(y), 6.0, 6.0, Stroke::new(hair, rgb(l.col)));
                    }
                }
                // tags: same-height levels share a row, side by side at the right edge, highest timeframe leftmost
                let tagh = 14.0f32;
                let mut items: Vec<(f32, usize)> = lv
                    .iter()
                    .enumerate()
                    .filter_map(|(k, l)| {
                        let y = y_of(l.price);
                        if y < plot.top() + tagh / 2.0 || y > plot.bottom() - tagh / 2.0 { None } else { Some((y, k)) }
                    })
                    .collect();
                items.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap());
                let mut rows: Vec<(f32, f32, Vec<usize>)> = vec![];
                for (y, k) in items {
                    if let Some(r) = rows.last_mut() {
                        if y - r.0 < tagh {
                            r.1 += y;
                            r.2.push(k);
                            continue;
                        }
                    }
                    rows.push((y, y, vec![k]));
                }
                let mut prev = -1e9f32;
                let plot_r = plot.right() - 4.0;
                for (_, sum, mut ks) in rows {
                    let mut y = sum / ks.len() as f32;
                    if y < prev + tagh {
                        y = prev + tagh;
                    }
                    prev = y;
                    ks.sort_by(|a, b| lv[*b].rank.cmp(&lv[*a].rank));
                    let ws: Vec<f32> = ks.iter().map(|k| text_w(&painter, &lv[*k].k, &lv_font) + 8.0).collect();
                    let total: f32 = ws.iter().sum::<f32>() + 2.0 * (ws.len() as f32 - 1.0);
                    let mut x = plot_r - total;
                    for (j, k) in ks.iter().enumerate() {
                        let r = Rect::from_min_size(Pos2::new(x, y - tagh / 2.0), Vec2::new(ws[j], tagh));
                        pill(&painter, r, rgb(lv[*k].col), &lv[*k].k, lum_text(lv[*k].col), lv_font.clone());
                        x += ws[j] + 2.0;
                    }
                }
            } else {
                // each group set: (period, tag prefix, [close, high, low] colours, line width x, tags [C, H, L], extra shift)
                let silver = (0xb8c4d9u32, 0x00b862u32, 0xff4444u32);
                let mut sets: Vec<(Per, &str, [u32; 3], f32, [&str; 3], i64)> = vec![];
                let std_cols = [silver.0, silver.1, silver.2];
                let alt_cols = [0x8a3ffc, 0xa3e635, 0xff7a59]; // violet close / lime high / coral low
                match tf {
                    "W" => {
                        sets.push((Per::Quarter, "PQ", std_cols, 1.0, ["PQC", "PQH", "PQL"], 2 * 86400));
                        if self.show_py {
                            sets.push((Per::Year, "PY", alt_cols, 2.0, ["PYC", "PYH", "PYL"], 2 * 86400));
                        }
                    }
                    "D" => {
                        sets.push((Per::Month, "PM", std_cols, 1.0, ["PMC", "PMH", "PML"], 0));
                        if self.show_pq {
                            sets.push((Per::Quarter, "PQ", alt_cols, 2.0, ["", "PQH", "PQL"], 0));
                        }
                    }
                    _ if secs >= 3600 => {
                        sets.push((Per::Week, "PW", std_cols, 1.0, ["PWC", "PWH", "PWL"], 0));
                        if tf == "8h" && self.show_pm {
                            sets.push((Per::Month, "PM", alt_cols, 2.0, ["", "PMH", "PML"], 0));
                        }
                    }
                    _ => {
                        sets.push((Per::Day, "PD", std_cols, 1.0, ["PDC", "PDH", "PDL"], 0));
                        if tf == "30m" && self.show_pw {
                            sets.push((Per::Week, "PW", std_cols, 2.0, ["", "PWH", "PWL"], 0));
                        }
                    }
                }
                for (per, _pre, cols, wmul, tags, extra) in sets {
                    let gs = period_groups(&cs, &ds, per, anchor, extra);
                    for (gi, g) in gs.iter().enumerate() {
                        let last_g = gi + 1 == gs.len();
                        let end = if last_g { g.i1 as f32 } else { gs[gi + 1].i0 as f32 };
                        let (x0, x1) = (x_of(g.i0 as f32), x_of(end));
                        if x1 < plot.left() || x0 > plot.right() {
                            continue;
                        }
                        for (v, col) in [(g.h, cols[1]), (g.l, cols[2]), (g.c, cols[0])] {
                            if !v.is_finite() {
                                continue;
                            }
                            let y = y_of(v);
                            if y > plot.top() && y < plot.bottom() {
                                dash_h(&clip, x0.max(plot.left()), x1.min(plot.right()), snapc(y), 6.0, 6.0, Stroke::new(hair * wmul, rgb(col)));
                            }
                        }
                        if last_g {
                            for (v, col, tag) in [(g.c, cols[0], tags[0]), (g.h, cols[1], tags[1]), (g.l, cols[2], tags[2])] {
                                if tag.is_empty() || !v.is_finite() || v <= lo || v >= hi {
                                    continue;
                                }
                                let y = y_of(v);
                                let r = Rect::from_min_size(Pos2::new(plot.right() + 2.0, y - 9.0), Vec2::new(AXIS_W - 4.0, 18.0));
                                pill(&painter, r, rgb(col), &format!("{tag} {}", fmt_price(v, step)), lum_text(col), FontId::monospace(11.0));
                            }
                        }
                    }
                }
            }

            // ═════════ NQ round numbers ═════════
            if self.sym == "NQ" && !self.hide_nums {
                if intraday {
                    let mut p = (lo / 250.0).ceil() * 250.0;
                    while p < hi {
                        let m = (p as i64).rem_euclid(1000);
                        let big = (m == 0) as i32;
                        let colr: u32 = if m == 0 { 0xffd24a } else if m == 500 { 0xff9f1a } else { 0x22e5ff };
                        let y = y_of(p);
                        let (yl, st) = (snapc(y), Stroke::new(if big == 1 { 2.0 * hair } else { hair }, rgb(colr)));
                        if m == 0 {
                            clip.hline(plot.x_range(), yl, st);
                        } else if m == 500 {
                            dash_h(&clip, plot.left(), plot.right(), yl, 10.0, 4.0, st);
                        } else {
                            dash_h(&clip, plot.left(), plot.right(), yl, 1.0, 3.0, st);
                        }
                        let title = if m < 100 { format!("0{m}") } else { m.to_string() };
                        let tw = text_w(&painter, &title, &lv_font) + 8.0;
                        let inv = (p as i64) % 2500 == 0; // 20000 / 22500 / 25000...: dark fill, coloured text + outline
                        let tr = Rect::from_min_size(Pos2::new(plot.right() - tw - 1.0, y - 8.0), Vec2::new(tw, 16.0));
                        let ar = Rect::from_min_size(Pos2::new(plot.right() + 2.0, y - 9.0), Vec2::new(AXIS_W - 4.0, 18.0));
                        for (r, txt) in [(tr, title.clone()), (ar, fmt_price(p, step))] {
                            if inv {
                                painter.rect_filled(r, 2.0, bg);
                                painter.rect_stroke(r, 2.0, Stroke::new(hair.max(1.0), rgb(colr)), egui::StrokeKind::Inside);
                                painter.text(r.center(), Align2::CENTER_CENTER, txt, FontId::monospace(11.0), rgb(colr));
                            } else {
                                pill(&painter, r, rgb(colr), &txt, rgb(0x040d1a), FontId::monospace(11.0));
                            }
                        }
                        p += 250.0;
                    }
                } else if secs <= 604800 {
                    let mut p = (lo / 2500.0).ceil() * 2500.0;
                    while p < hi {
                        let y = y_of(p);
                        dash_h(&clip, plot.left(), plot.right(), snapc(y), 6.0, 6.0, Stroke::new(hair, rgb(0xffbf00)));
                        let ar = Rect::from_min_size(Pos2::new(plot.right() + 2.0, y - 9.0), Vec2::new(AXIS_W - 4.0, 18.0));
                        pill(&painter, ar, rgb(0xffbf00), &fmt_price(p, step), rgb(0x040d1a), FontId::monospace(11.0));
                        p += 2500.0;
                    }
                }
            }

            // ═════════ NY open (intraday mode): latest 9:30 candle ═════════
            if intraday {
                let mut oi = None;
                for i in (0..n).rev() {
                    let sod = cs[i].e.rem_euclid(86400);
                    if matches!(weekday_of(cs[i].e), 0 | 6) {
                        continue;
                    }
                    if sod <= 34200 && sod + secs > 34200 {
                        oi = Some((i, sod));
                        break;
                    }
                }
                if let Some((oi, sod)) = oi {
                    let e930 = cs[oi].e - sod + 34200;
                    let px = if sod == 34200 {
                        Some(cs[oi].o)
                    } else {
                        let k = m5.partition_point(|b| b.e < e930);
                        m5.get(k).filter(|b| b.e == e930).map(|b| b.o)
                    };
                    let day_end = cs[oi].e - sod + 64800;
                    let mut ei = oi;
                    while ei + 1 < n && cs[ei + 1].e < day_end {
                        ei += 1;
                    }
                    let live = ei == n - 1 && cs[ei].e + secs <= day_end;
                    let x = x_of(oi as f32);
                    let x1 = if live { plot.right() } else { x_of(ei as f32) + sp };
                    if x >= plot.left() && x <= plot.right() {
                        dash_v(&clip, snapc(x), plot.top(), plot.bottom(), 2.0, 4.0, Stroke::new(hair, hexc(0x4aa3ff, 0.9)));
                    }
                    if let Some(px) = px {
                        let y = y_of(px);
                        if y > plot.top() && y < plot.bottom() && x <= plot.right() && x1 >= plot.left() {
                            dash_h(&clip, x.max(plot.left()), x1.min(plot.right()), snapc(y), 6.0, 4.0, Stroke::new(hair, hexc(0xe8f0ff, 0.9)));
                            clip.text(Pos2::new(x1.min(plot.right()) - 4.0, snapc(y) - 3.0), Align2::RIGHT_BOTTOM, "NY Open", small.clone(), hexc(0xe8f0ff, 0.9));
                        }
                    }
                }
            }

            // ═════════ 400R swings: big = solid lines (NinjaTrader Swing, strength 5), drawn behind the candles ═════════
            let swing_tk = tick_size(&self.sym) * 0.5;
            let (small_h, small_l) = if is_range {
                let (_, _, ph, pl) = nt_swing(&cs, SMALL_SWING, swing_tk);
                (ph, pl)
            } else {
                (vec![], vec![])
            };
            if is_range {
                let (hp, lp, ph, pl) = nt_swing(&cs, BIG_SWING, swing_tk);
                let (red, grn) = (rgb(0xff4444), UP_FILL);
                let mut segs: Vec<(usize, usize, f64, bool)> = vec![]; // (from bar, to bar, price, is high)
                if self.swing_ext {
                    // every confirmed swing runs on until a later bar hits its price (or to the right edge)
                    for &i in &ph {
                        let v = cs[i].h;
                        segs.push((i, (i + 1..n).find(|&j| cs[j].h >= v).unwrap_or(usize::MAX), v, true));
                    }
                    for &i in &pl {
                        let v = cs[i].l;
                        segs.push((i, (i + 1..n).find(|&j| cs[j].l <= v).unwrap_or(usize::MAX), v, false));
                    }
                } else {
                    for (a, b, v) in swing_runs(&hp) {
                        segs.push((a, if b + 1 >= n.saturating_sub(1) { usize::MAX } else { b }, v, true));
                    }
                    for (a, b, v) in swing_runs(&lp) {
                        segs.push((a, if b + 1 >= n.saturating_sub(1) { usize::MAX } else { b }, v, false));
                    }
                }
                for (a, b, v, is_high) in segs {
                    let y = y_of(v);
                    if y < plot.top() || y > plot.bottom() {
                        continue;
                    }
                    let x0 = x_of(a as f32);
                    let x1 = if b == usize::MAX { plot.right() } else { x_of(b as f32) };
                    if x1 < plot.left() || x0 > plot.right() {
                        continue;
                    }
                    clip.hline(x0.max(plot.left())..=x1.min(plot.right()), snapc(y), Stroke::new(2.0, if is_high { red } else { grn }));
                }
            }
            // ═════════ candles (all drawn on whole physical pixels) ═════════
            let wick_px = ((WICK_PTS * ppp).round() as i32).max(1);
            let bw = {
                let w = ((sp * BODY_RATIO * ppp).floor() as i32).max(1);
                if wick_px % 2 != w % 2 { w + 1 } else { w } // body and wick share a parity so the wick is centred
            };
            let px = |v: i32| v as f32 / ppp;
            let rect_px = |l: i32, t: i32, r: i32, b: i32| Rect::from_min_max(Pos2::new(px(l), px(t)), Pos2::new(px(r), px(b)));
            for i in first_vis..=last_vis.min(n - 1) {
                let c = cs[i];
                let up = c.c >= c.o;
                let (fill, line) = if up { (UP_FILL, UP_LINE) } else { (DN_FILL, DN_LINE) };
                let wick_col = fill;
                let cx = (x_of(i as f32) * ppp).round() as i32;
                let (top, bot) = {
                    let a = (y_of(c.o.max(c.c)) * ppp).round() as i32;
                    let b = (y_of(c.o.min(c.c)) * ppp).round() as i32;
                    (a, b.max(a + 1))
                };
                let (hh, ll) = ((y_of(c.h) * ppp).round() as i32, (y_of(c.l) * ppp).round() as i32);
                let wl = cx - wick_px / 2;
                clip.rect_filled(rect_px(wl, hh, wl + wick_px, ll.max(hh + 1)), 0.0, wick_col);
                let left = cx - bw / 2;
                let right = left + bw;
                let body_h = bot - top;
                if bw <= 2 * wick_px + 1 || body_h <= 2 * wick_px + 1 {
                    clip.rect_filled(rect_px(left, top, right, bot), 0.0, fill);
                } else {
                    clip.rect_filled(rect_px(left, top, right, bot), 0.0, line);
                    clip.rect_filled(rect_px(left + wick_px, top + wick_px, right - wick_px, bot - wick_px), 0.0, fill);
                }
            }

            // small swings (strength 2): a star past the wick
            for &i in &small_h {
                let p = Pos2::new(x_of(i as f32), y_of(cs[i].h) - 11.0);
                if plot.expand(8.0).contains(p) {
                    star_shape(&clip, p, 6.0, rgb(0xff2a2a));
                }
            }
            for &i in &small_l {
                let p = Pos2::new(x_of(i as f32), y_of(cs[i].l) + 11.0);
                if plot.expand(8.0).contains(p) {
                    star_shape(&clip, p, 6.0, rgb(0x1fe05a));
                }
            }

            // ═════════ 5m EMA 20 / 50 (NQ, intraday mode) ═════════
            if intraday && self.sym == "NQ" && m5.len() > 50 {
                for (len, tag, col, tcol, dashed) in [(20usize, "5m 20", 0x1d4ed8u32, 0x3b6fe0u32, false), (50, "5m 50", 0xce4b05, 0xee6a1f, true)] {
                    let pts = ema_points(&cs, &m5, len, secs);
                    let line: Vec<Pos2> = pts
                        .iter()
                        .filter(|(i, _)| *i + 1 >= first_vis && *i <= last_vis + 1)
                        .map(|(i, v)| Pos2::new(x_of(*i as f32), y_of(*v)))
                        .collect();
                    if line.len() >= 2 {
                        let st = Stroke::new(2.0, rgb(col));
                        if !dashed {
                            fill_under(&clip, &line, col, plot.bottom());
                        }
                        if dashed {
                            dashed_by_x(&clip, &line, x_of(0.0), 6.0, 4.0, st);
                        } else {
                            clip.add(egui::Shape::line(line.clone(), st));
                        }
                    }
                    if let Some((li, lvv)) = pts.last() {
                        let (lx, ly) = (x_of(*li as f32), y_of(*lvv));
                        if ly >= plot.top() + 8.0 && ly <= plot.bottom() - 8.0 {
                            let tw = text_w(&painter, tag, &small);
                            let mut tx = lx + 6.0;
                            if tx + tw > plot.right() - 2.0 {
                                tx = (plot.right() - tw - 2.0).max(plot.left() + 2.0);
                            }
                            clip.rect_filled(Rect::from_min_max(Pos2::new(tx - 2.0, ly - 7.0), Pos2::new(tx + tw + 2.0, ly + 7.0)), 0.0, hexc(0x040d1a, 0.75));
                            clip.text(Pos2::new(tx, ly), Align2::LEFT_CENTER, tag, small.clone(), rgb(tcol));
                        }
                    }
                }
            }

            // ═════════ range chart: 14 / 50 EMA of the bar closes (50 green) ═════════
            if is_range {
                let closes: Vec<f64> = cs.iter().map(|c| c.c).collect();
                for (len, col) in [(EMA_FAST, 0xffb020u32), (EMA_SLOW, 0x00ff55)] {
                    let em = ema_of(&closes, len);
                    let line: Vec<Pos2> = (first_vis.saturating_sub(1)..=(last_vis + 1).min(n - 1))
                        .filter_map(|i| em[i].map(|v| Pos2::new(x_of(i as f32), y_of(v))))
                        .collect();
                    if line.len() >= 2 {
                        clip.add(egui::Shape::line(line, Stroke::new(2.0, rgb(col))));
                    }
                    if let Some(Some(v)) = em.last() {
                        let (lx, ly) = (x_of((n - 1) as f32), y_of(*v));
                        if ly >= plot.top() + 8.0 && ly <= plot.bottom() - 8.0 {
                            let tag = format!("EMA {len}");
                            let tw = text_w(&painter, &tag, &small);
                            let tx = (lx + 6.0).min(plot.right() - tw - 2.0).max(plot.left() + 2.0);
                            clip.rect_filled(Rect::from_min_max(Pos2::new(tx - 2.0, ly - 7.0), Pos2::new(tx + tw + 2.0, ly + 7.0)), 0.0, hexc(0x040d1a, 0.75));
                            clip.text(Pos2::new(tx, ly), Align2::LEFT_CENTER, tag, small.clone(), rgb(col));
                        }
                    }
                }
                // EMA 8: colour follows its position vs the EMA 14 (dark green above, dark red below)
                {
                    let e8 = ema_of(&closes, 8);
                    let e14 = ema_of(&closes, EMA_FAST);
                    let (g8, r8) = (0x14a040u32, 0xc42222u32);
                    let lo = first_vis.saturating_sub(1).max(1);
                    let hi = (last_vis + 1).min(n - 1);
                    for i in lo..=hi {
                        if let (Some(a), Some(b), Some(f)) = (e8[i - 1], e8[i], e14[i]) {
                            let col = if b >= f { g8 } else { r8 };
                            clip.line_segment([Pos2::new(x_of((i - 1) as f32), y_of(a)), Pos2::new(x_of(i as f32), y_of(b))], Stroke::new(2.0, rgb(col)));
                        }
                    }
                    if let (Some(Some(v)), Some(Some(f))) = (e8.last(), e14.last()) {
                        let (lx, ly) = (x_of((n - 1) as f32), y_of(*v));
                        if ly >= plot.top() + 8.0 && ly <= plot.bottom() - 8.0 {
                            let tw = text_w(&painter, "EMA 8", &small);
                            let tx = (lx + 6.0).min(plot.right() - tw - 2.0).max(plot.left() + 2.0);
                            clip.rect_filled(Rect::from_min_max(Pos2::new(tx - 2.0, ly - 7.0), Pos2::new(tx + tw + 2.0, ly + 7.0)), 0.0, hexc(0x040d1a, 0.75));
                            clip.text(Pos2::new(tx, ly), Align2::LEFT_CENTER, "EMA 8", small.clone(), rgb(if *v >= *f { g8 } else { r8 }));
                        }
                    }
                }
            }

            // ═════════ 400R: how long the forming bar has been open (hh:mm:ss), bottom right ═════════
            if is_range && !cs.is_empty() {
                let now = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0);
                let d = (now - cs[cs.len() - 1].t).max(0);
                let txt = format!("{:02}:{:02}:{:02}", d / 3600, (d % 3600) / 60, d % 60);
                let tw = text_w(&painter, &txt, &small);
                let (rx, by) = (plot.right() - 8.0, plot.bottom() - 8.0);
                clip.rect_filled(Rect::from_min_max(Pos2::new(rx - tw - 4.0, by - 14.0), Pos2::new(rx + 3.0, by + 2.0)), 0.0, hexc(0x040d1a, 0.75));
                clip.text(Pos2::new(rx, by), Align2::RIGHT_BOTTOM, txt, small.clone(), rgb(0x00e5ff));
                ui.ctx().request_repaint_after(Duration::from_millis(500));
            }

            // ═════════ last price ═════════
            let ly = snapc(y_of(last.c));
            let lcol = if last.c >= last.o { UP_FILL } else { DN_FILL };
            if ly > plot.top() && ly < plot.bottom() {
                dash_h(&clip, plot.left(), plot.right(), ly, 2.0, 3.0, Stroke::new(hair, lcol));
            }
            // price tag with the bar countdown underneath (as on the web chart)
            let now = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0);
            let remain = (last.t + secs - now).max(0);
            let cd = if secs > 86400 {
                format!("{}d:{}h", remain / 86400, (remain % 86400) / 3600)
            } else if secs >= 14400 {
                format!("{:02}:{:02}:{:02}", remain / 3600, (remain % 3600) / 60, remain % 60)
            } else {
                format!("{}:{:02}", remain / 60, remain % 60)
            };
            let tag_h = if is_range { 24.0 } else { 40.0 };
            let ly_c = ly.clamp(plot.top() + tag_h / 2.0, plot.bottom() - tag_h / 2.0);
            let tcol = if last.c >= last.o { rgb(0x00b862) } else { rgb(0xff4444) };
            let r = Rect::from_min_size(Pos2::new(plot.right() + 2.0, ly_c - tag_h / 2.0), Vec2::new(AXIS_W - 4.0, tag_h));
            tag_box(&painter, r, tcol);
            grad_rect(&painter, r.shrink(1.5), Color32::from_white_alpha(46), Color32::from_white_alpha(46), Color32::from_white_alpha(0), Color32::from_white_alpha(0));
            painter.text(Pos2::new(r.center().x, if is_range { r.center().y } else { r.top() + 11.0 }), Align2::CENTER_CENTER, fmt_price(last.c, step), FontId::monospace(13.0), Color32::WHITE);
            if !is_range {
                let cr = Rect::from_center_size(Pos2::new(r.center().x, r.bottom() - 11.0), Vec2::new(text_w(&painter, &cd, &FontId::monospace(13.0)) + 8.0, 16.0));
                tag_box(&painter, cr, hexc(0x040d1a, 0.9));
                painter.text(cr.center(), Align2::CENTER_CENTER, cd, FontId::monospace(13.0), rgb(0x2ec4b6));
            }

            // ═════════ live orders from NinjaTrader (EdgeFeed /orders), on the futures prices of the NT candles ═════════
            // Labels sit on the left. Drag a label up or down to move the real order; the blue entry chip sits right beside them (never over the stop)
            // and its X closes the trade at market and cancels the stop and target. A waiting entry order is tan with its own X (cancels just that order).
            // With two accounts in the same symbol every label carries its account name.
            if src_nt {
                let (orders, ents) = {
                    let s = self.shared.lock().unwrap();
                    (s.orders.clone(), s.entries.clone())
                };
                let oqf = FontId::monospace(15.0);
                let tk = tick_size(&self.sym);
                let now_i = std::time::Instant::now();
                // a move waiting for NinjaTrader: keep the line where you dropped it until NinjaTrader shows it there (or says no)
                if let Some((pid, ppx, t0)) = self.ord_pend.clone() {
                    let cur = orders.iter().find(|o| o.id == pid).map(|o| o.px);
                    if cur.map_or(false, |c| (c - ppx).abs() < tk * 0.5) {
                        self.ord_pend = None;
                    } else if t0.elapsed() > Duration::from_secs(4) {
                        self.ord_pend = None;
                        if cur.is_some() {
                            self.shared.lock().unwrap().toast = Some(("NinjaTrader did not accept that move".to_string(), false, now_i));
                        }
                    } else {
                        ctx.request_repaint_after(Duration::from_millis(250));
                    }
                }
                // account names show only when more than one account has something on this symbol
                let mut accts: Vec<String> = vec![];
                for o in &orders {
                    if !accts.contains(&o.acct) {
                        accts.push(o.acct.clone());
                    }
                }
                for e in &ents {
                    if !accts.contains(&e.4) {
                        accts.push(e.4.clone());
                    }
                }
                let multi = accts.len() > 1;
                // contract names show only when more than one contract (NQ and NNQ) has something on this symbol
                let mut insts: Vec<String> = vec![];
                for o in &orders { if !o.inst.is_empty() && !insts.contains(&o.inst) { insts.push(o.inst.clone()); } }
                for e in &ents { if !e.5.is_empty() && !insts.contains(&e.5) { insts.push(e.5.clone()); } }
                let multi_i = insts.len() > 1;
                // hotkeys: Alt+C close the trade, Alt+R reverse it, Alt+E stop to break even, Alt+F flatten the account(s)
                if !ctx.egui_wants_keyboard_input() && (!orders.is_empty() || !ents.is_empty()) {
                    let (kc, ke, kf, kr) = ctx.input(|i| (i.modifiers.alt && !i.modifiers.ctrl && i.key_pressed(egui::Key::C), i.modifiers.alt && !i.modifiers.ctrl && i.key_pressed(egui::Key::E), i.modifiers.alt && !i.modifiers.ctrl && i.key_pressed(egui::Key::F), i.modifiers.alt && !i.modifiers.ctrl && i.key_pressed(egui::Key::R)));
                    if (kc || ke || kf || kr) && self.close_sent.map_or(true, |t| t.elapsed() > Duration::from_millis(1500)) {
                        self.close_sent = Some(now_i);
                        if kc {
                            self.shared.lock().unwrap().toast = Some(("Closing trade…".to_string(), true, now_i));
                            for e in &ents {
                                send_cmd(self.shared.clone(), ctx.clone(), format!("/close?acct={}&sym={}", urlenc(&e.4), urlenc(if e.5.is_empty() { self.sym.as_str() } else { e.5.as_str() })));
                            }
                        } else if kr {
                            self.shared.lock().unwrap().toast = Some(("Reversing…".to_string(), true, now_i));
                            for e in &ents {
                                send_cmd(self.shared.clone(), ctx.clone(), format!("/reverse?acct={}&sym={}", urlenc(&e.4), urlenc(if e.5.is_empty() { self.sym.as_str() } else { e.5.as_str() })));
                            }
                        } else if kf {
                            self.shared.lock().unwrap().toast = Some(("Flat…".to_string(), true, now_i));
                            for a in &accts {
                                send_cmd(self.shared.clone(), ctx.clone(), format!("/flat?acct={}", urlenc(a)));
                            }
                        } else {
                            let (mut n, mut bad) = (0, 0);
                            for o in orders.iter().filter(|o| !o.entry && o.is_stop) {
                                let Some((avg, kk)) = o.val else { continue };
                                let bp = if kk > 0.0 { avg + BE_PTS } else { avg - BE_PTS };
                                let wrong = if kk > 0.0 { bp >= last.c } else { bp <= last.c };
                                if wrong {
                                    bad += 1;
                                    continue;
                                }
                                n += 1;
                                self.ord_pend = Some((o.id.clone(), bp, now_i));
                                send_cmd(self.shared.clone(), ctx.clone(), format!("/move?id={}&px={}", urlenc(&o.id), bp));
                            }
                            let msg = if n > 0 { "Stop to entry +10 pts" } else if bad > 0 { "Break even: price isn't 10 pts past your entry yet" } else { "No stop to move" };
                            self.shared.lock().unwrap().toast = Some((msg.to_string(), n > 0, now_i));
                        }
                    }
                }
                let pre_of = |a: &str, i: &str| -> String { let mut p = String::new(); if multi { p.push_str(a); p.push(' '); } if multi_i { p.push_str(i); p.push(' '); } p };
                let pre_max: String = pre_of(accts.iter().max_by_key(|x| x.len()).map(|x| x.as_str()).unwrap_or(""), insts.iter().max_by_key(|x| x.len()).map(|x| x.as_str()).unwrap_or(""));
                // the price each order is drawn at: under the pointer while dragging, the dropped price while waiting, else NinjaTrader's
                let mut views: Vec<(usize, f64)> = vec![];
                for (i, o) in orders.iter().enumerate() {
                    let mut px = o.px;
                    if let Some(d) = self.ord_drag.as_ref().filter(|d| d.0 == o.id) {
                        px = d.2;
                    } else if let Some(p) = self.ord_pend.as_ref().filter(|p| p.0 == o.id) {
                        px = p.1;
                    }
                    views.push((i, px));
                }
                let any_pend = orders.iter().any(|o| o.entry);
                let label_of = |is_stop: bool, usd: f64, pre: &str| format!("{}{}   {}${:.2}", pre, if is_stop { "STOP" } else { "TARGET" }, if usd >= 0.0 { "+" } else { "-" }, usd.abs());
                // fixed width so the entry chip beside the labels does not jiggle while dollars change
                let mut lw = text_w(&painter, &format!("{}TARGET   +$9999.00", pre_max), &oqf) + 52.0;
                if any_pend {
                    lw = lw.max(text_w(&painter, &format!("{}BUY LMT 9   waiting to fill", pre_max), &oqf) + 36.0);
                }
                // label heights: pushed apart so nothing overlaps (not while a drag is in progress, so the dragged label stays under the pointer)
                let mut ly: Vec<f32> = views.iter().map(|v| y_of(v.1)).collect();
                let mut cy: Vec<f32> = ents.iter().map(|e| y_of(e.0)).collect();
                if self.ord_drag.is_none() {
                    spread_y(&mut ly);
                    spread_y(&mut cy);
                }
                // entry lines: your average price, with the open profit / loss at the last price (like NinjaTrader's own)
                for (i, (avg, dir, qty, pv, acct, inst)) in ents.iter().enumerate() {
                    let ey = y_of(*avg);
                    if ey < plot.top() || ey > plot.bottom() {
                        continue;
                    }
                    let upl = (last.c - avg) * dir * pv * qty;
                    let etxt = format!("{}{} {}   {}${:.2}", pre_of(acct, inst), if *dir > 0.0 { "LONG" } else { "SHORT" }, qty, if upl >= 0.0 { "+" } else { "-" }, upl.abs());
                    let ecol = rgb(0x4da3ff);
                    dash_h(&clip, plot.left(), plot.right(), snapc(ey), 3.0, 3.0, Stroke::new(hair.max(1.0), ecol));
                    let etw = text_w(&painter, &etxt, &oqf) + 26.0;
                    let ecol_txt = if upl >= 0.0 { rgb(0x5cf59a) } else { rgb(0xff8a8a) };
                    let ex = plot.left() + 12.0 + if views.is_empty() { 0.0 } else { lw + 10.0 + if any_pend { 30.0 } else { 0.0 } };
                    let er = Rect::from_min_size(Pos2::new(ex, cy[i] - 12.0), Vec2::new(etw, 24.0));
                    order_tag(&clip, er, ecol, &etxt, ecol_txt, oqf.clone());
                    // X: close this account's trade in one click (market close + cancel stop and target)
                    // reverse: close this trade and open the same size the other way (market), no new stop or target
                    let rr = Rect::from_min_size(Pos2::new(er.right() + 4.0, cy[i] - 12.0), Vec2::new(26.0, 24.0));
                    let rrsp = ui.interact(rr, egui::Id::new(("ordr", acct.as_str())), Sense::click());
                    let rhot = rrsp.hovered();
                    clip.rect_filled(rr, 4.0, if rhot { rgb(0x4a3410) } else { rgb(0x0a1424) });
                    clip.rect_stroke(rr, 4.0, Stroke::new(1.5, rgb(0xffb15c)), egui::StrokeKind::Inside);
                    let rc = rr.center();
                    let rs = Stroke::new(2.0, Color32::WHITE);
                    clip.line_segment([rc + Vec2::new(-4.0, 5.0), rc + Vec2::new(-4.0, -5.0)], rs);
                    clip.line_segment([rc + Vec2::new(-7.0, -2.0), rc + Vec2::new(-4.0, -5.0)], rs);
                    clip.line_segment([rc + Vec2::new(-1.0, -2.0), rc + Vec2::new(-4.0, -5.0)], rs);
                    clip.line_segment([rc + Vec2::new(4.0, -5.0), rc + Vec2::new(4.0, 5.0)], rs);
                    clip.line_segment([rc + Vec2::new(1.0, 2.0), rc + Vec2::new(4.0, 5.0)], rs);
                    clip.line_segment([rc + Vec2::new(7.0, 2.0), rc + Vec2::new(4.0, 5.0)], rs);
                    if rhot {
                        ctx.set_cursor_icon(egui::CursorIcon::PointingHand);
                        let tip = format!("Reverse: close {}, go {} {}", if *dir > 0.0 { "long" } else { "short" }, if *dir > 0.0 { "short" } else { "long" }, qty);
                        let tr = Rect::from_min_size(Pos2::new(rr.left(), rr.top() - 30.0), Vec2::new(text_w(&painter, &tip, &oqf) + 20.0, 24.0));
                        order_tag(&clip, tr, rgb(0xff8c00), &tip, rgb(0xffb15c), oqf.clone());
                    }
                    if rrsp.clicked() && self.rev_sent.map_or(true, |t| t.elapsed() > Duration::from_secs(3)) {
                        self.rev_sent = Some(now_i);
                        self.shared.lock().unwrap().toast = Some(("Reversing…".to_string(), true, now_i));
                        send_cmd(self.shared.clone(), ctx.clone(), format!("/reverse?acct={}&sym={}", urlenc(acct), urlenc(if inst.is_empty() { self.sym.as_str() } else { inst.as_str() })));
                    }
                    let xr = Rect::from_min_size(Pos2::new(er.right() + 34.0, cy[i] - 12.0), Vec2::new(26.0, 24.0));
                    let xrsp = ui.interact(xr, egui::Id::new(("ordx", acct.as_str())), Sense::click());
                    let hot = xrsp.hovered();
                    clip.rect_filled(xr, 4.0, if hot { rgb(0x4a1c24) } else { rgb(0x0a1424) });
                    clip.rect_stroke(xr, 4.0, Stroke::new(1.5, rgb(0xff8a8a)), egui::StrokeKind::Inside);
                    let xc = xr.center();
                    let xs = Stroke::new(2.0, Color32::WHITE);
                    clip.line_segment([xc + Vec2::new(-5.0, -5.0), xc + Vec2::new(5.0, 5.0)], xs);
                    clip.line_segment([xc + Vec2::new(5.0, -5.0), xc + Vec2::new(-5.0, 5.0)], xs);
                    if hot {
                        ctx.set_cursor_icon(egui::CursorIcon::PointingHand);
                    }
                    if xrsp.clicked() && self.close_sent.map_or(true, |t| t.elapsed() > Duration::from_secs(3)) {
                        self.close_sent = Some(now_i);
                        self.shared.lock().unwrap().toast = Some(("Closing trade…".to_string(), true, now_i));
                        send_cmd(self.shared.clone(), ctx.clone(), format!("/close?acct={}&sym={}", urlenc(acct), urlenc(if inst.is_empty() { self.sym.as_str() } else { inst.as_str() })));
                    }
                    order_tag(&painter, Rect::from_min_size(Pos2::new(plot.right() + 2.0, ey - 12.0), Vec2::new(AXIS_W - 4.0, 24.0)), ecol, &fmt_price(*avg, step), Color32::WHITE, FontId::monospace(13.0));
                }
                for (k, (oi, px)) in views.iter().enumerate() {
                    let o = &orders[*oi];
                    let oy = y_of(*px);
                    if oy < plot.top() || oy > plot.bottom() {
                        continue;
                    }
                    let tan = 0xc9985a_u32;
                    let ocol: u32 = if o.entry { tan } else if o.is_stop { 0xff4d4d } else { 0x2ee67a };
                    let otxt: u32 = if o.is_stop { 0xff8a8a } else { 0x5cf59a };
                    let busy = self.ord_drag.as_ref().map_or(false, |d| d.0 == o.id);
                    if busy {
                        // faint line where the order still is, until you let go
                        let oy0 = y_of(o.px);
                        dash_h(&clip, plot.left(), plot.right(), snapc(oy0), 2.0, 5.0, Stroke::new(hair.max(1.0), rgb(ocol).gamma_multiply(0.4)));
                    }
                    if o.entry {
                        dash_h(&clip, plot.left(), plot.right(), snapc(oy), 2.0, 5.0, Stroke::new(if busy { 2.0 } else { hair.max(1.0) }, rgb(ocol)));
                    } else {
                        dash_h(&clip, plot.left(), plot.right(), snapc(oy), 6.0, 4.0, Stroke::new(if busy { 2.0 } else { hair.max(1.0) }, rgb(ocol)));
                    }
                    let orr = Rect::from_min_size(Pos2::new(plot.right() + 2.0, oy - 12.0), Vec2::new(AXIS_W - 4.0, 24.0));
                    order_tag(&painter, orr, rgb(ocol), &fmt_price(*px, step), Color32::WHITE, FontId::monospace(13.0));
                    let lr = Rect::from_min_size(Pos2::new(plot.left() + 12.0, ly[k] - 12.0), Vec2::new(lw, 24.0));
                    let pre = pre_of(&o.acct, &o.inst);
                    let mut drawn = false;
                    if o.entry {
                        // a waiting entry order: BUY LMT 1  waiting to fill
                        let t1 = format!("{}{} {} {}", pre, if o.buy { "BUY" } else { "SELL" }, if o.is_stop { "STP" } else { "LMT" }, o.qty);
                        order_tag(&clip, lr, rgb(tan), &t1, Color32::WHITE, oqf.clone());
                        let gap = text_w(&painter, &t1, &oqf) + text_w(&painter, "   ", &oqf);
                        clip.text(Pos2::new(lr.left() + 14.0 + gap, lr.center().y), Align2::LEFT_CENTER, "waiting to fill", oqf.clone(), rgb(0xebcb9c));
                        drawn = true;
                        // X: cancel just this order
                        let cr = Rect::from_min_size(Pos2::new(lr.right() + 4.0, lr.top()), Vec2::new(26.0, 24.0));
                        let crsp = ui.interact(cr, egui::Id::new(("ordc", o.id.as_str())), Sense::click());
                        let hot = crsp.hovered();
                        clip.rect_filled(cr, 4.0, if hot { rgb(0x4a1c24) } else { rgb(0x0a1424) });
                        clip.rect_stroke(cr, 4.0, Stroke::new(1.5, rgb(0xff8a8a)), egui::StrokeKind::Inside);
                        let cc = cr.center();
                        let cs = Stroke::new(2.0, Color32::WHITE);
                        clip.line_segment([cc + Vec2::new(-5.0, -5.0), cc + Vec2::new(5.0, 5.0)], cs);
                        clip.line_segment([cc + Vec2::new(5.0, -5.0), cc + Vec2::new(-5.0, 5.0)], cs);
                        if hot {
                            ctx.set_cursor_icon(egui::CursorIcon::PointingHand);
                        }
                        if crsp.clicked() {
                            self.shared.lock().unwrap().toast = Some(("Cancelling order…".to_string(), true, now_i));
                            send_cmd(self.shared.clone(), ctx.clone(), format!("/cancel?id={}", urlenc(&o.id)));
                        }
                    } else if let Some((avg, kk)) = o.val {
                        // what this order is worth in dollars (position average to this level x contracts)
                        let txt = label_of(o.is_stop, (*px - avg) * kk, &pre);
                        order_tag(&clip, lr, rgb(ocol), &txt, rgb(otxt), oqf.clone());
                        // three-line grip: "drag me"
                        let gx = lr.right() - 24.0;
                        for i in 0..3 {
                            let gy = lr.center().y - 5.0 + i as f32 * 5.0;
                            clip.line_segment([Pos2::new(gx, gy), Pos2::new(gx + 16.0, gy)], Stroke::new(1.5, if busy { Color32::WHITE } else { rgb(0x888780) }));
                        }
                        drawn = true;
                    }
                    if drawn {
                        let rsp = ui.interact(lr, egui::Id::new(("ordl", o.id.as_str())), Sense::click_and_drag());
                        if rsp.hovered() || rsp.dragged() {
                            ctx.set_cursor_icon(egui::CursorIcon::ResizeVertical);
                        }
                        if rsp.drag_started() {
                            self.ord_drag = Some((o.id.clone(), o.is_stop, *px));
                            self.ord_pend = None;
                        }
                        if rsp.dragged() {
                            if let Some(pp) = ctx.input(|i| i.pointer.latest_pos()) {
                                let y = pp.y.clamp(plot.top() + 2.0, plot.bottom() - 2.0);
                                let np = ((p_of(y) / tk).round() * tk).max(tk);
                                if let Some(d) = self.ord_drag.as_mut() {
                                    if d.0 == o.id {
                                        d.2 = np;
                                    }
                                }
                                ctx.request_repaint();
                            }
                        }
                        if rsp.drag_stopped() {
                            if let Some(d) = self.ord_drag.take() {
                                if d.0 == o.id && (d.2 - o.px).abs() >= tk * 0.5 {
                                    // a stop must stay on the losing side of the market and a target on the winning side (a waiting buy limit below, buy stop above ...),
                                    // or NinjaTrader would reject it
                                    let bad = if o.entry {
                                        if o.buy { if o.is_stop { d.2 <= last.c } else { d.2 >= last.c } } else if o.is_stop { d.2 >= last.c } else { d.2 <= last.c }
                                    } else {
                                        let long = o.val.map_or(true, |(_, kk)| kk > 0.0);
                                        if o.is_stop { if long { d.2 >= last.c } else { d.2 <= last.c } } else if long { d.2 <= last.c } else { d.2 >= last.c }
                                    };
                                    if bad {
                                        self.shared.lock().unwrap().toast = Some((format!("{} can't go there - it is on the wrong side of the market", if o.entry { "Entry order" } else if o.is_stop { "Stop" } else { "Target" }), false, now_i));
                                    } else {
                                        self.ord_pend = Some((d.0.clone(), d.2, now_i));
                                        send_cmd(self.shared.clone(), ctx.clone(), format!("/move?id={}&px={}", urlenc(&d.0), d.2));
                                    }
                                }
                            }
                        }
                    }
                }
            }
            // short messages from the order buttons (refusals, "closing")
            {
                let t = self.shared.lock().unwrap().toast.clone();
                if let Some((msg, ok, t0)) = t {
                    if t0.elapsed() < Duration::from_secs(4) {
                        let f = FontId::monospace(15.0);
                        let tw = text_w(&painter, &msg, &f) + 28.0;
                        let r = Rect::from_center_size(Pos2::new(plot.center().x, plot.top() + 36.0), Vec2::new(tw, 32.0));
                        order_tag(&painter, r, if ok { rgb(0x2ee67a) } else { rgb(0xff4d4d) }, &msg, Color32::WHITE, f);
                        ctx.request_repaint_after(Duration::from_millis(400));
                    } else {
                        self.shared.lock().unwrap().toast = None;
                    }
                }
            }

            // ═════════ measure tool (shift + drag): points, ticks, dollars, bars, time ═════════
            if self.drag_mode == 3 && resp.dragged() {
                if let Some(cur) = ctx.input(|i| i.pointer.latest_pos()) {
                    let cur = Pos2::new(cur.x.clamp(plot.left(), plot.right()), cur.y.clamp(plot.top(), plot.bottom()));
                    let idx_of = |x: f32| ri - (plot.right() - x) / sp;
                    let tk0 = tick_size(&self.sym);
                    let snap = |p: f64| (p / tk0).round() * tk0; // like TradingView, measure from tick to tick
                    if self.meas.is_none() {
                        if let Some(p0) = self.meas_p0 {
                            self.meas = Some((idx_of(p0.x).round(), snap(p_of(p0.y)), idx_of(p0.x).round(), snap(p_of(p0.y))));
                        }
                    }
                    if let Some(m) = self.meas.as_mut() {
                        m.2 = idx_of(cur.x).round();
                        m.3 = snap(p_of(cur.y));
                    }
                }
            }
            if let Some((i0, p0, i1, p1)) = self.meas {
                let up = p1 >= p0;
                let col = if up { Color32::from_rgb(41, 121, 255) } else { Color32::from_rgb(242, 54, 69) };
                let (x0, x1) = (x_of(i0), x_of(i1));
                let (y0, y1) = (y_of(p0), y_of(p1));
                let rect = Rect::from_min_max(Pos2::new(x0.min(x1), y0.min(y1)), Pos2::new(x0.max(x1), y0.max(y1)));
                painter.rect_filled(rect, 0.0, Color32::from_rgba_unmultiplied(col.r(), col.g(), col.b(), 55));
                painter.rect_stroke(rect, 0.0, Stroke::new(1.0, col), egui::StrokeKind::Inside);
                let c = rect.center();
                painter.line_segment([Pos2::new(rect.left(), c.y), Pos2::new(rect.right(), c.y)], Stroke::new(1.0, col));
                painter.line_segment([Pos2::new(c.x, rect.top()), Pos2::new(c.x, rect.bottom())], Stroke::new(1.0, col));
                let hx = if x1 >= x0 { rect.right() } else { rect.left() };
                let hs = if x1 >= x0 { -7.0 } else { 7.0 };
                painter.add(egui::Shape::convex_polygon(vec![Pos2::new(hx, c.y), Pos2::new(hx + hs, c.y - 4.0), Pos2::new(hx + hs, c.y + 4.0)], col, Stroke::NONE));
                let (vy, vs) = if up { (rect.top(), 7.0) } else { (rect.bottom(), -7.0) };
                painter.add(egui::Shape::convex_polygon(vec![Pos2::new(c.x, vy), Pos2::new(c.x - 4.0, vy + vs), Pos2::new(c.x + 4.0, vy + vs)], col, Stroke::NONE));

                // label
                let pts = p1 - p0;
                let tk = tick_size(&self.sym);
                let dec = if tk >= 0.01 { 2 } else { 4 };
                let sign = if pts >= 0.0 { "+" } else { "-" };
                let money = |v: f64| -> String {
                    let a = v.abs();
                    let body = if a >= 10.0 { format!("{:.0}", a) } else { format!("{:.2}", a) };
                    let mut out = String::new();
                    let (int, frac) = match body.split_once('.') { Some((i, f)) => (i.to_string(), format!(".{f}")), None => (body.clone(), String::new()) };
                    for (k, ch) in int.chars().enumerate() {
                        if k > 0 && (int.len() - k) % 3 == 0 { out.push(','); }
                        out.push(ch);
                    }
                    format!("{}${}{}", if v < 0.0 { "-" } else { "" }, out, frac)
                };
                let mut lines: Vec<String> = vec![format!("{sign}{:.dec$}  ·  {:.0} ticks", pts.abs(), (pts.abs() / tk).round())];
                let pv: Option<f64> = match self.sym.as_str() { "ES" => Some(50.0), "NQ" => Some(20.0), "CL" => Some(1000.0), "GC" => Some(100.0), "NG" => Some(10000.0), _ => None };
                if let Some(pv) = pv {
                    let ex: &[(&str, f64)] = match self.sym.as_str() {
                        "ES" => &[("MES", 5.0), ("NES", 0.5)],
                        "NQ" => &[("MNQ", 2.0), ("NNQ", 0.2)],
                        "CL" => &[("QM", 500.0), ("MCL", 100.0)],
                        "GC" => &[("MGC", 10.0), ("1OZ", 1.0)],
                        "NG" => &[("QG", 2500.0), ("MNG", 1000.0)],
                        _ => &[],
                    };
                    lines.push(format!("{} (1 {})", money(pts * pv), self.sym));
                    if !ex.is_empty() {
                        lines.push(ex.iter().map(|(n, v)| format!("{} ({})", money(pts * v), n)).collect::<Vec<_>>().join("  ·  "));
                    }
                }
                let nb = ((i1 - i0).abs().round() as i64).max(0);
                let secs_d = (l_to_t(i1 as f64) - l_to_t(i0 as f64)).abs() as i64;
                let dur = if secs_d >= 86400 { format!("{}d {}h", secs_d / 86400, (secs_d % 86400) / 3600) } else if secs_d >= 3600 { format!("{}h {:02}m", secs_d / 3600, (secs_d % 3600) / 60) } else { format!("{}m", secs_d / 60) };
                lines.push(format!("{nb} bars  ·  {dur}"));
                let w0 = lines.iter().map(|t| painter.layout_no_wrap(t.clone(), FontId::monospace(13.0), Color32::WHITE).size().x).fold(0.0f32, f32::max) + 22.0;
                if w0 > self.meas_w { self.meas_w = w0; }
                let w = self.meas_w; // only ever grows while measuring, so the box doesn't resize under the cursor
                let h = lines.len() as f32 * 19.0 + 10.0;
                let lx = (c.x - w / 2.0).clamp(plot.left() + 2.0, (plot.right() - w - 2.0).max(plot.left() + 2.0));
                let mut ly = rect.top() - h - 8.0; // always above the box (no flip when dragging past the start point)
                if ly < plot.top() + 2.0 { ly = rect.bottom() + 8.0; }
                if ly + h > plot.bottom() - 2.0 { ly = (plot.bottom() - h - 2.0).max(plot.top() + 2.0); }
                tag_box(&painter, Rect::from_min_size(Pos2::new(lx, ly), Vec2::new(w, h)), col);
                for (k, t) in lines.iter().enumerate() {
                    painter.text(Pos2::new(lx + 11.0, ly + 5.0 + k as f32 * 19.0), Align2::LEFT_TOP, t, FontId::monospace(13.0), Color32::WHITE);
                }
            }
            // ═════════ linked crosshair (other window's pointer) ═════════
            let hovering = resp.hover_pos().map_or(false, |p| plot.contains(p));
            if let Some(g) = self.link_g {
                if hovering && !self.xh_off {
                    if let Some(pos) = resp.hover_pos() {
                        let tm = l_to_t((ri - (plot.right() - pos.x) / sp) as f64) as i64;
                        let _ = self.link_tx.send(format!("{{\"id\":\"{}\",\"g\":{},\"t\":\"xh\",\"time\":{},\"price\":{}}}", self.link_id, g, tm, p_of(pos.y)));
                        self.link_xh = None;
                        self.link_xh_sent = true;
                    }
                } else if self.link_xh_sent {
                    self.link_xh_sent = false;
                    let _ = self.link_tx.send(format!("{{\"id\":\"{}\",\"g\":{},\"t\":\"xhc\"}}", self.link_id, g));
                }
            }
            if !hovering && !self.xh_off {
                if let Some((tm, pr, _)) = self.link_xh {
                    let x = snapc(x_of(t_to_l(tm as f64) as f32));
                    let y = snapc(y_of(pr));
                    let col = Stroke::new(hair, Color32::from_rgb(120, 140, 175));
                    if x >= plot.left() && x <= plot.right() {
                        painter.add(egui::Shape::dashed_line(&[Pos2::new(x, plot.top()), Pos2::new(x, plot.bottom())], col, 4.0, 4.0));
                    }
                    if y >= plot.top() && y <= plot.bottom() {
                        painter.add(egui::Shape::dashed_line(&[Pos2::new(plot.left(), y), Pos2::new(plot.right(), y)], col, 4.0, 4.0));
                        let r = Rect::from_min_size(Pos2::new(plot.right() + 2.0, y - 10.0), Vec2::new(AXIS_W - 4.0, 20.0));
                        tag_box(&painter, r, Color32::from_rgb(60, 72, 96));
                        painter.text(r.center(), Align2::CENTER_CENTER, fmt_price(pr, step), FontId::monospace(13.0), Color32::WHITE);
                    }
                }
            }
            // ═════════ crosshair + readout ═════════
            let hov_now = resp.hover_pos().filter(|p| plot.contains(*p));
            if hov_now.is_some() {
                self.xh_last = hov_now;
            }
            let fa = ctx.animate_bool_with_time(egui::Id::new("xh_fade"), hov_now.is_some() && !self.xh_off, 0.12); // crosshair fades in / out
            if let Some(pos) = hov_now.or(if fa > 0.0 { self.xh_last } else { None }) {
                let idx = ((ri - (plot.right() - pos.x) / sp).round() as i64).clamp(0, n as i64 - 1) as usize;
                let c = cs[idx];
                if fa > 0.0 {
                    let wh = Color32::WHITE.gamma_multiply(fa);
                    let tagc = Color32::from_rgb(60, 72, 96).gamma_multiply(fa);
                    let dash = |a: Pos2, b: Pos2| {
                        painter.add(egui::Shape::dashed_line(&[a, b], Stroke::new(hair, Color32::from_rgb(120, 140, 175).gamma_multiply(fa)), 4.0, 4.0));
                    };
                    dash(Pos2::new(plot.left(), snapc(pos.y)), Pos2::new(plot.right(), snapc(pos.y)));
                    dash(Pos2::new(snapc(pos.x), plot.top()), Pos2::new(snapc(pos.x), plot.bottom()));
                    let r = Rect::from_min_size(Pos2::new(plot.right() + 2.0, pos.y - 10.0), Vec2::new(AXIS_W - 4.0, 20.0));
                    tag_box(&painter, r, tagc);
                    painter.text(r.center(), Align2::CENTER_CENTER, fmt_price(p_of(pos.y), step), FontId::monospace(13.0), wh);
                    let d = et(c.t);
                    let tl = if secs >= 86400 { d.format("%a %d %b %Y").to_string() } else { d.format("%a %d %b %Y  %H:%M").to_string() };
                    let tw = 150.0;
                    let r = Rect::from_min_size(Pos2::new((pos.x - tw / 2.0).clamp(plot.left(), plot.right() - tw), plot.bottom() + 3.0), Vec2::new(tw, 20.0));
                    tag_box(&painter, r, tagc);
                    painter.text(r.center(), Align2::CENTER_CENTER, tl, FontId::monospace(12.0), wh);
                }
                // the O/H/L/C readout stays even with the crosshair lines hidden
                let ra = if self.xh_off { 1.0 } else { fa };
                let chg = c.c - c.o;
                painter.text(
                    Pos2::new(plot.left() + 10.0, plot.top() + 8.0),
                    Align2::LEFT_TOP,
                    format!("O {:.2}  H {:.2}  L {:.2}  C {:.2}  {:+.2}", c.o, c.h, c.l, c.c, chg),
                    FontId::monospace(13.0),
                    (if chg >= 0.0 { UP_FILL } else { DN_FILL }).gamma_multiply(ra),
                );
            }
            painter.text(
                Pos2::new(plot.left() + 10.0, plot.top() + 28.0),
                Align2::LEFT_TOP,
                if is_range { format!("{} · {}-tick range ({} pts)", self.sym, RANGE_TICKS, RANGE_TICKS * tick_size(&self.sym)) } else { format!("{} · {}", self.sym, self.tf) },
                FontId::proportional(13.0),
                Color32::from_rgb(120, 140, 175),
            );
            ctx.request_repaint_after(Duration::from_millis(500));
        });
    }
}

/// Windows 11: paints the window's own title bar (DWM caption colour). Older Windows ignores it and keeps the normal bar.
#[cfg(windows)]
fn set_title_green(frame: &eframe::Frame, green: bool) {
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};
    #[link(name = "dwmapi")]
    unsafe extern "system" {
        fn DwmSetWindowAttribute(hwnd: isize, attr: u32, value: *const u32, size: u32) -> i32;
    }
    let Ok(h) = frame.window_handle() else { return };
    let RawWindowHandle::Win32(w) = h.as_raw() else { return };
    let hwnd = w.hwnd.get();
    // COLORREF is 0x00BBGGRR; 0xFFFFFFFF puts the default back
    let (cap, txt): (u32, u32) = if green { (0x0062_B800, 0x000F_2104) } else { (0xFFFF_FFFF, 0xFFFF_FFFF) };
    unsafe {
        DwmSetWindowAttribute(hwnd, 35, &cap, 4); // DWMWA_CAPTION_COLOR
        DwmSetWindowAttribute(hwnd, 36, &txt, 4); // DWMWA_TEXT_COLOR
    }
}
#[cfg(not(windows))]
fn set_title_green(_frame: &eframe::Frame, _green: bool) {}

fn main() -> eframe::Result {
    let args: Vec<String> = std::env::args().collect();
    let get = |k: &str| args.iter().position(|a| a == k).and_then(|i| args.get(i + 1)).cloned();
    let sym = canon_sym(&get("--sym").unwrap_or_else(|| "NQ".into()));
    let asked = sym.clone();
    let sym = if SYMS.iter().any(|s| s.0 == sym) { sym } else { "NQ".to_string() };
    let note = if asked != sym { format!("{asked} isn't in the GPU chart yet - showing {sym}") } else { String::new() };
    let tf = canon_tf(&get("--tf").unwrap_or_else(|| "5m".into()));
    let tok = std::env::var("EDGE_OANDA_TOKEN").unwrap_or_default();
    let base = std::env::var("EDGE_OANDA_BASE").unwrap_or_else(|_| "https://api-fxpractice.oanda.com".into());
    // --pos x,y and --size w,h (logical points) place the window beside the EDGE window; both optional
    let pair = |k: &str| -> Option<(f32, f32)> {
        let v = get(k)?;
        let (a, b) = v.split_once(',')?;
        Some((a.trim().parse().ok()?, b.trim().parse().ok()?))
    };
    let (w, h) = pair("--size").map(|(w, h)| (w.clamp(500.0, 4000.0), (h - 32.0).clamp(350.0, 3000.0))).unwrap_or((1300.0, 850.0));
    let mut vb = egui::ViewportBuilder::default().with_title("EDGE — Matrix").with_inner_size([w, h]).with_min_inner_size([500.0, 350.0]);
    vb = vb.with_icon(egui::IconData { rgba: include_bytes!("../icon_rgba.bin").to_vec(), width: 64, height: 64 });
    if let Some((x, y)) = pair("--pos") {
        vb = vb.with_position([x, y]);
    }
    let lg = get("--link").and_then(|v| v.parse::<usize>().ok()).filter(|g| *g < LINK_COLORS.len());
    let opts = eframe::NativeOptions { viewport: vb, multisampling: 4, ..Default::default() };
    eframe::run_native("EDGE Matrix", opts, Box::new(move |cc| Ok(Box::new({ let mut a = App::new(cc, sym, tf, base, tok); a.note = note.clone(); a.intraday = args.iter().any(|x| x == "--intraday"); a.link_g = lg; a }))))
}
