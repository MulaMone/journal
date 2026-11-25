#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]
//! EDGE native chart: candles from the same OANDA feed as the web chart, drawn through the GPU.
//! Launched by the EDGE desktop shell. Reads EDGE_OANDA_TOKEN (and optional EDGE_OANDA_BASE) from the environment.

use chrono::{DateTime, Datelike, Offset, TimeZone, Timelike};
use chrono_tz::America::New_York;
use eframe::egui::{self, Align2, Color32, FontId, Pos2, Rect, Sense, Stroke, Vec2};
use std::collections::BTreeMap;
use std::sync::{mpsc, Arc, Mutex};
use std::time::Duration;

// ───────────────────────── style (edit to taste) ─────────────────────────
const BG: Color32 = Color32::from_rgb(4, 13, 26);
const GRID: Color32 = Color32::from_rgb(20, 34, 58);
const AXIS_TXT: Color32 = Color32::from_rgb(214, 224, 242);
const UP_FILL: Color32 = Color32::from_rgb(0, 230, 118);
const UP_LINE: Color32 = Color32::from_rgb(4, 54, 31);
const DN_FILL: Color32 = Color32::from_rgb(255, 68, 68);
const DN_LINE: Color32 = Color32::from_rgb(74, 10, 10);
const BODY_RATIO: f32 = 0.70; // body width as a share of the bar spacing (NinjaTrader is ~0.70)
const WICK_PTS: f32 = 1.0; // wick + outline thickness in points (1px at 100% scaling, 2px at 150-200%)
const DEFAULT_SPACING: f32 = 13.0; // points between candles on open
const RIGHT_GAP_BARS: f32 = 8.0;
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
    ("NQ", "NAS100_USD"),
    ("ES", "SPX500_USD"),
    ("YM", "US30_USD"),
    ("RTY", "US2000_USD"),
    ("GC", "XAU_USD"),
    ("CL", "WTICO_USD"),
    ("NG", "NATGAS_USD"),
];
const TFS: [(&str, &str, i64); 8] = [
    ("2m", "M2", 120),
    ("5m", "M5", 300),
    ("15m", "M15", 900),
    ("30m", "M30", 1800),
    ("1h", "H1", 3600),
    ("4h", "H4", 14400),
    ("D", "D", 86400),
    ("W", "W", 604800),
];
const INTRA_TFS: [&str; 4] = ["2m", "5m", "30m", "1h"];
const HTFS: [(&str, &str, u32); 3] = [("1H", "H1", 1500), ("4H", "H4", 1000), ("D", "D", 500)];
const FUTURES: [&str; 5] = ["ES", "NQ", "CL", "GC", "NG"]; // sessions open 18:00 ET (the web chart's FUTURES_ALIGN_SYMS)

fn inst_for(sym: &str) -> &'static str {
    SYMS.iter().find(|s| s.0 == sym).map(|s| s.1).unwrap_or("NAS100_USD")
}
fn align_for(sym: &str) -> i64 {
    if FUTURES.contains(&sym) { 18 } else { 17 }
}

/// EDGE symbol names that map onto the symbols this chart knows.
fn canon_sym(s: &str) -> String {
    let s = s.to_uppercase();
    match s.as_str() {
        "MNQ" => "NQ",
        "MES" => "ES",
        "GOLD" => "GC",
        "OIL" => "CL",
        "NATGAS" => "NG",
        other => other,
    }
    .to_string()
}

fn off(t: i64) -> i64 {
    New_York.timestamp_opt(t, 0).single().map(|d| d.offset().fix().local_minus_utc() as i64).unwrap_or(-18000)
}

#[derive(Default)]
struct Shared {
    main: Vec<Candle>,
    daily: Vec<Candle>,
    m5: Vec<Candle>,
    htf: Vec<Candle>,
    status: String,
    version: u64, // bumps when a fresh full load replaces the main data
}

enum Req {
    Load { sym: String, tf: String },
    Htf { sym: String, tf: String }, // tf is an HTFS label
}

