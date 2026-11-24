#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]
//! EDGE native chart: candles from the same OANDA feed as the web chart, drawn through the GPU.
//! Launched by the EDGE desktop shell. Reads EDGE_OANDA_TOKEN (and optional EDGE_OANDA_BASE) from the environment.

use chrono::{DateTime, Datelike, Duration as CDur, TimeZone, Timelike};
use chrono_tz::America::New_York;
use eframe::egui::{self, Align2, Color32, FontId, Pos2, Rect, Sense, Stroke, Vec2};
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
    o: f64,
    h: f64,
    l: f64,
    c: f64,
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

fn inst_for(sym: &str) -> &'static str {
    SYMS.iter().find(|s| s.0 == sym).map(|s| s.1).unwrap_or("NAS100_USD")
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

#[derive(Default)]
struct Shared {
    candles: Vec<Candle>,
    status: String,
    version: u64, // bumps when a fresh full load replaces the data
}

enum Req {
    Load { sym: String, tf: String },
}

fn fetch(base: &str, tok: &str, inst: &str, gran: &str, count: u32) -> Result<Vec<Candle>, String> {
    let url = format!(
        "{base}/v3/instruments/{inst}/candles?granularity={gran}&price=M&count={count}&alignmentTimezone=America%2FNew_York&dailyAlignment=17"
    );
    let resp = ureq::get(&url)
        .set("Authorization", &format!("Bearer {tok}"))
        .timeout(Duration::from_secs(15))
        .call()
        .map_err(|e| match e {
            ureq::Error::Status(code, _) => format!("OANDA returned HTTP {code} (check the token)"),
            other => format!("network error: {other}"),
        })?;
    let v: serde_json::Value = resp.into_json().map_err(|e| format!("bad JSON: {e}"))?;
    let arr = v["candles"].as_array().ok_or("no candles in response")?;
    let mut out = Vec::with_capacity(arr.len());
    for c in arr {
        let t = c["time"].as_str().and_then(|s| DateTime::parse_from_rfc3339(s).ok());
        let m = &c["mid"];
        let p = |k: &str| m[k].as_str().and_then(|s| s.parse::<f64>().ok());
        if let (Some(t), Some(o), Some(h), Some(l), Some(cl)) = (t, p("o"), p("h"), p("l"), p("c")) {
            out.push(Candle { t: t.timestamp(), o, h, l, c: cl });
        }
    }
    Ok(out)
}