fn fetch(base: &str, tok: &str, inst: &str, gran: &str, count: u32, align: i64) -> Result<Vec<Candle>, String> {
    let url = format!(
        "{base}/v3/instruments/{inst}/candles?granularity={gran}&price=M&count={count}&alignmentTimezone=America%2FNew_York&dailyAlignment={align}"
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

fn worker(rx: mpsc::Receiver<Req>, shared: Arc<Mutex<Shared>>, ctx: egui::Context, base: String, tok: String) {
    let mut cur: Option<(String, String)> = None; // (sym, main gran)
    let mut htf_cur: Option<(String, String)> = None; // (sym, htf gran)
    let mut tick = 0u32;
    let set_status = |s: &str| {
        shared.lock().unwrap().status = s.to_string();
        ctx.request_repaint();
    };
    loop {
        match rx.recv_timeout(Duration::from_secs(2)) {
            Ok(Req::Load { sym, tf }) => {
                set_status("loading…");
                let gran = TFS.iter().find(|t| t.0 == tf).map(|t| t.1).unwrap_or("M5");
                let al = align_for(&sym);
                match fetch(&base, &tok, inst_for(&sym), gran, 3000, al) {
                    Ok(cs) => {
                        let mut s = shared.lock().unwrap();
                        s.main = cs;
                        s.status.clear();
                        s.version += 1;
                        cur = Some((sym.clone(), gran.to_string()));
                    }
                    Err(e) => {
                        set_status(&e);
                        continue;
                    }
                }
                ctx.request_repaint();
                // levels feed + 5m feed (EMAs, NY open)
                if let Ok(d) = fetch(&base, &tok, inst_for(&sym), "D", 800, al) {
                    shared.lock().unwrap().daily = d;
                }
                if let Ok(m) = fetch(&base, &tok, inst_for(&sym), "M5", 3000, al) {
                    shared.lock().unwrap().m5 = m;
                }
                ctx.request_repaint();
            }
            Ok(Req::Htf { sym, tf }) => {
                if let Some(h) = HTFS.iter().find(|h| h.0 == tf) {
                    if let Ok(b) = fetch(&base, &tok, inst_for(&sym), h.1, h.2, align_for(&sym)) {
                        shared.lock().unwrap().htf = b;
                        htf_cur = Some((sym, h.1.to_string()));
                        ctx.request_repaint();
                    }
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                tick += 1;
                if let Some((sym, gran)) = &cur {
                    let al = align_for(sym);
                    if let Ok(cs) = fetch(&base, &tok, inst_for(sym), gran, 3, al) {
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
fn text_w(p: &egui::Painter, s: &str, f: &FontId) -> f32 {
    p.layout_no_wrap(s.to_string(), f.clone(), Color32::WHITE).size().x
}
fn pill(p: &egui::Painter, r: Rect, fill: Color32, text: &str, tcol: Color32, f: FontId) {
    p.rect_filled(r, 2.0, fill);
    p.text(r.center(), Align2::CENTER_CENTER, text, f, tcol);
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
fn period_groups(cs: &[Candle], ds: &[Candle], per: Per, anchor: i64) -> Vec<Group> {
    let m = agg(ds, per, anchor);
    let mut out: Vec<Group> = vec![];
    let mut cur_k = i64::MIN;
    for (i, c) in cs.iter().enumerate() {
        let k = key_of(per, c.e, anchor);
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

struct App {
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
}

impl App {
    fn new(cc: &eframe::CreationContext<'_>, sym: String, tf: String, base: String, tok: String) -> Self {
        let mut v = egui::Visuals::dark();
        v.panel_fill = BG;
        v.window_fill = BG;
        cc.egui_ctx.set_visuals(v);
        let shared = Arc::new(Mutex::new(Shared::default()));
        let (tx, rx) = mpsc::channel();
        let has_token = !tok.is_empty();
        {
            let (sh, ctx) = (shared.clone(), cc.egui_ctx.clone());
            std::thread::spawn(move || worker(rx, sh, ctx, base, tok));
        }
        let app = App {
            sym,
            tf,
            shared,
            tx,
            spacing: DEFAULT_SPACING,
            right_idx: 0.0,
            seen_version: 0,
            has_token,
            intraday: false,
            show_fvg: true,
            show_htf: true,
            htf_i: 1,
            hide_nums: false,
        };
        app.reload();
        app
    }

    fn reload(&self) {
        let _ = self.tx.send(Req::Load { sym: self.sym.clone(), tf: self.tf.clone() });
        let _ = self.tx.send(Req::Htf { sym: self.sym.clone(), tf: HTFS[self.htf_i].0.to_string() });
    }
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        // ── toolbar ──
        egui::Panel::top("bar").show(ui, |ui| {
            ui.horizontal(|ui| {
                let mut changed = false;
                for (s, _) in SYMS.iter() {
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
                }
                if ui.selectable_label(self.show_fvg, "FVG").clicked() {
                    self.show_fvg = !self.show_fvg;
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
                if self.sym == "NQ" && ui.selectable_label(!self.hide_nums, "#").clicked() {
                    self.hide_nums = !self.hide_nums;
                }
                if changed {
                    self.reload();
                }
                let st = self.shared.lock().unwrap().status.clone();
                if !st.is_empty() {
                    ui.separator();
                    ui.colored_label(Color32::from_rgb(255, 191, 0), st);
                }
            });
        });

        egui::CentralPanel::default().frame(egui::Frame::NONE.fill(BG)).show(ui, |ui| {
            let full = ui.available_rect_before_wrap();
            if !self.has_token {
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
            if ver != self.seen_version {
                self.seen_version = ver;
                self.spacing = DEFAULT_SPACING;
                self.right_idx = (n - 1) as f32 + RIGHT_GAP_BARS;
            }

            // ── interaction ──
            if resp.dragged() {
                self.right_idx -= resp.drag_delta().x / self.spacing;
            }
            if resp.double_clicked() {
                self.spacing = DEFAULT_SPACING;
                self.right_idx = (n - 1) as f32 + RIGHT_GAP_BARS;
            }
            if resp.hovered() {
                let scroll = ctx.input(|i| i.smooth_scroll_delta.y);
                if scroll != 0.0 {
                    let anchor = ctx.input(|i| i.pointer.hover_pos()).map(|p| p.x).unwrap_or(plot.right());
                    let idx_at = self.right_idx - (plot.right() - anchor) / self.spacing;
                    self.spacing = (self.spacing * (scroll * 0.0025).exp()).clamp(3.0, 70.0);
                    self.right_idx = idx_at + (plot.right() - anchor) / self.spacing;
                }
            }
            self.right_idx = self.right_idx.clamp(5.0, (n - 1) as f32 + 200.0);
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
            let y_of = move |p: f64| plot.bottom() - ((p - lo) / (hi - lo)) as f32 * plot.height();
            let p_of = move |y: f32| lo + ((plot.bottom() - y) / plot.height()) as f64 * (hi - lo);

            // ── grid + price axis ──
            let step = nice_step((hi - lo) / (plot.height() as f64 / 70.0).max(2.0));
            let mut p = (lo / step).ceil() * step;
            let axis_font = FontId::monospace(13.0);
            while p < hi {
                let y = snapc(y_of(p));
                painter.hline(plot.x_range(), y, Stroke::new(hair, GRID));
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
                        painter.vline(x, plot.y_range(), Stroke::new(hair, GRID));
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
            painter.rect_stroke(plot, 0.0, Stroke::new(hair, GRID), egui::StrokeKind::Inside);
            let clip = painter.with_clip_rect(plot);
            let small = FontId::monospace(10.0);
            let tiny = FontId::monospace(9.0);

            let align = align_for(&self.sym);
            let anchor = (24 - align) * 3600;
            let tf = self.tf.as_str();
            let intraday = self.intraday;

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

            // ═════════ 9:30 ET open (vertical line on every past day <=30m) ═════════
            let weekday_of = |e: i64| weekday(e.div_euclid(86400));
            if !intraday && secs <= 1800 {
                for i in first_vis..=last_vis.min(n - 1) {
                    if cs[i].e.rem_euclid(86400) == 34200 && !matches!(weekday_of(cs[i].e), 0 | 6) {
                        dash_v(&clip, snapc(x_of(i as f32)), plot.top(), plot.bottom(), 2.0, 4.0, Stroke::new(hair, hexc(0x4aa3ff, 0.9)));
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
                    clip.rect_filled(Rect::from_min_max(Pos2::new(x0, y1), Pos2::new(plot.right(), y2)), 0.0, hexc(0xff2bd6, 0.2));
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
                    clip.rect_filled(Rect::from_min_max(Pos2::new(x0, yt), Pos2::new(plot.right(), yb)), 0.0, hexc(0xff7a1a, 0.2));
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
            } else if secs < 604800 && secs != 14400 || tf == "D" || tf == "4h" {
                let (per, pre) = if tf == "D" { (Per::Month, "PM") } else if secs >= 3600 { (Per::Week, "PW") } else { (Per::Day, "PD") };
                let gs = period_groups(&cs, &ds, per, anchor);
                for (gi, g) in gs.iter().enumerate() {
                    let last_g = gi + 1 == gs.len();
                    let end = if last_g { g.i1 as f32 } else { gs[gi + 1].i0 as f32 };
                    let (x0, x1) = (x_of(g.i0 as f32), x_of(end));
                    if x1 < plot.left() || x0 > plot.right() {
                        continue;
                    }
                    for (v, col) in [(g.c, 0xb8c4d9u32), (g.h, 0x00b862), (g.l, 0xff4444)] {
                        if !v.is_finite() {
                            continue;
                        }
                        let y = y_of(v);
                        if y > plot.top() && y < plot.bottom() {
                            dash_h(&clip, x0.max(plot.left()), x1.min(plot.right()), snapc(y), 6.0, 6.0, Stroke::new(hair, rgb(col)));
                        }
                    }
                    if last_g {
                        for (nm, v, col) in [("C", g.c, 0xb8c4d9u32), ("H", g.h, 0x00b862), ("L", g.l, 0xff4444)] {
                            if v.is_finite() && v > lo && v < hi {
                                let y = y_of(v);
                                let r = Rect::from_min_size(Pos2::new(plot.right() + 2.0, y - 9.0), Vec2::new(AXIS_W - 4.0, 18.0));
                                pill(&painter, r, rgb(col), &format!("{pre}{nm} {}", fmt_price(v, step)), lum_text(col), FontId::monospace(11.0));
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
                                painter.rect_filled(r, 2.0, BG);
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
                let cx = (x_of(i as f32) * ppp).round() as i32;
                let (top, bot) = {
                    let a = (y_of(c.o.max(c.c)) * ppp).round() as i32;
                    let b = (y_of(c.o.min(c.c)) * ppp).round() as i32;
                    (a, b.max(a + 1))
                };
                let (hh, ll) = ((y_of(c.h) * ppp).round() as i32, (y_of(c.l) * ppp).round() as i32);
                let wl = cx - wick_px / 2;
                clip.rect_filled(rect_px(wl, hh, wl + wick_px, ll.max(hh + 1)), 0.0, fill);
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
                        if dashed {
                            clip.add(egui::Shape::dashed_line(&line, st, 6.0, 4.0));
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

            // ═════════ last price ═════════
            let ly = snapc(y_of(last.c));
            let lcol = if last.c >= last.o { UP_FILL } else { DN_FILL };
            if ly > plot.top() && ly < plot.bottom() {
                dash_h(&clip, plot.left(), plot.right(), ly, 2.0, 3.0, Stroke::new(hair, lcol));
            }
            let ly_c = ly.clamp(plot.top() + 10.0, plot.bottom() - 10.0);
            let r = Rect::from_min_size(Pos2::new(plot.right() + 2.0, ly_c - 10.0), Vec2::new(AXIS_W - 4.0, 20.0));
            painter.rect_filled(r, 2.0, lcol);
            painter.text(r.center(), Align2::CENTER_CENTER, fmt_price(last.c, step), FontId::monospace(13.0), Color32::BLACK);

            // ═════════ crosshair + readout ═════════
            if let Some(pos) = resp.hover_pos().filter(|p| plot.contains(*p)) {
                let dash = |a: Pos2, b: Pos2| {
                    painter.add(egui::Shape::dashed_line(&[a, b], Stroke::new(hair, Color32::from_rgb(120, 140, 175)), 4.0, 4.0));
                };
                dash(Pos2::new(plot.left(), snapc(pos.y)), Pos2::new(plot.right(), snapc(pos.y)));
                dash(Pos2::new(snapc(pos.x), plot.top()), Pos2::new(snapc(pos.x), plot.bottom()));
                let r = Rect::from_min_size(Pos2::new(plot.right() + 2.0, pos.y - 10.0), Vec2::new(AXIS_W - 4.0, 20.0));
                painter.rect_filled(r, 2.0, Color32::from_rgb(60, 72, 96));
                painter.text(r.center(), Align2::CENTER_CENTER, fmt_price(p_of(pos.y), step), FontId::monospace(13.0), Color32::WHITE);
                let idx = ((ri - (plot.right() - pos.x) / sp).round() as i64).clamp(0, n as i64 - 1) as usize;
                let c = cs[idx];
                let d = et(c.t);
                let tl = if secs >= 86400 { d.format("%a %d %b %Y").to_string() } else { d.format("%a %d %b %Y  %H:%M").to_string() };
                let tw = 150.0;
                let r = Rect::from_min_size(Pos2::new((pos.x - tw / 2.0).clamp(plot.left(), plot.right() - tw), plot.bottom() + 3.0), Vec2::new(tw, 20.0));
                painter.rect_filled(r, 2.0, Color32::from_rgb(60, 72, 96));
                painter.text(r.center(), Align2::CENTER_CENTER, tl, FontId::monospace(12.0), Color32::WHITE);
                let chg = c.c - c.o;
                painter.text(
                    Pos2::new(plot.left() + 10.0, plot.top() + 8.0),
                    Align2::LEFT_TOP,
                    format!("O {:.2}  H {:.2}  L {:.2}  C {:.2}  {:+.2}", c.o, c.h, c.l, c.c, chg),
                    FontId::monospace(13.0),
                    if chg >= 0.0 { UP_FILL } else { DN_FILL },
                );
            }
            painter.text(
                Pos2::new(plot.left() + 10.0, plot.top() + 28.0),
                Align2::LEFT_TOP,
                format!("{} · {}", self.sym, self.tf),
                FontId::proportional(13.0),
                Color32::from_rgb(120, 140, 175),
            );
        });
    }
}

fn main() -> eframe::Result {
    let args: Vec<String> = std::env::args().collect();
    let get = |k: &str| args.iter().position(|a| a == k).and_then(|i| args.get(i + 1)).cloned();
    let sym = canon_sym(&get("--sym").unwrap_or_else(|| "NQ".into()));
    let sym = if SYMS.iter().any(|s| s.0 == sym) { sym } else { "NQ".to_string() };
    let tf = get("--tf").unwrap_or_else(|| "5m".into());
    let tf = if TFS.iter().any(|t| t.0 == tf) { tf } else { "5m".to_string() };
    let tok = std::env::var("EDGE_OANDA_TOKEN").unwrap_or_default();
    let base = std::env::var("EDGE_OANDA_BASE").unwrap_or_else(|_| "https://api-fxpractice.oanda.com".into());
    let opts = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default().with_title("EDGE — Native Chart").with_inner_size([1300.0, 850.0]).with_min_inner_size([700.0, 450.0]),
        ..Default::default()
    };
    eframe::run_native("EDGE Native Chart", opts, Box::new(move |cc| Ok(Box::new({ let mut a = App::new(cc, sym, tf, base, tok); a.intraday = args.iter().any(|x| x == "--intraday"); a }))))
}