fn worker(rx: mpsc::Receiver<Req>, shared: Arc<Mutex<Shared>>, ctx: egui::Context, base: String, tok: String) {
    let mut cur: Option<(String, String)> = None;
    loop {
        match rx.recv_timeout(Duration::from_secs(2)) {
            Ok(Req::Load { sym, tf }) => {
                {
                    let mut s = shared.lock().unwrap();
                    s.status = "loading…".into();
                }
                ctx.request_repaint();
                let gran = TFS.iter().find(|t| t.0 == tf).map(|t| t.1).unwrap_or("M5");
                match fetch(&base, &tok, inst_for(&sym), gran, 3000) {
                    Ok(cs) => {
                        let mut s = shared.lock().unwrap();
                        s.candles = cs;
                        s.status.clear();
                        s.version += 1;
                        cur = Some((sym, gran.to_string()));
                    }
                    Err(e) => {
                        shared.lock().unwrap().status = e;
                    }
                }
                ctx.request_repaint();
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                // live poll: merge the newest few candles
                if let Some((sym, gran)) = &cur {
                    if let Ok(cs) = fetch(&base, &tok, inst_for(sym), gran, 3) {
                        let mut s = shared.lock().unwrap();
                        for c in cs {
                            match s.candles.last().map(|l| l.t) {
                                Some(lt) if c.t == lt => *s.candles.last_mut().unwrap() = c,
                                Some(lt) if c.t > lt => s.candles.push(c),
                                _ => {
                                    if let Some(x) = s.candles.iter_mut().rev().take(5).find(|x| x.t == c.t) {
                                        *x = c;
                                    }
                                }
                            }
                        }
                        drop(s);
                        ctx.request_repaint();
                    }
                }
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => return,
        }
    }
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
        let _ = tx.send(Req::Load { sym: sym.clone(), tf: tf.clone() });
        App { sym, tf, shared, tx, spacing: DEFAULT_SPACING, right_idx: 0.0, seen_version: 0, has_token }
    }

    fn reload(&mut self) {
        let _ = self.tx.send(Req::Load { sym: self.sym.clone(), tf: self.tf.clone() });
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
    let d = if step >= 1.0 { 2 } else if step >= 0.1 { 2 } else { 4 };
    format!("{p:.d$}")
}

fn et(t: i64) -> DateTime<chrono_tz::Tz> {
    New_York.timestamp_opt(t, 0).single().unwrap_or_else(|| New_York.timestamp_opt(0, 0).unwrap())
}

/// Trading day key: the ET calendar day after shifting back 17 hours (session opens 18:00 ET).
fn day_key(t: i64) -> i64 {
    let d = et(t - 17 * 3600);
    d.year() as i64 * 10000 + d.month() as i64 * 100 + d.day() as i64
}

struct Levels {
    pdh: f64,
    pdl: f64,
    pdc: f64,
}

fn prev_day_levels(cs: &[Candle]) -> Option<Levels> {
    let last = cs.last()?;
    let today = day_key(last.t);
    let mut prev: Option<i64> = None;
    for c in cs.iter().rev() {
        let k = day_key(c.t);
        if k != today {
            prev = Some(k);
            break;
        }
    }
    let pk = prev?;
    let day: Vec<&Candle> = cs.iter().filter(|c| day_key(c.t) == pk).collect();
    if day.is_empty() {
        return None;
    }
    Some(Levels {
        pdh: day.iter().map(|c| c.h).fold(f64::MIN, f64::max),
        pdl: day.iter().map(|c| c.l).fold(f64::MAX, f64::min),
        pdc: day.last().unwrap().c,
    })
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
                    "No OANDA token received.\nOpen this chart from the NATIVE button in EDGE (it passes your saved token).",
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

            let (cs, ver) = {
                let s = self.shared.lock().unwrap();
                (s.candles.clone(), s.version)
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
            let x_of = |i: f32| plot.right() - (self.right_idx - i) * self.spacing;
            let first_vis = ((self.right_idx - (plot.width() / self.spacing)).floor() as i64 - 1).max(0) as usize;
            let last_vis = ((self.right_idx).ceil() as i64 + 1).min(n as i64 - 1).max(0) as usize;

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
            let y_of = |p: f64| plot.bottom() - ((p - lo) / (hi - lo)) as f32 * plot.height();
            let p_of = |y: f32| lo + ((plot.bottom() - y) / plot.height()) as f64 * (hi - lo);

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
            let every = ((95.0 / self.spacing).ceil() as usize).max(1);
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

            // ── levels (previous day H / L / C) ──
            let clip = painter.with_clip_rect(plot);
            if let Some(lv) = prev_day_levels(&cs) {
                for (name, price, col) in [
                    ("PDH", lv.pdh, Color32::from_rgb(0, 230, 118)),
                    ("PDL", lv.pdl, Color32::from_rgb(255, 68, 68)),
                    ("PDC", lv.pdc, Color32::from_rgb(190, 200, 220)),
                ] {
                    if price > lo && price < hi {
                        let y = snapc(y_of(price));
                        let mut x = plot.left();
                        while x < plot.right() {
                            clip.hline(x..=(x + 5.0).min(plot.right()), y, Stroke::new(hair, col.gamma_multiply(0.7)));
                            x += 10.0;
                        }
                        let r = Rect::from_min_size(Pos2::new(plot.right() + 2.0, y - 9.0), Vec2::new(AXIS_W - 4.0, 18.0));
                        painter.rect_filled(r, 2.0, col.gamma_multiply(0.85));
                        painter.text(r.center(), Align2::CENTER_CENTER, format!("{name} {}", fmt_price(price, step)), FontId::monospace(11.0), Color32::BLACK);
                    }
                }
            }

            // ── candles (all drawn on whole physical pixels) ──
            let wick_px = ((WICK_PTS * ppp).round() as i32).max(1);
            let bw = {
                let w = ((self.spacing * BODY_RATIO * ppp).floor() as i32).max(1);
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

            // ── last price ──
            let last = cs[n - 1];
            let ly = snapc(y_of(last.c));
            let lcol = if last.c >= last.o { UP_FILL } else { DN_FILL };
            if ly > plot.top() && ly < plot.bottom() {
                let mut x = plot.left();
                while x < plot.right() {
                    clip.hline(x..=(x + 2.0).min(plot.right()), ly, Stroke::new(hair, lcol));
                    x += 5.0;
                }
            }
            let ly_c = ly.clamp(plot.top() + 10.0, plot.bottom() - 10.0);
            let r = Rect::from_min_size(Pos2::new(plot.right() + 2.0, ly_c - 10.0), Vec2::new(AXIS_W - 4.0, 20.0));
            painter.rect_filled(r, 2.0, lcol);
            painter.text(r.center(), Align2::CENTER_CENTER, fmt_price(last.c, step), FontId::monospace(13.0), Color32::BLACK);

            // ── crosshair + readout ──
            if let Some(pos) = resp.hover_pos().filter(|p| plot.contains(*p)) {
                let dash = |a: Pos2, b: Pos2| {
                    painter.add(egui::Shape::dashed_line(&[a, b], Stroke::new(hair, Color32::from_rgb(120, 140, 175)), 4.0, 4.0));
                };
                dash(Pos2::new(plot.left(), snapc(pos.y)), Pos2::new(plot.right(), snapc(pos.y)));
                dash(Pos2::new(snapc(pos.x), plot.top()), Pos2::new(snapc(pos.x), plot.bottom()));
                let r = Rect::from_min_size(Pos2::new(plot.right() + 2.0, pos.y - 10.0), Vec2::new(AXIS_W - 4.0, 20.0));
                painter.rect_filled(r, 2.0, Color32::from_rgb(60, 72, 96));
                painter.text(r.center(), Align2::CENTER_CENTER, fmt_price(p_of(pos.y), step), FontId::monospace(13.0), Color32::WHITE);
                let idx = ((self.right_idx - (plot.right() - pos.x) / self.spacing).round() as i64).clamp(0, n as i64 - 1) as usize;
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
            // title watermark-free header
            painter.text(
                Pos2::new(plot.left() + 10.0, plot.top() + 28.0),
                Align2::LEFT_TOP,
                format!("{} · {}", self.sym, self.tf),
                FontId::proportional(13.0),
                Color32::from_rgb(120, 140, 175),
            );
        });

        let _ = CDur::seconds(0);
    }
}

fn main() -> eframe::Result {
    let args: Vec<String> = std::env::args().collect();
    let get = |k: &str| args.iter().position(|a| a == k).and_then(|i| args.get(i + 1)).cloned();
    let sym = canon_sym(&get("--sym").unwrap_or_else(|| "NQ".into()));
    let sym = if SYMS.iter().any(|s| s.0 == sym) { sym } else { "NQ".to_string() };
    let tf = get("--tf").unwrap_or_else(|| "5m".into());
    let tok = std::env::var("EDGE_OANDA_TOKEN").unwrap_or_default();
    let base = std::env::var("EDGE_OANDA_BASE").unwrap_or_else(|_| "https://api-fxpractice.oanda.com".into());
    let opts = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default().with_title("EDGE — Native Chart").with_inner_size([1300.0, 850.0]).with_min_inner_size([700.0, 450.0]),
        ..Default::default()
    };
    eframe::run_native("EDGE Native Chart", opts, Box::new(move |cc| Ok(Box::new(App::new(cc, sym, tf, base, tok)))))
}
